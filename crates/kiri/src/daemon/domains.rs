//! Container domains: `http://<name>.kiri.local` reaches a running
//! container's HTTP port, and Compose services answer at
//! `<service>.<project>.kiri.local`. dns.rs points those names at loopback;
//! `*.kiri.localhost` works too, with no setup, since macOS resolves
//! `*.localhost` by itself. An unprivileged process may listen on port 80 on
//! the wildcard address, so serving needs no root. Only connections from
//! this Mac are served.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use super::engine::Engine;

/// Largest request head read before routing.
const MAX_HEAD: usize = 64 * 1024;

/// How often a router that stepped aside tries its port again.
const RECLAIM_EVERY: Duration = Duration::from_secs(3);

/// Where the domain routers listen: only macOS needs the wildcard (the VM
/// reaches the router over its own network); on a Linux host or in WSL the
/// router is for this machine alone, so it stays on loopback.
fn router_addrs(port: u16) -> [String; 2] {
    if cfg!(target_os = "macos") {
        [format!("0.0.0.0:{port}"), format!("[::]:{port}")]
    } else {
        [format!("127.0.0.1:{port}"), format!("[::1]:{port}")]
    }
}

/// One router: plain HTTP, or HTTPS with tls.rs's certificates.
struct Router {
    port: u16,
    engine: Arc<Engine>,
    tls: Option<Arc<rustls::ServerConfig>>,
    /// How many addresses it listened on at start (IPv4, and IPv6 where
    /// the host has it): taking the port back needs them all again.
    want: std::sync::atomic::AtomicUsize,
    /// While serving: one wake pipe and accept thread per listener. Empty
    /// while it has stepped aside for a container.
    serving: Mutex<Vec<(OwnedFd, JoinHandle<()>)>>,
}

/// The routers, for a container that publishes their port (`step_aside`).
static ROUTERS: Mutex<Vec<Arc<Router>>> = Mutex::new(Vec::new());

impl Router {
    /// Binds the router's addresses and starts their accept threads; an
    /// error when fewer than `min` (at least one) could be bound, so a
    /// router never comes back on IPv6 alone while a container has IPv4.
    fn listen(self: &Arc<Self>, min: usize) -> io::Result<Vec<(OwnedFd, JoinHandle<()>)>> {
        let listeners: Vec<TcpListener> = router_addrs(self.port).iter().filter_map(|a| TcpListener::bind(a).ok()).collect();
        if listeners.len() < min.max(1) {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("port {} is taken", self.port)));
        }
        let mut out = Vec::new();
        for listener in listeners {
            listener.set_nonblocking(true)?;
            let (wake_r, wake_w) = super::ports::pipe()?;
            let me = self.clone();
            let name = if self.tls.is_some() { "kiri-domains-tls" } else { "kiri-domains" };
            out.push((wake_w, std::thread::Builder::new().name(name.into()).spawn(move || me.accept(&listener, &wake_r))?));
        }
        Ok(out)
    }

    /// Serves `listener` until its wake pipe closes; the listener closes with it.
    fn accept(&self, listener: &TcpListener, wake: &OwnedFd) {
        loop {
            let mut fds = [
                libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            // SAFETY: polling two descriptors we own.
            if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if fds[1].revents != 0 {
                return; // stepped aside
            }
            while let Ok((conn, _)) = listener.accept() {
                let _ = conn.set_nonblocking(false);
                // A client that never finishes its request must not hold a thread forever.
                let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
                let (engine, tls) = (self.engine.clone(), self.tls.clone());
                let _ = std::thread::Builder::new().name("kiri-domain-conn".into()).spawn(move || {
                    let _ = match tls {
                        Some(config) => serve_tls(conn, &engine, config),
                        None => serve(conn, &engine),
                    };
                });
            }
        }
    }

    /// Stops serving; returns once every listener is closed.
    fn stop(&self) -> bool {
        let threads = std::mem::take(&mut *self.serving.lock().unwrap_or_else(PoisonError::into_inner));
        let stopped = !threads.is_empty();
        for (wake, thread) in threads {
            drop(wake);
            let _ = thread.join();
        }
        stopped
    }
}

