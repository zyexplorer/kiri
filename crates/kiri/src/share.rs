//! `kiri share`: a public HTTPS URL for a local port or container, through
//! the Kiri Share edge (`edge.runkiri.com`, tunnels on `*.runkiri.cloud`).
//!
//! # API (used by the CLI below and by the daemon for the UI)
//!
//! - [`resolve`] turns `3000`, `host:port` or a container name into a [`Target`].
//! - [`start`]`(`[`Options`]`, on_event)` opens a tunnel on its own threads and
//!   returns a [`Handle`]; [`Handle::stop`] closes it. It reconnects by itself
//!   until stopped, refused, or told by the edge not to.
//! - `on_event` gets every [`Event`]: `Connecting`, `Live { url, … }`,
//!   `Request { method, path, status, ms, bytes_in, bytes_out, at }`,
//!   `Notice`, `Reconnecting`, and finally `Ended { reason, error }`.
//! - Account: [`load`] / [`save`] `~/.kiri/share.json` ([`Config`]),
//!   [`login_dev`], [`login_start`] + [`login_poll`] (GitHub device flow),
//!   [`logout`], [`whoami`].
//!
//! # How it works
//!
//! One TLS connection to the edge carries every visitor connection as a
//! numbered stream (`kiri-share-proto`). Threads, not async: one reads the
//! edge connection, and each stream has one thread per direction to the
//! local service. Writes go through a `Mutex<ClientConnection>`; the reader
//! reads the raw socket without the lock and only takes it to decrypt.
//! Per-stream window credit bounds what any stream can buffer.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::io::{self, IsTerminal, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use kiri_share_proto::{self as proto, Msg};
use rustls::pki_types::ServerName;
use rustls::pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[cfg(unix)]
use crate::client;

const DEFAULT_SERVER: &str = "edge.runkiri.com:443";
/// The edge pings every 30 s; three missed pings means the link is dead.
const IDLE: Duration = Duration::from_secs(90);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const LOCAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

// From build.rs: the release's build key, if this is an official build.
include!(concat!(env!("OUT_DIR"), "/share_build_key.rs"));

/// The build key official releases answer the edge's challenge with (see
/// `kiri_share_proto::attest`), as its id and bytes. `None` in builds made
/// without `KIRI_SHARE_BUILD_KEY`; their tunnels show as unverified.
fn build_key() -> Option<(&'static str, [u8; proto::attest::KEY_LEN])> {
    let (id, masked, pad) = std::hint::black_box(BUILD_KEY)?;
    let mut key = [0u8; proto::attest::KEY_LEN];
    for (k, (m, p)) in key.iter_mut().zip(masked.iter().zip(pad)) {
        *k = m ^ p;
    }
    Some((id, key))
}

// ---------------------------------------------------------------- config

/// `~/.kiri/share.json`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Edge to use instead of edge.runkiri.com, as `host[:port]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// Random id of this install, so a ban can follow the machine.
    #[serde(default)]
    pub device: String,
}

fn config_path() -> io::Result<PathBuf> {
    if let Some(p) = std::env::var_os("KIRI_SHARE_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(home.join(".kiri/share.json"))
}

// ---------------------------------------------------------------- traffic file

/// What a `kiri share` on this computer has served, for the window, which
/// can't see another process's traffic: `~/.kiri/shares/<pid>.json` as
/// `{url, requests, in, out}`, beside the account file. Written every two
/// seconds while it changes, and removed when the share ends; one left by a
/// share that was killed matches no live link, and goes after a day.
struct TrafficFile {
    path: PathBuf,
    url: Mutex<String>,
    counts: [AtomicU64; 3],
    dirty: AtomicBool,
    done: AtomicBool,
}

impl TrafficFile {
    fn start() -> Option<Arc<TrafficFile>> {
        let dir = traffic_dir().ok()?;
        std::fs::create_dir_all(&dir).ok()?;
        // Old ones from shares that were killed.
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| {
                t.elapsed()
                    .is_ok_and(|age| age > Duration::from_secs(86_400))
            }) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let f = Arc::new(TrafficFile {
            path: dir.join(format!("{}.json", std::process::id())),
            url: Mutex::new(String::new()),
            counts: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            dirty: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        let w = f.clone();
        std::thread::spawn(move || {
            while !w.done.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(2));
                if w.dirty.swap(false, Ordering::SeqCst) && !w.done.load(Ordering::SeqCst) {
                    w.write();
                }
            }
        });
        Some(f)
    }

    fn write(&self) {
        let [r, i, o] = &self.counts;
        let v = json!({"url": *self.url.lock().unwrap(), "requests": r.load(Ordering::Relaxed), "in": i.load(Ordering::Relaxed), "out": o.load(Ordering::Relaxed)});
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, v.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    fn on(&self, e: &Event) {
        match e {
            Event::Live { url, .. } => {
                *self.url.lock().unwrap() = url.clone();
                self.write();
            }
            Event::Request(r) => {
                for (c, n) in self.counts.iter().zip([1, r.bytes_in, r.bytes_out]) {
                    c.fetch_add(n, Ordering::Relaxed);
                }
                self.dirty.store(true, Ordering::SeqCst);
            }
            Event::Ended { .. } => {
                self.done.store(true, Ordering::SeqCst);
                let _ = std::fs::remove_file(&self.path);
            }
            _ => {}
        }
    }
}

fn traffic_dir() -> io::Result<PathBuf> {
    Ok(config_path()?
        .parent()
        .map(|p| p.join("shares"))
        .unwrap_or_else(|| PathBuf::from("shares")))
}

/// Every `kiri share` traffic file on this computer, by link: `url → [requests, in, out]`.
pub fn traffic() -> HashMap<String, [u64; 3]> {
    traffic_dir().map(|d| traffic_in(&d)).unwrap_or_default()
}

fn traffic_in(dir: &std::path::Path) -> HashMap<String, [u64; 3]> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
    {
        let Ok(v) = std::fs::read(e.path())
            .map(|b| serde_json::from_slice::<Value>(&b).unwrap_or_default())
        else {
            continue;
        };
        if let Some(url) = v["url"].as_str().filter(|u| !u.is_empty()) {
            out.insert(
                url.to_owned(),
                ["requests", "in", "out"].map(|k| v[k].as_u64().unwrap_or(0)),
            );
        }
    }
    out
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_err()
    {
        // Not expected on unix; fall back to the hasher's random keys.
        for (i, b) in buf.iter_mut().enumerate() {
            *b = std::collections::hash_map::RandomState::new().hash_one(i) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// The edge shows a computer's tunnels under this: the same for the app
/// and `kiri share` on one computer, though their device ids can differ
/// (Kiri Dev keeps its own config). A hash of the hardware id, never the id
/// itself; the device id when there is none.
pub fn machine_id(device: &str) -> String {
    static RAW: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let raw = RAW.get_or_init(|| {
        #[cfg(target_os = "macos")]
        let id = std::process::Command::new("/usr/sbin/ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()
            .and_then(|o| platform_uuid(&String::from_utf8_lossy(&o.stdout)).map(str::to_owned));
        #[cfg(not(target_os = "macos"))]
        let id = std::fs::read_to_string("/etc/machine-id")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        id
    });
    match raw {
        Some(r) => machine_hash(r),
        None => device.to_owned(),
    }
}

fn machine_hash(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("kiri-share-machine:{raw}").as_bytes())
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `IOPlatformUUID` from `ioreg -rd1 -c IOPlatformExpertDevice`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn platform_uuid(ioreg: &str) -> Option<&str> {
    let line = ioreg.lines().find(|l| l.contains("\"IOPlatformUUID\""))?;
    let v = line.split('=').nth(1)?.trim().trim_matches('"');
    (!v.is_empty()).then_some(v)
}

/// This computer's name as people know it: the Mac's Computer Name, else
/// the hostname.
pub fn host_name() -> String {
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let run = |cmd: &str, args: &[&str]| {
            std::process::Command::new(cmd)
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        #[cfg(target_os = "macos")]
        let name =
            run("/usr/sbin/scutil", &["--get", "ComputerName"]).filter(|n| !n.trim().is_empty());
        #[cfg(not(target_os = "macos"))]
        let name = std::env::var("COMPUTERNAME").ok();
        let name = name
            .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
            .or_else(|| run("hostname", &[]))
            .unwrap_or_default();
        clean_host(&name)
    })
    .clone()
}

/// One line of at most 64 characters, no control characters.
fn clean_host(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect()
}

/// Reads the config, creating the device id on first use.
pub fn load() -> io::Result<Config> {
    let path = config_path()?;
    let mut cfg: Config = match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b)
            .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e),
    };
    if cfg.device.is_empty() {
        cfg.device = random_hex(16);
        save(&cfg)?;
    }
    // For CI: a token from the dashboard, without signing in.
    if let Ok(t) = std::env::var("KIRI_SHARE_TOKEN")
        && !t.is_empty()
    {
        cfg.token = Some(t);
    }
    Ok(cfg)
}

/// The config as it is, without creating it (or a device id): for
/// diagnostics, which must not leave state behind. `KIRI_SHARE_TOKEN` wins,
/// as in [`load`].
#[cfg_attr(not(unix), allow(dead_code))]
pub fn read_config() -> Config {
    let mut cfg: Config = config_path()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if let Ok(t) = std::env::var("KIRI_SHARE_TOKEN")
        && !t.is_empty()
    {
        cfg.token = Some(t);
    }
    cfg
}

/// Writes the config atomically, readable only by this user (it holds the token).
pub fn save(cfg: &Config) -> io::Result<()> {
    let path = config_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(&tmp)?;
    f.write_all(
        serde_json::to_string_pretty(cfg)
            .map_err(io::Error::other)?
            .as_bytes(),
    )?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)
}

// ---------------------------------------------------------------- the edge

#[derive(Debug, Clone, PartialEq, Eq)]
struct Server {
    host: String,
    port: u16,
    /// Where to dial, when it isn't `host:port` (`KIRI_SHARE_CONNECT`, for tests).
    dial: String,
}

fn parse_server(s: &str) -> (String, u16) {
    let s = s
        .trim()
        .trim_start_matches("https://")
        .trim_end_matches('/');
    match s.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h.to_ascii_lowercase(), p.parse().unwrap_or(443))
        }
        _ => (s.to_ascii_lowercase(), 443),
    }
}

fn server(cfg: &Config) -> Server {
    let raw = std::env::var("KIRI_SHARE_SERVER")
        .ok()
        .or_else(|| cfg.server.clone())
        .unwrap_or_else(|| DEFAULT_SERVER.to_owned());
    let (host, port) = parse_server(&raw);
    let dial = std::env::var("KIRI_SHARE_CONNECT").unwrap_or_else(|_| format!("{host}:{port}"));
    Server { host, port, dial }
}

