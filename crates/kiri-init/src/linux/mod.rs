mod agent;
mod cgroup;
mod container;
mod devices;
mod copy;
mod dns;
mod fsnotify;
mod fsview;
mod hostopts;
mod inroot;
mod layers;
mod net;
mod netlink;
mod pid1;
mod relay;
mod roots;
mod seccomp;
mod sys;
mod transport;
mod usb;
mod usage;
mod volmount;

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use kiri_proto::{HOST_PORT, Hello, Msg, PROTOCOL};

use sys::{boottime_ns, check, mount};

/// Where images and containers live: the data disk if attached, else tmpfs.
const DATA: &str = "/var/lib/kiri";

pub fn main() {
    let init_start_ns = boottime_ns();
    let result = run(init_start_ns);
    if let Err(e) = &result {
        // Nothing above us can handle this; say why on the console and stop.
        eprintln!("kiri-init: {e}");
    }
    if transport::is_host_process() {
        std::process::exit(i32::from(result.is_err()));
    }
    power_off();
}

fn run(init_start_ns: u64) -> io::Result<()> {
    let paths = if transport::is_host_process() {
        // On a Linux host: the host's kernel filesystems, network and clock.
        host_dirs()?
    } else {
        leave_initramfs()?;
        mount_kernel_filesystems()?;
        sys::sethostname(b"kiri")?;
        // Containers may bind the VM's /etc/hostname, as on a Linux host.
        let _ = std::fs::write("/etc/hostname", "kiri\n");
        sys::loopback_up()?;
        data_dirs()?
    };

    let served = serve(&paths, init_start_ns);
    if transport::is_host_process() {
        // However the agent stops (Shutdown, its daemon gone, SIGTERM, an
        // error), nothing of the engine stays behind on a Linux host.
        container::sweep(&paths, &paths.cgroup);
        net::remove_host_network();
    }
    served
}

fn serve(paths: &container::Paths, init_start_ns: u64) -> io::Result<()> {
    let mut conn = dial_host()?;
    let (mem_total_kib, mem_available_kib) = meminfo();
    let mut out = Vec::with_capacity(256);
    Msg::Hello(Hello { protocol: PROTOCOL, init_start_ns, ready_ns: boottime_ns(), mem_total_kib, mem_available_kib })
        .encode(&mut out);
    conn.write_all(&out)?;
    agent::serve(conn, paths)
}

/// Re-roots PID 1 onto a tmpfs. The kernel refuses pivot_root while the
/// current root is the initramfs, and containers need pivot_root (chroot
/// alone can be escaped). This is what switch_root does for normal systems.
fn leave_initramfs() -> io::Result<()> {
    sys::mkdir(c"/root", 0o755);
    mount(c"root", c"/root", c"tmpfs", 0, Some(c"mode=0755"))?;
    // /lib/modules for containers that bind it (kind does); the kernel has
    // no modules, so it stays empty.
    for d in [c"/root/dev", c"/root/proc", c"/root/sys", c"/root/run", c"/root/tmp", c"/root/etc", c"/root/var", c"/root/lib", c"/root/lib/modules"] {
        sys::mkdir(d, 0o755);
    }
    // Free the initramfs copy of this binary; the running image stays mapped.
    let _ = std::fs::remove_file("/init");
    // SAFETY: plain path syscalls with valid NUL-terminated strings.
    unsafe {
        check(libc::chdir(c"/root".as_ptr()) as _, "chdir /root")?;
        check(libc::mount(c".".as_ptr(), c"/".as_ptr(), std::ptr::null(), libc::MS_MOVE, std::ptr::null()) as _, "move root")?;
        check(libc::chroot(c".".as_ptr()) as _, "chroot")?;
        check(libc::chdir(c"/".as_ptr()) as _, "chdir /")?;
    }
    // The console descriptors were opened from the old root's /dev/console
    // and keep working; nothing else refers to the initramfs any more.
    Ok(())
}

