//! `kiri ui`: a tiny native window over `kiri daemon`.
//!
//! The system's own WebKit in a native window: AppKit through objc2 on the
//! Mac (`mac.rs`), GTK on Linux (`gtk.rs`). No UI framework and no bundled
//! browser. The page is a single embedded HTML file
//! with hand-written CSS. It sends JSON commands with
//! `webkit.messageHandlers.kiri.postMessage('{"cmd":"start"}')`, and the
//! engine thread pushes JSON events back with `kiri.on({...})`.

use std::fmt::Write as _;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

// The window itself, per system; everything else here is shared.
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
use mac as window;
#[cfg(target_os = "linux")]
mod gtk;
#[cfg(target_os = "macos")]
use crate::{import, setup, update};
#[cfg(target_os = "linux")]
use gtk as window;
pub use window::run;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux::{import, setup, update};

mod assemble;
mod brands;
mod detail;
mod hub;
mod k8s;
mod lastlogs;
mod logins;
mod needs;
mod notes;
mod recheck;
mod sharing;
#[cfg(unix)]
mod term;
mod termwire;
mod usage;

use crate::client;
use crate::diag;

const PAGE: &str = include_str!("index.html");

/// How often the VM panel's figures refresh. The lists refresh on events.
const TICK: Duration = Duration::from_secs(2);
/// The lists also come again this often without an event, so "Up 5 min" and
/// "3 min ago" keep counting. Background requests: they don't keep the VM awake.
const LIST_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
enum Cmd {
    /// Make sure the daemon runs and the VM is awake.
    Start,
    /// Stop every container and let the VM sleep.
    Stop,
    /// The window opened: make sure the engine service runs, so docker and
    /// the lists work, without waking the VM. It stays asleep (Ready) until
    /// something needs it; only Start wakes it on purpose.
    Connect,
    Quit,
    /// Something changed: send the lists again.
    Refresh,
    Pull(String),
    /// An image, after the stopped containers made from it (their ids).
    RemoveImage(String, Vec<String>),
    /// Run an image detached with its own command (window Run / custom paste).
    Run(RunArgs),
    /// `start`, `stop`, `restart`, `pause`, `unpause` or `remove`.
    Container {
        action: String,
        id: String,
    },
    RemoveVolume(String),
    RemoveNetwork(String),
    /// Settings → About: the engine's details.
    About,
    /// Settings → Domains: turn `*.kiri.local` on or off (asks for the password).
    Domains(bool),
    /// New published ports for a container: `[{container_port, host_port, proto, host_ip}]`.
    Ports {
        id: String,
        ports: serde_json::Value,
    },
    Env {
        id: String,
        env: Vec<String>,
    },
}

impl Cmd {
    /// What an action is called in the log (and its op id), and what it acts on.
    fn op(&self) -> Option<(&str, String)> {
        Some(match self {
            Cmd::Start => ("start", String::new()),
            Cmd::Stop => ("stop", String::new()),
            Cmd::Pull(name) => ("pull", name.clone()),
            Cmd::RemoveImage(name, _) => ("rmi", name.clone()),
            Cmd::Run(a) => ("run", a.image.clone()),
            Cmd::Container { action, id } => (action.as_str(), id.chars().take(12).collect()),
            Cmd::RemoveVolume(name) => ("rmvol", name.clone()),
            Cmd::RemoveNetwork(id) => ("rmnet", id.chars().take(12).collect()),
            Cmd::Ports { id, .. } => ("ports", id.chars().take(12).collect()),
            Cmd::Env { id, .. } => ("env", id.chars().take(12).collect()),
            Cmd::Domains(on) => ("domains", on.to_string()),
            Cmd::Quit | Cmd::Refresh | Cmd::About | Cmd::Connect => return None,
        })
    }
}

/// A command as the page sends it.
#[derive(serde::Deserialize)]
struct PageCmd {
    cmd: String,
    #[serde(default)]
    name: String,
    /// A container's ID, or a share's number: the page sends either as text
    /// or as a number.
    #[serde(default, deserialize_with = "text_or_number")]
    id: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    ports: serde_json::Value,
    /// A command line (`exec`) or a path in a container (`files`, `preview`, `reveal`).
    #[serde(default)]
    path: String,
    /// A setting's value (`setpref`).
    #[serde(default)]
    value: serde_json::Value,
    /// Optional container name for `run` (`docker run --name`).
    #[serde(default)]
    container: String,
    /// Optional host port for `run` (`docker run -p`). `0` means unset.
    #[serde(default)]
    port: u64,
    /// Optional command for `run` (`docker run IMAGE …`). Empty keeps the
    /// image's `CMD`.
    #[serde(default)]
    command: String,
    /// Container port from a pasted `-p host:container` (0 = unset).
    #[serde(default)]
    target_port: u64,
    /// Bind mounts from a pasted `-v` / `--volume`.
    #[serde(default)]
    binds: Vec<String>,
    /// `--entrypoint` from a pasted docker run (empty keeps the image's).
    #[serde(default)]
    entrypoint: String,
    /// `--restart` policy name from a pasted docker run.
    #[serde(default)]
    restart: String,
    /// Full pasted `docker run` line for `Labels["kiri.start.command"]`.
    #[serde(default)]
    start_command: String,
    /// `--network` / `--net` from a pasted docker run (e.g. `host`).
    #[serde(default)]
    network: String,
}

/// What the window's Run sends to create and start a container.
#[derive(Debug, Clone)]
struct RunArgs {
    image: String,
    env: Vec<String>,
    container: String,
    host_port: Option<u16>,
    /// From a custom `-p host:container`; None keeps `primary_exposed`.
    target_port: Option<u16>,
    command: String,
    binds: Vec<String>,
    entrypoint: String,
    restart: String,
    start_command: String,
    network: String,
}

fn text_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    use serde::Deserialize;
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    })
}

/// What the menu bar menu shows, as of the engine thread's last refresh.
struct MenuState {
    vm: &'static str,
    /// Running containers: id, name, local address when it has a web port.
    running: Vec<(String, String, Option<String>)>,
}

static MENU: std::sync::Mutex<MenuState> = std::sync::Mutex::new(MenuState {
    vm: "Not running",
    running: Vec::new(),
});

fn menu_state() -> std::sync::MutexGuard<'static, MenuState> {
    MENU.lock().unwrap_or_else(|e| e.into_inner())
}

/// A container's local address, as the page builds it: its chosen
/// `dev.kiri.domain`, else Compose's service.project, else its name.
fn local_url(c: &serde_json::Value) -> Option<String> {
    if c["Ports"].as_array().is_none_or(|p| p.is_empty()) {
        return None;
    }
    let label = |k: &str| c["Labels"][k].as_str().unwrap_or("");
    let suffix = if crate::daemon::dns::installed() {
        "kiri.local"
    } else {
        "kiri.localhost"
    };
    let chosen = label("dev.kiri.domain")
        .split(',')
        .next()
        .unwrap_or("")
        .trim();
    let chosen = chosen
        .strip_suffix(".kiri.localhost")
        .or_else(|| chosen.strip_suffix(".kiri.local"))
        .unwrap_or(chosen);
    let name = c["Names"][0].as_str().unwrap_or("").trim_start_matches('/');
    let host = match (chosen, label("com.docker.compose.project")) {
        (c, _) if !c.is_empty() => c.to_owned(),
        (_, p) if !p.is_empty() => format!("{}.{p}", label("com.docker.compose.service")),
        _ => name.to_owned(),
    };
    let scheme = if crate::daemon::tls::trusted() {
        "https"
    } else {
        "http"
    };
    Some(format!("{scheme}://{host}.{suffix}"))
}

/// Delivers one JSON event to the page. Callable from any thread.
fn emit(json: String) {
    window::eval(format!("window.kiri && kiri.on({json})"));
}

