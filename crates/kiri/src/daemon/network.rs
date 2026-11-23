//! Docker networks and address assignment (IPAM).
//!
//! `bridge` (the default, `kiri0`, 172.17.0.0/16), `host` and `none` always
//! exist. User-defined bridge networks get their own bridge and a /16 from
//! 172.18.0.0 up, as Docker picks them, and are saved to `networks.json`.
//! `IPAM.Config` (`--subnet`, `--gateway`, `--ip-range`, `--aux-address`)
//! picks them instead, and containers may then ask for fixed addresses.
//!
//! On a Linux host the pool keeps clear of the host's own networks, and of
//! the ones Docker Engine there brings back when it starts; a network whose
//! subnet one of them takes later moves, or is refused (`Network::settle`).
//!
//! `--ipv6` (Compose `enable_ipv6`) adds an IPv6 subnet beside the IPv4 one:
//! an IPv6 `IPAM.Config` entry, or a /64 from Kiri's own ULA pool (a /48
//! under fd00::/8 per engine), as Docker's default address pools hand out.
//! The default `bridge` stays IPv4-only, as Docker's does.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Network {
    pub id: String,
    pub name: String,
    /// `bridge`, `host` or `null`.
    pub driver: String,
    /// The bridge device in the guest (empty for host and none).
    pub bridge: String,
    pub subnet: [u8; 4],
    pub prefix: u8,
    pub gateway: [u8; 4],
    pub created_ns: u64,
    #[serde(default)]
    pub labels: serde_json::Value,
    #[serde(default)]
    pub internal: bool,
    /// The subnet came from `IPAM.Config`: only then may a container ask
    /// for an address of its own, as with Docker.
    #[serde(default)]
    pub user_subnet: bool,
    /// `--ip-range`: automatic addresses come from here only.
    #[serde(default)]
    pub ip_range: Option<([u8; 4], u8)>,
    /// `--aux-address name=ip`: never handed out.
    #[serde(default)]
    pub aux: BTreeMap<String, [u8; 4]>,
    /// `--opt` and `--ipam-opt`, kept for inspect.
    #[serde(default)]
    pub options: serde_json::Value,
    #[serde(default)]
    pub ipam_options: serde_json::Value,
    #[serde(default)]
    pub attachable: bool,
    /// An IPv6-only network (`--ipv4=false --ipv6`). It still has an IPv4
    /// subnet, inside Kiri only: each container's address there is the
    /// name the guest keeps it by (DNS, MAC), but nothing is given it, the
    /// bridge or the container, and inspect shows none, as with Docker.
    #[serde(default)]
    pub no_ipv4: bool,
    /// The IPv6 side of an `--ipv6` network.
    #[serde(default)]
    pub v6: Option<V6>,
    /// Host parts handed out (runtime only).
    #[serde(skip)]
    used: BTreeSet<u32>,
}

/// An IPv6 network's subnet and addresses, the IPv4 fields' counterparts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V6 {
    pub subnet: [u8; 16],
    pub prefix: u8,
    pub gateway: [u8; 16],
    /// From `IPAM.Config`, not the pool. (A container may ask for an IPv6
    /// address of its own, `--ip6`, either way, as with Docker 29.)
    #[serde(default)]
    pub user_subnet: bool,
    #[serde(default)]
    pub ip_range: Option<([u8; 16], u8)>,
    #[serde(default)]
    pub aux: BTreeMap<String, [u8; 16]>,
    /// Host parts handed out (runtime only).
    #[serde(skip)]
    used: BTreeSet<u128>,
}

pub fn ip6_str(a: [u8; 16]) -> String {
    std::net::Ipv6Addr::from(a).to_string()
}

pub fn parse_ip6(s: &str) -> Option<[u8; 16]> {
    s.parse::<std::net::Ipv6Addr>().ok().map(|a| a.octets())
}

fn mask6(prefix: u8) -> u128 {
    if prefix == 0 { 0 } else { u128::MAX << (128 - prefix as u32) }
}

pub fn in_subnet6(ip: [u8; 16], subnet: [u8; 16], prefix: u8) -> bool {
    u128::from_be_bytes(ip) & mask6(prefix) == u128::from_be_bytes(subnet)
}

pub fn overlaps6(a: ([u8; 16], u8), b: ([u8; 16], u8)) -> bool {
    in_subnet6(a.0, b.0, b.1) || in_subnet6(b.0, a.0, a.1)
}

impl V6 {
    /// Docker's `"fd00:1::/64"`.
    pub fn subnet_str(&self) -> String {
        format!("{}/{}", ip6_str(self.subnet), self.prefix)
    }

    pub fn ip_range_str(&self) -> Option<String> {
        self.ip_range.map(|(a, p)| format!("{}/{p}", ip6_str(a)))
    }

    fn reserved(&self, ip: [u8; 16]) -> bool {
        ip == self.gateway || self.aux.values().any(|a| *a == ip)
    }

    /// Host part of `ip` in the subnet.
    fn host(&self, ip: [u8; 16]) -> u128 {
        u128::from_be_bytes(ip).wrapping_sub(u128::from_be_bytes(self.subnet))
    }

    /// The highest host part: IPv6 has no broadcast address, so the last one
    /// is a host too, as Docker hands it out.
    fn last(&self) -> u128 {
        !mask6(self.prefix)
    }

    /// The next free address from ::1 up (or the range), past the gateway,
    /// the auxiliary addresses and `avoid`.
    fn allocate_avoiding(&mut self, avoid: &[[u8; 16]]) -> Option<[u8; 16]> {
        let base = u128::from_be_bytes(self.subnet);
        let (first, last) = match self.ip_range {
            Some((r, p)) => {
                let start = u128::from_be_bytes(r) - base;
                (start.max(1), (start + !mask6(p)).min(self.last()))
            }
            None => (1, self.last()),
        };
        // Taken addresses are few: the search ends within as many steps.
        let mut h = first;
        loop {
            if h > last {
                return None;
            }
            let ip = (base + h).to_be_bytes();
            if !self.used.contains(&h) && !self.reserved(ip) && !avoid.contains(&ip) {
                self.used.insert(h);
                return Some(ip);
            }
            h = h.checked_add(1)?;
        }
    }

    fn release(&mut self, ip: [u8; 16]) {
        let h = self.host(ip);
        self.used.remove(&h);
    }
}

impl Network {
    pub fn builtin(&self) -> bool {
        matches!(self.name.as_str(), "bridge" | "host" | "none")
    }

    /// Docker's `"172.17.0.0/16"`.
    pub fn subnet_str(&self) -> String {
        format!("{}/{}", ip_str(self.subnet), self.prefix)
    }

    /// Docker's `"172.17.0.0/16"`.
    pub fn ip_range_str(&self) -> Option<String> {
        self.ip_range.map(|(a, p)| format!("{}/{p}", ip_str(a)))
    }

    /// Whether `ip` is in the subnet.
    pub fn contains(&self, ip: [u8; 4]) -> bool {
        in_subnet(ip, self.subnet, self.prefix)
    }

    /// Addresses no container may have: the gateway and the auxiliary ones.
    fn reserved(&self, ip: [u8; 4]) -> bool {
        ip == self.gateway || self.aux.values().any(|a| *a == ip)
    }

    /// The next free address, skipping the network, broadcast, gateway and
    /// auxiliary addresses and `avoid`; only from `--ip-range` when one is set.
    fn allocate_avoiding(&mut self, avoid: &[[u8; 4]]) -> Option<[u8; 4]> {
        let base = u32::from_be_bytes(self.subnet);
        let size = 1u32 << (32 - self.prefix as u32);
        let (first, last) = match self.ip_range {
            Some((r, p)) => {
                let start = u32::from_be_bytes(r) - base;
                (start.max(1), (start + (1u32 << (32 - p as u32)) - 1).min(size - 2))
            }
            None => (1, size - 2),
        };
        let host = (first..=last).find(|h| !self.used.contains(h) && !self.reserved((base + h).to_be_bytes()) && !avoid.contains(&(base + h).to_be_bytes()))?;
        self.used.insert(host);
        Some((base + host).to_be_bytes())
    }

    /// Whether a container may ask for `ip` here (`--ip`, Compose
    /// `ipv4_address`), in Docker's words when not. Whether it is free is
    /// only known when the container starts.
    pub fn check_static(&self, ip: [u8; 4]) -> Result<(), String> {
        if self.bridge.is_empty() {
            return Err(format!("user specified IP address is supported only on bridge networks, not {}", self.name));
        }
        if self.builtin() {
            return Err("user specified IP address is supported on user defined networks only".into());
        }
        if !self.user_subnet {
            return Err("user specified IP address is supported only when connecting to networks with user configured subnets".into());
        }
        let size = 1u32 << (32 - self.prefix as u32);
        let host = u32::from_be_bytes(ip).wrapping_sub(u32::from_be_bytes(self.subnet));
        if !self.contains(ip) || host == 0 || host == size - 1 {
            return Err(format!("no configured subnet or ip-range contain the IP address {}", ip_str(ip)));
        }
        Ok(())
    }

