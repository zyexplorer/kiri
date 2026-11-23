//! Pull-through mirrors of Docker Hub, as daemon.json's `registry-mirrors`
//! (Settings → Registries, `kiri registry mirror`) lists them: a host (with
//! an optional port) and, for mirrors that keep the upstream registry in the
//! path (`mirror.example.com/docker.io`, say), a path prefix. As Docker's,
//! they apply to `docker.io` only: other registries are pulled from directly.

use crate::reference::is_local;

/// The longest path a mirror's address may have: a registry name or two.
const MAX_PREFIX: usize = 255;

/// One mirror: `host` is a bare `host[:port]`, as `Reference::registry` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mirror {
    pub host: String,
    pub prefix: Option<String>,
    /// Written `http://`: asked over plain HTTP, as Docker asks such a
    /// mirror. A mirror is never sent credentials, so none go in the clear.
    pub http: bool,
}

impl Mirror {
    /// Parses what a person types in Settings or passes to
    /// `kiri registry mirror add`: `mirror.example.com`, `https://mirror.example.com`,
    /// `https://mirror.example.com/docker.io`, `http://mirror.lan:5000`.
    ///
    /// A mirror is HTTPS unless it is written `http://`, as with Docker, and
    /// is asked over that scheme only: never probed, never down from HTTPS
    /// to plain HTTP. On this machine or a host in `insecure-registries`
    /// its certificate goes unchecked. A trailing `/v2` is not part of the
    /// address.
    pub fn parse(input: &str) -> Result<Mirror, String> {
        let text = input.trim();
        if text.is_empty() {
            return Err("a mirror address cannot be empty".into());
        }
        let with_scheme = if text.contains("://") { text.to_owned() } else { format!("https://{text}") };
        let uri: ureq::http::Uri = with_scheme.parse().map_err(|_| format!("not an address: {text}"))?;
        let scheme = uri.scheme_str().unwrap_or("https").to_ascii_lowercase();
        let authority = uri.authority().ok_or_else(|| format!("no host in {text}"))?;
        if authority.as_str().contains('@') {
            return Err(format!("a mirror address has no username or password: {text}"));
        }
        let written = authority.as_str().to_ascii_lowercase();
        if crate::config::is_hub(&written) {
            return Err("Docker Hub is not a mirror of itself: it is always reached directly, over verified HTTPS".into());
        }
        let host = crate::config::canonical(&written).ok_or_else(|| format!("not a host name: {written}"))?;
        if scheme != "https" && scheme != "http" {
            return Err(format!("a mirror is an http or https address: {text}"));
        }
        if uri.query().is_some() || text.contains('#') {
            return Err(format!("a mirror address has no query or fragment: {text}"));
        }
        // Both `https://host` and `https://host/v2` name the same service.
        let path = uri.path().trim_matches('/');
        let path = if path == "v2" { "" } else { path.strip_suffix("/v2").unwrap_or(path) };
        let path = path.trim_matches('/');
        // Path segments a repository could have: no empty, `.` or `..` one,
        // which would put the request somewhere else on the mirror.
        let segment = |seg: &str| !seg.is_empty() && seg != "." && seg != ".." && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b));
        if !path.is_empty() && (path.len() > MAX_PREFIX || !path.split('/').all(segment)) {
            return Err(format!("not a mirror path: {path}"));
        }
        Ok(Mirror { http: scheme == "http", host, prefix: (!path.is_empty()).then(|| path.to_owned()) })
    }

    /// What Settings shows, `kiri registry mirror` prints and daemon.json
    /// keeps.
    pub fn as_url(&self) -> String {
        let scheme = if self.http { "http" } else { "https" };
        match &self.prefix {
            Some(prefix) => format!("{scheme}://{}/{prefix}", self.host),
            None => format!("{scheme}://{}", self.host),
        }
    }

    /// BuildKit's `mirrors` entry: `host`, or `host/prefix` for a mirror that
    /// keeps the upstream in the path.
    pub fn buildkit(&self) -> String {
        match &self.prefix {
            Some(prefix) => format!("{}/{prefix}", self.host),
            None => self.host.clone(),
        }
    }

    pub fn is_local(&self) -> bool {
        is_local(&self.host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_addresses_people_type() {
        let m = Mirror::parse("mirror.example.com").unwrap();
        assert_eq!(m.host, "mirror.example.com");
        assert_eq!(m.prefix, None);
        assert_eq!(m.as_url(), "https://mirror.example.com");

        assert_eq!(Mirror::parse("https://Mirror.Example.com/").unwrap().as_url(), "https://mirror.example.com");
        assert_eq!(Mirror::parse("https://mirror.example.net/docker.io").unwrap().prefix.as_deref(), Some("docker.io"));
        assert_eq!(Mirror::parse("https://mirror.example.com/v2").unwrap().prefix, None);
        // An explicit https:// stays so, loopback or not, and so does a
        // bare address.
        assert_eq!(Mirror::parse("https://127.0.0.1:5000").unwrap().as_url(), "https://127.0.0.1:5000");
        assert_eq!(Mirror::parse("http://127.0.0.1:5000").unwrap().as_url(), "http://127.0.0.1:5000");
        assert!(!Mirror::parse("127.0.0.1:5000").unwrap().http);
        assert_eq!(Mirror::parse("https://mirror.example.net/docker.io").unwrap().buildkit(), "mirror.example.net/docker.io");
        // Docker asks a mirror written http:// over plain HTTP; so does Kiri.
        let plain = Mirror::parse("http://Mirror.Lan:5000/").unwrap();
        assert_eq!((plain.host.as_str(), plain.http, plain.as_url().as_str()), ("mirror.lan:5000", true, "http://mirror.lan:5000"));
        assert!(!Mirror::parse("mirror.lan:5000").unwrap().http);
    }

    #[test]
    fn docker_hub_is_not_a_mirror() {
        for hub in ["docker.io", "http://docker.io", "https://docker.io", "http://index.docker.io", "http://registry-1.docker.io", "https://registry-1.docker.io:443/x", "http://registry-1.docker.io.", "https://docker.io."] {
            assert!(Mirror::parse(hub).is_err(), "{hub}");
        }
    }

    #[test]
    fn refuses_what_is_not_a_mirror() {
        for bad in ["", "   ", "https://", "ftp://mirror.example.com", "https://user:pw@mirror.example.com", "https://mirror.example.com/a b", "https://mirror.example.com/?x=1", "https://mirror.example.com/#x"] {
            assert!(Mirror::parse(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_mirror_is_a_plain_host_and_a_clean_path() {
        // The same checks as an insecure host: ports of plain digits, real labels.
        for bad in ["https://mirror.lan:", "https://mirror.lan:0", "https://mirror.lan:+5000", "https://a..b", "https://mirror.lan:99999"] {
            assert!(Mirror::parse(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(Mirror::parse(&format!("https://{}.example.com", "a".repeat(64))).is_err());
        // Paths that step out of the prefix, or say nothing.
        for bad in ["https://m.example.com/a/../b", "https://m.example.com/..", "https://m.example.com/a//b", "https://m.example.com/./a"] {
            assert!(Mirror::parse(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(Mirror::parse(&format!("https://m.example.com/{}", "a".repeat(256))).is_err());
        assert_eq!(Mirror::parse("https://[FD00:0::1]:5000").unwrap().host, "[fd00::1]:5000");
    }
}
