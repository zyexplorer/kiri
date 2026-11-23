//! Guest networking: the uplink (eth0, configured by DHCP from Apple's NAT),
//! container bridges with masquerade, and each container's veth.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use super::firewall;
use super::netlink::{self, Netlink};
use super::sys;

pub const UPLINK: &str = "eth0";
/// What `host.docker.internal` resolves to inside containers. Connections to
/// it are redirected to the agent and relayed to the Mac's localhost.
pub const HOST_ADDR: [u8; 4] = [198, 19, 248, 254];
/// Where the redirect lands: the agent's listener for host-bound connections.
pub const HOST_RELAY_PORT: u16 = 1026;
/// Container interfaces stay below this, so any uplink MTU fits.
const CONTAINER_MTU: u32 = 1500;

#[derive(Debug, Clone, Default)]
pub struct Lease {
    pub ip: [u8; 4],
    pub prefix: u8,
    pub gateway: [u8; 4],
    pub dns: Vec<[u8; 4]>,
}

pub fn ip_str(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
    s.parse::<std::net::Ipv4Addr>().ok().map(|a| a.octets())
}

/// A container's IPv6 side on an IPv6 network: its address and prefix, and
/// the gateway (the bridge's own address).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ip6 {
    pub ip: [u8; 16],
    pub prefix: u8,
    pub gateway: [u8; 16],
}

impl Ip6 {
    /// From a network's `ip6=` and `gw6=` settings; None on IPv4-only ones.
    pub fn from_opts(opts: kiri_proto::StrList<'_>) -> Option<Ip6> {
        use kiri_proto::net_opts as k;
        let (ip, prefix) = k::values(opts, k::IP6).next()?.split_once('/')?;
        let gateway = k::values(opts, k::GW6).next()?;
        Some(Ip6 {
            ip: ip.parse::<std::net::Ipv6Addr>().ok()?.octets(),
            prefix: prefix.parse().ok().filter(|p| *p <= 128)?,
            gateway: gateway.parse::<std::net::Ipv6Addr>().ok()?.octets(),
        })
    }

    /// The network's subnet, as the masquerade rule matches it.
    fn subnet(&self) -> [u8; 16] {
        let bits = u128::from_be_bytes(self.gateway);
        let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix as u32) };
        (bits & mask).to_be_bytes()
    }
}

/// How a container is plugged into one network, from its settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Plug {
    /// `--internal`: no default route.
    pub internal: bool,
    /// IPv6-only: no IPv4 address on the container or the bridge.
    pub no_ipv4: bool,
}

impl Plug {
    pub fn from_opts(opts: kiri_proto::StrList<'_>) -> Plug {
        use kiri_proto::net_opts as k;
        Plug { internal: k::values(opts, k::INTERNAL).next().is_some(), no_ipv4: k::values(opts, k::NO_IPV4).next().is_some() }
    }
}

pub fn ip6_str(a: [u8; 16]) -> String {
    std::net::Ipv6Addr::from(a).to_string()
}

/// The MAC a container gets on an address unless one is asked for, made
/// from that address as Docker does (`02:42` then the four bytes). A
/// container that takes over a freed address then takes over its MAC too,
/// so the host and the other containers, which may still hold the old
/// container's entry in their neighbour tables, reach the new one at once.
/// With a random MAC they would send to the old one until the entry expired,
/// several seconds, and connections through a published port failed.
pub fn mac_for(ip: [u8; 4]) -> [u8; 6] {
    [0x02, 0x42, ip[0], ip[1], ip[2], ip[3]]
}

/// `02:42:ac:11:00:02`.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut mac {
        *b = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// On a Linux host: the interface of the default route, already configured
/// by the host, gets the masquerade and the host.docker.internal redirect.
pub fn host_uplink() -> io::Result<()> {
    let routes = std::fs::read_to_string("/proc/net/route")?;
    // Iface Destination Gateway …: the default route has destination 0.
    let iface = routes
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() > 2 && f[1] == "00000000").then(|| f[0].to_owned())
        })
        .next()
        .ok_or_else(|| io::Error::other("no default route"))?;
    let index = netlink::index_of(&iface)?;
    sys::write_file("/proc/sys/net/ipv4/ip_forward", "1")?;
    // No host.docker.internal loopback redirect on a Linux host: it would
    // let containers reach the real machine's 127.0.0.1 services. host.docker
    // .internal maps to the bridge gateway instead (write_etc_files).
    let _ = HOST_UPLINK_INDEX.set(index);
    let uplink6 = std::fs::read_to_string("/proc/net/ipv6_route").ok().and_then(|r| default_route6_iface(&r));
    firewall::set_uplinks(iface.clone(), uplink6.unwrap_or(iface));
    through_firewall(firewall::Why::Start);
    install_nat(Nat { uplink: index, redirect: false, dnat: Vec::new(), gateway: [0; 4] })
}

