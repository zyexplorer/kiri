//! Kiri's images as a registry, for its BuildKit: `FROM myapp:dev` (an
//! image built or loaded here, never pushed) works as it does with Docker.
//!
//! A read-only registry API on 127.0.0.1, which containers reach at
//! host.docker.internal. buildkitd.toml names it as a mirror of every
//! registry Kiri has images from; a mirror that answers 404 is skipped, so
//! anything not here comes from the registry itself. Paths carry a secret
//! (`/v2/<token>/<registry>/<repo>/…`), so other local users can't read
//! images through it. Manifests are made from the store's records (layers,
//! config), the same bytes for the same image every time.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use serde_json::{Value, json};

use kiri_image::{Image, Reference};

use super::engine::Engine;

/// Tried first, so a builder started by buildx after a daemon restart still
/// finds the mirror its config names.
const PORT: u16 = 47_391;
const DEV_PORT: u16 = 47_392;

static LISTENING: OnceLock<Option<u16>> = OnceLock::new();

/// The mirror's secret path segment, kept in the state directory so it
/// survives restarts (the builder's config keeps naming it).
fn token() -> String {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            let path = super::state_dir()
                .ok()
                .map(|d| d.join("build-mirror-token"));
            if let Some(t) = path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|t| t.trim().to_owned())
                .filter(|t| t.len() == 32)
            {
                return t;
            }
            let t = super::engine::random_hex(16);
            if let Some(p) = path {
                let _ = save_token(&p, &t);
            }
            t
        })
        .clone()
}

/// Writes the token readable by its owner only from the moment the file
/// exists: written and then chmod-ed, it was the umask's 0644 in between,
/// and other local users could read it there.
fn save_token(path: &std::path::Path, token: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // One already there (not a valid token) is replaced, mode and all.
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(token.as_bytes())
}

/// Starts the server (at the daemon's start: a listening socket costs
/// nothing while no build asks).
pub fn start(engine: &Arc<Engine>) {
    LISTENING.get_or_init(|| {
        let wanted = if crate::flavor::dev() { DEV_PORT } else { PORT };
        let listener = TcpListener::bind(("127.0.0.1", wanted))
            .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
            .ok()?;
        let port = listener.local_addr().ok()?.port();
        let me = Arc::downgrade(engine);
        // On a Linux host the builder reaches the mirror at the default
        // bridge's gateway (mirror_host): containers have no way to the
        // host's loopback there, so it listens on that address too.
        #[cfg(target_os = "linux")]
        if let Some(gateway) = bridge_gateway(&engine.lock_net()) {
            match bind_freely(gateway, port) {
                Ok(l) => {
                    let me = me.clone();
                    let _ = std::thread::Builder::new()
                        .name("kiri-build-mirror".into())
                        .spawn(move || serve(l, me));
                }
                Err(e) => crate::diag::warn!(
                    "image mirror not on the bridge; builds pull from registries",
                    error = e
                ),
            }
        }
        std::thread::Builder::new()
            .name("kiri-build-mirror".into())
            .spawn(move || serve(listener, me))
            .ok()?;
        crate::diag::info!("image mirror for builds", port = port);
        Some(port)
    });
}

/// buildkitd.toml lines: this mirror for every registry Kiri has images
/// from (Docker Hub always), over plain HTTP.
#[cfg(target_os = "linux")]
fn mirror_host(networks: &super::network::Networks) -> String {
    bridge_gateway(networks).map_or_else(|| super::ports::HOST_ADDR.into(), super::network::ip_str)
}

/// The default bridge's gateway, which containers reach the host at.
#[cfg(target_os = "linux")]
fn bridge_gateway(networks: &super::network::Networks) -> Option<[u8; 4]> {
    networks
        .find("bridge")
        .map(|n| n.gateway)
        .filter(|g| *g != [0; 4])
}