/// A container is publishing host `port` and found it taken: when a router
/// holds it, the router lets it go, so the container gets the port (Laravel
/// Sail's `80:80`, say), and `true` says to bind again. Container names on
/// that port pause meanwhile; the router takes the port back once it is
/// free (`reclaim`). Other routers, and other ports, are left alone.
pub fn step_aside(port: u16) -> bool {
    let routers: Vec<Arc<Router>> = ROUTERS.lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|r| r.port == port).cloned().collect();
    let mut stepped = false;
    for r in routers {
        if r.stop() {
            crate::diag::info!("a container publishes the container-domain router's port: the router lets it go until it is free", port = port);
            stepped = true;
        }
    }
    stepped
}

/// Whether a router serves `port` now. Containers reach the Mac's loopback
/// through host.docker.internal (ports::host_bound), and the router routes
/// by Host header alone: through it a container would reach every other
/// container, whatever network it is on.
pub fn routes(port: u16) -> bool {
    ROUTERS.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|r| r.port == port && !r.serving.lock().unwrap_or_else(PoisonError::into_inner).is_empty())
}

/// Every few seconds, a router that stepped aside tries its port again.
fn reclaim() {
    loop {
        std::thread::sleep(RECLAIM_EVERY);
        let idle: Vec<Arc<Router>> = ROUTERS.lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|r| r.serving.lock().unwrap_or_else(PoisonError::into_inner).is_empty()).cloned().collect();
        for r in idle {
            let mut serving = r.serving.lock().unwrap_or_else(PoisonError::into_inner);
            if serving.is_empty()
                && let Ok(threads) = r.listen(r.want.load(std::sync::atomic::Ordering::Relaxed))
            {
                *serving = threads;
                crate::diag::info!("the container-domain router has its port again", port = r.port);
            }
        }
    }
}

/// Adds a router to the list, starting the take-back thread with the first
/// (once: the list is empty again after `reset` turns a test copy's off).
fn register(router: Arc<Router>) -> io::Result<()> {
    static RECLAIMING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let mut routers = ROUTERS.lock().unwrap_or_else(PoisonError::into_inner);
    if !RECLAIMING.swap(true, std::sync::atomic::Ordering::Relaxed)
        && let Err(e) = std::thread::Builder::new().name("kiri-domains-reclaim".into()).spawn(reclaim)
    {
        RECLAIMING.store(false, std::sync::atomic::Ordering::Relaxed);
        return Err(e);
    }
    routers.push(router);
    Ok(())
}

/// A router on `port`, listening; an error when the port is taken.
fn bind_router(engine: Arc<Engine>, port: u16, tls: Option<Arc<rustls::ServerConfig>>) -> io::Result<Arc<Router>> {
    let router = Arc::new(Router { port, engine, tls, want: 0.into(), serving: Mutex::new(Vec::new()) });
    let threads = router.listen(1)?;
    router.want.store(threads.len(), std::sync::atomic::Ordering::Relaxed);
    *router.serving.lock().unwrap_or_else(PoisonError::into_inner) = threads;
    Ok(router)
}

fn start_router(engine: Arc<Engine>, kind: Kind, tls: Option<Arc<rustls::ServerConfig>>) -> io::Result<u16> {
    let port = kind.port();
    let how = if env_port(kind.var()).is_some() { format!("set {}", kind.var()) } else { "pick another in Settings → Domains".into() };
    register(bind_router(engine, port, tls).map_err(|_| io::Error::new(io::ErrorKind::AddrInUse, format!("port {port} is taken; {how}")))?)?;
    Ok(port)
}

/// Starts the HTTPS router on port 443, or the port Settings → Domains (or
/// `KIRI_HTTPS_PORT`) gives: the same routing, behind TLS with certificates
/// from tls.rs. Returns the port.
pub fn start_tls(engine: Arc<Engine>) -> io::Result<u16> {
    start_router(engine, Kind::Https, Some(super::tls::server_config()?))
}

