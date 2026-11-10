//! The window's Kubernetes section on Windows: Pods and Services of the
//! `kiri-k8s` cluster, a pod's logs, Delete, and Start, Stop, the network
//! settings and Delete cluster, as crates/kiri/src/ui/k8s.rs and `kiri k8s`
//! (crates/kiri/src/k8s.rs) are on the Mac.
//!
//! Everything goes through the engine's API on the pipe. `kubectl` runs
//! inside the node container by the exec API, so Windows needs no kubectl
//! and no kubeconfig; the lists are read only while the node runs, with
//! requests marked as the window's own, which don't count as use. The
//! cluster is started, stopped and recreated with the containers API, as
//! `kiri k8s` does on the Mac (there is no `kiri k8s` on Windows): its
//! settings are in %LOCALAPPDATA%\Kiri\k8s.json and its kubeconfig beside
//! them; its `kiri` context is put into %USERPROFILE%\.kube\config at each
//! start and whenever the window finds the engine with the node running.

use std::io::{self, BufRead, Read};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::client::{self, Closer};
use crate::engine::{brand, brands, emit, notice};

// The lists as the page shows them, and the names it may send: the Mac's.
#[path = "../../kiri/src/ui/k8s_items.rs"]
mod items;
use items::{summarize, valid};
// kubectl's own file, as the Mac's `kiri k8s` writes it.
#[path = "../../kiri/src/kubeconfig.rs"]
mod kubeconfig;

const IMAGE: (&str, &str) = ("rancher/k3s", "v1.37.1-k3s1");
/// The node container; the window reads the cluster through it.
pub(crate) const NAME: &str = "kiri-k8s";
const VOLUME: &str = "kiri-k8s";
/// The API server's port in the node, and the port it is published on by default.
const API_PORT: u16 = 6443;
/// How many ports after the asked-for one a start tries before giving up.
const PORT_TRIES: u16 = 20;
/// The label that keeps the port a node was made with, for while it's stopped.
const PORT_LABEL: &str = "dev.kiri.k8s.port";
/// The label that keeps the settings a node was made with.
const CONFIG_LABEL: &str = "dev.kiri.k8s.config";

/// A list is being read: the page asks every few seconds, and a slow
/// answer must not pile up more.
static LISTING: AtomicBool = AtomicBool::new(false);
/// After a list that got no answer, the next waits this long (unix seconds):
/// asking a node that is struggling every few seconds would only pile more
/// kubectl processes onto it.
static QUIET_UNTIL: AtomicU64 = AtomicU64::new(0);
/// The command running for the window ("start", "stop", "set",
/// "deletecluster"), one at a time.
static OP: Mutex<Option<&'static str>> = Mutex::new(None);
/// The one pod log stream the page shows, as detail.rs keeps the container's.
static LOGS: Mutex<Option<Closer>> = Mutex::new(None);
static LOGS_GEN: AtomicU64 = AtomicU64::new(0);
/// The node's process for the stream: closing the connection doesn't end an
/// exec, so a stop kills it.
static LOGS_PID: Mutex<Option<u32>> = Mutex::new(None);

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(f);
}

fn context() -> &'static str {
    crate::flavor::CONTEXT.get()
}

/// A command from the page: `{"cmd":"k8s","action":…,"value":{ns,pod,container}}`.
pub fn handle(action: &str, value: &Value) {
    let arg = |k: &str| value[k].as_str().unwrap_or_default().to_owned();
    match action {
        "list" => list(),
        "start" => op("start", |say| start(say)),
        "stop" => op("stop", |say| stop(say)),
        "deletecluster" => op("deletecluster", |say| delete_cluster(say)),
        // Settings → Kubernetes → Network: saved, and the node recreated with them.
        "set" => {
            let sans: Vec<String> = value["tls_san"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
            let lan = value["lan"] == true;
            let port = value["port"].as_u64().and_then(|p| u16::try_from(p).ok());
            op("set", move |say| set(lan, sans, port, say));
        }
        "delete" => delete(arg("ns"), arg("pod")),
        "logs" => logs(arg("ns"), arg("pod"), arg("container"), value["previous"] == true),
        "logsstop" => stop_logs(),
        "copy" => copy(value["text"].as_str().unwrap_or("")),
        _ => {}
    }
}

/// Settings' Copy buttons (the API server's address, a kubectl command).
fn copy(text: &str) {
    match crate::engine::clipboard(text) {
        Ok(()) => emit(notice("ok", &format!("Copied {text}"))),
        Err(_) => emit(notice("error", "Couldn't copy to the clipboard")),
    }
}

// --- the cluster's settings ---------------------------------------------------------

/// The cluster's settings, in %LOCALAPPDATA%\Kiri\k8s.json: whether its API
/// server (and, through the engine, its LoadBalancer services) answers on
/// every interface rather than 127.0.0.1, extra names for its certificate,
/// and the port asked for. They are applied when the node container is
/// created, so a change recreates it; the cluster's state is in the volume
/// and stays.
#[derive(Default, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Config {
    #[serde(default)]
    lan: bool,
    #[serde(default)]
    tls_san: Vec<String>,
    /// The port asked for the API server (6443 when unset). When it is taken
    /// the node takes the next free one; `port` in its container says which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
}