    /// Whether a container may ask for `ip` here (`--ip6`, Compose
    /// `ipv6_address`), in Docker's words when not.
    pub fn check_static6(&self, ip: [u8; 16]) -> Result<(), String> {
        if self.bridge.is_empty() {
            return Err(format!("user specified IP address is supported only on bridge networks, not {}", self.name));
        }
        if self.builtin() {
            return Err("user specified IP address is supported on user defined networks only".into());
        }
        let Some(v6) = &self.v6 else {
            return Err(format!("invalid endpoint settings: an IPv6 address was given, but network {} does not have IPv6 enabled", self.name));
        };
        // Docker 29 takes one on an automatically allocated IPv6 subnet too
        // (moby#51569), unlike IPv4.
        if !in_subnet6(ip, v6.subnet, v6.prefix) || v6.host(ip) == 0 {
            return Err(format!("no configured subnet or ip-range contain the IP address {}", ip6_str(ip)));
        }
        Ok(())
    }

    fn reserve6(&mut self, ip: [u8; 16]) -> Result<(), String> {
        self.check_static6(ip)?;
        let v6 = self.v6.as_mut().expect("checked");
        let h = v6.host(ip);
        if v6.reserved(ip) || !v6.used.insert(h) {
            return Err(format!("Address already in use: {}", ip6_str(ip)));
        }
        Ok(())
    }

    /// Takes `ip` for a container that asked for it.
    fn reserve(&mut self, ip: [u8; 4]) -> Result<(), String> {
        self.check_static(ip)?;
        let host = u32::from_be_bytes(ip) - u32::from_be_bytes(self.subnet);
        if self.reserved(ip) || !self.used.insert(host) {
            return Err(format!("Address already in use: {}", ip_str(ip)));
        }
        Ok(())
    }

    fn release(&mut self, ip: [u8; 4]) {
        let host = u32::from_be_bytes(ip).wrapping_sub(u32::from_be_bytes(self.subnet));
        self.used.remove(&host);
    }
}

#[derive(Debug)]
pub struct Networks {
    list: Vec<Network>,
    path: PathBuf,
    /// This engine's ULA /48 (fd + 40 bits), where `--ipv6` networks without
    /// a subnet get theirs: made from the engine's directory, so it stays
    /// the same across restarts and differs between engines.
    ula: [u8; 6],
}

/// The /48 for an engine at `root`: RFC 4193's fd00::/8 and a 40-bit global
/// ID, here from a hash of the path in place of a random one.
fn ula_prefix(root: &std::path::Path) -> [u8; 6] {
    use sha2::Digest;
    let h = sha2::Sha256::digest(root.to_string_lossy().as_bytes());
    [0xfd, h[0], h[1], h[2], h[3], h[4]]
}

/// The /64 an `--ipv6` network without a subnet gets: subnet ID from a hash
/// of the name, so a network made again gets the same one, moved on past
/// any taken one (by Kiri, or routed on a Linux host).
fn free_subnet6(ula: [u8; 6], name: &str, taken: &[([u8; 16], u8)]) -> Option<([u8; 16], u8)> {
    use sha2::Digest;
    let h = sha2::Sha256::digest(name.as_bytes());
    let start = u16::from_be_bytes([h[0], h[1]]);
    let routes = host_routes6();
    (0..=u16::MAX).map(|i| start.wrapping_add(i)).find_map(|id| {
        let mut s = [0u8; 16];
        s[..6].copy_from_slice(&ula);
        s[6..8].copy_from_slice(&id.to_be_bytes());
        let cand = (s, 64u8);
        (!taken.iter().chain(&routes).any(|t| overlaps6(*t, cand))).then_some(cand)
    })
}

pub fn ip_str(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn builtin(name: &str, driver: &str, bridge: &str, subnet: [u8; 4], gateway: [u8; 4]) -> Network {
    Network {
        // Stable ids, so clients that remember them keep working.
        id: format!("{:0<64}", name.bytes().map(|b| format!("{b:02x}")).collect::<String>()),
        name: name.into(),
        driver: driver.into(),
        bridge: bridge.into(),
        subnet,
        prefix: if bridge.is_empty() { 0 } else { 16 },
        gateway,
        created_ns: 0,
        labels: serde_json::json!({}),
        internal: false,
        user_subnet: false,
        ip_range: None,
        aux: BTreeMap::new(),
        options: serde_json::json!({}),
        ipam_options: serde_json::Value::Null,
        attachable: false,
        no_ipv4: false,
        v6: None,
        used: BTreeSet::new(),
    }
}

/// `--opt com.docker.network.bridge.name=…`. Kiri names each bridge itself
/// (`kiri0`, `kiri1`, …) and the guest's firewall recognises its bridges by
/// that prefix (the forward filter, masquerading, the host.docker.internal
/// and DNS redirects, the loopback guard), so another name is refused rather
/// than reported and not applied.
pub const BRIDGE_NAME_OPT: &str = "com.docker.network.bridge.name";

/// The options a network keeps, or why one of them cannot be applied. An
/// empty bridge name is Docker's "unset" and is dropped.
fn checked_options(options: serde_json::Value) -> Result<serde_json::Value, String> {
    let serde_json::Value::Object(mut map) = options else { return Ok(serde_json::json!({})) };
    match map.get(BRIDGE_NAME_OPT).map(|v| v.as_str().unwrap_or("?")) {
        None => {}
        Some("") => {
            map.remove(BRIDGE_NAME_OPT);
        }
        Some(name) => {
            return Err(format!(
                "Kiri does not support the network option {BRIDGE_NAME_OPT} ({name}): Kiri names each network's bridge itself (kiri1, kiri2, …), and its firewall rules depend on those names. Create the network without it"
            ));
        }
    }
    Ok(serde_json::Value::Object(map))
}

/// A network's options as inspect shows them: a bridge name that is not the
/// bridge's own (kept by a Kiri that did not refuse it) is left out.
pub fn shown_options(n: &Network) -> serde_json::Value {
    let mut o = if n.options.is_object() { n.options.clone() } else { serde_json::json!({}) };
    if let Some(map) = o.as_object_mut()
        && map.get(BRIDGE_NAME_OPT).is_some_and(|v| v.as_str() != Some(n.bridge.as_str()))
    {
        map.remove(BRIDGE_NAME_OPT);
    }
    o
}

/// What `POST /networks/create` asks for beyond the name.
#[derive(Debug, Default)]
pub struct Settings {
    pub labels: serde_json::Value,
    pub internal: bool,
    pub attachable: bool,
    pub options: serde_json::Value,
    pub ipam_options: serde_json::Value,
    pub ipam: Option<Ipam>,
    /// `EnableIPv6`.
    pub ipv6: bool,
    /// `EnableIPv4: false`: IPv6 only (with `ipv6`).
    pub no_ipv4: bool,
    /// An IPv6 `IPAM.Config` entry; without one an IPv6 network gets a /64
    /// from the pool.
    pub ipam6: Option<Ipam6>,
}

/// One IPv6 `IPAM.Config` entry, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ipam6 {
    pub subnet: [u8; 16],
    pub prefix: u8,
    pub gateway: Option<[u8; 16]>,
    pub ip_range: Option<([u8; 16], u8)>,
    pub aux: BTreeMap<String, [u8; 16]>,
}

/// One IPv4 `IPAM.Config` entry, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ipam {
    pub subnet: [u8; 4],
    pub prefix: u8,
    pub gateway: Option<[u8; 4]>,
    pub ip_range: Option<([u8; 4], u8)>,
    pub aux: BTreeMap<String, [u8; 4]>,
}

pub fn in_subnet(ip: [u8; 4], subnet: [u8; 4], prefix: u8) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
    u32::from_be_bytes(ip) & mask == u32::from_be_bytes(subnet)
}

/// Whether two subnets share any address.
pub fn overlaps(a: ([u8; 4], u8), b: ([u8; 4], u8)) -> bool {
    in_subnet(a.0, b.0, b.1) || in_subnet(b.0, a.0, a.1)
}

pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
    s.parse::<std::net::Ipv4Addr>().ok().map(|a| a.octets())
}

/// `02:42:ac:11:00:02`.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut mac {
        let p = parts.next().filter(|p| p.len() == 2)?;
        *b = u8::from_str_radix(p, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// `10.5.0.0/16`; `what` names it in errors, as Docker does.
fn parse_cidr(s: &str, what: &str) -> Result<([u8; 4], u8), String> {
    let bad = || format!("invalid {what} {s}: invalid CIDR block notation");
    let (ip, bits) = s.split_once('/').ok_or_else(bad)?;
    let ip = parse_ip(ip).ok_or_else(bad)?;
    let bits: u8 = bits.parse().ok().filter(|b| *b <= 32).ok_or_else(bad)?;
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits as u32) };
    let net = (u32::from_be_bytes(ip) & mask).to_be_bytes();
    if net != ip {
        return Err(format!("invalid {what} {s}: it should be {}/{bits}", ip_str(net)));
    }
    Ok((ip, bits))
}

/// `fd00:1::/64`; `what` names it in errors, as Docker does.
fn parse_cidr6(s: &str, what: &str) -> Result<([u8; 16], u8), String> {
    let bad = || format!("invalid {what} {s}: invalid CIDR block notation");
    let (ip, bits) = s.split_once('/').ok_or_else(bad)?;
    let ip = parse_ip6(ip).ok_or_else(bad)?;
    let bits: u8 = bits.parse().ok().filter(|b| *b <= 128).ok_or_else(bad)?;
    let net = (u128::from_be_bytes(ip) & mask6(bits)).to_be_bytes();
    if net != ip {
        return Err(format!("invalid {what} {s}: it should be {}/{bits}", ip6_str(net)));
    }
    Ok((ip, bits))
}

