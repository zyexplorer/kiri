//! Just enough rtnetlink and nf_tables netlink to build Docker-style
//! networking without `ip` or `nft` binaries: bridges, veth pairs, addresses,
//! routes and one masquerade rule.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::sys;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const NLM_F_REPLACE: u16 = 0x100;
const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_FD: u16 = 28;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const RTA_DST: u16 = 1;
const RTA_GATEWAY: u16 = 5;
const RTA_OIF: u16 = 4;

/// One netlink message under construction.
pub struct Msg {
    buf: Vec<u8>,
    nests: Vec<usize>,
}

fn pad(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

impl Msg {
    pub fn new(ty: u16, flags: u16) -> Msg {
        let mut buf = vec![0u8; 16];
        buf[4..6].copy_from_slice(&ty.to_ne_bytes());
        buf[6..8].copy_from_slice(&(flags | NLM_F_REQUEST).to_ne_bytes());
        Msg { buf, nests: Vec::new() }
    }

    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        pad(&mut self.buf);
        self
    }

    pub fn attr(&mut self, ty: u16, data: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
        self.buf.extend_from_slice(&ty.to_ne_bytes());
        self.buf.extend_from_slice(data);
        pad(&mut self.buf);
        self
    }

    pub fn attr_str(&mut self, ty: u16, s: &str) -> &mut Self {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        self.attr(ty, &v)
    }

    pub fn attr_u32(&mut self, ty: u16, v: u32) -> &mut Self {
        self.attr(ty, &v.to_ne_bytes())
    }

    /// nf_tables attributes are big-endian.
    pub fn attr_be32(&mut self, ty: u16, v: u32) -> &mut Self {
        self.attr(ty, &v.to_be_bytes())
    }

    pub fn nest(&mut self, ty: u16) -> &mut Self {
        self.nests.push(self.buf.len());
        self.buf.extend_from_slice(&[0, 0]);
        self.buf.extend_from_slice(&(ty | 0x8000).to_ne_bytes()); // NLA_F_NESTED
        self
    }

    pub fn end(&mut self) -> &mut Self {
        let start = self.nests.pop().expect("nest() before end()");
        let len = (self.buf.len() - start) as u16;
        self.buf[start..start + 2].copy_from_slice(&len.to_ne_bytes());
        self
    }

    fn finish(&mut self, seq: u32) -> &[u8] {
        let len = self.buf.len() as u32;
        self.buf[0..4].copy_from_slice(&len.to_ne_bytes());
        self.buf[8..12].copy_from_slice(&seq.to_ne_bytes());
        &self.buf
    }
}

pub struct Netlink {
    fd: OwnedFd,
    seq: u32,
    buf: Vec<u8>,
}

impl Netlink {
    /// A socket in the calling thread's current network namespace.
    pub fn open(protocol: i32) -> io::Result<Netlink> {
        // SAFETY: plain socket creation; owned immediately.
        let fd = sys::check(unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, protocol) } as _, "netlink")?;
        // SAFETY: `fd` is a fresh descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        Ok(Netlink { fd, seq: 1, buf: vec![0u8; 16 * 1024] })
    }

    /// A route socket inside another network namespace: entered just long
    /// enough to create the socket, which stays bound to that namespace.
    pub fn open_in(netns: &std::fs::File) -> io::Result<Netlink> {
        let own = std::fs::File::open("/proc/self/ns/net")?;
        sys::setns(netns.as_raw_fd(), libc::CLONE_NEWNET)?;
        let nl = Netlink::open(libc::NETLINK_ROUTE);
        sys::setns(own.as_raw_fd(), libc::CLONE_NEWNET)?;
        nl
    }

    /// Sends messages (one batch) and waits for every acknowledgement.
    pub fn request(&mut self, msgs: &mut [Msg]) -> io::Result<()> {
        let mut out = Vec::new();
        let first = self.seq;
        for m in msgs.iter_mut() {
            out.extend_from_slice(m.finish(self.seq));
            self.seq += 1;
        }
        // SAFETY: sending a buffer we own on a socket we own (to the kernel).
        sys::check(unsafe { libc::send(self.fd.as_raw_fd(), out.as_ptr().cast(), out.len(), 0) } as _, "netlink send")?;
        let mut acked = 0;
        let want = msgs.iter().filter(|m| u16::from_ne_bytes([m.buf[6], m.buf[7]]) & NLM_F_ACK != 0).count();
        while acked < want {
            // SAFETY: receiving into a buffer we own.
            let n = sys::check(unsafe { libc::recv(self.fd.as_raw_fd(), self.buf.as_mut_ptr().cast(), self.buf.len(), 0) } as _, "netlink recv")? as usize;
            let mut off = 0;
            while off + 16 <= n {
                let len = u32::from_ne_bytes(self.buf[off..off + 4].try_into().expect("4")) as usize;
                let ty = u16::from_ne_bytes(self.buf[off + 4..off + 6].try_into().expect("2"));
                let seq = u32::from_ne_bytes(self.buf[off + 8..off + 12].try_into().expect("4"));
                if len < 16 || off + len > n {
                    break;
                }
                if ty == NLMSG_ERROR && seq >= first && seq < self.seq {
                    let code = i32::from_ne_bytes(self.buf[off + 16..off + 20].try_into().expect("4"));
                    acked += 1;
                    if code != 0 {
                        return Err(io::Error::from_raw_os_error(-code));
                    }
                } else if ty == NLMSG_DONE {
                    acked = want;
                }
                off += (len + 3) & !3;
            }
        }
        Ok(())
    }
}

/// `struct ifinfomsg`.
fn ifinfo(index: i32, flags: u32, change: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0] = libc::AF_UNSPEC as u8;
    b[4..8].copy_from_slice(&index.to_ne_bytes());
    b[8..12].copy_from_slice(&flags.to_ne_bytes());
    b[12..16].copy_from_slice(&change.to_ne_bytes());
    b
}

/// Creates bridge `name` with a MAC address of its own. Without one a Linux
/// bridge takes the lowest address among its ports, so the gateway's MAC
/// changed whenever a container joined or left: every other container's ARP
/// entry for it went stale, and their connections stalled, were reset or
/// broke mid-TLS until it was asked again. Docker gives its bridges an
/// address for the same reason. A bridge left by an earlier run without
/// one (a Linux host) gets one too.
pub fn create_bridge(nl: &mut Netlink, name: &str) -> io::Result<()> {
    match nl.request(&mut [new_bridge(name, bridge_mac(random_bytes()))]) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
            // NET_ADDR_SET (3): someone set it, so it no longer follows the ports.
            let kind = std::fs::read_to_string(format!("/sys/class/net/{name}/addr_assign_type")).unwrap_or_default();
            if kind.trim() == "3" {
                return Ok(());
            }
            let mut m = Msg::new(RTM_NEWLINK, NLM_F_ACK);
            m.raw(&ifinfo(index_of(name)?, 0, 0)).attr(IFLA_ADDRESS, &bridge_mac(random_bytes()));
            nl.request(&mut [m])
        }
        r => r,
    }
}

