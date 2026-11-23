//! `kiri host-access <user>`: everything the window's onboarding on a Linux
//! host needs root for, in one go, so the system asks for a password once.
//! The window runs it through pkexec (the polkit action dev.kiri.host-access,
//! packaging/linux/dev.kiri.host-access.policy); `sudo kiri host-access $USER`
//! does the same from a terminal.
//!
//! 1. The `kiri` group, if the package's install didn't make it.
//! 2. `user` in it.
//! 3. The engine: started (and started at boot) if it isn't running,
//!    restarted if it is but doesn't answer.
//! 4. The socket's access list, with `user` on it (crate::daemon::access), so
//!    the change applies now, without signing in again.
//!
//! `--restart` restarts a running engine as well (the window's Restart
//! engine, after the package was upgraded), and waits for it to answer.
//!
//! Exit status: 0 done; 10 done, but this system keeps no access lists, so
//! `user` must log out and back in once before Kiri lets them in.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const GROUP: &str = "kiri";
const SOCKET: &str = crate::client::SERVICE_SOCKET;
/// The exit status that says: done, but sign in again before it applies.
pub const SIGN_IN_AGAIN: u8 = 10;

/// Trusted locations for system administration binaries invoked as root.
const SYSTEM_DIRS: &[&str] = &["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/run/current-system/sw/bin"];
const SAFE_PATH: &str = "/usr/sbin:/sbin:/usr/bin:/bin:/run/current-system/sw/bin";

fn find_in(dirs: &[&Path], name: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| p.is_file())
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let dirs: Vec<&Path> = SYSTEM_DIRS.iter().map(Path::new).collect();
    find_in(&dirs, name)
}

fn tool_cmd(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear().env("PATH", SAFE_PATH).env("LC_ALL", "C");
    cmd
}

fn format_failure(what: &str, status: ExitStatus, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr).trim().to_owned();
    if detail.is_empty() { format!("{what} failed ({status})") } else { format!("{what} failed ({status}): {detail}") }
}

fn add_member_with(user: &str, group: &str, usermod: Option<&Path>, gpasswd: Option<&Path>) -> io::Result<()> {
    let mut usermod_err = None;
    // usermod everywhere we package for; gpasswd where it isn't (and for
    // directory accounts not in /etc/passwd, which usermod refuses).
    if let Some(prog) = usermod {
        match tool_cmd(prog).args(["-aG", group, "--", user]).stdin(Stdio::null()).output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => usermod_err = Some(format_failure("usermod", out.status, &out.stderr)),
            Err(e) => usermod_err = Some(format!("usermod: {e}")),
        }
    }
    if let Some(prog) = gpasswd {
        match tool_cmd(prog).args(["-a", user, group]).stdin(Stdio::null()).output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                let err = format_failure("gpasswd", out.status, &out.stderr);
                return Err(io::Error::other(match usermod_err {
                    Some(u) => format!("{err} (after {u})"),
                    None => err,
                }));
            }
            Err(e) => {
                let err = format!("gpasswd: {e}");
                return Err(io::Error::other(match usermod_err {
                    Some(u) => format!("{err} (after {u})"),
                    None => err,
                }));
            }
        }
    }
    Err(io::Error::other(usermod_err.unwrap_or_else(|| "neither usermod nor gpasswd found in system directories".into())))
}

#[allow(clippy::too_many_arguments)]
fn run_with(
    user: &str,
    group: &str,
    resolve_account: impl Fn(&str) -> io::Result<Option<String>>,
    list_members: impl Fn(&str) -> io::Result<Vec<String>>,
    create_group: impl Fn(&str) -> io::Result<()>,
    add_member: impl Fn(&str, &str) -> io::Result<()>,
    start_engine: impl Fn() -> io::Result<()>,
    grant_acl: impl Fn(&str) -> io::Result<usize>,
    mut emit: impl FnMut(&str),
) -> io::Result<u8> {
    let Some(account) = resolve_account(user)? else {
        return Err(io::Error::other(format!("no account named {user}")));
    };
    match list_members(group) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_group(group)?,
        Err(e) => return Err(e),
    }
    if !list_members(group)?.iter().any(|m| m == &account) {
        add_member(&account, group)?;
        emit(&format!("{user} is in the {group} group"));
    }
    start_engine()?;
    match grant_acl(group) {
        Ok(_) => {
            emit(&format!("{user} can use Kiri now"));
            Ok(0)
        }
        Err(e) if crate::daemon::access::unsupported(&e) => {
            emit(&format!("{user} can use Kiri after logging out and back in (this system keeps no access lists)"));
            Ok(SIGN_IN_AGAIN)
        }
        Err(e) if crate::daemon::access::acl_too_large(&e) => {
            emit(&format!("{user} can use Kiri after logging out and back in (the {group} group has too many members for the socket access list)"));
            Ok(SIGN_IN_AGAIN)
        }
        Err(e) => Err(io::Error::other(format!("couldn't give {user} the socket: {e}"))),
    }
}

