//! HostConfig settings applied when a container starts, from the `opts`
//! lines of `Run` and `Exec`: resource limits (`--ulimit`), sysctls,
//! devices and device rules, and supplementary groups (`--group-add`). The
//! daemon has checked them already; this side refuses anything malformed
//! rather than trusting it.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use kiri_proto::StrList;

use super::devices::{self, Rule};
use super::sys;

/// Which namespace a sysctl is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ns {
    Net,
    Ipc,
    Uts,
}

#[derive(Debug)]
pub struct Sysctl {
    pub ns: Ns,
    /// Relative to /proc/sys.
    pub path: String,
    pub value: String,
}

#[derive(Debug)]
pub struct Device {
    pub host: String,
    pub container: String,
    pub perms: String,
}

/// A device node to make in the container, worked out before clone.
#[derive(Debug)]
pub struct Node {
    /// Relative to the container root.
    pub target: CString,
    /// File type and permission bits, as on the host's node.
    pub mode: libc::mode_t,
    pub major: u32,
    pub minor: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Default)]
pub struct HostOpts {
    /// (RLIMIT_* resource, soft, hard); u64::MAX is unlimited.
    pub rlimits: Vec<(u32, u64, u64)>,
    pub sysctls: Vec<Sysctl>,
    pub devices: Vec<Device>,
    pub rules: Vec<Rule>,
    /// `--group-add`: names (looked up in the container's /etc/group) or ids.
    pub groups: Vec<String>,
}

fn bad(line: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("bad host option {line:?}"))
}

pub fn parse(list: StrList<'_>) -> io::Result<HostOpts> {
    let mut o = HostOpts::default();
    for line in list.iter() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["rlimit", res, soft, hard] => {
                let n = |s: &str| s.parse::<u64>().map_err(|_| bad(line));
                let res: u32 = res.parse().map_err(|_| bad(line))?;
                if res > 15 {
                    return Err(bad(line));
                }
                o.rlimits.push((res, n(soft)?, n(hard)?));
            }
            ["sysctl", ns, path, value] => {
                let ns = match *ns {
                    "net" => Ns::Net,
                    "ipc" => Ns::Ipc,
                    "uts" => Ns::Uts,
                    _ => return Err(bad(line)),
                };
                let first = path.split('/').next().unwrap_or("");
                let allowed = match ns {
                    Ns::Net => first == "net",
                    Ns::Ipc => (first == "kernel" && path.split('/').count() == 2) || path.starts_with("fs/mqueue/"),
                    Ns::Uts => *path == "kernel/domainname",
                };
                if !allowed || path.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
                    return Err(bad(line));
                }
                o.sysctls.push(Sysctl { ns, path: (*path).to_owned(), value: (*value).to_owned() });
            }
            ["device", host, container, perms] => {
                for p in [host, container] {
                    if !p.starts_with('/') || p.split('/').any(|c| c == "..") {
                        return Err(bad(line));
                    }
                }
                devices::access_bits(perms).ok_or_else(|| bad(line))?;
                o.devices.push(Device { host: (*host).to_owned(), container: (*container).to_owned(), perms: (*perms).to_owned() });
            }
            ["devrule", kind, major, minor, perms] => o.rules.push(devices::parse_rule(kind, major, minor, perms).ok_or_else(|| bad(line))?),
            ["group", g] if !g.is_empty() => o.groups.push((*g).to_owned()),
            _ => return Err(bad(line)),
        }
    }
    Ok(o)
}

/// Docker's error for a device it cannot add.
fn gather_error(path: &str, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("error gathering device information while adding custom device {path:?}: {e}"))
}

/// The nodes `--device` asks for and the rules that let the container use
/// them. A directory adds every device below it, as Docker does.
pub fn resolve_devices(opts: &HostOpts) -> io::Result<(Vec<Node>, Vec<Rule>)> {
    let mut nodes = Vec::new();
    let mut rules = opts.rules.clone();
    for d in &opts.devices {
        let access = devices::access_bits(&d.perms).unwrap_or(devices::MKNOD | devices::READ | devices::WRITE);
        let meta = fs::metadata(&d.host).map_err(|e| gather_error(&d.host, errno_text(&e)))?;
        let mut found = Vec::new();
        if meta.is_dir() {
            walk(Path::new(&d.host), Path::new(&d.container), &mut found)?;
        } else if meta.file_type().is_char_device() || meta.file_type().is_block_device() {
            found.push((d.container.clone(), meta));
        } else {
            return Err(gather_error(&d.host, "not a device node"));
        }
        for (target, m) in found {
            let (major, minor) = (libc::major(m.rdev()) as u32, libc::minor(m.rdev()) as u32);
            let kind = if m.file_type().is_block_device() { 1 } else { 2 };
            rules.push(Rule { kind, major: Some(major), minor: Some(minor), access });
            let rel = target.trim_start_matches('/');
            nodes.push(Node { target: CString::new(rel).map_err(|_| gather_error(&d.host, "bad path"))?, mode: m.mode(), major, minor, uid: m.uid(), gid: m.gid() });
        }
    }
    Ok((nodes, rules))
}

