//! Settings → Diagnostics on Windows: the window's log
//! (%LOCALAPPDATA%\Kiri\kiri-win.log) and the engine's (/var/log/kiri.log in
//! WSL), as the lines the page shows from ~/.kiri/logs on the Mac
//! (crates/kiri/src/diag/report.rs `page_lines`); a report saved to the
//! Desktop or sent to Kiri Cloud, and the logs folder in Explorer. A panic
//! is a line in the window's log: there are no crash files on Windows.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::engine::emit;

#[path = "../../kiri/src/diag/redact.rs"]
#[allow(dead_code)]
mod redact;
use crate::daemon::time;

/// How much of each log is read from its end, as on the Mac.
const TAIL_BYTES: u64 = 512 << 10;
/// Kiri Cloud takes reports up to 1 MB.
const MAX_BODY: usize = 1 << 20;

fn kiri_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Kiri"))
}

fn redactor() -> redact::Redactor {
    redact::Redactor::new(std::env::var("USERPROFILE").ok(), crate::share::read_config().token.into_iter().collect())
}

/// `2026-10-29T14:07:36.000Z` for a log's seconds since 1970.
/// None for a torn line: two threads once wrote their timestamps back to
/// back (`17933419751793341975 DBG ...`), and that must not panic.
fn stamp(secs: u64) -> Option<String> {
    // Past 2200 the number is not a timestamp.
    if secs > 7_000_000_000 {
        return None;
    }
    let t = time::rfc3339(UNIX_EPOCH.checked_add(Duration::from_secs(secs))?);
    Some(format!("{}.000Z", t.get(..19)?))
}

/// The window's line `2026-11-09T03:29:27.230Z bridge started` (or, as
/// it was written before 0.1.19, `1793282688 bridge started`), as the page's
/// `{time, level, part, msg}`. Panics are errors; failures warnings.
fn window_line(line: &str) -> Option<Value> {
    let (time, msg) = line.split_once(' ')?;
    let time = if time.len() == 24 && time.ends_with('Z') && time.as_bytes()[10] == b'T' { time.to_owned() } else { stamp(time.parse().ok()?)? };
    let lower = msg.to_lowercase();
    let level = if lower.starts_with("panic") {
        "error"
    } else if ["error", "failed", "could not", "couldn't", "can't"].iter().any(|w| lower.contains(w)) {
        "warn"
    } else {
        "info"
    };
    Some(json!({"time": time, "level": level, "part": "ui", "msg": redact::patterns(msg.trim())}))
}

/// The engine's line `2026-10-29T14:07:36.584Z INFO  container start id=9138 name="a b"`:
/// the words up to the first `key=value` are the message, the rest fields.
fn engine_line(line: &str) -> Option<Value> {
    let mut rest = line.trim_end();
    let (time, after) = rest.split_once(' ')?;
    if time.len() < 20 || !time.ends_with('Z') || time.as_bytes()[10] != b'T' {
        return None;
    }
    let (level, after) = after.trim_start().split_once(' ').unwrap_or((after.trim_start(), ""));
    let level = match level {
        "ERROR" => "error",
        "WARN" => "warn",
        "DEBUG" | "TRACE" => "debug",
        _ => "info",
    };
    rest = after.trim_start();
    let mut msg = Vec::new();
    let mut fields = serde_json::Map::new();
    while !rest.is_empty() {
        let end = rest.find(' ').unwrap_or(rest.len());
        let word = &rest[..end];
        match word.split_once('=') {
            Some((k, v)) if !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => {
                let start = rest.len() - rest[k.len() + 1..].len();
                let (value, used) = if v.starts_with('"') {
                    // A quoted value runs to its closing quote, spaces and all.
                    let body = &rest[start + 1..];
                    let close = body.find('"').unwrap_or(body.len());
                    (body[..close].to_owned(), start + 1 + (close + 1).min(body.len()))
                } else {
                    (v.to_owned(), end)
                };
                fields.insert(k.to_owned(), Value::from(redact::patterns(&value)));
                rest = rest[used.min(rest.len())..].trim_start();
            }
            _ if fields.is_empty() => {
                msg.push(word);
                rest = rest[end..].trim_start();
            }
            // Words after the fields began belong to the last field.
            _ => {
                rest = rest[end..].trim_start();
            }
        }
    }
    let mut v = json!({"time": time, "level": level, "part": "daemon", "msg": redact::patterns(&msg.join(" "))});
    if !fields.is_empty() {
        v["fields"] = Value::Object(fields);
    }
    Some(v)
}

fn tail_file(path: &std::path::Path) -> String {
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let len = f.metadata().map_or(0, |m| m.len());
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).is_ok() {
        let _ = f.read_to_end(&mut buf);
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    // A cut first line is dropped.
    if len > TAIL_BYTES { text.split_once('\n').map_or(String::new(), |(_, r)| r.to_owned()) } else { text }
}

