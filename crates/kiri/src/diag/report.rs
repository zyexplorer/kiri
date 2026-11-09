//! `kiri diag`: Kiri's own log, read or followed, and the report to send us.
//!
//! The report is one text file: versions and the system, whether the
//! engine runs, the last lines of every log, recent crash files and macOS's
//! own crash reports for Kiri, all through [`Redactor::for_user`].
//!
//! # Kiri Cloud
//!
//! `kiri diag report --send` (and, only when the person turned crash
//! reports on, each new crash file) goes to `POST /v1/reports` on the Kiri
//! Share edge, as JSON of at most 1 MB:
//!
//! ```text
//! {"kind":"crash"|"report","part":"ui"|"daemon"|"cli","app_version":"0.1.4",
//!  "flavor":"kiri"|"kiri-dev","os":"macOS 26.1","arch":"aarch64",
//!  "install_id":"<32 hex digits, kept in ~/.kiri/install_id>",
//!  "created_at":"<RFC 3339>","text":"<the redacted report or crash>"}
//! ```
//!
//! A report the person sends carries `Authorization: Bearer <token>` when
//! signed in to Kiri Cloud, so it can be answered. A crash never does: it
//! goes without the person's account. Crash uploads also leave out the image
//! and container names their log lines name. It answers `201 {"id":"rep_…"}`, the id a person can quote to us. Crash
//! files that went become `<name>.sent.txt`; any other answer leaves them
//! for the next try.

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::Part;
use super::redact::Redactor;
use crate::daemon::time;

const USAGE: &str = "\
usage: kiri diag logs [-f] [--ui|--engine|--cli] [--errors] [--op ID] [-n N]
       kiri diag report [--out PATH] [--send]

logs     Kiri's own log, oldest first: the window (--ui), the engine
         (--engine) and the command line (--cli), all three by default.
         --errors keeps warnings and errors; --op ID one window action's
         lines (the id an error's Details show); -n N lines (default 50);
         -f keeps printing new ones.
report   One file to send us: versions, whether the engine runs, recent
         log lines and crashes, with your home folder and any tokens taken
         out. Saved to the Desktop (or --out PATH). --send also sends it to
         Kiri Cloud and prints its id.";

/// A report body may be at most this big.
pub const MAX_BODY: usize = 1 << 20;
/// Lines of each log in a report.
const REPORT_LINES: usize = 300;
/// How much of one log a report or `kiri diag logs` reads, from the end.
const TAIL_BYTES: u64 = 512 << 10;
/// How much of each log a report keeps.
const LOG_BUDGET: usize = 120 << 10;
const CRASHES: usize = 5;
/// Log lines a crash upload carries: its part's last ones before it.
const CRASH_LOG_LINES: usize = 200;
const CRASH_BUDGET: usize = 48 << 10;
const MAC_REPORTS: usize = 3;
const MAC_REPORT_BUDGET: usize = 24 << 10;

pub fn run(args: &[String]) -> io::Result<()> {
    match args.first().map(String::as_str) {
        Some("logs") => logs_cli(&args[1..]),
        Some("report") => report_cli(&args[1..]),
        _ => Err(io::Error::other(USAGE)),
    }
}

// ---------------------------------------------------------------- reading logs

fn log_file(state: &Path, part: Part) -> PathBuf {
    state.join("logs").join(format!("{}.log", part.name()))
}

/// The last `max` bytes of a file, from the first whole line.
fn tail(path: &Path, max: u64) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(max);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(max).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    match (from > 0, text.find('\n')) {
        (true, Some(i)) => text[i + 1..].to_owned(),
        (true, None) => String::new(),
        (false, _) => text,
    }
}

/// Which lines to show: warnings and errors only, one action's only.
#[derive(Debug, Default, Clone, Copy)]
pub struct Filter<'a> {
    pub errors: bool,
    pub op: Option<&'a str>,
}

impl Filter<'_> {
    fn keep(&self, v: &Value) -> bool {
        (!self.errors || matches!(v["level"].as_str(), Some("error" | "warn")))
            && self
                .op
                .is_none_or(|op| v["fields"]["op"].as_str() == Some(op))
    }
}

/// The last `n` events of these parts' logs (their moved-aside `.1` too)
/// that pass `filter`, oldest first.
pub fn read_lines(state: &Path, parts: &[Part], filter: Filter<'_>, n: usize) -> Vec<Value> {
    let mut all: Vec<Value> = Vec::new();
    for &part in parts {
        let path = log_file(state, part);
        for p in [super::rotated(&path, 1), path] {
            all.extend(
                tail(&p, TAIL_BYTES)
                    .lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .filter(|v| v.is_object() && filter.keep(v)),
            );
        }
    }
    // RFC 3339 in UTC sorts as text; the sort is stable within a log.
    all.sort_by(|a, b| a["time"].as_str().cmp(&b["time"].as_str()));
    let skip = all.len().saturating_sub(n);
    all.drain(..skip);
    all
}