impl Config {
    fn api_port(&self) -> u16 {
        self.port.unwrap_or(API_PORT)
    }
}

/// %LOCALAPPDATA%\Kiri, where the window keeps its settings.
fn state_dir() -> io::Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Kiri")).ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))
}

fn config_path() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("k8s.json"))
}

fn config() -> Config {
    config_path().ok().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Public resolvers for the node. WSL's NAT gateway DNS often fails inside
/// the container (`lookup …: Try again`), so system pods never leave
/// ContainerCreating; pin working servers on create.
fn node_dns() -> &'static [&'static str] {
    &["223.5.5.5", "8.8.8.8", "1.1.1.1"]
}

/// What of the settings a node is made with, as its label keeps them. The
/// drives' folder is in it too, so a node made before it had it is made
/// again at its next start. `"dns1"` marks nodes created with HostConfig.Dns.
fn made_with(cfg: &Config) -> String {
    serde_json::to_string(&(cfg.lan, &cfg.tls_san, cfg.api_port(), drives(), "dns1")).unwrap_or_default()
}

/// Where WSL puts this PC's drives (`/mnt`, unless `[automount] root` in
/// the engine distro's wsl.conf moves it), found once. The node sees it at
/// the same path, so a pod's `hostPath: /mnt/d/...` is drive D, as it is
/// with k3s in WSL itself (Rancher Desktop). The bind is recursive: every
/// drive WSL has mounted when the node starts comes with it.
fn drives() -> Option<String> {
    static ROOT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let mut wsl = crate::win::tool("wsl.exe");
        std::os::windows::process::CommandExt::creation_flags(&mut wsl, 0x0800_0000); // CREATE_NO_WINDOW
        if let Some(d) = crate::engine::distro() {
            wsl.args(["-d", &d]);
        }
        let out = wsl.args(["--", "wslpath", "-a", "C:/"]).stdin(std::process::Stdio::null()).output().ok()?;
        drives_root(&String::from_utf8_lossy(&out.stdout))
    })
    .clone()
}

/// The drives' folder from `wslpath -a C:/` (`/mnt/c/` → `/mnt`); none
/// when drives sit at the top (`/c/`), which isn't a folder to bind.
fn drives_root(c: &str) -> Option<String> {
    let c = c.trim().trim_end_matches('/');
    let (root, letter) = c.rsplit_once('/')?;
    (letter.eq_ignore_ascii_case("c") && root.starts_with('/') && root.len() > 1 && !root.contains(['\\', ':'])).then(|| root.to_owned())
}

/// The node's binds: its state, then the drives' folder.
fn binds() -> Vec<String> {
    std::iter::once(format!("{VOLUME}:/var/lib/rancher/k3s")).chain(drives().map(|d| format!("{d}:{d}"))).collect()
}

/// k3s private-registry file for Docker Hub mirrors (Settings → Network).
/// See https://docs.k3s.io/installation/private-registry
fn registries_yaml(mirrors: &[String], insecure: &[String]) -> String {
    let mut out = String::from("mirrors:\n");
    if mirrors.is_empty() {
        out.push_str("  docker.io:\n    endpoint: []\n");
    } else {
        out.push_str("  docker.io:\n    endpoint:\n");
        for m in mirrors {
            out.push_str(&format!("      - \"{m}\"\n"));
        }
    }
    let mut hosts: Vec<String> = mirrors
        .iter()
        .filter(|m| m.starts_with("http://"))
        .filter_map(|m| {
            let rest = m.strip_prefix("http://")?;
            let host = rest.split('/').next()?.trim();
            (!host.is_empty()).then(|| host.to_owned())
        })
        .collect();
    for h in insecure {
        if !h.contains('/') {
            hosts.push(h.clone());
        }
    }
    hosts.sort();
    hosts.dedup();
    if !hosts.is_empty() {
        out.push_str("configs:\n");
        for h in hosts {
            out.push_str(&format!("  \"{h}\":\n    tls:\n      insecure_skip_verify: true\n"));
        }
    }
    out
}