fn new_bridge(name: &str, mac: [u8; 6]) -> Msg {
    let mut m = Msg::new(RTM_NEWLINK, NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL);
    m.raw(&ifinfo(0, 0, 0)).attr_str(IFLA_IFNAME, name).attr(IFLA_ADDRESS, &mac);
    m.nest(IFLA_LINKINFO).attr_str(IFLA_INFO_KIND, "bridge").end();
    m
}

/// `b` as a locally administered unicast MAC address.
fn bridge_mac(mut b: [u8; 6]) -> [u8; 6] {
    b[0] = (b[0] & 0xfc) | 0x02;
    b
}

fn random_bytes() -> [u8; 6] {
    let mut b = [0u8; 6];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    b
}

/// Creates a veth pair: `host` stays here, attached to `bridge` and up; its
/// peer is created straight inside `netns` as `peer`. The kernel picks the
/// peer's interface index: a new namespace is not always only lo (with ipip
/// or gre loaded it has tunl0, gre0 … too), so it is looked up by name.
pub fn create_veth(nl: &mut Netlink, host: &str, bridge_index: i32, netns: &std::fs::File, mtu: u32, peer: &str, peer_mac: Option<[u8; 6]>) -> io::Result<()> {
    let mut m = Msg::new(RTM_NEWLINK, NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL);
    m.raw(&ifinfo(0, libc::IFF_UP as u32, libc::IFF_UP as u32)).attr_str(IFLA_IFNAME, host).attr_u32(IFLA_MASTER, bridge_index as u32);
    m.attr_u32(IFLA_MTU, mtu);
    m.nest(IFLA_LINKINFO).attr_str(IFLA_INFO_KIND, "veth");
    m.nest(IFLA_INFO_DATA).nest(VETH_INFO_PEER);
    m.raw(&ifinfo(0, 0, 0)).attr_str(IFLA_IFNAME, peer).attr_u32(IFLA_NET_NS_FD, netns.as_raw_fd() as u32).attr_u32(IFLA_MTU, mtu);
    if let Some(mac) = peer_mac {
        m.attr(IFLA_ADDRESS, &mac);
    }
    m.end().end().end();
    nl.request(&mut [m])
}

pub fn delete_link(nl: &mut Netlink, name: &str) -> io::Result<()> {
    let mut m = Msg::new(RTM_DELLINK, NLM_F_ACK);
    m.raw(&ifinfo(0, 0, 0)).attr_str(IFLA_IFNAME, name);
    nl.request(&mut [m])
}

pub fn set_up(nl: &mut Netlink, index: i32) -> io::Result<()> {
    let mut m = Msg::new(RTM_NEWLINK, NLM_F_ACK);
    m.raw(&ifinfo(index, libc::IFF_UP as u32, libc::IFF_UP as u32));
    nl.request(&mut [m])
}

pub fn add_address(nl: &mut Netlink, index: i32, addr: [u8; 4], prefix: u8) -> io::Result<()> {
    let mut m = Msg::new(RTM_NEWADDR, NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL);
    // struct ifaddrmsg: family, prefixlen, flags, scope, index.
    let mut ifa = [0u8; 8];
    ifa[0] = libc::AF_INET as u8;
    ifa[1] = prefix;
    ifa[4..8].copy_from_slice(&(index as u32).to_ne_bytes());
    m.raw(&ifa).attr(IFA_LOCAL, &addr).attr(IFA_ADDRESS, &addr);
    match nl.request(&mut [m]) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        r => r,
    }
}

/// An IPv6 address, usable at once: no duplicate address detection, which
/// would leave it tentative (not usable as a source) for about a second.
/// Kiri hands out every address on its networks, so none can be taken.
pub fn add_address6(nl: &mut Netlink, index: i32, addr: [u8; 16], prefix: u8) -> io::Result<()> {
    const IFA_F_NODAD: u8 = 0x02;
    let mut m = Msg::new(RTM_NEWADDR, NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL);
    let mut ifa = [0u8; 8];
    ifa[0] = libc::AF_INET6 as u8;
    ifa[1] = prefix;
    ifa[2] = IFA_F_NODAD;
    ifa[4..8].copy_from_slice(&(index as u32).to_ne_bytes());
    m.raw(&ifa).attr(IFA_LOCAL, &addr).attr(IFA_ADDRESS, &addr);
    match nl.request(&mut [m]) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        r => r,
    }
}

/// The IPv6 default route through `gateway` on `index`. The first network
/// with IPv6 a container joins keeps it: a second one is EEXIST, left be.
pub fn add_default_route6(nl: &mut Netlink, index: i32, gateway: [u8; 16]) -> io::Result<()> {
    let mut m = Msg::new(RTM_NEWROUTE, NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL);
    let mut rt = [0u8; 12];
    rt[0] = libc::AF_INET6 as u8;
    rt[4] = libc::RT_TABLE_MAIN;
    rt[5] = libc::RTPROT_BOOT;
    rt[6] = libc::RT_SCOPE_UNIVERSE;
    rt[7] = libc::RTN_UNICAST;
    m.raw(&rt).attr(RTA_GATEWAY, &gateway).attr_u32(RTA_OIF, index as u32);
    match nl.request(&mut [m]) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        r => r,
    }
}

pub fn add_default_route(nl: &mut Netlink, index: i32, gateway: [u8; 4]) -> io::Result<()> {
    let mut m = Msg::new(RTM_NEWROUTE, NLM_F_ACK | NLM_F_CREATE);
    // struct rtmsg: family, dst_len, src_len, tos, table, protocol, scope, type, flags.
    let mut rt = [0u8; 12];
    rt[0] = libc::AF_INET as u8;
    rt[4] = libc::RT_TABLE_MAIN;
    rt[5] = libc::RTPROT_BOOT;
    rt[6] = libc::RT_SCOPE_UNIVERSE;
    rt[7] = libc::RTN_UNICAST;
    m.raw(&rt).attr(RTA_GATEWAY, &gateway).attr_u32(RTA_OIF, index as u32);
    match nl.request(&mut [m]) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        r => r,
    }
}

/// Adds (or replaces) the route `dst/prefix via gateway`, or removes it.
/// Removing one that isn't there is fine.
pub fn set_route(nl: &mut Netlink, dst: [u8; 4], prefix: u8, gateway: [u8; 4], add: bool) -> io::Result<()> {
    let mut m = if add { Msg::new(RTM_NEWROUTE, NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE) } else { Msg::new(RTM_DELROUTE, NLM_F_ACK) };
    let mut rt = [0u8; 12];
    rt[0] = libc::AF_INET as u8;
    rt[1] = prefix;
    rt[4] = libc::RT_TABLE_MAIN;
    rt[5] = libc::RTPROT_BOOT;
    rt[6] = libc::RT_SCOPE_UNIVERSE;
    rt[7] = libc::RTN_UNICAST;
    m.raw(&rt).attr(RTA_DST, &dst);
    if add {
        m.attr(RTA_GATEWAY, &gateway);
    }
    match nl.request(&mut [m]) {
        Err(e) if !add && e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        r => r,
    }
}

