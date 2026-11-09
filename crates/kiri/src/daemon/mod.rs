//! `kiri daemon`: owns the VM and serves the Docker Engine API on a Unix
//! socket. The docker CLI (context `kiri`), the kiri CLI and the window are
//! all clients of it.

#[cfg(target_os = "linux")]
pub mod access;
mod api;
mod binds;
mod builder;
mod daemon_cfg;

/// Hub registry mirrors from `~/.kiri/daemon.json` (Settings → Network).
pub(crate) fn hub_mirrors() -> Vec<String> {
    daemon_cfg::mirrors()
}

/// Insecure registry hosts/CIDRs from `~/.kiri/daemon.json`.
pub(crate) fn insecure_registries() -> Vec<String> {
    daemon_cfg::insecure()
}
pub mod dns;
mod domains;
mod engine;
mod filters;
mod health;
mod hostopts;
mod http;
mod logfile;
mod mirror;
mod names;
mod network;
#[cfg(target_os = "macos")]
mod nfs;
mod ops;
pub(crate) mod ports;
pub mod proxy;
mod pulls;
pub mod time;
pub mod tls;
mod vmcpu;
mod vmlink;
mod volumes;

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::diag;

/// `~/.kiri` (`~/.kiri-dev` for Kiri Dev): the daemon's state (the CA key, proxy settings). Always under
/// HOME: `KIRI_SOCKET` may point the socket anywhere, but it must not move
/// private state into a directory another user can prepare.
pub fn state_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(home.join(crate::flavor::STATE.get()))
}

/// `~/.kiri/run/docker.sock`, beside OrbStack's `~/.orbstack/run/docker.sock`.
pub fn socket_path() -> io::Result<PathBuf> {
    if let Some(s) = std::env::var_os("KIRI_SOCKET") {
        return Ok(PathBuf::from(s));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(home
        .join(crate::flavor::STATE.get())
        .join("run/docker.sock"))
}

/// Raises the soft open-file limit to the hard one, as Go does for dockerd.
/// launchd starts the daemon with 256, and each relayed connection holds
/// four or five descriptors: a LAN client opening ~60 connections to a
/// published port ran it out, and the daemon fell over with its VM.
fn raise_open_files() {
    // SAFETY: rlimit is plain data; get/setrlimit read and write it.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        // macOS refuses more than OPEN_MAX for the soft limit.
        let want = if cfg!(target_os = "macos") {
            lim.rlim_max.min(10240)
        } else {
            lim.rlim_max
        };
        if lim.rlim_cur < want {
            lim.rlim_cur = want;
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}

pub fn run(_args: &[String]) -> io::Result<()> {
    let t0 = Instant::now();
    raise_open_files();
    let socket = socket_path()?;
    if UnixStream::connect(&socket).is_ok() {
        // Not an error: launchd (KeepAlive on failure) must not retry it.
        diag::info!("engine already running", socket = socket.display());
        return Ok(());
    }
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&socket); // stale, from a daemon that died
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    // On a Linux host the daemon runs as root; `KIRI_SOCKET_GROUP=kiri`
    // lets that group's users in, as the docker group does for dockerd.
    let access_group = std::env::var_os("KIRI_SOCKET_GROUP");
    if let Some(group) = &access_group {
        socket_group(&socket, group)?;
    }
    // Each member by name too, so one just added needn't sign in again; and
    // connections are checked against the group when made and while open, so
    // a member removed is let go at once, whatever the list still says.
    #[cfg(target_os = "linux")]
    let gate = match &access_group {
        Some(group) => {
            let gate = access::Gate::new(&socket, group)?;
            match gate.grant() {
                Ok(n) => diag::info!("socket access list written", members = n),
                Err(e) => diag::warn!(
                    "socket access list not written: new members sign in again first",
                    error = e
                ),
            }
            gate.clone().watch()?;
            Some(gate)
        }
        None => None,
    };

    // SIGINT/SIGTERM shut down cleanly: power the guest off, remove the socket.
    // Blocked here so every thread inherits it; one thread waits for them.
    // SAFETY: plain signal-mask calls on a local set.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };

    let store = kiri_image::Store::open(kiri_image::Store::default_root()?)?;
    let engine = engine::Engine::boot(store)
        .inspect_err(|e| diag::error!("engine did not start", error = e))?;
    diag::info!(
        "engine ready",
        version = env!("CARGO_PKG_VERSION"),
        ms = t0.elapsed().as_millis(),
        socket = format!("unix://{}", socket.display())
    );
    // Crash files from earlier runs, if the person said they may go.
    diag::report::spawn_crash_sender(Duration::from_secs(30), Some(Duration::from_secs(6 * 3600)));
    // Container files in Finder, at ~/Kiri. A Linux host has them on its
    // own disk already.
    #[cfg(target_os = "macos")]
    finder(&engine);
    kiri_image::set_proxy_source(proxy::for_registry);
    kiri_image::set_mirrors_source(daemon_cfg::for_registry);
    kiri_image::set_insecure_source(daemon_cfg::for_insecure);
    kiri_image::set_downloads_source(daemon_cfg::for_downloads);
    kiri_image::set_uploads_source(daemon_cfg::for_uploads);
    // The managed BuildKit stops itself when builds have been idle a while.
    builder::watch(engine.clone());
    // A Kubernetes node the engine restarted gets its kubectl context.
    crate::k8s::sync_at_start();
    // Kiri Dev leaves *.kiri.local and ports 80 and 443 to the installed
    // Kiri, unless given ports of its own.
    let routers = !crate::flavor::dev() || std::env::var_os("KIRI_HTTP_PORT").is_some();
    if crate::flavor::dev() {
        diag::info!(format!(
            "{}: container domains off (KIRI_HTTP_PORT and KIRI_HTTPS_PORT turn them on)",
            crate::flavor::APP
        ));
    } else if let Err(e) = dns::start() {
        diag::warn!("*.kiri.local off", error = e);
    }
    if routers {
        match domains::start_tls(engine.clone()) {
            Ok(port) => diag::info!(
                "HTTPS for container domains",
                port = port,
                trusted = tls::trusted()
            ),
            Err(e) => diag::warn!("HTTPS for container domains off", error = e),
        }
        match domains::start(engine.clone()) {
            Ok(port) => diag::info!(format!(
                "container domains on http://<name>.{}{}",
                if dns::installed() {
                    "kiri.local"
                } else {
                    "kiri.localhost"
                },
                if port == 80 {
                    String::new()
                } else {
                    format!(":{port}")
                }
            )),
            Err(e) => diag::warn!("container domains off", error = e),
        }
    }

    watch_own_app(engine.clone(), socket.clone());

    let on_signal = engine.clone();
    let sock = socket.clone();
    std::thread::Builder::new()
        .name("kiri-signals".into())
        .spawn(move || {
            let mut sig = 0;
            // SAFETY: waits on the blocked set; writes the signal number to `sig`.
            unsafe { libc::sigwait(&set, &mut sig) };
            diag::info!("engine shutting down", signal = sig);
            #[cfg(target_os = "macos")]
            nfs::unmount();
            let _ = std::fs::remove_file(&sock);
            on_signal.shutdown();
            // Give the guest a moment to power off before the VM is released.
            std::thread::sleep(std::time::Duration::from_millis(50));
            std::process::exit(0);
        })?;

    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let engine = engine.clone();
        #[cfg(target_os = "linux")]
        let gate = gate.clone();
        let spawned = std::thread::Builder::new()
            .name("kiri-api".into())
            .spawn(move || {
                // Checked here, not in the accept loop: a slow user lookup holds up
                // this connection only.
                #[cfg(target_os = "linux")]
                let _seat = match &gate {
                    Some(gate) => match gate.admit(&conn) {
                        Some(seat) => Some(seat),
                        None => return,
                    },
                    None => None,
                };
                let _ = api::serve(conn, engine);
            });
        if let Err(e) = spawned {
            diag::error!("cannot start a connection thread", error = e);
        }
    }
    Ok(())
}

