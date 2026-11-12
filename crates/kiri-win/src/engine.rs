//! The window's engine, as crates/kiri/src/ui/mod.rs is on the Mac: a client
//! of the daemon that answers the page's commands and sends it the lists
//! whenever an event says they changed, so the page never polls. The page's
//! protocol is the Mac app's, unchanged; what macOS does itself (Finder,
//! Terminal, the keychain, /etc/resolver) has a Windows equivalent here or
//! is reported as not available yet.

use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};

use crate::app::{Handle, Menu};
use crate::client;
use crate::detail;

#[path = "../../kiri/src/ui/brands.rs"]
pub(crate) mod brands;
// Share in the window, the Mac's own file: it needs only emit, notice, crate::client and crate::share.
#[path = "../../kiri/src/ui/sharing.rs"]
pub(crate) mod sharing;
// What an image won't start without, the Mac's own file: it needs only crate::client.
#[path = "../../kiri/src/ui/needs.rs"]
mod needs;
// What the window costs, the Mac's own file.
#[path = "../../kiri/src/ui/usage.rs"]
mod usage;
// Signing in to registries, the Mac's own file: it needs only crate::client.
#[path = "../../kiri/src/ui/logins.rs"]
mod logins;

/// How often the VM panel's figures refresh. The lists refresh on events.
const TICK: Duration = Duration::from_secs(2);
/// The lists also come again this often without an event, so "Up 5 min" and
/// "3 min ago" keep counting.
const LIST_EVERY: Duration = Duration::from_secs(30);
/// When the lists last went to the page, by any route.
static LISTED: std::sync::LazyLock<std::sync::Mutex<std::time::Instant>> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::time::Instant::now()));

#[derive(Debug, Clone)]
pub(crate) enum Cmd {
    Start,
    Stop,
    Refresh,
    Pull(String),
    /// Image name/tag, and stopped containers made from it to remove first
    /// (the page's "Remove both" / "Remove all"). Empty when unused.
    RemoveImage(String, Vec<String>),
    /// Image, env from the Run popover, container name, an optional host
    /// port (None = PublishAllPorts only), and optional command line that
    /// replaces the image's `CMD` (empty keeps the image default; stored as
    /// `kiri.start.command`).
    Run(String, Vec<String>, String, Option<u16>, String),
    Container { action: String, id: String },
    RemoveVolume(String),
    RemoveNetwork(String),
    About,
    Ports { id: String, ports: Value },
    Env { id: String, env: Vec<String> },
}

/// A command as the page sends it.
#[derive(serde::Deserialize)]
struct PageCmd {
    cmd: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    ports: Value,
    #[serde(default)]
    path: String,
    #[serde(default)]
    value: Value,
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
}

/// What the engine needs from the rest of kiri-win.
#[derive(Debug, Clone)]
pub struct Env {
    pub distro: Option<String>,
    pub pipe: String,
}

static TX: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);
static ENV: Mutex<Option<Env>> = Mutex::new(None);
/// The tray's view of running containers: id, name.
static RUNNING: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub(crate) fn send(cmd: Cmd) {
    if let Some(tx) = TX.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        let _ = tx.send(cmd);
    }
}

fn env() -> Env {
    ENV.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or(Env { distro: None, pipe: r"\\.\pipe\kiri".into() })
}

/// The WSL distro Kiri's engine runs in, when not the default one.
pub(crate) fn distro() -> Option<String> {
    env().distro
}

/// Starts the engine thread and the event watcher.
pub fn start(e: Env) {
    *ENV.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
    let (tx, rx) = mpsc::channel();
    *TX.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx.clone());
    let _ = std::thread::Builder::new().name("kiri-engine".into()).spawn(move || engine(&rx, tx));
}

/// Delivers one JSON event to the page. Callable from any thread.
pub fn emit(json: String) {
    Handle.eval(format!("window.kiri && kiri.on({json})"));
}

pub fn notice(kind: &str, text: &str) -> String {
    json!({"type": "notice", "kind": kind, "text": text}).to_string()
}

fn state(name: &str, error: Option<&str>) -> String {
    match error {
        Some(e) => json!({"type": "state", "state": name, "error": e}),
        None => json!({"type": "state", "state": name}),
    }
    .to_string()
}

