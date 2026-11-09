//! A container's own page: its details, live logs, a command line and its
//! files. Each runs on its own thread, so a slow container never stalls the
//! window or the engine thread.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

use super::{emit, notice};
use crate::client;

/// The one log stream the page shows, so opening another ends it. Each
/// stream has a generation: one that has been replaced sends nothing more,
/// even what it had already read.
static LOGS: Mutex<Option<UnixStream>> = Mutex::new(None);
static LOGS_GEN: AtomicU64 = AtomicU64::new(0);
/// The same for the live stats on the Overview tab.
static STATS: Mutex<Option<UnixStream>> = Mutex::new(None);
static STATS_GEN: AtomicU64 = AtomicU64::new(0);

/// A file preview's limit; larger files open in Finder instead.
const PREVIEW_MAX: u64 = 256 * 1024;

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(f);
}

pub fn inspect(id: String) {
    spawn("kiri-inspect", move || {
        match client::get(&format!("/containers/{id}/json")) {
            Ok(info) => emit(json!({"type": "inspect", "id": id, "info": info}).to_string()),
            Err(e) => emit(notice("error", &e.to_string())),
        }
    });
}

/// Streams the container's output, the past first, then as it happens.
/// Batches of a few milliseconds keep a chatty container from flooding the page.
pub fn logs(id: String) {
    stop_logs();
    let generation = LOGS_GEN.load(Ordering::SeqCst);
    spawn("kiri-logs", move || {
        let tty = client::get(&format!("/containers/{id}/json"))
            .is_ok_and(|v| v["Config"]["Tty"] == true);
        let mut stream = match client::Stream::open(&format!(
            "/containers/{id}/logs?follow=1&stdout=1&stderr=1&tail={}",
            super::lastlogs::LIVE_TAIL
        )) {
            Ok(s) => s,
            Err(e) => return emit(notice("error", &format!("Logs: {e}"))),
        };
        if let Ok(closer) = stream.closer() {
            *LOGS.lock().unwrap_or_else(|e| e.into_inner()) = Some(closer);
        }
        let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
        let flush_id = id.clone();
        spawn("kiri-logs-emit", move || {
            let mut batch: Vec<(bool, Vec<u8>)> = Vec::new();
            loop {
                match rx.recv_timeout(Duration::from_millis(60)) {
                    Ok(piece) => {
                        batch.push(piece);
                        if batch.len() < 400 {
                            continue;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected)
                        if LOGS_GEN.load(Ordering::SeqCst) != generation =>
                    {
                        return;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        send_batch(&flush_id, &mut batch);
                        return emit(json!({"type": "logs-end", "id": flush_id}).to_string());
                    }
                }
                if LOGS_GEN.load(Ordering::SeqCst) != generation {
                    return;
                }
                send_batch(&flush_id, &mut batch);
            }
        });
        let mut pending = Vec::new();
        while let Some(c) = stream.next() {
            if LOGS_GEN.load(Ordering::SeqCst) != generation {
                break;
            }
            if tty {
                let _ = tx.send((false, c));
                continue;
            }
            // Docker's 8-byte frame headers: stream, 3 zero bytes, big-endian length.
            pending.extend_from_slice(&c);
            while pending.len() >= 8 {
                let len =
                    u32::from_be_bytes([pending[4], pending[5], pending[6], pending[7]]) as usize;
                if pending.len() < 8 + len {
                    break;
                }
                let err = pending[0] == 2;
                let data = pending[8..8 + len].to_vec();
                pending.drain(..8 + len);
                let _ = tx.send((err, data));
            }
        }
    });
}

fn send_batch(id: &str, batch: &mut Vec<(bool, Vec<u8>)>) {
    if batch.is_empty() {
        return;
    }
    let parts: Vec<Value> = batch
        .drain(..)
        .map(|(err, d)| json!({"e": err, "t": String::from_utf8_lossy(&d)}))
        .collect();
    emit(json!({"type": "logs", "id": id, "parts": parts}).to_string());
}

/// A stopped container's last lines (crates/kiri/src/ui/lastlogs.rs):
/// `{type: "lastlogs", id, lines}`; no lines if the log can't be read.
pub fn last_logs(id: String) {
    spawn("kiri-lastlogs", move || {
        let tty = client::get(&format!("/containers/{id}/json"))
            .is_ok_and(|v| v["Config"]["Tty"] == true);
        let lines = client::request(
            "GET",
            &format!(
                "/containers/{id}/logs?stdout=1&stderr=1&tail={}",
                super::lastlogs::KEEP
            ),
            None,
        )
        .map(|r| super::lastlogs::lines(&r.body, tty, super::lastlogs::KEEP))
        .unwrap_or_default();
        emit(json!({"type": "lastlogs", "id": id, "lines": lines}).to_string());
    });
}

pub fn stop_logs() {
    LOGS_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(s) = LOGS.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
}

/// Live figures for the Overview tab, once a second, as Docker's stats
/// stream gives them: CPU % across the VM's CPUs, memory (page cache left
/// out, as `docker stats` does), network and disk totals, processes.
pub fn stats(id: String) {
    stop_stats();
    let generation = STATS_GEN.load(Ordering::SeqCst);
    spawn("kiri-stats", move || {
        let Ok(mut stream) = client::Stream::open(&format!("/containers/{id}/stats?stream=1"))
        else {
            return;
        };
        if let Ok(closer) = stream.closer() {
            *STATS.lock().unwrap_or_else(|e| e.into_inner()) = Some(closer);
        }
        let mut pending = Vec::new();
        while let Some(c) = stream.next() {
            if STATS_GEN.load(Ordering::SeqCst) != generation {
                return;
            }
            pending.extend_from_slice(&c);
            while let Some(i) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=i).collect();
                if let Ok(s) = serde_json::from_slice::<Value>(&line) {
                    emit(sample(&id, &s).to_string());
                }
            }
        }
        if STATS_GEN.load(Ordering::SeqCst) == generation {
            emit(json!({"type": "stats-end", "id": id}).to_string());
        }
    });
}