/// Hub mirrors + insecure hosts from the engine's daemon.json.
fn daemon_mirrors() -> (Vec<String>, Vec<String>) {
    let Ok(v) = client::get("/kiri/daemon") else {
        return (Vec::new(), Vec::new());
    };
    let settings = &v["settings"];
    let mirrors = settings["registry-mirrors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_owned))
        .collect();
    let insecure = settings["insecure-registries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_owned))
        .collect();
    (mirrors, insecure)
}

/// Writes Settings → Network mirrors into the node's registries.yaml.
/// Returns whether the file changed (caller should restart the node).
fn apply_registries() -> io::Result<bool> {
    let (mirrors, insecure) = daemon_mirrors();
    let yaml = registries_yaml(&mirrors, &insecure);
    let script = "set -e; d=/etc/rancher/k3s; f=$d/registries.yaml; mkdir -p $d; \
         printf '%s' \"$1\" > $d/.kiri-registries.tmp; \
         if cmp -s $d/.kiri-registries.tmp $f 2>/dev/null; then rm -f $d/.kiri-registries.tmp; else mv $d/.kiri-registries.tmp $f; echo written; fi";
    let (code, out) = client::exec(NAME, &["sh", "-c", script, "sh", &yaml])?;
    if code != 0 {
        return Err(io::Error::other(format!("could not write registries.yaml: {}", out.trim())));
    }
    Ok(out.contains("written"))
}

/// The port a node container publishes its API server on: its live binding
/// while it runs, else the one it was made with.
fn bound_port(info: &Value) -> Option<u16> {
    let live = &info["NetworkSettings"]["Ports"][format!("{API_PORT}/tcp")][0]["HostPort"];
    let made = &info["Config"]["Labels"][PORT_LABEL];
    live.as_str().or(made.as_str()).and_then(|p| p.parse().ok())
}

/// A port no program on this PC holds (another Kiri's cluster, Docker
/// Desktop's, a tunnel): nothing answers on it and it can be bound. WSL
/// relays the engine's published ports to Windows' localhost, so a port
/// held in WSL answers here too.
fn port_free(port: u16, lan: bool) -> bool {
    let local = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    if std::net::TcpStream::connect_timeout(&local, Duration::from_millis(300)).is_ok() {
        return false;
    }
    let ip = if lan { [0, 0, 0, 0] } else { [127, 0, 0, 1] };
    std::net::TcpListener::bind(std::net::SocketAddr::from((ip, port))).is_ok()
}

/// The asked-for port, or the first free one of the next few.
fn pick_port(want: u16, lan: bool) -> io::Result<u16> {
    let last = want.saturating_add(PORT_TRIES);
    (want..=last).find(|&p| port_free(p, lan)).ok_or_else(|| {
        io::Error::other(format!(
            "ports {want}–{last} are all in use on this PC, so the cluster's API server has none to take. Choose another in Settings → Kubernetes, or stop what holds them, such as another Kubernetes"
        ))
    })
}

/// An extra certificate name: an IP address or a DNS name (`*.` allowed).
fn valid_san(name: &str) -> bool {
    name.parse::<std::net::IpAddr>().is_ok()
        || (!name.is_empty()
            && name.len() <= 253
            && name.trim_start_matches("*.").split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')))
}

// --- the engine, as the window's background requests ---------------------------------

/// One request marked as the window's own, so refreshing the lists never
/// counts as use of the engine: its status and JSON.
fn request(method: &str, path: &str, body: Option<&Value>) -> io::Result<(u16, Value)> {
    let r = client::request_with(method, path, body, true)?;
    Ok((r.status, r.json()))
}

/// A request of the cluster's own (start, stop): checked.
fn ok(r: io::Result<client::Response>) -> io::Result<client::Response> {
    let r = r?;
    if r.ok() { Ok(r) } else { Err(io::Error::other(r.error())) }
}

/// The node's state ("running", "exited", …, or "absent"), and the port its
/// API server is published on.
fn node_state() -> io::Result<(String, Option<u16>)> {
    let (status, info) = request("GET", &format!("/containers/{NAME}/json"), None)?;
    if status == 404 {
        return Ok(("absent".into(), None));
    }
    Ok((info["State"]["Status"].as_str().unwrap_or("unknown").to_owned(), bound_port(&info)))
}

fn exists() -> io::Result<bool> {
    Ok(client::request("GET", &format!("/containers/{NAME}/json"), None)?.ok())
}

/// `kubectl …` in the node: its exit code and output. A starved API server
/// can leave kubectl waiting for good, `--request-timeout` or not, so the
/// node kills it after 30 s and the window stops reading soon after: none
/// pile up, and the page hears it isn't answering.
fn kubectl(args: &[&str]) -> io::Result<(i64, String)> {
    let cmd: Vec<&str> = ["timeout", "-s", "KILL", "30", "kubectl"].into_iter().chain(args.iter().copied()).collect();
    let mut e = client::exec_start(NAME, &cmd, true)?;
    // The pipe has no read timeout: a watchdog ends the read instead.
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let fired = std::sync::Arc::new(AtomicBool::new(false));
    let (closer, done2, fired2) = (e.closer(), done.clone(), fired.clone());
    spawn("kiri-k8s-kubectl-watch", move || {
        for _ in 0..350 {
            std::thread::sleep(Duration::from_millis(100));
            if done2.load(Ordering::SeqCst) {
                return;
            }
        }
        fired2.store(true, Ordering::SeqCst);
        closer.close();
    });
    let mut out = Vec::new();
    let read = e.read_to_end(&mut out);
    done.store(true, Ordering::SeqCst);
    if fired.load(Ordering::SeqCst) {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "kubectl got no answer in 35 s"));
    }
    read?;
    let code = request("GET", &format!("/exec/{}/json", e.id), None)?.1["ExitCode"].as_i64().unwrap_or(-1);
    Ok((code, String::from_utf8_lossy(&out).replace("\r\n", "\n")))
}

// --- the lists ------------------------------------------------------------------------

/// Pods and Services, or why there are none: `{"type":"k8s","state":…}`.
fn list() {
    if LISTING.swap(true, Ordering::SeqCst) {
        return;
    }
    spawn("kiri-k8s-list", || {
        let event = read_lists();
        LISTING.store(false, Ordering::SeqCst);
        emit(event.to_string());
    });
}

fn current_op() -> Option<&'static str> {
    *OP.lock().unwrap_or_else(|e| e.into_inner())
}