/// The engine's log, read in WSL; empty while WSL isn't running, so that
/// opening Diagnostics never starts it.
fn engine_log() -> String {
    if !crate::client::alive() {
        return String::new();
    }
    let mut cmd = crate::win::tool("wsl.exe");
    if let Some(d) = crate::engine::distro() {
        cmd.args(["-d", &d]);
    }
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.args(["-u", "root", "--", "tail", "-c", &TAIL_BYTES.to_string(), "/var/log/kiri.log"]).stderr(std::process::Stdio::null()).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    text.split_once('\n').map_or(String::new(), |(_, r)| r.to_owned())
}

fn read_lines(part: &str) -> Vec<Value> {
    let mut all = Vec::new();
    if part.is_empty() || part == "ui" {
        if let Some(dir) = kiri_dir() {
            all.extend(tail_file(&dir.join("kiri-win.log")).lines().filter_map(window_line));
        }
    }
    if part.is_empty() || part == "daemon" {
        all.extend(engine_log().lines().filter_map(engine_line));
    }
    all.sort_by(|a, b| a["time"].as_str().cmp(&b["time"].as_str()));
    all
}

/// The page's `diaglogs`: the last `n` lines, warnings and errors only with
/// `errors`, only an action's with `op`.
fn page_lines(part: &str, errors: bool, op: &str, n: usize) -> Value {
    let lines: Vec<Value> = read_lines(part)
        .into_iter()
        .filter(|l| !errors || l["level"] == "warn" || l["level"] == "error")
        .filter(|l| op.is_empty() || l["fields"]["op"] == op)
        .collect();
    let skip = lines.len().saturating_sub(n.clamp(1, 2000));
    json!({"type": "diaglogs", "part": part, "op": op, "lines": lines[skip..]})
}

fn windows_version() -> String {
    let mut cmd = crate::win::tool("cmd.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    cmd.args(["/c", "ver"]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "Windows".into())
}

/// The report: versions, the container list if asked, and both logs.
fn report_text(containers: bool) -> String {
    let mut out = format!("Kiri for Windows {}\n{}\n{}\n", env!("CARGO_PKG_VERSION"), windows_version(), std::env::consts::ARCH);
    if containers {
        out.push_str("\n== Containers ==\n");
        match crate::client::get("/containers/json?all=1") {
            Ok(Value::Array(list)) => {
                for c in list {
                    let name = c["Names"][0].as_str().unwrap_or_default().trim_start_matches('/');
                    out.push_str(&format!("{name}  {}  {}\n", c["Image"].as_str().unwrap_or_default(), c["State"].as_str().unwrap_or_default()));
                }
            }
            Ok(_) => {}
            Err(e) => out.push_str(&format!("(not read: {e})\n")),
        }
    }
    let plain = |l: &Value| {
        let fields = l["fields"].as_object().map(|f| f.iter().map(|(k, v)| format!(" {k}={}", v.as_str().unwrap_or_default())).collect::<String>()).unwrap_or_default();
        format!("{} {:5} {}{fields}\n", l["time"].as_str().unwrap_or_default(), l["level"].as_str().unwrap_or_default().to_uppercase(), l["msg"].as_str().unwrap_or_default())
    };
    for (title, part) in [("Window", "ui"), ("Engine", "daemon")] {
        out.push_str(&format!("\n== {title} log ==\n"));
        out.extend(read_lines(part).iter().map(plain));
    }
    redactor().apply(&out)
}

fn save_report(text: &str) -> io::Result<PathBuf> {
    let home = std::env::var_os("USERPROFILE").ok_or_else(|| io::Error::other("USERPROFILE is not set"))?;
    let desktop = PathBuf::from(home).join("Desktop");
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let t = time::rfc3339(UNIX_EPOCH + Duration::from_secs(secs));
    let path = desktop.join(format!("Kiri report {} {}.txt", &t[..10], t[11..19].replace(':', "")));
    std::fs::write(&path, text)?;
    Ok(path)
}

/// This install's random id, made once, as the Mac's ~/.kiri/install_id.
fn install_id() -> String {
    let Some(dir) = kiri_dir() else { return String::new() };
    let path = dir.join("install_id");
    if let Ok(id) = std::fs::read_to_string(&path)
        && id.trim().len() == 32
        && id.trim().bytes().all(|b| b.is_ascii_hexdigit())
    {
        return id.trim().to_owned();
    }
    use std::hash::{BuildHasher, Hasher};
    let half = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.write_u32(std::process::id());
        h.finish()
    };
    let id = format!("{:016x}{:016x}", half(), half());
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, &id);
    id
}