/// Starts the router on port 80, or the port Settings → Domains (or
/// `KIRI_HTTP_PORT`) gives. Returns the port.
pub fn start(engine: Arc<Engine>) -> io::Result<u16> {
    start_router(engine, Kind::Http, None)
}

/// The two routers.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Http,
    Https,
}

impl Kind {
    fn var(self) -> &'static str {
        match self {
            Kind::Http => "KIRI_HTTP_PORT",
            Kind::Https => "KIRI_HTTPS_PORT",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Kind::Http => "http",
            Kind::Https => "https",
        }
    }
    fn default(self) -> u16 {
        match self {
            Kind::Http => 80,
            Kind::Https => 443,
        }
    }
    /// The port it should use: the environment variable, then Settings →
    /// Domains (`~/.kiri/domains.json`), then 80 or 443.
    fn port(self) -> u16 {
        env_port(self.var()).or_else(|| saved_port(self)).unwrap_or(self.default())
    }
    /// The running router, serving or stepped aside.
    fn router(self) -> Option<Arc<Router>> {
        ROUTERS.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|r| r.tls.is_some() == (self == Kind::Https)).cloned()
    }
}

fn env_port(var: &str) -> Option<u16> {
    std::env::var(var).ok().and_then(|p| p.trim().parse().ok()).filter(|&p| p != 0)
}

/// Settings → Domains keeps the routers' ports here.
fn settings_path() -> Option<std::path::PathBuf> {
    super::state_dir().ok().map(|d| d.join("domains.json"))
}

fn saved_port(kind: Kind) -> Option<u16> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(settings_path()?).ok()?).ok()?;
    v[kind.key()].as_u64().and_then(|p| u16::try_from(p).ok()).filter(|&p| p != 0)
}

/// Whether this engine has router ports of its own: Kiri Dev starts its
/// routers only then, leaving 80 and 443 to the installed Kiri.
pub fn own_ports() -> bool {
    env_port(Kind::Http.var()).is_some() || settings_path().is_some_and(|p| p.exists())
}

/// Settings → Domains: the routers' ports, whether each runs and serves
/// (not while a container has its port), and which ones an environment
/// variable fixes.
pub fn report() -> serde_json::Value {
    let mut v = serde_json::json!({});
    for kind in [Kind::Http, Kind::Https] {
        let r = kind.router();
        v[kind.key()] = serde_json::json!({
            "port": r.as_ref().map_or(kind.port(), |r| r.port),
            "default": kind.default(),
            // Off: no router (its port was taken at start, or Kiri Dev
            // without ports of its own). Running but not serving: it
            // stepped aside for a container.
            "running": r.is_some(),
            "serving": r.is_some_and(|r| !r.serving.lock().unwrap_or_else(PoisonError::into_inner).is_empty()),
            "env": env_port(kind.var()).map(|_| kind.var()),
        });
    }
    v
}

/// One change of ports at a time.
static MOVING: Mutex<()> = Mutex::new(());

/// Settings → Domains: moves the routers to `http` and `https`, at once,
/// with no engine restart. Every new port is bound before an old one
/// closes, so a taken port changes nothing and says so. Saved only once it
/// worked. Connections already open finish on the old port.
pub fn set_ports(engine: &Arc<Engine>, http: u16, https: u16) -> Result<serde_json::Value, String> {
    let _one = MOVING.lock().unwrap_or_else(PoisonError::into_inner);
    if http == 0 || https == 0 {
        return Err("ports must be 1 to 65535".into());
    }
    if http == https {
        return Err("HTTP and HTTPS need different ports".into());
    }
    move_routers(engine, http, https)?;
    let path = settings_path().ok_or("HOME is not set")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::json!({"http": http, "https": https}).to_string())
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| e.to_string())?;
    Ok(report())
}

