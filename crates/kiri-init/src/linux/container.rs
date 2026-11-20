//! Starts containers and execs.
//!
//! A container is one clone3 straight into fresh namespaces and its cgroup;
//! the child sets up its root and execs. An exec joins a running container's
//! namespaces and cgroup the same way. No runc, no shim, no extra fork:
//! every process is a direct child of the agent, so waiting just works.

use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use kiri_proto::{ExecSpec, RunSpec, StrList, net_opts, run_flags};

use super::inroot::{self, Last};
use super::layers;
use super::net::{self, Bridges};
use super::sys::{self, CLONE_INTO_CGROUP, CloneArgs, Received, child_fail};

/// Capabilities a container keeps; Docker's default set. All others are
/// dropped from the bounding set before exec.
const KEEP_CAPS: [u32; 14] = [
    0,  // CHOWN
    1,  // DAC_OVERRIDE
    3,  // FOWNER
    4,  // FSETID
    5,  // KILL
    6,  // SETGID
    7,  // SETUID
    8,  // SETPCAP
    10, // NET_BIND_SERVICE
    13, // NET_RAW
    18, // SYS_CHROOT
    27, // MKNOD
    29, // AUDIT_WRITE
    31, // SETFCAP
];
const LAST_CAP: u32 = 40;

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// What networking the agent can offer a new container right now.
pub struct NetCtx<'a> {
    pub bridges: &'a mut Bridges,
    pub dns: &'a mut super::dns::Dns,
    /// The VM's upstream resolvers, once DHCP has delivered them.
    pub upstream_dns: &'a [[u8; 4]],
    /// For `--network container:<id>`: that container's PID 1, if running.
    pub join_pid: Option<libc::pid_t>,
    /// For `--pid container:<id>`: that container's PID 1, if running.
    pub pid_join_pid: Option<libc::pid_t>,
}

/// Where containers live under the data directory.
#[derive(Debug)]
pub struct Paths {
    pub layers: PathBuf,
    pub containers: PathBuf,
    pub cgroup: PathBuf,
}

/// A process the agent started: a container's PID 1, or an exec.
#[derive(Debug)]
pub struct Process {
    /// Container id, or exec id for an exec.
    pub id: String,
    /// For an exec, the container it runs in.
    pub container: Option<String>,
    pub pid: libc::pid_t,
    /// Pipe read end, or the pty master for a TTY.
    pub stdout: Option<OwnedFd>,
    /// None for a TTY: the terminal merges both streams.
    pub stderr: Option<OwnedFd>,
    /// Write end for `Input`: a pipe, or a duplicate of the pty master.
    pub stdin: Option<OwnedFd>,
    pub tty: bool,
    /// (bridge, ip) registered with the DNS server, to drop on exit.
    pub dns_entries: Vec<(String, [u8; 4])>,
    /// Host sides of its veths. Deleted on exit: a container sharing this
    /// network namespace would otherwise keep them, and the address that
    /// goes back to the pool, alive.
    pub veths: Vec<String>,
    /// Inode of its own network namespace, or of the one it joined; 0 for
    /// the host's and for execs. Says whether a namespace outlives this
    /// container (see `teardown`).
    pub netns: u64,
    /// It joined another container's network namespace (`--network
    /// container:`): its child does that after the clone, so until its root
    /// is in place its own `/proc/<pid>/ns/net` may still be the host's.
    pub joins_netns: bool,
    /// A container's own compiled seccomp program, empty when it runs under
    /// the default. Its execs run under the same one.
    pub seccomp: Vec<u8>,
    /// What to undo when a container exits; None for execs.
    teardown: Option<Teardown>,
    /// A container whose root was not in place when `spawn` returned: the
    /// read end of its child's "root ready" pipe, until `root` sees the
    /// byte (or the end). None for every other process.
    starting: Option<OwnedFd>,
    /// Its child ended before its root was in place: the reason it gave
    /// (see `child_report`). The start failed; the agent answers the
    /// daemon's `AwaitStart` with it and tears the container down, with no
    /// `Exited`, as a start that fails within `ROOT_READY_WAIT` is.
    ended_before_ready: Option<String>,
}

/// Where a container's root stands (see [`Process::root`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    /// Mounts made and `pivot_root` done: an exec or copy may enter it.
    Ready,
    /// Still being made (a new volume is being filled from the image).
    Starting,
    /// The child ended before its root was in place.
    Ended,
}

impl Process {
    /// Settles where its root stands, without waiting: the child's byte
    /// means ready, the pipe's end means it ended first, nothing yet means
    /// still starting.
    pub fn root(&mut self) -> Root {
        self.settle(Some(std::time::Duration::ZERO))
    }

    /// Waits, as long as it takes, for the child to say its root is in
    /// place or to end. Called before the agent kills a container: a child
    /// still filling a new volume must finish first, or the volume is left
    /// half full and, no longer empty, is never filled again.
    pub fn wait_root(&mut self) -> Root {
        self.settle(None)
    }

    fn settle(&mut self, wait: Option<std::time::Duration>) -> Root {
        if self.ended_before_ready.is_some() {
            return Root::Ended;
        }
        let Some(fd) = &self.starting else { return Root::Ready };
        match root_word(fd.as_raw_fd(), wait) {
            Ok(RootWord::NotYet) => Root::Starting,
            Ok(RootWord::Ready) => {
                self.starting = None;
                Root::Ready
            }
            Ok(RootWord::Ended) | Err(_) => {
                self.starting = None;
                // Written before it exited, and not forwarded while it was
                // starting: still in the pipe.
                let why = self.stderr.as_ref().and_then(|f| child_report(f.as_raw_fd()));
                self.ended_before_ready = Some(why.unwrap_or_else(|| NOT_READY.to_owned()));
                Root::Ended
            }
        }
    }

    /// Whether its root was in place when last settled (see `root`).
    /// Nothing may enter a container before its root is in place, and its
    /// output is not read until then (only a failure message can be there).
    pub fn entered(&self) -> bool {
        self.starting.is_none() && self.ended_before_ready.is_none()
    }

    /// The pipe the child says "root ready" on, while it is still starting:
    /// the agent polls it, so it settles as soon as the child speaks.
    pub fn starting_fd(&self) -> Option<RawFd> {
        self.starting.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Why its start failed, once settled as `Root::Ended`.
    pub fn start_failure(&self) -> Option<&str> {
        self.ended_before_ready.as_deref()
    }
}

/// A child that ended before its root was in place and said nothing.
const NOT_READY: &str = "the container exited before its root was ready";

/// What a started container gives the DNS server (see `spawn`).
struct Names {
    /// Its A records: not on an IPv6-only network.
    v4: bool,
    v6: Option<[u8; 16]>,
    /// `--dns` servers.
    servers: Vec<[u8; 4]>,
}

#[cfg(test)]
impl Process {
    /// A container's process with nothing to tear down, for the agent's tests.
    pub fn for_test(id: &str, pid: libc::pid_t) -> Self {
        Process {
            id: id.to_owned(),
            container: None,
            pid,
            stdout: None,
            stderr: None,
            stdin: None,
            tty: false,
            dns_entries: Vec::new(),
            veths: Vec::new(),
            netns: 0,
            joins_netns: false,
            seccomp: Vec::new(),
            teardown: None,
            starting: None,
            ended_before_ready: None,
        }
    }

    /// The same, still waiting for its root on `ready` (see `root`), with
    /// the read end of its stderr if given.
    pub fn starting_for_test(id: &str, pid: libc::pid_t, ready: OwnedFd, stderr: Option<OwnedFd>) -> Self {
        Process { starting: Some(ready), stderr, ..Process::for_test(id, pid) }
    }
}

#[derive(Debug)]
struct Teardown {
    base: PathBuf,
    cgroup: PathBuf,
    remove: bool,
}

/// Everything the child needs, prepared before clone so the child does as
/// little as possible between clone and exec.
struct Prepared {
    argv: Vec<CString>,
    envp: Vec<CString>,
    path_dirs: Vec<Vec<u8>>,
    hostname: Option<Vec<u8>>,
    /// `--domainname`, set in the container's UTS namespace.
    domainname: Option<Vec<u8>>,
    /// `--network container:<id>`: that container's network namespace, joined
    /// instead of a new one.
    net_join: Option<fs::File>,
    /// Generated /etc files to bind over the image's: (source, target relative to the root).
    etc_files: Vec<(CString, CString)>,
    mounts: Vec<Mount>,
    /// For a bridged container: the child waits here until the agent has
    /// set up its network, so the program never starts without it.
    net_ready: Option<OwnedFd>,
    cwd: CString,
    user: String,
    net_none: bool,
    tty: bool,
    stdin: bool,
    privileged: bool,
    /// Capabilities kept, one bit each.
    caps: u64,
    seccomp: bool,
    /// The program the daemon compiled from the container's own profile;
    /// empty for the built-in default.
    profile: Vec<u8>,
    no_new_privs: bool,
    readonly_rootfs: bool,
    shm_size: u64,
    /// `--init`: PID 1 supervises the program instead of being it.
    init: bool,
    /// Ulimits, groups and device nodes from the `opts` lines.
    host: HostApplied,
    /// The child's end of the pipe that says its root is in place (mounts
    /// made, `pivot_root` done), so a start is not reported before an exec
    /// can join its mount namespace (#311).
    root_ready: Option<OwnedFd>,
}

/// The parts of [`super::hostopts::HostOpts`] the child applies itself.
#[derive(Default)]
struct HostApplied {
    rlimits: Vec<(u32, u64, u64)>,
    groups: Vec<String>,
    nodes: Vec<super::hostopts::Node>,
}

/// Docker's default capability set as a mask.
fn default_caps() -> u64 {
    KEEP_CAPS.iter().fold(0, |m, c| m | 1 << c)
}

/// The child's ends of its standard streams.
enum ChildStdio {
    /// The child makes a pty inside its own /dev and sends the master back.
    Tty { sock: OwnedFd },
    Pipes { stdin: Option<OwnedFd>, stdout: OwnedFd, stderr: OwnedFd },
}

/// The agent's ends.
enum ParentStdio {
    Tty { sock: OwnedFd },
    Pipes { stdin: Option<OwnedFd>, stdout: OwnedFd, stderr: OwnedFd },
}

/// How long a start waits for the child to say its root is in place before
/// it goes on without it. The usual child is ready within a few
/// milliseconds, so a normal start waits for it exactly as before. One that
/// first fills a new volume from a big image folder can take seconds, and
/// the agent serves every other container's exec, output, DNS and ports
/// from this same thread: waiting longer would hold all of them up. Past
/// this, the start is answered `starting` and the daemon asks again.
const ROOT_READY_WAIT: std::time::Duration = std::time::Duration::from_millis(20);

/// What the child's "root ready" pipe says.
#[derive(Debug, PartialEq, Eq)]
enum RootWord {
    Ready,
    /// Closed without the byte: the child ended first.
    Ended,
    NotYet,
}

/// Reads the child's word on its root, waiting at most `wait` (None: as long
/// as it takes).
fn root_word(fd: RawFd, wait: Option<std::time::Duration>) -> io::Result<RootWord> {
    let deadline = wait.map(|w| std::time::Instant::now() + w);
    loop {
        let timeout = deadline.map_or(-1, |d| {
            // Rounded up: poll counts whole milliseconds.
            d.saturating_duration_since(std::time::Instant::now()).as_micros().div_ceil(1000).min(i32::MAX as u128) as i32
        });
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if n == 0 {
            return Ok(RootWord::NotYet);
        }
        if n > 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    let mut ready = [0u8; 1];
    loop {
        // SAFETY: reading one byte into a local buffer from a pipe that has
        // data or is closed, so this does not wait.
        let n = unsafe { libc::read(fd, ready.as_mut_ptr().cast(), 1) };
        match n {
            1 => return Ok(RootWord::Ready),
            0 => return Ok(RootWord::Ended),
            _ => {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => return Ok(RootWord::NotYet),
                    _ => return Err(e),
                }
            }
        }
    }
}

/// What the child said before it ended, if it could not start (see
/// `sys::child_fail`): read from where it reports, without waiting.
fn child_report(fd: RawFd) -> Option<String> {
    let mut ready = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd, no wait.
    if unsafe { libc::poll(&mut ready, 1, 0) } != 1 || ready.revents & libc::POLLIN == 0 {
        return None;
    }
    let mut buf = [0u8; 2048];
    // SAFETY: reading into a local buffer from a descriptor that has data.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return None;
    }
    start_failure(&String::from_utf8_lossy(&buf[..n as usize])).map(str::to_owned)
}

/// The reason in a child's "kiri: cannot start container: <reason>" line.
fn start_failure(said: &str) -> Option<&str> {
    said.lines().find_map(|l| l.strip_prefix("kiri: cannot start container: ")).map(str::trim).filter(|why| !why.is_empty())
}

fn stdio(p: &Prepared) -> io::Result<(ParentStdio, ChildStdio)> {
    if p.tty {
        let (parent, child) = sys::socketpair()?;
        return Ok((ParentStdio::Tty { sock: parent }, ChildStdio::Tty { sock: child }));
    }
    let (out_r, out_w) = sys::pipe()?;
    let (err_r, err_w) = sys::pipe()?;
    let (in_w, in_r) = if p.stdin {
        let (r, w) = sys::pipe()?;
        sys::set_nonblocking(w.as_raw_fd())?;
        (Some(w), Some(r))
    } else {
        (None, None)
    };
    Ok((
        ParentStdio::Pipes { stdin: in_w, stdout: out_r, stderr: err_r },
        ChildStdio::Pipes { stdin: in_r, stdout: out_w, stderr: err_w },
    ))
}

/// After clone: collect the agent's ends. For a TTY this waits for the child
/// to send the pty master, or the reason it could not.
fn finish_stdio(parent: ParentStdio, child: ChildStdio) -> io::Result<(Option<OwnedFd>, Option<OwnedFd>, Option<OwnedFd>)> {
    drop(child);
    match parent {
        ParentStdio::Pipes { stdin, stdout, stderr } => Ok((stdin, Some(stdout), Some(stderr))),
        ParentStdio::Tty { sock } => match sys::recv_fd(sock.as_raw_fd())? {
            Received::Fd(master) => {
                sys::set_nonblocking(master.as_raw_fd())?;
                let input = master.try_clone()?;
                Ok((Some(input), Some(master), None))
            }
            Received::Message(m) if m.is_empty() => Err(io::Error::other("container exited before its terminal was ready")),
            Received::Message(m) => Err(io::Error::other(m)),
        },
    }
}

