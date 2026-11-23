//! What Kiri's daemon.json says about registries, with Docker's meaning for
//! each key:
//!
//! - `insecure-registries`: hosts (`registry.lan:5000`) and networks
//!   (`10.0.0.0/8`) reached without a verified certificate. HTTPS is tried
//!   first without checking the certificate, then plain HTTP. Only the
//!   hosts listed: every other registry keeps verified HTTPS, and
//!   credentials only ever go in the clear to a listed host. A network
//!   covers registries named by an address in it (`10.1.2.3:5000`); a
//!   registry named by a host name is covered only when that name is
//!   listed, whatever it resolves to.
//! - `registry-mirrors`: pull-through mirrors of Docker Hub ([`Mirror`]).
//!
//! The daemon reads the file and hands each client a snapshot
//! ([`crate::registry::set_config_source`]).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::mirror::Mirror;
use crate::reference::is_local;

/// The longest host name DNS allows, and the longest label in one.
const MAX_NAME: usize = 253;
const MAX_LABEL: usize = 63;

/// One `insecure-registries` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Insecure {
    /// `host` or `host:port`, matched as written, as Docker matches it: an
    /// entry without a port does not cover the same host with one.
    Host(String),
    /// A network in CIDR form: covers a registry named by an address in it.
    Net(IpAddr, u8),
}

impl Insecure {
    /// Docker's spellings: `registry.lan:5000`, `registry.lan`,
    /// `192.168.1.10:5000`, `[fd00::1]:5000`, `10.0.0.0/8`. An `http://` or
    /// `https://` in front is dropped, as Docker drops it. Docker Hub is
    /// refused, and so is a network wide enough to take in much of the
    /// internet ([`narrow_enough`]): insecure is for named hosts and the
    /// networks they are on, never for everything.
    pub fn parse(input: &str) -> Result<Insecure, String> {
        let text = input.trim();
        let lower = text.to_ascii_lowercase();
        let bare = lower.strip_prefix("http://").or_else(|| lower.strip_prefix("https://")).unwrap_or(&lower).trim_end_matches('/');
        if bare.is_empty() {
            return Err("an insecure registry needs a host, such as registry.lan:5000".into());
        }
        if let Some((ip, bits)) = bare.split_once('/') {
            let ip: IpAddr = ip.parse().map_err(|_| format!("not a host or a network: {text}"))?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let bits: u8 = Some(bits).filter(|b| !b.is_empty() && b.len() <= 3 && b.bytes().all(|c| c.is_ascii_digit())).and_then(|b| b.parse().ok()).filter(|&b| b <= max).ok_or_else(|| format!("not a network: {text}"))?;
            let net = mask(ip, bits);
            if !narrow_enough(net, bits) {
                return Err(format!("{text} takes in addresses on the internet: list the registries' hosts, or a private network such as 10.0.0.0/8"));
            }
            return Ok(Insecure::Net(net, bits));
        }
        let host = canonical(bare).ok_or_else(|| format!("not a host or a network: {text}"))?;
        if is_hub(&host) {
            return Err("Docker Hub is always reached over verified HTTPS".into());
        }
        Ok(Insecure::Host(host))
    }

    /// As daemon.json keeps it.
    pub fn as_string(&self) -> String {
        match self {
            Insecure::Host(h) => h.clone(),
            Insecure::Net(ip, bits) => format!("{ip}/{bits}"),
        }
    }
}

/// IPv6 ranges that hold IPv4 addresses in their last 32 bits: mapped
/// (`::ffff:a.b.c.d`), the old compatible form (`::a.b.c.d`) and NAT64's
/// well-known prefix (`64:ff9b::a.b.c.d`). A network in one is as wide as
/// the IPv4 network it holds.
const IPV4_IN_IPV6: [Ipv6Addr; 3] = [Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), Ipv6Addr::UNSPECIFIED, Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0)];

