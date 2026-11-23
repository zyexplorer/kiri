//! The embedded DNS server for user-defined networks, as Docker has: at
//! 127.0.0.11:53 inside each container's network namespace (what its
//! resolv.conf names, as with Docker; the socket is on a high port there,
//! with port 53 redirected to it, so the container can bind port 53
//! itself: see `embedded_socket`), and on each bridge's gateway address,
//! port 53. Container names and aliases on the container's networks resolve
//! to their addresses (A, and AAAA on an IPv6 network); everything else is
//! forwarded to the VM's upstream resolver (or the container's own `--dns`
//! servers) and the answer relayed back. The 127.0.0.11 socket is opened in
//! the container's namespace by this agent, so no process runs in there.
//!
//! In the VM the upstream is the Mac's resolver for the NAT network, at its
//! gateway (192.168.64.1). macOS doesn't always answer there: after a
//! network change, sleep or a VPN, lookups are refused or go unanswered
//! while the Mac itself resolves fine. So a lookup the gateway refuses, or
//! leaves unanswered for `FALLBACK_AFTER`, is asked again through the Kiri
//! daemon on the Mac, over vsock, which asks the Mac's own resolvers
//! (crates/kiri/src/daemon/dns.rs). A container's own `--dns` servers are
//! tried in the order given, as Docker's embedded DNS does (at most
//! `MAX_OWN_SERVERS`, each given `OWN_WAIT`): the next one when one refuses
//! (nothing listens, the network or host is unreachable, or it answers
//! REFUSED or SERVFAIL), can't be sent to, or leaves the query unanswered.
//! When none is left, or `OWN_DEADLINE` has passed, the client is told
//! SERVFAIL at once. They are never second-guessed by the VM's resolver or
//! the host.

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use super::sys;

const TTL: u32 = 600;
/// Names every container's /etc/hosts has for the host. They're answered here
/// too, as Docker Desktop's DNS does, for what reads DNS rather than
/// /etc/hosts: a kind or k3s node's CoreDNS forwards its pods' lookups here.
/// Whether `n` can be a name in the network's DNS: non-empty, at most 253
/// bytes, labels of 1 to 63 of letters, digits, `-` and `_` joined by dots
/// (no leading, trailing or doubled dot, so never the root).
fn dns_name(n: &str) -> bool {
    n.len() <= 253 && n.split('.').all(|l| (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

const HOST_NAMES: &[&str] = &["host.docker.internal", "host.kiri.internal", "gateway.docker.internal"];
/// Forwarded queries not answered within this are dropped.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);
/// At most this many forwarded queries in flight (each has its own socket);
/// more are dropped, and the client asks again.
const MAX_PENDING: usize = 256;
/// At most this many of them from one container (or one address on a
/// bridge), so a container flooding lookups can't take DNS from the rest.
const MAX_PER_CLIENT: usize = 64;
/// How long the agent's loop may wait for the host to take a relayed
/// lookup's connection.
const RELAY_CONNECT: Duration = Duration::from_millis(250);
/// After the host failed to take one, lookups aren't relayed for this long:
/// each would stall the loop for `RELAY_CONNECT` again.
const RELAY_BACKOFF: Duration = Duration::from_secs(5);
/// A lookup the VM's resolver hasn't answered within this goes through the
/// host (VM only). Gateway answers take milliseconds; a slow recursive one
/// that loses the race is simply replaced by the host's. (A container's own
/// `--dns` server has `OWN_WAIT` before its next one is asked.)
const FALLBACK_AFTER: Duration = Duration::from_secs(1);
/// Docker's limit: it uses the first three `--dns` servers and no more.
pub const MAX_OWN_SERVERS: usize = 3;
/// How long one of a container's own `--dns` servers has to answer before the
/// next is asked; Docker's wait for an external server.
const OWN_WAIT: Duration = Duration::from_secs(4);
/// The most a query to a container's own servers lives: each server's wait.
const OWN_DEADLINE: Duration = Duration::from_secs(4 * MAX_OWN_SERVERS as u64);
/// Queries with its own `--dns` servers one container may have in flight:
/// they can live `OWN_DEADLINE`, so they get a smaller share of the slots
/// than `MAX_PER_CLIENT`. One more is answered SERVFAIL at once.
const MAX_OWN_PER_CLIENT: usize = 16;
const _: () = assert!(MAX_OWN_PER_CLIENT < MAX_PER_CLIENT);
/// Largest reply the host relay may send (a DNS over TCP frame's limit).
const MAX_RELAYED: usize = 2 + 65_535;

/// Docker's embedded DNS address, inside each container.
pub const EMBEDDED: [u8; 4] = [127, 0, 0, 11];

struct Server {
    /// Its own number, never reused: what a forwarded query names it by.
    id: u64,
    /// The networks whose names it answers: a gateway's one, or every
    /// network of the container it is in.
    bridges: Vec<String>,
    sock: OwnedFd,
    /// In a container's namespace: (its PID, the address its `--dns`
    /// servers are kept under). Every query there comes from 127.0.0.1.
    container: Option<(i32, [u8; 4])>,
}

struct Pending {
    /// Its own socket, connected to the resolver: a fresh source port for
    /// every query, as Docker's embedded DNS does, so one lost or blocked
    /// flow doesn't take the resolver away from every later query, and an
    /// answer has to guess the port as well as the ID. Once the query goes
    /// through the host, the vsock stream to it instead.
    sock: OwnedFd,
    /// The ID it went out with.
    our: u16,
    /// Who asked, for `MAX_PER_CLIENT`: the container's address.
    key: [u8; 4],
    /// The server the query came in on (`Server::id`).
    server: u64,
    client: libc::sockaddr_in,
    id: u16,
    question: Vec<u8>,
    /// When it was asked of the server it is with now.
    sent: Instant,
    /// When the query came in.
    started: Instant,
    /// The query as sent (with `our` ID), to send again.
    query: Vec<u8>,
    /// With the container's own `--dns` servers (kept under `key`): the
    /// position of the one it is with. None for the VM's resolver.
    own: Option<usize>,
    /// It may still go through the host: in the VM, asked of the VM's own
    /// resolver (not `--dns` servers), and not tried there yet.
    may_relay: bool,
    /// Sent through the host: the reply's bytes so far, `u16 len | reply`.
    relayed: Option<Vec<u8>>,
}

/// One name's IPv6 addresses, each with its container's IPv4 address.
type Owned6 = Vec<([u8; 4], [u8; 16])>;

#[derive(Default)]
pub struct Dns {
    servers: Vec<Server>,
    /// bridge → name (lowercase) → addresses.
    names: HashMap<String, HashMap<String, Vec<[u8; 4]>>>,
    /// bridge → name → (the container's IPv4 address there, its IPv6 one):
    /// the IPv4 address is what `unregister` names the container by.
    names6: HashMap<String, HashMap<String, Owned6>>,
    /// bridge → its Docker network's name: the order a container's server
    /// searches its networks in.
    networks: HashMap<String, String>,
    /// Container address → its `--dns` servers, used instead of the VM's.
    clients: HashMap<[u8; 4], Vec<[u8; 4]>>,
    pending: Vec<Pending>,
    /// Lookups the VM's resolver fails go through the host (in the VM; a
    /// Linux host's agent asks the host's resolvers itself).
    pub relay: bool,
    /// The host didn't take a relayed lookup: none are tried until then.
    relay_down_until: Option<Instant>,
    /// Bridges whose gateway wouldn't take the listener (`serve_or_warn`):
    /// the warning is said once for each.
    refused: std::collections::HashSet<String>,
}

/// A fresh `Server::id`.
fn server_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn udp_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket creation; owned immediately.
    let fd = sys::check(
        unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) } as _,
        "dns socket",
    )?;
    // SAFETY: `fd` is fresh and ours.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn sockaddr(ip: [u8; 4], port: u16) -> libc::sockaddr_in {
    // SAFETY: sockaddr_in is plain data; all-zero is valid.
    let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    a.sin_family = libc::AF_INET as libc::sa_family_t;
    a.sin_port = port.to_be();
    a.sin_addr.s_addr = u32::from_ne_bytes(ip);
    a
}