/// Moves the routers to `http` and `https` (starting one that isn't
/// running); the caller holds `MOVING`.
fn move_routers(engine: &Arc<Engine>, http: u16, https: u16) -> Result<(), String> {
    let mut moves: Vec<(Option<Arc<Router>>, Arc<Router>)> = Vec::new();
    for (kind, want) in [(Kind::Http, http), (Kind::Https, https)] {
        if let Some(p) = env_port(kind.var())
            && p != want
        {
            return Err(format!("{} sets this port to {p}; remove it to choose the port here", kind.var()));
        }
        let old = kind.router();
        if old.as_ref().is_some_and(|r| r.port == want) {
            continue;
        }
        let tls = match kind {
            Kind::Https => Some(super::tls::server_config().map_err(|e| e.to_string())?),
            Kind::Http => None,
        };
        match bind_router(engine.clone(), want, tls) {
            Ok(new) => moves.push((old, new)),
            Err(_) => {
                for (_, new) in moves {
                    new.stop();
                }
                return Err(format!("port {want} is in use; pick another"));
            }
        }
    }
    for (old, new) in moves {
        if let Some(old) = old {
            ROUTERS.lock().unwrap_or_else(PoisonError::into_inner).retain(|r| !Arc::ptr_eq(r, &old));
            old.stop();
        }
        let port = new.port;
        register(new).map_err(|e| e.to_string())?;
        crate::diag::info!("the container-domain router moved", port = port);
    }
    Ok(())
}

/// The routers' ports with nothing saved, as the daemon starts them: the
/// environment's, else 80 and 443; `None` for a test copy (Kiri Dev, Kiri
/// Repro...) without KIRI_HTTP_PORT, which starts none and so never takes
/// 80 and 443 from the installed Kiri.
fn default_ports(dev: bool, env: impl Fn(Kind) -> Option<u16>) -> Option<(u16, u16)> {
    if dev && env(Kind::Http).is_none() {
        return None;
    }
    Some((env(Kind::Http).unwrap_or(Kind::Http.default()), env(Kind::Https).unwrap_or(Kind::Https.default())))
}

/// Settings → Domains back to the start: forgets the saved ports and puts
/// the routers where a restart would, with no restart. A test copy's
/// routers stop.
pub fn reset(engine: &Arc<Engine>) -> Result<serde_json::Value, String> {
    let _one = MOVING.lock().unwrap_or_else(PoisonError::into_inner);
    match default_ports(crate::flavor::dev(), |k| env_port(k.var())) {
        Some((http, https)) => move_routers(engine, http, https)?,
        None => {
            let off = std::mem::take(&mut *ROUTERS.lock().unwrap_or_else(PoisonError::into_inner));
            for r in off {
                r.stop();
                crate::diag::info!("the container-domain router stopped", port = r.port);
            }
        }
    }
    match settings_path().map(std::fs::remove_file) {
        Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => return Err(e.to_string()),
        _ => {}
    }
    Ok(report())
}