/// The interface of the IPv6 default route in /proc/net/ipv6_route (dest
/// `::/0`), but not `lo`, which holds the unreachable default.
fn default_route6_iface(routes: &str) -> Option<String> {
    routes.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() == 10 && f[0].bytes().all(|b| b == b'0') && f[1] == "00" && f[9] != "lo").then(|| f[9].to_owned())
    })
}

/// Linux host: a socket that hears of every nf_tables change, so a firewall
/// started, reloaded or flushed after the engine (Docker's, ufw, firewalld,
/// nftables.service) is brought in line at once. None if the kernel refuses it.
pub fn firewall_events() -> Option<OwnedFd> {
    netlink::nftables_events().inspect_err(|e| eprintln!("kiri-init: not watching the host's firewall: {e}")).ok()
}

/// `firewall_events` heard of a change: reads it and brings the host's
/// firewall, and Kiri's own tables, in line.
pub fn firewall_changed(events: &OwnedFd) {
    netlink::drain(events);
    through_firewall(firewall::Why::Event);
}

/// `firewall::sync`, with Kiri's own tables built again as last built when
/// it asks: a flush of the whole ruleset (`nft flush ruleset`, as
/// nftables.service runs it) took them, and containers had no masquerade,
/// no published ports and no forward filter keeping the LAN off their
/// unpublished ports; or the forward chain's firewalld guard changed.
fn through_firewall(why: firewall::Why) {
    firewall::sync(why, |[v4, v6]| {
        // Built before (`refresh_nat` takes the lock itself).
        let built = NAT.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        if v4
            && built
            && let Err(e) = refresh_nat()
        {
            eprintln!("kiri-init: NAT: {e}");
        }
        let nat6 = NAT6.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if v6
            && !nat6.0.is_empty()
            && let Err(e) = netlink::install_nat6(&nat6.0, &nat6.1)
        {
            eprintln!("kiri-init: IPv6 NAT: {e}");
        }
    });
}

/// What Kiri's IPv6 table was last built from (`install_nat6`), to build it
/// again after a flush.
#[allow(clippy::type_complexity)]
static NAT6: std::sync::Mutex<(Vec<([u8; 16], u8)>, Vec<String>)> = std::sync::Mutex::new((Vec::new(), Vec::new()));

/// What Kiri's IPv4 table was last built from, so it can be rebuilt when a
/// bridge comes or goes (the forward filter has a rule per bridge).
struct Nat {
    uplink: i32,
    redirect: bool,
    dnat: Vec<netlink::Dnat>,
    gateway: [u8; 4],
}

static NAT: std::sync::Mutex<Option<Nat>> = std::sync::Mutex::new(None);
/// The kernel has no nft_fib_ipv4 (`fib daddr type local`): ports
/// published on every address are matched per address of this machine.
static NO_FIB4: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Builds Kiri's IPv4 table from `nat` and the bridges made so far, and
/// keeps `nat` for `refresh_nat`.
fn install_nat(nat: Nat) -> io::Result<()> {
    use std::sync::atomic::Ordering;
    let bridges = MADE_BRIDGES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let any_addr = nat.dnat.iter().any(|d| d.host_ip == [0; 4]);
    let install = |local: Option<&[[u8; 4]]>| netlink::install_nat_from(nat.uplink, HOST_ADDR, HOST_RELAY_PORT, nat.redirect, &nat.dnat, nat.gateway, &bridges, local);
    let r = if any_addr && NO_FIB4.load(Ordering::Relaxed) {
        install(Some(&local_addrs()))
    } else {
        match install(None) {
            Err(e) if any_addr && e.raw_os_error() == Some(libc::ENOENT) => {
                eprintln!("kiri-init: no nft_fib_ipv4 in this kernel; ports published on every address match this machine's addresses one by one");
                NO_FIB4.store(true, Ordering::Relaxed);
                install(Some(&local_addrs()))
            }
            r => r,
        }
    };
    if r.is_ok() {
        *NAT.lock().unwrap_or_else(|e| e.into_inner()) = Some(nat);
    }
    r
}