fn tls_config(alpn: &[u8]) -> io::Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(ca) = std::env::var_os("KIRI_SHARE_CA") {
        let certs = rustls::pki_types::CertificateDer::pem_file_iter(&ca)
            .map_err(|e| io::Error::other(format!("KIRI_SHARE_CA: {e}")))?;
        for c in certs {
            roots
                .add(c.map_err(|e| io::Error::other(format!("KIRI_SHARE_CA: {e}")))?)
                .map_err(|e| io::Error::other(format!("KIRI_SHARE_CA: {e}")))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut c = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    c.alpn_protocols = vec![alpn.to_vec()];
    Ok(Arc::new(c))
}

fn dial(srv: &Server, alpn: &[u8]) -> io::Result<(TcpStream, rustls::ClientConnection)> {
    let mut last = io::Error::other(format!("{} has no address", srv.dial));
    let mut sock = None;
    for addr in srv.dial.to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                sock = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let mut sock = sock.ok_or(last)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    let name = ServerName::try_from(srv.host.clone())
        .map_err(|e| io::Error::other(format!("{}: {e}", srv.host)))?;
    let mut conn =
        rustls::ClientConnection::new(tls_config(alpn)?, name).map_err(io::Error::other)?;
    // Frames are flushed to the socket as soon as they're written.
    conn.set_buffer_limit(None);
    while conn.is_handshaking() {
        conn.complete_io(&mut sock)?;
    }
    Ok((sock, conn))
}

/// One HTTPS request to the edge's API. Returns the status and JSON body.
fn api(
    srv: &Server,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> io::Result<(u16, Value)> {
    let (sock, conn) = dial(srv, b"http/1.1")?;
    sock.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut s = rustls::StreamOwned::new(conn, sock);
    let payload = body.map(Value::to_string).unwrap_or_default();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: kiri/{}\r\nAccept: application/json\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        srv.host,
        env!("CARGO_PKG_VERSION"),
        payload.len()
    )?;
    s.flush()?;
    let mut raw = Vec::new();
    match s.read_to_end(&mut raw) {
        Ok(_) => {}
        // Servers often close without TLS close_notify after `Connection: close`.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !raw.is_empty() => {}
        Err(e) => return Err(e),
    }
    parse_http_response(&raw)
}

fn parse_http_response(raw: &[u8]) -> io::Result<(u16, Value)> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "the Kiri Share server sent a malformed answer",
        )
    };
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(bad)?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad())?;
    let status = parse_status_line(head.as_bytes()).ok_or_else(bad)?;
    let mut body = raw[split + 4..].to_vec();
    if head
        .lines()
        .any(|l| l.to_ascii_lowercase().replace(' ', "") == "transfer-encoding:chunked")
    {
        body = dechunk(&body).ok_or_else(bad)?;
    }
    Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = b.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&b[..line_end]).ok()?;
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        b = &b[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(b.get(..size)?);
        b = b.get(size + 2..)?;
    }
}

fn api_error(v: &Value, status: u16) -> io::Error {
    io::Error::other(
        v["error"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("the Kiri Share server answered {status}")),
    )
}

/// Kiri Cloud's diagnostics intake, `POST /v1/reports` on the same edge
/// (`KIRI_SHARE_SERVER` and the config's `server` move it, as for sharing).
/// `signed` sends the Kiri Cloud token when there is one, so a report the
/// person sent can be answered; crashes always go without it, so they never
/// name the person's GitHub account. See `crate::diag::report` (and
/// crates/kiri-win/src/diag.rs on Windows).
pub fn post_report(body: &Value, signed: bool) -> io::Result<(u16, Value)> {
    let cfg = read_config();
    api(
        &server(&cfg),
        "POST",
        "/v1/reports",
        Some(body),
        cfg.token.as_deref().filter(|_| signed),
    )
}

// ---------------------------------------------------------------- account

/// Signs in without GitHub. Only edges started with `--dev` allow it.
pub fn login_dev(login: &str) -> io::Result<String> {
    let mut cfg = load()?;
    let (status, v) = api(
        &server(&cfg),
        "POST",
        "/v1/login/dev",
        Some(&json!({ "login": login, "device": cfg.device })),
        None,
    )?;
    finish_login(&mut cfg, status, &v)
}

fn finish_login(cfg: &mut Config, status: u16, v: &Value) -> io::Result<String> {
    let (Some(token), Some(login)) = (v["token"].as_str(), v["login"].as_str()) else {
        return Err(api_error(v, status));
    };
    cfg.token = Some(token.to_owned());
    cfg.login = Some(login.to_owned());
    save(cfg)?;
    Ok(login.to_owned())
}

/// GitHub device flow, step one: the code the person types at `verification_uri`.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Duration,
    pub expires: Instant,
    device_code: String,
}

pub fn login_start() -> io::Result<DeviceCode> {
    let cfg = load()?;
    let (status, v) = api(
        &server(&cfg),
        "POST",
        "/v1/login/start",
        Some(&json!({})),
        None,
    )?;
    let (Some(device_code), Some(user_code), Some(uri)) = (
        v["device_code"].as_str(),
        v["user_code"].as_str(),
        v["verification_uri"].as_str(),
    ) else {
        return Err(api_error(&v, status));
    };
    Ok(DeviceCode {
        device_code: device_code.to_owned(),
        user_code: user_code.to_owned(),
        verification_uri: uri.to_owned(),
        interval: Duration::from_secs(
            v["interval"]
                .as_u64()
                .unwrap_or(5)
                .clamp(1, MAX_DEVICE_CODE_SECS),
        ),
        expires: device_code_deadline(Instant::now(), v["expires_in"].as_u64().unwrap_or(900)),
    })
}

/// The longest a sign-in code is waited on, and the longest wait between
/// polls, whatever the edge says: its numbers are untrusted, and
/// `u64::MAX` seconds overflowed `Instant + Duration` (a panic).
const MAX_DEVICE_CODE_SECS: u64 = 3600;

fn device_code_deadline(now: Instant, expires_in: u64) -> Instant {
    let ttl = Duration::from_secs(expires_in.min(MAX_DEVICE_CODE_SECS));
    now.checked_add(ttl).unwrap_or(now)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginPoll {
    /// Not approved yet; ask again after this long.
    Pending(Duration),
    /// Signed in as this GitHub login; the token is saved.
    Done(String),
}

/// GitHub device flow, step two. Call every `interval` until `Done`.
pub fn login_poll(code: &DeviceCode) -> io::Result<LoginPoll> {
    if Instant::now() > code.expires {
        return Err(io::Error::other(
            "the sign-in code expired; run `kiri share login` again",
        ));
    }
    let mut cfg = load()?;
    let (status, v) = api(
        &server(&cfg),
        "POST",
        "/v1/login/poll",
        Some(&json!({ "device_code": code.device_code, "device": cfg.device })),
        None,
    )?;
    if status == 202 {
        let extra = if v["slow_down"].as_bool() == Some(true) {
            Duration::from_secs(5)
        } else {
            Duration::ZERO
        };
        return Ok(LoginPoll::Pending(code.interval + extra));
    }
    finish_login(&mut cfg, status, &v).map(LoginPoll::Done)
}

pub fn logout() -> io::Result<()> {
    let mut cfg = load()?;
    cfg.token = None;
    cfg.login = None;
    save(&cfg)
}

/// The account and its limits, as the edge reports them.
pub fn whoami() -> io::Result<Value> {
    let cfg = load()?;
    let token = cfg
        .token
        .as_deref()
        .ok_or_else(|| io::Error::other("not signed in; run `kiri share login`"))?;
    let (status, v) = api(&server(&cfg), "GET", "/v1/whoami", None, Some(token))?;
    if status == 401 {
        return Err(io::Error::other(
            "your sign-in is no longer valid; run `kiri share login`",
        ));
    }
    if status != 200 {
        return Err(api_error(&v, status));
    }
    Ok(v)
}

// ---------------------------------------------------------------- reserved names

/// A signed-in API call: the saved token, and a plain message when it has expired.
fn signed_in(method: &str, path: &str, body: Option<&Value>) -> io::Result<(u16, Value)> {
    let cfg = load()?;
    let token = cfg
        .token
        .as_deref()
        .ok_or_else(|| io::Error::other("not signed in; run `kiri share login`"))?;
    let (status, v) = api(&server(&cfg), method, path, body, Some(token))?;
    if status == 401 {
        return Err(io::Error::other(
            "your sign-in is no longer valid; run `kiri share login`",
        ));
    }
    Ok((status, v))
}

/// The account's reserved names, how many its plan allows, and names Kiri
/// Cloud recently didn't approve.
#[derive(Debug, Clone, Default)]
pub struct Names {
    pub items: Vec<ReservedName>,
    pub limit: Option<u64>,
    pub refused: Vec<RefusedName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedName {
    pub name: String,
    pub url: String,
    pub in_use: bool,
    /// Asked for and waiting for review: held for this account, but shares
    /// get a random address until it's approved.
    pub pending: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedName {
    pub name: String,
    pub reason: String,
}

fn reserved_name(v: &Value) -> ReservedName {
    ReservedName {
        name: v["name"].as_str().unwrap_or_default().to_owned(),
        url: v["url"].as_str().unwrap_or_default().to_owned(),
        in_use: v["in_use"].as_bool().unwrap_or(false),
        // Edges from before review send no status: approved.
        pending: v["status"].as_str() == Some("pending"),
        created_at: v["created_at"].as_str().unwrap_or_default().to_owned(),
    }
}

pub fn names() -> io::Result<Names> {
    let (status, v) = signed_in("GET", "/v1/names", None)?;
    if status != 200 {
        return Err(api_error(&v, status));
    }
    let refused = v["refused"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| RefusedName {
            name: r["name"].as_str().unwrap_or_default().to_owned(),
            reason: r["reason"].as_str().unwrap_or_default().to_owned(),
        })
        .collect();
    Ok(Names {
        items: v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(reserved_name)
            .collect(),
        limit: v["limit"].as_u64(),
        refused,
    })
}

/// Asks to reserve `name` for this account. Kiri Cloud reviews each name
/// (202, `pending`); once approved `--name NAME` gets exactly
/// `NAME.<tunnel domain>`, and nobody else can use it.
pub fn reserve(name: &str) -> io::Result<ReservedName> {
    let (status, v) = signed_in("POST", "/v1/names", Some(&json!({ "name": name })))?;
    match status {
        200..=202 => Ok(reserved_name(&v)),
        403 if v["error"]
            .as_str()
            .is_some_and(|e| e.contains("isn't open")) =>
        {
            Err(api_error(&v, status))
        }
        403 => Err(io::Error::other(format!(
            "{}. Release one with `kiri share release NAME`, or see your names with `kiri share names`.",
            v["error"]
                .as_str()
                .unwrap_or("your plan's reserved names are all in use")
                .trim_end_matches('.')
        ))),
        _ => Err(api_error(&v, status)),
    }
}

pub fn release(name: &str) -> io::Result<()> {
    let (status, v) = signed_in(
        "DELETE",
        &format!("/v1/names/{}", name.to_ascii_lowercase()),
        None,
    )?;
    match status {
        200 | 204 => Ok(()),
        404 => Err(io::Error::other(format!(
            "`{name}` isn't one of your reserved names"
        ))),
        _ => Err(api_error(&v, status)),
    }
}

// ---------------------------------------------------------------- the account's tunnels

/// A live tunnel on the account, from any of its computers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveTunnel {
    pub name: String,
    /// The name without the suffix the edge added: `flower-demo`.
    pub label: String,
    pub url: String,
    /// See [`machine_id`]; empty from clients older than it.
    pub machine: String,
    pub device: String,
    pub host: String,
    /// `app` or `cli`.
    pub kind: String,
    /// What opened it, as it told the edge: `Kiri Dev 0.9.4`, `kiri 0.9.4`.
    pub client: String,
    pub target: String,
    /// Unix seconds.
    pub started: u64,
}

/// The account's live tunnels and how many its plan allows at once.
#[derive(Debug, Clone, Default)]
pub struct LiveTunnels {
    pub items: Vec<LiveTunnel>,
    pub limit: Option<u64>,
}

fn live_tunnel(v: &Value) -> LiveTunnel {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
    LiveTunnel {
        name: s("name"),
        label: Some(s("label"))
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| s("name")),
        url: s("url"),
        machine: s("machine"),
        device: s("device"),
        host: s("host"),
        kind: s("kind"),
        client: s("client"),
        target: s("target"),
        started: v["started"].as_u64().unwrap_or(0),
    }
}

/// Every live tunnel on the account, whichever computer opened it.
pub fn tunnels() -> io::Result<LiveTunnels> {
    let (status, v) = signed_in("GET", "/v1/tunnels/live", None)?;
    match status {
        200 => Ok(LiveTunnels {
            items: v["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(live_tunnel)
                .collect(),
            limit: v["limit"].as_u64(),
        }),
        404 => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Kiri Cloud can't list tunnels",
        )),
        _ => Err(api_error(&v, status)),
    }
}