fn send_to(fd: RawFd, data: &[u8], to: &libc::sockaddr_in) {
    // SAFETY: sending a buffer we own to a valid sockaddr_in.
    unsafe {
        libc::sendto(
            fd,
            data.as_ptr().cast(),
            data.len(),
            0,
            (to as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as u32,
        )
    };
}

impl Dns {
    /// Starts answering on a bridge's gateway address (once per bridge).
    /// A bridge made again with another gateway (its network's subnet moved
    /// off one the host took, #489) gets a listener there in place of the
    /// old one, which no address of the bridge's reaches any more.
    pub fn serve(&mut self, bridge: &str, gateway: [u8; 4]) -> io::Result<()> {
        let at_gateway = |s: &Server| s.container.is_none() && s.bridges.iter().any(|b| b == bridge);
        if let Some(s) = self.servers.iter().find(|s| at_gateway(s)) {
            if local_addr(&s.sock) == Some(gateway) {
                return Ok(());
            }
            self.servers.retain(|s| !at_gateway(s));
            self.forget_pending();
        }
        let sock = bound(gateway, 53)?;
        self.servers.push(Server { id: server_id(), bridges: vec![bridge.to_owned()], sock, container: None });
        Ok(())
    }

    /// `serve`, for a container start that can go on without it: false
    /// when the gateway's listener can't be made, said once per bridge.
    /// The container then asks the resolver in its own namespace, at
    /// 127.0.0.11 (container.rs `settle_resolver`). A WSL 2 host under
    /// `networkingMode=Consomme` answers EINVAL to every fixed-port bind on
    /// a bridge address in its own namespace (#477), and a gateway can be
    /// held by another program; binds in a container's namespace are not
    /// affected.
    pub fn serve_or_warn(&mut self, bridge: &str, gateway: [u8; 4]) -> bool {
        match self.serve(bridge, gateway) {
            Ok(()) => true,
            Err(e) => {
                if self.refused.insert(bridge.to_owned()) {
                    eprintln!(
                        "kiri-init: dns: cannot listen on {bridge}'s gateway {}:53 ({e}); its containers ask Kiri's resolver at 127.0.0.11 in their own network instead",
                        super::net::ip_str(gateway)
                    );
                }
                false
            }
        }
    }

    /// Starts answering at 127.0.0.11:53 in container `pid`'s network
    /// namespace `netns` (lo up) for the names on `bridge`, or adds `bridge`
    /// to the networks its server answers for. `owner` is the address its
    /// `--dns` servers are kept under (`set_upstream`).
    pub fn serve_in(&mut self, netns: &std::fs::File, pid: i32, owner: [u8; 4], bridge: &str) -> io::Result<()> {
        if let Some(s) = self.servers.iter_mut().find(|s| s.container.is_some_and(|(p, _)| p == pid)) {
            if !s.bridges.iter().any(|b| b == bridge) {
                s.bridges.push(bridge.to_owned());
            }
            return Ok(());
        }
        let sock = super::netlink::in_netns(netns, embedded_socket)?;
        self.servers.push(Server { id: server_id(), bridges: vec![bridge.to_owned()], sock, container: Some((pid, owner)) });
        Ok(())
    }

    /// Container `pid` left `bridge` (`network disconnect`).
    pub fn leave(&mut self, pid: i32, bridge: &str) {
        for s in self.servers.iter_mut().filter(|s| s.container.is_some_and(|(p, _)| p == pid)) {
            s.bridges.retain(|b| b != bridge);
        }
    }

    /// The address container `pid`'s 127.0.0.11 server keeps its `--dns`
    /// servers under (`set_upstream`), if it has one.
    pub fn owner_of(&self, pid: i32) -> Option<[u8; 4]> {
        self.servers.iter().find_map(|s| s.container.filter(|(p, _)| *p == pid).map(|(_, owner)| owner))
    }

    /// Whether the agent answers DNS at `ip` on a bridge (its gateway).
    pub fn serves_at(&self, ip: [u8; 4]) -> bool {
        self.servers.iter().filter(|s| s.container.is_none()).any(|s| local_addr(&s.sock) == Some(ip))
    }

    /// Container `pid` is gone: its 127.0.0.11 server goes.
    pub fn drop_container(&mut self, pid: i32) {
        self.servers.retain(|s| !s.container.is_some_and(|(p, _)| p == pid));
        self.forget_pending();
    }

    /// Forwarded queries whose server is gone go too.
    fn forget_pending(&mut self) {
        let servers = &self.servers;
        self.pending.retain(|p| servers.iter().any(|s| s.id == p.server));
    }

    pub fn register(&mut self, bridge: &str, names: impl Iterator<Item = impl AsRef<str>>, ip: [u8; 4]) {
        let table = self.names.entry(bridge.to_owned()).or_default();
        for n in names {
            let list = table.entry(n.as_ref().to_ascii_lowercase()).or_default();
            if !list.contains(&ip) {
                list.push(ip);
            }
        }
    }

    /// The IPv6 address of the container that has `owner` on this network.
    pub fn register6(&mut self, bridge: &str, names: impl Iterator<Item = impl AsRef<str>>, owner: [u8; 4], ip: [u8; 16]) {
        let table = self.names6.entry(bridge.to_owned()).or_default();
        for n in names {
            let list = table.entry(n.as_ref().to_ascii_lowercase()).or_default();
            if !list.contains(&(owner, ip)) {
                list.push((owner, ip));
            }
        }
    }

    /// `bridge` is Docker network `name` (`net_opts::NETWORK`).
    pub fn name_network(&mut self, bridge: &str, name: &str) {
        if self.networks.get(bridge).is_none_or(|n| n != name) {
            self.networks.insert(bridge.to_owned(), name.to_owned());
        }
    }

    /// A removed network's bridge: its server and names go.
    pub fn forget_bridge(&mut self, bridge: &str) {
        self.servers.retain(|s| s.container.is_some() || !s.bridges.iter().any(|b| b == bridge));
        for s in &mut self.servers {
            s.bridges.retain(|b| b != bridge);
        }
        self.names.remove(bridge);
        self.names6.remove(bridge);
        self.networks.remove(bridge);
        self.refused.remove(bridge);
        self.forget_pending();
    }

    /// Where queries from `client` that no container name answers go;
    /// empty means the VM's resolvers.
    pub fn set_upstream(&mut self, client: [u8; 4], mut servers: Vec<[u8; 4]>) {
        servers.truncate(MAX_OWN_SERVERS);
        if servers.is_empty() {
            self.clients.remove(&client);
        } else {
            self.clients.insert(client, servers);
        }
    }

    pub fn unregister(&mut self, bridge: &str, ip: [u8; 4]) {
        self.clients.remove(&ip);
        // Its queries in flight go with it: a container given the address
        // next never gets them, or their answers.
        self.pending.retain(|p| p.key != ip);
        self.drop_names(bridge, ip);
    }

    /// `docker rename`: the names the container at `ip` answers to on
    /// `bridge` become `names`, those that are DNS names (see `dns_name`).
    /// Unlike `unregister`, its own `--dns` servers (kept under its address)
    /// stay. `ip` keys the container whether or not the network has IPv4
    /// addresses: `v4` false (an IPv6-only network) registers no A record.
    pub fn rename(&mut self, bridge: &str, ip: [u8; 4], ip6: Option<[u8; 16]>, v4: bool, names: &[impl AsRef<str>]) {
        self.drop_names(bridge, ip);
        let names = || names.iter().filter(|n| dns_name(n.as_ref()));
        if v4 {
            self.register(bridge, names(), ip);
        }
        if let Some(v6) = ip6 {
            self.register6(bridge, names(), ip, v6);
        }
    }

    /// Every name the container at `ip` has on `bridge`, A and AAAA.
    fn drop_names(&mut self, bridge: &str, ip: [u8; 4]) {
        if let Some(table) = self.names.get_mut(bridge) {
            for list in table.values_mut() {
                list.retain(|a| *a != ip);
            }
            table.retain(|_, list| !list.is_empty());
        }
        if let Some(table) = self.names6.get_mut(bridge) {
            for list in table.values_mut() {
                list.retain(|(owner, _)| *owner != ip);
            }
            table.retain(|_, list| !list.is_empty());
        }
    }

    /// Descriptors to poll: each server's, plus each forwarded query's.
    pub fn fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.servers.iter().map(|s| s.sock.as_raw_fd()).chain(self.pending.iter().map(|p| p.sock.as_raw_fd()))
    }

    pub fn owns(&self, fd: RawFd) -> bool {
        self.fds().any(|f| f == fd)
    }

    pub fn on_readable(&mut self, fd: RawFd, upstream: &[[u8; 4]]) {
        let mut buf = [0u8; 1500];
        if let Some(at) = self.pending.iter().position(|p| p.sock.as_raw_fd() == fd) {
            if self.pending[at].relayed.is_some() {
                self.read_relayed(at);
                return;
            }
            // A forwarded answer: hand it back to whoever asked.
            loop {
                // SAFETY: receiving into a buffer we own.
                let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
                if n < 0 && io::Error::last_os_error().raw_os_error().is_some_and(is_unreachable) {
                    // Nothing listens, or there is no way there: the next
                    // server, or the host if this query may go through it.
                    self.hand_over(at);
                    break;
                }
                if n < 12 {
                    break;
                }
                let msg = &mut buf[..n as usize];
                let p = &self.pending[at];
                if u16::from_be_bytes([msg[0], msg[1]]) == p.our && response_matches(msg, p) {
                    // REFUSED or SERVFAIL from one of its own servers: the
                    // next one, as with Docker. From the last, it goes back.
                    if matches!(msg[3] & 0x0f, 2 | 5) && self.pending[at].own.is_some() && self.ask_next_own(at) {
                        break;
                    }
                    self.answered(at, msg);
                    break;
                }
            }
            return;
        }
        let Some(server) = self.servers.iter().position(|s| s.sock.as_raw_fd() == fd) else {
            return;
        };
        loop {
            // SAFETY: sockaddr_in is plain data; recvfrom fills it in.
            let mut from: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_in>() as u32;
            // SAFETY: receiving into buffers we own.
            let n = unsafe { libc::recvfrom(fd, buf.as_mut_ptr().cast(), buf.len(), 0, (&mut from as *mut libc::sockaddr_in).cast(), &mut len) };
            if n < 12 {
                break;
            }
            let query = &buf[..n as usize];
            match self.answer(server, query) {
                Some(reply) => send_to(fd, &reply, &from),
                None => self.forward(server, query, from, upstream),
            }
        }
    }

    /// Answers a query for a name on this network, or None to forward it.
    fn answer(&self, server: usize, q: &[u8]) -> Option<Vec<u8>> {
        let (name, qtype, end) = parse_question(q)?;
        let host = vec![super::net::HOST_ADDR];
        // Of the server's networks that have the name, the first by Docker
        // network name answers, as with Docker: not the first joined. A
        // name Compose gives a service on every network it joins then
        // answers from the project's own network rather than a shared one
        // where another project's service has it too (#294).
        // The host's names are the host's whatever a container is called:
        // a name registered for one of them is never answered.
        let is_host = HOST_NAMES.contains(&name.as_str());
        let bridge = self.servers[server]
            .bridges
            .iter()
            .filter(|b| !is_host && (self.names.get(*b).is_some_and(|t| t.contains_key(&name)) || self.names6.get(*b).is_some_and(|t| t.contains_key(&name))))
            .min_by_key(|b| self.networks.get(*b).map_or(b.as_str(), String::as_str));
        let addrs = bridge.and_then(|b| self.names.get(b)).and_then(|t| t.get(&name));
        let addrs6 = bridge.and_then(|b| self.names6.get(b)).and_then(|t| t.get(&name));
        // The host's names answer A with the host's address, and AAAA with
        // no records: the host relay is IPv4 only.
        let addrs = match addrs {
            None if is_host => Some(&host),
            a => a,
        };
        // A container name with no address of the asked type is NODATA, not
        // forwarded: the name is this network's.
        if addrs.is_none() && addrs6.is_none() {
            return None;
        }
        let a: &[[u8; 4]] = if qtype == 1 || qtype == 255 { addrs.map_or(&[], Vec::as_slice) } else { &[] };
        let aaaa: &[([u8; 4], [u8; 16])] = if qtype == 28 || qtype == 255 { addrs6.map_or(&[], Vec::as_slice) } else { &[] };
        let mut r = Vec::with_capacity(end + 16 * a.len() + 28 * aaaa.len());
        r.extend_from_slice(&q[0..2]); // id
        r.extend_from_slice(&[0x81, 0x80]); // response, recursion desired + available
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&((a.len() + aaaa.len()) as u16).to_be_bytes());
        r.extend_from_slice(&[0, 0, 0, 0]);
        r.extend_from_slice(&q[12..end]);
        for ip in a {
            r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1]);
            r.extend_from_slice(&TTL.to_be_bytes());
            r.extend_from_slice(&[0, 4]);
            r.extend_from_slice(ip);
        }
        for (_, ip) in aaaa {
            r.extend_from_slice(&[0xc0, 0x0c, 0, 28, 0, 1]);
            r.extend_from_slice(&TTL.to_be_bytes());
            r.extend_from_slice(&[0, 16]);
            r.extend_from_slice(ip);
        }
        Some(r)
    }

    fn forward(&mut self, server: usize, q: &[u8], client: libc::sockaddr_in, upstream: &[[u8; 4]]) {
        let now = Instant::now();
        // A query with the container's own servers past its deadline is
        // answered SERVFAIL here too, before the slots are counted.
        for at in (0..self.pending.len()).rev() {
            if self.pending[at].own.is_some() && now.duration_since(self.pending[at].started) >= OWN_DEADLINE {
                self.fail(at);
            }
        }
        self.pending.retain(|p| p.own.is_some() || now.duration_since(p.sent) < FORWARD_TIMEOUT);
        let key = self.servers[server].container.map_or(client.sin_addr.s_addr.to_ne_bytes(), |(_, owner)| owner);
        let own = self.clients.get(&key).map(|v| (v.len(), v.first().copied()));
        let Some((_, _, end)) = parse_question(q) else {
            return;
        };
        let (mut mine, mut mine_own) = (0, 0);
        for p in self.pending.iter().filter(|p| p.key == key) {
            mine += 1;
            mine_own += usize::from(p.own.is_some());
        }
        if self.pending.len() >= MAX_PENDING || mine >= MAX_PER_CLIENT || (own.is_some() && mine_own >= MAX_OWN_PER_CLIENT) {
            // With its own servers the client is told at once (they may
            // hold a slot for `OWN_DEADLINE`); else it asks again.
            if own.is_some() {
                send_to(self.servers[server].sock.as_raw_fd(), &servfail(id_of(q), rd(q), &q[12..end]), &client);
            }
            return;
        }
        let Some(our) = random_id() else { return };
        let mut msg = q.to_vec();
        let id = u16::from_be_bytes([msg[0], msg[1]]);
        msg[0..2].copy_from_slice(&our.to_be_bytes());
        let may_relay = self.relay && own.is_none();
        let mut at = None;
        let sock = match own {
            // The first of its own servers that can be sent to.
            Some((len, _)) => (0..len).find_map(|i| {
                let sock = self.clients.get(&key).and_then(|v| v.get(i)).and_then(|d| ask(*d, &msg))?;
                at = Some(i);
                Some(sock)
            }),
            None => upstream.first().and_then(|d| ask(*d, &msg)),
        };
        // No resolver yet, or it can't be reached: the host, if allowed.
        let (sock, relayed) = match sock {
            Some(s) => (s, false),
            None if may_relay => match self.relay_stream(&msg) {
                Some(s) => (s, true),
                None => return,
            },
            // None of its own can be sent to: SERVFAIL, not silence.
            None if own.is_some() => {
                if let Some(s) = self.servers.get(server) {
                    send_to(s.sock.as_raw_fd(), &servfail(id_of(q), rd(q), &q[12..end]), &client);
                }
                return;
            }
            None => return,
        };
        let p = Pending {
            sock,
            our,
            key,
            server: self.servers[server].id,
            client,
            id,
            question: q[12..end].to_vec(),
            sent: now,
            started: now,
            query: msg,
            own: at,
            may_relay: may_relay && !relayed,
            relayed: relayed.then(Vec::new),
        };
        self.pending.push(p);
    }

    /// Hands a matching answer to whoever asked, and forgets the query.
    fn answered(&mut self, at: usize, msg: &mut [u8]) {
        let p = self.pending.swap_remove(at);
        msg[0..2].copy_from_slice(&p.id.to_be_bytes());
        if let Some(s) = self.servers.iter().find(|s| s.id == p.server) {
            send_to(s.sock.as_raw_fd(), msg, &p.client);
        }
    }

    /// Sends pending query `at` to the next of the container's own servers
    /// that it can be sent to, on a fresh socket (a fresh source port) and
    /// with a fresh ID, so a server can't know what the one before it was
    /// sent. False, with the query untouched, when none is left (or the
    /// container's list is gone) or `OWN_DEADLINE` has passed.
    fn ask_next_own(&mut self, at: usize) -> bool {
        let p = &self.pending[at];
        let Some(mut i) = p.own else { return false };
        let key = p.key;
        if p.started.elapsed() >= OWN_DEADLINE || self.clients.get(&key).is_none_or(|v| i + 1 >= v.len()) {
            return false;
        }
        let Some(our) = random_id() else { return false };
        let was = [p.query[0], p.query[1]];
        self.pending[at].query[0..2].copy_from_slice(&our.to_be_bytes());
        loop {
            i += 1;
            let Some(dest) = self.clients.get(&key).and_then(|v| v.get(i)).copied() else { break };
            if let Some(sock) = ask(dest, &self.pending[at].query) {
                let p = &mut self.pending[at];
                p.sock = sock;
                p.our = our;
                p.own = Some(i);
                p.sent = Instant::now();
                return true;
            }
        }
        self.pending[at].query[0..2].copy_from_slice(&was);
        false
    }

    /// The server a query is with can't be reached or refused: the next of
    /// the container's own, else the host if the query may go through it,
    /// else (its own servers all failed) SERVFAIL to the client.
    fn hand_over(&mut self, at: usize) {
        if self.ask_next_own(at) {
            return;
        }
        if self.pending[at].own.is_some() {
            self.fail(at);
        } else {
            self.relay(at);
        }
    }

    /// Answers pending query `at` SERVFAIL and forgets it (closing its socket).
    fn fail(&mut self, at: usize) {
        let p = self.pending.swap_remove(at);
        if let Some(s) = self.servers.iter().find(|s| s.id == p.server) {
            let reply = servfail(p.id, rd(&p.query), &p.question);
            send_to(s.sock.as_raw_fd(), &reply, &p.client);
        }
    }

    /// Sends pending query `at` again, through the host, once.
    fn relay(&mut self, at: usize) {
        if !self.pending[at].may_relay {
            return;
        }
        self.pending[at].may_relay = false;
        let query = std::mem::take(&mut self.pending[at].query);
        let stream = self.relay_stream(&query);
        let p = &mut self.pending[at];
        p.query = query;
        if let Some(stream) = stream {
            p.sock = stream;
            p.relayed = Some(Vec::new());
            p.sent = Instant::now();
        }
    }

    /// Reads the host's reply to query `at` as it arrives.
    fn read_relayed(&mut self, at: usize) {
        let fd = self.pending[at].sock.as_raw_fd();
        let mut buf = [0u8; 4096];
        let mut eof = false;
        loop {
            // SAFETY: reading into a buffer we own.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let got = self.pending[at].relayed.get_or_insert_with(Vec::new);
                got.extend_from_slice(&buf[..n as usize]);
                if got.len() > MAX_RELAYED {
                    eof = true;
                    break;
                }
                continue;
            }
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
                break;
            }
            eof = true;
            break;
        }
        let got = self.pending[at].relayed.take().unwrap_or_default();
        match framed_reply(&got) {
            Some(reply) => {
                let mut msg = reply.to_vec();
                let p = &self.pending[at];
                if msg.len() >= 12 && u16::from_be_bytes([msg[0], msg[1]]) == p.our && response_matches(&msg, p) {
                    self.answered(at, &mut msg);
                } else {
                    self.pending.swap_remove(at);
                }
            }
            // Gone before a whole reply: the client asks again.
            None if eof => {
                self.pending.swap_remove(at);
            }
            None => self.pending[at].relayed = Some(got),
        }
    }

    /// A stream to the host's DNS relay with `msg` sent on it, non-blocking
    /// for the reply; None while the host is taken for down.
    fn relay_stream(&mut self, msg: &[u8]) -> Option<OwnedFd> {
        if self.relay_down_until.is_some_and(|t| Instant::now() < t) {
            return None;
        }
        let Ok(stream) = super::transport::dial_within(kiri_proto::DNS_RELAY_PORT, RELAY_CONNECT) else {
            self.relay_down_until = Some(Instant::now() + RELAY_BACKOFF);
            return None;
        };
        self.relay_down_until = None;
        let mut frame = Vec::with_capacity(2 + msg.len());
        frame.extend_from_slice(&u16::try_from(msg.len()).ok()?.to_be_bytes());
        frame.extend_from_slice(msg);
        // A few hundred bytes on a fresh stream: one write, never short.
        // SAFETY: writing a buffer we own to a socket we own.
        if unsafe { libc::write(stream.as_raw_fd(), frame.as_ptr().cast(), frame.len()) } != frame.len() as isize {
            return None;
        }
        sys::set_nonblocking(stream.as_raw_fd()).ok()?;
        Some(stream)
    }

    /// Sends lookups the VM's resolver has left unanswered too long
    /// through the host, and hands one a container's own server has left
    /// unanswered for `OWN_WAIT` to its next (SERVFAIL when none is left or
    /// `OWN_DEADLINE` has passed). Drops what no one is waiting for any more.
    /// Called on every turn of the agent's loop, after the replies are read.
    pub fn on_timer(&mut self) {
        let now = Instant::now();
        // Backwards: a removal moves the last entry into its place.
        for at in (0..self.pending.len()).rev() {
            let p = &self.pending[at];
            if p.own.is_some() {
                if now.duration_since(p.started) >= OWN_DEADLINE {
                    self.fail(at);
                } else if now.duration_since(p.sent) >= OWN_WAIT {
                    self.hand_over(at);
                }
            } else if p.may_relay && now.duration_since(p.sent) >= FALLBACK_AFTER {
                self.relay(at);
            } else if now.duration_since(p.sent) >= FORWARD_TIMEOUT {
                self.pending.swap_remove(at);
            }
        }
    }

    /// Milliseconds until `on_timer` has something to do; -1 for never.
    pub fn timeout_ms(&self) -> i32 {
        let now = Instant::now();
        let left = |from: Instant, wait: Duration| wait.saturating_sub(now.duration_since(from));
        self.pending
            .iter()
            .filter_map(|p| {
                let due = if p.own.is_some() {
                    left(p.sent, OWN_WAIT).min(left(p.started, OWN_DEADLINE))
                } else if p.may_relay {
                    left(p.sent, FALLBACK_AFTER)
                } else {
                    // Swept by the next lookup (or `on_timer`), no wake-up for it.
                    return None;
                };
                Some(wait_ms(due))
            })
            .min()
            .unwrap_or(-1)
    }
}