fn read_lists() -> Value {
    let mut v = read_state();
    v["want_port"] = json!(config().api_port());
    // What Settings → Kubernetes shows: the context, the network settings, a command running.
    v["op"] = json!(current_op());
    v["context"] = json!(context());
    v["config"] = json!(config());
    v
}

fn read_state() -> Value {
    let starting = current_op() == Some("start");
    let (state, port) = match node_state() {
        Ok(s) => s,
        Err(e) => return json!({"type": "k8s", "state": "unreachable", "starting": starting, "error": e.to_string()}),
    };
    // The port in the node's real binding, so the page shows the one in use.
    let mut v = read_running(&state, starting);
    v["port"] = json!(port);
    v
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn read_running(state: &str, starting: bool) -> Value {
    if state != "running" {
        return json!({"type": "k8s", "state": state, "starting": starting});
    }
    let now = now();
    if now < QUIET_UNTIL.load(Ordering::SeqCst) {
        return json!({"type": "k8s", "state": "running", "ready": false, "starting": starting, "error": "the API server didn't answer; asking again shortly"});
    }
    // Warnings go to stderr, which the terminal mixes in: the JSON starts at
    // the first line that opens an object.
    let parsed = kubectl(&["get", "pods,services", "--all-namespaces", "-o", "json", "--request-timeout=20s"]).and_then(|(code, out)| {
        let at = out.find("\n{").map(|i| i + 1).or_else(|| out.starts_with('{').then_some(0));
        match at.and_then(|i| serde_json::from_str::<Value>(&out[i..]).ok()) {
            Some(v) if code == 0 => Ok(v),
            // Killed by `timeout`: the API server never answered.
            _ if code == 137 => Err(io::Error::new(io::ErrorKind::TimedOut, "kubectl got no answer in 30 s")),
            _ => Err(io::Error::other(out.trim().lines().last().unwrap_or("kubectl failed").to_owned())),
        }
    });
    match parsed {
        Ok(v) => summarize(&v, starting),
        // Up, but the API server isn't answering yet (it takes a few seconds after a start).
        Err(e) => {
            // A timeout (the node itself not answering) waits 30 s; a quick refusal, while the API starts, doesn't.
            let timed_out = matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut);
            if timed_out {
                QUIET_UNTIL.store(now + 30, Ordering::SeqCst);
            }
            let error = if timed_out { "the node didn't answer in 30 s".to_owned() } else { e.to_string() };
            json!({"type": "k8s", "state": "running", "ready": false, "starting": starting, "error": error})
        }
    }
}

// --- actions --------------------------------------------------------------------------

/// Deletes a pod without waiting for it to go; the next list shows it terminating.
fn delete(ns: String, pod: String) {
    if !valid(&ns) || !valid(&pod) {
        return;
    }
    spawn("kiri-k8s-delete", move || {
        match kubectl(&["delete", "pod", "-n", &ns, &pod, "--wait=false"]) {
            Ok((0, _)) => emit(notice("ok", &format!("Deleting pod {pod}"))),
            Ok((_, out)) => emit(notice("error", out.trim().lines().last().unwrap_or("kubectl delete failed"))),
            Err(e) => emit(notice("error", &e.to_string())),
        }
        list();
    });
}

