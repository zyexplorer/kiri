//! Thin wrappers over the syscalls the guest uses. Everything unsafe lives
//! here or right next to the one call that needs it.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub fn boottime_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn check(ret: libc::c_long, what: &str) -> io::Result<libc::c_long> {
    if ret < 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("{what}: {e}")));
    }
    Ok(ret)
}

pub fn cstr(p: impl AsRef<Path>) -> io::Result<CString> {
    CString::new(p.as_ref().as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

pub fn mount(src: &CStr, target: &CStr, fstype: &CStr, flags: libc::c_ulong, data: Option<&CStr>) -> io::Result<()> {
    // SAFETY: every pointer is a valid NUL-terminated string or null.
    let ret = unsafe {
        libc::mount(src.as_ptr(), target.as_ptr(), fstype.as_ptr(), flags, data.map_or(std::ptr::null(), |d| d.as_ptr().cast()))
    };
    check(ret as _, &format!("mount {}", target.to_string_lossy())).map(drop)
}

/// Mounts with no source or type, for changing propagation (`MS_PRIVATE` …).
pub fn remount(target: &CStr, flags: libc::c_ulong) -> io::Result<()> {
    // SAFETY: `target` is a valid NUL-terminated string; the rest may be null.
    let ret = unsafe { libc::mount(std::ptr::null(), target.as_ptr(), std::ptr::null(), flags, std::ptr::null()) };
    check(ret as _, &format!("remount {}", target.to_string_lossy())).map(drop)
}

pub fn umount_lazy(target: &Path) -> io::Result<()> {
    let t = cstr(target)?;
    // SAFETY: `t` is a valid NUL-terminated string.
    check(unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) } as _, "umount").map(drop)
}

pub fn mkdir(p: &CStr, mode: libc::mode_t) {
    // SAFETY: `p` is a valid NUL-terminated string. EEXIST is expected and fine.
    unsafe { libc::mkdir(p.as_ptr(), mode) };
}

pub fn mknod(p: &CStr, mode: libc::mode_t, major: u32, minor: u32) -> io::Result<()> {
    // SAFETY: `p` is a valid NUL-terminated string.
    let ret = unsafe { libc::mknod(p.as_ptr(), mode, libc::makedev(major, minor)) };
    check(ret as _, &format!("mknod {}", p.to_string_lossy())).map(drop)
}

pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } as _, "pipe2")?;
    // SAFETY: pipe2 succeeded, so both descriptors are fresh and ours.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

pub fn write_file(p: &str, contents: &str) -> io::Result<()> {
    std::fs::write(p, contents).map_err(|e| io::Error::new(e.kind(), format!("write {p}: {e}")))
}

/// Brings the loopback interface up in the current network namespace.
pub fn loopback_up() -> io::Result<()> {
    // SAFETY: plain socket creation; the fd is owned immediately.
    let fd = check(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) } as _, "socket")?;
    // SAFETY: `fd` is a fresh, valid descriptor that nothing else owns.
    let sock = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    // SAFETY: ifreq is plain data; all-zero is a valid value.
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(b"lo\0") {
        *dst = *src as libc::c_char;
    }
    req.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    use std::os::fd::AsRawFd;
    // SAFETY: `req` is a valid ifreq for SIOCSIFFLAGS.
    check(unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS as libc::Ioctl, &req) } as _, "lo up").map(drop)
}

pub fn sethostname(name: &[u8]) -> io::Result<()> {
    // SAFETY: `name` is valid for `name.len()` bytes.
    check(unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) } as _, "sethostname").map(drop)
}

pub fn setdomainname(name: &[u8]) -> io::Result<()> {
    // SAFETY: `name` is valid for `name.len()` bytes.
    check(unsafe { libc::setdomainname(name.as_ptr().cast(), name.len()) } as _, "setdomainname").map(drop)
}

