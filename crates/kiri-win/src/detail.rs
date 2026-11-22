//! The container page, as crates/kiri/src/ui/detail.rs is on the Mac:
//! inspect, live logs and stats, a command line, a terminal and its files.
//! The Mac reads files through its NFS mount at ~/Kiri; here they come
//! through the daemon: listings and previews by exec, saving by the archive
//! API, so a container must be running to browse it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};

use crate::client::{self, Closer};
use crate::engine::{emit, notice, trf};

#[path = "../../kiri/src/ui/lastlogs.rs"]
mod lastlogs;

/// The one log stream and the one stats stream: what ends each, and a
/// generation so a stream that was replaced says nothing more.
static LOGS: Mutex<Option<Closer>> = Mutex::new(None);
static LOGS_GEN: AtomicU64 = AtomicU64::new(0);
static STATS: Mutex<Option<Closer>> = Mutex::new(None);
static STATS_GEN: AtomicU64 = AtomicU64::new(0);

/// A file previewed as text up to this size.
const PREVIEW_MAX: usize = 256 * 1024;

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(f);
}

pub fn inspect(id: String) {
    spawn("kiri-inspect", move || match client::get(&format!("/containers/{id}/json")) {
        Ok(info) => emit(json!({"type": "inspect", "id": id, "info": info}).to_string()),
        Err(e) => emit(notice("error", &e.to_string())),
    });
}