/// Whether a network may be listed: one inside the ranges that are never on
/// the internet (private, shared, loopback, link-local and unique-local
/// addresses), or a small one anywhere (a /16 or longer for IPv4, a /48 or
/// longer for IPv6). A few short networks (`0.0.0.0/1` and `128.0.0.0/1`)
/// would otherwise make every registry named by its address insecure. An
/// IPv6 network over IPv4 addresses ([`IPV4_IN_IPV6`]) is judged as the
/// IPv4 network it holds, and one that takes in a whole such range is not
/// narrow at any width.
fn narrow_enough(net: IpAddr, bits: u8) -> bool {
    if let IpAddr::V6(v6) = net {
        for range in IPV4_IN_IPV6 {
            let range = IpAddr::V6(range);
            if bits >= 96 && mask(net, 96) == range {
                let [.., a, b, c, d] = v6.octets();
                return narrow_enough(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), bits - 96);
            }
            if bits < 96 && mask(range, bits) == net {
                return false;
            }
        }
    }
    let inside = |base: IpAddr, len: u8| bits >= len && mask(net, len) == base;
    let v4 = |a: [u8; 4], len: u8| inside(IpAddr::V4(Ipv4Addr::from(a)), len);
    let v6 = |a: &str, len: u8| a.parse::<Ipv6Addr>().is_ok_and(|a| inside(IpAddr::V6(a), len));
    match net {
        IpAddr::V4(_) => {
            bits >= 16
                || v4([10, 0, 0, 0], 8)
                || v4([172, 16, 0, 0], 12)
                || v4([192, 168, 0, 0], 16)
                || v4([100, 64, 0, 0], 10)
                || v4([127, 0, 0, 0], 8)
                || v4([169, 254, 0, 0], 16)
        }
        IpAddr::V6(_) => bits >= 48 || v6("fc00::", 7) || v6("fe80::", 10) || v6("::1", 128),
    }
}

/// `host[:port]` as it is matched: lowercase, an IPv6 address in its
/// shortest form (`[fd00:0::1]` is `[fd00::1]`), a port of plain digits.
/// `None` for anything that is not a host: a name of letters, digits, `-`
/// and `_` in labels DNS allows, an IPv4 address, or an IPv6 address in
/// brackets, with an optional port from 1 to 65535. A user name (`@`), a
/// path, a query or anything else makes it no host at all.
pub fn canonical(s: &str) -> Option<String> {
    let s = s.to_ascii_lowercase();
    let label = |l: &str| !l.is_empty() && l.len() <= MAX_LABEL && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let port_ok = |p: &str| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) && !p.starts_with('0') && p.parse::<u32>().is_ok_and(|p| (1..=65535).contains(&p));
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (ip, after) = rest.split_once(']')?;
        let ip: Ipv6Addr = ip.parse().ok()?;
        let port = match after {
            "" => None,
            a => Some(a.strip_prefix(':')?),
        };
        (format!("[{ip}]"), port)
    } else {
        let (h, p) = s.rsplit_once(':').map_or((s.as_str(), None), |(h, p)| (h, Some(p)));
        if h.len() > MAX_NAME || !h.split('.').all(label) {
            return None;
        }
        (h.to_owned(), p)
    };
    match port {
        None => Some(host),
        Some(p) if port_ok(p) => Some(format!("{host}:{p}")),
        Some(_) => None,
    }
}

/// Docker Hub's names. Hub is always reached over verified HTTPS: no
/// setting, and no address the name resolves to, changes that.
pub fn is_hub(host: &str) -> bool {
    let name = host_name(host);
    ["docker.io", "index.docker.io", "registry-1.docker.io", "registry.hub.docker.com", "auth.docker.io"].iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// The host of `host[:port]` or `[v6]:port`, without the port, and without
/// whatever a `user@` in front would hide it behind.
fn host_name(s: &str) -> &str {
    let s = s.rsplit('@').next().unwrap_or(s);
    if let Some(rest) = s.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    // `registry-1.docker.io.` (a trailing dot) is the same name.
    s.rsplit_once(':').map_or(s, |(h, _)| h).trim_end_matches('.')
}

fn mask(ip: IpAddr, bits: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v) => {
            let m = if bits == 0 { 0 } else { u32::MAX << (32 - u32::from(bits)) };
            IpAddr::V4((u32::from(v) & m).into())
        }
        IpAddr::V6(v) => {
            let m = if bits == 0 { 0 } else { u128::MAX << (128 - u32::from(bits)) };
            IpAddr::V6((u128::from(v) & m).into())
        }
    }
}

/// How a registry host is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Verified HTTPS: every registry nothing below covers.
    Https,
    /// A registry on this machine (Docker treats loopback as insecure) or
    /// listed in `insecure-registries`: HTTPS without checking the
    /// certificate, else plain HTTP.
    Insecure,
}

