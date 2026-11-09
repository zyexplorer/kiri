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
//! Exit status: 0 done; 10 done, but this system keeps no access lists, so
//! `user` must log out and back in once before Kiri lets them in.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const GROUP: &str = "kiri";
const SOCKET: &str = crate::client::SERVICE_SOCKET;
/// The exit status that says: done, but sign in again before it applies.
pub const SIGN_IN_AGAIN: u8 = 10;

pub fn run(args: &[String]) -> io::Result<u8> {
    let [user] = args else {
        return Err(io::Error::other("usage: kiri host-access USER"));
    };
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other(format!(
            "needs root: sudo kiri host-access {user}"
        )));
    }
    if crate::daemon::access::uid(user).is_none() {
        return Err(io::Error::other(format!("no account named {user}")));
    }
    if crate::daemon::access::members(GROUP).is_err() {
        must(
            Command::new("groupadd").args(["--system", GROUP]),
            "groupadd",
        )?;
    }
    if !crate::daemon::access::members(GROUP)?
        .iter()
        .any(|m| m == user)
    {
        // usermod everywhere we package for; gpasswd where it isn't.
        let added = Command::new("usermod")
            .args(["-aG", GROUP, user])
            .status()
            .is_ok_and(|s| s.success());
        if !added {
            must(Command::new("gpasswd").args(["-a", user, GROUP]), "gpasswd")?;
        }
        println!("{user} is in the {GROUP} group");
    }
    engine()?;
    match crate::daemon::access::grant(Path::new(SOCKET), GROUP) {
        Ok(_) => {
            println!("{user} can use Kiri now");
            Ok(0)
        }
        Err(e) if crate::daemon::access::unsupported(&e) => {
            println!(
                "{user} can use Kiri after logging out and back in (this system keeps no access lists)"
            );
            Ok(SIGN_IN_AGAIN)
        }
        Err(e) => Err(io::Error::other(format!(
            "couldn't give {user} the socket: {e}"
        ))),
    }
}

/// The engine running and answering on its socket.
fn engine() -> io::Result<()> {
    if answers() {
        return Ok(());
    }
    let active = Command::new("systemctl")
        .args(["is-active", "--quiet", "kiri"])
        .status()
        .is_ok_and(|s| s.success());
    if active {
        must(
            Command::new("systemctl").args(["restart", "kiri"]),
            "systemctl restart kiri",
        )?;
    } else {
        must(
            Command::new("systemctl").args(["enable", "--now", "kiri"]),
            "systemctl enable --now kiri",
        )?;
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if answers() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(io::Error::other(
        "Kiri's engine didn't start: journalctl -u kiri says why",
    ))
}

fn answers() -> bool {
    UnixStream::connect(SOCKET).is_ok()
}

fn must(cmd: &mut Command, what: &str) -> io::Result<()> {
    let status = cmd
        .status()
        .map_err(|e| io::Error::other(format!("{what}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{what} failed ({status})")))
    }
}
