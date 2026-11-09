//! The window's Kubernetes section: Pods and Services of the `kiri k8s`
//! cluster, a pod's logs, Delete, and Start when it is off.
//!
//! Everything goes through `kubectl` inside the `kiri-k8s` container, by the
//! engine's exec API, so the Mac needs no kubectl and no kubeconfig. Nothing
//! here starts the cluster or wakes the VM on its own: the lists are read
//! only while the node container runs (and so the VM is awake), with
//! requests marked as the window's own, which don't count as use.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

use super::{brand, brands, emit, notice};
use crate::client;
use crate::k8s::NAME;

// The lists as the page shows them, and the names it may send: shared with the Windows window,
// which includes the file too; `brand` and `brands` above are for it.
#[path = "k8s_items.rs"]
mod items;
use items::{summarize, valid};

/// A list is being read: the page asks every few seconds, and a slow
/// answer must not pile up more.
static LISTING: AtomicBool = AtomicBool::new(false);
/// After a list that got no answer, the next waits this long (unix seconds):
/// asking a node that is struggling (a memory-starved VM) every few seconds
/// would only pile more kubectl processes onto it.
static QUIET_UNTIL: AtomicU64 = AtomicU64::new(0);
/// The `kiri k8s` command running for the window ("start", "stop", "set",
/// "deletecluster"), one at a time.
static OP: Mutex<Option<&'static str>> = Mutex::new(None);
/// The one pod log stream the page shows, as detail.rs keeps the container's.
static LOGS: Mutex<Option<UnixStream>> = Mutex::new(None);
static LOGS_GEN: AtomicU64 = AtomicU64::new(0);
/// The node's process for the stream: closing the connection doesn't end an
/// exec, so a stop kills it.
static LOGS_PID: Mutex<Option<u32>> = Mutex::new(None);

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(f);
}

/// A command from the page: `{"cmd":"k8s","action":…,"value":{ns,pod,container}}`.
pub fn handle(action: &str, value: &Value) {
    let arg = |k: &str| value[k].as_str().unwrap_or_default().to_owned();
    match action {
        "list" => list(),
        "start" => op("start", vec!["start".into()]),
        "stop" => op("stop", vec!["stop".into()]),
        "deletecluster" => op("deletecluster", vec!["delete".into()]),
        // Settings → Kubernetes → Network: saved, and the node recreated with them.
        "set" => {
            let sans: Vec<&str> = value["tls_san"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let lan = if value["lan"] == true { "on" } else { "off" };
            let mut args: Vec<String> = vec![
                "set".into(),
                "--lan".into(),
                lan.into(),
                "--tls-san".into(),
                sans.join(","),
            ];
            if let Some(port) = value["port"].as_u64() {
                args.extend(["--port".into(), port.to_string()]);
            }
            op("set", args);
        }
        "delete" => delete(arg("ns"), arg("pod")),
        "logs" => logs(
            arg("ns"),
            arg("pod"),
            arg("container"),
            value["previous"] == true,
        ),
        "logsstop" => stop_logs(),
        "copy" => copy(value["text"].as_str().unwrap_or("")),
        _ => {}
    }
}

/// Settings' Copy buttons (the API server's address, a kubectl command), by
/// pbcopy: the page's own clipboard needs a gesture WebKit doesn't always see.
fn copy(text: &str) {
    use std::io::Write;
    let done = std::process::Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            c.stdin.take().expect("piped").write_all(text.as_bytes())?;
            c.wait()
        });
    match done {
        Ok(s) if s.success() => emit(notice("ok", &format!("Copied {text}"))),
        _ => emit(notice("error", "Couldn't copy to the clipboard")),
    }
}

// --- the engine, as the window's background requests ---------------------------------

fn connect() -> io::Result<UnixStream> {
    let s = UnixStream::connect(client::socket()?)?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    Ok(s)
}

/// Writes one request marked as the window's own (`X-Kiri-Background`), so
/// refreshing the lists never counts as use of the VM.
fn write_request(
    s: &mut UnixStream,
    method: &str,
    path: &str,
    body: Option<&Value>,
    extra: &str,
) -> io::Result<()> {
    let payload = body.map(Value::to_string).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: kiri\r\nX-Kiri-Background: 1\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    )
}