/// The registry settings of daemon.json. The default is none: every
/// registry over verified HTTPS, Docker Hub direct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryConfig {
    pub insecure: Vec<Insecure>,
    pub mirrors: Vec<Mirror>,
}

impl RegistryConfig {
    /// Whether `host` (`registry.lan:5000`, as a reference names it) is
    /// listed in `insecure-registries`: written in the list, or named by an
    /// address inside a listed network. A host name is never looked up for
    /// a network: what it resolves to can change between the check and the
    /// connection, and a name with several addresses would be checked
    /// against one and reached at another. Docker Hub never is, and nothing
    /// that is not a plain `host[:port]` is.
    pub fn is_insecure(&self, host: &str) -> bool {
        if self.insecure.is_empty() || is_hub(host) {
            return false;
        }
        let Some(host) = canonical(host) else { return false };
        if self.insecure.iter().any(|e| matches!(e, Insecure::Host(h) if *h == host)) {
            return true;
        }
        let Some(ip) = host_ip(&host) else { return false };
        self.insecure.iter().any(|e| match e {
            Insecure::Net(net, bits) => ip.is_ipv4() == net.is_ipv4() && mask(ip, *bits) == *net,
            Insecure::Host(_) => false,
        })
    }

    /// Whether `host` is written in the list as a host (not reached by a
    /// network): the only way a registry may name another host for its
    /// blobs and have unchecked TLS or plain HTTP.
    pub fn is_listed_host(&self, host: &str) -> bool {
        if is_hub(host) {
            return false;
        }
        let Some(host) = canonical(host) else { return false };
        self.insecure.iter().any(|e| matches!(e, Insecure::Host(h) if *h == host))
    }

    /// Whether `host` may be reached without a verified certificate: a
    /// registry on this machine, as Docker treats loopback, or one in the
    /// list. Docker Hub never, and nothing that is not a plain host.
    pub fn is_unverified(&self, host: &str) -> bool {
        !is_hub(host) && canonical(host).is_some() && (is_local(host) || self.is_insecure(host))
    }

    /// How `host` is reached: Docker Hub always verified, loopback and listed
    /// hosts insecure (HTTPS unchecked, else plain HTTP, as Docker does),
    /// the rest over verified HTTPS.
    pub fn reach(&self, host: &str) -> Reach {
        if self.is_unverified(host) { Reach::Insecure } else { Reach::Https }
    }

    /// Whether `mirror` may be used: one written `https://` always; one
    /// written `http://` only when its host may go without verified TLS
    /// anyway (on this machine, or in `insecure-registries`), so that a
    /// build and a pull reach it the same way, and the answers a pull takes
    /// for Docker Hub's never cross the network in the clear from a host
    /// nobody marked insecure.
    pub fn mirror_allowed(&self, mirror: &Mirror) -> bool {
        !mirror.http || mirror.is_local() || self.is_unverified(&mirror.host)
    }

    /// The mirrors a pull of a Docker Hub image tries, in order.
    pub fn usable_mirrors(&self) -> impl Iterator<Item = &Mirror> {
        self.mirrors.iter().filter(|m| self.mirror_allowed(m))
    }
}

