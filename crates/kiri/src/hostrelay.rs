//! `kiri hostrelay`: lets Kiri's engine in WSL2 reach a proxy on Windows' own
//! loopback, `127.0.0.1:7892`, where most Windows proxy apps listen. In
//! WSL's default NAT networking the distro's 127.0.0.1 is its own, and
//! Windows' firewall keeps the WSL network away from Windows' ports, so a
//! proxy the browser uses was refused to every image pull.
//!
//! The connections take the way kiri-win already talks to WSL: a wsl.exe
//! process's stdin and stdout. kiri-win (crates/kiri-win/src/relay.rs)
//! starts this and asks it to LISTEN for each proxy port; it listens on the
//! distro's 127.0.0.1 and sends each connection to it back as frames, and
//! kiri-win dials the proxy on Windows. kiri-win dials only the ports it
//! asked for, so nothing in WSL can reach any other Windows port this way.
//! The engine pulls through `127.0.0.1:<the port here>` (daemon/proxy.rs).
//!
//! Both ends share this file. Frames are the bridge's (crates/kiri/src/bridge.rs):
//! kind, connection id (u32 BE), length (u32 BE), then the data.

// Each build uses one end: the WSL end in Linux's kiri, the Windows end in
// kiri-win. The tests run both.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// WSL → Windows: a connection to the proxy on the port in the data (u16 BE).
pub const OPEN: u8 = 1;
pub const DATA: u8 = 2;
/// No more data from this side (a half-close).
pub const EOF: u8 = 3;
/// The connection is gone, both ways.
pub const CLOSE: u8 = 4;
/// Windows → WSL: listen for this Windows port (u16 BE).
pub const LISTEN: u8 = 6;
/// WSL → Windows: the Windows port, then the distro's port that leads to
/// it (both u16 BE); 0 when it could not listen.
pub const LISTENING: u8 = 7;

/// Neither end sends more data than this in a frame; a longer one is refused
/// before it is allocated.
const CHUNK: usize = 64 * 1024;
const MAX_FRAME: usize = 1 << 20;
/// Frames waiting for one connection's writer. When it is full the reader
/// waits, which slows the other connections too; a proxy relay has a few
/// connections that all keep reading, and nothing grows without bound.
const QUEUE: usize = 64;

pub type Out = Arc<Mutex<dyn Write + Send>>;

pub fn send(out: &Out, kind: u8, id: u32, data: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(9 + data.len());
    frame.push(kind);
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(data);
    let mut w = out.lock().unwrap_or_else(PoisonError::into_inner);
    w.write_all(&frame)?;
    w.flush()
}

/// The next frame, or None when the other end has gone.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, u32, Vec<u8>)>> {
    let mut h = [0u8; 9];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let id = u32::from_be_bytes([h[1], h[2], h[3], h[4]]);
    let len = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes")));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok(Some((h[0], id, data)))
}