/// The status line and the headers that matter here.
fn read_head(r: &mut impl BufRead) -> io::Result<(u16, Option<usize>, bool)> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status line"))?;
    let (mut len, mut chunked) = (None, false);
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = line.trim_end().split_once(':') {
            match k.to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().ok(),
                "transfer-encoding" => chunked = v.trim().eq_ignore_ascii_case("chunked"),
                _ => {}
            }
        }
    }
    Ok((status, len, chunked))
}

fn request(method: &str, path: &str, body: Option<&Value>) -> io::Result<(u16, Value)> {
    let mut s = connect()?;
    write_request(&mut s, method, path, body, "Connection: close\r\n")?;
    let mut r = BufReader::new(s);
    let (status, len, chunked) = read_head(&mut r)?;
    let mut buf = Vec::new();
    if status == 204 || status == 304 {
        return Ok((status, Value::Null));
    }
    if chunked {
        loop {
            let mut size = String::new();
            r.read_line(&mut size)?;
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            let mut chunk = vec![0; n + 2];
            r.read_exact(&mut chunk)?;
            buf.extend_from_slice(&chunk[..n]);
        }
    } else if let Some(n) = len {
        buf.resize(n, 0);
        r.read_exact(&mut buf)?;
    } else {
        r.read_to_end(&mut buf)?;
    }
    Ok((status, serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

/// The node container's state: "running", "exited", …, or "absent".
/// The node's state, and the Mac port its API server is published on.
fn node_state() -> io::Result<(String, Option<u16>)> {
    let (status, info) = request("GET", &format!("/containers/{NAME}/json"), None)?;
    if status == 404 {
        return Ok(("absent".into(), None));
    }
    Ok((
        info["State"]["Status"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned(),
        crate::k8s::bound_port(&info),
    ))
}

/// Starts `cmd` in the node container and returns the connection its
/// output arrives on (unframed: the exec has a terminal) and the exec's id.
fn exec_start(cmd: &[&str]) -> io::Result<(BufReader<UnixStream>, String)> {
    let body = json!({"Cmd": cmd, "AttachStdout": true, "AttachStderr": true, "Tty": true});
    let (status, created) = request("POST", &format!("/containers/{NAME}/exec"), Some(&body))?;
    if !(200..300).contains(&status) {
        return Err(io::Error::other(
            created["message"]
                .as_str()
                .unwrap_or("exec failed")
                .to_owned(),
        ));
    }
    let id = created["Id"].as_str().unwrap_or_default().to_owned();
    let mut s = connect()?;
    write_request(
        &mut s,
        "POST",
        &format!("/exec/{id}/start"),
        Some(&json!({"Detach": false, "Tty": true})),
        "Connection: Upgrade\r\nUpgrade: tcp\r\n",
    )?;
    let mut r = BufReader::new(s);
    let (status, _, _) = read_head(&mut r)?;
    if status != 101 && status != 200 {
        return Err(io::Error::other(format!("exec: HTTP {status}")));
    }
    Ok((r, id))
}

/// `kubectl …` in the node: its exit code and output. A starved API server
/// can leave kubectl waiting for good, `--request-timeout` or not, so the
/// node kills it after 30 s and the window stops reading soon after: none
/// pile up, and the page hears it isn't answering.
fn kubectl(args: &[&str]) -> io::Result<(i64, String)> {
    let cmd: Vec<&str> = ["timeout", "-s", "KILL", "30", "kubectl"]
        .into_iter()
        .chain(args.iter().copied())
        .collect();
    let (mut r, id) = exec_start(&cmd)?;
    r.get_ref()
        .set_read_timeout(Some(std::time::Duration::from_secs(35)))?;
    let mut out = Vec::new();
    r.read_to_end(&mut out)?;
    let code = request("GET", &format!("/exec/{id}/json"), None)?.1["ExitCode"]
        .as_i64()
        .unwrap_or(-1);
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
    v["want_port"] = json!(crate::k8s::config().api_port());
    // What Settings → Kubernetes shows: the context, the network settings, a command running.
    v["op"] = json!(current_op());
    v["context"] = json!(crate::flavor::CONTEXT.get());
    v["config"] = json!(crate::k8s::config());
    v
}

fn read_state() -> Value {
    let starting = current_op() == Some("start");
    let (state, port) = match node_state() {
        Ok(s) => s,
        Err(e) => {
            return json!({"type": "k8s", "state": "unreachable", "starting": starting, "error": e.to_string()});
        }
    };
    // The port in the node's real binding, so the page shows the one in use.
    let mut v = read_running(&state, starting);
    v["port"] = json!(port);
    v
}

fn read_running(state: &str, starting: bool) -> Value {
    if state != "running" {
        return json!({"type": "k8s", "state": state, "starting": starting});
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if now < QUIET_UNTIL.load(Ordering::SeqCst) {
        return json!({"type": "k8s", "state": "running", "ready": false, "starting": starting, "error": "the API server didn't answer; asking again shortly"});
    }
    // Warnings go to stderr, which the terminal mixes in: the JSON starts at
    // the first line that opens an object.
    let parsed = kubectl(&[
        "get",
        "pods,services",
        "--all-namespaces",
        "-o",
        "json",
        "--request-timeout=20s",
    ])
    .and_then(|(code, out)| {
        let at = out
            .find("\n{")
            .map(|i| i + 1)
            .or_else(|| out.starts_with('{').then_some(0));
        match at.and_then(|i| serde_json::from_str::<Value>(&out[i..]).ok()) {
            Some(v) if code == 0 => Ok(v),
            // Killed by `timeout`: the API server never answered.
            _ if code == 137 => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "kubectl got no answer in 30 s",
            )),
            _ => Err(io::Error::other(
                out.trim()
                    .lines()
                    .last()
                    .unwrap_or("kubectl failed")
                    .to_owned(),
            )),
        }
    });
    match parsed {
        Ok(v) => summarize(&v, starting),
        // Up, but the API server isn't answering yet (it takes a few seconds after a start).
        Err(e) => {
            // A timeout (the node itself not answering) waits 30 s; a quick refusal, while the API starts, doesn't.
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                QUIET_UNTIL.store(now + 30, Ordering::SeqCst);
            }
            let error = if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                "the node didn't answer in 30 s".to_owned()
            } else {
                e.to_string()
            };
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
            Ok((_, out)) => emit(notice(
                "error",
                out.trim().lines().last().unwrap_or("kubectl delete failed"),
            )),
            Err(e) => emit(notice("error", &e.to_string())),
        }
        list();
    });
}