fn sample(id: &str, s: &Value) -> Value {
    let n = |v: &Value| v.as_f64().unwrap_or(0.0);
    let cpu_delta = n(&s["cpu_stats"]["cpu_usage"]["total_usage"])
        - n(&s["precpu_stats"]["cpu_usage"]["total_usage"]);
    let sys_delta =
        n(&s["cpu_stats"]["system_cpu_usage"]) - n(&s["precpu_stats"]["system_cpu_usage"]);
    let cpus = n(&s["cpu_stats"]["online_cpus"]).max(1.0);
    let has_pre = s["precpu_stats"]["system_cpu_usage"].is_number();
    // A share of all the CPUs the container can use (100% = every one busy),
    // not docker's per-core figure, which reads 250% on a busy four-CPU engine.
    let cpu = if has_pre && sys_delta > 0.0 && cpu_delta >= 0.0 {
        (cpu_delta / sys_delta * 100.0).min(100.0)
    } else {
        0.0
    };
    let mem = n(&s["memory_stats"]["usage"]) - n(&s["memory_stats"]["stats"]["inactive_file"]);
    let io = |op: &str| {
        s["blkio_stats"]["io_service_bytes_recursive"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["op"] == op)
            .map(|e| n(&e["value"]))
            .sum::<f64>()
    };
    json!({
        "type": "stats", "id": id, "first": !has_pre,
        "cpu": (cpu * 1000.0).round() / 1000.0, "cpus": cpus,
        "mem": mem.max(0.0), "limit": n(&s["memory_stats"]["limit"]),
        "rx": n(&s["networks"]["eth0"]["rx_bytes"]), "tx": n(&s["networks"]["eth0"]["tx_bytes"]),
        "read": io("read"), "write": io("write"),
        "pids": n(&s["pids_stats"]["current"]),
    })
}

pub fn stop_stats() {
    STATS_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(s) = STATS.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
}

/// One command, as `sh -c`, with its output and exit code.
pub fn exec(id: String, cmd: String) {
    spawn("kiri-exec", move || {
        let event = match client::exec(&id, &["sh", "-c", &cmd]) {
            Ok((code, output)) => {
                json!({"type": "exec", "id": id, "cmd": cmd, "code": code, "output": output})
            }
            Err(e) => {
                json!({"type": "exec", "id": id, "cmd": cmd, "code": -1, "output": e.to_string()})
            }
        };
        emit(event.to_string());
    });
}