pub fn run(args: &[String]) -> io::Result<u8> {
    let (restart, args) = match args {
        [flag, rest @ ..] if flag == "--restart" => (true, rest),
        _ => (false, args),
    };
    let [user] = args else { return Err(io::Error::other("usage: kiri host-access [--restart] USER")) };
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other(format!("needs root: sudo kiri host-access {}{user}", if restart { "--restart " } else { "" })));
    }
    run_with(
        user,
        GROUP,
        crate::daemon::access::account_name,
        crate::daemon::access::members,
        |group| {
            let groupadd = find_tool("groupadd").ok_or_else(|| io::Error::other("groupadd not found in system directories"))?;
            must_output(tool_cmd(&groupadd).args(["--system", group]), "groupadd")
        },
        |member, group| add_member_with(member, group, find_tool("usermod").as_deref(), find_tool("gpasswd").as_deref()),
        || engine(restart),
        |group| crate::daemon::access::grant(Path::new(SOCKET), group),
        |line| println!("{line}"),
    )
}

/// The engine running and answering on its socket; with `restart`, a
/// running one restarted (on the binary the package installed).
fn engine(restart: bool) -> io::Result<()> {
    if answers() && !restart {
        return Ok(());
    }
    let systemctl = find_tool("systemctl").ok_or_else(|| io::Error::other("systemctl not found in system directories"))?;
    let active = tool_cmd(&systemctl).args(["is-active", "--quiet", "kiri"]).stdin(Stdio::null()).status().is_ok_and(|s| s.success());
    if active {
        must(tool_cmd(&systemctl).args(["restart", "kiri"]), "systemctl restart kiri")?;
    } else {
        must(tool_cmd(&systemctl).args(["enable", "--now", "kiri"]), "systemctl enable --now kiri")?;
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if answers() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(io::Error::other("Kiri's engine didn't start: journalctl -u kiri says why"))
}

fn answers() -> bool {
    UnixStream::connect(SOCKET).is_ok()
}

fn must(cmd: &mut Command, what: &str) -> io::Result<()> {
    let status = cmd.status().map_err(|e| io::Error::other(format!("{what}: {e}")))?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("{what} failed ({status})"))) }
}