pub fn spawn(spec: &RunSpec<'_>, paths: &Paths, net: NetCtx<'_>) -> io::Result<Process> {
    if spec.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no command to run"));
    }
    let layers: Vec<&str> = spec.layers.iter().collect();
    let rootfs = super::roots::acquire(paths, spec.id, &layers)?;
    let base = paths.containers.join(spec.id);

    let cgroup = paths.cgroup.join(spec.id);
    let remove = spec.flags & run_flags::REMOVE != 0;
    let undo = Teardown { base: base.clone(), cgroup: cgroup.clone(), remove };
    // A restart that fails must not take the stopped container's files with it.
    let cleanup = |e: io::Error| {
        undo_now(&undo, remove);
        e
    };
    // A previous run may have left sub-cgroups (Kubernetes, Docker in
    // Docker make them); its cgroup would then refuse processes.
    remove_cgroup(&cgroup);
    if !paths.cgroup.is_dir() {
        super::containers_cgroup(&paths.cgroup);
    }
    let _ = fs::create_dir(&cgroup);
    if spec.limits != kiri_proto::Limits::default() {
        super::cgroup::apply_limits(&cgroup, &spec.limits).map_err(cleanup)?;
    }
    let cgroup_fd = fs::File::open(&cgroup).map_err(cleanup)?;
    let mut host = super::hostopts::parse(spec.opts).map_err(cleanup)?;
    if spec.flags & run_flags::NET_HOST != 0 && host.sysctls.iter().any(|s| s.ns == super::hostopts::Ns::Net) {
        return Err(cleanup(io::Error::new(io::ErrorKind::InvalidInput, "net.* sysctls are not allowed in the host network namespace")));
    }
    // `--device` nodes are looked up now, so a missing one fails the start.
    let (nodes, device_rules) = super::hostopts::resolve_devices(&host).map_err(cleanup)?;
    // Deny block-device read/write (Docker's default device cgroup). If the
    // kernel cannot attach the filter, CAP_MKNOD is dropped below instead.
    let device_filtered = spec.flags & run_flags::PRIVILEGED != 0
        || match super::devices::restrict(cgroup_fd.as_raw_fd(), &device_rules) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("kiri: device filter unavailable, dropping CAP_MKNOD: {e}");
                false
            }
        };

    let mut prepared = prepare(spec.argv, spec.env, spec.cwd, spec.user, Some(spec.hostname), spec.flags, spec.caps).map_err(cleanup)?;
    if !device_filtered {
        prepared.caps &= !(1 << super::devices::CAP_MKNOD);
    }
    // Looked at now, so a program the kernel would refuse fails the start
    // with a message rather than the child.
    if !spec.seccomp.is_empty() {
        super::seccomp::check(spec.seccomp).map_err(cleanup)?;
        prepared.profile = spec.seccomp.to_vec();
    }
    if spec.limits.shm_size > 0 {
        prepared.shm_size = spec.limits.shm_size;
    }
    prepared.domainname = net_opts::values(spec.net.opts, net_opts::DOMAINNAME).next().filter(|d| !d.is_empty()).map(|d| d.as_bytes().to_vec());
    let mut netns = 0;
    if let Some(target) = net_opts::values(spec.net.opts, net_opts::NETNS).next() {
        let pid = net.join_pid.ok_or_else(|| cleanup(io::Error::other(format!("cannot join network of a non running container: {target}"))))?;
        let joined = fs::File::open(format!("/proc/{pid}/ns/net")).map_err(cleanup)?;
        netns = netns_inode(&joined);
        prepared.net_join = Some(joined);
    }
    // `--pid container:`: entered by the agent just around the clone (see
    // below); the fd is the agent's alone, the child closes its copy.
    let mut pid_join = None;
    if let Some(target) = net_opts::values(spec.net.opts, net_opts::PIDNS).next() {
        let pid = net.pid_join_pid.ok_or_else(|| cleanup(io::Error::other(format!("cannot join PID of a non running container: {target}"))))?;
        pid_join = Some(fs::File::open(format!("/proc/{pid}/ns/pid")).map_err(cleanup)?);
    }
    prepared.host = HostApplied { rlimits: host.rlimits.clone(), groups: host.groups.clone(), nodes };
    // Small, short-lived files: kept in memory (/run is tmpfs), not on the data disk.
    let etc_dir = PathBuf::from("/run/kiri").join(spec.id);
    let embedded_dns = embedded_dns(spec, net.upstream_dns, net.dns.relay);
    let nameserver = embedded_dns.then(|| embedded_address(spec, net.dns.relay));
    prepared.etc_files = write_etc_files(spec, &etc_dir, net.upstream_dns, nameserver).map_err(cleanup)?;
    prepared.mounts = parse_mounts(spec.mounts).map_err(cleanup)?;
    // A network namespace of its own gets Docker's two defaults: any port
    // and ping without root. Host networking, and `--network container:`
    // (that container already has them), keep what they have.
    if spec.flags & run_flags::NET_HOST == 0 && prepared.net_join.is_none() {
        super::hostopts::add_net_defaults(&mut host.sysctls);
    }
    let bridged = !spec.net.bridge.is_empty() && spec.flags & (run_flags::NET_HOST | run_flags::NET_NONE) == 0 && prepared.net_join.is_none();
    let mut go = None;
    // Sysctls are written from here once the namespaces exist: the child
    // waits for that as it does for its network.
    if bridged || !host.sysctls.is_empty() {
        let (r, w) = sys::pipe().map_err(cleanup)?;
        prepared.net_ready = Some(r);
        go = Some(w);
    }
    // The child says when its root and namespace are in place; a start that
    // returns before that lets an immediate exec join the agent's root
    // instead of the container's, where its image users don't exist (#311).
    let (root_ready, child_root_ready) = sys::pipe().map_err(cleanup)?;
    prepared.root_ready = Some(child_root_ready);
    let (parent_io, child_io) = stdio(&prepared).map_err(cleanup)?;
    let rootfs_c = sys::cstr(&rootfs).map_err(cleanup)?;

    let mut flags = (libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC | libc::CLONE_NEWCGROUP) as u64 | CLONE_INTO_CGROUP;
    if spec.flags & run_flags::NET_HOST == 0 && prepared.net_join.is_none() {
        flags |= libc::CLONE_NEWNET as u64;
    }
    // `--pid host` keeps the agent's PID namespace, `--pid container:` takes
    // the other container's; otherwise the container gets its own.
    if spec.flags & run_flags::PID_HOST == 0 && pid_join.is_none() {
        flags |= libc::CLONE_NEWPID as u64;
    }
    let args = CloneArgs { flags, exit_signal: libc::SIGCHLD as u64, cgroup: cgroup_fd.as_raw_fd() as u64, ..Default::default() };

    // setns(CLONE_NEWPID) only changes where the agent's children go, so the
    // child is cloned straight into the other container's PID namespace, as
    // an exec is, and the agent's own is put back right after.
    let own_pid_ns = match &pid_join {
        Some(ns) => {
            let own = fs::File::open("/proc/self/ns/pid").map_err(cleanup)?;
            sys::setns(ns.as_raw_fd(), libc::CLONE_NEWPID).map_err(cleanup)?;
            Some(own)
        }
        None => None,
    };
    // SAFETY: the agent is single-threaded, as clone3 without CLONE_VM needs.
    let cloned = unsafe { sys::clone3(&args) };
    if let Ok(0) = cloned {
        container_child(&prepared, &rootfs_c, &child_io);
    }
    let restored = own_pid_ns.map_or(Ok(()), |own| sys::setns(own.as_raw_fd(), libc::CLONE_NEWPID));
    drop(pid_join);
    let pid = cloned.map_err(cleanup)?;
    if let Err(e) = restored {
        // The agent's next children would land in the other container: a
        // start that cannot undo that is not one to go on with.
        // SAFETY: killing and reaping the child we just cloned.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        return Err(cleanup(e));
    }
    // The child holds its own copy of the write end: only then does closing
    // it (the child exiting early) read as EOF here.
    drop(prepared.root_ready.take());
    if flags & libc::CLONE_NEWNET as u64 != 0 {
        netns = fs::File::open(format!("/proc/{pid}/ns/net")).map_or(0, |f| netns_inode(&f));
    }
    drop(cgroup_fd);
    // The child makes its mounts before it waits for its network, and a
    // mount that fails ends it with a message of its own. A step here then
    // fails on a child that is already gone ("dns: No such file or
    // directory", "network: Out of memory"): that is only where it was
    // noticed, so the child's own message is the error.
    let report = match &parent_io {
        ParentStdio::Tty { sock } => sock.as_raw_fd(),
        ParentStdio::Pipes { stderr, .. } => stderr.as_raw_fd(),
    };
    // Set once the start goes on without the child's root (see below).
    let starting = std::cell::Cell::new(false);
    // The child never becomes a Process, so nothing else tears it down:
    // its root goes back here, with its cgroup and runtime files.
    let kill_child = |e: io::Error| {
        // A child still filling a new volume finishes first, as it did when
        // the start always waited for it: killed half-way, the volume would
        // stay half full and, no longer empty, never be filled again.
        if starting.get() {
            let _ = root_word(root_ready.as_raw_fd(), None);
        }
        let own = child_report(report);
        // SAFETY: killing the child we just cloned.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        // Gone before its cgroup and root go: its namespace holds the root
        // until then, and a busy cgroup can't be removed.
        // SAFETY: reaping the child we just killed; it isn't in `procs`.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        undo_now(&undo, remove);
        match own {
            Some(why) => io::Error::new(e.kind(), why),
            None => e,
        }
    };
    // Before `pivot_root`, an exec joining its mount namespace gets the
    // agent's `/etc/passwd`, and an image user such as `dev` is unknown
    // (#311). So the start waits for the root, briefly (ROOT_READY_WAIT):
    // past that the child is filling a new volume, and the start goes on
    // without it, answered `starting`; the agent lets nothing enter it until
    // `Process::root` says it is in place, and the daemon waits for that
    // with `AwaitStart`. Nothing below needs the child's root: the network,
    // DNS and sysctls go through its namespaces (/proc/<pid>/ns).
    // A start with a terminal (`-t`) is the exception: its master comes from
    // the child only once its root and network are in place
    // (`finish_stdio` waits for it), so it waits for the root as before.
    match root_word(root_ready.as_raw_fd(), Some(ROOT_READY_WAIT)) {
        Ok(RootWord::Ready) => {}
        Ok(RootWord::Ended) => return Err(kill_child(io::Error::other(NOT_READY))),
        Ok(RootWord::NotYet) if prepared.tty => match root_word(root_ready.as_raw_fd(), None) {
            Ok(RootWord::Ready) => {}
            Ok(_) => return Err(kill_child(io::Error::other(NOT_READY))),
            Err(e) => return Err(kill_child(e)),
        },
        Ok(RootWord::NotYet) => starting.set(true),
        Err(e) => return Err(kill_child(e)),
    }
    let mut veths = Vec::new();
    // Names, AAAA records and `--dns` servers to give the DNS server, once
    // nothing can fail any more: a failed start must not leave its names
    // answering (#325). Nothing is answered before then anyway, since the
    // agent serves DNS from its loop, after this returns.
    let mut names: Option<Names> = None;
    // Not bridged, but with sysctls to write: no network to wait for.
    if let Some(go) = go.take_if(|_| !bridged) {
        drop(prepared.net_ready.take());
        super::hostopts::apply_sysctls(pid, &host.sysctls).map_err(kill_child)?;
        // SAFETY: one byte to a pipe we own: "go on".
        unsafe { libc::write(go.as_raw_fd(), b"g".as_ptr().cast(), 1) };
    }
    if let Some(go) = go {
        drop(prepared.net_ready.take());
        // Named after the container, plus a per-start counter: after a
        // restart the previous veth may still be on its way out with the old
        // network namespace. 15 characters at most, the kernel's limit.
        static STARTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = STARTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0xffff;
        let host_if = format!("kv{}{n:04x}", &spec.id[..spec.id.len().min(9)]);
        let mac = net_opts::values(spec.net.opts, net_opts::MAC).next().and_then(net::parse_mac);
        let v6 = net::Ip6::from_opts(spec.net.opts);
        let plug = net::Plug::from_opts(spec.net.opts);
        net::attach_container(net.bridges, spec.net.bridge, &host_if, pid, spec.net.ip, spec.net.prefix, spec.net.gateway, mac, v6, plug)
            .map_err(|e| kill_child(io::Error::new(e.kind(), format!("network: {e}"))))?;
        veths.push(host_if);
        if embedded_dns {
            if let Some(name) = net_opts::values(spec.net.opts, net_opts::NETWORK).next() {
                net.dns.name_network(spec.net.bridge, name);
            }
            // An IPv6-only network's bridge has no IPv4 address to serve on,
            // and its containers no A records.
            if !plug.no_ipv4 {
                net.dns.serve(spec.net.bridge, spec.net.gateway).map_err(|e| kill_child(io::Error::new(e.kind(), format!("dns: {e}"))))?;
            }
            // 127.0.0.11 in the container, when its resolv.conf names it. Not
            // when it names the gateway (the default bridge in the VM): the
            // listener would only hold port 53, and a DNS server in the
            // container could not bind it.
            if nameserver == Some(super::dns::EMBEDDED) {
                // `pid` is this new child's, so a server already kept under
                // it belonged to a process that exited and was reaped, still
                // waiting for its output to be read (see `admit`): it must
                // not stand in for this container's own.
                net.dns.drop_container(pid);
                std::fs::File::open(format!("/proc/{pid}/ns/net"))
                    .and_then(|ns| {
                        net.dns.serve_in(&ns, pid, spec.net.ip, spec.net.bridge)?;
                        // User networks it joins once it runs: their names
                        // answer from its first lookup (#449).
                        for b in net_opts::values(spec.net.opts, net_opts::DNS_BRIDGE) {
                            net.dns.serve_in(&ns, pid, spec.net.ip, b)?;
                        }
                        Ok(())
                    })
                    .map_err(|e| kill_child(io::Error::new(e.kind(), format!("dns: {e}"))))?;
            }
            // `--dns`: names the network does not know go to these instead.
            let servers: Vec<[u8; 4]> = net_opts::values(spec.net.opts, net_opts::NAMESERVER).filter_map(net::parse_ip).collect();
            names = Some(Names { v4: !plug.no_ipv4, v6: v6.map(|v| v.ip), servers });
        }
        // Its 127.0.0.11 listener goes with it.
        super::hostopts::apply_sysctls(pid, &host.sysctls).map_err(|e| {
            net.dns.drop_container(pid);
            kill_child(e)
        })?;
        // SAFETY: one byte to a pipe we own: "your network is ready".
        unsafe { libc::write(go.as_raw_fd(), b"g".as_ptr().cast(), 1) };
    }
    let (stdin, stdout, stderr) = finish_stdio(parent_io, child_io).map_err(|e| {
        net.dns.drop_container(pid);
        kill_child(e)
    })?;
    if let Some(Names { v4, v6, servers }) = names {
        if v4 {
            net.dns.register(spec.net.bridge, spec.net.dns_names.iter(), spec.net.ip);
        }
        if let Some(ip) = v6 {
            net.dns.register6(spec.net.bridge, spec.net.dns_names.iter(), spec.net.ip, ip);
        }
        net.dns.set_upstream(spec.net.ip, servers);
    }
    Ok(Process {
        id: spec.id.to_owned(),
        container: None,
        pid,
        stdout,
        stderr,
        stdin,
        tty: prepared.tty,
        dns_entries: (bridged && embedded_dns).then(|| (spec.net.bridge.to_owned(), spec.net.ip)).into_iter().collect(),
        veths,
        netns,
        joins_netns: net_opts::values(spec.net.opts, net_opts::NETNS).next().is_some(),
        seccomp: spec.seccomp.to_vec(),
        teardown: Some(Teardown { base, cgroup, remove }),
        starting: starting.get().then_some(root_ready),
        ended_before_ready: None,
    })
}

/// Marks a container whose writable layer holds only whole files and whole
/// renamed directories (no overlayfs redirects or metadata-only copies).
const PLAIN_UPPER: &str = "upper-plain";

/// Whether the container's writable layer is plain: marked so, or still
/// empty, in which case it is marked now, before its first mount writes
/// anything. One written under a kernel's own defaults before Kiri chose
/// (Arch turns redirects and metacopy on) isn't, and stays readable only
/// with those defaults.
fn plain_upper(base: &Path) -> io::Result<bool> {
    let marker = base.join(PLAIN_UPPER);
    if marker.exists() {
        return Ok(true);
    }
    if fs::read_dir(base.join("upper"))?.next().is_some() {
        // A mount that began at the same time may have marked it and
        // written into it since this one looked for the marker.
        return Ok(marker.exists());
    }
    fs::write(marker, b"")?;
    Ok(true)
}

/// The overlayfs features to mount with, whatever the kernel's defaults
/// (Arch, CachyOS and xanmod turn them on, #227):
/// - index=off always: with index on, a second mount of an upper dir still
///   in use fails with EBUSY, and a copy into a created container mounts it
///   while the last mount is being let go. Safe for any existing upper dir.
/// - redirect_dir=nofollow, metacopy=off for a plain upper dir: `docker
///   diff` and `commit` read the upper dir as it is, so it must hold whole
///   renamed directories and whole files. An upper dir written with those
///   features keeps the kernel's defaults, or its redirected directories and
///   metadata-only files would be refused (EPERM) after an update.
fn overlay_features(plain: bool) -> &'static str {
    if plain { "index=off,redirect_dir=nofollow,metacopy=off" } else { "index=off" }
}

/// Mounts a container's root: its image layers under its writable layer,
/// at `<containers>/<id>/rootfs`. Returns that path. Only `roots` calls
/// this, which keeps it to one mount per container.
pub(super) fn mount_rootfs(paths: &Paths, id: &str, image_layers: &[&str]) -> io::Result<PathBuf> {
    valid_id(id)?;
    let base = paths.containers.join(id);
    let (upper, work, rootfs) = (base.join("upper"), base.join("work"), base.join("rootfs"));
    let _ = sys::umount_lazy(&rootfs);
    for d in [&upper, &work, &rootfs] {
        fs::create_dir_all(d)?;
    }
    // Lower dirs are relative to the agent's working directory (the layer
    // store), which keeps the option string short enough for many layers.
    let mut lower = String::with_capacity(image_layers.len() * 65);
    let mut seen = Vec::new();
    for digest in image_layers.iter() {
        if !layers::present(&paths.layers, digest) {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("layer {digest} is not unpacked")));
        }
        // An image can hold the same layer twice (often the empty one,
        // sha256:4f4fb700…), and the store keeps it once: overlayfs refuses
        // a directory listed twice, with ELOOP. The layers come top first,
        // and the topmost copy alone gives the same tree, so later ones go.
        if seen.contains(&digest) {
            continue;
        }
        seen.push(digest);
        if !lower.is_empty() {
            lower.push(':');
        }
        lower.push_str(layers::hex(digest)?);
    }
    let opts = format!("lowerdir={lower},upperdir={},workdir={},{}", upper.display(), work.display(), overlay_features(plain_upper(&base)?));
    sys::mount(c"overlay", &sys::cstr(&rootfs)?, c"overlay", 0, Some(&CString::new(opts)?))?;
    Ok(rootfs)
}