/// Rebuilds Kiri's IPv4 table as last built, for the bridges there are now.
fn refresh_nat() -> io::Result<()> {
    let nat = NAT.lock().unwrap_or_else(|e| e.into_inner()).take();
    match nat {
        Some(n) => {
            let keep = Nat { uplink: n.uplink, redirect: n.redirect, dnat: n.dnat.clone(), gateway: n.gateway };
            let r = install_nat(n);
            if r.is_err() {
                *NAT.lock().unwrap_or_else(|e| e.into_inner()) = Some(keep);
            }
            r
        }
        None => Ok(()),
    }
}

/// Whether one of `servers` is an address of this machine itself, as WSL's
/// DNS forwarder is (10.255.255.254, on `lo`). Containers on a bridge can't
/// count on such a resolver: under WSL a third of the queries from a
/// container's namespace go unanswered, while the host's own are fine. Their
/// queries then go through the agent's resolver, at the bridge's gateway
/// (container.rs embedded_address), which asks it from the host's side.
pub fn local_resolver(servers: &[[u8; 4]]) -> bool {
    !servers.is_empty() && local_resolver_among(servers, &local_addrs())
}

/// `local_resolver`, given this machine's addresses.
pub fn local_resolver_among(servers: &[[u8; 4]], local: &[[u8; 4]]) -> bool {
    servers.iter().any(|s| local.contains(s))
}