/// When the app this daemon runs from is deleted (appwatch.rs): docker
/// build handed back (buildwith.rs: what was there before; the choice
/// stays, so the app taken up again takes it again), and the engine stops.
fn watch_own_app(engine: std::sync::Arc<engine::Engine>, socket: std::path::PathBuf) {
    crate::appwatch::watch(move |exe| {
        diag::error!(
            "the app this engine runs from was deleted; stopping and handing docker build back",
            path = exe.display()
        );
        if let Err(e) = crate::buildwith::places().and_then(|p| p.give_back()) {
            diag::warn!("docker build not handed back", error = e);
        }
        #[cfg(target_os = "macos")]
        nfs::unmount();
        let _ = std::fs::remove_file(&socket);
        engine.shutdown();
        std::thread::sleep(Duration::from_millis(50));
        // 0: launchd restarts the engine on failure only, and there is
        // nothing left to restart.
        std::process::exit(0);
    });
}

/// Serves container files over NFS and mounts them at ~/Kiri.
/// KIRI_NO_FINDER=1 skips the mount.
#[cfg(target_os = "macos")]
fn finder(engine: &std::sync::Arc<engine::Engine>) {
    match nfs::start(engine.clone()) {
        Ok(socket) if std::env::var_os("KIRI_NO_FINDER").is_none() => match nfs::mount(&socket) {
            Ok(dir) => diag::info!("container files in Finder", at = dir.display()),
            Err(e) => diag::warn!("Finder mount failed", error = e, nfs = socket.display()),
        },
        Ok(socket) => diag::info!("container files over NFS", at = socket.display()),
        Err(e) => diag::warn!("container files off", error = e),
    }
}

/// Gives `group` read-write access to the socket.
fn socket_group(socket: &std::path::Path, group: &std::ffi::OsStr) -> io::Result<()> {
    let name = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(group))
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: getgrnam with a valid C string; the result is read at once.
    let gid = unsafe {
        let g = libc::getgrnam(name.as_ptr());
        if g.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no group {}", group.to_string_lossy()),
            ));
        }
        (*g).gr_gid
    };
    std::os::unix::fs::chown(socket, None, Some(gid))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))
}