fn port_of(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

enum Op {
    Data(Vec<u8>),
    Eof,
}

struct Entry {
    tx: SyncSender<Op>,
    /// Set once the connection is made; a CLOSE before then just forgets it.
    stream: Option<TcpStream>,
}

/// The connections open through one relay, by id.
#[derive(Default)]
pub struct Conns(Mutex<HashMap<u32, Entry>>);

impl Conns {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Entry>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new connection: what arrives for it waits here until it is made.
    fn open(&self, id: u32) -> Receiver<Op> {
        let (tx, rx) = sync_channel(QUEUE);
        self.map().insert(id, Entry { tx, stream: None });
        rx
    }

    /// A frame for one of the connections.
    fn dispatch(&self, kind: u8, id: u32, data: Vec<u8>) {
        let op = match kind {
            DATA => Op::Data(data),
            EOF => Op::Eof,
            CLOSE => {
                if let Some(e) = self.map().remove(&id)
                    && let Some(s) = e.stream
                {
                    let _ = s.shutdown(Shutdown::Both);
                }
                return;
            }
            _ => return,
        };
        // Sent outside the lock: a full queue waits for its own writer only.
        let tx = self.map().get(&id).map(|e| e.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(op);
        }
    }

}

/// Joins `stream` to connection `id`: what it reads goes to the other end
/// as DATA, then EOF; what arrives for it (`rx`) is written to it.
fn attach(conns: &Arc<Conns>, out: &Out, id: u32, rx: Receiver<Op>, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let (Ok(reader), Ok(writer)) = (stream.try_clone(), stream.try_clone()) else {
        conns.map().remove(&id);
        let _ = send(out, CLOSE, id, &[]);
        return;
    };
    match conns.map().get_mut(&id) {
        Some(e) => e.stream = Some(stream),
        // Closed while it was being made.
        None => {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
    }
    // Each half that ends counts; the second one forgets the connection.
    let halves = Arc::new(AtomicU8::new(0));
    let done = {
        let conns = conns.clone();
        let halves = halves.clone();
        move || {
            if halves.fetch_add(1, Ordering::SeqCst) == 1 {
                conns.map().remove(&id);
            }
        }
    };
    let done2 = done.clone();
    let (c, o) = (conns.clone(), out.clone());
    std::thread::spawn(move || {
        let mut writer = writer;
        for op in rx {
            match op {
                Op::Data(d) => {
                    if writer.write_all(&d).is_err() {
                        let _ = writer.shutdown(Shutdown::Both);
                        c.map().remove(&id);
                        let _ = send(&o, CLOSE, id, &[]);
                        return;
                    }
                }
                Op::Eof => {
                    let _ = writer.shutdown(Shutdown::Write);
                    break;
                }
            }
        }
        done2();
    });
    let o = out.clone();
    let c = conns.clone();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = send(&o, EOF, id, &[]);
                    break;
                }
                Ok(n) => {
                    if send(&o, DATA, id, &buf[..n]).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    // Gone while the other end may still be sending: say so.
                    if c.map().remove(&id).is_some() {
                        let _ = send(&o, CLOSE, id, &[]);
                    }
                    break;
                }
            }
        }
        done();
    });
}

/// The WSL end (`kiri hostrelay`): listens as kiri-win asks and sends each
/// connection over `out`, until kiri-win goes, and the listeners with it.
pub fn listen_side(input: impl Read, out: &Out) -> io::Result<()> {
    let conns = Arc::new(Conns::default());
    let next = Arc::new(AtomicU32::new(1));
    let mut listening: HashMap<u16, u16> = HashMap::new();
    let mut r = BufReader::with_capacity(CHUNK, input);
    while let Some((kind, id, data)) = read_frame(&mut r)? {
        if kind != LISTEN {
            conns.dispatch(kind, id, data);
            continue;
        }
        let Some(target) = port_of(&data, 0) else { continue };
        let local = match listening.get(&target) {
            Some(&p) => p,
            None => {
                let p = listen(target, &conns, out, &next).unwrap_or(0);
                if p != 0 {
                    listening.insert(target, p);
                }
                p
            }
        };
        let mut reply = target.to_be_bytes().to_vec();
        reply.extend_from_slice(&local.to_be_bytes());
        send(out, LISTENING, 0, &reply)?;
    }
    Ok(())
}

/// Listens on the distro's loopback for `target`; its port.
fn listen(target: u16, conns: &Arc<Conns>, out: &Out, next: &Arc<AtomicU32>) -> io::Result<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    let port = l.local_addr()?.port();
    let (conns, out, next) = (conns.clone(), out.clone(), next.clone());
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { continue };
            let id = next.fetch_add(1, Ordering::SeqCst);
            let rx = conns.open(id);
            if send(&out, OPEN, id, &target.to_be_bytes()).is_err() {
                return;
            }
            attach(&conns, &out, id, rx, s);
        }
    });
    Ok(port)
}