/// Arguments for clone3(2). libc has no definition for it on musl yet.
#[repr(C)]
#[derive(Default)]
pub struct CloneArgs {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

pub const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// Like fork(), but straight into new namespaces and a cgroup. Returns 0 in
/// the child and the child's pid in the parent.
///
/// # Safety
/// The caller must be single-threaded, as with fork(): the child gets a copy
/// of this thread only, so no other thread may hold a lock it needs.
pub unsafe fn clone3(args: &CloneArgs) -> io::Result<libc::pid_t> {
    // SAFETY: `args` is a valid clone_args of the size passed; the caller
    // upholds the single-thread requirement.
    let ret = unsafe { libc::syscall(libc::SYS_clone3, args as *const CloneArgs, std::mem::size_of::<CloneArgs>()) };
    check(ret, "clone3").map(|p| p as libc::pid_t)
}

/// The engine's own message as one line of the container's log: a value
/// over several lines (a key a `user:` named) never goes on, unhidden, over
/// the lines after it.
/// Each line break is one space; every other character stays as it is, so
/// what Kiri hides in Logs finds the value as it is.
pub fn one_line(s: &str) -> String {
    s.replace("\r\n", " ").replace(['\n', '\r'], " ")
}

/// Closes every descriptor from `first` up. For the child between clone and
/// exec, which holds a copy of all of the agent's descriptors: close-on-exec
/// closes them only at execve, and until then one is still a way out of the
/// container (a working directory of `/proc/self/fd/N`).
pub fn close_from(first: RawFd) {
    // SAFETY: closing descriptors in this process; nothing is used after.
    unsafe {
        if libc::syscall(libc::SYS_close_range, first as libc::c_uint, libc::c_uint::MAX, 0) == 0 {
            return;
        }
        // close_range is Linux 5.9+; older kernels close one at a time.
        let max = match libc::sysconf(libc::_SC_OPEN_MAX) {
            n if n > 0 => n.min(1 << 20) as RawFd,
            _ => 1 << 16,
        };
        for fd in first..max {
            libc::close(fd);
        }
    }
}

/// Whether the current directory is inside this process's root. The kernel
/// gives a directory outside it as `(unreachable)/…`, which is how a
/// working directory reached through a leaked descriptor shows.
pub fn cwd_inside_root() -> bool {
    let mut buf = [0u8; 4096];
    // SAFETY: getcwd writes at most `buf.len()` bytes into `buf`.
    let n = unsafe { libc::syscall(libc::SYS_getcwd, buf.as_mut_ptr(), buf.len()) };
    n > 0 && buf[0] == b'/'
}

/// Writes `msg` straight to `fd` and exits without unwinding or running any
/// destructors. Only for the child between clone and exec.
pub fn child_fail(fd: RawFd, what: &str, err: io::Error) -> ! {
    let msg = format!("kiri: cannot start container: {}\n", one_line(&format!("{what}: {err}")));
    // SAFETY: writing a valid buffer to a descriptor we hold, then exiting.
    unsafe {
        libc::write(fd, msg.as_ptr().cast(), msg.len());
        libc::_exit(127)
    }
}

/// Reports an execve that failed and exits as Docker ends that container:
/// 126 when the command may not be run, 127 when it is missing, 255 when
/// runc's exec itself fails (a script without a shebang, a missing
/// interpreter). Only for the child between clone and exec. `init`: the
/// program runs under `--init`, which Docker leaves to tini.
pub fn exec_fail(fd: RawFd, exe: &CStr, err: io::Error, init: bool) -> ! {
    let exe = exe.to_string_lossy();
    let path = Path::new(&*exe);
    let found = Found { exists: path.exists(), dir: path.is_dir() };
    let (code, why) = exec_failure(&exe, &err, found, init);
    // runc reports what it refuses before the start as the daemon's error,
    // and a failed exec after it on the container's stderr, as is.
    let msg = if code == 255 { format!("{why}\n") } else { format!("kiri: cannot start container: {}\n", one_line(&why)) };
    // SAFETY: writing a valid buffer to a descriptor we hold, then exiting.
    unsafe {
        libc::write(fd, msg.as_ptr().cast(), msg.len());
        libc::_exit(code)
    }
}

/// What is at the path execve refused.
#[derive(Clone, Copy, Default)]
pub struct Found {
    pub exists: bool,
    pub dir: bool,
}

/// The exit code and message for a command execve would not run, worded
/// as runc words them. Refused before the start: `exec: "/etc/passwd":
/// permission denied` ends 126 (`exec: "/etc": is a directory: permission
/// denied` for a directory, which Linux refuses with EACCES), a missing
/// one 127 (`stat /x: no such file or directory`). Failing in runc's exec
/// after the start, 255: `exec /x: exec format error` for a file that is
/// no program (a script without a shebang), `exec /x: no such file or
/// directory` when the file is there but its interpreter isn't.
/// Under `--init` (tini), a missing interpreter is 127 and the rest 126.
pub fn exec_failure(exe: &str, err: &io::Error, found: Found, init: bool) -> (i32, String) {
    match err.raw_os_error() {
        Some(libc::EACCES | libc::EISDIR) if found.dir => (126, format!("exec: \"{exe}\": is a directory: permission denied")),
        Some(libc::EACCES) => (126, format!("exec: \"{exe}\": permission denied")),
        Some(libc::ENOEXEC) if init => (126, format!("exec: \"{exe}\": exec format error")),
        Some(libc::ENOEXEC) => (255, format!("exec {exe}: exec format error")),
        Some(libc::ENOENT) if found.exists && init => (127, format!("exec {exe}: no such file or directory")),
        Some(libc::ENOENT) if found.exists => (255, format!("exec {exe}: no such file or directory")),
        Some(libc::ENOENT) => (127, format!("exec: \"{exe}\": stat {exe}: no such file or directory")),
        Some(libc::ENOTDIR) => (127, format!("exec: \"{exe}\": stat {exe}: not a directory")),
        _ => (127, format!("exec: \"{exe}\": {err}")),
    }
}

pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` has room for the two descriptors socketpair writes.
    check(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } as _, "socketpair")?;
    // SAFETY: socketpair succeeded, so both descriptors are fresh and ours.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Sends one descriptor over a Unix socket (SCM_RIGHTS).
pub fn send_fd(sock: RawFd, fd: RawFd) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: msghdr is plain data; all-zero is valid before filling it in.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg` points at a control buffer big enough for one fd.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        check(libc::sendmsg(sock, &msg, 0) as _, "sendmsg").map(drop)
    }
}