/// Starts a process inside a running container: same namespaces, same cgroup.
pub fn exec(spec: &ExecSpec<'_>, target_pid: libc::pid_t, paths: &Paths, profile: &[u8]) -> io::Result<Process> {
    if spec.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no command to run"));
    }
    valid_id(spec.exec_id)?;
    let ns = |name: &str| fs::File::open(format!("/proc/{target_pid}/ns/{name}"));
    let target_pid_ns = ns("pid")?;
    let namespaces = [
        (ns("net")?, libc::CLONE_NEWNET),
        (ns("uts")?, libc::CLONE_NEWUTS),
        (ns("ipc")?, libc::CLONE_NEWIPC),
        (ns("cgroup")?, libc::CLONE_NEWCGROUP),
        // Last: entering the mount namespace also moves the root.
        (ns("mnt")?, libc::CLONE_NEWNS),
    ];
    let own_pid_ns = fs::File::open("/proc/self/ns/pid")?;
    let cgroup_fd = fs::File::open(paths.cgroup.join(spec.container_id))?;

    let mut prepared = prepare(spec.argv, spec.env, spec.cwd, spec.user, None, spec.flags, spec.caps)?;
    // The container's ulimits and groups apply to its execs too, as with runc.
    let host = super::hostopts::parse(spec.opts)?;
    prepared.host = HostApplied { rlimits: host.rlimits, groups: host.groups, nodes: Vec::new() };
    // An exec runs under its container's profile, not the default.
    prepared.profile = profile.to_vec();
    let (parent_io, child_io) = stdio(&prepared)?;
    let args = CloneArgs {
        flags: CLONE_INTO_CGROUP,
        exit_signal: libc::SIGCHLD as u64,
        cgroup: cgroup_fd.as_raw_fd() as u64,
        ..Default::default()
    };

    // setns(CLONE_NEWPID) only changes where the agent's *children* go, so
    // the exec is cloned straight into the container's PID namespace and
    // stays the agent's direct child. The agent's own namespace is put back
    // right after.
    sys::setns(target_pid_ns.as_raw_fd(), libc::CLONE_NEWPID)?;
    // SAFETY: the agent is single-threaded, as clone3 without CLONE_VM needs.
    let mut cloned = unsafe { sys::clone3(&args) };
    // A container that manages its own cgroups (Docker in Docker, BuildKit)
    // moves its processes into sub-cgroups, after which its own cgroup takes
    // no processes. Join the one its init lives in, as runc does.
    let init_cgroup;
    if cloned.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::ResourceBusy) {
        let rel = fs::read_to_string(format!("/proc/{target_pid}/cgroup")).unwrap_or_default();
        let rel = rel.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("").trim_start_matches('/').to_owned();
        init_cgroup = fs::File::open(std::path::Path::new("/sys/fs/cgroup").join(&rel));
        match &init_cgroup {
            Ok(fd) => {
                let retry = CloneArgs { cgroup: fd.as_raw_fd() as u64, ..args };
                // SAFETY: as above.
                cloned = unsafe { sys::clone3(&retry) }.map_err(|e| io::Error::new(e.kind(), format!("joining cgroup {rel}: {e}")));
            }
            Err(e) => cloned = Err(io::Error::new(e.kind(), format!("cgroup {rel}: {e}"))),
        }
    }
    if let Ok(0) = cloned {
        exec_child(&prepared, &namespaces, &child_io);
    }
    let restored = sys::setns(own_pid_ns.as_raw_fd(), libc::CLONE_NEWPID);
    let pid = cloned?;
    restored?;
    drop(cgroup_fd);
    let (stdin, stdout, stderr) = finish_stdio(parent_io, child_io).map_err(|e| {
        // SAFETY: killing the child we just cloned.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        e
    })?;
    Ok(Process {
        id: spec.exec_id.to_owned(),
        container: Some(spec.container_id.to_owned()),
        pid,
        stdout,
        stderr,
        stdin,
        tty: prepared.tty,
        dns_entries: Vec::new(),
        veths: Vec::new(),
        netns: 0,
        joins_netns: false,
        seccomp: Vec::new(),
        teardown: None,
        starting: None,
        ended_before_ready: None,
    })
}

/// Unmounts a container and, for `--rm`, deletes its writable layer.
/// Runs after the exit has been reported, off the critical path. Execs have
/// nothing to undo.
/// Whether the container's processes were killed for memory. Read before
/// `teardown`, which removes the cgroup.
pub fn oom_killed(p: &Process) -> bool {
    p.teardown.as_ref().is_some_and(|t| super::cgroup::oom_killed(&t.cgroup))
}

/// `netns_shared`: another live process still uses the container's network
/// namespace (a container started with `--network container:`). Only then
/// are its veths deleted here, or they would live on with that namespace
/// and keep its address. Otherwise the next container to be plugged in
/// deletes them (see `net::retire`).
pub fn teardown(p: &Process, netns_shared: bool) {
    if netns_shared {
        for v in &p.veths {
            let _ = net::detach(v);
        }
    } else {
        net::retire(&p.veths);
    }
    if let Some(t) = &p.teardown {
        undo_now(t, t.remove);
    }
}

fn netns_inode(f: &fs::File) -> u64 {
    use std::os::unix::fs::MetadataExt;
    f.metadata().map_or(0, |m| m.ino())
}

/// Removes a cgroup and every cgroup below it, deepest first. They are
/// empty once the container's processes are gone; rmdir is all it takes.
fn remove_cgroup(dir: &std::path::Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                remove_cgroup(&e.path());
            }
        }
    }
    let _ = fs::remove_dir(dir);
}

fn undo_now(t: &Teardown, remove_files: bool) {
    // A copy still in progress keeps the root mounted, and a removed
    // container's files, until it ends.
    super::roots::release(&t.base);
    if let Some(id) = t.base.file_name() {
        let _ = fs::remove_dir_all(PathBuf::from("/run/kiri").join(id));
    }
    remove_cgroup(&t.cgroup);
    if remove_files {
        super::roots::remove(&t.base);
    }
}

/// Deletes a stopped container's directory (its writable layer).
/// Deletes a named volume's data.
/// Where volumes live: `<name>/_data` under it.
pub const VOLUMES: &str = "/var/lib/kiri/volumes";

pub fn remove_volume(name: &str) {
    // Docker's rule for volume names: [a-zA-Z0-9][a-zA-Z0-9_.-]*
    let valid = name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
    if valid {
        // An NFS export or a host folder mounted there is not the volume's
        // to delete: unmount it first, and keep everything if that fails.
        let data = format!("/var/lib/kiri/volumes/{name}/_data");
        if !super::volmount::release(&data) {
            eprintln!("kiri-init: volume {name}: {data} is still mounted, not deleting it");
            return;
        }
        let _ = fs::remove_dir_all(PathBuf::from(VOLUMES).join(name));
    }
}

pub fn remove(paths: &Paths, id: &str) {
    if valid_id(id).is_ok() {
        super::roots::remove(&paths.containers.join(id));
    }
}

/// Linux host: what a previous agent that died without cleaning up (killed,
/// or its machine's daemon killed with it) left of this data directory's
/// containers under `cgroup`: their processes are killed, their mounts,
/// runtime files and cgroups removed, and a veth something else kept alive
/// deleted. Only containers of this data directory are touched: the
/// cgroup and the network namespace may hold another engine's too.
pub fn sweep(paths: &Paths, cgroup: &std::path::Path) {
    let Ok(entries) = fs::read_dir(&paths.containers) else { return };
    let ids: Vec<String> = entries.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|id| valid_id(id).is_ok() && id.len() >= 12).collect();
    for id in &ids {
        let cg = cgroup.join(id);
        if cg.is_dir() {
            kill_cgroup(&cg);
        }
        // Every root this data directory left mounted, stopped containers'
        // too (a copy in progress when the agent went), as many times as it
        // was stacked: the next start mounts it fresh.
        let rootfs = paths.containers.join(id).join("rootfs");
        for _ in 0..16 {
            if sys::umount_lazy(&rootfs).is_err() {
                break;
            }
        }
        // systemd removes a stopped service's cgroups (#230), so this runs
        // whether or not the container's cgroup is still there.
        let _ = fs::remove_dir_all(PathBuf::from("/run/kiri").join(id));
        if cg.is_dir() {
            remove_cgroup(&cg);
        }
    }
    // Host-side veths are kv<9 characters of the id><4 hex digits>.
    let Ok(links) = fs::read_dir("/sys/class/net") else { return };
    for link in links.flatten() {
        let Ok(name) = link.file_name().into_string() else { continue };
        let ours = name.strip_prefix("kv").and_then(|rest| rest.get(..9)).is_some_and(|p| ids.iter().any(|id| id.starts_with(p)));
        if ours {
            let _ = net::detach(&name);
        }
    }
}

/// Kills every process in a cgroup and below it, and waits (briefly) until
/// it is empty.
fn kill_cgroup(dir: &std::path::Path) {
    // cgroup.kill is Linux 5.14+; before it, each process, deepest first,
    // frozen first: a process that exits between reading cgroup.procs and
    // the kill frees its pid for another process to take (#324). Frozen
    // (waited for), none exits on its own, and SIGKILL still ends them.
    if fs::write(dir.join("cgroup.kill"), "1").is_err() {
        let _ = super::cgroup::freeze(dir, true);
        fn each(dir: &std::path::Path) {
            if let Ok(entries) = fs::read_dir(dir) {
                for e in entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
                    each(&e.path());
                }
            }
            for pid in fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default().lines().filter_map(|l| l.parse::<libc::pid_t>().ok()) {
                // SAFETY: SIGKILL to a process of a container cgroup we own.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        each(dir);
    }
    for _ in 0..300 {
        if fs::read_to_string(dir.join("cgroup.events")).map_or(true, |e| e.lines().any(|l| l == "populated 0")) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Something outlived the kill (stuck in the kernel): thawed, so the
    // cgroup isn't left frozen for the next start of this container.
    let _ = fs::write(dir.join("cgroup.freeze"), "0");
}

/// Deletes every container directory not in `keep` and not in `running`.
pub fn retain(paths: &Paths, keep: StrList<'_>, running: &[&str]) {
    let Ok(entries) = fs::read_dir(&paths.containers) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !keep.iter().any(|k| k == name) && !running.contains(&name) {
            remove(paths, name);
        }
    }
}

pub(super) fn valid_id(id: &str) -> io::Result<()> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad id"));
    }
    Ok(())
}

/// Creates `cwd` and its parents, 0755, when it is missing. Errors are left
/// to the chdir after the user switch, which reports them.
///
/// The path is looked up inside the container's root, one component at a
/// time, never walked as it is: this runs while the child still holds the
/// agent's descriptors, and a plain `mkdir -p` of `/proc/self/fd/N/..`
/// would make directories outside the container, as root.
fn make_workdir(root: BorrowedFd<'_>, cwd: &std::ffi::CStr) {
    let _ = inroot::open(root, Path::new(std::ffi::OsStr::from_bytes(cwd.to_bytes())), Last::Dir);
}

fn prepare(argv: StrList<'_>, env: StrList<'_>, cwd: &str, user: &str, hostname: Option<&str>, flags: u32, caps: u64) -> io::Result<Prepared> {
    let c = |s: &str| CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argument contains NUL"));
    let argv = argv.iter().map(c).collect::<io::Result<Vec<_>>>()?;
    let mut envp = env.iter().map(c).collect::<io::Result<Vec<_>>>()?;
    let path = env.iter().find_map(|e| e.strip_prefix("PATH=")).unwrap_or(DEFAULT_PATH);
    if !env.iter().any(|e| e.starts_with("PATH=")) {
        envp.push(c(&format!("PATH={DEFAULT_PATH}"))?);
    }
    if let Some(h) = hostname {
        if !env.iter().any(|e| e.starts_with("HOSTNAME=")) {
            envp.push(c(&format!("HOSTNAME={h}"))?);
        }
    }
    let tty = flags & run_flags::TTY != 0;
    if tty && !env.iter().any(|e| e.starts_with("TERM=")) {
        envp.push(c("TERM=xterm")?);
    }
    Ok(Prepared {
        argv,
        envp,
        path_dirs: path.split(':').filter(|d| !d.is_empty()).map(|d| d.as_bytes().to_vec()).collect(),
        hostname: hostname.map(|h| h.as_bytes().to_vec()),
        domainname: None,
        net_join: None,
        etc_files: Vec::new(),
        mounts: Vec::new(),
        net_ready: None,
        cwd: c(if cwd.is_empty() { "/" } else { cwd })?,
        user: user.to_owned(),
        net_none: flags & run_flags::NET_NONE != 0,
        tty,
        stdin: flags & run_flags::STDIN != 0,
        privileged: flags & run_flags::PRIVILEGED != 0,
        caps: if caps == 0 { default_caps() } else { caps },
        seccomp: flags & (run_flags::PRIVILEGED | run_flags::NO_SECCOMP) == 0,
        profile: Vec::new(),
        no_new_privs: flags & run_flags::NO_NEW_PRIVS != 0,
        readonly_rootfs: flags & run_flags::READONLY_ROOTFS != 0,
        shm_size: 64 << 20,
        init: flags & run_flags::INIT != 0,
        host: HostApplied::default(),
        root_ready: None,
    })
}