/// This machine's IPv4 addresses but loopback, what `fib daddr type local`
/// matches.
fn local_addrs() -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below; entries are read only while it lives.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return out;
    }
    let mut p = list;
    while !p.is_null() {
        // SAFETY: `p` is an entry of the list getifaddrs returned.
        let ifa = unsafe { &*p };
        if !ifa.ifa_addr.is_null() && i32::from(unsafe { (*ifa.ifa_addr).sa_family }) == libc::AF_INET {
            // SAFETY: an AF_INET address is a sockaddr_in.
            let a = unsafe { (*(ifa.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr }.to_ne_bytes();
            if a[0] != 127 && !out.contains(&a) {
                out.push(a);
            }
        }
        p = ifa.ifa_next;
    }
    // SAFETY: the list getifaddrs returned, freed once.
    unsafe { libc::freeifaddrs(list) };
    out
}

/// The Linux host's uplink interface, once `host_uplink` found it.
static HOST_UPLINK_INDEX: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

/// Linux host: makes these, and only these, the published ports the kernel
/// forwards (see `netlink::Dnat`).
pub fn publish(dnat: &[netlink::Dnat]) -> io::Result<()> {
    let index = *HOST_UPLINK_INDEX.get().ok_or_else(|| io::Error::other("no uplink"))?;
    install_nat(Nat { uplink: index, redirect: false, dnat: dnat.to_vec(), gateway: [0; 4] })
}

/// Brings the uplink up and returns a DHCP client for it, or None if the VM
/// has no network card.
pub fn uplink() -> io::Result<Option<Dhcp>> {
    let Ok(index) = netlink::index_of(UPLINK) else { return Ok(None) };
    let mut nl = Netlink::open(libc::NETLINK_ROUTE)?;
    netlink::set_up(&mut nl, index)?;
    sys::write_file("/proc/sys/net/ipv4/ip_forward", "1")?;
    // The tunnel drivers are built in, and each would add its idle fallback
    // device (tunl0, gre0, sit0, ip6tnl0...) to every container's network:
    // they stay in the VM's own, as on a machine that loads them as modules.
    let _ = sys::write_file("/proc/sys/net/core/fb_tunnels_only_for_init_net", "1");
    install_nat(Nat { uplink: index, redirect: true, dnat: Vec::new(), gateway: [0; 4] })?;
    let _ = VM_UPLINK_INDEX.set(index);
    Ok(Some(Dhcp::start(index)?))
}

/// The VM's uplink (eth0), once `uplink` brought it up.
static VM_UPLINK_INDEX: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

/// In the VM: makes these the published ports the kernel forwards from the
/// uplink, for connections the Mac makes from `gateway` (its address on the
/// NAT network). The table is rebuilt whole, so the host.docker.internal
/// redirect stays.
pub fn publish_vm(dnat: &[netlink::Dnat], gateway: [u8; 4]) -> io::Result<()> {
    let index = *VM_UPLINK_INDEX.get().ok_or_else(|| io::Error::other("no uplink"))?;
    install_nat(Nat { uplink: index, redirect: true, dnat: dnat.to_vec(), gateway })
}

enum DhcpState {
    Discovering,
    Requesting { offered: [u8; 4], server: [u8; 4] },
    Bound,
}

/// A small DHCP client run from the agent's poll loop: the agent must stay
/// single-threaded, and boot should never wait for a lease.
pub struct Dhcp {
    sock: OwnedFd,
    index: i32,
    mac: [u8; 6],
    xid: u32,
    state: DhcpState,
    next_send: Instant,
    attempt: u32,
    pub lease: Option<Lease>,
}

impl Dhcp {
    fn start(index: i32) -> io::Result<Dhcp> {
        // SAFETY: plain socket setup; the fd is owned immediately.
        let sock = unsafe {
            let fd = sys::check(libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) as _, "dhcp socket")?;
            OwnedFd::from_raw_fd(fd as i32)
        };
        let one: libc::c_int = 1;
        let set = |opt, val: &[u8]| {
            // SAFETY: setsockopt with a buffer of the stated length.
            unsafe { libc::setsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, opt, val.as_ptr().cast(), val.len() as u32) }
        };
        set(libc::SO_BROADCAST, &one.to_ne_bytes());
        set(libc::SO_REUSEADDR, &one.to_ne_bytes());
        set(libc::SO_BINDTODEVICE, b"eth0\0");
        let addr = sockaddr([0, 0, 0, 0], 68);
        // SAFETY: binding to a valid sockaddr_in.
        sys::check(unsafe { libc::bind(sock.as_raw_fd(), (&addr as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as u32) } as _, "dhcp bind")?;
        let mut d = Dhcp {
            mac: mac_of(UPLINK)?,
            sock,
            index,
            xid: random_u32(),
            state: DhcpState::Discovering,
            next_send: Instant::now(),
            attempt: 0,
            lease: None,
        };
        d.send()?;
        Ok(d)
    }

    pub fn fd(&self) -> i32 {
        self.sock.as_raw_fd()
    }

    /// Milliseconds until the next retransmission, for poll's timeout.
    pub fn timeout_ms(&self) -> i32 {
        if matches!(self.state, DhcpState::Bound) {
            return -1;
        }
        self.next_send.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32
    }

    pub fn on_timer(&mut self) -> io::Result<()> {
        if !matches!(self.state, DhcpState::Bound) && Instant::now() >= self.next_send {
            self.send()?;
        }
        Ok(())
    }

    fn send(&mut self) -> io::Result<()> {
        let mut p = vec![0u8; 240];
        p[0] = 1; // BOOTREQUEST
        p[1] = 1; // Ethernet
        p[2] = 6;
        p[4..8].copy_from_slice(&self.xid.to_be_bytes());
        p[10] = 0x80; // ask for broadcast replies: we have no address yet
        p[28..34].copy_from_slice(&self.mac);
        p[236..240].copy_from_slice(&[99, 130, 83, 99]);
        match self.state {
            DhcpState::Discovering => p.extend_from_slice(&[53, 1, 1]),
            DhcpState::Requesting { offered, server } => {
                p.extend_from_slice(&[53, 1, 3, 50, 4]);
                p.extend_from_slice(&offered);
                p.extend_from_slice(&[54, 4]);
                p.extend_from_slice(&server);
            }
            DhcpState::Bound => return Ok(()),
        }
        p.extend_from_slice(&[55, 3, 1, 3, 6, 255]);
        p.resize(300, 0);
        let to = sockaddr([255, 255, 255, 255], 67);
        // SAFETY: sending a buffer we own to a valid sockaddr_in.
        unsafe { libc::sendto(self.sock.as_raw_fd(), p.as_ptr().cast(), p.len(), 0, (&to as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as u32) };
        self.attempt += 1;
        self.next_send = Instant::now() + Duration::from_millis((250u64 << self.attempt.min(5)).min(4000));
        Ok(())
    }

    /// Handles a reply. Returns true once a lease has been configured.
    pub fn on_readable(&mut self) -> io::Result<bool> {
        let mut buf = [0u8; 1500];
        loop {
            // SAFETY: receiving into a buffer we own.
            let n = unsafe { libc::recv(self.sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n <= 0 {
                return Ok(false);
            }
            let p = &buf[..n as usize];
            if p.len() < 240 || p[0] != 2 || p[4..8] != self.xid.to_be_bytes() || p[236..240] != [99, 130, 83, 99] {
                continue;
            }
            let yiaddr: [u8; 4] = p[16..20].try_into().expect("4");
            let (mut kind, mut mask, mut router, mut server, mut dns) = (0u8, [255, 255, 255, 0], [0u8; 4], [0u8; 4], Vec::new());
            let mut i = 240;
            while i + 1 < p.len() && p[i] != 255 {
                if p[i] == 0 {
                    i += 1;
                    continue;
                }
                let (code, len) = (p[i], p[i + 1] as usize);
                let Some(v) = p.get(i + 2..i + 2 + len) else { break };
                match code {
                    53 if len == 1 => kind = v[0],
                    1 if len == 4 => mask = v.try_into().expect("4"),
                    3 if len >= 4 => router = v[..4].try_into().expect("4"),
                    54 if len == 4 => server = v.try_into().expect("4"),
                    6 => dns = v.chunks_exact(4).map(|c| c.try_into().expect("4")).collect(),
                    _ => {}
                }
                i += 2 + len;
            }
            match (&self.state, kind) {
                (DhcpState::Discovering, 2) => {
                    self.state = DhcpState::Requesting { offered: yiaddr, server };
                    self.attempt = 0;
                    self.send()?;
                }
                (DhcpState::Requesting { .. }, 5) => {
                    let lease = Lease { ip: yiaddr, prefix: u32::from_be_bytes(mask).count_ones() as u8, gateway: router, dns };
                    let mut nl = Netlink::open(libc::NETLINK_ROUTE)?;
                    netlink::add_address(&mut nl, self.index, lease.ip, lease.prefix)?;
                    netlink::add_default_route(&mut nl, self.index, lease.gateway)?;
                    self.state = DhcpState::Bound;
                    self.lease = Some(lease);
                    return Ok(true);
                }
                (DhcpState::Requesting { .. }, 6) => {
                    // NAK: start over.
                    self.state = DhcpState::Discovering;
                    self.attempt = 0;
                    self.send()?;
                }
                _ => {}
            }
        }
    }
}

fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    u32::from_ne_bytes(b) | 1
}

fn sockaddr(ip: [u8; 4], port: u16) -> libc::sockaddr_in {
    // SAFETY: sockaddr_in is plain data; all-zero is valid.
    let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    a.sin_family = libc::AF_INET as libc::sa_family_t;
    a.sin_port = port.to_be();
    a.sin_addr.s_addr = u32::from_ne_bytes(ip);
    a
}

fn mac_of(name: &str) -> io::Result<[u8; 6]> {
    // SAFETY: plain socket + SIOCGIFHWADDR on a zeroed ifreq.
    unsafe {
        let fd = sys::check(libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) as _, "socket")?;
        let sock = OwnedFd::from_raw_fd(fd as i32);
        let mut req: libc::ifreq = std::mem::zeroed();
        for (d, s) in req.ifr_name.iter_mut().zip(name.bytes()) {
            *d = s as libc::c_char;
        }
        sys::check(libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFHWADDR as libc::Ioctl, &mut req) as _, "hwaddr")?;
        let mut mac = [0u8; 6];
        for (d, s) in mac.iter_mut().zip(req.ifr_ifru.ifru_hwaddr.sa_data.iter()) {
            *d = *s as u8;
        }
        Ok(mac)
    }
}