fn serve(mut client: TcpStream, engine: &Engine) -> io::Result<()> {
    // The listener faces the network; only this Mac may use it.
    if !client.peer_addr()?.ip().is_loopback() {
        return Ok(());
    }
    client.set_nodelay(true)?;
    let Some((head, guest)) = route(&mut client, engine, "http")? else { return Ok(()) };
    let mut to_guest = std::fs::File::from(guest.fd.try_clone()?);
    // What the client sent so far goes first, unchanged.
    to_guest.write_all(&head)?;
    let mut from_guest = std::fs::File::from(guest.fd.try_clone()?);
    let mut client_read = client.try_clone()?;
    let up = std::thread::Builder::new().name("kiri-domain-up".into()).spawn(move || {
        let _ = io::copy(&mut client_read, &mut to_guest);
        // SAFETY: half-closing a socket we own.
        unsafe { libc::shutdown(to_guest.as_raw_fd(), libc::SHUT_WR) };
    })?;
    let _ = io::copy(&mut from_guest, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = up.join();
    drop(guest);
    Ok(())
}

/// Reads the request head, finds the container its Host names and connects
/// to it. `None` once it has answered by itself (no such container, an
/// unreachable one, or a client that went away).
fn route(client: &mut impl ReadWrite, engine: &Engine, scheme: &str) -> io::Result<Option<(Vec<u8>, kiri_vmm::VsockStream)>> {
    let mut head = Vec::with_capacity(2048);
    let mut buf = [0u8; 4096];
    let end = loop {
        let n = client.read(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        head.extend_from_slice(&buf[..n]);
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if head.len() > MAX_HEAD {
            respond(client, 431, "Request Header Fields Too Large", "")?;
            return Ok(None);
        }
    };
    let text = String::from_utf8_lossy(&head[..end]);
    let host = text
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("host")).map(|(_, v)| v.trim().to_owned()))
        .unwrap_or_default();
    let Some(target) = engine.route(&host) else {
        // The running containers are listed only for this machine's own
        // names. A page reached through DNS rebinding asks with its own
        // domain as the Host, and could read the list from this answer.
        let running = if local_host(&host) {
            let list: String = engine.domains().iter().map(|d| { let d = escape(d); format!("<li><a href=\"{scheme}://{d}\">{d}</a></li>") }).collect();
            format!("<p>Running containers:</p><ul>{}</ul>", if list.is_empty() { "<li>none</li>".to_owned() } else { list })
        } else {
            String::new()
        };
        let body = format!(
            "<!doctype html><meta charset=utf-8><title>Kiri</title><body style=\"font:15px -apple-system,sans-serif;margin:40px\">\
             <h1>No running container for {}</h1>{running}",
            escape(&host),
        );
        respond(client, 404, "Not Found", &body)?;
        return Ok(None);
    };
    let port = web_port(engine, &target);
    match engine.relay_to(target.ip, port) {
        Ok(g) => Ok(Some((head, g))),
        Err(e) => {
            respond(client, 502, "Bad Gateway", &format!("Kiri could not reach the container: {}", escape(&e.to_string())))?;
            Ok(None)
        }
    }
}

/// A running container a domain names, and what says which port is its
/// web port.
pub struct Target {
    pub ip: [u8; 4],
    pub id: String,
    /// `dev.kiri.http.port`: the owner said so.
    pub label: Option<u16>,
    /// Container ports published with `-p` or `-P`.
    pub published: Vec<u16>,
    /// Ports the image declares (EXPOSE) or `--expose` adds.
    pub exposed: Vec<u16>,
}

/// Ports web servers usually listen on, most likely first.
const WEB_PORTS: [u16; 14] = [80, 8080, 3000, 8000, 5000, 5173, 4200, 8888, 9000, 8081, 4000, 3001, 15672, 8443];
/// Ports that are surely not a web page: databases, queues, caches.
const NOT_WEB: [u16; 12] = [5432, 3306, 6379, 27017, 5672, 4369, 25672, 9042, 11211, 1433, 1521, 2181];

/// The ports that could be the web port, best first, and whether the first
/// is certain (the label, or the only port published): published ports
/// before ports only declared, and within each, common web ports first and
/// database and queue ports last.
fn candidates(t: &Target) -> (Vec<u16>, bool) {
    if let Some(p) = t.label {
        return (vec![p], true);
    }
    let rank = |p: &u16| WEB_PORTS.iter().position(|w| w == p).unwrap_or(if NOT_WEB.contains(p) { usize::MAX } else { WEB_PORTS.len() });
    let sorted = |v: &[u16]| {
        let mut v = v.to_vec();
        v.sort_unstable_by_key(|p| (rank(p), *p));
        v.dedup();
        v
    };
    let mut out = sorted(&t.published);
    let sure = out.len() == 1;
    out.extend(sorted(&t.exposed).into_iter().filter(|p| !t.published.contains(p)));
    (out, sure)
}

/// Of the candidates, the first the container listens on; with none
/// declared, the best port it listens on; else the first, or 80.
fn choose(cands: &[u16], listening: Option<&[u16]>) -> u16 {
    let Some(on) = listening.filter(|l| !l.is_empty()) else { return cands.first().copied().unwrap_or(80) };
    if let Some(p) = cands.iter().find(|p| on.contains(p)) {
        return *p;
    }
    if cands.is_empty() {
        let (best, _) = candidates(&Target { ip: [0; 4], id: String::new(), label: None, published: Vec::new(), exposed: on.to_vec() });
        return best[0];
    }
    cands[0]
}