/// A full interactive shell in a terminal window (Terminal.app on the Mac), through the docker CLI.
pub fn terminal(name: &str) -> io::Result<()> {
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(io::Error::other("not a container name"));
    }
    let docker = client::docker_cli().ok_or_else(|| io::Error::other("no docker CLI found"))?;
    let line = format!(
        "'{}' --context kiri exec -it {name} sh -c 'exec $(command -v bash || command -v sh)'",
        docker.display()
    );
    // A .command file Terminal opens, rather than scripting Terminal, which
    // would make macOS ask for permission to control it. It removes itself.
    let dir = super::state_dir()
        .ok_or_else(|| io::Error::other("HOME is not set"))?
        .join("run");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("shell-{name}.command"));
    std::fs::write(&file, format!("#!/bin/sh\nrm -f \"$0\"\nexec {line}\n"))?;
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(&file)
        .spawn()?;
    #[cfg(target_os = "linux")]
    linux_terminal(&file)?;
    Ok(())
}

/// The first terminal app found runs the script: Debian's chosen one, then
/// GNOME's (Ptyxis, then the older GNOME Terminal), KDE's and xterm.
#[cfg(target_os = "linux")]
fn linux_terminal(file: &std::path::Path) -> io::Result<()> {
    let tries: [(&str, &[&str]); 5] = [
        ("x-terminal-emulator", &["-e"]),
        ("ptyxis", &["--new-window", "--"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("xterm", &["-e"]),
    ];
    for (app, args) in tries {
        match std::process::Command::new(app).args(args).arg(file).spawn() {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    let _ = std::fs::remove_file(file);
    Err(io::Error::other("no terminal app found"))
}

/// The container's root on the Mac (the Finder view), and `path` inside it.
/// Only plain components are allowed, so nothing outside the container is read.
fn host_path(name: &str, path: &str) -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let root = home.join(crate::flavor::APP.get()).join(name);
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Err(io::Error::other("not a container name"));
    }
    let rel = Path::new(path.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(io::Error::other("not a path in the container"));
    }
    Ok(root.join(rel))
}

pub fn files(name: String, path: String) {
    spawn("kiri-files", move || {
        let event = match list(&name, &path) {
            Ok(entries) => json!({"type": "files", "name": name, "path": path, "entries": entries}),
            Err(e) => {
                json!({"type": "files", "name": name, "path": path, "entries": [], "error": e.to_string()})
            }
        };
        emit(event.to_string());
    });
}

fn list(name: &str, path: &str) -> io::Result<Vec<Value>> {
    let dir = host_path(name, path)?;
    let mut entries: Vec<Value> = Vec::new();
    for e in std::fs::read_dir(&dir)?.flatten() {
        let link = e.file_type().is_ok_and(|t| t.is_symlink());
        let meta = std::fs::metadata(e.path()).or_else(|_| e.metadata());
        let (is_dir, size, mtime) = meta.map_or((false, 0, 0), |m| {
            (
                m.is_dir(),
                m.len(),
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs()),
            )
        });
        entries.push(json!({"name": e.file_name().to_string_lossy(), "dir": is_dir, "size": size, "mtime": mtime, "link": link}));
    }
    entries.sort_by(|a, b| {
        (b["dir"].as_bool(), a["name"].as_str()).cmp(&(a["dir"].as_bool(), b["name"].as_str()))
    });
    Ok(entries)
}

pub fn preview(name: String, path: String) {
    spawn("kiri-preview", move || {
        let event = match read_text(&name, &path) {
            Ok(Some(text)) => json!({"type": "preview", "name": name, "path": path, "text": text}),
            Ok(None) => json!({"type": "preview", "name": name, "path": path, "binary": true}),
            Err(e) => {
                json!({"type": "preview", "name": name, "path": path, "error": e.to_string()})
            }
        };
        emit(event.to_string());
    });
}

/// The file's text, or `None` when it is binary or too large to show.
fn read_text(name: &str, path: &str) -> io::Result<Option<String>> {
    let file = host_path(name, path)?;
    if std::fs::metadata(&file)?.len() > PREVIEW_MAX {
        return Ok(None);
    }
    let bytes = std::fs::read(&file)?;
    Ok(String::from_utf8(bytes).ok().filter(|t| !t.contains('\0')))
}

/// Copies a file out of the container into ~/Downloads (keeping one already
/// there: `name 2.ext`), then shows it.
pub fn download(name: &str, path: &str) -> io::Result<PathBuf> {
    let from = host_path(name, path)?;
    let file = from
        .file_name()
        .ok_or_else(|| io::Error::other("not a file"))?
        .to_owned();
    let downloads = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?
        .join("Downloads");
    let mut to = downloads.join(&file);
    let (stem, ext) = (
        Path::new(&file)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        Path::new(&file)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy())),
    );
    let mut n = 2;
    while to.exists() {
        to = downloads.join(format!("{stem} {n}{}", ext.as_deref().unwrap_or("")));
        n += 1;
    }
    std::fs::copy(&from, &to)?;
    Ok(to)
}