/// A command on the cluster, on its own thread, as `kiri k8s` runs on the
/// Mac: each line it says is the page's progress, and its error the toast.
fn op(name: &'static str, run: impl FnOnce(&dyn Fn(&str)) -> io::Result<()> + Send + 'static) {
    {
        let mut running = OP.lock().unwrap_or_else(|e| e.into_inner());
        if running.is_some() {
            return emit(notice("error", "Kubernetes is busy with another change; try again in a moment"));
        }
        *running = Some(name);
    }
    spawn("kiri-k8s-op", move || {
        let say = |line: &str| emit(json!({"type": "k8sop", "op": name, "state": "progress", "line": line.trim_end_matches('…')}).to_string());
        // Make sure the engine is up; the cluster's own requests then wake it.
        let result = crate::engine::ensure_daemon().and_then(|()| run(&say));
        *OP.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // A new or restarted node answers at once: no waiting out an earlier timeout.
        QUIET_UNTIL.store(0, Ordering::SeqCst);
        match result {
            Ok(()) => {
                // The port the node really got, for the notice.
                let port = node_state().ok().filter(|(state, _)| state == "running").and_then(|(_, p)| p);
                emit(json!({"type": "k8sop", "op": name, "state": "done", "context": context(), "port": port, "want_port": config().api_port()}).to_string());
            }
            Err(e) => {
                crate::serve::log(&format!("k8s {name}: {e}"));
                emit(json!({"type": "k8sop", "op": name, "state": "error", "error": e.to_string()}).to_string());
            }
        }
        list();
    });
}