/// The local time of a log line's UTC one: `2026-10-28 10:41:07.123`.
fn local(t: &str) -> String {
    let secs = time::parse_unix(t);
    if secs == 0 {
        return t.to_owned();
    }
    let ms = t
        .get(20..23)
        .filter(|m| m.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or("000");
    let at = secs as libc::time_t;
    // SAFETY: localtime_r fills the tm it is given and keeps no pointer.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&at, &mut tm).is_null() {
            return t.to_owned();
        }
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{ms}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// A log line for people, fields by name: `<time> WARN  daemon container exited exit_code=137 id=3f2a`.
/// `utc` keeps the line's own time (reports, read elsewhere); `color` for a terminal.
pub fn render(v: &Value, utc: bool, color: bool) -> String {
    let time = v["time"].as_str().unwrap_or("");
    let level = v["level"].as_str().unwrap_or("?");
    let tag = format!("{:<5}", level.to_ascii_uppercase());
    let tag = match (color, level) {
        (true, "error") => format!("\x1b[31m{tag}\x1b[0m"),
        (true, "warn") => format!("\x1b[33m{tag}\x1b[0m"),
        (true, "debug") => format!("\x1b[2m{tag}\x1b[0m"),
        _ => tag,
    };
    let mut s = format!(
        "{} {tag} {:<6} {}",
        if utc { time.to_owned() } else { local(time) },
        v["part"].as_str().unwrap_or("?"),
        v["msg"].as_str().unwrap_or("")
    );
    if let Some(fields) = v["fields"].as_object() {
        for (k, val) in fields {
            let val = val
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| val.to_string());
            if val.contains(char::is_whitespace) || val.is_empty() {
                let _ = write!(s, " {k}={val:?}");
            } else {
                let _ = write!(s, " {k}={val}");
            }
        }
    }
    s
}

fn logs_cli(args: &[String]) -> io::Result<()> {
    let (mut parts, mut follow, mut errors, mut n, mut op) =
        (Vec::new(), false, false, 50usize, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-f" | "--follow" => follow = true,
            "--ui" | "--window" => parts.push(Part::Ui),
            "--engine" | "--daemon" => parts.push(Part::Daemon),
            "--cli" => parts.push(Part::Cli),
            "--errors" => errors = true,
            "--op" => {
                op = Some(
                    it.next()
                        .ok_or_else(|| io::Error::other("--op needs an id"))?
                        .clone(),
                )
            }
            "-n" => {
                n = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| io::Error::other("-n needs a number"))?
            }
            s if s
                .strip_prefix("-n")
                .is_some_and(|v| v.parse::<usize>().is_ok()) =>
            {
                n = s[2..].parse().unwrap_or(n)
            }
            _ => return Err(io::Error::other(USAGE)),
        }
    }
    if parts.is_empty() {
        parts = Part::ALL.to_vec();
    }
    let state = super::state_dir().ok_or_else(|| io::Error::other("HOME is not set"))?;
    let color = io::stdout().is_terminal();
    let mut out = io::stdout().lock();
    let filter = Filter {
        errors,
        op: op.as_deref(),
    };
    let lines = read_lines(&state, &parts, filter, n);
    if lines.is_empty() && !follow {
        eprintln!(
            "nothing logged yet in {}",
            Redactor::home_only().apply(&state.join("logs").display().to_string())
        );
    }
    for v in &lines {
        writeln!(out, "{}", render(v, false, color))?;
    }
    if !follow {
        return Ok(());
    }
    out.flush()?;
    // From the end of each log as it is now; a log that shrank was moved
    // aside, and is read again from its start.
    let mut at: Vec<(PathBuf, u64, Vec<u8>)> = parts
        .iter()
        .map(|&p| log_file(&state, p))
        .map(|p| {
            let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            (p, len, Vec::new())
        })
        .collect();
    loop {
        std::thread::sleep(Duration::from_millis(400));
        for (path, offset, partial) in &mut at {
            let len = std::fs::metadata(&*path).map(|m| m.len()).unwrap_or(0);
            if len < *offset {
                *offset = 0;
                partial.clear();
            }
            if len == *offset {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(&*path) else {
                continue;
            };
            f.seek(SeekFrom::Start(*offset))?;
            let mut got = Vec::new();
            f.take(len - *offset).read_to_end(&mut got)?;
            *offset += got.len() as u64;
            partial.extend_from_slice(&got);
            let Some(end) = partial.iter().rposition(|&b| b == b'\n') else {
                continue;
            };
            let whole: Vec<u8> = partial.drain(..=end).collect();
            for line in String::from_utf8_lossy(&whole).lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line)
                    && filter.keep(&v)
                {
                    writeln!(out, "{}", render(&v, false, color))?;
                }
            }
            out.flush()?;
        }
    }
}

// ---------------------------------------------------------------- the report

/// What the report says beyond the files.
#[derive(Debug)]
pub struct Facts {
    pub engine: String,
    pub crash_reports: bool,
    pub created: SystemTime,
    /// The container list, when the person ticked it in the window.
    pub containers: Option<String>,
}

/// The engine as the report describes it, asked without waking the VM.
fn engine_line() -> String {
    if !crate::client::alive() {
        return "not running".into();
    }
    let version = crate::client::get_background("/version").unwrap_or_default();
    let info = crate::client::get_background("/info").unwrap_or_default();
    format!(
        "running (engine {}, VM {}, {} of {} containers running, {} images)",
        version["Version"].as_str().unwrap_or("?"),
        if info["KiriVmAwake"] == true {
            "awake"
        } else {
            "asleep"
        },
        info["ContainersRunning"].as_u64().unwrap_or(0),
        info["Containers"].as_u64().unwrap_or(0),
        info["Images"].as_u64().unwrap_or(0),
    )
}

/// The engine's containers, one a line: name, image and status. Nothing
/// from inside them. Asked without waking the VM.
fn container_lines() -> String {
    if !crate::client::alive() {
        return "(the engine is not running)\n".into();
    }
    let list = crate::client::get_background("/containers/json?all=1").unwrap_or_default();
    let mut s = String::new();
    for c in list.as_array().into_iter().flatten() {
        let name = c["Names"][0]
            .as_str()
            .unwrap_or("?")
            .trim_start_matches('/');
        let _ = writeln!(
            s,
            "{name}  {}  {}",
            c["Image"].as_str().unwrap_or("?"),
            c["Status"].as_str().unwrap_or("?")
        );
    }
    if s.is_empty() {
        s.push_str("(none)\n");
    }
    s
}