fn engine(rx: &Receiver<Cmd>, tx: Sender<Cmd>) {
    watch(tx);
    // Windows' proxy for the daemon, now and every minute.
    std::thread::spawn(|| loop {
        send_windows_proxy();
        std::thread::sleep(Duration::from_secs(60));
    });
    let mut pending: Option<Cmd> = None;
    loop {
        let cmd = match pending.take() {
            Some(c) => Ok(c),
            None => rx.recv_timeout(TICK),
        };
        match cmd {
            Ok(Cmd::Start) => {
                emit(state("booting", None));
                match ensure_daemon().and_then(|()| wake()) {
                    Ok(()) => {
                        std::thread::spawn(ensure_builder);
                        refresh();
                    }
                    Err(e) => emit(state("stopped", Some(&e.to_string()))),
                }
            }
            // Stop means off: every container stopped, the engine off until
            // Start (nothing wakes it meanwhile), and this window's shares closed.
            Ok(Cmd::Stop) => {
                emit(state("stopping", None));
                sharing::close_all();
                let stopped = match expect_ok(client::request("POST", "/kiri/stop", None)) {
                    // An engine older than Stop: its sleep.
                    Err(e) if e.to_string().contains("404") => expect_ok(client::request("POST", "/kiri/sleep", None)),
                    r => r,
                };
                if let Err(e) = stopped {
                    emit(notice("error", &e.to_string()));
                }
                refresh();
            }
            Ok(Cmd::Refresh) => {
                // Events come in bursts (create, start, …): one refresh per
                // burst. Anything else that arrives meanwhile runs next.
                loop {
                    match rx.recv_timeout(Duration::from_millis(60)) {
                        Ok(Cmd::Refresh) => {}
                        Ok(other) => {
                            pending = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                refresh();
            }
            Ok(Cmd::Pull(name)) => pull(name),
            Ok(Cmd::Run(name, env, container, host_port, command)) => {
                // The page shows its spinner until this says how it went.
                // Name/port conflicts stay on the Run popover (`message`).
                let result = run_image(&name, &env, &container, host_port, command.as_str());
                let message = result.as_ref().err().map(std::string::ToString::to_string);
                emit(json!({"type": "ran", "name": name, "error": message.is_some(), "message": message}).to_string());
            }
            Ok(Cmd::RemoveImage(name, with)) => {
                // The stopped containers made from it go first, never forced: a
                // running one refuses, and then the image stays too.
                let mut blocked = None;
                for id in &with {
                    match client::request("DELETE", &format!("/containers/{}", path_escape(id)), None) {
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
                    None => match client::request("DELETE", &format!("/images/{}", path_escape(&name)), None) {
                        Ok(r) if r.ok() => {
                            // Only untagged when another name still points at it: nothing freed.
                            let deleted = r.json().as_array().is_some_and(|a| a.iter().any(|v| v.get("Deleted").is_some()));
                            emit(json!({"type": "removed", "name": name, "deleted": deleted}).to_string());
                        }
                        Ok(r) => emit(notice("error", &image_refusal(&name, &r.error()))),
                        Err(e) => emit(notice("error", &e.to_string())),
                    },
                }
            }
            Ok(Cmd::Container { action, id }) => {
                let (method, path) = match action.as_str() {
                    "remove" => ("DELETE", format!("/containers/{id}?force=1&v=1")),
                    a @ ("start" | "stop" | "restart" | "pause" | "unpause") => ("POST", format!("/containers/{id}/{a}")),
                    _ => continue,
                };
                let result = expect_ok(client::request(method, &path, None));
                // Refused to start (a port taken): the container's page says why.
                if let (Err(e), "start") = (&result, action.as_str()) {
                    emit(json!({"type": "startfailed", "id": id, "error": e.to_string()}).to_string());
                }
                act(result);
            }
            Ok(Cmd::RemoveVolume(name)) => act(expect_ok(client::request("DELETE", &format!("/volumes/{}", path_escape(&name)), None))),
            Ok(Cmd::RemoveNetwork(id)) => act(expect_ok(client::request("DELETE", &format!("/networks/{id}"), None))),
            Ok(Cmd::Ports { id, ports }) => act(set_ports(&id, ports)),
            Ok(Cmd::Env { id, env }) => act(set_env(&id, env)),
            Ok(Cmd::About) => about(),
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                vm_panel();
                if LISTED.lock().unwrap_or_else(|e| e.into_inner()).elapsed() >= LIST_EVERY {
                    refresh();
                }
            }
        }
    }
}

/// Follows the daemon's event stream on its own thread; each event asks
/// the engine for a refresh. Reconnects when the daemon comes back.
fn watch(tx: Sender<Cmd>) {
    let _ = std::thread::Builder::new().name("kiri-ui-events".into()).spawn(move || loop {
        if client::alive() {
            if tx.send(Cmd::Refresh).is_err() {
                return;
            }
            // A Kubernetes node already running gets its kubectl context.
            crate::k8s::sync_soon();
            let _ = client::events(|_| tx.send(Cmd::Refresh).is_ok());
        }
        std::thread::sleep(Duration::from_secs(1));
    });
}

pub fn expect_ok(r: io::Result<client::Response>) -> io::Result<()> {
    let r = r?;
    if r.ok() { Ok(()) } else { Err(io::Error::other(r.error())) }
}

/// A container's name, for messages; its short id if it has none.
fn container_name(id: &str) -> String {
    client::get(&format!("/containers/{}/json", path_escape(id)))
        .ok()
        .and_then(|c| c["Name"].as_str().map(|n| n.trim_start_matches('/').to_owned()))
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
        Some(id) => format!("{name} is still used by the container {}. Remove the container first.", container_name(id)),
        None => error.to_owned(),
    }
}

fn act(result: io::Result<()>) {
    if let Err(e) = result {
        emit(notice("error", &e.to_string()));
    }
}

pub(crate) fn path_escape(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~:/@".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// The daemon's wake; a Linux host has no VM to wake, which is fine.
fn wake() -> io::Result<()> {
    let r = client::request("POST", "/kiri/wake", None)?;
    if r.ok() || r.status == 404 { Ok(()) } else { Err(io::Error::other(r.error())) }
}

/// Makes sure the daemon answers: Ubuntu uses the `kiri` systemd service;
/// the small Kiri Linux distro starts /usr/local/bin/kiri-start instead.
pub(crate) fn ensure_daemon() -> io::Result<()> {
    if client::alive() {
        return Ok(());
    }
    crate::serve::log("engine: not answering; starting it in WSL");
    // The engine this app installed, not one an update left behind.
    match crate::update::sync_engine() {
        Ok(true) => crate::serve::log("engine: copied this version's engine into WSL"),
        Ok(false) => {}
        Err(e) => crate::serve::log(&format!("engine: could not copy the engine into WSL ({e}); starting the one there")),
    }
    let mut cmd = crate::win::tool("wsl.exe");
    if let Some(d) = &env().distro {
        cmd.args(["-d", d]);
    }
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let started = cmd.args(["-u", "root", "--", "systemctl", "start", "kiri"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    if !started.is_ok_and(|s| s.success()) {
        let mut cmd = crate::win::tool("wsl.exe");
        if let Some(d) = &env().distro {
            cmd.args(["-d", d]);
        }
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        let _ = cmd.args(["-u", "root", "--", "/usr/local/bin/kiri-start"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
    for _ in 0..100 {
        if client::alive() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::other("Kiri's engine did not start in WSL: run contrib\\windows\\kiri.ps1 to install it"))
}

/// `docker build` through Kiri, as on the Mac: BuildKit in a Kiri container
/// (buildx's docker-container driver), loading what it builds into Kiri.
/// That container is the daemon's managed builder, made when buildx first
/// looks for it and stopped when idle (crates/kiri/src/daemon/builder.rs);
/// without buildx the CLI's classic POST /build uses the same BuildKit.
/// kiri.ps1 makes it the builder `docker build` uses (BUILDX_BUILDER) when
/// Kiri is the only engine; beside Docker Desktop, `buildx use` makes it the
/// one for the `kiri` context, and `buildx install` makes plain `docker
/// build` use it, as on the Mac (crates/kiri/src/setup.rs `builder`), or
/// it builds with buildx's fallback for the context, which never loads the
/// image into Kiri. Needs the `kiri` context, which kiri.ps1 makes.
fn ensure_builder() {
    const BUILDER: &str = "kiri-build";
    let docker = crate::detail::docker_cli();
    let run = |args: &[&str]| {
        let mut cmd = crate::win::tool(&docker);
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        cmd.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
    };
    if !run(&["context", "inspect", "kiri"]) || !run(&["buildx", "version"]) {
        return;
    }
    if !run(&["buildx", "inspect", BUILDER]) && !run(&["buildx", "create", "--name", BUILDER, "--driver", "docker-container", "--driver-opt", "default-load=true", "kiri"]) {
        crate::serve::log("docker buildx create failed");
        return;
    }
    // `--default`: kept per context, so it stays the builder for `kiri`
    // whatever another context selects later.
    if !run(&["--context", "kiri", "buildx", "use", "--default", BUILDER]) || !run(&["buildx", "install"]) {
        crate::serve::log("docker buildx use or install failed");
    }
}

/// Sends the page every list, from the daemon.
fn refresh() {
    *LISTED.lock().unwrap_or_else(|e| e.into_inner()) = std::time::Instant::now();
    let lists = (|| -> io::Result<Value> {
        let containers = client::get("/containers/json?all=1")?;
        let images = client::get("/images/json")?;
        let volumes = client::get("/volumes")?;
        let networks = client::get("/networks")?;
        let add_icon = |mut v: Value, name_of: &dyn Fn(&Value) -> String| {
            if let Some(list) = v.as_array_mut() {
                for item in list {
                    item["icon"] = json!(brand(&name_of(item)));
                }
            }
            v
        };
        let containers = add_icon(containers, &|c| c["Image"].as_str().unwrap_or_default().to_owned());
        let images = add_icon(images, &|i| i["RepoTags"][0].as_str().unwrap_or_default().to_owned());
        Ok(json!({"type": "lists", "containers": containers, "images": images, "volumes": volumes["Volumes"], "networks": networks}))
    })();
    match lists {
        Ok(v) => {
            *RUNNING.lock().unwrap_or_else(PoisonError::into_inner) = v["containers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["State"] == "running")
                .map(|c| (c["Id"].as_str().unwrap_or("").to_owned(), c["Names"][0].as_str().unwrap_or("").trim_start_matches('/').to_owned()))
                .collect();
            emit(v.to_string());
            vm_panel();
        }
        Err(_) => {
            RUNNING.lock().unwrap_or_else(PoisonError::into_inner).clear();
            Handle.set_menu(Menu { vm: "Not running".into(), running: Vec::new() });
            emit(state("stopped", None));
        }
    }
}

/// The sidebar's engine panel and the tray menu's state.
fn vm_panel() {
    match client::get("/info") {
        Ok(info) => {
            // A Linux host has no VM to put to sleep: it is always awake.
            let awake = info["KiriVmAwake"].as_bool().unwrap_or(true);
            let mib = |k: &str| info[k].as_u64().unwrap_or(0) >> 20;
            let target = if info["KiriMemoryTarget"].is_null() { info["MemTotal"].as_u64().unwrap_or(0) >> 20 } else { mib("KiriMemoryTarget") };
            let ceiling = if info["KiriMemoryCeiling"].is_null() { info["MemTotal"].as_u64().unwrap_or(0) >> 20 } else { mib("KiriMemoryCeiling") };
            // Prefer live `nproc`, else `.wslconfig`: the engine's NCPU can
            // stay at the old Linux default (2) when Settings only changed
            // `.wslconfig`, which does not update the daemon's vm.json.
            let cpus = crate::wslconfig::running()
                .map(|(c, _)| c)
                .unwrap_or_else(|| crate::wslconfig::configured().0);
            emit(json!({
                "type": "vm", "awake": awake, "target_mib": target, "ceiling_mib": ceiling,
                // Missing from an engine older than the field: 0, not NaN on the page.
                "used_mib": mib("KiriMemoryUsed"),
                "running": info["ContainersRunning"], "cpus": cpus,
                "daemon_kib": info["KiriAgentMemory"].as_u64().unwrap_or(0) >> 10,
                "ip": info["KiriVmIp"].as_str().unwrap_or(""),
            })
            .to_string());
            // Stopped (Stop in the window): off until Start, unlike asleep.
            let stopped = info["KiriStopped"] == true;
            let awake = awake && !stopped;
            emit(state(if awake { "running" } else if stopped { "stopped" } else { "asleep" }, None));
            let running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner).clone();
            Handle.set_menu(Menu { vm: if awake { "Running" } else if stopped { "Stopped" } else { "Asleep" }.into(), running });
        }
        Err(_) => {
            Handle.set_menu(Menu { vm: "Not running".into(), running: Vec::new() });
            emit(state("stopped", None));
        }
    }
}

/// The brand logo for an image name like `bitnami/redis:7`: first by the
/// whole repository path, then by its last component.
pub(crate) fn brand(name: &str) -> Option<[&'static str; 2]> {
    let name = name.split_once('@').map_or(name, |(n, _)| n);
    let repo = match name.rfind(':') {
        Some(i) if !name[i..].contains('/') => &name[..i],
        _ => name,
    };
    let last = repo.rsplit('/').next().unwrap_or(repo);
    let slug = [repo, last].into_iter().find_map(|key| brands::IMAGES.binary_search_by_key(&key, |(k, _)| k).ok().map(|i| brands::IMAGES[i].1))?;
    let i = brands::BRANDS.binary_search_by_key(&slug, |(s, _, _)| s).ok()?;
    let (_, hex, path) = brands::BRANDS[i];
    Some([hex, path])
}

/// A pull the page asked for, from the moment it asks: its image, whether
/// its thread has taken it, its connection once open, and whether Cancel
/// was pressed. A pull's entry is the oldest untaken one with its name.
struct Pulling {
    run: u64,
    name: String,
    taken: bool,
    closer: Option<client::Closer>,
    cancelled: bool,
}

static PULLING: Mutex<Vec<Pulling>> = Mutex::new(Vec::new());

/// Cancel on a pull's row: its request to the engine is closed, which stops
/// the engine's pull (unless a `docker pull` is following it too). A read
/// is only stopped while it waits, so this goes on stopping until the pull
/// has ended.
fn cancel_pull(name: &str) {
    let mut found = false;
    for p in PULLING.lock().unwrap_or_else(PoisonError::into_inner).iter_mut().filter(|p| p.name == name) {
        p.cancelled = true;
        found = true;
    }
    if !found {
        return;
    }
    let name = name.to_owned();
    let _ = std::thread::Builder::new().name("kiri-pull-cancel".into()).spawn(move || {
        for _ in 0..600 {
            let closers: Vec<client::Closer> = {
                let pulls = PULLING.lock().unwrap_or_else(PoisonError::into_inner);
                if !pulls.iter().any(|p| p.name == name && p.cancelled) {
                    return;
                }
                pulls.iter().filter(|p| p.name == name && p.cancelled).filter_map(|p| p.closer.clone()).collect()
            };
            for c in closers {
                c.close();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

/// Pulls through the daemon on its own thread; progress shows as a busy row.
fn pull(name: String) {
    let _ = std::thread::Builder::new().name("kiri-pull".into()).spawn(move || {
        let reference = name.trim().to_owned();
        // This pull's entry; dropped when the pull ends, however it ends.
        let mine = PULLING.lock().unwrap_or_else(PoisonError::into_inner).iter_mut().find(|p| p.name == reference && !p.taken).map(|p| {
            p.taken = true;
            p.run
        });
        let entry = |f: &mut dyn FnMut(&mut Pulling)| {
            if let Some(p) = PULLING.lock().unwrap_or_else(PoisonError::into_inner).iter_mut().find(|p| Some(p.run) == mine) {
                f(p);
            }
        };
        let cancelled = || {
            let mut c = false;
            entry(&mut |p| c = p.cancelled);
            c
        };
        let forget = || {
            let mut pulls = PULLING.lock().unwrap_or_else(PoisonError::into_inner);
            pulls.retain(|p| Some(p.run) != mine);
        };
        if reference.is_empty() || reference.contains(char::is_whitespace) {
            forget();
            emit(notice("error", &format!("Not an image name: {reference}")));
            return;
        }
        if cancelled() {
            forget();
            emit(json!({"type": "pull", "state": "done", "name": reference, "cancelled": true}).to_string());
            return;
        }
        emit(json!({"type": "pull", "state": "started", "name": reference}).to_string());
        let began = std::time::Instant::now();
        let (image, tag) = match reference.rfind(':') {
            Some(i) if !reference[i..].contains('/') => (&reference[..i], &reference[i + 1..]),
            _ => (reference.as_str(), "latest"),
        };
        // The daemon's progress lines, streamed, make one bar over every
        // layer, as the Mac shows; and the "is ready" toast at the end.
        let result = (|| -> io::Result<()> {
            let mut layers: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
            let mut error = None;
            let mut finished = false;
            let mut told = std::time::Instant::now();
            let path = format!("/images/create?fromImage={}&tag={}", path_escape(image), path_escape(tag));
            // Signed in to its registry: the sign-in goes with it, as docker's does.
            let auth = logins::header_for(image);
            // Kept for Cancel; cancelled already, it's closed now.
            let opened = |c: client::Closer| {
                if cancelled() {
                    c.close();
                }
                entry(&mut |p| p.closer = Some(c.clone()));
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
                if !layers.is_empty() && told.elapsed() >= Duration::from_millis(150) && !cancelled() {
                    told = std::time::Instant::now();
                    let (total, got) = layers.values().fold((0, 0), |(t, g), (size, done)| (t + size, g + done));
                    emit(json!({"type": "pull", "state": "progress", "name": reference, "got": got, "total": total}).to_string());
                }
            });
            // Closed by Cancel before the engine said it was done.
            if !finished && cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let status = status?;
            match error {
                Some(e) => Err(io::Error::other(e)),
                None if (200..300).contains(&status) => Ok(()),
                None => Err(io::Error::other(format!("pull failed (HTTP {status})"))),
            }
        })();
        let stopped = result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::Interrupted) && cancelled();
        forget();
        // A failed pull says why on its own row in Images, as on the Mac.
        match result {
            Err(_) if stopped => emit(json!({"type": "pull", "state": "done", "name": reference, "cancelled": true}).to_string()),
            Ok(()) => {
                emit(json!({"type": "pull", "state": "done", "name": reference}).to_string());
                emit(json!({"type": "pulled", "name": reference, "ms": began.elapsed().as_millis() as u64, "icon": brand(&reference)}).to_string());
            }
            Err(e) => emit(json!({"type": "pull", "state": "done", "name": reference, "error": e.to_string()}).to_string()),
        }
    });
}

/// Where a published port answers: `localhost:<port>`, the handy URL, noting
/// that the bind is every interface (the default, as Docker's), or else the
/// address it was bound to (`-p 127.0.0.1:8080:80`).
fn published(b: &Value) -> Option<String> {
    let p = b["HostPort"].as_str()?;
    Some(match b["HostIp"].as_str().unwrap_or("") {
        "" | "0.0.0.0" | "::" => format!("localhost:{p} (all interfaces)"),
        ip => format!("{ip}:{p}"),
    })
}

fn host_ports(info: &Value, with_inner: bool) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(map) = info["NetworkSettings"]["Ports"].as_object() {
        for (inner, list) in map {
            for b in list.as_array().into_iter().flatten() {
                if let Some(at) = published(b) {
                    out.push(if with_inner { format!("{at} → {inner}") } else { at });
                }
            }
        }
    }
    out
}

fn set_ports(id: &str, ports: Value) -> io::Result<()> {
    expect_ok(client::request("POST", &format!("/kiri/containers/{id}/ports"), Some(&json!({"ports": ports}))))?;
    let now = host_ports(&client::get(&format!("/containers/{id}/json"))?, true);
    emit(notice("ok", &if now.is_empty() { "Ports removed".to_owned() } else { format!("Ports saved: {}", now.join(", ")) }));
    Ok(())
}

/// A container's environment, as the Mac's: the engine restarts it with the
/// new one if it was running, or starts it with it if it had stopped.
fn set_env(id: &str, env: Vec<String>) -> io::Result<()> {
    let r = client::request("POST", &format!("/kiri/containers/{id}/env"), Some(&json!({"env": env})))?;
    if r.status == 404 && !r.error().contains("No such container") {
        return Err(io::Error::other("This engine can't change a container's environment yet: update Kiri's engine in WSL and try again."));
    }
    expect_ok(Ok(r))?;
    let info = client::get(&format!("/containers/{id}/json"))?;
    let name = info["Name"].as_str().unwrap_or_default().trim_start_matches('/');
    let how = if info["State"]["Running"].as_bool() == Some(true) { "restarted with" } else { "has" };
    emit(notice("ok", &format!("{name} {how} the new environment")));
    emit(json!({"type": "inspect", "id": id, "info": info}).to_string());
    Ok(())
}

/// The first exposed port on `image` to pin a host port to: prefer TCP, then
/// any other, in name order (`80/tcp` before `443/tcp`).
fn primary_exposed(image: &str) -> Option<String> {
    let info = client::get(&format!("/images/{}/json", path_escape(image))).ok()?;
    let map = info["Config"]["ExposedPorts"].as_object()?;
    let mut keys: Vec<_> = map.keys().cloned().collect();
    keys.sort();
    keys.iter().find(|k| k.ends_with("/tcp")).cloned().or_else(|| keys.into_iter().next())
}

fn flatten_continuations(s: &str) -> String {
    let mut out = String::new();
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let cont = t.ends_with('\\')
            && t.as_bytes().iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1;
        let piece = if cont { t[..t.len() - 1].trim_end() } else { t };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(piece);
    }
    out
}

/// Splits a Run popover command the way a simple shell would: whitespace
/// separates args; single or double quotes keep spaces inside one arg.
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

fn shell_arg(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/=@+,".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn docker_run_line(
    image: &str,
    env: &[String],
    name: &str,
    bindings: &serde_json::Map<String, Value>,
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
        let Some(list) = bindings.get(&key).and_then(|v| v.as_array()) else { continue };
        let host = list.first().and_then(|b| b.get("HostPort")).and_then(|p| p.as_str()).unwrap_or("");
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
        lines.push(format!("  {}", argv.iter().map(|a| shell_arg(a)).collect::<Vec<_>>().join(" ")));
    }
    let last = lines.len().saturating_sub(1);
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| if i == last { line } else { format!("{line} \\") })
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_image(image: &str, env: &[String], container: &str, host_port: Option<u16>, command: &str) -> io::Result<()> {
    let mut bindings = serde_json::Map::new();
    if let Some(hp) = host_port {
        let key = primary_exposed(image).ok_or_else(|| io::Error::other("this image exposes no ports to publish on the host"))?;
        bindings.insert(key, json!([{"HostIp": "", "HostPort": hp.to_string()}]));
    }
    let name = container.trim().trim_start_matches('/');
    let start_cmd = command.trim();
    let argv = split_command(start_cmd);
    // Labels must stay single-line without `\` continuations; the page
    // pretty-prints with line breaks again.
    let run_line = docker_run_line(image, env, name, &bindings, start_cmd)
        .lines()
        .map(|l| l.trim().trim_end_matches('\\').trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut body = json!({"Image": image, "Tty": true, "OpenStdin": true, "Env": env,
        "Labels": {"kiri.start.command": run_line},
        "HostConfig": {"PublishAllPorts": true, "PortBindings": bindings}});
    if !argv.is_empty() {
        body["Cmd"] = json!(argv);
    }
    let began = std::time::Instant::now();
    let path = match container.trim() {
        "" => "/containers/create".to_owned(),
        name => format!("/containers/create?name={}", path_escape(name.trim_start_matches('/'))),
    };
    let created = client::request("POST", &path, Some(&body))?;
    if !created.ok() {
        return Err(io::Error::other(created.error()));
    }
    let id = created.json()["Id"].as_str().unwrap_or_default().to_owned();
    expect_ok(client::request("POST", &format!("/containers/{id}/start"), None))?;
    let ms = began.elapsed().as_millis() as u64;
    let info = client::get(&format!("/containers/{id}/json"))?;
    let name = info["Name"].as_str().unwrap_or_default().trim_start_matches('/').to_owned();
    let ports = host_ports(&info, false);
    // View on the toast opens the container's page; the list stays put, as on
    // the Mac, and `image` puts it beside that image's Run button.
    let text = if ports.is_empty() { format!("Started {name}") } else { format!("Started {name} on {}", ports.join(", ")) };
    emit(json!({"type": "notice", "kind": "ok", "text": text, "open": id, "image": image, "ms": ms}).to_string());
    Ok(())
}

fn about() {
    let info = client::get("/info").unwrap_or(Value::Null);
    let version = client::get("/version").unwrap_or(Value::Null);
    emit(
        json!({
            "type": "about", "app": env!("CARGO_PKG_VERSION"), "running": !info.is_null(),
            "engine": version["Version"], "api": version["ApiVersion"], "kernel": info["KernelVersion"],
            "cpus": info["NCPU"], "memory": info["MemTotal"], "awake": info["KiriVmAwake"].as_bool().unwrap_or(true), "os": info["OperatingSystem"],
        })
        .to_string(),
    );
}

/// The status bar's left side: this build, and which engine `docker` talks
/// to by default, as `docker context show` says (it knows DOCKER_CONTEXT,
/// DOCKER_HOST and wherever Docker Desktop keeps the CLI config).
fn status_event() -> String {
    let context = docker(&["context", "show"]).ok().map(|s| s.trim().to_owned()).filter(|c| !c.is_empty());
    json!({"type": "status", "version": env!("CARGO_PKG_VERSION"), "share": true, "app": "Kiri", "ours": "kiri", "context": context.unwrap_or_else(|| "default".into())}).to_string()
}

/// Runs the docker CLI without a console window: its output, or why it failed.
fn docker(args: &[&str]) -> io::Result<String> {
    let mut cmd = crate::win::tool(&crate::detail::docker_cli());
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.args(args).output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned()))
    }
}

fn builds_event() -> String {
    match crate::builds::list() {
        Ok(b) => json!({"type": "builds", "builds": b}),
        Err(e) => json!({"type": "builds", "error": e.to_string()}),
    }
    .to_string()
}

/// Which builder docker build uses, as chosen (crate::buildwith). An older
/// Kiri named its builder in the user's BUILDX_BUILDER, which sent every
/// build to Kiri whatever docker used: that goes, as Kiri's.
fn build_with() {
    // The user's own value (HKCU\Environment) goes only if it is still
    // Kiri's. By reg.exe, not a PowerShell -Command: kiri-win runs no
    // script text it carries (crate::win says why).
    if std::env::var("BUILDX_BUILDER").is_ok_and(|b| b == crate::builds::BUILDER.get()) {
        let reg = |args: &[&str]| {
            let mut cmd = crate::win::tool("reg.exe");
            std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
            cmd.args(args).output()
        };
        let ours = reg(&["query", r"HKCU\Environment", "/v", "BUILDX_BUILDER"]).is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout).lines().any(|l| {
                let w: Vec<&str> = l.split_whitespace().collect();
                w.len() == 3 && w[0].eq_ignore_ascii_case("BUILDX_BUILDER") && w[2] == crate::builds::BUILDER.get()
            })
        });
        if ours && reg(&["delete", r"HKCU\Environment", "/v", "BUILDX_BUILDER", "/f"]).is_ok_and(|o| o.status.success()) {
            crate::win::environment_changed();
        }
    }
    if let Err(e) = crate::buildwith::apply() {
        crate::serve::log(&format!("docker build: could not set which builder it uses ({e})"));
    }
}

/// Which engine plain `docker` uses: Kiri, or back to the one before.
fn use_context(name: &str) {
    let result = if name.is_empty() { Err(io::Error::other("no context named")) } else { docker(&["context", "use", name]).map(drop) };
    emit(status_event());
    emit(json!({"type": "usedcontext", "name": name, "error": result.as_ref().err().map(|e| e.to_string())}).to_string());
}

/// Which names containers answer at: <name>.kiri.local once its DNS rule
/// is in (crate::dns), else <name>.kiri.localhost, which Windows' browsers
/// resolve to this machine with no setup. HTTPS with Kiri's certificate is
/// the Mac's only.
fn domains_event() -> String {
    // The rule alone is not enough: nothing answers if port 53 was taken.
    let on = crate::dns::installed() && crate::dns::serving();
    json!({"type": "domains", "on": on, "suffix": if on { "kiri.local" } else { "kiri.localhost" }, "https": false}).to_string()
}

/// Settings → Domains, or the welcome: adds or removes the DNS rule.
fn domains(on: bool) {
    let _ = std::thread::Builder::new().name("kiri-domains".into()).spawn(move || {
        if on && !crate::dns::serving() {
            emit(notice("error", "Another program holds 127.0.0.1:53, so Kiri cannot answer for <name>.kiri.local; containers stay at <name>.kiri.localhost"));
            emit(domains_event());
            return;
        }
        match crate::dns::install(on) {
            // Saying no to Windows' prompt is a choice, not an error.
            Err(e) if e.to_string().contains("canceled") => {}
            Err(e) => emit(notice("error", &format!("Could not change container names: {e}"))),
            Ok(()) if on => emit(notice("ok", "Containers now answer at <name>.kiri.local")),
            Ok(()) => emit(notice("ok", "Containers answer at <name>.kiri.localhost")),
        }
        emit(domains_event());
        send(Cmd::Refresh);
    });
}

/// Puts `text` on the clipboard, by clip.exe: the page's own clipboard
/// needs a gesture the WebView doesn't always see.
pub(crate) fn clipboard(text: &str) -> io::Result<()> {
    use std::io::Write;
    let mut cmd = crate::win::tool("clip.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let mut child = cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
    child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?.write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() { Ok(()) } else { Err(io::Error::other("clip.exe failed")) }
}

/// The window's settings, in %LOCALAPPDATA%\Kiri\ui.json.
fn prefs_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Kiri").join("ui.json"))
}

/// The settings, with the defaults for what the person hasn't picked: Kiri
/// starts dark until Settings → Appearance says otherwise.
pub fn prefs() -> Value {
    let mut p: Value = prefs_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({}));
    if !p.is_object() {
        p = json!({});
    }
    if p.get("theme").is_none() {
        p["theme"] = "dark".into();
    }
    p
}

fn set_pref(name: &str, value: Value) {
    // Keep in step with crates/kiri/src/ui/mod.rs::set_pref (Mac/Linux): personal
    // UI needs listLayout, columnWidths, lang, etc. to stick on Windows too.
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
    let mut p = prefs();
    if !p.is_object() {
        p = json!({});
    }
    // Same as Mac: crash reports only cover crashes from when they were turned on.
    if name == "crashReports" {
        if value != true {
            if let Some(o) = p.as_object_mut() {
                o.remove("crashReportsSince");
            }
        } else if p[name] != true {
            p["crashReportsSince"] = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .into();
        }
    }
    p[name] = value;
    let tmp = path.with_extension("json.tmp");
    let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&tmp, p.to_string())).and_then(|()| std::fs::rename(&tmp, &path));
    if saved.is_err() {
        emit(notice("error", "Could not save the setting to %LOCALAPPDATA%\\Kiri\\ui.json"));
    }
}

