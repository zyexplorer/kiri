//! Pulls images from and pushes them to OCI and Docker registries, with
//! the credentials `docker login` stored (the CLI sends them along).

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::cancel::Cancel;
use crate::config::{Reach, RegistryConfig};
use crate::mirror::Mirror;
use crate::tags::{next_link, version_order};
use crate::reference::Reference;
use crate::store::{Image, Layer, RunConfig, Store, sha256_digest};

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json";

/// Layers downloaded at once. Registries and CDNs serve a few streams well;
/// more mostly adds contention.
const PARALLEL_LAYERS: usize = 4;

#[derive(Debug)]
pub enum Error {
    Http(String),
    Status { url: String, status: u16, body: String },
    NoPlatform { wanted: String, available: Vec<String> },
    Manifest(String),
    /// No such image or tag, in the words `docker pull` uses.
    NotFound(String),
    Io(io::Error),
    /// Stopped by its [`Cancel`]; nothing was recorded.
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(e) => write!(f, "network error: {e}"),
            Error::Status { url, status, body } => {
                write!(f, "{url} returned HTTP {status}")?;
                if !body.is_empty() {
                    write!(f, ": {}", body.chars().take(200).collect::<String>())?;
                }
                Ok(())
            }
            Error::NoPlatform { wanted, available } => {
                write!(f, "no {wanted} image; available: {}", available.join(", "))
            }
            Error::Manifest(e) => write!(f, "unexpected manifest: {e}"),
            Error::NotFound(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Cancelled => write!(f, "pull cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        Error::Http(e.to_string())
    }
}

/// The platform to pull, e.g. `linux/arm64`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
}

impl Platform {
    /// `linux/amd64`, `linux/arm64/v8`, `amd64`: the CLI's `--platform`.
    pub fn parse(s: &str) -> Option<Platform> {
        let mut parts = s.split('/');
        let first = parts.next()?;
        let (os, arch) = match parts.next() {
            Some(arch) => (first, arch),
            None => ("linux", first),
        };
        let arch = match arch {
            "x86_64" | "x86-64" => "amd64",
            "aarch64" => "arm64",
            a => a,
        };
        (!os.is_empty() && !arch.is_empty()).then(|| Platform { os: os.to_owned(), architecture: arch.to_owned() })
    }

    /// Linux on this machine's CPU architecture, which is what the guest runs.
    pub fn host_linux() -> Platform {
        let architecture = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            other => other,
        };
        Platform { os: "linux".into(), architecture: architecture.into() }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)
    }
}

#[derive(Debug, Clone)]
pub enum PushProgress {
    Exists { digest: String },
    Uploading { digest: String, size: u64 },
    Pushed { digest: String },
}

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// Docker layer media types, spelled the OCI way for an OCI manifest.
fn oci_layer_type(media_type: &str) -> &str {
    match media_type {
        "application/vnd.docker.image.rootfs.diff.tar.gzip" => "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.docker.image.rootfs.diff.tar" => "application/vnd.oci.image.layer.v1.tar",
        other => other,
    }
}

/// The response if it is a success, else its status and body as an error.
fn ok(mut resp: ureq::http::Response<ureq::Body>, url: &str) -> Result<ureq::http::Response<ureq::Body>, Error> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(resp);
    }
    let body = resp.body_mut().with_config().limit(64 << 10).read_to_string().unwrap_or_default();
    Err(Error::Status { url: url.to_owned(), status, body })
}

#[derive(Debug, Clone)]
pub enum Progress {
    Resolved { layers: usize, bytes: u64, cached: usize },
    /// Each layer, once the manifest is read and before any downloads.
    Layer { digest: String, size: u64, cached: bool },
    /// Bytes of a layer so far, every half MB or so.
    Downloading { digest: String, got: u64, size: u64 },
    LayerDone { digest: String, bytes: u64, cached: bool },
    /// A mirror that did not have the image (or did not answer): the pull
    /// tries the next mirror, then Docker Hub itself. `reason` is the
    /// mirror's answer made printable: no control characters, 200 at most.
    MirrorFallback { mirror: String, reason: String },
    /// The image came from this mirror rather than from Docker Hub itself
    /// (the image records it too, [`Image::mirror`]).
    FromMirror { mirror: String },
}

/// What `docker login` saved, as the CLI sends it in `X-Registry-Auth`
/// (base64url JSON) and in `POST /auth`.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    /// `base64(user:password)`, which some clients send instead.
    pub auth: String,
    pub server_address: String,
    /// An OAuth refresh token from an earlier login.
    pub identity_token: String,
    /// A bearer token to use as is.
    pub registry_token: String,
}

/// Reads the way Docker reads an AuthConfig: keys match in any case, a JSON
/// null leaves a field as it was (docker-java sends every field, the unused
/// ones as null, #654), and a top-level null is an empty config.
impl<'de> Deserialize<'de> for Credentials {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(CredentialsVisitor { loose: false })
    }
}

/// One entry of the CLI's `X-Registry-Config`, read the way Docker reads it:
/// an entry that is not an object is empty, and a field of the wrong type is
/// skipped, leaving the rest of the entry and the other entries as they are.
#[derive(Debug, Clone, Default)]
pub struct LooseCredentials(pub Credentials);

impl<'de> Deserialize<'de> for LooseCredentials {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(CredentialsVisitor { loose: true }).map(LooseCredentials)
    }
}

/// An AuthConfig key, matched in any ASCII case without copying it.
#[derive(Clone, Copy)]
enum Field {
    Username,
    Password,
    Auth,
    ServerAddress,
    IdentityToken,
    RegistryToken,
    Other,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Field;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a field name")
            }
            fn visit_str<E>(self, key: &str) -> Result<Field, E> {
                const NAMES: [(&str, Field); 6] = [
                    ("username", Field::Username),
                    ("password", Field::Password),
                    ("auth", Field::Auth),
                    ("serveraddress", Field::ServerAddress),
                    ("identitytoken", Field::IdentityToken),
                    ("registrytoken", Field::RegistryToken),
                ];
                Ok(NAMES.into_iter().find(|(n, _)| n.eq_ignore_ascii_case(key)).map_or(Field::Other, |(_, f)| f))
            }
        }
        d.deserialize_identifier(V)
    }
}

/// A field's value when loose: a string is the value, anything else
/// (null, a number, a bool, an array, an object) leaves the field as it was.
struct Loose(Option<String>);