/// The IPv6 gateway and prefix a bridge has, if its network has IPv6.
type Bridge6 = Option<([u8; 16], u8)>;

/// Bridges that exist so far: name → (index, gateway (none when
/// IPv6-only), prefix, IPv6).
#[derive(Default)]
pub struct Bridges {
    made: HashMap<String, (i32, Option<[u8; 4]>, u8, Bridge6)>,
}

impl Bridges {
    /// Creates the bridge (once) with its gateway address (and IPv6 one)
    /// and brings it up, with its rules in Kiri's tables. All or nothing: on
    /// an error the bridge is gone again and not recorded, so the next
    /// container tries afresh rather than starting with half the rules.
    pub fn ensure(&mut self, name: &str, gateway: Option<[u8; 4]>, prefix: u8, v6: Option<Ip6>) -> io::Result<i32> {
        let want6 = v6.map(|v| (v.gateway, v.prefix));
        match self.made.get(name) {
            Some(&(i, g, p, b6)) if (g, p, b6) == (gateway, prefix, want6) => return Ok(i),
            // The network was removed and its bridge name reused for one
            // with another subnet: start the bridge afresh.
            Some(_) => {
                self.remove(name);
            }
            None => {}
        }
        let r = self.bring_up(name, gateway, prefix, v6);
        if r.is_err() {
            if let Ok(mut nl) = Netlink::open(libc::NETLINK_ROUTE) {
                let _ = netlink::delete_link(&mut nl, name);
            }
            MADE_BRIDGES.lock().unwrap_or_else(|e| e.into_inner()).retain(|m| m != name);
            let had6 = matches!(self.made.remove(name), Some((.., Some(_))));
            let _ = refresh_nat();
            if had6 {
                let _ = self.update_v6(None);
            }
        }
        r
    }