/// Closes one of the account's tunnels, wherever it runs. The edge tells
/// the computer running it that it was stopped from this one.
pub fn close_tunnel(name: &str) -> io::Result<()> {
    let (status, v) = signed_in(
        "DELETE",
        &format!("/v1/tunnels/{name}"),
        Some(&json!({ "host": host_name() })),
    )?;
    match status {
        200 | 204 => Ok(()),
        404 => Err(io::Error::other(format!("{name} isn't live any more"))),
        _ => Err(api_error(&v, status)),
    }
}

/// What `kiri PORT --name NAME` did about reserving NAME first.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AutoReserve {
    /// Already this account's.
    Mine,
    /// This account asked for it and it's waiting for review.
    Waiting,
    /// Reserved just now (an edge from before review).
    Reserved(String),
    /// Asked for just now; waiting for review.
    Requested(String),
    /// Every slot is used: the tunnel gets NAME-xxxxxx.
    Full(u64),
    /// Someone else's, blocked or invalid: the edge decides the address.
    Unavailable,
}

/// Reserves NAME when it's free and a slot is left, so `--name` gives the
/// exact address. Never stops the tunnel: any failure just leaves the edge to
/// pick the address as before.
fn auto_reserve(name: &str) -> io::Result<AutoReserve> {
    let name = name.to_ascii_lowercase();
    let mine = names()?;
    if let Some(n) = mine.items.iter().find(|n| n.name == name) {
        return Ok(if n.pending {
            AutoReserve::Waiting
        } else {
            AutoReserve::Mine
        });
    }
    if let Some(limit) = mine.limit
        && mine.items.len() as u64 >= limit
    {
        return Ok(AutoReserve::Full(limit));
    }
    let (status, v) = signed_in("GET", &format!("/v1/names/check?name={name}"), None)?;
    if status != 200 || v["available"].as_bool() != Some(true) {
        return Ok(AutoReserve::Unavailable);
    }
    Ok(match reserve(&name) {
        Ok(r) if r.pending => AutoReserve::Requested(r.url),
        Ok(r) => AutoReserve::Reserved(r.url),
        Err(_) => AutoReserve::Unavailable,
    })
}

// ---------------------------------------------------------------- targets

/// What a tunnel forwards to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `host:port` to connect to for each visitor connection.
    pub addr: String,
    /// Suggested first label of the public hostname (a container name), or empty.
    pub label: String,
    /// How to describe it to people, such as `localhost:3000` or `web (localhost:8080)`.
    pub display: String,
}

/// Whether `kiri ARG` means "share this": a port number or `host:port`.
/// No Kiri or docker command looks like either.
pub fn is_shorthand(arg: &str) -> bool {
    port_or_addr(arg).is_some()
}

fn port_or_addr(arg: &str) -> Option<Target> {
    if !arg.is_empty() && arg.len() <= 5 && arg.bytes().all(|b| b.is_ascii_digit()) {
        let port: u16 = arg.parse().ok().filter(|p| *p > 0)?;
        let addr = format!("localhost:{port}");
        return Some(Target {
            addr: addr.clone(),
            label: String::new(),
            display: addr,
        });
    }
    let (host, port) = arg.rsplit_once(':')?;
    let port: u16 = port.parse().ok().filter(|p| *p > 0)?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0");
    let label = if local || host.parse::<std::net::IpAddr>().is_ok() {
        String::new()
    } else {
        host.split('.').next().unwrap_or("").to_owned()
    };
    let addr = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Some(Target {
        addr: addr.clone(),
        label,
        display: addr,
    })
}

/// The first published TCP port of a container, as Docker's inspect JSON
/// describes it; lowest container port first.
fn published_port(inspect: &Value) -> Option<String> {
    let ports = inspect["NetworkSettings"]["Ports"].as_object()?;
    let mut found: Vec<(u16, String)> = Vec::new();
    for (key, bindings) in ports {
        let Some(cport) = key.strip_suffix("/tcp").and_then(|p| p.parse::<u16>().ok()) else {
            continue;
        };
        for b in bindings.as_array().into_iter().flatten() {
            let Some(hport) = b["HostPort"].as_str().filter(|p| !p.is_empty()) else {
                continue;
            };
            let host = match b["HostIp"].as_str().unwrap_or("") {
                "" | "0.0.0.0" | "::" => "localhost".to_owned(),
                ip if ip.contains(':') => format!("[{ip}]"),
                ip => ip.to_owned(),
            };
            found.push((cport, format!("{host}:{hport}")));
        }
    }
    found.sort();
    found.into_iter().next().map(|(_, a)| a)
}

/// `3000`, `host:port`, or the name of a Kiri container with a published port.
pub fn resolve(arg: &str) -> io::Result<Target> {
    if let Some(t) = port_or_addr(arg) {
        return Ok(t);
    }
    container_target(arg)
}

/// [`resolve`] with the container lookup supplied: `inspect` answers the
/// engine's `GET /containers/{name}/json`, `Ok(None)` when there is no such
/// container. The windows (Mac and Windows) look containers up through
/// their own clients.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn resolve_via(
    arg: &str,
    inspect: impl FnOnce(&str) -> io::Result<Option<Value>>,
) -> io::Result<Target> {
    if let Some(t) = port_or_addr(arg) {
        return Ok(t);
    }
    match inspect(arg)? {
        Some(c) => container_from(arg, &c),
        None => Err(io::Error::other(format!(
            "no port or container named `{arg}`"
        ))),
    }
}

#[cfg(unix)]
fn container_target(arg: &str) -> io::Result<Target> {
    if !client::alive() {
        return Err(io::Error::other(format!(
            "`{arg}` isn't a port, and Kiri isn't running to look up a container by that name"
        )));
    }
    let r = client::request("GET", &format!("/containers/{arg}/json"), None)?;
    if r.status == 404 {
        return Err(io::Error::other(format!(
            "no port or container named `{arg}`"
        )));
    }
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    container_from(arg, &r.json())
}

/// A running container's published port as a target, from its inspect JSON.
#[cfg_attr(not(unix), allow(dead_code))]
fn container_from(arg: &str, c: &Value) -> io::Result<Target> {
    let name = c["Name"]
        .as_str()
        .unwrap_or(arg)
        .trim_start_matches('/')
        .to_owned();
    if c["State"]["Running"].as_bool() != Some(true) {
        return Err(io::Error::other(format!(
            "container `{name}` isn't running; start it with `kiri start {name}`"
        )));
    }
    let addr = published_port(c).ok_or_else(|| {
        io::Error::other(format!("container `{name}` publishes no ports; run it with -p (for example `kiri run -p 8080:80 …`) or share a port number"))
    })?;
    Ok(Target {
        display: format!("{name} ({addr})"),
        label: name,
        addr,
    })
}

/// `kiri share` on Windows has no client of the engine; Kiri's window there
/// looks containers up through [`resolve_via`].
#[cfg(windows)]
fn container_target(arg: &str) -> io::Result<Target> {
    Err(io::Error::other(format!(
        "`{arg}` isn't a port; container names are not supported by `kiri share` on Windows yet"
    )))
}

// ---------------------------------------------------------------- HTTP peeking

/// `GET /path HTTP/1.1` at the start of a stream → method and path.
pub fn parse_request_line(b: &[u8]) -> Option<(String, String)> {
    let end = b
        .iter()
        .take(8192)
        .position(|&c| c == b'\r' || c == b'\n')?;
    let line = std::str::from_utf8(&b[..end]).ok()?;
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let path = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some()
        || method.is_empty()
        || method.len() > 16
        || !method.bytes().all(|c| c.is_ascii_uppercase())
        || !version.starts_with("HTTP/")
    {
        return None;
    }
    Some((method.to_owned(), path.to_owned()))
}

/// `HTTP/1.1 200 OK` at the start of a response → 200.
pub fn parse_status_line(b: &[u8]) -> Option<u16> {
    let rest = b.strip_prefix(b"HTTP/1.")?;
    let rest = rest.get(1..)?.strip_prefix(b" ")?;
    let code = rest.get(..3)?;
    if !code.iter().all(u8::is_ascii_digit)
        || rest
            .get(3)
            .is_some_and(|&c| c != b' ' && c != b'\r' && c != b'\n')
    {
        return None;
    }
    std::str::from_utf8(code).ok()?.parse().ok()
}

// ---------------------------------------------------------------- tunnels

#[derive(Debug, Clone)]
pub struct Options {
    pub target: Target,
    /// Preferred first label of the hostname; defaults to `target.label`.
    pub name: Option<String>,
    /// Opened by the app, not `kiri share`: the edge lists it under the
    /// app's name.
    pub app: bool,
}

/// One visitor request, logged when its stream ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// 0 when the local service never answered.
    pub status: u16,
    pub ms: u64,
    /// Visitor → local service.
    pub bytes_in: u64,
    /// Local service → visitor.
    pub bytes_out: u64,
    pub at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Connecting {
        server: String,
    },
    /// `domains`: the verified custom domains that also reach this tunnel,
    /// as URLs (empty from edges that don't report them).
    Live {
        url: String,
        rate: u32,
        remaining: u64,
        warning: bool,
        domains: Vec<String>,
    },
    Request(Request),
    /// Something the edge or the local side wants the person to know.
    Notice(String),
    Reconnecting {
        reason: String,
        delay: Duration,
    },
    /// The tunnel is closed for good. `error` is false when stopped on purpose.
    Ended {
        reason: String,
        error: bool,
    },
}

/// A running tunnel. Dropping it leaves the tunnel running; call [`Handle::stop`].
#[derive(Debug)]
// `stop` and `is_finished` are for the daemon, which hosts tunnels for the UI.
#[allow(dead_code)]
pub struct Handle {
    stop: Arc<Stopper>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct Stopper {
    stopped: Mutex<bool>,
    cv: Condvar,
    /// The live edge socket, shut down to wake the reader.
    sock: Mutex<Option<TcpStream>>,
}

impl Stopper {
    fn is_stopped(&self) -> bool {
        *self.stopped.lock().unwrap()
    }