/// `index_of` inside another network namespace.
pub fn index_in(netns: &std::fs::File, name: &str) -> io::Result<i32> {
    in_netns(netns, || index_of(name))
}

/// Runs `f` with this thread in `netns`, then back in its own.
pub fn in_netns<T>(netns: &std::fs::File, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let own = std::fs::File::open("/proc/self/ns/net")?;
    sys::setns(netns.as_raw_fd(), libc::CLONE_NEWNET)?;
    let r = f();
    sys::setns(own.as_raw_fd(), libc::CLONE_NEWNET)?;
    r
}

pub fn index_of(name: &str) -> io::Result<i32> {
    let c = std::ffi::CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid NUL-terminated name.
    let i = unsafe { libc::if_nametoindex(c.as_ptr()) };
    if i == 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("no interface {name}")));
    }
    Ok(i as i32)
}

// nf_tables, through NETLINK_NETFILTER.
const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFNL_MSG_BATCH_BEGIN: u16 = 0x10;
const NFNL_MSG_BATCH_END: u16 = 0x11;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_DELTABLE: u16 = 2;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_NEWRULE: u16 = 6;

/// The start of a batch that replaces Kiri's table of `family`: add it
/// (nothing happens when it is there already), then delete it, so the rest
/// of the batch makes it again from nothing. One batch keeps the old table
/// if anything in it is refused. DESTROYTABLE would do it in one message,
/// but only from Linux 6.3, and older kernels (WSL 2's 5.15, Ubuntu 22.04)
/// refuse a whole batch with a message they don't know.
fn clear_table(family: u8) -> [Msg; 2] {
    let mut add = nft_in(family, NFT_MSG_NEWTABLE, NLM_F_ACK | NLM_F_CREATE);
    add.attr_str(1, "kiri");
    let mut delete = nft_in(family, NFT_MSG_DELTABLE, NLM_F_ACK);
    delete.attr_str(1, "kiri");
    [add, delete]
}

fn nfgen(family: u8, res_id: u16) -> [u8; 4] {
    let mut b = [0u8; 4];
    b[0] = family;
    b[2..4].copy_from_slice(&res_id.to_be_bytes());
    b
}

fn nft(msg: u16, flags: u16) -> Msg {
    nft_in(libc::NFPROTO_IPV4 as u8, msg, flags)
}

/// An nf_tables message for a table of `family` (NFPROTO_IPV4 or _IPV6).
fn nft_in(family: u8, msg: u16, flags: u16) -> Msg {
    let mut m = Msg::new((NFNL_SUBSYS_NFTABLES << 8) | msg, flags);
    m.raw(&nfgen(family, 0));
    m
}

/// A nat chain on `hook` (NF_INET_*) at `priority`.
fn nat_chain(name: &str, hook: u32, priority: i32) -> Msg {
    let mut chain = nft(NFT_MSG_NEWCHAIN, NLM_F_ACK | NLM_F_CREATE);
    chain.attr_str(1, "kiri").attr_str(3, name);
    chain.nest(4).attr_be32(1, hook).attr_be32(2, priority as u32).end();
    chain.attr_be32(5, 1).attr_str(7, "nat");
    chain
}

/// Kiri's container bridges are kiri0, kiri1, … (the daemon names them).
const BRIDGE_PREFIX: &[u8] = b"kiri";

/// `ip daddr <addr> tcp redirect to :<port>`: connections to `addr` (any
/// port) go to a local listener, which can read the original port back.
/// With `from_bridges`, only packets arriving on a Kiri bridge match
/// (`iifname "kiri*"`): the redirect leads to the host's loopback, which
/// nothing on the uplink (the LAN, on a Linux host) may reach.
fn redirect_rule(chain: &str, addr: [u8; 4], port: u16, from_bridges: bool) -> Msg {
    let mut rule = nft(NFT_MSG_NEWRULE, NLM_F_ACK | NLM_F_CREATE | 0x800);
    rule.attr_str(1, "kiri").attr_str(2, chain);
    rule.nest(4);
    if from_bridges {
        // meta iifname => reg 1 (NFT_META_IIFNAME is 6); cmp eq on its first
        // bytes only, a prefix match as nft compiles `iifname "kiri*"`.
        rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 6).end().end();
        rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, BRIDGE_PREFIX).end().end().end();
    }
    // meta l4proto => reg 1; cmp eq tcp
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 16).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &[libc::IPPROTO_TCP as u8]).end().end().end();
    // payload network header + 16 (daddr), 4 bytes => reg 1; cmp eq addr
    rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 16).attr_be32(4, 4).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &addr).end().end().end();
    // immediate reg 1 = port; redir to it
    rule.nest(1).attr_str(1, "immediate").nest(2).attr_be32(1, 1).nest(2).attr(1, &port.to_be_bytes()).end().end().end();
    rule.nest(1).attr_str(1, "redir").nest(2).attr_be32(1, 1).end().end();
    rule.end();
    rule
}

/// `iifname "kiri*" ip daddr <addr> udp dport 53 redirect to :53`: DNS
/// sent to the host's address goes to the embedded DNS on the receiving
/// bridge's gateway. A kind node rewrites its resolv.conf from 127.0.0.11 to
/// host.docker.internal's address for its pods, expecting the engine's DNS
/// there, as Docker Desktop has it.
fn dns_redirect_rule(addr: [u8; 4]) -> Msg {
    let mut rule = nft(NFT_MSG_NEWRULE, NLM_F_ACK | NLM_F_CREATE | 0x800);
    rule.attr_str(1, "kiri").attr_str(2, "prerouting");
    rule.nest(4);
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 6).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, BRIDGE_PREFIX).end().end().end();
    // meta l4proto => reg 1; cmp eq udp
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 16).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &[libc::IPPROTO_UDP as u8]).end().end().end();
    rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 16).attr_be32(4, 4).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &addr).end().end().end();
    // payload transport header + 2 (dport), 2 bytes => reg 1; cmp eq 53
    rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 2).attr_be32(3, 2).attr_be32(4, 2).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &53u16.to_be_bytes()).end().end().end();
    rule.nest(1).attr_str(1, "immediate").nest(2).attr_be32(1, 1).nest(2).attr(1, &53u16.to_be_bytes()).end().end().end();
    rule.nest(1).attr_str(1, "redir").nest(2).attr_be32(1, 1).end().end();
    rule.end();
    rule
}

/// A published port forwarded in the kernel. `host_ip` 0.0.0.0 means any
/// address of this machine but loopback (`fib daddr type local`, as Docker
/// matches it, and `ip daddr != 127.0.0.0/8`): the host's own loopback
/// connections are relayed by the daemon, which reaches the container
/// itself, so no bridge needs `route_localnet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dnat {
    pub udp: bool,
    pub host_ip: [u8; 4],
    pub host_port: u16,
    pub to_ip: [u8; 4],
    pub to_port: u16,
}