/// Device nodes under `host`, each with its path under `container`.
fn walk(host: &Path, container: &Path, out: &mut Vec<(String, fs::Metadata)>) -> io::Result<()> {
    for e in fs::read_dir(host).map_err(|e| gather_error(&host.to_string_lossy(), errno_text(&e)))?.flatten() {
        let Ok(m) = fs::metadata(e.path()) else { continue };
        let target = container.join(e.file_name());
        if m.is_dir() {
            walk(&e.path(), &target, out)?;
        } else if m.file_type().is_char_device() || m.file_type().is_block_device() {
            out.push((target.to_string_lossy().into_owned(), m));
        }
    }
    Ok(())
}

/// Makes the device nodes in the container (relative paths looked up
/// inside `root`, its root before pivot_root). A node already there (a
/// privileged container's copy of the host's devices) is left alone.
pub fn make_nodes(root: BorrowedFd<'_>, nodes: &[Node]) -> io::Result<()> {
    for n in nodes {
        let path = Path::new(std::ffi::OsStr::from_bytes(n.target.as_bytes()));
        let at = super::inroot::open(root, path, super::inroot::Last::Node(n.mode, libc::makedev(n.major, n.minor)))?;
        if !at.created {
            continue;
        }
        let p = at.proc_path();
        // SAFETY: valid NUL-terminated path; mknod applied the umask, and the
        // node keeps the host node's mode and owner, as Docker's does.
        unsafe {
            libc::chmod(p.as_ptr(), n.mode & 0o7777);
            libc::chown(p.as_ptr(), n.uid, n.gid);
        }
    }
    Ok(())
}

/// What Docker writes in every container with a network namespace of its
/// own: a non-root process may listen on any port (nginx or Caddy on 80 as
/// a user) and ping without CAP_NET_RAW. A `--sysctl` for the same key
/// wins, as it does on Docker.
const NET_DEFAULTS: [(&str, &str); 2] = [("net/ipv4/ip_unprivileged_port_start", "0"), ("net/ipv4/ping_group_range", "0 2147483647")];

/// Puts Docker's defaults first in `sysctls`, leaving out any key the
/// container sets itself.
pub fn add_net_defaults(sysctls: &mut Vec<Sysctl>) {
    let defaults = NET_DEFAULTS
        .iter()
        .filter(|(path, _)| !sysctls.iter().any(|s| s.path == *path))
        .map(|(path, value)| Sysctl { ns: Ns::Net, path: (*path).to_owned(), value: (*value).to_owned() });
    let mine: Vec<Sysctl> = defaults.collect();
    sysctls.splice(0..0, mine);
}

/// Writes the sysctls from the agent, inside the container's namespaces:
/// /proc/sys shows the calling task's network, IPC and UTS namespace, so
/// the agent joins the one each key belongs to, writes, and comes back.
/// Runs after the network is set up, so per-interface keys (eth0) exist.
pub fn apply_sysctls(pid: libc::pid_t, sysctls: &[Sysctl]) -> io::Result<()> {
    for s in sysctls {
        let (name, kind) = match s.ns {
            Ns::Net => ("net", libc::CLONE_NEWNET),
            Ns::Ipc => ("ipc", libc::CLONE_NEWIPC),
            Ns::Uts => ("uts", libc::CLONE_NEWUTS),
        };
        let own = fs::File::open(format!("/proc/self/ns/{name}"))?;
        let theirs = fs::File::open(format!("/proc/{pid}/ns/{name}"))?;
        // The container shares the agent's own namespace (host network,
        // directly or through `--network container:`): the key would change
        // the whole VM or Linux host. The daemon refuses this first.
        if same_namespace(&own, &theirs)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("sysctl {:?} not allowed in host {name} namespace", s.path.replace('/', ".")),
            ));
        }
        sys::setns(theirs.as_raw_fd(), kind)?;
        let written = fs::write(format!("/proc/sys/{}", s.path), &s.value);
        // Coming back must work, or the agent itself is in the container.
        if let Err(e) = sys::setns(own.as_raw_fd(), kind) {
            eprintln!("kiri-init: cannot leave a container's {name} namespace: {e}");
            std::process::abort();
        }
        written.map_err(|e| io::Error::new(e.kind(), format!("write sysctl key {}: {}", s.path.replace('/', "."), errno_text(&e))))?;
    }
    Ok(())
}

