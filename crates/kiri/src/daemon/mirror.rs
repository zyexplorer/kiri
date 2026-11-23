//! Kiri's images as a registry, for its BuildKit: `FROM myapp:dev` (an
//! image built or loaded here, never pushed) works as it does with Docker.
//!
//! A read-only registry API on 127.0.0.1, which containers reach at
//! host.docker.internal. buildkitd.toml names it as a mirror of every
//! registry Kiri has images from; a mirror that answers 404 is skipped, so
//! anything not here comes from the registry itself. Paths carry a secret
//! (`/v2/<token>/<registry>/<repo>/…`), so other local users can't read
//! images through it. Manifests are made from the store's records (layers,
//! config), the same bytes for the same image every time.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use serde_json::{Value, json};

use kiri_image::{Image, Reference};

use super::engine::Engine;

/// Tried first, so a builder started by buildx after a daemon restart still
/// finds the mirror its config names.
const PORT: u16 = 47_391;
const DEV_PORT: u16 = 47_392;

static LISTENING: OnceLock<Option<u16>> = OnceLock::new();

/// The mirror's secret path segment, kept in the state directory so it
/// survives restarts (the builder's config keeps naming it).
fn token() -> String {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            let path = super::state_dir().ok().map(|d| d.join("build-mirror-token"));
            if let Some(t) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| t.trim().to_owned()).filter(|t| t.len() == 32) {
                return t;
            }
            let t = super::engine::random_hex(16);
            if let Some(p) = path {
                let _ = save_token(&p, &t);
            }
            t
        })
        .clone()
}

/// Writes the token readable by its owner only from the moment the file
/// exists: written and then chmod-ed, it was the umask's 0644 in between,
/// and other local users could read it there.
fn save_token(path: &std::path::Path, token: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // One already there (not a valid token) is replaced, mode and all.
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(token.as_bytes())
}

/// Starts the server (at the daemon's start: a listening socket costs
/// nothing while no build asks).
pub fn start(engine: &Arc<Engine>) {
    LISTENING.get_or_init(|| {
        let wanted = if crate::flavor::dev() { DEV_PORT } else { PORT };
        let listener = TcpListener::bind(("127.0.0.1", wanted)).or_else(|_| TcpListener::bind(("127.0.0.1", 0))).ok()?;
        let port = listener.local_addr().ok()?.port();
        let me = Arc::downgrade(engine);
        // On a Linux host the builder reaches the mirror at the default
        // bridge's gateway (mirror_host): containers have no way to the
        // host's loopback there, so it listens on that address too.
        #[cfg(target_os = "linux")]
        if let Some(gateway) = bridge_gateway(&engine.lock_net()) {
            match bind_freely(gateway, port) {
                Ok(l) => {
                    let me = me.clone();
                    let _ = std::thread::Builder::new().name("kiri-build-mirror".into()).spawn(move || serve(l, me));
                }
                Err(e) => crate::diag::warn!("image mirror not on the bridge; builds pull from registries", error = e),
            }
        }
        std::thread::Builder::new().name("kiri-build-mirror".into()).spawn(move || serve(listener, me)).ok()?;
        crate::diag::info!("image mirror for builds", port = port);
        Some(port)
    });
}

/// buildkitd.toml lines: this mirror for every registry Kiri has images
/// from (Docker Hub always), over plain HTTP.
#[cfg(target_os = "linux")]
fn mirror_host(networks: &super::network::Networks) -> String {
    bridge_gateway(networks).map_or_else(|| super::ports::HOST_ADDR.into(), super::network::ip_str)
}

/// The default bridge's gateway, which containers reach the host at.
#[cfg(target_os = "linux")]
fn bridge_gateway(networks: &super::network::Networks) -> Option<[u8; 4]> {
    networks.find("bridge").map(|n| n.gateway).filter(|g| *g != [0; 4])
}