/// `[ip daddr <host_ip> | fib daddr type local ip daddr != 127.0.0.0/8]
/// <tcp|udp> dport <host_port> dnat to <to_ip>:<to_port>`.
fn dnat_rule(chain: &str, d: &Dnat) -> Msg {
    dnat_rule_from(chain, d, None)
}

/// `dnat_rule`, matching only packets that arrive on interface `from.0`
/// from address `from.1` (`iif <uplink> ip saddr <gateway> ...`).
fn dnat_rule_from(chain: &str, d: &Dnat, from: Option<(i32, [u8; 4])>) -> Msg {
    let mut rule = nft(NFT_MSG_NEWRULE, NLM_F_ACK | NLM_F_CREATE | 0x800);
    rule.attr_str(1, "kiri").attr_str(2, chain);
    rule.nest(4);
    if let Some((iif, saddr)) = from {
        // meta iif (NFT_META_IIF is 4) => reg 1; cmp eq the interface index
        rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 4).end().end();
        rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &(iif as u32).to_ne_bytes()).end().end().end();
        // payload network header + 12 (saddr), 4 bytes => reg 1; cmp eq saddr
        rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 12).attr_be32(4, 4).end().end();
        rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &saddr).end().end().end();
    }
    if d.host_ip == [0; 4] {
        // fib daddr type => reg 1 (NFTA_FIB_DREG, RESULT = ADDRTYPE 3, FLAGS = DADDR 2); cmp eq RTN_LOCAL
        rule.nest(1).attr_str(1, "fib").nest(2).attr_be32(1, 1).attr_be32(2, 3).attr_be32(3, 2).end().end();
        rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &2u32.to_ne_bytes()).end().end().end();
        // payload daddr => reg 1; reg 1 &= 255.0.0.0; cmp neq 127.0.0.0. Without
        // it a packet for 127.0.0.1 is rewritten before routing: one arriving
        // from the LAN would reach the container instead of being dropped.
        rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 16).attr_be32(4, 4).end().end();
        rule.nest(1).attr_str(1, "bitwise").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 4);
        rule.nest(4).attr(1, &[255, 0, 0, 0]).end().nest(5).attr(1, &[0; 4]).end().end().end();
        rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 1).nest(3).attr(1, &[127, 0, 0, 0]).end().end().end();
    } else {
        rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 16).attr_be32(4, 4).end().end();
        rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &d.host_ip).end().end().end();
    }
    let proto = if d.udp { libc::IPPROTO_UDP } else { libc::IPPROTO_TCP } as u8;
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 16).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &[proto]).end().end().end();
    // payload transport header + 2 (dport), 2 bytes => reg 1; cmp eq host_port
    rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 2).attr_be32(3, 2).attr_be32(4, 2).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &d.host_port.to_be_bytes()).end().end().end();
    // immediate reg 1 = to_ip, reg 2 = to_port; nat dnat ip addr reg 1 proto reg 2
    rule.nest(1).attr_str(1, "immediate").nest(2).attr_be32(1, 1).nest(2).attr(1, &d.to_ip).end().end().end();
    rule.nest(1).attr_str(1, "immediate").nest(2).attr_be32(1, 2).nest(2).attr(1, &d.to_port.to_be_bytes()).end().end().end();
    rule.nest(1).attr_str(1, "nat").nest(2).attr_be32(1, 1).attr_be32(2, libc::AF_INET as u32).attr_be32(3, 1).attr_be32(5, 2).attr_be32(7, 2).end().end();
    rule.end();
    rule
}

/// `iif <uplink> ip saddr <gateway> oifname "kiri*" masquerade random-fully`:
/// in the VM, the Mac's connections to published ports reach the container
/// from the bridge's own address on a fresh port: the container sees the
/// bridge gateway, as it did when the agent relayed and as Docker's proxy
/// shows, and a Mac source port reused within the container's 60 s
/// TIME_WAIT never meets the old socket.
fn published_masq_rule(uplink_index: i32, gateway: [u8; 4]) -> Msg {
    let mut rule = nft(NFT_MSG_NEWRULE, NLM_F_ACK | NLM_F_CREATE | 0x800);
    rule.attr_str(1, "kiri").attr_str(2, "postrouting");
    rule.nest(4);
    // meta iif (NFT_META_IIF is 4) => reg 1; cmp eq the uplink
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 4).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &(uplink_index as u32).to_ne_bytes()).end().end().end();
    rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 12).attr_be32(4, 4).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &gateway).end().end().end();
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 7).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, BRIDGE_PREFIX).end().end().end();
    // masq, NFTA_MASQ_FLAGS = NF_NAT_RANGE_PROTO_RANDOM_FULLY (16)
    rule.nest(1).attr_str(1, "masq").nest(2).attr_be32(1, 16).end().end();
    rule.end();
    rule
}

/// Bits of `ct state` (NF_CT_STATE_BIT) and `ct status` (IPS_DST_NAT).
const CT_ESTABLISHED: u32 = 1 << 1;
const CT_RELATED: u32 = 1 << 2;
const CT_STATUS_DNAT: u32 = 1 << 5;

/// The forward filter, so nothing but the containers' own traffic, replies
/// and published ports reaches a container through the kernel:
///
/// ```text
/// chain forward { type filter hook forward priority 0; policy accept;
///   oifname != "kiri*" accept            # not to a container: not ours
///   ct state established,related accept
///   ct status dnat accept                # a published port
///   iifname "kiri1" oifname "kiri1" accept   # same network, one per bridge
///   drop
/// }
/// ```
///
/// Without it, anything that can route to the engine (another VM on the
/// NAT network, a LAN neighbour of a Linux host) reached every container
/// port, published or not, with ip_forward on. Only traffic headed into a
/// Kiri bridge is judged: a Linux host's other forwarding stays as it was.
///
/// Containers on different networks do not reach each other, as Docker's
/// DOCKER-ISOLATION chains keep them apart: a packet from one bridge is
/// let through only when it leaves by that same bridge. nf_tables cannot
/// compare `iifname` with `oifname` directly, so there is one rule per
/// bridge and the table is rebuilt when a bridge comes or goes. (It used
/// `fib daddr . iif oif`, which needs nft_fib_ipv4/nft_fib_ipv6: WSL2's
/// kernel has no nft_fib_ipv6, and the IPv6 table was refused there.)
/// Traffic within a bridge is switched and never reaches this hook unless
/// br_netfilter is loaded, in which case iif and oif are both that bridge
/// and it passes. A container on two networks talks to each over its own
/// interface there; a published port on another network is still reached
/// through the host's address (ct status dnat), as with Docker.
fn forward_filter(bridges: &[String]) -> Vec<Msg> {
    forward_filter_in(libc::NFPROTO_IPV4 as u8, bridges)
}

