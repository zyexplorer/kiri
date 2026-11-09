//! Container domains: `http://<name>.kiri.local` reaches a running
//! container's HTTP port, and Compose services answer at
//! `<service>.<project>.kiri.local`. dns.rs points those names at loopback;
//! `*.kiri.localhost` works too, with no setup, since macOS resolves
//! `*.localhost` by itself. An unprivileged process may listen on port 80 on
//! the wildcard address, so serving needs no root. Only connections from
//! this Mac are served.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use super::engine::Engine;

/// Largest request head read before routing.
const MAX_HEAD: usize = 64 * 1024;

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

/// Starts the HTTPS router on port 443 (or `KIRI_HTTPS_PORT`): the same
/// routing, behind TLS with certificates from tls.rs. Returns the port.
pub fn start_tls(engine: Arc<Engine>) -> io::Result<u16> {
    let port: u16 = std::env::var("KIRI_HTTPS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(443);
    let config = super::tls::server_config()?;
    let mut bound = false;
    for addr in router_addrs(port) {
        let Ok(listener) = TcpListener::bind(&addr) else {
            continue;
        };
        bound = true;
        let (engine, config) = (engine.clone(), config.clone());
        std::thread::Builder::new()
            .name("kiri-domains-tls".into())
            .spawn(move || {
                for conn in listener.incoming().flatten() {
                    // A client that never finishes its request must not hold a thread forever.
                    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(30)));
                    let (engine, config) = (engine.clone(), config.clone());
                    let _ = std::thread::Builder::new()
                        .name("kiri-domain-tls".into())
                        .spawn(move || {
                            let _ = serve_tls(conn, &engine, config);
                        });
                }
            })?;
    }
    if !bound {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("port {port} is taken; set KIRI_HTTPS_PORT"),
        ));
    }
    Ok(port)
}

/// Starts the router on port 80 (or `KIRI_HTTP_PORT`). Returns the port.
pub fn start(engine: Arc<Engine>) -> io::Result<u16> {
    let port: u16 = std::env::var("KIRI_HTTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(80);
    let mut bound = false;
    for addr in router_addrs(port) {
        let Ok(listener) = TcpListener::bind(&addr) else {
            continue;
        };
        bound = true;
        let engine = engine.clone();
        std::thread::Builder::new()
            .name("kiri-domains".into())
            .spawn(move || {
                for conn in listener.incoming().flatten() {
                    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(30)));
                    let engine = engine.clone();
                    let _ = std::thread::Builder::new()
                        .name("kiri-domain-conn".into())
                        .spawn(move || {
                            let _ = serve(conn, &engine);
                        });
                }
            })?;
    }
    if !bound {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("port {port} is taken; set KIRI_HTTP_PORT"),
        ));
    }
    Ok(port)
}