/// Two /proc/*/ns files name the same namespace when their inodes match.
fn same_namespace(a: &fs::File, b: &fs::File) -> io::Result<bool> {
    let (a, b) = (a.metadata()?, b.metadata()?);
    Ok((a.dev(), a.ino()) == (b.dev(), b.ino()))
}

/// Sets the resource limits in the child, before it drops capabilities
/// (raising a hard limit needs CAP_SYS_RESOURCE). runc's error on failure.
pub fn set_rlimits(rlimits: &[(u32, u64, u64)]) -> io::Result<()> {
    for &(res, soft, hard) in rlimits {
        let lim = [soft, hard];
        // SAFETY: prlimit64 on ourselves with a valid { cur, max } pair of u64s.
        let r = unsafe { libc::syscall(libc::SYS_prlimit64, 0, res as libc::c_long, lim.as_ptr(), std::ptr::null_mut::<u64>()) };
        if r != 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("error setting rlimit type {res}: {}", errno_text(&e))));
        }
    }
    Ok(())
}

/// An errno as Go prints it (`no such file or directory`), for messages
/// that match Docker's.
pub fn errno_text(e: &io::Error) -> String {
    let Some(n) = e.raw_os_error() else { return e.to_string() };
    // SAFETY: strerror returns a valid NUL-terminated string for any errno.
    let s = unsafe { std::ffi::CStr::from_ptr(libc::strerror(n)) }.to_string_lossy().into_owned();
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or(s)
}

/// The supplementary groups for a user, as Docker gives them: the user's
/// own group `gid` first, then, read from the container's /etc/passwd and
/// /etc/group as runc does, every group that lists the user, unless the run
/// named a group (`user:group`), then `--group-add`.
pub fn supplementary_groups(user: &str, uid: u32, gid: u32, add: &[String]) -> io::Result<Vec<libc::gid_t>> {
    let group_file = fs::read_to_string("/etc/group").unwrap_or_default();
    groups_from(&group_file, user, || user_name(uid), gid, add)
}