/// The address a canonical `host[:port]` names, if it is one.
fn host_ip(host: &str) -> Option<IpAddr> {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next()?.parse::<Ipv6Addr>().ok().map(IpAddr::V6);
    }
    host.split(':').next()?.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(entries: &[&str]) -> RegistryConfig {
        RegistryConfig { insecure: entries.iter().map(|e| Insecure::parse(e).unwrap()).collect(), mirrors: Vec::new() }
    }

    #[test]
    fn entries_are_read_as_docker_reads_them() {
        assert_eq!(Insecure::parse("Registry.LAN:5000").unwrap(), Insecure::Host("registry.lan:5000".into()));
        assert_eq!(Insecure::parse("http://registry.lan:5000/").unwrap(), Insecure::Host("registry.lan:5000".into()));
        assert_eq!(Insecure::parse("https://registry.lan").unwrap(), Insecure::Host("registry.lan".into()));
        assert_eq!(Insecure::parse("[fd00::1]:5000").unwrap(), Insecure::Host("[fd00::1]:5000".into()));
        // One IPv6 address, one spelling.
        assert_eq!(Insecure::parse("[FD00:0:0::1]:5000").unwrap(), Insecure::Host("[fd00::1]:5000".into()));
        assert_eq!(Insecure::parse("10.1.2.3/8").unwrap().as_string(), "10.0.0.0/8");
        assert_eq!(Insecure::parse("fd00::/8").unwrap().as_string(), "fd00::/8");
        assert_eq!(Insecure::parse("203.0.113.0/24").unwrap().as_string(), "203.0.113.0/24");
    }

    #[test]
    fn everything_and_docker_hub_are_refused() {
        for bad in [
            "", " ", "http://", "0.0.0.0/0", "::/0", "docker.io", "index.docker.io", "registry-1.docker.io:443", "registry.hub.docker.com", "reg lan", "reg.lan:port", "reg.lan:0", "10.0.0.0/33", "[nope]:5000", "a..b", "reg.lan/path",
            // A port is plain digits, 1 to 65535.
            "reg.lan:+5000", "reg.lan:05000", "reg.lan:65536", "reg.lan:",
            // A user name in front hides the real host.
            "10.0.0.1:@registry-1.docker.io", "user@reg.lan",
        ] {
            assert!(Insecure::parse(bad).is_err(), "{bad:?} should be refused");
        }
        let long_label = format!("{}.lan", "a".repeat(64));
        let long_name = format!("{}lan", "abcdefghi.".repeat(26));
        assert!(Insecure::parse(&long_label).is_err());
        assert!(Insecure::parse(&long_name).is_err());
        assert!(Insecure::parse(&format!("{}.lan", "a".repeat(63))).is_ok());
    }

    #[test]
    fn networks_that_take_in_the_internet_are_refused() {
        // Halves and quarters of everything, which no /0 check catches.
        for wide in ["0.0.0.0/1", "128.0.0.0/1", "::/1", "8000::/1", "64.0.0.0/2", "8.0.0.0/8", "11.0.0.0/8", "2000::/3", "2001:db8::/32", "172.0.0.0/8"] {
            assert!(Insecure::parse(wide).is_err(), "{wide}");
        }
        // Private and other never-routed ranges, at any width inside them,
        // and small networks anywhere.
        for ok in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "fc00::/7", "fd12:3456::/32", "fe80::/10", "203.0.113.0/24", "198.51.0.0/16", "2001:db8:1::/48"] {
            assert!(Insecure::parse(ok).is_ok(), "{ok}");
        }
    }

    /// IPv6 networks over IPv4 addresses are as wide as the IPv4 network in
    /// them: all of IPv4 written as mapped or NAT64 addresses is refused.
    #[test]
    fn ipv4_written_as_ipv6_is_held_to_the_ipv4_widths() {
        for wide in ["::ffff:0:0/96", "::ffff:0.0.0.0/97", "::/48", "::/64", "::/95", "::/96", "64:ff9b::/96", "64:ff9b::/64", "::ffff:8.0.0.0/104", "::ffff:0:0/80"] {
            assert!(Insecure::parse(wide).is_err(), "{wide}");
        }
        for ok in ["::ffff:10.0.0.0/104", "::ffff:192.168.0.0/112", "::ffff:203.0.113.0/120", "64:ff9b::10.1.0.0/112", "::1/128", "::ffff:198.51.0.0/112"] {
            assert!(Insecure::parse(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn only_listed_hosts_are_insecure() {
        let c = cfg(&["registry.lan:5000", "plain.lan", "[fd00::1]:5000"]);
        assert!(c.is_insecure("registry.lan:5000"));
        assert!(c.is_insecure("REGISTRY.lan:5000"));
        // Docker matches the name as written: another port is another registry.
        assert!(!c.is_insecure("registry.lan:5001"));
        assert!(!c.is_insecure("registry.lan"));
        assert!(c.is_insecure("plain.lan"));
        assert!(!c.is_insecure("plain.lan:5000"));
        assert!(!c.is_insecure("ghcr.io"));
        assert!(c.is_insecure("[fd00:0::1]:5000"), "the same address, spelt longer");
        assert!(!RegistryConfig::default().is_insecure("registry.lan:5000"));
    }

    #[test]
    fn a_network_covers_registries_named_by_an_address_in_it() {
        let c = cfg(&["192.168.7.0/24", "fd00:1::/32"]);
        assert!(c.is_insecure("192.168.7.20:5000"));
        assert!(c.is_insecure("192.168.7.20"));
        assert!(!c.is_insecure("192.168.8.20:5000"));
        assert!(c.is_insecure("[fd00:1::5]:5000"));
        assert!(!c.is_insecure("[fd00:2::5]:5000"));
        // A name is never looked up for a network, whatever it resolves to:
        // localhost is 127.0.0.1, and the network does not cover it.
        let lo = cfg(&["127.0.0.0/8"]);
        assert!(lo.is_insecure("127.0.0.1:5000"));
        assert!(!lo.is_insecure("localhost:5000"));
        assert!(!lo.is_listed_host("127.0.0.1:5000"));
    }

    #[test]
    fn what_is_not_a_plain_host_is_never_insecure() {
        // A user name in front of the real host, as a login server might be
        // written: neither the address before the `@` nor loopback counts.
        let c = cfg(&["10.0.0.0/8", "127.0.0.0/8"]);
        for weird in ["10.0.0.1:@registry-1.docker.io", "10.0.0.1:@ghcr.io", "127.0.0.1:@evil.example", "10.0.0.1:5000/x", "10.0.0.1:5000?x", "10.0.0.1#x"] {
            assert!(!c.is_insecure(weird), "{weird}");
            assert!(!c.is_unverified(weird), "{weird}");
            assert_eq!(c.reach(weird), Reach::Https, "{weird}");
        }
        assert!(is_hub("10.0.0.1:@registry-1.docker.io"));
    }

    #[test]
    fn docker_hub_is_never_insecure_whatever_it_resolves_to() {
        // 127.0.0.0/8 is where the hub names would resolve with a hosts-file
        // entry; the network must not cover them.
        let c = cfg(&["10.0.0.0/8", "127.0.0.0/8", "192.168.0.0/16"]);
        for hub in ["docker.io", "index.docker.io", "registry-1.docker.io", "registry-1.docker.io:443", "REGISTRY-1.docker.io", "registry.hub.docker.com"] {
            assert!(!c.is_insecure(hub), "{hub}");
            assert!(!c.is_unverified(hub), "{hub}");
            assert_eq!(c.reach(hub), Reach::Https, "{hub}");
        }
        assert!(!RegistryConfig::default().is_insecure("10.1.1.1"));
        // A trailing dot names the same host.
        for hub in ["registry-1.docker.io.", "docker.io.", "index.docker.io.:443"] {
            assert!(is_hub(hub), "{hub}");
            assert!(!c.is_unverified(hub), "{hub}");
        }
    }

    #[test]
    fn a_network_alone_is_not_a_listed_host() {
        let c = cfg(&["10.0.0.0/8", "registry.lan:5000"]);
        assert!(c.is_insecure("10.1.2.3:5000"));
        assert!(!c.is_listed_host("10.1.2.3:5000"));
        assert!(c.is_listed_host("registry.lan:5000"));
    }

    #[test]
    fn each_host_is_reached_its_own_way() {
        let c = cfg(&["registry.lan:5000"]);
        assert_eq!(c.reach("registry.lan:5000"), Reach::Insecure);
        assert_eq!(c.reach("localhost:5000"), Reach::Insecure);
        // A name under .localhost may be sent to DNS: it is not this machine.
        assert_eq!(c.reach("evil.localhost:5000"), Reach::Https);
        assert!(!c.mirror_allowed(&Mirror::parse("http://evil.localhost:5000").unwrap()));
        assert_eq!(c.reach("mirror.example.com"), Reach::Https);
        assert_eq!(c.reach("ghcr.io"), Reach::Https);
    }

    #[test]
    fn a_plain_http_mirror_is_used_only_on_a_host_that_may_go_unverified() {
        let mut c = cfg(&["mirror.lan:5000", "10.0.0.0/8"]);
        c.mirrors = ["http://mirror.lan:5000", "http://other.lan:5000", "http://127.0.0.1:5000", "http://10.1.2.3:5000", "https://mirror.example.com"].iter().map(|m| Mirror::parse(m).unwrap()).collect();
        let used: Vec<String> = c.usable_mirrors().map(Mirror::as_url).collect();
        assert_eq!(used, ["http://mirror.lan:5000", "http://127.0.0.1:5000", "http://10.1.2.3:5000", "https://mirror.example.com"]);
        assert!(!c.mirror_allowed(&Mirror::parse("http://other.lan:5000").unwrap()));
        assert!(RegistryConfig::default().mirror_allowed(&Mirror::parse("https://other.lan").unwrap()));
    }
}