/// At most `max` bytes of `text`, from its end, starting at a whole line.
fn last_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut from = text.len() - max;
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let cut = &text[from..];
    cut.find('\n').map_or(cut, |i| &cut[i + 1..])
}

/// At most `max` bytes of `text`, from its start, marked when cut.
fn first_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut to = max;
    while !text.is_char_boundary(to) {
        to -= 1;
    }
    format!("{}\n[… {} more bytes]\n", &text[..to], text.len() - to)
}

/// Files in `dir` whose names pass `want`, newest first by modification time.
fn newest(dir: &Path, want: impl Fn(&str) -> bool) -> Vec<(PathBuf, SystemTime)> {
    let mut found: Vec<(PathBuf, SystemTime)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| want(&e.file_name().to_string_lossy()))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    found
}

/// The crash files in `dir`, sent or not, newest first by their time.
fn crash_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| crash_name(n).is_some())
        .collect();
    names.sort_by_key(|n| std::cmp::Reverse(crash_name(n).map_or(0, |(t, _)| t)));
    names
}

/// `1792800000-daemon.txt` (or `-1.txt`, `.sent.txt`, `.txt.sending`):
/// its time and part.
fn crash_name(name: &str) -> Option<(u64, Part)> {
    let stem = name
        .strip_suffix(".sending")
        .unwrap_or(name)
        .strip_suffix(".txt")?;
    let stem = stem.strip_suffix(".sent").unwrap_or(stem);
    let mut bits = stem.split('-');
    let secs = bits.next()?.parse().ok()?;
    let part = Part::parse(bits.next()?)?;
    Some((secs, part))
}

/// The report's text, redacted. `home` holds `Library/Logs/DiagnosticReports`;
/// `state` is `~/.kiri`.
pub fn assemble(home: &Path, state: &Path, facts: &Facts, redactor: &Redactor) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "Kiri diagnostics report");
    let _ = writeln!(s, "Created:  {}", time::rfc3339(facts.created));
    let _ = writeln!(
        s,
        "App:      {} {} ({}), made by the {}",
        crate::flavor::APP,
        env!("CARGO_PKG_VERSION"),
        crate::flavor::CONTEXT,
        super::part().name()
    );
    let _ = writeln!(s, "System:   {}", super::system().line());
    let _ = writeln!(s, "Engine:   {}", facts.engine);
    let _ = writeln!(
        s,
        "Crash reports: {}",
        if facts.crash_reports {
            "sent automatically"
        } else {
            "not sent automatically"
        }
    );

    for (part, title) in [
        (Part::Daemon, "engine"),
        (Part::Ui, "window"),
        (Part::Cli, "command line"),
    ] {
        let path = log_file(state, part);
        let lines: Vec<String> = read_lines(state, &[part], Filter::default(), REPORT_LINES)
            .iter()
            .map(|v| render(v, true, false))
            .collect();
        let _ = writeln!(s, "\n==== Log: {title} ({}) ====", path.display());
        if lines.is_empty() {
            let _ = writeln!(s, "(empty)");
        } else {
            s.push_str(last_bytes(&(lines.join("\n") + "\n"), LOG_BUDGET));
        }
    }

    // The engine's own stderr, as launchd (or the window) keeps it.
    let raw = state.join("daemon.log");
    let _ = writeln!(s, "\n==== Engine output ({}) ====", raw.display());
    let text = tail(&raw, LOG_BUDGET as u64);
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        let _ = writeln!(s, "(empty)");
    } else {
        let _ = writeln!(s, "{}", lines[lines.len().saturating_sub(200)..].join("\n"));
    }

    if let Some(list) = &facts.containers {
        let _ = writeln!(s, "\n==== Containers (names, images and status only) ====");
        s.push_str(list);
    }

    let crashes = state.join("crashes");
    let names = crash_files(&crashes);
    let _ = writeln!(
        s,
        "\n==== Crashes ({}, {} in all) ====",
        crashes.display(),
        names.len()
    );
    for name in names.iter().take(CRASHES) {
        let text = std::fs::read_to_string(crashes.join(name)).unwrap_or_default();
        let _ = writeln!(s, "--- {name} ---");
        s.push_str(&first_bytes(&text, CRASH_BUDGET));
    }

    // What macOS wrote down itself: crashes our hook can't see (a signal, a
    // hang the system killed, an abort inside a framework).
    let reports = home.join("Library/Logs/DiagnosticReports");
    let month = SystemTime::now() - Duration::from_secs(30 * 86_400);
    let ips: Vec<_> = newest(&reports, |n| {
        n.to_ascii_lowercase().starts_with("kiri") && n.ends_with(".ips")
    })
    .into_iter()
    .filter(|(_, t)| *t >= month)
    .collect();
    let _ = writeln!(
        s,
        "\n==== macOS crash reports ({}, last 30 days: {}) ====",
        reports.display(),
        ips.len()
    );
    for (path, _) in ips.iter().take(MAC_REPORTS) {
        let text = String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned();
        let _ = writeln!(
            s,
            "--- {} ---",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        s.push_str(&first_bytes(&text, MAC_REPORT_BUDGET));
        s.push('\n');
    }
    redactor.apply(&s)
}

/// This Mac's report, as `kiri diag report` and the window make it; the
/// window may add the container list.
pub fn report_text(containers: bool) -> io::Result<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let state = home.join(crate::flavor::STATE.get());
    let facts = Facts {
        engine: engine_line(),
        crash_reports: crash_reports_on(),
        created: SystemTime::now(),
        containers: containers.then(container_lines),
    };
    Ok(assemble(&home, &state, &facts, &Redactor::for_user()))
}

