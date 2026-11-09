//! What the Mac's `setup`, `update` and `import` do for the window, as Linux
//! does it: the package manager installs and updates Kiri, the engine is a
//! system service, and there is no OrbStack to import from.

/// Why the window can't reach the engine, and what to do about it.
pub fn engine_down() -> std::io::Error {
    let socket = std::path::Path::new(crate::client::SERVICE_SOCKET);
    std::io::Error::other(match std::os::unix::net::UnixStream::connect(socket) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            "This session can't reach Kiri yet: log out and back in once, so your account's new kiri group applies"
        }
        _ if socket.exists() => "Kiri's engine isn't answering: sudo systemctl restart kiri",
        _ => "Kiri's engine isn't running: sudo systemctl enable --now kiri",
    })
}

pub mod setup {
    use std::io;

    /// The docker CLI comes from the distribution; nothing to set up after it.
    pub fn after_tools() -> io::Result<()> {
        Ok(())
    }

    /// `docker build` through Kiri's BuildKit builder, as on the Mac (needs
    /// the docker-buildx package, which the .deb pulls in).
    pub use crate::builds::ensure_builder;

    pub fn domains() -> io::Result<()> {
        Err(io::Error::other(
            "*.kiri.local isn't on Linux yet; containers answer at <name>.kiri.localhost",
        ))
    }

    pub fn remove_domains() -> io::Result<()> {
        Ok(())
    }

    /// The package's service provides /var/run/docker.sock (kiri.service).
    pub fn docker_socket_note() -> Option<String> {
        None
    }

    /// Which engine plain `docker` uses: the CLI's own switch.
    pub fn use_context(name: &str) -> io::Result<()> {
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(io::Error::other("not a context name"));
        }
        let out = std::process::Command::new("docker")
            .args(["context", "use", name])
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(
                String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            ))
        }
    }
}

pub mod update {
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    const BY_APT: &str = "on Linux, Kiri updates with your other packages (apt or dnf)";

    #[derive(serde::Serialize)]
    pub struct Release {
        pub version: String,
    }

    /// Whether `candidate` is a later version than `current`, as on the Mac.
    pub fn newer(candidate: &str, current: &str) -> bool {
        let parts = |v: &str| -> Vec<u64> {
            v.trim_start_matches('v')
                .split(['.', '-'])
                .map(|p| p.parse().unwrap_or(0))
                .collect()
        };
        parts(candidate) > parts(current)
    }

    pub fn latest() -> io::Result<Release> {
        Err(io::Error::other(BY_APT))
    }

    pub fn download(
        _: &Release,
        _: &mut dyn FnMut(u64, Option<u64>),
        _: &AtomicBool,
    ) -> io::Result<()> {
        Err(io::Error::other(BY_APT))
    }

    pub fn staged() -> Option<String> {
        None
    }

    pub fn install_staged() -> io::Result<String> {
        Err(io::Error::other(BY_APT))
    }

    pub fn app() -> io::Result<PathBuf> {
        std::env::current_exe()
    }

    /// The engine is the `kiri` system service: restarting it is root's.
    pub fn restart_engine() {
        crate::diag::info!("the engine restarts with: sudo systemctl restart kiri");
    }
}

pub mod import {
    use std::io;

    use serde_json::Value;

    /// OrbStack and Docker Desktop are the Mac's; on Linux there's none to list.
    pub fn sources() -> Vec<Value> {
        Vec::new()
    }

    fn none() -> io::Error {
        io::Error::other("importing isn't on Linux")
    }

    pub fn inventory(_: &str) -> io::Result<Value> {
        Err(none())
    }

    pub fn image(_: &str, _: &str) -> io::Result<()> {
        Err(none())
    }

    pub fn volume(_: &str, _: &str) -> io::Result<()> {
        Err(none())
    }

    pub fn container(_: &str, _: &str) -> io::Result<()> {
        Err(none())
    }
}

/// What the window checks when it opens on a Linux host, before anything
/// else, and how it fixes each: the package installs the engine but can't
/// start it for, or give access to, the person who'll use it. The page shows
/// the checklist (index.html, `onboarding`) until every required check passes,
/// so the main window never opens onto an engine it can't reach.
pub mod onboarding {
    use std::io;
    use std::path::Path;
    use std::process::{Command, Stdio};

    const SOCKET: &str = crate::client::SERVICE_SOCKET;
    /// The package's command; pkexec wants a full path.
    const KIRI: &str = "/usr/bin/kiri";