/// Writes the container's /etc/hosts, /etc/resolv.conf and /etc/hostname
/// beside its writable layer; the child binds them over the image's files.
fn write_etc_files(spec: &RunSpec<'_>, base: &std::path::Path, upstream: &[[u8; 4]], embedded: Option<[u8; 4]>) -> io::Result<Vec<(CString, CString)>> {
    fs::create_dir_all(base)?;
    // `--network container:<id>` shares that container's files, as Docker does.
    if let Some(target) = net_opts::values(spec.net.opts, net_opts::NETNS).next() {
        valid_id(target)?;
        let theirs = PathBuf::from("/run/kiri").join(target);
        let mut out = Vec::new();
        for name in ["hosts", "resolv.conf", "hostname"] {
            let src = theirs.join(name);
            fs::metadata(&src).map_err(|e| io::Error::new(e.kind(), format!("network container {target}: {name}: {e}")))?;
            out.push((sys::cstr(&src)?, CString::new(format!("etc/{name}")).expect("no NUL")));
        }
        return Ok(out);
    }
    let domain = net_opts::values(spec.net.opts, net_opts::DOMAINNAME).next().filter(|d| !d.is_empty());
    let mut hosts = String::from("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    // On an IPv6-only network the IPv4 address is Kiri's own name for it.
    let v4 = spec.net.ip != [0; 4] && !net::Plug::from_opts(spec.net.opts).no_ipv4;
    if v4 {
        match domain {
            Some(d) => hosts.push_str(&format!("{}\t{}.{d} {}\n", net::ip_str(spec.net.ip), spec.hostname, spec.hostname)),
            None => hosts.push_str(&format!("{}\t{}\n", net::ip_str(spec.net.ip), spec.hostname)),
        }
    }
    if spec.net.ip != [0; 4] {
        // On an IPv6 network the IPv6 address names it too, as Docker writes it.
        if let Some(v) = net::Ip6::from_opts(spec.net.opts) {
            match domain {
                Some(d) => hosts.push_str(&format!("{}\t{}.{d} {}\n", net::ip6_str(v.ip), spec.hostname, spec.hostname)),
                None => hosts.push_str(&format!("{}\t{}\n", net::ip6_str(v.ip), spec.hostname)),
            }
        }
    }
    // On a Linux host there is no loopback redirect; host.docker.internal
    // points at the bridge gateway, like Docker's host-gateway, so it reaches
    // host services on 0.0.0.0 but not ones bound only to 127.0.0.1.
    // A host-network container is on the host already, loopback included,
    // so there the name is the loopback address.
    let host_addr = net::ip_str(net::HOST_ADDR);
    let host_net = spec.flags & run_flags::NET_HOST != 0;
    let gateway = if host_net { "127.0.0.1".to_owned() } else { net::ip_str(spec.net.gateway) };
    let map_host = super::transport::is_host_process() && (host_net || spec.net.gateway != [0; 4]);
    for line in spec.net.extra_hosts.iter() {
        if map_host {
            hosts.push_str(&line.replace(&host_addr, &gateway));
        } else {
            hosts.push_str(line);
        }
        hosts.push('\n');
    }
    let resolv = resolv_conf(spec, upstream, embedded);
    let files = [("hosts", hosts), ("resolv.conf", resolv), ("hostname", format!("{}\n", spec.hostname))];
    let mut out = Vec::with_capacity(files.len());
    for (name, text) in files {
        let src = base.join(name);
        fs::write(&src, text)?;
        out.push((sys::cstr(&src)?, CString::new(format!("etc/{name}")).expect("no NUL")));
    }
    Ok(out)
}

/// The container's resolv.conf. On a user-defined network the embedded DNS
/// at 127.0.0.11 is the only nameserver, as with Docker (it forwards to
/// `--dns` servers); elsewhere `--dns` servers replace the VM's.
/// `--dns-search` and `--dns-option` apply either way.
/// Whether the container's DNS goes to the agent's resolver (127.0.0.11):
/// on a user network, as Docker's embedded DNS; and on the default bridge
/// when the host's resolver is one of its own addresses (net::local_resolver)
/// or, in the VM (`relay`), so that lookups the Mac's gateway resolver fails
/// can go through the host (dns.rs), unless the container names its own
/// servers (`--dns`).
fn embedded_dns(spec: &RunSpec<'_>, upstream: &[[u8; 4]], relay: bool) -> bool {
    if spec.flags & run_flags::DNS_GATEWAY != 0 {
        return true;
    }
    let bridged = !spec.net.bridge.is_empty()
        && spec.flags & (run_flags::NET_HOST | run_flags::NET_NONE) == 0
        && net_opts::values(spec.net.opts, net_opts::NETNS).next().is_none();
    bridged && net_opts::values(spec.net.opts, net_opts::NAMESERVER).next().is_none() && (relay || net::local_resolver(upstream))
}

/// The address a container that uses the agent's resolver names in its
/// resolv.conf: 127.0.0.11 on a user network, as Docker's embedded DNS;
/// on the default bridge in the VM, the bridge's gateway, where the agent
/// answers too. Not a loopback address there, because what runs builds
/// in a container takes loopback nameservers out of the copy it gives
/// each step and puts Google's in their place (BuildKit does, for every
/// step that isn't `--network=host`): those steps would ask 8.8.8.8, not
/// the Mac's resolver, and VPN and company names would stop resolving.
fn embedded_address(spec: &RunSpec<'_>, relay: bool) -> [u8; 4] {
    if relay && spec.flags & run_flags::DNS_GATEWAY == 0 && spec.net.gateway != [0; 4] { spec.net.gateway } else { super::dns::EMBEDDED }
}

fn resolv_conf(spec: &RunSpec<'_>, upstream: &[[u8; 4]], embedded: Option<[u8; 4]>) -> String {
    let mut resolv = String::new();
    let mut servers = net_opts::values(spec.net.opts, net_opts::NAMESERVER).peekable();
    if let Some(ns) = embedded {
        resolv.push_str(&format!("nameserver {}\n", net::ip_str(ns)));
    } else if servers.peek().is_some() {
        for d in servers {
            resolv.push_str(&format!("nameserver {d}\n"));
        }
    } else if upstream.is_empty() {
        // The VM has no lease yet; a public resolver beats none.
        resolv.push_str("nameserver 1.1.1.1\n");
    } else {
        for d in upstream {
            resolv.push_str(&format!("nameserver {}\n", net::ip_str(*d)));
        }
    }
    // `--dns-search .` means no search domains.
    let search: Vec<&str> = net_opts::values(spec.net.opts, net_opts::SEARCH).filter(|s| *s != ".").collect();
    if !search.is_empty() {
        resolv.push_str(&format!("search {}\n", search.join(" ")));
    }
    let options: Vec<&str> = net_opts::values(spec.net.opts, net_opts::OPTION).collect();
    if !options.is_empty() {
        resolv.push_str(&format!("options {}\n", options.join(" ")));
    }
    resolv
}

/// A running container's resolv.conf once it has joined a user network
/// (`network connect`): the embedded DNS is its only nameserver, as Docker
/// rewrites the file (#449); search domains and options stay. Also the
/// servers it named that `is_kiri` doesn't know as Kiri's own resolvers
/// (`--dns`), for the embedded DNS to forward to. None when it already
/// names the embedded DNS or names no server at all.
pub(super) fn joined_user_network(resolv: &str, is_kiri: impl Fn([u8; 4]) -> bool) -> Option<(String, Vec<[u8; 4]>)> {
    let named: Vec<[u8; 4]> = resolv.lines().filter_map(|l| l.strip_prefix("nameserver")).filter_map(|a| net::parse_ip(a.trim())).collect();
    if named.is_empty() || named.contains(&super::dns::EMBEDDED) {
        return None;
    }
    let mut text = format!("nameserver {}\n", net::ip_str(super::dns::EMBEDDED));
    for line in resolv.lines().filter(|l| !l.starts_with("nameserver")) {
        text.push_str(line);
        text.push('\n');
    }
    Some((text, named.into_iter().filter(|a| !is_kiri(*a)).collect()))
}

/// Points running container `id`'s resolv.conf at the embedded DNS after it
/// joined a user network, rewriting the file the container has bound in
/// place, so the container sees the change. Returns its `--dns` servers.
pub(super) fn use_embedded_dns(id: &str, is_kiri: impl Fn([u8; 4]) -> bool) -> io::Result<Option<Vec<[u8; 4]>>> {
    valid_id(id)?;
    let file = PathBuf::from("/run/kiri").join(id).join("resolv.conf");
    let Some((text, servers)) = joined_user_network(&fs::read_to_string(&file)?, is_kiri) else { return Ok(None) };
    // Truncated and written, not replaced: the container's bind mount
    // holds this inode.
    fs::write(&file, text)?;
    Ok(Some(servers))
}

/// One mount, resolved before clone.
struct Mount {
    /// A guest path, or None for a tmpfs.
    source: Option<CString>,
    /// Relative to the container root (the child mounts before pivot_root).
    target: CString,
    ro: bool,
    dir: bool,
    /// Fill an empty volume from the image's files at the target, as
    /// Docker does the first time a volume is used.
    copy_up: bool,
    /// tmpfs options (`size=64m,mode=1777`).
    tmpfs_opts: Option<CString>,
    /// tmpfs mount flags (`noexec`, `nosuid`, `nodev`, `ro`).
    tmpfs_flags: libc::c_ulong,
}

/// `source\ttarget\tflags` entries. Flags: `ro`, `create` (make the source
/// directory), `copyup`, `tmpfs` (source ignored) and tmpfs options and
/// flags (`noexec`, `exec`, `nosuid`, …; later ones win). A volume with
/// driver options adds `\ttype\tdevice\tflags(hex)\tdata`, and is mounted
/// at its source first.
fn parse_mounts(list: StrList<'_>) -> io::Result<Vec<Mount>> {
    let mut out = Vec::with_capacity(list.len());
    for m in list.iter() {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("bad mount {m:?}"));
        // Three fields: source, target, flags. Seven: the same, then a
        // volume's driver options (`type`, `device`, flags, data). Any other
        // count is a separator inside a field — a target alone comes from an
        // image's VOLUME line — and is refused here as well as at the host.
        let f: Vec<&str> = m.split('\t').collect();
        if f.len() != 3 && f.len() != 7 {
            return Err(bad());
        }
        let (src, given_target, words) = (f[0], f[1], f[2]);
        let flags: Vec<&str> = words.split(',').filter(|f| !f.is_empty()).collect();
        if !given_target.starts_with('/') || given_target.split('/').any(|c| c == "..") {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad mount target {given_target}")));
        }
        let rel = given_target.trim_start_matches('/');
        let target = CString::new(if rel.is_empty() { "." } else { rel })?;
        let ro = flags.contains(&"ro");
        if flags.contains(&"tmpfs") {
            let opts: Vec<&str> = flags.iter().copied().filter(|f| f.contains('=')).collect();
            let opts = if opts.is_empty() { "mode=1777".to_owned() } else { opts.join(",") };
            let tmpfs_flags = tmpfs_flags(&flags);
            let ro = tmpfs_flags & libc::MS_RDONLY != 0;
            out.push(Mount { source: None, target, ro, dir: true, copy_up: false, tmpfs_opts: Some(CString::new(opts)?), tmpfs_flags });
            continue;
        }
        // A missing source becomes a directory, as Docker does; one that is
        // there already (a file such as /etc/hostname, too) is used as it is.
        if flags.contains(&"create") && fs::symlink_metadata(src).is_err() {
            fs::create_dir_all(src)?;
        }
        if f.len() == 7 && !f[3].is_empty() {
            super::volmount::ensure(src, f[3], f[4], f[5], f[6])?;
        }
        let meta = fs::metadata(src).map_err(|e| io::Error::new(e.kind(), format!("mount source {src}: {e}")))?;
        out.push(Mount {
            source: Some(sys::cstr(src)?),
            target,
            ro,
            dir: meta.is_dir(),
            copy_up: flags.contains(&"copyup"),
            tmpfs_opts: None,
            tmpfs_flags: 0,
        });
    }
    // Parents before what's under them, as Docker sorts mounts: the CLI sends
    // `-v` in no set order, and `/app/node_modules` mounted before `/app`
    // would be hidden by it. The sort is stable, so equal depths keep theirs.
    out.sort_by_key(|m| depth(&m.target));
    Ok(out)
}

/// How deep a mount target is: `.` is 0, `app` 1, `app/node_modules` 2.
fn depth(target: &CStr) -> usize {
    let t = target.to_bytes();
    if t == b"." { 0 } else { t.split(|&b| b == b'/').filter(|c| !c.is_empty()).count() }
}

/// A tmpfs's mount flags from its flag words, in order, so `exec` after
/// Docker's default `noexec` wins.
fn tmpfs_flags(words: &[&str]) -> libc::c_ulong {
    let mut f = 0;
    for w in words {
        match *w {
            "ro" => f |= libc::MS_RDONLY,
            "rw" => f &= !libc::MS_RDONLY,
            "noexec" => f |= libc::MS_NOEXEC,
            "exec" => f &= !libc::MS_NOEXEC,
            "nosuid" => f |= libc::MS_NOSUID,
            "suid" => f &= !libc::MS_NOSUID,
            "nodev" => f |= libc::MS_NODEV,
            "dev" => f &= !libc::MS_NODEV,
            "noatime" => f |= libc::MS_NOATIME,
            "atime" => f &= !libc::MS_NOATIME,
            _ => {}
        }
    }
    f
}

/// Mounts one entry into the container (relative target, before
/// pivot_root), creating the target as a directory or an empty file. The
/// target is looked up inside `root`, so an image's symlink cannot point
/// the mount, the copy-up or the new directory outside the container.
fn apply_mount(root: BorrowedFd<'_>, image: Option<BorrowedFd<'_>>, m: &Mount) -> io::Result<()> {
    let t = Path::new(std::ffi::OsStr::from_bytes(m.target.to_bytes()));
    if let Some(opts) = &m.tmpfs_opts {
        let at = inroot::open(root, t, Last::Dir)?;
        return sys::mount(c"tmpfs", &at.proc_path(), c"tmpfs", m.tmpfs_flags, Some(opts));
    }
    let src = m.source.as_ref().expect("bind mounts have a source");
    let at = inroot::open(root, t, if m.dir { Last::Dir } else { Last::File })?;
    let target = at.proc_path();
    if m.dir && m.copy_up {
        // A new volume is filled from the image's directory, as Docker does,
        // never from a bind mounted over it earlier (`./:/app` then
        // `/app/node_modules` must get the image's node_modules, not the
        // host's): looked up in the image's own tree when there is one.
        // The image tree also holds the container's own layer, so it can show
        // the folder Kiri just made (or made on an earlier start) for this
        // mount. That isn't the image's: copying its root-owned 755 over the
        // volume would undo a tmpfs volume's uid=, gid= and mode= (#296).
        // So: when the lookup reaches the very folder this start created,
        // it's marked and skipped; on later starts the mark skips it. The
        // image's own folders have no mark, and one made inside an earlier
        // bind (`./:/app` then `/app/node_modules`) is a different folder, so
        // both still copy up as before.
        let from = match image.and_then(|img| inroot::open(img, t, Last::Exists).ok()) {
            Some(f) if at.created && same_dir(&f, &at) => {
                mark_mountpoint(&target);
                None
            }
            Some(f) if is_mountpoint_mark(&f.proc_path()) => None,
            other => other,
        };
        let from_path = match &from {
            Some(f) => Some(f.proc_path()),
            None if image.is_none() && !at.created => Some(target.clone()),
            None => None,
        };
        if let Some(from_path) = from_path {
            let src_path = Path::new(std::ffi::OsStr::from_bytes(src.to_bytes()));
            let image_path = Path::new(std::ffi::OsStr::from_bytes(from_path.to_bytes()));
            let empty = fs::read_dir(src_path).map(|mut d| d.next().is_none()).unwrap_or(false);
            if empty && image_path.is_dir() {
                copy_tree(image_path, src_path)?;
            }
        }
    }
    sys::mount(src, &target, c"none", libc::MS_BIND | libc::MS_REC, None)?;
    if m.ro {
        // The descriptor is the directory under the new mount; look again
        // to reach the mount itself.
        let at = inroot::open(root, t, Last::Exists)?;
        sys::mount(c"none", &at.proc_path(), c"none", libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC, None)?;
    }
    Ok(())
}

/// The xattr on a folder Kiri made in the container's layer to mount on.
const MOUNTPOINT_MARK: &std::ffi::CStr = c"user.kiri.mountpoint";

fn mark_mountpoint(dir: &std::ffi::CStr) {
    // SAFETY: a valid NUL-terminated path and a one-byte value. A filesystem
    // without user xattrs refuses it; the same-folder check still covers the
    // first start then.
    unsafe { libc::setxattr(dir.as_ptr(), MOUNTPOINT_MARK.as_ptr(), b"1".as_ptr().cast(), 1, 0) };
}

fn is_mountpoint_mark(dir: &std::ffi::CStr) -> bool {
    let mut v = [0u8; 1];
    // SAFETY: reads at most `v.len()` bytes into `v`.
    unsafe { libc::getxattr(dir.as_ptr(), MOUNTPOINT_MARK.as_ptr(), v.as_mut_ptr().cast(), v.len()) >= 0 }
}

/// Both descriptors name the same folder (device and inode).
fn same_dir(a: &inroot::Found, b: &inroot::Found) -> bool {
    use std::os::unix::fs::MetadataExt;
    let meta = |f: &inroot::Found| fs::metadata(Path::new(std::ffi::OsStr::from_bytes(f.proc_path().to_bytes())));
    matches!((meta(a), meta(b)), (Ok(x), Ok(y)) if x.dev() == y.dev() && x.ino() == y.ino())
}

/// A detached copy of the current directory's mount (the container's
/// root) without the mounts on it: the image as it is under any bind.
/// None where the kernel has no open_tree (before Linux 5.2); volumes are
/// then filled from what the path shows, as before.
fn image_tree() -> Option<OwnedFd> {
    const OPEN_TREE_CLONE: libc::c_uint = 1;
    // SAFETY: a valid NUL-terminated path; the result is a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_open_tree, libc::AT_FDCWD, c".".as_ptr(), OPEN_TREE_CLONE | libc::O_CLOEXEC as libc::c_uint) };
    // SAFETY: a descriptor the kernel just returned, owned only here.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Copies a directory tree keeping modes, owners and symlinks: what Docker
/// does to fill a new volume from the image. `from` may be a
/// `/proc/self/fd/N` path to the image's directory (see `apply_mount`): it
/// is opened through that link (that is how the image's folder is reached)
/// and its own owner and mode are read off the opened folder, never off the
/// link, which is root's with mode 000.
///
/// This runs with the agent's full rights, before the container has its own
/// root, and another container that already has the volume mounted can write
/// in it meanwhile. So the volume's own folder is opened once, as its bind
/// mount will resolve it, and nothing inside it is ever followed or looked up
/// by path again. Every entry is made, filled and given its owner and mode through
/// descriptors (`mkdirat`, `openat` with O_EXCL | O_NOFOLLOW, `symlinkat`,
/// `fchown`, `fchmod`). A name that is already there is not touched: a real
/// folder is entered and filled, anything else (a file, a link, even one
/// pointing at a folder) stays as it is and nothing below it is copied. What
/// is in the volume wins, as when a volume is not empty.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> io::Result<()> {
    let src = open_at(libc::AT_FDCWD, &sys::cstr(from)?, libc::O_RDONLY | libc::O_DIRECTORY)?;
    // The volume's own folder is opened as the bind mount of it will be: a
    // volume folder that is a link to another disk, made on the host, still
    // works. Everything up to and including it is the host's volume store,
    // which no container can write in or replace; only what is inside the
    // volume can change during the fill, and nothing inside is followed.
    let dst = open_at(libc::AT_FDCWD, &sys::cstr(to)?, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let st = fstat_at(src.as_raw_fd(), c"", libc::AT_EMPTY_PATH)?;
    fill_dir(src.as_fd(), dst.as_fd())?;
    keep_owner_mode(dst.as_raw_fd(), &st)
}

/// Copies what is in the folder `src` into the folder `dst`, entry by entry.
fn fill_dir(src: BorrowedFd<'_>, dst: BorrowedFd<'_>) -> io::Result<()> {
    // The listing goes through our own descriptor, so a source folder that
    // is replaced meanwhile is not the one read.
    for entry in fs::read_dir(format!("/proc/self/fd/{}", src.as_raw_fd()))? {
        let name = CString::new(entry?.file_name().as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let (s, d) = (src.as_raw_fd(), dst.as_raw_fd());
        let st = fstat_at(s, &name, libc::AT_SYMLINK_NOFOLLOW)?;
        match st.st_mode & libc::S_IFMT {
            libc::S_IFLNK => {
                let target = read_link_at(s, &name)?;
                // SAFETY: valid NUL-terminated strings and a folder descriptor.
                if unsafe { libc::symlinkat(target.as_ptr(), d, name.as_ptr()) } < 0 {
                    match io::Error::last_os_error() {
                        e if e.raw_os_error() == Some(libc::EEXIST) => continue,
                        e => return Err(e),
                    }
                }
                // SAFETY: as above. Owner only (a link has no mode of its
                // own); by name, but never following a link.
                unsafe { libc::fchownat(d, name.as_ptr(), st.st_uid, st.st_gid, libc::AT_SYMLINK_NOFOLLOW) };
            }
            libc::S_IFDIR => {
                let sub = open_at(s, &name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)?;
                // SAFETY: a valid NUL-terminated name and a folder descriptor.
                let made = unsafe { libc::mkdirat(d, name.as_ptr(), 0o700) } == 0;
                if !made && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(io::Error::last_os_error());
                }
                // Whatever is there now, only a real folder is entered; a
                // link or a file that took the name (before or during the
                // fill) is left alone.
                let child = match open_at(d, &name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW) {
                    Ok(fd) => fd,
                    Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR | libc::ENOENT)) => continue,
                    Err(e) => return Err(e),
                };
                fill_dir(sub.as_fd(), child.as_fd())?;
                // A folder that was already there keeps its own owner and mode.
                if made {
                    keep_owner_mode(child.as_raw_fd(), &st)?;
                }
            }
            libc::S_IFREG => {
                // O_NONBLOCK: never wait on a file that turned into a fifo.
                let from = open_at(s, &name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)?;
                // O_NONBLOCK does not cover a device node: what was opened
                // must still be a regular file, or it is skipped, before
                // anything is made at the destination. Owner and mode come
                // from what was opened, not from the lookup by name.
                let st = fstat_at(from.as_raw_fd(), c"", libc::AT_EMPTY_PATH)?;
                if st.st_mode & libc::S_IFMT != libc::S_IFREG {
                    continue;
                }
                // Private until the mode is set, and never an existing name.
                let to = match open_at_mode(d, &name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW, 0o600) {
                    Ok(fd) => fd,
                    Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                    Err(e) => return Err(e),
                };
                let (mut from, mut to) = (fs::File::from(from), fs::File::from(to));
                io::copy(&mut from, &mut to)?; // copy_file_range or sendfile
                keep_owner_mode(to.as_raw_fd(), &st)?;
            }
            _ => {} // devices, fifos and sockets are not copied
        }
    }
    Ok(())
}