/// `forward_filter` for a table of `family`: the same rules hold for IPv6.
fn forward_filter_in(family: u8, bridges: &[String]) -> Vec<Msg> {
    let nft = |msg, flags| nft_in(family, msg, flags);
    let mut chain = nft(NFT_MSG_NEWCHAIN, NLM_F_ACK | NLM_F_CREATE);
    chain.attr_str(1, "kiri").attr_str(3, "forward");
    chain.nest(4).attr_be32(1, 2).attr_be32(2, 0).end(); // NF_INET_FORWARD, priority 0
    chain.attr_be32(5, 1).attr_str(7, "filter");
    let rule = |build: &dyn Fn(&mut Msg), verdict: u32| {
        let mut rule = nft(NFT_MSG_NEWRULE, NLM_F_ACK | NLM_F_CREATE | 0x800);
        rule.attr_str(1, "kiri").attr_str(2, "forward");
        rule.nest(4);
        build(&mut rule);
        // immediate verdict: dreg 0 (NFT_REG_VERDICT), NFTA_DATA_VERDICT { code }
        rule.nest(1).attr_str(1, "immediate").nest(2).attr_be32(1, 0).nest(2).nest(2).attr_be32(1, verdict).end().end().end().end();
        rule.end();
        rule
    };
    // meta iifname/oifname (6/7) => reg 1; cmp `op` on the "kiri" prefix
    let ifname = |key: u32, op: u32| {
        move |r: &mut Msg| {
            r.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, key).end().end();
            r.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, op).nest(3).attr(1, BRIDGE_PREFIX).end().end().end();
        }
    };
    // ct <key> => reg 1; reg 1 &= mask; cmp neq 0
    let ct_any = |key: u32, mask: u32| {
        move |r: &mut Msg| {
            r.nest(1).attr_str(1, "ct").nest(2).attr_be32(1, 1).attr_be32(2, key).end().end();
            r.nest(1).attr_str(1, "bitwise").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 4);
            r.nest(4).attr(1, &mask.to_ne_bytes()).end().nest(5).attr(1, &0u32.to_ne_bytes()).end().end().end();
            r.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 1).nest(3).attr(1, &0u32.to_ne_bytes()).end().end().end();
        }
    };
    // meta iifname, oifname => reg 1; cmp eq the whole name (IFNAMSIZ, NUL-padded)
    let same_bridge = |name: &str| {
        let mut full = [0u8; 16];
        let n = name.len().min(15);
        full[..n].copy_from_slice(&name.as_bytes()[..n]);
        move |r: &mut Msg| {
            for key in [6, 7] {
                r.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, key).end().end();
                r.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &full).end().end().end();
            }
        }
    };
    let (accept, drop) = (libc::NF_ACCEPT as u32, libc::NF_DROP as u32);
    let mut msgs = vec![
        chain,
        rule(&ifname(7, 1), accept),                           // NFT_CMP_NEQ
        rule(&ct_any(0, CT_ESTABLISHED | CT_RELATED), accept), // NFT_CT_STATE
        rule(&ct_any(2, CT_STATUS_DNAT), accept),              // NFT_CT_STATUS
    ];
    msgs.extend(bridges.iter().map(|b| rule(&same_bridge(b), accept)));
    msgs.push(rule(&|_| {}, drop));
    msgs
}

/// The input filter that keeps containers off the host's loopback:
///
/// ```text
/// chain input { type filter hook input priority 0; policy accept;
///   iifname "kiri*" ip daddr 127.0.0.0/8 ct state & (established|related) == 0
///     ct status & dnat == 0 drop
/// }
/// ```
///
/// With `route_localnet` on a bridge, the kernel would take a packet a
/// container sends to 127.0.0.1 (raw socket, gateway MAC) as local: any
/// service bound to the host's loopback answered (the CVE-2020-8558 hole,
/// closed as kube-proxy does). Kiri no longer sets it (the daemon relays
/// the host's loopback connections to published ports), but something
/// else on a Linux host may (kube-proxy sets `conf.all.route_localnet`),
/// so the guard stays. Replies to established connections still pass.
/// Harmless in the VM, which never sets `route_localnet`.
fn localnet_guard() -> Vec<Msg> {
    let mut chain = nft(NFT_MSG_NEWCHAIN, NLM_F_ACK | NLM_F_CREATE);
    chain.attr_str(1, "kiri").attr_str(3, "input");
    chain.nest(4).attr_be32(1, 1).attr_be32(2, 0).end(); // NF_INET_LOCAL_IN, priority 0
    chain.attr_be32(5, 1).attr_str(7, "filter");
    let mut rule = nft(NFT_MSG_NEWRULE, NLM_F_ACK | NLM_F_CREATE | 0x800);
    rule.attr_str(1, "kiri").attr_str(2, "input");
    rule.nest(4);
    // meta iifname => reg 1; cmp eq "kiri" prefix
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 6).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, BRIDGE_PREFIX).end().end().end();
    // payload daddr => reg 1; reg 1 &= 255.0.0.0; cmp eq 127.0.0.0
    rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 16).attr_be32(4, 4).end().end();
    rule.nest(1).attr_str(1, "bitwise").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 4);
    rule.nest(4).attr(1, &[255, 0, 0, 0]).end().nest(5).attr(1, &[0; 4]).end().end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &[127, 0, 0, 0]).end().end().end();
    // ct state & (established|related) == 0; ct status & dnat == 0
    for (key, mask) in [(0, CT_ESTABLISHED | CT_RELATED), (2, CT_STATUS_DNAT)] {
        rule.nest(1).attr_str(1, "ct").nest(2).attr_be32(1, 1).attr_be32(2, key).end().end();
        rule.nest(1).attr_str(1, "bitwise").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 4);
        rule.nest(4).attr(1, &mask.to_ne_bytes()).end().nest(5).attr(1, &0u32.to_ne_bytes()).end().end().end();
        rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &0u32.to_ne_bytes()).end().end().end();
    }
    rule.nest(1).attr_str(1, "immediate").nest(2).attr_be32(1, 0).nest(2).nest(2).attr_be32(1, libc::NF_DROP as u32).end().end().end().end();
    rule.end();
    vec![chain, rule]
}