/// `~/Desktop/kiri-report-20261028-104107.txt` (the home folder when there
/// is no Desktop).
fn default_out() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let desktop = home.join("Desktop");
    let stamp: String = local(&super::now())
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == ' ')
        .take(15)
        .collect::<String>()
        .replace(' ', "-");
    Ok(if desktop.is_dir() { desktop } else { home }.join(format!("kiri-report-{stamp}.txt")))
}

/// Writes the report, readable only by this user. Returns where.
pub fn save_report(text: &str, out: Option<PathBuf>) -> io::Result<PathBuf> {
    let path = match out {
        Some(p) => p,
        None => default_out()?,
    };
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(text.as_bytes())?;
    Ok(path)
}

/// Shows a file in Finder.
pub fn reveal(path: &Path) {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(path)
            .spawn();
    }
}

fn report_cli(args: &[String]) -> io::Result<()> {
    let (mut out, mut send_it) = (None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" | "-o" => {
                out = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| io::Error::other("--out needs a path"))?,
                ))
            }
            "--send" => send_it = true,
            _ => return Err(io::Error::other(USAGE)),
        }
    }
    let text = report_text(false)?;
    let chosen = out.is_some();
    let path = save_report(&text, out)?;
    println!(
        "saved the report to {}",
        Redactor::home_only().apply(&path.display().to_string())
    );
    if !chosen {
        reveal(&path);
    }
    if send_it {
        let id = send("report", super::part(), SystemTime::now(), &text)?;
        super::info!("report sent", id = id);
        println!("sent to Kiri Cloud as {id}: quote it when you write to us");
    }
    Ok(())
}

// ---------------------------------------------------------------- Kiri Cloud

/// `~/.kiri/install_id`: 128 random bits in hex, made on first use, so
/// reports from one Mac can be told apart without saying whose it is.
pub fn install_id(state: &Path) -> String {
    let path = state.join("install_id");
    if let Ok(id) = std::fs::read_to_string(&path)
        && id.trim().len() == 32
        && id.trim().bytes().all(|b| b.is_ascii_hexdigit())
    {
        return id.trim().to_owned();
    }
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        // Not expected; the time and pid are still unlikely to repeat.
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            ^ u128::from(std::process::id());
        bytes = seed.to_le_bytes();
    }
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::create_dir_all(state);
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(id.as_bytes()));
    id
}

/// The upload's JSON, the text cut short (and marked) to keep it within
/// [`MAX_BODY`].
pub fn body(kind: &str, part: Part, created: SystemTime, text: &str, install_id: &str) -> Value {
    let sys = super::system();
    let mut text = text.to_owned();
    loop {
        let v = json!({
            "kind": kind,
            "part": part.name(),
            "app_version": env!("CARGO_PKG_VERSION"),
            "flavor": super::flavor(),
            "os": sys.os,
            "arch": sys.arch,
            "install_id": install_id,
            "created_at": time::rfc3339(created),
            "text": text,
        });
        let size = v.to_string().len();
        if size <= MAX_BODY {
            return v;
        }
        const MARK: &str = "\n[… cut to fit 1 MB]\n";
        let mut keep = text.len().saturating_sub(size - MAX_BODY + MARK.len() + 64);
        while !text.is_char_boundary(keep) {
            keep -= 1;
        }
        text.truncate(keep);
        text.push_str(MARK);
    }
}

/// Sends a report or crash; returns the id Kiri Cloud gave it.
pub fn send(kind: &str, part: Part, created: SystemTime, text: &str) -> io::Result<String> {
    let state = super::state_dir().ok_or_else(|| io::Error::other("HOME is not set"))?;
    let body = body(kind, part, created, text, &install_id(&state));
    // Only a report the person sends carries their sign-in; a crash never does.
    let (status, v) = crate::share::post_report(&body, kind == "report")?;
    let why = v["error"]
        .as_str()
        .map(|e| format!(": {e}"))
        .unwrap_or_default();
    match status {
        200 | 201 => v["id"]
            .as_str()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("Kiri Cloud took the report but sent back no id")),
        429 => Err(io::Error::other(format!(
            "Kiri Cloud is taking no more reports for now (429){why}; try again later"
        ))),
        _ => Err(io::Error::other(format!(
            "Kiri Cloud answered {status}{why}"
        ))),
    }
}

/// Whether the person turned on sending crash reports (`"crashReports":
/// true` in `ui.json`, which the window's Settings write). Off by default.
#[cfg(any(target_os = "macos", all(target_os = "linux", feature = "window")))]
pub fn crash_reports_on() -> bool {
    crate::ui::prefs()["crashReports"] == true
}

/// No window, no setting: a Linux host never sends crashes by itself.
#[cfg(not(any(target_os = "macos", all(target_os = "linux", feature = "window"))))]
pub fn crash_reports_on() -> bool {
    false
}

/// When crash reports were turned on, in Unix seconds. 0 for a setting from
/// before this was kept: those people's waiting crashes still go.
#[cfg(any(target_os = "macos", all(target_os = "linux", feature = "window")))]
fn crash_reports_since() -> u64 {
    crate::ui::prefs()["crashReportsSince"]
        .as_u64()
        .unwrap_or(0)
}

#[cfg(not(any(target_os = "macos", all(target_os = "linux", feature = "window"))))]
fn crash_reports_since() -> u64 {
    0
}

/// Crash files waiting to go, oldest first: not sent, not being sent, and not
/// from before `since`. Turning reports on doesn't send what came earlier,
/// which includes crashes the person answered "Not now" to.
fn pending(dir: &Path, since: u64) -> Vec<String> {
    let waiting = |n: &String| {
        !n.ends_with(".sent.txt")
            && !n.ends_with(".sending")
            && crash_name(n).is_some_and(|(t, _)| t >= since)
    };
    let mut names: Vec<String> = crash_files(dir).into_iter().filter(waiting).collect();
    names.reverse();
    names
}

