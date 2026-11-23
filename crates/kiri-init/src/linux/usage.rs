//! What the engine's storage takes on disk, for `docker system df`: each
//! unpacked layer, each container's writable layer and each volume, as
//! `du -x` counts it (allocated blocks, hard links once, never across into
//! another filesystem such as a host folder or NFS export mounted on a
//! volume). Walking tens of gigabytes takes a while, so the agent runs this
//! in a child process of its own (`Job`), never on a thread: the agent must
//! stay single-threaded, since it starts containers and execs with clone3.
//! A thread that held the allocator's lock at that moment left the child
//! with the lock taken and nobody to release it: the child hung on its
//! first allocation, before its root was in place, and the start never
//! finished, nor did a stop, exec or rm of that container (with a terminal,
//! nor did the agent's loop, which waits for that root, and so every request
//! after it). The stuck child also kept the agent's listening sockets, so
//! the next engine start could not start an agent.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// A measurement running in a child process. The agent polls `fd` and calls
/// `read` when it is readable; the child is reaped like any other child.
pub struct Job {
    /// Only the tests look: the agent reaps it like any other child.
    #[cfg_attr(not(test), allow(dead_code))]
    pid: libc::pid_t,
    out: OwnedFd,
    buf: Vec<u8>,
}

impl Job {
    /// Forks a child that runs `measure` and writes its lines to a pipe.
    /// The caller must be single-threaded, as with any fork that allocates
    /// in the child (the agent is).
    pub fn start(layers: &Path, containers: &Path, volumes: &Path) -> io::Result<Job> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` has room for the two descriptors pipe2 writes.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe2 succeeded, so both descriptors are fresh and ours.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // SAFETY: the agent is single-threaded, so the child is a consistent
        // copy. It only measures, writes and exits.
        match unsafe { libc::fork() } {
            0 => {
                drop(r);
                // Nothing of the agent's but stdio and the pipe: a child left
                // behind must not hold its lock or its sockets.
                super::sys::close_now(&super::sys::close_ranges(&[w.as_raw_fd()]));
                let mut text = measure(layers, containers, volumes).join("\n");
                text.push('\n');
                let mut left = text.as_bytes();
                while !left.is_empty() {
                    // SAFETY: writing from a buffer we own to a descriptor we own.
                    let n = unsafe { libc::write(w.as_raw_fd(), left.as_ptr().cast(), left.len()) };
                    if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if n <= 0 {
                        break;
                    }
                    left = &left[n as usize..];
                }
                // SAFETY: leaving the child without running the parent's destructors.
                unsafe { libc::_exit(0) }
            }
            pid if pid < 0 => Err(io::Error::last_os_error()),
            pid => {
                drop(w);
                // SAFETY: fcntl on a descriptor we own.
                unsafe { libc::fcntl(r.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
                Ok(Job { pid, out: r, buf: Vec::new() })
            }
        }
    }

    pub fn fd(&self) -> RawFd {
        self.out.as_raw_fd()
    }

    #[cfg(test)]
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Takes what the child has written so far. Once it has closed its end
    /// (done, or gone), the lines it gave; until then None.
    pub fn read(&mut self) -> Option<Vec<String>> {
        let mut chunk = [0u8; 16 << 10];
        loop {
            // SAFETY: reading into a buffer we own from a descriptor we own.
            let n = unsafe { libc::read(self.out.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
            match n {
                0 => break,
                n if n > 0 => self.buf.extend_from_slice(&chunk[..n as usize]),
                _ => match io::Error::last_os_error().kind() {
                    io::ErrorKind::Interrupted => {}
                    io::ErrorKind::WouldBlock => return None,
                    _ => break,
                },
            }
        }
        // Only whole lines: a child that died mid-line gave no number for it.
        let text = String::from_utf8_lossy(&self.buf);
        let whole = text.rfind('\n').map_or("", |end| &text[..end]);
        Some(whole.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect())
    }
}

/// One `KIND NAME BYTES` line per entry (see `kiri_proto::Msg::Usage`).
pub fn measure(layers: &Path, containers: &Path, volumes: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    // Every entry in the layer store, half-done unpacks included, so the
    // total is what the store really takes.
    for (name, path) in entries(layers) {
        lines.push(format!("l {name} {}", du(&path)));
    }
    for (id, path) in entries(containers) {
        let upper = path.join("upper");
        if upper.is_dir() {
            lines.push(format!("c {id} {}", du(&upper)));
        }
    }
    for (name, path) in entries(volumes) {
        lines.push(format!("v {name} {}", du(&path)));
    }
    lines
}

/// The directories in `dir`, by a name that is safe on one line.
fn entries(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| (name, e.path()))
        })
        .collect()
}

/// Bytes allocated under `root`, on `root`'s filesystem only.
pub fn du(root: &Path) -> u64 {
    let Ok(top) = fs::symlink_metadata(root) else { return 0 };
    let dev = top.dev();
    let mut seen = HashSet::new();
    let mut total = top.blocks() * 512;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            let Ok(m) = entry.metadata() else { continue };
            if m.dev() != dev {
                continue;
            }
            if m.nlink() > 1 && !m.is_dir() && !seen.insert(m.ino()) {
                continue;
            }
            total += m.blocks() * 512;
            if m.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_files_once_and_lists_each_kind() {
        let root = std::env::temp_dir().join(format!("kiri-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (layers, containers, volumes) = (root.join("layers"), root.join("containers"), root.join("volumes"));
        fs::create_dir_all(layers.join("ab12")).unwrap();
        fs::create_dir_all(containers.join("c1/upper")).unwrap();
        fs::create_dir_all(volumes.join("data/_data/sub")).unwrap();
        fs::write(layers.join("ab12/f"), vec![1u8; 64 << 10]).unwrap();
        fs::write(volumes.join("data/_data/sub/big"), vec![1u8; 256 << 10]).unwrap();
        // A hard link costs nothing more; a symlink is not followed.
        fs::hard_link(volumes.join("data/_data/sub/big"), volumes.join("data/_data/again")).unwrap();
        std::os::unix::fs::symlink(&layers, volumes.join("data/_data/link")).unwrap();
        // A plain file in the store is not an entry.
        fs::write(layers.join("ab12.done"), b"").unwrap();

        let lines = measure(&layers, &containers, &volumes);
        let size = |kind: &str, name: &str| -> u64 {
            let l = lines.iter().find(|l| l.starts_with(&format!("{kind} {name} "))).unwrap_or_else(|| panic!("no {kind} {name} in {lines:?}"));
            l.rsplit(' ').next().unwrap().parse().unwrap()
        };
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(size("l", "ab12") >= 64 << 10);
        assert!(size("c", "c1") < 64 << 10);
        let v = size("v", "data");
        assert!((256 << 10..512 << 10).contains(&v), "the hard link was counted twice, or the symlink followed: {v}");
        let _ = fs::remove_dir_all(&root);
    }

    /// The measurement runs in a child process, not on a thread of the
    /// agent's: a thread there broke every clone3 that raced it (the child
    /// inherited a held allocator lock and hung). Here the job's pid is
    /// another process, which gives the same lines as `measure`.
    #[test]
    fn a_job_measures_in_a_child_process() {
        let root = std::env::temp_dir().join(format!("kiri-usage-job-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (layers, containers, volumes) = (root.join("layers"), root.join("containers"), root.join("volumes"));
        // More lines than a pipe holds, so the child waits on the reader.
        for i in 0..3000 {
            fs::create_dir_all(layers.join(format!("{i:064x}"))).unwrap();
        }
        fs::create_dir_all(volumes.join("data/_data")).unwrap();
        fs::write(volumes.join("data/_data/f"), vec![1u8; 8 << 10]).unwrap();

        let mut job = Job::start(&layers, &containers, &volumes).unwrap();
        // SAFETY: getpid has no preconditions.
        assert_ne!(job.pid(), unsafe { libc::getpid() }, "measured in the agent's own process");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let lines = loop {
            let mut pfd = libc::pollfd { fd: job.fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            unsafe { libc::poll(&mut pfd, 1, 100) };
            if let Some(lines) = job.read() {
                break lines;
            }
            assert!(std::time::Instant::now() < deadline, "the measurement did not finish");
        };
        let mut status = 0;
        // SAFETY: reaping the child the job forked.
        assert_eq!(unsafe { libc::waitpid(job.pid(), &mut status, 0) }, job.pid());
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "status {status:#x}");
        let mut want = measure(&layers, &containers, &volumes);
        let mut got = lines;
        want.sort();
        got.sort();
        assert_eq!(got.len(), 3001);
        assert_eq!(got, want);
        let _ = fs::remove_dir_all(&root);
    }
}
