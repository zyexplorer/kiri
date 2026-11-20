//! Linux's files in /etc, for bind mounts on a Mac.
//!
//! Compose files and tools written for a Linux host mount /etc/passwd,
//! /etc/group, /etc/resolv.conf, /etc/os-release, /etc/machine-id and
//! /etc/timezone, and expect Linux's. The VM keeps next to nothing in /etc,
//! and the Mac's own are macOS's: its passwd has neither the person's
//! account (those are in Directory Services) nor Linux's `nobody`. So Kiri
//! keeps plain Linux ones in its data folder, written again whenever one is
//! mounted, and a mount of those paths gets them. /etc/hosts and
//! /etc/localtime are the Mac's own (see `api::mac_source`).

use std::io;
use std::path::{Path, PathBuf};

/// The files kept, by their name in /etc.
pub const FILES: &[&str] = &["passwd", "group", "resolv.conf", "os-release", "machine-id", "timezone"];

/// The person running Kiri, who gets a line in passwd and group: what
/// `--user $(id -u):$(id -g)` with a mounted /etc/passwd is for.
pub struct Who {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
}

impl Who {
    fn current() -> Who {
        // SAFETY: getuid and getgid take no arguments and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty() && !v.contains([':', '\n']));
        Who { user: var("USER").unwrap_or_else(|| "kiri".into()), uid, gid, home: var("HOME").unwrap_or_else(|| "/".into()) }
    }
}

/// Where the files are kept: `etc` in Kiri's data folder, which the VM
/// sees with the rest of the Mac.
pub fn dir() -> io::Result<PathBuf> {
    Ok(kiri_image::Store::default_root()?.join("etc"))
}

/// One of `FILES`, written as it should read now; its path.
pub fn file(name: &str) -> io::Result<PathBuf> {
    file_in(&dir()?, name, &Who::current())
}

/// All of them, for a mount of /etc itself; the folder.
pub fn all() -> io::Result<PathBuf> {
    let (dir, who) = (dir()?, Who::current());
    for name in FILES {
        file_in(&dir, name, &who)?;
    }
    // The VM's own name, as its /etc/hostname has it.
    put(&dir.join("hostname"), "kiri\n")?;
    Ok(dir)
}

fn file_in(dir: &Path, name: &str, who: &Who) -> io::Result<PathBuf> {
    let path = dir.join(name);
    let text = match name {
        // Made once and kept: a machine-id that changed would not be one.
        "machine-id" => match std::fs::read_to_string(&path) {
            Ok(id) if is_machine_id(&id) => return Ok(path),
            _ => format!("{}\n", super::engine::random_hex(16)),
        },
        "timezone" => format!("{}\n", zone_of(std::fs::read_link("/etc/localtime").ok().as_deref())),
        _ => contents(name, who).ok_or_else(|| io::Error::other(format!("no /etc/{name} is kept")))?,
    };
    put(&path, &text)?;
    Ok(path)
}

/// Writes `text` unless the file says it already, so a mount does not
/// change a file other containers have open for nothing.
fn put(path: &Path, text: &str) -> io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
}