    fn bring_up(&mut self, name: &str, gateway: Option<[u8; 4]>, prefix: u8, v6: Option<Ip6>) -> io::Result<i32> {
        let mut nl = Netlink::open(libc::NETLINK_ROUTE)?;
        netlink::create_bridge(&mut nl, name)?;
        let mut made = MADE_BRIDGES.lock().unwrap_or_else(|e| e.into_inner());
        if !made.iter().any(|m| m == name) {
            made.push(name.to_owned());
        }
        drop(made);
        let index = netlink::index_of(name)?;
        if let Some(g) = gateway {
            netlink::add_address(&mut nl, index, g, prefix)?;
        }
        if let Some(v) = v6 {
            netlink::add_address6(&mut nl, index, v.gateway, v.prefix)?;
        }
        netlink::set_up(&mut nl, index)?;
        if super::transport::is_host_process() {
            // Published ports on a Linux host are relayed on loopback, never
            // DNATed from 127.0.0.1, so the bridge needs no `route_localnet`;
            // an older engine set it on bridges that may still exist.
            let _ = sys::write_file(&format!("/proc/sys/net/ipv4/conf/{name}/route_localnet"), "0");
            // A firewall started after the engine, in iptables-legacy
            // (which makes no nf_tables event) too.
            through_firewall(firewall::Why::Bridge);
        }
        // The forward filter's rule for this bridge.
        refresh_nat()?;
        self.made.insert(name.to_owned(), (index, gateway, prefix, v6.map(|v| (v.gateway, v.prefix))));
        if let Some(v) = v6 {
            self.update_v6(Some(v))?;
        }
        Ok(index)
    }

    /// A removed network's bridge: deleting it takes its addresses and
    /// routes along; the IPv6 NAT rule for it goes too.
    pub fn remove(&mut self, name: &str) {
        if let Ok(mut nl) = Netlink::open(libc::NETLINK_ROUTE) {
            let _ = netlink::delete_link(&mut nl, name);
        }
        MADE_BRIDGES.lock().unwrap_or_else(|e| e.into_inner()).retain(|m| m != name);
        if let Err(e) = refresh_nat() {
            eprintln!("kiri-init: NAT: {e}");
        }
        if let Some((.., Some(_))) = self.made.remove(name) {
            if let Err(e) = self.update_v6(None) {
                eprintln!("kiri-init: IPv6 NAT: {e}");
            }
        }
    }

    /// Brings the IPv6 table in line with the IPv6 bridges: forwarding on
    /// with the first, the table rebuilt for each, and both undone with
    /// the last, so an engine without IPv6 networks has none of it.
    fn update_v6(&mut self, added: Option<Ip6>) -> io::Result<()> {
        let mut subnets: Vec<([u8; 16], u8)> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for (name, (.., b6)) in &self.made {
            if let Some((g, p)) = *b6 {
                subnets.push((Ip6 { ip: g, prefix: p, gateway: g }.subnet(), p));
                names.push(name.clone());
            }
        }
        if added.is_some() {
            ipv6_forwarding(true)?;
        }
        let r = netlink::install_nat6(&subnets, &names);
        if r.is_ok() {
            *NAT6.lock().unwrap_or_else(|e| e.into_inner()) = (subnets.clone(), names.clone());
        }
        if subnets.is_empty() {
            let _ = ipv6_forwarding(false);
        }
        r
    }
}

/// What `ipv6_forwarding` changed, to put back: the old `all.forwarding`
/// and the interfaces whose `accept_ra` went from 1 to 2.
static V6_SAVED: std::sync::Mutex<Option<(String, Vec<String>)>> = std::sync::Mutex::new(None);

/// Turns IPv6 forwarding on (once) or back to how it was. A router ignores
/// router advertisements unless `accept_ra` is 2, so interfaces that took
/// them (the VM's uplink, a Linux host's own) are set to 2 first and keep
/// their IPv6 addresses and default route.
fn ipv6_forwarding(on: bool) -> io::Result<()> {
    let mut saved = V6_SAVED.lock().unwrap_or_else(|e| e.into_inner());
    let conf = "/proc/sys/net/ipv6/conf";
    if on {
        if saved.is_some() {
            return Ok(());
        }
        let old = std::fs::read_to_string(format!("{conf}/all/forwarding")).unwrap_or_default().trim().to_owned();
        if old == "1" {
            *saved = Some((old, Vec::new()));
            return Ok(());
        }
        let mut raised = Vec::new();
        for entry in std::fs::read_dir(conf)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), "all" | "default" | "lo") || name.starts_with("kiri") || name.starts_with("kv") {
                continue;
            }
            let path = format!("{conf}/{name}/accept_ra");
            if std::fs::read_to_string(&path).is_ok_and(|v| v.trim() == "1") && sys::write_file(&path, "2").is_ok() {
                raised.push(name);
            }
        }
        sys::write_file(&format!("{conf}/all/forwarding"), "1")?;
        *saved = Some((old, raised));
    } else if let Some((old, raised)) = saved.take() {
        if old != "1" {
            let _ = sys::write_file(&format!("{conf}/all/forwarding"), if old.is_empty() { "0" } else { &old });
        }
        for name in raised {
            let _ = sys::write_file(&format!("{conf}/{name}/accept_ra"), "1");
        }
    }
    Ok(())
}