/// One IPv6 `IPAM.Config` entry.
fn parse_ipam6(c: &serde_json::Value, subnet: &str) -> Result<Ipam6, String> {
    let field = |k: &str| c[k].as_str().unwrap_or("").to_owned();
    let (net, prefix) = parse_cidr6(subnet, "subnet")?;
    // A network needs room for the gateway and at least one container.
    if !(1..=126).contains(&prefix) {
        return Err(format!("invalid subnet {subnet}: Kiri supports IPv6 prefixes from /1 to /126"));
    }
    let inside = |ip: [u8; 16]| in_subnet6(ip, net, prefix);
    let ip_range = match field("IPRange") {
        r if r.is_empty() => None,
        r => {
            let (rn, rp) = parse_cidr6(&r, "ip-range")?;
            if rp < prefix || !inside(rn) {
                return Err(format!("invalid ip-range {r}: parent subnet {subnet} does not contain ip-range"));
            }
            Some((rn, rp))
        }
    };
    let address = |s: &str, what: &str| -> Result<[u8; 16], String> {
        let ip = parse_ip6(s).ok_or_else(|| format!("invalid {what} {s}: invalid address"))?;
        if !inside(ip) {
            return Err(format!("invalid {what} {s}: parent subnet {subnet} does not contain ip address {s}"));
        }
        Ok(ip)
    };
    let gateway = match field("Gateway") {
        g if g.is_empty() => None,
        g => Some(address(&g, "gateway")?),
    };
    let mut aux = BTreeMap::new();
    if let Some(map) = c["AuxiliaryAddresses"].as_object() {
        for (name, ip) in map {
            aux.insert(name.clone(), address(ip.as_str().unwrap_or(""), &format!("auxiliary address {name}"))?);
        }
    }
    Ok(Ipam6 { subnet: net, prefix, gateway, ip_range, aux })
}

/// The `IPAM` object of a network create request: the default driver, at
/// most one IPv4 and one IPv6 pool.
pub fn parse_ipam(v: &serde_json::Value) -> Result<Option<Ipam>, String> {
    parse_ipam_both(v).map(|(v4, _)| v4)
}