/// The Windows end: dials the proxy for each connection the WSL end sends,
/// on the ports `allowed` names, and hands LISTENING replies to `listening`.
pub fn dial_side(input: impl Read, out: &Out, allowed: impl Fn(u16) -> bool, listening: impl Fn(u16, u16)) -> io::Result<()> {
    let conns = Arc::new(Conns::default());
    let mut r = BufReader::with_capacity(CHUNK, input);
    while let Some((kind, id, data)) = read_frame(&mut r)? {
        match kind {
            OPEN => {
                let target = port_of(&data, 0).filter(|&p| allowed(p));
                let Some(target) = target else {
                    send(out, CLOSE, id, &[])?;
                    continue;
                };
                // Data that arrives while it dials waits in the queue.
                let rx = conns.open(id);
                let (c, o) = (conns.clone(), out.clone());
                std::thread::spawn(move || match dial(target) {
                    Ok(s) => attach(&c, &o, id, rx, s),
                    Err(_) => {
                        c.map().remove(&id);
                        let _ = send(&o, CLOSE, id, &[]);
                    }
                });
            }
            LISTENING => {
                if let (Some(target), Some(local)) = (port_of(&data, 0), port_of(&data, 2)) {
                    listening(target, local);
                }
            }
            _ => conns.dispatch(kind, id, data),
        }
    }
    Ok(())
}

/// This machine's loopback at `port`: IPv4, then IPv6.
fn dial(port: u16) -> io::Result<TcpStream> {
    let wait = Duration::from_secs(5);
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), wait).or_else(|e| TcpStream::connect_timeout(&(std::net::Ipv6Addr::LOCALHOST, port).into(), wait).map_err(|_| e))
}