/// A listener on `ip:port` before the address exists: the bridge (and
/// its gateway address) is made with the first container (IP_FREEBIND).
#[cfg(target_os = "linux")]
fn bind_freely(ip: [u8; 4], port: u16) -> io::Result<TcpListener> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: plain socket creation; owned at once.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is fresh and ours.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    for opt in [(libc::SOL_SOCKET, libc::SO_REUSEADDR), (libc::IPPROTO_IP, libc::IP_FREEBIND)] {
        // SAFETY: setsockopt with an int of the stated size on a socket we own.
        if unsafe { libc::setsockopt(fd, opt.0, opt.1, (&one as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: sockaddr_in is plain data; all-zero is valid, then filled in.
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = port.to_be();
    sin.sin_addr.s_addr = u32::from_ne_bytes(ip);
    // SAFETY: bind and listen on our socket with a sockaddr_in of its size.
    unsafe {
        if libc::bind(fd, (&sin as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t) != 0 || libc::listen(fd, 128) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(TcpListener::from(sock))
}

#[cfg(not(target_os = "linux"))]
fn mirror_host(_networks: &super::network::Networks) -> String {
    super::ports::HOST_ADDR.into()
}

/// Where a build reaches this computer: the address `--add-host
/// x:host-gateway` gets, the one the mirror is reached at. On a Linux host
/// (and Windows' engine in WSL2) that's the default bridge's gateway, as
/// for `docker run`; 198.19.248.254 only leads anywhere in the Mac's VM.
pub fn host_addr(engine: &Engine) -> String {
    mirror_host(&engine.lock_net())
}

/// One `[registry."host"]` table of buildkitd.toml. `http` unset leaves
/// BuildKit's default (plain HTTP for `localhost`, HTTPS for the rest).
#[derive(Default, Debug, PartialEq)]
struct Table {
    mirrors: Vec<String>,
    http: Option<bool>,
    insecure: bool,
}

/// The table for `registry`, made if it isn't there yet: BuildKit refuses a
/// buildkitd.toml that names one twice, and Kiri's own mirror, daemon.json's
/// mirrors and its insecure hosts may all cover the same one.
fn table<'a>(tables: &'a mut Vec<(String, Table)>, registry: &str) -> &'a mut Table {
    let i = match tables.iter().position(|(r, _)| r == registry) {
        Some(i) => i,
        None => {
            tables.push((registry.to_owned(), Table::default()));
            tables.len() - 1
        }
    };
    &mut tables[i].1
}

/// Adds `mirror` to `registry`'s list, in order, once.
fn add_mirror(tables: &mut Vec<(String, Table)>, registry: &str, mirror: String) {
    let t = table(tables, registry);
    if !t.mirrors.contains(&mirror) {
        t.mirrors.push(mirror);
    }
}

/// The registries buildkitd.toml has tables for: Docker Hub, and each one
/// an image in the store came from (not this machine's), or came from
/// since the engine started, so removing a registry's last image leaves
/// builds from it as they were.
pub fn registry_names(engine: &Engine) -> Vec<String> {
    let mut names = Vec::new();
    for i in engine.store.images().unwrap_or_default() {
        if let Ok(r) = Reference::parse(&i.name)
            && !names.contains(&r.registry)
            && !kiri_image::reference::is_local(&r.registry)
        {
            names.push(r.registry);
        }
    }
    remembered(names)
}

/// Registries the store's images have named since the engine started: a
/// few dozen at most, bounded all the same.
static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
const MAX_SEEN: usize = 256;

/// `now` (the store's registries) with those [`SEEN`] before, Docker Hub
/// first; `now` is remembered too.
fn remembered(now: Vec<String>) -> Vec<String> {
    remembered_in(&mut SEEN.lock().unwrap_or_else(|e| e.into_inner()), now)
}

/// [`remembered`] with `seen` as the memory. Every registry in `now` is
/// always in the answer; past [`MAX_SEEN`] the oldest of those no longer
/// in the store are forgotten. Docker Hub is always there, and takes no
/// place in the memory.
fn remembered_in(seen: &mut Vec<String>, now: Vec<String>) -> Vec<String> {
    for r in &now {
        if r != "docker.io" && !seen.contains(r) {
            seen.push(r.clone());
        }
    }
    while seen.len() > MAX_SEEN {
        let Some(i) = seen.iter().position(|r| !now.contains(r)) else { break };
        seen.remove(i);
    }
    let mut names = vec!["docker.io".to_owned()];
    names.extend(seen.iter().cloned());
    names
}

/// Those of `names` reached without verified TLS by `c`: what buildkitd.toml
/// marks `http` and `insecure` beyond the hosts `c` names itself (a network
/// entry covers a registry only once an image from it has been pulled).
pub fn unverified(c: &kiri_image::RegistryConfig, names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = names.iter().filter(|n| c.is_unverified(n)).cloned().collect();
    out.sort();
    out.dedup();
    out
}

/// buildkitd.toml's registry tables, for `c` (daemon.json's settings) and
/// `names` ([`registry_names`]).
pub fn config(engine: &Engine, c: &kiri_image::RegistryConfig, names: &[String]) -> String {
    let mut tables: Vec<(String, Table)> = Vec::new();
    if let Some(port) = LISTENING.get().copied().flatten() {
        let host = format!("{}:{port}", mirror_host(&engine.lock_net()));
        let token = token();
        for name in names {
            add_mirror(&mut tables, name, format!("{host}/{token}/{name}"));
        }
        table(&mut tables, &host).http = Some(true);
    }
    registries(&mut tables, c, names);
    let mut out = String::new();
    for (registry, t) in tables {
        out.push_str(&format!("[registry.\"{registry}\"]\n"));
        if !t.mirrors.is_empty() {
            let list = t.mirrors.iter().map(|m| format!("\"{m}\"")).collect::<Vec<_>>().join(", ");
            out.push_str(&format!("  mirrors = [{list}]\n"));
        }
        if let Some(http) = t.http {
            out.push_str(&format!("  http = {http}\n"));
        }
        if t.insecure {
            out.push_str("  insecure = true\n");
        }
    }
    out
}

/// daemon.json's part of buildkitd.toml, so a build reaches each registry
/// the way a pull does. Its Docker Hub mirrors (the usable ones:
/// `RegistryConfig::mirror_allowed`) come after Kiri's own mirror, which
/// serves images built or loaded here and answers 404 for everything else.
///
/// BuildKit (util/resolver) reads `http` and `insecure` together: both set
/// is HTTPS without a certificate check, falling back to plain HTTP when
/// the answer is not TLS (unlike a pull, a portless plain-HTTP registry on
/// port 80 does not fall back when port 443 is closed), which is what a
/// pull does for a host it probes (a listed host, a registry the store
/// names inside a listed network);
/// `http` alone is plain HTTP only; `insecure` alone (with `http = false`,
/// so `localhost` is not sent plain HTTP by default) is unchecked HTTPS
/// only. A mirror is asked over the scheme it was written with, as a pull
/// asks it: `http://` gets `http` alone, `https://` on a host that may go
/// unverified gets `insecure` alone, and one elsewhere keeps verified
/// HTTPS. A host two mirrors name with different schemes is probed. Every
/// other host keeps verified HTTPS, a plain-HTTP mirror's host included
/// (such a mirror is used only when its host is listed). BuildKit has no
/// network form, so a network reaches builds through the registries in
/// `known` (the store's), and a listed host's own table.
fn registries(tables: &mut Vec<(String, Table)>, c: &kiri_image::RegistryConfig, known: &[String]) {
    // Each mirror host and its written scheme (`Some(http)`), `None` where
    // two mirrors on it disagree.
    let mut schemes: Vec<(&str, Option<bool>)> = Vec::new();
    for mirror in c.usable_mirrors() {
        add_mirror(tables, "docker.io", mirror.buildkit());
        match schemes.iter_mut().find(|(h, _)| *h == mirror.host) {
            Some((_, s)) if *s != Some(mirror.http) => *s = None,
            Some(_) => {}
            None => schemes.push((&mirror.host, Some(mirror.http))),
        }
    }
    let probed = |t: &mut Table| {
        t.http = Some(true);
        t.insecure = true;
    };
    for (host, scheme) in &schemes {
        match scheme {
            Some(true) => table(tables, host).http = Some(true),
            Some(false) if c.is_unverified(host) => {
                let t = table(tables, host);
                t.http = Some(false);
                t.insecure = true;
            }
            Some(false) => {}
            None if c.is_unverified(host) => probed(table(tables, host)),
            None => {}
        }
    }
    let listed = c.insecure.iter().filter_map(|e| match e {
        kiri_image::Insecure::Host(h) => Some(h.as_str()),
        kiri_image::Insecure::Net(..) => None,
    });
    for host in listed.chain(known.iter().map(String::as_str)) {
        if c.is_unverified(host) && !schemes.iter().any(|(h, _)| *h == host) {
            probed(table(tables, host));
        }
    }
}

fn serve(listener: TcpListener, engine: Weak<Engine>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let Some(engine) = engine.upgrade() else { return };
        let _ = std::thread::Builder::new().name("kiri-build-mirror".into()).spawn(move || {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
            let _ = answer(conn, &engine);
        });
    }
}

/// One request, then the connection closes.
fn answer(conn: TcpStream, engine: &Engine) -> io::Result<()> {
    let mut r = BufReader::new((&conn).take(16 << 10));
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
    }
    let mut w = &conn;
    let head = |w: &mut &TcpStream, status: &str, extra: &str, len: u64| write!(w, "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {len}\r\n{extra}\r\n");
    if method != "GET" && method != "HEAD" {
        return head(&mut w, "405 Method Not Allowed", "", 0);
    }
    match lookup(engine, target.split('?').next().unwrap_or_default(), &token()) {
        Some(Found::Manifest(bytes, digest)) => {
            head(&mut w, "200 OK", &format!("Content-Type: application/vnd.oci.image.manifest.v1+json\r\nDocker-Content-Digest: {digest}\r\n"), bytes.len() as u64)?;
            if method == "GET" {
                w.write_all(&bytes)?;
            }
        }
        Some(Found::Blob(path, digest)) => {
            let mut f = std::fs::File::open(&path)?;
            let len = f.metadata()?.len();
            head(&mut w, "200 OK", &format!("Content-Type: application/octet-stream\r\nDocker-Content-Digest: {digest}\r\n"), len)?;
            if method == "GET" {
                io::copy(&mut f, &mut w)?;
            }
        }
        None => head(&mut w, "404 Not Found", "", 0)?,
    }
    w.flush()
}

#[derive(Debug)]
enum Found {
    Manifest(Vec<u8>, String),
    Blob(std::path::PathBuf, String),
}

/// An image's manifest as Kiri serves it, and its digest.
fn manifest(store: &kiri_image::Store, i: &Image) -> Option<(Vec<u8>, String)> {
    let config_size = std::fs::metadata(store.blob_path(&i.config_digest).ok()?).ok()?.len();
    let m = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": i.config_digest, "size": config_size},
        "layers": i.layers.iter().map(|l| json!({"mediaType": l.media_type, "digest": l.digest, "size": l.size})).collect::<Vec<Value>>(),
    });
    let bytes = serde_json::to_vec(&m).ok()?;
    let digest = kiri_image::store::sha256_digest(&bytes);
    Some((bytes, digest))
}