/// The web port for a request: certain ones at once; otherwise asks the
/// container what it listens on, remembered for a few seconds since a page
/// opens many connections.
fn web_port(engine: &Engine, t: &Target) -> u16 {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static SEEN: Mutex<Option<HashMap<String, (Instant, u16)>>> = Mutex::new(None);
    let (cands, sure) = candidates(t);
    if sure || cands.len() == 1 && !t.exposed.is_empty() {
        return cands[0];
    }
    let fresh = |at: Instant| at.elapsed() < Duration::from_secs(10);
    if let Some(&(at, p)) = SEEN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(&t.id) {
        if fresh(at) {
            return p;
        }
    }
    let on = engine.listening(&t.id).ok().map(|(reach, _)| reach);
    let port = choose(&cands, on.as_deref());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = seen.get_or_insert_with(HashMap::new);
    seen.retain(|_, (at, _)| fresh(*at));
    seen.insert(t.id.clone(), (Instant::now(), port));
    port
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// One HTTPS connection: TLS with the browser, plain bytes to the container,
/// pumped in one poll loop (a TLS session cannot be split across threads).
fn serve_tls(tcp: TcpStream, engine: &Engine, config: Arc<rustls::ServerConfig>) -> io::Result<()> {
    if !tcp.peer_addr()?.ip().is_loopback() {
        return Ok(());
    }
    tcp.set_nodelay(true)?;
    let conn = rustls::ServerConnection::new(config).map_err(|e| io::Error::other(e.to_string()))?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let Some((head, guest)) = route(&mut tls, engine, "https")? else {
        tls.conn.send_close_notify();
        let _ = tls.flush();
        return Ok(());
    };
    let mut container = std::fs::File::from(guest.fd.try_clone()?);
    container.write_all(&head)?;
    let (conn, sock) = (&mut tls.conn, &mut tls.sock);
    let mut buf = vec![0u8; 64 * 1024];
    let mut client_open = true;
    loop {
        // Decrypted bytes rustls already holds go first: the socket will not
        // signal them again.
        while client_open {
            match conn.reader().read(&mut buf) {
                Ok(0) => {
                    client_open = false;
                    // SAFETY: half-closing a socket we own.
                    unsafe { libc::shutdown(container.as_raw_fd(), libc::SHUT_WR) };
                }
                Ok(n) => container.write_all(&buf[..n])?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    // Closed without TLS's close_notify: the same, for the container.
                    client_open = false;
                    // SAFETY: half-closing a socket we own.
                    unsafe { libc::shutdown(container.as_raw_fd(), libc::SHUT_WR) };
                }
            }
        }
        while conn.wants_write() {
            conn.write_tls(sock)?;
        }
        let mut fds = [
            libc::pollfd { fd: sock.as_raw_fd(), events: if client_open { libc::POLLIN } else { 0 }, revents: 0 },
            libc::pollfd { fd: container.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: two valid pollfds, owned for the call.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let ready = |f: &libc::pollfd| f.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
        if client_open && ready(&fds[0]) {
            match conn.read_tls(sock) {
                Ok(0) | Err(_) => {
                    client_open = false;
                    // SAFETY: half-closing a socket we own.
                    unsafe { libc::shutdown(container.as_raw_fd(), libc::SHUT_WR) };
                }
                Ok(_) => {
                    if conn.process_new_packets().is_err() {
                        let _ = conn.write_tls(sock);
                        break;
                    }
                }
            }
        }
        if ready(&fds[1]) {
            match container.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => conn.writer().write_all(&buf[..n])?,
            }
        }
    }
    conn.send_close_notify();
    while conn.wants_write() {
        if conn.write_tls(sock).is_err() {
            break;
        }
    }
    drop(guest);
    Ok(())
}

fn respond(client: &mut impl Write, status: u16, reason: &str, body: &str) -> io::Result<()> {
    write!(
        client,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Whether a request's Host is a name for this machine itself: `localhost`,
/// anything under `.localhost` (which always resolves to loopback), a
/// `.kiri.local` name, or a loopback address. Another site's domain is not,
/// even when its DNS answers with 127.0.0.1.
fn local_host(host: &str) -> bool {
    let host = host.trim();
    // Without the port: `[::1]:80`, `127.0.0.1:80`, `web.kiri.localhost:8080`.
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => match host.rsplit_once(':') {
            Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
            _ => host,
        },
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost" || name.ends_with(".localhost") || name.ends_with(".kiri.local") || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::{candidates, choose, default_ports, Kind, Target};

    /// Back to the start: Kiri's routers return to 80 and 443 (or the
    /// environment's ports); a test copy's stop, unless the environment
    /// gives it ports, so it never takes 80 and 443 from Kiri.
    #[test]
    fn reset_puts_back_the_ports_a_start_would_use() {
        let none = |_: Kind| None;
        let env = |k: Kind| Some(if k == Kind::Http { 8080 } else { 8443 });
        let http_only = |k: Kind| (k == Kind::Http).then_some(8080);
        assert_eq!(default_ports(false, none), Some((80, 443)));
        assert_eq!(default_ports(false, env), Some((8080, 8443)));
        assert_eq!(default_ports(true, none), None, "a test copy starts no routers");
        assert_eq!(default_ports(true, env), Some((8080, 8443)));
        assert_eq!(default_ports(true, http_only), Some((8080, 443)), "as the daemon starts it");
        assert_eq!(default_ports(true, |k: Kind| (k == Kind::Https).then_some(8443)), None, "KIRI_HTTPS_PORT alone starts none");
    }

    /// The "no such container" page lists the running containers for this
    /// machine's own names only: a rebinding page's Host is its own domain.
    #[test]
    fn only_this_machines_names_get_the_container_list() {
        for local in ["localhost", "localhost:80", "nope.kiri.localhost", "NOPE.Kiri.Localhost:8080", "web.shop.kiri.local", "api.kiri.local:18780", "127.0.0.1", "127.0.0.1:80", "[::1]", "[::1]:443", "anything.localhost."] {
            assert!(super::local_host(local), "{local}");
        }
        for foreign in ["", "rebind.example.com", "rebind.example.com:80", "kiri.localhost.example.com", "localhost.example.com", "192.168.1.10", "10.0.0.1:80", "[2001:db8::1]:80", "notlocalhost", "kiri.local.evil.io"] {
            assert!(!super::local_host(foreign), "{foreign}");
        }
    }

    fn t(label: Option<u16>, published: &[u16], exposed: &[u16]) -> Target {
        Target { ip: [0; 4], id: String::new(), label, published: published.to_vec(), exposed: exposed.to_vec() }
    }

    #[test]
    fn the_port_published_wins_over_the_ones_declared() {
        // `-p 8080:3000` on an image that declares 80.
        assert_eq!(candidates(&t(None, &[3000], &[80, 3000])), (vec![3000, 80], true));
        assert_eq!(candidates(&t(Some(9000), &[3000], &[80])), (vec![9000], true));
    }

    #[test]
    fn several_ports_put_web_ports_first_and_databases_last() {
        // rabbitmq:management with -P.
        let (c, sure) = candidates(&t(None, &[4369, 5671, 5672, 15671, 15672, 15691, 15692, 25672], &[]));
        assert!(!sure);
        assert_eq!(c[0], 15672);
        assert_eq!(&c[c.len() - 3..], &[4369, 5672, 25672]);
        // Declared 443 and 80: 80, not the lower-sorted one.
        assert_eq!(candidates(&t(None, &[], &[443, 80])).0[0], 80);
    }

    #[test]
    fn what_it_listens_on_decides_among_candidates() {
        assert_eq!(choose(&[80, 8080], Some(&[8080])), 8080);
        assert_eq!(choose(&[80, 8080], None), 80);
        // Nothing declared or published: the best port it listens on.
        assert_eq!(choose(&[], Some(&[5432, 8000])), 8000);
        assert_eq!(choose(&[], None), 80);
        // Listening elsewhere only: keep the first candidate.
        assert_eq!(choose(&[3000], Some(&[9229])), 3000);
    }
}