/// `kiri k8s …`, as its own process, as the command line runs it: each line
/// it prints is the page's progress, and its last error line the toast.
fn op(name: &'static str, args: Vec<String>) {
    {
        let mut running = OP.lock().unwrap_or_else(|e| e.into_inner());
        if running.is_some() {
            return emit(notice(
                "error",
                "Kubernetes is busy with another change; try again in a moment",
            ));
        }
        *running = Some(name);
    }
    spawn("kiri-k8s-op", move || {
        let _op = crate::diag::enter_op(crate::diag::new_op(&format!("k8s{name}")));
        crate::diag::info!("action started", action = format!("k8s {name}"));
        let result = (|| -> io::Result<()> {
            let exe = std::env::current_exe()?;
            // Make sure the engine is up; the cluster's own requests then wake the VM.
            super::ensure_daemon()?;
            let mut child = std::process::Command::new(exe)
                .arg("k8s")
                .args(&args)
                // Its lines (and its requests to the engine) under this op.
                .env("KIRI_OP", crate::diag::op().unwrap_or_default())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
            let stderr = child.stderr.take();
            let err = std::thread::spawn(move || {
                let mut s = String::new();
                if let Some(mut e) = stderr {
                    let _ = e.read_to_string(&mut s);
                }
                s
            });
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    emit(json!({"type": "k8sop", "op": name, "state": "progress", "line": line.trim_end_matches('…')}).to_string());
                }
            }
            let status = child.wait()?;
            let err = err.join().unwrap_or_default();
            if status.success() {
                return Ok(());
            }
            let why = err
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("kiri k8s failed")
                .trim();
            Err(io::Error::other(
                why.strip_prefix("kiri: ").unwrap_or(why).to_owned(),
            ))
        })();
        *OP.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // A new or restarted node answers at once: no waiting out an earlier timeout.
        QUIET_UNTIL.store(0, Ordering::SeqCst);
        match result {
            Ok(()) => {
                // The port the node really got, for the notice.
                let port = node_state()
                    .ok()
                    .filter(|(state, _)| state == "running")
                    .and_then(|(_, p)| p);
                emit(json!({"type": "k8sop", "op": name, "state": "done", "context": crate::flavor::CONTEXT.get(), "port": port, "want_port": crate::k8s::config().api_port()}).to_string())
            }
            Err(e) => {
                crate::diag::warn!("action failed", action = format!("k8s {name}"), error = e);
                emit(json!({"type": "k8sop", "op": name, "state": "error", "error": e.to_string(), "logop": crate::diag::op()}).to_string())
            }
        }
        list();
    });
}

