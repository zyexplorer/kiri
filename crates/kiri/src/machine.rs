//! `kiri machine`: long-lived Linux machines, as OrbStack has. A machine is
//! a container that keeps running, from a distro image, with the host user's
//! name and uid inside (and passwordless sudo), and the homes folder
//! (`/Users` on a Mac, `/home` on Linux) at the same path as on the host.
//! `kiri machine shell` opens a login shell in it. On Windows the commands
//! run as root in Kiri's WSL distro (crates/kiri/src/wsl.rs), so the
//! machine's user is root there.

use std::io;
use std::process::Command;

use serde_json::json;

use crate::client;

const LABEL: &str = "dev.kiri.machine";

/// `kiri machine create NAME --distro ubuntu` picks one of these.
const DISTROS: [(&str, &str); 7] = [
    ("ubuntu", "ubuntu:24.04"),
    ("debian", "debian:trixie"),
    ("alpine", "alpine:3.22"),
    ("fedora", "fedora:42"),
    ("arch", "archlinux:latest"),
    ("rocky", "rockylinux:9"),
    ("opensuse", "opensuse/tumbleweed:latest"),
];

fn container(name: &str) -> String {
    format!("machine-{name}")
}

fn ok(r: io::Result<client::Response>) -> io::Result<client::Response> {
    let r = r?;
    if r.ok() { Ok(r) } else { Err(io::Error::other(r.error())) }
}

pub fn run(args: &[String]) -> io::Result<()> {
    if !client::alive() {
        return Err(io::Error::other("kiri daemon is not running: open Kiri.app or run `kiri daemon`"));
    }
    let name = args.get(1).map(String::as_str).unwrap_or_default();
    let need_name = || if name.is_empty() { Err(io::Error::other("name a machine")) } else { Ok(()) };
    match args.first().map(String::as_str) {
        Some("create") => {
            need_name()?;
            let pick = args.iter().position(|a| a == "--distro" || a == "--image").and_then(|i| args.get(i + 1)).map_or("ubuntu", String::as_str);
            create(name, pick, args.iter().any(|a| a == "--systemd"))
        }
        Some("list" | "ls") => list(),
        Some("shell") => {
            need_name()?;
            let cmd: Vec<&str> = args.iter().position(|a| a == "--").map(|i| args[i + 1..].iter().map(String::as_str).collect()).unwrap_or_default();
            shell(name, &cmd)
        }
        Some(verb @ ("start" | "stop" | "restart")) => {
            need_name()?;
            // Already running, or already stopped: Docker's 304, with no
            // body, is done (it printed an empty `kiri: ` and failed).
            let r = client::request("POST", &format!("/containers/{}/{verb}", container(name)), None)?;
            if !r.ok() && r.status != 304 {
                return Err(io::Error::other(r.error()));
            }
            Ok(())
        }
        Some("delete" | "rm") => {
            need_name()?;
            ok(client::request("DELETE", &format!("/containers/{}?force=1&v=1", container(name)), None))?;
            println!("deleted machine {name}");
            Ok(())
        }
        _ => Err(io::Error::other(
            "usage: kiri machine create NAME [--distro ubuntu|debian|alpine|fedora|arch|rocky|opensuse | --image IMAGE] [--systemd]\n       kiri machine list | shell NAME [-- CMD...] | start | stop | restart | delete NAME",
        )),
    }
}

/// The host's homes folder, mounted at the same path in every machine.
const HOMES: &str = if cfg!(target_os = "macos") { "/Users" } else { "/home" };

/// The host user's name, uid and gid. root is always `root`: under
/// `wsl -u root` (Windows) USER is not set, and a made-up name with uid 0
/// was a user no machine had ("unknown user user").
fn user() -> (String, u32, u32) {
    // SAFETY: getuid/getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let name = if uid == 0 { "root".to_owned() } else { std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "user".to_owned()) };
    (name, uid, gid)
}

/// The user's home in the machine: under HOMES, as on the host; root's is /root.
fn home(user: &str) -> String {
    if user == "root" { "/root".to_owned() } else { format!("{HOMES}/{user}") }
}