/// In the VM, the host.docker.internal listener (`port`, on 0.0.0.0) takes
/// only connections the prerouting/output redirect sent it:
///
/// ```text
/// chain input { …
///   tcp dport <port> ct status & dnat == 0 drop
/// }
/// ```
///
/// It must listen on every address, since `redirect` delivers to the
/// receiving bridge's own address (or 127.0.0.1 from the VM itself). The
/// agent already closes a connection whose SO_ORIGINAL_DST is not the host
/// address, so a direct one (another VM on the NAT network dialling the
/// VM's address, a container dialling its gateway) never reached the Mac;
/// now it is not even accepted. Redirected connections are DNATed and pass.
fn host_relay_guard(port: u16) -> Msg {
    let mut rule = nft(NFT_MSG_NEWRULE, NLM_F_ACK | NLM_F_CREATE | 0x800);
    rule.attr_str(1, "kiri").attr_str(2, "input");
    rule.nest(4);
    // meta l4proto => reg 1; cmp eq tcp
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 16).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &[libc::IPPROTO_TCP as u8]).end().end().end();
    // payload transport header + 2 (dport), 2 bytes => reg 1; cmp eq port
    rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 2).attr_be32(3, 2).attr_be32(4, 2).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &port.to_be_bytes()).end().end().end();
    // ct status & dnat == 0
    rule.nest(1).attr_str(1, "ct").nest(2).attr_be32(1, 1).attr_be32(2, 2).end().end();
    rule.nest(1).attr_str(1, "bitwise").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 4);
    rule.nest(4).attr(1, &CT_STATUS_DNAT.to_ne_bytes()).end().nest(5).attr(1, &0u32.to_ne_bytes()).end().end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &0u32.to_ne_bytes()).end().end().end();
    rule.nest(1).attr_str(1, "immediate").nest(2).attr_be32(1, 0).nest(2).nest(2).attr_be32(1, libc::NF_DROP as u32).end().end().end().end();
    rule.end();
    rule
}

/// A published port's DNAT rules: one, or with `local` (a kernel without
/// nft_fib_ipv4, so no `fib daddr type local`) one per address of this
/// machine for a port published on every address.
fn dnat_rules(chain: &str, d: &Dnat, local: Option<&[[u8; 4]]>) -> Vec<Msg> {
    match local {
        Some(addrs) if d.host_ip == [0; 4] => addrs.iter().map(|a| dnat_rule(chain, &Dnat { host_ip: *a, ..*d })).collect(),
        _ => vec![dnat_rule(chain, d)],
    }
}

/// `ip6 saddr <subnet>/<prefix> oifname != "kiri*" masquerade`: an IPv6
/// network's traffic leaving the bridges gets the outgoing interface's
/// address, as Docker's MASQUERADE rule per network does (NAT66). Matched by
/// source, as Docker does: an input interface is not known in postrouting.
fn masq6_rule(subnet: [u8; 16], prefix: u8) -> Msg {
    let mut rule = nft_in(libc::NFPROTO_IPV6 as u8, NFT_MSG_NEWRULE, NLM_F_ACK | NLM_F_CREATE | 0x800);
    rule.attr_str(1, "kiri").attr_str(2, "postrouting");
    rule.nest(4);
    let mut mask = [0u8; 16];
    for (i, b) in mask.iter_mut().enumerate() {
        let bits = (prefix as usize).saturating_sub(i * 8).min(8);
        *b = if bits == 0 { 0 } else { 0xffu8 << (8 - bits) };
    }
    // payload network header + 8 (saddr), 16 bytes => reg 1; reg 1 &= mask; cmp eq subnet
    rule.nest(1).attr_str(1, "payload").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 8).attr_be32(4, 16).end().end();
    rule.nest(1).attr_str(1, "bitwise").nest(2).attr_be32(1, 1).attr_be32(2, 1).attr_be32(3, 16);
    rule.nest(4).attr(1, &mask).end().nest(5).attr(1, &[0; 16]).end().end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0).nest(3).attr(1, &subnet).end().end().end();
    // meta oifname => reg 1; cmp neq the "kiri" prefix
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 7).end().end();
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 1).nest(3).attr(1, BRIDGE_PREFIX).end().end().end();
    rule.nest(1).attr_str(1, "masq").end();
    rule.end();
    rule
}

/// Kiri's IPv6 table, for the IPv6 networks `subnets`, rebuilt whole (and
/// removed with the last one, so an engine with none has no IPv6 rules):
///
/// ```text
/// table ip6 kiri {
///   chain postrouting { type nat hook postrouting priority 100;
///     ip6 saddr <subnet> oifname != "kiri*" masquerade   # one per network
///   }
///   chain forward { … }   (the IPv4 table's `forward_filter`)
/// }
/// ```
pub fn install_nat6(subnets: &[([u8; 16], u8)], bridges: &[String]) -> io::Result<()> {
    let v6 = libc::NFPROTO_IPV6 as u8;
    let mut nl = Netlink::open(libc::NETLINK_NETFILTER)?;
    let mut begin = Msg::new(NFNL_MSG_BATCH_BEGIN, 0);
    begin.raw(&nfgen(libc::AF_UNSPEC as u8, NFNL_SUBSYS_NFTABLES));
    let [add, delete] = clear_table(v6);
    let mut end = Msg::new(NFNL_MSG_BATCH_END, 0);
    end.raw(&nfgen(libc::AF_UNSPEC as u8, NFNL_SUBSYS_NFTABLES));
    let mut msgs = vec![begin, add, delete];
    if !subnets.is_empty() {
        let mut table = nft_in(v6, NFT_MSG_NEWTABLE, NLM_F_ACK | NLM_F_CREATE);
        table.attr_str(1, "kiri");
        let mut chain = nft_in(v6, NFT_MSG_NEWCHAIN, NLM_F_ACK | NLM_F_CREATE);
        chain.attr_str(1, "kiri").attr_str(3, "postrouting");
        chain.nest(4).attr_be32(1, 4).attr_be32(2, 100).end();
        chain.attr_be32(5, 1).attr_str(7, "nat");
        msgs.push(table);
        msgs.push(chain);
        msgs.extend(subnets.iter().map(|(s, p)| masq6_rule(*s, *p)));
        msgs.extend(forward_filter_in(v6, bridges));
    }
    msgs.push(end);
    nl.request(&mut msgs)
}

/// Removes Kiri's table (a Linux host's engine stopping), if there is one.
pub fn remove_nat() -> io::Result<()> {
    let mut nl = Netlink::open(libc::NETLINK_NETFILTER)?;
    let mut begin = Msg::new(NFNL_MSG_BATCH_BEGIN, 0);
    begin.raw(&nfgen(libc::AF_UNSPEC as u8, NFNL_SUBSYS_NFTABLES));
    let [add, delete] = clear_table(libc::NFPROTO_IPV4 as u8);
    let mut end = Msg::new(NFNL_MSG_BATCH_END, 0);
    end.raw(&nfgen(libc::AF_UNSPEC as u8, NFNL_SUBSYS_NFTABLES));
    nl.request(&mut [begin, add, delete, end])
}