    /// Each check's state for the page: "ok", "no" (fix it), "bad" (broken:
    /// fix it), "off" (can't tell until an earlier one passes) or "restart"
    /// (fixed, applies after signing in again). `ready` when Kiri can run
    /// containers for this account; docker and builds are offered, not required
    /// (someone may keep docker on another engine on purpose).
    pub fn checks() -> serde_json::Value {
        let installed = [
            "/lib/systemd/system/kiri.service",
            "/usr/lib/systemd/system/kiri.service",
            "/etc/systemd/system/kiri.service",
        ]
        .iter()
        .any(|p| Path::new(p).exists());
        let (running, access) = match std::os::unix::net::UnixStream::connect(SOCKET) {
            Ok(_) => ("ok", "ok"),
            // The engine is there; this session may not use it. In the group
            // already but refused: the group applies at the next sign-in, and
            // the access list that would have let it in now isn't there.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => (
                "ok",
                if in_group_not_session() {
                    "restart"
                } else {
                    "no"
                },
            ),
            Err(_) if Path::new(SOCKET).exists() => ("bad", "off"),
            Err(_) => ("no", "off"),
        };
        let usable = access == "ok";
        let docker = if !usable {
            "off"
        } else if docker_uses_kiri() {
            "ok"
        } else {
            "no"
        };
        let builds = if !usable {
            "off"
        } else if builder_made() {
            "ok"
        } else {
            "no"
        };
        let ready = installed && usable;
        // Setup is done when docker and builds pass too, or someone chose to
        // skip them (docker kept on another engine on purpose): until then
        // the checklist stays and Open Kiri waits.
        let skipped = skipped();
        let done = |k: &str, state: &str| state == "ok" || skipped.iter().any(|s| s == k);
        let complete = ready && done("docker", docker) && done("builds", builds);
        serde_json::json!({
            "type": "onboarding",
            "ready": ready,
            "complete": complete,
            "skipped": skipped,
            "user": user(),
            "checks": {"engine": if installed { "ok" } else { "no" }, "running": running, "access": access, "docker": docker, "builds": builds},
        })
    }

    /// Whether to show the checklist at all: setup isn't complete.
    pub fn needed(checks: &serde_json::Value) -> bool {
        checks["complete"] != true
    }

    /// The optional checks someone chose to skip ("docker", "builds"), kept
    /// in the window's settings (~/.kiri/ui.json, `setupSkip`).
    fn skipped() -> Vec<String> {
        crate::ui::prefs()["setupSkip"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    }

    /// Skips one optional check, or takes the skip back (`undo`).
    pub fn skip(what: &str, undo: bool) -> serde_json::Value {
        let mut list = skipped();
        list.retain(|s| s != what);
        if !undo && matches!(what, "docker" | "builds") {
            list.push(what.to_owned());
        }
        serde_json::json!(list)
    }

    /// Fixes one check, or "all" of them; Err says why it couldn't. Root's
    /// part (the engine, access) is one `kiri host-access` through pkexec, so
    /// the system asks for the password once.
    pub fn fix(what: &str) -> io::Result<()> {
        let c = checks();
        let needs = |k: &str| c["checks"][k] != "ok";
        if matches!(what, "running" | "access" | "all") && (needs("running") || needs("access")) {
            host_access()?;
        }
        if matches!(what, "docker" | "all") && !docker_uses_kiri() {
            use_kiri()?;
        }
        if matches!(what, "builds" | "all") && !builder_made() {
            crate::builds::builder()?;
        }
        Ok(())
    }

    fn host_access() -> io::Result<()> {
        if which("pkexec").is_none() {
            return Err(io::Error::other(format!(
                "this system has no pkexec to ask for your password: in a terminal, run  sudo kiri host-access {}",
                user()
            )));
        }
        let out = Command::new("pkexec")
            .args([KIRI, "host-access", &user()])
            .stdin(Stdio::null())
            .output()?;
        match out.status.code() {
            Some(0) | Some(10) => Ok(()),
            // pkexec: 126 the dialog was dismissed or the password refused, 127 not allowed.
            Some(126) | Some(127) => Err(io::Error::other(
                "Not changed: the password prompt was cancelled",
            )),
            _ => {
                let why = String::from_utf8_lossy(&out.stderr);
                let why = why
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("kiri host-access failed")
                    .trim_start_matches("kiri: ");
                Err(io::Error::other(why.to_owned()))
            }
        }
    }

    /// `docker` reaches Kiri: its current endpoint is Kiri's socket, or the
    /// standard socket that leads to it. Kiri's own `docker` (the package's
    /// wrapper) does unless someone chose another context.
    pub fn docker_uses_kiri() -> bool {
        let Some(docker) = crate::client::docker_cli() else {
            return false;
        };
        let Ok(out) = Command::new(docker)
            .args([
                "context",
                "inspect",
                "--format",
                "{{.Endpoints.docker.Host}}",
            ])
            .stderr(Stdio::null())
            .output()
        else {
            return false;
        };
        let host = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        match host.strip_prefix("unix://") {
            Some(p) if p == SOCKET => true,
            Some(p) => std::fs::canonicalize(p).is_ok_and(|real| real == Path::new(SOCKET)),
            None => false,
        }
    }

    /// The `kiri` docker context, made if missing, as the current one.
    fn use_kiri() -> io::Result<()> {
        let docker = crate::client::docker_cli().ok_or_else(|| {
            io::Error::other(
                "no docker command here: install the kiri package (not kiri-lite) or docker's CLI",
            )
        })?;
        let _ = Command::new(&docker)
            .args([
                "context",
                "create",
                "kiri",
                "--docker",
                &format!("host=unix://{SOCKET}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        super::setup::use_context("kiri")
    }

    fn builder_made() -> bool {
        let dir = std::env::var_os("BUILDX_CONFIG")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                let config = std::env::var_os("DOCKER_CONFIG")
                    .map(std::path::PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".docker")))?;
                Some(config.join("buildx"))
            });
        dir.is_some_and(|d| {
            d.join("instances")
                .join(crate::builds::BUILDER.get())
                .exists()
        })
    }

    /// In the kiri group by /etc/group, but this process (this session) isn't.
    fn in_group_not_session() -> bool {
        let me = user();
        let listed = crate::daemon::access::members("kiri").is_ok_and(|m| m.contains(&me));
        listed && !session_has_group("kiri")
    }

    fn session_has_group(group: &str) -> bool {
        let Ok(name) = std::ffi::CString::new(group) else {
            return false;
        };
        // SAFETY: getgrnam with a valid C string, read at once; getgroups
        // fills at most the buffer's length.
        unsafe {
            let g = libc::getgrnam(name.as_ptr());
            if g.is_null() {
                return false;
            }
            let gid = (*g).gr_gid;
            if libc::getegid() == gid {
                return true;
            }
            let mut groups = vec![0 as libc::gid_t; 256];
            let n = libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr());
            n > 0 && groups[..n as usize].contains(&gid)
        }
    }

    fn user() -> String {
        // SAFETY: getpwuid on our own uid; the name is copied at once.
        let from_uid = unsafe {
            let p = libc::getpwuid(libc::getuid());
            (!p.is_null()).then(|| {
                std::ffi::CStr::from_ptr((*p).pw_name)
                    .to_string_lossy()
                    .into_owned()
            })
        };
        from_uid
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_default()
    }

    fn which(cmd: &str) -> Option<std::path::PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(cmd))
            .find(|p| p.is_file())
    }

    /// Ends this session, so the next sign-in has the kiri group: the
    /// desktop's own log-out (it asks to confirm), else logind.
    pub fn log_out() -> io::Result<()> {
        if which("gnome-session-quit").is_some()
            && Command::new("gnome-session-quit")
                .arg("--logout")
                .status()?
                .success()
        {
            return Ok(());
        }
        if let Some(qdbus) = ["qdbus6", "qdbus"].iter().find(|q| which(q).is_some())
            && Command::new(qdbus)
                .args(["org.kde.Shutdown", "/Shutdown", "logout"])
                .status()?
                .success()
        {
            return Ok(());
        }
        let session = std::env::var("XDG_SESSION_ID").map_err(|_| {
            io::Error::other("couldn't tell which session to end: log out from the system menu")
        })?;
        if Command::new("loginctl")
            .args(["terminate-session", &session])
            .status()?
            .success()
        {
            Ok(())
        } else {
            Err(io::Error::other("couldn't log out: use the system menu"))
        }
    }

    pub fn restart() -> io::Result<()> {
        if which("gnome-session-quit").is_some()
            && Command::new("gnome-session-quit")
                .arg("--reboot")
                .status()?
                .success()
        {
            return Ok(());
        }
        if Command::new("systemctl").arg("reboot").status()?.success() {
            Ok(())
        } else {
            Err(io::Error::other("couldn't restart: use the system menu"))
        }
    }
}