/// `parse_ipam`, with the IPv6 entry too.
pub fn parse_ipam_both(v: &serde_json::Value) -> Result<(Option<Ipam>, Option<Ipam6>), String> {
    match v["Driver"].as_str().unwrap_or("") {
        "" | "default" => {}
        d => return Err(format!("Kiri supports only the default IPAM driver, not {d}")),
    }
    let configs = v["Config"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut out: Option<Ipam> = None;
    let mut out6: Option<Ipam6> = None;
    for c in configs {
        let field = |k: &str| c[k].as_str().unwrap_or("").to_owned();
        let subnet = field("Subnet");
        if subnet.contains(':') {
            if out6.is_some() {
                return Err("Kiri supports one IPv6 subnet per network".into());
            }
            out6 = Some(parse_ipam6(c, &subnet)?);
            continue;
        }
        if subnet.is_empty() {
            if field("Gateway").is_empty() && field("IPRange").is_empty() {
                continue;
            }
            return Err("invalid network config: an IP range or gateway needs a subnet".into());
        }
        if out.is_some() {
            return Err("Kiri supports one IPv4 subnet per network".into());
        }
        let (net, prefix) = parse_cidr(&subnet, "subnet")?;
        if prefix > 30 || prefix < 8 {
            return Err(format!("invalid subnet {subnet}: Kiri supports prefixes from /8 to /30"));
        }
        let inside = |ip: [u8; 4]| in_subnet(ip, net, prefix);
        let ip_range = match field("IPRange") {
            r if r.is_empty() => None,
            r => {
                let (rn, rp) = parse_cidr(&r, "ip-range")?;
                if rp < prefix || !inside(rn) {
                    return Err(format!("invalid ip-range {r}: parent subnet {subnet} does not contain ip-range"));
                }
                Some((rn, rp))
            }
        };
        let address = |s: &str, what: &str| -> Result<[u8; 4], String> {
            let ip = parse_ip(s).ok_or_else(|| format!("invalid {what} {s}: invalid address"))?;
            if !inside(ip) {
                return Err(format!("invalid {what} {s}: parent subnet {subnet} does not contain ip address {s}"));
            }
            Ok(ip)
        };
        let gateway = match field("Gateway") {
            g if g.is_empty() => None,
            g => Some(address(&g, "gateway")?),
        };
        let mut aux = BTreeMap::new();
        if let Some(map) = c["AuxiliaryAddresses"].as_object() {
            for (name, ip) in map {
                aux.insert(name.clone(), address(ip.as_str().unwrap_or(""), &format!("auxiliary address {name}"))?);
            }
        }
        out = Some(Ipam { subnet: net, prefix, gateway, ip_range, aux });
    }
    Ok((out, out6))
}

impl Networks {
    pub fn load(root: &std::path::Path) -> io::Result<Networks> {
        let path = root.join("networks.json");
        let saved: Vec<Network> = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice::<Vec<Network>>(&b).unwrap_or_default().into_iter().filter(|n| !n.builtin()).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        // A saved network whose subnet the host has taken since (Docker
        // started while Kiri was stopped), or Docker will take back when it
        // starts, moves off it; one whose subnet the user gave is refused at
        // its next container if the host holds it (#489).
        let host = host_networks();
        let mut saved = saved;
        let mut moved = false;
        for i in 0..saved.len() {
            let taken: Vec<([u8; 4], u8)> = saved.iter().enumerate().filter(|(j, n)| *j != i && !n.bridge.is_empty()).map(|(_, n)| (n.subnet, n.prefix)).collect();
            let n = &mut saved[i];
            match n.settle(&taken, &host) {
                Ok(Some(was)) => {
                    crate::diag::warn!("network moved off a subnet the host has taken", network = n.name, from = was, to = n.subnet_str());
                    moved = true;
                }
                Ok(None) => {}
                Err(e) => crate::diag::warn!("network's subnet is taken on the host", error = e),
            }
        }
        // 172.17.0.0/16 as Docker uses, unless the host is already on it
        // (Kiri on a Linux host inside another engine's container), Docker
        // here will take it back when it starts, or a saved network has it.
        // Chosen afresh at each start: never saved, as Docker's own default
        // bridge is not.
        let taken: Vec<([u8; 4], u8)> = saved.iter().map(|n| (n.subnet, n.prefix)).collect();
        let (subnet, prefix) = default_bridge_subnet(&taken, &host)?;
        let mut bridge = builtin("bridge", "bridge", "kiri0", subnet, [subnet[0], subnet[1], subnet[2], 1]);
        bridge.prefix = prefix;
        // `KIRI_BRIDGE_IPV6=1`, as Docker's daemon.json `"ipv6": true`: the
        // default bridge gets an IPv6 /64 too. Off by default, as Docker's.
        let ula = ula_prefix(root);
        if std::env::var("KIRI_BRIDGE_IPV6").is_ok_and(|v| v == "1" || v == "true") {
            let taken6: Vec<([u8; 16], u8)> = saved.iter().filter_map(|n| n.v6.as_ref()).map(|v| (v.subnet, v.prefix)).collect();
            if let Some((s6, p6)) = free_subnet6(ula, "bridge", &taken6) {
                let gateway = (u128::from_be_bytes(s6) + 1).to_be_bytes();
                bridge.v6 = Some(V6 { subnet: s6, prefix: p6, gateway, user_subnet: false, ip_range: None, aux: BTreeMap::new(), used: BTreeSet::new() });
            }
        }
        let mut list = vec![bridge, builtin("host", "host", "", [0; 4], [0; 4]), builtin("none", "null", "", [0; 4], [0; 4])];
        list.extend(saved);
        let networks = Networks { list, path, ula };
        if moved {
            networks.save();
        }
        Ok(networks)
    }

    /// Before a network's first container: its subnet checked against the
    /// host's once more (`Network::settle`), as the host may have taken it
    /// since it was chosen. Bridges are made at a network's first container,
    /// so Docker started after Kiri, or at boot before Kiri's containers
    /// came back, may hold it by then. Nothing to do while containers are
    /// on it (their bridge, and its route, came first), off a Linux host
    /// (Kiri's VM has its own network), or for host and none.
    pub fn settle(&mut self, key: &str) -> Result<(), String> {
        if self.find(key).is_none_or(|n| n.bridge.is_empty() || !n.used.is_empty()) {
            return Ok(());
        }
        self.settle_with(key, &host_networks())
    }

    fn settle_with(&mut self, key: &str, host: &[HostNet]) -> Result<(), String> {
        let Some(n) = self.find(key) else { return Ok(()) };
        if n.bridge.is_empty() || !n.used.is_empty() || host.is_empty() {
            return Ok(());
        }
        let name = n.name.clone();
        let taken: Vec<([u8; 4], u8)> = self.list.iter().filter(|o| !o.bridge.is_empty() && o.name != name).map(|o| (o.subnet, o.prefix)).collect();
        let Some(n) = self.find_mut(&name) else { return Ok(()) };
        let Some(was) = n.settle(&taken, host)? else { return Ok(()) };
        crate::diag::warn!("network moved off a subnet the host has taken", network = name, from = was, to = n.subnet_str());
        let saved = !n.builtin();
        if saved {
            self.save();
        }
        Ok(())
    }

    fn save(&self) {
        let user: Vec<&Network> = self.list.iter().filter(|n| !n.builtin()).collect();
        if let Ok(json) = serde_json::to_vec(&user) {
            let tmp = self.path.with_extension("json.tmp");
            if let Err(e) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &self.path)) {
                crate::diag::error!("saving networks failed", error = e);
            }
        }
    }

    pub fn all(&self) -> &[Network] {
        &self.list
    }

    /// By name, full id or id prefix. `default` is Docker's alias for `bridge`.
    pub fn find(&self, key: &str) -> Option<&Network> {
        let key = if key == "default" || key.is_empty() { "bridge" } else { key };
        self.list
            .iter()
            .find(|n| n.name == key || n.id == key)
            .or_else(|| self.list.iter().find(|n| key.len() >= 6 && n.id.starts_with(key)))
    }

    pub(super) fn find_mut(&mut self, name: &str) -> Option<&mut Network> {
        self.list.iter_mut().find(|n| n.name == name)
    }

    pub fn create(&mut self, name: &str, s: Settings) -> Result<Network, String> {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) {
            return Err(format!("invalid network name {name:?}"));
        }
        if self.find(name).is_some_and(|n| n.name == name) {
            return Err(format!("network with name {name} already exists"));
        }
        if s.no_ipv4 && !s.ipv6 {
            return Err("invalid network config: IPv4 or IPv6 must be enabled".into());
        }
        let options = checked_options(s.options.clone())?;
        if s.no_ipv4 && s.ipam.is_some() {
            return Err("invalid network config: an IPv4 subnet was given, but IPv4 is disabled (--ipv4=false)".into());
        }
        let taken: Vec<([u8; 4], u8)> = self.list.iter().filter(|n| !n.bridge.is_empty()).map(|n| (n.subnet, n.prefix)).collect();
        let (subnet, gateway) = match &s.ipam {
            Some(ipam) => {
                if taken.iter().any(|t| overlaps(*t, (ipam.subnet, ipam.prefix))) {
                    return Err("invalid pool request: Pool overlaps with other one on this address space".into());
                }
                // Docker's default gateway: the subnet's first address.
                let first = (u32::from_be_bytes(ipam.subnet) + 1).to_be_bytes();
                ((ipam.subnet, ipam.prefix), ipam.gateway.unwrap_or(first))
            }
            None => {
                let subnet = free_subnet(&taken, false).ok_or("no free subnet left")?;
                let mut gateway = subnet.0;
                gateway[3] = 1;
                (subnet, gateway)
            }
        };
        let v6 = if s.ipv6 {
            let taken6: Vec<([u8; 16], u8)> = self.list.iter().filter_map(|n| n.v6.as_ref()).map(|v| (v.subnet, v.prefix)).collect();
            let (subnet, prefix, user) = match &s.ipam6 {
                Some(i) => {
                    if taken6.iter().any(|t| overlaps6(*t, (i.subnet, i.prefix))) {
                        return Err("invalid pool request: Pool overlaps with other one on this address space".into());
                    }
                    (i.subnet, i.prefix, true)
                }
                None => {
                    let (s, p) = free_subnet6(self.ula, name, &taken6).ok_or("no free IPv6 subnet left")?;
                    (s, p, false)
                }
            };
            let first = (u128::from_be_bytes(subnet) + 1).to_be_bytes();
            Some(V6 {
                subnet,
                prefix,
                gateway: s.ipam6.as_ref().and_then(|i| i.gateway).unwrap_or(first),
                user_subnet: user,
                ip_range: s.ipam6.as_ref().and_then(|i| i.ip_range),
                aux: s.ipam6.as_ref().map(|i| i.aux.clone()).unwrap_or_default(),
                used: BTreeSet::new(),
            })
        } else {
            if s.ipam6.is_some() {
                return Err("invalid network config: an IPv6 subnet needs IPv6 enabled (--ipv6, Compose enable_ipv6: true)".into());
            }
            None
        };
        let index = (1..).find(|i| !self.list.iter().any(|n| n.bridge == format!("kiri{i}"))).expect("unbounded");
        let n = Network {
            id: super::engine::random_hex(32),
            name: name.into(),
            driver: "bridge".into(),
            bridge: format!("kiri{index}"),
            subnet: subnet.0,
            prefix: subnet.1,
            gateway,
            created_ns: now_ns(),
            labels: s.labels,
            internal: s.internal,
            user_subnet: s.ipam.is_some(),
            ip_range: s.ipam.as_ref().and_then(|i| i.ip_range),
            aux: s.ipam.map(|i| i.aux).unwrap_or_default(),
            options,
            ipam_options: s.ipam_options,
            attachable: s.attachable,
            no_ipv4: s.no_ipv4,
            v6,
            used: BTreeSet::new(),
        };
        self.list.push(n.clone());
        self.save();
        Ok(n)
    }

    pub fn remove(&mut self, key: &str) -> Result<(), String> {
        let name = self.find(key).map(|n| n.name.clone()).ok_or_else(|| format!("network {key} not found"))?;
        let n = self.find_mut(&name).expect("found");
        if n.builtin() {
            return Err(format!("{name} is a pre-defined network and cannot be removed"));
        }
        if !n.used.is_empty() || n.v6.as_ref().is_some_and(|v| !v.used.is_empty()) {
            return Err(format!("error while removing network: network {name} has active endpoints"));
        }
        self.list.retain(|n| n.name != name);
        self.save();
        Ok(())
    }

    /// A free address that none of `avoid` (fixed addresses other
    /// containers will ask for) is, so those still get theirs.
    pub fn allocate_avoiding(&mut self, name: &str, avoid: &[[u8; 4]]) -> Option<[u8; 4]> {
        self.find_mut(name).and_then(|n| n.allocate_avoiding(avoid))
    }

    /// Takes a fixed address on a network, or says why it cannot.
    pub fn reserve(&mut self, name: &str, ip: [u8; 4]) -> Result<(), String> {
        self.find_mut(name).ok_or_else(|| format!("network {name} not found"))?.reserve(ip)
    }

    pub fn release(&mut self, name: &str, ip: [u8; 4]) {
        if let Some(n) = self.find_mut(name) {
            n.release(ip);
        }
    }

    /// An IPv6 address on an `--ipv6` network: the fixed one asked for, or
    /// the next free one past `avoid`. None for a network without IPv6.
    pub fn allocate6(&mut self, name: &str, want: Option<[u8; 16]>, avoid: &[[u8; 16]]) -> Result<Option<[u8; 16]>, String> {
        let n = self.find_mut(name).ok_or_else(|| format!("network {name} not found"))?;
        if n.v6.is_none() {
            return match want {
                Some(ip) => n.check_static6(ip).map(|()| None),
                None => Ok(None),
            };
        }
        match want {
            Some(ip) => n.reserve6(ip).map(|()| Some(ip)),
            None => n.v6.as_mut().expect("checked").allocate_avoiding(avoid).map(Some).ok_or_else(|| format!("no free IPv6 address left on network {name}")),
        }
    }

    pub fn release6(&mut self, name: &str, ip: [u8; 16]) {
        if let Some(v6) = self.find_mut(name).and_then(|n| n.v6.as_mut()) {
            v6.release(ip);
        }
    }

    pub fn in_use(&self, name: &str) -> bool {
        self.list.iter().any(|n| n.name == name && !n.used.is_empty())
    }
}

/// The default bridge's subnet, or an error when the whole pool is taken:
/// falling back to 172.17.0.0/16 anyway would hand out a subnet the host
/// already routes (#489), so the daemon fails to start loudly instead.
fn default_bridge_subnet(taken: &[([u8; 4], u8)], host: &[HostNet]) -> io::Result<([u8; 4], u8)> {
    free_subnet_with(taken, true, host).ok_or_else(|| io::Error::other(NO_FREE_SUBNET))
}

const NO_FREE_SUBNET: &str = "no free subnet for the default bridge: 172.17.0.0/16 … 172.31.0.0/16 and 10.89.0.0/24 … 10.89.255.0/24 are all routed on this host or used by another network";

/// The first free subnet: 172.17.0.0/16 (only for the default bridge), then
/// 172.18.0.0/16 … 172.31.0.0/16, then 10.89.N.0/24, skipping ones Kiri
/// already uses and ones on the host's own links and interfaces.
fn free_subnet(taken: &[([u8; 4], u8)], default_bridge: bool) -> Option<([u8; 4], u8)> {
    free_subnet_with(taken, default_bridge, &host_networks())
}

/// The first free subnet against `host`'s networks, as `free_subnet` does
/// against the live host's. Split out so tests can feed it a host.
fn free_subnet_with(taken: &[([u8; 4], u8)], default_bridge: bool, host: &[HostNet]) -> Option<([u8; 4], u8)> {
    let first = if default_bridge { 17 } else { 18 };
    (first..=31u8)
        .map(|b| ([172, b, 0, 0], 16u8))
        .chain((0..=255u8).map(|c| ([10, 89, c, 0], 24u8)))
        .find(|(s, p)| !taken.iter().any(|t| overlaps(*t, (*s, *p))) && !host.iter().any(|h| h.overlaps(*s, *p)))
}