/// [`supplementary_groups`] over `group_file`'s text; `uid_name` gives the
/// name /etc/passwd has for a numeric user.
fn groups_from(group_file: &str, user: &str, uid_name: impl FnOnce() -> Option<String>, gid: u32, add: &[String]) -> io::Result<Vec<libc::gid_t>> {
    let groups: Vec<Vec<&str>> = group_file.lines().map(|l| l.split(':').collect()).filter(|f: &Vec<&str>| f.len() >= 3).collect();
    // Docker always lists the primary group, so getgroups() includes it
    // (moby's getUser starts AdditionalGids with the user's gid).
    let mut out = vec![gid];
    let (name, group) = user.split_once(':').map_or((user, None), |(u, g)| (u, Some(g)));
    if group.is_none() {
        let name = if name.is_empty() || name.parse::<u32>().is_ok() { uid_name() } else { Some(name.to_owned()) };
        if let Some(name) = name {
            for f in &groups {
                if f.get(3).is_some_and(|members| members.split(',').any(|m| m.trim() == name)) {
                    if let Ok(gid) = f[2].parse() {
                        out.push(gid);
                    }
                }
            }
        }
    }
    for g in add {
        let gid = match g.parse::<u32>() {
            Ok(id) => id,
            Err(_) => groups
                .iter()
                .find(|f| f[0] == g)
                .and_then(|f| f[2].parse().ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unable to find group {g}: no matching entries in group file")))?,
        };
        out.push(gid);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The name /etc/passwd gives `uid`.
fn user_name(uid: u32) -> Option<String> {
    let text = fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 3 && f[2].parse() == Ok(uid)).then(|| f[0].to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_users_own_group_is_a_supplementary_group_as_with_docker() {
        let group = "root:x:0:root\nwheel:x:10:root,app\napp:x:1000:\nextra:x:2000:\n";
        let groups = |user: &str, gid: u32, add: &[&str]| {
            let add: Vec<String> = add.iter().map(|g| g.to_string()).collect();
            groups_from(group, user, || Some("app".to_owned()), gid, &add).unwrap()
        };
        // --user 1000:1000: the named group, nothing from /etc/group.
        assert_eq!(groups("1000:1000", 1000, &[]), [1000]);
        // --user app (and its uid alone): its group, then its memberships.
        assert_eq!(groups("app", 1000, &[]), [10, 1000]);
        assert_eq!(groups("1000", 1000, &[]), [10, 1000]);
        // --group-add beside it; a group given twice is listed once.
        assert_eq!(groups("1000:1000", 1000, &["2000", "app"]), [1000, 2000]);
        assert_eq!(groups("65534:1234", 1234, &[]), [1234]);
        assert!(groups_from(group, "1000:1000", || None, 1000, &["nope".to_owned()]).is_err());
    }

    fn list(lines: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        kiri_proto::encode_retain(&mut buf, lines);
        buf
    }

    #[test]
    fn a_container_gets_dockers_port_and_ping_defaults_unless_it_sets_them() {
        let mut none = Vec::new();
        add_net_defaults(&mut none);
        let keys: Vec<(&str, &str)> = none.iter().map(|s| (s.path.as_str(), s.value.as_str())).collect();
        assert_eq!(keys, [("net/ipv4/ip_unprivileged_port_start", "0"), ("net/ipv4/ping_group_range", "0 2147483647")]);
        let mut own = vec![Sysctl { ns: Ns::Net, path: "net/ipv4/ip_unprivileged_port_start".into(), value: "1024".into() }];
        add_net_defaults(&mut own);
        let keys: Vec<(&str, &str)> = own.iter().map(|s| (s.path.as_str(), s.value.as_str())).collect();
        assert_eq!(keys, [("net/ipv4/ping_group_range", "0 2147483647"), ("net/ipv4/ip_unprivileged_port_start", "1024")], "--sysctl wins");
    }

    #[test]
    fn parses_and_refuses() {
        let buf = list(&["rlimit\t7\t65536\t65536", "sysctl\tnet\tnet/core/somaxconn\t1024", "device\t/dev/fuse\t/dev/fuse\trwm", "devrule\tc\t10\t229\trwm", "group\tvideo"]);
        let kiri_proto::Msg::Retain(l) = kiri_proto::decode(&buf).unwrap() else { panic!() };
        let o = parse(l).unwrap();
        assert_eq!(o.rlimits, [(7, 65536, 65536)]);
        assert_eq!((o.sysctls[0].ns, o.sysctls[0].path.as_str()), (Ns::Net, "net/core/somaxconn"));
        assert_eq!(o.devices[0].container, "/dev/fuse");
        assert_eq!(o.rules.len(), 1);
        assert_eq!(o.groups, ["video"]);
        for bad in ["sysctl\tnet\tkernel/core_pattern\t|x", "sysctl\tipc\tkernel/../x\t1", "sysctl\tuts\tkernel/hostname\tx", "rlimit\t99\t1\t1", "device\tdev\t/x\trwm", "nope"] {
            let buf = list(&[bad]);
            let kiri_proto::Msg::Retain(l) = kiri_proto::decode(&buf).unwrap() else { panic!() };
            assert!(parse(l).is_err(), "{bad}");
        }
    }

    #[test]
    fn own_namespace_is_refused() {
        // A "container" that is this very process shares every namespace.
        let me = std::process::id() as libc::pid_t;
        let own = fs::File::open("/proc/self/ns/net").unwrap();
        assert!(same_namespace(&own, &fs::File::open(format!("/proc/{me}/ns/net")).unwrap()).unwrap());
        assert!(!same_namespace(&own, &fs::File::open("/proc/self/ns/ipc").unwrap()).unwrap());
        let s = Sysctl { ns: Ns::Net, path: "net/ipv4/ip_unprivileged_port_start".into(), value: "77".into() };
        let e = apply_sysctls(me, &[s]).unwrap_err();
        assert_eq!(e.to_string(), "sysctl \"net.ipv4.ip_unprivileged_port_start\" not allowed in host net namespace");
    }
}