fn must_output(cmd: &mut Command, what: &str) -> io::Result<()> {
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| io::Error::other(format!("{what}: {e}")))?;
    if out.status.success() { Ok(()) } else { Err(io::Error::other(format_failure(what, out.status, &out.stderr))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn make_temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("kiri-hostaccess-{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn hostaccess_resolves_tools_from_trusted_dirs_only() {
        let dir = make_temp_dir("find");
        let untrusted = dir.join("untrusted");
        let trusted1 = dir.join("trusted1");
        let trusted2 = dir.join("trusted2");
        std::fs::create_dir_all(&untrusted).unwrap();
        std::fs::create_dir_all(&trusted1).unwrap();
        std::fs::create_dir_all(&trusted2).unwrap();
        write_script(&untrusted, "usermod", "#!/bin/sh\nexit 0\n");
        let expected = write_script(&trusted2, "usermod", "#!/bin/sh\nexit 0\n");

        assert_eq!(find_in(&[trusted1.as_path(), trusted2.as_path()], "usermod"), Some(expected));
        assert_eq!(find_in(&[trusted1.as_path()], "usermod"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hostaccess_usermod_uses_clean_env_option_terminator_and_surfaces_stderr() {
        let dir = make_temp_dir("usermod");
        let env_dump = dir.join("env");
        let args_dump = dir.join("args");
        let usermod_ok = write_script(
            &dir,
            "usermod-ok",
            &format!(
                "#!/bin/sh\n\
                 export -p > \"{}\"\n\
                 printf '%s\\n' \"$@\" > \"{}\"\n\
                 exit 0\n",
                env_dump.display(),
                args_dump.display(),
            ),
        );
        add_member_with("alice", "kiri", Some(&usermod_ok), None).unwrap();
        assert_eq!(std::fs::read_to_string(&args_dump).unwrap(), "-aG\nkiri\n--\nalice\n");
        let env_text = std::fs::read_to_string(&env_dump).unwrap();
        let mut saw_path = false;
        let mut saw_lc_all = false;
        for line in env_text.lines() {
            let rest = line.strip_prefix("export ").unwrap_or(line);
            let (k, v) = rest.split_once('=').unwrap_or((rest, ""));
            match k {
                "PATH" => {
                    assert!(v.contains("/usr/sbin:/sbin:/usr/bin:/bin"), "unexpected PATH: {line}");
                    saw_path = true;
                }
                "LC_ALL" => {
                    assert!(v == "'C'" || v == "C" || v == "\"C\"", "unexpected LC_ALL: {line}");
                    saw_lc_all = true;
                }
                "PWD" | "SHLVL" | "_" => {}
                _ => panic!("unexpected inherited env var in usermod: {line}"),
            }
        }
        assert!(saw_path && saw_lc_all, "missing PATH or LC_ALL in {env_text}");

        let usermod_locked = write_script(&dir, "usermod-locked", "#!/bin/sh\necho 'cannot lock /etc/group' >&2\nexit 10\n");
        let gpasswd_ok = write_script(&dir, "gpasswd-ok", "#!/bin/sh\n[ \"$1\" = '-a' ] && [ \"$2\" = 'alice' ] && [ \"$3\" = 'kiri' ]\n");
        let gpasswd_fail = write_script(&dir, "gpasswd-fail", "#!/bin/sh\necho 'read-only /etc/group' >&2\nexit 1\n");

        // On hosts without gpasswd, usermod stderr is surfaced in the error.
        let err = add_member_with("alice", "kiri", Some(&usermod_locked), None).unwrap_err().to_string();
        assert!(err.contains("cannot lock /etc/group"), "missing usermod stderr: {err}");

        // When gpasswd succeeds (e.g. directory account), add succeeds cleanly.
        add_member_with("alice", "kiri", Some(&usermod_locked), Some(&gpasswd_ok)).unwrap();

        // When both fail, both diagnostics are surfaced.
        let err = add_member_with("alice", "kiri", Some(&usermod_locked), Some(&gpasswd_fail)).unwrap_err().to_string();
        assert!(err.contains("read-only /etc/group") && err.contains("cannot lock /etc/group"), "missing diagnostics: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hostaccess_prints_requested_user_while_adding_canonical_name() {
        use std::sync::Mutex;

        let added = Mutex::new(Vec::<(String, String)>::new());
        let mut lines = Vec::<String>::new();
        let status = run_with(
            "KCanon349",
            "kiri",
            |_| Ok(Some("kcanon349@corp.example".into())),
            |_| Ok(vec![]),
            |_| Ok(()),
            |member, group| {
                added.lock().unwrap().push((member.to_owned(), group.to_owned()));
                Ok(())
            },
            || Ok(()),
            |_| Ok(1),
            |line| lines.push(line.to_owned()),
        )
        .unwrap();
        assert_eq!(status, 0);
        assert_eq!(*added.lock().unwrap(), vec![("kcanon349@corp.example".into(), "kiri".into())]);
        assert_eq!(lines, vec!["KCanon349 is in the kiri group", "KCanon349 can use Kiri now"]);

        for (err_no, expected_substr) in [(libc::EOPNOTSUPP, "keeps no access lists"), (libc::ENOSPC, "too many members")] {
            let mut out = Vec::<String>::new();
            let code = run_with(
                "KCanon349",
                "kiri",
                |_| Ok(Some("kcanon349@corp.example".into())),
                |_| Ok(vec!["kcanon349@corp.example".into()]),
                |_| Ok(()),
                |_, _| panic!("already a member"),
                || Ok(()),
                |_| Err(io::Error::from_raw_os_error(err_no)),
                |line| out.push(line.to_owned()),
            )
            .unwrap();
            assert_eq!(code, SIGN_IN_AGAIN);
            assert_eq!(out.len(), 1);
            assert!(out[0].starts_with("KCanon349 can use Kiri after logging out"), "got: {}", out[0]);
            assert!(out[0].contains(expected_substr), "got: {}", out[0]);
            assert!(!out[0].contains("kcanon349@corp.example"), "must not print resolved record: {}", out[0]);
        }

        let err = run_with(
            "KCanon349",
            "kiri",
            |_| Ok(Some("kcanon349@corp.example".into())),
            |_| Ok(vec!["kcanon349@corp.example".into()]),
            |_| Ok(()),
            |_, _| Ok(()),
            || Ok(()),
            |_| Err(io::Error::other("boom")),
            |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("KCanon349") && !err.contains("kcanon349@corp.example"), "got: {err}");
    }
}