/// Kiri's NAT table:
///
/// ```text
/// table ip kiri {
///   chain postrouting { type nat hook postrouting priority 100; oif <uplink> masquerade }
///   chain prerouting  { type nat hook prerouting priority -100; ip daddr <host> tcp redirect to :<port> }
///   chain output      { type nat hook output priority -100;     ip daddr <host> tcp redirect to :<port> }
///   chain forward     { … }   (see `forward_filter`)
///   chain input       { … }   (see `localnet_guard`)
/// }
/// ```
///
/// Everything leaving through the uplink is source-NATed to the VM's
/// address; connections to the reserved host address go to the agent, which
/// relays them to the Mac.
///
/// Built by `net::install_nat`. In the VM (`redirect`), `dnat` are published ports the
/// Mac reaches over the NAT network, so they match only connections from
/// the uplink's gateway `gateway` (the Mac itself): other VMs on the NAT
/// network, or the LAN routed through the Mac, never reach a container.
#[allow(clippy::too_many_arguments)]
pub fn install_nat_from(uplink_index: i32, host_addr: [u8; 4], host_port: u16, redirect: bool, dnat: &[Dnat], gateway: [u8; 4], bridges: &[String], local: Option<&[[u8; 4]]>) -> io::Result<()> {
    let mut nl = Netlink::open(libc::NETLINK_NETFILTER)?;
    // The table is replaced, not appended to (on a Linux host it outlives
    // the agent and changes with every published port), in one batch:
    // see clear_table.
    let mut begin = Msg::new(NFNL_MSG_BATCH_BEGIN, 0);
    begin.raw(&nfgen(libc::AF_UNSPEC as u8, NFNL_SUBSYS_NFTABLES));
    let [add, delete] = clear_table(libc::NFPROTO_IPV4 as u8);

    let mut table = nft(NFT_MSG_NEWTABLE, NLM_F_ACK | NLM_F_CREATE);
    table.attr_str(1, "kiri"); // NFTA_TABLE_NAME

    let mut chain = nft(NFT_MSG_NEWCHAIN, NLM_F_ACK | NLM_F_CREATE);
    chain.attr_str(1, "kiri").attr_str(3, "postrouting"); // NFTA_CHAIN_TABLE, NFTA_CHAIN_NAME
    chain.nest(4).attr_be32(1, 4).attr_be32(2, 100).end(); // NFTA_CHAIN_HOOK: hooknum POSTROUTING, priority
    chain.attr_be32(5, 1).attr_str(7, "nat"); // NFTA_CHAIN_POLICY accept, NFTA_CHAIN_TYPE

    let mut rule = nft(NFT_MSG_NEWRULE, NLM_F_ACK | NLM_F_CREATE | 0x800); // NLM_F_APPEND
    rule.attr_str(1, "kiri").attr_str(2, "postrouting"); // NFTA_RULE_TABLE, NFTA_RULE_CHAIN
    rule.nest(4); // NFTA_RULE_EXPRESSIONS
    // meta load oif => reg 1 (NFTA_META_DREG, NFTA_META_KEY = NFT_META_OIF, which is 5)
    rule.nest(1).attr_str(1, "meta").nest(2).attr_be32(1, 1).attr_be32(2, 5).end().end();
    // cmp eq reg 1, uplink index
    rule.nest(1).attr_str(1, "cmp").nest(2).attr_be32(1, 1).attr_be32(2, 0);
    rule.nest(3).attr(1, &(uplink_index as u32).to_ne_bytes()).end().end().end();
    // masquerade
    rule.nest(1).attr_str(1, "masq").end();
    rule.end();

    let mut end = Msg::new(NFNL_MSG_BATCH_END, 0);
    end.raw(&nfgen(libc::AF_UNSPEC as u8, NFNL_SUBSYS_NFTABLES));
    if !redirect {
        // A Linux host: no host.docker.internal redirect, but its published
        // ports are DNATed here (prerouting for the LAN, output for the host
        // itself), the whole table rebuilt on every change, atomically.
        let mut msgs = vec![begin, add, delete, table, chain, rule];
        msgs.extend(forward_filter(bridges));
        msgs.extend(localnet_guard());
        msgs.push(nat_chain("prerouting", 0, -100));
        msgs.push(nat_chain("output", 3, -100));
        for d in dnat {
            msgs.extend(dnat_rules("prerouting", d, local));
            msgs.extend(dnat_rules("output", d, local));
        }
        msgs.push(end);
        return nl.request(&mut msgs);
    }
    let pre = nat_chain("prerouting", 0, -100); // NF_INET_PRE_ROUTING
    let out = nat_chain("output", 3, -100); // NF_INET_LOCAL_OUT
    // Only from Kiri's own bridges: the LAN must not reach the loopback relay.
    let pre_rule = redirect_rule("prerouting", host_addr, host_port, true);
    // The host's own connections (and `--network host` containers).
    let out_rule = redirect_rule("output", host_addr, host_port, false);
    let mut msgs = vec![begin, add, delete, table, chain, rule, pre, out, dns_redirect_rule(host_addr), pre_rule, out_rule];
    msgs.extend(forward_filter(bridges));
    msgs.extend(localnet_guard());
    msgs.push(host_relay_guard(host_port));
    for d in dnat.iter().filter(|d| !d.udp) {
        msgs.push(dnat_rule_from("prerouting", d, Some((uplink_index, gateway))));
    }
    if dnat.iter().any(|d| !d.udp) {
        msgs.push(published_masq_rule(uplink_index, gateway));
    }
    msgs.push(end);
    nl.request(&mut msgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The attributes in `buf`: (type without NLA_F_NESTED, payload).
    fn attrs(mut buf: &[u8]) -> Vec<(u16, &[u8])> {
        let mut out = Vec::new();
        while buf.len() >= 4 {
            let len = u16::from_ne_bytes([buf[0], buf[1]]) as usize;
            let ty = u16::from_ne_bytes([buf[2], buf[3]]) & 0x7fff;
            out.push((ty, &buf[4..len]));
            buf = &buf[(len + 3) & !3..];
        }
        out
    }

    fn get<'a>(a: &[(u16, &'a [u8])], ty: u16) -> &'a [u8] {
        a.iter().find(|(t, _)| *t == ty).map(|(_, d)| *d).expect("attribute")
    }

    fn be32(d: &[u8]) -> u32 {
        u32::from_be_bytes(d.try_into().unwrap())
    }

    /// The table is replaced with messages every kernel since 3.13 knows:
    /// add (no error when it is there), then delete, in the batch that
    /// makes it again. Linux 6.3's DESTROYTABLE (26) made older kernels
    /// refuse the whole batch, so containers had no NAT and no firewall.
    #[test]
    fn the_table_is_replaced_with_messages_older_kernels_know() {
        for family in [libc::NFPROTO_IPV4 as u8, libc::NFPROTO_IPV6 as u8] {
            let [add, delete] = clear_table(family);
            let ty = |m: &Msg| u16::from_ne_bytes([m.buf[4], m.buf[5]]);
            let flags = |m: &Msg| u16::from_ne_bytes([m.buf[6], m.buf[7]]);
            assert_eq!(ty(&add), (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWTABLE);
            assert_eq!(flags(&add) & (NLM_F_CREATE | NLM_F_EXCL), NLM_F_CREATE, "adding must not fail when the table exists");
            assert_eq!(ty(&delete), (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_DELTABLE);
            for m in [&add, &delete] {
                assert_eq!(m.buf[16], family);
                assert_eq!(get(&attrs(&m.buf[20..]), 1), b"kiri\0");
                assert_ne!(ty(m) & 0xff, 26, "DESTROYTABLE needs Linux 6.3");
            }
        }
    }

    #[test]
    fn a_bridge_is_made_with_a_mac_address_of_its_own() {
        let m = new_bridge("kiri0", bridge_mac([0xff; 6]));
        let a = attrs(&m.buf[32..]); // nlmsghdr, ifinfomsg
        assert_eq!(get(&a, IFLA_ADDRESS), &[0xfe, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(get(&a, IFLA_IFNAME), b"kiri0\0");
        // Locally administered, never multicast.
        for b in [[0u8; 6], [0x01; 6], [0xff; 6]] {
            assert_eq!(bridge_mac(b)[0] & 0x03, 0x02);
        }
    }

    /// A rule's expressions: (name, its NFTA_EXPR_DATA attributes).
    fn exprs(m: &Msg) -> Vec<(String, Vec<(u16, &[u8])>)> {
        let top = attrs(&m.buf[20..]); // nlmsghdr, nfgenmsg
        assert_eq!(get(&top, 1), b"kiri\0");
        attrs(get(&top, 4))
            .into_iter()
            .map(|(_, e)| {
                let e = attrs(e);
                let name = String::from_utf8(get(&e, 1).split(|b| *b == 0).next().unwrap().to_vec()).unwrap();
                (name, e.iter().find(|(t, _)| *t == 2).map_or(Vec::new(), |(_, d)| attrs(d)))
            })
            .collect()
    }

    fn names(m: &Msg) -> Vec<String> {
        exprs(m).into_iter().map(|(n, _)| n).collect()
    }

    /// The verdict of a rule ending in `immediate`.
    fn verdict(m: &Msg) -> u32 {
        let e = exprs(m);
        let (name, data) = e.last().unwrap();
        assert_eq!(name, "immediate");
        be32(get(&attrs(get(&attrs(get(data, 2)), 2)), 1))
    }

    #[test]
    fn forward_keeps_networks_apart() {
        let msgs = forward_filter(&["kiri1".to_owned(), "kiri22".to_owned()]);
        assert_eq!(msgs.len(), 7);
        let rules = &msgs[1..];
        // Nothing from a bridge is accepted on the interface name alone: that
        // let any network reach any other.
        for r in rules {
            let e = exprs(r);
            let iifname_only = names(r) == ["meta", "cmp", "immediate"] && be32(get(&e[0].1, 2)) == 6;
            assert!(!iifname_only, "a bridge-to-any-bridge accept");
        }
        // One rule per bridge: in by it and out by it, the whole name. No
        // fib expression anywhere (WSL2's kernel has no nft_fib_ipv6).
        for (same, name) in rules[3..5].iter().zip([&b"kiri1"[..], b"kiri22"]) {
            assert_eq!(names(same), ["meta", "cmp", "meta", "cmp", "immediate"]);
            let e = exprs(same);
            let mut full = [0u8; 16];
            full[..name.len()].copy_from_slice(name);
            assert_eq!(be32(get(&e[0].1, 2)), 6, "meta iifname");
            assert_eq!(be32(get(&e[2].1, 2)), 7, "meta oifname");
            for c in [&e[1].1, &e[3].1] {
                assert_eq!(be32(get(c, 2)), 0, "cmp eq");
                assert_eq!(get(&attrs(get(c, 3)), 1), &full);
            }
            assert_eq!(verdict(same), libc::NF_ACCEPT as u32);
        }
        // and everything else headed into a bridge is dropped
        assert_eq!(names(&rules[5]), ["immediate"]);
        assert_eq!(verdict(&rules[5]), libc::NF_DROP as u32);
        // ahead of it: not into a bridge, replies, published ports
        assert_eq!(verdict(&rules[0]), libc::NF_ACCEPT as u32);
        assert_eq!(be32(get(&exprs(&rules[0])[0].1, 2)), 7, "meta oifname");
        assert_eq!(be32(get(&exprs(&rules[1])[0].1, 2)), 0, "ct state");
        assert_eq!(be32(get(&exprs(&rules[2])[0].1, 2)), 2, "ct status");
    }

    #[test]
    fn ipv6_masquerade_matches_the_network_by_source() {
        let subnet = [0xfd, 0, 0, 1, 0, 2, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0];
        let r = masq6_rule(subnet, 64);
        assert_eq!(r.buf[16], libc::NFPROTO_IPV6 as u8, "an ip6 table");
        assert_eq!(names(&r), ["payload", "bitwise", "cmp", "meta", "cmp", "masq"]);
        let e = exprs(&r);
        // network header + 8, 16 bytes: ip6 saddr
        assert_eq!((be32(get(&e[0].1, 2)), be32(get(&e[0].1, 3)), be32(get(&e[0].1, 4))), (1, 8, 16));
        let mask = get(&attrs(get(&e[1].1, 4)), 1);
        assert_eq!(mask, &[255, 255, 255, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(get(&attrs(get(&e[2].1, 3)), 1), &subnet);
        assert_eq!(be32(get(&e[3].1, 2)), 7, "meta oifname");
        assert_eq!(be32(get(&e[4].1, 2)), 1, "cmp neq");
        let odd = masq6_rule(subnet, 61);
        let e = exprs(&odd);
        assert_eq!(get(&attrs(get(&e[1].1, 4)), 1)[..8], [255, 255, 255, 255, 255, 255, 255, 0xf8]);
        // the IPv6 forward filter is the IPv4 one in another family
        let bridges = ["kiri3".to_owned()];
        let f = forward_filter_in(libc::NFPROTO_IPV6 as u8, &bridges);
        assert_eq!(f.len(), forward_filter(&bridges).len());
        assert!(f[1..].iter().all(|m| !names(m).contains(&"fib".to_owned())), "no nft_fib_ipv6 needed");
        assert!(f.iter().all(|m| m.buf[16] == libc::NFPROTO_IPV6 as u8));
    }

    #[test]
    fn host_relay_takes_only_redirected_connections() {
        let r = host_relay_guard(1026);
        assert_eq!(names(&r), ["meta", "cmp", "payload", "cmp", "ct", "bitwise", "cmp", "immediate"]);
        let e = exprs(&r);
        assert_eq!(get(&attrs(get(&e[1].1, 3)), 1), &[libc::IPPROTO_TCP as u8]);
        // transport header + 2, 2 bytes: dport
        assert_eq!((be32(get(&e[2].1, 2)), be32(get(&e[2].1, 3)), be32(get(&e[2].1, 4))), (2, 2, 2));
        assert_eq!(get(&attrs(get(&e[3].1, 3)), 1), &1026u16.to_be_bytes());
        assert_eq!(be32(get(&e[4].1, 2)), 2, "ct status");
        assert_eq!(get(&attrs(get(&e[5].1, 4)), 1), &CT_STATUS_DNAT.to_ne_bytes());
        assert_eq!(be32(get(&e[6].1, 2)), 0, "== 0");
        assert_eq!(verdict(&r), libc::NF_DROP as u32);
    }
}