/// A wait in whole milliseconds, rounded up: a wake-up a fraction of a
/// millisecond early would find nothing due and poll again at once.
fn wait_ms(due: Duration) -> i32 {
    due.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32
}

/// An error a connected UDP socket reports when the way to the server is
/// closed: nothing listens there, or the host or network can't be reached,
/// or sending is not permitted. The next server needs no wait.
fn is_unreachable(errno: i32) -> bool {
    matches!(errno, libc::ECONNREFUSED | libc::EHOSTUNREACH | libc::ENETUNREACH | libc::EPERM)
}

/// Query `q`'s ID.
fn id_of(q: &[u8]) -> u16 {
    u16::from_be_bytes([q[0], q[1]])
}

/// Whether query `q` asked for recursion (RD), which a reply copies.
fn rd(q: &[u8]) -> bool {
    q.get(2).is_some_and(|f| f & 0x01 != 0)
}

/// A SERVFAIL for a query with this `id`, RD flag and `question` section.
fn servfail(id: u16, rd: bool, question: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(12 + question.len());
    r.extend_from_slice(&id.to_be_bytes());
    r.extend_from_slice(&[0x80 | u8::from(rd), 0x82]); // response, RD as asked, recursion available, SERVFAIL
    r.extend_from_slice(&1u16.to_be_bytes());
    r.extend_from_slice(&[0; 6]);
    r.extend_from_slice(question);
    r
}