/// Follows a container's logs, batched so a busy one does not flood the page.
pub fn logs(id: String) {
    stop_logs();
    let generation = LOGS_GEN.load(Ordering::SeqCst);
    spawn("kiri-logs", move || {
        let tty = client::get(&format!("/containers/{id}/json")).map(|i| i["Config"]["Tty"] == true).unwrap_or(false);
        let mut stream = match client::Stream::open(&format!("/containers/{id}/logs?follow=1&stdout=1&stderr=1&tail={}", lastlogs::LIVE_TAIL)) {
            Ok(s) => s,
            Err(e) => {
                emit(notice("error", &trf("Logs: {error}", &[("error", &e)])));
                return;
            }
        };
        *LOGS.lock().unwrap_or_else(PoisonError::into_inner) = Some(stream.closer());
        let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
        let emit_id = id.clone();
        spawn("kiri-logs-emit", move || {
            let mut parts = Vec::new();
            let flush = |parts: &mut Vec<Value>| {
                if !parts.is_empty() && LOGS_GEN.load(Ordering::SeqCst) == generation {
                    emit(json!({"type": "logs", "id": emit_id, "parts": std::mem::take(parts)}).to_string());
                }
                parts.clear();
            };
            loop {
                match rx.recv_timeout(Duration::from_millis(60)) {
                    Ok((e, t)) => {
                        parts.push(json!({"e": e, "t": String::from_utf8_lossy(&t)}));
                        if parts.len() >= 400 {
                            flush(&mut parts);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => flush(&mut parts),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        flush(&mut parts);
                        if LOGS_GEN.load(Ordering::SeqCst) == generation {
                            emit(json!({"type": "logs-end", "id": emit_id}).to_string());
                        }
                        return;
                    }
                }
            }
        });
        // Docker's multiplexed frames: [stream, 0, 0, 0, len u32 BE] data.
        let mut pending = Vec::new();
        while let Some(c) = stream.next() {
            if LOGS_GEN.load(Ordering::SeqCst) != generation {
                return;
            }
            if tty {
                let _ = tx.send((false, c));
                continue;
            }
            pending.extend_from_slice(&c);
            while pending.len() >= 8 {
                let len = u32::from_be_bytes([pending[4], pending[5], pending[6], pending[7]]) as usize;
                if pending.len() < 8 + len {
                    break;
                }
                let frame: Vec<u8> = pending.drain(..8 + len).collect();
                let _ = tx.send((frame[0] == 2, frame[8..].to_vec()));
            }
        }
    });
}

/// A stopped container's last lines, as on the Mac (the shared
/// crates/kiri/src/ui/lastlogs.rs): `{type: "lastlogs", id, lines}`.
pub fn last_logs(id: String) {
    spawn("kiri-lastlogs", move || {
        let tty = client::get(&format!("/containers/{id}/json")).is_ok_and(|v| v["Config"]["Tty"] == true);
        let lines = client::request("GET", &format!("/containers/{id}/logs?stdout=1&stderr=1&tail={}", lastlogs::KEEP), None)
            .map(|r| lastlogs::lines(&r.body, tty, lastlogs::KEEP))
            .unwrap_or_default();
        emit(json!({"type": "lastlogs", "id": id, "lines": lines}).to_string());
    });
}

pub fn stop_logs() {
    LOGS_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(c) = LOGS.lock().unwrap_or_else(PoisonError::into_inner).take() {
        c.close();
    }
}

/// Live figures for the Overview tab, once a second, as `docker stats` gives them.
pub fn stats(id: String) {
    stop_stats();
    let generation = STATS_GEN.load(Ordering::SeqCst);
    spawn("kiri-stats", move || {
        let Ok(mut stream) = client::Stream::open(&format!("/containers/{id}/stats?stream=1")) else { return };
        *STATS.lock().unwrap_or_else(PoisonError::into_inner) = Some(stream.closer());
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

pub fn stop_stats() {
    STATS_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(c) = STATS.lock().unwrap_or_else(PoisonError::into_inner).take() {
        c.close();
    }
}

fn sample(id: &str, s: &Value) -> Value {
    let n = |v: &Value| v.as_f64().unwrap_or(0.0);
    let cpu_delta = n(&s["cpu_stats"]["cpu_usage"]["total_usage"]) - n(&s["precpu_stats"]["cpu_usage"]["total_usage"]);
    let sys_delta = n(&s["cpu_stats"]["system_cpu_usage"]) - n(&s["precpu_stats"]["system_cpu_usage"]);
    let cpus = n(&s["cpu_stats"]["online_cpus"]).max(1.0);
    let has_pre = s["precpu_stats"]["system_cpu_usage"].is_number();
    let cpu = if has_pre && sys_delta > 0.0 && cpu_delta >= 0.0 { cpu_delta / sys_delta * cpus * 100.0 } else { 0.0 };
    let mem = n(&s["memory_stats"]["usage"]) - n(&s["memory_stats"]["stats"]["inactive_file"]);
    let io = |op: &str| s["blkio_stats"]["io_service_bytes_recursive"].as_array().into_iter().flatten().filter(|e| e["op"] == op).map(|e| n(&e["value"])).sum::<f64>();
    json!({
        "type": "stats", "id": id, "first": !has_pre,
        "cpu": (cpu * 10.0).round() / 10.0, "cpus": cpus,
        "mem": mem.max(0.0), "limit": n(&s["memory_stats"]["limit"]),
        "rx": n(&s["networks"]["eth0"]["rx_bytes"]), "tx": n(&s["networks"]["eth0"]["tx_bytes"]),
        "read": io("read"), "write": io("write"),
        "pids": n(&s["pids_stats"]["current"]),
    })
}

/// The page's command line: one command, its output and exit code.
pub fn exec(id: String, line: String) {
    spawn("kiri-exec", move || {
        let (code, output) = client::exec(&id, &["sh", "-c", &line]).unwrap_or_else(|e| (-1, e.to_string()));
        emit(json!({"type": "exec", "id": id, "cmd": line, "code": code, "output": output}).to_string());
    });
}

/// A path inside the container, as the page gives it: absolute, no `..`.
fn container_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    if parts.contains(&"..") {
        return None;
    }
    Some(format!("/{}", parts.join("/")))
}

/// Lists a directory: `stat` on each entry, in the container.
pub fn files(name: String, path: String) {
    spawn("kiri-files", move || {
        let ev = |entries: Value, error: Option<String>| {
            let mut v = json!({"type": "files", "name": name, "path": path, "entries": entries});
            if let Some(e) = error {
                v["error"] = json!(e);
            }
            emit(v.to_string());
        };
        let Some(dir) = container_path(&path) else { return ev(json!([]), Some("not a path in the container".into())) };
        // link|type|size|mtime|name per entry; the name last, as it may hold '|'.
        let script = r#"cd -- "$1" || exit 2
for f in .* *; do
  case "$f" in .|..) continue;; esac
  [ -e "$f" ] || [ -L "$f" ] || continue
  if [ -L "$f" ]; then l=L; else l=-; fi
  s=$(stat -L -c '%F|%s|%Y' -- "$f" 2>/dev/null || stat -c '%F|%s|%Y' -- "$f") && printf '%s|%s|%s\n' "$l" "$s" "$f"
done"#;
        match client::exec(&name, &["sh", "-c", script, "sh", &dir]) {
            Ok((0, out)) => {
                let mut entries: Vec<Value> = out
                    .lines()
                    .filter_map(|l| {
                        let mut p = l.splitn(5, '|');
                        let (link, kind, size, mtime, file) = (p.next()?, p.next()?, p.next()?, p.next()?, p.next()?);
                        Some(json!({"name": file, "dir": kind == "directory", "size": size.parse::<u64>().unwrap_or(0), "mtime": mtime.parse::<u64>().unwrap_or(0), "link": link == "L"}))
                    })
                    .collect();
                entries.sort_by(|a, b| b["dir"].as_bool().cmp(&a["dir"].as_bool()).then_with(|| a["name"].as_str().cmp(&b["name"].as_str())));
                ev(json!(entries), None);
            }
            Ok((_, out)) => ev(json!([]), Some(if out.trim().is_empty() { "cannot open this directory".into() } else { out.trim().to_owned() })),
            Err(e) => ev(json!([]), Some(format!("{e} (a container's files can be browsed while it runs)"))),
        }
    });
}

/// A file's first 256 KiB as text, or that it is binary.
pub fn preview(name: String, path: String) {
    spawn("kiri-preview", move || {
        let base = json!({"type": "preview", "name": name, "path": path});
        let with = |k: &str, v: Value| {
            let mut e = base.clone();
            e[k] = v;
            emit(e.to_string());
        };
        let Some(file) = container_path(&path) else { return with("error", json!("not a path in the container")) };
        match read_file(&name, &file) {
            Ok(bytes) if bytes.len() > PREVIEW_MAX || bytes.contains(&0) => with("binary", json!(true)),
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => with("text", json!(text)),
                Err(_) => with("binary", json!(true)),
            },
            Err(e) => with("error", json!(e.to_string())),
        }
    });
}

/// A single safe Windows filename from a container file name. A Linux name
/// can hold `\`, `:`, `..` or a drive/UNC prefix, which `Path::join` would
/// read as path syntax and write outside Downloads (into Startup, or to an
/// SMB server). Everything unsafe becomes `_`.
pub(crate) fn safe_filename(leaf: &str) -> String {
    let mut s: String = leaf
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) || (c as u32) < 0x20 || matches!(c as u32, 0x200B..=0x200F | 0x202A..=0x202E | 0x2066..=0x2069) { '_' } else { c })
        .collect();
    // No leading/trailing dots or spaces (Windows strips them, which could
    // re-expose an extension or an empty name).
    s = s.trim_matches(|c| c == '.' || c == ' ').to_owned();
    s.truncate(200);
    if s.is_empty() {
        s = "file".into();
    }
    // Reserved device names (CON, NUL, COM1…) are unusable as filenames.
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    s
}

