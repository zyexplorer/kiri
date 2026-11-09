//! Bind sources as clients send them. The docker CLI and Compose on Windows
//! (through kiri-win's pipe into WSL) send `C:\src:/app`, and tools that
//! drive the engine mount its socket into containers (Testcontainers' Ryuk,
//! CI runners). Both are made into paths this daemon can mount.

use std::path::Path;
use std::sync::OnceLock;

use super::engine::Error;

/// Where WSL mounts the Windows drives, and which distro the daemon runs in.
#[derive(Debug, Clone, PartialEq)]
pub struct Wsl {
    /// `/mnt/` unless `[automount] root` in /etc/wsl.conf moves it; ends in `/`.
    pub root: String,
    /// For `\\wsl$\<distro>\…` paths; when unknown, any distro is taken as this one.
    pub distro: Option<String>,
}

/// The WSL the daemon runs in (kiri-win's engine), or None on a Mac or a
/// Linux host, where a Windows path is never a path.
pub fn wsl() -> Option<&'static Wsl> {
    static WSL: OnceLock<Option<Wsl>> = OnceLock::new();
    WSL.get_or_init(detect).as_ref()
}

fn detect() -> Option<Wsl> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // WSL2's kernel names itself: 5.15.153.1-microsoft-standard-WSL2.
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
    if !release.to_ascii_lowercase().contains("microsoft") {
        return None;
    }
    let root = automount_root(&std::fs::read_to_string("/etc/wsl.conf").unwrap_or_default());
    // The boot command that starts the daemon has no WSL_DISTRO_NAME;
    // wslpath knows the name all the same.
    let distro = std::env::var("WSL_DISTRO_NAME")
        .ok()
        .filter(|d| !d.is_empty())
        .or_else(|| {
            let out = std::process::Command::new("wslpath")
                .args(["-w", "/"])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            distro_of(String::from_utf8_lossy(&out.stdout).trim())
        });
    Some(Wsl { root, distro })
}

/// `[automount] root` from /etc/wsl.conf's text, as WSL reads it.
pub fn automount_root(conf: &str) -> String {
    let mut section = String::new();
    let mut root = None;
    for line in conf.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.trim().to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if section == "automount" && key.trim().eq_ignore_ascii_case("root") {
            let value = value.trim().trim_matches('"').trim();
            if value.starts_with('/') {
                root = Some(value.to_owned());
            }
        }
    }
    let mut root = root.unwrap_or_else(|| "/mnt/".to_owned());
    if !root.ends_with('/') {
        root.push('/');
    }
    root
}

/// The distro in `\\wsl.localhost\Ubuntu\…` (or `\\wsl$\…`).
fn distro_of(unc: &str) -> Option<String> {
    let s = unc.replace('\\', "/");
    let rest = s.strip_prefix("//")?;
    let (host, path) = rest.split_once('/')?;
    if !(host.eq_ignore_ascii_case("wsl$") || host.eq_ignore_ascii_case("wsl.localhost")) {
        return None;
    }
    let distro = path.split('/').next().filter(|d| !d.is_empty())?;
    Some(distro.to_owned())
}

/// One `-v` spec (HostConfig.Binds) as source, target and options. In WSL a
/// drive letter's colon belongs to the source (`C:\src:/app:ro`), and the
/// source is made a path in this Linux.
pub fn split(spec: &str, wsl: Option<&Wsl>) -> Result<(String, String, String), Error> {
    let bad = || Error::BadRequest(format!("invalid volume specification: {spec}"));
    // `c:/app` is the volume `c` at /app, as in moby's Windows parser: a
    // drive is the source only when an absolute target follows it.
    let skip = match wsl {
        Some(_)
            if spec[drive_prefix(spec)..]
                .split(':')
                .nth(1)
                .is_some_and(|d| d.starts_with('/')) =>
        {
            drive_prefix(spec)
        }
        _ => 0,
    };
    let Some(i) = spec[skip..].find(':').map(|i| skip + i) else {
        return Err(bad());
    };
    let (src, rest) = (&spec[..i], &spec[i + 1..]);
    let (dst, opts) = match rest.split(':').collect::<Vec<_>>().as_slice() {
        [d] => (*d, ""),
        [d, o] => (*d, *o),
        _ => return Err(bad()),
    };
    let src = match wsl {
        // A volume name stays a name; only paths are translated.
        Some(w) if looks_windows(src) => source(src, w)?,
        _ => src.to_owned(),
    };
    Ok((src, dst.to_owned(), opts.to_owned()))
}