/// `kiri k8s start`: the node container, made if need be, started and
/// waited for; then the kubeconfig.
fn start(say: &dyn Fn(&str)) -> io::Result<()> {
    let t0 = Instant::now();
    let cfg = config();
    let want = cfg.api_port();
    // A stopped node keeps its port unless something else has taken it since;
    // then it is made again (its state is in the volume) on a free one.
    let mut moved_to = None;
    if exists()? {
        let info = client::get(&format!("/containers/{NAME}/json"))?;
        let running = info["State"]["Running"] == true;
        // Settings / Dns label changed (cluster state lives in the volume).
        let stale = match info["Config"]["Labels"][CONFIG_LABEL].as_str() {
            Some(made) => made != made_with(&cfg),
            None => bound_port(&info) != Some(want),
        };
        if stale {
            say("applying the changed settings (the cluster's state is kept)…");
            ok(client::request("DELETE", &format!("/containers/{NAME}?force=1"), None))?;
        } else if !running {
            if let Some(held) = bound_port(&info).filter(|&p| !port_free(p, cfg.lan)) {
                let port = pick_port(want, cfg.lan)?;
                say(&format!("port {held} is in use now; moving the API server to {port}…"));
                ok(client::request("DELETE", &format!("/containers/{NAME}?force=1"), None))?;
                moved_to = Some(port);
            }
        }
    }
    if !exists()? {
        let port = match moved_to {
            Some(p) => p,
            None => pick_port(want, cfg.lan)?,
        };
        if port != want {
            say(&format!("port {want} is in use; the API server takes {port}"));
        }
        let (repo, tag) = IMAGE;
        say(&format!("pulling {repo}:{tag}…"));
        pull(repo, tag)?;
        let mut cmd: Vec<String> = ["server", "--disable", "traefik", "--tls-san", "127.0.0.1", "--tls-san", "localhost", "--write-kubeconfig-mode", "644"].map(String::from).to_vec();
        for name in cfg.tls_san.iter().filter(|n| valid_san(n)) {
            cmd.extend(["--tls-san".to_owned(), name.clone()]);
        }
        let body = json!({
            "Image": format!("{repo}:{tag}"),
            "Cmd": cmd,
            "Hostname": "kiri",
            "Labels": {"dev.kiri.k8s": "1", PORT_LABEL: port.to_string(), CONFIG_LABEL: made_with(&cfg)},
            "ExposedPorts": {"6443/tcp": {}},
            "HostConfig": {
                "Privileged": true,
                "Tmpfs": {"/run": "", "/var/run": ""},
                "Binds": binds(),
                "Dns": node_dns(),
                "PortBindings": {"6443/tcp": [{"HostIp": if cfg.lan { "0.0.0.0" } else { "127.0.0.1" }, "HostPort": port.to_string()}]},
                "RestartPolicy": {"Name": "unless-stopped"},
            },
        });
        ok(client::request("POST", &format!("/containers/create?name={NAME}"), Some(&body)))?;
    }
    let started = client::request("POST", &format!("/containers/{NAME}/start"), None)?;
    if !started.ok() && started.status != 304 {
        return Err(io::Error::other(started.error()));
    }
    // Settings → Network mirrors: k3s pulls through its own containerd.
    match apply_registries() {
        Ok(true) => {
            say("applying registry mirrors to the cluster…");
            let _ = client::request("POST", &format!("/containers/{NAME}/restart?t=20"), None)?;
        }
        Ok(false) => {}
        Err(e) => say(&format!("registry mirrors not applied to the cluster: {e}")),
    }
    say("waiting for the node…");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok((0, out)) = client::exec(NAME, &["timeout", "-s", "KILL", "20", "kubectl", "get", "nodes", "--no-headers"])
            && out.split_whitespace().nth(1) == Some("Ready")
        {
            break;
        }
        if Instant::now() > deadline {
            return Err(io::Error::other(format!("the node was not ready after 3 minutes; see `docker --context {} logs {NAME}`", context())));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let port = bound_port(&client::get(&format!("/containers/{NAME}/json"))?).unwrap_or(API_PORT);
    kubeconfig(port, say)?;
    let moved = if port != want { format!(" ({want} was in use)") } else { String::new() };
    say(&format!("Kubernetes is ready in {:.0} s, API on 127.0.0.1:{port}{moved}: kubectl --context {} get nodes", t0.elapsed().as_secs_f64(), context()));
    Ok(())
}

/// Pulls the node's image through the engine, waiting for the whole of it.
fn pull(repo: &str, tag: &str) -> io::Result<()> {
    let mut error = None;
    let path = format!("/images/create?fromImage={}&tag={}", crate::engine::path_escape(repo), crate::engine::path_escape(tag));
    let status = client::post_lines(&path, |v| {
        if let Some(e) = v["error"].as_str().or(v["message"].as_str()) {
            error = Some(e.to_owned());
        }
    })?;
    match error {
        Some(e) => Err(io::Error::other(e)),
        None if (200..300).contains(&status) => Ok(()),
        None => Err(io::Error::other(format!("pull failed (HTTP {status})"))),
    }
}

/// `kiri k8s stop`: the node stops; its state is kept.
fn stop(say: &dyn Fn(&str)) -> io::Result<()> {
    ok(client::request("POST", &format!("/containers/{NAME}/stop"), None))?;
    say("Kubernetes stopped (its state is kept; Start resumes it)");
    Ok(())
}

/// `kiri k8s delete`: the node, its volume, the kubeconfig and the context.
fn delete_cluster(say: &dyn Fn(&str)) -> io::Result<()> {
    let _ = client::request("DELETE", &format!("/containers/{NAME}?force=1"), None)?;
    let _ = client::request("DELETE", &format!("/volumes/{VOLUME}"), None)?;
    let _ = std::fs::remove_file(state_dir()?.join("kubeconfig"));
    if let Err(e) = forget() {
        say(&format!("could not remove kubectl context `{}` ({e})", context()));
    }
    say("Kubernetes deleted, with its state");
    Ok(())
}

/// `kiri k8s set`: saves the settings and, if the cluster exists and they
/// changed, recreates its node with them.
fn set(lan: bool, sans: Vec<String>, port: Option<u16>, say: &dyn Fn(&str)) -> io::Result<()> {
    let mut cfg = config();
    let before = (cfg.lan, cfg.tls_san.clone(), cfg.api_port());
    cfg.lan = lan;
    let list: Vec<String> = sans.iter().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).collect();
    if let Some(bad) = list.iter().find(|n| !valid_san(n)) {
        return Err(io::Error::other(format!("not an IP address or domain name: {bad}")));
    }
    cfg.tls_san = list;
    if let Some(p) = port {
        if p < 1024 {
            return Err(io::Error::other(format!("the API server port must be a number from 1024 to 65535, not `{p}`")));
        }
        cfg.port = (p != API_PORT).then_some(p);
    }
    let node = if exists()? { Some(client::get(&format!("/containers/{NAME}/json"))?) } else { None };
    // A port asked for that the node doesn't already hold must be free now.
    let port = cfg.api_port();
    if port != before.2 && node.as_ref().and_then(bound_port) != Some(port) && !port_free(port, cfg.lan) {
        return Err(io::Error::other(format!("port {port} is in use on this PC; choose another")));
    }
    let path = config_path()?;
    std::fs::create_dir_all(path.parent().expect("has a parent"))?;
    std::fs::write(&path, serde_json::to_vec(&cfg).map_err(io::Error::other)?)?;
    say(&format!(
        "API server on {}:{port}; extra certificate names: {}",
        if cfg.lan { "every interface" } else { "127.0.0.1" },
        if cfg.tls_san.is_empty() { "none".to_owned() } else { cfg.tls_san.join(", ") }
    ));
    let Some(node) = node else { return Ok(()) };
    if (cfg.lan, cfg.tls_san.clone(), port) == before {
        return Ok(());
    }
    if node["State"]["Running"] != true {
        // A stopped node is made again with them at its next start.
        say("they apply when the cluster next starts");
        return Ok(());
    }
    say("recreating the node with them (the cluster's state is kept)…");
    ok(client::request("DELETE", &format!("/containers/{NAME}?force=1"), None))?;
    start(say)
}