    /// Sleeps up to `d`; returns true if stopped meanwhile.
    fn sleep(&self, d: Duration) -> bool {
        let g = self.stopped.lock().unwrap();
        let (g, _) = self.cv.wait_timeout_while(g, d, |s| !*s).unwrap();
        *g
    }
}

#[allow(dead_code)]
impl Handle {
    /// Closes the tunnel and waits for its threads to wind down.
    pub fn stop(mut self) {
        self.signal();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn signal(&self) {
        *self.stop.stopped.lock().unwrap() = true;
        self.stop.cv.notify_all();
        if let Some(s) = self.stop.sock.lock().unwrap().as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Waits until the tunnel ends on its own (refused, taken down, stopped).
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

type OnEvent = Arc<dyn Fn(Event) + Send + Sync>;

/// Opens a tunnel. Fails at once only if not signed in; everything after
/// that, including the first connection, is reported through `on_event`.
pub fn start(
    opts: Options,
    on_event: impl Fn(Event) + Send + Sync + 'static,
) -> io::Result<Handle> {
    let cfg = load()?;
    let token = cfg
        .token
        .clone()
        .ok_or_else(|| io::Error::other("not signed in; run `kiri share login`"))?;
    let srv = server(&cfg);
    let stop = Arc::new(Stopper::default());
    let on_event: OnEvent = Arc::new(move |e: Event| {
        log_event(&e);
        on_event(e)
    });
    let s = stop.clone();
    let version = env!("CARGO_PKG_VERSION");
    let client = if opts.app {
        format!("{} {version}", crate::flavor::APP)
    } else {
        format!("kiri {version}")
    };
    let who = Who {
        machine: machine_id(&cfg.device),
        host: host_name(),
        device: cfg.device,
        client,
    };
    let thread = std::thread::Builder::new()
        .name("share".into())
        .spawn(move || run_tunnel(srv, token, who, opts, s, on_event))?;
    Ok(Handle {
        stop,
        thread: Some(thread),
    })
}

/// A tunnel's turns in Kiri's log: up, down, back, ended. Not its
/// requests, and never the token.
fn log_event(e: &Event) {
    #[cfg(unix)]
    match e {
        Event::Live { url, .. } => crate::diag::info!("share connected", url = url),
        Event::Reconnecting { reason, delay } => crate::diag::warn!(
            "share disconnected; reconnecting",
            reason = reason,
            delay_ms = delay.as_millis()
        ),
        Event::Ended {
            reason,
            error: true,
        } => crate::diag::error!("share ended", reason = reason),
        Event::Ended { error: false, .. } => crate::diag::info!("share stopped"),
        Event::Notice(n) => crate::diag::debug!("share notice", text = n),
        Event::Connecting { .. } | Event::Request(_) => {}
    }
    #[cfg(not(unix))]
    let _ = e;
}

enum End {
    /// Try again.
    Lost(String),
    /// Edge restarting; try again soon.
    Again(String),
    /// Don't try again.
    Final(String),
}

fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_millis(500) * 2u32.pow(attempt.min(6));
    let base = base.min(Duration::from_secs(30));
    // 50–150 %, so a fleet of clients doesn't reconnect in lockstep.
    let jitter = std::collections::hash_map::RandomState::new().hash_one(Instant::now()) % 1000;
    base * (500 + jitter as u32) / 1000
}

/// How long after losing a link its name may still look busy: the edge
/// notices a connection gone quiet only after 90 s, checked every 30 s.
const RECLAIM: Duration = Duration::from_secs(180);

/// The edge refused the name because the link's own last connection, lost a
/// moment ago, still holds it there: worth waiting for, not giving up on.
fn still_closing(reason: &str, lost: Option<Instant>) -> bool {
    lost.is_some_and(|t| t.elapsed() < RECLAIM) && reason.contains("already in use")
}

/// Who a tunnel's Hello says it is.
struct Who {
    device: String,
    machine: String,
    host: String,
    client: String,
}

fn run_tunnel(
    srv: Server,
    token: String,
    who: Who,
    opts: Options,
    stop: Arc<Stopper>,
    on_event: OnEvent,
) {
    let mut name = opts
        .name
        .clone()
        .unwrap_or_else(|| opts.target.label.clone());
    let mut attempt = 0u32;
    // When a session that was live ended.
    let mut lost = None;
    let reason = loop {
        if stop.is_stopped() {
            break None;
        }
        on_event(Event::Connecting {
            server: srv.host.clone(),
        });
        let end = match session(&srv, &token, &who, &name, &opts.target, &stop, &on_event) {
            Ok((end, issued)) => {
                if let Some(issued) = issued {
                    attempt = 0;
                    // Asking for the same name next time keeps the URL
                    // across reconnects and edge restarts.
                    name = issued;
                    lost = Some(Instant::now());
                }
                end
            }
            Err(e) => End::Lost(e.to_string()),
        };
        if stop.is_stopped() {
            break None;
        }
        let (reason, delay) = match end {
            End::Final(r) if still_closing(&r, lost) => (
                "the link's last connection is still closing".into(),
                backoff(attempt).max(Duration::from_secs(5)),
            ),
            End::Final(r) => break Some(r),
            End::Again(r) => (r, Duration::from_millis(200) + backoff(0)),
            End::Lost(r) => (r, backoff(attempt)),
        };
        attempt += 1;
        on_event(Event::Reconnecting { reason, delay });
        if stop.sleep(delay) {
            break None;
        }
    };
    match reason {
        Some(r) => on_event(Event::Ended {
            reason: r,
            error: true,
        }),
        None => on_event(Event::Ended {
            reason: "stopped".into(),
            error: false,
        }),
    }
}

struct Session {
    tls: Mutex<rustls::ClientConnection>,
    sock: TcpStream,
    streams: Mutex<HashMap<u32, Arc<Stream>>>,
    target: Target,
    on_event: OnEvent,
    /// Last time a "nothing is answering" notice went out.
    offline_notice: Mutex<Option<Instant>>,
    /// What the Hello carried, which an answer to `Challenge` is bound to.
    token: String,
    device: String,
    /// Where the extra connections dial.
    srv: Server,
    lanes: Lanes,
}

/// The connections a session spreads stream bytes over when the edge allows
/// more than one (see `kiri_share_proto`, Lanes). Empty until `Welcome`, and
/// for edges that don't: then streams send `Data` on the first connection.
#[derive(Default)]
struct Lanes {
    list: Mutex<Vec<Arc<Lane>>>,
    /// Bytes a stream may have in flight; 0 until `Welcome`.
    window: AtomicU32,
    turn: AtomicUsize,
    /// Extra connections' sockets, shut down when the session ends.
    socks: Mutex<Vec<TcpStream>>,
    over: AtomicBool,
}

/// One connection's queue of `Chunk` frames, written by its own thread.
struct Lane {
    tx: mpsc::SyncSender<Vec<u8>>,
    queued: AtomicUsize,
}

/// Frames a lane's queue holds before a sender waits.
const LANE_QUEUE: usize = 64;

impl Lanes {
    fn window(&self) -> u32 {
        match self.window.load(Ordering::Acquire) {
            0 => proto::INITIAL_WINDOW,
            w => w,
        }
    }

    /// The lane with the least queued, or None when streams send `Data`.
    fn pick(&self) -> Option<Arc<Lane>> {
        let list = self.list.lock().unwrap();
        if list.is_empty() {
            return None;
        }
        let start = self.turn.fetch_add(1, Ordering::Relaxed) % list.len();
        (0..list.len())
            .map(|i| &list[(start + i) % list.len()])
            .min_by_key(|l| l.queued.load(Ordering::Relaxed))
            .cloned()
    }

    fn end(&self) {
        self.over.store(true, Ordering::Release);
        self.list.lock().unwrap().clear();
        for s in self.socks.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

impl Lane {
    /// Queues a frame; false if the lane's writer has stopped.
    fn send(&self, frame: Vec<u8>) -> bool {
        self.queued.fetch_add(frame.len(), Ordering::Relaxed);
        self.tx.send(frame).is_ok()
    }
}

/// Writes `bytes` to a TLS connection, whole.
fn write_all_tls(
    tls: &Mutex<rustls::ClientConnection>,
    sock: &TcpStream,
    mut bytes: &[u8],
) -> io::Result<()> {
    let mut c = tls.lock().unwrap();
    let mut sock = sock;
    while !bytes.is_empty() {
        let n = c.writer().write(bytes)?;
        bytes = &bytes[n..];
        while c.wants_write() {
            c.write_tls(&mut sock)?;
        }
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
    }
    Ok(())
}

struct Stream {
    to_local: mpsc::Sender<Option<Vec<u8>>>,
    credit: Mutex<u32>,
    cv: Condvar,
    reset: AtomicBool,
    local: Mutex<Option<TcpStream>>,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    request: Mutex<Option<(String, String)>>,
    status: Mutex<u16>,
}

impl Stream {
    fn kill(&self) {
        self.reset.store(true, Ordering::Release);
        self.cv.notify_all();
        if let Some(s) = self.local.lock().unwrap().as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

impl Session {
    fn send(&self, m: Msg<'_>) -> io::Result<()> {
        let mut buf = Vec::new();
        m.encode(&mut buf);
        self.send_raw(&buf)
    }

    fn send_raw(&self, bytes: &[u8]) -> io::Result<()> {
        write_all_tls(&self.tls, &self.sock, bytes)
    }

    /// Ends the session so it reconnects: a lane that dropped may have lost
    /// chunks.
    fn lost_lane(&self) {
        if !self.lanes.over.load(Ordering::Acquire) {
            let _ = self.sock.shutdown(Shutdown::Both);
        }
    }

    /// Starts a writer thread for a lane that writes with `write`, and adds it.
    fn add_lane(self: &Arc<Self>, write: impl Fn(&[u8]) -> io::Result<()> + Send + 'static) {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(LANE_QUEUE);
        let lane = Arc::new(Lane {
            tx,
            queued: AtomicUsize::new(0),
        });
        let (sess, me) = (self.clone(), lane.clone());
        let spawned = std::thread::Builder::new()
            .name("share-lane-out".into())
            .spawn(move || {
                while let Ok(f) = rx.recv() {
                    let r = write(&f);
                    me.queued.fetch_sub(f.len(), Ordering::Relaxed);
                    if r.is_err() {
                        sess.lost_lane();
                        break;
                    }
                }
            });
        if spawned.is_ok() && !self.lanes.over.load(Ordering::Acquire) {
            self.lanes.list.lock().unwrap().push(lane);
        }
    }

    /// After a `Welcome` that allows lanes: the first connection becomes a
    /// lane too, and each extra one dials and joins on its own thread.
    fn start_lanes(self: &Arc<Self>, extra: u32, key: &str) {
        let me = Arc::downgrade(self);
        self.add_lane(move |b| match me.upgrade() {
            Some(s) => s.send_raw(b),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        });
        for _ in 0..extra {
            let (sess, key) = (self.clone(), key.to_owned());
            let _ = std::thread::Builder::new()
                .name("share-lane".into())
                .spawn(move || sess.run_lane(&key));
        }
    }

    /// One extra connection: join, then answer pings until it or the
    /// session ends. One the edge refuses is simply not used.
    fn run_lane(self: Arc<Self>, key: &str) {
        let Ok((sock, conn)) = dial(&self.srv, proto::ALPN) else {
            return;
        };
        let (Ok(mut reader), Ok(keep)) = (sock.try_clone(), sock.try_clone()) else {
            return;
        };
        if self.lanes.over.load(Ordering::Acquire) {
            return;
        }
        self.lanes.socks.lock().unwrap().push(keep);
        let _ = reader.set_read_timeout(Some(IDLE));
        let conn = Arc::new((Mutex::new(conn), sock));
        let mut join = Vec::new();
        Msg::Join { key }.encode(&mut join);
        if write_all_tls(&conn.0, &conn.1, &join).is_err() {
            return;
        }
        let mut joined = false;
        let mut raw = vec![0u8; 16 * 1024];
        let mut plain: Vec<u8> = Vec::new();
        loop {
            let n = match reader.read(&mut raw) {
                Ok(n) if n > 0 => n,
                _ => break,
            };
            {
                let mut c = conn.0.lock().unwrap();
                let mut input = &raw[..n];
                while !input.is_empty() {
                    if c.read_tls(&mut input).is_err() || c.process_new_packets().is_err() {
                        drop(c);
                        return self.lane_gone(joined);
                    }
                    if let Err(e) = c.reader().read_to_end(&mut plain)
                        && e.kind() != io::ErrorKind::WouldBlock
                    {
                        drop(c);
                        return self.lane_gone(joined);
                    }
                }
            }
            let mut pos = 0;
            while plain.len() - pos >= 4 {
                let Ok(len) = proto::frame_len(plain[pos..pos + 4].try_into().unwrap()) else {
                    return self.lane_gone(joined);
                };
                if plain.len() - pos < len {
                    break;
                }
                match proto::decode(&plain[pos..pos + len]) {
                    Ok(Msg::Joined) if !joined => {
                        joined = true;
                        let c = conn.clone();
                        self.add_lane(move |b| write_all_tls(&c.0, &c.1, b));
                    }
                    Ok(Msg::Ping(v)) => {
                        let mut pong = Vec::new();
                        Msg::Pong(v).encode(&mut pong);
                        if write_all_tls(&conn.0, &conn.1, &pong).is_err() {
                            return self.lane_gone(joined);
                        }
                    }
                    // Refused: not this time. Goodbye: the session hears it too.
                    Ok(Msg::Refused(_) | Msg::Goodbye { .. }) => return,
                    _ => {}
                }
                pos += len;
            }
            plain.drain(..pos);
        }
        self.lane_gone(joined);
    }

    fn lane_gone(&self, joined: bool) {
        if joined {
            self.lost_lane();
        }
    }

    fn end_all(&self) {
        self.lanes.end();
        for (_, s) in self.streams.lock().unwrap().drain() {
            s.kill();
            let _ = s.to_local.send(None);
        }
    }

    /// Handles one frame; `Some` ends the session.
    fn handle(self: &Arc<Self>, frame: &[u8], welcomed: &mut Option<String>) -> Option<End> {
        let m = match proto::decode(frame) {
            Ok(m) => m,
            Err(e) => return Some(End::Lost(format!("bad frame from the edge: {e}"))),
        };
        match m {
            Msg::Welcome(w) => {
                *welcomed = Some(issued_name(w.url));
                // We set FEATURE_WINDOW, so the window holds with or
                // without lanes; the edge grants credit by it.
                self.lanes.window.store(w.send_window(), Ordering::Release);
                if w.lanes > 0 && !w.lane_key.is_empty() {
                    self.start_lanes(w.lanes.min(15), w.lane_key);
                }
                let domains = w
                    .domains
                    .split(',')
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_owned)
                    .collect();
                (self.on_event)(Event::Live {
                    url: w.url.to_owned(),
                    rate: w.rate,
                    remaining: w.remaining,
                    warning: w.warning,
                    domains,
                });
            }
            Msg::Refused(r) => return Some(End::Final(r.to_owned())),
            Msg::Open { stream, .. } => self.open(stream),
            Msg::Data { stream, data } => {
                let s = self.streams.lock().unwrap().get(&stream).cloned();
                if let Some(s) = s {
                    s.bytes_in.fetch_add(data.len() as u64, Ordering::Relaxed);
                    if s.to_local.send(Some(data.to_vec())).is_err() {
                        self.streams.lock().unwrap().remove(&stream);
                        s.kill();
                        let _ = self.send(Msg::Reset { stream });
                    }
                }
            }
            Msg::Window { stream, credit } => {
                if let Some(s) = self.streams.lock().unwrap().get(&stream) {
                    let mut c = s.credit.lock().unwrap();
                    *c = c.saturating_add(credit).min(self.lanes.window());
                    s.cv.notify_all();
                }
            }
            Msg::Close { stream } => {
                if let Some(s) = self.streams.lock().unwrap().get(&stream) {
                    let _ = s.to_local.send(None);
                }
            }
            Msg::Reset { stream } => {
                if let Some(s) = self.streams.lock().unwrap().remove(&stream) {
                    s.kill();
                    let _ = s.to_local.send(None);
                }
            }
            Msg::Ping(v) => {
                if let Err(e) = self.send(Msg::Pong(v)) {
                    return Some(End::Lost(e.to_string()));
                }
            }
            Msg::Pong(_) => {}
            Msg::Notice(n) => (self.on_event)(Event::Notice(n.to_owned())),
            Msg::Goodbye {
                reconnect: true,
                reason,
            } => return Some(End::Again(reason.to_owned())),
            Msg::Goodbye {
                reconnect: false,
                reason,
            } => return Some(End::Final(reason.to_owned())),
            Msg::Challenge { nonce } => {
                // Only sent to clients that offered an answer; one without a
                // key replies with an empty one rather than stall the edge.
                let (key, mac) = match build_key() {
                    Some((id, key)) => (
                        id,
                        proto::attest::proof(&key, &nonce, &self.token, &self.device),
                    ),
                    None => ("", [0; proto::attest::MAC_LEN]),
                };
                if let Err(e) = self.send(Msg::Attest { key, mac }) {
                    return Some(End::Lost(e.to_string()));
                }
            }
            Msg::Joined => {}
            Msg::Hello(_) | Msg::Attest { .. } | Msg::Join { .. } | Msg::Chunk { .. } => {
                return Some(End::Lost("unexpected frame from the edge".into()));
            }
        }
        None
    }

    /// A visitor connected: register the stream at once (data may follow
    /// before the local connection is up), then connect on its own thread.
    fn open(self: &Arc<Self>, id: u32) {
        let (tx, rx) = mpsc::channel();
        let stream = Arc::new(Stream {
            to_local: tx,
            credit: Mutex::new(self.lanes.window()),
            cv: Condvar::new(),
            reset: AtomicBool::new(false),
            local: Mutex::new(None),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            request: Mutex::new(None),
            status: Mutex::new(0),
        });
        self.streams.lock().unwrap().insert(id, stream.clone());
        let sess = self.clone();
        let spawned = std::thread::Builder::new()
            .name("share-stream".into())
            .spawn(move || sess.carry(id, stream, rx));
        if spawned.is_err() {
            self.streams.lock().unwrap().remove(&id);
            let _ = self.send(Msg::Reset { stream: id });
        }
    }

    fn connect_local(&self) -> io::Result<TcpStream> {
        let mut last = io::Error::other("no address");
        for addr in self.target.addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&addr, LOCAL_CONNECT_TIMEOUT) {
                Ok(s) => return Ok(s),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Runs one stream: this thread carries local → edge; a second carries
    /// edge → local.
    fn carry(self: Arc<Self>, id: u32, stream: Arc<Stream>, rx: mpsc::Receiver<Option<Vec<u8>>>) {
        let started = Instant::now();
        let at = SystemTime::now();
        let local = match self.connect_local() {
            Ok(s) => s,
            Err(e) => {
                self.streams.lock().unwrap().remove(&id);
                let _ = self.send(Msg::Reset { stream: id });
                // The request line has usually arrived by now; log it as unanswered.
                if let Ok(Some(first)) = rx.recv_timeout(Duration::from_millis(200))
                    && let Some((method, path)) = parse_request_line(&first)
                {
                    (self.on_event)(Event::Request(Request {
                        method,
                        path,
                        status: 0,
                        ms: started.elapsed().as_millis() as u64,
                        bytes_in: first.len() as u64,
                        bytes_out: 0,
                        at,
                    }));
                }
                let mut last = self.offline_notice.lock().unwrap();
                if last.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
                    *last = Some(Instant::now());
                    (self.on_event)(Event::Notice(format!(
                        "nothing is answering on {} ({e})",
                        self.target.addr
                    )));
                }
                return;
            }
        };
        let _ = local.set_nodelay(true);
        let (Ok(writer_sock), Ok(handle)) = (local.try_clone(), local.try_clone()) else {
            self.streams.lock().unwrap().remove(&id);
            let _ = self.send(Msg::Reset { stream: id });
            return;
        };
        *stream.local.lock().unwrap() = Some(handle);
        if stream.reset.load(Ordering::Acquire) {
            let _ = local.shutdown(Shutdown::Both);
        }

        let sess = self.clone();
        let st = stream.clone();
        let writer = std::thread::Builder::new()
            .name("share-stream-in".into())
            .spawn(move || sess.to_local(id, st, rx, writer_sock));

        let mut local = local;
        let mut buf = vec![0u8; proto::MAX_CHUNK];
        let mut first = true;
        // Over lanes, bytes go as numbered chunks on whichever lane is least busy.
        let mut seq = 0u32;
        loop {
            let allow = {
                let mut c = stream.credit.lock().unwrap();
                while *c == 0 && !stream.reset.load(Ordering::Acquire) {
                    c = stream.cv.wait(c).unwrap();
                }
                if stream.reset.load(Ordering::Acquire) {
                    break;
                }
                (*c as usize).min(proto::MAX_CHUNK)
            };
            let n = match local.read(&mut buf[..allow]) {
                Ok(0) => {
                    if !stream.reset.load(Ordering::Acquire) {
                        match self.lanes.pick() {
                            Some(lane) => {
                                lane.send(proto::chunk_header(id, seq, true, 0).to_vec());
                            }
                            None => {
                                let _ = self.send(Msg::Close { stream: id });
                            }
                        }
                    }
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    if !stream.reset.swap(true, Ordering::AcqRel) {
                        self.streams.lock().unwrap().remove(&id);
                        let _ = self.send(Msg::Reset { stream: id });
                    }
                    break;
                }
            };
            if first {
                first = false;
                if let Some(code) = parse_status_line(&buf[..n]) {
                    *stream.status.lock().unwrap() = code;
                }
            }
            *stream.credit.lock().unwrap() -= n as u32;
            let sent = match self.lanes.pick() {
                Some(lane) => {
                    let mut frame = Vec::with_capacity(proto::CHUNK_HEADER + n);
                    frame.extend_from_slice(&proto::chunk_header(id, seq, false, n));
                    frame.extend_from_slice(&buf[..n]);
                    seq = seq.wrapping_add(1);
                    lane.send(frame)
                }
                None => {
                    let mut frame = Vec::with_capacity(proto::HEADER + 4 + n);
                    frame.extend_from_slice(&proto::data_header(id, n));
                    frame.extend_from_slice(&buf[..n]);
                    self.send_raw(&frame).is_ok()
                }
            };
            if !sent {
                stream.kill();
                break;
            }
            stream.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
        }
        if let Ok(w) = writer {
            let _ = w.join();
        }
        self.streams.lock().unwrap().remove(&id);
        let request = stream.request.lock().unwrap().take();
        if let Some((method, path)) = request {
            (self.on_event)(Event::Request(Request {
                method,
                path,
                status: *stream.status.lock().unwrap(),
                ms: started.elapsed().as_millis() as u64,
                bytes_in: stream.bytes_in.load(Ordering::Relaxed),
                bytes_out: stream.bytes_out.load(Ordering::Relaxed),
                at,
            }));
        }
    }

    /// Edge → local service, returning window credit as data is written.
    fn to_local(
        &self,
        id: u32,
        stream: Arc<Stream>,
        rx: mpsc::Receiver<Option<Vec<u8>>>,
        mut local: TcpStream,
    ) {
        let mut unacked = 0u32;
        let mut first = true;
        while let Ok(msg) = rx.recv() {
            let Some(data) = msg else {
                let _ = local.shutdown(Shutdown::Write);
                break;
            };
            if first {
                first = false;
                *stream.request.lock().unwrap() = parse_request_line(&data);
            }
            if local.write_all(&data).is_err() {
                if !stream.reset.swap(true, Ordering::AcqRel) {
                    self.streams.lock().unwrap().remove(&id);
                    let _ = self.send(Msg::Reset { stream: id });
                }
                stream.kill();
                break;
            }
            unacked += data.len() as u32;
            if unacked >= proto::INITIAL_WINDOW / 4 {
                if self
                    .send(Msg::Window {
                        stream: id,
                        credit: unacked,
                    })
                    .is_err()
                {
                    break;
                }
                unacked = 0;
            }
        }
    }
}

/// The tunnel name in a Welcome URL: the first label of its host.
fn issued_name(url: &str) -> String {
    url.trim_start_matches("https://")
        .split(['.', ':', '/'])
        .next()
        .unwrap_or("")
        .to_owned()
}

/// One connection to the edge, until it ends. Returns how it ended and,
/// if the edge welcomed the tunnel, the name it issued.
fn session(
    srv: &Server,
    token: &str,
    who: &Who,
    name: &str,
    target: &Target,
    stop: &Stopper,
    on_event: &OnEvent,
) -> io::Result<(End, Option<String>)> {
    let (sock, conn) = dial(srv, proto::ALPN)?;
    if conn.alpn_protocol() != Some(proto::ALPN) {
        return Ok((
            End::Final(format!("{} isn't a Kiri Share server", srv.host)),
            None,
        ));
    }
    let mut reader = sock.try_clone()?;
    reader.set_read_timeout(Some(IDLE))?;
    *stop.sock.lock().unwrap() = Some(sock.try_clone()?);
    if stop.is_stopped() {
        let _ = sock.shutdown(Shutdown::Both);
    }
    let sess = Arc::new(Session {
        tls: Mutex::new(conn),
        sock,
        streams: Mutex::new(HashMap::new()),
        target: target.clone(),
        on_event: on_event.clone(),
        offline_notice: Mutex::new(None),
        token: token.to_owned(),
        device: who.device.clone(),
        srv: srv.clone(),
        lanes: Lanes::default(),
    });
    let features = proto::FEATURE_LANES
        | proto::FEATURE_WINDOW
        | if build_key().is_some() {
            proto::FEATURE_ATTEST
        } else {
            0
        };
    sess.send(Msg::Hello(proto::Hello {
        protocol: proto::PROTOCOL,
        token,
        device: &who.device,
        name,
        target: &target.display,
        client: &who.client,
        features,
        machine: &who.machine,
        host: &who.host,
    }))?;

    let mut welcomed = None;
    let end = read_loop(&sess, &mut reader, &mut welcomed);
    sess.end_all();
    let _ = sess.sock.shutdown(Shutdown::Both);
    *stop.sock.lock().unwrap() = None;
    Ok((end, welcomed))
}

fn read_loop(sess: &Arc<Session>, reader: &mut TcpStream, welcomed: &mut Option<String>) -> End {
    let mut raw = vec![0u8; 32 * 1024];
    let mut plain: Vec<u8> = Vec::new();
    loop {
        let n = match reader.read(&mut raw) {
            Ok(0) => return End::Lost("the edge closed the connection".into()),
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return End::Lost("the edge stopped answering".into());
            }
            Err(e) => return End::Lost(e.to_string()),
        };
        let mut closed = false;
        {
            let mut c = sess.tls.lock().unwrap();
            let mut input = &raw[..n];
            while !input.is_empty() {
                if let Err(e) = c.read_tls(&mut input) {
                    return End::Lost(e.to_string());
                }
                if let Err(e) = c.process_new_packets() {
                    return End::Lost(format!("TLS: {e}"));
                }
                match c.reader().read_to_end(&mut plain) {
                    Ok(_) => closed = true,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return End::Lost(e.to_string()),
                }
            }
            let mut sock = &sess.sock;
            while c.wants_write() {
                if c.write_tls(&mut sock).is_err() {
                    break;
                }
            }
        }
        let mut pos = 0;
        while plain.len() - pos >= 4 {
            let len = match proto::frame_len(plain[pos..pos + 4].try_into().unwrap()) {
                Ok(l) => l,
                Err(e) => return End::Lost(format!("bad frame from the edge: {e}")),
            };
            if plain.len() - pos < len {
                break;
            }
            if let Some(end) = sess.handle(&plain[pos..pos + len], welcomed) {
                return end;
            }
            pos += len;
        }
        plain.drain(..pos);
        if plain.capacity() > 256 * 1024 && plain.len() < 64 * 1024 {
            plain.shrink_to(64 * 1024);
        }
        if closed {
            return End::Lost("the edge closed the connection".into());
        }
    }
}

// ---------------------------------------------------------------- the CLI

const USAGE: &str = "\
usage: kiri share PORT | HOST:PORT | CONTAINER [NAME | --name NAME] [--no-reserve] [--json]
       kiri share reserve NAME     ask to keep NAME.runkiri.cloud for yourself
       kiri share names            your reserved names, and requests waiting for review
       kiri share release NAME     give a reserved name up, or withdraw a request
       kiri share login [--dev LOGIN]
       kiri share logout
       kiri share status [--json]

`kiri 3000` shares port 3000 at a random address; `kiri 3000 myapp` (or
--name myapp) at myapp.runkiri.cloud once it's reserved. Kiri Cloud checks each name before it's
yours, usually within a day, and emails you; until then shares get a random address. If the name is free
and your plan has a slot left, it's asked for first; --no-reserve uses it only as the start of a random
address. --json prints
each event as a line of JSON (`event`: connecting, live, request, notice,
reconnecting or ended), for programs that run a share.";

#[derive(Debug, Clone, Copy)]
struct Paint {
    on: bool,
}

impl Paint {
    fn new() -> Paint {
        Paint {
            on: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn wrap(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }

    /// Kiri's mustard, #E0B420.
    fn accent(&self, s: &str) -> String {
        self.wrap("38;2;224;180;32", s)
    }
    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn status(&self, code: u16) -> String {
        let text = if code == 0 {
            "---".to_owned()
        } else {
            code.to_string()
        };
        let color = match code {
            0 | 500.. => "31",
            400..=499 => "33",
            300..=399 => "36",
            _ => "32",
        };
        self.wrap(color, &text)
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn limits_line(rate: u32, remaining: u64, warning: bool) -> String {
    let mut parts = vec![if rate == 0 {
        "no speed limit".to_owned()
    } else {
        format!("{}/s", human_bytes(rate as u64))
    }];
    if remaining != u64::MAX {
        parts.push(format!("{} left this month", human_bytes(remaining)));
    }
    if warning {
        parts.push("browsers see a warning page first".into());
    }
    parts.join(" · ")
}

fn clock(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both variants write only into `tm`, which outlives the call.
    #[cfg(unix)]
    let failed = unsafe { libc::localtime_r(&secs, &mut tm) }.is_null();
    #[cfg(windows)]
    let failed = unsafe { libc::localtime_s(&mut tm, &secs) } != 0;
    if failed {
        return "--:--:--".into();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn shorten(path: &str, max: usize) -> String {
    if path.chars().count() <= max {
        return path.to_owned();
    }
    let mut s: String = path.chars().take(max - 1).collect();
    s.push('…');
    s
}

/// Prints events the way `kiri share` shows them.
fn printer(target: String) -> impl Fn(Event) + Send + Sync + 'static {
    let p = Paint::new();
    let shown_url = Mutex::new(None::<String>);
    let header_done = AtomicBool::new(false);
    move |e| match e {
        Event::Connecting { server } => {
            if !header_done.swap(true, Ordering::Relaxed) {
                println!();
                println!(
                    "  {}  {}",
                    p.accent(&p.bold("◆ Kiri Share")),
                    p.dim(&format!("connecting to {server}…"))
                );
            }
        }
        Event::Live {
            url,
            rate,
            remaining,
            warning,
            domains,
        } => {
            let mut shown = shown_url.lock().unwrap();
            if shown.as_deref() == Some(url.as_str()) {
                println!("  {}  {}", p.accent("●"), p.dim("back online"));
                return;
            }
            let first = shown.is_none();
            *shown = Some(url.clone());
            if !first {
                println!(
                    "  {}  {} {}",
                    p.accent("●"),
                    p.dim("new address:"),
                    p.bold(&url)
                );
                return;
            }
            println!("  {}  {:<9}{}", p.accent("●"), "linked", target);
            println!(
                "  {}  {:<9}{}",
                p.accent("●"),
                "live",
                p.accent(&p.bold(&url))
            );
            for d in &domains {
                println!(
                    "  {}  {:<9}{}",
                    p.accent("●"),
                    "domain",
                    p.accent(&p.bold(d))
                );
            }
            println!(
                "     {:<9}{}",
                "",
                p.dim(&limits_line(rate, remaining, warning))
            );
            println!();
            println!(
                "  {}",
                p.dim("Requests show up below. Press Ctrl+C to stop sharing.")
            );
            println!();
        }
        Event::Request(r) => {
            println!(
                "  {}  {:<7} {}  {:<40} {:>7}  {:>9}",
                p.dim(&clock(r.at)),
                p.bold(&r.method),
                p.status(r.status),
                shorten(&r.path, 40),
                format!("{} ms", r.ms),
                human_bytes(r.bytes_out),
            );
        }
        Event::Notice(n) => println!("  {}  {n}", p.accent("!")),
        Event::Reconnecting { reason, delay } => {
            println!(
                "  {}  {} {}",
                p.dim("↻"),
                p.dim(&reason),
                p.dim(&format!("— reconnecting in {:.1}s", delay.as_secs_f32()))
            )
        }
        Event::Ended { reason, error } => {
            if error {
                eprintln!("  {}  {reason}", p.wrap("31", "✕"));
            }
        }
    }
}

pub fn run(args: &[String]) -> io::Result<()> {
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => Err(io::Error::other(USAGE)),
        Some("login") => login_cli(&args[1..]),
        Some("logout") => {
            logout()?;
            println!("Signed out of Kiri Share.");
            Ok(())
        }
        Some("status") => status_cli(args.iter().any(|a| a == "--json")),
        Some("names") => names_cli(),
        Some("reserve") => {
            let p = Paint::new();
            let name = args
                .get(1)
                .ok_or_else(|| io::Error::other("usage: kiri share reserve NAME"))?;
            let r = reserve(name)?;
            if r.pending {
                println!("  {}  Asked for {}", p.accent("◌"), p.bold(&r.url));
                println!("     {}", p.dim("We check each name before it's yours, usually within a day, and email you when it's decided."));
                println!("     {}", p.dim("Nobody else can ask for it meanwhile. See where it stands with `kiri share names`."));
            } else {
                println!(
                    "  {}  {} is yours: {}",
                    p.accent("●"),
                    p.bold(&r.name),
                    p.accent(&p.bold(&r.url))
                );
                println!(
                    "     {}",
                    p.dim(&format!("Share on it with `kiri PORT --name {}`.", r.name))
                );
            }
            Ok(())
        }
        Some("release") => {
            let name = args
                .get(1)
                .ok_or_else(|| io::Error::other("usage: kiri share release NAME"))?;
            release(name)?;
            println!("Released `{name}`. Anyone can ask for it now.");
            Ok(())
        }
        Some(target) => {
            let flag_name = match args.iter().position(|a| a == "--name") {
                Some(i) => Some(
                    args.get(i + 1)
                        .ok_or_else(|| io::Error::other(USAGE))?
                        .clone(),
                ),
                None => None,
            };
            // `kiri 3000 myapp` is `kiri 3000 --name myapp`.
            let mut rest = args[1..]
                .iter()
                .filter(|a| !a.starts_with("--") && Some(*a) != flag_name.as_ref());
            let name = match (flag_name.as_ref(), rest.next()) {
                (None, Some(n)) => Some(n.clone()),
                (Some(n), None) => Some(n.clone()),
                (None, None) => None,
                (Some(_), Some(extra)) => {
                    return Err(io::Error::other(format!(
                        "unexpected argument `{extra}`\n{USAGE}"
                    )));
                }
            };
            if let Some(extra) = rest.next() {
                return Err(io::Error::other(format!(
                    "unexpected argument `{extra}`\n{USAGE}"
                )));
            }
            // --json: one JSON object per line on stdout, for programs that run a share.
            let json = args.iter().any(|a| a == "--json");
            let target = match resolve(target) {
                Ok(t) => t,
                Err(e) if json => {
                    println!(
                        "{}",
                        serde_json::json!({ "event": "ended", "reason": e.to_string(), "error": true })
                    );
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
            if let Some(n) = name
                .as_ref()
                .filter(|_| !args.iter().any(|a| a == "--no-reserve"))
            {
                let p = Paint::new();
                match auto_reserve(n) {
                    Ok(
                        AutoReserve::Reserved(_)
                        | AutoReserve::Requested(_)
                        | AutoReserve::Waiting
                        | AutoReserve::Full(_),
                    ) if json => {}
                    Ok(AutoReserve::Reserved(url)) => {
                        println!();
                        println!(
                            "  {}  Reserved {} for you. It's yours across restarts.",
                            p.accent("●"),
                            p.bold(&url)
                        );
                    }
                    Ok(AutoReserve::Requested(url)) => {
                        println!();
                        println!(
                            "  {}  Asked for {}; until it's approved (usually within a day)",
                            p.accent("◌"),
                            p.bold(&url)
                        );
                        println!("     {}", p.dim("this share gets a random address. We'll email you when it's decided."));
                    }
                    Ok(AutoReserve::Waiting) => {
                        println!();
                        println!("  {}  {}", p.accent("◌"), p.dim(&format!("{n} is waiting for review, so this share gets a random address for now.")));
                    }
                    Ok(AutoReserve::Full(limit)) => {
                        println!();
                        println!(
                            "  {}  {}",
                            p.dim("·"),
                            p.dim(&format!("Your plan's {limit} reserved name{} in use, so this address gets a random ending. See `kiri share names`.", if limit == 1 { " is" } else { "s are" }))
                        );
                    }
                    Ok(AutoReserve::Mine | AutoReserve::Unavailable) | Err(_) => {}
                }
            }
            let failed = Arc::new(Mutex::new(None::<String>));
            let print: Box<dyn Fn(Event) + Send + Sync> = if json {
                Box::new(|e| println!("{}", event_json(&e)))
            } else {
                Box::new(printer(target.display.clone()))
            };
            let f = failed.clone();
            let traffic = TrafficFile::start();
            let handle = start(
                Options {
                    target,
                    name,
                    app: false,
                },
                move |e| {
                    if let Event::Ended {
                        reason,
                        error: true,
                    } = &e
                    {
                        *f.lock().unwrap() = Some(reason.clone());
                    }
                    if let Some(t) = &traffic {
                        t.on(&e);
                    }
                    print(e);
                },
            );
            let handle = match handle {
                Ok(h) => h,
                Err(e) if json => {
                    println!(
                        "{}",
                        serde_json::json!({ "event": "ended", "reason": e.to_string(), "error": true })
                    );
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
            handle.wait();
            match failed.lock().unwrap().take() {
                Some(_) => Err(io::Error::other("the tunnel closed")),
                None => Ok(()),
            }
        }
    }
}

fn login_cli(args: &[String]) -> io::Result<()> {
    let p = Paint::new();
    if let Some(i) = args.iter().position(|a| a == "--dev") {
        let login = args.get(i + 1).map_or("dev", String::as_str);
        let who = login_dev(login)?;
        println!(
            "Signed in to Kiri Share as {} (development edge).",
            p.bold(&who)
        );
        return Ok(());
    }
    let code = login_start()?;
    println!();
    println!(
        "  {}  Sign in to Kiri Share with GitHub",
        p.accent(&p.bold("◆"))
    );
    println!("     Open {} and enter", p.bold(&code.verification_uri));
    println!();
    println!("         {}", p.accent(&p.bold(&code.user_code)));
    println!();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg(&code.verification_uri)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", &code.verification_uri])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    println!("     {}", p.dim("Waiting for you to approve…"));
    let mut wait = code.interval;
    loop {
        std::thread::sleep(wait);
        match login_poll(&code)? {
            LoginPoll::Pending(next) => wait = next,
            LoginPoll::Done(who) => {
                println!("  {}  Signed in as {}.", p.accent("●"), p.bold(&who));
                return Ok(());
            }
        }
    }
}

fn names_cli() -> io::Result<()> {
    let p = Paint::new();
    let n = names()?;
    let limit = n.limit.map_or_else(
        || "no limit".to_owned(),
        |l| format!("{} of {l} used", n.items.len()),
    );
    println!(
        "  {}  {}",
        p.accent(&p.bold("◆ Reserved names")),
        p.dim(&limit)
    );
    if n.items.is_empty() {
        println!(
            "     {}",
            p.dim("None yet. Ask for one with `kiri share reserve NAME`.")
        );
    }
    for r in &n.items {
        if r.pending {
            println!(
                "     {:<24} {}  {}",
                p.bold(&r.name),
                p.accent("◌ waiting"),
                p.dim("for review, usually within a day")
            );
        } else {
            let state = if r.in_use {
                p.accent("● live")
            } else {
                p.dim("○ idle")
            };
            println!("     {:<24} {}  {}", p.bold(&r.name), state, p.dim(&r.url));
        }
    }
    if !n.refused.is_empty() {
        println!();
        println!("  {}", p.bold("Not approved"));
        for r in &n.refused {
            println!(
                "     {:<24} {}  {}",
                p.bold(&r.name),
                p.dim("✕"),
                p.dim(&r.reason)
            );
        }
    }
    Ok(())
}

/// An event as `kiri share --json` prints it: `event` names it, the other fields are its own.
fn event_json(e: &Event) -> serde_json::Value {
    use serde_json::json;
    match e {
        Event::Connecting { server } => json!({ "event": "connecting", "server": server }),
        Event::Live {
            url,
            rate,
            remaining,
            warning,
            domains,
        } => {
            json!({ "event": "live", "url": url, "rate": rate, "remaining": remaining, "warning": warning, "domains": domains })
        }
        Event::Request(r) => json!({
            "event": "request", "method": r.method, "path": r.path, "status": r.status, "ms": r.ms,
            "bytes_in": r.bytes_in, "bytes_out": r.bytes_out,
            "at": r.at.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        }),
        Event::Notice(text) => json!({ "event": "notice", "text": text }),
        Event::Reconnecting { reason, delay } => {
            json!({ "event": "reconnecting", "reason": reason, "delay_ms": delay.as_millis() as u64 })
        }
        Event::Ended { reason, error } => {
            json!({ "event": "ended", "reason": reason, "error": error })
        }
    }
}

fn status_cli(json: bool) -> io::Result<()> {
    let p = Paint::new();
    let cfg = load()?;
    let srv = server(&cfg);
    if json {
        // Signed in or not, for programs: `{"signed_in", "server", "login"}`.
        let login = match cfg.token {
            Some(_) => whoami()
                .ok()
                .and_then(|v| v["login"].as_str().map(str::to_owned)),
            None => None,
        };
        println!(
            "{}",
            serde_json::json!({ "signed_in": login.is_some(), "server": srv.host, "login": login })
        );
        return Ok(());
    }
    if cfg.token.is_none() {
        println!(
            "Not signed in to Kiri Share ({}). Run `kiri share login`.",
            srv.host
        );
        return Ok(());
    }
    let v = whoami()?;
    let rate = v["limits"]["rate"].as_u64().unwrap_or(0) as u32;
    let cap = v["usage"]["cap"].as_u64().unwrap_or(u64::MAX);
    let used = v["usage"]["used"].as_u64().unwrap_or(0);
    println!(
        "  {}  {}",
        p.accent(&p.bold("◆ Kiri Share")),
        p.dim(&srv.host)
    );
    println!(
        "     {:<10}{}",
        "account",
        p.bold(v["login"].as_str().unwrap_or("?"))
    );
    println!("     {:<10}{}", "trust", v["trust"].as_i64().unwrap_or(0));
    match v["limits"]["tunnels"].as_u64() {
        Some(n) => println!("     {:<10}up to {n} at once", "tunnels"),
        None => println!("     {:<10}no limit", "tunnels"),
    }
    println!(
        "     {:<10}{}",
        "speed",
        if rate == 0 {
            "no limit".to_owned()
        } else {
            format!("{}/s", human_bytes(rate as u64))
        }
    );
    if cap == u64::MAX {
        println!(
            "     {:<10}{} this month, no cap",
            "used",
            human_bytes(used)
        );
    } else {
        println!(
            "     {:<10}{} of {} this month",
            "used",
            human_bytes(used),
            human_bytes(cap)
        );
    }
    if v["limits"]["warning"].as_bool() == Some(true) {
        println!("     {:<10}browsers see a warning page first", "visitors");
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_name_waiting_for_review_is_pending() {
        let v = serde_json::json!({ "name": "acme-shop", "url": "https://acme-shop.runkiri.cloud", "in_use": false, "status": "pending", "created_at": "2026-11-01T05:00:00Z" });
        assert!(super::reserved_name(&v).pending);
        let v = serde_json::json!({ "name": "acme-shop", "url": "https://acme-shop.runkiri.cloud", "in_use": true, "status": "approved" });
        assert!(!super::reserved_name(&v).pending);
        // Edges from before review send no status: the name is theirs.
        let v = serde_json::json!({ "name": "acme-shop", "url": "https://acme-shop.runkiri.cloud", "in_use": true });
        assert!(!super::reserved_name(&v).pending);
    }

    #[test]
    fn a_kiri_share_writes_its_traffic_for_the_window() {
        let dir = std::env::temp_dir().join(format!("kiri-traffic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = TrafficFile {
            path: dir.join("1.json"),
            url: Mutex::new(String::new()),
            counts: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            dirty: AtomicBool::new(false),
            done: AtomicBool::new(false),
        };
        f.on(&Event::Live {
            url: "https://dock-4k.runkiri.cloud".into(),
            rate: 0,
            remaining: 0,
            warning: false,
            domains: Vec::new(),
        });
        let req = |i, o| {
            Event::Request(Request {
                method: "GET".into(),
                path: "/".into(),
                status: 200,
                ms: 1,
                bytes_in: i,
                bytes_out: o,
                at: SystemTime::now(),
            })
        };
        f.on(&req(100, 2000));
        f.on(&req(50, 48));
        f.write();
        assert_eq!(
            traffic_in(&dir).get("https://dock-4k.runkiri.cloud"),
            Some(&[2, 150, 2048])
        );
        f.on(&Event::Ended {
            reason: "stopped".into(),
            error: false,
        });
        assert!(traffic_in(&dir).is_empty(), "removed when it ends");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn events_print_as_json_lines() {
        let live = event_json(&Event::Live {
            url: "https://a.runkiri.cloud".into(),
            rate: 0,
            remaining: 5,
            warning: false,
            domains: vec![],
        });
        assert_eq!(live["event"], "live");
        assert_eq!(live["url"], "https://a.runkiri.cloud");
        let end = event_json(&Event::Ended {
            reason: "stopped".into(),
            error: false,
        });
        assert_eq!(
            (end["event"].as_str(), end["error"].as_bool()),
            (Some("ended"), Some(false))
        );
        let wait = event_json(&Event::Reconnecting {
            reason: "edge closed".into(),
            delay: Duration::from_millis(1500),
        });
        assert_eq!(wait["delay_ms"], 1500);
        assert!(
            !event_json(&Event::Notice("hi".into()))
                .to_string()
                .contains('\n')
        );
    }

    use super::*;

    #[test]
    fn a_name_held_a_moment_ago_is_waited_for() {
        let busy = "that name is already in use by another of your tunnels";
        assert!(
            still_closing(busy, Some(Instant::now())),
            "our own last connection, still closing on the edge"
        );
        assert!(
            !still_closing(busy, None),
            "never live: someone else has it"
        );
        assert!(
            !still_closing(
                busy,
                Instant::now().checked_sub(RECLAIM + Duration::from_secs(1))
            ),
            "long gone: give up"
        );
        assert!(!still_closing(
            "you've used this month's free transfer",
            Some(Instant::now())
        ));
    }

    #[test]
    fn request_lines() {
        assert_eq!(
            parse_request_line(b"GET /a?b=1 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("GET".into(), "/a?b=1".into()))
        );
        assert_eq!(
            parse_request_line(b"POST / HTTP/1.0\r\n"),
            Some(("POST".into(), "/".into()))
        );
        assert_eq!(parse_request_line(b"get / HTTP/1.1\r\n"), None);
        assert_eq!(parse_request_line(b"GET /\r\n"), None);
        assert_eq!(parse_request_line(b"\x16\x03\x01binary"), None);
        assert_eq!(
            parse_request_line(b"GET / HTTP/1.1"),
            None,
            "no line end yet"
        );
    }

    #[test]
    fn status_lines() {
        assert_eq!(parse_status_line(b"HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_status_line(b"HTTP/1.0 404 Not Found\r\n"), Some(404));
        assert_eq!(parse_status_line(b"HTTP/1.1 101\r\n"), Some(101));
        assert_eq!(parse_status_line(b"HTTP/1.1 2000 OK"), None);
        assert_eq!(parse_status_line(b"<html>"), None);
    }

    #[test]
    fn server_addresses() {
        assert_eq!(
            parse_server("edge.runkiri.com"),
            ("edge.runkiri.com".into(), 443)
        );
        assert_eq!(
            parse_server("https://Edge.Example:8443/"),
            ("edge.example".into(), 8443)
        );
        assert_eq!(parse_server("localhost:"), ("localhost:".into(), 443));
    }

    #[test]
    fn targets() {
        assert_eq!(port_or_addr("3000").unwrap().addr, "localhost:3000");
        assert!(port_or_addr("0").is_none());
        assert!(port_or_addr("99999").is_none());
        let t = port_or_addr("api.internal:8080").unwrap();
        assert_eq!(
            (t.addr.as_str(), t.label.as_str()),
            ("api.internal:8080", "api")
        );
        assert_eq!(port_or_addr("127.0.0.1:5173").unwrap().label, "");
        assert_eq!(port_or_addr("[::1]:3000").unwrap().addr, "[::1]:3000");
        assert!(port_or_addr("web").is_none());
    }

    #[test]
    fn container_ports() {
        let inspect = json!({"NetworkSettings": {"Ports": {
            "443/tcp": [{"HostIp": "127.0.0.1", "HostPort": "8443"}],
            "80/tcp": [{"HostIp": "0.0.0.0", "HostPort": "8080"}],
            "53/udp": [{"HostIp": "", "HostPort": "5353"}],
            "9000/tcp": null,
        }}});
        assert_eq!(published_port(&inspect).as_deref(), Some("localhost:8080"));
        assert_eq!(
            published_port(&json!({"NetworkSettings": {"Ports": {"80/tcp": null}}})),
            None
        );
    }

    #[test]
    fn http_answers() {
        let (s, v) =
            parse_http_response(b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\n{\"ok\":true}")
                .unwrap();
        assert_eq!((s, v["ok"].as_bool()), (200, Some(true)));
        let (s, v) = parse_http_response(b"HTTP/1.1 202 Accepted\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n").unwrap();
        assert_eq!((s, v["a"].as_i64()), (202, Some(1)));
        assert!(parse_http_response(b"garbage").is_err());
    }

    #[test]
    fn sizes_and_limits() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(512 * 1024), "512 KB");
        assert_eq!(
            limits_line(512 * 1024, 10 << 30, true),
            "512 KB/s · 10.0 GB left this month · browsers see a warning page first"
        );
        assert_eq!(limits_line(0, u64::MAX, false), "no speed limit");
    }

    #[test]
    fn machine_ids_hash_the_hardware_id() {
        let ioreg = "+-o J316sAP  <class IOPlatformExpertDevice>\n    {\n      \"IOPlatformSerialNumber\" = \"C02XYZ\"\n      \"IOPlatformUUID\" = \"5D1E8C2A-0B7F-4E21-9A3C-6F0D2B9E1A44\"\n    }\n";
        let uuid = platform_uuid(ioreg).unwrap();
        assert_eq!(uuid, "5D1E8C2A-0B7F-4E21-9A3C-6F0D2B9E1A44");
        assert_eq!(platform_uuid("no such line"), None);
        let id = machine_hash(uuid);
        assert_eq!(id.len(), 24);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(
            !id.contains("5d1e8c2a") && !id.to_uppercase().contains("5D1E8C2A"),
            "never the id itself"
        );
        assert_eq!(id, machine_hash(uuid), "stable");
        assert_ne!(id, machine_hash("another"));
        // This computer's: the same every call, and never empty.
        assert_eq!(machine_id("dev1"), machine_id("dev1"));
        assert!(!machine_id("dev1").is_empty());
    }

    #[test]
    fn host_names_are_one_short_line() {
        assert_eq!(clean_host("  paul-mbp\n"), "paul-mbp");
        assert_eq!(clean_host("a\u{7}b"), "ab");
        assert_eq!(clean_host(&"x".repeat(100)).chars().count(), 64);
        assert_eq!(clean_host("Paul’s MacBook Pro"), "Paul’s MacBook Pro");
    }

    #[test]
    fn live_tunnels_read_from_the_edge() {
        let t = live_tunnel(&json!({
            "name": "api-7fq2ab", "label": "api", "url": "https://api-7fq2ab.runkiri.cloud", "machine": "m1", "device": "d1", "host": "paul-desktop",
            "kind": "app", "client": "Kiri Dev 0.9.4", "target": "api (localhost:3000)", "started": 1_800_000_000u64,
        }));
        assert_eq!(
            (
                t.name.as_str(),
                t.label.as_str(),
                t.host.as_str(),
                t.kind.as_str(),
                t.started
            ),
            ("api-7fq2ab", "api", "paul-desktop", "app", 1_800_000_000)
        );
        // An edge that leaves fields out: the name is the label.
        assert_eq!(
            live_tunnel(&json!({ "name": "x" })),
            LiveTunnel {
                name: "x".into(),
                label: "x".into(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn config_round_trip() {
        let dir = std::env::temp_dir().join(format!("kiri-share-test-{}", std::process::id()));
        let path = dir.join("share.json");
        // SAFETY: this is the only test that reads KIRI_SHARE_CONFIG.
        unsafe { std::env::set_var("KIRI_SHARE_CONFIG", &path) };
        let cfg = load().unwrap();
        assert_eq!(cfg.device.len(), 32, "a device id is made on first load");
        let saved = Config {
            server: Some("edge.example:8443".into()),
            token: Some("ks_x".into()),
            login: Some("ada".into()),
            device: cfg.device.clone(),
        };
        save(&saved).unwrap();
        assert_eq!(load().unwrap(), saved);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn build_key_is_the_one_built_with() {
        let built = option_env!("KIRI_SHARE_BUILD_KEY").filter(|v| !v.trim().is_empty());
        match (built.and_then(proto::attest::parse_key), build_key()) {
            (None, None) => {}
            (Some(want), Some(got)) => {
                assert_eq!(want, got);
                let (_, masked, _) = BUILD_KEY.unwrap();
                assert_ne!(masked, got.1, "stored masked");
            }
            (want, got) => panic!(
                "built with {:?}, have {:?}",
                want.map(|k| k.0),
                got.map(|k| k.0)
            ),
        }
    }

    #[test]
    fn backoff_grows_with_jitter_and_caps() {
        assert!(
            backoff(0) >= Duration::from_millis(250) && backoff(0) <= Duration::from_millis(750)
        );
        assert!(backoff(20) <= Duration::from_secs(45));
    }

    #[test]
    fn device_code_lifetimes_are_clamped() {
        let now = Instant::now();
        assert_eq!(
            device_code_deadline(now, 900),
            now + Duration::from_secs(900)
        );
        // A hostile expires_in neither panics nor waits more than an hour.
        assert_eq!(
            device_code_deadline(now, u64::MAX),
            now + Duration::from_secs(MAX_DEVICE_CODE_SECS)
        );
    }
}