/// A subnet the host itself is on, outside Kiri's bridges: an on-link route
/// or an interface's address, and the interface it is on. Or one Docker
/// will take back when it starts (`docker_saved`): not on the host now.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostNet {
    net: u32,
    mask: u32,
    dev: String,
    saved: bool,
}

fn mask4(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) }
}

impl HostNet {
    fn new(dev: &str, addr: [u8; 4], mask: u32) -> HostNet {
        HostNet { net: u32::from_be_bytes(addr) & mask, mask, dev: dev.to_owned(), saved: false }
    }

    /// Shares any address with `subnet`: a new subnet keeps clear of it.
    fn overlaps(&self, subnet: [u8; 4], prefix: u8) -> bool {
        let net = u32::from_be_bytes(subnet);
        (net & self.mask) == self.net || (self.net & mask4(prefix)) == net
    }

    /// Lies inside `subnet` (or is it), so the host routes that part of it,
    /// `gateway` included when it is in it, to `dev` and not to Kiri's
    /// bridge: the kernel picks the more specific route, and of two equal
    /// ones the older. A host network around Kiri's leaves it working.
    fn inside(&self, subnet: [u8; 4], prefix: u8) -> bool {
        self.mask >= mask4(prefix) && (self.net & mask4(prefix)) == u32::from_be_bytes(subnet)
    }

    fn contains(&self, ip: [u8; 4]) -> bool {
        u32::from_be_bytes(ip) & self.mask == self.net
    }

    fn cidr(&self) -> String {
        format!("{}/{}", ip_str(self.net.to_be_bytes()), self.mask.count_ones())
    }
}

impl Network {
    /// A subnet a host network has taken since it was chosen (#489): Docker
    /// started while Kiri was stopped, or before this network's first
    /// container, and its docker0 or br-… holds it now. A subnet from the
    /// pool (the default bridge's, or one made without `--subnet`) moves to
    /// a free one: no container can have asked for an address on it, and
    /// each gets a new one when it starts, as its address is handed out
    /// then. A subnet the user gave stays (as Docker keeps it), unless the
    /// host's network takes its gateway's part: then nothing on it would
    /// work, and its containers are refused with the reason. Returns what
    /// moved, for the log.
    fn settle(&mut self, taken: &[([u8; 4], u8)], host: &[HostNet]) -> Result<Option<String>, String> {
        if self.bridge.is_empty() || self.no_ipv4 {
            return Ok(None);
        }
        let inside: Vec<&HostNet> = host.iter().filter(|h| h.inside(self.subnet, self.prefix)).collect();
        let Some(h) = inside.first() else { return Ok(None) };
        // Only a network on the host now breaks this one now; one Docker
        // saved is reason to move a pool subnet before it does, not to
        // refuse the user's.
        let shadowed = inside.iter().find(|h| !h.saved && h.contains(self.gateway));
        let refuse = |h: &HostNet| {
            format!(
                "network {}'s subnet {} is also on this host's interface {} ({}), another engine's network (Docker's docker0 or br-…, say), so its containers could not reach their gateway {}. Remove that network, or make {} again with another --subnet",
                self.name,
                self.subnet_str(),
                h.dev,
                h.cidr(),
                ip_str(self.gateway),
                self.name
            )
        };
        if self.user_subnet {
            return match shadowed {
                Some(h) => Err(refuse(h)),
                None => Ok(None),
            };
        }
        let Some((s, p)) = free_subnet_with(taken, self.name == "bridge", host) else {
            return match shadowed {
                Some(h) => Err(refuse(h)),
                None => Ok(None),
            };
        };
        let was = format!("{} ({} {} as {})", self.subnet_str(), if h.saved { "Docker keeps" } else { "on" }, h.dev, h.cidr());
        self.subnet = s;
        self.prefix = p;
        self.gateway = [s[0], s[1], s[2], 1];
        Ok(Some(was))
    }
}

/// The host's own networks: its on-link routes plus the subnets its
/// interfaces have addresses on, outside Kiri's bridges. Only a Linux host
/// shares its routing table and interfaces with Kiri.
#[cfg(target_os = "linux")]
fn host_networks() -> Vec<HostNet> {
    let mut out = on_link_routes(&std::fs::read_to_string("/proc/net/route").unwrap_or_default());
    out.extend(host_addrs());
    out.extend(docker_saved());
    out
}

/// The bridges Docker Engine on this host will bring back when it starts
/// (docker0 and its networks), as its store keeps them: dockerd restores
/// each with its saved address and never looks at what else is on the host
/// then (moby daemon/libnetwork/drivers/bridge/bridge_store.go,
/// populateNetworks; for docker0, daemon/daemon_unix.go, selectBIP, takes
/// the address docker0 has). A Kiri network on one of them while Docker is
/// stopped would break Docker's, or Kiri's, when Docker starts. Only its
/// very first start picks a subnet around what the host has.
#[cfg(target_os = "linux")]
fn docker_saved() -> Vec<HostNet> {
    // daemon.json's data-root, else /var/lib/docker; the snap's own.
    let root = std::fs::read("/etc/docker/daemon.json")
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["data-root"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "/var/lib/docker".into());
    let mut out = Vec::new();
    for dir in [root.as_str(), "/var/snap/docker/common/var-lib-docker"] {
        let path = std::path::Path::new(dir).join("network/files/local-kv.db");
        match std::fs::File::open(&path) {
            Ok(f) => {
                use std::io::Read;
                let mut b = Vec::new();
                // A few dozen KiB for a few networks.
                if f.take(16 << 20).read_to_end(&mut b).is_ok() {
                    out.extend(docker_bridges(&b));
                }
            }
            // A store Kiri can't read (not root): Docker's default.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => out.push(HostNet { saved: true, ..HostNet::new("docker0", [172, 17, 0, 1], mask4(16)) }),
            Err(_) => {}
        }
    }
    out
}