/// Sends the crash files not sent yet, when the person opted in. Stops at
/// the first failure: the rest wait for the next try.
pub fn send_crashes() {
    if !crash_reports_on() {
        return;
    }
    let Some(dir) = super::crashes_dir() else {
        return;
    };
    // A sender that died mid-way leaves `.sending`: free it after a while.
    for (path, t) in newest(&dir, |n| n.ends_with(".sending")) {
        if t.elapsed().unwrap_or_default() > Duration::from_secs(600) {
            let _ = std::fs::rename(&path, path.with_extension(""));
        }
    }
    let redactor = Redactor::for_user();
    for name in pending(&dir, crash_reports_since()) {
        // Another process (the window and the engine both try) has it.
        let Some(claimed) = claim(&dir, &name) else {
            continue;
        };
        if let Err(e) = send_claimed(&dir, &name, &claimed, &redactor) {
            super::warn!(
                "crash report not sent; it is tried again later",
                file = name,
                error = e
            );
            break;
        }
    }
}

/// Takes a crash file for sending (`<name>.sending`), unless another
/// sender has it.
fn claim(dir: &Path, name: &str) -> Option<PathBuf> {
    crash_name(name)?;
    let claimed = dir.join(format!("{name}.sending"));
    std::fs::rename(dir.join(name), &claimed).ok()?;
    Some(claimed)
}

/// Sends a claimed crash file: sent, it becomes `<stem>.sent.txt` ending
/// with its id; not, it goes back for the next try.
fn send_claimed(dir: &Path, name: &str, claimed: &Path, redactor: &Redactor) -> io::Result<String> {
    let (secs, part) = crash_name(name).ok_or_else(|| io::Error::other("not a crash file"))?;
    let text = std::fs::read_to_string(claimed).unwrap_or_default();
    let text = match super::state_dir() {
        Some(state) => crash_upload(&state, secs, part, &text),
        None => text,
    };
    match send(
        "crash",
        part,
        UNIX_EPOCH + Duration::from_secs(secs),
        &redactor.apply(&text),
    ) {
        Ok(id) => {
            let _ = std::fs::OpenOptions::new()
                .append(true)
                .open(claimed)
                .and_then(|mut f| writeln!(f, "\nSent to Kiri Cloud as {id}"));
            let sent = dir.join(format!(
                "{}.sent.txt",
                name.strip_suffix(".txt").unwrap_or(name)
            ));
            let _ = std::fs::rename(claimed, sent);
            super::info!("crash report sent", file = name, id = id);
            Ok(id)
        }
        Err(e) => {
            let _ = std::fs::rename(claimed, dir.join(name));
            Err(e)
        }
    }
}

/// Log fields that name the person's images and containers.
const NAME_FIELDS: [&str; 5] = ["image", "name", "container", "reference", "repo"];

/// What a crash upload holds: the crash file, then the last lines its part
/// logged up to the crash, without the image and container names they
/// carry, wherever those appear.
fn crash_upload(state: &Path, secs: u64, part: Part, text: &str) -> String {
    let until = time::rfc3339(UNIX_EPOCH + Duration::from_secs(secs + 1));
    let mut lines: Vec<Value> = read_lines(state, &[part], Filter::default(), 2000)
        .into_iter()
        .filter(|v| v["time"].as_str().is_some_and(|t| t < until.as_str()))
        .collect();
    lines.drain(..lines.len().saturating_sub(CRASH_LOG_LINES));
    let mut s = text.trim_end().to_owned();
    let _ = writeln!(
        s,
        "\n\n==== The last {} log lines before it ====",
        lines.len()
    );
    for l in &lines {
        let _ = writeln!(s, "{}", render(l, true, false));
    }
    without_names(&s, &lines)
}

/// `text` with every image and container name that `lines` log taken out,
/// longest first, so `web` doesn't leave half of `web-admin`.
fn without_names(text: &str, lines: &[Value]) -> String {
    let mut names: Vec<&str> = lines
        .iter()
        .flat_map(|v| {
            NAME_FIELDS
                .iter()
                .filter_map(move |k| v["fields"][*k].as_str())
        })
        .map(str::trim)
        .filter(|n| n.len() >= 3 && *n != "[redacted]")
        .collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    names.dedup();
    names
        .into_iter()
        .fold(text.to_owned(), |s, n| replace_whole(&s, n, "[name]"))
}

/// Every `name` in `s` that stands alone (not inside a longer word, so a
/// container `web` leaves `webpki` be, but not `web.kiri.local`).
fn replace_whole(s: &str, name: &str, with: &str) -> String {
    let joins = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-');
    let (mut out, mut last, mut from) = (String::with_capacity(s.len()), 0, 0);
    while let Some(i) = s[from..].find(name).map(|i| i + from) {
        let end = i + name.len();
        if !joins(s[..i].chars().next_back()) && !joins(s[end..].chars().next()) {
            out.push_str(&s[last..i]);
            out.push_str(with);
            last = end;
        }
        from = end;
    }
    out.push_str(&s[last..]);
    out
}

/// The id a sent crash file ends with.
fn sent_id(text: &str) -> Option<&str> {
    text.lines()
        .rev()
        .find_map(|l| l.strip_prefix("Sent to Kiri Cloud as "))
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

/// Sends one crash file the person picked in the window, whether or not
/// crash reports go by themselves. Returns its id.
pub fn send_crash(name: &str) -> io::Result<String> {
    let dir = super::crashes_dir().ok_or_else(|| io::Error::other("HOME is not set"))?;
    if name.contains('/') || name.ends_with(".sending") || crash_name(name).is_none() {
        return Err(io::Error::other("not a crash file"));
    }
    if name.ends_with(".sent.txt") {
        let text = std::fs::read_to_string(dir.join(name))?;
        return sent_id(&text)
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("it was sent, but its id isn't in the file"));
    }
    let claimed =
        claim(&dir, name).ok_or_else(|| io::Error::other("Kiri is sending it already"))?;
    send_claimed(&dir, name, &claimed, &Redactor::for_user())
}