/// A host path the page may open in Explorer: a drive letter, or WSL's UNC share.
fn host_path_ok(path: &str) -> bool {
    let p = path.trim();
    if p.is_empty() || p.contains('\0') || p.contains('\n') || p.contains('\r') {
        return false;
    }
    let bytes = p.as_bytes();
    // `C:\…` or `C:/…`
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    // `\\wsl$\Distro\…` (Explorer needs backslashes)
    let lower = p.to_ascii_lowercase();
    lower.starts_with(r"\\wsl$\") || lower.starts_with(r"\\wsl.localhost\")
}

/// A page link: only Kiri's own addresses, as on the Mac.
fn allowed_url(url: &str) -> bool {
    if url == "https://kiri.canestack.com" || url == "https://learn.microsoft.com/windows/wsl/about" || url == "https://github.com/libracoder/kiri/issues/new" || url == "https://github.com/libracoder/kiri" {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) else { return false };
    // The authority is up to the first path/query/fragment. Reject credentials
    // (user@host, so the real host is after the @) and backslashes (which some
    // parsers treat as a separator), then take the host before the port.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') || authority.contains('\\') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or("");
    host == "localhost" || host.ends_with(".kiri.localhost") || host.ends_with(".kiri.local")
}

#[cfg(test)]
mod tests {
    use super::{allowed_url, host_path_ok, page_cmd_tokens_ok};

    #[test]
    fn notes_cmds_allow_cjk_and_slash_ids() {
        let id = "新建子目录/未命名-n1794464409860";
        assert!(page_cmd_tokens_ok("noteread", id, ""));
        assert!(page_cmd_tokens_ok("noterename", id, ""));
        assert!(page_cmd_tokens_ok("notedelete", id, ""));
        assert!(page_cmd_tokens_ok("imgnote", "library/nginx:latest", ""));
        // Docker ids still rejected when non-plain.
        assert!(!page_cmd_tokens_ok("container", id, ""));
        assert!(!page_cmd_tokens_ok("files", "abc", id));
    }

    #[test]
    fn only_kiris_own_urls_open() {
        assert!(allowed_url("http://localhost:8080/app"));
        assert!(allowed_url("https://web.kiri.localhost/"));
        assert!(allowed_url("http://api.kiri.local"));
        assert!(allowed_url("https://github.com/libracoder/kiri"));
        assert!(!allowed_url("http://evil.example/"));
        // Bypass attempts the audit found.
        assert!(!allowed_url("http://localhost:1@evil.example/"));
        assert!(!allowed_url("http://evil.example\\x.kiri.localhost"));
        assert!(!allowed_url("https://evil.example/#.kiri.localhost"));
        assert!(!allowed_url("ftp://localhost/"));
    }

    #[test]
    fn host_reveal_only_local_paths() {
        assert!(host_path_ok(r"C:\Users\a\Kiri"));
        assert!(host_path_ok(r"\\wsl$\Ubuntu\var\lib\kiri\containers\x\container.log"));
        assert!(host_path_ok(r"\\wsl.localhost\Ubuntu\var\lib\kiri\x"));
        assert!(!host_path_ok(""));
        assert!(!host_path_ok("http://evil.example/"));
        assert!(!host_path_ok("cmd.exe"));
        assert!(!host_path_ok("C:noslash"));
    }
}

/// Whether a page command's id/name may be handled. Docker-facing ids stay
/// ASCII tokens; notes and image remarks allow path/ref characters.
fn page_cmd_tokens_ok(cmd: &str, id: &str, name: &str) -> bool {
    let notes_cmd = matches!(
        cmd,
        "notes"
            | "noteread"
            | "notesave"
            | "notedelete"
            | "notemkdir"
            | "notermdir"
            | "noterename"
            | "notemove"
            | "notereveal"
            | "imgnotes"
            | "imgnote"
            | "imgnotesave"
    );
    if notes_cmd {
        return true;
    }
    let plain = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
    plain(id) && (!matches!(cmd, "files" | "preview" | "download" | "terminal") || plain(name))
}

/// Handles one message from the page, on the window's thread: quick things
/// here, the rest on the engine thread or a thread of their own.
pub fn on_message(msg: &str) {
    let Ok(c) = serde_json::from_str::<PageCmd>(msg) else { return };
    // Ids and container names are plain tokens; anything else is not from
    // the page as written, and never reaches a request line.
    if !page_cmd_tokens_ok(&c.cmd, &c.id, &c.name) {
        return;
    }
    match c.cmd.as_str() {
        // An uncaught error in the page (the shim in app.rs reports it).
        "jserror" => crate::serve::log(&format!("page: {}", c.name.chars().take(300).collect::<String>())),
        "ready" => {
            emit(json!({"type": "prefs", "prefs": prefs()}).to_string());
            // KIRI_STAR_PROMPT=1 shows the star card now, as on the Mac.
            if std::env::var_os("KIRI_STAR_PROMPT").is_some() {
                emit(r#"{"type":"starprompt"}"#.to_owned());
            }
            emit(domains_event());
            emit(status_event());
            // The first time, offer <name>.kiri.local, as the Mac does.
            if !crate::dns::installed() && crate::dns::serving() && prefs()["domainsAsked"] != true {
                set_pref("domainsAsked", json!(true));
                emit(r#"{"type":"welcome"}"#.to_owned());
            }
            // Which builder docker build uses: made so, and asked once with
            // another engine here (the Mac's welcome comes first, as there).
            std::thread::spawn(|| {
                build_with();
                if let Some(engine) = crate::buildwith::ask() {
                    emit(json!({"type": "buildask", "engine": engine}).to_string());
                }
                emit(crate::buildwith::event());
            });
            send(Cmd::Refresh);
            // Opening the window starts Kiri, unless it was stopped: that waits for Start.
            std::thread::spawn(|| {
                if !client::get("/info").is_ok_and(|i| i["KiriStopped"] == true) {
                    send(Cmd::Start);
                }
            });
            // Anything newer than this build, and a staged update not yet in.
            crate::update::check_now(false);
            // For checking the page by hand in a debug build.
            if cfg!(debug_assertions) {
                if let Ok(js) = std::env::var("KIRI_UI_EVAL") {
                    Handle.eval(js);
                }
            }
        }
        "start" => send(Cmd::Start),
        "stop" => send(Cmd::Stop),
        "refresh" => send(Cmd::Refresh),
        "pull" => {
            static RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let run = RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            PULLING.lock().unwrap_or_else(PoisonError::into_inner).push(Pulling { run, name: c.name.trim().to_owned(), taken: false, closer: None, cancelled: false });
            send(Cmd::Pull(c.name));
        }
        // Cancel on a pull's row, as on the Mac.
        "cancelpull" => cancel_pull(c.name.trim()),
        // Pull image's suggestions, from Docker Hub through a cache.
        "hubsearch" => crate::hub::ask(crate::hub::Kind::Search, c.name, emit),
        "hubtags" => crate::hub::ask(crate::hub::Kind::Tags, c.name, emit),
        "rmi" => {
            let with = c.value.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            send(Cmd::RemoveImage(c.name, with));
        }
        "run" => {
            let env = c.value.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            let host_port = u16::try_from(c.port).ok().filter(|&p| p > 0);
            send(Cmd::Run(c.name, env, c.container, host_port, c.command));
        }
        "container" => send(Cmd::Container { action: c.action, id: c.id }),
        "ports" => send(Cmd::Ports { id: c.id, ports: c.ports }),
        "env" => {
            let env = c.value.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            send(Cmd::Env { id: c.id, env });
        }
        "rmvol" => send(Cmd::RemoveVolume(c.name)),
        "rmnet" => send(Cmd::RemoveNetwork(c.id)),
        "about" => send(Cmd::About),
        // Settings → Builds and the first-run question; with a value, switches.
        "buildwith" => {
            let with = c.value.as_str().and_then(crate::buildwith::With::parse);
            std::thread::spawn(move || {
                if let Some(with) = with
                    && let Err(e) = crate::buildwith::choose(with)
                {
                    emit(notice("error", &format!("Couldn't change which builder docker build uses: {e}")));
                }
                emit(crate::buildwith::event());
            });
        }
        // The docker CLI can take a moment: off the window's thread.
        "usecontext" => {
            std::thread::spawn(move || use_context(&c.name));
        }
        // The Kubernetes section: Pods, Services, a pod's logs, Start.
        "k8s" => crate::k8s::handle(&c.action, &c.value),
        // Share: public links for a port or container, and the Share account.
        "shares" => sharing::list(),
        "share" => sharing::open(c.name, c.value.as_str().unwrap_or_default().to_owned()),
        // A Kubernetes service or pod: value {ip, port, label}.
        "k8sshare" => {
            let v = &c.value;
            let port = v["port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(0);
            sharing::open_k8s(c.name, v["ip"].as_str().unwrap_or_default().to_owned(), port, v["label"].as_str().unwrap_or_default().to_owned(), v["name"].as_str().unwrap_or_default().to_owned());
        }
        "shareusage" => sharing::usage(),
        // The account's live tunnels from every computer, and closing one this window didn't open.
        "sharetunnels" => sharing::tunnels(),
        "sharetraffic" => sharing::traffic(),
        "usage" => {
            std::thread::spawn(|| emit(usage::event()));
        }
        "sharestop" => sharing::stop_tunnel(c.name),
        "sharebrowse" => sharing::browse_tunnel(&c.name),
        "unshare" => sharing::close(c.id.parse().unwrap_or(0)),
        "shareopen" => sharing::browse(c.id.parse().unwrap_or(0)),
        "sharelogin" => sharing::sign_in(),
        "sharecancel" => sharing::cancel_sign_in(),
        "sharelogout" => sharing::sign_out(),
        // The built-in terminal: its shells, and its own clipboard and links.
        "term" => match c.name.as_str() {
            // A selection can be a whole screen of output.
            "copy" => {
                if let Some(text) = c.value.as_str().filter(|t| !t.is_empty() && t.len() <= crate::term::COPY_MAX)
                    && clipboard(text).is_err()
                {
                    emit(notice("error", "Couldn't copy to the clipboard"));
                }
            }
            "paste" => {
                let id = c.id;
                crate::term::paste(move |text| emit(crate::term::paste_event(&id, &text)));
            }
            // A link in a shell's output: web pages only, never a file or an app's scheme.
            "link" => {
                if let Some(url) = c.value.as_str().filter(|u| crate::term::web_link(u)) {
                    crate::app::open_url(url);
                }
            }
            action => crate::term::handle(action, &c.id, &c.value, emit),
        },
        // The page can't write the clipboard itself; IDs and short text only.
        "copy" => {
            if !c.name.is_empty() && c.name.len() <= 4096 && clipboard(&c.name).is_err() {
                emit(notice("error", "Couldn't copy to the clipboard"));
            }
        }
        // About's Check now.
        "updatecheck" => crate::update::check_now(true),
        // The update card and the status bar's update chip.
        "updatedownload" => crate::update::download_now(),
        "updatecancel" => crate::update::cancel_download(),
        "updaterestart" => crate::update::restart_now(),
        "restartengine" => crate::update::restart_engine_now(),
        // What's new: the feed names the page, so only runkiri.com's.
        "updatenotes" => {
            if c.name.strip_prefix("https://runkiri.com/").is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~/#".contains(&b))) {
                crate::app::open_url(&c.name);
            }
        }
        "restartvm" => crate::wslconfig::restart(),
        "inspect" => detail::inspect(c.id),
        "logs" => detail::logs(c.id),
        "logsstop" => detail::stop_logs(),
        // Registries signed in to, signing in (checked by the engine first) and out.
        "logins" | "login" | "logout" => logins::handle(&c.cmd, c.name, &c.value, emit, || Ok(())),
        // Before Run: what the image's start script won't start without.
        "needs" => {
            let image = c.name;
            std::thread::spawn(move || emit(needs::event(&image)));
        }
        // A container that stopped: its last lines, for why.
        "lastlogs" => detail::last_logs(c.id),
        "stats" => detail::stats(c.id),
        "statsstop" => detail::stop_stats(),
        "exec" => detail::exec(c.id, c.path),
        "files" => detail::files(c.name, c.path),
        "preview" => detail::preview(c.name, c.path),
        "download" => detail::download(c.name, c.path),
        "reveal" => emit(notice("error", "Container files are not on Windows' disk: use Save to Downloads")),
        // Host path the page already mapped (\\wsl$\… or a drive letter): open in Explorer.
        "hostreveal" => {
            let path = c.path;
            if !host_path_ok(&path) {
                emit(notice("error", "That path cannot be opened from here"));
            } else if !crate::app::reveal(std::path::Path::new(&path)) {
                emit(notice("error", "Could not open that path in Explorer"));
            }
        }
        "terminal" => detail::terminal(&c.name, &env().pipe),
        "open" => {
            if allowed_url(&c.name) {
                crate::app::open_url(&c.name);
            }
        }
        "setpref" => {
            if c.name == "theme" {
                let theme = c.value.as_str().unwrap_or("").to_owned();
                Handle.run(move |ui| ui.set_theme(&theme));
            }
            set_pref(&c.name, c.value);
        }
        "fonts" => emit(json!({"type": "fonts", "families": crate::app::font_families(false), "mono": crate::app::font_families(true)}).to_string()),
        "proxy" => {
            std::thread::spawn(|| proxy(None));
        }
        "setproxy" => {
            let body = c.value;
            std::thread::spawn(move || proxy(Some(body)));
        }
        // Settings → Network: registry mirrors (`daemon.json`), same as Mac ui/mod.rs.
        "daemon" => {
            std::thread::spawn(|| daemon(None));
        }
        "setdaemon" => {
            let body = c.value;
            std::thread::spawn(move || daemon(Some(body)));
        }
        // Settings → Resources: WSL's CPUs and memory, in .wslconfig.
        "resources" => crate::wslconfig::resources(None),
        "setresources" => crate::wslconfig::resources(Some(c.value)),
        "domains" => domains(c.value.as_bool().unwrap_or(false)),
        // The Builds view: BuildKit's history, read only while the view is open.
        "builds" => {
            std::thread::spawn(|| emit(builds_event()));
        }
        "build" => {
            std::thread::spawn(move || {
                emit(match crate::builds::detail(&c.name, &c.id) {
                    Ok(mut b) => {
                        b["type"] = "build".into();
                        b
                    }
                    Err(e) => json!({"type": "build", "builder": c.name, "id": c.id, "error": {"message": e.to_string()}, "steps": [], "failed": true}),
                }
                .to_string());
            });
        }
        // Builds → Layers: the image a build made.
        "buildlayers" => {
            std::thread::spawn(move || {
                emit(match crate::builds::layers(&c.name, &c.id) {
                    Ok(mut l) => {
                        l["type"] = "buildlayers".into();
                        l
                    }
                    Err(e) => json!({"type": "buildlayers", "builder": c.name, "id": c.id, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        // Images → an image's layers, read by the engine in WSL (crate::scan).
        "imagelayers" => {
            std::thread::spawn(move || {
                emit(match crate::scan::image_layers(&c.name) {
                    Ok(mut l) => {
                        l["type"] = "imagelayers".into();
                        l
                    }
                    Err(e) => json!({"type": "imagelayers", "name": c.name, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        // Security: scan an image (name or ID) with Grype, or show its last scan.
        "scan" => {
            std::thread::spawn(move || {
                let name = c.name;
                let mut last = (String::new(), 255u8);
                let r = crate::scan::scan(&name, &mut |phase, pct| {
                    if (phase, pct) != (last.0.as_str(), last.1) {
                        last = (phase.to_owned(), pct);
                        emit(json!({"type": "scanprogress", "name": name, "phase": phase, "pct": pct}).to_string());
                    }
                });
                emit(match r {
                    Ok(res) => json!({"type": "scanresult", "name": name, "result": res}),
                    Err(e) => json!({"type": "scanresult", "name": name, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        // An image's scan history, or one scan of it (`id` its time).
        "scanhistory" => {
            std::thread::spawn(move || {
                emit(match crate::scan::history(&c.name) {
                    Ok(h) => json!({"type": "scanhistory", "name": c.name, "scans": h["scans"]}),
                    Err(e) => json!({"type": "scanhistory", "name": c.name, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        "scanopen" => {
            std::thread::spawn(move || {
                emit(match crate::scan::open(&c.name, c.id.parse::<u64>().ok()) {
                    Ok(res) => json!({"type": "scanresult", "name": c.name, "result": res}),
                    Err(e) => json!({"type": "scanresult", "name": c.name, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        "savesbom" => {
            std::thread::spawn(move || match crate::scan::save_sbom(&c.name, c.id.parse().unwrap_or(0), &c.path) {
                Ok(to) => {
                    emit(notice("ok", &format!("Saved the SBOM to Downloads as {}", to.file_name().unwrap_or_default().to_string_lossy())));
                    crate::app::reveal(&to);
                }
                Err(e) => emit(notice("error", &format!("Could not save the SBOM: {e}"))),
            });
        }
        "advisory" => crate::scan::open_advisory(&c.name),
        "rmbuild" => {
            std::thread::spawn(move || {
                if let Err(e) = crate::builds::remove(&c.name, &c.id) {
                    emit(notice("error", &format!("Couldn't remove the build: {e}")));
                }
                emit(builds_event());
            });
        }
        // Settings → Diagnostics and an error's Details: crate::diag.
        cmd @ ("diaglogs" | "diagreport" | "diagsend" | "diagcrashes" | "diagsendcrash" | "diagfolder") => {
            crate::diag::handle(cmd, c.name, c.id, &c.value);
        }
        "importsources" => emit(json!({"type": "importsources", "sources": []}).to_string()),
        // Personal Markdown notes under %LOCALAPPDATA%\Kiri\notes, plus
        // per-image remarks — same command set as crates/kiri/src/ui/mod.rs.
        "notes" | "noteread" | "notesave" | "notedelete" | "notemkdir" | "notermdir" | "noterename" | "notemove" | "notereveal"
        | "imgnotes" | "imgnote" | "imgnotesave" => {
            let (cmd, id, value) = (c.cmd.clone(), if c.id.is_empty() { c.name.clone() } else { c.id.clone() }, c.value);
            std::thread::spawn(move || {
                let state = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Kiri"));
                crate::notes::handle(&cmd, &id, &value, state, emit);
            });
        }
        "zoom" => Handle.run(crate::app::Ui::toggle_maximized),
        // The window has Windows' own title bar to drag.
        "drag" | "toolsretry" | "inventory" | "import" => {}
        _ => {}
    }
}

/// Windows' proxy, from Internet Settings, for the daemon's "system" mode:
/// `{http, https, no_proxy, pac}`. ProxyServer is `host:port` for every
/// protocol, or `http=host:port;https=host:port`.
fn windows_proxy() -> Value {
    let mut cmd = crate::win::tool("reg");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let field = |name: &str| out.lines().find_map(|l| {
        let mut p = l.split_whitespace();
        (p.next()? == name).then(|| p.skip(1).collect::<Vec<_>>().join(" "))
    });
    let enabled = field("ProxyEnable").is_some_and(|v| v == "0x1");
    let server = if enabled { field("ProxyServer").unwrap_or_default() } else { String::new() };
    let (mut http, mut https) = (None, None);
    if server.contains('=') {
        for part in server.split(';') {
            match part.split_once('=') {
                Some(("http", v)) => http = Some(v.to_owned()),
                Some(("https", v)) => https = Some(v.to_owned()),
                _ => {}
            }
        }
    } else if !server.is_empty() {
        (http, https) = (Some(server.clone()), Some(server));
    }
    let no_proxy: Vec<String> = if enabled {
        field("ProxyOverride").unwrap_or_default().split(';').map(str::trim).filter(|h| !h.is_empty() && *h != "<local>").map(str::to_owned).collect()
    } else {
        Vec::new()
    };
    json!({"http": http, "https": https, "no_proxy": no_proxy, "pac": field("AutoConfigURL").is_some()})
}

/// Tells the daemon what Windows' proxy is.
fn send_windows_proxy() {
    let _ = client::request("PUT", "/kiri/proxy/system", Some(&windows_proxy()));
}

/// Settings → Network: the daemon's proxy settings.
fn proxy(set: Option<Value>) {
    send_windows_proxy();
    if let Some(body) = set {
        match expect_ok(client::request("POST", "/kiri/proxy", Some(&body))) {
            Ok(()) => emit(notice("ok", "Proxy settings saved")),
            Err(e) => emit(notice("error", &format!("Could not save the proxy settings: {e}"))),
        }
    }
    let ev = match client::get("/kiri/proxy") {
        Ok(v) => {
            let mut v = v;
            v["type"] = json!("proxy");
            v
        }
        Err(e) => json!({"type": "proxy", "error": e.to_string()}),
    };
    emit(ev.to_string());
}

/// How long Settings → Network waits on `/kiri/daemon`. Without a deadline a
/// dead WSL bridge leaves the page on “Saving…” forever (no notice, no event).
const DAEMON_WAIT: Duration = Duration::from_secs(30);

/// Settings → Network: registry mirrors / daemon.json (same as Mac ui/mod.rs).
fn daemon(set: Option<Value>) {
    // Wake the engine first: asleep or mid-bridge-restart, an untimed POST hangs.
    if let Err(e) = ensure_daemon() {
        if set.is_some() {
            emit(notice("error", &format!("Could not save daemon.json: {e}")));
        }
        emit(json!({"type": "daemon", "error": e.to_string()}).to_string());
        return;
    }
    if let Some(body) = set {
        match client::request_within("POST", "/kiri/daemon", Some(&body), false, Some(DAEMON_WAIT)) {
            Ok(r) if r.ok() => emit(notice("ok", "daemon.json saved")),
            Ok(r) => emit(notice("error", &r.error())),
            Err(e) => emit(notice("error", &format!("Could not save daemon.json: {e}"))),
        }
    }
    let ev = match client::request_within("GET", "/kiri/daemon", None, false, Some(DAEMON_WAIT)) {
        Ok(r) if r.ok() => {
            let mut v = r.json();
            v["type"] = json!("daemon");
            v
        }
        Ok(r) => json!({"type": "daemon", "error": r.error()}),
        Err(e) => json!({"type": "daemon", "error": e.to_string()}),
    };
    emit(ev.to_string());
}

/// The tray's commands.
pub fn on_tray(cmd: &str) {
    match cmd {
        "start" => send(Cmd::Start),
        "stop" => send(Cmd::Stop),
        "update" => crate::update::check_now(true),
        c => {
            if let Some(id) = c.strip_prefix("container:") {
                emit(json!({"type": "detail", "id": id}).to_string());
            }
        }
    }
}