/// What came over a socket: a descriptor, or the bytes of an error message.
pub enum Received {
    Fd(OwnedFd),
    Message(String),
}

/// Receives one descriptor, or whatever text the peer sent instead.
pub fn recv_fd(sock: RawFd) -> io::Result<Received> {
    let mut buf = [0u8; 512];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: msghdr is plain data; all-zero is valid before filling it in.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg` describes buffers we own; MSG_CMSG_CLOEXEC marks the fd.
    let n = check(unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) } as _, "recvmsg")?;
    // SAFETY: walking the control messages recvmsg filled in.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if !cmsg.is_null() && (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>());
            return Ok(Received::Fd(OwnedFd::from_raw_fd(fd)));
        }
    }
    Ok(Received::Message(String::from_utf8_lossy(&buf[..n as usize]).trim().to_owned()))
}

pub fn setns(fd: RawFd, nstype: libc::c_int) -> io::Result<()> {
    // SAFETY: plain syscall on a namespace descriptor we hold.
    check(unsafe { libc::setns(fd, nstype) } as _, "setns").map(drop)
}

pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl calls on a descriptor we own.
    unsafe {
        let flags = check(libc::fcntl(fd, libc::F_GETFL) as _, "fcntl")?;
        check(libc::fcntl(fd, libc::F_SETFL, flags as libc::c_int | libc::O_NONBLOCK) as _, "fcntl").map(drop)
    }
}

/// Opens a new pseudo-terminal pair from the current mount namespace's
/// /dev/ptmx. Returns (master, slave).
pub fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: plain open/ioctl calls; descriptors are owned right away.
    unsafe {
        let master = check(libc::open(c"/dev/ptmx".as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) as _, "open /dev/ptmx")?;
        let master = OwnedFd::from_raw_fd(master as RawFd);
        let unlock: libc::c_int = 0;
        check(libc::ioctl(master.as_raw_fd(), libc::TIOCSPTLCK as libc::Ioctl, &unlock) as _, "unlock pty")?;
        let mut n: libc::c_uint = 0;
        check(libc::ioctl(master.as_raw_fd(), libc::TIOCGPTN as libc::Ioctl, &mut n) as _, "pty number")?;
        let path = CString::new(format!("/dev/pts/{n}")).expect("no NUL");
        let slave = check(libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) as _, "open pty slave")?;
        Ok((master, OwnedFd::from_raw_fd(slave as RawFd)))
    }
}

/// The descriptor ranges to close so that only 0, 1, 2 and `keep` stay
/// open: (first, last), inclusive, in order.
pub fn close_ranges(keep: &[RawFd]) -> Vec<(libc::c_uint, libc::c_uint)> {
    let mut keep: Vec<libc::c_uint> = keep.iter().filter(|&&fd| fd > 2).map(|&fd| fd as libc::c_uint).collect();
    keep.sort_unstable();
    keep.dedup();
    let mut out = Vec::with_capacity(keep.len() + 1);
    let mut next = 3;
    for fd in keep {
        if fd > next {
            out.push((next, fd - 1));
        }
        next = fd + 1;
    }
    out.push((next, libc::c_uint::MAX));
    out
}

/// Closes the ranges `close_ranges` gave, without allocating. Before Linux
/// 5.9 there is no close_range, and the descriptors stay open, as they
/// always did before.
pub fn close_now(ranges: &[(libc::c_uint, libc::c_uint)]) {
    for &(first, last) in ranges {
        // SAFETY: closing descriptors of this process, which the caller
        // no longer uses.
        unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) };
    }
}

pub fn resize_pty(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCSWINSZ with a valid winsize on a descriptor we own.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as libc::Ioctl, &ws) };
}

#[cfg(test)]
mod tests {
    use std::io;

    /// A value over several lines never starts a line of its own in the log.
    #[test]
    fn the_engines_message_is_one_line() {
        assert_eq!(super::one_line("user: unknown user -----BEGIN KEY-----\nAAA\r\n-----END KEY-----"), "user: unknown user -----BEGIN KEY----- AAA -----END KEY-----");
        // A backslash or a quote stays as the value has it.
        assert_eq!(super::one_line("user: unknown user Bs-x\\yy\"4dd4"), "user: unknown user Bs-x\\yy\"4dd4");
        assert_eq!(super::one_line("DOM\\bob\\n"), "DOM\\bob\\n");
        assert_eq!(super::one_line("x\ny\rz"), "x y z");
    }

    #[test]
    fn only_the_kept_descriptors_stay_open() {
        let all = libc::c_uint::MAX;
        assert_eq!(super::close_ranges(&[]), vec![(3, all)]);
        // Standard streams are never closed; order and repeats don't matter.
        assert_eq!(super::close_ranges(&[9, 2, 4, 9, 5, 0]), vec![(3, 3), (6, 8), (10, all)]);
        assert_eq!(super::close_ranges(&[3]), vec![(4, all)]);
        // The real thing, in a child of its own: the kept ones stay open,
        // the rest are closed. Forked from a test with threads, so the
        // child only makes system calls (the ranges are worked out first).
        let (unrelated, unrelated_w) = super::pipe().unwrap();
        let (kept, kept_w) = super::pipe().unwrap();
        let (report, report_w) = super::pipe().unwrap();
        use std::os::fd::AsRawFd;
        let keep = [kept.as_raw_fd(), report_w.as_raw_fd()];
        let ranges = super::close_ranges(&keep);
        let check = [unrelated.as_raw_fd(), unrelated_w.as_raw_fd(), kept_w.as_raw_fd(), report.as_raw_fd()];
        // SAFETY: the child makes only async-signal-safe system calls and
        // leaves with _exit.
        unsafe {
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                super::close_now(&ranges);
                let open = |fd| libc::fcntl(fd, libc::F_GETFD) >= 0;
                let mut ok = keep.iter().all(|&fd| open(fd)) && (0..3).all(open);
                ok &= check.iter().all(|&fd| !open(fd));
                libc::write(report_w.as_raw_fd(), if ok { b"y".as_ptr() } else { b"n".as_ptr() }.cast(), 1);
                libc::_exit(0);
            }
            drop(report_w);
            let mut said = [0u8; 1];
            assert_eq!(libc::read(report.as_raw_fd(), said.as_mut_ptr().cast(), 1), 1, "the child said nothing");
            libc::waitpid(pid, std::ptr::null_mut(), 0);
            assert_eq!(&said, b"y", "a kept descriptor was closed, or another left open");
        }
    }

    /// A command that may not be run ends 126 and one that is missing 127,
    /// as with Docker (`docker run alpine /etc/passwd` is 126).
    #[test]
    fn exec_failures_end_as_docker_ends_them() {
        use super::Found;
        let os = io::Error::from_raw_os_error;
        let f = super::exec_failure;
        let file = Found { exists: true, dir: false };
        let dir = Found { exists: true, dir: true };
        let none = Found::default();
        assert_eq!(f("/etc/passwd", &os(libc::EACCES), file, false), (126, "exec: \"/etc/passwd\": permission denied".into()));
        // Linux refuses a directory with EACCES; runc names it.
        assert_eq!(f("/etc", &os(libc::EACCES), dir, false), (126, "exec: \"/etc\": is a directory: permission denied".into()));
        assert_eq!(f("/etc", &os(libc::EISDIR), dir, true).0, 126);
        assert_eq!(f("/nope", &os(libc::ENOENT), none, false), (127, "exec: \"/nope\": stat /nope: no such file or directory".into()));
        // runc's exec fails after the start: 255, its own words.
        assert_eq!(f("/x", &os(libc::ENOEXEC), file, false), (255, "exec /x: exec format error".into()));
        assert_eq!(f("/run.sh", &os(libc::ENOENT), file, false), (255, "exec /run.sh: no such file or directory".into()));
        // tini (--init): a missing interpreter is 127.
        assert_eq!(f("/run.sh", &os(libc::ENOENT), file, true).0, 127);
    }

    /// The real thing: execve of a directory fails with EACCES, which runc
    /// words as a directory.
    #[test]
    fn exec_of_a_directory_says_so() {
        let c = std::ffi::CString::new("/").unwrap();
        let argv = [c.as_ptr(), std::ptr::null()];
        let envp = [std::ptr::null()];
        // SAFETY: valid NULL-terminated arrays; execve fails and returns.
        unsafe { libc::execve(c.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        let err = io::Error::last_os_error();
        assert_eq!(super::exec_failure("/", &err, super::Found { exists: true, dir: true }, false), (126, "exec: \"/\": is a directory: permission denied".into()), "{err}");
    }

    /// The real thing: execve of a file that is not executable fails with
    /// EACCES, which is 126.
    #[test]
    fn exec_of_a_plain_file_is_126() {
        let dir = std::env::temp_dir().join(format!("kiri-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("plain");
        std::fs::write(&file, b"not a program").unwrap();
        let c = std::ffi::CString::new(file.to_str().unwrap()).unwrap();
        let argv = [c.as_ptr(), std::ptr::null()];
        let envp = [std::ptr::null()];
        // SAFETY: valid NULL-terminated arrays; execve fails and returns.
        unsafe { libc::execve(c.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        let err = io::Error::last_os_error();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(super::exec_failure(file.to_str().unwrap(), &err, super::Found { exists: true, dir: false }, false).0, 126, "{err}");
    }
}
