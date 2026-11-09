//! Published ports, host side: a listener per `-p` binding on the Mac. Each
//! client connection is passed on over the VM's NAT network to a port the
//! guest kernel forwards (DNAT) to the container, so the guest copies
//! nothing; vsock is several times slower in Virtualization.framework. Where
//! that network is unreachable (a VPN that takes over its routes, an older
//! agent), the connection gets its own vsock stream into the VM instead,
//! whose agent connects to the container and relays. No root either way.
//!
//! On a Linux host the daemon shares the kernel with the containers: the
//! kernel forwards (DNAT) every published port reached on a non-loopback
//! address, and the daemon relays the host's own loopback connections
//! itself, with splice(2), as Docker's userland proxy does. A relay ends
//! the client's TCP connection on `lo` and opens a second one to the
//! container, so the work of one stream is split across two flows and
//! CPUs; through the bridge alone, one CPU carried it all and the port
//! ran at about half of Docker's speed on 127.0.0.1.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::vmlink::VmLink;

/// The guest agent's vsock port for forwarded connections.
const RELAY_PORT: u32 = 1025;
/// …and for forwarded UDP, one stream per client address.
const UDP_RELAY_PORT: u32 = 1029;
/// Forwarded connections whose replies come back framed, so their end is
/// a frame and not the stream's half-close, which Virtualization.framework
/// sometimes drops; agents before it have only `RELAY_PORT`.
const RELAY_FRAMED_PORT: u32 = 1030;
static NO_FRAMED_RELAY: AtomicBool = AtomicBool::new(false);
/// A UDP client quiet this long has its session closed, as conntrack does.
const UDP_IDLE: Duration = Duration::from_secs(60);
const TCP_IDLE: Duration = Duration::from_secs(300);
const MAX_TCP_SESSIONS: usize = 1024;
static TCP_SESSIONS: AtomicUsize = AtomicUsize::new(0);
/// Connections from other machines (a port published on 0.0.0.0 is open
/// to the LAN): each may hold this many at once, and all of them together
/// half of `MAX_TCP_SESSIONS`, so a LAN host keeping idle connections open
/// can neither use up the Mac's own (loopback is not limited by these) nor
/// every other machine's.
const MAX_PEER_SESSIONS: usize = 64;
const MAX_REMOTE_SESSIONS: usize = MAX_TCP_SESSIONS / 2;
static REMOTE_SESSIONS: Mutex<BTreeMap<IpAddr, usize>> = Mutex::new(BTreeMap::new());
/// Guest ports the VM's kernel forwards published TCP ports on, one per
/// binding: above Linux's ephemeral range (32768-60999), so never a port
/// the guest picks for a connection of its own.
const NAT_PORTS: std::ops::RangeInclusive<u16> = 61000..=65535;
static NAT_IN_USE: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());
/// A connect over the NAT network answers in well under a millisecond, or
/// not at all when something else has its route. A SYN the network dropped
/// would be sent again only after 1 s: the connection goes over vsock
/// instead, where the agent connects within the guest.
const NAT_CONNECT: Duration = Duration::from_millis(300);
/// After a connect found no route (a VPN, or macOS's Local Network
/// permission not given), or two in a row had no answer, connections go
/// over vsock this long before the NAT network is tried again.
const NAT_RETRY: Duration = Duration::from_secs(30);
static NAT_DOWN_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
static NAT_TIMEOUTS: AtomicUsize = AtomicUsize::new(0);
/// New connections over the NAT network: a burst of `NAT_BURST`, then
/// `NAT_RATE` a second; the rest go over vsock, which takes thousands a
/// second (bulk transfers are what the NAT network is for). Each connection
/// there is a flow in the Mac's packet filter, whose state table (10,000
/// entries, shared by the whole Mac) keeps it for up to 90 s after it
/// closes. Once that is full the Mac drops every new SYN on the network
/// until entries expire: ab against a host-network nginx in the guest
/// stopped after 9,706 connections. And the guest masquerades each one to
/// a random port of the bridge address, which, past a few hundred a second,
/// lands on ports the container still holds in TIME_WAIT with timestamps
/// from another connection: the SYN is dropped (PAWS) and retried after 1 s.
const NAT_BURST: f64 = 512.0;
const NAT_RATE: f64 = 32.0;
static NAT_BUDGET: Mutex<Option<(f64, Instant)>> = Mutex::new(None);
/// Per read and write in the relays: a buffer that starts small, so an
/// idle or chatty connection holds little, and doubles each time a read
/// fills it, so a bulk transfer makes fewer, larger system calls (1 MiB
/// costs a quarter less CPU per byte than 256 KiB from the VM to the Mac).
const COPY_BUF: usize = 64 * 1024;
const COPY_BUF_MAX: usize = 1024 * 1024;

/// A guest port in `NAT_PORTS`, free again when dropped.
struct NatPort(u16);