/// A UDP socket connected to `dest`:53 with `msg` sent on it, or None if
/// it couldn't be sent (no route, the network down).
fn ask(dest: [u8; 4], msg: &[u8]) -> Option<OwnedFd> {
    let sock = udp_socket().ok()?;
    let to = sockaddr(dest, 53);
    // Connected, so it only receives datagrams from this resolver.
    // SAFETY: connecting to a valid sockaddr_in.
    if unsafe { libc::connect(sock.as_raw_fd(), (&to as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as u32) } != 0 {
        return None;
    }
    // SAFETY: sending a buffer we own on a connected socket.
    if unsafe { libc::send(sock.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) } < 0 {
        return None;
    }
    Some(sock)
}

/// The reply in `u16 len | reply`, once all of it is there.
fn framed_reply(got: &[u8]) -> Option<&[u8]> {
    let len = u16::from_be_bytes([*got.first()?, *got.get(1)?]) as usize;
    got.get(2..2 + len)
}

/// The embedded DNS's socket in a container's network namespace, which
/// the calling thread is in: on a free high port of 127.0.0.11, with
/// 127.0.0.11:53 redirected to it, as Docker does, so that a DNS server of
/// the container's own (dnsmasq, Pi-hole, CoreDNS, AdGuard) can bind
/// 0.0.0.0:53 while the container's lookups still reach this one. Where the
/// redirect can't be made (a kernel without nf_nat), port 53 itself.
fn embedded_socket() -> io::Result<OwnedFd> {
    let sock = bound(EMBEDDED, 0)?;
    match local_port(&sock).and_then(|port| super::netlink::redirect_embedded_dns(EMBEDDED, port)) {
        Ok(()) => Ok(sock),
        Err(e) => {
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("kiri-init: dns: no redirect to the embedded DNS ({e}); it takes port 53 in each container");
            }
            drop(sock);
            bound(EMBEDDED, 53)
        }
    }
}

/// The address socket `sock` is bound to.
fn local_addr(sock: &OwnedFd) -> Option<[u8; 4]> {
    // SAFETY: sockaddr_in is plain data; getsockname fills at most `len` bytes.
    let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    // SAFETY: a socket we own and a buffer of `len` bytes.
    let named = unsafe { libc::getsockname(sock.as_raw_fd(), (&mut a as *mut libc::sockaddr_in).cast(), &mut len) } == 0;
    named.then(|| a.sin_addr.s_addr.to_ne_bytes())
}

/// The port socket `sock` is bound to.
fn local_port(sock: &OwnedFd) -> io::Result<u16> {
    // SAFETY: sockaddr_in is plain data; getsockname fills at most `len` bytes.
    let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    // SAFETY: a socket we own and a buffer of `len` bytes.
    sys::check(unsafe { libc::getsockname(sock.as_raw_fd(), (&mut a as *mut libc::sockaddr_in).cast(), &mut len) } as _, "dns getsockname")?;
    Ok(u16::from_be(a.sin_port))
}

/// A UDP socket bound to `ip`:`port` (0: a free one).
fn bound(ip: [u8; 4], port: u16) -> io::Result<OwnedFd> {
    let sock = udp_socket()?;
    let addr = sockaddr(ip, port);
    // SAFETY: binding to a valid sockaddr_in.
    sys::check(
        unsafe { libc::bind(sock.as_raw_fd(), (&addr as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as u32) } as _,
        "dns bind",
    )?;
    Ok(sock)
}

fn response_matches(msg: &[u8], pending: &Pending) -> bool {
    msg.get(2).is_some_and(|flags| flags & 0x80 != 0) && msg.get(12..12 + pending.question.len()).is_some_and(|question| question == pending.question)
}

fn random_id() -> Option<u16> {
    let mut bytes = [0u8; 2];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut bytes).ok()?;
    Some(u16::from_ne_bytes(bytes))
}

/// The first question: (lowercase name without the trailing dot, qtype,
/// offset just past the question).
fn parse_question(q: &[u8]) -> Option<(String, u16, usize)> {
    if u16::from_be_bytes([q[4], q[5]]) != 1 {
        return None;
    }
    let mut i = 12;
    let mut name = String::new();
    loop {
        let len = *q.get(i)? as usize;
        if len == 0 {
            i += 1;
            break;
        }
        if len & 0xc0 != 0 {
            return None; // compression in a question: not from a sane client
        }
        let label = q.get(i + 1..i + 1 + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&String::from_utf8_lossy(label).to_ascii_lowercase());
        i += 1 + len;
    }
    let qtype = u16::from_be_bytes([*q.get(i)?, *q.get(i + 1)?]);
    // QCLASS must be there too: both callers copy q[12..end], and a slice
    // past the packet would abort PID 1 (panic = "abort"), and with it the VM.
    q.get(12..i + 4)?;
    Some((name, qtype, i + 4))
}

#[cfg(test)]
mod tests {
    use super::{Dns, Pending, Server, response_matches, udp_socket};

