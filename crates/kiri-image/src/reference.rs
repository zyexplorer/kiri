//! Image references like `alpine`, `nginx:1.27`, `ghcr.io/o/r@sha256:…`.

use std::fmt;
use std::net::IpAddr;
use std::sync::OnceLock;

/// Docker Hub's name for itself in references, and the host its API lives on.
const DOCKER_HUB: &str = "docker.io";
const DOCKER_HUB_API: &str = "registry-1.docker.io";

/// Hosts and CIDRs from `daemon.json`'s `insecure-registries`.
pub type InsecureSource = fn() -> Vec<String>;
static INSECURE_SOURCE: OnceLock<InsecureSource> = OnceLock::new();

/// Called once by the daemon, so every pull and push honours insecure registries.
pub fn set_insecure_source(f: InsecureSource) {
    let _ = INSECURE_SOURCE.set(f);
}

fn insecure_list() -> Vec<String> {
    INSECURE_SOURCE.get().map(|f| f()).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    /// Registry as written in the reference (`docker.io`, `ghcr.io`, `localhost:5000`).
    pub registry: String,
    /// Repository path (`library/alpine`, `owner/app`).
    pub repository: String,
    /// Tag or digest (`latest`, `sha256:…`).
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid image reference: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

impl Reference {
    pub fn parse(input: &str) -> Result<Reference, ParseError> {
        let bad = || ParseError(input.to_owned());
        if input.is_empty() || input.chars().any(char::is_whitespace) {
            return Err(bad());
        }
        let (name, reference) = if let Some((name, digest)) = input.split_once('@') {
            if crate::store::digest_hex(digest).is_err() {
                return Err(bad());
            }
            // `name:tag@digest` names the digest; the tag is only a label, as
            // in Docker, but it must still be a valid one.
            let name = match name.rfind(':') {
                Some(i) if !name[i..].contains('/') => {
                    let tag = &name[i + 1..];
                    if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) {
                        return Err(bad());
                    }
                    &name[..i]
                }
                _ => name,
            };
            (name, digest.to_owned())
        } else {
            // A colon after the last slash is a tag; one before it is a port.
            match input.rfind(':') {
                Some(i) if !input[i..].contains('/') => (&input[..i], input[i + 1..].to_owned()),
                _ => (input, "latest".to_owned()),
            }
        };
        // Repository names are lowercase; tags may use any case.
        if name.is_empty() || reference.is_empty() || name.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(bad());
        }
        // The first component is a registry only if it looks like a host.
        let (registry, repository) = match name.split_once('/') {
            Some((first, rest)) if first.contains('.') || first.contains(':') || first == "localhost" => {
                (first.to_owned(), rest.to_owned())
            }
            _ => (DOCKER_HUB.to_owned(), name.to_owned()),
        };
        let repository = if registry == DOCKER_HUB && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        if repository.split('/').any(str::is_empty) {
            return Err(bad());
        }
        Ok(Reference { registry, repository, reference })
    }

    /// Host that serves the registry API.
    pub fn api_host(&self) -> &str {
        if self.registry == DOCKER_HUB { DOCKER_HUB_API } else { &self.registry }
    }

    /// `https://host`, or `http://host` for a local registry or one listed
    /// in `insecure-registries`, as Docker's daemon does.
    pub fn api_base(&self) -> String {
        format!("{}://{}", if is_insecure(self.api_host()) { "http" } else { "https" }, self.api_host())
    }

    pub fn is_digest(&self) -> bool {
        self.reference.starts_with("sha256:")
    }

    /// The short form people type and `docker images` shows: `alpine:latest`,
    /// `owner/app:1`, `ghcr.io/o/r:tag`.
    pub fn short(&self) -> String {
        let repo = if self.registry == DOCKER_HUB {
            self.repository.strip_prefix("library/").unwrap_or(&self.repository).to_owned()
        } else {
            format!("{}/{}", self.registry, self.repository)
        };
        let sep = if self.is_digest() { '@' } else { ':' };
        format!("{repo}{sep}{}", self.reference)
    }
}

/// Host name without a port (`localhost:5000` → `localhost`, `[::1]:5000` → `::1`).
pub fn host_name(host: &str) -> &str {
    if host.starts_with('[') {
        host.split(']').next().unwrap_or(host).trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or(host)
    }
}

/// `localhost:5000`, `127.0.0.1:5000`, `[::1]:5000`, `registry.localhost`.
pub fn is_local(host: &str) -> bool {
    let name = host_name(host);
    name == "localhost" || name.ends_with(".localhost") || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A `daemon.json` `insecure-registries` entry: `host`, `host:port`, or a
/// CIDR. Empty or unreadable → none.
pub fn normalize_insecure(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // A CIDR has no scheme; `http://host/` is a host, not a CIDR.
    if s.contains('/') && !s.contains("://") {
        return valid_cidr(s).then(|| s.to_owned());
    }
    let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    let s = s.split('/').next().unwrap_or(s).trim_end_matches('/');
    if s.is_empty() {
        return None;
    }
    Some(s.to_ascii_lowercase())
}

fn valid_cidr(s: &str) -> bool {
    let Some((ip, bits)) = s.split_once('/') else {
        return false;
    };
    let Ok(bits) = bits.parse::<u8>() else {
        return false;
    };
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => bits <= 32,
        Ok(IpAddr::V6(_)) => bits <= 128,
        Err(_) => false,
    }
}