/// Shows the file or folder in Finder.
pub fn reveal(name: &str, path: &str) -> io::Result<()> {
    let p = host_path(name, path)?;
    super::reveal(&p);
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Against a running daemon with a running `myapp` nginx container:
    /// `KIRI_LIVE=1 cargo test -p kiri live_container_page -- --nocapture`.
    #[test]
    fn live_container_page() {
        if std::env::var_os("KIRI_LIVE").is_none() {
            return;
        }
        let mut s =
            crate::client::Stream::open("/containers/myapp/logs?follow=1&stdout=1&stderr=1")
                .unwrap();
        let first = s.next().unwrap();
        assert!(
            first.len() > 8 && (first[0] == 1 || first[0] == 2),
            "framed output: {first:?}"
        );
        s.closer()
            .unwrap()
            .shutdown(std::net::Shutdown::Both)
            .unwrap();
        // What was already buffered may still come out, then it ends.
        assert!(
            (0..10_000).any(|_| s.next().is_none()),
            "closer ends the stream"
        );
        let etc = super::list("myapp", "/etc").unwrap();
        assert!(
            etc.iter().any(|e| e["name"] == "nginx" && e["dir"] == true),
            "{etc:?}"
        );
        let conf = super::read_text("myapp", "/etc/nginx/nginx.conf")
            .unwrap()
            .unwrap();
        assert!(conf.contains("worker_processes"));
        assert_eq!(
            super::read_text("myapp", "/bin/busybox").unwrap(),
            None,
            "binary is not previewed"
        );
        let (code, out) =
            crate::client::exec("myapp", &["sh", "-c", "nginx -v 2>&1; exit 3"]).unwrap();
        assert_eq!((code, out.contains("nginx version")), (3, true), "{out}");
        // Two stats samples: the second has a CPU figure and sane memory.
        let mut st = crate::client::Stream::open("/containers/myapp/stats?stream=1").unwrap();
        let mut lines = Vec::new();
        let mut buf = Vec::new();
        while lines.len() < 2 {
            buf.extend_from_slice(&st.next().unwrap());
            while let Some(i) = buf.iter().position(|&b| b == b'\n') {
                lines.push(super::sample(
                    "myapp",
                    &serde_json::from_slice(&buf.drain(..=i).collect::<Vec<_>>()).unwrap(),
                ));
            }
        }
        let (a, b) = (&lines[0], &lines[1]);
        assert_eq!(
            (a["first"].as_bool(), b["first"].as_bool()),
            (Some(true), Some(false))
        );
        let (cpu, mem, limit, pids) = (
            b["cpu"].as_f64().unwrap(),
            b["mem"].as_f64().unwrap(),
            b["limit"].as_f64().unwrap(),
            b["pids"].as_f64().unwrap(),
        );
        assert!((0.0..=400.0).contains(&cpu), "cpu {cpu}");
        assert!(mem > 1e6 && mem < limit, "mem {mem} of {limit}");
        assert!(pids >= 2.0, "nginx has a master and workers: {pids}");
        eprintln!("stats: cpu {cpu}% mem {mem} of {limit}, pids {pids}");
    }

    #[test]
    fn keeps_paths_inside_the_container() {
        assert!(
            super::host_path("web", "/etc/nginx/nginx.conf")
                .unwrap()
                .ends_with("Kiri/web/etc/nginx/nginx.conf")
        );
        assert!(super::host_path("web", "").unwrap().ends_with("Kiri/web"));
        assert!(super::host_path("web", "/etc/../../other").is_err());
        assert!(super::host_path("../web", "/").is_err());
        assert!(super::host_path("", "/").is_err());
    }
}