/// Every bridge this agent brought up, for `remove_bridges`.
static MADE_BRIDGES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Linux host, stopping: removes the bridges this agent brought up (only
/// those: another engine's may share the network namespace) and Kiri's
/// nftables table, so nothing of the engine is left in the host's network.
pub fn remove_host_network() {
    let names = std::mem::take(&mut *MADE_BRIDGES.lock().unwrap_or_else(|e| e.into_inner()));
    if let Ok(mut nl) = Netlink::open(libc::NETLINK_ROUTE) {
        for name in names {
            let _ = netlink::delete_link(&mut nl, &name);
        }
    }
    if let Err(e) = netlink::remove_nat() {
        eprintln!("kiri-init: removing the nftables table: {e}");
    }
    let _ = netlink::install_nat6(&[], &[]);
    let _ = ipv6_forwarding(false);
    through_firewall(firewall::Why::Stop);
}

/// Gives a freshly cloned container its network: a veth pair whose peer
/// appears inside the container's namespace as eth0 with `ip`, a default
/// route through the bridge, and loopback up.
#[allow(clippy::too_many_arguments)]
pub fn attach_container(
    bridges: &mut Bridges,
    bridge: &str,
    host_ifname: &str,
    pid: libc::pid_t,
    ip: [u8; 4],
    prefix: u8,
    gateway: [u8; 4],
    mac: Option<[u8; 6]>,
    v6: Option<Ip6>,
    plug: Plug,
) -> io::Result<()> {
    attach(bridges, bridge, host_ifname, pid, 0, ip, prefix, gateway, mac, v6, plug)
}

/// Plugs the container into `bridge` as `eth{n}`. Only `eth0` gets the
/// default route; more networks add their subnets, as with Docker. The
/// first network with IPv6 gives the IPv6 default route. An `internal`
/// network gives no default route at all, so traffic beyond it fails at
/// once (unreachable), as with Docker. On a network without IPv6 the
/// interface has IPv6 off (no link-local address), as Docker does. On an
/// IPv6-only network `ip` is only the name the container is kept by (its
/// MAC is made from it): neither the interface nor the bridge gets it.
#[allow(clippy::too_many_arguments)]
pub fn attach(bridges: &mut Bridges, bridge: &str, host_ifname: &str, pid: libc::pid_t, n: u32, ip: [u8; 4], prefix: u8, gateway: [u8; 4], mac: Option<[u8; 6]>, v6: Option<Ip6>, plug: Plug) -> io::Result<()> {
    let Plug { internal, no_ipv4 } = plug;
    let mac = Some(mac.unwrap_or_else(|| mac_for(ip)));
    let bridge_index = bridges.ensure(bridge, (!no_ipv4).then_some(gateway), prefix, v6)?;
    let netns = std::fs::File::open(format!("/proc/{pid}/ns/net"))?;
    let mut host = Netlink::open(libc::NETLINK_ROUTE)?;
    // A stopped container's veth may still hold this address and MAC.
    delete_retired(&mut host);
    let ifname = format!("eth{n}");
    netlink::create_veth(&mut host, host_ifname, bridge_index, &netns, CONTAINER_MTU, &ifname, mac)?;
    // By name: a new namespace may have tunl0, gre0 … before it.
    let index = netlink::index_in(&netns, &ifname)?;
    if v6.is_none() {
        // Before it is up, so it never gets a link-local address. A kernel
        // without IPv6 has no such file, and nothing to turn off.
        let _ = netlink::in_netns(&netns, || sys::write_file(&format!("/proc/sys/net/ipv6/conf/{ifname}/disable_ipv6"), "1"));
    }
    let mut inner = Netlink::open_in(&netns)?;
    netlink::set_up(&mut inner, 1)?; // lo
    if !no_ipv4 {
        netlink::add_address(&mut inner, index, ip, prefix)?;
    }
    if let Some(v) = v6 {
        netlink::add_address6(&mut inner, index, v.ip, v.prefix)?;
    }
    netlink::set_up(&mut inner, index)?;
    if n == 0 && !internal && !no_ipv4 {
        netlink::add_default_route(&mut inner, index, gateway)?;
    }
    if let Some(v) = v6.filter(|_| !internal) {
        netlink::add_default_route6(&mut inner, index, v.gateway)?;
    }
    Ok(())
}

/// Host-side veths of stopped containers, deleted by the next `attach`.
static RETIRED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Notes a stopped container's host-side veths for the next `attach` to
/// delete. Deleting one waits out RCU, 15-40 ms that every request after
/// the exit would queue behind on the agent's loop; and the agent stays
/// single-threaded, as clone3 without CLONE_VM needs. Left to the kernel
/// they went only as the namespace did, a second or more later: by then the
/// next container had the same address and MAC, the bridge sent its replies
/// to the old namespace, and that answered with resets (a download that
/// broke mid-TLS or stalled).
pub fn retire(veths: &[String]) {
    RETIRED.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(veths);
}

/// Deletes the veths `retire` noted. Gone already (the kernel took them
/// with their namespace) is fine.
fn delete_retired(nl: &mut Netlink) {
    for v in std::mem::take(&mut *RETIRED.lock().unwrap_or_else(|e| e.into_inner())) {
        let _ = netlink::delete_link(nl, &v);
    }
}

/// Unplugs a container's extra network by its host-side veth.
pub fn detach(host_ifname: &str) -> io::Result<()> {
    let mut host = Netlink::open(libc::NETLINK_ROUTE)?;
    netlink::delete_link(&mut host, host_ifname)
}

/// The LISTEN lines of a process's network namespace, IPv4 and IPv6, as the
/// kernel writes them; the host reads the ports out. Read from /proc, so the
/// container needs no tools of its own. Only listeners are kept: a busy
/// server's connections would not fit in a frame.
pub fn listening(pid: libc::pid_t) -> Vec<String> {
    let mut out = Vec::new();
    for file in ["tcp", "tcp6"] {
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/net/{file}")) else { continue };
        // The fourth field is the state; 0A is TCP_LISTEN.
        out.extend(text.lines().filter(|l| l.split_whitespace().nth(3) == Some("0A")).take(512).map(str::to_owned));
    }
    out
}

/// Routes `dst/prefix` through `via` in the VM, or removes that route: how
/// the host's relay reaches a Kubernetes node's pods and services.
pub fn route(dst: [u8; 4], prefix: u8, via: [u8; 4], add: bool) -> io::Result<()> {
    let mut host = Netlink::open(libc::NETLINK_ROUTE)?;
    netlink::set_route(&mut host, dst, prefix, via, add)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_settings_come_from_the_options() {
        let mut buf = Vec::new();
        kiri_proto::encode_net_attach(&mut buf, "c1", "eth1", "kiri2", [10, 5, 0, 2], 24, [10, 5, 0, 1], &["web"], &["ip6=fd00:1::2/64", "gw6=fd00:1::1"]);
        let Ok(kiri_proto::Msg::NetAttach { net, .. }) = kiri_proto::decode(&buf) else { panic!() };
        let v = Ip6::from_opts(net.opts).unwrap();
        assert_eq!(ip6_str(v.ip), "fd00:1::2");
        assert_eq!(ip6_str(v.gateway), "fd00:1::1");
        assert_eq!(v.prefix, 64);
        assert_eq!(ip6_str(v.subnet()), "fd00:1::");
        let mut buf = Vec::new();
        kiri_proto::encode_net_attach::<&str>(&mut buf, "c1", "eth1", "kiri2", [10, 5, 0, 2], 24, [10, 5, 0, 1], &[], &[]);
        let Ok(kiri_proto::Msg::NetAttach { net, .. }) = kiri_proto::decode(&buf) else { panic!() };
        assert_eq!(Ip6::from_opts(net.opts), None);
    }

    #[test]
    fn the_ipv6_uplink_is_the_default_routes_interface() {
        let routes = "\
fd00000000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001     eth0
00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000002 00000000 00000003    wlan0
00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo
";
        assert_eq!(default_route6_iface(routes).as_deref(), Some("wlan0"));
        // Only the unreachable default on lo: no IPv6 uplink.
        assert_eq!(default_route6_iface(&routes.lines().skip(2).collect::<Vec<_>>().join("\n")), None);
    }
}