impl<'de> Deserialize<'de> for Loose {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Loose;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_str<E>(self, v: &str) -> Result<Loose, E> {
                Ok(Loose(Some(v.to_owned())))
            }
            fn visit_string<E>(self, v: String) -> Result<Loose, E> {
                Ok(Loose(Some(v)))
            }
            fn visit_unit<E>(self) -> Result<Loose, E> {
                Ok(Loose(None))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Loose, E> {
                Ok(Loose(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Loose, E> {
                Ok(Loose(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Loose, E> {
                Ok(Loose(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Loose, E> {
                Ok(Loose(None))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut s: A) -> Result<Loose, A::Error> {
                while s.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(Loose(None))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Loose, A::Error> {
                while m.next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?.is_some() {}
                Ok(Loose(None))
            }
        }
        d.deserialize_any(V)
    }
}

/// Reads an AuthConfig object. Strict, as for `X-Registry-Auth` and
/// `POST /auth`: a value that is neither a string nor null is an error.
/// Loose, as for an `X-Registry-Config` entry: such a value is skipped, and
/// an entry that is not an object reads as empty.
struct CredentialsVisitor {
    loose: bool,
}

impl CredentialsVisitor {
    fn other<E: serde::de::Error>(&self, u: serde::de::Unexpected) -> Result<Credentials, E> {
        if self.loose { Ok(Credentials::default()) } else { Err(E::invalid_type(u, self)) }
    }
}

impl<'de> serde::de::Visitor<'de> for CredentialsVisitor {
    type Value = Credentials;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a registry credentials object")
    }
    fn visit_unit<E>(self) -> Result<Credentials, E> {
        Ok(Credentials::default())
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Credentials, E> {
        self.other(serde::de::Unexpected::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Credentials, E> {
        self.other(serde::de::Unexpected::Signed(v))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Credentials, E> {
        self.other(serde::de::Unexpected::Unsigned(v))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Credentials, E> {
        self.other(serde::de::Unexpected::Float(v))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Credentials, E> {
        self.other(serde::de::Unexpected::Str(v))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut s: A) -> Result<Credentials, A::Error> {
        if !self.loose {
            return Err(serde::de::Error::invalid_type(serde::de::Unexpected::Seq, &self));
        }
        while s.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(Credentials::default())
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Credentials, A::Error> {
        let mut c = Credentials::default();
        while let Some(key) = m.next_key::<Field>()? {
            let field = match key {
                Field::Username => &mut c.username,
                Field::Password => &mut c.password,
                Field::Auth => &mut c.auth,
                Field::ServerAddress => &mut c.server_address,
                Field::IdentityToken => &mut c.identity_token,
                Field::RegistryToken => &mut c.registry_token,
                Field::Other => {
                    m.next_value::<serde::de::IgnoredAny>()?;
                    continue;
                }
            };
            let v = if self.loose { m.next_value::<Loose>()?.0 } else { m.next_value::<Option<String>>()? };
            if let Some(v) = v {
                *field = v;
            }
        }
        Ok(c)
    }
}

impl Credentials {
    /// Decodes an `X-Registry-Auth` header. Empty or unreadable means none.
    pub fn from_header(header: &str) -> Option<Credentials> {
        let json = crate::b64::decode(header.trim())?;
        let mut c: Credentials = serde_json::from_slice(&json).ok()?;
        if c.username.is_empty() && !c.auth.is_empty() {
            let pair = crate::b64::decode(&c.auth).and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
            if let Some((u, p)) = pair.split_once(':') {
                c.username = u.to_owned();
                c.password = p.to_owned();
            }
        }
        (!c.username.is_empty() || !c.identity_token.is_empty() || !c.registry_token.is_empty()).then_some(c)
    }

    fn basic(&self) -> Option<String> {
        (!self.username.is_empty()).then(|| format!("Basic {}", crate::b64::encode(format!("{}:{}", self.username, self.password).as_bytes())))
    }
}

/// A client is cheap: the connection pool, the tokens and what each
/// registry's auth challenge said are shared by the whole process (below),
/// so the next pull skips the handshakes and token round trips this one made.
#[derive(Debug)]
pub struct Client {
    agent: ureq::Agent,
    credentials: Option<Credentials>,
    /// daemon.json's registry settings, as they were when the client was
    /// made: which hosts are insecure, and Docker Hub's mirrors.
    config: Arc<RegistryConfig>,
    /// Whether Docker Hub pulls go through the mirrors: not for the client
    /// a mirror itself is asked with.
    use_mirrors: bool,
    /// For the client a mirror is asked with: its host and the scheme it
    /// was written with, used as written (`http://` straight over plain
    /// HTTP, `https://` never down to it), as Docker uses a mirror's.
    mirror_scheme: Option<(String, &'static str)>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Where the program decides the proxy: `None` leaves ureq's default (the
/// `HTTPS_PROXY` environment), `Some(None)` is no proxy, `Some(Some((url,
/// skip)))` a proxy and the hosts that bypass it.
pub type ProxySource = fn() -> Option<Option<(String, Vec<String>)>>;
static PROXY_SOURCE: std::sync::OnceLock<ProxySource> = std::sync::OnceLock::new();

/// Called once by the daemon, so every client follows its proxy settings.
pub fn set_proxy_source(f: ProxySource) {
    let _ = PROXY_SOURCE.set(f);
}

/// Where the program reads daemon.json's registry settings
/// (`insecure-registries`, `registry-mirrors`), installed once at startup as
/// the proxy source is. Without one (the CLI's own pulls, tests) there are
/// none: every registry over verified HTTPS, Docker Hub direct.
pub type ConfigSource = fn() -> Arc<RegistryConfig>;
static CONFIG_SOURCE: std::sync::OnceLock<ConfigSource> = std::sync::OnceLock::new();

/// Called once by the daemon, so every client follows its daemon.json.
pub fn set_config_source(f: ConfigSource) {
    let _ = CONFIG_SOURCE.set(f);
}

type ProxyChoice = Option<Option<(String, Vec<String>)>>;

fn proxy(chosen: ProxyChoice) -> Option<Option<ureq::Proxy>> {
    let Some((url, skip)) = chosen? else { return Some(None) };
    let parsed = ureq::Proxy::new(&url).ok()?;
    let mut b = ureq::Proxy::builder(parsed.protocol()).host(parsed.host()).port(parsed.port());
    if let Some(u) = parsed.username() {
        b = b.username(u);
    }
    if let Some(p) = parsed.password() {
        b = b.password(p);
    }
    for s in &skip {
        b = b.no_proxy(s);
    }
    b.build().ok().map(Some)
}

/// The `host[:port]` a URL names, lowercase.
fn url_host(url: &str) -> Option<String> {
    url.parse::<ureq::http::Uri>().ok()?.authority().map(|a| a.as_str().rsplit('@').next().unwrap_or_default().to_ascii_lowercase())
}

/// A canonical `host[:port]` without its port: `[::1]:80` is `[::1]`.
fn without_port(host: &str) -> &str {
    match host.strip_prefix('[') {
        Some(_) => host.split_inclusive(']').next().unwrap_or(host),
        None => host.split(':').next().unwrap_or(host),
    }
}

/// The port a `host[:port]` names: `Ok(None)` when it names none, `Err`
/// when what follows the colon is not a port from 1 to 65535.
fn named_port(host: &str) -> Result<Option<u16>, ()> {
    if host.ends_with(']') {
        return Ok(None);
    }
    match host.rsplit_once(':') {
        None => Ok(None),
        Some((_, p)) => p.parse::<u16>().ok().filter(|p| *p != 0).map(Some).ok_or(()),
    }
}


fn is_https(url: &str) -> bool {
    url.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"))
}

fn is_http(url: &str) -> bool {
    url.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://"))
}

/// Whether `s` starts with a URI scheme and `://` (RFC 3986: a letter, then
/// letters, digits, `+`, `-` or `.`), as `https://host/x` does and
/// `/blob?next=https://x` does not.
fn has_scheme(s: &str) -> bool {
    s.find("://").is_some_and(|i| {
        let scheme = &s[..i];
        scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
    })
}

/// A redirect's `Location`, made absolute against the URL that sent it
/// (RFC 3986 section 5): an absolute URI stays, `//host/x` takes the scheme
/// of `from`, `/x` its origin, `x` its directory; `from`'s own query and
/// fragment are not part of that directory.
fn joined(from: &str, location: &str) -> String {
    if has_scheme(location) {
        return location.to_owned();
    }
    let from = from.split(['?', '#']).next().unwrap_or(from);
    let origin_end = from.find("://").map_or(0, |i| i + 3);
    let scheme = &from[..origin_end];
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{scheme}{rest}");
    }
    let origin = &from[..from[origin_end..].find('/').map_or(from.len(), |i| origin_end + i)];
    if location.starts_with('/') {
        format!("{origin}{location}")
    } else if location.starts_with('?') {
        format!("{from}{location}")
    } else {
        match from.rfind('/').filter(|&i| i >= origin.len()) {
            Some(i) => format!("{}{location}", &from[..=i]),
            None => format!("{origin}/{location}"),
        }
    }
}

/// Where a response finally came from, set by [`Client::get_following`]
/// (the request's own URL when it was not redirected).
#[derive(Clone)]
struct FinalUrl(String);

/// A status that sends the request elsewhere.
fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn final_url(resp: &ureq::http::Response<ureq::Body>) -> Option<String> {
    use ureq::ResponseExt;
    resp.extensions().get::<FinalUrl>().map(|f| f.0.clone()).or_else(|| Some(resp.get_uri().to_string()))
}

/// Whether a response that ended at `final_url` came from the registry at
/// `base`: the same scheme, host and port, or the same host moved up from
/// http to https (the default ports 80 to 443, or the same explicit port),
/// which is the registry upgrading the connection, not handing it to anyone.
/// A different host, a different port, or https down to http is not.
fn is_own_origin(final_url: &str, base: &str) -> bool {
    let (Some(f), Some(b)) = (origin_of(final_url), origin_of(base)) else { return false };
    f == b || (!b.0 && f.0 && f.1 == b.1 && (f.2 == b.2 || (b.2 == 80 && f.2 == 443)))
}

/// Where a login's Basic credentials may go, for a challenge that came from
/// `final_url` after asking `url`: there only when it is the registry's own
/// origin ([`is_own_origin`]: the same one, or the same host moved up to
/// HTTPS), never anywhere a redirect took the request.
fn basic_login_target(final_url: &str, url: &str) -> Option<String> {
    is_own_origin(final_url, url).then(|| final_url.to_owned())
}

/// The scheme, host and port a URL names (the default port of its scheme if
/// it has none): what must all match for a request to stay "the same place".
fn origin_of(url: &str) -> Option<(bool, String, u16)> {
    let uri: ureq::http::Uri = url.parse().ok()?;
    let https = match uri.scheme_str()?.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return None,
    };
    let host = uri.host()?;
    // One spelling per host: `[fd00:0::1]` and `[fd00::1]` are one address.
    let host = crate::config::canonical(host).unwrap_or_else(|| host.to_ascii_lowercase());
    Some((https, host, uri.port_u16().unwrap_or(if https { 443 } else { 80 })))
}

/// How long an idle connection waits in the pool for the next request.
/// Registries and their CDNs keep one open about a minute or more, and a
/// reused connection saves the TCP and TLS handshakes: two round trips,
/// 400 ms to Docker Hub from Europe, per host per pull.
const IDLE: Duration = Duration::from_secs(90);

/// The process's one agent, so its connection pool outlives a pull. It is
/// built again only when the proxy settings change. Like the insecure one,
/// it follows no redirect itself: [`Client::get_following`] does, so every
/// hop is checked (never from HTTPS down to plain HTTP to a host the list
/// does not allow, credentials only to the origin first asked).
fn shared_agent() -> ureq::Agent {
    static AGENT: Mutex<Option<(ProxyChoice, ureq::Agent)>> = Mutex::new(None);
    cached_agent(&AGENT, false)
}

/// The agent for HTTPS to hosts in `insecure-registries`: the shared one's
/// settings with the certificate left unchecked. It follows no redirect
/// itself: [`Client::get`] does, so a redirect away from a listed host
/// goes to a verified connection. Made the first time a listed host is
/// asked, never otherwise.
fn insecure_agent() -> ureq::Agent {
    static AGENT: Mutex<Option<(ProxyChoice, ureq::Agent)>> = Mutex::new(None);
    cached_agent(&AGENT, true)
}

fn cached_agent(slot: &Mutex<Option<(ProxyChoice, ureq::Agent)>>, insecure: bool) -> ureq::Agent {
    let chosen = PROXY_SOURCE.get().and_then(|f| f());
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((c, agent)) = slot.as_ref()
        && *c == chosen
    {
        return agent.clone();
    }
    let mut cfg = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .max_idle_age(IDLE)
        // Parallel layer downloads each hold a connection to the registry
        // and one to its CDN; keep them all for the next pull.
        .max_idle_connections(32)
        .max_idle_connections_per_host(PARALLEL_LAYERS * 2)
        .user_agent(concat!("kiri/", env!("CARGO_PKG_VERSION")))
        // Redirects are followed by hand, hop by hop ([`Client::get_following`]).
        .max_redirects(0);
    // A listed host's certificate is not checked, as Docker reaches an
    // insecure registry; every other agent keeps ureq's verified TLS.
    if insecure {
        cfg = cfg.tls_config(crate::connect::unverified_tls_config());
    }
    if let Some(p) = proxy(chosen.clone()) {
        cfg = cfg.proxy(p);
    }
    // Happy Eyeballs rather than ureq's one address after another, so an
    // IPv6 address without a working route does not stall the pull.
    let agent = ureq::Agent::with_parts(cfg.build(), crate::connect::connector(), ureq::unversioned::resolver::DefaultResolver::default());
    *slot = Some((chosen, agent.clone()));
    agent
}

/// Bearer tokens (and Basic answers) by registry, repository, actions and
/// who asked, with when each stops being used. A registry says how long a
/// token lasts (`expires_in`, 60 s if it doesn't; Docker Hub gives 300 s).
static TOKENS: std::sync::LazyLock<Mutex<HashMap<String, (String, Instant)>>> = std::sync::LazyLock::new(Default::default);

/// Each registry's last bearer challenge without its scope, so a pull from
/// a registry seen before asks for its token straight away instead of
/// first sending a request only to be told where the token server is.
static CHALLENGES: std::sync::LazyLock<Mutex<HashMap<String, String>>> = std::sync::LazyLock::new(Default::default);

/// What Docker Hub answers every anonymous request with, so even the first
/// pull from it can fetch the token while it connects to the registry. If
/// Hub ever changes it, the registry's own challenge takes over.
const DOCKER_HUB_CHALLENGE: &str = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#;

/// Manifests fetched by digest, which never change: re-pulling an image
/// (after `rmi`, or the same image under another tag) skips that round trip.
/// Kept per repository and per who asked (`manifest_key`), so a manifest
/// read with one repository's access, or with credentials, is not handed
/// out for another repository or to an anonymous pull.
static MANIFESTS: std::sync::LazyLock<Mutex<ManifestCache>> = std::sync::LazyLock::new(Default::default);

#[derive(Default)]
struct ManifestCache {
    by_digest: HashMap<String, (Vec<u8>, String)>,
    bytes: usize,
}

impl ManifestCache {
    /// Real manifests are a few KB; this holds hundreds.
    const MAX_BYTES: usize = 4 << 20;

    fn get(&self, key: &str) -> Option<(Vec<u8>, String)> {
        self.by_digest.get(key).cloned()
    }

    /// Keeps `bytes` under `key` if they are what `digest` names.
    fn put(&mut self, key: &str, digest: &str, bytes: &[u8], media_type: &str) {
        if bytes.len() > Self::MAX_BYTES || sha256_digest(bytes) != digest {
            return;
        }
        if self.bytes + bytes.len() > Self::MAX_BYTES {
            self.by_digest.clear();
            self.bytes = 0;
        }
        if self.by_digest.insert(key.to_owned(), (bytes.to_vec(), media_type.to_owned())).is_none() {
            self.bytes += bytes.len();
        }
    }
}

/// The longest a token is used, whatever the registry says: its
/// `expires_in` is untrusted, and `u64::MAX` seconds overflowed the
/// deadline (a panic, so the daemon went down).
const MAX_TOKEN_TTL: Duration = Duration::from_secs(3600);

/// How long to use a token the registry says lasts `ttl`: a little less,
/// so one is not sent just as it runs out, and never over an hour.
fn token_lifetime(ttl: Duration) -> Duration {
    let ttl = ttl.min(MAX_TOKEN_TTL);
    ttl - ttl / 10
}

/// A bearer challenge reduced to its realm and service: the scope it named
/// was for the repository that request was about.
fn without_scope(challenge: &str) -> Option<String> {
    if !challenge.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("bearer ")) {
        return None;
    }
    let p = parse_challenge(&challenge[7..]);
    let realm = p.get("realm")?;
    Some(match p.get("service") {
        Some(service) => format!(r#"Bearer realm="{realm}",service="{service}""#),
        None => format!(r#"Bearer realm="{realm}""#),
    })
}

/// Tries a blob download gets in a row without receiving anything.
const BLOB_TRIES: u32 = 5;

/// A failure worth trying again: the network (a reset, a stall, a refused
/// or timed-out connect) or the registry being briefly unwell, not an
/// answer that will be the same next time.
fn transient(e: &Error) -> bool {
    match e {
        Error::Http(_) => true,
        Error::Status { status, .. } => *status >= 500 || *status == 408 || *status == 429,
        Error::Io(e) => {
            // ureq's own errors (a timeout among them) come wrapped.
            if let Some(u) = e.get_ref().and_then(|i| i.downcast_ref::<ureq::Error>()) {
                return matches!(u, ureq::Error::Timeout(_) | ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::HostNotFound);
            }
            matches!(
                e.kind(),
                io::ErrorKind::TimedOut
                    | io::ErrorKind::WouldBlock
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
            )
        }
        _ => false,
    }
}

/// Reads the rest of a response, so its connection goes back to the pool
/// instead of being closed (ureq only reuses a connection read to the end).
fn drain(mut resp: ureq::http::Response<ureq::Body>) {
    let _ = resp.body_mut().with_config().limit(64 << 10).read_to_vec();
}

/// A pooled connection the server closed just as it was reused: the request
/// never got an answer and is safe to send again on a new connection.
fn stale(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::Io(e) if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted | io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof))
}

/// The scheme each `insecure-registries` host answered on, and until when
/// that is trusted: HTTPS is asked first, as Docker asks it, and a host
/// that only speaks plain HTTP is asked that way from then on. Keyed by the
/// host in its one spelling ([`crate::config::canonical`]), and at most
/// [`MAX_SCHEMES`] hosts: past that the expired ones go, and if none has,
/// all of them.
static SCHEMES: std::sync::LazyLock<Mutex<HashMap<String, (&'static str, Instant)>>> = std::sync::LazyLock::new(Default::default);

/// One lock per host being asked, so pulls that need the same host at once
/// wait for one answer instead of each asking (and each waiting out a host
/// that does not answer).
static PROBING: std::sync::LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = std::sync::LazyLock::new(Default::default);

/// How long a found scheme is used before the host is asked again.
const SCHEME_TTL: Duration = Duration::from_secs(300);
/// How long a host that answered neither is left before it is asked again.
const SCHEME_FAILED_TTL: Duration = Duration::from_secs(30);
const MAX_SCHEMES: usize = 256;

fn remembered_scheme(key: &str) -> Option<&'static str> {
    SCHEMES.lock().unwrap_or_else(|e| e.into_inner()).get(key).copied().filter(|(_, until)| Instant::now() < *until).map(|(s, _)| s)
}

fn remember_scheme(key: String, scheme: &'static str, ttl: Duration) {
    let mut schemes = SCHEMES.lock().unwrap_or_else(|e| e.into_inner());
    if schemes.len() >= MAX_SCHEMES && !schemes.contains_key(&key) {
        let now = Instant::now();
        schemes.retain(|_, (_, until)| now < *until);
        if schemes.len() >= MAX_SCHEMES {
            schemes.clear();
        }
    }
    schemes.insert(key, (scheme, Instant::now() + ttl));
}

/// The scheme an `insecure-registries` host answers on: `https` if it
/// answers HTTPS at all (any status, the certificate unchecked), else
/// `http` if it answers plain HTTP. Remembered for [`SCHEME_TTL`]; a host
/// that answers neither is asked over HTTPS, so the error says what that
/// said, and is remembered so for [`SCHEME_FAILED_TTL`].
fn insecure_scheme(host: &str) -> &'static str {
    let key = crate::config::canonical(host).unwrap_or_else(|| host.to_ascii_lowercase());
    if let Some(scheme) = remembered_scheme(&key) {
        return scheme;
    }
    let lock = {
        let mut probing = PROBING.lock().unwrap_or_else(|e| e.into_inner());
        probing.retain(|_, l| Arc::strong_count(l) > 1);
        probing.entry(key.clone()).or_default().clone()
    };
    // Waits for another pull's probe of the same host, but no longer than
    // this pull runs: a stopped pull returns at once.
    let _one = loop {
        match lock.try_lock() {
            Ok(g) => break g,
            Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) if crate::cancel::current_cancelled() => return "https",
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    // Another pull may have asked while this one waited.
    if let Some(scheme) = remembered_scheme(&key) {
        return scheme;
    }
    // A server that waits instead of refusing a handshake it can't read
    // gets as long as Docker gives a TLS handshake.
    // An answer from the host itself, any status: not a proxy's word that
    // it could not reach it (502, 503 or 504 to a plain-HTTP request).
    let answers = |url: String, agent: ureq::Agent| {
        agent.get(&url).config().timeout_global(Some(Duration::from_secs(10))).build().call().is_ok_and(|resp| {
            let gateway = !is_https(&url) && matches!(resp.status().as_u16(), 502..=504);
            drain(resp);
            !gateway
        })
    };
    // Stopped before it began, or while waiting for another pull's probe:
    // nothing is asked, and nothing learnt.
    if crate::cancel::current_cancelled() {
        return "https";
    }
    let (scheme, ttl) = if answers(format!("https://{host}/v2/"), insecure_agent()) {
        ("https", SCHEME_TTL)
    } else if crate::cancel::current_cancelled() {
        // Stopped during the first probe: the second is not started.
        return "https";
    } else if answers(format!("http://{host}/v2/"), shared_agent()) {
        ("http", SCHEME_TTL)
    } else if crate::cancel::current_cancelled() {
        // Stopped, not unanswered: nothing learnt about the host.
        return "https";
    } else {
        ("https", SCHEME_FAILED_TTL)
    };
    remember_scheme(key, scheme, ttl);
    scheme
}

impl Client {
    pub fn new() -> Client {
        let config = CONFIG_SOURCE.get().map(|f| f()).unwrap_or_default();
        Client { agent: shared_agent(), credentials: None, config, use_mirrors: true, mirror_scheme: None }
    }

    /// This client with `config` in place of daemon.json's settings.
    pub fn with_config(mut self, config: RegistryConfig) -> Client {
        self.config = Arc::new(config);
        self
    }

    /// `https://host`, and for a registry on this machine or in
    /// `insecure-registries` whichever of HTTPS (its certificate unchecked)
    /// and plain HTTP it answers on, HTTPS first. A mirror is asked with the
    /// scheme it was written with, unprobed ([`Client::for_mirror`]).
    fn base(&self, host: &str) -> String {
        let scheme = match (self.written_scheme(host), self.config.reach(host)) {
            (Some(scheme), _) => scheme,
            (None, Reach::Https) => "https",
            (None, Reach::Insecure) => insecure_scheme(host),
        };
        format!("{scheme}://{host}")
    }

    /// The scheme `host` was written with, when it is the mirror this
    /// client asks.
    fn written_scheme(&self, host: &str) -> Option<&'static str> {
        self.mirror_scheme.as_ref().filter(|(h, _)| h.eq_ignore_ascii_case(host)).map(|(_, s)| *s)
    }

    /// The agent for a request to `url`, a registry's own address: the one
    /// that leaves the certificate unchecked only for HTTPS to a registry on
    /// this machine or in `insecure-registries`. Everything else is verified.
    fn agent_for(&self, url: &str) -> ureq::Agent {
        match url_host(url) {
            Some(host) if is_https(url) && self.config.is_unverified(&host) => insecure_agent(),
            _ => self.agent.clone(),
        }
    }

    /// Whether a request that `owner` (a registry's `host[:port]`) caused,
    /// for its token service, a blob, an upload, may go to `host` without a
    /// verified certificate or over plain HTTP: only when `owner` is itself
    /// insecure (listed, or on this machine) and `host` is `owner`, a host
    /// written in the list, or on this machine too. A network entry alone
    /// does not make a host one an insecure registry may send people to.
    fn unverified_ok(&self, host: &str, owner: &str) -> bool {
        self.config.is_unverified(owner)
            && !crate::config::is_hub(host)
            && (crate::config::canonical(host).is_some_and(|h| Some(h) == crate::config::canonical(owner))
                || self.config.is_listed_host(host)
                || (crate::reference::is_local(owner) && crate::reference::is_local(host)))
    }

    /// The agent for a hop of a request that `owner` caused.
    fn agent_for_hop(&self, url: &str, owner: &str) -> ureq::Agent {
        match url_host(url) {
            Some(host) if is_https(url) && self.unverified_ok(&host, owner) => insecure_agent(),
            _ => self.agent.clone(),
        }
    }

    /// Whether a token `realm` the registry named may be asked: over HTTPS,
    /// or over plain HTTP only when the request that got the 401 was itself
    /// plain HTTP (`plain_http`) and the realm is on the insecure registry's
    /// own host and port (a registry on plain HTTP names its own address for
    /// tokens). Fails closed: an HTTPS request (or an unknown scheme) never
    /// gets a plain-HTTP realm. For a registry on this machine its own port
    /// on this machine counts as its own, however the realm spells loopback
    /// (`localhost`, `127.0.0.1`, `[::1]`); a name under `.localhost` does
    /// not, as a resolver may send it to DNS. Never in the clear to another
    /// host or port, listed or not, and never because of a network entry.
    fn realm_is_secure(&self, realm: &str, registry: &str, plain_http: bool) -> bool {
        if is_https(realm) {
            return true;
        }
        if !plain_http || !is_http(realm) {
            return false;
        }
        let Some(host) = url_host(realm) else { return false };
        let same_host = if crate::reference::is_local(&host) {
            crate::reference::is_local(registry)
        } else {
            let name = |h: &str| crate::config::canonical(h).map(|c| without_port(&c).to_owned());
            name(&host).is_some_and(|h| Some(h) == name(registry))
        };
        let realm_port = named_port(&host).map(|p| p.unwrap_or(80));
        let registry_port = named_port(registry).map(|p| p.unwrap_or(80));
        let same_port = matches!((realm_port, registry_port), (Ok(a), Ok(b)) if a == b);
        self.config.is_unverified(registry) && !crate::config::is_hub(&host) && same_host && same_port
    }

    /// Who the tokens are for: a fingerprint of the credentials, so one
    /// user's token is never sent for another, or for an anonymous pull.
    fn who(&self) -> String {
        match &self.credentials {
            None => "-".into(),
            Some(c) => sha256_digest(format!("{}\0{}\0{}\0{}", c.username, c.password, c.identity_token, c.registry_token).as_bytes()),
        }
    }

    fn token_key(&self, r: &Reference, actions: &str) -> String {
        format!("{}/{} {actions} {}", r.registry, r.repository, self.who())
    }

    fn manifest_key(&self, r: &Reference, reference: &str) -> String {
        format!("{}/{}@{reference} {}", r.registry, r.repository, self.who())
    }

    fn cached_token(key: &str) -> Option<String> {
        let mut tokens = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
        match tokens.get(key) {
            Some((header, until)) if Instant::now() < *until => Some(header.clone()),
            Some(_) => {
                tokens.remove(key);
                None
            }
            None => None,
        }
    }

    fn remember_token(key: String, header: String, ttl: Duration) {
        let mut tokens = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        tokens.retain(|_, (_, until)| now < *until);
        let Some(until) = now.checked_add(token_lifetime(ttl)) else { return };
        tokens.insert(key, (header, until));
    }

    /// Before a pull's first request: when there is no token for it yet but
    /// the registry's challenge is known, gets the token while a connection
    /// to the registry opens, rather than connecting, being refused and
    /// only then asking for a token. Saves a round trip and overlaps the
    /// handshakes. A failed token is left to the usual path, which reports
    /// it; a registry that cannot be reached at all is reported at once,
    /// rather than waiting out the connect timeout a second time.
    fn warm_up(&self, r: &Reference, actions: &str) -> Result<(), Error> {
        let key = self.token_key(r, actions);
        if self.credentials.as_ref().is_some_and(|c| !c.registry_token.is_empty()) || Self::cached_token(&key).is_some() {
            return Ok(());
        }
        let known = CHALLENGES.lock().unwrap_or_else(|e| e.into_inner()).get(&r.registry).cloned();
        let Some(challenge) = known.or_else(|| (r.registry == "docker.io").then(|| DOCKER_HUB_CHALLENGE.to_owned())) else { return Ok(()) };
        let url = format!("{}/v2/", self.base(r.api_host()));
        let agent = self.agent_for(&url);
        std::thread::scope(|s| {
            // Two connections: after the manifests, the config and the first
            // layer are fetched at once.
            let connections: Vec<_> = (0..2).map(|_| s.spawn(|| agent.get(&url).call().map(drain))).collect();
            // Never answer a plain-HTTP realm speculatively from cached probe
            // state: plain-HTTP realms wait for an actual plain-HTTP 401.
            if let Ok((header, ttl)) = self.answer(&challenge, &format!("repository:{}:{actions}", r.repository), &r.registry, false) {
                Self::remember_token(key, header, ttl);
            }
            let mut failed = None;
            for c in connections {
                match c.join() {
                    Ok(Ok(())) => return Ok(()),
                    Ok(Err(e)) => failed = Some(e),
                    Err(_) => {}
                }
            }
            failed.map_or(Ok(()), |e| Err(e.into()))
        })
    }

    /// The same client, authenticating as `credentials`.
    pub fn with_credentials(mut self, credentials: Option<Credentials>) -> Client {
        self.credentials = credentials;
        self
    }

    /// The mirrors this client's Docker Hub pulls try: none for the client a
    /// mirror is itself asked with, and only the usable ones
    /// ([`RegistryConfig::mirror_allowed`]).
    fn mirrors(&self) -> Vec<&Mirror> {
        if self.use_mirrors { self.config.usable_mirrors().collect() } else { Vec::new() }
    }

    /// The same connection pool and settings, no credentials and no
    /// mirrors: what a mirror is asked with, so a Docker Hub sign-in never
    /// leaves for a third party's registry. Mirrors serve public images;
    /// one that needs a login is not supported.
    fn anonymous(&self) -> Client {
        Client { agent: self.agent.clone(), credentials: None, config: self.config.clone(), use_mirrors: false, mirror_scheme: None }
    }

    /// [`Client::anonymous`] for the mirror at `target`: it is asked over
    /// `scheme` (the scheme its address was written with), as Docker asks a
    /// mirror, with no probe and no fall back from HTTPS to plain HTTP. HTTPS
    /// to a mirror on this machine or in `insecure-registries` leaves the
    /// certificate unchecked, as for any registry there.
    fn for_mirror(&self, target: &Reference, scheme: Option<&'static str>) -> Client {
        Client { mirror_scheme: scheme.map(|s| (target.registry.clone(), s)), ..self.anonymous() }
    }

    /// The references a pull of `name` tries, in order: the configured
    /// mirrors of Docker Hub (each carrying the scheme it was written with),
    /// then Docker Hub itself (`None`). Only Hub is mirrored, as with
    /// Docker's `registry-mirrors`. A pull signed in to Docker Hub asks the
    /// mirrors only for its official images (`library/…`, all public): any
    /// other repository may be the person's private one, whose name a mirror
    /// must not learn and whose content it must not stand in for. Docker asks
    /// the mirrors for those too.
    fn targets(&self, name: &Reference) -> Vec<(Reference, Option<&'static str>)> {
        let mut targets: Vec<(Reference, Option<&'static str>)> = Vec::new();
        let private_maybe = self.credentials.is_some() && !name.repository.starts_with("library/");
        if name.registry == "docker.io" && !private_maybe {
            for mirror in self.mirrors() {
                let target = (name.via(&mirror.host, mirror.prefix.as_deref()), Some(if mirror.http { "http" } else { "https" }));
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        targets.push((name.clone(), None));
        targets
    }

    /// Pulls `reference` for `platform` into `store` and records it in the
    /// image index. Layers already in the store are not downloaded again.
    pub fn pull(
        &self,
        store: &Store,
        reference: &Reference,
        platform: &Platform,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<Image, Error> {
        self.pull_with(store, reference, platform, progress, &Cancel::new())
    }

    /// [`Client::pull`] that `cancel` can stop from another thread: it then
    /// ends with [`Error::Cancelled`], its downloads stopped and no image
    /// recorded. Layers it finished stay in the store, for the next pull.
    /// Docker Hub is asked through the configured mirrors first, in order;
    /// the image keeps the name it was asked for.
    pub fn pull_with(
        &self,
        store: &Store,
        reference: &Reference,
        platform: &Platform,
        progress: &(dyn Fn(Progress) + Sync),
        cancel: &Cancel,
    ) -> Result<Image, Error> {
        let targets = self.targets(reference);
        self.pull_from(store, reference, &targets, platform, progress, cancel)
    }

    /// [`Client::pull_with`] over an explicit list of references: the first
    /// that resolves the image is where its layers come from. `name` is the
    /// reference the image is recorded as.
    fn pull_from(
        &self,
        store: &Store,
        name: &Reference,
        targets: &[(Reference, Option<&'static str>)],
        platform: &Platform,
        progress: &(dyn Fn(Progress) + Sync),
        cancel: &Cancel,
    ) -> Result<Image, Error> {
        let _on = cancel.enter();
        let mut last: Option<Error> = None;
        for (target, scheme) in targets {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            // A mirror is asked without the user's Docker Hub credentials;
            // the official registry is asked with them. The layers come from
            // where the manifest did, asked the same way.
            let mirror = scheme.is_some() || target.registry != name.registry;
            let anonymous = mirror.then(|| self.for_mirror(target, *scheme));
            let client = anonymous.as_ref().unwrap_or(self);
            // A mirror that fails anywhere in the pull (no manifest, a layer
            // missing or wrong, an image for another platform) hands it to
            // the next, and Docker Hub last.
            let pulled = client.resolve(target, platform, cancel).and_then(|r| client.download_layers(store, name, &r, platform, progress, cancel));
            match pulled {
                Ok(image) => {
                    if mirror {
                        progress(Progress::FromMirror { mirror: target.registry.clone() });
                    }
                    return Ok(image);
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(_) if cancel.is_cancelled() => return Err(Error::Cancelled),
                Err(e) if mirror => {
                    progress(Progress::MirrorFallback { mirror: target.registry.clone(), reason: printable(&e.to_string()) });
                    last = Some(e);
                }
                Err(e) => return Err(not_found(e, name)),
            }
        }
        let e = last.unwrap_or_else(|| Error::Manifest("no registry to pull from".into()));
        Err(not_found(e, name))
    }

    /// Reads the manifest for `r` and picks this platform's: the first half
    /// of a pull, before anything is downloaded. Errors are raw registry
    /// replies; the caller turns the last one into `docker pull`'s wording.
    fn resolve(&self, r: &Reference, platform: &Platform, cancel: &Cancel) -> Result<Resolved, Error> {
        let _on = cancel.enter();
        let stopped = |e: Error| if cancel.is_cancelled() { Error::Cancelled } else { e };
        self.warm_up(r, "pull").map_err(stopped)?;
        let (bytes, media_type) = self.manifest(r, &r.reference).map_err(stopped)?;
        // A pull by digest must get exactly that content, whether it is a
        // single manifest or a multi-arch index. Without this, a registry
        // could answer a pinned digest with any index naming any children.
        if r.is_digest() && sha256_digest(&bytes) != r.reference {
            return Err(Error::Manifest("manifest failed digest check".into()));
        }
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| Error::Manifest(e.to_string()))?;

        // An index lists one manifest per platform; pick ours and fetch it.
        let index_digest = manifest.manifests.is_some().then(|| sha256_digest(&bytes));
        let (manifest, digest) = if let Some(entries) = manifest.manifests {
            let wanted = entries.iter().find(|m| {
                m.platform.as_ref().is_some_and(|p| p.os == platform.os && p.architecture == platform.architecture)
            });
            let Some(entry) = wanted else {
                return Err(Error::NoPlatform {
                    wanted: platform.to_string(),
                    available: entries
                        .iter()
                        .filter_map(|m| m.platform.as_ref())
                        .filter(|p| p.os != "unknown")
                        .map(|p| format!("{}/{}", p.os, p.architecture))
                        .collect(),
                });
            };
            let (bytes, _) = self.manifest(r, &entry.digest).map_err(stopped)?;
            if sha256_digest(&bytes) != entry.digest {
                return Err(Error::Manifest(format!("manifest {} failed digest check", entry.digest)));
            }
            let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| Error::Manifest(e.to_string()))?;
            (m, entry.digest.clone())
        } else {
            if r.is_digest() && sha256_digest(&bytes) != r.reference {
                return Err(Error::Manifest("manifest failed digest check".into()));
            }
            (manifest, sha256_digest(&bytes))
        };
        Ok(Resolved { target: r.clone(), index_digest, digest, manifest, media_type })
    }

    /// The second half of a pull: the config and layers of `resolved`,
    /// downloaded from the registry that resolved it, and recorded under
    /// `name`, the reference the user asked for.
    fn download_layers(
        &self,
        store: &Store,
        name: &Reference,
        resolved: &Resolved,
        platform: &Platform,
        progress: &(dyn Fn(Progress) + Sync),
        cancel: &Cancel,
    ) -> Result<Image, Error> {
        let Resolved { target, index_digest, digest, manifest, media_type } = resolved;
        let (Some(config), Some(layers)) = (manifest.config.as_ref(), manifest.layers.as_ref()) else {
            return Err(Error::Manifest(format!("{media_type} has no config or layers")));
        };

        let layers: Vec<Layer> = layers
            .iter()
            .map(|l| Layer { digest: l.digest.clone(), size: l.size, media_type: l.media_type.clone() })
            .collect();
        // The sizes are the registry's word: a total that overflows is no image.
        let total = layers.iter().try_fold(0u64, |sum, l| sum.checked_add(l.size)).ok_or_else(|| Error::Manifest("the layers' sizes add up to more than any image".into()))?;
        // Download missing layers in parallel, with a fixed number of workers
        // pulling from a shared queue, the largest first: the pull takes as
        // long as its slowest stream, so the big layer must not start last.
        let mut queue = layers.iter().collect::<Vec<_>>();
        queue.sort_by_key(|l| l.size);
        let queue = Mutex::new(queue);
        let first_error: Mutex<Option<Error>> = Mutex::new(None);
        // Bounded: the manifest declares the size, and a config is small.
        let fetch_config = || -> Result<(), Error> {
            let _on = cancel.enter();
            if store.has_blob(&config.digest, config.size) {
                return Ok(());
            }
            self.download_with(store, target, &config.digest, crate::store::MAX_CONFIG, Some(config.size), &|_| {}, cancel).map(|_| ())
        };
        // A mirror stands in for Docker Hub: an image for another platform
        // than the one asked is not Hub's answer, and the pull moves on
        // before any layer is fetched. (From the registry itself it is
        // kept, as Docker keeps it.)
        let mirror = (target.registry != name.registry).then(|| target.registry.clone());
        if mirror.is_some() {
            fetch_config().map_err(|e| if cancel.is_cancelled() { Error::Cancelled } else { e })?;
            let cfg: ImageConfig = serde_json::from_slice(&store.read_config(&config.digest)?).map_err(|e| Error::Manifest(e.to_string()))?;
            if cfg.os.as_deref() != Some(platform.os.as_str()) || cfg.architecture.as_deref() != Some(platform.architecture.as_str()) {
                return Err(Error::NoPlatform { wanted: platform.to_string(), available: vec![format!("{}/{}", cfg.os.as_deref().unwrap_or_default(), cfg.architecture.as_deref().unwrap_or_default())] });
            }
        }
        // The layers are announced once the pull will fetch them: a mirror
        // passed over above shows none.
        let cached = layers.iter().filter(|l| store.has_blob(&l.digest, l.size)).count();
        progress(Progress::Resolved { layers: layers.len(), bytes: total, cached });
        for l in &layers {
            progress(Progress::Layer { digest: l.digest.clone(), size: l.size, cached: store.has_blob(&l.digest, l.size) });
        }
        std::thread::scope(|s| {
            // From the registry itself the config comes alongside the layers
            // rather than before them: it is small, and fetching it first
            // cost a round trip to the registry and one to its CDN before
            // any layer started.
            if mirror.is_none() {
                s.spawn(|| {
                    if let Err(e) = fetch_config() {
                        first_error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                    }
                });
            }
            for _ in 0..PARALLEL_LAYERS.min(layers.len()) {
                s.spawn(|| {
                    let _on = cancel.enter();
                    loop {
                        let Some(layer) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop() else { return };
                        if cancel.is_cancelled() || first_error.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                            return;
                        }
                        let cached = store.has_blob(&layer.digest, layer.size);
                        let result = if cached {
                            Ok(layer.size)
                        } else {
                            // No more than the manifest says (one byte over is
                            // what tells a longer body from the end), and
                            // exactly that.
                            let on_bytes = |got| progress(Progress::Downloading { digest: layer.digest.clone(), got, size: layer.size });
                            self.download_with(store, target, &layer.digest, layer.size.saturating_add(1), Some(layer.size), &on_bytes, cancel)
                        };
                        match result {
                            Ok(bytes) => progress(Progress::LayerDone { digest: layer.digest.clone(), bytes, cached }),
                            Err(e) => {
                                first_error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                                return;
                            }
                        }
                    }
                });
            }
        });
        // Cancelled: whatever a stopped download said, that's why.
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(e) = first_error.into_inner().unwrap_or_else(|e| e.into_inner()) {
            return Err(e);
        }
        let cfg_bytes = store.read_config(&config.digest)?;
        let cfg: ImageConfig = serde_json::from_slice(&cfg_bytes).map_err(|e| Error::Manifest(e.to_string()))?;

        let run = cfg.config.unwrap_or_default();
        let image = Image {
            name: name.short(),
            digest: digest.clone(),
            index_digest: index_digest.clone(),
            config_digest: config.digest.clone(),
            size: total,
            layers,
            platform: platform.to_string(),
            created: cfg.created.unwrap_or_default(),
            pulled_at: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            config: RunConfig {
                env: run.env.unwrap_or_default(),
                entrypoint: run.entrypoint.unwrap_or_default(),
                cmd: run.cmd.unwrap_or_default(),
                working_dir: run.working_dir.unwrap_or_default(),
                user: run.user.unwrap_or_default(),
            },
            mirror,
        };
        // The last moment a cancel can stop it: after this it is pulled.
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        store.put_image(image.clone())?;
        Ok(image)
    }

    fn manifest(&self, r: &Reference, reference: &str) -> Result<(Vec<u8>, String), Error> {
        let by_digest = reference.starts_with("sha256:");
        let key = self.manifest_key(r, reference);
        if by_digest && let Some(hit) = MANIFESTS.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return Ok(hit);
        }
        let url = format!("{}/v2/{}/manifests/{reference}", self.base(r.api_host()), r.repository);
        let mut resp = self.get(r, &url, Some(ACCEPT))?;
        let media_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = resp.body_mut().with_config().limit(4 << 20).read_to_vec()?;
        if by_digest {
            MANIFESTS.lock().unwrap_or_else(|e| e.into_inner()).put(&key, reference, &bytes, &media_type);
        }
        Ok((bytes, media_type))
    }

    /// Streams one blob into the store, verifying its digest. Returns its size.
    #[cfg(test)]
    fn download(&self, store: &Store, r: &Reference, digest: &str, on_bytes: &dyn Fn(u64)) -> Result<u64, Error> {
        self.download_with(store, r, digest, u64::MAX, None, on_bytes, &Cancel::new())
    }

    /// Downloads a blob, telling `on_bytes` how much has arrived every
    /// half MB, so a pull can show a progress bar. A connection that drops
    /// or stalls ([`crate::connect::STALL`]) is resumed from where it got to
    /// (a `Range` request; from the start if the server ignores it), as
    /// Docker retries a layer, up to [`BLOB_TRIES`] times in a row without
    /// progress. Refuses bodies over `max` bytes, and one that is not
    /// exactly `want` bytes when given (before it goes into the store), and
    /// stops (removing what it wrote) once `cancel` is.
    #[allow(clippy::too_many_arguments)]
    fn download_with(&self, store: &Store, r: &Reference, digest: &str, max: u64, want: Option<u64>, on_bytes: &dyn Fn(u64), cancel: &Cancel) -> Result<u64, Error> {
        // A declared size bounds the read too: one byte over it is enough
        // to tell a longer body from the end.
        let max = want.map_or(max, |w| w.saturating_add(1).min(max));
        let url = format!("{}/v2/{}/blobs/{digest}", self.base(r.api_host()), r.repository);
        let mut out = store.blob_writer(digest)?;
        let mut buf = vec![0u8; 256 * 1024];
        let (mut got, mut told) = (0u64, 0u64);
        let mut failed = 0;
        loop {
            let before = got;
            let result = (|| -> Result<(), Error> {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let mut resp = self.get_from(r, &url, got)?;
                if got > 0 && resp.status().as_u16() != 206 {
                    // The whole blob again: start over.
                    out = store.blob_writer(digest)?;
                    (got, told) = (0, 0);
                }
                let mut body = resp.body_mut().with_config().limit(max.saturating_sub(got)).reader();
                loop {
                    let n = body.read(&mut buf)?;
                    // A shut-down connection reads as an early end.
                    if cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    if n == 0 {
                        return Ok(());
                    }
                    out.write_all(&buf[..n])?;
                    got += n as u64;
                    if got - told >= 512 * 1024 {
                        on_bytes(got);
                        told = got;
                    }
                }
            })();
            match result {
                Ok(()) if want.is_some_and(|w| w != got) => {
                    return Err(Error::Manifest(format!("{digest} is {got} bytes, the manifest says {}", want.unwrap_or_default())));
                }
                Ok(()) => return Ok(out.finish()?),
                Err(_) if cancel.is_cancelled() => return Err(Error::Cancelled),
                Err(e) if transient(&e) => {
                    failed = if got > before { 1 } else { failed + 1 };
                    if failed >= BLOB_TRIES {
                        return Err(e);
                    }
                    cancel.sleep(Duration::from_millis(500 << (failed - 1).min(4)));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// GET a blob from byte `from` on (all of it from 0).
    fn get_from(&self, r: &Reference, url: &str, from: u64) -> Result<ureq::http::Response<ureq::Body>, Error> {
        if from == 0 {
            return self.get(r, url, None);
        }
        let range = [("Range", format!("bytes={from}-"))];
        let resp = self.authed(r, "pull", |auth| self.send_get(url, auth, &range))?;
        ok(resp, url)
    }

    /// GET with registry auth.
    fn get(&self, r: &Reference, url: &str, accept: Option<&str>) -> Result<ureq::http::Response<ureq::Body>, Error> {
        let accept: Vec<(&str, String)> = accept.map(|a| vec![("Accept", a.to_owned())]).unwrap_or_default();
        let resp = self.authed(r, "pull", |auth| match self.send_get(url, auth, &accept) {
            Err(e) if stale(&e) => self.send_get(url, auth, &accept),
            r => r,
        })?;
        ok(resp, url)
    }

    /// GET `url` with `headers`, and `auth` as its `Authorization`, its
    /// redirects followed by [`Client::get_following`]: no agent follows
    /// one itself.
    fn send_get(&self, url: &str, auth: Option<&str>, headers: &[(&str, String)]) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let owner = url_host(url).unwrap_or_default();
        self.get_following(url, &owner, auth, headers, &[])
    }

    /// Where a redirect from `current` to `location` goes, and whether the
    /// `Authorization` stays on the request. It stays only while every hop
    /// has been the same origin (scheme, host and port) as `origin`: once a
    /// request leaves it, the header is gone for good, bouncing back
    /// included. A redirect from HTTPS to plain HTTP goes only to a host
    /// `owner` may send people to without TLS ([`Client::unverified_ok`]).
    fn hop(&self, owner: &str, origin: &str, current: &str, location: &str, keep_auth: bool) -> Result<(String, bool), ureq::Error> {
        if location.trim().is_empty() {
            return Err(ureq::Error::Io(io::Error::other(format!("{current} redirects without a Location"))));
        }
        let next = joined(current, location);
        let refuse = |why: &str| ureq::Error::Io(io::Error::other(format!("{current} redirects to {next}: {why}")));
        if !(is_https(&next) || next.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://"))) {
            return Err(refuse("not an http or https address"));
        }
        if is_https(current) && !is_https(&next) && !url_host(&next).is_some_and(|h| self.unverified_ok(&h, owner)) {
            return Err(refuse("https to http, to a host not listed in insecure-registries"));
        }
        let same = origin_of(&next).is_some() && origin_of(&next) == origin_of(origin);
        Ok((next, keep_auth && same))
    }

    /// GET `url` for a request that `owner` caused, following redirects by
    /// hand (no agent follows one itself): each hop on the agent its own
    /// host calls for (a blob redirected from a listed host to a storage
    /// server is fetched verified), `auth` only to the origin first asked
    /// (see [`Client::hop`]). `query` goes on the first request only.
    fn get_following(&self, url: &str, owner: &str, auth: Option<&str>, headers: &[(&str, String)], query: &[(&str, &str)]) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        self.following(false, url, owner, auth, headers, query)
    }

    /// [`Client::get_following`] for a HEAD.
    fn head_following(&self, url: &str, owner: &str, auth: Option<&str>) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        self.following(true, url, owner, auth, &[], &[])
    }

    fn following(&self, head: bool, url: &str, owner: &str, auth: Option<&str>, headers: &[(&str, String)], query: &[(&str, &str)]) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let origin = url.to_owned();
        let mut url = url.to_owned();
        let mut keep_auth = true;
        for hop in 0..10 {
            let agent = self.agent_for_hop(&url, owner);
            let mut req = if head { agent.head(&url) } else { agent.get(&url) };
            if hop == 0 {
                for (k, v) in query {
                    req = req.query(*k, *v);
                }
            }
            for (k, v) in headers {
                req = req.header(*k, v);
            }
            if let Some(a) = auth.filter(|_| keep_auth) {
                req = req.header("Authorization", a);
            }
            let mut resp = req.call()?;
            let redirect = is_redirect(resp.status().as_u16());
            let location = redirect.then(|| resp.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_owned)).flatten();
            let Some(location) = location else {
                resp.extensions_mut().insert(FinalUrl(url));
                return Ok(resp);
            };
            drain(resp);
            (url, keep_auth) = self.hop(owner, &origin, &url, &location, keep_auth)?;
        }
        Err(ureq::Error::TooManyRedirects)
    }

    /// Makes a request with registry auth: on a 401 it answers the challenge
    /// (a bearer token for `actions` from the realm, or Basic credentials),
    /// caches the result per repository, and tries once more.
    fn authed(
        &self,
        r: &Reference,
        actions: &str,
        call: impl Fn(Option<&str>) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<ureq::http::Response<ureq::Body>, Error> {
        let key = self.token_key(r, actions);
        let registry_base = self.base(r.api_host());
        let mut resp = None;
        for attempt in 0..2 {
            let header = Self::cached_token(&key).or_else(|| {
                self.credentials.as_ref().filter(|c| !c.registry_token.is_empty()).map(|c| format!("Bearer {}", c.registry_token))
            });
            let rsp = call(header.as_deref())?;
            // A 401 from anywhere but the registry's own origin (a storage
            // host a redirect or an upload address named) is not the
            // registry asking: its challenge is not answered with the
            // registry's credentials.
            let from = final_url(&rsp);
            let own = from.as_deref().is_some_and(|u| is_own_origin(u, &registry_base));
            if rsp.status().as_u16() == 401 && attempt == 0 && own {
                let plain_http = from.as_deref().is_some_and(is_http);
                let challenge = rsp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                drain(rsp);
                let (answer, ttl) = self.answer(&challenge, &format!("repository:{}:{actions}", r.repository), &r.registry, plain_http).map_err(|e| match e {
                    Error::Status { url, status, body } if url.is_empty() => Error::Status { url: self.base(r.api_host()), status, body },
                    e => e,
                })?;
                if let Some(c) = without_scope(&challenge) {
                    CHALLENGES.lock().unwrap_or_else(|e| e.into_inner()).insert(r.registry.clone(), c);
                }
                Self::remember_token(key.clone(), answer, ttl);
                continue;
            }
            resp = Some(rsp);
            break;
        }
        resp.ok_or_else(|| Error::Status { url: self.base(r.api_host()), status: 401, body: "unauthorized after token refresh".into() })
    }

    /// The `Authorization` header that answers a `WWW-Authenticate` challenge
    /// from `registry`, and how long it lasts.
    fn answer(&self, challenge: &str, default_scope: &str, registry: &str, plain_http: bool) -> Result<(String, Duration), Error> {
        if challenge.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("basic")) {
            let basic = self.credentials.as_ref().and_then(Credentials::basic).ok_or_else(|| Error::Status {
                url: String::new(),
                status: 401,
                body: "unauthorized: authentication required (docker login)".into(),
            })?;
            return Ok((basic, Duration::from_secs(300)));
        }
        let params = parse_challenge(challenge);
        let scope = params.get("scope").cloned().unwrap_or_else(|| default_scope.to_owned());
        let (token, ttl) = self.token(&params, Some(&scope), registry, plain_http)?;
        Ok((format!("Bearer {token}"), ttl))
    }

    /// A bearer token from the challenge's realm, as the logged-in user if
    /// any, and how long it lasts.
    fn token(&self, params: &HashMap<String, String>, scope: Option<&str>, registry: &str, plain_http: bool) -> Result<(String, Duration), Error> {
        let realm = params.get("realm").ok_or_else(|| Error::Http("registry sent no token realm".into()))?;
        let service = params.get("service").map(String::as_str).unwrap_or_default();
        // The registry names the realm, so a registry (or whoever answers for
        // it) could send the request anywhere, in the clear: a plain-HTTP
        // realm is asked only on the registry's own host and port when the
        // challenge itself arrived over plain HTTP (see `realm_is_secure`).
        if !self.realm_is_secure(realm, registry, plain_http) {
            return Err(Error::Http(format!("{registry} named the token service {realm}: plain HTTP is not asked")));
        }
        let creds = self.credentials.as_ref();
        let mut resp = if let Some(c) = creds.filter(|c| !c.identity_token.is_empty()) {
            // An identity token is an OAuth refresh token.
            let mut form = vec![("grant_type", "refresh_token"), ("refresh_token", c.identity_token.as_str()), ("service", service), ("client_id", "kiri")];
            if let Some(s) = scope {
                form.push(("scope", s));
            }
            self.agent_for_hop(realm, registry).post(realm.as_str()).send_form(form)?
        } else {
            let mut query: Vec<(&str, &str)> = Vec::new();
            if let Some(s) = scope {
                query.push(("scope", s));
            }
            if !service.is_empty() {
                query.push(("service", service));
            }
            let mut auth = None;
            if let Some(c) = creds {
                query.push(("account", &c.username));
                auth = c.basic();
            }
            // Redirects (an insecure token service may redirect) are followed
            // by the same helper as a registry's own, so credentials stay
            // with the realm's origin.
            self.get_following(realm, registry, auth.as_deref(), &[], &query)?
        };
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(Error::Status { url: realm.clone(), status, body: "unauthorized: incorrect username or password".into() });
        }
        if status != 200 {
            return Err(Error::Status { url: realm.clone(), status, body: String::new() });
        }
        let t: Token = serde_json::from_slice(&resp.body_mut().read_to_vec()?).map_err(|e| Error::Http(e.to_string()))?;
        // The distribution spec: 60 seconds when the server does not say.
        let ttl = Duration::from_secs(t.expires_in.filter(|&s| s > 0).unwrap_or(60));
        Ok((t.token.or(t.access_token).ok_or_else(|| Error::Http("token response had no token".into()))?, ttl))
    }

    /// `docker search`, against Docker Hub's search API.
    pub fn search(&self, term: &str, limit: u32) -> Result<Vec<serde_json::Value>, Error> {
        let url = "https://index.docker.io/v1/search";
        let mut resp = self.agent.get(url).query("q", term).query("n", limit.to_string()).config().max_redirects(5).https_only(true).build().call()?;
        if resp.status().as_u16() != 200 {
            return Err(Error::Status { url: url.to_owned(), status: resp.status().as_u16(), body: String::new() });
        }
        let body: serde_json::Value = serde_json::from_slice(&resp.body_mut().with_config().limit(4 << 20).read_to_vec()?).map_err(|e| Error::Http(e.to_string()))?;
        Ok(body["results"].as_array().cloned().unwrap_or_default())
    }

    /// A Docker Hub repository's tags, most recently pushed first:
    /// `postgres` gives `latest`, `17-alpine`, `17`… Hub's own API, since
    /// the registry's tag list is unordered and has no dates.
    pub fn hub_tags(&self, repo: &str, limit: u32) -> Result<Vec<String>, Error> {
        let repo = if repo.contains('/') { repo.to_owned() } else { format!("library/{repo}") };
        let url = format!("https://hub.docker.com/v2/repositories/{repo}/tags");
        let mut resp = self.agent.get(&url).query("page_size", limit.to_string()).query("ordering", "last_updated").config().max_redirects(5).https_only(true).build().call()?;
        if resp.status().as_u16() != 200 {
            return Err(Error::Status { url, status: resp.status().as_u16(), body: String::new() });
        }
        let body: serde_json::Value = serde_json::from_slice(&resp.body_mut().with_config().limit(4 << 20).read_to_vec()?).map_err(|e| Error::Http(e.to_string()))?;
        Ok(body["results"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect())
    }

    /// A repository's tags from the registry itself (`/v2/<repo>/tags/list`),
    /// for registries other than Docker Hub: GitHub's, ECR's. The list has
    /// no dates, so they are put newest version first by their names, and
    /// the signature and attestation tags (`sha256-….sig`) are left out.
    pub fn tags(&self, r: &Reference, limit: usize) -> Result<Vec<String>, Error> {
        // The list comes a page at a time, in name order: all of it is read
        // (up to `TAG_PAGES` pages) before the newest can be picked out.
        const TAG_PAGES: usize = 20;
        let base = self.base(r.api_host());
        let mut url = format!("{base}/v2/{}/tags/list?n=1000", r.repository);
        let mut tags: Vec<String> = Vec::new();
        for _ in 0..TAG_PAGES {
            let mut resp = self.get(r, &url, None)?;
            let next = resp.headers().get("link").and_then(|v| v.to_str().ok()).and_then(next_link).map(str::to_owned);
            let body: serde_json::Value = serde_json::from_slice(&resp.body_mut().with_config().limit(4 << 20).read_to_vec()?).map_err(|e| Error::Http(e.to_string()))?;
            tags.extend(body["tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).filter(|t| !t.starts_with("sha256-")).map(str::to_owned));
            // Only a next page on the same registry is followed.
            match next {
                Some(n) if n.starts_with("/v2/") => url = format!("{base}{n}"),
                _ => break,
            }
        }
        tags.sort_by(|a, b| version_order(b, a));
        tags.truncate(limit);
        Ok(tags)
    }

    /// What the registry serves for `r` right now, for `GET
    /// /distribution/{name}/json` (`docker manifest inspect`, and the update
    /// checks of Watchtower, Arcane, Portainer): the manifest's or index's
    /// descriptor, and the platforms it is for. Nothing is pulled; for a
    /// single-platform manifest its config is read for the platform.
    pub fn distribution(&self, r: &Reference) -> Result<serde_json::Value, Error> {
        let (bytes, media_type) = self.manifest(r, &r.reference).map_err(|e| not_found(e, r))?;
        if r.is_digest() && sha256_digest(&bytes) != r.reference {
            return Err(Error::Manifest("manifest failed digest check".into()));
        }
        let manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| Error::Manifest(e.to_string()))?;
        // The platform of a single manifest is in its config. It is only a
        // nicety: the descriptor is what update checks compare, so a config
        // that can't be had (a cache without the blob, a rate limit) leaves
        // the platforms empty instead of failing the answer, as on Docker.
        // Only a well-formed digest goes into the URL.
        let config = match manifest["config"]["digest"].as_str() {
            Some(digest) if manifest["manifests"].is_null() && crate::store::digest_hex(digest).is_ok() => {
                let url = format!("{}/v2/{}/blobs/{digest}", self.base(r.api_host()), r.repository);
                self.get(r, &url, None)
                    .ok()
                    .and_then(|mut resp| resp.body_mut().with_config().limit(4 << 20).read_to_vec().ok())
                    .filter(|body| sha256_digest(body) == digest)
                    .and_then(|body| serde_json::from_slice(&body).ok())
            }
            _ => None,
        };
        Ok(distribution_json(&bytes, &media_type, &manifest, config.as_ref()))
    }

    /// `docker login`: checks the credentials against the registry at
    /// `server` (`https://index.docker.io/v1/`, `ghcr.io`, `localhost:5000`).
    pub fn login(&self, server: &str) -> Result<(), Error> {
        let host = login_host(server)?;
        let host = host.as_str();
        let url = format!("{}/v2/", self.base(host));
        let resp = self.get_following(&url, host, None, &[], &[])?;
        match resp.status().as_u16() {
            200 => Ok(()),
            // A 401 from another origin (a redirect off the registry) is not
            // the registry asking: its challenge gets no credentials.
            401 if !final_url(&resp).is_some_and(|u| is_own_origin(&u, &url)) => {
                Err(Error::Status { url, status: 401, body: "unauthorized: the registry sent the request elsewhere".into() })
            }
            401 => {
                let plain_http = final_url(&resp).is_some_and(|u| is_http(&u));
                let challenge = resp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                if challenge.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("basic")) {
                    // Where the challenge came from: the registry moved up to
                    // HTTPS answers there, not at the plain-HTTP address. Any
                    // other origin is refused before credentials are made.
                    let at = final_url(&resp).and_then(|f| basic_login_target(&f, &url)).ok_or_else(|| Error::Status {
                        url: url.clone(),
                        status: 401,
                        body: "unauthorized: the registry sent the request elsewhere".into(),
                    })?;
                    let (auth, _) = self.answer(&challenge, "", host, plain_http)?;
                    let again = self.get_following(&at, host, Some(&auth), &[], &[])?;
                    return match again.status().as_u16() {
                        200 => Ok(()),
                        s => Err(Error::Status { url, status: s, body: "unauthorized: incorrect username or password".into() }),
                    };
                }
                // A token for no particular repository proves the password.
                self.token(&parse_challenge(&challenge), None, host, plain_http).map(|_| ())
            }
            s => Err(Error::Status { url, status: s, body: String::new() }),
        }
    }

    /// Uploads `image` as `target` (`docker push`): blobs the registry
    /// lacks, then a manifest. Returns the manifest's digest and size.
    pub fn push(&self, store: &Store, image: &Image, target: &Reference, progress: &dyn Fn(PushProgress)) -> Result<(String, u64), Error> {
        let base = self.base(target.api_host());
        let repo = &target.repository;
        // Every blob held until the manifest is up: the image removed
        // meanwhile must not take a blob away mid-push.
        let _held = crate::archive::hold(store, std::slice::from_ref(image))?;
        let config_size = std::fs::metadata(store.blob_path(&image.config_digest)?)?.len();
        let blobs: Vec<(String, u64)> =
            image.layers.iter().map(|l| (l.digest.clone(), l.size)).chain(std::iter::once((image.config_digest.clone(), config_size))).collect();
        for (digest, size) in &blobs {
            let url = format!("{base}/v2/{repo}/blobs/{digest}");
            let owner = url_host(&url).unwrap_or_default();
            // Redirects (to where the registry keeps the blob) are followed
            // hop by hop, as a GET's are: the blob is there only if the
            // last answer says so.
            let head = self.authed(target, "pull,push", |auth| self.head_following(&url, &owner, auth))?;
            if head.status().as_u16() == 200 {
                progress(PushProgress::Exists { digest: digest.clone() });
                continue;
            }
            progress(PushProgress::Uploading { digest: digest.clone(), size: *size });
            // Start an upload session, then send the blob in one PUT.
            let start_url = format!("{base}/v2/{repo}/blobs/uploads/");
            let started = self.authed(target, "pull,push", |auth| {
                let mut req = self.agent_for(&start_url).post(&start_url).header("Content-Length", "0");
                if let Some(a) = auth {
                    req = req.header("Authorization", a);
                }
                req.send_empty()
            })?;
            let started = ok(started, &start_url)?;
            let location = started.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().trim().to_owned();
            if location.is_empty() {
                return Err(Error::Http(format!("{start_url} started an upload without saying where to send it (no Location)")));
            }
            let location = joined(&start_url, &location);
            let put_url = format!("{location}{}digest={digest}", if location.contains('?') { '&' } else { '?' });
            // The registry's credentials go to the upload address only when
            // it is the registry's own origin; another host gets the blob and
            // none, and plain HTTP only where `insecure-registries` allows it.
            let own_origin = origin_of(&put_url).is_some() && origin_of(&put_url) == origin_of(&start_url);
            let owner = url_host(&start_url).unwrap_or_default();
            if !own_origin && is_https(&start_url) && !is_https(&put_url) && !url_host(&put_url).is_some_and(|h| self.unverified_ok(&h, &owner)) {
                return Err(Error::Http(format!("{start_url} sent the upload to {put_url}: https to http, to a host not listed in insecure-registries")));
            }
            let path = store.blob_path(digest)?;
            let put = self.authed(target, "pull,push", |auth| {
                let mut f = std::fs::File::open(&path).map_err(ureq::Error::Io)?;
                let mut req = self.agent_for_hop(&put_url, &owner).put(&put_url).header("Content-Type", "application/octet-stream").header("Content-Length", &size.to_string());
                if let Some(a) = auth.filter(|_| own_origin) {
                    req = req.header("Authorization", a);
                }
                req.send(ureq::SendBody::from_reader(&mut f))
            })?;
            ok(put, &put_url)?;
            progress(PushProgress::Pushed { digest: digest.clone() });
        }
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": image.config_digest, "size": config_size},
            "layers": image.layers.iter().map(|l| serde_json::json!({"mediaType": oci_layer_type(&l.media_type), "digest": l.digest, "size": l.size})).collect::<Vec<_>>(),
        });
        let bytes = serde_json::to_vec(&manifest).map_err(|e| Error::Manifest(e.to_string()))?;
        let url = format!("{base}/v2/{repo}/manifests/{}", target.reference);
        let put = self.authed(target, "pull,push", |auth| {
            let mut req = self.agent_for(&url).put(&url).header("Content-Type", OCI_MANIFEST);
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            req.send(&bytes[..])
        })?;
        ok(put, &url)?;
        Ok((sha256_digest(&bytes), bytes.len() as u64))
    }
}

/// The registry host a `docker login` server names: `https://index.docker.io/v1/`,
/// `ghcr.io`, `localhost:5000`, `HTTP://Registry.LAN:5000/`. Docker Hub's
/// names (any case, a trailing dot, its port) are its API host. Anything
/// that is not a plain `host[:port]` after the scheme and before the path,
/// a user name (`x@host`), a query or a fragment among them, is refused.
fn login_host(server: &str) -> Result<String, Error> {
    let text = server.trim();
    let lower = text.to_ascii_lowercase();
    let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")).unwrap_or(&lower);
    if rest.contains(['@', '?', '#', '\\']) {
        return Err(Error::Http(format!("not a registry address: {text}")));
    }
    let host = rest.split('/').next().unwrap_or_default();
    // No server at all is Docker Hub; a scheme or a path with no host is no
    // server.
    if lower.is_empty() || crate::config::is_hub(host) {
        return Ok("registry-1.docker.io".into());
    }
    if host.is_empty() {
        return Err(Error::Http(format!("not a registry address: {text}")));
    }
    crate::config::canonical(host).ok_or_else(|| Error::Http(format!("not a registry address: {text}")))
}

/// Text a remote server sent, safe to print: control characters (a
/// terminal's escape sequences among them) become spaces, and it is cut at
/// 200 characters.
fn printable(text: &str) -> String {
    let mut out: String = text.chars().take(200).map(|c| if c.is_control() { ' ' } else { c }).collect();
    if text.chars().nth(200).is_some() {
        out.push('…');
    }
    out
}

/// Parses `Bearer realm="…",service="…",scope="…"`.
fn parse_challenge(header: &str) -> HashMap<String, String> {
    let rest = header.strip_prefix("Bearer ").unwrap_or(header);
    let mut out = HashMap::new();
    let mut s = rest;
    while let Some(eq) = s.find('=') {
        let key = s[..eq].trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let after = &s[eq + 1..];
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                None => (quoted, ""),
            }
        } else {
            match after.find(',') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        out.insert(key, value.to_owned());
        s = next;
    }
    out
}

#[derive(Deserialize)]
struct Token {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<u64>,
}

/// A manifest resolved for one platform, and the registry that answered:
/// a mirror or Docker Hub itself. Its layers are downloaded from `target`.
struct Resolved {
    target: Reference,
    index_digest: Option<String>,
    digest: String,
    manifest: Manifest,
    media_type: String,
}

/// Either an index (has `manifests`) or an image manifest (has `config` and `layers`).
#[derive(Deserialize)]
struct Manifest {
    manifests: Option<Vec<IndexEntry>>,
    config: Option<Descriptor>,
    layers: Option<Vec<Descriptor>>,
}

#[derive(Deserialize)]
struct IndexEntry {
    digest: String,
    platform: Option<PlatformJson>,
}

#[derive(Deserialize)]
struct PlatformJson {
    os: String,
    architecture: String,
}

#[derive(Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    size: u64,
}

#[derive(Deserialize)]
struct ImageConfig {
    created: Option<String>,
    config: Option<ContainerConfig>,
    os: Option<String>,
    architecture: Option<String>,
}

#[derive(Deserialize, Default)]
struct ContainerConfig {
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,
    #[serde(rename = "User")]
    user: Option<String>,
}

/// Docker's answer to `GET /distribution/{name}/json`: the descriptor of
/// what the registry served (`bytes`), and its platforms, from the index's
/// entries (all of them, as Docker lists them) or, for a single manifest,
/// from its `config` blob. The manifest's own mediaType comes first, as
/// Docker trusts it over a registry's Content-Type.
fn distribution_json(bytes: &[u8], content_type: &str, manifest: &serde_json::Value, config: Option<&serde_json::Value>) -> serde_json::Value {
    let header = content_type.split(';').next().unwrap_or_default().trim();
    let media_type = manifest["mediaType"].as_str().filter(|m| !m.is_empty()).unwrap_or(header);
    let platform = |p: &serde_json::Value| {
        let text = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
        let mut out = serde_json::json!({"architecture": text("architecture"), "os": text("os")});
        for k in ["variant", "os.version"] {
            if let Some(v) = p[k].as_str().filter(|v| !v.is_empty()) {
                out[k] = v.into();
            }
        }
        if let Some(f) = p["os.features"].as_array().filter(|f| !f.is_empty()) {
            out["os.features"] = f.clone().into();
        }
        out
    };
    let platforms: Vec<serde_json::Value> = match manifest["manifests"].as_array() {
        Some(entries) => entries.iter().map(|m| platform(&m["platform"])).collect(),
        None => config.filter(|c| c["os"].is_string()).map(platform).into_iter().collect(),
    };
    serde_json::json!({
        "Descriptor": {"mediaType": media_type, "digest": sha256_digest(bytes), "size": bytes.len()},
        "Platforms": platforms,
    })
}

/// A registry answers a name that doesn't exist with 401 (Docker Hub hides
/// private repositories that way) and a missing tag with 404. Say it as
/// `docker pull` does, not as the raw registry reply; a wrong password stays as it is.
fn not_found(e: Error, r: &Reference) -> Error {
    match e {
        Error::Status { status: 401 | 403, ref body, .. } if !body.contains("incorrect username or password") => {
            let short = r.short();
            let repo = short.rsplit_once(if r.is_digest() { '@' } else { ':' }).map_or(short.as_str(), |(n, _)| n);
            Error::NotFound(format!("pull access denied for {repo}, repository does not exist or may require 'docker login'"))
        }
        Error::Status { status: 404, .. } => Error::NotFound(format!("manifest for {} not found: manifest unknown", r.short())),
        e => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header as clients send it: base64 of the JSON, URL-safe and
    /// unpadded like docker-java and the CLI.
    fn header(json: &str) -> String {
        crate::b64::encode(json.as_bytes()).replace('+', "-").replace('/', "_").trim_end_matches('=').to_owned()
    }

    /// docker-java serialises every field of its AuthConfig, the ones it
    /// does not use as JSON null; Docker reads null as empty (#654).
    #[test]
    fn credentials_read_null_fields_as_empty() {
        let java = r#"{"username":"me","password":"pw","serveraddress":"registry.example.com","identitytoken":null,"registrytoken":null,"email":null,"stackOrchestrator":null,"auth":null}"#;
        let c = Credentials::from_header(&header(java)).expect("username and password with null fields");
        assert_eq!((c.username.as_str(), c.password.as_str(), c.server_address.as_str()), ("me", "pw", "registry.example.com"));
        assert_eq!(c.basic().as_deref(), Some(format!("Basic {}", crate::b64::encode(b"me:pw")).as_str()));
        assert!(c.identity_token.is_empty() && c.registry_token.is_empty() && c.auth.is_empty());

        // Every field null: no sign-in, so the pull goes anonymous as before.
        let all = r#"{"username":null,"password":null,"auth":null,"serveraddress":null,"identitytoken":null,"registrytoken":null,"email":null}"#;
        assert!(Credentials::from_header(&header(all)).is_none());
        let c: Credentials = serde_json::from_str(all).expect("POST /auth body with every field null");
        assert!(c.username.is_empty() && c.server_address.is_empty());
        // A server address alone, the credential fields null: still no sign-in.
        let addr = r#"{"serveraddress":"registry.example.com","username":null,"password":null,"auth":null,"identitytoken":null,"registrytoken":null}"#;
        assert!(Credentials::from_header(&header(addr)).is_none());

        // Docker reads keys in any case, a null after a value leaves it, and a top-level null is empty.
        let c = Credentials::from_header(&header(r#"{"UserName":"me","PASSWORD":"pw","serverAddress":"r.example","identityToken":"tok","RegistryToken":"rt"}"#)).unwrap();
        assert_eq!((c.username.as_str(), c.password.as_str(), c.server_address.as_str(), c.identity_token.as_str(), c.registry_token.as_str()), ("me", "pw", "r.example", "tok", "rt"));
        let c = Credentials::from_header(&header(r#"{"username":"me","username":null,"password":"pw"}"#)).unwrap();
        assert_eq!(c.username, "me");
        let c: Credentials = serde_json::from_str("null").expect("top-level null is an empty config");
        assert!(c.username.is_empty() && c.server_address.is_empty());

        // Only a username and password.
        let c = Credentials::from_header(&header(r#"{"username":"me","password":"pw"}"#)).unwrap();
        assert_eq!((c.username.as_str(), c.password.as_str(), c.server_address.as_str()), ("me", "pw", ""));

        // A null server address is empty, and the rest is still used.
        let c = Credentials::from_header(&header(r#"{"username":"me","password":"pw","serveraddress":null}"#)).unwrap();
        assert_eq!((c.username.as_str(), c.server_address.as_str()), ("me", ""));

        // identitytoken null beside a password, and a real one with the rest null.
        let c = Credentials::from_header(&header(r#"{"username":"me","password":"pw","identitytoken":null}"#)).unwrap();
        assert_eq!((c.username.as_str(), c.identity_token.as_str()), ("me", ""));
        let c = Credentials::from_header(&header(r#"{"username":null,"password":null,"serveraddress":"ghcr.io","identitytoken":"tok"}"#)).unwrap();
        assert_eq!((c.username.as_str(), c.identity_token.as_str(), c.server_address.as_str()), ("", "tok", "ghcr.io"));
        let c = Credentials::from_header(&header(r#"{"username":null,"registrytoken":"bearer-tok","identitytoken":null}"#)).unwrap();
        assert_eq!(c.registry_token, "bearer-tok");

        // `auth` (base64 user:password) with the username null.
        let auth = format!(r#"{{"username":null,"password":null,"auth":"{}"}}"#, crate::b64::encode(b"me:pw"));
        let c = Credentials::from_header(&header(&auth)).unwrap();
        assert_eq!((c.username.as_str(), c.password.as_str()), ("me", "pw"));

        // Fields Kiri does not know are ignored, nulls, objects and all.
        let extra = r#"{"username":"me","password":"pw","email":null,"stackOrchestrator":null,"x":{"y":[1,null]},"n":3}"#;
        assert_eq!(Credentials::from_header(&header(extra)).unwrap().username, "me");
    }

    /// Later keys win whatever their case, a null after any spelling leaves
    /// the value, and near-miss keys are not fields.
    #[test]
    fn credentials_keys_in_any_case_last_wins() {
        let c: Credentials = serde_json::from_str(r#"{"serveraddress":"old","ServerAddress":"new","identitytoken":"old","IDENTITYTOKEN":"new"}"#).unwrap();
        assert_eq!((c.server_address.as_str(), c.identity_token.as_str()), ("new", "new"));
        let c: Credentials = serde_json::from_str(r#"{"ServerAddress":"old","serveraddress":"new","IdentityToken":"old","identitytoken":"new"}"#).unwrap();
        assert_eq!((c.server_address.as_str(), c.identity_token.as_str()), ("new", "new"));
        let c: Credentials = serde_json::from_str(r#"{"ServerAddress":"addr","serveraddress":null,"IdentityToken":"tok","identitytoken":null,"IDENTITYTOKEN":null}"#).unwrap();
        assert_eq!((c.server_address.as_str(), c.identity_token.as_str()), ("addr", "tok"));
        // An empty string is a value, so it clears an earlier one.
        let c: Credentials = serde_json::from_str(r#"{"serveraddress":"addr","ServerAddress":""}"#).unwrap();
        assert_eq!(c.server_address, "");
        let c: Credentials = serde_json::from_str(r#"{"user":"a","usernames":"b","user_name":"c","passwordx":"d","identity_token":"e","serveraddres":"f"}"#).unwrap();
        assert!(c.username.is_empty() && c.password.is_empty() && c.identity_token.is_empty() && c.server_address.is_empty());
        let long = format!(r#"{{"{}":"x","username":"me"}}"#, "u".repeat(1 << 16));
        assert_eq!(serde_json::from_str::<Credentials>(&long).unwrap().username, "me");
        assert!(Credentials::from_header(&header("null")).is_none());
    }

    /// Unknown fields are skipped however deep they go; a known field that
    /// is neither a string nor null, or a config that is not an object, is
    /// an error, as Docker's decoder says for X-Registry-Auth.
    #[test]
    fn credentials_unknown_skipped_wrong_types_refused() {
        let c: Credentials = serde_json::from_str(r#"{"Email":{"a":[{"b":{"c":[null,1,"s",true]}}]},"username":"me","Extra":[[],{}],"password":"pw"}"#).unwrap();
        assert_eq!((c.username.as_str(), c.password.as_str()), ("me", "pw"));
        for bad in [r#"{"username":1}"#, r#"{"Password":true}"#, r#"{"serveraddress":{}}"#, r#"{"IdentityToken":[]}"#, r#"{"auth":1.5}"#, r#"{"registrytoken":-1}"#, r#""me""#, "1", "true", "[]", r#"[{"username":"me"}]"#] {
            assert!(serde_json::from_str::<Credentials>(bad).is_err(), "{bad}");
            assert!(Credentials::from_header(&header(bad)).is_none(), "{bad}");
        }
    }

    /// An X-Registry-Config entry: what is not an object is empty, and a
    /// field of the wrong type is skipped, keeping the rest.
    #[test]
    fn loose_credentials_skip_what_does_not_fit() {
        for other in ["null", "1", "-1", "1.5", "true", r#""me""#, "[]", r#"[{"username":"me"}]"#] {
            let LooseCredentials(c) = serde_json::from_str(other).unwrap_or_else(|e| panic!("{other}: {e}"));
            assert!(c.username.is_empty() && c.password.is_empty() && c.auth.is_empty(), "{other}");
        }
        let LooseCredentials(c) = serde_json::from_str(r#"{"username":"me","username":1,"Password":"pw","password":{"x":[1]},"identitytoken":[],"auth":true,"x":{"y":null}}"#).unwrap();
        assert_eq!((c.username.as_str(), c.password.as_str(), c.identity_token.as_str(), c.auth.as_str()), ("me", "pw", "", ""));
        let LooseCredentials(c) = serde_json::from_str(r#"{"identitytoken":"old","IdentityToken":"new","identityToken":null}"#).unwrap();
        assert_eq!(c.identity_token, "new");
    }

    #[test]
    fn credentials_header_reads_every_base64_form() {
        // This password puts both a '+' and a '/' in the standard encoding, so URL-safe differs.
        for pad in ["", " ", "  "] {
            let json = format!(r#"{{"username":"me","password":"x?????>","serveraddress":null,"identitytoken":null}}{pad}"#);
            let std = crate::b64::encode(json.as_bytes());
            let url = std.replace('+', "-").replace('/', "_");
            for form in [std.clone(), std.trim_end_matches('=').to_owned(), url.clone(), url.trim_end_matches('=').to_owned()] {
                let c = Credentials::from_header(&form).unwrap_or_else(|| panic!("{form}"));
                assert_eq!((c.username.as_str(), c.password.as_str()), ("me", "x?????>"), "{form}");
            }
        }
        let e = crate::b64::encode(br#"{"username":"me","password":"x?????>","serveraddress":null,"identitytoken":null}"#);
        assert!(e.contains('+') && e.contains('/'));
    }

    #[test]
    fn credentials_header_empty_or_unreadable_is_none() {
        for bad in ["", "   ", "!!!", "Z", &header("not json"), &header("{"), &header("[]"), &header("null"), &header(r#"{"username":1}"#)] {
            assert!(Credentials::from_header(bad).is_none(), "{bad:?}");
        }
        // Empty strings, as the CLI sends when it sends them, are no sign-in either.
        assert!(Credentials::from_header(&header(r#"{"username":"","password":"","identitytoken":""}"#)).is_none());
    }

    #[test]
    fn a_missing_image_reads_as_docker_says_it() {
        let r = Reference::parse("gall").unwrap();
        let denied = Error::Status { url: "https://registry-1.docker.io/v2/library/gall/manifests/latest".into(), status: 401, body: r#"{"errors":[{"code":"UNAUTHORIZED"}]}"#.into() };
        assert_eq!(not_found(denied, &r).to_string(), "pull access denied for gall, repository does not exist or may require 'docker login'");
        let tag = Reference::parse("alpine:nope").unwrap();
        assert_eq!(not_found(Error::Status { url: String::new(), status: 404, body: String::new() }, &tag).to_string(), "manifest for alpine:nope not found: manifest unknown");
        let wrong = Error::Status { url: String::new(), status: 401, body: "unauthorized: incorrect username or password".into() };
        assert!(matches!(not_found(wrong, &r), Error::Status { status: 401, .. }));
        assert!(matches!(not_found(Error::Status { url: String::new(), status: 500, body: String::new() }, &r), Error::Status { status: 500, .. }));
    }

    #[test]
    fn distribution_answers_as_docker_does() {
        use serde_json::json;
        // An index: every entry's platform, attestations too, as Docker lists them.
        let index = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
            {"digest": "sha256:a", "platform": {"architecture": "amd64", "os": "linux"}},
            {"digest": "sha256:b", "platform": {"architecture": "arm64", "os": "linux", "variant": "v8"}},
            {"digest": "sha256:c", "platform": {"architecture": "unknown", "os": "unknown"}}]});
        let bytes = serde_json::to_vec(&index).unwrap();
        let got = distribution_json(&bytes, "application/vnd.oci.image.index.v1+json", &index, None);
        assert_eq!(got["Descriptor"], json!({"mediaType": "application/vnd.oci.image.index.v1+json", "digest": sha256_digest(&bytes), "size": bytes.len()}));
        assert_eq!(got["Platforms"], json!([{"architecture": "amd64", "os": "linux"}, {"architecture": "arm64", "os": "linux", "variant": "v8"}, {"architecture": "unknown", "os": "unknown"}]));
        // A single manifest: the platform comes from its config; its own
        // mediaType wins over a registry's generic Content-Type.
        let single = json!({"schemaVersion": 2, "mediaType": "application/vnd.docker.distribution.manifest.v2+json", "config": {"digest": "sha256:cfg"}});
        let bytes = serde_json::to_vec(&single).unwrap();
        let got = distribution_json(&bytes, "", &single, Some(&json!({"architecture": "arm64", "os": "linux", "rootfs": {}})));
        assert_eq!(got["Descriptor"]["mediaType"], "application/vnd.docker.distribution.manifest.v2+json");
        assert_eq!(got["Platforms"], json!([{"architecture": "arm64", "os": "linux"}]));
        assert_eq!(distribution_json(&bytes, "application/json; charset=utf-8", &single, None)["Platforms"], json!([]));
        assert_eq!(distribution_json(&bytes, "application/json; charset=utf-8", &single, None)["Descriptor"]["mediaType"], "application/vnd.docker.distribution.manifest.v2+json");
        let bare = json!({"schemaVersion": 2, "config": {"digest": "sha256:cfg"}});
        assert_eq!(distribution_json(&serde_json::to_vec(&bare).unwrap(), "application/vnd.oci.image.manifest.v1+json; x=1", &bare, None)["Descriptor"]["mediaType"], "application/vnd.oci.image.manifest.v1+json");
    }

    /// A loopback registry that answers by path: `answer` gives the status,
    /// Content-Type and body for each request; every path asked is kept.
    fn path_server(answer: impl Fn(&str) -> (u16, &'static str, Vec<u8>) + Send + 'static) -> (String, std::sync::Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        std::thread::spawn(move || {
            for c in listener.incoming() {
                let Ok(mut c) = c else { break };
                let mut first = [0u8; 1];
                if c.peek(&mut first).unwrap_or(0) == 1 && first[0] == 0x16 {
                    let _ = c.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && c.read(&mut b).unwrap_or(0) == 1 {
                    head.push(b[0]);
                }
                let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_owned();
                let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
                // The scheme probe a loopback registry gets: answered, not counted.
                if path == "/v2/" {
                    let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                    continue;
                }
                seen.lock().unwrap().push(path.clone());
                let (status, ct, body) = answer(&path);
                let _ = write!(c, "HTTP/1.1 {status} X\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = c.write_all(&body);
            }
        });
        (host, asked)
    }

    // The answer stands without the config: a cache missing the blob (or a
    // rate limit on it) leaves the platforms empty, and a malformed config
    // digest is never put in a URL.
    #[test]
    fn distribution_does_without_the_config() {
        let config = br#"{"architecture":"arm64","os":"linux"}"#.to_vec();
        let cfg_digest = sha256_digest(&config);
        for (case, digest, serve_config) in [("present", cfg_digest.clone(), true), ("missing", cfg_digest.clone(), false), ("malformed", "sha256:../../x?".to_owned(), true)] {
            let manifest = serde_json::to_vec(&serde_json::json!({"schemaVersion": 2, "mediaType": OCI_MANIFEST, "config": {"digest": digest}, "layers": []})).unwrap();
            let (m, c) = (manifest.clone(), config.clone());
            let (host, asked) = path_server(move |path| {
                if path.contains("/manifests/") {
                    (200, OCI_MANIFEST, m.clone())
                } else if path.contains("/blobs/") && serve_config {
                    (200, "application/octet-stream", c.clone())
                } else {
                    (404, "application/json", b"{}".to_vec())
                }
            });
            let r = Reference::parse(&format!("{host}/app:1")).unwrap();
            let got = test_client().distribution(&r).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(got["Descriptor"]["digest"], sha256_digest(&manifest), "{case}");
            assert_eq!(got["Descriptor"]["mediaType"], OCI_MANIFEST, "{case}");
            let platforms = if case == "present" { serde_json::json!([{"architecture": "arm64", "os": "linux"}]) } else { serde_json::json!([]) };
            assert_eq!(got["Platforms"], platforms, "{case}");
            let blobs = asked.lock().unwrap().iter().filter(|p| p.contains("/blobs/")).count();
            assert_eq!(blobs, usize::from(case != "malformed"), "{case}: blob requests");
        }
    }

    #[test]
    fn parses_docker_hub_challenge() {
        let p = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#,
        );
        assert_eq!(p["realm"], "https://auth.docker.io/token");
        assert_eq!(p["service"], "registry.docker.io");
        assert_eq!(p["scope"], "repository:library/alpine:pull");
    }

    #[test]
    fn a_remembered_challenge_drops_the_scope() {
        let hub = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#;
        assert_eq!(without_scope(hub).as_deref(), Some(DOCKER_HUB_CHALLENGE));
        assert_eq!(without_scope(r#"bearer realm="https://ghcr.io/token""#).as_deref(), Some(r#"Bearer realm="https://ghcr.io/token""#));
        assert_eq!(without_scope(r#"Basic realm="registry""#), None);
        assert_eq!(without_scope(r#"Bearer service="x""#), None);
    }

    #[test]
    fn tokens_are_used_a_little_less_long_than_they_last() {
        assert_eq!(token_lifetime(Duration::from_secs(300)), Duration::from_secs(270));
        assert_eq!(token_lifetime(Duration::from_secs(60)), Duration::from_secs(54));
        // A hostile expires_in is clamped, and remembering it cannot panic.
        assert_eq!(token_lifetime(Duration::from_secs(u64::MAX)), Duration::from_secs(3240));
        assert_eq!(token_lifetime(Duration::MAX), Duration::from_secs(3240));
        Client::remember_token("sec-test".into(), "Bearer x".into(), Duration::from_secs(u64::MAX));
        assert!(TOKENS.lock().unwrap().get("sec-test").is_some_and(|(_, until)| *until <= Instant::now() + MAX_TOKEN_TTL));
        let t: Token = serde_json::from_str(r#"{"token":"a","expires_in":300,"issued_at":"2026-10-30T06:00:00Z"}"#).unwrap();
        assert_eq!(t.expires_in, Some(300));
    }

    #[test]
    fn tokens_are_kept_apart_by_who_asked() {
        let r = Reference::parse("alpine").unwrap();
        let anon = Client { agent: shared_agent(), credentials: None, config: Default::default(), use_mirrors: true, mirror_scheme: None };
        let alice = Client { agent: shared_agent(), credentials: Some(Credentials { username: "alice".into(), password: "a".into(), ..Default::default() }), config: Default::default(), use_mirrors: true, mirror_scheme: None };
        let other = Client { agent: shared_agent(), credentials: Some(Credentials { username: "alice".into(), password: "b".into(), ..Default::default() }), config: Default::default(), use_mirrors: true, mirror_scheme: None };
        let keys = [anon.token_key(&r, "pull"), alice.token_key(&r, "pull"), other.token_key(&r, "pull"), alice.token_key(&r, "pull,push")];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert!(!alice.token_key(&r, "pull").contains("alice"), "the key holds a fingerprint, not the name");
        assert_eq!(anon.token_key(&r, "pull"), Client::new().token_key(&r, "pull"));
        // Cached manifests too: one fetched with credentials is not an
        // anonymous pull's, nor another account's.
        let d = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        assert_ne!(anon.manifest_key(&r, d), alice.manifest_key(&r, d));
        assert_ne!(alice.manifest_key(&r, d), other.manifest_key(&r, d));
        assert_eq!(anon.manifest_key(&r, d), Client::new().manifest_key(&r, d));
        assert_ne!(anon.manifest_key(&r, d), anon.manifest_key(&Reference::parse("busybox").unwrap(), d));
    }

    #[test]
    fn token_cache_expires() {
        let key = "test.invalid/expiring pull -".to_owned();
        Client::remember_token(key.clone(), "Bearer a".into(), Duration::from_secs(60));
        assert_eq!(Client::cached_token(&key).as_deref(), Some("Bearer a"));
        Client::remember_token(key.clone(), "Bearer b".into(), Duration::ZERO);
        assert_eq!(Client::cached_token(&key), None);
    }

    #[test]
    fn manifest_cache_keeps_only_what_the_digest_names() {
        let mut c = ManifestCache::default();
        let body = br#"{"schemaVersion":2}"#;
        let digest = sha256_digest(body);
        let key = format!("docker.io/library/alpine@{digest}");
        c.put(&key, &digest, b"something else", OCI_MANIFEST);
        assert!(c.get(&key).is_none());
        c.put(&key, &digest, body, OCI_MANIFEST);
        assert_eq!(c.get(&key), Some((body.to_vec(), OCI_MANIFEST.to_owned())));
        assert!(c.get(&format!("docker.io/library/other@{digest}")).is_none());
        c.put(&key, &digest, body, OCI_MANIFEST);
        assert_eq!(c.bytes, body.len());
        // Over the limit, it starts again rather than growing.
        let big = vec![b' '; ManifestCache::MAX_BYTES - 4];
        let big_digest = sha256_digest(&big);
        c.put(&big_digest, &big_digest, &big, OCI_MANIFEST);
        assert!(c.get(&key).is_none());
        assert_eq!(c.bytes, big.len());
    }

    /// A pull stopped before the scheme probe starts asks nothing at all:
    /// here a listener that would record the connection.
    #[test]
    fn a_stopped_pull_probes_no_scheme() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let cancel = Cancel::new();
        cancel.cancel();
        let scheme = {
            let _on = cancel.enter();
            insecure_scheme(&host)
        };
        assert_eq!(scheme, "https");
        std::thread::sleep(Duration::from_millis(200));
        assert!(listener.accept().is_err(), "a probe was sent");
        assert!(remembered_scheme(&host).is_none(), "nothing is remembered");
    }

    #[test]
    fn credentials_only_use_https_or_a_loopback_realm() {
        let c = Client::new();
        assert!(c.realm_is_secure("https://auth.example.com/token", "registry.example.com", false));
        assert!(c.realm_is_secure("https://auth.example.com/token", "registry.example.com", true));
        assert!(c.realm_is_secure("http://127.0.0.1:5000/token", "localhost:5000", true));
        assert!(c.realm_is_secure("http://[::1]:5000/token", "localhost:5000", true));
        assert!(!c.realm_is_secure("http://127.0.0.1:5000/token", "localhost:5000", false), "an HTTPS challenge gets no plain-HTTP realm");
        // A registry named without a port: on plain HTTP's port when the
        // challenge arrived over plain HTTP, and never when it arrived over HTTPS.
        assert!(!c.realm_is_secure("http://localhost/token", "127.0.0.1", false));
        assert!(c.realm_is_secure("http://localhost/token", "127.0.0.1", true));
        for (realm, registry) in [
            ("http://auth.example.com/token", "registry.example.com"),
            ("http://auth.example.com/token", "localhost:5000"),
            ("http://127.evil.com/token", "127.evil.com"),
            ("http://192.168.1.10/token", "localhost:5000"),
            // Another port on this machine, or a .localhost name a resolver
            // may send to DNS.
            ("http://127.0.0.1:8080/token", "localhost:5000"),
            ("http://localhost/token", "localhost:5000"),
            ("http://auth.localhost:5000/token", "localhost:5000"),
            // A .localhost registry is not this machine either.
            ("http://127.0.0.1:5000/token", "evil.localhost:5000"),
            ("http://localhost:5000/token", "evil.localhost:5000"),
            ("http://evil.localhost:5000/token", "evil.localhost:5000"),
            // A port that is not one matches nothing, not even another.
            ("http://127.0.0.1:0/token", "localhost:0"),
            ("http://localhost:/token", "localhost:x"),
        ] {
            assert!(!c.realm_is_secure(realm, registry, true), "{realm} for {registry}");
            assert!(!c.realm_is_secure(realm, registry, false), "{realm} for {registry}");
        }
        // A registry on this machine that answered HTTPS (on 443) gets no
        // plain-HTTP realm.
        assert!(!c.realm_is_secure("http://[::1]/token", "[::1]", false));
        assert!(!c.realm_is_secure("http://127.0.0.1/token", "[::1]", false));
        assert!(!c.realm_is_secure("http://127.0.0.1:443/token", "[::1]", false), "a registry that answered HTTPS gets no plain-HTTP realm");
        assert_eq!((named_port("h:5000"), named_port("h"), named_port("[::1]"), named_port("[::1]:80")), (Ok(Some(5000)), Ok(None), Ok(None), Ok(Some(80))));
        assert_eq!((named_port("h:"), named_port("h:0"), named_port("h:70000"), named_port("h:x")), (Err(()), Err(()), Err(()), Err(())));
        // A plain-HTTP realm gets them only when insecure-registries lists
        // its host (a registry on plain HTTP names its own address).
        let listed = Client::new().with_config(RegistryConfig { insecure: vec![crate::config::Insecure::parse("registry.lan").unwrap()], ..Default::default() });
        assert!(listed.realm_is_secure("http://registry.lan/service/token", "registry.lan", true));
        assert!(!listed.realm_is_secure("http://registry.lan/service/token", "registry.lan", false));
        assert!(!listed.realm_is_secure("http://auth.example.com/token", "registry.lan", true));
        assert!(!listed.realm_is_secure("http://registry.lan:8080/service/token", "registry.lan", true));
        assert!(!c.realm_is_secure("http://registry.lan/service/token", "registry.lan", true));
    }

    /// A registry on loopback serving one blob: `answer` gets each request's
    /// `Range` start (0 without one) and the connection, and writes the reply.
    fn blob_server(answer: impl Fn(u64, &mut std::net::TcpStream) + Send + Sync + 'static) -> (String, std::thread::JoinHandle<Vec<u64>>) {
        let answer = std::sync::Arc::new(answer);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let served = std::thread::spawn(move || {
            let mut seen = Vec::new();
            listener.set_nonblocking(true).unwrap();
            let end = Instant::now() + Duration::from_secs(20);
            while Instant::now() < end {
                let Ok((mut c, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                c.set_nonblocking(false).unwrap();
                // A TLS handshake on this plain-HTTP port is refused at once,
                // as Go's and nginx's servers refuse it.
                let mut first = [0u8; 1];
                if c.peek(&mut first).unwrap_or(0) == 1 && first[0] == 0x16 {
                    let _ = c.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && c.read(&mut b).unwrap_or(0) == 1 {
                    head.push(b[0]);
                }
                let head = String::from_utf8_lossy(&head).to_lowercase();
                if head.starts_with("quit") || head.is_empty() {
                    break;
                }
                // The scheme probe a loopback registry gets: answered, not counted.
                if head.starts_with("get /v2/ ") {
                    let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                    continue;
                }
                let from = head.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|r| r.trim_end_matches('-').parse().ok()).unwrap_or(0);
                seen.push(from);
                // Each on its own, so a stalled answer does not hold up the next.
                let answer = answer.clone();
                std::thread::spawn(move || answer(from, &mut c));
            }
            seen
        });
        (host, served)
    }

    fn quit(host: &str) {
        let _ = std::net::TcpStream::connect(host).and_then(|mut c| c.write_all(b"QUIT\r\n\r\n"));
    }

    fn test_client() -> Client {
        let cfg = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build();
        let agent = ureq::Agent::with_parts(cfg, crate::connect::connector_with(Duration::from_millis(300)), ureq::unversioned::resolver::DefaultResolver::default());
        Client { agent, credentials: None, config: Default::default(), use_mirrors: true, mirror_scheme: None }
    }

    /// A registry that takes every push: blobs are missing until put, and
    /// it records what it was sent, by digest. `QUIT` ends it.
    fn push_server() -> (String, std::thread::JoinHandle<HashMap<String, Vec<u8>>>) {
        push_server_with("/v2/t/blobs/uploads/1")
    }

    /// [`push_server`] that starts each upload with `location` as its
    /// `Location` (none at all when empty), and records each blob's upload
    /// path too, as `path:<path>`.
    fn push_server_with(location: &'static str) -> (String, std::thread::JoinHandle<HashMap<String, Vec<u8>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let served = std::thread::spawn(move || {
            let mut blobs = HashMap::new();
            'conns: for c in listener.incoming() {
                let c = c.unwrap();
                // A TLS handshake on this plain-HTTP port is refused at once.
                let mut first = [0u8; 1];
                if c.peek(&mut first).unwrap_or(0) == 1 && first[0] == 0x16 {
                    let _ = std::io::Write::write_all(&mut &c, b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let mut c = std::io::BufReader::new(c);
                loop {
                    let mut head = String::new();
                    let mut line = String::new();
                    while line != "\r\n" {
                        line.clear();
                        if std::io::BufRead::read_line(&mut c, &mut line).unwrap_or(0) == 0 {
                            continue 'conns;
                        }
                        head.push_str(&line);
                    }
                    let first = head.lines().next().unwrap_or_default().to_owned();
                    if first.starts_with("QUIT") {
                        break 'conns;
                    }
                    let len: usize = head.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length: ").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                    let mut body = vec![0u8; len];
                    std::io::Read::read_exact(&mut c, &mut body).unwrap();
                    let path = first.split(' ').nth(1).unwrap_or_default().to_owned();
                    // The scheme probe goes through another agent: its
                    // connection ends here, so the next one is served.
                    if first.starts_with("GET /v2/ ") {
                        let _ = std::io::Write::write_all(c.get_mut(), b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                        continue 'conns;
                    }
                    let reply = if first.starts_with("HEAD") {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_owned()
                    } else if first.starts_with("POST") {
                        let at = if location.is_empty() { String::new() } else { format!("Location: {location}\r\n") };
                        format!("HTTP/1.1 202 Accepted\r\n{at}Content-Length: 0\r\n\r\n")
                    } else {
                        if !path.contains("/manifests/") {
                            blobs.insert(format!("path:{}", path.split('?').next().unwrap_or_default()), Vec::new());
                            blobs.insert(path.split("digest=").nth(1).unwrap_or_default().to_owned(), body);
                        }
                        "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n".to_owned()
                    };
                    std::io::Write::write_all(c.get_mut(), reply.as_bytes()).unwrap();
                }
            }
            blobs
        });
        (host, served)
    }

    /// The image removed while it is being pushed (its tag deleted by
    /// another client): the push still sends every blob it began with.
    #[test]
    fn a_push_keeps_its_blobs_while_the_image_is_removed() {
        use std::io::Write as _;
        let root = std::env::temp_dir().join(format!("kiri-push-rm-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let put = |bytes: &[u8]| {
            let mut w = store.ingest().unwrap();
            w.write_all(bytes).unwrap();
            w.finish().unwrap()
        };
        let (layer, size) = put(b"\x1f\x8bnot really gzip");
        let config = serde_json::json!({"os": "linux", "architecture": "amd64", "config": {}});
        let (cfg, _) = put(&serde_json::to_vec(&config).unwrap());
        let image = crate::archive::from_config("gone:1".into(), &cfg, &config, vec![Layer { digest: layer.clone(), size, media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into() }]);
        store.put_image(image.clone()).unwrap();
        let (host, served) = push_server();
        let target = Reference::parse(&format!("{host}/t:1")).unwrap();
        let removed = std::sync::atomic::AtomicBool::new(false);
        let pushed = test_client().push(&store, &image, &target, &|_| {
            if !removed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                store.remove_image("gone:1").unwrap();
            }
        });
        quit(&host);
        let blobs = served.join().unwrap();
        pushed.unwrap();
        assert!(blobs.contains_key(&layer), "the layer went up");
        assert!(blobs.contains_key(&cfg), "the config went up");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A one-layer image in a fresh store under `name`.
    fn push_image(name: &str) -> (std::path::PathBuf, Store, Image) {
        use std::io::Write as _;
        let root = std::env::temp_dir().join(format!("kiri-push-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let put = |bytes: &[u8]| {
            let mut w = store.ingest().unwrap();
            w.write_all(bytes).unwrap();
            w.finish().unwrap()
        };
        let (layer, size) = put(b"\x1f\x8bnot really gzip");
        let config = serde_json::json!({"os": "linux", "architecture": "amd64", "config": {}});
        let (cfg, _) = put(&serde_json::to_vec(&config).unwrap());
        let image = crate::archive::from_config(format!("{name}:1"), &cfg, &config, vec![Layer { digest: layer, size, media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into() }]);
        store.put_image(image.clone()).unwrap();
        (root, store, image)
    }

    /// An upload started with a relative `Location` is sent under the
    /// upload path it was started at, as RFC 3986 joins it.
    #[test]
    fn a_relative_upload_location_stays_under_the_upload_path() {
        let (root, store, image) = push_image("rel");
        let (host, served) = push_server_with("7f3a");
        let target = Reference::parse(&format!("{host}/t:1")).unwrap();
        let pushed = test_client().push(&store, &image, &target, &|_| {});
        quit(&host);
        let blobs = served.join().unwrap();
        pushed.unwrap();
        assert!(blobs.contains_key("path:/v2/t/blobs/uploads/7f3a"), "{:?}", blobs.keys().collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An upload started without a `Location` has nowhere to go: the push
    /// stops there, and no blob is sent anywhere.
    #[test]
    fn an_upload_without_a_location_is_refused() {
        let (root, store, image) = push_image("noloc");
        let (host, served) = push_server_with("");
        let target = Reference::parse(&format!("{host}/t:1")).unwrap();
        let pushed = test_client().push(&store, &image, &target, &|_| {});
        quit(&host);
        let blobs = served.join().unwrap();
        assert!(pushed.is_err());
        assert!(blobs.is_empty(), "{:?}", blobs.keys().collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn blob() -> (Vec<u8>, String) {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let digest = sha256_digest(&data);
        (data, digest)
    }

    #[test]
    fn a_stalled_blob_resumes_where_it_stopped() {
        let (data, digest) = blob();
        let half = data.len() / 2;
        let body = data.clone();
        let (host, served) = blob_server(move |from, c| {
            if from == 0 {
                // Half the blob, then silence with the connection open.
                let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = c.write_all(&body[..half]);
                std::thread::sleep(Duration::from_secs(2));
            } else {
                let rest = &body[from as usize..];
                let _ = write!(c, "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\n\r\n", rest.len(), body.len() - 1, body.len());
                let _ = c.write_all(rest);
            }
        });
        let root = std::env::temp_dir().join(format!("kiri-stall-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let r = Reference::parse(&format!("{host}/test/blob:1")).unwrap();
        let started = Instant::now();
        let got = test_client().download(&store, &r, &digest, &|_| {}).unwrap();
        assert_eq!(got, data.len() as u64);
        assert!(store.has_blob(&digest, data.len() as u64));
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        quit(&host);
        assert_eq!(served.join().unwrap(), [0, half as u64]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_server_without_ranges_sends_the_blob_again() {
        let (data, digest) = blob();
        let body = data.clone();
        let tries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (host, served) = blob_server(move |_, c| {
            let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            // The first connection drops a third of the way in.
            if tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                let _ = c.write_all(&body[..body.len() / 3]);
                let _ = c.shutdown(std::net::Shutdown::Both);
            } else {
                let _ = c.write_all(&body);
            }
        });
        let root = std::env::temp_dir().join(format!("kiri-norange-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let r = Reference::parse(&format!("{host}/test/blob:1")).unwrap();
        assert_eq!(test_client().download(&store, &r, &digest, &|_| {}).unwrap(), data.len() as u64);
        assert!(store.has_blob(&digest, data.len() as u64));
        quit(&host);
        let seen = served.join().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_blob_that_never_arrives_gives_up() {
        let (_, digest) = blob();
        let (host, served) = blob_server(|_, c| {
            let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n");
            std::thread::sleep(Duration::from_millis(400));
        });
        let root = std::env::temp_dir().join(format!("kiri-dead-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let r = Reference::parse(&format!("{host}/test/blob:1")).unwrap();
        assert!(test_client().download(&store, &r, &digest, &|_| {}).is_err());
        quit(&host);
        assert_eq!(served.join().unwrap().len(), BLOB_TRIES as usize);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cancelled_download_stops_at_once_and_leaves_nothing() {
        let (data, digest) = blob();
        let body = data.clone();
        let (host, served) = blob_server(move |_, c| {
            // Half the blob, then silence with the connection open.
            let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = c.write_all(&body[..body.len() / 2]);
            std::thread::sleep(Duration::from_secs(5));
        });
        let root = std::env::temp_dir().join(format!("kiri-cancel-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let r = Reference::parse(&format!("{host}/test/blob:1")).unwrap();
        // Stalls only give up after 10 s: the cancel must not wait for that.
        let cfg = ureq::Agent::config_builder().http_status_as_error(false).build();
        let agent = ureq::Agent::with_parts(cfg, crate::connect::connector_with(Duration::from_secs(10)), ureq::unversioned::resolver::DefaultResolver::default());
        let client = Client { agent, credentials: None, config: Default::default(), use_mirrors: true, mirror_scheme: None };
        // A loopback registry's scheme is asked once (HTTPS, then HTTP):
        // done before the clock for the cancel starts.
        let _ = client.base(&host);
        let cancel = Cancel::new();
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            c.cancel();
        });
        let started = Instant::now();
        let result = {
            let _on = cancel.enter();
            client.download_with(&store, &r, &digest, u64::MAX, None, &|_| {}, &cancel)
        };
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert!(!store.has_blob(&digest, data.len() as u64));
        assert_eq!(std::fs::read_dir(root.join("tmp")).unwrap().count(), 0, "the partial blob is removed");
        quit(&host);
        assert_eq!(served.join().unwrap(), [0]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_next_page_of_tags_comes_from_the_link_header() {
        assert_eq!(next_link(r#"</v2/o/r/tags/list?last=1.9&n=1000>; rel="next""#), Some("/v2/o/r/tags/list?last=1.9&n=1000"));
        assert_eq!(next_link(r#"<https://elsewhere.example/x>; rel="prev""#), None);
    }

    #[test]
    fn registry_tags_are_newest_version_first() {
        let mut tags = ["1.9", "1.10", "1.2-rc1", "latest", "1.2", "1.2.1", "main"].map(String::from).to_vec();
        tags.sort_by(|a, b| version_order(b, a));
        assert_eq!(tags, ["1.10", "1.9", "1.2.1", "1.2", "1.2-rc1", "main", "latest"]);
    }

    #[test]
    fn transient_failures_are_the_network_and_5xx() {
        assert!(transient(&Error::Io(io::Error::other(ureq::Error::Timeout(ureq::Timeout::RecvBody)))));
        assert!(transient(&Error::Io(io::ErrorKind::ConnectionReset.into())));
        assert!(transient(&Error::Status { url: String::new(), status: 503, body: String::new() }));
        assert!(!transient(&Error::Status { url: String::new(), status: 404, body: String::new() }));
        assert!(!transient(&Error::Io(io::Error::new(io::ErrorKind::InvalidData, "digest mismatch"))));
        assert!(!transient(&Error::Io(io::Error::other(ureq::Error::BodyExceedsLimit(10)))));
    }

    #[test]
    fn mirrors_are_tried_in_order_and_only_for_docker_hub() {
        let hub = Reference::parse("nginx:1.27").unwrap();
        let client = Client::new().with_config(RegistryConfig {
            mirrors: vec![Mirror::parse("mirror.example.com").unwrap(), Mirror::parse("mirror.example.net/docker.io").unwrap()],
            ..Default::default()
        });
        let targets = client.targets(&hub);
        let hosts: Vec<&str> = targets.iter().map(|(r, _)| r.registry.as_str()).collect();
        assert_eq!(hosts, ["mirror.example.com", "mirror.example.net", "docker.io"]);
        assert_eq!(targets[1].0.repository, "docker.io/library/nginx");
        assert_eq!(targets[2].0.repository, "library/nginx");
        assert_eq!(targets[2].0.reference, "1.27");

        let ghcr = Reference::parse("ghcr.io/o/r:v1").unwrap();
        assert_eq!(client.targets(&ghcr), vec![(ghcr.clone(), None)]);
    }

    #[test]
    fn a_mirror_serves_the_image_under_the_original_name() {
        let (mirror, mirror_seen) = image_server(200);
        let (origin, origin_seen) = image_server(200);
        let root = std::env::temp_dir().join(format!("kiri-mirror-hit-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let name = Reference::parse("alpine:3.22").unwrap();
        let targets = vec![(name.via(&mirror, None), None), (name.via(&origin, None), None)];
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        let image = test_client().pull_from(&store, &name, &targets, &platform, &|_| {}, &Cancel::new()).unwrap();
        assert_eq!(image.name, "alpine:3.22", "the mirror's name never shows");
        assert_eq!(image.layers.len(), 1);
        quit(&mirror);
        quit(&origin);
        assert!(mirror_seen.join().unwrap().iter().any(|p| p.contains("/manifests/")));
        assert!(origin_seen.join().unwrap().is_empty(), "the origin is not asked when the mirror has it");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_mirror_is_never_sent_the_docker_hub_sign_in() {
        let (mirror, mirror_seen) = image_server(200);
        let root = std::env::temp_dir().join(format!("kiri-mirror-creds-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let name = Reference::parse("alpine:3.22").unwrap();
        let targets = vec![(name.via(&mirror, None), None), (name.clone(), None)];
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        let signed_in = test_client().with_credentials(Some(Credentials { username: "alice".into(), password: "hub-password".into(), registry_token: "hub-token".into(), ..Default::default() }));
        let image = signed_in.pull_from(&store, &name, &targets, &platform, &|_| {}, &Cancel::new()).unwrap();
        assert_eq!(image.name, "alpine:3.22");
        quit(&mirror);
        let seen = mirror_seen.join().unwrap();
        assert!(seen.iter().any(|p| p.contains("/blobs/")), "the layers came from the mirror: {seen:?}");
        let sent: Vec<&String> = seen.iter().filter(|p| p.ends_with("(with credentials)")).collect();
        assert!(sent.is_empty(), "the mirror was sent the Docker Hub sign-in: {sent:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_mirror_without_the_image_falls_back_to_the_registry() {
        let (mirror, mirror_seen) = image_server(404);
        let (origin, origin_seen) = image_server(200);
        let root = std::env::temp_dir().join(format!("kiri-mirror-miss-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let name = Reference::parse("alpine:3.22").unwrap();
        let targets = vec![(name.via(&mirror, None), None), (name.via(&origin, None), None)];
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        let image = test_client().pull_from(&store, &name, &targets, &platform, &|_| {}, &Cancel::new()).unwrap();
        assert_eq!(image.name, "alpine:3.22");
        quit(&mirror);
        quit(&origin);
        assert!(mirror_seen.join().unwrap().iter().any(|p| p.contains("/manifests/")));
        assert!(origin_seen.join().unwrap().iter().any(|p| p.contains("/manifests/")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cancelled_pull_does_not_try_the_next_registry() {
        let (mirror, mirror_seen) = image_server(200);
        let (origin, origin_seen) = image_server(200);
        let root = std::env::temp_dir().join(format!("kiri-mirror-cancel-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let name = Reference::parse("alpine:3.22").unwrap();
        let targets = vec![(name.via(&mirror, None), None), (name.via(&origin, None), None)];
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        let cancel = Cancel::new();
        cancel.cancel();
        let result = test_client().pull_from(&store, &name, &targets, &platform, &|_| {}, &cancel);
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        quit(&mirror);
        quit(&origin);
        assert!(mirror_seen.join().unwrap().is_empty());
        assert!(origin_seen.join().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// insecure-registries: a listed host that only speaks plain HTTP is
    /// pulled from over HTTP (after HTTPS, which it refuses), and the same
    /// host unlisted is not. 0.0.0.0 reaches this machine without being a
    /// loopback name, so it stands for a registry elsewhere on the network.
    #[cfg(unix)]
    #[test]
    fn a_listed_plain_http_registry_is_pulled_and_an_unlisted_one_is_not() {
        let (host, seen) = image_server(200);
        let host = host.replace("127.0.0.1", "0.0.0.0");
        let root = std::env::temp_dir().join(format!("kiri-insecure-http-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let name = Reference::parse(&format!("{host}/team/app:1")).unwrap();
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        let refused = test_client().pull(&store, &name, &platform, &|_| {});
        assert!(refused.is_err(), "pulled from an unlisted plain-HTTP registry");
        let listed = test_client().with_config(RegistryConfig { insecure: vec![crate::config::Insecure::parse(&host).unwrap()], ..Default::default() });
        let image = listed.pull(&store, &name, &platform, &|_| {}).unwrap();
        assert_eq!(image.name, format!("{host}/team/app:1"));
        quit(&host.replace("0.0.0.0", "127.0.0.1"));
        assert!(seen.join().unwrap().iter().any(|p| p.contains("/blobs/")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only HTTPS to a listed host goes without a checked certificate; a
    /// redirect from it to a storage server, any other host, and plain
    /// HTTP use the verified agent.
    #[test]
    fn only_a_listed_host_skips_the_certificate_check() {
        let c = test_client().with_config(RegistryConfig { insecure: vec![crate::config::Insecure::parse("registry.lan:5000").unwrap()], ..Default::default() });
        let unverified = |url: &str| c.agent_for(url).config().tls_config().disable_verification();
        assert!(unverified("https://registry.lan:5000/v2/"));
        assert!(!unverified("https://storage.example.com/blob"));
        assert!(!unverified("https://registry.lan:5001/v2/"));
        assert!(!unverified("http://registry.lan:5000/v2/"));
        assert!(!Client::new().agent_for("https://registry.lan:5000/v2/").config().tls_config().disable_verification());
        // And the insecure agent leaves redirects to send_get.
        assert_eq!(c.agent_for("https://registry.lan:5000/v2/").config().max_redirects(), 0);
    }

    /// Where a registry's own requests may go unverified: a name under
    /// `.localhost` is not this machine, so a registry on this machine does
    /// not send them there, and one so named is not insecure at all.
    #[test]
    fn a_dot_localhost_name_is_not_this_machine_for_hops() {
        let c = test_client();
        assert!(c.unverified_ok("127.0.0.1:5000", "localhost:5000"));
        assert!(!c.unverified_ok("evil.localhost:5000", "localhost:5000"));
        assert!(!c.unverified_ok("localhost:5000", "evil.localhost:5000"));
        assert!(!c.unverified_ok("evil.localhost:5000", "evil.localhost:5000"));
    }

    /// A registry covered by a network may send its own requests back to
    /// itself however it spells its address: `[fd00:0::1]` is `[fd00::1]`.
    #[test]
    fn a_registry_is_its_own_host_in_any_spelling() {
        let c = test_client().with_config(RegistryConfig { insecure: vec![crate::config::Insecure::parse("fd00::/8").unwrap()], mirrors: Vec::new() });
        assert!(c.unverified_ok("[fd00::1]:5000", "[fd00:0::1]:5000"));
        assert!(c.unverified_ok("[FD00::1]:5000", "[fd00::1]:5000"));
        assert!(!c.unverified_ok("[fd00::2]:5000", "[fd00::1]:5000"), "another address in the network is not the registry");
    }

    /// A listed registry named without a port, off this machine, is held
    /// to the same port rule as one on it: a plain-HTTP realm on port 80 is
    /// its own only when the challenge arrived over plain HTTP, never after
    /// an HTTPS challenge.
    #[test]
    fn a_listed_registrys_plain_http_realm_is_on_its_own_port() {
        let listed = |e: &[&str]| test_client().with_config(RegistryConfig { insecure: e.iter().map(|e| crate::config::Insecure::parse(e).unwrap()).collect(), mirrors: Vec::new() });
        let c = listed(&["realm-https.lan", "realm-http.lan", "realm-port.lan:5000"]);
        assert!(!c.realm_is_secure("http://realm-https.lan/token", "realm-https.lan", false));
        assert!(!c.realm_is_secure("http://realm-https.lan:443/token", "realm-https.lan", false));
        assert!(c.realm_is_secure("http://realm-http.lan/token", "realm-http.lan", true));
        assert!(c.realm_is_secure("http://realm-http.lan:80/token", "realm-http.lan", true));
        assert!(!c.realm_is_secure("http://realm-http.lan:8080/token", "realm-http.lan", true));
        assert!(c.realm_is_secure("http://realm-port.lan:5000/token", "realm-port.lan:5000", true));
        assert!(!c.realm_is_secure("http://realm-port.lan:5000/token", "realm-port.lan:5000", false));
        assert!(!c.realm_is_secure("http://realm-port.lan/token", "realm-port.lan:5000", true));
        assert!(!c.realm_is_secure("http://realm-port.lan:5001/token", "realm-port.lan:5000", true));
        // A network entry, named by address: the same rule.
        let net = listed(&["10.0.0.0/8"]);
        assert!(!net.realm_is_secure("http://10.9.8.7/token", "10.9.8.7", false));
        assert_eq!((without_port("[::1]:80"), without_port("[::1]"), without_port("h.lan:5000"), without_port("h.lan")), ("[::1]", "[::1]", "h.lan", "h.lan"));
    }

    /// A plain-HTTP token realm is allowed only when the request that got
    /// the 401 was itself plain HTTP: an HTTPS 401 on an explicit port
    /// refuses a plain-HTTP realm even if the probe cache entry has expired,
    /// an HTTP request upgraded to HTTPS before the 401 refuses it too, and
    /// a genuine plain-HTTP 401 still works when the cache entry is gone.
    #[test]
    fn a_plain_http_realm_requires_a_plain_http_challenge() {
        // 1. Self-signed HTTPS registry on an explicit port: cache expires
        // before the 401 arrives during login and during pull (authed).
        let at_host: Arc<std::sync::OnceLock<String>> = Arc::default();
        let h1 = at_host.clone();
        let (host, seen) = dual_recording_server(move |scheme, head| {
            let host = h1.get().unwrap();
            if scheme == "https" && head.starts_with("GET /v2/") {
                SCHEMES.lock().unwrap().remove(host);
                format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://{host}/token\",service=\"r\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"tok\"}".into()
            }
        });
        at_host.set(host.clone()).unwrap();
        let c = test_client().with_credentials(me());
        remember_scheme(host.clone(), "https", Duration::from_secs(60));
        assert!(c.login(&host).is_err(), "HTTPS login 401 must refuse a plain-HTTP realm");
        let root = std::env::temp_dir().join(format!("kiri-realm-https-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        remember_scheme(host.clone(), "https", Duration::from_secs(60));
        let r = Reference::parse(&format!("{host}/team/app:1")).unwrap();
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        assert!(c.pull(&store, &r, &platform, &|_| {}).is_err(), "HTTPS pull 401 must refuse a plain-HTTP realm");
        let _ = std::fs::remove_dir_all(&root);
        let plain_reqs: Vec<_> = seen.lock().unwrap().iter().filter(|(s, _)| *s == "http").cloned().collect();
        assert!(plain_reqs.is_empty(), "sent credentials to plain-HTTP realm after HTTPS 401: {plain_reqs:?}");

        // 2. Plain HTTP upgraded to HTTPS before the 401: the 401 arrived
        // over HTTPS, so a plain-HTTP realm is refused even while the probe
        // cache still says http.
        let up_host: Arc<std::sync::OnceLock<String>> = Arc::default();
        let h2 = up_host.clone();
        let (up, up_seen) = dual_recording_server(move |scheme, head| {
            let host = h2.get().unwrap();
            if scheme == "http" && head.starts_with("GET /v2/") {
                redirect(&format!("https://{host}/v2/"))
            } else if scheme == "https" && head.starts_with("GET /v2/") {
                format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://{host}/token\",service=\"r\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"tok\"}".into()
            }
        });
        up_host.set(up.clone()).unwrap();
        remember_scheme(up.clone(), "http", Duration::from_secs(60));
        assert!(c.login(&up).is_err(), "HTTP->HTTPS upgraded 401 must refuse a plain-HTTP realm");
        assert!(!up_seen.lock().unwrap().iter().any(|(_, h)| h.starts_with("GET /token")), "asked /token after upgrade: {:?}", up_seen.lock().unwrap());

        // 3. Genuine plain-HTTP 401: still works when the probe cache expires
        // before the 401 arrives.
        let p_host: Arc<std::sync::OnceLock<String>> = Arc::default();
        let h3 = p_host.clone();
        let (plain, plain_seen) = recording_server(move |head| {
            let host = h3.get().unwrap();
            if head.starts_with("GET /token") {
                "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"tok\"}".into()
            } else {
                SCHEMES.lock().unwrap().remove(host);
                format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://{host}/token\",service=\"r\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            }
        });
        p_host.set(plain.clone()).unwrap();
        remember_scheme(plain.clone(), "http", Duration::from_secs(60));
        assert!(c.login(&plain).is_ok(), "plain-HTTP 401 should still be allowed");
        assert!(plain_seen.lock().unwrap().iter().any(|h| h.starts_with("GET /token")));
    }

    /// `warm_up` never answers a plain-HTTP token realm speculatively from
    /// cached probe state (where the registry may now redirect to HTTPS or
    /// serve HTTPS-only); HTTPS realms still prefetch.
    #[test]
    fn warm_up_never_answers_a_plain_http_realm_speculatively() {
        let p_host: Arc<std::sync::OnceLock<String>> = Arc::default();
        let h1 = p_host.clone();
        let (plain, plain_seen) = recording_server(move |head| {
            if head.starts_with("GET /token") {
                "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"tok\"}".into()
            } else {
                redirect(&format!("https://{}/v2/", h1.get().unwrap()))
            }
        });
        p_host.set(plain.clone()).unwrap();
        remember_scheme(plain.clone(), "http", Duration::from_secs(60));
        CHALLENGES.lock().unwrap().insert(plain.clone(), format!(r#"Bearer realm="http://{plain}/token",service="r""#));
        let c = test_client().with_credentials(me());
        let r = Reference::parse(&format!("{plain}/team/app:1")).unwrap();
        c.warm_up(&r, "pull").unwrap();
        let seen = plain_seen.lock().unwrap().join("\n").to_ascii_lowercase();
        assert!(!seen.contains("get /token") && !seen.contains("authorization"), "warm_up sent credentials to plain-HTTP realm: {seen}");
        assert!(Client::cached_token(&c.token_key(&r, "pull")).is_none());

        // An HTTPS realm still prefetches during warm_up.
        let (tls, tls_seen) = tls_recording_server(|head| {
            if head.starts_with("GET /token") {
                "HTTP/1.1 200 OK\r\nContent-Length: 22\r\nConnection: close\r\n\r\n{\"token\":\"prefetched\"}".into()
            } else {
                OK.into()
            }
        });
        remember_scheme(tls.clone(), "https", Duration::from_secs(60));
        CHALLENGES.lock().unwrap().insert(tls.clone(), format!(r#"Bearer realm="https://{tls}/token",service="r""#));
        let tls_r = Reference::parse(&format!("{tls}/team/app:1")).unwrap();
        c.warm_up(&tls_r, "pull").unwrap();
        assert_eq!(Client::cached_token(&c.token_key(&tls_r, "pull")).as_deref(), Some("Bearer prefetched"));
        assert!(tls_seen.lock().unwrap().iter().any(|h| h.starts_with("GET /token")));
    }

    /// A mirror is asked over the scheme it was written with, unprobed:
    /// `http://` straight over plain HTTP, `https://` over HTTPS only, even
    /// on this machine where a registry is probed and may fall back. The
    /// server here would answer a probe's plain-HTTP request; it is never
    /// asked.
    #[test]
    fn a_mirror_is_asked_over_the_scheme_it_was_written_with() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let conns = Arc::new(AtomicUsize::new(0));
        let seen = conns.clone();
        std::thread::spawn(move || {
            for c in listener.incoming() {
                let Ok(mut c) = c else { continue };
                seen.fetch_add(1, Ordering::SeqCst);
                let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
            }
        });
        let hub = Reference::parse("alpine:3").unwrap();
        for (written, scheme) in [(format!("https://{host}"), "https"), (format!("http://{host}"), "http")] {
            let c = test_client().with_config(RegistryConfig { insecure: Vec::new(), mirrors: vec![Mirror::parse(&written).unwrap()] });
            let (target, carried) = c.targets(&hub).remove(0);
            assert_eq!(target.registry, host);
            assert_eq!(carried, Some(scheme));
            let m = c.for_mirror(&target, carried);
            assert_eq!(m.base(&target.registry), format!("{scheme}://{host}"), "{written}");
            assert_eq!(m.written_scheme(&target.registry), Some(scheme));
            assert!(m.credentials.is_none() && !m.use_mirrors);
        }
        assert_eq!(conns.load(Ordering::SeqCst), 0, "no probe");
        // Docker Hub itself, on the same client, is not touched by it.
        let c = test_client().with_config(RegistryConfig { insecure: Vec::new(), mirrors: vec![Mirror::parse(&format!("http://{host}")).unwrap()] });
        assert_eq!(c.for_mirror(&Reference::parse(&format!("{host}/library/alpine:3")).unwrap(), Some("http")).base("registry-1.docker.io"), "https://registry-1.docker.io");
    }

    /// Two mirrors on the same host with different prefixes and schemes each
    /// keep the scheme they were written with: `https://` never downgrades
    /// to plain HTTP because an earlier mirror on the same host was `http://`,
    /// and `http://` is not sent over HTTPS because an earlier one was `https://`.
    #[test]
    fn same_host_mirrors_keep_each_mirrors_scheme() {
        let hub = Reference::parse("alpine:3").unwrap();
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        for (first, second, want_first, want_second) in [
            ("http", "https", "http", "https"),
            ("https", "http", "https", "http"),
        ] {
            let cancel = Arc::new(Cancel::new());
            let stop = cancel.clone();
            let (host, seen) = dual_recording_server(move |_scheme, head| {
                if head.contains("/v2/b/") {
                    stop.cancel();
                }
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            });
            let mirrors = vec![
                Mirror::parse(&format!("{first}://{host}/a")).unwrap(),
                Mirror::parse(&format!("{second}://{host}/b")).unwrap(),
            ];
            let root = std::env::temp_dir().join(format!("kiri-same-host-mirror-{first}-{second}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let store = Store::open(&root).unwrap();
            let client = test_client().with_config(RegistryConfig { insecure: Vec::new(), mirrors });
            let _ = client.pull_with(&store, &hub, &platform, &|_| {}, &cancel);
            let _ = std::fs::remove_dir_all(&root);
            let reqs: Vec<(&'static str, String)> = seen
                .lock()
                .unwrap()
                .iter()
                .map(|(s, h)| (*s, h.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_owned()))
                .collect();
            assert!(reqs.iter().any(|(s, p)| *s == want_first && p.starts_with("/v2/a/")), "{first}->{second}: {reqs:?}");
            assert!(reqs.iter().any(|(s, p)| *s == want_second && p.starts_with("/v2/b/")), "{first}->{second}: {reqs:?}");
            assert!(!reqs.iter().any(|(s, p)| *s != want_second && p.starts_with("/v2/b/")), "{first}->{second}: {reqs:?}");
        }
    }

    /// A registry on self-signed HTTPS that answers 401: it answered
    /// HTTPS, so it is asked over HTTPS from then on (no plain HTTP), and a
    /// login's Basic answer goes to it there, the certificate unchecked.
    #[test]
    fn a_self_signed_registry_that_challenges_stays_on_https() {
        let (host, seen) = tls_recording_server(|_| "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"r\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into());
        assert_eq!(insecure_scheme(&host), "https");
        assert_eq!(remembered_scheme(&host), Some("https"));
        let c = test_client().with_credentials(me());
        assert_eq!(c.base(&host), format!("https://{host}"));
        assert!(c.login(&host).is_err(), "the registry refuses the password");
        let heads = seen.lock().unwrap().join("\n").to_ascii_lowercase();
        assert!(heads.contains("authorization: basic"), "the Basic answer went over HTTPS: {heads}");
    }

    /// A mirror that wants a token, asked by a client signed in to Docker
    /// Hub: its challenge is answered anonymously, and nothing of the Hub
    /// sign-in reaches it, the token request included.
    #[test]
    fn a_mirrors_token_challenge_is_answered_without_the_hub_sign_in() {
        let me_host: Arc<std::sync::OnceLock<String>> = Arc::default();
        let at = me_host.clone();
        let (mirror, seen) = serve_recording(false, move |head| {
            let lower = head.to_ascii_lowercase();
            if head.starts_with("GET /token") {
                "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"mt\"}".into()
            } else if lower.contains("authorization: bearer mt") {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            } else {
                format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://{}/token\",service=\"m\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", at.get().unwrap())
            }
        });
        me_host.set(mirror.clone()).unwrap();
        let root = std::env::temp_dir().join(format!("kiri-mirror-401-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let name = Reference::parse("alpine:3.22").unwrap();
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        let signed_in = test_client().with_credentials(Some(Credentials { username: "alice".into(), password: "hub-password".into(), registry_token: "hub-token".into(), ..Default::default() }));
        assert!(signed_in.pull_from(&store, &name, &[(name.via(&mirror, None), None)], &platform, &|_| {}, &Cancel::new()).is_err());
        let heads = seen.lock().unwrap().join("\n").to_ascii_lowercase();
        assert!(heads.contains("get /token"), "the mirror's challenge was answered: {heads}");
        let token_request = heads.split("\r\n\r\n").find(|h| h.contains("get /token")).unwrap_or_default().to_owned();
        assert!(!token_request.contains("authorization"), "{token_request}");
        assert!(!heads.contains("basic ") && !heads.contains("hub-token") && !heads.contains("alice"), "{heads}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_redirect_is_made_absolute_against_where_it_came_from() {
        assert_eq!(joined("https://r.lan:5000/v2/a/blobs/x", "https://s3.example.com/b?sig=1"), "https://s3.example.com/b?sig=1");
        assert_eq!(joined("https://r.lan:5000/v2/a/blobs/x", "/storage/x"), "https://r.lan:5000/storage/x");
        assert_eq!(joined("https://r.lan:5000/v2/a/blobs/x", "y"), "https://r.lan:5000/v2/a/blobs/y");
        assert_eq!(joined("https://r.lan:5000", "y"), "https://r.lan:5000/y");
        // A URL that ends in `/` is its own directory: an upload's relative
        // Location stays under the upload path.
        assert_eq!(joined("https://r.lan:5000/v2/a/blobs/uploads/", "6d1c"), "https://r.lan:5000/v2/a/blobs/uploads/6d1c");
        assert_eq!(joined("https://r.lan:5000/", "y"), "https://r.lan:5000/y");
        // A scheme is letters before `://`, not any `://` in the text.
        assert_eq!(joined("https://r.lan:5000/v2/a/blobs/x", "/blob?next=https://evil.example/x"), "https://r.lan:5000/blob?next=https://evil.example/x");
        assert_eq!(joined("https://r.lan:5000/v2/a", "ftp://files.example/x"), "ftp://files.example/x");
        // `//host/x` keeps the scheme and takes the host.
        assert_eq!(joined("https://r.lan:5000/v2/a", "//cdn.example.com/blob"), "https://cdn.example.com/blob");
        assert_eq!(joined("http://r.lan/v2/a", "//cdn.example.com/blob"), "http://cdn.example.com/blob");
        // `from`'s query is not part of its directory.
        assert_eq!(joined("https://r.lan:5000/v2/a/up?next=/x/y", "z"), "https://r.lan:5000/v2/a/z");
        assert_eq!(joined("https://r.lan:5000/v2/a/up?next=/x/y", "/z"), "https://r.lan:5000/z");
        assert_eq!(joined("https://r.lan:5000?a=/b", "y"), "https://r.lan:5000/y");
    }

    fn listing(entries: &[&str]) -> Client {
        test_client().with_config(RegistryConfig { insecure: entries.iter().map(|e| crate::config::Insecure::parse(e).unwrap()).collect(), ..Default::default() })
    }

    /// An unlisted registry names a listed host, or a host in a listed
    /// network, as its token service: nothing goes there unverified or in the
    /// clear, and a listed registry may name only itself or a listed host.
    #[test]
    fn an_unlisted_registry_cannot_send_credentials_to_a_listed_realm() {
        let c = listing(&["auth.lan", "10.0.0.0/8"]);
        for realm in ["http://auth.lan/token", "http://10.1.2.3/token", "http://127.0.0.1:5000/token"] {
            assert!(!c.realm_is_secure(realm, "registry.example.com", true), "{realm}");
        }
        for realm in ["https://auth.lan/token", "https://10.1.2.3/token"] {
            assert!(!c.agent_for_hop(realm, "registry.example.com").config().tls_config().disable_verification(), "{realm}");
        }
        // A listed registry: itself and listed hosts, but not a host only a
        // network covers.
        let c = listing(&["registry.lan", "auth.lan", "10.0.0.0/8"]);
        assert!(c.realm_is_secure("http://registry.lan/token", "registry.lan", true));
        assert!(!c.realm_is_secure("http://registry.lan/token", "registry.lan", false));
        // Another listed host is still another host: no plain-HTTP realm there.
        assert!(!c.realm_is_secure("http://auth.lan/token", "registry.lan", true));
        assert!(!c.realm_is_secure("http://registry.lan:5001/token", "registry.lan", true));
        assert!(!c.realm_is_secure("http://10.1.2.3/token", "registry.lan", true));
        assert!(c.agent_for_hop("https://auth.lan/token", "registry.lan").config().tls_config().disable_verification());
        assert!(!c.agent_for_hop("https://10.1.2.3/token", "registry.lan").config().tls_config().disable_verification());
        // A registry the network covers may name itself, when the challenge
        // arrived over plain HTTP.
        assert!(c.realm_is_secure("http://10.1.2.4/token", "10.1.2.4", true));
        assert!(!c.realm_is_secure("http://10.1.2.4/token", "10.1.2.4", false));
        assert!(!c.realm_is_secure("http://10.1.2.5/token", "10.1.2.4", true));
    }

    /// The same, end to end: the token service is a plain-HTTP server in a
    /// listed network, and the registry that names it is not listed: it is
    /// not asked at all, with credentials or without.
    #[test]
    fn an_unlisted_registrys_plain_http_realm_is_not_asked() {
        let (realm_host, seen) = recording_server(|_| "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"t\"}  ".into());
        for creds in [me(), None] {
            let c = listing(&["127.0.0.0/8"]).with_credentials(creds);
            let params = HashMap::from([("realm".to_owned(), format!("http://{realm_host}/token")), ("service".to_owned(), "r".to_owned())]);
            assert!(c.token(&params, Some("repository:a:pull"), "registry.example.com", true).is_err());
        }
        assert!(seen.lock().unwrap().is_empty(), "{:?}", seen.lock().unwrap());
    }

    /// A listed registry names another listed host's plain-HTTP address as
    /// its token service: the credentials do not travel there.
    #[test]
    fn a_plain_http_realm_on_another_listed_host_gets_nothing() {
        let (realm_host, seen) = recording_server(|_| "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"t\"}  ".into());
        let realm_host = realm_host.replace("127.0.0.1", "0.0.0.0");
        let c = listing(&["registry.lan:5000", &realm_host]).with_credentials(me());
        let params = HashMap::from([("realm".to_owned(), format!("http://{realm_host}/token")), ("service".to_owned(), "r".to_owned())]);
        assert!(c.token(&params, Some("repository:a:pull"), "registry.lan:5000", true).is_err());
        assert!(seen.lock().unwrap().is_empty(), "{:?}", seen.lock().unwrap());
    }

    /// No agent follows a redirect itself, so a registry over verified
    /// HTTPS cannot send a request on to plain HTTP: every hop goes through
    /// `hop`, which refuses it.
    #[test]
    fn a_verified_registry_cannot_redirect_to_plain_http() {
        assert_eq!(shared_agent().config().max_redirects(), 0);
        assert_eq!(insecure_agent().config().max_redirects(), 0);
        let (plain, plain_seen) = recording_server(|_| OK.into());
        let target = format!("http://{plain}/stolen");
        let (https, _) = tls_recording_server(move |_| redirect(&target));
        // 0.0.0.0 reaches the server without being loopback: a registry
        // elsewhere, which nothing lists. The agent here skips the
        // certificate check only so the test server can answer; the client
        // treats the host as verified.
        let https = https.replace("127.0.0.1", "0.0.0.0");
        let cfg = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).tls_config(crate::connect::unverified_tls_config()).build();
        let agent = ureq::Agent::with_parts(cfg, crate::connect::connector(), ureq::unversioned::resolver::DefaultResolver::default());
        let c = Client { agent, credentials: None, config: Default::default(), use_mirrors: true, mirror_scheme: None };
        assert!(!c.config.is_unverified(&https));
        let got = c.send_get(&format!("https://{https}/v2/x"), Some("Basic c2VjcmV0"), &[]);
        assert!(got.is_err(), "followed down to plain HTTP: {:?}", got.map(|r| r.status()));
        assert!(plain_seen.lock().unwrap().is_empty());
    }

    /// A server on loopback that records each request's head and answers it
    /// with `reply(head)`; plain HTTP, or TLS with a throwaway certificate.
    fn recording_server(reply: impl Fn(&str) -> String + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
        serve_recording(false, reply)
    }

    fn tls_recording_server(reply: impl Fn(&str) -> String + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
        serve_recording(true, reply)
    }

    type SchemeLog = Arc<Mutex<Vec<(&'static str, String)>>>;

    /// A server on one port that answers both TLS (ClientHello `0x16`) and
    /// plain HTTP, recording each request with the scheme it arrived over.
    fn dual_recording_server(reply: impl Fn(&'static str, &str) -> String + Send + Sync + 'static) -> (String, SchemeLog) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap().self_signed(&key).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![cert.der().clone()], rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into())
                .unwrap(),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let reply = Arc::new(reply);
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let (log, reply, config) = (log.clone(), reply.clone(), config.clone());
                std::thread::spawn(move || {
                    let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut first = [0u8; 1];
                    let tls = sock.peek(&mut first).unwrap_or(0) == 1 && first[0] == 0x16;
                    let scheme = if tls { "https" } else { "http" };
                    let mut stream: Box<dyn ReadWrite> = if tls {
                        match rustls::ServerConnection::new(config) {
                            Ok(conn) => Box::new(rustls::StreamOwned::new(conn, sock)),
                            Err(_) => return,
                        }
                    } else {
                        Box::new(sock)
                    };
                    let mut head = Vec::new();
                    let mut b = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut b).unwrap_or(0) == 1 {
                        head.push(b[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_string();
                    if head.is_empty() {
                        return;
                    }
                    log.lock().unwrap().push((scheme, head.clone()));
                    let _ = stream.write_all(reply(scheme, &head).as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        (host, seen)
    }

    fn serve_recording(tls: bool, reply: impl Fn(&str) -> String + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
        let config = tls.then(|| {
            let key = rcgen::KeyPair::generate().unwrap();
            let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap().self_signed(&key).unwrap();
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            Arc::new(
                rustls::ServerConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .unwrap()
                    .with_no_client_auth()
                    .with_single_cert(vec![cert.der().clone()], rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into())
                    .unwrap(),
            )
        });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let reply = Arc::new(reply);
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let (log, reply, config) = (log.clone(), reply.clone(), config.clone());
                std::thread::spawn(move || {
                    let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
                    // A TLS handshake on a plain-HTTP port is refused at once.
                    let mut first = [0u8; 1];
                    if config.is_none() && sock.peek(&mut first).unwrap_or(0) == 1 && first[0] == 0x16 {
                        let _ = (&sock).write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    let mut stream: Box<dyn ReadWrite> = match config {
                        Some(c) => match rustls::ServerConnection::new(c) {
                            Ok(conn) => Box::new(rustls::StreamOwned::new(conn, sock)),
                            Err(_) => return,
                        },
                        None => Box::new(sock),
                    };
                    let mut head = Vec::new();
                    let mut b = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut b).unwrap_or(0) == 1 {
                        head.push(b[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_string();
                    if head.is_empty() {
                        return;
                    }
                    log.lock().unwrap().push(head.clone());
                    let _ = stream.write_all(reply(&head).as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        (host, seen)
    }

    trait ReadWrite: Read + Write + Send {}
    impl<T: Read + Write + Send> ReadWrite for T {}

    fn redirect(to: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

    fn with_auth(seen: &Arc<Mutex<Vec<String>>>) -> Vec<bool> {
        seen.lock().unwrap().iter().map(|h| h.to_ascii_lowercase().contains("\nauthorization: basic c2vjcmv0")).collect()
    }

    #[test]
    fn a_redirect_off_the_https_origin_to_plain_http_loses_the_credentials() {
        let (plain, plain_seen) = recording_server(|_| OK.into());
        let target = format!("http://{plain}/stolen");
        let (https, https_seen) = tls_recording_server(move |_| redirect(&target));
        let c = listing(&["127.0.0.0/8"]);
        let resp = c.send_get(&format!("https://{https}/v2/x"), Some("Basic c2VjcmV0"), &[]).unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(with_auth(&https_seen), [true]);
        assert_eq!(with_auth(&plain_seen), [false]);
    }

    #[test]
    fn credentials_do_not_come_back_after_a_hop_to_another_origin() {
        // A (https) -> B (https, another port) -> A again.
        let (a_slot, b_slot) = (Arc::new(Mutex::new(String::new())), Arc::new(Mutex::new(String::new())));
        let (a_to_b, b_to_a) = (b_slot.clone(), a_slot.clone());
        let (a, a_seen) = tls_recording_server(move |head| if head.contains("/again") { OK.into() } else { redirect(&format!("https://{}/hop", a_to_b.lock().unwrap())) });
        let (b, b_seen) = tls_recording_server(move |_| redirect(&format!("https://{}/again", b_to_a.lock().unwrap())));
        *a_slot.lock().unwrap() = a.clone();
        *b_slot.lock().unwrap() = b;
        let c = listing(&["127.0.0.0/8"]);
        let resp = c.send_get(&format!("https://{a}/v2/x"), Some("Basic c2VjcmV0"), &[]).unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(with_auth(&a_seen), [true, false]);
        assert_eq!(with_auth(&b_seen), [false]);
    }

    /// The token service of a listed registry redirects (to a plain-HTTP
    /// host the list allows): followed, and the credentials stay with the
    /// origin that was asked.
    #[test]
    fn a_token_service_that_redirects_is_followed_without_the_credentials_leaving() {
        let (b, b_seen) = recording_server(|_| "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"t\"}  ".into());
        let target = format!("http://{b}/token2");
        let (a, a_seen) = tls_recording_server(move |_| redirect(&target));
        let c = listing(&[]).with_credentials(Some(Credentials { username: "me".into(), password: "secret".into(), ..Default::default() }));
        let params = HashMap::from([("realm".to_owned(), format!("https://{a}/token")), ("service".to_owned(), "r".to_owned())]);
        let (token, _) = c.token(&params, Some("repository:a:pull"), &a, false).unwrap();
        assert_eq!(token, "t");
        assert_eq!(a_seen.lock().unwrap().len(), 1);
        let first = a_seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(first.contains("authorization: basic") && first.contains("account=me"), "{first}");
        let second = b_seen.lock().unwrap().join("\n").to_ascii_lowercase();
        assert!(second.contains("get /token2"), "{second}");
        assert!(!second.contains("authorization"), "{second}");
    }

    /// The upload address a registry names is sent the registry's credentials
    /// only when it is the registry's own origin.
    #[test]
    fn an_upload_to_another_origin_carries_no_credentials() {
        use std::io::Write as _;
        let root = std::env::temp_dir().join(format!("kiri-push-origin-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let mut w = store.ingest().unwrap();
        w.write_all(b"\x1f\x8bnot really gzip").unwrap();
        let (layer, size) = w.finish().unwrap();
        let config = serde_json::json!({"os": "linux", "architecture": "amd64", "config": {}});
        let mut w = store.ingest().unwrap();
        w.write_all(&serde_json::to_vec(&config).unwrap()).unwrap();
        let (cfg, _) = w.finish().unwrap();
        let image = crate::archive::from_config("up:1".into(), &cfg, &config, vec![Layer { digest: layer.clone(), size, media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into() }]);
        store.put_image(image.clone()).unwrap();
        let (b, b_seen) = recording_server(|_| "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into());
        let (a, a_seen) = recording_server(move |head| {
            if head.starts_with("GET /v2/ ") {
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()
            } else if head.starts_with("HEAD") {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            } else if head.starts_with("POST") {
                format!("HTTP/1.1 202 Accepted\r\nLocation: http://{b}/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            }
        });
        let c = test_client().with_credentials(Some(Credentials { registry_token: "tok".into(), ..Default::default() }));
        c.push(&store, &image, &Reference::parse(&format!("{a}/t:1")).unwrap(), &|_| {}).unwrap();
        let (a_heads, b_heads) = (a_seen.lock().unwrap().join("\n").to_ascii_lowercase(), b_seen.lock().unwrap().join("\n").to_ascii_lowercase());
        assert!(a_heads.contains("authorization: bearer tok"), "the registry itself is sent them: {a_heads}");
        assert!(b_heads.contains("put /elsewhere"), "{b_heads}");
        assert!(!b_heads.contains("authorization"), "{b_heads}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A blob's HEAD that is redirected is followed: a blob the storage
    /// answers 404 for is uploaded, one it has is not.
    #[test]
    fn a_redirected_blob_check_is_followed_to_its_answer() {
        use std::io::Write as _;
        let root = std::env::temp_dir().join(format!("kiri-push-head-redirect-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let mut w = store.ingest().unwrap();
        w.write_all(b"\x1f\x8bnot really gzip either").unwrap();
        let (layer, size) = w.finish().unwrap();
        let config = serde_json::json!({"os": "linux", "architecture": "amd64", "config": {}});
        let mut w = store.ingest().unwrap();
        w.write_all(&serde_json::to_vec(&config).unwrap()).unwrap();
        let (cfg, _) = w.finish().unwrap();
        let image = crate::archive::from_config("up:1".into(), &cfg, &config, vec![Layer { digest: layer, size, media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into() }]);
        store.put_image(image.clone()).unwrap();
        for (storage_status, uploads) in [("404 Not Found", true), ("200 OK", false)] {
            let (storage, storage_seen) = recording_server(move |_| format!("HTTP/1.1 {storage_status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"));
            let (a, a_seen) = recording_server(move |head| {
                if head.starts_with("GET /v2/ ") {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()
                } else if head.starts_with("HEAD") {
                    format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{storage}/blob\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                } else if head.starts_with("POST") {
                    "HTTP/1.1 202 Accepted\r\nLocation: /v2/t/blobs/uploads/1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                } else {
                    "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                }
            });
            test_client().push(&store, &image, &Reference::parse(&format!("{a}/t:1")).unwrap(), &|_| {}).unwrap();
            let heads = a_seen.lock().unwrap().join("\n").to_ascii_lowercase();
            assert!(storage_seen.lock().unwrap().join("\n").to_ascii_lowercase().contains("head /blob"), "the HEAD was followed");
            assert_eq!(heads.contains("post /v2/t/blobs/uploads/"), uploads, "{storage_status}: {heads}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    fn me() -> Option<Credentials> {
        Some(Credentials { username: "me".into(), password: "secret".into(), ..Default::default() })
    }

    /// A host that is not the registry answers 401 and names a token service
    /// of its own: the registry's credentials do not go there.
    const CHALLENGE: &str = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://EVIL/token\",service=\"x\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    #[test]
    fn an_upload_host_that_challenges_is_not_given_the_credentials() {
        use std::io::Write as _;
        let root = std::env::temp_dir().join(format!("kiri-push-challenge-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let mut w = store.ingest().unwrap();
        w.write_all(b"\x1f\x8bnot really gzip").unwrap();
        let (layer, size) = w.finish().unwrap();
        let config = serde_json::json!({"os": "linux", "architecture": "amd64", "config": {}});
        let mut w = store.ingest().unwrap();
        w.write_all(&serde_json::to_vec(&config).unwrap()).unwrap();
        let (cfg, _) = w.finish().unwrap();
        let image = crate::archive::from_config("up:1".into(), &cfg, &config, vec![Layer { digest: layer, size, media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into() }]);
        store.put_image(image.clone()).unwrap();
        let evil_host = Arc::new(Mutex::new(String::new()));
        let h = evil_host.clone();
        let (evil, evil_seen) = recording_server(move |head| {
            if head.starts_with("GET /token") {
                "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"t\"}  ".into()
            } else {
                CHALLENGE.replace("EVIL", &h.lock().unwrap())
            }
        });
        *evil_host.lock().unwrap() = evil.clone();
        let (a, _) = recording_server(move |head| {
            if head.starts_with("GET /v2/ ") {
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()
            } else if head.starts_with("HEAD") {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            } else if head.starts_with("POST") {
                format!("HTTP/1.1 202 Accepted\r\nLocation: http://{evil}/upload\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            }
        });
        let c = test_client().with_credentials(me());
        let pushed = c.push(&store, &image, &Reference::parse(&format!("{a}/t:1")).unwrap(), &|_| {});
        assert!(pushed.is_err(), "the upload host refused");
        let heads = evil_seen.lock().unwrap().join("\n").to_ascii_lowercase();
        assert!(heads.contains("put /upload"), "{heads}");
        assert!(!heads.contains("authorization"), "{heads}");
        assert!(!heads.contains("account=me"), "{heads}");
        assert!(!heads.contains("get /token"), "the challenge was not answered: {heads}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_login_redirected_off_the_registry_sends_no_credentials() {
        let evil_host = Arc::new(Mutex::new(String::new()));
        let h = evil_host.clone();
        let (evil, evil_seen) = recording_server(move |head| {
            if head.starts_with("GET /token") {
                "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"t\"}  ".into()
            } else {
                CHALLENGE.replace("EVIL", &h.lock().unwrap())
            }
        });
        *evil_host.lock().unwrap() = evil.clone();
        // The first request is the scheme probe; the login after it is sent away.
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let (a, _) = recording_server(move |_| {
            if asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into() } else { redirect(&format!("http://{evil}/v2/")) }
        });
        let c = test_client().with_credentials(me());
        assert!(c.login(&a).is_err());
        let heads = evil_seen.lock().unwrap().join("\n").to_ascii_lowercase();
        assert!(heads.contains("get /v2/"), "{heads}");
        assert!(!heads.contains("authorization") && !heads.contains("account=me") && !heads.contains("get /token"), "{heads}");
    }

    /// A Basic challenge from where a login was redirected (another host)
    /// is not answered: the credentials go only to the registry's origin.
    #[test]
    fn a_basic_challenge_off_the_registry_gets_no_credentials() {
        let (evil, evil_seen) = recording_server(|_| "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into());
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let (a, _) = recording_server(move |_| {
            if asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into() } else { redirect(&format!("http://{evil}/v2/")) }
        });
        let c = test_client().with_credentials(me());
        assert!(c.login(&a).is_err());
        let heads = evil_seen.lock().unwrap().join("\n").to_ascii_lowercase();
        assert!(heads.contains("get /v2/"), "{heads}");
        assert!(!heads.contains("authorization"), "{heads}");
    }

    /// The registry moving a login up to HTTPS on its own host answers
    /// there, and is sent the credentials there.
    #[test]
    fn a_basic_login_is_sent_only_to_the_registrys_own_origin() {
        assert!(basic_login_target("https://r.lan/v2/", "http://r.lan/v2/").is_some());
        assert!(basic_login_target("http://r.lan:5000/v2/", "http://r.lan:5000/v2/").is_some());
        assert!(basic_login_target("https://evil.example/v2/", "https://r.lan/v2/").is_none());
        assert!(basic_login_target("http://r.lan:6000/v2/", "http://r.lan:5000/v2/").is_none());
        assert!(basic_login_target("http://r.lan/v2/", "https://r.lan/v2/").is_none());
    }

    /// The same with a list of networks (the slow path, where the verified
    /// agent follows the redirect itself): the evil host's own http realm
    /// gets no token request and no credentials.
    #[test]
    fn a_redirect_target_that_challenges_gets_nothing_with_a_network_listed() {
        let evil_host = Arc::new(Mutex::new(String::new()));
        let h = evil_host.clone();
        let (evil, evil_seen) = recording_server(move |head| {
            if head.starts_with("GET /token") {
                "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"t\"}  ".into()
            } else {
                CHALLENGE.replace("EVIL", &h.lock().unwrap())
            }
        });
        *evil_host.lock().unwrap() = evil.clone();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let (a, _) = recording_server(move |_| {
            if asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into() } else { redirect(&format!("http://{evil}/blob")) }
        });
        let c = listing(&["10.0.0.0/8"]).with_credentials(me());
        let r = Reference::parse(&format!("{a}/t/b:1")).unwrap();
        assert!(c.get(&r, &format!("http://{a}/v2/t/blobs/sha256:x"), None).is_err());
        let heads = evil_seen.lock().unwrap().join("\n").to_ascii_lowercase();
        assert!(heads.contains("get /blob"), "{heads}");
        assert!(!heads.contains("authorization") && !heads.contains("account=me") && !heads.contains("get /token"), "{heads}");
    }

    /// Only http and https URLs have an origin to compare.
    #[test]
    fn another_scheme_has_no_origin() {
        assert_eq!(origin_of("http://r.lan/v2/"), Some((false, "r.lan".into(), 80)));
        assert_eq!(origin_of("ftp://r.lan/v2/"), None);
        assert_eq!(origin_of("ws://r.lan/v2/"), None);
        assert!(!is_own_origin("ftp://r.lan/v2/", "ftp://r.lan/v2/"));
    }

    /// A pull stopped while the HTTPS probe runs does not go on to try
    /// plain HTTP: the host is connected to once.
    #[test]
    fn a_cancelled_scheme_probe_stops_after_https() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = accepted.clone();
        let cancel = Cancel::new();
        let stop = cancel.clone();
        // Stopped while the HTTPS probe is out: its connection is dropped
        // with the pull stopped, and the plain-HTTP probe is never sent.
        std::thread::spawn(move || {
            for c in listener.incoming() {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                stop.cancel();
                drop(c);
            }
        });
        let _on = cancel.enter();
        assert_eq!(insecure_scheme(&host), "https");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(remembered_scheme(&host), None, "nothing learnt about it");
    }

    /// A pull stopped while another pull probes the same host returns at
    /// once, not when that probe ends.
    #[test]
    fn a_stopped_pull_does_not_wait_for_another_probe() {
        let host = "waiting.test:5000";
        let lock = PROBING.lock().unwrap().entry(host.to_owned()).or_default().clone();
        let held = lock.lock().unwrap();
        let cancel = Cancel::new();
        let stop = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stop.cancel();
        });
        let started = Instant::now();
        let scheme = {
            let _on = cancel.enter();
            insecure_scheme(host)
        };
        assert_eq!(scheme, "https");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        drop(held);
    }

    /// A proxy's 502, 503 or 504 to the plain-HTTP probe is the proxy
    /// failing to reach the host, not the host answering plain HTTP.
    #[test]
    fn a_gateway_error_is_not_a_plain_http_answer() {
        for status in ["502 Bad Gateway", "503 Service Unavailable", "504 Gateway Timeout", "401 Unauthorized"] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
            let reply = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            std::thread::spawn(move || {
                for c in listener.incoming() {
                    let Ok(mut c) = c else { continue };
                    // A TLS handshake gets a plain-HTTP answer: no HTTPS here.
                    let _ = c.read(&mut [0u8; 1024]);
                    let _ = c.write_all(reply.as_bytes());
                }
            });
            let want = if status.starts_with('4') { "http" } else { "https" };
            assert_eq!(insecure_scheme(&host), want, "{status}");
        }
    }

    #[test]
    fn an_upgrade_to_https_on_the_same_host_is_the_registrys_own() {
        // Up from http on the same host: default ports, or the same explicit port.
        assert!(is_own_origin("https://r.lan/v2/", "http://r.lan"));
        assert!(is_own_origin("https://r.lan:5000/v2/", "http://r.lan:5000"));
        assert!(is_own_origin("https://R.LAN/x", "http://r.lan"));
        // Everything else is a hand-off.
        assert!(is_own_origin("http://r.lan/x", "http://r.lan"));
        assert!(!is_own_origin("https://other.lan/x", "http://r.lan"));
        assert!(!is_own_origin("https://r.lan:8443/x", "http://r.lan"));
        assert!(!is_own_origin("https://r.lan:5001/x", "http://r.lan:5000"));
        assert!(!is_own_origin("http://r.lan/x", "https://r.lan"), "https down to http");
        assert!(!is_own_origin("http://r.lan:81/x", "http://r.lan"));
        assert!(!is_own_origin("not a url", "http://r.lan"));
        assert!(is_own_origin("http://[fd00:0::1]:5000/v2/", "http://[FD00::1]:5000"));
    }

    #[test]
    fn a_redirect_target_that_challenges_is_not_given_the_credentials() {
        let evil_host = Arc::new(Mutex::new(String::new()));
        let h = evil_host.clone();
        let (evil, evil_seen) = recording_server(move |head| {
            if head.starts_with("GET /token") {
                "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\":\"t\"}  ".into()
            } else {
                CHALLENGE.replace("EVIL", &h.lock().unwrap())
            }
        });
        *evil_host.lock().unwrap() = evil.clone();
        let (a, _) = recording_server(move |_| redirect(&format!("http://{evil}/blob")));
        let c = test_client().with_credentials(me());
        let r = Reference::parse(&format!("{a}/t/b:1")).unwrap();
        assert!(c.get(&r, &format!("http://{a}/v2/t/blobs/sha256:x"), None).is_err());
        let heads = evil_seen.lock().unwrap().join("\n").to_ascii_lowercase();
        assert!(heads.contains("get /blob"), "{heads}");
        assert!(!heads.contains("authorization") && !heads.contains("account=me") && !heads.contains("get /token"), "{heads}");
    }

    #[test]
    fn a_redirect_inside_the_origin_keeps_the_credentials() {
        let (a, a_seen) = tls_recording_server(|head| if head.contains("/second") { OK.into() } else { redirect("/second") });
        let c = listing(&["127.0.0.0/8"]);
        c.send_get(&format!("https://{a}/v2/x"), Some("Basic c2VjcmV0"), &[]).unwrap();
        assert_eq!(with_auth(&a_seen), [true, true]);
    }

    #[test]
    fn https_to_http_goes_only_to_a_host_the_list_allows() {
        let c = listing(&["registry.lan:5000", "plain.lan"]);
        let origin = "https://registry.lan:5000/v2/x";
        let go = |from: &str, to: &str| c.hop("registry.lan:5000", origin, from, to, true);
        // Down to its own host or a listed one: yes, without the header.
        assert_eq!(go(origin, "http://registry.lan:5000/v2/x").unwrap(), ("http://registry.lan:5000/v2/x".to_owned(), false));
        assert!(go(origin, "http://plain.lan/x").is_ok());
        // Down to a host nobody listed, or to a network-only host: no.
        assert!(go(origin, "http://evil.example/x").is_err());
        // `//host/x` keeps https: not a downgrade, and not the origin either.
        assert_eq!(go(origin, "//evil.example/x").unwrap(), ("https://evil.example/x".to_owned(), false));
        assert!(go(origin, "ftp://registry.lan/x").is_err());
        // Up to https from a request already off the origin: the header stays gone.
        assert!(!c.hop("registry.lan:5000", origin, "http://x.example/a", "https://registry.lan:5000/v2/x", false).unwrap().1);
        // Same scheme, host and port: the header stays; another port: it goes.
        assert!(go(origin, "/other").unwrap().1);
        assert!(go(origin, "https://registry.lan:5000/other").unwrap().1);
        assert!(!go(origin, "https://registry.lan:5001/other").unwrap().1);
        // And once gone it stays gone.
        assert!(!c.hop("registry.lan:5000", origin, "https://s3.example.com/x", "https://registry.lan:5000/y", false).unwrap().1);
        let nets = listing(&["registry.lan:5000", "10.0.0.0/8"]);
        assert!(nets.hop("registry.lan:5000", origin, origin, "http://10.1.2.3/x", true).is_err());
    }

    fn pull_through(mirror: &str, origin: &str, tag: &str) -> (Result<Image, Error>, Vec<Progress>) {
        let root = std::env::temp_dir().join(format!("kiri-mirror-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let name = Reference::parse("alpine:3.22").unwrap();
        let targets = vec![(name.via(mirror, None), None), (name.via(origin, None), None)];
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        let said = Mutex::new(Vec::new());
        let result = test_client().pull_from(&store, &name, &targets, &platform, &|p| said.lock().unwrap().push(p), &Cancel::new());
        let _ = std::fs::remove_dir_all(&root);
        (result, said.into_inner().unwrap())
    }

    /// A mirror whose manifest resolves but whose layer is missing, or
    /// whose image is for another platform than its index says: the pull
    /// moves on to the next, and the image is the next one's.
    #[test]
    fn a_mirror_that_fails_after_the_manifest_hands_the_pull_on() {
        for (tag, mirror) in [("blob", image_server_with(200, "amd64", None)), ("arch", image_server_with(200, "arm64", Some(b"kiri registry mirror test layer".to_vec())))] {
            let (mirror, mirror_seen) = mirror;
            let (origin, origin_seen) = image_server(200);
            let (image, said) = pull_through(&mirror, &origin, tag);
            let image = image.unwrap();
            // The stand-in for Docker Hub here is a loopback server too, so
            // the image names it as where it came from.
            assert_eq!(image.mirror.as_deref(), Some(origin.as_str()), "{tag}: the image is the next one's: {said:?}");
            assert!(said.iter().any(|p| matches!(p, Progress::MirrorFallback { .. })), "{tag}");
            if tag == "arch" {
                // Passed over before any layer: none of its layers is shown.
                let fallback = said.iter().position(|p| matches!(p, Progress::MirrorFallback { .. })).unwrap();
                assert!(!said[..fallback].iter().any(|p| matches!(p, Progress::Resolved { .. } | Progress::Layer { .. })), "{said:?}");
            }
            quit(&mirror);
            quit(&origin);
            assert!(mirror_seen.join().unwrap().iter().any(|p| p.contains("/manifests/")), "{tag}");
            assert!(origin_seen.join().unwrap().iter().any(|p| p.contains("/blobs/")), "{tag}");
        }
    }

    /// An image a mirror served says so: in the image, and as the pull goes.
    #[test]
    fn an_image_from_a_mirror_records_the_mirror() {
        let (mirror, _) = image_server(200);
        let (origin, _) = image_server(200);
        let (image, said) = pull_through(&mirror, &origin, "record");
        assert_eq!(image.unwrap().mirror.as_deref(), Some(mirror.as_str()));
        assert!(said.iter().any(|p| matches!(p, Progress::FromMirror { mirror: m } if *m == mirror)));
        quit(&mirror);
        quit(&origin);
    }

    /// A layer longer than its manifest says is refused, not written out.
    #[test]
    fn a_layer_longer_than_the_manifest_says_is_refused() {
        let mut long = b"kiri registry mirror test layer".to_vec();
        long.extend_from_slice(&[b'x'; 4096]);
        let (origin, _) = image_server_with(200, "amd64", Some(long));
        let root = std::env::temp_dir().join(format!("kiri-long-layer-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let name = Reference::parse(&format!("{origin}/team/app:1")).unwrap();
        let platform = Platform { os: "linux".into(), architecture: "amd64".into() };
        assert!(test_client().pull(&store, &name, &platform, &|_| {}).is_err());
        assert_eq!(std::fs::read_dir(root.join("tmp")).map(|d| d.count()).unwrap_or(0), 0, "nothing is left of it");
        quit(&origin);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A layer longer than its manifest says, whose digest is the longer
    /// body's, is refused before it goes into the store, whatever limit the
    /// read had (here none, and no length header).
    #[test]
    fn a_layer_of_the_wrong_length_never_reaches_the_store() {
        let body = "kiri registry test layer, one byte longer than declared";
        let (host, _) = recording_server(move |_| format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n", body.len()));
        let root = std::env::temp_dir().join(format!("kiri-wrong-length-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let r = Reference::parse(&format!("{host}/team/app:1")).unwrap();
        let digest = sha256_digest(body.as_bytes());
        let declared = body.len() as u64 - 1;
        let got = test_client().download_with(&store, &r, &digest, u64::MAX, Some(declared), &|_| {}, &Cancel::new());
        assert!(got.is_err(), "{got:?}");
        assert!(!store.blob_path(&digest).unwrap().exists(), "the blob was not stored");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A blob with a declared size is read no further than one byte past
    /// it, whatever larger limit the caller gave (a config's is MAX_CONFIG).
    #[test]
    fn a_blob_is_read_no_further_than_its_declared_size() {
        let body = "x".repeat(64 * 1024);
        let sent = body.clone();
        let (host, _) = recording_server(move |_| format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{sent}\r\n0\r\n\r\n", sent.len()));
        let root = std::env::temp_dir().join(format!("kiri-declared-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let r = Reference::parse(&format!("{host}/team/app:1")).unwrap();
        let digest = sha256_digest(body.as_bytes());
        let got = test_client().download_with(&store, &r, &digest, crate::store::MAX_CONFIG, Some(100), &|_| {}, &Cancel::new());
        let err = format!("{got:?}");
        assert!(err.contains("BodyExceedsLimit(101)"), "{err}");
        assert!(!store.blob_path(&digest).unwrap().exists(), "the blob was not stored");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A mirror's image for another platform is turned down from its config,
    /// before any of its layers is fetched.
    #[test]
    fn a_mirror_for_another_platform_is_left_before_its_layers() {
        let (mirror, mirror_seen) = image_server_with(200, "arm64", Some(b"kiri registry mirror test layer".to_vec()));
        let (origin, _) = image_server(200);
        let (image, _) = pull_through(&mirror, &origin, "platform-first");
        assert!(image.is_ok());
        quit(&mirror);
        quit(&origin);
        let layer = sha256_digest(b"kiri registry mirror test layer");
        let asked = mirror_seen.join().unwrap();
        assert!(!asked.iter().any(|p| p.contains(&layer)), "{asked:?}");
    }

    /// Signed in to Docker Hub, a pull asks the mirrors only for official
    /// images: any other repository may be private.
    #[test]
    fn a_signed_in_pull_keeps_its_own_repositories_off_the_mirrors() {
        let client = Client::new().with_config(RegistryConfig { mirrors: vec![Mirror::parse("mirror.example.com").unwrap()], ..Default::default() });
        let signed_in = Client::new().with_config((*client.config).clone()).with_credentials(me());
        let official = Reference::parse("alpine:3.22").unwrap();
        let own = Reference::parse("someone/private:1").unwrap();
        assert_eq!(client.targets(&own).len(), 2, "anonymous: the mirror, then Hub");
        assert_eq!(signed_in.targets(&official).len(), 2);
        assert_eq!(signed_in.targets(&own), vec![(own.clone(), None)], "signed in: Hub only");
    }

    /// A plain-HTTP mirror on a host nobody marked insecure is not asked.
    #[test]
    fn a_plain_http_mirror_needs_its_host_listed() {
        let hub = Reference::parse("alpine:3.22").unwrap();
        let mirrors = vec![Mirror::parse("http://mirror.lan:5000").unwrap(), Mirror::parse("https://mirror.example.com").unwrap()];
        let unlisted = Client::new().with_config(RegistryConfig { mirrors: mirrors.clone(), ..Default::default() });
        let hosts = |c: &Client| c.targets(&hub).into_iter().map(|(r, _)| r.registry).collect::<Vec<_>>();
        assert_eq!(hosts(&unlisted), ["mirror.example.com", "docker.io"]);
        let listed = Client::new().with_config(RegistryConfig { mirrors, insecure: vec![crate::config::Insecure::parse("mirror.lan:5000").unwrap()] });
        assert_eq!(hosts(&listed), ["mirror.lan:5000", "mirror.example.com", "docker.io"]);
    }

    #[test]
    fn a_login_server_is_a_plain_host() {
        for (server, host) in [
            ("https://index.docker.io/v1/", "registry-1.docker.io"),
            ("", "registry-1.docker.io"),
            ("DOCKER.IO", "registry-1.docker.io"),
            ("HTTPS://Index.Docker.IO./v1/", "registry-1.docker.io"),
            ("registry-1.docker.io:443", "registry-1.docker.io"),
            ("ghcr.io", "ghcr.io"),
            ("HTTP://Registry.LAN:5000/", "registry.lan:5000"),
            ("[FD00:0::1]:5000", "[fd00::1]:5000"),
        ] {
            assert_eq!(login_host(server).unwrap(), host, "{server}");
        }
        for bad in ["10.0.0.1:@registry-1.docker.io", "10.0.0.1:@ghcr.io", "user@registry.lan", "registry.lan?x=1", "registry.lan#x", "registry.lan:+5000", "a..b", "https://", "http:///v2/", "/foo"] {
            assert!(login_host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_mirrors_answer_is_printed_without_its_control_characters() {
        assert_eq!(printable("no\x1b[31m red\r\nhere"), "no [31m red  here");
        let long = "x".repeat(500);
        assert_eq!(printable(&long).chars().count(), 201);
    }

    /// The scheme cache keeps one entry per host however it is spelt, stays
    /// bounded, and remembers a host that answered nothing for a while.
    #[test]
    fn the_scheme_cache_is_bounded_and_remembers_failures() {
        for i in 0..(MAX_SCHEMES + 50) {
            remember_scheme(format!("bound-{i}.test:5000"), "https", SCHEME_TTL);
        }
        assert!(SCHEMES.lock().unwrap().len() <= MAX_SCHEMES);
        // A port nothing listens on: refused at once, both ways, and then
        // not asked again until the short wait is over.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let host = format!("LOCALHOST:{port}");
        assert_eq!(insecure_scheme(&host), "https");
        let key = format!("localhost:{port}");
        let until = SCHEMES.lock().unwrap().get(&key).map(|(_, u)| *u);
        let until = until.expect("the failure is remembered, under the lowercase name");
        assert!(until <= Instant::now() + SCHEME_FAILED_TTL);
    }

    /// A registry that serves one image under any repository name: an index
    /// with one linux/amd64 manifest, its config and one layer.
    /// `manifest_status` answers a manifest request instead of the image
    /// when it is not 200, for a mirror that does not have it.
    fn image_server(manifest_status: u16) -> (String, std::thread::JoinHandle<Vec<String>>) {
        image_server_with(manifest_status, "amd64", Some(b"kiri registry mirror test layer".to_vec()))
    }

    /// [`image_server`] serving its image for `arch`, and `layer` as the
    /// layer's bytes: `None` answers 404 for it (a mirror missing a blob).
    fn image_server_with(manifest_status: u16, arch: &str, layer: Option<Vec<u8>>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let expected: Vec<u8> = b"kiri registry mirror test layer".to_vec();
        let layer_digest = sha256_digest(&expected);
        let layer_bytes = layer;
        let config: Vec<u8> = format!(r#"{{"architecture":"{arch}","os":"linux","config":{{}}}}"#).into_bytes();
        let config_digest = sha256_digest(&config);
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "size": config.len(), "digest": config_digest},
            "layers": [{"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip", "size": expected.len(), "digest": layer_digest}],
        })
        .to_string();
        let manifest_digest = sha256_digest(manifest.as_bytes());
        let index = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
            "manifests": [{
                "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                "size": manifest.len(),
                "digest": manifest_digest,
                "platform": {"architecture": "amd64", "os": "linux"},
            }],
        })
        .to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let served = std::thread::spawn(move || {
            let mut seen = Vec::new();
            listener.set_nonblocking(true).unwrap();
            let end = Instant::now() + Duration::from_secs(20);
            while Instant::now() < end {
                let Ok((mut c, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                c.set_nonblocking(false).unwrap();
                // A TLS handshake on this plain-HTTP port is refused at once,
                // as Go's and nginx's servers refuse it.
                let mut first = [0u8; 1];
                if c.peek(&mut first).unwrap_or(0) == 1 && first[0] == 0x16 {
                    let _ = c.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && c.read(&mut b).unwrap_or(0) == 1 {
                    head.push(b[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                if head.is_empty() {
                    break;
                }
                if head.starts_with("QUIT") {
                    break;
                }
                let path = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_string();
                // A request that carried credentials is marked, so a test can
                // tell what a mirror was sent.
                let authed = head.to_ascii_lowercase().contains("\nauthorization:");
                // The scheme probe a loopback registry gets is answered, not counted.
                if path != "/v2/" {
                    seen.push(if authed { format!("{path} (with credentials)") } else { path.clone() });
                }
                let (status, body, kind): (u16, Vec<u8>, &str) = if path == "/v2/" {
                    (200, b"{}".to_vec(), "application/json")
                } else if let Some(reference) = path.split_once("/manifests/").map(|(_, r)| r) {
                    if manifest_status != 200 {
                        (manifest_status, Vec::new(), "application/json")
                    } else if reference.starts_with("sha256:") {
                        (200, manifest.clone().into_bytes(), "application/vnd.docker.distribution.manifest.v2+json")
                    } else {
                        (200, index.clone().into_bytes(), "application/vnd.oci.image.index.v1+json")
                    }
                } else if let Some(digest) = path.split_once("/blobs/").map(|(_, d)| d) {
                    if digest == config_digest {
                        (200, config.clone(), "application/octet-stream")
                    } else if digest == layer_digest && let Some(bytes) = &layer_bytes {
                        (200, bytes.clone(), "application/octet-stream")
                    } else {
                        (404, Vec::new(), "application/json")
                    }
                } else {
                    (404, Vec::new(), "application/json")
                };
                let head = format!("HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = c.write_all(head.as_bytes());
                let _ = c.write_all(&body);
            }
            seen
        });
        (host, served)
    }
}