fn state(name: &str, error: Option<&str>) -> String {
    let mut s = format!(r#"{{"type":"state","state":"{name}""#);
    if let Some(e) = error {
        s.push_str(r#","error":"#);
        json_str(&mut s, e);
    }
    s.push('}');
    s
}

/// Appends `v` as a JSON string literal.
fn json_str(out: &mut String, v: &str) {
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The engine thread: a client of `kiri daemon`. It answers the window's
/// commands and sends it the daemon's lists whenever an event says they
/// changed, so the page never polls.
fn engine(rx: Receiver<Cmd>, tx: Sender<Cmd>) {
    watch(tx);
    let mut connected = false;
    // A command that came while a burst of refreshes was being gathered: it's next.
    let mut next = None;
    loop {
        let cmd = next.take().map_or_else(|| rx.recv_timeout(TICK), Ok);
        // An action the person started works under an op id: its log lines,
        // the engine's for its requests and its error notice all carry it.
        let _op = cmd.as_ref().ok().and_then(Cmd::op).map(|(kind, target)| {
            let op = diag::enter_op(diag::new_op(kind));
            diag::info!("action started", action = kind, target = target);
            op
        });
        match cmd {
            Ok(Cmd::Start) => {
                emit(state("booting", None));
                match ensure_daemon()
                    .and_then(|()| expect_ok(client::request("POST", "/kiri/wake", None)))
                {
                    Ok(()) => {
                        // `docker build` through Kiri, if setup ran before the daemon was up.
                        std::thread::spawn(setup::ensure_builder);
                        refresh(&mut connected);
                    }
                    Err(e) => {
                        diag::warn!("action failed", action = "start", error = e);
                        emit(state("stopped", Some(&e.to_string())));
                    }
                }
            }
            Ok(Cmd::Connect) => match ensure_daemon() {
                Ok(()) => {
                    std::thread::spawn(setup::ensure_builder);
                    refresh(&mut connected);
                }
                Err(e) => {
                    diag::warn!("engine not reached", error = e);
                    emit(state("stopped", Some(&e.to_string())));
                }
            },
            // Stop means off: every container stopped, the VM off until Start
            // (nothing wakes it meanwhile), and this window's shares closed.
            Ok(Cmd::Stop) => {
                emit(state("stopping", None));
                sharing::close_all();
                if let Err(e) = stop_engine() {
                    emit(notice("error", &e.to_string()));
                }
                refresh(&mut connected);
            }
            Ok(Cmd::Refresh) => {
                // Events come in bursts (create, start, …): one refresh per burst. Anything
                // else ends the burst and runs after it, so a removal among the events (58
                // failed ones going 60 ms apart) isn't lost and each shows as it's done.
                loop {
                    match rx.recv_timeout(Duration::from_millis(60)) {
                        Ok(Cmd::Refresh) => {}
                        Ok(other) => break next = Some(other),
                        Err(_) => break,
                    }
                }
                refresh(&mut connected);
            }
            Ok(Cmd::Pull(name)) => pull(name),
            Ok(Cmd::Run(a)) => {
                // The page shows Starting… until this says how it went.
                // Name/port conflicts stay on the Run popover (`message`); other
                // failures toast from the page when no popover is open.
                let name = a.image.clone();
                let result = run_image(&a);
                let message = result.as_ref().err().map(std::string::ToString::to_string);
                if let Some(ref e) = message {
                    diag::warn!("action failed", action = "run", error = e);
                }
                emit(serde_json::json!({"type": "ran", "name": name, "error": message.is_some(), "message": message}).to_string());
            }
            Ok(Cmd::RemoveImage(name, with)) => {
                // The stopped containers made from it go first, never forced: a
                // running one refuses, and then the image stays too.
                let mut blocked = None;
                for id in &with {
                    match client::request(
                        "DELETE",
                        &format!("/containers/{}", path_escape(id)),
                        None,
                    ) {
                        Ok(r) if r.ok() || r.status == 404 => {}
                        Ok(r) => {
                            blocked = Some(format!(
                                "Kept {name}: {} couldn't be removed ({})",
                                container_name(id),
                                r.error()
                            ))
                        }
                        Err(e) => blocked = Some(format!("Kept {name}: {e}")),
                    }
                    if blocked.is_some() {
                        break;
                    }
                }
                match blocked {
                    Some(why) => emit(notice("error", &why)),
                    None => match client::request(
                        "DELETE",
                        &format!("/images/{}", path_escape(&name)),
                        None,
                    ) {
                        Ok(r) if r.ok() => {
                            // Only untagged when another name still points at it: nothing freed.
                            let deleted = r
                                .json()
                                .as_array()
                                .is_some_and(|a| a.iter().any(|v| v.get("Deleted").is_some()));
                            emit(serde_json::json!({"type": "removed", "name": name, "deleted": deleted}).to_string());
                        }
                        Ok(r) => emit(notice("error", &image_refusal(&name, &r.error()))),
                        Err(e) => emit(notice("error", &e.to_string())),
                    },
                }
                // Older daemons send no image events: refresh by hand.
                refresh(&mut connected);
            }
            Ok(Cmd::Container { action, id }) => {
                let (method, path) = match action.as_str() {
                    "remove" => ("DELETE", format!("/containers/{id}?force=1&v=1")),
                    a @ ("start" | "stop" | "restart" | "pause" | "unpause") => {
                        ("POST", format!("/containers/{id}/{a}"))
                    }
                    _ => continue,
                };
                let result = expect_ok(client::request(method, &path, None));
                // Refused to start (a port taken): the container's page says why.
                if let (Err(e), "start") = (&result, action.as_str()) {
                    emit(serde_json::json!({"type": "startfailed", "id": id, "error": e.to_string()}).to_string());
                }
                act(&action, result);
            }
            Ok(Cmd::RemoveVolume(name)) => act(
                "rmvol",
                expect_ok(client::request(
                    "DELETE",
                    &format!("/volumes/{}", path_escape(&name)),
                    None,
                )),
            ),
            Ok(Cmd::Ports { id, ports }) => act("ports", set_ports(&id, ports)),
            Ok(Cmd::Env { id, env }) => act("env", set_env(&id, env)),
            Ok(Cmd::About) => about(),
            Ok(Cmd::Domains(on)) => {
                let result = if on {
                    setup::domains()
                } else {
                    setup::remove_domains()
                };
                // Cancelling macOS's password prompt is a choice, not an error.
                match result {
                    Err(e) if !e.to_string().contains("-128") => emit(notice(
                        "error",
                        &format!("Could not change container names: {e}"),
                    )),
                    _ if crate::daemon::dns::installed() => {
                        emit(notice("ok", "Containers now answer at <name>.kiri.local"))
                    }
                    _ if !on => emit(notice("ok", "Containers answer at <name>.kiri.localhost")),
                    _ => {}
                }
                // /var/run/docker.sock was asked for with the same password.
                if let Some(note) = setup::docker_socket_note().filter(|_| on) {
                    emit(notice("ok", &note));
                }
                emit(domains_event());
                refresh(&mut connected);
            }
            Ok(Cmd::RemoveNetwork(id)) => act(
                "rmnet",
                expect_ok(client::request("DELETE", &format!("/networks/{id}"), None)),
            ),
            Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                vm_panel(&mut connected);
                if connected
                    && LISTED.lock().unwrap_or_else(|e| e.into_inner()).elapsed() >= LIST_EVERY
                {
                    refresh(&mut connected);
                }
            }
        }
    }
}

/// Follows the daemon's event stream on its own thread; each event asks
/// the engine for a refresh. Reconnects when the daemon comes back.
fn watch(tx: Sender<Cmd>) {
    let _ = std::thread::Builder::new()
        .name("kiri-ui-events".into())
        .spawn(move || {
            loop {
                if client::alive() {
                    if tx.send(Cmd::Refresh).is_err() {
                        return;
                    }
                    let _ = client::events(|_| tx.send(Cmd::Refresh).is_ok());
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
}

fn expect_ok(r: io::Result<client::Response>) -> io::Result<()> {
    let r = r?;
    if r.ok() {
        Ok(())
    } else {
        Err(io::Error::other(r.error()))
    }
}

/// An action's outcome: errors become a toast, and a log line saying what
/// was attempted; success shows in the next refresh, which its event triggers.
fn act(what: &str, result: io::Result<()>) {
    if let Err(e) = result {
        diag::warn!("action failed", action = what, error = e);
        emit(toast("error", &e.to_string()));
    }
}

fn path_escape(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~:/@".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Opens a URL, file or folder the way the system would on a double-click.
pub(crate) fn open(target: impl AsRef<std::ffi::OsStr>) {
    let opener = if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(target).spawn();
}

/// Shows a file selected in its folder: Finder on the Mac, the file manager
/// on Linux (or just the folder, when it can't select).
pub(crate) fn reveal(path: &std::path::Path) {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(path)
            .spawn();
        return;
    }
    let uri = format!("file://{}", path.display());
    let shown = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.FileManager1",
            "--object-path",
            "/org/freedesktop/FileManager1",
        ])
        .args([
            "--method",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("['{uri}']"),
            "",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !shown && let Some(dir) = path.parent() {
        open(dir);
    }
}

/// Kiri's folder in the home directory, ~/.kiri: the window's settings and
/// files. On the Mac the engine's socket is in it too; on Linux the engine is
/// a system service with its socket in /run/kiri. `KIRI_SOCKET` moves it, as
/// on the Mac (the tests use that).
pub(crate) fn state_dir() -> Option<std::path::PathBuf> {
    if cfg!(target_os = "linux") && std::env::var_os("KIRI_SOCKET").is_none() {
        return std::env::var_os("HOME")
            .map(|h| std::path::Path::new(&h).join(crate::flavor::STATE.get()));
    }
    client::socket()
        .ok()?
        .parent()?
        .parent()
        .map(std::path::Path::to_path_buf)
}

/// The window's settings, kept in ~/.kiri/ui.json: font, mono, scale.
fn prefs_path() -> Option<std::path::PathBuf> {
    let dir = state_dir()?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("ui.json"))
}

/// The settings, with the defaults for what the person hasn't picked: Kiri
/// starts dark until Settings → Appearance says otherwise.
pub(crate) fn prefs() -> serde_json::Value {
    let mut p: serde_json::Value = prefs_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !p.is_object() {
        p = serde_json::json!({});
    }
    if p.get("theme").is_none() {
        p["theme"] = "dark".into();
    }
    p
}

fn set_pref(name: &str, value: serde_json::Value) {
    if !matches!(
        name,
        "font"
            | "mono"
            | "scale"
            | "termH"
            | "termFs"
            | "termFont"
            | "domainsAsked"
            | "theme"
            | "accent"
            | "lang"
            | "importSeen"
            | "updateAuto"
            | "updateSkip"
            | "updateSnoozeUntil"
            | "columnWidths"
            | "listLayout"
            | "crashReports"
            | "ctxSnooze"
            | "ctxBack"
            | "logSize"
            | "starAsked"
            | "setupSkip"
    ) {
        return;
    }
    let Some(path) = prefs_path() else { return };
    let mut all = prefs();
    if !all.is_object() {
        all = serde_json::json!({});
    }
    // Crash reports go from when they're turned on: crashes from before that
    // wait for the person to pick them.
    if name == "crashReports" {
        if value != true {
            if let Some(o) = all.as_object_mut() {
                o.remove("crashReportsSince");
            }
        } else if all[name] != true {
            all["crashReportsSince"] = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .into();
        }
    }
    all[name] = value;
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, all.to_string())
        .and_then(|()| std::fs::rename(&tmp, &path))
        .is_err()
    {
        emit(notice(
            "error",
            "Could not save the setting to ~/.kiri/ui.json",
        ));
    }
}

/// Reads (or saves, then reads) the proxy settings and what is in effect.
fn proxy(save: Option<serde_json::Value>) {
    if let Some(body) = save {
        match client::request("POST", "/kiri/proxy", Some(&body)) {
            Ok(r) if r.ok() => emit(notice("ok", "Proxy settings saved")),
            Ok(r) => emit(notice("error", &r.error())),
            Err(e) => emit(notice("error", &e.to_string())),
        }
    }
    match client::get("/kiri/proxy") {
        Ok(mut v) => {
            v["type"] = "proxy".into();
            emit(v.to_string());
        }
        Err(e) => emit(serde_json::json!({"type": "proxy", "error": e.to_string()}).to_string()),
    }
}

/// Reads (or saves, then reads) `~/.kiri/daemon.json` (registry mirrors).
fn daemon(save: Option<serde_json::Value>) {
    if let Some(body) = save {
        match client::request("POST", "/kiri/daemon", Some(&body)) {
            Ok(r) if r.ok() => emit(notice("ok", "daemon.json saved")),
            Ok(r) => emit(notice("error", &r.error())),
            Err(e) => emit(notice("error", &e.to_string())),
        }
    }
    match client::get("/kiri/daemon") {
        Ok(mut v) => {
            v["type"] = "daemon".into();
            emit(v.to_string());
        }
        Err(e) => emit(serde_json::json!({"type": "daemon", "error": e.to_string()}).to_string()),
    }
}

/// Reads (or saves, then reads) the VM's CPUs and memory for Settings.
fn resources(save: Option<serde_json::Value>) {
    let saved = save.map(|body| client::request("POST", "/kiri/resources", Some(&body)));
    let now = client::get("/kiri/resources");
    match saved {
        // While the VM runs, the toast offers to restart it now.
        Some(Ok(r)) if r.ok() => {
            let awake = now.as_ref().is_ok_and(|v| v["awake"] == true);
            emit(serde_json::json!({"type": "notice", "kind": "ok", "text": "Saved. The VM uses these from its next start", "act": if awake { "restartvm" } else { "" }}).to_string());
        }
        Some(Ok(r)) => emit(notice("error", &r.error())),
        Some(Err(e)) => emit(notice("error", &e.to_string())),
        None => {}
    }
    match now {
        Ok(mut v) => {
            v["type"] = "resources".into();
            emit(v.to_string());
        }
        Err(e) => {
            emit(serde_json::json!({"type": "resources", "error": e.to_string()}).to_string())
        }
    }
}

/// A docker CLI from elsewhere (Docker Desktop, OrbStack, Homebrew): Kiri
/// Lite then has nothing to download. An app opened from Finder has a short
/// PATH, so the usual places are looked at too.
fn other_docker_cli() -> bool {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let mut dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend([
        std::path::PathBuf::from("/usr/local/bin"),
        std::path::PathBuf::from("/opt/homebrew/bin"),
        home.join(".orbstack/bin"),
    ]);
    dirs.iter().any(|d| d.join("docker").is_file())
}

/// Downloads Kiri Lite's tools in the background, with progress for the page.
fn setup_tools() {
    std::thread::spawn(|| {
        let mut last = std::time::Instant::now();
        let result = crate::tools::ensure(&mut |file, index, count, pct| {
            if pct == 100 || last.elapsed() > Duration::from_millis(150) {
                last = std::time::Instant::now();
                emit(serde_json::json!({"type": "tools", "state": "downloading", "file": file, "index": index, "count": count, "pct": pct}).to_string());
            }
        });
        match result.and_then(|()| setup::after_tools()) {
            Ok(()) => emit(serde_json::json!({"type": "tools", "state": "done"}).to_string()),
            Err(e) => emit(
                serde_json::json!({"type": "tools", "state": "error", "error": e.to_string()})
                    .to_string(),
            ),
        }
    });
}

/// The status bar's left side: this build, and which engine `docker` talks
/// to by default (`DOCKER_CONTEXT`, else the CLI config's current context).
/// Set by the card's ×-in-the-chip: stops a download in progress.
static UPDATE_CANCEL: AtomicBool = AtomicBool::new(false);
/// A download is running; a second Download waits for it.
static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

/// About's update row: what the last check found. `state` is uptodate,
/// available, package (Linux: a new .deb or .rpm), test, dev or error.
fn update_state(state: &str, latest: Option<&str>, error: Option<String>) {
    let checked = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    emit(serde_json::json!({"type": "updatestate", "state": state, "current": env!("CARGO_PKG_VERSION"), "latest": latest, "error": error, "checked": checked}).to_string());
}

/// Tells the page what there is: a staged update ("updateready"), else a
/// newer release ("update"); and whether the engine runs an older build than
/// the app ("enginestale"). `manual` (Check for Updates…, About's Check now)
/// also says when there is nothing new, and shows a skipped or snoozed
/// version again. With "Check for updates automatically" off, only a manual
/// check asks the feed.
fn update_check(manual: bool) {
    // Once, when the window opens: an older `kiri` that terminals run instead
    // of this one is how an update or reinstall seems not to have happened.
    if !manual
        && !crate::flavor::dev()
        && let Some(p) = crate::stale::stale_kiri()
    {
        emit(notice(
            "error",
            &format!(
                "{} comes first on your PATH, so terminals run it instead of this Kiri. If it's an older Kiri, remove it: rm {0}",
                p.display()
            ),
        ));
    }
    if let Some(v) = crate::client::request("GET", "/version", None)
        .ok()
        .filter(|r| r.ok())
        .and_then(|r| r.json()["Version"].as_str().map(str::to_owned))
        && update::newer(env!("CARGO_PKG_VERSION"), &v)
    {
        emit(serde_json::json!({"type": "enginestale", "version": v}).to_string());
    }
    feed_check(manual);
}

/// Whether the feed has a newer release, and one already staged; the
/// window's check and the 6-hourly one (recheck.rs).
fn feed_check(manual: bool) {
    // Kiri Dev is rebuilt from source, never updated from the feed, unless
    // KIRI_UPDATE_URL names a test feed (to try the update card and What's new).
    if crate::flavor::dev() && std::env::var_os("KIRI_UPDATE_URL").is_none() {
        if manual {
            emit(notice(
                "ok",
                &format!(
                    "{} doesn't update itself: rebuild it with cargo xtask app-dev",
                    crate::flavor::APP
                ),
            ));
        }
        update_state("dev", None, None);
        return;
    }
    // A test build (0.1.8-dev.42, scripts/ci/test-version.sh) isn't updated
    // from the feed either: it would "update" back to the release it came from.
    if env!("CARGO_PKG_VERSION").contains("-dev") {
        if manual {
            emit(notice(
                "ok",
                &format!(
                    "Kiri {} is a test build: it doesn't update itself",
                    env!("CARGO_PKG_VERSION")
                ),
            ));
        }
        update_state("test", None, None);
        return;
    }
    if let Some(v) = update::staged() {
        emit(serde_json::json!({"type": "updateready", "version": v}).to_string());
        return;
    }
    if !manual && prefs()["updateAuto"] == false {
        return;
    }
    recheck::checked();
    // Linux's kiri comes from a .deb or .rpm: the feed only says whether a
    // newer one exists, and About links to the releases page, so no card.
    if cfg!(target_os = "linux") {
        match update::latest() {
            Ok(r) => update_state(
                "package",
                update::newer(&r.version, env!("CARGO_PKG_VERSION")).then_some(r.version.as_str()),
                None,
            ),
            Err(e) => update_state("error", None, Some(e.to_string())),
        }
        return;
    }
    match update::latest() {
        Ok(r) if update::newer(&r.version, env!("CARGO_PKG_VERSION")) => {
            update_state("available", Some(&r.version), None);
            emit(serde_json::json!({"type": "update", "release": r, "current": env!("CARGO_PKG_VERSION"), "manual": manual}).to_string());
        }
        Ok(_) => {
            update_state("uptodate", None, None);
            if manual {
                emit(notice(
                    "ok",
                    &format!("Kiri {} is the newest version", env!("CARGO_PKG_VERSION")),
                ));
            }
        }
        Err(e) => {
            update_state("error", None, Some(e.to_string()));
            if manual {
                emit(notice("error", &format!("Couldn't check for updates: {e}")));
            }
        }
    }
}

/// Download on the card: fetches, checks and stages the release in the
/// background, reporting progress a few times a second.
fn update_download() {
    if UPDATE_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    UPDATE_CANCEL.store(false, Ordering::Relaxed);
    let _ = std::thread::Builder::new().name("kiri-update".into()).spawn(|| {
        let result = update::latest().and_then(|r| {
            let mut last = std::time::Instant::now() - Duration::from_secs(1);
            update::download(
                &r,
                &mut |got, total| {
                    if last.elapsed() >= Duration::from_millis(250) {
                        last = std::time::Instant::now();
                        emit(serde_json::json!({"type": "updateprogress", "got": got, "total": total}).to_string());
                    }
                },
                &UPDATE_CANCEL,
            )
            .map(|()| r.version)
        });
        UPDATE_RUNNING.store(false, Ordering::SeqCst);
        emit(match result {
            Ok(v) => serde_json::json!({"type": "updateready", "version": v}).to_string(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => r#"{"type":"updatecancelled"}"#.to_owned(),
            Err(e) => serde_json::json!({"type": "updateerror", "error": e.to_string()}).to_string(),
        });
    });
}

/// Restart now: the staged app goes in, the engine restarts on it, and the
/// new window opens once this one has quit.
fn update_restart() {
    match update::install_staged() {
        Ok(_) => {
            update::restart_engine();
            reopen_after_exit();
            window::quit();
        }
        Err(e) => {
            emit(serde_json::json!({"type": "updateerror", "error": e.to_string()}).to_string())
        }
    }
}

/// Opens the app (by then the new one) as soon as this process has gone.
fn reopen_after_exit() {
    let Ok(app) = update::app() else { return };
    let script =
        "while /bin/kill -0 \"$1\" 2>/dev/null; do /bin/sleep 0.2; done; exec /usr/bin/open \"$2\"";
    let _ = std::process::Command::new("/bin/sh")
        .args(["-c", script, "sh", &std::process::id().to_string()])
        .arg(&app)
        .spawn();
}

fn status_event() -> String {
    let context = std::env::var("DOCKER_CONTEXT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            let cfg: serde_json::Value = serde_json::from_slice(
                &std::fs::read(std::path::Path::new(&home).join(".docker/config.json")).ok()?,
            )
            .ok()?;
            cfg["currentContext"].as_str().map(str::to_owned)
        });
    // Linux: docker's default endpoint is the standard socket, which leads to
    // Kiri while no Docker engine has it (the package links it), and the
    // package's docker goes to Kiri's socket: that is docker using Kiri, as
    // the setup checklist says, not "Docker's default engine".
    #[cfg(target_os = "linux")]
    let context = if context.as_deref().is_none_or(|c| c == "default")
        && linux::onboarding::docker_uses_kiri()
    {
        Some(crate::flavor::CONTEXT.get().to_owned())
    } else {
        context
    };
    serde_json::json!({"type": "status", "version": env!("CARGO_PKG_VERSION"), "dev": crate::flavor::dev(), "share": true, "app": crate::flavor::APP.get(), "ours": crate::flavor::CONTEXT.get(), "context": context.unwrap_or_else(|| "default".into())}).to_string()
}

/// Which names containers answer at now.
fn domains_event() -> String {
    let on = crate::daemon::dns::installed();
    serde_json::json!({"type": "domains", "on": on, "suffix": if on { "kiri.local" } else { "kiri.localhost" }, "https": crate::daemon::tls::trusted()}).to_string()
}

/// Settings → About: this build, and the engine as the daemon reports it.
fn about() {
    let info = client::get("/info").unwrap_or(serde_json::Value::Null);
    let version = client::get("/version").unwrap_or(serde_json::Value::Null);
    emit(
        serde_json::json!({
            "type": "about",
            "app": env!("CARGO_PKG_VERSION"),
            "running": !info.is_null(),
            "engine": version["Version"],
            "api": version["ApiVersion"],
            "kernel": info["KernelVersion"],
            "cpus": info["NCPU"],
            "memory": info["MemTotal"],
            "awake": info["KiriVmAwake"],
            "os": info["OperatingSystem"],
        })
        .to_string(),
    );
}

/// Where a published port answers: `localhost:<port>`, the handy URL, noting
/// that the bind is every interface (the default, as Docker's), or else the
/// address it was bound to (`-p 127.0.0.1:8080:80`).
fn published(b: &serde_json::Value) -> Option<String> {
    let p = b["HostPort"].as_str()?;
    Some(match b["HostIp"].as_str().unwrap_or("") {
        "" | "0.0.0.0" | "::" => format!("localhost:{p} (all interfaces)"),
        ip => format!("{ip}:{p}"),
    })
}

/// Saves a container's ports and says where it is reachable now.
fn set_ports(id: &str, ports: serde_json::Value) -> io::Result<()> {
    expect_ok(client::request(
        "POST",
        &format!("/kiri/containers/{id}/ports"),
        Some(&serde_json::json!({"ports": ports})),
    ))?;
    let info = client::get(&format!("/containers/{id}/json"))?;
    let mut now: Vec<String> = Vec::new();
    if let Some(map) = info["NetworkSettings"]["Ports"].as_object() {
        for (inner, list) in map {
            for b in list.as_array().into_iter().flatten() {
                if let Some(at) = published(b) {
                    now.push(format!("{at} → {inner}"));
                }
            }
        }
    }
    let text = if now.is_empty() {
        "Ports removed".to_owned()
    } else {
        format!("Ports saved: {}", now.join(", "))
    };
    emit(notice("ok", &text));
    Ok(())
}

/// A container's new environment. The engine keeps the container, files and
/// all, and restarts it if it was running.
fn set_env(id: &str, env: Vec<String>) -> io::Result<()> {
    let r = client::request(
        "POST",
        &format!("/kiri/containers/{id}/env"),
        Some(&serde_json::json!({"env": env})),
    )?;
    if r.status == 404 && !r.error().contains("No such container") {
        return Err(io::Error::other(
            "This engine can't change a container's environment yet. Restart the engine (Settings → Engine) and try again.",
        ));
    }
    expect_ok(Ok(r))?;
    let info = client::get(&format!("/containers/{id}/json"))?;
    let name = info["Name"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches('/');
    let how = if info["State"]["Running"].as_bool() == Some(true) {
        "restarted with"
    } else {
        "has"
    };
    emit(notice("ok", &format!("{name} {how} the new environment")));
    emit(serde_json::json!({"type": "inspect", "id": id, "info": info}).to_string());
    Ok(())
}

/// The window's Run: the image with its own command, detached, then the
/// Containers view and a note saying where to open it. Each port the image
/// declares is published on the same port of the Mac when that is free
/// (gallerybox's 4567 on localhost:4567), otherwise on a free one, as
/// `docker run -P` does. An image that declares none gets an offer to
/// publish what it turns out to listen on.
fn run_image(a: &RunArgs) -> io::Result<()> {
    let own = same_ports(&a.image);
    let began = std::time::Instant::now();
    let id = match create_and_start(a, &own) {
        // Taken between the check and the start: let the engine pick.
        // A host port the person chose is kept; only the same-port guesses retry.
        Err(e)
            if a.host_port.is_none()
                && !own.is_empty()
                && e.to_string().contains("already allocated") =>
        {
            create_and_start(a, &[])?
        }
        r => r?,
    };
    let image = a.image.as_str();
    let info = client::get(&format!("/containers/{id}/json"))?;
    let name = info["Name"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_owned();
    // Just the name: the container's page says where it answers.
    let published = info["NetworkSettings"]["Ports"]
        .as_object()
        .is_some_and(|m| {
            m.values()
                .any(|l| l.as_array().is_some_and(|l| !l.is_empty()))
        });
    let text = format!("Started {name}");
    // View on the toast opens the container's page; `image` puts it beside
    // that image's Run button when the list shows it; `ms` is create and start.
    let ms = began.elapsed().as_millis() as u64;
    emit(serde_json::json!({"type": "notice", "kind": "ok", "text": text, "open": id, "image": image, "ms": ms}).to_string());
    if !published {
        offer_listening(id, name);
    }
    Ok(())
}

/// Asks a container that publishes nothing what it listens on, for up to
/// 10 s since apps take a moment to bind, and offers to publish that. Quiet
/// if it never listens, stops, or the engine is too old to say.
fn offer_listening(id: String, name: String) {
    std::thread::spawn(move || {
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            match listening(&id) {
                None => return,
                Some(ports) if !ports.is_empty() => {
                    return emit(listening_event(&id, &name, &ports, true));
                }
                Some(_) => {}
            }
        }
    });
}

/// The TCP ports a running container listens on that a published port
/// would reach, each with the Mac port to publish it on: the same one when
/// free, else 0 for the engine to pick. None if the engine can't say (an
/// older engine or VM, or the container stopped).
fn listening(id: &str) -> Option<Vec<(u16, u16)>> {
    let v =
        client::get_background(&format!("/kiri/containers/{}/listening", path_escape(id))).ok()?;
    let ports = v["tcp"]
        .as_array()?
        .iter()
        .filter_map(|p| u16::try_from(p.as_u64()?).ok());
    Some(
        ports
            .map(|p| (p, if free_on_mac(p, "tcp") { p } else { 0 }))
            .collect(),
    )
}

/// `offer` makes the page say so in a notice; otherwise it only notes the
/// ports for the container's page.
fn listening_event(id: &str, name: &str, ports: &[(u16, u16)], offer: bool) -> String {
    let ports: Vec<_> = ports
        .iter()
        .map(|(c, h)| serde_json::json!({"container_port": c, "host_port": h}))
        .collect();
    serde_json::json!({"type": "listening", "id": id, "name": name, "ports": ports, "offer": offer})
        .to_string()
}

/// Joins shell line continuations (`\` at end of line) into one line so a
/// pasted multiline command becomes ordinary argv tokens.
fn flatten_continuations(s: &str) -> String {
    let mut out = String::new();
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let cont = t.ends_with('\\')
            && t.as_bytes()
                .iter()
                .rev()
                .take_while(|&&b| b == b'\\')
                .count()
                % 2
                == 1;
        let piece = if cont {
            t[..t.len() - 1].trim_end()
        } else {
            t
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(piece);
    }
    out
}

/// Splits a Run popover command the way a simple shell would: whitespace
/// separates args; single or double quotes keep spaces inside one arg.
/// Line-ending `\` continuations are folded first.
fn split_command(s: &str) -> Vec<String> {
    let flat = flatten_continuations(s);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in flat.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Quotes one shell argument for a copy-pasteable `docker run` line.
fn shell_arg(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.:/=@+,".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// A copy-pasteable `docker run -d …` block matching how Kiri creates the
/// container, broken with `\` for the container page's preview.
fn docker_run_line(
    image: &str,
    env: &[String],
    name: &str,
    bindings: &serde_json::Map<String, serde_json::Value>,
    command: &str,
) -> String {
    let mut lines: Vec<String> = vec!["docker run -d -it -P".into()];
    if !name.is_empty() {
        lines.push(format!("  --name {}", shell_arg(name)));
    }
    for e in env {
        lines.push(format!("  -e {}", shell_arg(e)));
    }
    let mut keys: Vec<_> = bindings.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let Some(list) = bindings.get(&key).and_then(|v| v.as_array()) else {
            continue;
        };
        let host = list
            .first()
            .and_then(|b| b.get("HostPort"))
            .and_then(|p| p.as_str())
            .unwrap_or("");
        let (port, proto) = match key.split_once('/') {
            Some((p, "udp")) => (p, "/udp"),
            Some((p, _)) => (p, ""),
            None => (key.as_str(), ""),
        };
        if host.is_empty() {
            continue;
        }
        lines.push(format!("  -p {host}:{port}{proto}"));
    }
    lines.push(format!("  {}", shell_arg(image)));
    let argv = split_command(command);
    if !argv.is_empty() {
        lines.push(format!(
            "  {}",
            argv.iter().map(|a| shell_arg(a)).collect::<Vec<_>>().join(" ")
        ));
    }
    let last = lines.len().saturating_sub(1);
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            if i == last {
                line
            } else {
                format!("{line} \\")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Creates and starts `a.image` with every declared port published, `own` on
/// the same host port. A container that fails to start is removed.
/// `a.container` is `docker run --name`: empty lets the engine pick one.
/// `a.host_port` pins a host port; with `a.target_port` (custom `-p H:C`) that
/// container port is used even when the image declares no `EXPOSE`. Otherwise
/// the image's primary exposed port is used. `a.command` replaces `CMD`;
/// `a.entrypoint` replaces the image entrypoint when set. The stored
/// `kiri.start.command` label prefers a pasted `a.start_command`.
fn create_and_start(a: &RunArgs, own: &[String]) -> io::Result<String> {
    let image = a.image.as_str();
    let env = a.env.as_slice();
    let container = a.container.as_str();
    let mut bindings: serde_json::Map<String, serde_json::Value> = own
        .iter()
        .map(|key| {
            let port = key.split('/').next().unwrap_or_default();
            (
                key.clone(),
                serde_json::json!([{"HostIp": "", "HostPort": port}]),
            )
        })
        .collect();
    if let Some(hp) = a.host_port {
        let key = if let Some(tp) = a.target_port {
            format!("{tp}/tcp")
        } else if let Some(k) = primary_exposed(image) {
            k
        } else {
            return Err(io::Error::other(
                "this image exposes no ports to publish on the host",
            ));
        };
        bindings.insert(
            key,
            serde_json::json!([{"HostIp": "", "HostPort": hp.to_string()}]),
        );
    }
    let name = container.trim().trim_start_matches('/');
    let start_cmd = a.command.trim();
    let argv = split_command(start_cmd);
    // Labels must stay single-line without `\` continuations; the page
    // pretty-prints with line breaks again.
    let run_line = {
        let raw = a.start_command.trim();
        let src = if raw.is_empty() {
            docker_run_line(image, env, name, &bindings, start_cmd)
        } else {
            raw.to_owned()
        };
        src.lines()
            .map(|l| l.trim().trim_end_matches('\\').trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut host = serde_json::json!({"PublishAllPorts": true, "PortBindings": bindings});
    if !a.binds.is_empty() {
        host["Binds"] = serde_json::json!(a.binds);
    }
    if !a.restart.trim().is_empty() {
        host["RestartPolicy"] =
            serde_json::json!({"Name": a.restart.trim(), "MaximumRetryCount": 0});
    }
    let network = a.network.trim();
    if !network.is_empty() {
        host["NetworkMode"] = serde_json::json!(network);
    }
    let mut body = serde_json::json!({"Image": image, "Tty": true, "OpenStdin": true, "Env": env,
        "Labels": {"kiri.start.command": run_line},
        "HostConfig": host});
    if !argv.is_empty() {
        body["Cmd"] = serde_json::json!(argv);
    }
    let ep = a.entrypoint.trim();
    if !ep.is_empty() {
        body["Entrypoint"] = serde_json::json!(split_command(ep));
    }
    let path = match container.trim() {
        "" => "/containers/create".to_owned(),
        name => format!(
            "/containers/create?name={}",
            path_escape(name.trim_start_matches('/'))
        ),
    };
    let created = client::request("POST", &path, Some(&body))?;
    if !created.ok() {
        return Err(io::Error::other(created.error()));
    }
    let id = created.json()["Id"].as_str().unwrap_or_default().to_owned();
    if let Err(e) = expect_ok(client::request(
        "POST",
        &format!("/containers/{id}/start"),
        None,
    )) {
        let _ = client::request("DELETE", &format!("/containers/{id}?force=1"), None);
        return Err(e);
    }
    Ok(id)
}

/// The first exposed port on `image` to pin a host port to: prefer TCP, then
/// any other, in name order (`80/tcp` before `443/tcp`).
fn primary_exposed(image: &str) -> Option<String> {
    let info = client::get(&format!("/images/{}/json", path_escape(image))).ok()?;
    let map = info["Config"]["ExposedPorts"].as_object()?;
    let mut keys: Vec<_> = map.keys().cloned().collect();
    keys.sort();
    keys.iter()
        .find(|k| k.ends_with("/tcp"))
        .cloned()
        .or_else(|| keys.into_iter().next())
}

/// The ports `image` declares (`4567/tcp`) whose number is free on the Mac.
fn same_ports(image: &str) -> Vec<String> {
    let Ok(info) = client::get(&format!("/images/{}/json", path_escape(image))) else {
        return Vec::new();
    };
    let Some(map) = info["Config"]["ExposedPorts"].as_object() else {
        return Vec::new();
    };
    map.keys()
        .filter(|key| {
            let (port, proto) = key.split_once('/').unwrap_or((key, "tcp"));
            port.parse::<u16>()
                .is_ok_and(|port| free_on_mac(port, proto))
        })
        .cloned()
        .collect()
}

/// Whether nothing on the Mac holds `port` on every interface.
fn free_on_mac(port: u16, proto: &str) -> bool {
    match proto {
        "tcp" => std::net::TcpListener::bind(("0.0.0.0", port)).is_ok(),
        "udp" => std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok(),
        _ => false,
    }
}

/// Starts `kiri daemon` in the background if it is not running. It outlives
/// the window: closing the window leaves containers running.
#[cfg_attr(target_os = "linux", allow(unreachable_code))]
fn ensure_daemon() -> io::Result<()> {
    if client::alive() {
        return Ok(());
    }
    // Linux: the engine is a system service, started by systemd, not by the window.
    #[cfg(target_os = "linux")]
    return Err(linux::engine_down());
    diag::info!("starting the engine");
    let exe = std::env::current_exe()?;
    let log_dir = client::socket()?
        .parent()
        .and_then(|p| p.parent())
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| io::Error::other("no ~/.kiri"))?;
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Its own session, so quitting the window does not take it down.
    // SAFETY: setsid is async-signal-safe and runs in the child before exec.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if client::alive() {
            return Ok(());
        }
    }
    Err(io::Error::other(format!(
        "kiri daemon did not start; see {}",
        log_dir.join("daemon.log").display()
    )))
}

/// Sends the page every list, from the daemon.
/// When the lists last went to the page, by any route.
static LISTED: std::sync::LazyLock<std::sync::Mutex<std::time::Instant>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::time::Instant::now()));

fn refresh(connected: &mut bool) {
    *LISTED.lock().unwrap_or_else(|e| e.into_inner()) = std::time::Instant::now();
    let lists = (|| -> io::Result<serde_json::Value> {
        let containers = client::get_background("/containers/json?all=1")?;
        let images = client::get_background("/images/json")?;
        let volumes = client::get_background("/volumes")?;
        let networks = client::get_background("/networks")?;
        let add_icon = |mut v: serde_json::Value,
                        name_of: &dyn Fn(&serde_json::Value) -> String| {
            if let Some(list) = v.as_array_mut() {
                for item in list {
                    let icon = brand(&name_of(item));
                    item["icon"] = serde_json::json!(icon);
                }
            }
            v
        };
        let containers = add_icon(containers, &|c| {
            c["Image"].as_str().unwrap_or_default().to_owned()
        });
        let images = add_icon(images, &|i| {
            i["RepoTags"][0].as_str().unwrap_or_default().to_owned()
        });
        Ok(
            serde_json::json!({"type": "lists", "containers": containers, "images": images, "volumes": volumes["Volumes"], "networks": networks}),
        )
    })();
    match lists {
        Ok(v) => {
            menu_state().running = v["containers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["State"] == "running")
                .map(|c| {
                    (
                        c["Id"].as_str().unwrap_or("").to_owned(),
                        c["Names"][0]
                            .as_str()
                            .unwrap_or("")
                            .trim_start_matches('/')
                            .to_owned(),
                        local_url(c),
                    )
                })
                .collect();
            emit(v.to_string());
            vm_panel(connected);
        }
        Err(_) => {
            *connected = false;
            *menu_state() = MenuState {
                vm: "Not running",
                running: Vec::new(),
            };
            emit(state("stopped", None));
        }
    }
}

/// The engine's Stop, or, from an engine older than it, its sleep.
fn stop_engine() -> io::Result<()> {
    match expect_ok(client::request("POST", "/kiri/stop", None)) {
        Err(e) if e.to_string().contains("404") => {
            expect_ok(client::request("POST", "/kiri/sleep", None))
        }
        r => r,
    }
}

/// Whether Kiri was stopped (Stop in the window): then it stays off, even
/// when the window opens, until Start.
fn engine_stopped() -> bool {
    client::alive() && client::get_background("/info").is_ok_and(|i| i["KiriStopped"] == true)
}

/// The sidebar's VM panel: awake, asleep (wakes on the next request) or
/// stopped (off until Start), and memory.
fn vm_panel(connected: &mut bool) {
    match client::get_background("/info") {
        Ok(info) => {
            *connected = true;
            let awake = info["KiriVmAwake"].as_bool().unwrap_or(false);
            let stopped = !awake && info["KiriStopped"] == true;
            menu_state().vm = if awake {
                "Running"
            } else if stopped {
                "Stopped"
            } else {
                "Asleep"
            };
            let mib = |k: &str| info[k].as_u64().unwrap_or(0) >> 20;
            emit(
                serde_json::json!({
                    "type": "vm",
                    "awake": awake,
                    "target_mib": mib("KiriMemoryTarget"),
                    "used_mib": mib("KiriMemoryUsed"),
                    "ceiling_mib": mib("KiriMemoryCeiling"),
                    "stall_pct": info["KiriMemoryStall"],
                    "oom_kills": info["KiriOomKills"],
                    "running": info["ContainersRunning"],
                    "cpus": info["NCPU"],
                    "cpu_pct": info["KiriCpuPercent"],
                    "daemon_kib": info["KiriAgentMemory"].as_u64().unwrap_or(0) >> 10,
                })
                .to_string(),
            );
            emit(state(
                if awake {
                    "running"
                } else if stopped {
                    "stopped"
                } else {
                    "asleep"
                },
                None,
            ));
        }
        Err(_) => {
            if *connected {
                *connected = false;
            }
            emit(state("stopped", None));
        }
    }
}

/// A container's name, for messages; its short id if it has none.
fn container_name(id: &str) -> String {
    client::get(&format!("/containers/{}/json", path_escape(id)))
        .ok()
        .and_then(|c| {
            c["Name"]
                .as_str()
                .map(|n| n.trim_start_matches('/').to_owned())
        })
        .unwrap_or_else(|| id.chars().take(12).collect())
}

/// Docker's "unable to remove repository reference … (must force) - container
/// X is using its referenced image", in plain words with the container's name.
fn image_refusal(name: &str, error: &str) -> String {
    match error
        .split("container ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .filter(|_| error.contains("is using"))
    {
        Some(id) => format!(
            "{name} is still used by the container {}. Remove the container first.",
            container_name(id)
        ),
        None => error.to_owned(),
    }
}

/// A toast; an error one is logged too.
fn notice(kind: &str, text: &str) -> String {
    if kind == "error" {
        diag::warn!("error shown", text = text);
    }
    toast(kind, text)
}

/// The notice event itself. An error carries the op of the action it came
/// from, so the page's Details can ask for that action's log lines
/// (`diaglogs` with `id` set to it).
fn toast(kind: &str, text: &str) -> String {
    let mut s = format!(r#"{{"type":"notice","kind":"{kind}","text":"#);
    json_str(&mut s, text);
    if let Some(op) = diag::op().filter(|_| kind == "error") {
        s.push_str(r#","op":"#);
        json_str(&mut s, &op);
    }
    s.push('}');
    s
}

/// Looks up the brand logo for an image name like `bitnami/redis:7`: first by
/// the whole repository path, then by its last component.
fn brand(name: &str) -> Option<[&'static str; 2]> {
    let name = name.split_once('@').map_or(name, |(n, _)| n);
    let repo = match name.rfind(':') {
        Some(i) if !name[i..].contains('/') => &name[..i],
        _ => name,
    };
    let last = repo.rsplit('/').next().unwrap_or(repo);
    let slug = [repo, last].into_iter().find_map(|key| {
        brands::IMAGES
            .binary_search_by_key(&key, |(k, _)| k)
            .ok()
            .map(|i| brands::IMAGES[i].1)
    })?;
    let i = brands::BRANDS
        .binary_search_by_key(&slug, |(s, _, _)| s)
        .ok()?;
    let (_, hex, path) = brands::BRANDS[i];
    Some([hex, path])
}

/// The "it's here" event after a pull: the name, how long it took and the
/// brand logo, so the window can show more than a plain notice.
fn pulled(reference: &str, ms: u128) -> String {
    let mut s = r#"{"type":"pulled","name":"#.to_string();
    json_str(&mut s, reference);
    s.push_str(&format!(r#","ms":{ms},"icon":"#));
    match brand(reference) {
        Some([hex, path]) => {
            s.push('[');
            json_str(&mut s, hex);
            s.push(',');
            json_str(&mut s, path);
            s.push(']');
        }
        None => s.push_str("null"),
    }
    s.push('}');
    s
}

/// A pull the page asked for, from the moment it asks (a Cancel can come
/// before the pull's turn): which it is, its image, its connection to the
/// engine once open, and whether Cancel was pressed.
struct Pulling {
    run: u64,
    name: String,
    taken: bool,
    conn: Option<std::os::unix::net::UnixStream>,
    cancelled: bool,
}

static PULLING: std::sync::Mutex<Vec<Pulling>> = std::sync::Mutex::new(Vec::new());

fn pulling() -> std::sync::MutexGuard<'static, Vec<Pulling>> {
    PULLING.lock().unwrap_or_else(|e| e.into_inner())
}

/// The page asked for a pull: noted, so Cancel finds it even while it waits.
fn asked_to_pull(name: &str) {
    static RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let Ok(r) = kiri_image::Reference::parse(name.trim()) else {
        return;
    };
    let run = RUN.fetch_add(1, Ordering::Relaxed);
    pulling().push(Pulling {
        run,
        name: r.short(),
        taken: false,
        conn: None,
        cancelled: false,
    });
}

/// Cancel on a pull's row: its request to the engine is closed, which stops
/// the engine's pull (unless a `docker pull` is following it too).
fn cancel_pull(name: &str) {
    for p in pulling().iter_mut().filter(|p| p.name == name) {
        p.cancelled = true;
        if let Some(c) = &p.conn {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Whether the pull `run` was cancelled.
fn pull_cancelled(run: Option<u64>) -> bool {
    run.is_some_and(|run| pulling().iter().any(|p| p.run == run && p.cancelled))
}

/// Pulls through the daemon on its own thread; progress shows as a busy row.
fn pull(name: String) {
    let op = diag::op();
    let spawned = std::thread::Builder::new().name("kiri-pull".into()).spawn(move || {
        let _op = op.map(diag::enter_op);
        let reference = match kiri_image::Reference::parse(name.trim()) {
            Ok(r) => r.short(),
            Err(e) => return emit(notice("error", &e.to_string())),
        };
        // This pull's note: the oldest one asked for under this name.
        let run = pulling().iter_mut().find(|p| p.name == reference && !p.taken).map(|p| {
            p.taken = true;
            p.run
        });
        if pull_cancelled(run) {
            pulling().retain(|p| Some(p.run) != run);
            return emit(serde_json::json!({"type": "pull", "state": "done", "name": reference, "cancelled": true}).to_string());
        }
        let mut started = r#"{"type":"pull","state":"started","name":"#.to_string();
        json_str(&mut started, &reference);
        started.push('}');
        emit(started);
        let began = std::time::Instant::now();
        let (from, tag) = match reference.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
            Some((f, t)) => (f.to_owned(), t.to_owned()),
            None => (reference.clone(), "latest".to_owned()),
        };
        let result = ensure_daemon().and_then(|()| {
            // Each layer's size and bytes so far, for one bar over them all.
            let mut layers: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
            let mut error = None;
            let mut finished = false;
            let mut told = std::time::Instant::now();
            let path = format!("/images/create?fromImage={}&tag={}", path_escape(&from), path_escape(&tag));
            // Signed in to its registry: the sign-in goes with it, as docker's does.
            let auth = logins::header_for(&from);
            // Kept for Cancel, which shuts it; cancelled already, it's shut now.
            let opened = |c: &std::os::unix::net::UnixStream| {
                if let Some(p) = pulling().iter_mut().find(|p| Some(p.run) == run) {
                    if p.cancelled {
                        let _ = c.shutdown(std::net::Shutdown::Both);
                    } else {
                        p.conn = c.try_clone().ok();
                    }
                }
            };
            let status = client::post_lines_closable(&path, auth.as_deref(), opened, |v| {
                let id = v["id"].as_str().unwrap_or_default().to_owned();
                match v["status"].as_str().unwrap_or_default() {
                    "Pulling fs layer" => {
                        layers.insert(id, (v["size"].as_u64().unwrap_or(0), 0));
                    }
                    "Downloading" => {
                        let d = &v["progressDetail"];
                        layers.insert(id, (d["total"].as_u64().unwrap_or(0), d["current"].as_u64().unwrap_or(0)));
                    }
                    "Pull complete" => {
                        if let Some(l) = layers.get_mut(&id) {
                            l.1 = l.0;
                        }
                    }
                    _ => {}
                }
                if let Some(e) = v["error"].as_str().or(v["message"].as_str()) {
                    error = Some(e.to_owned());
                }
                if v["status"].as_str().is_some_and(|s| s.starts_with("Status: ")) {
                    finished = true;
                }
                // A few updates a second is enough for a bar; none once cancelled.
                if !layers.is_empty() && told.elapsed() >= Duration::from_millis(150) && !pull_cancelled(run) {
                    told = std::time::Instant::now();
                    let (total, got) = layers.values().fold((0, 0), |(t, g), (size, done)| (t + size, g + done));
                    emit(serde_json::json!({"type": "pull", "state": "progress", "name": reference, "got": got, "total": total}).to_string());
                }
            });
            // Shut by Cancel before the engine said it was done: cancelled,
            // whether the read ended in an error or looked like a clean end.
            if !finished && pull_cancelled(run) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let status = status?;
            match error {
                Some(e) => Err(io::Error::other(e)),
                None if (200..300).contains(&status) => Ok(()),
                None => Err(io::Error::other(format!("pull failed (HTTP {status})"))),
            }
        });
        let cancelled = result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::Interrupted && pull_cancelled(run));
        pulling().retain(|p| Some(p.run) != run);
        // A failed pull says why on its own row in Images, so the error
        // rides on its done event rather than a notice of its own.
        let mut done = serde_json::json!({"type": "pull", "state": "done", "name": reference});
        if cancelled {
            diag::info!("pull cancelled", image = reference);
            done["cancelled"] = true.into();
        } else if let Err(e) = &result {
            let text = pull_error(&e.to_string(), &from, &tag);
            diag::warn!("error shown", text = text);
            done["error"] = text.into();
            if let Some(op) = diag::op() {
                done["op"] = op.into();
            }
        }
        emit(done.to_string());
        if result.is_ok() {
            emit(pulled(&reference, began.elapsed().as_millis()));
        }
    });
    if let Err(e) = spawned {
        emit(notice("error", &e.to_string()));
    }
}

/// A name typed into Pull image that doesn't exist, said plainly instead
/// of as the registry's reply (or the raw reply an older engine passes on).
fn pull_error(e: &str, from: &str, tag: &str) -> String {
    let raw =
        |code: &str| e.contains("/manifests/") && e.contains(&format!(" returned HTTP {code}"));
    if e.starts_with("pull access denied for ")
        || ((raw("401") || raw("403")) && !e.contains("incorrect username or password"))
    {
        format!(
            "Couldn't find an image called {from}. Check the name; if it's private, sign in with docker login."
        )
    } else if (e.starts_with("manifest for ") && e.ends_with("not found: manifest unknown"))
        || raw("404")
    {
        format!("{from} has no tag {tag}.")
    } else {
        e.to_owned()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_name_that_isnt_there_says_so() {
        use super::pull_error;
        assert_eq!(
            pull_error(
                "pull access denied for gall, repository does not exist or may require 'docker login'",
                "gall",
                "latest"
            ),
            "Couldn't find an image called gall. Check the name; if it's private, sign in with docker login."
        );
        assert_eq!(
            pull_error(
                "manifest for alpine:nope not found: manifest unknown",
                "alpine",
                "nope"
            ),
            "alpine has no tag nope."
        );
        let old = r#"https://registry-1.docker.io/v2/library/gall/manifests/latest returned HTTP 401: {"errors":[{"code":"UNAUTHORIZED"}]}"#;
        assert_eq!(
            pull_error(old, "gall", "latest"),
            "Couldn't find an image called gall. Check the name; if it's private, sign in with docker login."
        );
        assert_eq!(
            pull_error(
                "https://registry-1.docker.io/v2/library/alpine/manifests/nope returned HTTP 404",
                "alpine",
                "nope"
            ),
            "alpine has no tag nope."
        );
        assert_eq!(
            pull_error("network error: timed out", "alpine", "latest"),
            "network error: timed out"
        );
    }

    #[test]
    fn page_command_ids_may_be_numbers() {
        let msg: super::PageCmd = serde_json::from_str(r#"{"cmd":"unshare","id":7}"#).unwrap();
        assert_eq!(msg.id, "7");
        let msg: super::PageCmd = serde_json::from_str(r#"{"cmd":"unshare","id":"8"}"#).unwrap();
        assert_eq!(msg.id, "8");
        let msg: super::PageCmd = serde_json::from_str(r#"{"cmd":"refresh"}"#).unwrap();
        assert_eq!(msg.id, "");
    }

    #[test]
    fn run_command_splits_like_a_simple_shell() {
        assert!(super::split_command("").is_empty());
        assert!(super::split_command("   ").is_empty());
        assert_eq!(super::split_command("sleep 3600"), ["sleep", "3600"]);
        assert_eq!(
            super::split_command(r#"nginx -g "daemon off;""#),
            ["nginx", "-g", "daemon off;"]
        );
        assert_eq!(
            super::split_command("sh -c 'echo hi'"),
            ["sh", "-c", "echo hi"]
        );
        assert_eq!(
            super::split_command("sleep \\\n  3600"),
            ["sleep", "3600"]
        );
        assert_eq!(
            super::split_command("-itd \\\n  -v /data:/data \\\n  alpine"),
            ["-itd", "-v", "/data:/data", "alpine"]
        );
    }

    #[test]
    fn docker_run_line_looks_like_cli() {
        let mut bindings = serde_json::Map::new();
        bindings.insert(
            "80/tcp".into(),
            serde_json::json!([{"HostIp": "", "HostPort": "8080"}]),
        );
        let line = super::docker_run_line(
            "nginx:alpine",
            &["FOO=bar baz".into()],
            "web",
            &bindings,
            "sleep 10",
        );
        assert_eq!(
            line,
            "docker run -d -it -P \\\n  --name web \\\n  -e 'FOO=bar baz' \\\n  -p 8080:80 \\\n  nginx:alpine \\\n  sleep 10"
        );
    }

    use super::brand;

    #[test]
    fn finds_brands_by_repository() {
        assert!(brand("nginx:alpine").is_some());
        assert!(brand("bitnami/redis:7").is_some());
        assert!(brand("postgres@sha256:abc").is_some());
        assert!(brand("localhost:5000/nginx:1").is_some());
        assert!(brand("something-unknown:latest").is_none());
    }

    #[test]
    fn saves_known_prefs_only() {
        let dir = std::env::temp_dir().join(format!("kiri-prefs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("run")).unwrap();
        // SAFETY: the only test that reads or writes KIRI_SOCKET.
        unsafe { std::env::set_var("KIRI_SOCKET", dir.join("run/docker.sock")) };
        super::set_pref("font", "Georgia".into());
        super::set_pref("scale", 1.1.into());
        super::set_pref("script", "x".into());
        super::set_pref("starAsked", true.into());
        super::set_pref(
            "listLayout",
            serde_json::json!({"containers": "grid", "images": "list"}),
        );
        let p = super::prefs();
        assert_eq!(
            (p["font"].as_str(), p["scale"].as_f64(), p.get("script")),
            (Some("Georgia"), Some(1.1), None)
        );
        assert_eq!(p["starAsked"], true);
        assert_eq!(p["listLayout"]["containers"], "grid");
        assert_eq!(p["listLayout"]["images"], "list");
        // Check for updates automatically stays off across launches.
        super::set_pref("updateAuto", false.into());
        assert_eq!(super::prefs()["updateAuto"], false);
        // Crash reports remember when they were turned on, and only the
        // setting itself can say so.
        super::set_pref("crashReportsSince", 1.into());
        assert_eq!(super::prefs().get("crashReportsSince"), None);
        super::set_pref("crashReports", true.into());
        let since = super::prefs()["crashReportsSince"].as_u64().unwrap();
        assert!(since > 1);
        std::fs::write(
            dir.join("ui.json"),
            super::prefs().to_string().replace(&since.to_string(), "5"),
        )
        .unwrap();
        super::set_pref("crashReports", true.into());
        assert_eq!(
            super::prefs()["crashReportsSince"],
            5,
            "already on: its time stays"
        );
        super::set_pref("crashReports", false.into());
        assert_eq!(super::prefs().get("crashReportsSince"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_installed_fonts() {
        let all = super::window::font_families(false);
        let mono = super::window::font_families(true);
        // Fonts every Mac has; on Linux, whatever fontconfig lists.
        if cfg!(target_os = "macos") {
            assert!(all.iter().any(|f| f == "Helvetica Neue"), "{all:?}");
            assert!(mono.iter().any(|f| f == "Menlo"), "{mono:?}");
            assert!(!mono.iter().any(|f| f == "Helvetica Neue"));
        } else {
            assert!(
                !all.is_empty() && !mono.is_empty() && mono.len() < all.len(),
                "{all:?} {mono:?}"
            );
        }
        assert!(all.iter().chain(&mono).all(|f| !f.starts_with('.')));
        assert!(
            all.windows(2)
                .all(|w| w[0].to_lowercase() < w[1].to_lowercase())
        );
    }

    #[test]
    fn catalog_is_sorted_for_binary_search() {
        assert!(super::brands::IMAGES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(super::brands::BRANDS.windows(2).all(|w| w[0].0 < w[1].0));
    }
}

/// A command from the page. `autostart` and `view` are the window's: the
/// page's "ready" acts on them.
fn handle(text: &str, tx: &Sender<Cmd>, autostart: bool, view: Option<&str>) {
    let msg = match serde_json::from_str::<PageCmd>(text) {
        Ok(m) => m,
        // Said, not dropped quietly: a command the page sent wrong did nothing.
        Err(e) => return diag::warn!("page command not understood", error = e),
    };
    let _ = match msg.cmd.as_str() {
        "start" => tx.send(Cmd::Start),
        "stop" => tx.send(Cmd::Stop),
        "refresh" => tx.send(Cmd::Refresh),
        "pull" => {
            asked_to_pull(&msg.name);
            tx.send(Cmd::Pull(msg.name))
        }
        // Cancel on a pull's row.
        "cancelpull" => {
            cancel_pull(&msg.name);
            Ok(())
        }
        // Pull image's suggestions, from Docker Hub through a cache.
        "hubsearch" => {
            hub::ask(hub::Kind::Search, msg.name, emit);
            Ok(())
        }
        "hubtags" => {
            hub::ask(hub::Kind::Tags, msg.name, emit);
            Ok(())
        }
        "rmi" => {
            let with = msg
                .value
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            tx.send(Cmd::RemoveImage(msg.name, with))
        }
        "run" => {
            let env = msg
                .value
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let host_port = u16::try_from(msg.port).ok().filter(|&p| p > 0);
            let target_port = u16::try_from(msg.target_port).ok().filter(|&p| p > 0);
            tx.send(Cmd::Run(RunArgs {
                image: msg.name,
                env,
                container: msg.container,
                host_port,
                target_port,
                command: msg.command,
                binds: msg.binds,
                entrypoint: msg.entrypoint,
                restart: msg.restart,
                start_command: msg.start_command,
                network: msg.network,
            }))
        }
        "container" => tx.send(Cmd::Container {
            action: msg.action,
            id: msg.id,
        }),
        "rmvol" => tx.send(Cmd::RemoveVolume(msg.name)),
        "rmnet" => tx.send(Cmd::RemoveNetwork(msg.id)),
        "ports" => tx.send(Cmd::Ports {
            id: msg.id,
            ports: msg.ports,
        }),
        // A container's page: what it listens on, when it publishes nothing.
        "listening" => {
            std::thread::spawn(move || {
                if let Some(ports) = listening(&msg.id) {
                    emit(listening_event(&msg.id, "", &ports, false));
                }
            });
            Ok(())
        }
        "env" => {
            let env = msg
                .value
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            tx.send(Cmd::Env { id: msg.id, env })
        }
        // Settings: font lists, read off the main thread so the window keeps drawing.
        "fonts" => {
            std::thread::spawn(|| {
                emit(serde_json::json!({"type": "fonts", "families": window::font_families(false), "mono": window::font_families(true)}).to_string())
            });
            Ok(())
        }
        // About's Check now.
        "updatecheck" => {
            std::thread::spawn(|| update_check(true));
            Ok(())
        }
        // The update card and the status bar's update chip.
        "updatedownload" => {
            update_download();
            Ok(())
        }
        "updatecancel" => {
            UPDATE_CANCEL.store(true, Ordering::Relaxed);
            Ok(())
        }
        "updaterestart" => {
            std::thread::spawn(update_restart);
            Ok(())
        }
        // What's new: the feed names the page, so only runkiri.com's.
        "updatenotes" => {
            if msg
                .name
                .strip_prefix("https://runkiri.com/")
                .is_some_and(|rest| {
                    rest.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~/#".contains(&b))
                })
            {
                open(&msg.name);
            }
            Ok(())
        }
        "restartengine" => {
            std::thread::spawn(|| {
                let began = std::time::Instant::now();
                update::restart_engine();
                emit(r#"{"type":"enginestale","version":null}"#.to_owned());
                // The page's engine toast (as Windows sends after its restart).
                emit(serde_json::json!({"type": "enginerestarted", "version": env!("CARGO_PKG_VERSION"), "ms": began.elapsed().as_millis() as u64}).to_string());
            });
            Ok(())
        }
        "setpref" => {
            if msg.name == "theme" {
                window::set_appearance(msg.value.as_str().unwrap_or("system"));
            }
            set_pref(&msg.name, msg.value);
            Ok(())
        }
        "about" => tx.send(Cmd::About),
        "domains" => tx.send(Cmd::Domains(msg.value.as_bool().unwrap_or(false))),
        // Settings → Domains → HTTPS: macOS asks for the password.
        "https" => {
            let on = msg.value.as_bool().unwrap_or(false);
            std::thread::spawn(move || {
                let result = if on {
                    crate::daemon::tls::trust()
                } else {
                    crate::daemon::tls::untrust()
                };
                match result {
                    Ok(()) if on => emit(notice(
                        "ok",
                        "Kiri's certificate is trusted: containers open over HTTPS",
                    )),
                    Ok(()) => emit(notice("ok", "Kiri's certificate is no longer trusted")),
                    Err(e)
                        if e.to_string().contains("canceled")
                            || e.to_string().contains("cancelled") => {}
                    Err(e) => emit(notice(
                        "error",
                        &format!("Could not change the certificate's trust: {e}"),
                    )),
                }
                emit(domains_event());
            });
            Ok(())
        }
        // The Builds view: BuildKit's history, read only while the view is open.
        "builds" => {
            std::thread::spawn(|| {
                emit(
                    match crate::builds::list() {
                        Ok(b) => serde_json::json!({"type": "builds", "builds": b}),
                        Err(e) => serde_json::json!({"type": "builds", "error": e.to_string()}),
                    }
                    .to_string(),
                )
            });
            Ok(())
        }
        "build" => {
            let (builder, id) = (msg.name, msg.id);
            std::thread::spawn(move || {
                emit(match crate::builds::detail(&builder, &id) {
                    Ok(mut b) => {
                        b["type"] = "build".into();
                        b
                    }
                    Err(e) => serde_json::json!({"type": "build", "builder": builder, "id": id, "error": {"message": e.to_string()}, "steps": [], "failed": true}),
                }.to_string())
            });
            Ok(())
        }
        // Security: scan an image (name or ID) with Grype, or show its last scan.
        "scan" => {
            let name = msg.name;
            std::thread::spawn(move || {
                let mut last = (String::new(), 255u8);
                let r = crate::scan::scan(&name, &mut |phase, pct| {
                    if (phase, pct) != (last.0.as_str(), last.1) {
                        last = (phase.to_owned(), pct);
                        emit(serde_json::json!({"type": "scanprogress", "name": name, "phase": phase, "pct": pct}).to_string());
                    }
                });
                emit(match r {
                    Ok(res) => serde_json::json!({"type": "scanresult", "name": name, "result": res}),
                    Err(e) => serde_json::json!({"type": "scanresult", "name": name, "error": e.to_string()}),
                }.to_string())
            });
            Ok(())
        }
        // An image's scan history, or one scan of it (`id` its time).
        "scanhistory" => {
            let name = msg.name;
            std::thread::spawn(move || {
                emit(match crate::scan::history(&name) {
                    Ok(h) => serde_json::json!({"type": "scanhistory", "name": name, "scans": h["scans"]}),
                    Err(e) => serde_json::json!({"type": "scanhistory", "name": name, "error": e.to_string()}),
                }.to_string())
            });
            Ok(())
        }
        "scanopen" => {
            let (name, at) = (msg.name, msg.id.parse::<u64>().ok());
            std::thread::spawn(move || {
                emit(match crate::scan::open(&name, at) {
                    Ok(res) => serde_json::json!({"type": "scanresult", "name": name, "result": res}),
                    Err(e) => serde_json::json!({"type": "scanresult", "name": name, "error": e.to_string()}),
                }.to_string())
            });
            Ok(())
        }
        "savesbom" => {
            match crate::scan::save_sbom(&msg.name, msg.id.parse().unwrap_or(0), &msg.path) {
                Ok(to) => {
                    emit(notice(
                        "ok",
                        &format!(
                            "Saved the SBOM to Downloads as {}",
                            to.file_name().unwrap_or_default().to_string_lossy()
                        ),
                    ));
                    reveal(&to);
                }
                Err(e) => emit(notice("error", &format!("Could not save the SBOM: {e}"))),
            }
            Ok(())
        }
        // Share: public links for a port or container, and the Share account.
        "shares" => {
            sharing::list();
            Ok(())
        }
        "share" => {
            sharing::open(msg.name, msg.value.as_str().unwrap_or_default().to_owned());
            Ok(())
        }
        // A Kubernetes service or pod: value {ip, port, label}.
        "k8sshare" => {
            let v = &msg.value;
            let port = v["port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or(0);
            sharing::open_k8s(
                msg.name,
                v["ip"].as_str().unwrap_or_default().to_owned(),
                port,
                v["label"].as_str().unwrap_or_default().to_owned(),
                v["name"].as_str().unwrap_or_default().to_owned(),
            );
            Ok(())
        }
        "shareusage" => {
            sharing::usage();
            Ok(())
        }
        // The account's live tunnels from every computer, and closing one this window didn't open.
        // What the window costs, for the status bar.
        "usage" => {
            std::thread::spawn(|| emit(usage::event()));
            Ok(())
        }
        "sharetraffic" => {
            sharing::traffic();
            Ok(())
        }
        "sharetunnels" => {
            sharing::tunnels();
            Ok(())
        }
        "sharestop" => {
            sharing::stop_tunnel(msg.name);
            Ok(())
        }
        "sharebrowse" => {
            sharing::browse_tunnel(&msg.name);
            Ok(())
        }
        "unshare" => {
            sharing::close(msg.id.parse().unwrap_or(0));
            Ok(())
        }
        "shareopen" => {
            sharing::browse(msg.id.parse().unwrap_or(0));
            Ok(())
        }
        "sharelogin" => {
            sharing::sign_in();
            Ok(())
        }
        "sharecancel" => {
            sharing::cancel_sign_in();
            Ok(())
        }
        "sharelogout" => {
            sharing::sign_out();
            Ok(())
        }
        // The built-in terminal: its shells, and its own clipboard and links.
        "term" => {
            match msg.name.as_str() {
                // A selection can be a whole screen of output.
                "copy" => {
                    if let Some(text) = msg
                        .value
                        .as_str()
                        .filter(|t| !t.is_empty() && t.len() <= termwire::COPY_MAX)
                    {
                        window::copy(text);
                    }
                }
                "paste" => {
                    let id = msg.id;
                    window::paste(move |text| emit(termwire::paste(&id, &text)));
                }
                // A link in a shell's output: web pages only, never a file or an app's scheme.
                "link" => {
                    if let Some(url) = msg.value.as_str().filter(|u| termwire::web_link(u)) {
                        open(url);
                    }
                }
                action => term::handle(action, &msg.id, &msg.value, emit),
            }
            Ok(())
        }
        // The page can't write the clipboard itself; IDs and short text only.
        "copy" => {
            if !msg.name.is_empty() && msg.name.len() <= 4096 {
                window::copy(&msg.name);
            }
            Ok(())
        }
        "advisory" => {
            crate::scan::open_advisory(&msg.name);
            Ok(())
        }
        // Images → an image's layers.
        "imagelayers" => {
            let name = msg.name;
            std::thread::spawn(move || {
                emit(match crate::builds::image_layers(&name) {
                    Ok(mut l) => {
                        l["type"] = "imagelayers".into();
                        l
                    }
                    Err(e) => serde_json::json!({"type": "imagelayers", "name": name, "error": e.to_string()}),
                }.to_string())
            });
            Ok(())
        }
        // Builds → Layers: the image a build made.
        "buildlayers" => {
            let (builder, id) = (msg.name, msg.id);
            std::thread::spawn(move || {
                emit(match crate::builds::layers(&builder, &id) {
                    Ok(mut l) => {
                        l["type"] = "buildlayers".into();
                        l
                    }
                    Err(e) => serde_json::json!({"type": "buildlayers", "builder": builder, "id": id, "error": e.to_string()}),
                }.to_string())
            });
            Ok(())
        }
        "rmbuild" => {
            let (builder, id) = (msg.name, msg.id);
            std::thread::spawn(move || {
                if let Err(e) = crate::builds::remove(&builder, &id) {
                    emit(notice("error", &format!("Couldn't remove the build: {e}")));
                }
                emit(
                    match crate::builds::list() {
                        Ok(b) => serde_json::json!({"type": "builds", "builds": b}),
                        Err(e) => serde_json::json!({"type": "builds", "error": e.to_string()}),
                    }
                    .to_string(),
                )
            });
            Ok(())
        }
        // Which engine plain `docker` uses: Kiri, or back to the one before.
        "usecontext" => {
            let result = setup::use_context(&msg.name);
            emit(status_event());
            emit(serde_json::json!({"type": "usedcontext", "name": msg.name, "error": result.as_ref().err().map(|e| e.to_string())}).to_string());
            Ok(())
        }
        // Settings → Builds: clear Kiri's builder cache (`docker builder
        // prune --all`), which Images and Volumes don't list.
        "buildprune" => {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let e = match client::request("POST", "/build/prune?all=1", None) {
                    Ok(r) if r.ok() => {
                        serde_json::json!({"type": "buildpruned", "freed": r.json()["SpaceReclaimed"]})
                    }
                    Ok(r) => serde_json::json!({"type": "buildpruned", "error": r.error()}),
                    Err(e) => serde_json::json!({"type": "buildpruned", "error": e.to_string()}),
                };
                emit(e.to_string());
                let _ = tx.send(Cmd::Refresh);
            });
            Ok(())
        }
        // Settings → Builds, and the first-run question: which builder
        // plain `docker build` uses. With a value, switches to it.
        "buildwith" => {
            let with = msg.value.as_str().and_then(crate::buildwith::With::parse);
            std::thread::spawn(move || {
                if let Some(with) = with {
                    match crate::buildwith::choose(with) {
                        // Kiri's builder made now if it wasn't yet; it takes the choice.
                        Ok(()) => setup::ensure_builder(),
                        Err(e) => emit(notice(
                            "error",
                            &format!("Couldn't change which builder docker build uses: {e}"),
                        )),
                    }
                }
                emit(crate::buildwith::event());
            });
            Ok(())
        }
        // Settings → Import, from OrbStack or Docker Desktop.
        "importsources" => {
            std::thread::spawn(|| {
                emit(
                    serde_json::json!({"type": "importsources", "sources": import::sources()})
                        .to_string(),
                )
            });
            Ok(())
        }
        "inventory" => {
            let source = msg.name;
            std::thread::spawn(move || {
                let event = match import::inventory(&source) {
                    Ok(mut v) => {
                        v["type"] = "inventory".into();
                        v
                    }
                    Err(e) => {
                        serde_json::json!({"type": "inventory", "source": source, "error": e.to_string()})
                    }
                };
                emit(event.to_string());
            });
            Ok(())
        }
        "import" => {
            let (source, picked, tx) = (msg.name, msg.value, tx.clone());
            std::thread::spawn(move || {
                let (mut done, mut failed) = (0, 0);
                for (kind, f) in [
                    ("images", import::image as fn(&str, &str) -> io::Result<()>),
                    ("volumes", import::volume),
                    ("containers", import::container),
                ] {
                    for name in picked[kind]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str())
                    {
                        emit(serde_json::json!({"type": "importstep", "kind": kind, "name": name, "state": "running"}).to_string());
                        match f(&source, name) {
                            Ok(()) => {
                                done += 1;
                                emit(serde_json::json!({"type": "importstep", "kind": kind, "name": name, "state": "done"}).to_string());
                            }
                            Err(e) => {
                                failed += 1;
                                emit(serde_json::json!({"type": "importstep", "kind": kind, "name": name, "state": "error", "error": e.to_string()}).to_string());
                            }
                        }
                    }
                }
                emit(
                    serde_json::json!({"type": "importdone", "done": done, "failed": failed})
                        .to_string(),
                );
                let _ = tx.send(Cmd::Refresh);
            });
            Ok(())
        }
        // Settings → Network: the proxy.
        "proxy" => {
            std::thread::spawn(|| proxy(None));
            Ok(())
        }
        "setproxy" => {
            let body = msg.value;
            std::thread::spawn(move || proxy(Some(body)));
            Ok(())
        }
        // Settings → Network: registry mirrors (`~/.kiri/daemon.json`).
        "daemon" => {
            std::thread::spawn(|| daemon(None));
            Ok(())
        }
        "setdaemon" => {
            let body = msg.value;
            std::thread::spawn(move || daemon(Some(body)));
            Ok(())
        }
        // Settings → Resources.
        "resources" => {
            std::thread::spawn(|| resources(None));
            Ok(())
        }
        "setresources" => {
            let body = msg.value;
            std::thread::spawn(move || resources(Some(body)));
            Ok(())
        }
        "restartvm" => {
            std::thread::spawn(|| {
                let _op = diag::enter_op(diag::new_op("restartvm"));
                let result = expect_ok(client::request("POST", "/kiri/sleep", None))
                    .and_then(|()| expect_ok(client::request("POST", "/kiri/wake", None)));
                match result {
                    Ok(()) => emit(notice(
                        "ok",
                        "The Linux VM restarted with the new resources",
                    )),
                    Err(e) => emit(notice("error", &format!("Could not restart the VM: {e}"))),
                }
                resources(None);
            });
            Ok(())
        }
        // The Kubernetes section: Pods, Services, a pod's logs, Start.
        "k8s" => {
            k8s::handle(&msg.action, &msg.value);
            Ok(())
        }
        // A container's page.
        "inspect" => {
            detail::inspect(msg.id);
            Ok(())
        }
        "logs" => {
            detail::logs(msg.id);
            Ok(())
        }
        "toolsretry" => {
            setup_tools();
            Ok(())
        }
        "logsstop" => {
            detail::stop_logs();
            Ok(())
        }
        // Registries signed in to, signing in (checked by the engine
        // first) and out.
        "logins" | "login" | "logout" => {
            logins::handle(&msg.cmd, msg.name, &msg.value, emit, ensure_daemon);
            Ok(())
        }
        // Before Run: what the image's start script won't start without.
        "needs" => {
            let image = msg.name;
            std::thread::spawn(move || emit(needs::event(&image)));
            Ok(())
        }
        // Personal Markdown notes under ~/.kiri/notes, plus per-image remarks.
        "notes" | "noteread" | "notesave" | "notedelete" | "notemkdir" | "notermdir" | "noterename" | "notemove" | "notereveal"
        | "imgnotes" | "imgnote" | "imgnotesave" => {
            let (cmd, id, value) = (
                msg.cmd.clone(),
                if msg.id.is_empty() {
                    msg.name.clone()
                } else {
                    msg.id.clone()
                },
                msg.value,
            );
            std::thread::spawn(move || {
                notes::handle(&cmd, &id, &value, state_dir(), emit);
            });
            Ok(())
        }
        // A container that stopped: its last lines, for why.
        "lastlogs" => {
            detail::last_logs(msg.id);
            Ok(())
        }
        "stats" => {
            detail::stats(msg.id);
            Ok(())
        }
        "statsstop" => {
            detail::stop_stats();
            Ok(())
        }
        "exec" => {
            detail::exec(msg.id, msg.path);
            Ok(())
        }
        "terminal" => {
            if let Err(e) = detail::terminal(&msg.name) {
                emit(notice("error", &format!("Could not open Terminal: {e}")));
            }
            Ok(())
        }
        "files" => {
            detail::files(msg.name, msg.path);
            Ok(())
        }
        "preview" => {
            detail::preview(msg.name, msg.path);
            Ok(())
        }
        "download" => {
            match detail::download(&msg.name, &msg.path) {
                Ok(to) => {
                    emit(notice(
                        "ok",
                        &format!(
                            "Saved to Downloads as {}",
                            to.file_name().unwrap_or_default().to_string_lossy()
                        ),
                    ));
                    reveal(&to);
                }
                Err(e) => emit(notice("error", &format!("Could not save the file: {e}"))),
            }
            Ok(())
        }
        "reveal" => {
            if let Err(e) = detail::reveal(&msg.name, &msg.path) {
                emit(notice("error", &e.to_string()));
            }
            Ok(())
        }
        // Only the container's own addresses: a page cannot open arbitrary URLs.
        "open" => {
            let local = msg
                .name
                .strip_prefix("http://")
                .or_else(|| msg.name.strip_prefix("https://"))
                .and_then(|r| r.split(['/', ':']).next())
                .is_some_and(|h| {
                    h == "localhost" || h.ends_with(".kiri.localhost") || h.ends_with(".kiri.local")
                });
            // A build warning's rule page (docs.docker.com/go/dockerfile/rule/…).
            let docs = msg
                .name
                .strip_prefix("https://docs.docker.com/go/")
                .is_some_and(|rest| {
                    rest.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-/".contains(&b))
                });
            if local
                || docs
                || msg.name == "https://runkiri.com"
                || msg.name == "https://runkiri.com/share/notice"
                || msg.name == "https://canestack.com"
                || msg.name == "https://github.com/libracoder/kiri/issues/new"
                || msg.name == "https://github.com/libracoder/kiri"
                || msg.name == "https://github.com/anchore/grype"
            {
                open(&msg.name);
            }
            Ok(())
        }
        // The page covers the title bar, and WebKit ignores CSS drag
        // regions, so the page asks for the drag itself on mouse-down
        // over empty chrome. The message lands after the mouse-down:
        // if the button is already up (a quick click), AppKit's
        // current event is the mouse-up, and a drag started then waits
        // for a release that already happened, holding the window
        // until the next click.
        "drag" => {
            window::drag();
            Ok(())
        }
        // ⌘W when the built-in terminal doesn't have the focus.
        #[cfg(target_os = "macos")]
        "closewindow" => {
            window::close_window();
            Ok(())
        }
        "zoom" => {
            window::zoom();
            Ok(())
        }
        // Diagnostics, for the error Details and Settings → Diagnostics:
        // the log (`name` a part or "", `id` an op, `value`
        // {errors, n}), a report saved to the Desktop, or sent
        // (`value` {containers} adds the container list), the
        // recent crash files, one of them sent (`name`), and the
        // logs folder in Finder.
        "diaglogs" => {
            let (part, op) = (msg.name, msg.id);
            let (errors, n) = (
                msg.value["errors"] == true,
                msg.value["n"].as_u64().unwrap_or(200) as usize,
            );
            std::thread::spawn(move || {
                emit(diag::report::page_lines(&part, errors, &op, n).to_string())
            });
            Ok(())
        }
        "diagreport" => {
            let containers = msg.value["containers"] == true;
            std::thread::spawn(move || {
                let saved = diag::report::report_text(containers)
                    .and_then(|text| diag::report::save_report(&text, None));
                emit(match saved {
                    Ok(path) => {
                        diag::report::reveal(&path);
                        serde_json::json!({"type": "diagreport", "path": diag::redact::Redactor::home_only().apply(&path.display().to_string())})
                    }
                    Err(e) => serde_json::json!({"type": "diagreport", "error": e.to_string()}),
                }.to_string())
            });
            Ok(())
        }
        "diagsend" => {
            let containers = msg.value["containers"] == true;
            std::thread::spawn(move || {
                let _op = diag::enter_op(diag::new_op("diagsend"));
                let sent = diag::report::report_text(containers).and_then(|text| {
                    diag::report::send(
                        "report",
                        diag::Part::Ui,
                        std::time::SystemTime::now(),
                        &text,
                    )
                });
                emit(
                    match sent {
                        Ok(id) => {
                            diag::info!("report sent", id = id);
                            serde_json::json!({"type": "diagsend", "id": id})
                        }
                        Err(e) => {
                            diag::warn!("report not sent", error = e);
                            serde_json::json!({"type": "diagsend", "error": e.to_string()})
                        }
                    }
                    .to_string(),
                )
            });
            Ok(())
        }
        "diagcrashes" => {
            std::thread::spawn(|| emit(diag::report::page_crashes(10).to_string()));
            Ok(())
        }
        "diagsendcrash" => {
            let name = msg.name;
            std::thread::spawn(move || {
                let _op = diag::enter_op(diag::new_op("diagsend"));
                emit(match diag::report::send_crash(&name) {
                    Ok(id) => serde_json::json!({"type": "diagsendcrash", "name": name, "id": id}),
                    Err(e) => {
                        diag::warn!("crash report not sent", file = name, error = e);
                        serde_json::json!({"type": "diagsendcrash", "name": name, "error": e.to_string()})
                    }
                }.to_string())
            });
            Ok(())
        }
        "diagfolder" => {
            if let Some(dir) = diag::logs_dir() {
                let _ = std::fs::create_dir_all(&dir);
                open(&dir);
            }
            Ok(())
        }
        // The page is loaded, so events sent from now on will land.
        "ready" => {
            if let Some(v) = view {
                let mut ev = String::from(r#"{"type":"view","view":"#);
                json_str(&mut ev, v);
                ev.push('}');
                emit(ev);
            }
            emit(serde_json::json!({"type": "prefs", "prefs": prefs()}).to_string());
            // KIRI_STAR_PROMPT=1 shows the star card now, asked before or not (for testing).
            if std::env::var_os("KIRI_STAR_PROMPT").is_some() {
                emit(r#"{"type":"starprompt"}"#.to_owned());
            }
            // Kiri Lite: fetch the docker CLI, Compose and buildx the
            // first time, unless this Mac already has a docker CLI.
            if crate::tools::lite() && !crate::tools::installed() && !other_docker_cli() {
                setup_tools();
            }
            emit(domains_event());
            emit(status_event());
            // A crash since the window last opened, for the page to
            // offer a report; crash files go if the person allowed it.
            std::thread::spawn(|| {
                if let Some(crash) = diag::report::last_crash() {
                    emit(crash.to_string());
                }
            });
            diag::report::spawn_crash_sender(Duration::from_secs(5), None);
            // A newer release, checked when the window opens and every 6
            // hours after (recheck.rs), quietly; one already downloaded; an
            // engine still on an older build.
            std::thread::spawn(|| update_check(false));
            recheck::start(|| feed_check(false));
            // The first time the app opens: ask, once, whether to use *.kiri.local (the Mac only, so far).
            if cfg!(target_os = "macos")
                && !crate::flavor::dev()
                && !crate::daemon::dns::installed()
                && prefs()["domainsAsked"] != true
            {
                set_pref("domainsAsked", true.into());
                emit(r#"{"type":"welcome"}"#.to_owned());
            }
            // Another engine is here and nobody has chosen: ask once which
            // builder docker build uses (after the welcome, if that shows).
            std::thread::spawn(|| {
                if let Some(engine) = crate::buildwith::ask() {
                    emit(serde_json::json!({"type": "buildask", "engine": engine}).to_string());
                }
                emit(crate::buildwith::event());
            });
            // Linux: Kiri checks it can run containers for this account
            // first; until it can, the page shows the setup checklist and
            // nothing starts (its Open Kiri sends "onbopen").
            #[cfg(target_os = "linux")]
            {
                let checks = linux::onboarding::checks();
                if linux::onboarding::needed(&checks) {
                    emit(checks.to_string());
                    return;
                }
            }
            let _ = tx.send(Cmd::Refresh);
            // Opening the window reaches the engine (starting its service if it
            // isn't running) but doesn't wake it: asleep, it reads Ready, and the
            // next docker command or action wakes it. One someone stopped waits
            // for Start.
            if autostart && !engine_stopped() {
                tx.send(Cmd::Connect)
            } else {
                Ok(())
            }
        }
        // The Linux setup checklist: check again, fix one or "all", open the
        // window once it's ready, or sign in again for the group to apply.
        #[cfg(target_os = "linux")]
        "onbcheck" => {
            emit(linux::onboarding::checks().to_string());
            Ok(())
        }
        #[cfg(target_os = "linux")]
        "onbfix" => {
            let what = msg.name;
            std::thread::spawn(move || {
                let result = linux::onboarding::fix(&what);
                let mut checks = linux::onboarding::checks();
                if let Err(e) = result {
                    diag::warn!(
                        "action failed",
                        action = "onboarding fix",
                        target = what,
                        error = e
                    );
                    checks["error"] = e.to_string().into();
                }
                emit(checks.to_string());
            });
            Ok(())
        }
        // The checklist's Skip (and taking it back) for docker and builds.
        #[cfg(target_os = "linux")]
        "onbskip" => {
            let undo = msg.value.as_bool() == Some(false);
            set_pref("setupSkip", linux::onboarding::skip(&msg.name, undo));
            emit(linux::onboarding::checks().to_string());
            Ok(())
        }
        #[cfg(target_os = "linux")]
        "onbopen" => {
            let _ = tx.send(Cmd::Refresh);
            tx.send(Cmd::Connect)
        }
        #[cfg(target_os = "linux")]
        "onblogout" | "onbrestart" => {
            let result = if msg.cmd == "onblogout" {
                linux::onboarding::log_out()
            } else {
                linux::onboarding::restart()
            };
            if let Err(e) = result {
                emit(notice("error", &e.to_string()));
            }
            Ok(())
        }
        _ => Ok(()),
    };
}