fn create(name: &str, pick: &str, systemd: bool) -> io::Result<()> {
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || name.starts_with('-') {
        return Err(io::Error::other("a machine name is letters, digits and dashes"));
    }
    let image = DISTROS.iter().find(|(d, _)| *d == pick).map_or(pick, |(_, i)| *i);
    let (repo, tag) = match image.rsplit_once(':') {
        Some((r, t)) if !t.contains('/') => (r, t),
        _ => (image, "latest"),
    };
    println!("pulling {repo}:{tag}…");
    let pulled = ok(client::request("POST", &format!("/images/create?fromImage={repo}&tag={tag}"), None))?;
    if let Some(e) = String::from_utf8_lossy(&pulled.body).lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).find_map(|v| v["error"].as_str().map(str::to_owned)) {
        return Err(io::Error::other(e));
    }
    let (user, uid, gid) = user();
    let home = home(&user);
    // The host user, made once; every distro has one of these tools. An
    // image user that already has the uid (Ubuntu's `ubuntu` is 1000, the
    // first user's uid on most Linux hosts) is renamed to the host user.
    let init = format!(
        r#"set -e
if ! id -u {user} >/dev/null 2>&1; then
  getent group {gid} >/dev/null 2>&1 || groupadd -g {gid} {user} 2>/dev/null || addgroup -g {gid} {user} 2>/dev/null || true
  g=$(getent group {gid} | cut -d: -f1)
  old=$(getent passwd {uid} | cut -d: -f1)
  {{ [ -n "$old" ] && usermod -l {user} -d {home} "$old" 2>/dev/null; }} || useradd -o -M -u {uid} -g "${{g:-{gid}}}" -d {home} -s /bin/sh {user} 2>/dev/null || adduser -D -H -u {uid} -G "${{g:-{user}}}" -h {home} {user} 2>/dev/null || true
fi
mkdir -p /etc/sudoers.d && echo '{user} ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/kiri && chmod 440 /etc/sudoers.d/kiri
# Slim images have no sudo: fetch it once, in the background.
command -v sudo >/dev/null 2>&1 || (apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq sudo || apk add -q sudo || dnf -y -q install sudo || pacman -Sy --noconfirm sudo || zypper -n -q in sudo) >/dev/null 2>&1 &
{tail}"#,
        tail = if systemd {
            // A full init: systemd as PID 1 (installed first if the image lacks it).
            "wait\nfor i in /lib/systemd/systemd /usr/lib/systemd/systemd; do [ -x $i ] && exec $i; done\n(apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq systemd systemd-sysv dbus || dnf -y -q install systemd || pacman -Sy --noconfirm systemd || zypper -n -q in systemd) >/dev/null 2>&1\nfor i in /lib/systemd/systemd /usr/lib/systemd/systemd; do [ -x $i ] && exec $i; done\necho 'no systemd for this distro' >&2; exit 1"
        } else {
            "trap 'exit 0' TERM INT\nwhile :; do sleep 3600 & wait $!; done"
        }
    );
    let mut body = json!({
        "Image": format!("{repo}:{tag}"),
        "Cmd": ["/bin/sh", "-c", init],
        "Hostname": name,
        "Labels": {LABEL: name},
        "HostConfig": {
            "Binds": [format!("{HOMES}:{HOMES}")],
            "RestartPolicy": {"Name": "unless-stopped"},
            "CapAdd": ["NET_ADMIN", "SYS_PTRACE"],
        },
    });
    if systemd {
        // systemd needs its own cgroup tree, tmpfs /run and /tmp, and stops
        // on SIGRTMIN+3.
        body["StopSignal"] = "SIGRTMIN+3".into();
        body["Env"] = json!(["container=docker"]);
        body["HostConfig"]["Privileged"] = true.into();
        body["HostConfig"]["Tmpfs"] = json!({"/run": "", "/run/lock": "", "/tmp": ""});
    }
    ok(client::request("POST", &format!("/containers/create?name={}", container(name)), Some(&body)))?;
    ok(client::request("POST", &format!("/containers/{}/start", container(name)), None))?;
    // The user is made by the machine's own first command: wait for it, or
    // a `machine shell` straight after was "unknown user" (seen on Fedora).
    let wait = format!("for i in $(seq 300); do id -u {user} >/dev/null 2>&1 && exit 0; sleep 0.1; done; exit 1");
    if client::exec(&container(name), &["sh", "-c", &wait])?.0 != 0 {
        return Err(io::Error::other(format!("machine {name} is running, but the user {user} could not be added to it")));
    }
    println!("machine {name} ({image}) is running: kiri machine shell {name}");
    Ok(())
}

fn list() -> io::Result<()> {
    let filters = format!("{{\"label\":[\"{LABEL}\"]}}");
    let rows = client::get(&format!("/containers/json?all=1&filters={}", urlish(&filters)))?;
    println!("{:<16} {:<28} {}", "NAME", "IMAGE", "STATE");
    for c in rows.as_array().into_iter().flatten() {
        println!("{:<16} {:<28} {}", c["Labels"][LABEL].as_str().unwrap_or("?"), c["Image"].as_str().unwrap_or(""), c["Status"].as_str().unwrap_or(""));
    }
    Ok(())
}

fn urlish(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A login shell as the host user, in the same directory as on the host when
/// it is under HOMES, else in the user's home. The docker CLI handles the
/// terminal.
fn shell(name: &str, cmd: &[&str]) -> io::Result<()> {
    let docker = client::docker_cli().ok_or_else(|| io::Error::other("no docker CLI found (Kiri.app bundles one)"))?;
    let (user, _, _) = user();
    let home = home(&user);
    let cwd = std::env::current_dir().ok().filter(|d| d.starts_with(HOMES)).map_or_else(|| home.clone(), |d| d.display().to_string());
    let mut c = Command::new(docker);
    // The engine the other machine commands use (client::socket(), so
    // KIRI_SOCKET too), as `kiri exec` does, not a docker context: with
    // `--context kiri`, a second Kiri's machines (or any, once that context
    // was removed or pointed elsewhere) were "No such container".
    c.env("DOCKER_HOST", format!("unix://{}", client::socket()?.display())).env_remove("DOCKER_CONTEXT");
    // SAFETY: isatty has no preconditions.
    let tty = unsafe { libc::isatty(0) } == 1;
    c.args(["exec", if tty { "-it" } else { "-i" }, "-u", &user, "-w", &cwd, "-e", &format!("HOME={home}"), &container(name)]);
    if cmd.is_empty() {
        // bash when the distro has it, else its sh.
        c.args(["sh", "-c", "exec $(command -v bash || command -v sh) -l"]);
    } else {
        c.args(cmd);
    }
    let status = c.status()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