/// A listener on `ip:port` before the address exists: the bridge (and
/// its gateway address) is made with the first container (IP_FREEBIND).
#[cfg(target_os = "linux")]
fn bind_freely(ip: [u8; 4], port: u16) -> io::Result<TcpListener> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: plain socket creation; owned at once.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is fresh and ours.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    for opt in [
        (libc::SOL_SOCKET, libc::SO_REUSEADDR),
        (libc::IPPROTO_IP, libc::IP_FREEBIND),
    ] {
        // SAFETY: setsockopt with an int of the stated size on a socket we own.
        if unsafe {
            libc::setsockopt(
                fd,
                opt.0,
                opt.1,
                (&one as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: sockaddr_in is plain data; all-zero is valid, then filled in.
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = port.to_be();
    sin.sin_addr.s_addr = u32::from_ne_bytes(ip);
    // SAFETY: bind and listen on our socket with a sockaddr_in of its size.
    unsafe {
        if libc::bind(
            fd,
            (&sin as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) != 0
            || libc::listen(fd, 128) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(TcpListener::from(sock))
}

#[cfg(not(target_os = "linux"))]
fn mirror_host(_networks: &super::network::Networks) -> String {
    super::ports::HOST_ADDR.into()
}

/// Where a build reaches this computer: the address `--add-host
/// x:host-gateway` gets, the one the mirror is reached at. On a Linux host
/// (and Windows' engine in WSL2) that's the default bridge's gateway, as
/// for `docker run`; 198.19.248.254 only leads anywhere in the Mac's VM.
pub fn host_addr(engine: &Engine) -> String {
    mirror_host(&engine.lock_net())
}

pub fn config(engine: &Engine) -> String {
    let Some(port) = LISTENING.get().copied().flatten() else {
        return String::new();
    };
    let host = format!("{}:{port}", mirror_host(&engine.lock_net()));
    let mut registries = vec!["docker.io".to_owned()];
    for i in engine.store.images().unwrap_or_default() {
        if let Ok(r) = Reference::parse(&i.name)
            && !registries.contains(&r.registry)
            && !kiri_image::reference::is_local(&r.registry)
        {
            registries.push(r.registry);
        }
    }
    let token = token();
    let mut out = String::new();
    for r in registries {
        out.push_str(&format!(
            "[registry.\"{r}\"]\n  mirrors = [\"{host}/{token}/{r}\"]\n"
        ));
    }
    out.push_str(&format!("[registry.\"{host}\"]\n  http = true\n"));
    out
}

fn serve(listener: TcpListener, engine: Weak<Engine>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let Some(engine) = engine.upgrade() else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("kiri-build-mirror".into())
            .spawn(move || {
                let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = answer(conn, &engine);
            });
    }
}

/// One request, then the connection closes.
fn answer(conn: TcpStream, engine: &Engine) -> io::Result<()> {
    let mut r = BufReader::new((&conn).take(16 << 10));
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or_default().to_owned(),
    );
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
    }
    let mut w = &conn;
    let head = |w: &mut &TcpStream, status: &str, extra: &str, len: u64| {
        write!(
            w,
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {len}\r\n{extra}\r\n"
        )
    };
    if method != "GET" && method != "HEAD" {
        return head(&mut w, "405 Method Not Allowed", "", 0);
    }
    match lookup(
        engine,
        target.split('?').next().unwrap_or_default(),
        &token(),
    ) {
        Some(Found::Manifest(bytes, digest)) => {
            head(
                &mut w,
                "200 OK",
                &format!(
                    "Content-Type: application/vnd.oci.image.manifest.v1+json\r\nDocker-Content-Digest: {digest}\r\n"
                ),
                bytes.len() as u64,
            )?;
            if method == "GET" {
                w.write_all(&bytes)?;
            }
        }
        Some(Found::Blob(path, digest)) => {
            let mut f = std::fs::File::open(&path)?;
            let len = f.metadata()?.len();
            head(
                &mut w,
                "200 OK",
                &format!(
                    "Content-Type: application/octet-stream\r\nDocker-Content-Digest: {digest}\r\n"
                ),
                len,
            )?;
            if method == "GET" {
                io::copy(&mut f, &mut w)?;
            }
        }
        None => head(&mut w, "404 Not Found", "", 0)?,
    }
    w.flush()
}

#[derive(Debug)]
enum Found {
    Manifest(Vec<u8>, String),
    Blob(std::path::PathBuf, String),
}

/// An image's manifest as Kiri serves it, and its digest.
fn manifest(store: &kiri_image::Store, i: &Image) -> Option<(Vec<u8>, String)> {
    let config_size = std::fs::metadata(store.blob_path(&i.config_digest).ok()?)
        .ok()?
        .len();
    let m = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": i.config_digest, "size": config_size},
        "layers": i.layers.iter().map(|l| json!({"mediaType": l.media_type, "digest": l.digest, "size": l.size})).collect::<Vec<Value>>(),
    });
    let bytes = serde_json::to_vec(&m).ok()?;
    let digest = kiri_image::store::sha256_digest(&bytes);
    Some((bytes, digest))
}