impl Drop for NatPort {
    fn drop(&mut self) {
        NAT_IN_USE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

fn nat_port() -> Option<NatPort> {
    let mut used = NAT_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    // The lowest free one: a port is reused only once its rule is gone.
    let port = NAT_PORTS.clone().find(|p| !used.contains(p))?;
    used.insert(port);
    Some(NatPort(port))
}

fn nat_usable() -> bool {
    let mut down = NAT_DOWN_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
    match *down {
        Some(t) if Instant::now() < t => false,
        Some(_) => {
            *down = None;
            true
        }
        None => true,
    }
}

/// Takes one new connection's place from the NAT network's budget, if one is left.
fn nat_budget() -> bool {
    let mut budget = NAT_BUDGET.lock().unwrap_or_else(|e| e.into_inner());
    let (left, ok) = take_budget(*budget, Instant::now());
    *budget = Some(left);
    ok
}

/// A token bucket: `NAT_BURST` full, `NAT_RATE` a second back.
fn take_budget(state: Option<(f64, Instant)>, now: Instant) -> ((f64, Instant), bool) {
    let (tokens, at) = state.unwrap_or((NAT_BURST, now));
    let tokens =
        (tokens + now.saturating_duration_since(at).as_secs_f64() * NAT_RATE).min(NAT_BURST);
    if tokens >= 1.0 {
        ((tokens - 1.0, now), true)
    } else {
        ((tokens, now), false)
    }
}

/// One relayed connection; `peer` is set for one from another machine.
struct TcpPermit {
    peer: Option<IpAddr>,
}

impl Drop for TcpPermit {
    fn drop(&mut self) {
        TCP_SESSIONS.fetch_sub(1, Ordering::AcqRel);
        if let Some(peer) = self.peer {
            let mut peers = REMOTE_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = peers.get_mut(&peer) {
                *n -= 1;
                if *n == 0 {
                    peers.remove(&peer);
                }
            }
        }
    }
}

fn tcp_permit(peer: IpAddr) -> Option<TcpPermit> {
    let peer = peer.to_canonical();
    let mut remote =
        (!peer.is_loopback()).then(|| REMOTE_SESSIONS.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(peers) = &remote {
        if peers.get(&peer).copied().unwrap_or(0) >= MAX_PEER_SESSIONS
            || peers.values().sum::<usize>() >= MAX_REMOTE_SESSIONS
        {
            return None;
        }
    }
    let mut current = TCP_SESSIONS.load(Ordering::Relaxed);
    loop {
        if current >= MAX_TCP_SESSIONS {
            return None;
        }
        match TCP_SESSIONS.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
    if let Some(peers) = &mut remote {
        *peers.entry(peer).or_default() += 1;
    }
    Some(TcpPermit {
        peer: remote.map(|_| peer),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub container_port: u16,
    /// `tcp` or `udp`.
    pub proto: String,
    /// Empty means all interfaces, as Docker does.
    pub host_ip: String,
    /// 0 asks for any free port; the chosen one is reported once bound.
    pub host_port: u16,
}

/// A running forward for one container; dropping it closes every listener.
pub struct Forwards {
    wake: Vec<OwnedFd>,
    threads: Vec<JoinHandle<()>>,
    /// Linux host: the kernel forwards these (sockets stay bound so the
    /// ports read as taken); dropping tells the agent to stop.
    kernel: Option<(Arc<VmLink>, String, Vec<TcpListener>, Vec<UdpSocket>)>,
    /// In the VM: the guest ports its kernel forwards for these listeners;
    /// dropping tells the agent to stop, then frees the ports.
    nat: Option<(Arc<VmLink>, String, Vec<NatPort>)>,
    /// What was actually bound: (binding, host address, host port).
    pub bound: Vec<(Binding, String, u16)>,
}

impl std::fmt::Debug for Forwards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forwards")
            .field("bound", &self.bound)
            .finish()
    }
}

impl Drop for Forwards {
    fn drop(&mut self) {
        if let Some((vm, id, ..)) = self.kernel.take() {
            let _ = vm.unpublish(&id);
        }
        if let Some((vm, id, _ports)) = self.nat.take() {
            let _ = vm.unpublish(&id);
        }
        // Closing the write ends wakes every listener thread.
        self.wake.clear();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe succeeded, so both are fresh and ours.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn kernel_host_ip(ip: &str) -> Option<[u8; 4]> {
    if ip.is_empty() || ip == "0.0.0.0" {
        Some([0; 4])
    } else {
        ip.parse::<std::net::Ipv4Addr>().ok().map(|a| a.octets())
    }
}

/// Linux host: whether the kernel forwards a binding on `ip`. Every IPv4
/// address but loopback (the rule for 0.0.0.0 skips 127.0.0.0/8 too):
/// loopback is relayed, so the bridges never need `route_localnet`.
fn kernel_forwards(ip: &str) -> bool {
    kernel_host_ip(ip).is_some_and(|a| a[0] != 127)
}

/// Linux host: whether a binding's listener also relays, for the
/// connections the kernel leaves to it: loopback, IPv6, and the 127.0.0.1
/// share of one on every address.
fn relays_too(ip: &str) -> bool {
    !kernel_forwards(ip) || kernel_host_ip(ip) == Some([0; 4])
}

/// A socket bound to `addr`; an IPv6 one with IPV6_V6ONLY, so `[::]:port`
/// sits beside `0.0.0.0:port` (as Docker binds both) instead of clashing
/// with it, and serves IPv6 alone.
fn bind_socket(addr: SocketAddr, udp: bool) -> io::Result<OwnedFd> {
    let (family, ty) = (
        if addr.is_ipv6() {
            libc::AF_INET6
        } else {
            libc::AF_INET
        },
        if udp {
            libc::SOCK_DGRAM
        } else {
            libc::SOCK_STREAM
        },
    );
    // SAFETY: plain socket creation; owned at once.
    let fd = unsafe { libc::socket(family, ty, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is fresh and ours.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    let set = |level, opt| {
        // SAFETY: setsockopt with an int of the stated size.
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                level,
                opt,
                (&one as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    // As std's TcpListener::bind does: a port in TIME_WAIT can be bound again.
    if !udp {
        set(libc::SOL_SOCKET, libc::SO_REUSEADDR);
    }
    if addr.is_ipv6() && set(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY) != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the sockaddr storage is filled for `addr`'s family and sized to match.
    let (storage, len) = sockaddr_of(addr);
    if unsafe {
        libc::bind(
            sock.as_raw_fd(),
            (&storage as *const libc::sockaddr_storage).cast(),
            len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: listen on a bound stream socket we own.
    if !udp && unsafe { libc::listen(sock.as_raw_fd(), 1024) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

fn sockaddr_of(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: sockaddr_storage is plain data; all-zero is valid.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            // SAFETY: sockaddr_storage is large and aligned enough for a sockaddr_in.
            let sin = unsafe {
                &mut *(&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>()
            };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
            #[cfg(target_os = "macos")]
            {
                sin.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
            }
            std::mem::size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            // SAFETY: as above, for a sockaddr_in6.
            let sin6 = unsafe {
                &mut *(&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>()
            };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_addr.s6_addr = a.ip().octets();
            sin6.sin6_scope_id = a.scope_id();
            #[cfg(target_os = "macos")]
            {
                sin6.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
            }
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    (storage, len as libc::socklen_t)
}

/// `host_ip` and `port` as a socket address: `::1` and `[::1]` both work,
/// as the CLI sends `-p [::1]:8080:80` bare and the API may bracket it.
fn host_addr(ip: &str, port: u16) -> io::Result<SocketAddr> {
    let bare = ip
        .strip_prefix('[')
        .and_then(|i| i.strip_suffix(']'))
        .unwrap_or(ip);
    bare.parse::<IpAddr>()
        .map(|a| SocketAddr::new(a, port))
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad host address {ip}"),
            )
        })
}

/// What one binding listens on: its own address, or with no address both
/// 0.0.0.0 and `::` (the IPv6 one only when the host has IPv6), as Docker
/// binds them and lists them in `docker port`.
fn listen_addrs(b: &Binding) -> Vec<(String, bool)> {
    match b.host_ip.as_str() {
        "" => vec![("0.0.0.0".to_owned(), true), ("::".to_owned(), false)],
        ip => vec![(
            ip.strip_prefix('[')
                .and_then(|i| i.strip_suffix(']'))
                .unwrap_or(ip)
                .to_owned(),
            true,
        )],
    }
}

/// Binds every port first (so a clash fails the start cleanly), then starts
/// one accept thread per listener.
pub fn start(
    vm: Arc<VmLink>,
    id: &str,
    bindings: &[Binding],
    target_ip: [u8; 4],
) -> io::Result<Forwards> {
    let mut listeners = Vec::with_capacity(bindings.len());
    let mut udp = Vec::new();
    for b in bindings {
        // A port picked by the kernel (0) is picked once: the `::` listener
        // takes the one 0.0.0.0 got.
        let mut port = b.host_port;
        for (ip, required) in listen_addrs(b) {
            let addr = host_addr(&ip, port)?;
            let taken = |e: io::Error| {
                if e.kind() == io::ErrorKind::AddrInUse {
                    let shown = if addr.is_ipv6() {
                        format!("[{ip}]")
                    } else {
                        ip.clone()
                    };
                    io::Error::new(
                        e.kind(),
                        format!(
                            "Bind for {shown}:{} failed: port is already allocated",
                            b.host_port
                        ),
                    )
                } else {
                    e
                }
            };
            let sock = match bind_socket(addr, b.proto == "udp") {
                Ok(s) => s,
                Err(e) if required => return Err(taken(e)),
                // No IPv6 on this host, or `::` of this port held by something
                // else: IPv4 alone, as Docker does without IPv6.
                Err(e) => {
                    crate::diag::debug!(
                        "published port has no IPv6 listener",
                        port = port,
                        error = e
                    );
                    continue;
                }
            };
            if b.proto == "udp" {
                let s = UdpSocket::from(sock);
                s.set_nonblocking(true)?;
                port = s.local_addr()?.port();
                udp.push((b.clone(), ip, port, s));
                continue;
            }
            let l = TcpListener::from(sock);
            l.set_nonblocking(true)?;
            port = l.local_addr()?.port();
            listeners.push((b.clone(), ip, port, l));
        }
    }
    let mut fw = Forwards {
        wake: Vec::new(),
        threads: Vec::new(),
        kernel: None,
        nat: None,
        bound: Vec::new(),
    };
    // On a Linux host the daemon shares the kernel with the containers, so
    // the kernel can rewrite the packets (nftables DNAT) instead of two
    // user-space relays copying every byte; that serves every address but
    // loopback. The listeners below relay the rest. A target on loopback (a
    // --network host container) stays relayed: that is a local socket.
    if cfg!(target_os = "linux") && target_ip != [127, 0, 0, 1] {
        let all = udp
            .iter()
            .map(|(b, ip, port, _)| (b, ip, *port, true))
            .chain(
                listeners
                    .iter()
                    .map(|(b, ip, port, _)| (b, ip, *port, false)),
            );
        let mut published = Ok(false);
        for (b, ip, port, is_udp) in all.filter(|(_, ip, ..)| kernel_forwards(ip)) {
            let p = kiri_proto::Publish {
                id,
                udp: is_udp,
                host_ip: kernel_host_ip(ip).expect("checked IPv4 binding"),
                host_port: port,
                to_ip: target_ip,
                to_port: b.container_port,
            };
            published = vm.publish(p).map(|()| true);
            if published.is_err() {
                break;
            }
        }
        match published {
            Ok(any) => {
                // Sockets only the kernel serves stay bound, so the ports read as taken.
                let (kernel_udp, relay_udp): (Vec<_>, Vec<_>) =
                    udp.into_iter().partition(|(_, ip, ..)| !relays_too(ip));
                let (kernel_tcp, relay_tcp): (Vec<_>, Vec<_>) = listeners
                    .into_iter()
                    .partition(|(_, ip, ..)| !relays_too(ip));
                fw.bound.extend(
                    kernel_udp
                        .iter()
                        .map(|(b, ip, port, _)| (b.clone(), ip.clone(), *port)),
                );
                fw.bound.extend(
                    kernel_tcp
                        .iter()
                        .map(|(b, ip, port, _)| (b.clone(), ip.clone(), *port)),
                );
                if any {
                    fw.kernel = Some((
                        vm.clone(),
                        id.to_owned(),
                        kernel_tcp.into_iter().map(|(.., s)| s).collect(),
                        kernel_udp.into_iter().map(|(.., s)| s).collect(),
                    ));
                }
                udp = relay_udp;
                listeners = relay_tcp;
            }
            Err(e) => {
                let _ = vm.unpublish(id);
                crate::diag::warn!(
                    "kernel port forwarding failed; relaying in user space",
                    id = &id[..id.len().min(12)],
                    error = e
                );
            }
        }
    }
    for (b, ip, port, sock) in udp {
        let (wake_r, wake_w) = pipe()?;
        fw.wake.push(wake_w);
        let vm = vm.clone();
        let container_port = b.container_port;
        fw.threads.push(
            std::thread::Builder::new()
                .name(format!("kiri-udp-{port}"))
                .spawn(move || {
                    udp_loop(&sock, &wake_r, &vm, target_ip, container_port);
                })?,
        );
        fw.bound.push((b, ip, port));
    }
    // In the VM: the guest kernel forwards each TCP binding from a port of
    // its own on the NAT network. A target on loopback stays relayed.
    let mut nat = vec![None; listeners.len()];
    if cfg!(target_os = "macos") && target_ip != [127, 0, 0, 1] && !listeners.is_empty() {
        match publish_nat(
            &vm,
            id,
            &listeners
                .iter()
                .map(|(b, ..)| b.container_port)
                .collect::<Vec<_>>(),
            target_ip,
        ) {
            Ok((addrs, ports)) => {
                nat = addrs.into_iter().map(Some).collect();
                fw.nat = Some((vm.clone(), id.to_owned(), ports));
            }
            Err(e) => crate::diag::warn!(
                "NAT port forwarding unavailable; relaying over vsock",
                id = &id[..id.len().min(12)],
                error = e
            ),
        }
    }
    for ((b, ip, port, listener), nat) in listeners.into_iter().zip(nat) {
        let (wake_r, wake_w) = pipe()?;
        fw.wake.push(wake_w);
        let vm = vm.clone();
        let container_port = b.container_port;
        fw.threads.push(
            std::thread::Builder::new()
                .name(format!("kiri-port-{port}"))
                .spawn(move || {
                    accept_loop(&listener, &wake_r, &vm, target_ip, container_port, nat);
                })?,
        );
        fw.bound.push((b, ip, port));
    }
    Ok(fw)
}

/// Asks the agent to forward a guest port to each container port, from the
/// Mac only; returns the address the Mac dials for each. All or nothing.
fn publish_nat(
    vm: &VmLink,
    id: &str,
    container_ports: &[u16],
    target_ip: [u8; 4],
) -> io::Result<(Vec<SocketAddr>, Vec<NatPort>)> {
    let (guest_ip, _gateway) = vm.uplink()?;
    let mut addrs = Vec::with_capacity(container_ports.len());
    let mut ports = Vec::with_capacity(container_ports.len());
    for &to_port in container_ports {
        let Some(p) = nat_port() else {
            let _ = vm.unpublish(id);
            return Err(io::Error::other("no free guest port"));
        };
        let publish = kiri_proto::Publish {
            id,
            udp: false,
            host_ip: guest_ip,
            host_port: p.0,
            to_ip: target_ip,
            to_port,
        };
        if let Err(e) = vm.publish(publish) {
            let _ = vm.unpublish(id);
            return Err(e);
        }
        addrs.push(SocketAddr::from((guest_ip, p.0)));
        ports.push(p);
    }
    Ok((addrs, ports))
}

/// One client address's session: a stream into the guest relay.
struct UdpSession {
    to_guest: std::fs::File,
    last: Instant,
    reader: JoinHandle<()>,
    // Keeps the connection open until the session ends.
    _guest: kiri_vmm::VsockStream,
}

impl UdpSession {
    fn close(self) {
        // SAFETY: shutting down a socket we own wakes the reader with EOF.
        unsafe { libc::shutdown(self.to_guest.as_raw_fd(), libc::SHUT_RDWR) };
        let _ = self.reader.join();
    }
}

/// Datagrams from each client go to the container over its own session;
/// replies come back to that client. Quiet sessions close.
fn udp_loop(sock: &UdpSocket, wake: &OwnedFd, vm: &Arc<VmLink>, ip: [u8; 4], port: u16) {
    let mut sessions: std::collections::HashMap<SocketAddr, UdpSession> =
        std::collections::HashMap::new();
    let mut buf = vec![0u8; 65_536];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: polling two descriptors we own; wake up now and then to reap.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, 5_000) };
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
        if fds[1].revents != 0 {
            break; // the container stopped
        }
        while let Ok((len, peer)) = sock.recv_from(&mut buf) {
            if !sessions.contains_key(&peer) {
                // Cap sessions so a source-spoofing flood cannot exhaust
                // threads and guest connections; drop the oldest to make room.
                if sessions.len() >= 1024 {
                    if let Some(old) = sessions.iter().min_by_key(|(_, s)| s.last).map(|(p, _)| *p)
                    {
                        if let Some(s) = sessions.remove(&old) {
                            s.close();
                        }
                    }
                }
                match open_udp_session(sock, vm, ip, port, peer) {
                    Ok(s) => {
                        sessions.insert(peer, s);
                    }
                    Err(_) => continue,
                }
            }
            let s = sessions.get_mut(&peer).expect("just inserted");
            s.last = Instant::now();
            let mut frame = Vec::with_capacity(len + 2);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
            frame.extend_from_slice(&buf[..len]);
            if s.to_guest.write_all(&frame).is_err() {
                if let Some(s) = sessions.remove(&peer) {
                    s.close();
                }
            }
        }
        let idle: Vec<SocketAddr> = sessions
            .iter()
            .filter(|(_, s)| s.last.elapsed() >= UDP_IDLE || s.reader.is_finished())
            .map(|(a, _)| *a)
            .collect();
        for a in idle {
            if let Some(s) = sessions.remove(&a) {
                s.close();
            }
        }
    }
    for (_, s) in sessions.drain() {
        s.close();
    }
}

fn open_udp_session(
    sock: &UdpSocket,
    vm: &VmLink,
    ip: [u8; 4],
    port: u16,
    peer: SocketAddr,
) -> io::Result<UdpSession> {
    let guest = vm.connect(UDP_RELAY_PORT)?;
    let mut to_guest = std::fs::File::from(guest.fd.try_clone()?);
    let mut header = [0u8; 6];
    header[..4].copy_from_slice(&ip);
    header[4..].copy_from_slice(&port.to_be_bytes());
    to_guest.write_all(&header)?;
    let mut from_guest = std::fs::File::from(guest.fd.try_clone()?);
    // A clone shares the socket (and its non-blocking mode); a send to a
    // client rarely has to wait, and a dropped reply is UDP's to lose.
    let reply = sock.try_clone()?;
    let reader = std::thread::Builder::new()
        .name("kiri-udp-reply".into())
        .spawn(move || {
            let mut len = [0u8; 2];
            let mut data = vec![0u8; 65_536];
            while from_guest.read_exact(&mut len).is_ok() {
                let n = u16::from_be_bytes(len) as usize;
                if from_guest.read_exact(&mut data[..n]).is_err() {
                    break;
                }
                let _ = reply.send_to(&data[..n], peer);
            }
        })?;
    Ok(UdpSession {
        to_guest,
        last: Instant::now(),
        reader,
        _guest: guest,
    })
}

fn accept_loop(
    listener: &TcpListener,
    wake: &OwnedFd,
    vm: &Arc<VmLink>,
    ip: [u8; 4],
    port: u16,
    nat: Option<SocketAddr>,
) {
    loop {
        let mut fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: polling two descriptors we own.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[1].revents != 0 {
            return; // the container stopped
        }
        while let Ok((client, from)) = listener.accept() {
            let Some(permit) = tcp_permit(from.ip()) else {
                continue;
            };
            let vm = vm.clone();
            let _ = std::thread::Builder::new()
                .name("kiri-conn".into())
                .spawn(move || {
                    // Held until the connection is done.
                    let _permit = permit;
                    let _ = relay(client, &vm, ip, port, nat);
                });
        }
    }
}

/// One client connection: a vsock stream to the agent, the target address,
/// then bytes both ways until both sides are done.
/// A vsock stream to the guest relay, already pointed at `ip:port`.
pub fn open_relay(vm: &VmLink, ip: [u8; 4], port: u16) -> io::Result<kiri_vmm::VsockStream> {
    let guest = vm.connect(RELAY_PORT)?;
    let mut header = [0u8; 6];
    header[..4].copy_from_slice(&ip);
    header[4..].copy_from_slice(&port.to_be_bytes());
    std::fs::File::from(guest.fd.try_clone()?).write_all(&header)?;
    Ok(guest)
}

/// `open_relay` on the agent's framed port, where it has one; true if so.
fn open_framed_relay(
    vm: &VmLink,
    ip: [u8; 4],
    port: u16,
) -> io::Result<(kiri_vmm::VsockStream, bool)> {
    if !NO_FRAMED_RELAY.load(Ordering::Relaxed) {
        match vm.connect(RELAY_FRAMED_PORT) {
            Ok(guest) => {
                let mut header = [0u8; 6];
                header[..4].copy_from_slice(&ip);
                header[4..].copy_from_slice(&port.to_be_bytes());
                std::fs::File::from(guest.fd.try_clone()?).write_all(&header)?;
                return Ok((guest, true));
            }
            // An older agent: nothing listens there.
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                NO_FRAMED_RELAY.store(true, Ordering::Relaxed)
            }
            Err(e) => return Err(e),
        }
    }
    Ok((open_relay(vm, ip, port)?, false))
}

fn relay(
    client: TcpStream,
    vm: &VmLink,
    ip: [u8; 4],
    port: u16,
    nat: Option<SocketAddr>,
) -> io::Result<()> {
    client.set_nonblocking(false)?;
    client.set_nodelay(true)?;
    client.set_read_timeout(Some(TCP_IDLE))?;
    client.set_write_timeout(Some(TCP_IDLE))?;
    // A Linux host reaches the container's address itself.
    if cfg!(target_os = "linux") {
        return relay_direct(client, ip, port);
    }
    if let Some(addr) = nat.filter(|_| nat_usable() && nat_budget()) {
        match TcpStream::connect_timeout(&addr, NAT_CONNECT) {
            Ok(guest) => {
                NAT_TIMEOUTS.store(0, Ordering::Relaxed);
                return relay_tcp(client, guest);
            }
            // Refused is the container (not listening yet), and vsock gets
            // the same answer; no route is the network itself, and so is no
            // answer twice running. This connection goes over vsock either way.
            Err(e) if e.kind() != io::ErrorKind::ConnectionRefused => {
                let timed_out = e.kind() == io::ErrorKind::TimedOut;
                if !timed_out || NAT_TIMEOUTS.fetch_add(1, Ordering::Relaxed) >= 1 {
                    NAT_TIMEOUTS.store(0, Ordering::Relaxed);
                    *NAT_DOWN_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Instant::now() + NAT_RETRY);
                    crate::diag::warn!(
                        "the VM's NAT network is unreachable; relaying published ports over vsock for now",
                        to = addr,
                        error = e
                    );
                }
            }
            Err(_) => {}
        }
    }
    let (guest, framed) = open_framed_relay(vm, ip, port)?;
    socket_timeouts(guest.fd.as_raw_fd())?;
    let mut to_guest = std::fs::File::from(guest.fd.try_clone()?);
    let mut from_guest = std::fs::File::from(guest.fd.try_clone()?);
    let mut client_read = client.try_clone()?;
    let mut client_write = client;
    let up = std::thread::Builder::new()
        .name("kiri-conn-up".into())
        .spawn(move || {
            let _ = copy(&mut client_read, &mut to_guest);
            // Pass the client's half-close on.
            // SAFETY: half-closing a socket we own.
            unsafe { libc::shutdown(to_guest.as_raw_fd(), libc::SHUT_WR) };
        })?;
    let _ = if framed {
        copy_frames(&mut from_guest, &mut client_write)
    } else {
        copy(&mut from_guest, &mut client_write)
    };
    let _ = client_write.shutdown(Shutdown::Write);
    let _ = up.join();
    // `guest` drops here: the vsock connection is released on the VM's queue.
    drop(guest);
    Ok(())
}

/// A client connection passed on over the NAT network: two sockets, with
/// each half-close passed on.
fn relay_tcp(client: TcpStream, guest: TcpStream) -> io::Result<()> {
    guest.set_nodelay(true)?;
    guest.set_read_timeout(Some(TCP_IDLE))?;
    guest.set_write_timeout(Some(TCP_IDLE))?;
    let (mut client_read, mut guest_write) = (client.try_clone()?, guest.try_clone()?);
    let up = std::thread::Builder::new()
        .name("kiri-conn-up".into())
        .spawn(move || {
            let _ = copy(&mut client_read, &mut guest_write);
            let _ = guest_write.shutdown(Shutdown::Write);
        })?;
    let (mut guest_read, mut client_write) = (guest, client);
    let _ = copy(&mut guest_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

/// How long a Linux host waits for a container to answer a connect: it is
/// on a local bridge, so it answers at once or refuses, unless it is busy.
const DIRECT_CONNECT: Duration = Duration::from_secs(2);
/// The pipe each spliced direction moves bytes through (Go's size, which
/// docker-proxy uses): fewer, larger system calls for a bulk transfer.
#[cfg(target_os = "linux")]
const SPLICE_PIPE: libc::c_int = 1 << 20;

/// Linux host: one client connection to the container's address, the bytes
/// moved by splice(2) through a pipe each way (never copied to user space),
/// one thread per direction, each half-close passed on.
fn relay_direct(client: TcpStream, ip: [u8; 4], port: u16) -> io::Result<()> {
    let target = TcpStream::connect_timeout(&SocketAddr::from((ip, port)), DIRECT_CONNECT)?;
    target.set_nodelay(true)?;
    target.set_read_timeout(Some(TCP_IDLE))?;
    target.set_write_timeout(Some(TCP_IDLE))?;
    let (client_read, target_write) = (client.try_clone()?, target.try_clone()?);
    let up = std::thread::Builder::new()
        .name("kiri-conn-up".into())
        .spawn(move || {
            let _ = splice_all(&client_read, &target_write);
            let _ = target_write.shutdown(Shutdown::Write);
        })?;
    let _ = splice_all(&target, &client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

/// Moves everything `from` sends to `to` until its end (only a Linux host
/// relays directly; splice(2) is Linux's).
#[cfg(not(target_os = "linux"))]
fn splice_all(mut from: &TcpStream, mut to: &TcpStream) -> io::Result<()> {
    copy(&mut from, &mut to)
}

/// Moves everything `from` sends to `to` until its end, through a pipe.
#[cfg(target_os = "linux")]
fn splice_all(from: &TcpStream, to: &TcpStream) -> io::Result<()> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 succeeded, so both are fresh and ours.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // A bigger pipe is an optimisation only; the default 64 KiB works too.
    // SAFETY: fcntl on a pipe we own.
    let size = match unsafe { libc::fcntl(w.as_raw_fd(), libc::F_SETPIPE_SZ, SPLICE_PIPE) } {
        n if n > 0 => n as usize,
        _ => 64 * 1024,
    };
    let splice = |from: i32, to: i32, len: usize| loop {
        // SAFETY: splice between descriptors we own, no offsets.
        let n = unsafe {
            libc::splice(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                len,
                libc::SPLICE_F_MOVE,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    loop {
        let n = splice(from.as_raw_fd(), w.as_raw_fd(), size)?;
        if n == 0 {
            return Ok(());
        }
        let mut left = n;
        while left > 0 {
            left -= splice(r.as_raw_fd(), to.as_raw_fd(), left)?;
        }
    }
}

fn copy(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n])?;
        if n == buf.len() && buf.len() < COPY_BUF_MAX {
            buf.resize(buf.len() * 2, 0);
        }
    }
}

/// What the agent sends on `RELAY_FRAMED_PORT`: `u32 len | bytes` frames,
/// an empty one last.
#[derive(Default)]
struct Deframer {
    head: [u8; 4],
    have: usize,
    left: usize,
}

impl Deframer {
    /// Passes the payload in `data` on to `to`; true once the end frame is in.
    fn feed(&mut self, mut data: &[u8], to: &mut impl Write) -> io::Result<bool> {
        while !data.is_empty() {
            if self.left > 0 {
                let n = self.left.min(data.len());
                to.write_all(&data[..n])?;
                self.left -= n;
                data = &data[n..];
                continue;
            }
            let n = (4 - self.have).min(data.len());
            self.head[self.have..self.have + n].copy_from_slice(&data[..n]);
            self.have += n;
            data = &data[n..];
            if self.have == 4 {
                self.have = 0;
                self.left = u32::from_be_bytes(self.head) as usize;
                if self.left == 0 {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// `copy` from a framed relay, up to its end frame (or the stream's end).
fn copy_frames(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buf = vec![0u8; COPY_BUF];
    let mut frames = Deframer::default();
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 || frames.feed(&buf[..n], to)? {
            return Ok(());
        }
        if n == buf.len() && buf.len() < COPY_BUF_MAX {
            buf.resize(buf.len() * 2, 0);
        }
    }
}

fn socket_timeouts(fd: std::os::fd::RawFd) -> io::Result<()> {
    let timeout = libc::timeval {
        tv_sec: TCP_IDLE.as_secs() as _,
        tv_usec: 0,
    };
    // SAFETY: timeout is valid for the duration of both calls on an owned socket.
    let result = unsafe {
        let len = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
        let read = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&timeout as *const libc::timeval).cast(),
            len,
        );
        let write = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            (&timeout as *const libc::timeval).cast(),
            len,
        );
        read | write
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// What containers reach as `host.docker.internal`.
pub const HOST_ADDR: &str = "198.19.248.254";
/// vsock port the guest dials for host-bound connections.
pub const HOST_BOUND_PORT: u32 = 1026;

/// One host-bound connection from a container: the guest sends
/// `ip[4] port[2]` (always 127.0.0.1 today), and we connect there on the Mac.
pub fn serve_host_bound(stream: kiri_vmm::VsockStream) {
    let _ = std::thread::Builder::new()
        .name("kiri-host-bound".into())
        .spawn(move || {
            let _ = host_bound(stream);
        });
}

fn host_bound(stream: kiri_vmm::VsockStream) -> io::Result<()> {
    let mut from_guest = std::fs::File::from(stream.fd.try_clone()?);
    let mut header = [0u8; 6];
    from_guest.read_exact(&mut header)?;
    let ip = std::net::Ipv4Addr::new(header[0], header[1], header[2], header[3]);
    let port = u16::from_be_bytes([header[4], header[5]]);
    // The guest's relay only ever asks for the Mac's loopback; anything else
    // is a guest process speaking the protocol itself, which gets nothing.
    if ip != std::net::Ipv4Addr::LOCALHOST {
        crate::diag::warn!(
            "refused a host-bound connection; only 127.0.0.1 is relayed",
            to = format!("{ip}:{port}")
        );
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not loopback",
        ));
    }
    // Refused or unreachable: dropping closes the container's connection,
    // which is what it would see from a real host too.
    let target = TcpStream::connect((ip, port))?;
    target.set_nodelay(true)?;
    let mut to_guest = std::fs::File::from(stream.fd.try_clone()?);
    let mut target_read = target.try_clone()?;
    let mut target_write = target;
    let down = std::thread::Builder::new()
        .name("kiri-host-bound-down".into())
        .spawn(move || {
            let _ = copy(&mut target_read, &mut to_guest);
            // SAFETY: half-closing a socket we own.
            unsafe { libc::shutdown(to_guest.as_raw_fd(), libc::SHUT_WR) };
        })?;
    let _ = copy(&mut from_guest, &mut target_write);
    let _ = target_write.shutdown(Shutdown::Write);
    let _ = down.join();
    drop(stream);
    Ok(())
}

/// vsock port the guest dials for SSH agent connections.
pub const SSH_AGENT_PORT: u32 = 1027;

/// The Mac's SSH agent socket: `$SSH_AUTH_SOCK`, or launchd's if the daemon
/// was not started from a shell that has it.
fn ssh_auth_sock() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("SSH_AUTH_SOCK") {
        return Some(p.into());
    }
    let out = std::process::Command::new("/bin/launchctl")
        .args(["getenv", "SSH_AUTH_SOCK"])
        .output()
        .ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!p.is_empty()).then(|| p.into())
}

/// One container connection to the SSH agent, relayed to the Mac's agent.
pub fn serve_ssh_agent(stream: kiri_vmm::VsockStream) {
    let _ = std::thread::Builder::new()
        .name("kiri-ssh-agent".into())
        .spawn(move || {
            let Some(path) = ssh_auth_sock() else { return };
            let Ok(agent) = std::os::unix::net::UnixStream::connect(path) else {
                return;
            };
            let (Ok(mut agent_read), Ok(from_guest), Ok(to_guest)) = (
                agent.try_clone(),
                stream.fd.try_clone(),
                stream.fd.try_clone(),
            ) else {
                return;
            };
            let mut from_guest = std::fs::File::from(from_guest);
            let mut to_guest = std::fs::File::from(to_guest);
            let mut agent_write = agent;
            let down = std::thread::spawn(move || {
                let _ = copy(&mut agent_read, &mut to_guest);
                // SAFETY: half-closing a socket we own.
                unsafe { libc::shutdown(to_guest.as_raw_fd(), libc::SHUT_WR) };
            });
            let _ = copy(&mut from_guest, &mut agent_write);
            let _ = agent_write.shutdown(Shutdown::Write);
            let _ = down.join();
            drop(stream);
        });
}

/// vsock port the guest dials for the engine's API: containers that mount
/// the docker socket (crates/kiri/src/daemon/binds.rs).
pub const ENGINE_API_PORT: u32 = 1036;

/// One container connection to the engine's API, relayed to this daemon's
/// own socket, so it is served like any other client.
pub fn serve_engine_api(stream: kiri_vmm::VsockStream) {
    let _ = std::thread::Builder::new()
        .name("kiri-engine-api".into())
        .spawn(move || {
            let Ok(api) = super::socket_path().and_then(std::os::unix::net::UnixStream::connect)
            else {
                return;
            };
            let (Ok(mut api_read), Ok(from_guest), Ok(to_guest)) = (
                api.try_clone(),
                stream.fd.try_clone(),
                stream.fd.try_clone(),
            ) else {
                return;
            };
            let mut from_guest = std::fs::File::from(from_guest);
            let mut to_guest = std::fs::File::from(to_guest);
            let mut api_write = api;
            let down = std::thread::spawn(move || {
                let _ = copy(&mut api_read, &mut to_guest);
                // SAFETY: half-closing a socket we own.
                unsafe { libc::shutdown(to_guest.as_raw_fd(), libc::SHUT_WR) };
            });
            let _ = copy(&mut from_guest, &mut api_write);
            let _ = api_write.shutdown(Shutdown::Write);
            let _ = down.join();
            drop(stream);
        });
}

/// The TCP ports in a container's `/proc/net/tcp` and `tcp6` lines that are
/// listening: those a published port reaches, then those it does not. The
/// relay dials the container's IPv4 address, so a socket bound to loopback
/// alone, or to one IPv6 address, is out of its reach; with host networking
/// it dials 127.0.0.1, so loopback is in reach there. Each port once, sorted.
pub fn listening<S: AsRef<str>>(lines: &[S], host_network: bool) -> (Vec<u16>, Vec<u16>) {
    use std::collections::BTreeSet;
    use std::net::{Ipv4Addr, Ipv6Addr};
    let v4_reach = |ip: Ipv4Addr| !ip.is_loopback() || host_network;
    let (mut reach, mut local) = (BTreeSet::new(), BTreeSet::new());
    for line in lines {
        let f: Vec<&str> = line.as_ref().split_whitespace().collect();
        // The fourth field is the state; 0A is LISTEN.
        if f.get(3) != Some(&"0A") {
            continue;
        }
        let Some((addr, port)) = f.get(1).and_then(|a| a.split_once(':')) else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(port, 16) else {
            continue;
        };
        // The kernel prints each 32-bit word of the address in the guest's
        // own byte order, which is little-endian on every guest we run.
        let words: Option<Vec<[u8; 4]>> = (0..addr.len() / 8)
            .map(|i| {
                addr.get(i * 8..i * 8 + 8)
                    .and_then(|w| u32::from_str_radix(w, 16).ok())
                    .map(u32::to_le_bytes)
            })
            .collect();
        let ok = match (addr.len(), words) {
            (8, Some(w)) => v4_reach(Ipv4Addr::from(w[0])),
            (32, Some(w)) => {
                let ip = Ipv6Addr::from(<[u8; 16]>::try_from(w.concat()).unwrap_or_default());
                ip.is_unspecified() || ip.to_ipv4_mapped().is_some_and(v4_reach)
            }
            _ => continue,
        };
        if port != 0 {
            if ok {
                reach.insert(port)
            } else {
                local.insert(port)
            };
        }
    }
    let local = local.difference(&reach).copied().collect();
    (reach.into_iter().collect(), local)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PEER_SESSIONS, MAX_REMOTE_SESSIONS, MAX_TCP_SESSIONS, NAT_PORTS, REMOTE_SESSIONS,
        TCP_SESSIONS, kernel_forwards, kernel_host_ip, listening, nat_port, relays_too, tcp_permit,
    };
    use std::net::IpAddr;
    use std::sync::atomic::Ordering;

    #[test]
    fn non_ipv4_bindings_never_become_wildcard() {
        assert_eq!(kernel_host_ip(""), Some([0; 4]));
        assert_eq!(kernel_host_ip("0.0.0.0"), Some([0; 4]));
        assert_eq!(kernel_host_ip("127.0.0.1"), Some([127, 0, 0, 1]));
        for ip in ["::", "::1", "[::1]", "2001:db8::1", "not-an-ip"] {
            assert_eq!(kernel_host_ip(ip), None, "{ip}");
        }
    }

    #[test]
    fn linux_hosts_relay_loopback_and_forward_the_rest() {
        // The kernel takes every IPv4 address but loopback.
        for ip in ["", "0.0.0.0", "192.168.1.5", "10.0.0.1"] {
            assert!(kernel_forwards(ip), "{ip}");
        }
        for ip in ["127.0.0.1", "127.0.0.2", "::1", "::", "not-an-ip"] {
            assert!(!kernel_forwards(ip), "{ip}");
        }
        // The relay takes loopback, IPv6, and every-address's 127.0.0.1 share.
        for ip in ["", "0.0.0.0", "127.0.0.1", "::1", "::"] {
            assert!(relays_too(ip), "{ip}");
        }
        assert!(!relays_too("192.168.1.5"));
    }

    const TCP: [&str; 5] = [
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode",
        "   0: 00000000:11D7 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4101 1 0000000000000000 100 0 0 10 0",
        "   1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4102 1 0000000000000000 100 0 0 10 0",
        "   2: 0200A8C0:11D7 0100A8C0:C350 01 00000000:00000000 00:00000000 00000000     0        0 4103 1 0000000000000000 20 4 30 10 -1",
        "   3: 0200A8C0:01BB 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4104 1 0000000000000000 100 0 0 10 0",
    ];
    const TCP6: [&str; 5] = [
        "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode",
        "   0: 00000000000000000000000000000000:11D7 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4201 1 0000000000000000 100 0 0 10 0",
        "   1: 00000000000000000000000001000000:0CEA 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4202 1 0000000000000000 100 0 0 10 0",
        "   2: 0000000000000000FFFF00000100007F:2382 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4203 1 0000000000000000 100 0 0 10 0",
        "   3: 0000000000000000FFFF00000200A8C0:0050 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4204 1 0000000000000000 100 0 0 10 0",
    ];

    #[test]
    fn ipv4_listeners_only_and_loopback_apart() {
        // 4567 on every address, 443 on the container's own; 8080 on loopback
        // only; the established 4567 connection and the header are skipped.
        assert_eq!(listening(&TCP, false), (vec![443, 4567], vec![8080]));
        assert_eq!(listening(&TCP, true), (vec![443, 4567, 8080], vec![]));
    }

    #[test]
    fn ipv6_listeners() {
        // [::]:4567 and ::ffff:192.168.0.2:80 are reachable over IPv4; [::1]:3306
        // and ::ffff:127.0.0.1:9090 are loopback.
        assert_eq!(listening(&TCP6, false), (vec![80, 4567], vec![3306, 9090]));
        // The relay's 127.0.0.1 reaches a mapped loopback socket, never ::1.
        assert_eq!(listening(&TCP6, true), (vec![80, 4567, 9090], vec![3306]));
    }

    #[test]
    fn a_port_on_both_families_counts_once() {
        let both: Vec<&str> = TCP.iter().chain(TCP6.iter()).copied().collect();
        assert_eq!(
            listening(&both, false),
            (vec![80, 443, 4567], vec![3306, 8080, 9090])
        );
        // Bound to loopback and to every address: reachable, not listed twice.
        let lines = [
            TCP[1],
            TCP[3]
                .replace("0200A8C0:11D7", "0100007F:11D7")
                .replace(" 01 ", " 0A ")
                .as_str(),
        ]
        .map(str::to_owned);
        assert_eq!(listening(&lines, false), (vec![4567], vec![]));
        assert_eq!(
            listening(&["garbage", "1: zz:11D7 0:0 0A", ""], false),
            (vec![], vec![])
        );
    }

    #[test]
    fn tcp_permits_are_bounded_and_released() {
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let held: Vec<_> = (0..MAX_TCP_SESSIONS)
            .map(|_| tcp_permit(local).expect("permit below limit"))
            .collect();
        assert!(tcp_permit(local).is_none());
        drop(held);
        assert_eq!(TCP_SESSIONS.load(Ordering::Relaxed), 0);
        assert!(tcp_permit(local).is_some());
        assert_eq!(TCP_SESSIONS.load(Ordering::Relaxed), 0);

        // One LAN host: its own share only, and the Mac still gets in.
        let lan: IpAddr = "192.168.2.50".parse().unwrap();
        let flood: Vec<_> = (0..MAX_PEER_SESSIONS)
            .map(|_| tcp_permit(lan).expect("below the peer limit"))
            .collect();
        assert!(tcp_permit(lan).is_none());
        assert!(
            tcp_permit("::ffff:192.168.2.50".parse().unwrap()).is_none(),
            "a v4-mapped address is the same peer"
        );
        assert!(tcp_permit("192.168.2.51".parse().unwrap()).is_some());
        assert!(tcp_permit(local).is_some());
        assert!(tcp_permit("::1".parse().unwrap()).is_some());
        // Many LAN hosts: half the sessions at most, the rest stay for the Mac.
        let many: Vec<_> = (0..MAX_REMOTE_SESSIONS)
            .filter_map(|i| tcp_permit(IpAddr::from([10, 0, (i / 250) as u8, (i % 250) as u8 + 1])))
            .collect();
        assert_eq!(flood.len() + many.len(), MAX_REMOTE_SESSIONS);
        assert!(tcp_permit("10.9.9.9".parse().unwrap()).is_none());
        let mac: Vec<_> = (0..MAX_TCP_SESSIONS - MAX_REMOTE_SESSIONS)
            .map(|_| tcp_permit(local).expect("the Mac's share"))
            .collect();
        assert!(tcp_permit(local).is_none());
        drop((flood, many, mac));
        assert_eq!(TCP_SESSIONS.load(Ordering::Relaxed), 0);
        assert!(REMOTE_SESSIONS.lock().unwrap().is_empty());
    }

    #[test]
    fn the_nat_budget_is_a_burst_then_a_rate() {
        use std::time::{Duration, Instant};
        // How many connections get through at `at`, and the state after.
        let drain = |mut state: Option<(f64, Instant)>, at: Instant| {
            let mut n = 0;
            while let (next, true) = super::take_budget(state, at) {
                state = Some(next);
                n += 1;
            }
            (n, state.unwrap_or_else(|| super::take_budget(state, at).0))
        };
        let t0 = Instant::now();
        let (took, state) = drain(None, t0);
        assert_eq!(took, super::NAT_BURST as usize);
        assert!(!super::take_budget(Some(state), t0).1);
        // A second later, `NAT_RATE` more; never more than the burst.
        let later = |secs| t0 + Duration::from_secs(secs);
        assert_eq!(drain(Some(state), later(1)).0, super::NAT_RATE as usize);
        assert_eq!(
            super::take_budget(None, later(3600)).0.0,
            super::NAT_BURST - 1.0
        );
        assert_eq!(
            super::take_budget(Some(state), later(3600)).0.0,
            super::NAT_BURST - 1.0
        );
    }

    #[test]
    fn frames_pass_their_payload_and_stop_at_the_empty_one() {
        let mut stream = Vec::new();
        for part in [&b"HTTP/1.0 200 OK\r\n"[..], b"", b"after the end"] {
            stream.extend_from_slice(&(part.len() as u32).to_be_bytes());
            stream.extend_from_slice(part);
        }
        // Whole, and cut at every byte: frames and lengths split across reads.
        for cut in 0..stream.len() {
            let (mut d, mut out) = (super::Deframer::default(), Vec::new());
            let ended = d.feed(&stream[..cut], &mut out).unwrap()
                || d.feed(&stream[cut..], &mut out).unwrap();
            assert!(ended, "cut at {cut}");
            assert_eq!(out, b"HTTP/1.0 200 OK\r\n", "cut at {cut}");
        }
        let mut out = Vec::new();
        super::copy_frames(&mut &stream[..], &mut out).unwrap();
        assert_eq!(out, b"HTTP/1.0 200 OK\r\n");
        // A stream that stops without its end frame ends the copy too.
        let mut out = Vec::new();
        super::copy_frames(&mut &stream[..10], &mut out).unwrap();
        assert_eq!(out, b"HTTP/1");
    }

    #[test]
    fn nat_ports_are_the_lowest_free_and_released() {
        let a = nat_port().expect("free port");
        let b = nat_port().expect("free port");
        assert!(NAT_PORTS.contains(&a.0) && NAT_PORTS.contains(&b.0));
        assert_ne!(a.0, b.0);
        // Above Linux's ephemeral range, so never a guest connection's own.
        assert!(*NAT_PORTS.start() > 60999);
        let freed = a.0;
        drop(a);
        let c = nat_port().expect("free port");
        assert_eq!(c.0, freed);
    }
}