/// openat(2) as an owned descriptor (`O_CLOEXEC` always).
fn open_at(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    open_at_mode(dir, name, flags, 0)
}

fn open_at_mode(dir: RawFd, name: &CStr, flags: libc::c_int, mode: libc::mode_t) -> io::Result<OwnedFd> {
    // SAFETY: a valid NUL-terminated name; the result is a new descriptor or -1.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC, mode as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor the kernel just returned, owned only here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat_at(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<libc::stat> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a valid NUL-terminated name and a buffer the size of `stat`.
    if unsafe { libc::fstatat(dir, name.as_ptr(), st.as_mut_ptr(), flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat filled it in.
    Ok(unsafe { st.assume_init() })
}

fn read_link_at(dir: RawFd, name: &CStr) -> io::Result<CString> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: a valid NUL-terminated name; at most `buf.len()` bytes are written.
    let n = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    CString::new(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The owner and the mode of `st` on the open file or folder `fd`. Owner
/// first, then mode: a chown clears set-user-ID and set-group-ID. A refused
/// chown (not root) is no error, as before.
fn keep_owner_mode(fd: RawFd, st: &libc::stat) -> io::Result<()> {
    // SAFETY: plain calls on a descriptor we own.
    unsafe { libc::fchown(fd, st.st_uid, st.st_gid) };
    // SAFETY: as above.
    if unsafe { libc::fchmod(fd, st.st_mode & 0o7777) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Binds `src` over `target` (relative to the container root, looked up
/// inside it), creating the target (and /etc) first if the image does not
/// have it.
fn bind_file(root: BorrowedFd<'_>, src: &CStr, target: &CStr) -> io::Result<()> {
    // Only create the target when the image lacks it: opening an existing
    // lower-layer file for writing would copy it up into the writable layer,
    // three times per container, for nothing. A bind mount sits over a
    // lower-layer file just fine, and an O_PATH lookup copies nothing.
    let at = inroot::open(root, Path::new(std::ffi::OsStr::from_bytes(target.to_bytes())), Last::File)?;
    sys::mount(src, &at.proc_path(), c"none", libc::MS_BIND, None)
}

/// Where the child reports a failure before it has execed: the stderr pipe,
/// or the pty socket (the agent reads text there instead of a descriptor).
fn report_fd(io: &ChildStdio) -> RawFd {
    match io {
        ChildStdio::Tty { sock } => sock.as_raw_fd(),
        ChildStdio::Pipes { stderr, .. } => stderr.as_raw_fd(),
    }
}

/// The descriptors a container's child, right after its clone, still uses
/// of those it inherited: its standard streams (or terminal socket), which
/// are also where it reports a failure; the network namespace it joins;
/// the pipes it says "root ready" on and waits for its network on. Every
/// other descriptor it uses it opens itself, later (its root, the image's
/// tree, its terminal). Standard input, output and error (0, 1, 2) always
/// stay. Every field of `Prepared` is named, so a descriptor added to it
/// is not closed by mistake: the compiler asks about it here.
fn child_fds(p: &Prepared, io: &ChildStdio) -> Vec<RawFd> {
    let Prepared {
        argv: _,
        envp: _,
        path_dirs: _,
        hostname: _,
        domainname: _,
        net_join,
        etc_files: _,
        mounts: _,
        net_ready,
        cwd: _,
        user: _,
        net_none: _,
        tty: _,
        stdin: _,
        privileged: _,
        caps: _,
        seccomp: _,
        profile: _,
        no_new_privs: _,
        readonly_rootfs: _,
        shm_size: _,
        init: _,
        // Paths, numbers and names only.
        host: HostApplied { rlimits: _, groups: _, nodes: _ },
        root_ready,
    } = p;
    let mut fds = match io {
        ChildStdio::Tty { sock } => vec![sock.as_raw_fd()],
        ChildStdio::Pipes { stdin, stdout, stderr } => {
            let mut v = vec![stdout.as_raw_fd(), stderr.as_raw_fd()];
            v.extend(stdin.as_ref().map(AsRawFd::as_raw_fd));
            v
        }
    };
    fds.extend(net_join.as_ref().map(AsRawFd::as_raw_fd));
    fds.extend(net_ready.as_ref().map(AsRawFd::as_raw_fd));
    fds.extend(root_ready.as_ref().map(AsRawFd::as_raw_fd));
    fds
}

/// Runs in the cloned child: PID 1 of the container, in its own namespaces
/// and cgroup. Never returns: it execs or exits 127 with a message.
fn container_child(p: &Prepared, rootfs: &CStr, io: &ChildStdio) -> ! {
    // First of all: the clone gave this child a copy of every descriptor
    // the agent had open, and filling a new volume can take seconds while
    // the agent goes on. Held here, a network's DNS socket keeps its
    // address bound after the network is removed (created again, it cannot
    // bind it), and other containers' stdin and relayed connections never
    // see their end. Only what this child uses is kept.
    sys::close_now(&sys::close_ranges(&child_fds(p, io)));
    let err = report_fd(io);
    let fail = |what: &str, e: io::Error| -> ! { child_fail(err, what, e) };

    // Mount events must not leak back into the agent's namespace.
    sys::remount(c"/", libc::MS_REC | libc::MS_PRIVATE).unwrap_or_else(|e| fail("make mounts private", e));
    if let Some(ns) = &p.net_join {
        sys::setns(ns.as_raw_fd(), libc::CLONE_NEWNET).unwrap_or_else(|e| fail("join network container", e));
    }
    // SAFETY: `rootfs` is a valid NUL-terminated path.
    if unsafe { libc::chdir(rootfs.as_ptr()) } != 0 {
        fail("enter rootfs", io::Error::last_os_error());
    }
    let root = inroot::open_root(Path::new(".")).unwrap_or_else(|e| fail("enter rootfs", e));
    let root = root.as_fd();
    mount_container_filesystems(p.privileged, p.shm_size).unwrap_or_else(|e| fail("mount", e));
    super::hostopts::make_nodes(root, &p.host.nodes).unwrap_or_else(|e| fail("devices", e));
    for (src, target) in &p.etc_files {
        bind_file(root, src, target).unwrap_or_else(|e| fail(&target.to_string_lossy(), e));
    }
    // The image's tree without the mounts made on it, for filling new
    // volumes; closed (and so unmounted) once they are all in place.
    let image = if p.mounts.iter().any(|m| m.copy_up) { image_tree() } else { None };
    for m in &p.mounts {
        apply_mount(root, image.as_ref().map(|f| f.as_fd()), m).unwrap_or_else(|e| fail(&format!("mount {}", m.target.to_string_lossy()), e));
    }
    drop(image);

    // pivot_root(".", ".") stacks the old root under the new one; detaching
    // it leaves only the container's filesystem visible.
    // SAFETY: both arguments are valid NUL-terminated strings.
    if unsafe { libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) } != 0 {
        fail("pivot_root", io::Error::last_os_error());
    }
    // SAFETY: valid NUL-terminated path.
    unsafe {
        libc::umount2(c".".as_ptr(), libc::MNT_DETACH);
        libc::chdir(c"/".as_ptr());
    }
    // Docker creates a missing working directory (mkdir -p) as root, 0755,
    // before it switches user and before the root goes read-only, so both
    // `-u 1000 -w /missing` and `--read-only -w /missing` start (#370).
    make_workdir(root, &p.cwd);
    if p.readonly_rootfs {
        // Only the root itself: volumes, tmpfs and /dev stay as they are.
        sys::mount(c"none", c"/", c"none", libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY, None).unwrap_or_else(|e| fail("read-only root", e));
    }
    if let Some(h) = &p.hostname {
        sys::sethostname(h).unwrap_or_else(|e| fail("hostname", e));
    }
    if let Some(d) = &p.domainname {
        sys::setdomainname(d).unwrap_or_else(|e| fail("domainname", e));
    }
    if p.net_none {
        sys::loopback_up().unwrap_or_else(|e| fail("loopback", e));
    }
    if let Some(done) = &p.root_ready {
        // SAFETY: one byte to a pipe we own: root, mounts and namespaces
        // are in place, so an exec can join them now.
        if unsafe { libc::write(done.as_raw_fd(), b"r".as_ptr().cast(), 1) } != 1 {
            fail("ready", io::Error::last_os_error());
        }
    }
    if let Some(ready) = &p.net_ready {
        let mut b = [0u8; 1];
        // SAFETY: reading one byte into a local buffer.
        if unsafe { libc::read(ready.as_raw_fd(), b.as_mut_ptr().cast(), 1) } != 1 {
            fail("network", io::Error::other("the agent could not set up this container's network"));
        }
    }
    finish_child(p, io, true)
}

/// Runs in the cloned child of an exec: joins the container's remaining
/// namespaces (the PID namespace was set before clone), then execs.
fn exec_child(p: &Prepared, namespaces: &[(fs::File, libc::c_int)], io: &ChildStdio) -> ! {
    let err = report_fd(io);
    for (fd, kind) in namespaces {
        sys::setns(fd.as_raw_fd(), *kind).unwrap_or_else(|e| child_fail(err, "join container", e));
    }
    // SAFETY: valid NUL-terminated path.
    unsafe { libc::chdir(c"/".as_ptr()) };
    finish_child(p, io, false)
}

/// Binds the container's terminal over /dev/console; neither a privileged
/// nor an unprivileged container's /dev has one of its own.
fn bind_console(slave: &OwnedFd) {
    let Ok(pts) = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd())) else { return };
    let Ok(pts) = CString::new(pts.into_os_string().into_encoded_bytes()) else { return };
    if fs::symlink_metadata("/dev/console").is_err() {
        let _ = fs::File::create("/dev/console");
    }
    let _ = sys::mount(&pts, c"/dev/console", c"", libc::MS_BIND, None);
}

/// The part containers and execs share: standard streams, capabilities,
/// user, working directory, exec. `console`: a container's own start, whose
/// terminal also becomes its /dev/console (an exec's doesn't).
fn finish_child(p: &Prepared, io: &ChildStdio, console: bool) -> ! {
    let err = report_fd(io);
    let fail = |what: &str, e: io::Error| -> ! { child_fail(err, what, e) };

    match io {
        ChildStdio::Tty { sock } => {
            // The pty comes from the container's own /dev/pts, so `tty` and
            // job control see a terminal that exists inside the container.
            let (master, slave) = sys::open_pty().unwrap_or_else(|e| fail("terminal", e));
            sys::send_fd(sock.as_raw_fd(), master.as_raw_fd()).unwrap_or_else(|e| fail("terminal", e));
            drop(master);
            // As runc does: /dev/console is this terminal, not the VM's
            // console, so what an init such as systemd writes there reaches
            // `docker logs` (kind waits for its "Reached target" line).
            if console {
                bind_console(&slave);
            }
            // SAFETY: new session with the pty as its controlling terminal,
            // then the pty as all three standard streams.
            unsafe {
                libc::setsid();
                if libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as libc::Ioctl, 0) != 0
                    || libc::dup2(slave.as_raw_fd(), 0) < 0
                    || libc::dup2(slave.as_raw_fd(), 1) < 0
                    || libc::dup2(slave.as_raw_fd(), 2) < 0
                {
                    fail("terminal", io::Error::last_os_error());
                }
            }
        }
        ChildStdio::Pipes { stdin, stdout, stderr } => {
            // SAFETY: plain descriptor operations on descriptors we own.
            unsafe {
                // A session of its own, as runc gives every container and
                // exec. Otherwise it stays in the agent's process group, which
                // on a Linux host is the daemon's too: Apache's prefork stop
                // (killpg(getpgrp(), SIGTERM)) then reached `kiri daemon`, and
                // systemd took the whole engine down with it (#191).
                libc::setsid();
                let input = match stdin {
                    Some(fd) => fd.as_raw_fd(),
                    None => libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY),
                };
                if input < 0 || libc::dup2(input, 0) < 0 || libc::dup2(stdout.as_raw_fd(), 1) < 0 || libc::dup2(stderr.as_raw_fd(), 2) < 0 {
                    fail("stdio", io::Error::last_os_error());
                }
            }
        }
    }

    // From here on stderr is the container's own (the pipe or the terminal),
    // which is where the user will see why a start failed.
    let fail = |what: &str, e: io::Error| -> ! { child_fail(2, what, e) };

    // While still root with every capability: raising a hard limit needs
    // CAP_SYS_RESOURCE.
    super::hostopts::set_rlimits(&p.host.rlimits).unwrap_or_else(|e| fail("ulimits", e));

    for cap in 0..=LAST_CAP {
        if !p.privileged && p.caps & (1 << cap) == 0 {
            // SAFETY: PR_CAPBSET_DROP takes a capability number; EINVAL for
            // capabilities this kernel does not know is fine to ignore.
            unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) };
        }
    }

    if p.no_new_privs {
        // SAFETY: a plain prctl.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            fail("no-new-privileges", io::Error::last_os_error());
        }
    }
    // As runc: a container's own profile is installed while still root with
    // CAP_SYS_ADMIN, which installing a filter needs, unless the container has
    // no-new-privileges. Then it is installed as late as possible, right
    // before execve, after the user switch and the capability drop. The host
    // boundary always goes first: vsock is not namespaced, and privileges
    // inside the VM are no reason to reach the host.
    let late_profile = p.no_new_privs && !p.profile.is_empty();
    if p.profile.is_empty() {
        super::seccomp::install(p.seccomp.then_some(p.caps))
    } else if late_profile {
        super::seccomp::install_boundary()
    } else {
        super::seccomp::install_profile(&p.profile)
    }
    .unwrap_or_else(|e| fail("seccomp", e));
    let (uid, gid) = resolve_user(&p.user).unwrap_or_else(|e| fail("user", e));
    // The user's group, /etc/group memberships and --group-add, as Docker gives them.
    let groups = super::hostopts::supplementary_groups(&p.user, uid, gid, &p.host.groups).unwrap_or_else(|e| fail("groups", e));
    give_stdio([0, 1, 2], uid).unwrap_or_else(|e| fail("stdio", e));
    // The child holds a copy of every descriptor the agent had open, the
    // container's cgroup directory among them. Only stdio is needed from
    // here, so the rest go before the chdir: a working directory of
    // `/proc/self/fd/N` (an image's WORKDIR, `-w`) would otherwise leave
    // the process in a directory outside its root (CVE-2024-21626 in runc).
    sys::close_from(3);
    // SAFETY: plain id-setting syscalls; `groups` outlives setgroups.
    unsafe {
        if libc::setgroups(groups.len() as _, groups.as_ptr()) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
            fail("switch user", io::Error::last_os_error());
        }
        if libc::chdir(p.cwd.as_ptr()) != 0 {
            // Docker creates the whole working directory path, like mkdir -p.
            let _ = fs::create_dir_all(std::ffi::OsStr::from_encoded_bytes_unchecked(p.cwd.as_bytes()));
            if libc::chdir(p.cwd.as_ptr()) != 0 {
                fail("working directory", io::Error::last_os_error());
            }
        }
    }
    // And whatever the route, a working directory outside the container's
    // root fails the start, as runc 1.1.12 does.
    if !sys::cwd_inside_root() {
        fail("working directory", io::Error::new(io::ErrorKind::PermissionDenied, "outside the container's root"));
    }

    let exe = find_executable(p).unwrap_or_else(|| {
        fail(&p.argv[0].to_string_lossy(), io::Error::new(io::ErrorKind::NotFound, "executable file not found in $PATH"))
    });
    let mut argv: Vec<*const libc::c_char> = p.argv.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    // Docker sets HOME from the user's passwd entry when the image and the
    // run do not: /root for root, else / if the user has no entry.
    let home = (!p.envp.iter().any(|e| e.as_bytes().starts_with(b"HOME="))).then(|| {
        let dir = home_of(uid).unwrap_or_else(|| if uid == 0 { "/root".to_owned() } else { "/".to_owned() });
        CString::new(format!("HOME={dir}")).unwrap_or_default()
    });
    let mut envp: Vec<*const libc::c_char> = p.envp.iter().map(|e| e.as_ptr()).collect();
    if let Some(h) = &home {
        envp.push(h.as_ptr());
    }
    envp.push(std::ptr::null());
    let install_late = || {
        if late_profile {
            super::seccomp::install_late(&p.profile).unwrap_or_else(|e| fail("seccomp", e));
        }
    };
    if console && p.init {
        install_late();
        super::pid1::run(&exe, argv.as_ptr(), envp.as_ptr(), p.tty);
    }
    // The program starts as Docker would start it: nothing blocked and
    // SIGPIPE at its default. The agent blocks SIGCHLD for its signalfd and
    // Rust ignores SIGPIPE, and both survive exec: with SIGCHLD blocked, a
    // shell's `cmd & wait` never wakes once cmd has already exited, and
    // with SIGPIPE ignored, `yes | head -1` never ends.
    // SAFETY: signal calls on a local set in the single-threaded child.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
    install_late();
    // SAFETY: NULL-terminated arrays of valid C strings that outlive the call.
    unsafe { libc::execve(exe.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    sys::exec_fail(2, &exe, io::Error::last_os_error(), false)
}

/// The container's /proc, /sys, /dev (with the standard device nodes),
/// /dev/pts, /dev/shm, /dev/mqueue and a read-only cgroup view. Relative
/// paths: the current directory is the container root. A privileged
/// container gets the VM's devices and writable /sys and cgroups instead.
fn mount_container_filesystems(privileged: bool, shm_size: u64) -> io::Result<()> {
    let nosuid_nodev_noexec = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    let ro = if privileged { 0 } else { libc::MS_RDONLY };
    for d in [c"proc", c"sys", c"dev"] {
        sys::mkdir(d, 0o755);
        // Everything below is mounted by relative path, so these must be
        // the image's own directories, not symlinks out of it (runc refuses
        // the same).
        let path = Path::new(std::ffi::OsStr::from_bytes(d.to_bytes()));
        if !fs::symlink_metadata(path)?.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("/{} in the image must be a directory", path.display())));
        }
    }
    sys::mount(c"proc", c"proc", c"proc", nosuid_nodev_noexec, None)?;
    sys::mount(c"sysfs", c"sys", c"sysfs", nosuid_nodev_noexec | ro, None)?;
    // Never devtmpfs, even for a privileged container: the kernel has one
    // devtmpfs, so mounting it again hands the container the host's own
    // /dev, and the links made below (dev/ptmx -> pts/ptmx) would replace
    // the host's device nodes. On a Linux host that broke every terminal on
    // the machine. A privileged container gets copies of the host's devices
    // on its own tmpfs instead, as runc does.
    sys::mount(c"tmpfs", c"dev", c"tmpfs", libc::MS_NOSUID, Some(c"mode=755,size=65536k"))?;
    if privileged {
        copy_host_devices(Path::new("/dev"), Path::new("dev"))?;
    }
    for (name, major, minor) in [
        (c"dev/null", 1, 3),
        (c"dev/zero", 1, 5),
        (c"dev/full", 1, 7),
        (c"dev/random", 1, 8),
        (c"dev/urandom", 1, 9),
        (c"dev/tty", 5, 0),
    ] {
        if privileged {
            // Copied from the host already.
            continue;
        }
        sys::mknod(name, libc::S_IFCHR | 0o666, major, minor)?;
        // mknod applies the umask; every user in the container needs these.
        // SAFETY: valid NUL-terminated path.
        unsafe { libc::chmod(name.as_ptr(), 0o666) };
    }
    for (target, link) in [
        (c"/proc/self/fd", c"dev/fd"),
        (c"/proc/self/fd/0", c"dev/stdin"),
        (c"/proc/self/fd/1", c"dev/stdout"),
        (c"/proc/self/fd/2", c"dev/stderr"),
        (c"pts/ptmx", c"dev/ptmx"),
    ] {
        // SAFETY: valid NUL-terminated strings; the host copy may have some.
        unsafe {
            libc::unlink(link.as_ptr());
            libc::symlink(target.as_ptr(), link.as_ptr())
        };
    }
    for d in [c"dev/pts", c"dev/shm", c"dev/mqueue"] {
        sys::mkdir(d, 0o755);
    }
    sys::mount(c"devpts", c"dev/pts", c"devpts", libc::MS_NOSUID | libc::MS_NOEXEC, Some(c"newinstance,ptmxmode=0666,mode=0620,gid=5"))?;
    let shm = CString::new(format!("mode=1777,size={}k", shm_size.div_ceil(1024))).expect("no NUL");
    sys::mount(c"shm", c"dev/shm", c"tmpfs", nosuid_nodev_noexec, Some(&shm))?;
    sys::mount(c"mqueue", c"dev/mqueue", c"mqueue", nosuid_nodev_noexec, None)?;
    sys::mkdir(c"sys/fs/cgroup", 0o755);
    sys::mount(c"cgroup2", c"sys/fs/cgroup", c"cgroup2", nosuid_nodev_noexec | ro, None)?;
    if !privileged {
        harden_proc()?;
    }
    Ok(())
}