/// Follows a pod container's log: the last lines, then as they come, in
/// batches of a few milliseconds as the container page's logs. `previous`
/// shows the run before the last restart instead (a crash loop's reason).
fn logs(ns: String, pod: String, container: String, previous: bool) {
    stop_logs();
    if !valid(&ns) || !valid(&pod) || !valid(&container) {
        return;
    }
    let generation = LOGS_GEN.load(Ordering::SeqCst);
    let key = format!(
        "{ns}/{pod}/{container}{}",
        if previous { "/previous" } else { "" }
    );
    spawn("kiri-k8s-logs", move || {
        let which = if previous { "--previous" } else { "-f" };
        // The shell says its pid first, then becomes kubectl.
        let (mut r, _) = match exec_start(&[
            "sh",
            "-c",
            "echo $$; exec kubectl logs \"$@\"",
            "logs",
            which,
            "--tail=2000",
            "-n",
            &ns,
            &pod,
            "-c",
            &container,
        ]) {
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
        if let Ok(closer) = r.get_ref().try_clone() {
            *LOGS.lock().unwrap_or_else(|e| e.into_inner()) = Some(closer);
        }
        // No read timeout: a quiet pod can say nothing for minutes.
        let _ = r.get_ref().set_read_timeout(None);
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
                    if LOGS_GEN.load(Ordering::SeqCst) != generation
                        || tx.send(buf[..n].to_vec()).is_err()
                    {
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
        if let Ok((mut r, _)) = exec_start(&["kill", &pid.to_string()]) {
            let _ = r.read_to_end(&mut Vec::new());
        }
    });
}

fn stop_logs() {
    LOGS_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(pid) = LOGS_PID.lock().unwrap_or_else(|e| e.into_inner()).take() {
        kill(pid);
    }
    if let Some(s) = LOGS.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    /// Against a running cluster (`kiri k8s start`), read-only:
    /// `KIRI_LIVE_K8S=1 KIRI_SOCKET=~/.kiri-dev/run/docker.sock cargo test -p kiri live_k8s_lists -- --nocapture`.
    #[test]
    fn live_k8s_lists() {
        if std::env::var_os("KIRI_LIVE_K8S").is_none() {
            return;
        }
        let v = super::read_lists();
        assert_eq!(
            (v["state"].as_str(), v["ready"].as_bool()),
            (Some("running"), Some(true)),
            "{v}"
        );
        let pods = v["pods"].as_array().unwrap();
        assert!(
            pods.iter()
                .any(|p| p["ns"] == "kube-system"
                    && p["name"].as_str().unwrap().starts_with("coredns")),
            "{pods:?}"
        );
        assert!(
            v["services"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "kubernetes" && s["ns"] == "default")
        );
        for p in pods {
            eprintln!(
                "{:<12} {:<44} {:<18} {:<5} {}",
                p["ns"].as_str().unwrap(),
                p["name"].as_str().unwrap(),
                p["status"].as_str().unwrap(),
                p["ready"].as_str().unwrap(),
                p["restarts"]
            );
        }
        let (code, out) = super::kubectl(&["version", "--client"]).unwrap();
        assert!(code == 0 && out.contains("Client Version"), "{out}");
    }
}