/// The window's Diagnostics: the newest `n` crash files with their
/// message, where, whether they went (and the id), and the text.
fn crash_list(dir: &Path, n: usize) -> Vec<Value> {
    crash_files(dir)
        .into_iter()
        .take(n)
        .filter_map(|name| {
            let (secs, part) = crash_name(&name)?;
            let text = std::fs::read_to_string(dir.join(&name)).unwrap_or_default();
            let field = |k: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(k))
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            };
            Some(json!({
                "name": name,
                "part": part.name(),
                "time": time::rfc3339(UNIX_EPOCH + Duration::from_secs(secs)),
                "message": field("Message:"),
                "location": field("Location:"),
                "sent": name.ends_with(".sent.txt"),
                "sending": name.ends_with(".sending"),
                "id": sent_id(&text),
                "text": first_bytes(&text, CRASH_BUDGET),
            }))
        })
        .collect()
}

/// The page's `diagcrashes`.
pub fn page_crashes(n: usize) -> Value {
    let crashes = super::crashes_dir()
        .map(|dir| crash_list(&dir, n))
        .unwrap_or_default();
    json!({"type": "diagcrashes", "crashes": crashes})
}

/// Sends waiting crash files in the background after `delay`, then every
/// `every` if given (the engine runs for days).
pub fn spawn_crash_sender(delay: Duration, every: Option<Duration>) {
    let _ = std::thread::Builder::new()
        .name("kiri-crash-send".into())
        .spawn(move || {
            std::thread::sleep(delay);
            loop {
                send_crashes();
                match every {
                    Some(d) => std::thread::sleep(d),
                    None => return,
                }
            }
        });
}

/// The newest crash since the window last opened, for the page's
/// `lastcrash` event; remembers this opening. Nothing the first time.
pub fn last_crash() -> Option<Value> {
    let dir = super::crashes_dir()?;
    let marker = dir.join(".window-opened");
    let since: Option<u64> = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse().ok());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&marker, now.to_string());
    let since = since?;
    let names: Vec<String> = crash_files(&dir)
        .into_iter()
        .filter(|n| crash_name(n).is_some_and(|(t, _)| t >= since))
        .collect();
    let name = names.first()?;
    let (secs, part) = crash_name(name)?;
    let text = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
    let message = text
        .lines()
        .find_map(|l| l.strip_prefix("Message:"))
        .unwrap_or("")
        .trim()
        .to_owned();
    Some(json!({
        "type": "lastcrash",
        "name": name,
        "id": sent_id(&text),
        "part": part.name(),
        "time": time::rfc3339(UNIX_EPOCH + Duration::from_secs(secs)),
        "message": message,
        "file": Redactor::home_only().apply(&dir.join(name).display().to_string()),
        "count": names.len(),
        "sent": name.ends_with(".sent.txt"),
        "crashReports": crash_reports_on(),
    }))
}

/// The page's `diaglogs`: the last `n` events, as their JSON objects; with
/// `op`, only that action's lines, from the window and the engine.
pub fn page_lines(part: &str, errors: bool, op: &str, n: usize) -> Value {
    let parts = Part::parse(part).map_or_else(|| Part::ALL.to_vec(), |p| vec![p]);
    let filter = Filter {
        errors,
        op: Some(op).filter(|o| !o.is_empty()),
    };
    let lines = super::state_dir()
        .map(|s| read_lines(&s, &parts, filter, n.clamp(1, 2000)))
        .unwrap_or_default();
    json!({"type": "diaglogs", "part": part, "op": op, "lines": lines})
}

#[cfg(test)]
mod tests {
    use super::super::Level;
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kiri-report-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn line(time: &str, level: Level, part: Part, msg: &str) -> String {
        super::super::json_line(time, level, part, msg, &[])
    }