/// How many leading bytes hold a colon that is not a separator: `C:` and
/// `\\?\C:` (moby's Windows parser allows both).
fn drive_prefix(spec: &str) -> usize {
    let long = [r"\\?\", "//?/"]
        .iter()
        .find(|p| spec.starts_with(**p))
        .map_or(0, |p| p.len());
    let b = &spec.as_bytes()[long..];
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        long + 2
    } else {
        0
    }
}

/// A source in one of Windows' spellings, rather than a Linux path or a
/// volume name.
fn looks_windows(src: &str) -> bool {
    src.contains('\\') || src.starts_with("//") || drive_prefix(src) > 0
}

/// A bind source from Windows, as a path in this distro:
/// - `C:\src`, `C:/src`, `\\?\C:\src` → `/mnt/c/src`
/// - `//c/src` (Git Bash's spelling) → `/mnt/c/src`
/// - `\\wsl$\<distro>\home\me`, `\\wsl.localhost\<distro>\home\me` → `/home/me`
/// - `\\.\pipe\docker_engine` → `/var/run/docker.sock` (the engine itself)
/// - `//var/run/docker.sock` (Testcontainers on Windows) → `/var/run/docker.sock`
///
/// Anything else (a network share, another distro's files, a relative
/// path) is refused: WSL can't reach it as a path here.
pub fn source(src: &str, wsl: &Wsl) -> Result<String, Error> {
    let bad = |why: &str| Error::BadRequest(format!("invalid mount source {src}: {why}"));
    if !looks_windows(src) {
        return Ok(src.to_owned());
    }
    let long = src
        .strip_prefix(r"\\?\")
        .or_else(|| src.strip_prefix("//?/"))
        .unwrap_or(src);
    if drive_prefix(long) == 2 {
        return Ok(drive(wsl, long.as_bytes()[0], &long[3..]));
    }
    let s = src.replace('\\', "/");
    let Some(rest) = s.strip_prefix("//") else {
        return Err(bad("not an absolute path"));
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if host.len() == 1 && host.as_bytes()[0].is_ascii_alphabetic() {
        return Ok(drive(wsl, host.as_bytes()[0], path));
    }
    if host.eq_ignore_ascii_case("wsl$") || host.eq_ignore_ascii_case("wsl.localhost") {
        let (distro, inner) = path.split_once('/').unwrap_or((path, ""));
        if distro.is_empty() {
            return Err(bad("no WSL distro named"));
        }
        if let Some(ours) = wsl
            .distro
            .as_deref()
            .filter(|d| !d.eq_ignore_ascii_case(distro))
        {
            return Err(bad(&format!(
                "it is in the WSL distro {distro}, and Kiri runs in {ours}; use a path on a Windows drive or in {ours}"
            )));
        }
        return Ok(clean(&format!("/{inner}")));
    }
    if host == "." && path.eq_ignore_ascii_case("pipe/docker_engine") {
        return Ok("/var/run/docker.sock".to_owned());
    }
    // `//var/run/docker.sock`: a doubled slash, not a server.
    if !src.contains('\\') && host != "." && host != "?" && !host.is_empty() && !path.is_empty() {
        return Ok(clean(&format!("/{rest}")));
    }
    Err(bad(
        "Kiri's WSL can reach the Windows drives (C:\\…) and its own files, not network shares or devices",
    ))
}

/// A path on drive `letter`, under WSL's automount root.
fn drive(wsl: &Wsl, letter: u8, rest: &str) -> String {
    clean(&format!(
        "{}{}/{}",
        wsl.root,
        letter.to_ascii_lowercase() as char,
        rest.replace('\\', "/")
    ))
}

/// Single slashes, none at the end.
fn clean(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for c in p.chars() {
        if c == '/' && out.ends_with('/') {
            continue;
        }
        out.push(c);
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// Where a container that mounts the engine's socket is given one in the
/// VM: the agent relays it to the daemon (crates/kiri-init/src/linux/relay.rs).
#[cfg(target_os = "macos")]
const VM_ENGINE_SOCK: &str = "/run/host-services/docker.sock";

/// The paths clients mean as "the engine" when they mount it into a
/// container: Docker's own, and on the Mac the socket the client was given
/// (`DOCKER_HOST=unix://~/.kiri/run/docker.sock`, which Testcontainers
/// mounts into Ryuk).
const ENGINE_SOCKS: &[&str] = &[
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/private/var/run/docker.sock",
];

/// What to mount for a bind of `src`, when it is the engine's socket. On a
/// Mac the VM's relay to this daemon, as Docker Desktop and OrbStack give
/// the VM's engine. On a Linux host (and WSL) a socket at that path is
/// mounted as it is, as Docker does (Docker's own, or the package's link to
/// Kiri); a missing `/var/run/docker.sock` is Kiri's, rather than a new
/// empty directory.
pub fn engine_socket(src: &str) -> Option<String> {
    let own = super::socket_path().ok()?;
    let own = own.to_string_lossy();
    engine_socket_for(src, &own, |p| Path::new(p).exists())
}

fn engine_socket_for(src: &str, own: &str, exists: impl Fn(&str) -> bool) -> Option<String> {
    let src = clean(src);
    #[cfg(target_os = "macos")]
    {
        let _ = exists;
        (ENGINE_SOCKS.contains(&src.as_str()) || src == own).then(|| VM_ENGINE_SOCK.to_owned())
    }
    #[cfg(not(target_os = "macos"))]
    {
        (ENGINE_SOCKS.contains(&src.as_str()) && !exists(&src)).then(|| own.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wsl() -> Wsl {
        Wsl {
            root: "/mnt/".into(),
            distro: Some("kiri".into()),
        }
    }

    fn ok(spec: &str) -> (String, String, String) {
        split(spec, Some(&wsl())).unwrap_or_else(|e| panic!("{spec}: {e:?}"))
    }

    fn t(s: &str, d: &str, o: &str) -> (String, String, String) {
        (s.into(), d.into(), o.into())
    }

    #[test]
    fn windows_drive_binds() {
        assert_eq!(ok(r"C:\src:/app"), t("/mnt/c/src", "/app", ""));
        assert_eq!(ok("C:/src:/app:ro"), t("/mnt/c/src", "/app", "ro"));
        assert_eq!(
            ok(r"c:\Users\Me\My Project\src:/app:rw"),
            t("/mnt/c/Users/Me/My Project/src", "/app", "rw")
        );
        assert_eq!(ok(r"D:\:/data"), t("/mnt/d", "/data", ""));
        assert_eq!(ok(r"C:\src\:/app"), t("/mnt/c/src", "/app", ""));
        assert_eq!(ok(r"\\?\C:\src:/app"), t("/mnt/c/src", "/app", ""));
        assert_eq!(ok("//c/src:/app"), t("/mnt/c/src", "/app", ""));
        assert_eq!(ok("//C/src/:/app:ro,z"), t("/mnt/c/src", "/app", "ro,z"));
        // Compose resolves `./src` against a Windows project directory.
        assert_eq!(
            ok(r"C:\Users\me\proj\src:/usr/src/app:rw"),
            t("/mnt/c/Users/me/proj/src", "/usr/src/app", "rw")
        );
    }

    #[test]
    fn linux_specs_are_unchanged_in_wsl() {
        assert_eq!(ok("/home/me/src:/app"), t("/home/me/src", "/app", ""));
        assert_eq!(ok("/mnt/c/src:/app:ro"), t("/mnt/c/src", "/app", "ro"));
        assert_eq!(ok("data:/var/lib/data"), t("data", "/var/lib/data", ""));
        assert_eq!(
            ok("data:/var/lib/data:ro,nocopy"),
            t("data", "/var/lib/data", "ro,nocopy")
        );
        // A drive with no target after it: the one-letter volume `c` at /app.
        assert_eq!(ok("c:/app"), t("c", "/app", ""));
        assert_eq!(ok("C:/src:ro"), t("C", "/src", "ro"));
    }

    #[test]
    fn wsl_and_pipe_sources() {
        assert_eq!(ok(r"\\wsl$\kiri\home\me:/app"), t("/home/me", "/app", ""));
        assert_eq!(
            ok(r"\\wsl.localhost\Kiri\home\me\:/app:ro"),
            t("/home/me", "/app", "ro")
        );
        assert_eq!(ok("//wsl.localhost/kiri/srv:/srv"), t("/srv", "/srv", ""));
        assert_eq!(
            ok(r"\\.\pipe\docker_engine:/var/run/docker.sock"),
            t("/var/run/docker.sock", "/var/run/docker.sock", "")
        );
        assert_eq!(
            ok("//var/run/docker.sock:/var/run/docker.sock:rw"),
            t("/var/run/docker.sock", "/var/run/docker.sock", "rw")
        );
        // Distro unknown: taken as Kiri's own.
        let any = Wsl {
            root: "/mnt/".into(),
            distro: None,
        };
        assert_eq!(
            split(r"\\wsl$\Ubuntu\home:/h", Some(&any)).unwrap(),
            t("/home", "/h", "")
        );
    }

    #[test]
    fn refused_sources() {
        for spec in [
            r"\\wsl$\Ubuntu\home\me:/app",
            r"\\server\share\dir:/app",
            r"\\?\UNC\server\share:/app",
            r"\\.\pipe\other:/app",
            r".\src:/app",
            r"src\app:/app",
            r"C:\src:/app:ro:extra",
            "nocolon",
        ] {
            assert!(
                split(spec, Some(&wsl())).is_err(),
                "{spec} should be refused"
            );
        }
    }

    #[test]
    fn automount_root_follows_wsl_conf() {
        let w = Wsl {
            root: automount_root("[automount]\nroot = /windows/\noptions = \"metadata\"\n"),
            distro: None,
        };
        assert_eq!(w.root, "/windows/");
        assert_eq!(split(r"C:\src:/app", Some(&w)).unwrap().0, "/windows/c/src");
        assert_eq!(
            automount_root("[Automount]\n  Root=\"/\"  # all drives at /c, /d\n"),
            "/"
        );
        assert_eq!(
            split(
                r"E:\x:/x",
                Some(&Wsl {
                    root: "/".into(),
                    distro: None
                })
            )
            .unwrap()
            .0,
            "/e/x"
        );
        assert_eq!(automount_root("[automount]\nroot = /win"), "/win/");
        assert_eq!(
            automount_root("[boot]\nroot = /nope/\ncommand = kiri daemon\n"),
            "/mnt/"
        );
        assert_eq!(automount_root(""), "/mnt/");
        assert_eq!(automount_root("[automount]\nroot = relative\n"), "/mnt/");
    }

    #[test]
    fn distro_from_wslpath() {
        assert_eq!(distro_of(r"\\wsl.localhost\kiri\"), Some("kiri".into()));
        assert_eq!(
            distro_of(r"\\wsl$\Ubuntu-24.04\"),
            Some("Ubuntu-24.04".into())
        );
        assert_eq!(distro_of(r"C:\"), None);
    }

    #[test]
    fn outside_wsl_specs_split_on_every_colon() {
        assert_eq!(
            split("/src:/app:ro", None).unwrap(),
            t("/src", "/app", "ro")
        );
        assert_eq!(split("C:/app", None).unwrap(), t("C", "/app", ""));
    }

    #[test]
    fn mounts_sources() {
        let w = wsl();
        assert_eq!(source(r"C:\src", &w).unwrap(), "/mnt/c/src");
        assert_eq!(source("/home/me", &w).unwrap(), "/home/me");
        assert_eq!(source("myvolume", &w).unwrap(), "myvolume");
        assert!(source(r"\\server\share", &w).is_err());
    }

    #[test]
    fn engine_socket_aliases() {
        let none = |_: &str| false;
        let all = |_: &str| true;
        #[cfg(target_os = "macos")]
        {
            let own = "/Users/me/.kiri/run/docker.sock";
            for s in [
                "/var/run/docker.sock",
                "/private/var/run/docker.sock",
                "/run/docker.sock",
                own,
                "/Users/me/.kiri/run//docker.sock",
            ] {
                assert_eq!(
                    engine_socket_for(s, own, all).as_deref(),
                    Some(VM_ENGINE_SOCK),
                    "{s}"
                );
            }
            assert_eq!(
                engine_socket_for("/Users/me/.orbstack/run/docker.sock", own, none),
                None
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            let own = "/run/kiri/docker.sock";
            assert_eq!(
                engine_socket_for("/var/run/docker.sock", own, none).as_deref(),
                Some(own)
            );
            assert_eq!(
                engine_socket_for("/run/docker.sock", own, none).as_deref(),
                Some(own)
            );
            // Docker's socket, or the package's link to Kiri's: mounted as it is.
            assert_eq!(engine_socket_for("/var/run/docker.sock", own, all), None);
            assert_eq!(engine_socket_for(own, own, all), None);
        }
        assert_eq!(
            engine_socket_for("/srv/app.sock", "/x/docker.sock", none),
            None
        );
    }
}
