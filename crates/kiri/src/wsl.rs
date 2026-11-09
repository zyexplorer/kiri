//! `kiri k8s`, `kiri machine` and `kiri diag` on Windows: the engine's own
//! commands, run by the Linux `kiri` in the WSL distro Kiri's engine lives
//! in, as the engine runs (root, with its HOME), so it finds the daemon's
//! socket. Arguments go through as they are: Windows paths are not
//! translated.

use std::io;
use std::path::PathBuf;
use std::process::Command;

/// The commands passed to the engine in WSL.
pub const COMMANDS: &[&str] = &["k8s", "machine", "diag"];

/// In Kiri Linux, kiri-start wakes the engine if it isn't up; in a distro
/// of the user's own, systemd or wsl.conf runs it.
const SCRIPT: &str = r#"export HOME=/var/lib/kiri-daemon; [ -x /usr/local/bin/kiri-start ] && /usr/local/bin/kiri-start >/dev/null 2>&1; exec /usr/local/bin/kiri "$@""#;

/// Runs `kiri <args>` in WSL and exits with its exit code.
pub fn run(args: &[String]) -> io::Result<()> {
    let distro = distro().ok_or_else(|| {
        io::Error::other("Kiri isn't set up yet. Open Kiri from the Start menu first")
    })?;
    // wsl.exe from System32, never one found in the current folder.
    let system =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let status = Command::new(system.join("System32").join("wsl.exe"))
        .args([
            "-d", &distro, "-u", "root", "-e", "sh", "-c", SCRIPT, "kiri",
        ])
        .args(args)
        .status()
        .map_err(|e| io::Error::other(format!("could not run wsl.exe ({e})")))?;
    std::process::exit(status.code().unwrap_or(1))
}

/// The distro setup put the engine in (crates/kiri-win/src/setup.rs).
fn distro() -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Kiri");
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("setup.json")).ok()?).ok()?;
    v["distro"].as_str().map(str::to_owned)
}
