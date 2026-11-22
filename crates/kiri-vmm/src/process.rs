//! Linux: no VM. The guest agent (`kiri-init`) runs as a root process on the
//! host's kernel, and the vsock ports become unix sockets in a private
//! directory: the agent listens on `g<port>.sock`, the daemon on
//! `h<port>.sock`. Everything above this module is unchanged.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The same fields as on macOS, so callers build one `Config`. On Linux,
/// `initrd` names the agent binary (`kiri-init`); kernel, disk, shares,
/// memory and network fields do not apply: the host provides them.
#[derive(Debug)]
pub struct Config {
    pub cpus: usize,
    pub memory_mib: u64,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub cmdline: String,
    pub console: Option<OwnedFd>,
    pub agent_port: u32,
    pub disk: Option<PathBuf>,
    pub share: Option<crate::Share>,
    pub rosetta_tag: Option<String>,
    pub nat_mac: Option<String>,
}

#[derive(Debug)]
pub enum Event {
    /// The agent connected. The fd is a stream socket owned by the caller.
    GuestConnected(OwnedFd),
    /// The agent exited, with the reason if it failed.
    Stopped(Option<String>),
}

#[derive(Debug)]
pub struct Vm {
    dir: PathBuf,
    child: Arc<Mutex<Child>>,
    events: Receiver<Event>,
    closing: Arc<AtomicBool>,
    retired: bool,
}

fn runtime_directory(root: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    root.join(format!("kiri-agent-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// A connection to one of the agent's ports.
#[derive(Debug)]
pub struct VsockStream {
    pub fd: OwnedFd,
}

/// What `Vm::listen` calls for each connection the agent makes.
pub type Handler = Box<dyn Fn(VsockStream) + Send + Sync>;

/// Rosetta is a macOS feature.
pub fn rosetta_installed() -> bool {
    false
}

impl Vm {
    /// Containers need root on a Linux host (namespaces, cgroups, netlink).
    pub fn is_supported() -> bool {
        // SAFETY: geteuid cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    pub fn start(cfg: Config) -> io::Result<Vm> {
        // The agent's sockets take root commands, so their folder must be
        // root's alone: /run, not an inherited XDG_RUNTIME_DIR (`sudo -E`
        // keeps the user's, where they could have put the folder first).
        let dir = runtime_directory(Path::new("/run"));
        // Made private in one step; an existing entry (a planted folder or
        // link) fails it rather than being used.
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700).create(&dir)?;
        let (sender, events) = mpsc::channel();
        // The agent's first dial: the host's control connection.
        let control = UnixListener::bind(dir.join(format!("h{}.sock", cfg.agent_port)))?;
        let tx = sender.clone();
        std::thread::Builder::new().name("kiri-agent-accept".into()).spawn(move || {
            if let Ok((s, _)) = control.accept() {
                let _ = tx.send(Event::GuestConnected(OwnedFd::from(s)));
            }
        })?;
        let mut cmd = Command::new(&cfg.initrd);
        cmd.env("KIRI_AGENT_DIR", &dir).env_remove("KIRI_SOCKET");
        if let Some(fd) = cfg.console {
            cmd.stderr(std::process::Stdio::from(fd.try_clone()?)).stdout(std::process::Stdio::from(fd));
        } else {
            cmd.stdout(std::process::Stdio::null());
        }
        let child = Arc::new(Mutex::new(cmd.spawn().map_err(|e| io::Error::new(e.kind(), format!("starting {}: {e}", cfg.initrd.display())))?));
        let (waiter, tx) = (child.clone(), sender);
        std::thread::Builder::new().name("kiri-agent-wait".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_millis(100));
            let status = waiter.lock().unwrap_or_else(|e| e.into_inner()).try_wait();
            match status {
                Ok(Some(s)) => {
                    let _ = tx.send(Event::Stopped((!s.success()).then(|| format!("the agent exited: {s}"))));
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = tx.send(Event::Stopped(Some(e.to_string())));
                    return;
                }
            }
        })?;
        Ok(Vm { dir, child, events, closing: Arc::new(AtomicBool::new(false)), retired: false })
    }

    pub fn next_event(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }

    pub fn force_stop(&self) {
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }

    pub fn is_running(&self) -> bool {
        matches!(self.child.lock().unwrap_or_else(|e| e.into_inner()).try_wait(), Ok(None))
    }

    /// Confirms process exit and completes cleanup before a successor starts.
    pub fn retire(&mut self, graceful: Duration, forced: Duration) -> io::Result<()> {
        if self.retired {
            return Ok(());
        }
        self.closing.store(true, Ordering::Release);
        let child = &self.child;
        super::stop::retire(
            || child.lock().unwrap_or_else(|e| e.into_inner()).try_wait().map(|s| s.is_some()),
            |_| child.lock().unwrap_or_else(|e| e.into_inner()).kill(),
            graceful,
            forced,
        )?;
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.retired = true;
        Ok(())
    }

    /// No balloon: the agent shares the host's memory as it is.
    pub fn set_memory_target(&self, _bytes: u64) -> Option<u64> {
        None
    }

    /// Accepts the agent's connections to `port`.
    pub fn listen(&self, port: u32, handler: Handler) -> io::Result<()> {
        if self.closing.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "the VM is retiring"));
        }
        let path = self.dir.join(format!("h{port}.sock"));
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path)?;
        std::thread::Builder::new().name(format!("kiri-agent-{port}")).spawn(move || {
            for s in l.incoming().flatten() {
                handler(VsockStream { fd: OwnedFd::from(s) });
            }
        })?;
        Ok(())
    }

    /// Opens a stream to a port the agent listens on.
    pub fn connect(&self, port: u32) -> io::Result<VsockStream> {
        self.connector().connect(port)
    }

    /// What opens streams to the agent's ports from any thread, while the
    /// `Vm` itself is kept elsewhere.
    pub fn connector(&self) -> Connector {
        Connector { dir: self.dir.clone(), closing: self.closing.clone() }
    }
}