/// Recreates the host's character and block devices under `to` (the
/// container's fresh /dev tmpfs), keeping their mode and owner, for a
/// privileged container. Skips what the container gets its own of (pts,
/// shm, mqueue, console, the fd links) and symlinks, as runc does. Devices
/// that appear on the host later don't show up, as on Docker.
fn copy_host_devices(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if from == Path::new("/dev")
            && matches!(name.as_bytes(), b"pts" | b"shm" | b"mqueue" | b"fd" | b"console" | b".lxc" | b".lxd-mounts" | b".udev")
        {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
        let ft = meta.file_type();
        let dest = to.join(&name);
        if ft.is_dir() {
            if fs::create_dir(&dest).is_ok() {
                let _ = copy_host_devices(&entry.path(), &dest);
            }
            continue;
        }
        if !(ft.is_char_device() || ft.is_block_device()) {
            continue;
        }
        let Ok(c) = CString::new(dest.as_os_str().as_bytes()) else { continue };
        // SAFETY: valid NUL-terminated path.
        if unsafe { libc::mknod(c.as_ptr(), meta.mode() as libc::mode_t, meta.rdev() as libc::dev_t) } != 0 {
            continue;
        }
        // mknod applies the umask; keep the host's permissions and owner.
        // SAFETY: valid NUL-terminated path.
        unsafe {
            libc::chmod(c.as_ptr(), (meta.mode() & 0o7777) as libc::mode_t);
            libc::chown(c.as_ptr(), meta.uid(), meta.gid());
        }
    }
    Ok(())
}

/// Makes the dangerous parts of /proc read-only or hides them, as Docker's
/// default `readonlyPaths` and `maskedPaths` do. The container is real root
/// (no user namespace), and these sysctls and files are not namespaced, so
/// without this a container could set `kernel.core_pattern` or
/// `kernel.modprobe` and run code on the host. Paths are relative to the
/// container root (the current directory before pivot_root).
fn harden_proc() -> io::Result<()> {
    let ro = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    // /proc/sys holds writable sysctls (core_pattern, modprobe, …): a path
    // to host code. This one must succeed.
    sys::mount(c"proc/sys", c"proc/sys", c"none", libc::MS_BIND, None)?;
    sys::remount(c"proc/sys", ro as libc::c_ulong)?;
    // The rest are best-effort: a path the guest kernel lacks is already
    // absent, so failing to cover it is not a hole.
    for p in [c"proc/sysrq-trigger", c"proc/irq", c"proc/bus", c"proc/fs"] {
        if sys::mount(p, p, c"none", libc::MS_BIND, None).is_ok() {
            let _ = sys::remount(p, ro as libc::c_ulong);
        }
    }
    // Hide files that leak host memory or kernel addresses, by covering them
    // with /dev/null (already created above).
    for p in [c"proc/kcore", c"proc/keys", c"proc/latency_stats", c"proc/timer_list", c"proc/timer_stats", c"proc/sched_debug", c"proc/interrupts"] {
        let _ = sys::mount(c"dev/null", p, c"none", libc::MS_BIND, None);
    }
    // Directories to hide, covered with an empty read-only tmpfs.
    for p in [c"proc/scsi", c"proc/acpi", c"proc/asound", c"sys/firmware", c"sys/devices/virtual/powercap"] {
        let _ = sys::mount(c"tmpfs", p, c"tmpfs", (libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) as libc::c_ulong, Some(c"size=0k"));
    }
    Ok(())
}

/// `""`, `root`, `1000`, `1000:1000`, `redis`, `redis:redis`: looked up in
/// the container's /etc/passwd and /etc/group when not numeric.
/// The home directory /etc/passwd gives `uid`.
fn home_of(uid: u32) -> Option<String> {
    let text = fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 6 && f[2].parse() == Ok(uid)).then(|| f[5].to_owned()).filter(|h| !h.is_empty())
    })
}

/// As runc's `fixStdioPermissions`: `uid` owns its standard streams `fds`, so it
/// can reopen them by path (`/proc/self/fd/2`, `/dev/stderr`), as php-fpm
/// and nginx open their logs. The agent made them as root, and Linux checks
/// a pipe's or terminal's owner on such an open.
///
/// Only this process's own streams can change hands: `stdio` makes new
/// pipes for every start and every exec, a TTY is a pty the child opens in
/// the container's own devpts instance, and stdin without `-i` is /dev/null.
/// Even so, only a pipe or a terminal is given away, never /dev/null or
/// anything else. The owner changes and the group stays, as in runc. This
/// runs as root, before the user switch: only root may give a file away.
fn give_stdio(fds: [RawFd; 3], uid: libc::uid_t) -> io::Result<()> {
    for fd in fds {
        if !needs_owner(fd, uid)? {
            continue;
        }
        // SAFETY: fchown on one of the process's own descriptors; -1 keeps the group.
        if unsafe { libc::fchown(fd, uid, libc::gid_t::MAX) } != 0 {
            let e = io::Error::last_os_error();
            // As runc, the start goes on without it only for the errors a
            // mount or user namespace causes; anything else fails the start.
            if !chown_refused(&e) {
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Whether `fd` is a pipe or a terminal that `uid` doesn't own.
fn needs_owner(fd: RawFd, uid: libc::uid_t) -> io::Result<bool> {
    // SAFETY: fstat into a zeroed struct, and isatty, on a descriptor number.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        let pipe = st.st_mode & libc::S_IFMT == libc::S_IFIFO;
        Ok(st.st_uid != uid && (pipe || libc::isatty(fd) == 1))
    }
}

/// The `fchown` errors runc lets a start through with: EINVAL (the group
/// isn't mapped), EPERM (the owner isn't mapped), EROFS (read-only).
fn chown_refused(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EINVAL | libc::EPERM | libc::EROFS))
}

fn resolve_user(user: &str) -> io::Result<(libc::uid_t, libc::gid_t)> {
    if user.is_empty() {
        return Ok((0, 0));
    }
    let (u, g) = user.split_once(':').map_or((user, None), |(u, g)| (u, Some(g)));
    let lookup = |file: &str, name: &str| -> Option<(u32, u32)> {
        let text = fs::read_to_string(file).ok()?;
        text.lines().find_map(|l| {
            let mut f = l.split(':');
            if f.next()? != name {
                return None;
            }
            let id = f.nth(1)?.parse().ok()?;
            let gid = f.next().and_then(|g| g.parse().ok()).unwrap_or(id);
            Some((id, gid))
        })
    };
    let not_found = |what: &str| io::Error::new(io::ErrorKind::NotFound, format!("unknown {what} {user}"));
    let (uid, primary_gid) = match u.parse::<u32>() {
        Ok(id) => (id, lookup_gid_for_uid(id).unwrap_or(id)),
        Err(_) => lookup("/etc/passwd", u).ok_or_else(|| not_found("user"))?,
    };
    let gid = match g {
        None => primary_gid,
        Some(g) => match g.parse::<u32>() {
            Ok(id) => id,
            Err(_) => lookup("/etc/group", g).map(|(id, _)| id).ok_or_else(|| not_found("group"))?,
        },
    };
    Ok((uid, gid))
}

fn lookup_gid_for_uid(uid: u32) -> Option<u32> {
    let text = fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.get(2)?.parse::<u32>().ok()? == uid).then(|| f.get(3)?.parse().ok()).flatten()
    })
}

fn find_executable(p: &Prepared) -> Option<CString> {
    let name = p.argv[0].as_bytes();
    // An empty name is no program: joined to a PATH directory it would name
    // the directory itself and fail with a misleading "is a directory".
    if name.is_empty() {
        return None;
    }
    if name.contains(&b'/') {
        return Some(p.argv[0].clone());
    }
    p.path_dirs.iter().find_map(|dir| {
        let mut full = Vec::with_capacity(dir.len() + 1 + name.len());
        full.extend_from_slice(dir);
        full.push(b'/');
        full.extend_from_slice(name);
        let c = CString::new(full).ok()?;
        // SAFETY: valid NUL-terminated path.
        (unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0).then_some(c)
    })
}