/// `/v2/<token>/<registry>/<repo…>/manifests/<tag|digest>` or `…/blobs/<digest>`.
fn parse(path: &str, token: &str) -> Option<(String, String, bool, String)> {
    let rest = path
        .strip_prefix("/v2/")?
        .strip_prefix(token)?
        .strip_prefix('/')?;
    let (registry, rest) = rest.split_once('/')?;
    let (repo, kind, reference) = if let Some((repo, r)) = rest.rsplit_once("/manifests/") {
        (repo, true, r)
    } else {
        let (repo, r) = rest.rsplit_once("/blobs/")?;
        (repo, false, r)
    };
    (!repo.is_empty() && !reference.is_empty() && !reference.contains('/')).then(|| {
        (
            registry.to_owned(),
            repo.to_owned(),
            kind,
            reference.to_owned(),
        )
    })
}

fn lookup(engine: &Engine, path: &str, token: &str) -> Option<Found> {
    let (registry, repo, is_manifest, reference) = parse(path, token)?;
    let images = engine.store.images().ok()?;
    let in_repo = |i: &&Image| {
        Reference::parse(&i.name).is_ok_and(|n| n.registry == registry && n.repository == repo)
    };
    if is_manifest {
        if reference.starts_with("sha256:") {
            return images
                .iter()
                .filter(in_repo)
                .find_map(|i| manifest(&engine.store, i).filter(|(_, d)| *d == reference))
                .map(|(b, d)| Found::Manifest(b, d));
        }
        let wanted = Reference::parse(&format!("{registry}/{repo}:{reference}"))
            .ok()?
            .short();
        let i = images.iter().find(|i| i.name == wanted)?;
        let (b, d) = manifest(&engine.store, i)?;
        return Some(Found::Manifest(b, d));
    }
    // Only blobs of that repository's images.
    let owned = images
        .iter()
        .filter(in_repo)
        .any(|i| i.config_digest == reference || i.layers.iter().any(|l| l.digest == reference));
    let path = engine.store.blob_path(&reference).ok()?;
    (owned && path.is_file()).then_some(Found::Blob(path, reference))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mirror_paths() {
        assert_eq!(
            parse("/v2/tok/docker.io/library/myapp/manifests/dev", "tok"),
            Some((
                "docker.io".into(),
                "library/myapp".into(),
                true,
                "dev".into()
            ))
        );
        assert_eq!(
            parse("/v2/tok/ghcr.io/o/r/blobs/sha256:ab", "tok"),
            Some(("ghcr.io".into(), "o/r".into(), false, "sha256:ab".into()))
        );
        assert_eq!(
            parse("/v2/wrong/docker.io/library/a/manifests/x", "tok"),
            None
        );
        assert_eq!(parse("/v2/tok/docker.io/manifests/x", "tok"), None);
        assert_eq!(parse("/v2/", "tok"), None);
    }

    #[test]
    fn mirror_host_matches_the_host_network() {
        let dir = std::env::temp_dir().join(format!("kiri-mirror-host-{}", std::process::id()));
        let networks = super::super::network::Networks::load(&dir).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(
            mirror_host(&networks),
            super::super::network::ip_str(networks.find("bridge").unwrap().gateway)
        );
        #[cfg(not(target_os = "linux"))]
        assert_eq!(mirror_host(&networks), super::super::ports::HOST_ADDR);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_file_is_private_from_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir().join(format!("kiri-mirror-token-{}", std::process::id()));
        // A leftover readable by everyone is replaced, not reused with its mode.
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_token(&p, "0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
        let _ = std::fs::remove_file(&p);
    }
}