    #[test]
    fn reads_the_last_lines_of_several_logs_in_time_order() {
        let home = temp_home("read");
        let state = home.join(".kiri");
        std::fs::create_dir_all(state.join("logs")).unwrap();
        let tagged = |t: &str, part: Part, msg: &str| {
            super::super::json_line(t, Level::Info, part, msg, &[("op", "run-7c2e".into())])
        };
        let daemon = [
            tagged("2026-10-28T09:00:01.000Z", Part::Daemon, "a"),
            line("2026-10-28T09:00:03.000Z", Level::Error, Part::Daemon, "c"),
        ]
        .join("\n");
        std::fs::write(state.join("logs/daemon.log"), daemon + "\nnot json\n").unwrap();
        std::fs::write(
            state.join("logs/daemon.log.1"),
            line("2026-10-28T08:00:00.000Z", Level::Warn, Part::Daemon, "old") + "\n",
        )
        .unwrap();
        std::fs::write(
            state.join("logs/ui.log"),
            super::super::json_line(
                "2026-10-28T09:00:02.000Z",
                Level::Warn,
                Part::Ui,
                "b",
                &[("op", "run-7c2e".into())],
            ) + "\n",
        )
        .unwrap();
        let msgs = |v: Vec<Value>| {
            v.iter()
                .map(|l| l["msg"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
                .join(",")
        };
        assert_eq!(
            msgs(read_lines(&state, &Part::ALL, Filter::default(), 50)),
            "old,a,b,c"
        );
        assert_eq!(
            msgs(read_lines(&state, &Part::ALL, Filter::default(), 2)),
            "b,c"
        );
        assert_eq!(
            msgs(read_lines(
                &state,
                &Part::ALL,
                Filter {
                    errors: true,
                    op: None
                },
                50
            )),
            "old,b,c"
        );
        assert_eq!(
            msgs(read_lines(&state, &[Part::Ui], Filter::default(), 50)),
            "b"
        );
        assert_eq!(
            msgs(read_lines(
                &state,
                &Part::ALL,
                Filter {
                    errors: false,
                    op: Some("run-7c2e")
                },
                50
            )),
            "a,b"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn crash_uploads_leave_out_image_and_container_names() {
        let line = |msg: &str, fields: &[(&str, String)]| -> Value {
            serde_json::from_str(&super::super::json_line(
                "2026-10-28T09:41:07.123Z",
                Level::Info,
                Part::Daemon,
                msg,
                fields,
            ))
            .unwrap()
        };
        let lines = [
            line(
                "image pull failed",
                &[
                    ("image", "registry.acme.corp/secret-app:1.2".into()),
                    (
                        "error",
                        "registry.acme.corp/secret-app:1.2: unauthorized".into(),
                    ),
                ],
            ),
            line(
                "container exited",
                &[("name", "web".into()), ("image", "nginx".into())],
            ),
        ];
        let text = format!(
            "panic at webpki: web.kiri.local is gone\n{}\n{}",
            render(&lines[0], true, false),
            render(&lines[1], true, false)
        );
        let out = without_names(&text, &lines);
        assert!(
            !out.contains("secret-app") && !out.contains("acme") && !out.contains("nginx"),
            "{out}"
        );
        assert!(
            out.contains("webpki")
                && out.contains("[name].kiri.local")
                && out.contains("name=[name]"),
            "{out}"
        );
    }

    #[test]
    fn renders_lines_for_people() {
        let v: Value = serde_json::from_str(&super::super::json_line(
            "2026-10-28T09:41:07.123Z",
            Level::Warn,
            Part::Daemon,
            "container exited",
            &[("id", "3f2a".into()), ("error", "no such file".into())],
        ))
        .unwrap();
        assert_eq!(
            render(&v, true, false),
            r#"2026-10-28T09:41:07.123Z WARN  daemon container exited error="no such file" id=3f2a"#
        );
        assert!(
            render(&v, false, false)
                .ends_with(".123 WARN  daemon container exited error=\"no such file\" id=3f2a")
        );
    }

    #[test]
    fn names_crash_files() {
        assert_eq!(
            crash_name("1792800000-daemon.txt"),
            Some((1_792_800_000, Part::Daemon))
        );
        assert_eq!(
            crash_name("1792800000-ui-1.txt"),
            Some((1_792_800_000, Part::Ui))
        );
        assert_eq!(
            crash_name("1792800000-cli.sent.txt"),
            Some((1_792_800_000, Part::Cli))
        );
        assert_eq!(
            crash_name("1792800000-cli.txt.sending"),
            Some((1_792_800_000, Part::Cli))
        );
        assert_eq!(crash_name(".window-opened"), None);
        assert_eq!(crash_name("notes.txt"), None);
    }

    #[test]
    fn assembles_a_redacted_report() {
        let home = temp_home("assemble");
        let state = home.join(".kiri");
        let h = home.display().to_string();
        std::fs::create_dir_all(state.join("logs")).unwrap();
        std::fs::create_dir_all(state.join("crashes")).unwrap();
        let reports = home.join("Library/Logs/DiagnosticReports");
        std::fs::create_dir_all(&reports).unwrap();
        let mut log = String::new();
        for i in 0..400 {
            log += &line(
                &format!("2026-10-28T09:{:02}:{:02}.000Z", i / 60, i % 60),
                Level::Info,
                Part::Daemon,
                &format!("event {i} in {h}/.kiri"),
            );
            log.push('\n');
        }
        std::fs::write(state.join("logs/daemon.log"), log).unwrap();
        std::fs::write(
            state.join("daemon.log"),
            format!(
                "2026-10-28T09:00:00.000Z kiri daemon: ready on unix://{h}/.kiri/run/docker.sock\n"
            ),
        )
        .unwrap();
        std::fs::write(
            state.join("crashes/1792800000-daemon.txt"),
            "Kiri crash report\nMessage:  boom with Bearer abcdefghijklmnop\n",
        )
        .unwrap();
        std::fs::write(
            state.join("crashes/1792700000-ui.sent.txt"),
            "Kiri crash report\nMessage:  older\n",
        )
        .unwrap();
        std::fs::write(reports.join("kiri-2026-10-28-094107.ips"), format!("{{\"app_name\":\"kiri\",\"crashReporterKey\":\"ABCD-1234\"}}\n{{\"procPath\":\"{h}/Applications/Kiri.app\"}}")).unwrap();
        std::fs::write(reports.join("Safari-2026-10-28-094107.ips"), "not ours").unwrap();
        let facts = Facts {
            engine: "not running".into(),
            crash_reports: false,
            created: UNIX_EPOCH + Duration::from_secs(1_792_800_000),
            containers: Some("web  nginx:alpine  Up 2 hours\n".into()),
        };
        let text = assemble(
            &home,
            &state,
            &facts,
            &Redactor::new(Some(h.clone()), Vec::new()),
        );

        assert!(
            text.starts_with("Kiri diagnostics report\nCreated:  2026-10-24T00:00:00"),
            "{text}"
        );
        assert!(
            text.contains("Engine:   not running")
                && text.contains("Crash reports: not sent automatically")
        );
        assert!(text.contains("==== Log: engine (~/.kiri/logs/daemon.log) ===="));
        // The last 300 of 400 lines.
        assert!(
            text.contains("event 399 in ~/.kiri")
                && text.contains("event 100 in")
                && !text.contains("event 99 in")
        );
        assert!(text.contains("==== Log: window") && text.contains("(empty)"));
        assert!(text.contains("kiri daemon: ready on unix://~/.kiri/run/docker.sock"));
        assert!(text.contains(
            "==== Containers (names, images and status only) ====\nweb  nginx:alpine  Up 2 hours\n"
        ));
        assert!(
            text.contains("2 in all")
                && text.contains("--- 1792800000-daemon.txt ---")
                && text.contains("boom with Bearer [redacted]")
        );
        assert!(
            text.find("1792800000-daemon").unwrap() < text.find("1792700000-ui").unwrap(),
            "newest crash first"
        );
        assert!(
            text.contains("--- kiri-2026-10-28-094107.ips ---")
                && text.contains("~/Applications/Kiri.app")
                && !text.contains("Safari")
        );
        assert!(!text.contains("ABCD-1234") && !text.contains(&h), "{text}");
        assert!(text.len() < MAX_BODY);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn bodies_follow_the_contract_and_fit() {
        let created = UNIX_EPOCH + Duration::from_secs(1_792_800_000);
        let v = body(
            "crash",
            Part::Daemon,
            created,
            "text",
            "0123456789abcdef0123456789abcdef",
        );
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        for k in [
            "kind",
            "part",
            "app_version",
            "flavor",
            "os",
            "arch",
            "install_id",
            "created_at",
            "text",
        ] {
            assert!(keys.contains(&k), "{k} missing");
        }
        assert_eq!(
            (v["kind"].as_str(), v["part"].as_str(), v["flavor"].as_str()),
            (Some("crash"), Some("daemon"), Some("kiri"))
        );
        assert_eq!(
            v["created_at"].as_str().unwrap()[..19],
            *"2026-10-24T00:00:00"
        );
        // Quotes and newlines double in JSON: still under 1 MB.
        let big = "\"\n".repeat(700_000);
        let v = body("report", Part::Cli, created, &big, "x");
        assert!(v.to_string().len() <= MAX_BODY);
        assert!(
            v["text"]
                .as_str()
                .unwrap()
                .ends_with("[… cut to fit 1 MB]\n")
        );
    }

    #[test]
    fn install_ids_are_made_once() {
        let home = temp_home("install");
        let a = install_id(&home);
        assert_eq!(a.len(), 32);
        assert_eq!(install_id(&home), a);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lists_crashes_with_their_ids() {
        let home = temp_home("list");
        std::fs::write(
            home.join("1792800000-daemon.txt"),
            "Kiri crash report\nLocation: vm/balloon.rs:212:41\nMessage:  boom\n",
        )
        .unwrap();
        std::fs::write(
            home.join("1792700000-ui.sent.txt"),
            "Kiri crash report\nMessage:  older\n\nSent to Kiri Cloud as rep_51c0e8d4\n",
        )
        .unwrap();
        std::fs::write(home.join(".window-opened"), "1").unwrap();
        let list = crash_list(&home, 10);
        assert_eq!(list.len(), 2);
        assert_eq!(
            (
                list[0]["part"].as_str(),
                list[0]["message"].as_str(),
                list[0]["location"].as_str()
            ),
            (Some("daemon"), Some("boom"), Some("vm/balloon.rs:212:41"))
        );
        assert_eq!(
            (list[0]["sent"].as_bool(), list[0]["id"].is_null()),
            (Some(false), true)
        );
        assert_eq!(
            (list[1]["sent"].as_bool(), list[1]["id"].as_str()),
            (Some(true), Some("rep_51c0e8d4"))
        );
        assert_eq!(crash_list(&home, 1).len(), 1);
        assert!(claim(&home, "notes.txt").is_none());
        // An upload carries its part's lines up to the crash, not after.
        let state = home.join(".kiri");
        std::fs::create_dir_all(state.join("logs")).unwrap();
        let log: Vec<String> = (0..250)
            .map(|i| {
                line(
                    &format!("2026-10-24T00:{:02}:{:02}.000Z", i / 60, i % 60),
                    Level::Info,
                    Part::Daemon,
                    &format!("e{i}"),
                )
            })
            .collect();
        std::fs::write(state.join("logs/daemon.log"), log.join("\n") + "\n").unwrap();
        let up = crash_upload(
            &state,
            1_792_800_000 + 240,
            Part::Daemon,
            "Kiri crash report\n",
        );
        assert!(
            up.starts_with("Kiri crash report\n\n==== The last 200 log lines before it ====\n"),
            "{up}"
        );
        assert!(
            up.contains(" e240")
                && up.contains(" e41\n")
                && !up.contains(" e40\n")
                && !up.contains(" e241"),
            "{up}"
        );
        assert!(
            claim(&home, "1792800000-daemon.txt")
                .is_some_and(|p| p.ends_with("1792800000-daemon.txt.sending"))
        );
        assert!(
            claim(&home, "1792800000-daemon.txt").is_none(),
            "claimed once"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn crash_files_wait_oldest_first() {
        let home = temp_home("pending");
        for n in [
            "1792800000-daemon.txt",
            "1792700000-ui.txt",
            "1792600000-cli.sent.txt",
            "1792500000-ui.txt.sending",
            ".window-opened",
        ] {
            std::fs::write(home.join(n), "x").unwrap();
        }
        assert_eq!(
            pending(&home, 0),
            ["1792700000-ui.txt", "1792800000-daemon.txt"]
        );
        // Crashes from before they were turned on stay here.
        assert_eq!(pending(&home, 1_792_700_001), ["1792800000-daemon.txt"]);
        assert_eq!(pending(&home, 1_792_800_000), ["1792800000-daemon.txt"]);
        assert!(pending(&home, 1_792_800_001).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }
}