fn serve(mut client: TcpStream, engine: &Engine) -> io::Result<()> {
    // The listener faces the network; only this Mac may use it.
    if !client.peer_addr()?.ip().is_loopback() {
        return Ok(());
    }
    client.set_nodelay(true)?;
    let Some((head, guest)) = route(&mut client, engine, "http")? else {
        return Ok(());
    };
    let mut to_guest = std::fs::File::from(guest.fd.try_clone()?);
    // What the client sent so far goes first, unchanged.
    to_guest.write_all(&head)?;
    let mut from_guest = std::fs::File::from(guest.fd.try_clone()?);
    let mut client_read = client.try_clone()?;
    let up = std::thread::Builder::new()
        .name("kiri-domain-up".into())
        .spawn(move || {
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
fn route(
    client: &mut impl ReadWrite,
    engine: &Engine,
    scheme: &str,
) -> io::Result<Option<(Vec<u8>, kiri_vmm::VsockStream)>> {
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
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
                .map(|(_, v)| v.trim().to_owned())
        })
        .unwrap_or_default();
    let Some(target) = engine.route(&host) else {
        let list: String = engine
            .domains()
            .iter()
            .map(|d| {
                let d = escape(d);
                format!("<li><a href=\"{scheme}://{d}\">{d}</a></li>")
            })
            .collect();
        let body = format!(
            "<!doctype html><meta charset=utf-8><title>Kiri</title><body style=\"font:15px -apple-system,sans-serif;margin:40px\">\
             <h1>No running container for {}</h1><p>Running containers:</p><ul>{}</ul>",
            escape(&host),
            if list.is_empty() {
                "<li>none</li>".to_owned()
            } else {
                list
            }
        );
        respond(client, 404, "Not Found", &body)?;
        return Ok(None);
    };
    let port = web_port(engine, &target);
    match engine.relay_to(target.ip, port) {
        Ok(g) => Ok(Some((head, g))),
        Err(e) => {
            respond(
                client,
                502,
                "Bad Gateway",
                &format!(
                    "Kiri could not reach the container: {}",
                    escape(&e.to_string())
                ),
            )?;
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
const WEB_PORTS: [u16; 14] = [
    80, 8080, 3000, 8000, 5000, 5173, 4200, 8888, 9000, 8081, 4000, 3001, 15672, 8443,
];
/// Ports that are surely not a web page: databases, queues, caches.
const NOT_WEB: [u16; 12] = [
    5432, 3306, 6379, 27017, 5672, 4369, 25672, 9042, 11211, 1433, 1521, 2181,
];

/// The ports that could be the web port, best first, and whether the first
/// is certain (the label, or the only port published): published ports
/// before ports only declared, and within each, common web ports first and
/// database and queue ports last.
fn candidates(t: &Target) -> (Vec<u16>, bool) {
    if let Some(p) = t.label {
        return (vec![p], true);
    }
    let rank = |p: &u16| {
        WEB_PORTS
            .iter()
            .position(|w| w == p)
            .unwrap_or(if NOT_WEB.contains(p) {
                usize::MAX
            } else {
                WEB_PORTS.len()
            })
    };
    let sorted = |v: &[u16]| {
        let mut v = v.to_vec();
        v.sort_unstable_by_key(|p| (rank(p), *p));
        v.dedup();
        v
    };
    let mut out = sorted(&t.published);
    let sure = out.len() == 1;
    out.extend(
        sorted(&t.exposed)
            .into_iter()
            .filter(|p| !t.published.contains(p)),
    );
    (out, sure)
}

/// Of the candidates, the first the container listens on; with none
/// declared, the best port it listens on; else the first, or 80.
fn choose(cands: &[u16], listening: Option<&[u16]>) -> u16 {
    let Some(on) = listening.filter(|l| !l.is_empty()) else {
        return cands.first().copied().unwrap_or(80);
    };
    if let Some(p) = cands.iter().find(|p| on.contains(p)) {
        return *p;
    }
    if cands.is_empty() {
        let (best, _) = candidates(&Target {
            ip: [0; 4],
            id: String::new(),
            label: None,
            published: Vec::new(),
            exposed: on.to_vec(),
        });
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
    if let Some(&(at, p)) = SEEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .get(&t.id)
    {
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
    let conn =
        rustls::ServerConnection::new(config).map_err(|e| io::Error::other(e.to_string()))?;
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
            libc::pollfd {
                fd: sock.as_raw_fd(),
                events: if client_open { libc::POLLIN } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: container.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfds, owned for the call.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let ready =
            |f: &libc::pollfd| f.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
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

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::{Target, candidates, choose};

    fn t(label: Option<u16>, published: &[u16], exposed: &[u16]) -> Target {
        Target {
            ip: [0; 4],
            id: String::new(),
            label,
            published: published.to_vec(),
            exposed: exposed.to_vec(),
        }
    }

    #[test]
    fn the_port_published_wins_over_the_ones_declared() {
        // `-p 8080:3000` on an image that declares 80.
        assert_eq!(
            candidates(&t(None, &[3000], &[80, 3000])),
            (vec![3000, 80], true)
        );
        assert_eq!(
            candidates(&t(Some(9000), &[3000], &[80])),
            (vec![9000], true)
        );
    }

    #[test]
    fn several_ports_put_web_ports_first_and_databases_last() {
        // rabbitmq:management with -P.
        let (c, sure) = candidates(&t(
            None,
            &[4369, 5671, 5672, 15671, 15672, 15691, 15692, 25672],
            &[],
        ));
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