#[cfg(test)]
mod tests {
    /// Waits, for at most 5 seconds, until no writer of the pipe is left:
    /// `poll` reports POLLHUP on the read end then, whatever the events
    /// asked for. Tests run as threads of one process, and a child another
    /// test forks holds a copy of every open descriptor until it execs or
    /// exits, so a write end dropped here is not yet closed for the reader.
    /// The real agent is single-threaded, so this cannot happen there.
    fn wait_all_writers_gone(read_end: std::os::fd::RawFd) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let mut pfd = libc::pollfd { fd: read_end, events: 0, revents: 0 };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
            if n > 0 && pfd.revents & libc::POLLHUP != 0 {
                return;
            }
            if n < 0 {
                assert_eq!(std::io::Error::last_os_error().kind(), std::io::ErrorKind::Interrupted, "poll failed");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the write end is still held somewhere 5 s after it was dropped"
            );
        }
    }

    /// A container's child closes every inherited descriptor but these
    /// (see `child_fds`): each one it uses must be among them, for a plain
    /// start with stdin, one with a terminal, one that joins another
    /// container's network and one whose network and sysctls it waits for.
    #[test]
    fn a_container_child_keeps_every_descriptor_it_uses() {
        use super::{ChildStdio, child_fds, sys};
        use std::os::fd::{AsRawFd, RawFd};
        let mut buf = Vec::new();
        let net = Net { bridge: "kiri1", ip: [10, 5, 0, 2], prefix: 24, gateway: [10, 5, 0, 1], extra_hosts: &[], dns_names: &[], opts: &[] };
        encode_run(&mut buf, "c1", &[], &["sh"], &[], "/", "", "c1", run_flags::INIT, net, &[], &Limits::default(), 0, &[] as &[&str], &[]);
        let Ok(Msg::Run(spec)) = decode(&buf) else { panic!() };
        let mut p = super::prepare(spec.argv, spec.env, spec.cwd, spec.user, Some(spec.hostname), spec.flags, spec.caps).unwrap();
        let sorted = |mut v: Vec<RawFd>| {
            v.sort_unstable();
            v
        };
        // Nothing of its own yet: only its streams.
        let (r0, w0) = sys::pipe().unwrap();
        let (r1, w1) = sys::pipe().unwrap();
        let (r2, w2) = sys::pipe().unwrap();
        let pipes = ChildStdio::Pipes { stdin: Some(r0), stdout: w1, stderr: w2 };
        let streams = |io: &ChildStdio| match io {
            ChildStdio::Pipes { stdin, stdout, stderr } => {
                let mut v = vec![stdout.as_raw_fd(), stderr.as_raw_fd()];
                v.extend(stdin.as_ref().map(|f| f.as_raw_fd()));
                v
            }
            ChildStdio::Tty { sock } => vec![sock.as_raw_fd()],
        };
        assert_eq!(sorted(child_fds(&p, &pipes)), sorted(streams(&pipes)));
        // Every descriptor a start can hand it.
        let joined = std::fs::File::open("/dev/null").unwrap();
        let (net_ready, go) = sys::pipe().unwrap();
        let (root_r, root_ready) = sys::pipe().unwrap();
        let theirs = [joined.as_raw_fd(), net_ready.as_raw_fd(), root_ready.as_raw_fd()];
        p.net_join = Some(joined);
        p.net_ready = Some(net_ready);
        p.root_ready = Some(root_ready);
        assert_eq!(sorted(child_fds(&p, &pipes)), sorted([streams(&pipes), theirs.to_vec()].concat()));
        // A terminal: the socket its pty master goes back on, nothing else.
        p.tty = true;
        let (_agent, sock) = sys::socketpair().unwrap();
        let tty = ChildStdio::Tty { sock };
        assert_eq!(sorted(child_fds(&p, &tty)), sorted([streams(&tty), theirs.to_vec()].concat()));
        drop((w0, r1, r2, go, root_r));
    }

    #[test]
    fn a_starting_root_settles_without_waiting() {
        use super::{Process, Root};
        use std::os::fd::AsRawFd;
        // Nothing written yet: still starting, and asking does not wait.
        let (r, w) = super::sys::pipe().unwrap();
        let mut p = Process::starting_for_test("c1", 100, r, None);
        let t = std::time::Instant::now();
        assert_eq!(p.root(), Root::Starting);
        assert!(t.elapsed() < std::time::Duration::from_millis(50), "settling must not wait");
        assert!(!p.entered());
        // The child's byte: ready, and stays so.
        // SAFETY: one byte to a pipe we own.
        assert_eq!(unsafe { libc::write(w.as_raw_fd(), b"r".as_ptr().cast(), 1) }, 1);
        assert_eq!(p.root(), Root::Ready);
        assert!(p.entered());
        drop(w);
        assert_eq!(p.root(), Root::Ready, "the pipe is let go once ready");
        // Closed without the byte: the child ended first, and stays so.
        let (r, w) = super::sys::pipe().unwrap();
        let r_fd = r.as_raw_fd();
        let mut p = Process::starting_for_test("c2", 101, r, None);
        drop(w);
        wait_all_writers_gone(r_fd);
        assert_eq!(p.root(), Root::Ended);
        assert_eq!(p.root(), Root::Ended);
        assert!(!p.entered());
        assert_eq!(p.start_failure(), Some(super::NOT_READY), "it said nothing");
        // A process that never waited is ready.
        assert_eq!(Process::for_test("c3", 102).root(), Root::Ready);
        assert_eq!(Process::for_test("c3", 102).start_failure(), None);
    }

    #[test]
    fn a_child_that_fails_while_starting_gives_its_own_reason() {
        use super::{Process, Root};
        use std::os::fd::AsRawFd;
        let (r, w) = super::sys::pipe().unwrap();
        let (err_r, err_w) = super::sys::pipe().unwrap();
        let r_fd = r.as_raw_fd();
        let mut p = Process::starting_for_test("c1", 100, r, Some(err_r));
        let said = b"kiri: cannot start container: mount /payload: No space left on device (os error 28)\n";
        // SAFETY: writing a local buffer to a pipe we own.
        assert_eq!(unsafe { libc::write(err_w.as_raw_fd(), said.as_ptr().cast(), said.len()) }, said.len() as isize);
        assert_eq!(p.root(), Root::Starting, "its message alone settles nothing");
        drop(w);
        wait_all_writers_gone(r_fd);
        assert_eq!(p.root(), Root::Ended);
        assert_eq!(p.start_failure(), Some("mount /payload: No space left on device (os error 28)"));
    }

    #[test]
    fn waiting_for_a_root_lasts_until_the_child_says() {
        use super::{Process, Root};
        use std::os::fd::AsRawFd;
        let (r, w) = super::sys::pipe().unwrap();
        let mut p = Process::starting_for_test("c1", 100, r, None);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            // SAFETY: one byte to a pipe we own.
            unsafe { libc::write(w.as_raw_fd(), b"r".as_ptr().cast(), 1) };
            w
        });
        let t = std::time::Instant::now();
        assert_eq!(p.wait_root(), Root::Ready);
        assert!(t.elapsed() >= std::time::Duration::from_millis(90), "waited {:?}", t.elapsed());
        drop(writer.join().unwrap());
        // Ended while waited for.
        let (r, w) = super::sys::pipe().unwrap();
        let mut p = Process::starting_for_test("c2", 101, r, None);
        let closer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(w);
        });
        assert_eq!(p.wait_root(), Root::Ended);
        closer.join().unwrap();
    }

    #[test]
    fn a_start_waits_for_the_root_only_briefly() {
        use std::os::fd::AsRawFd;
        let (r, w) = super::sys::pipe().unwrap();
        let t = std::time::Instant::now();
        assert_eq!(super::root_word(r.as_raw_fd(), Some(super::ROOT_READY_WAIT)).unwrap(), super::RootWord::NotYet);
        let waited = t.elapsed();
        assert!(waited >= super::ROOT_READY_WAIT && waited < super::ROOT_READY_WAIT * 10, "waited {waited:?}");
        // SAFETY: one byte to a pipe we own.
        assert_eq!(unsafe { libc::write(w.as_raw_fd(), b"r".as_ptr().cast(), 1) }, 1);
        assert_eq!(super::root_word(r.as_raw_fd(), Some(super::ROOT_READY_WAIT)).unwrap(), super::RootWord::Ready);
        drop(w);
        wait_all_writers_gone(r.as_raw_fd());
        assert_eq!(super::root_word(r.as_raw_fd(), Some(super::ROOT_READY_WAIT)).unwrap(), super::RootWord::Ended);
    }

    use kiri_proto::{Limits, Msg, Net, decode, encode_run, run_flags};

    // #227: a new container's writable layer is marked plain before it is
    // first mounted, and keeps that; one written before (no mark, not
    // empty) keeps the kernel's own overlay features, so an update never
    // makes its redirected or metadata-only files unreadable.
    #[test]
    fn only_a_fresh_writable_layer_gets_plain_overlay_features() {
        let base = std::env::temp_dir().join(format!("kiri-plain-upper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("upper")).unwrap();
        assert!(super::plain_upper(&base).unwrap(), "fresh");
        assert!(base.join(super::PLAIN_UPPER).exists());
        std::fs::write(base.join("upper/written"), b"x").unwrap();
        assert!(super::plain_upper(&base).unwrap(), "marked, then written to");
        std::fs::remove_file(base.join(super::PLAIN_UPPER)).unwrap();
        assert!(!super::plain_upper(&base).unwrap(), "written before the mark existed");
        assert!(!base.join(super::PLAIN_UPPER).exists(), "an old layer is never marked");
        assert_eq!(super::overlay_features(true), "index=off,redirect_dir=nofollow,metacopy=off");
        assert_eq!(super::overlay_features(false), "index=off");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Only a pipe or a terminal the user doesn't own yet changes hands:
    /// never /dev/null, a file or a socket, which a stream could otherwise be.
    #[test]
    fn only_pipes_and_terminals_are_given_to_the_user() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let me = std::fs::metadata("/proc/self").unwrap().uid();
        let other = me.wrapping_add(1);
        let (r, w) = super::sys::pipe().unwrap();
        assert!(super::needs_owner(w.as_raw_fd(), other).unwrap(), "a pipe");
        assert!(super::needs_owner(r.as_raw_fd(), other).unwrap(), "its read end");
        assert!(!super::needs_owner(w.as_raw_fd(), me).unwrap(), "a pipe the user owns already");
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(!super::needs_owner(null.as_raw_fd(), other).unwrap(), "/dev/null");
        let file = std::fs::File::open("/proc/self/status").unwrap();
        assert!(!super::needs_owner(file.as_raw_fd(), other).unwrap(), "a file");
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!super::needs_owner(a.as_raw_fd(), other).unwrap(), "a socket");
        if let Ok(ptmx) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/ptmx") {
            assert!(super::needs_owner(ptmx.as_raw_fd(), other).unwrap(), "a terminal");
        }
        assert!(super::needs_owner(9999, other).is_err(), "a closed descriptor");
    }

    /// `give_stdio` through real `fchown` calls: run as root, a pipe changes
    /// hands; run as anyone else, `fchown` gets EPERM and the start goes on,
    /// as in runc. /dev/null never changes, and a descriptor it can't
    /// inspect fails the start.
    #[test]
    fn give_stdio_gives_pipes_and_fails_only_on_unexpected_errors() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let owner = |fd: i32| std::fs::metadata(format!("/proc/self/fd/{fd}")).unwrap().uid();
        // SAFETY: a plain getter.
        let me = unsafe { libc::geteuid() };
        let other = me.wrapping_add(1);
        let (r, w) = super::sys::pipe().unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        let null_owner = owner(null.as_raw_fd());
        super::give_stdio([r.as_raw_fd(), w.as_raw_fd(), null.as_raw_fd()], other).unwrap();
        let pipe_owner = if me == 0 { other } else { me };
        assert_eq!((owner(r.as_raw_fd()), owner(w.as_raw_fd()), owner(null.as_raw_fd())), (pipe_owner, pipe_owner, null_owner));
        assert!(super::give_stdio([9999, r.as_raw_fd(), w.as_raw_fd()], other).is_err(), "a closed descriptor");
    }

    #[test]
    fn only_namespace_and_mount_chown_errors_let_a_start_through() {
        for (errno, refused) in [(libc::EPERM, true), (libc::EINVAL, true), (libc::EROFS, true), (libc::EBADF, false), (libc::EIO, false)] {
            assert_eq!(super::chown_refused(&std::io::Error::from_raw_os_error(errno)), refused, "errno {errno}");
        }
    }

    /// A volume is filled from the image's directory as `apply_mount` reaches
    /// it: through `/proc/self/fd/N` of an O_PATH descriptor. The volume
    /// gets the directory's mode (not the link's 000) and its files.
    #[test]
    fn copy_tree_through_a_proc_fd_keeps_the_directory_mode() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let base = std::env::temp_dir().join(format!("kiri-copy-tree-{}", std::process::id()));
        let (img, vol) = (base.join("img"), base.join("vol"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(img.join("sub")).unwrap();
        std::fs::create_dir_all(&vol).unwrap();
        std::fs::write(img.join("conf"), b"hello").unwrap();
        std::fs::set_permissions(img.join("conf"), std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::set_permissions(img.join("sub"), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink("conf", img.join("link")).unwrap();
        std::fs::set_permissions(&img, std::fs::Permissions::from_mode(0o751)).unwrap();

        let fd = super::inroot::open_root(&img).unwrap();
        let through = std::path::PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        super::copy_tree(&through, &vol).unwrap();

        let mode = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().mode() & 0o7777;
        assert_eq!(mode(&vol), 0o751, "the volume takes the image directory's mode");
        assert_eq!(std::fs::metadata(&vol).unwrap().uid(), std::fs::metadata(&img).unwrap().uid());
        assert_eq!(std::fs::read(vol.join("conf")).unwrap(), b"hello");
        assert_eq!(mode(&vol.join("conf")), 0o640);
        assert_eq!(mode(&vol.join("sub")), 0o700);
        assert_eq!(std::fs::read_link(vol.join("link")).unwrap(), std::path::Path::new("conf"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A fresh scratch folder holding `img` (the image's folder), `vol` (the
    /// new volume) and `out` (somewhere a container's link could point).
    fn fill_dirs(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("kiri-fill-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in ["img", "vol", "out"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        (base.clone(), base.join("img"), base.join("vol"), base.join("out"))
    }

    fn set_mode(p: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode_of(p: &std::path::Path) -> u32 {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(p).unwrap().mode() & 0o7777
    }

    fn names(p: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(p).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    /// Another container already has the volume and put a link where the
    /// image has a folder: the fill must not write through it.
    #[test]
    fn fill_leaves_a_link_named_like_a_folder_alone() {
        let (base, img, vol, out) = fill_dirs("dirlink");
        std::fs::create_dir_all(img.join("data/deep")).unwrap();
        std::fs::write(img.join("data/x"), b"x").unwrap();
        std::fs::write(img.join("data/deep/y"), b"y").unwrap();
        set_mode(&img.join("data"), 0o700);
        std::fs::write(out.join("keep"), b"keep").unwrap();
        set_mode(&out, 0o755);
        std::os::unix::fs::symlink(&out, vol.join("data")).unwrap();

        super::copy_tree(&img, &vol).unwrap();

        assert_eq!(names(&out), ["keep"], "nothing was made behind the link");
        assert_eq!(mode_of(&out), 0o755, "the folder behind the link was not re-moded");
        assert_eq!(std::fs::read_link(vol.join("data")).unwrap(), out, "the link is still a link");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The same for a link named like a file, to a file and to nothing.
    #[test]
    fn fill_leaves_a_link_named_like_a_file_alone() {
        let (base, img, vol, out) = fill_dirs("filelink");
        std::fs::write(img.join("conf"), b"from the image").unwrap();
        std::fs::write(img.join("other"), b"from the image").unwrap();
        std::fs::write(out.join("target"), b"outside").unwrap();
        set_mode(&out.join("target"), 0o600);
        std::os::unix::fs::symlink(out.join("target"), vol.join("conf")).unwrap();
        std::os::unix::fs::symlink(out.join("not-there"), vol.join("other")).unwrap();

        super::copy_tree(&img, &vol).unwrap();

        assert_eq!(std::fs::read(out.join("target")).unwrap(), b"outside");
        assert_eq!(mode_of(&out.join("target")), 0o600);
        assert!(!out.join("not-there").exists(), "a dangling link is not created through either");
        assert_eq!(names(&out), ["target"]);
        assert_eq!(std::fs::read_link(vol.join("conf")).unwrap(), out.join("target"));
        assert_eq!(std::fs::read_link(vol.join("other")).unwrap(), out.join("not-there"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A link to an outside file whose name is a file in the image with
    /// another mode: the outside file's mode stays.
    #[test]
    fn fill_does_not_re_mode_what_a_link_points_at() {
        let (base, img, vol, out) = fill_dirs("modelink");
        std::fs::write(img.join("secret"), b"").unwrap();
        set_mode(&img.join("secret"), 0o777);
        std::fs::write(out.join("target"), b"").unwrap();
        set_mode(&out.join("target"), 0o600);
        std::os::unix::fs::symlink(out.join("target"), vol.join("secret")).unwrap();

        super::copy_tree(&img, &vol).unwrap();

        assert_eq!(mode_of(&out.join("target")), 0o600);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A real folder already there is filled; a real file keeps its contents
    /// and mode; a link where the image has a link is left too.
    #[test]
    fn fill_enters_a_real_folder_and_keeps_a_real_file() {
        let (base, img, vol, _out) = fill_dirs("existing");
        std::fs::create_dir_all(img.join("d")).unwrap();
        std::fs::write(img.join("d/new"), b"new").unwrap();
        std::fs::write(img.join("d/old"), b"image").unwrap();
        std::fs::write(img.join("f"), b"image").unwrap();
        set_mode(&img.join("f"), 0o755);
        std::os::unix::fs::symlink("image-target", img.join("l")).unwrap();
        std::fs::create_dir_all(vol.join("d")).unwrap();
        set_mode(&vol.join("d"), 0o711);
        std::fs::write(vol.join("d/old"), b"volume").unwrap();
        std::fs::write(vol.join("f"), b"volume").unwrap();
        set_mode(&vol.join("f"), 0o640);
        std::os::unix::fs::symlink("volume-target", vol.join("l")).unwrap();

        super::copy_tree(&img, &vol).unwrap();

        assert_eq!(std::fs::read(vol.join("d/new")).unwrap(), b"new", "the folder was entered and filled");
        assert_eq!(std::fs::read(vol.join("d/old")).unwrap(), b"volume");
        assert_eq!(mode_of(&vol.join("d")), 0o711, "a folder that was there keeps its mode");
        assert_eq!(std::fs::read(vol.join("f")).unwrap(), b"volume");
        assert_eq!(mode_of(&vol.join("f")), 0o640);
        assert_eq!(std::fs::read_link(vol.join("l")).unwrap(), std::path::Path::new("volume-target"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A folder in the image where the volume has a file is left, and nothing
    /// is made below it.
    #[test]
    fn fill_leaves_a_file_named_like_a_folder_alone() {
        let (base, img, vol, _out) = fill_dirs("filefolder");
        std::fs::create_dir_all(img.join("d")).unwrap();
        std::fs::write(img.join("d/x"), b"x").unwrap();
        std::fs::write(vol.join("d"), b"volume").unwrap();

        super::copy_tree(&img, &vol).unwrap();

        assert_eq!(std::fs::read(vol.join("d")).unwrap(), b"volume");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The plain case: everything is copied with its mode, links as links,
    /// fifos skipped, an empty folder kept.
    #[test]
    fn fill_copies_a_plain_tree() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let (base, img, vol, _out) = fill_dirs("plain");
        std::fs::create_dir_all(img.join("a/b/c")).unwrap();
        std::fs::create_dir_all(img.join("empty")).unwrap();
        std::fs::create_dir_all(img.join("tmp")).unwrap();
        std::fs::write(img.join("a/b/file"), b"hello").unwrap();
        std::fs::write(img.join("a/suid"), b"#!").unwrap();
        std::fs::write(img.join("a/sgid"), b"#!").unwrap();
        let big = vec![7u8; 3 << 20];
        std::fs::write(img.join("big"), &big).unwrap();
        std::os::unix::fs::symlink("b/file", img.join("a/link")).unwrap();
        std::os::unix::fs::symlink("/nowhere/at/all", img.join("dangling")).unwrap();
        let fifo = super::sys::cstr(img.join("fifo")).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        set_mode(&img.join("a/suid"), 0o4755);
        set_mode(&img.join("a/sgid"), 0o2755);
        set_mode(&img.join("tmp"), 0o1777);
        set_mode(&img.join("a/b/c"), 0o750);
        set_mode(&img.join("empty"), 0o500);
        set_mode(&img, 0o2751);

        super::copy_tree(&img, &vol).unwrap();

        assert_eq!(std::fs::read(vol.join("a/b/file")).unwrap(), b"hello");
        assert_eq!(std::fs::read(vol.join("big")).unwrap(), big);
        assert_eq!(mode_of(&vol.join("a/suid")), 0o4755);
        assert_eq!(mode_of(&vol.join("a/sgid")), 0o2755);
        assert_eq!(mode_of(&vol.join("tmp")), 0o1777);
        assert_eq!(mode_of(&vol.join("a/b/c")), 0o750);
        assert_eq!(mode_of(&vol.join("empty")), 0o500);
        assert!(names(&vol.join("empty")).is_empty());
        assert_eq!(mode_of(&vol), 0o2751, "the volume's own folder gets the image folder's mode");
        assert_eq!(std::fs::metadata(&vol).unwrap().uid(), std::fs::metadata(&img).unwrap().uid());
        assert_eq!(std::fs::read_link(vol.join("a/link")).unwrap(), std::path::Path::new("b/file"));
        assert_eq!(std::fs::read_link(vol.join("dangling")).unwrap(), std::path::Path::new("/nowhere/at/all"));
        assert!(!std::fs::symlink_metadata(vol.join("a/link")).unwrap().file_type().is_dir());
        assert!(!vol.join("fifo").exists() && std::fs::symlink_metadata(img.join("fifo")).unwrap().file_type().is_fifo());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// An empty image folder gives an empty volume with the folder's mode.
    #[test]
    fn fill_from_an_empty_folder_keeps_its_mode() {
        let (base, img, vol, _out) = fill_dirs("emptysrc");
        set_mode(&img, 0o710);
        super::copy_tree(&img, &vol).unwrap();
        assert!(names(&vol).is_empty());
        assert_eq!(mode_of(&vol), 0o710);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Runs the fill on a thread and fails if it has not returned in 5
    /// seconds (a fill that waits on a fifo must not hang the test binary).
    fn fill_bounded(img: &std::path::Path, vol: &std::path::Path) -> std::io::Result<()> {
        let (img, vol) = (img.to_path_buf(), vol.to_path_buf());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(super::copy_tree(&img, &vol));
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the fill did not return within 5 seconds")
    }

    fn mkfifo(p: &std::path::Path) {
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    fn is_fifo(p: &std::path::Path) -> bool {
        use std::os::unix::fs::FileTypeExt;
        std::fs::symlink_metadata(p).unwrap().file_type().is_fifo()
    }

    /// A fifo already in the volume where the image has a file: the fill
    /// neither waits on it nor replaces it. (On main's code the open of the
    /// fifo by path waits for ever: this fails there.)
    #[test]
    fn fill_leaves_a_fifo_named_like_a_file_alone() {
        let (base, img, vol, _out) = fill_dirs("fifofile");
        std::fs::write(img.join("pipe"), b"from the image").unwrap();
        std::fs::write(img.join("plain"), b"plain").unwrap();
        mkfifo(&vol.join("pipe"));

        fill_bounded(&img, &vol).unwrap();

        assert!(is_fifo(&vol.join("pipe")), "the fifo is still a fifo");
        assert_eq!(std::fs::read(vol.join("plain")).unwrap(), b"plain");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A fifo in the volume where the image has a folder: nothing is made
    /// below it, and the fill does not wait. (On main's code the fill chmods
    /// it and the copy of the children fails on it, so the fill returns an
    /// error there: this fails.)
    #[test]
    fn fill_leaves_a_fifo_named_like_a_folder_alone() {
        let (base, img, vol, _out) = fill_dirs("fifodir");
        std::fs::create_dir_all(img.join("data")).unwrap();
        std::fs::write(img.join("data/x"), b"x").unwrap();
        mkfifo(&vol.join("data"));

        fill_bounded(&img, &vol).unwrap();

        assert!(is_fifo(&vol.join("data")), "the fifo is still a fifo");
        assert_eq!(names(&vol), ["data"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A file in the volume that is a hard link to a file outside it, named
    /// like a file in the image: the outside file is not written or re-moded.
    /// (On main's code the file is opened for writing and truncated, and
    /// given the image's mode: this fails.)
    #[test]
    fn fill_leaves_a_hard_link_named_like_a_file_alone() {
        let (base, img, vol, out) = fill_dirs("hardlink");
        std::fs::write(img.join("conf"), b"from the image").unwrap();
        set_mode(&img.join("conf"), 0o644);
        std::fs::write(out.join("target"), b"outside, not to be touched").unwrap();
        set_mode(&out.join("target"), 0o600);
        std::fs::hard_link(out.join("target"), vol.join("conf")).unwrap();

        fill_bounded(&img, &vol).unwrap();

        assert_eq!(std::fs::read(out.join("target")).unwrap(), b"outside, not to be touched");
        assert_eq!(mode_of(&out.join("target")), 0o600);
        assert_eq!(std::fs::read(vol.join("conf")).unwrap(), b"outside, not to be touched");
        assert_eq!(mode_of(&vol.join("conf")), 0o600);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A link to nothing, named like a folder in the image: nothing is
    /// created at its target. (On main's code mkdir fails with EEXIST and
    /// the path is then chmodded and entered by path, which fails on the
    /// missing target, so the fill returns an error: this fails.)
    #[test]
    fn fill_leaves_a_dangling_link_named_like_a_folder_alone() {
        let (base, img, vol, out) = fill_dirs("dangling");
        std::fs::create_dir_all(img.join("data")).unwrap();
        std::fs::write(img.join("data/x"), b"x").unwrap();
        let gone = out.join("not-there");
        std::os::unix::fs::symlink(&gone, vol.join("data")).unwrap();

        fill_bounded(&img, &vol).unwrap();

        assert!(!gone.exists(), "nothing was made at the link's target");
        assert_eq!(std::fs::read_link(vol.join("data")).unwrap(), gone, "the link is still a link");
        assert_eq!(names(&out), Vec::<String>::new());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Smoke test: one thread keeps swapping one name in the volume between
    /// a real folder and a link to `out` while the main thread fills fresh
    /// copies of the volume, for about a second. Nothing of the image may
    /// ever appear in `out`. The only errors accepted are the ones a racing
    /// writer can cause inside the volume: ENOENT / ENOTEMPTY / EEXIST /
    /// ENOTDIR / ELOOP (the name was removed, replaced or filled under us).
    /// The writer works on the volume itself, so the fill is run on the same
    /// volume again and again; a name that is already filled is left alone.
    /// (On main's code a fill that lands on the link writes into `out`.)
    #[test]
    fn fill_holds_when_a_name_keeps_changing() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (base, img, vol, out) = fill_dirs("race");
        std::fs::create_dir_all(img.join("data/deep")).unwrap();
        for f in ["data/a", "data/b", "data/deep/c"] {
            std::fs::write(img.join(f), b"from the image").unwrap();
        }
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let (stop, vol, out) = (stop.clone(), vol.clone(), out.clone());
            std::thread::spawn(move || {
                let name = vol.join("data");
                let mut link = false;
                while !stop.load(Ordering::Relaxed) {
                    // Remove whatever is there (a folder with files, or a
                    // link), then put the other kind in its place.
                    if std::fs::symlink_metadata(&name).map(|m| m.is_dir()).unwrap_or(false) {
                        let _ = std::fs::remove_dir_all(&name);
                    } else {
                        let _ = std::fs::remove_file(&name);
                    }
                    let _ = if link { std::os::unix::fs::symlink(&out, &name) } else { std::fs::create_dir(&name) };
                    link = !link;
                }
            })
        };
        let start = std::time::Instant::now();
        let mut errors = Vec::new();
        while start.elapsed() < std::time::Duration::from_secs(1) {
            // Called directly: nothing here can make the fill wait, and a
            // thread for every fill runs into the thread limit under load.
            if let Err(e) = super::copy_tree(&img, &vol) {
                match e.raw_os_error() {
                    Some(libc::ENOENT | libc::ENOTEMPTY | libc::EEXIST | libc::ENOTDIR | libc::ELOOP) => {}
                    _ => errors.push(e.to_string()),
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        assert_eq!(names(&out), Vec::<String>::new(), "nothing of the image was written behind the link");
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A volume folder that is itself a link (the volume moved to another
    /// disk on the host) is filled where it points, as it is mounted.
    #[test]
    fn fill_goes_into_a_volume_folder_that_is_a_link() {
        let (base, img, vol, out) = fill_dirs("linkedvol");
        std::fs::write(img.join("a"), b"a").unwrap();
        std::fs::remove_dir(&vol).unwrap();
        std::os::unix::fs::symlink(&out, &vol).unwrap();
        super::copy_tree(&img, &vol).unwrap();
        assert_eq!(std::fs::read(out.join("a")).unwrap(), b"a");
        assert!(std::fs::symlink_metadata(&vol).unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A start that fails on a child already gone reports what the child
    /// said, not the later step that noticed.
    #[test]
    fn a_start_failure_is_the_childs_own_reason() {
        assert_eq!(super::start_failure("kiri: cannot start container: mount etc/hosts: mount /proc/self/fd/24: Not a directory (os error 20)\n"), Some("mount etc/hosts: mount /proc/self/fd/24: Not a directory (os error 20)"));
        assert_eq!(super::start_failure("noise\nkiri: cannot start container: mount: boom\nmore"), Some("mount: boom"));
        assert_eq!(super::start_failure("hello from the program\n"), None);
        assert_eq!(super::start_failure("kiri: cannot start container: \n"), None);
        // As the child writes it: a backslash as it is, line breaks as spaces.
        let said = format!("kiri: cannot start container: {}\n", super::sys::one_line("user: unknown user DOM\\bob"));
        assert_eq!(super::start_failure(&said), Some("user: unknown user DOM\\bob"));
        let said = format!("kiri: cannot start container: {}\n", super::sys::one_line("user: unknown user a\nb"));
        assert_eq!(super::start_failure(&said), Some("user: unknown user a b"));
        assert_eq!(super::start_failure(""), None);
    }

    /// `network connect` to a user network (#449): the embedded DNS becomes
    /// the only nameserver; search and options stay; `--dns` servers are
    /// handed back for it to forward to, Kiri's own resolvers are not.
    #[test]
    fn joining_a_user_network_names_the_embedded_dns() {
        let kiri = |a: [u8; 4]| a == [172, 17, 0, 1] || a == [192, 168, 64, 1];
        let (text, fwd) = super::joined_user_network("nameserver 172.17.0.1\nsearch corp.example\noptions ndots:2\n", kiri).expect("rewritten");
        assert_eq!(text, "nameserver 127.0.0.11\nsearch corp.example\noptions ndots:2\n");
        assert!(fwd.is_empty(), "the bridge gateway is Kiri's own resolver");
        let (text, fwd) = super::joined_user_network("nameserver 8.8.8.8\nnameserver 192.168.64.1\n", kiri).expect("rewritten");
        assert_eq!(text, "nameserver 127.0.0.11\n");
        assert_eq!(fwd, [[8, 8, 8, 8]], "--dns servers are forwarded to");
        assert!(super::joined_user_network("nameserver 127.0.0.11\n", kiri).is_none(), "already on the embedded DNS");
        assert!(super::joined_user_network("", kiri).is_none(), "no file to speak of");
    }

    #[test]
    fn mounts_go_parents_first_whatever_order_they_came_in() {
        let m = |t: &str| super::Mount { source: None, target: std::ffi::CString::new(t).unwrap(), ro: false, dir: true, copy_up: false, tmpfs_opts: None, tmpfs_flags: 0 };
        let mut mounts = vec![m("app/node_modules"), m("app"), m("data"), m("app/cache/x"), m(".")];
        mounts.sort_by_key(|x| super::depth(&x.target));
        let order: Vec<_> = mounts.iter().map(|x| x.target.to_str().unwrap()).collect();
        assert_eq!(order, [".", "app", "data", "app/node_modules", "app/cache/x"]);
    }

    fn resolv(flags: u32, opts: &[&str], upstream: &[[u8; 4]]) -> String {
        let mut buf = Vec::new();
        let net = Net { bridge: "kiri1", ip: [10, 5, 0, 2], prefix: 24, gateway: [10, 5, 0, 1], extra_hosts: &[], dns_names: &[], opts };
        encode_run(&mut buf, "c1", &[], &["sh"], &[], "/", "", "c1", flags, net, &[], &Limits::default(), 0, &[] as &[&str], &[]);
        let Ok(Msg::Run(spec)) = decode(&buf) else { panic!() };
        super::resolv_conf(&spec, upstream, (flags & run_flags::DNS_GATEWAY != 0).then_some(super::super::dns::EMBEDDED))
    }

    #[test]
    fn resolv_conf_follows_the_dns_settings() {
        let opts = ["nameserver=8.8.8.8", "search=corp.example", "search=.", "option=ndots:2", "option=timeout:1"];
        // A user network keeps the gateway's DNS, which forwards to 8.8.8.8.
        assert_eq!(resolv(run_flags::DNS_GATEWAY, &opts, &[[192, 168, 64, 1]]), "nameserver 127.0.0.11\nsearch corp.example\noptions ndots:2 timeout:1\n");
        // The default bridge and host networks get the servers themselves.
        assert_eq!(resolv(0, &opts, &[[192, 168, 64, 1]]), "nameserver 8.8.8.8\nsearch corp.example\noptions ndots:2 timeout:1\n");
        assert_eq!(resolv(0, &[], &[[192, 168, 64, 1]]), "nameserver 192.168.64.1\n");
    }

    #[test]
    fn a_resolver_at_one_of_the_hosts_own_addresses_goes_through_the_agent() {
        let mut buf = Vec::new();
        let net = Net { bridge: "kiri0", ip: [172, 17, 0, 2], prefix: 16, gateway: [172, 17, 0, 1], extra_hosts: &[], dns_names: &[], opts: &[] };
        encode_run(&mut buf, "c1", &[], &["sh"], &[], "/", "", "c1", 0, net, &[], &Limits::default(), 0, &[] as &[&str], &[]);
        let Ok(Msg::Run(spec)) = decode(&buf) else { panic!() };
        // TEST-NET-3: no machine running the tests has it.
        assert!(!super::embedded_dns(&spec, &[[203, 0, 113, 53]], false), "a resolver elsewhere is asked directly");
        assert!(!super::embedded_dns(&spec, &[], false), "no resolver: the public fallback, not the agent");
    }

    #[test]
    fn in_the_vm_the_default_bridge_goes_through_the_agent() {
        let spec_with = |flags: u32, opts: &[&str], f: &dyn Fn(&kiri_proto::RunSpec<'_>)| {
            let mut buf = Vec::new();
            let net = Net { bridge: "kiri0", ip: [172, 17, 0, 2], prefix: 16, gateway: [172, 17, 0, 1], extra_hosts: &[], dns_names: &[], opts };
            encode_run(&mut buf, "c1", &[], &["sh"], &[], "/", "", "c1", flags, net, &[], &Limits::default(), 0, &[] as &[&str], &[]);
            let Ok(Msg::Run(spec)) = decode(&buf) else { panic!() };
            f(&spec);
        };
        let gateway = [[192, 168, 64, 1]];
        spec_with(0, &[], &|s| assert!(super::embedded_dns(s, &gateway, true), "so a refused gateway can fall back to the host"));
        spec_with(0, &["nameserver=8.8.8.8"], &|s| assert!(!super::embedded_dns(s, &gateway, true), "--dns servers are asked directly"));
        spec_with(run_flags::NET_HOST, &[], &|s| assert!(!super::embedded_dns(s, &gateway, true), "host networking keeps the VM's resolv.conf"));
        // Named by the bridge's gateway, not 127.0.0.11: builders keep it
        // for their steps rather than swapping in 8.8.8.8.
        spec_with(0, &[], &|s| assert_eq!(super::embedded_address(s, true), [172, 17, 0, 1]));
        spec_with(0, &[], &|s| assert_eq!(super::resolv_conf(s, &gateway, Some(super::embedded_address(s, true))), "nameserver 172.17.0.1\n"));
        // A user network keeps Docker's 127.0.0.11, as does a Linux host.
        spec_with(run_flags::DNS_GATEWAY, &[], &|s| assert_eq!(super::embedded_address(s, true), super::super::dns::EMBEDDED));
        spec_with(0, &[], &|s| assert_eq!(super::embedded_address(s, false), super::super::dns::EMBEDDED));
    }

    #[test]
    fn macs_parse() {
        assert_eq!(super::net::parse_mac("02:42:ac:11:00:02"), Some([2, 0x42, 0xac, 0x11, 0, 2]));
        assert_eq!(super::net::mac_for([172, 17, 0, 2]), [2, 0x42, 0xac, 0x11, 0, 2]);
        assert_eq!(super::net::parse_mac("02:42:ac:11:00"), None);
        assert_eq!(super::net::parse_mac("02:42:ac:11:00:02:03"), None);
    }

    /// A mount line is three fields, or seven with a volume's driver options.
    /// An image's VOLUME key that carries tabs would otherwise supply the
    /// driver fields — a device to mount — so a line of any other shape is
    /// refused here as well as where the host writes it.
    #[test]
    fn a_mount_line_with_forged_fields_is_refused() {
        let net = Net { bridge: "", ip: [0; 4], prefix: 0, gateway: [0; 4], extra_hosts: &[], dns_names: &[], opts: &[] };
        let parse = |mounts: &[&str]| {
            let mut buf = Vec::new();
            encode_run(&mut buf, "c1", &[], &["sh"], &[], "/", "", "c1", 0, net, mounts, &Limits::default(), 0, &[] as &[&str], &[]);
            let Ok(Msg::Run(spec)) = decode(&buf) else { panic!("not a run") };
            super::parse_mounts(spec.mounts)
        };
        // What a VOLUME key of `/app\tx\tnone\t/mnt/mac\t1000\ty` becomes.
        let injected = "/var/lib/kiri/volumes/anon/_data\t/app\tx\tnone\t/mnt/mac\t1000\ty\tcreate,copyup";
        let e = parse(&[injected]).err().expect("an eight-field line is refused");
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        assert!(e.to_string().contains("bad mount"), "{e}");
        // A plain mount still parses, and keeps its target and flags.
        let dir = std::env::temp_dir().join(format!("kiri-mount-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = format!("{}\t/scratch\tro", dir.display());
        let mounts = parse(&[&plain]).unwrap();
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].ro);
        assert_eq!(mounts[0].target.to_bytes(), b"scratch");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