fn ip_in_cidr(ip: IpAddr, cidr: &str) -> bool {
    let Some((net, bits)) = cidr.split_once('/') else {
        return false;
    };
    let Ok(bits) = bits.parse::<u32>() else {
        return false;
    };
    match (ip, net.parse::<IpAddr>()) {
        (IpAddr::V4(ip), Ok(IpAddr::V4(net))) if bits <= 32 => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            (u32::from(ip) & mask) == (u32::from(net) & mask)
        }
        (IpAddr::V6(ip), Ok(IpAddr::V6(net))) if bits <= 128 => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            (u128::from(ip) & mask) == (u128::from(net) & mask)
        }
        _ => false,
    }
}

/// Loopback, or listed in `insecure-registries` (host, host:port, or CIDR).
pub fn is_insecure(host: &str) -> bool {
    if is_local(host) {
        return true;
    }
    let host = host.to_ascii_lowercase();
    let name = host_name(&host);
    for entry in insecure_list() {
        if entry.contains('/') {
            if let Ok(ip) = name.parse::<IpAddr>()
                && ip_in_cidr(ip, &entry)
            {
                return true;
            }
            continue;
        }
        if host == entry {
            return true;
        }
        // An entry without a port matches that host on any port, as Docker does.
        if !entry.contains(':') && name == entry {
            return true;
        }
    }
    false
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.short())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> (String, String, String) {
        let r = Reference::parse(s).unwrap();
        (r.registry, r.repository, r.reference)
    }

    #[test]
    fn parses_common_forms() {
        let s = |a: &str, b: &str, c: &str| (a.to_owned(), b.to_owned(), c.to_owned());
        assert_eq!(r("alpine"), s("docker.io", "library/alpine", "latest"));
        assert_eq!(r("nginx:1.27-alpine"), s("docker.io", "library/nginx", "1.27-alpine"));
        assert_eq!(r("bitnami/redis:7"), s("docker.io", "bitnami/redis", "7"));
        assert_eq!(r("ghcr.io/o/r:v1"), s("ghcr.io", "o/r", "v1"));
        assert_eq!(r("localhost:5000/app"), s("localhost:5000", "app", "latest"));
        let d = format!("sha256:{}", "a".repeat(64));
        assert_eq!(r(&format!("alpine@{d}")), s("docker.io", "library/alpine", &d));
    }

    #[test]
    fn a_digest_wins_over_a_tag() {
        let d = format!("sha256:{}", "a".repeat(64));
        for s in [format!("kindest/node:v1.37.0@{d}"), format!("kindest/node@{d}"), format!("docker.io/kindest/node:v1@{d}")] {
            let r = Reference::parse(&s).unwrap();
            assert_eq!((r.registry.as_str(), r.repository.as_str(), r.reference.as_str()), ("docker.io", "kindest/node", d.as_str()), "{s}");
            assert_eq!(r.short(), format!("kindest/node@{d}"));
        }
        let r = Reference::parse(&format!("localhost:5000/app:1@{d}")).unwrap();
        assert_eq!((r.registry.as_str(), r.repository.as_str()), ("localhost:5000", "app"));
        let r = Reference::parse(&format!("localhost:5000/app@{d}")).unwrap();
        assert_eq!((r.registry.as_str(), r.repository.as_str()), ("localhost:5000", "app"));
        for s in [format!("alpine:@{d}"), format!("alpine:b@d@{d}")] {
            assert!(Reference::parse(&s).is_err(), "{s}");
        }
    }

    #[test]
    fn short_form_round_trips() {
        for s in ["alpine:latest", "bitnami/redis:7", "ghcr.io/o/r:v1"] {
            assert_eq!(Reference::parse(s).unwrap().short(), s);
        }
    }

    #[test]
    fn rejects_garbage() {
        let non_hex = format!("alpine@sha256:{}", "g".repeat(64));
        let traversal = format!("alpine@sha256:{}", "../".repeat(21) + "ab");
        for s in ["", "a b", "alpine:", "alpine@sha256:short", "/x", "Alpine", &non_hex, &traversal] {
            assert!(Reference::parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn only_real_loopback_registries_are_local() {
        for host in ["localhost", "localhost:5000", "registry.localhost", "127.0.0.1", "127.1.2.3:5000", "[::1]:5000"] {
            assert!(is_local(host), "{host}");
        }
        for host in ["127.evil.com", "127.0.0.1.evil.com", "example.com", "[::2]:5000"] {
            assert!(!is_local(host), "{host}");
        }
        assert_eq!(Reference::parse("127.evil.com/owner/image").unwrap().api_base(), "https://127.evil.com");
    }

    #[test]
    fn normalize_insecure_accepts_hosts_and_cidrs() {
        assert_eq!(normalize_insecure("Registry.Example:5000").as_deref(), Some("registry.example:5000"));
        assert_eq!(normalize_insecure("http://registry.example/").as_deref(), Some("registry.example"));
        assert_eq!(normalize_insecure("10.0.0.0/8").as_deref(), Some("10.0.0.0/8"));
        assert_eq!(normalize_insecure("  ").as_deref(), None);
        assert_eq!(normalize_insecure("10.0.0.0/99").as_deref(), None);
    }

    #[test]
    fn insecure_list_matches_hosts_and_cidrs() {
        set_insecure_source(|| vec!["registry.example".into(), "corp:5000".into(), "10.1.0.0/16".into()]);
        assert!(is_insecure("registry.example"));
        assert!(is_insecure("registry.example:443"));
        assert!(is_insecure("corp:5000"));
        assert!(!is_insecure("corp:5001"));
        assert!(is_insecure("10.1.2.3:5000"));
        assert!(!is_insecure("10.2.0.1"));
        assert!(is_insecure("localhost:5000"), "loopback stays insecure");
    }
}