/// Where in a proxy address its host and port are, when the host is this
/// machine's loopback: `http://user:pw@127.0.0.1:7892/` → (7892, the span of
/// `127.0.0.1:7892`). Without a port there is nothing to relay.
pub fn loopback(url: &str) -> Option<(u16, std::ops::Range<usize>)> {
    let start = url.find("://").map_or(0, |i| i + 3);
    let rest = &url[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let at = rest[..end].rfind('@').map_or(0, |i| i + 1);
    let hostport = &rest[at..end];
    let (host, port) = hostport.rsplit_once(':')?;
    let port: u16 = port.parse().ok().filter(|&p| p != 0)?;
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    let is_loopback = host == "localhost" || host == "::1" || host.parse::<std::net::Ipv4Addr>().is_ok_and(|a| a.is_loopback());
    is_loopback.then_some((port, start + at..start + end))
}

/// The distro's own address for a Windows loopback proxy, through the relay.
pub fn relayed(url: &str, map: &HashMap<u16, u16>) -> Option<String> {
    let (port, span) = loopback(url)?;
    let local = map.get(&port)?;
    Some(format!("{}127.0.0.1:{local}{}", &url[..span.start], &url[span.end..]))
}

pub fn run() -> io::Result<()> {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    listen_side(io::stdin(), &out)
}

// Over unix socket pairs: these run with kiri's tests, not kiri-win's.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn proxy_addresses_on_loopback() {
        assert_eq!(loopback("127.0.0.1:7892").map(|l| l.0), Some(7892));
        let url = "http://user:p@ss@localhost:8080/";
        let (port, span) = loopback(url).unwrap();
        assert_eq!((port, &url[span]), (8080, "localhost:8080"));
        assert_eq!(loopback("http://[::1]:3128").map(|l| l.0), Some(3128));
        assert_eq!(loopback("socks5://127.0.0.2:1080").map(|l| l.0), Some(1080));
        for none in ["http://10.0.0.1:3128", "http://proxy.corp:8080", "http://127.0.0.1", "http://127.0.0.1:0", "", "http://127.0.0.1:x"] {
            assert_eq!(loopback(none), None, "{none}");
        }
        let map = HashMap::from([(7892, 41234)]);
        assert_eq!(relayed("http://u:p@127.0.0.1:7892/x", &map).as_deref(), Some("http://u:p@127.0.0.1:41234/x"));
        assert_eq!(relayed("127.0.0.1:7892", &map).as_deref(), Some("127.0.0.1:41234"));
        assert_eq!(relayed("http://127.0.0.1:9999", &map), None);
        assert_eq!(relayed("http://proxy.corp:7892", &map), None);
    }

    /// Both ends over a pair of sockets, as wsl.exe's stdin and stdout carry
    /// them: a "proxy" on this machine's loopback, reached through the relay.
    fn relay(allowed: u16) -> (Arc<Mutex<HashMap<u16, u16>>>, Out) {
        let (wsl, win) = UnixStream::pair().unwrap();
        let wsl_out: Out = Arc::new(Mutex::new(wsl.try_clone().unwrap()));
        let win_out: Out = Arc::new(Mutex::new(win.try_clone().unwrap()));
        std::thread::spawn(move || listen_side(wsl, &wsl_out));
        let ports = Arc::new(Mutex::new(HashMap::new()));
        let p = ports.clone();
        let o = win_out.clone();
        std::thread::spawn(move || dial_side(win, &o, move |t| t == allowed, move |t, l| { p.lock().unwrap().insert(t, l); }));
        (ports, win_out)
    }

    fn listen_for(ports: &Arc<Mutex<HashMap<u16, u16>>>, out: &Out, target: u16) -> u16 {
        send(out, LISTEN, 0, &target.to_be_bytes()).unwrap();
        for _ in 0..200 {
            if let Some(&l) = ports.lock().unwrap().get(&target) {
                return l;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no LISTENING for {target}");
    }

    #[test]
    fn connections_reach_the_proxy_and_back() {
        // The proxy: echoes each connection, upper-cased, until its half-close.
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = proxy.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in proxy.incoming() {
                let mut s = s.unwrap();
                std::thread::spawn(move || {
                    let mut all = Vec::new();
                    s.read_to_end(&mut all).unwrap();
                    s.write_all(&all.to_ascii_uppercase()).unwrap();
                });
            }
        });
        let (ports, out) = relay(target);
        let local = listen_for(&ports, &out, target);
        assert_ne!(local, 0);
        assert_eq!(listen_for(&ports, &out, target), local, "asking again keeps the port");
        // Several at once, one bigger than many frames and the queue.
        let big: Vec<u8> = (0..3_000_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let body = if i == 0 { big.clone() } else { format!("hello {i}").into_bytes() };
                std::thread::spawn(move || {
                    let mut c = TcpStream::connect(("127.0.0.1", local)).unwrap();
                    let mut w = c.try_clone().unwrap();
                    let b = body.clone();
                    let t = std::thread::spawn(move || {
                        w.write_all(&b).unwrap();
                        w.shutdown(Shutdown::Write).unwrap();
                    });
                    let mut got = Vec::new();
                    c.read_to_end(&mut got).unwrap();
                    t.join().unwrap();
                    assert_eq!(got, body.to_ascii_uppercase());
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn only_the_ports_asked_for_are_dialled() {
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = other.local_addr().unwrap().port();
        // kiri-win allows some other port; the WSL end listens for this one anyway.
        let (ports, out) = relay(1);
        let local = listen_for(&ports, &out, target);
        let mut c = TcpStream::connect(("127.0.0.1", local)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 1];
        assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)), "closed without reaching the port");
        other.set_nonblocking(true).unwrap();
        assert!(other.accept().is_err(), "nothing dialled the port");
    }

    #[test]
    fn a_proxy_that_is_not_running_closes_the_connection() {
        let gone = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = gone.local_addr().unwrap().port();
        drop(gone);
        let (ports, out) = relay(target);
        let local = listen_for(&ports, &out, target);
        let mut c = TcpStream::connect(("127.0.0.1", local)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buf = [0u8; 1];
        assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)));
    }
}