/// k3s' own kubeconfig, from the node.
fn k3s_yaml() -> io::Result<String> {
    let (code, raw) = client::exec(NAME, &["cat", "/etc/rancher/k3s/k3s.yaml"])?;
    if code != 0 || !raw.contains("server:") {
        return Err(io::Error::other("could not read the cluster's kubeconfig"));
    }
    Ok(raw)
}

/// The cluster's kubeconfig, renamed to `kiri`, at %LOCALAPPDATA%\Kiri\kubeconfig,
/// and its cluster, user and context put into %USERPROFILE%\.kube\config
/// (or the first of %KUBECONFIG%) by the Mac's own crates/kiri/src/kubeconfig.rs,
/// with no kubectl.exe needed. Its server is 127.0.0.1 and the port: WSL
/// relays the engine's published ports to Windows' localhost, and the
/// node's certificate names 127.0.0.1. Whether kubectl's file changed.
fn kubeconfig(port: u16, say: &dyn Fn(&str)) -> io::Result<bool> {
    let ctx = context();
    let ours = kubeconfig::from_k3s(&k3s_yaml()?, ctx, port);
    let own = state_dir()?.join("kubeconfig");
    std::fs::create_dir_all(own.parent().expect("has a parent"))?;
    std::fs::write(&own, &ours)?;
    let Some(kube) = kubeconfig::path() else {
        say(&format!("kubeconfig: {} (USERPROFILE is not set, so not in ~\\.kube\\config)", own.display()));
        return Ok(false);
    };
    let changed = kubeconfig::install(&kube, &ours, ctx, false)?;
    if changed {
        say(&format!("kubectl context `{ctx}` written to {}", kube.display()));
    }
    Ok(changed)
}

/// A sync running, so reconnects don't start a second.
static SYNCING: AtomicBool = AtomicBool::new(false);