/// Opens streams to the agent's ports (`Vm::connector`).
#[derive(Debug)]
pub struct Connector {
    dir: PathBuf,
    closing: Arc<AtomicBool>,
}

impl Connector {
    /// Opens a stream to a port the agent listens on. Its listeners appear
    /// just after it connects, so a moment of waiting is allowed for.
    pub fn connect(&self, port: u32) -> io::Result<VsockStream> {
        let path = self.dir.join(format!("g{port}.sock"));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if self.closing.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::NotConnected, "the VM is retiring"));
            }
            match UnixStream::connect(&path) {
                Ok(s) => return Ok(VsockStream { fd: OwnedFd::from(s) }),
                Err(e) if Instant::now() < deadline && matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.retired {
            return;
        }
        self.closing.store(true, Ordering::Release);
        self.force_stop();
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = runtime_directory(&std::env::temp_dir());
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn vm(&self, name: &str, child: Child) -> Vm {
            let dir = self.0.join(name);
            std::fs::create_dir(&dir).unwrap();
            let (_, events) = mpsc::channel();
            Vm { dir, child: Arc::new(Mutex::new(child)), events, closing: Arc::new(AtomicBool::new(false)), retired: false }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn retirement_waits_for_a_natural_exit_and_finishes_cleanup() {
        let fixture = Fixture::new();
        let marker = fixture.0.join("flushed");
        let child = Command::new("/bin/sh").args(["-c", "printf flushed > \"$1\"", "sh"]).arg(&marker).spawn().unwrap();
        let mut vm = fixture.vm("natural", child);
        let dir = vm.dir.clone();
        vm.retire(Duration::from_secs(2), Duration::from_secs(1)).unwrap();
        assert!(vm.child.lock().unwrap().try_wait().unwrap().unwrap().success(), "retirement must allow the graceful flush to finish");
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "flushed");
        assert!(!dir.exists());
        vm.retire(Duration::ZERO, Duration::ZERO).unwrap();
    }

    #[test]
    fn retained_old_handles_cannot_reach_or_delete_the_successor() {
        let fixture = Fixture::new();
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let mut old = fixture.vm("generation", child);
        let connector = old.connector();
        let dir = old.dir.clone();
        old.retire(Duration::ZERO, Duration::from_secs(2)).unwrap();
        assert!(old.child.lock().unwrap().try_wait().unwrap().is_some(), "force-stop success must mean the child was reaped");
        assert!(!dir.exists());
        std::fs::create_dir(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("g123.sock")).unwrap();
        assert_eq!(connector.connect(123).unwrap_err().kind(), io::ErrorKind::NotConnected);
        drop(old);
        assert!(dir.exists(), "late destruction of a retired VM must not remove successor resources");
        assert!(dir.join("g123.sock").exists());
        drop(listener);
    }

    #[test]
    fn cleanup_failure_retains_ownership_and_can_be_retried() {
        let fixture = Fixture::new();
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let mut vm = fixture.vm("cleanup", child);
        std::fs::remove_dir(&vm.dir).unwrap();
        std::fs::write(&vm.dir, b"not a directory").unwrap();
        assert!(vm.retire(Duration::ZERO, Duration::from_secs(2)).is_err());
        assert!(!vm.retired, "cleanup failure must not authorize a successor");
        assert!(vm.dir.is_file());
        std::fs::remove_file(&vm.dir).unwrap();
        std::fs::create_dir(&vm.dir).unwrap();
        vm.retire(Duration::ZERO, Duration::from_secs(2)).unwrap();
        assert!(vm.retired);
    }

    #[test]
    fn runtime_directories_belong_to_one_generation() {
        let root = Path::new("/run");
        assert_ne!(runtime_directory(root), runtime_directory(root));
    }
}