fn is_machine_id(id: &str) -> bool {
    let id = id.trim_end_matches('\n');
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The zone's name from where /etc/localtime leads
/// (`/var/db/timezone/zoneinfo/Africa/Lagos`); UTC when it says nothing.
fn zone_of(link: Option<&Path>) -> String {
    link.and_then(Path::to_str).and_then(|l| l.split_once("zoneinfo/")).map(|(_, zone)| zone.to_owned()).filter(|z| !z.is_empty()).unwrap_or_else(|| "UTC".into())
}

/// What a file says, for those that depend on nothing on disk.
fn contents(name: &str, who: &Who) -> Option<String> {
    // The accounts every Linux has, with Debian's numbers, then the person.
    const USERS: &[(&str, u32, u32, &str)] =
        &[("root", 0, 0, "/root"), ("daemon", 1, 1, "/usr/sbin"), ("bin", 2, 2, "/bin"), ("sys", 3, 3, "/dev"), ("www-data", 33, 33, "/var/www"), ("nobody", 65534, 65534, "/nonexistent")];
    const GROUPS: &[(&str, u32)] = &[("root", 0), ("daemon", 1), ("bin", 2), ("sys", 3), ("tty", 5), ("www-data", 33), ("nogroup", 65534)];
    Some(match name {
        "passwd" => {
            let mut out = String::new();
            for (user, uid, gid, home) in USERS {
                let shell = if *uid == 0 { "/bin/sh" } else { "/usr/sbin/nologin" };
                out.push_str(&format!("{user}:x:{uid}:{gid}:{user}:{home}:{shell}\n"));
            }
            if !USERS.iter().any(|(u, id, ..)| *id == who.uid || *u == who.user) {
                out.push_str(&format!("{}:x:{}:{}:{}:{}:/bin/sh\n", who.user, who.uid, who.gid, who.user, who.home));
            }
            out
        }
        "group" => {
            let mut out = String::new();
            for (group, gid) in GROUPS {
                out.push_str(&format!("{group}:x:{gid}:\n"));
            }
            // The person's group, under the Mac's name for it (20 is staff).
            if !GROUPS.iter().any(|(_, id)| *id == who.gid) {
                let group = if who.gid == 20 { "staff" } else { who.user.as_str() };
                out.push_str(&format!("{group}:x:{}:{}\n", who.gid, who.user));
            }
            out
        }
        // The Mac, which answers DNS for every container network.
        "resolv.conf" => format!("nameserver {}\n", super::ports::HOST_ADDR),
        "os-release" => {
            let v = env!("CARGO_PKG_VERSION");
            format!("NAME=\"Kiri\"\nID=kiri\nID_LIKE=linux\nVERSION_ID=\"{v}\"\nPRETTY_NAME=\"Kiri {v}\"\n")
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Who {
        Who { user: "ada".into(), uid: 501, gid: 20, home: "/Users/ada".into() }
    }

    #[test]
    fn passwd_and_group_are_linuxs_with_the_person_added() {
        // Account lines only (name, ids, home, shell): no secrets are in
        // these files. The messages still leave their text out.
        let accounts = contents("passwd", &me()).unwrap();
        assert!(accounts.starts_with("root:x:0:0:root:/root:/bin/sh\n"), "root comes first");
        assert!(accounts.contains("\nnobody:x:65534:65534:"), "Linux's nobody is listed");
        assert!(accounts.ends_with("ada:x:501:20:ada:/Users/ada:/bin/sh\n"), "the person comes last");
        assert!(accounts.lines().all(|l| l.split(':').count() == 7), "every line has seven fields");
        let groups = contents("group", &me()).unwrap();
        assert!(groups.contains("root:x:0:\n") && groups.contains("nogroup:x:65534:\n"), "Linux's groups are listed");
        assert!(groups.ends_with("staff:x:20:ada\n"), "the person's group comes last");
        // root running Kiri (a Linux host's layout, or sudo) is not listed twice.
        let root = Who { user: "root".into(), uid: 0, gid: 0, home: "/root".into() };
        assert_eq!(contents("passwd", &root).unwrap().matches("\nroot:").count() + 1, 1);
        assert_eq!(contents("group", &root).unwrap().lines().filter(|l| l.split(':').nth(2) == Some("0")).count(), 1);
    }

    #[test]
    fn the_rest_say_what_a_linux_host_would() {
        assert_eq!(contents("resolv.conf", &me()).unwrap(), format!("nameserver {}\n", super::super::ports::HOST_ADDR));
        let os = contents("os-release", &me()).unwrap();
        assert!(os.contains("ID=kiri\n") && os.contains("PRETTY_NAME=\"Kiri "), "{os}");
        assert_eq!(contents("hosts", &me()), None);
        assert_eq!(zone_of(Some(Path::new("/var/db/timezone/zoneinfo/Africa/Lagos"))), "Africa/Lagos");
        assert_eq!(zone_of(Some(Path::new("/usr/share/zoneinfo/UTC"))), "UTC");
        assert_eq!(zone_of(Some(Path::new("/somewhere/else"))), "UTC");
        assert_eq!(zone_of(None), "UTC");
    }

    #[test]
    fn files_are_written_once_and_the_machine_id_is_kept() {
        let dir = std::env::temp_dir().join(format!("kiri-hostetc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for name in FILES {
            let path = file_in(&dir, name, &me()).unwrap();
            assert!(std::fs::metadata(&path).unwrap().len() > 0, "{name}");
        }
        let id = std::fs::read_to_string(dir.join("machine-id")).unwrap();
        assert!(is_machine_id(&id), "{id}");
        file_in(&dir, "machine-id", &me()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("machine-id")).unwrap(), id);
        // A container that changed a mounted file does not change the next one's.
        std::fs::write(dir.join("passwd"), "gone\n").unwrap();
        file_in(&dir, "passwd", &me()).unwrap();
        assert!(std::fs::read_to_string(dir.join("passwd")).unwrap().starts_with("root:x:0:0:"));
        assert!(file_in(&dir, "shadow", &me()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