fn send_report(text: &str) -> io::Result<String> {
    let mut text = text.to_owned();
    let body = loop {
        let v = json!({
            "kind": "report",
            "part": "ui",
            "app_version": env!("CARGO_PKG_VERSION"),
            "flavor": if crate::flavor::dev() { "kiri-dev" } else { "kiri" },
            "os": windows_version(),
            "arch": std::env::consts::ARCH,
            "install_id": install_id(),
            "created_at": time::rfc3339(SystemTime::now()),
            "text": text,
        });
        let size = v.to_string().len();
        if size <= MAX_BODY {
            break v;
        }
        // The oldest lines go first.
        const MARK: &str = "[… cut to fit 1 MB]\n";
        let mut cut = (size - MAX_BODY + MARK.len() + 64).min(text.len());
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text = format!("{MARK}{}", &text[cut..]);
    };
    // A report the person sends: signed in, so it can be answered.
    let (status, v) = crate::share::post_report(&body, true)?;
    let why = v["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default();
    match status {
        200 | 201 => v["id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("Kiri Cloud took the report but sent back no id")),
        429 => Err(io::Error::other(format!("Kiri Cloud is taking no more reports for now (429){why}; try again later"))),
        _ => Err(io::Error::other(format!("Kiri Cloud answered {status}{why}"))),
    }
}

/// The page's diagnostics commands; false if `cmd` isn't one.
pub fn handle(cmd: &str, name: String, id: String, value: &Value) -> bool {
    match cmd {
        "diaglogs" => {
            let (errors, n) = (value["errors"] == true, value["n"].as_u64().unwrap_or(200) as usize);
            std::thread::spawn(move || emit(page_lines(&name, errors, &id, n).to_string()));
        }
        "diagreport" => {
            let containers = value["containers"] == true;
            std::thread::spawn(move || {
                emit(match save_report(&report_text(containers)) {
                    Ok(path) => {
                        crate::app::reveal(&path);
                        json!({"type": "diagreport", "path": redactor().apply(&path.display().to_string())})
                    }
                    Err(e) => json!({"type": "diagreport", "error": e.to_string()}),
                }
                .to_string());
            });
        }
        "diagsend" => {
            let containers = value["containers"] == true;
            std::thread::spawn(move || {
                emit(match send_report(&report_text(containers)) {
                    Ok(id) => {
                        crate::serve::log(&format!("report sent id={id}"));
                        json!({"type": "diagsend", "id": id})
                    }
                    Err(e) => {
                        crate::serve::log(&format!("report not sent: {e}"));
                        json!({"type": "diagsend", "error": e.to_string()})
                    }
                }
                .to_string());
            });
        }
        "diagcrashes" => emit(json!({"type": "diagcrashes", "crashes": []}).to_string()),
        "diagsendcrash" => emit(json!({"type": "diagsendcrash", "name": name, "error": "Windows keeps no crash files: send a report instead"}).to_string()),
        "diagfolder" => {
            if let Some(dir) = kiri_dir() {
                let mut cmd = crate::win::tool("explorer.exe");
                let _ = cmd.arg(dir).spawn();
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_lines_read_as_the_pages() {
        let v = window_line("1793282688 bridge started").unwrap();
        assert_eq!((v["time"].as_str(), v["level"].as_str(), v["part"].as_str(), v["msg"].as_str()), (Some("2026-10-29T14:04:48.000Z"), Some("info"), Some("ui"), Some("bridge started")));
        assert_eq!(window_line("1793282688 panic: at x").unwrap()["level"], "error");
        assert_eq!(window_line("1793282688 bridge failed: gone").unwrap()["level"], "warn");
        assert!(window_line("not a line").is_none());
        let v = window_line("2026-11-09T03:29:27.230Z setup engine: couldn't pull alpine").unwrap();
        assert_eq!((v["time"].as_str(), v["level"].as_str()), (Some("2026-11-09T03:29:27.230Z"), Some("warn")));
        // Two threads once wrote their timestamps back to back; that line is skipped, not a panic.
        assert!(window_line("17933419751793341975 DBG conn 14 \"POST /x\" end=4").is_none());
    }

    #[test]
    fn engine_lines_split_message_and_fields() {
        let v = engine_line(r#"2026-10-29T14:07:36.584Z INFO  container start id=9138 name="a b" image=grafana/grafana:latest"#).unwrap();
        assert_eq!(v["msg"], "container start");
        assert_eq!(v["level"], "info");
        assert_eq!(v["part"], "daemon");
        assert_eq!(v["fields"]["id"], "9138");
        assert_eq!(v["fields"]["name"], "a b");
        assert_eq!(v["fields"]["image"], "grafana/grafana:latest");
        let w = engine_line("2026-10-29T13:31:57.839Z WARN  VM stopped on request").unwrap();
        assert_eq!((w["level"].as_str(), w["msg"].as_str()), (Some("warn"), Some("VM stopped on request")));
        assert!(w.get("fields").is_none());
        assert!(engine_line("garbage").is_none());
    }
}