    #[test]
    fn host_names_answer_on_every_network() {
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: None });
        let q = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x04host\x06docker\x08internal\x00\x00\x01\x00\x01";
        let r = dns.answer(0, q).expect("answered, not forwarded");
        assert_eq!(&r[6..8], &[0, 1], "one answer");
        assert_eq!(&r[r.len() - 4..], &super::super::net::HOST_ADDR);
        let other = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01";
        assert!(dns.answer(0, other).is_none(), "anything else is forwarded");
        // AAAA: answered with no records (NODATA), not forwarded upstream.
        let q6 = query("host.kiri.internal", 28);
        let r = dns.answer(0, &q6).expect("AAAA answered, not forwarded");
        assert_eq!((r[3] & 0x0f, answers(&r, q6.len()).len()), (0, 0));
        let q = query("gateway.docker.internal", 255);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(1, super::super::net::HOST_ADDR.to_vec())]);
    }

    #[test]
    fn a_containers_server_answers_for_each_of_its_networks() {
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into(), "kiri2".into()], sock: udp_socket().unwrap(), container: Some((42, [10, 5, 0, 2])) });
        dns.register("kiri1", ["web"].iter(), [10, 5, 0, 3]);
        dns.register("kiri2", ["db"].iter(), [10, 6, 0, 3]);
        let q = query("db", 1);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(1, vec![10, 6, 0, 3])]);
        let q = query("web", 1);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(1, vec![10, 5, 0, 3])]);
        dns.leave(42, "kiri2");
        assert!(dns.answer(0, &query("db", 1)).is_none(), "gone with the network");
        dns.drop_container(42);
        assert!(dns.servers.is_empty());
    }

    /// #294: a container on its project's network and on a shared one, both
    /// with a `db`, joining the shared one first. Docker searches the
    /// networks by name, whatever the join order.
    #[test]
    fn a_containers_networks_are_searched_by_docker_network_name() {
        // (shared network, project network, which one answers)
        for (shared, project, shared_answers) in [("kiri_shared_t", "kra_default", true), ("ddev_default", "ddev-a_default", false), ("zz_shared", "kca_default", false)] {
            let mut dns = Dns::default();
            dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into(), "kiri2".into()], sock: udp_socket().unwrap(), container: Some((42, [10, 1, 0, 2])) });
            dns.name_network("kiri1", shared);
            dns.name_network("kiri2", project);
            // This project's db on both networks, another project's on the shared one.
            dns.register("kiri1", ["db"].iter(), [10, 1, 0, 3]);
            dns.register("kiri1", ["db"].iter(), [10, 1, 0, 4]);
            dns.register("kiri2", ["db"].iter(), [10, 2, 0, 3]);
            let q = query("db", 1);
            let got = answers(&dns.answer(0, &q).unwrap(), q.len());
            let want = if shared_answers { vec![(1, vec![10, 1, 0, 3]), (1, vec![10, 1, 0, 4])] } else { vec![(1, vec![10, 2, 0, 3])] };
            assert_eq!(got, want, "{shared} and {project}");
        }
    }

    #[test]
    fn a_removed_networks_name_goes_with_its_bridge() {
        let mut dns = Dns::default();
        dns.name_network("kiri1", "frontend");
        dns.forget_bridge("kiri1");
        assert!(dns.networks.is_empty());
    }

    #[test]
    fn every_forwarded_query_has_its_own_port_and_its_answer_goes_back() {
        use std::os::fd::AsRawFd;
        let bind = |ip: [u8; 4], port: u16| {
            let s = udp_socket().unwrap();
            let a = super::sockaddr(ip, port);
            assert_eq!(unsafe { libc::bind(s.as_raw_fd(), (&a as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as u32) }, 0, "bind");
            s
        };
        let name = |fd: i32| {
            let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_in>() as u32;
            unsafe { libc::getsockname(fd, (&mut a as *mut libc::sockaddr_in).cast(), &mut len) };
            a
        };
        let recv = |fd: i32| {
            let mut buf = [0u8; 512];
            let mut from: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_in>() as u32;
            for _ in 0..100 {
                let n = unsafe { libc::recvfrom(fd, buf.as_mut_ptr().cast(), buf.len(), 0, (&mut from as *mut libc::sockaddr_in).cast(), &mut len) };
                if n > 0 {
                    return (buf[..n as usize].to_vec(), from);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("nothing arrived");
        };
        // A resolver at 127.0.0.2:53, a server for the containers, a client.
        let resolver = bind([127, 0, 0, 2], 53);
        let server = bind([127, 0, 0, 3], 0);
        let client = bind([127, 0, 0, 1], 0);
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: server, container: None });
        let server_fd = dns.servers[0].sock.as_raw_fd();
        let to = name(server_fd);
        let mut ports = Vec::new();
        for id in [0x1111u16, 0x2222] {
            let mut q = query("example.com", 1);
            q[0..2].copy_from_slice(&id.to_be_bytes());
            super::send_to(client.as_raw_fd(), &q, &to);
            std::thread::sleep(std::time::Duration::from_millis(20));
            dns.on_readable(server_fd, &[[127, 0, 0, 2]]);
            let (got, from) = recv(resolver.as_raw_fd());
            ports.push(from.sin_port);
            // The resolver answers; its answer goes back with the client's ID.
            let mut reply = got.clone();
            reply[2] |= 0x80;
            super::send_to(resolver.as_raw_fd(), &reply, &from);
            std::thread::sleep(std::time::Duration::from_millis(20));
            let fd = dns.pending.iter().find(|p| p.our == u16::from_be_bytes([got[0], got[1]])).expect("in flight").sock.as_raw_fd();
            dns.on_readable(fd, &[[127, 0, 0, 2]]);
            let (answer, _) = recv(client.as_raw_fd());
            assert_eq!(u16::from_be_bytes([answer[0], answer[1]]), id);
        }
        assert_ne!(ports[0], ports[1], "a fresh source port per query");
        assert!(dns.pending.is_empty(), "answered queries close their sockets");
    }

    fn wait_readable(fd: i32) {
        let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: poll has room for one descriptor.
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 1000) }, 1, "nothing arrived");
        assert_ne!(poll.revents & (libc::POLLIN | libc::POLLERR), 0);
    }

    /// The address a connected socket sends to.
    fn peer(fd: i32) -> [u8; 4] {
        // SAFETY: sockaddr_in is plain data; getpeername fills it in.
        let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_in>() as u32;
        // SAFETY: getpeername writes at most `len` bytes into a sockaddr_in we own.
        assert_eq!(unsafe { libc::getpeername(fd, (&mut a as *mut libc::sockaddr_in).cast(), &mut len) }, 0);
        a.sin_addr.s_addr.to_ne_bytes()
    }

    #[test]
    fn own_servers_are_tried_in_order_after_a_refusal() {
        use std::net::UdpSocket;
        use std::os::fd::AsRawFd;
        let port = |fd: i32| {
            let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_in>() as u32;
            // SAFETY: getsockname fills a sockaddr_in we own.
            assert_eq!(unsafe { libc::getsockname(fd, (&mut a as *mut libc::sockaddr_in).cast(), &mut len) }, 0);
            a.sin_port
        };
        let resolver_ip = [127, 0, 0, 5];
        let resolver = UdpSocket::from(super::bound(resolver_ip, 53).unwrap());
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_nonblocking(true).unwrap();
        let std::net::SocketAddr::V4(addr) = client.local_addr().unwrap() else { panic!() };
        let from = super::sockaddr(addr.ip().octets(), addr.port());
        // One server still works; refusals move through the list in order.
        for servers in [vec![resolver_ip], vec![[127, 0, 0, 4], resolver_ip], vec![[127, 0, 0, 4], [127, 0, 0, 6], resolver_ip]] {
            let owner = [10, 5, 0, 2];
            let mut dns = Dns { relay: true, ..Default::default() };
            dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
            dns.set_upstream(owner, servers.clone());
            let q = query("example.com", 1);
            dns.forward(0, &q, from, &[]);
            assert_eq!(dns.pending.len(), 1);
            assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), servers[0], "the first server is asked first");
            let mut sent_query = dns.pending[0].query.clone();
            for &next in &servers[1..] {
                let fd = dns.pending[0].sock.as_raw_fd();
                let old_port = port(fd);
                dns.pending[0].sent = Instant::now() - super::FORWARD_TIMEOUT;
                wait_readable(fd);
                let retrying = Instant::now();
                dns.on_readable(fd, &[]);
                assert_eq!(dns.pending.len(), 1);
                let p = &dns.pending[0];
                assert_ne!(p.sock.as_raw_fd(), fd, "a fresh socket for the next server");
                assert_ne!(port(p.sock.as_raw_fd()), old_port, "a fresh source port");
                assert!(p.sent >= retrying, "a fresh retry survives the sweep");
                assert_eq!(p.query[2..], sent_query[2..], "the same query");
                assert_ne!(p.query[..2], sent_query[..2], "under a fresh ID");
                assert_eq!(peer(p.sock.as_raw_fd()), next, "servers are tried in the order given");
                sent_query = p.query.clone();
            }
            let mut buf = [0u8; 512];
            wait_readable(resolver.as_raw_fd());
            let (n, peer) = resolver.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], sent_query);
            let mut reply = buf[..n].to_vec();
            reply[2] |= 0x80;
            resolver.send_to(&reply, peer).unwrap();
            let fd = dns.pending[0].sock.as_raw_fd();
            wait_readable(fd);
            dns.on_readable(fd, &[]);
            assert!(dns.pending.is_empty(), "answered queries close their sockets");
            wait_readable(client.as_raw_fd());
            let n = client.recv(&mut buf).unwrap();
            assert_eq!(&buf[..2], &q[..2], "the client's original ID");
            assert_eq!(&buf[2..n], &reply[2..]);
            assert!(dns.relay_down_until.is_none(), "no host relay was tried");
        }
    }

    #[test]
    fn an_own_server_that_cant_be_sent_to_is_skipped() {
        use std::net::UdpSocket;
        use std::os::fd::AsRawFd;
        // The broadcast address can't be sent to (no permission, or no route):
        // `ask` gives up on it at once.
        let unsendable = [255, 255, 255, 255];
        let resolver_ip = [127, 0, 0, 10];
        let resolver = UdpSocket::from(super::bound(resolver_ip, 53).unwrap());
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(addr) = client.local_addr().unwrap() else { panic!() };
        let from = super::sockaddr(addr.ip().octets(), addr.port());
        let owner = [10, 5, 0, 2];
        let mut dns = Dns { relay: true, ..Default::default() };
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
        dns.set_upstream(owner, vec![unsendable, resolver_ip]);
        dns.forward(0, &query("example.com", 1), from, &[]);
        assert_eq!(dns.pending.len(), 1);
        assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), resolver_ip, "the server that can't be sent to was skipped");
        let mut buf = [0u8; 512];
        wait_readable(resolver.as_raw_fd());
        let (n, _) = resolver.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &dns.pending[0].query[..]);
        // With nothing to send to, no query is kept and the host is not asked.
        let mut dns = Dns { relay: true, ..Default::default() };
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
        dns.set_upstream(owner, vec![unsendable]);
        dns.forward(0, &query("example.com", 1), from, &[]);
        assert!(dns.pending.is_empty());
        assert!(dns.relay_down_until.is_none(), "no host relay was tried");
        wait_readable(client.as_raw_fd());
        let mut buf = [0u8; 512];
        let n = client.recv(&mut buf).unwrap();
        assert_eq!(buf[3] & 0x0f, 2, "SERVFAIL, not silence");
        assert!(n >= 12);
    }

    /// REFUSED or SERVFAIL from one of a container's own servers sends the
    /// query to its next one, as Docker's embedded DNS does; from the last
    /// one, that answer goes back to the client.
    #[test]
    fn an_own_server_that_answers_refused_hands_over_to_the_next() {
        use std::net::UdpSocket;
        use std::os::fd::AsRawFd;
        let (first_ip, second_ip) = ([127, 0, 0, 20], [127, 0, 0, 21]);
        let first = UdpSocket::from(super::bound(first_ip, 53).unwrap());
        let second = UdpSocket::from(super::bound(second_ip, 53).unwrap());
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(addr) = client.local_addr().unwrap() else { panic!() };
        let from = super::sockaddr(addr.ip().octets(), addr.port());
        let owner = [10, 5, 0, 2];
        // Answers the query waiting at `server` with `rcode`.
        let reply = |server: &UdpSocket, rcode: u8| {
            let mut buf = [0u8; 512];
            wait_readable(server.as_raw_fd());
            let (n, peer) = server.recv_from(&mut buf).unwrap();
            let mut r = buf[..n].to_vec();
            r[2] |= 0x80;
            r[3] = (r[3] & 0xf0) | rcode;
            server.send_to(&r, peer).unwrap();
        };
        for (rcode, servers, answered_by) in [(5u8, vec![first_ip, second_ip], 1usize), (2, vec![first_ip, second_ip], 1), (5, vec![first_ip], 0)] {
            let mut dns = Dns { relay: true, ..Default::default() };
            dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
            dns.set_upstream(owner, servers.clone());
            let q = query("example.com", 1);
            dns.forward(0, &q, from, &[]);
            reply(&first, rcode);
            let fd = dns.pending[0].sock.as_raw_fd();
            wait_readable(fd);
            dns.on_readable(fd, &[]);
            let mut buf = [0u8; 512];
            if answered_by == 1 {
                assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), second_ip, "rcode {rcode}: the next server");
                reply(&second, 0);
                let fd = dns.pending[0].sock.as_raw_fd();
                wait_readable(fd);
                dns.on_readable(fd, &[]);
                assert!(dns.pending.is_empty());
                wait_readable(client.as_raw_fd());
                let n = client.recv(&mut buf).unwrap();
                assert_eq!((&buf[..2], buf[3] & 0x0f, n), (&q[..2], 0, q.len()), "the next server's answer, with the client's ID");
            } else {
                assert!(dns.pending.is_empty(), "the last server's answer goes back");
                wait_readable(client.as_raw_fd());
                client.recv(&mut buf).unwrap();
                assert_eq!((&buf[..2], buf[3] & 0x0f), (&q[..2], rcode));
            }
            assert!(dns.relay_down_until.is_none(), "no host relay was tried");
        }
    }

    /// The client's side of a test: a socket and the address a server sees.
    fn client() -> (std::net::UdpSocket, libc::sockaddr_in) {
        let c = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(addr) = c.local_addr().unwrap() else { panic!() };
        (c, super::sockaddr(addr.ip().octets(), addr.port()))
    }

    /// What the client was told: (its ID, rcode), or None if nothing came.
    fn told(client: &std::net::UdpSocket, wait_ms: u64) -> Option<(u16, u8)> {
        client.set_read_timeout(Some(std::time::Duration::from_millis(wait_ms))).unwrap();
        let mut buf = [0u8; 512];
        let n = client.recv(&mut buf).ok()?;
        assert!(n >= 12);
        Some((u16::from_be_bytes([buf[0], buf[1]]), buf[3] & 0x0f))
    }

    /// A container's own server that leaves a query unanswered for
    /// `OWN_WAIT` hands it to the next one, under a fresh ID; never to the
    /// VM or host. The last one's silence ends in SERVFAIL, not a hang.
    #[test]
    fn a_silent_own_server_hands_over_after_the_wait() {
        use std::os::fd::AsRawFd;
        let (silent_ip, next_ip) = ([127, 0, 0, 22], [127, 0, 0, 23]);
        let _silent = std::net::UdpSocket::from(super::bound(silent_ip, 53).unwrap());
        let next = std::net::UdpSocket::from(super::bound(next_ip, 53).unwrap());
        let (client, from) = client();
        let owner = [10, 5, 0, 2];
        let mut dns = Dns { relay: true, ..Default::default() };
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
        dns.set_upstream(owner, vec![silent_ip, next_ip]);
        let q = query("example.com", 1);
        dns.forward(0, &q, from, &[]);
        let first_query = dns.pending[0].query.clone();
        assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), silent_ip);
        let t = dns.timeout_ms();
        assert!((0..=4000).contains(&t), "{t}");
        dns.on_timer();
        assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), silent_ip, "not before the wait");
        // Slow company DNS: a server that takes 1 s (the old wait) is still waited for.
        dns.pending[0].sent = Instant::now() - std::time::Duration::from_millis(1500);
        dns.on_timer();
        assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), silent_ip, "a server is given OWN_WAIT, not a second");
        dns.pending[0].sent = Instant::now() - super::OWN_WAIT;
        dns.on_timer();
        assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), next_ip, "the next server after the wait");
        let mut buf = [0u8; 512];
        wait_readable(next.as_raw_fd());
        let (n, _) = next.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &dns.pending[0].query[..]);
        assert_eq!(buf[2..n], first_query[2..], "the same question");
        assert_ne!(buf[..2], first_query[..2], "under a fresh ID");
        assert!(!dns.pending[0].may_relay && dns.relay_down_until.is_none(), "never the host");
        // Nothing left to hand over to: SERVFAIL to the client, entry gone.
        dns.pending[0].sent = Instant::now() - super::OWN_WAIT;
        dns.on_timer();
        assert!(dns.pending.is_empty());
        assert_eq!(told(&client, 1000), Some((u16::from_be_bytes([q[0], q[1]]), 2)));
        assert_eq!(dns.timeout_ms(), -1);
    }

    /// Every server of the container's own refuses: the client gets SERVFAIL
    /// at once and no entry or socket is left behind; the VM and host are
    /// never asked.
    #[test]
    fn exhausted_own_servers_answer_servfail_and_leave_nothing() {
        use std::os::fd::AsRawFd;
        let vm_ip = [127, 0, 0, 9];
        let vm = std::net::UdpSocket::from(super::bound(vm_ip, 53).unwrap());
        vm.set_nonblocking(true).unwrap();
        let (client, from) = client();
        for servers in [vec![[127, 0, 0, 7]], vec![[127, 0, 0, 7], [127, 0, 0, 8]]] {
            let attempts = servers.len();
            let owner = [10, 5, 0, 2];
            let mut dns = Dns { relay: true, ..Default::default() };
            dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
            dns.set_upstream(owner, servers);
            let q = query("example.com", 1);
            dns.forward(0, &q, from, &[vm_ip]);
            assert_eq!(dns.pending.len(), 1);
            for _ in 0..attempts {
                let Some(p) = dns.pending.first() else { break };
                let fd = p.sock.as_raw_fd();
                wait_readable(fd);
                dns.on_readable(fd, &[vm_ip]);
            }
            assert!(dns.pending.is_empty(), "no entry left to leak");
            assert_eq!(told(&client, 1000), Some((u16::from_be_bytes([q[0], q[1]]), 2)), "SERVFAIL, at once");
            let mut buf = [0u8; 512];
            assert_eq!(vm.recv(&mut buf).unwrap_err().kind(), std::io::ErrorKind::WouldBlock, "no VM lookup");
            assert!(dns.relay_down_until.is_none(), "no host relay was tried");
            assert_eq!(dns.timeout_ms(), -1);
        }
    }

    /// Docker uses three `--dns` servers and no more: a longer list is cut,
    /// so no query walks, or waits on, more than three.
    #[test]
    fn at_most_three_own_servers_are_kept_and_asked() {
        let mut dns = Dns::default();
        let owner = [10, 5, 0, 2];
        dns.set_upstream(owner, (1..=50).map(|i| [127, 0, 0, i]).collect());
        assert_eq!(dns.clients[&owner], [[127, 0, 0, 1], [127, 0, 0, 2], [127, 0, 0, 3]]);
        assert_eq!(super::OWN_DEADLINE, super::OWN_WAIT * 3, "a query lives for three servers' waits at most");
    }

    /// However the servers answer, a query to a container's own servers ends
    /// by `OWN_DEADLINE` (the hand-overs don't restart its clock), with SERVFAIL.
    #[test]
    fn a_query_to_own_servers_ends_at_the_deadline() {
        let ips = [[127, 0, 0, 30], [127, 0, 0, 31], [127, 0, 0, 32]];
        let _held: Vec<_> = ips.iter().map(|ip| std::net::UdpSocket::from(super::bound(*ip, 53).unwrap())).collect();
        let (client, from) = client();
        let owner = [10, 5, 0, 2];
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
        dns.set_upstream(owner, ips.to_vec());
        dns.forward(0, &query("example.com", 1), from, &[]);
        dns.pending[0].started = Instant::now() - super::OWN_DEADLINE;
        dns.pending[0].sent = Instant::now();
        assert!(dns.timeout_ms() <= 0);
        dns.on_timer();
        assert!(dns.pending.is_empty());
        assert!(told(&client, 1000).is_some_and(|(_, rcode)| rcode == 2));
        // An entry nobody waits for (the VM's resolver, not answered) is swept too.
        let mut p = pending();
        p.sent = Instant::now() - super::FORWARD_TIMEOUT;
        p.sock = udp_socket().unwrap();
        dns.pending.push(p);
        dns.on_timer();
        assert!(dns.pending.is_empty());
    }

    #[test]
    fn unreachable_errors_hand_over_without_a_wait() {
        for e in [libc::ECONNREFUSED, libc::EHOSTUNREACH, libc::ENETUNREACH, libc::EPERM] {
            assert!(super::is_unreachable(e), "{e}");
        }
        assert!(!super::is_unreachable(libc::EAGAIN));
    }

    /// A query goes to the next server under an ID of its own: the first
    /// server's knowledge of the ID it was sent doesn't help it answer the next's.
    #[test]
    fn each_hand_over_uses_a_fresh_id() {
        use std::os::fd::AsRawFd;
        let (a, b) = ([127, 0, 0, 40], [127, 0, 0, 41]);
        let _a = std::net::UdpSocket::from(super::bound(a, 53).unwrap());
        let _b = std::net::UdpSocket::from(super::bound(b, 53).unwrap());
        let (_client, from) = client();
        let owner = [10, 5, 0, 2];
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
        dns.set_upstream(owner, vec![a, b]);
        dns.forward(0, &query("example.com", 1), from, &[]);
        let first = dns.pending[0].our;
        assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), a);
        dns.pending[0].sent = Instant::now() - super::OWN_WAIT;
        dns.on_timer();
        assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), b);
        assert_ne!(dns.pending[0].our, first);
        assert_eq!(u16::from_be_bytes([dns.pending[0].query[0], dns.pending[0].query[1]]), dns.pending[0].our);
    }

    #[test]
    fn own_servers_go_with_the_container() {
        let mut dns = Dns::default();
        dns.set_upstream([10, 5, 0, 2], vec![[8, 8, 8, 8]]);
        assert_eq!(dns.clients.get(&[10, 5, 0, 2]), Some(&vec![[8, 8, 8, 8]]));
        dns.unregister("kiri1", [10, 5, 0, 2]);
        assert!(dns.clients.is_empty());
        dns.set_upstream([10, 5, 0, 3], vec![]);
        assert!(dns.clients.is_empty());
    }

    /// A container's queries in flight go with it, so a container given its
    /// address next never gets them handed over or answered.
    #[test]
    fn a_containers_queries_in_flight_go_with_it() {
        let owner = [10, 5, 0, 2];
        let mut dns = Dns::default();
        dns.set_upstream(owner, vec![[127, 0, 0, 1], [127, 0, 0, 2]]);
        let mut p = pending();
        p.key = owner;
        p.own = Some(0);
        dns.pending.push(p);
        let mut other = pending();
        other.key = [10, 5, 0, 3];
        dns.pending.push(other);
        dns.unregister("kiri1", owner);
        assert_eq!(dns.pending.len(), 1);
        assert_eq!(dns.pending[0].key, [10, 5, 0, 3], "another container's stay");
    }

    /// A query names the server it came in on by a number that is never
    /// reused, unlike a descriptor.
    #[test]
    fn servers_are_named_by_a_number_never_reused() {
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: None });
        let first = dns.servers.last().unwrap().id;
        dns.servers.pop();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: None });
        assert_ne!(dns.servers.last().unwrap().id, first);
        let mut p = pending();
        p.server = first;
        dns.pending.push(p);
        dns.forget_pending();
        assert!(dns.pending.is_empty(), "a query of a server that is gone goes");
    }

    /// The wait for `poll` is rounded up: a fraction of a millisecond left is
    /// a 1 ms wait, not a 0 that polls again at once until it is due.
    #[test]
    fn a_wait_under_a_millisecond_is_one_millisecond() {
        use std::time::Duration;
        assert_eq!(super::wait_ms(Duration::ZERO), 0);
        assert_eq!(super::wait_ms(Duration::from_nanos(1)), 1);
        assert_eq!(super::wait_ms(Duration::from_micros(999)), 1);
        assert_eq!(super::wait_ms(Duration::from_millis(1)), 1);
        assert_eq!(super::wait_ms(Duration::from_millis(1) + Duration::from_nanos(1)), 2);
    }

    /// A query past its deadline is answered SERVFAIL when the next query
    /// comes in, and doesn't hold its slot against it.
    #[test]
    fn expired_own_queries_free_their_slots_for_the_next() {
        let ip = [127, 0, 0, 61];
        let _held = std::net::UdpSocket::from(super::bound(ip, 53).unwrap());
        let (client, from) = client();
        let owner = [10, 5, 0, 2];
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
        dns.set_upstream(owner, vec![ip]);
        for _ in 0..super::MAX_OWN_PER_CLIENT {
            let mut p = pending();
            p.key = owner;
            p.own = Some(0);
            p.server = dns.servers[0].id;
            p.client = from;
            p.started = Instant::now() - super::OWN_DEADLINE;
            dns.pending.push(p);
        }
        dns.forward(0, &query("example.com", 1), from, &[]);
        assert_eq!(dns.pending.len(), 1, "only the new query is left");
        assert!(dns.pending[0].started.elapsed() < super::OWN_WAIT);
        assert!(told(&client, 1000).is_some_and(|(id, rcode)| id == 7 && rcode == 2), "the expired ones are answered");
    }

    /// Queries with its own servers live up to `OWN_DEADLINE`: one container
    /// has a smaller share of the slots for them, and past it is told
    /// SERVFAIL at once rather than left to wait.
    #[test]
    fn own_queries_have_a_smaller_share_and_are_told_past_it() {
        let ip = [127, 0, 0, 62];
        let _held = std::net::UdpSocket::from(super::bound(ip, 53).unwrap());
        let (client, from) = client();
        let owner = [10, 5, 0, 2];
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
        dns.set_upstream(owner, vec![ip]);
        for _ in 0..super::MAX_OWN_PER_CLIENT {
            dns.forward(0, &query("example.com", 1), from, &[]);
        }
        assert_eq!(dns.pending.len(), super::MAX_OWN_PER_CLIENT);
        let q = query("example.org", 1);
        dns.forward(0, &q, from, &[]);
        assert_eq!(dns.pending.len(), super::MAX_OWN_PER_CLIENT, "past its share");
        assert_eq!(told(&client, 1000), Some((u16::from_be_bytes([q[0], q[1]]), 2)), "SERVFAIL at once");
    }

    /// Past `OWN_DEADLINE`, or with no server after the one it is with, a
    /// query is not sent on and is left as it was.
    #[test]
    fn no_hand_over_past_the_deadline_or_the_last_server() {
        use std::os::fd::AsRawFd;
        let (a, b) = ([127, 0, 0, 63], [127, 0, 0, 64]);
        let _a = std::net::UdpSocket::from(super::bound(a, 53).unwrap());
        let next = std::net::UdpSocket::from(super::bound(b, 53).unwrap());
        next.set_nonblocking(true).unwrap();
        let (_client, from) = client();
        let owner = [10, 5, 0, 2];
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: Some((42, owner)) });
        dns.set_upstream(owner, vec![a, b]);
        dns.forward(0, &query("example.com", 1), from, &[]);
        let sent = dns.pending[0].query.clone();
        dns.pending[0].started = Instant::now() - super::OWN_DEADLINE;
        assert!(!dns.ask_next_own(0));
        assert_eq!(dns.pending[0].query, sent, "untouched");
        assert_eq!(peer(dns.pending[0].sock.as_raw_fd()), a);
        let mut buf = [0u8; 512];
        assert_eq!(next.recv(&mut buf).unwrap_err().kind(), std::io::ErrorKind::WouldBlock, "the next server isn't asked");
        dns.pending[0].started = Instant::now();
        dns.pending[0].own = Some(1);
        assert!(!dns.ask_next_own(0));
        assert_eq!(dns.pending[0].query, sent, "untouched with no server left");
    }

    /// SERVFAIL copies the query's recursion-desired flag.
    #[test]
    fn servfail_copies_recursion_desired() {
        let mut q = query("example.com", 1);
        assert!(super::rd(&q));
        assert_eq!(super::servfail(7, super::rd(&q), &q[12..])[2], 0x81);
        q[2] &= !0x01;
        assert_eq!(super::servfail(7, super::rd(&q), &q[12..])[2..4], [0x80, 0x82]);
        assert!(!super::rd(&[]));
    }
    use std::time::Instant;

    /// A query for `name` of `qtype`, as a resolver sends it.
    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0xab, 0xcd, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&[0, 1]);
        q
    }

    /// (type, data) of each answer.
    fn answers(r: &[u8], qlen: usize) -> Vec<(u16, Vec<u8>)> {
        let count = u16::from_be_bytes([r[6], r[7]]) as usize;
        let mut i = qlen;
        let mut out = Vec::new();
        for _ in 0..count {
            let ty = u16::from_be_bytes([r[i + 2], r[i + 3]]);
            let len = u16::from_be_bytes([r[i + 10], r[i + 11]]) as usize;
            out.push((ty, r[i + 12..i + 12 + len].to_vec()));
            i += 12 + len;
        }
        out
    }

    #[test]
    fn names_answer_a_and_aaaa() {
        let mut dns = Dns::default();
        // A server needs a socket; any UDP socket stands in for the bridge's.
        dns.servers.push(super::Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: super::udp_socket().unwrap(), container: None });
        dns.register("kiri1", ["web", "Alias"].iter(), [10, 5, 0, 2]);
        let v6 = [0xfd, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        dns.register6("kiri1", ["web", "Alias"].iter(), [10, 5, 0, 2], v6);
        dns.register("kiri1", ["old"].iter(), [10, 5, 0, 3]);

        let q = query("web", 1);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(1, vec![10, 5, 0, 2])]);
        let q = query("alias", 28);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(28, v6.to_vec())]);
        let q = query("web", 255);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()).len(), 2);
        // An IPv4-only container: AAAA is answered, empty (NODATA).
        let q = query("old", 28);
        let r = dns.answer(0, &q).unwrap();
        assert_eq!((r[3] & 0x0f, answers(&r, q.len()).len()), (0, 0));
        // Not a container: forwarded.
        assert!(dns.answer(0, &query("example.com", 28)).is_none());
        // Gone with the container's IPv4 address.
        dns.unregister("kiri1", [10, 5, 0, 2]);
        assert!(dns.answer(0, &query("web", 28)).is_none());
        dns.forget_bridge("kiri1");
        assert!(dns.servers.is_empty() && dns.names.is_empty() && dns.names6.is_empty());
    }

    /// The agent handles a rename by dropping every name registered for the
    /// container's address and putting back the new set (aliases kept).
    #[test]
    fn renaming_replaces_names_but_keeps_aliases() {
        let mut dns = Dns::default();
        dns.servers.push(super::Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: super::udp_socket().unwrap(), container: None });
        let ip = [10, 5, 0, 2];
        let v6 = [0xfd, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        dns.register("kiri1", ["old", "keep-alias"].iter(), ip);
        dns.register6("kiri1", ["old", "keep-alias"].iter(), ip, v6);
        // Another container's name on the same network is left alone.
        dns.register("kiri1", ["other"].iter(), [10, 5, 0, 3]);
        dns.set_upstream(ip, vec![[8, 8, 8, 8]]);
        dns.rename("kiri1", ip, Some(v6), true, &["new", "keep-alias"]);
        let q = query("new", 1);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(1, ip.to_vec())]);
        let q = query("new", 28);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(28, v6.to_vec())]);
        let q = query("keep-alias", 1);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(1, ip.to_vec())]);
        assert!(dns.answer(0, &query("old", 1)).is_none());
        assert!(dns.answer(0, &query("old", 28)).is_none());
        let q = query("other", 1);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(1, vec![10, 5, 0, 3])]);
        // Its own --dns servers are kept under its address: a rename keeps them.
        assert_eq!(dns.clients.get(&ip), Some(&vec![[8, 8, 8, 8]]));
    }

    /// On a network with no IPv4 addresses the container's address is only
    /// its key: a rename gives AAAA records and no A.
    #[test]
    fn renaming_on_an_ipv6_only_network_makes_no_a_records() {
        let mut dns = Dns::default();
        dns.servers.push(super::Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: super::udp_socket().unwrap(), container: None });
        let key = [10, 5, 0, 2];
        let v6 = [0xfd, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        dns.register6("kiri1", ["old"].iter(), key, v6);
        dns.rename("kiri1", key, Some(v6), false, &["new"]);
        let q = query("new", 28);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(28, v6.to_vec())]);
        let q = query("new", 1);
        assert!(answers(&dns.answer(0, &q).unwrap(), q.len()).is_empty(), "NODATA, not the internal key");
        assert!(dns.answer(0, &query("old", 28)).is_none());
    }

    /// Names that are not DNS names, and a name taken from the host, change
    /// nothing a container or the host is reached by.
    #[test]
    fn a_rename_never_registers_an_empty_name_or_takes_a_host_name() {
        let mut dns = Dns::default();
        dns.servers.push(super::Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: super::udp_socket().unwrap(), container: None });
        let ip = [10, 5, 0, 2];
        dns.rename("kiri1", ip, None, true, &["", ".", "a..b", "web", "host.docker.internal", "gateway.docker.internal"]);
        assert!(dns.names["kiri1"].keys().all(|n| !n.is_empty() && n != "." && n != "a..b"));
        let q = query("web", 1);
        assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(1, ip.to_vec())]);
        for host in ["host.docker.internal", "gateway.docker.internal"] {
            let q = query(host, 1);
            assert_eq!(answers(&dns.answer(0, &q).unwrap(), q.len()), [(1, super::super::net::HOST_ADDR.to_vec())], "{host} stays the host's");
        }
    }

    #[test]
    fn dns_names_are_labels_of_letters_digits_dashes_and_underscores() {
        for ok in ["web", "a.b", "my_db-1", "a".repeat(63).as_str()] {
            assert!(super::dns_name(ok), "{ok}");
        }
        for bad in ["", ".", "a.", ".a", "a..b", "a b", "a/b", &"a".repeat(64), &format!("{}.{}.{}.{}.{}", "a".repeat(60), "a".repeat(60), "a".repeat(60), "a".repeat(60), "a".repeat(60))] {
            assert!(!super::dns_name(bad), "{bad}");
        }
    }

    fn pending() -> Pending {
        Pending {
            sock: udp_socket().unwrap(),
            our: 1,
            key: [0; 4],
            server: 0,
            client: unsafe { std::mem::zeroed() },
            id: 7,
            question: b"\x03www\x07example\x03com\x00\x00\x01\x00\x01".to_vec(),
            sent: Instant::now(),
            query: Vec::new(),
            own: None,
            started: Instant::now(),
            may_relay: false,
            relayed: None,
        }
    }

    /// The default bridge moved off a subnet the host took (#489): its
    /// bridge is made again with another gateway, and the gateway's
    /// listener follows it rather than staying on the old address.
    #[test]
    fn a_gateway_listener_follows_its_bridges_new_gateway() {
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri0".into()], sock: super::bound([127, 0, 0, 2], 0).unwrap(), container: None });
        dns.serve("kiri0", [127, 0, 0, 2]).unwrap();
        assert_eq!(dns.servers.len(), 1, "the same gateway keeps its listener");
        // Port 53 may need privileges this test lacks: either way the old
        // listener is gone.
        let made = dns.serve("kiri0", [127, 0, 0, 3]).is_ok();
        assert!(!dns.serves_at([127, 0, 0, 2]));
        assert_eq!(dns.serves_at([127, 0, 0, 3]), made);
    }

    #[test]
    fn one_client_cant_take_every_forwarded_slot() {
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri0".into()], sock: udp_socket().unwrap(), container: None });
        let client = super::sockaddr([172, 17, 0, 2], 4000);
        for _ in 0..super::MAX_PER_CLIENT {
            let mut p = pending();
            p.key = [172, 17, 0, 2];
            dns.pending.push(p);
        }
        let q = query("example.com", 1);
        // Loopback as the resolver: sending always works.
        dns.forward(0, &q, client, &[[127, 0, 0, 1]]);
        assert_eq!(dns.pending.len(), super::MAX_PER_CLIENT, "that container's next lookup waits");
        dns.forward(0, &q, super::sockaddr([172, 17, 0, 3], 4000), &[[127, 0, 0, 1]]);
        assert_eq!(dns.pending.len(), super::MAX_PER_CLIENT + 1, "another container's goes out");
    }

    #[test]
    fn a_host_that_didnt_answer_isnt_dialled_again_at_once() {
        let mut dns = Dns { relay: true, ..Default::default() };
        // No host to dial in a test: the first try fails and backs off.
        assert!(dns.relay_stream(b"query").is_none());
        let until = dns.relay_down_until.expect("backing off");
        assert!(until > Instant::now());
        assert!(dns.relay_stream(b"query").is_none());
        assert_eq!(dns.relay_down_until, Some(until), "not tried again while backing off");
    }

    #[test]
    fn a_relayed_reply_is_whole_only_once_its_length_is_there() {
        use super::framed_reply;
        assert_eq!(framed_reply(&[]), None);
        assert_eq!(framed_reply(&[0]), None);
        assert_eq!(framed_reply(&[0, 3, 1, 2]), None);
        assert_eq!(framed_reply(&[0, 3, 1, 2, 3]), Some(&[1u8, 2, 3][..]));
        assert_eq!(framed_reply(&[0, 0]), Some(&[][..]));
    }

    #[test]
    fn only_lookups_the_vm_resolver_may_miss_wait_for_the_host() {
        let mut dns = Dns::default();
        assert_eq!(dns.timeout_ms(), -1, "nothing pending");
        dns.pending.push(pending());
        assert_eq!(dns.timeout_ms(), -1, "nothing to hand over");
        let mut p = pending();
        p.may_relay = true;
        dns.pending.push(p);
        let t = dns.timeout_ms();
        assert!((0..=1000).contains(&t), "{t}");
        // Past the wait it is tried through the host once, whatever comes of it.
        dns.pending[1].sent = Instant::now() - super::FALLBACK_AFTER;
        assert_eq!(dns.timeout_ms(), 0);
        dns.on_timer();
        assert!(!dns.pending[1].may_relay);
        assert_eq!(dns.timeout_ms(), -1);
    }

    #[test]
    fn the_hosts_reply_goes_back_to_whoever_asked() {
        use std::net::UdpSocket;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let std::net::SocketAddr::V4(addr) = client.local_addr().unwrap() else { panic!() };
        let mut dns = Dns::default();
        dns.servers.push(Server { id: super::server_id(), bridges: vec!["kiri1".into()], sock: udp_socket().unwrap(), container: None });
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the pair.
        assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0, fds.as_mut_ptr()) }, 0);
        // SAFETY: fresh descriptors from socketpair.
        let (ours, host) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut p = pending();
        p.sock = ours;
        p.server = dns.servers[0].id;
        p.client = super::sockaddr(addr.ip().octets(), addr.port());
        p.relayed = Some(Vec::new());
        let question = p.question.clone();
        dns.pending.push(p);
        let mut reply = vec![0, 1, 0x81, 0x80, 0, 1, 0, 0, 0, 0, 0, 0];
        reply.extend_from_slice(&question);
        let mut frame = (reply.len() as u16).to_be_bytes().to_vec();
        frame.extend_from_slice(&reply);
        let send = |b: &[u8]| {
            // SAFETY: writing a buffer we own to our end of the pair.
            assert_eq!(unsafe { libc::write(host.as_raw_fd(), b.as_ptr().cast(), b.len()) }, b.len() as isize)
        };
        // Half a reply waits for the rest.
        send(&frame[..5]);
        let fd = dns.pending[0].sock.as_raw_fd();
        dns.on_readable(fd, &[]);
        assert_eq!(dns.pending.len(), 1);
        send(&frame[5..]);
        dns.on_readable(fd, &[]);
        assert!(dns.pending.is_empty());
        let mut buf = [0u8; 512];
        let n = client.recv(&mut buf).unwrap();
        assert_eq!(&buf[..2], &[0, 7], "the client's own ID");
        assert_eq!(&buf[2..n], &reply[2..]);
    }

    /// A question cut off after QTYPE (no QCLASS) is not a question: one
    /// such 36-byte packet to 127.0.0.11 used to abort kiri-init and the VM.
    #[test]
    fn a_question_without_its_class_is_refused() {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in ["host", "docker", "internal"] {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.extend_from_slice(&[0, 0, 1]);
        assert_eq!(q.len(), 36);
        assert!(super::parse_question(&q).is_none(), "QCLASS missing");
        q.extend_from_slice(&[0]);
        assert!(super::parse_question(&q).is_none(), "half a QCLASS");
        q.extend_from_slice(&[1]);
        let (name, qtype, end) = super::parse_question(&q).expect("a whole question parses");
        assert_eq!((name.as_str(), qtype, end), ("host.docker.internal", 1, 38));
    }

    #[test]
    fn response_must_match_the_whole_question() {
        let p = pending();
        let mut response = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        response.extend_from_slice(&p.question);
        assert!(response_matches(&response, &p));

        let mut wrong_name = response.clone();
        wrong_name[12] = 3;
        wrong_name[13] = b'x';
        assert!(!response_matches(&wrong_name, &p));

        let mut wrong_type = response.clone();
        *wrong_type.last_mut().unwrap() = 6;
        assert!(!response_matches(&wrong_type, &p));

        response[2] = 0x01;
        assert!(!response_matches(&response, &p));
    }
}