/// The context of a cluster that is already running, each time the window
/// finds the engine (WSL restarted the node, or an older Kiri never wrote
/// it): in the background, once k3s has its kubeconfig, and rewritten if
/// its port or certificate changed. Nothing when no node runs.
pub(crate) fn sync_soon() {
    if SYNCING.swap(true, Ordering::SeqCst) {
        return;
    }
    spawn("kiri-k8s-context", || {
        let result = (|| -> io::Result<bool> {
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                let (status, info) = request("GET", &format!("/containers/{NAME}/json"), None)?;
                if status == 404 || info["State"]["Running"] != true {
                    return Ok(false);
                }
                if k3s_yaml().is_ok() {
                    return kubeconfig(bound_port(&info).unwrap_or(API_PORT), &|_| {});
                }
                if Instant::now() > deadline {
                    return Err(io::Error::other("the cluster's kubeconfig was not there in time"));
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })();
        SYNCING.store(false, Ordering::SeqCst);
        match result {
            Ok(true) => crate::serve::log(&format!("k8s: kubectl context `{}` written", context())),
            Ok(false) => {}
            Err(e) => crate::serve::log(&format!("k8s: kubectl context not written: {e}")),
        }
    });
}

/// Takes the cluster's cluster, context and user out of kubectl's file:
/// Delete cluster, and uninstalling (contrib/windows/kiri.ps1).
pub(crate) fn forget() -> io::Result<()> {
    let Some(kube) = kubeconfig::path() else { return Ok(()) };
    kubeconfig::uninstall(&kube, context()).map(|_| ())
}

// --- a pod's logs ---------------------------------------------------------------------

/// Follows a pod container's log: the last lines, then as they come, in
/// batches of a few milliseconds as the container page's logs. `previous`
/// shows the run before the last restart instead (a crash loop's reason).
fn logs(ns: String, pod: String, container: String, previous: bool) {
    stop_logs();
    if !valid(&ns) || !valid(&pod) || !valid(&container) {
        return;
    }
    let generation = LOGS_GEN.load(Ordering::SeqCst);
    let key = format!("{ns}/{pod}/{container}{}", if previous { "/previous" } else { "" });
    spawn("kiri-k8s-logs", move || {
        let which = if previous { "--previous" } else { "-f" };
        // The shell says its pid first, then becomes kubectl.
        let mut r = match client::exec_start(NAME, &["sh", "-c", "echo $$; exec kubectl logs \"$@\"", "logs", which, "--tail=2000", "-n", &ns, &pod, "-c", &container], true) {
            Ok(s) => s,
            Err(e) => return emit(notice("error", &format!("Logs: {e}"))),
        };
        let mut first = String::new();
        let _ = r.read_line(&mut first);
        if let Ok(pid) = first.trim().parse::<u32>() {
            if LOGS_GEN.load(Ordering::SeqCst) != generation {
                return kill(pid);
            }
            *LOGS_PID.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
        }
        *LOGS.lock().unwrap_or_else(|e| e.into_inner()) = Some(r.closer());
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let flush_key = key.clone();
        spawn("kiri-k8s-logs-emit", move || {
            let mut batch: Vec<u8> = Vec::new();
            loop {
                let ended = match rx.recv_timeout(Duration::from_millis(60)) {
                    Ok(piece) => {
                        batch.extend_from_slice(&piece);
                        if batch.len() < 256 * 1024 {
                            continue;
                        }
                        false
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => false,
                    Err(mpsc::RecvTimeoutError::Disconnected) => true,
                };
                if LOGS_GEN.load(Ordering::SeqCst) != generation {
                    return;
                }
                if !batch.is_empty() {
                    let text = String::from_utf8_lossy(&batch).replace("\r\n", "\n");
                    emit(json!({"type": "k8slogs", "key": flush_key, "parts": [{"e": false, "t": text}]}).to_string());
                    batch.clear();
                }
                if ended {
                    return emit(json!({"type": "k8slogs-end", "key": flush_key}).to_string());
                }
            }
        });
        let mut buf = [0u8; 16 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if LOGS_GEN.load(Ordering::SeqCst) != generation || tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        // Ended by itself: its pid may be another process's soon.
        if LOGS_GEN.load(Ordering::SeqCst) == generation {
            LOGS_PID.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    });
}

fn kill(pid: u32) {
    spawn("kiri-k8s-logs-kill", move || {
        if let Ok(mut r) = client::exec_start(NAME, &["kill", &pid.to_string()], true) {
            let _ = r.read_to_end(&mut Vec::new());
        }
    });
}

fn stop_logs() {
    LOGS_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(pid) = LOGS_PID.lock().unwrap_or_else(|e| e.into_inner()).take() {
        kill(pid);
    }
    if let Some(c) = LOGS.lock().unwrap_or_else(|e| e.into_inner()).take() {
        c.close();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_drives_folder_comes_from_wslpath() {
        assert_eq!(super::drives_root("/mnt/c/\n").as_deref(), Some("/mnt"));
        assert_eq!(super::drives_root("/windir/c/").as_deref(), Some("/windir"));
        for none in ["/c/", "", "C:/", "wslpath: error", "/mnt/d/"] {
            assert_eq!(super::drives_root(none), None, "{none}");
        }
    }

    #[test]
    fn the_node_sees_the_drives_at_their_own_paths() {
        let b = super::binds();
        assert_eq!(b[0], "kiri-k8s:/var/lib/rancher/k3s");
        if let Some(d) = super::drives() {
            assert!(b.contains(&format!("{d}:{d}")), "{b:?}");
        }
    }

    #[test]
    fn the_port_comes_from_the_binding_then_the_label() {
        let running = serde_json::json!({"NetworkSettings": {"Ports": {"6443/tcp": [{"HostIp": "127.0.0.1", "HostPort": "6444"}]}}, "Config": {"Labels": {"dev.kiri.k8s.port": "6450"}}});
        assert_eq!(super::bound_port(&running), Some(6444));
        let stopped = serde_json::json!({"NetworkSettings": {"Ports": {}}, "Config": {"Labels": {"dev.kiri.k8s.port": "6450"}}});
        assert_eq!(super::bound_port(&stopped), Some(6450));
        assert_eq!(super::bound_port(&serde_json::json!({})), None);
    }

    #[test]
    fn a_busy_port_moves_the_api_to_the_next_free_one() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        assert!(!super::port_free(port, false));
        let got = super::pick_port(port, false).unwrap();
        assert!(got > port && got <= port + super::PORT_TRIES, "{got}");
    }

    #[test]
    fn certificate_names_are_addresses_or_domains() {
        for ok in ["192.168.1.20", "::1", "my-pc.local", "*.example.com", "k8s"] {
            assert!(super::valid_san(ok), "{ok}");
        }
        for bad in ["", "a b", "-x.com", "a..b", "x/y", "--flag"] {
            assert!(!super::valid_san(bad), "{bad}");
        }
    }

    #[test]
    fn registries_yaml_lists_hub_mirrors() {
        let y = super::registries_yaml(
            &["https://docker.m.daocloud.io".into(), "http://mirror.local:5000".into()],
            &["harbor.example:9090".into(), "10.0.0.0/8".into()],
        );
        assert!(y.contains("      - \"https://docker.m.daocloud.io\"\n"), "{y}");
        assert!(y.contains("      - \"http://mirror.local:5000\"\n"), "{y}");
        assert!(y.contains("  \"harbor.example:9090\":\n    tls:\n      insecure_skip_verify: true\n"), "{y}");
        assert!(y.contains("  \"mirror.local:5000\":\n    tls:\n      insecure_skip_verify: true\n"), "{y}");
        assert!(!y.contains("10.0.0.0/8"), "{y}");
        assert!(super::registries_yaml(&[], &[]).contains("endpoint: []\n"));
    }
}
