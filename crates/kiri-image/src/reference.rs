//! Image references like `alpine`, `nginx:1.27`, `ghcr.io/o/r@sha256:…`.

use std::fmt;

/// Docker Hub's name for itself in references, and the host its API lives on.
const DOCKER_HUB: &str = "docker.io";
const DOCKER_HUB_API: &str = "registry-1.docker.io";

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

    /// `https://host`, or `http://host` for a registry on this machine,
    /// which Docker also treats as insecure by default.
    pub fn api_base(&self) -> String {
        format!("{}://{}", if is_local(self.api_host()) { "http" } else { "https" }, self.api_host())
    }

    pub fn is_digest(&self) -> bool {
        self.reference.starts_with("sha256:")
    }

    /// The same image, served by `host`: a mirror of this reference's
    /// registry. `prefix` goes before the repository for mirrors that keep
    /// the upstream registry in the path (`mirror.example.com/docker.io`); the
    /// tag or digest is kept as it is.
    pub fn via(&self, host: &str, prefix: Option<&str>) -> Reference {
        let repository = match prefix.map(str::trim).filter(|p| !p.is_empty()) {
            Some(prefix) => format!("{}/{}", prefix.trim_matches('/'), self.repository),
            None => self.repository.clone(),
        };
        Reference { registry: host.to_owned(), repository, reference: self.reference.clone() }
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

/// `localhost:5000`, `127.0.0.1:5000`, `[::1]:5000`: this machine by address
/// or as `localhost` itself, which resolves here without asking a name
/// server, as Docker's loopback set has it. A name under `.localhost` is not:
/// a resolver may send it to DNS.
pub fn is_local(host: &str) -> bool {
    let name = if host.starts_with('[') { host.split(']').next().unwrap_or(host).trim_start_matches('[') } else { host.split(':').next().unwrap_or(host) };
    name.eq_ignore_ascii_case("localhost") || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
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
    fn a_mirror_keeps_the_image_and_its_tag() {
        let nginx = Reference::parse("nginx:1.27").unwrap();
        let mirrored = nginx.via("mirror.example.com", None);
        assert_eq!(mirrored.registry, "mirror.example.com");
        assert_eq!(mirrored.repository, "library/nginx");
        assert_eq!(mirrored.reference, "1.27");
        assert_eq!(nginx.short(), "nginx:1.27", "the name people see is unchanged");

        let prefixed = nginx.via("mirror.example.net", Some("docker.io"));
        assert_eq!(prefixed.repository, "docker.io/library/nginx");

        let digest = format!("sha256:{}", "a".repeat(64));
        let pinned = Reference::parse(&format!("alpine@{digest}")).unwrap();
        assert_eq!(pinned.via("127.0.0.1:5000", None).reference, digest);
    }

    #[test]
    fn only_real_loopback_registries_are_local() {
        for host in ["localhost", "localhost:5000", "LocalHost:5000", "127.0.0.1", "127.1.2.3:5000", "[::1]:5000"] {
            assert!(is_local(host), "{host}");
        }
        for host in ["127.evil.com", "127.0.0.1.evil.com", "example.com", "[::2]:5000", "registry.localhost", "evil.localhost:5000"] {
            assert!(!is_local(host), "{host}");
        }
        assert_eq!(Reference::parse("127.evil.com/owner/image").unwrap().api_base(), "https://127.evil.com");
    }
}