fn mount_kernel_filesystems() -> io::Result<()> {
    let nosuid_nodev_noexec = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    let m = |src, target: &std::ffi::CStr, fstype, flags, data| {
        sys::mkdir(target, 0o755);
        mount(src, target, fstype, flags, data)
    };
    m(c"proc", c"/proc", c"proc", nosuid_nodev_noexec, None)?;
    m(c"sysfs", c"/sys", c"sysfs", nosuid_nodev_noexec, None)?;
    m(c"devtmpfs", c"/dev", c"devtmpfs", libc::MS_NOSUID, Some(c"mode=0755"))?;
    m(c"devpts", c"/dev/pts", c"devpts", libc::MS_NOSUID | libc::MS_NOEXEC, Some(c"ptmxmode=0666,mode=0620"))?;
    m(c"shm", c"/dev/shm", c"tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some(c"mode=1777"))?;
    m(c"cgroup2", c"/sys/fs/cgroup", c"cgroup2", nosuid_nodev_noexec, Some(c"nsdelegate,memory_recursiveprot"))?;
    m(c"run", c"/run", c"tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some(c"mode=0755"))?;
    m(c"tmp", c"/tmp", c"tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some(c"mode=1777"))?;
    Ok(())
}

/// On a Linux host: images and containers under /var/lib/kiri, as in the
/// VM, and containers under the `kiri` cgroup.
///
/// /var/lib/kiri is 0710 (the kiri group may pass through it, no one may
/// list it) and what is under it 0700: layer contents and container
/// directories are root's; containers reach them through their mounts. An
/// existing tree is tightened too, in case an older build left it 0755.
/// The umask is set here, not inherited: the unit's UMask=0027 is right for
/// the daemon, but the files this process makes for containers (their
/// /etc/hosts and resolv.conf, bind-mount targets, new volumes) and the
/// umask the containers' own processes start with must stay conventional
/// (022), or nothing running as a non-root user in a container could read
/// its own resolv.conf.
fn host_dirs() -> io::Result<container::Paths> {
    // SAFETY: umask only changes this process's file-creation mask.
    unsafe { libc::umask(0o022) };
    let root = PathBuf::from(DATA);
    let mkdir = |p: &std::path::Path, mode: u32| -> io::Result<()> {
        std::fs::create_dir_all(p)?;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    };
    mkdir(&root, 0o710)?;
    mkdir(&root.join("volumes"), 0o700)?;
    take_over(&root)?;
    let paths = container::Paths { layers: root.join("layers"), containers: root.join("containers"), cgroup: host_cgroup()? };
    mkdir(&paths.layers, 0o700)?;
    mkdir(&paths.containers, 0o700)?;
    // What an agent that died uncleanly left running: under the cgroup it
    // used (recorded below), and under this one.
    let record = root.join("agent.cgroup");
    if let Ok(old) = std::fs::read_to_string(&record) {
        let old = PathBuf::from(old.trim());
        if old != paths.cgroup && old.starts_with("/sys/fs/cgroup") {
            container::sweep(&paths, &old);
        }
    }
    container::sweep(&paths, &paths.cgroup);
    std::fs::write(&record, paths.cgroup.as_os_str().as_encoded_bytes())?;
    std::env::set_current_dir(&paths.layers)?;
    Ok(paths)
}

/// The lock on a Linux host's data directory, held while the agent runs.
static AGENT_LOCK: std::sync::OnceLock<File> = std::sync::OnceLock::new();

/// One agent per data directory on a Linux host: takes `agent.lock` (an
/// flock, released when the process ends however it ends). An agent that
/// still holds it is stopped if its daemon is gone (an orphan: SIGTERM, so
/// it cleans up, then SIGKILL), and waited for a few seconds otherwise (a
/// daemon still shutting down); if it stays, starting fails and says why
/// rather than failing later on its sockets and ports.
fn take_over(root: &std::path::Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let path = root.join("agent.lock");
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&path)?;
    let locked = |f: &File| {
        // SAFETY: flock on a descriptor we own.
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    };
    let start = std::time::Instant::now();
    let mut signalled = None;
    while !locked(&file) {
        let mut text = String::new();
        let _ = (&file).read_to_string(&mut text);
        let _ = std::io::Seek::rewind(&mut file);
        let old: libc::pid_t = text.trim().parse().unwrap_or(0);
        let daemon = parent_of(old);
        let orphan = old > 0 && !daemon.is_some_and(is_daemon);
        let waited = start.elapsed();
        if orphan && signalled.is_none() {
            eprintln!("kiri-init: stopping agent {old}, left running by a daemon that is gone");
            // SAFETY: signalling the process that holds our data directory's lock.
            unsafe { libc::kill(old, libc::SIGTERM) };
            signalled = Some(waited);
        } else if orphan && signalled.is_some_and(|t| waited - t > std::time::Duration::from_secs(5)) {
            // SAFETY: as above; it did not stop on SIGTERM.
            unsafe { libc::kill(old, libc::SIGKILL) };
        }
        if waited > std::time::Duration::from_secs(8) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another Kiri engine is running on this machine (agent {old}, daemon {}): stop it first", daemon.unwrap_or(0)),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    file.set_len(0)?;
    use std::os::unix::fs::FileExt;
    file.write_all_at(format!("{}\n", std::process::id()).as_bytes(), 0)?;
    let _ = AGENT_LOCK.set(file);
    Ok(())
}

/// A live process's parent, from /proc/<pid>/stat.
fn parent_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // pid (comm) state ppid …; comm may hold spaces and parentheses.
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// Whether a process is a `kiri daemon` (by its arguments: the binary may
/// have any name). PID 1 or a subreaper adopting an orphan is not.
fn is_daemon(pid: libc::pid_t) -> bool {
    pid > 1 && std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c.split(|b| *b == 0).any(|a| a == b"daemon"))
}

/// The cgroup containers go under on a Linux host, chosen as dockerd and
/// containerd choose theirs on cgroup v2:
///
/// - In a cgroup delegated to Kiri (a systemd service with `Delegate=yes`,
///   or one made for it that holds only the daemon and this agent): those
///   processes, and no others, move to its `kiri-host` leaf, so it can hand
///   controllers down; containers go in its `kiri`.
/// - At the root of a cgroup namespace that holds processes (Kiri inside a
///   container): they move to a `kiri-host` leaf, as Docker-in-Docker's
///   entrypoint does; containers go in `kiri` beside it.
/// - Anywhere else (a login session, WSL's `init.scope`: cgroups shared
///   with processes that are not Kiri's): nothing moves, and containers go
///   in `kiri` at the top of the hierarchy, as dockerd's cgroupfs driver
///   puts them in `docker`. Moving every process of a shared cgroup took
///   WSL's own along, one level deeper on every start.
///
/// Started again from inside a `kiri-host` leaf (the daemon stays there
/// when its agent restarts), it is not nested again.
fn host_cgroup() -> io::Result<PathBuf> {
    let own = std::fs::read_to_string("/proc/self/cgroup")?;
    let rel = own.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("/").trim().trim_start_matches('/').to_owned();
    let root = PathBuf::from("/sys/fs/cgroup");
    if !root.join("cgroup.controllers").exists() {
        mount_cgroup2(&root)?;
    }
    let own = root.join(&rel);
    let mut base = own.clone();
    while base != root && base.file_name().is_some_and(|n| n == "kiri-host") {
        base.pop();
    }
    let procs = |dir: &std::path::Path| -> Vec<String> {
        std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default().lines().map(str::to_owned).collect()
    };
    // Moves processes to the leaf beside the containers' cgroup.
    let to_leaf = |pids: Vec<String>| {
        let leaf = base.join("kiri-host");
        let _ = std::fs::create_dir(&leaf);
        for pid in pids {
            let _ = std::fs::write(leaf.join("cgroup.procs"), pid);
        }
    };
    let base = if base == root {
        // Only the hierarchy's true root (which has no cgroup.type) may hold
        // processes and hand out controllers; a namespace's root is not it.
        if root.join("cgroup.type").exists() {
            to_leaf(procs(&root));
        }
        root
    } else if delegated(&own, &procs(&own)) {
        if own != base.join("kiri-host") {
            to_leaf(procs(&own));
        }
        base
    } else {
        root
    };
    let kiri = base.join("kiri");
    containers_cgroup(&kiri);
    Ok(kiri)
}

/// Makes the cgroup containers go under, with the controllers handed down
/// to it and from it. Also when a container starts and it is gone: systemd
/// removes a stopped service's cgroups when it sees them empty, and on a
/// restart that can land after the new agent made this one, still empty
/// (Fedora 44's systemd 259 did); `kiri-host`, which holds the daemon,
/// stays, and every container start then failed with ENOENT.
pub fn containers_cgroup(kiri: &std::path::Path) {
    let _ = std::fs::create_dir(kiri);
    let Some(base) = kiri.parent() else { return };
    for c in ["+cpu", "+memory", "+pids", "+cpuset", "+io"] {
        let _ = std::fs::write(base.join("cgroup.subtree_control"), c);
        let _ = std::fs::write(kiri.join("cgroup.subtree_control"), c);
    }
}

/// `ip netns exec` mounts a fresh /sys for its network namespace, without
/// the cgroup tree, and every container start then failed with ENOENT. An
/// empty /sys/fs/cgroup gets cgroup2 mounted on it (in that command's
/// private mount namespace, so nothing outside changes); anything else
/// there (cgroup v1) fails the start now, saying why.
fn mount_cgroup2(root: &std::path::Path) -> io::Result<()> {
    let empty = std::fs::read_dir(root).map(|mut d| d.next().is_none());
    match empty {
        Ok(true) => mount(c"cgroup2", c"/sys/fs/cgroup", c"cgroup2", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None)
            .map_err(|e| io::Error::new(e.kind(), format!("/sys/fs/cgroup has no cgroup tree (ip netns exec remounts /sys) and mounting cgroup2 there failed: {e}"))),
        Ok(false) => Err(io::Error::new(io::ErrorKind::Unsupported, "Kiri needs cgroup v2 at /sys/fs/cgroup (this machine has cgroup v1 or a hybrid tree there)")),
        Err(e) => Err(io::Error::new(e.kind(), format!("/sys/fs/cgroup: {e}; Kiri needs cgroup v2 mounted there"))),
    }
}

/// Whether the agent's cgroup is Kiri's own: a systemd service's (with
/// `Delegate=yes`, the whole cgroup is the service's), or one holding no
/// process but this agent and its parent, the daemon.
fn delegated(own: &std::path::Path, pids: &[String]) -> bool {
    if own.extension().is_some_and(|e| e == "service") {
        return true;
    }
    let (me, parent) = (std::process::id().to_string(), std::os::unix::process::parent_id().to_string());
    pids.iter().all(|p| *p == me || *p == parent)
}

/// Mounts the data disk (or a tmpfs without one), creates the layer and
/// container directories and the cgroup that all containers live under.
fn data_dirs() -> io::Result<container::Paths> {
    sys::mkdir(c"/var/lib", 0o755);
    sys::mkdir(c"/var/lib/kiri", 0o755);
    // noinit_itable: the disk is made with lazy inode tables; zeroing them in
    // the background would only write gigabytes of zeros to the host.
    let disk = mount(c"/dev/vda", c"/var/lib/kiri", c"ext4", libc::MS_NOATIME, Some(c"noinit_itable"));
    if let Err(e) = disk {
        eprintln!("kiri-init: no data disk ({e}); keeping images in memory");
        mount(c"kiri", c"/var/lib/kiri", c"tmpfs", libc::MS_NOATIME, Some(c"mode=0755"))?;
    }
    // The Mac's filesystem over virtiofs, for bind mounts.
    sys::mkdir(c"/mnt", 0o755);
    sys::mkdir(c"/mnt/mac", 0o755);
    if let Err(e) = mount(c"kiri-mac", c"/mnt/mac", c"virtiofs", libc::MS_NOATIME, None) {
        eprintln!("kiri-init: no Mac file share ({e}); bind mounts of Mac paths will fail");
    }
    // An x86-64 guest (Intel Macs, WSL2) runs amd64 binaries natively.
    #[cfg(target_arch = "aarch64")]
    rosetta();
    std::fs::create_dir_all(format!("{DATA}/volumes"))?;
    let root = PathBuf::from(DATA);
    let paths = container::Paths {
        layers: root.join("layers"),
        containers: root.join("containers"),
        cgroup: PathBuf::from("/sys/fs/cgroup/kiri"),
    };
    std::fs::create_dir_all(&paths.layers)?;
    std::fs::create_dir_all(&paths.containers)?;
    // Stopped containers keep their writable layers across boots; the host
    // sends `Retain` with the ones it still knows about, and the rest go.
    let _ = std::fs::create_dir(&paths.cgroup);
    // cpu, memory and pids for limits; cpuset and io so containers that
    // run their own containers (Kubernetes, Docker in Docker) find them.
    sys::write_file("/sys/fs/cgroup/cgroup.subtree_control", "+cpu +memory +pids")?;
    sys::write_file("/sys/fs/cgroup/kiri/cgroup.subtree_control", "+cpu +memory +pids")?;
    for c in ["+cpuset", "+io"] {
        let _ = sys::write_file("/sys/fs/cgroup/cgroup.subtree_control", c);
        let _ = sys::write_file("/sys/fs/cgroup/kiri/cgroup.subtree_control", c);
    }
    // Overlay lower dirs are given relative to the layer store.
    std::env::set_current_dir(&paths.layers)?;
    Ok(paths)
}

/// x86-64 Linux binaries through Rosetta, when the Mac shares it: amd64
/// images then run as they do on Docker Desktop and OrbStack. The `F` flag
/// opens the interpreter now, so it works inside every container's mounts.
#[cfg(target_arch = "aarch64")]
fn rosetta() {
    sys::mkdir(c"/run/rosetta", 0o755);
    if mount(c"rosetta", c"/run/rosetta", c"virtiofs", libc::MS_RDONLY, None).is_err() {
        return;
    }
    if let Err(e) = mount(c"binfmt_misc", c"/proc/sys/fs/binfmt_misc", c"binfmt_misc", 0, None) {
        eprintln!("kiri-init: no binfmt_misc ({e}); amd64 images will not run");
        return;
    }
    const X86_64: &str = concat!(
        ":rosetta:M::",
        r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00",
        ":",
        r"\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff",
        ":/run/rosetta/rosetta:CF",
    );
    if let Err(e) = sys::write_file("/proc/sys/fs/binfmt_misc/register", X86_64) {
        eprintln!("kiri-init: registering Rosetta: {e}");
    }
}

fn dial_host() -> io::Result<File> {
    // The host registers its listener before the VM starts, so the first
    // attempt normally succeeds. Retry briefly in case the vsock device is
    // still being probed.
    let mut last = io::Error::from(io::ErrorKind::NotConnected);
    for attempt in 0..200 {
        match transport::dial(HOST_PORT) {
            Ok(sock) => return Ok(File::from(sock)),
            Err(e) => last = e,
        }
        if attempt > 0 && attempt % 50 == 0 {
            eprintln!("kiri-init: still dialing host: {last}");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Err(io::Error::new(last.kind(), format!("dial host: {last}")))
}

/// MemTotal and MemAvailable in KiB.
fn meminfo() -> (u64, u64) {
    let mut buf = [0u8; 2048];
    let Ok(n) = File::open("/proc/meminfo").and_then(|mut f| f.read(&mut buf)) else { return (0, 0) };
    let s = std::str::from_utf8(&buf[..n]).unwrap_or_default();
    let field = |name: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap_or(0)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

fn power_off() -> ! {
    // SAFETY: sync and reboot take no pointers.
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    // reboot only returns on failure; PID 1 must never exit.
    loop {
        // SAFETY: pause takes no arguments.
        unsafe { libc::pause() };
    }
}