/// `/v2/<token>/<registry>/<repo…>/manifests/<tag|digest>` or `…/blobs/<digest>`.
fn parse(path: &str, token: &str) -> Option<(String, String, bool, String)> {
    let rest = path.strip_prefix("/v2/")?.strip_prefix(token)?.strip_prefix('/')?;
    let (registry, rest) = rest.split_once('/')?;
    let (repo, kind, reference) = if let Some((repo, r)) = rest.rsplit_once("/manifests/") {
        (repo, true, r)
    } else {
        let (repo, r) = rest.rsplit_once("/blobs/")?;
        (repo, false, r)
    };
    (!repo.is_empty() && !reference.is_empty() && !reference.contains('/')).then(|| (registry.to_owned(), repo.to_owned(), kind, reference.to_owned()))
}

fn lookup(engine: &Engine, path: &str, token: &str) -> Option<Found> {
    let (registry, repo, is_manifest, reference) = parse(path, token)?;
    let images = engine.store.images().ok()?;
    let in_repo = |i: &&Image| Reference::parse(&i.name).is_ok_and(|n| n.registry == registry && n.repository == repo);
    if is_manifest {
        if reference.starts_with("sha256:") {
            return images.iter().filter(in_repo).find_map(|i| manifest(&engine.store, i).filter(|(_, d)| *d == reference)).map(|(b, d)| Found::Manifest(b, d));
        }
        let wanted = Reference::parse(&format!("{registry}/{repo}:{reference}")).ok()?.short();
        let i = images.iter().find(|i| i.name == wanted)?;
        let (b, d) = manifest(&engine.store, i)?;
        return Some(Found::Manifest(b, d));
    }
    // Only blobs of that repository's images.
    let owned = images.iter().filter(in_repo).any(|i| i.config_digest == reference || i.layers.iter().any(|l| l.digest == reference));
    let path = engine.store.blob_path(&reference).ok()?;
    (owned && path.is_file()).then_some(Found::Blob(path, reference))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry the store named once keeps its builder table after its
    /// last image is removed: the builder is not restarted without it.
    #[test]
    fn a_registry_seen_once_keeps_its_builder_table() {
        let first = remembered(vec!["10.77.0.9:5000".into(), "docker.io".into()]);
        assert_eq!(first[0], "docker.io");
        assert_eq!(first.iter().filter(|r| *r == "docker.io").count(), 1);
        let later = remembered(Vec::new());
        assert!(later.contains(&"10.77.0.9:5000".to_owned()), "{later:?}");
        let c = kiri_image::RegistryConfig { insecure: vec![kiri_image::Insecure::parse("10.0.0.0/8").unwrap()], ..Default::default() };
        assert_eq!(unverified(&c, &["10.77.0.9:5000".into(), "10.77.0.9:5000".into()]), ["10.77.0.9:5000"]);
    }

    /// daemon.json's hosts merge into one table each, as BuildKit wants:
    /// every host a pull reaches unverified is http and insecure in builds
    /// too, a network covers the registries the store names inside it, and
    /// nothing else changes.
    /// However many registries were seen before, every one the store names
    /// now has its table; the oldest gone from the store make room.
    #[test]
    fn a_full_memory_still_takes_the_stores_registries() {
        let mut seen = Vec::new();
        // One image at a time, each removed before the next is pulled.
        for i in 0..MAX_SEEN + 40 {
            remembered_in(&mut seen, vec![format!("old-{i}.lan:5000")]);
        }
        assert_eq!(seen.len(), MAX_SEEN);
        let names = remembered_in(&mut seen, vec!["docker.io".into(), "new.lan:5000".into()]);
        assert!(names.contains(&"new.lan:5000".to_owned()), "the new registry has its table");
        assert_eq!(names.iter().filter(|n| *n == "docker.io").count(), 1);
        assert!(seen.len() <= MAX_SEEN && !seen.contains(&"docker.io".to_owned()));
        assert!(!seen.contains(&"old-40.lan:5000".to_owned()) && seen.contains(&format!("old-{}.lan:5000", MAX_SEEN + 39)), "the oldest went");
    }

    #[test]
    fn daemon_json_registries_share_one_table_each() {
        let c = kiri_image::RegistryConfig {
            insecure: ["registry.lan:5000", "mirror.lan:5000", "10.0.0.0/8"].iter().map(|e| kiri_image::Insecure::parse(e).unwrap()).collect(),
            mirrors: vec![kiri_image::Mirror::parse("http://mirror.lan:5000").unwrap(), kiri_image::Mirror::parse("https://m.example.com/docker.io").unwrap()],
        };
        let mut tables = Vec::new();
        add_mirror(&mut tables, "docker.io", "198.19.248.254:1234/t/docker.io".into());
        registries(&mut tables, &c, &["docker.io".into(), "10.1.2.3:5000".into(), "ghcr.io".into()]);
        let names: Vec<&str> = tables.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(names, ["docker.io", "mirror.lan:5000", "registry.lan:5000", "10.1.2.3:5000"]);
        assert_eq!(tables[0].1.mirrors, ["198.19.248.254:1234/t/docker.io", "mirror.lan:5000", "m.example.com/docker.io"]);
        // The http:// mirror straight over plain HTTP, as a pull asks it.
        assert_eq!(tables[1].1, Table { mirrors: vec![], http: Some(true), insecure: false });
        for t in &tables[2..] {
            assert_eq!(t.1, Table { mirrors: vec![], http: Some(true), insecure: true });
        }
        let mut none = Vec::new();
        registries(&mut none, &kiri_image::RegistryConfig::default(), &["ghcr.io".into()]);
        assert!(none.is_empty());
    }

    /// A mirror is reached in builds over the scheme it was written with,
    /// as a pull reaches it: an https:// one on a listed host or on this
    /// machine is unchecked HTTPS with no fall back to plain HTTP, an
    /// http:// one plain HTTP with no TLS first, an https:// one elsewhere
    /// verified HTTPS. A listed host with no scheme is probed (both set).
    #[test]
    fn a_build_reaches_a_mirror_over_its_written_scheme() {
        let c = kiri_image::RegistryConfig {
            insecure: ["tls.lan:5000", "plain.lan:5000", "both.lan:5000", "reg.lan:5000"].iter().map(|e| kiri_image::Insecure::parse(e).unwrap()).collect(),
            mirrors: ["https://tls.lan:5000", "http://plain.lan:5000", "https://127.0.0.1:5000", "https://m.example.com", "https://both.lan:5000/a", "http://both.lan:5000/b"].iter().map(|m| kiri_image::Mirror::parse(m).unwrap()).collect(),
        };
        let mut tables = Vec::new();
        registries(&mut tables, &c, &[]);
        let get = |h: &str| tables.iter().find(|(r, _)| r == h).map(|(_, t)| (t.http, t.insecure));
        assert_eq!(get("tls.lan:5000"), Some((Some(false), true)));
        assert_eq!(get("127.0.0.1:5000"), Some((Some(false), true)));
        assert_eq!(get("plain.lan:5000"), Some((Some(true), false)));
        assert_eq!(get("both.lan:5000"), Some((Some(true), true)));
        assert_eq!(get("reg.lan:5000"), Some((Some(true), true)));
        assert_eq!(get("m.example.com"), None);
    }

    /// A plain-HTTP mirror on a host nobody listed: a pull does not use it,
    /// and neither does a build, and its host stays verified HTTPS for
    /// everything else (`FROM mirror.lan:5000/x`, a push).
    #[test]
    fn a_build_and_a_pull_agree_on_a_plain_http_mirror() {
        let c = kiri_image::RegistryConfig { mirrors: vec![kiri_image::Mirror::parse("http://mirror.lan:5000").unwrap()], ..Default::default() };
        let mut tables = Vec::new();
        registries(&mut tables, &c, &[]);
        assert!(tables.is_empty(), "{tables:?}");
        assert_eq!(c.usable_mirrors().count(), 0);
    }

    #[test]
    fn parses_mirror_paths() {
        assert_eq!(parse("/v2/tok/docker.io/library/myapp/manifests/dev", "tok"), Some(("docker.io".into(), "library/myapp".into(), true, "dev".into())));
        assert_eq!(parse("/v2/tok/ghcr.io/o/r/blobs/sha256:ab", "tok"), Some(("ghcr.io".into(), "o/r".into(), false, "sha256:ab".into())));
        assert_eq!(parse("/v2/wrong/docker.io/library/a/manifests/x", "tok"), None);
        assert_eq!(parse("/v2/tok/docker.io/manifests/x", "tok"), None);
        assert_eq!(parse("/v2/", "tok"), None);
    }

    #[test]
    fn mirror_host_matches_the_host_network() {
        let dir = std::env::temp_dir().join(format!("kiri-mirror-host-{}", std::process::id()));
        let networks = super::super::network::Networks::load(&dir).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(mirror_host(&networks), super::super::network::ip_str(networks.find("bridge").unwrap().gateway));
        #[cfg(not(target_os = "linux"))]
        assert_eq!(mirror_host(&networks), super::super::ports::HOST_ADDR);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_file_is_private_from_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir().join(format!("kiri-mirror-token-{}", std::process::id()));
        // A leftover readable by everyone is replaced, not reused with its mode.
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_token(&p, "0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "0123456789abcdef0123456789abcdef");
        let _ = std::fs::remove_file(&p);
    }
}