/// Docker's saved bridges in its store's bytes. The store is a bbolt file;
/// each bridge network's configuration is a JSON object in it, written by
/// moby's networkConfiguration.MarshalJSON with sorted keys, so its
/// `"AddressIPv4":"172.17.0.1/16"` comes just before its `"BridgeName"`.
/// A copy bbolt has not reused yet may still be there after the network
/// went: a subnet more to keep clear of, and nothing worse.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn docker_bridges(b: &[u8]) -> Vec<HostNet> {
    const ADDR: &[u8] = b"\"AddressIPv4\":\"";
    const NAME: &[u8] = b"\"BridgeName\":\"";
    let quoted = |at: usize| b.get(at..).and_then(|r| r.iter().position(|c| *c == b'"').map(|end| String::from_utf8_lossy(&r[..end]).into_owned()));
    let mut out: Vec<HostNet> = Vec::new();
    let mut i = 0;
    while let Some(off) = b.get(i..).and_then(|r| r.windows(ADDR.len()).position(|w| w == ADDR)) {
        i += off + ADDR.len();
        let Some(cidr) = quoted(i) else { break };
        let parsed = cidr.split_once('/').and_then(|(a, p)| Some((parse_ip(a)?, p.parse::<u8>().ok().filter(|p| (8..=32).contains(p))?)));
        let Some((ip, prefix)) = parsed else { continue };
        let window = &b[i..(i + 512).min(b.len())];
        let dev = window.windows(NAME.len()).position(|w| w == NAME).and_then(|o| quoted(i + o + NAME.len())).filter(|n| !n.is_empty()).unwrap_or_else(|| "a Docker bridge".into());
        let h = HostNet { saved: true, ..HostNet::new(&dev, ip, mask4(prefix)) };
        if !out.contains(&h) {
            out.push(h);
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn host_networks() -> Vec<HostNet> {
    Vec::new()
}

/// The subnets this machine's interfaces have addresses on, outside Kiri's
/// bridges and loopback. An address stays on an interface whose route is
/// gone (a `noprefixroute` one) or not yet there, so this catches what the
/// routing table misses.
#[cfg(target_os = "linux")]
fn host_addrs() -> Vec<HostNet> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below; entries are read only while it lives.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Vec::new();
    }
    let mut entries = Vec::new();
    let mut p = list;
    while !p.is_null() {
        // SAFETY: `p` is non-null and an entry of the list getifaddrs returned.
        let Some(ifa) = (unsafe { p.as_ref() }) else { break };
        // SAFETY: ifa_addr is checked non-null before its family is read.
        if !ifa.ifa_addr.is_null() && !ifa.ifa_netmask.is_null() && i32::from(unsafe { (*ifa.ifa_addr).sa_family }) == libc::AF_INET {
            // SAFETY: an AF_INET address and netmask are sockaddr_in.
            let a = unsafe { (*(ifa.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr }.to_ne_bytes();
            let m = unsafe { (*(ifa.ifa_netmask as *const libc::sockaddr_in)).sin_addr.s_addr }.to_ne_bytes();
            // SAFETY: the interface name is NUL-terminated by getifaddrs.
            let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
            entries.push((name, a, m));
        }
        p = ifa.ifa_next;
    }
    // SAFETY: the list getifaddrs returned, freed once.
    unsafe { libc::freeifaddrs(list) };
    interface_networks(entries)
}

/// The network of each interface address, skipping Kiri's own bridges and
/// loopback: a bridge Kiri made earlier is its own business, and
/// 127.0.0.0/8 never overlaps the pool.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn interface_networks(entries: Vec<(String, [u8; 4], [u8; 4])>) -> Vec<HostNet> {
    entries
        .into_iter()
        .filter(|(name, a, _)| !name.starts_with("kiri") && name != "lo" && a[0] != 127)
        .map(|(name, a, m)| HostNet::new(&name, a, u32::from_be_bytes(m)))
        .collect()
}

/// The host's own IPv6 routes, outside Kiri's bridges, the default route
/// and link-local and multicast ones. Only a Linux host shares its routing
/// table with Kiri.
#[cfg(target_os = "linux")]
fn host_routes6() -> Vec<([u8; 16], u8)> {
    let text = std::fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            // dest prefix src srcprefix next-hop metric ref use flags iface, in hex
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 10 || f[9].starts_with("kiri") || f[9] == "lo" {
                return None;
            }
            let dest = u128::from_str_radix(f[0], 16).ok()?.to_be_bytes();
            let prefix = u8::from_str_radix(f[1], 16).ok()?;
            let special = dest[0] == 0xff || (dest[0] == 0xfe && dest[1] & 0xc0 == 0x80);
            (prefix != 0 && !special).then_some((dest, prefix))
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn host_routes6() -> Vec<([u8; 16], u8)> {
    Vec::new()
}

/// The routes in `/proc/net/route`'s text that put the host on a subnet, as
/// Docker reserves them since 23.0 (moby#42598): not one through a gateway
/// (OpenVPN's 0.0.0.0/1 and 128.0.0.0/1), an unreachable or prohibit one, or
/// one to 0.0.0.0 (the default route, a WireGuard 0.0.0.0/1). Kiri's own
/// bridges' routes are left out. A bridge with no carrier (Docker's docker0
/// with no containers) keeps its route, marked linkdown: it is here too.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn on_link_routes(text: &str) -> Vec<HostNet> {
    const RTF_GATEWAY: u32 = 0x2;
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // A blackhole or unreachable route has no interface: "*".
            if f.len() < 8 || f[0].starts_with("kiri") || f[0] == "*" || u32::from_str_radix(f[3], 16).ok()? & RTF_GATEWAY != 0 {
                return None;
            }
            // Little-endian hex, as the kernel prints them.
            let dest = u32::from_str_radix(f[1], 16).ok()?.swap_bytes();
            let mask = u32::from_str_radix(f[7], 16).ok()?.swap_bytes();
            (dest != 0).then(|| HostNet::new(f[0], dest.to_be_bytes(), mask))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ipam: Option<Ipam>) -> Network {
        let mut n = builtin("x", "bridge", "kiri9", [10, 5, 0, 0], [10, 5, 0, 1]);
        n.name = "app".into();
        n.prefix = 24;
        if let Some(i) = ipam {
            n.user_subnet = true;
            n.ip_range = i.ip_range;
            n.aux = i.aux;
            if let Some(g) = i.gateway {
                n.gateway = g;
            }
        }
        n
    }

    #[test]
    fn ipam_config_is_read_and_checked() {
        let v = serde_json::json!({"Driver": "default", "Config": [{"Subnet": "10.5.0.0/24", "Gateway": "10.5.0.254", "IPRange": "10.5.0.128/25", "AuxiliaryAddresses": {"router": "10.5.0.200"}}]});
        let i = parse_ipam(&v).unwrap().unwrap();
        assert_eq!((i.subnet, i.prefix, i.gateway, i.ip_range), ([10, 5, 0, 0], 24, Some([10, 5, 0, 254]), Some(([10, 5, 0, 128], 25))));
        assert_eq!(i.aux.get("router"), Some(&[10, 5, 0, 200]));
        // Compose without an ipam block, and the CLI without --subnet.
        assert_eq!(parse_ipam(&serde_json::json!({"Driver": "default", "Config": []})).unwrap(), None);
        assert_eq!(parse_ipam(&serde_json::Value::Null).unwrap(), None);

        let bad = |cfg: serde_json::Value| parse_ipam(&serde_json::json!({"Config": [cfg]})).unwrap_err();
        assert_eq!(bad(serde_json::json!({"Subnet": "10.5.0.1/24"})), "invalid subnet 10.5.0.1/24: it should be 10.5.0.0/24");
        assert!(bad(serde_json::json!({"Subnet": "nope"})).contains("invalid CIDR block notation"));
        assert!(bad(serde_json::json!({"Subnet": "10.5.0.0/24", "Gateway": "10.6.0.1"})).contains("does not contain"));
        assert!(bad(serde_json::json!({"Subnet": "10.5.0.0/24", "IPRange": "10.5.0.0/16"})).contains("ip-range"));
        assert!(bad(serde_json::json!({"Subnet": "10.5.0.0/24", "AuxiliaryAddresses": {"a": "10.9.0.1"}})).contains("auxiliary address a"));
        assert!(parse_ipam(&serde_json::json!({"Driver": "other"})).is_err());
    }

    #[test]
    fn ipv6_ipam_config_is_read_and_checked() {
        let v = serde_json::json!({"Config": [
            {"Subnet": "10.5.0.0/24"},
            {"Subnet": "fd00:5::/64", "Gateway": "fd00:5::fe", "IPRange": "fd00:5::100/120", "AuxiliaryAddresses": {"r": "fd00:5::200"}},
        ]});
        let (v4, v6) = parse_ipam_both(&v).unwrap();
        assert_eq!(v4.unwrap().subnet, [10, 5, 0, 0]);
        let v6 = v6.unwrap();
        assert_eq!((ip6_str(v6.subnet), v6.prefix), ("fd00:5::".to_owned(), 64));
        assert_eq!(v6.gateway.map(ip6_str).as_deref(), Some("fd00:5::fe"));
        assert_eq!(v6.ip_range.map(|(a, p)| (ip6_str(a), p)), Some(("fd00:5::100".to_owned(), 120)));
        assert_eq!(v6.aux.get("r").copied().map(ip6_str).as_deref(), Some("fd00:5::200"));
        // An IPv6 entry alone is fine (the IPv4 subnet comes from the pool).
        let (v4, v6) = parse_ipam_both(&serde_json::json!({"Config": [{"Subnet": "fc00:f853:ccd:e793::/64"}]})).unwrap();
        assert!(v4.is_none() && v6.is_some());

        let bad = |cfg: serde_json::Value| parse_ipam_both(&serde_json::json!({"Config": [cfg]})).unwrap_err();
        assert_eq!(bad(serde_json::json!({"Subnet": "fd00:5::1/64"})), "invalid subnet fd00:5::1/64: it should be fd00:5::/64");
        assert!(bad(serde_json::json!({"Subnet": "fd00:5::/64", "Gateway": "fd00:6::1"})).contains("does not contain"));
        assert!(bad(serde_json::json!({"Subnet": "fd00:5::/64", "Gateway": "10.0.0.1"})).contains("invalid address"));
        assert!(bad(serde_json::json!({"Subnet": "fd00:5::/127"})).contains("prefixes"));
        assert!(bad(serde_json::json!({"Subnet": "fd00:5::/64", "IPRange": "fd00:5::/48"})).contains("ip-range"));
        let two = serde_json::json!({"Config": [{"Subnet": "fd00:5::/64"}, {"Subnet": "fd00:6::/64"}]});
        assert!(parse_ipam_both(&two).unwrap_err().contains("one IPv6 subnet"));
    }

    fn nets() -> (Networks, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("kiri-net-test-{}-{}", std::process::id(), now_ns()));
        std::fs::create_dir_all(&dir).unwrap();
        (Networks::load(&dir).unwrap(), dir)
    }

    #[test]
    fn ipv6_only_networks_need_ipv6_and_no_ipv4_subnet() {
        let dir = std::env::temp_dir().join(format!("kiri-net6only-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut n = Networks::load(&dir).unwrap();
        let only = n.create("only6", Settings { no_ipv4: true, ..v6(None) }).unwrap();
        assert!(only.no_ipv4 && only.v6.is_some());
        assert!(n.create("neither", Settings { no_ipv4: true, ..Default::default() }).unwrap_err().contains("IPv4 or IPv6 must be enabled"));
        let ipam = parse_ipam(&serde_json::json!({"Config": [{"Subnet": "10.77.0.0/24"}]})).unwrap();
        assert!(n.create("both", Settings { no_ipv4: true, ipam, ..v6(None) }).unwrap_err().contains("IPv4 is disabled"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A bridge name of the user's own is refused: Kiri's firewall knows
    /// its bridges by their `kiri` prefix. An empty one is Docker's unset;
    /// one kept by an older Kiri is not shown in inspect.
    #[test]
    fn a_bridge_name_of_ones_own_is_refused_not_reported() {
        let (mut n, dir) = nets();
        let opts = |v: &str| Settings { options: serde_json::json!({BRIDGE_NAME_OPT: v, "com.docker.network.bridge.enable_ip_masquerade": "false"}), ..Default::default() };
        let e = n.create("isolated", opts("cp-isolated")).unwrap_err();
        assert!(e.starts_with("Kiri does not support the network option com.docker.network.bridge.name (cp-isolated)"), "{e}");
        assert!(n.find("isolated").is_none(), "nothing made");
        let made = n.create("isolated", opts("")).unwrap();
        assert_eq!(made.options, serde_json::json!({"com.docker.network.bridge.enable_ip_masquerade": "false"}));
        let mut old = made.clone();
        old.options[BRIDGE_NAME_OPT] = "cp-isolated".into();
        assert_eq!(shown_options(&old), made.options);
        old.options[BRIDGE_NAME_OPT] = old.bridge.clone().into();
        assert_eq!(shown_options(&old)[BRIDGE_NAME_OPT], old.bridge.as_str(), "its own name is true");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn v6(ipam6: Option<Ipam6>) -> Settings {
        Settings { labels: serde_json::json!({}), ipv6: true, ipam6, ..Default::default() }
    }

    #[test]
    fn ipv6_networks_get_a_ula_subnet_and_addresses() {
        let (mut n, dir) = nets();
        // The default bridge is IPv4-only, as Docker's.
        assert!(n.find("bridge").unwrap().v6.is_none());
        let a = n.create("a", v6(None)).unwrap();
        let b = n.create("b", v6(None)).unwrap();
        let (va, vb) = (a.v6.clone().unwrap(), b.v6.clone().unwrap());
        assert_eq!((va.subnet[0], va.prefix), (0xfd, 64));
        assert_eq!(va.subnet[..6], vb.subnet[..6], "one /48 per engine");
        assert!(!overlaps6((va.subnet, 64), (vb.subnet, 64)));
        assert_eq!(va.gateway, (u128::from_be_bytes(va.subnet) + 1).to_be_bytes());
        assert!(!va.user_subnet);
        // The same name gets the same subnet again.
        n.remove("a").unwrap();
        assert_eq!(n.create("a", v6(None)).unwrap().v6.unwrap().subnet, va.subnet);
        // Addresses: ::2 up, past the gateway; released ones come back.
        let first = n.allocate6("a", None, &[]).unwrap().unwrap();
        assert_eq!(ip6_str(first), ip6_str((u128::from_be_bytes(va.subnet) + 2).to_be_bytes()));
        assert!(n.remove("a").unwrap_err().contains("active endpoints"));
        n.release6("a", first);
        n.remove("a").unwrap();
        // IPv4-only networks hand out none, and refuse a fixed one.
        n.create("plain", Settings::default()).unwrap();
        assert_eq!(n.allocate6("plain", None, &[]), Ok(None));
        assert!(n.allocate6("plain", parse_ip6("fd00::5"), &[]).unwrap_err().contains("does not have IPv6 enabled"));
        // Persisted.
        let again = Networks::load(&dir).unwrap();
        assert_eq!(again.find("b").unwrap().v6.as_ref().unwrap().subnet, vb.subnet);
        assert!(again.find("plain").unwrap().v6.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ipv6_subnets_and_fixed_addresses_follow_dockers_rules() {
        let (mut n, dir) = nets();
        let ipam = |s: &str| parse_ipam_both(&serde_json::json!({"Config": [{"Subnet": s}]})).unwrap().1;
        let k = n.create("kind", v6(ipam("fc00:f853:ccd:e793::/64"))).unwrap();
        let kv = k.v6.unwrap();
        assert_eq!(kv.subnet_str(), "fc00:f853:ccd:e793::/64");
        assert_eq!(ip6_str(kv.gateway), "fc00:f853:ccd:e793::1");
        assert!(n.create("again", v6(ipam("fc00:f853:ccd:e793::/80"))).unwrap_err().contains("Pool overlaps"));
        // A subnet without --ipv6 is refused, not dropped.
        let no = Settings { ipam6: ipam("fd00:9::/64"), ..Default::default() };
        assert!(n.create("no", no).unwrap_err().contains("IPv6 enabled"));
        // Fixed addresses: in the subnet, free, not the gateway.
        let want = parse_ip6("fc00:f853:ccd:e793::10");
        assert_eq!(n.allocate6("kind", want, &[]), Ok(want));
        assert!(n.allocate6("kind", want, &[]).unwrap_err().starts_with("Address already in use"));
        assert!(n.allocate6("kind", parse_ip6("fc00:f853:ccd:e793::1"), &[]).unwrap_err().starts_with("Address already in use"));
        assert!(n.allocate6("kind", parse_ip6("fd00:1::5"), &[]).unwrap_err().contains("no configured subnet"));
        // Automatic ones pass over addresses other containers asked for.
        let skip = parse_ip6("fc00:f853:ccd:e793::2").unwrap();
        assert_eq!(n.allocate6("kind", None, &[skip]).unwrap().map(ip6_str).as_deref(), Some("fc00:f853:ccd:e793::3"));
        // A pool subnet takes fixed addresses too, as Docker 29 does (moby#51569).
        n.create("pool", v6(None)).unwrap();
        let pv = n.find("pool").unwrap().v6.clone().unwrap();
        let inside = (u128::from_be_bytes(pv.subnet) + 9).to_be_bytes();
        assert_eq!(n.allocate6("pool", Some(inside), &[]), Ok(Some(inside)));
        assert!(n.allocate6("pool", parse_ip6("fd00:dead::9"), &[]).unwrap_err().contains("no configured subnet"));
        // A range: only from there.
        let r = parse_ipam_both(&serde_json::json!({"Config": [{"Subnet": "fd00:7::/64", "IPRange": "fd00:7::40/126"}]})).unwrap().1;
        n.create("ranged", v6(r)).unwrap();
        let got: Vec<String> = (0..4).filter_map(|_| n.allocate6("ranged", None, &[]).unwrap()).map(ip6_str).collect();
        assert_eq!(got, ["fd00:7::40", "fd00:7::41", "fd00:7::42", "fd00:7::43"]);
        assert!(n.allocate6("ranged", None, &[]).unwrap_err().contains("no free IPv6 address"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn host(dev: &str, addr: [u8; 4], prefix: u8) -> HostNet {
        HostNet::new(dev, addr, mask4(prefix))
    }

    #[test]
    fn default_bridge_skips_a_subnet_an_interface_address_uses() {
        // docker0 with 172.17.0.1/16 and no carrier, as in #489: its route
        // is linkdown, but the address is still configured.
        let host = interface_networks(vec![
            ("docker0".into(), [172, 17, 0, 1], [255, 255, 0, 0]),
            ("lo".into(), [127, 0, 0, 1], [255, 0, 0, 0]),
            ("lo".into(), [10, 255, 255, 254], [255, 255, 255, 255]),
            ("kiri0".into(), [172, 18, 0, 1], [255, 255, 0, 0]),
        ]);
        assert_eq!(host, [self::host("docker0", [172, 17, 0, 0], 16)]);
        assert_eq!(default_bridge_subnet(&[], &host).unwrap(), ([172, 18, 0, 0], 16));
        // Nothing on the host: 172.17.0.0/16, as Docker uses.
        assert_eq!(default_bridge_subnet(&[], &[]).unwrap(), ([172, 17, 0, 0], 16));
    }

    /// Docker's store as bbolt keeps it: each bridge network's settings a
    /// JSON object among binary pages, stale copies included.
    #[test]
    fn dockers_saved_bridges_are_read_from_its_store() {
        let cfg = |addr: &str, name: &str| format!(r#"{{"AddressIPv4":"{addr}","BridgeIfaceCreator":1,"BridgeName":"{name}","DefaultBridge":false}}"#);
        let mut store = vec![0u8, 1, 2, 0xff];
        for (a, n) in [("172.17.0.1/16", "docker0"), ("172.18.0.1/16", "br-0123456789ab"), ("172.17.0.1/16", "docker0"), ("nonsense", "x")] {
            store.extend(cfg(a, n).bytes());
            store.extend([0u8; 7]);
        }
        store.extend(br#"{"AddressIPv4":"10.5.0.1/24"}"#);
        let saved = docker_bridges(&store);
        let want = |dev: &str, a: [u8; 4], p: u8| HostNet { saved: true, ..host(dev, a, p) };
        assert_eq!(saved, [want("docker0", [172, 17, 0, 0], 16), want("br-0123456789ab", [172, 18, 0, 0], 16), want("a Docker bridge", [10, 5, 0, 0], 24)]);
        assert!(docker_bridges(b"").is_empty() && docker_bridges(br#""AddressIPv4":"172.17"#).is_empty());
        // The default bridge keeps clear of them, as of a live docker0.
        assert_eq!(default_bridge_subnet(&[], &saved).unwrap(), ([172, 19, 0, 0], 16));
    }

    /// Docker stopped with docker0 and a network saved: a pool network on
    /// one moves before Docker takes it back, but a subnet the user gave
    /// is not refused for what is not on the host yet.
    #[test]
    fn a_subnet_docker_will_take_back_moves_but_is_not_refused() {
        let (mut n, dir) = nets();
        let pool = n.create("pool", Settings::default()).unwrap();
        let ipam = parse_ipam(&serde_json::json!({"Config": [{"Subnet": "172.30.0.0/16"}]})).unwrap();
        n.create("mine", Settings { ipam, ..Default::default() }).unwrap();
        let saved = |a: [u8; 4]| HostNet { saved: true, ..host("br-0123456789ab", a, 16) };
        n.settle_with("mine", &[saved([172, 30, 0, 1])]).unwrap();
        assert_eq!(n.find("mine").unwrap().subnet_str(), "172.30.0.0/16");
        n.settle_with("pool", &[saved(pool.gateway)]).unwrap();
        assert_ne!(n.find("pool").unwrap().subnet, pool.subnet);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_bridge_errors_when_the_pool_is_full() {
        // Every candidate blocked: 172.17-31 by one /12 route, the 10.89
        // /24s by one /16. No silent fallback to 172.17.0.0/16 (#489).
        let host = [host("eth1", [172, 16, 0, 0], 12), host("eth2", [10, 89, 0, 0], 16)];
        assert!(default_bridge_subnet(&[], &host).is_err());
    }

    /// Docker started after the default bridge was chosen (while Kiri was
    /// stopped, or before its first container) and its docker0 holds
    /// 172.17.0.0/16 now: the default bridge moves before its first
    /// container, and stays while containers are on it (#489).
    #[test]
    fn the_default_bridge_moves_off_a_subnet_the_host_took() {
        // Subnets as this machine's routes left them at load: compared,
        // not assumed (a Linux test host may have Docker's networks).
        let (mut n, dir) = nets();
        let was = n.find("bridge").unwrap().clone();
        let docker0 = [host("docker0", was.gateway, was.prefix)];
        n.settle_with("bridge", &docker0).unwrap();
        let b = n.find("bridge").unwrap().clone();
        assert!(!overlaps((b.subnet, b.prefix), (was.subnet, was.prefix)), "{} moved off {}", b.subnet_str(), was.subnet_str());
        assert_eq!(b.gateway, [b.subnet[0], b.subnet[1], b.subnet[2], 1]);
        let ip = n.allocate_avoiding("bridge", &[]).unwrap();
        assert!(b.contains(ip) && ip != b.gateway, "containers get addresses in the new subnet");
        // With a container on it, its bridge (and route) came first: kept.
        n.settle_with("default", &[host("docker0", b.gateway, b.prefix)]).unwrap();
        assert_eq!(n.find("bridge").unwrap().subnet, b.subnet);
        // Not saved: chosen afresh at each start, as Docker's.
        assert!(!std::fs::read_to_string(dir.join("networks.json")).unwrap_or_default().contains("kiri0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A network made without --subnet moves off a subnet the host took
    /// since, and the move is saved; one with a --subnet the user gave is
    /// kept, as Docker keeps it, unless the host's network takes its
    /// gateway's part: then its containers are refused, saying why.
    #[test]
    fn saved_networks_move_or_refuse_when_the_host_took_their_subnet() {
        let (mut n, dir) = nets();
        let pool = n.create("pool", Settings::default()).unwrap();
        let ipam = parse_ipam(&serde_json::json!({"Config": [{"Subnet": "172.30.0.0/16"}]})).unwrap();
        n.create("mine", Settings { ipam, ..Default::default() }).unwrap();
        let br = host("br-0123456789ab", pool.gateway, pool.prefix);
        n.settle_with("pool", std::slice::from_ref(&br)).unwrap();
        let moved = n.find("pool").unwrap().clone();
        assert!(!br.overlaps(moved.subnet, moved.prefix) && moved.subnet != [172, 17, 0, 0], "{}", moved.subnet_str());
        assert!(!overlaps((moved.subnet, moved.prefix), (n.find("bridge").unwrap().subnet, 16)) && !overlaps((moved.subnet, moved.prefix), ([172, 30, 0, 0], 16)));
        // Saved as it is (a load would check it against this machine again).
        let on_disk: Vec<Network> = serde_json::from_slice(&std::fs::read(dir.join("networks.json")).unwrap()).unwrap();
        assert_eq!(on_disk.iter().find(|n| n.name == "pool").map(|n| n.subnet), Some(moved.subnet), "saved");
        // The user's subnet: an equal one on the host takes the gateway.
        let e = n.settle_with("mine", &[host("br-feedfacecafe", [172, 30, 0, 1], 16)]).unwrap_err();
        assert!(e.starts_with("network mine's subnet 172.30.0.0/16 is also on this host's interface br-feedfacecafe (172.30.0.0/16)"), "{e}");
        assert!(e.contains("another --subnet"), "{e}");
        assert_eq!(n.find("mine").unwrap().subnet_str(), "172.30.0.0/16", "never moved");
        // A part of it away from the gateway, or a host network around it,
        // leaves it working: kept, as is a pool network inside a wider one.
        n.settle_with("mine", &[host("eth1", [172, 30, 9, 1], 24)]).unwrap();
        let wide = [host("eth1", [moved.subnet[0], 0, 0, 0], 8)];
        n.settle_with("mine", &wide).unwrap();
        n.settle_with("pool", &wide).unwrap();
        assert_eq!((n.find("mine").unwrap().subnet_str(), n.find("pool").unwrap().subnet), ("172.30.0.0/16".to_owned(), moved.subnet));
        // Nowhere to move to and the gateway taken: refused, not clashing.
        let full = [host("eth1", moved.gateway, moved.prefix), host("eth2", [172, 16, 0, 0], 12), host("eth3", [10, 89, 0, 0], 16)];
        let e = n.settle_with("pool", &full).unwrap_err();
        assert!(e.contains(&format!("could not reach their gateway {}", ip_str(moved.gateway))), "{e}");
        // host and none have no subnet.
        n.settle_with("host", &full).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn overlapping_subnets_are_found() {
        assert!(overlaps(([172, 18, 0, 0], 16), ([172, 18, 5, 0], 24)));
        assert!(overlaps(([172, 18, 5, 0], 24), ([172, 18, 0, 0], 16)));
        assert!(!overlaps(([172, 18, 0, 0], 16), ([172, 19, 0, 0], 16)));
        // A user's 172.18.0.0/24 keeps the automatic choice off 172.18.0.0/16.
        let free = free_subnet(&[([172, 17, 0, 0], 16), ([172, 18, 0, 0], 24)], false).unwrap();
        assert!(!overlaps(free, ([172, 18, 0, 0], 24)) && !overlaps(free, ([172, 17, 0, 0], 16)));
    }

    #[test]
    fn only_on_link_routes_reserve_subnets() {
        // As /proc/net/route prints them: an OpenVPN full tunnel (`def1`), a
        // WireGuard one with no gateway, unreachable 10.0.0.0/8, blackhole 172.16.0.0/12.
        let text = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            tun0\t00000000\t0100080A\t0003\t0\t0\t0\t00000080\t0\t0\t0\n\
            tun0\t00000080\t0100080A\t0003\t0\t0\t0\t00000080\t0\t0\t0\n\
            tun0\t0000080A\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
            wg0\t00000000\t00000000\t0001\t0\t0\t0\t00000080\t0\t0\t0\n\
            *\t0000000A\t00000000\t0201\t0\t0\t0\t000000FF\t0\t0\t0\n\
            *\t000010AC\t00000000\t0001\t0\t0\t0\t0000F0FF\t0\t0\t0\n\
            eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
            kiri1\t000012AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n";
        assert_eq!(on_link_routes(text), [host("tun0", [10, 8, 0, 0], 24), host("eth0", [192, 168, 1, 0], 24)]);
    }

    #[test]
    fn addresses_come_from_the_range_and_skip_reserved_ones() {
        let mut aux = BTreeMap::new();
        aux.insert("r".to_owned(), [10, 5, 0, 129]);
        let mut n = net(Some(Ipam { subnet: [10, 5, 0, 0], prefix: 24, gateway: Some([10, 5, 0, 128]), ip_range: Some(([10, 5, 0, 128], 30)), aux }));
        // .128 is the gateway, .129 auxiliary: .130 and .131 are left.
        assert_eq!(n.allocate_avoiding(&[]), Some([10, 5, 0, 130]));
        assert_eq!(n.allocate_avoiding(&[]), Some([10, 5, 0, 131]));
        assert_eq!(n.allocate_avoiding(&[]), None);
        n.release([10, 5, 0, 130]);
        assert_eq!(n.allocate_avoiding(&[[10, 5, 0, 130]]), None);
        // Without a range: from .1 up, past the gateway.
        let mut d = net(None);
        assert_eq!(d.allocate_avoiding(&[]), Some([10, 5, 0, 2]));
        d.release([10, 5, 0, 2]);
        let mut g = net(Some(Ipam { subnet: [10, 5, 0, 0], prefix: 24, gateway: Some([10, 5, 0, 254]), ip_range: None, aux: BTreeMap::new() }));
        assert_eq!(g.allocate_avoiding(&[]), Some([10, 5, 0, 1]));
    }

    #[test]
    fn static_addresses_follow_dockers_rules() {
        let mut n = net(Some(Ipam { subnet: [10, 5, 0, 0], prefix: 24, gateway: None, ip_range: Some(([10, 5, 0, 128], 25)), aux: BTreeMap::new() }));
        // Outside the range but inside the subnet is fine, as with Docker.
        assert_eq!(n.reserve([10, 5, 0, 10]), Ok(()));
        assert!(n.reserve([10, 5, 0, 10]).unwrap_err().starts_with("Address already in use"));
        assert!(n.reserve([10, 5, 0, 1]).unwrap_err().starts_with("Address already in use"));
        assert!(n.check_static([10, 6, 0, 10]).unwrap_err().contains("no configured subnet"));
        assert!(n.check_static([10, 5, 0, 255]).is_err());
        n.release([10, 5, 0, 10]);
        assert_eq!(n.reserve([10, 5, 0, 10]), Ok(()));
        assert!(net(None).check_static([10, 5, 0, 10]).unwrap_err().contains("user configured subnets"));
        let mut b = net(None);
        b.name = "bridge".into();
        assert!(b.check_static([10, 5, 0, 10]).unwrap_err().contains("user defined networks only"));
    }
}