/// Copies a file to the Downloads folder, under a free name, and shows it.
pub fn download(name: String, path: String) {
    spawn("kiri-download", move || {
        let result = (|| -> std::io::Result<String> {
            let file = container_path(&path).ok_or_else(|| std::io::Error::other("not a path in the container"))?;
            let bytes = read_file(&name, &file)?;
            let downloads = std::env::var_os("USERPROFILE").map(|h| std::path::PathBuf::from(h).join("Downloads")).ok_or_else(|| std::io::Error::other("no Downloads folder"))?;
            let leaf = safe_filename(file.rsplit('/').next().unwrap_or("file"));
            let (stem, ext) = match leaf.rsplit_once('.') {
                Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
                _ => (leaf.clone(), String::new()),
            };
            // create_new picks the first free name without a separate exists()
            // check, so there is no swap between the check and the write.
            let mut n = 1;
            let (dest, mut f) = loop {
                let candidate = downloads.join(if n == 1 { leaf.clone() } else { format!("{stem} {n}{ext}") });
                // Defence in depth: the name must stay directly in Downloads.
                if candidate.parent() != Some(downloads.as_path()) {
                    return Err(std::io::Error::other("unsafe file name"));
                }
                match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
                    Ok(f) => break (candidate, f),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
                    Err(e) => return Err(e),
                }
            };
            std::io::Write::write_all(&mut f, &bytes)?;
            drop(f);
            // Mark-of-the-Web, as a browser download gets: SmartScreen and
            // Protected View then treat the file as from the internet. Best
            // effort (a FAT volume has no streams).
            let _ = std::fs::write(format!("{}:Zone.Identifier", dest.display()), "[ZoneTransfer]\r\nZoneId=3\r\n");
            crate::app::reveal(&dest);
            Ok(dest.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or(leaf))
        })();
        emit(match result {
            Ok(f) => notice("ok", &trf("Saved to Downloads as {file}", &[("file", &f)])),
            Err(e) => notice("error", &trf("Could not save: {error}", &[("error", &e)])),
        });
    });
}

