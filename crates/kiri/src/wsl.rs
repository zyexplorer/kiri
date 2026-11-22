//! `kiri k8s`, `kiri machine` and `kiri diag` on Windows: the engine's own
//! commands, run by the Linux `kiri` in the WSL distro Kiri's engine lives
//! in, as the engine runs (root, with its HOME), so it finds the daemon's
//! socket. Arguments go through as they are: Windows paths are not
//! translated. `kiri machine shell` is the exception: it needs a terminal
//! and a docker CLI, so it runs on Windows (machine_shell below).

use std::io;
use std::path::PathBuf;
use std::process::Command;

/// The commands passed to the engine in WSL.
pub const COMMANDS: &[&str] = &["k8s", "machine", "diag", "usb"];

/// In Kiri Linux, kiri-start wakes the engine if it isn't up; in a distro
/// of the user's own, systemd or wsl.conf runs it.
const SCRIPT: &str = r#"export HOME=/var/lib/kiri-daemon; [ -x /usr/local/bin/kiri-start ] && /usr/local/bin/kiri-start >/dev/null 2>&1; exec /usr/local/bin/kiri "$@""#;

/// Runs `kiri <args>` in WSL and exits with its exit code.
pub fn run(args: &[String]) -> io::Result<()> {
    if let [m, verb, name, rest @ ..] = args
        && m == "machine"
        && verb == "shell"
    {
        return machine_shell(name, rest);
    }
    let distro = distro().ok_or_else(|| io::Error::other("Kiri isn't set up yet. Open Kiri from the Start menu first"))?;
    // wsl.exe from System32, never one found in the current folder.
    let system = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let status = Command::new(system.join("System32").join("wsl.exe"))
        .args(["-d", &distro, "-u", "root", "-e", "sh", "-c", SCRIPT, "kiri"])
        .args(args)
        .status()
        .map_err(|e| io::Error::other(format!("could not run wsl.exe ({e})")))?;
    std::process::exit(status.code().unwrap_or(1))
}

/// The distro setup put the engine in (crates/kiri-win/src/setup.rs).
fn distro() -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Kiri");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("setup.json")).ok()?).ok()?;
    v["distro"].as_str().map(str::to_owned)
}

/// `kiri machine shell NAME [-- CMD...]` on Windows: Kiri's docker.exe runs
/// the exec, with the console as its terminal, as `kiri exec` does
/// (crates/kiri/src/docker.rs). Kiri's WSL distro has no docker CLI, so the
/// Linux `kiri` could not open it there. The machine was made by root in
/// WSL, so the shell is root's, in /root.
fn machine_shell(name: &str, rest: &[String]) -> io::Result<()> {
    use std::io::IsTerminal;
    let tty = if std::io::stdin().is_terminal() { "-it" } else { "-i" };
    let mut args: Vec<String> = ["exec", tty, "-u", "root", "-w", "/root", "-e", "HOME=/root"].map(str::to_owned).to_vec();
    args.push(format!("machine-{name}"));
    match rest {
        [dd, cmd @ ..] if dd == "--" && !cmd.is_empty() => args.extend(cmd.iter().cloned()),
        // bash when the distro has it, else its sh.
        _ => args.extend(["sh", "-c", "exec $(command -v bash || command -v sh) -l"].map(str::to_owned)),
    }
    crate::docker::run(&args)
}