fn escape(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// One regular file's contents, from the archive API's tar.
fn read_file(container: &str, file: &str) -> std::io::Result<Vec<u8>> {
    let r = client::request("GET", &format!("/containers/{container}/archive?path={}", escape(file)), None)?;
    if !r.ok() {
        return Err(std::io::Error::other(r.error()));
    }
    let tar = r.body;
    if tar.len() < 512 {
        return Err(std::io::Error::other("empty archive"));
    }
    if tar[156] == b'5' {
        return Err(std::io::Error::other("a directory"));
    }
    let size_field = String::from_utf8_lossy(&tar[124..136]);
    let size = usize::from_str_radix(size_field.trim_matches(|c: char| c == '\0' || c.is_whitespace()), 8).map_err(|_| std::io::Error::other("bad archive"))?;
    tar.get(512..512 + size).map(<[u8]>::to_vec).ok_or_else(|| std::io::Error::other("short archive"))
}

/// The docker CLI installed beside kiri-win (kiri.ps1 puts it in bin\),
/// else whichever is on PATH.
pub fn docker_cli() -> String {
    // The installed copy first; then the one beside kiri-win.exe (the
    // unzipped bundle), but only if it is the one this build shipped; else
    // whatever docker.exe is on PATH, found explicitly.
    let installed = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Kiri").join("bin").join("docker.exe"));
    if let Some(p) = installed.filter(|p| p.is_file()) {
        return p.to_string_lossy().into_owned();
    }
    if let Ok(p) = crate::setup::verified("bin/docker.exe") {
        if p.is_file() {
            return p.to_string_lossy().into_owned();
        }
    }
    std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).map(|d| d.join("docker.exe")).find(|d| d.is_file()))
        .map_or_else(|| "docker.exe".to_owned(), |d| d.to_string_lossy().into_owned())
}

/// A console with a shell in the container, as the Mac opens Terminal.
pub fn terminal(name: &str, pipe: &str) {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
        return;
    }
    // \\.\pipe\kiri is npipe:////./pipe/kiri to the docker CLI. Launch the
    // docker CLI directly in its own console, not through `cmd /c start`,
    // which would re-parse `&` etc. in the path. The name is validated above.
    let host = format!("npipe://{}", pipe.replace('\\', "/"));
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    let mut cmd = crate::win::tool(&docker_cli());
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NEW_CONSOLE);
    let spawned = cmd
        .args(["--host", &host, "exec", "-it", name, "sh", "-c", "exec $(command -v bash || command -v sh)"])
        .spawn();
    if let Err(e) = spawned {
        emit(notice("error", &trf("Could not open a terminal: {error}", &[("error", &e)])));
    }
}

#[cfg(test)]
mod tests {
    use super::safe_filename;

    #[test]
    fn filenames_cannot_escape() {
        // The result is always one Windows filename: no separators, colon,
        // drive or UNC prefix survives, and it cannot start with a dot.
        for bad in [r"..\Startup\x.cmd", r"C:\Windows\evil", r"\\host\share\x", "a.txt:ads", "../../etc/passwd"] {
            let got = safe_filename(bad);
            assert!(!got.contains(['\\', '/', ':']) && !got.starts_with('.'), "{bad:?} -> {got:?}");
        }
        // Trailing dots/spaces and empties.
        assert_eq!(safe_filename("name.   "), "name");
        assert_eq!(safe_filename("..."), "file");
        // Reserved device names.
        assert_eq!(safe_filename("NUL"), "_NUL");
        assert_eq!(safe_filename("com1.txt"), "_com1.txt");
        // Ordinary names pass through.
        assert_eq!(safe_filename("report.pdf"), "report.pdf");
    }
}
