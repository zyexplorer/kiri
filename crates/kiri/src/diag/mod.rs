//! Diagnostics: Kiri's own log, crash reports and `kiri diag`.
//!
//! Each process, the window (`ui`), the engine (`daemon`) and the command
//! line (`cli`), writes one JSON line per event to `~/.kiri/logs/<part>.log`:
//!
//! ```text
//! {"time":"2026-10-28T09:41:07.123Z","level":"warn","part":"daemon","msg":"container exited","fields":{"id":"3f2a9c1e0b7d","exit_code":"137"}}
//! ```
//!
//! `fields` is there only when there are some; their values are strings.
//! A log over 5 MB moves to `.1` (and `.1` to `.2`). `KIRI_LOG=debug` (or
//! `error`, `warn`, `info`, the default) sets how much is written.
//!
//! Only what helps explain a problem is logged: the VM, container and image
//! lifecycle, failures and their errors. Never environment values, tokens,
//! request or response bodies, or file contents; messages also pass through
//! [`redact::patterns`] on the way in.
//!
//! The window and the engine also send their stderr through a pipe (see
//! [`capture_stderr`]): each line gets a timestamp in the log, and a
//! timestamped copy goes on to the original stderr (the engine's
//! `~/.kiri/daemon.log`), with the log's own events beside them.
//!
//! A panic in any process leaves `~/.kiri/crashes/<unix time>-<part>.txt`.
//! `kiri diag report` gathers logs and crashes, redacted, into one file
//! (see [`report`]); crash files go to Kiri Cloud by themselves only when
//! the person turned that on (`"crashReports": true` in `ui.json`).

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub mod redact;
pub mod report;

pub use report::run;

/// Which process is writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Ui,
    Daemon,
    Cli,
}

impl Part {
    pub const ALL: [Part; 3] = [Part::Daemon, Part::Ui, Part::Cli];

    pub fn name(self) -> &'static str {
        match self {
            Part::Ui => "ui",
            Part::Daemon => "daemon",
            Part::Cli => "cli",
        }
    }

    pub fn parse(s: &str) -> Option<Part> {
        match s {
            "ui" | "window" => Some(Part::Ui),
            "daemon" | "engine" => Some(Part::Daemon),
            "cli" => Some(Part::Cli),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" | "trace" => Some(Level::Debug),
            _ => None,
        }
    }
}

static PART: OnceLock<Part> = OnceLock::new();
/// The original stderr once [`capture_stderr`] has taken fd 2; -1 before.
static ECHO: AtomicI32 = AtomicI32::new(-1);

/// This process's part; `cli` until [`init`] says otherwise.
pub fn part() -> Part {
    PART.get().copied().unwrap_or(Part::Cli)
}

fn max_level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        std::env::var("KIRI_LOG")
            .ok()
            .and_then(|v| Level::parse(&v))
            .unwrap_or(Level::Info)
    })
}

pub fn enabled(level: Level) -> bool {
    level <= max_level()
}

/// `diag::info!("VM booted", ms = 112, cpus = 4)`: one event at a level,
/// with fields (any `Display`). The message is any `&str` or `String`.
macro_rules! event {
    ($level:expr, $msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        if $crate::diag::enabled($level) {
            $crate::diag::write($level, ::std::convert::AsRef::<str>::as_ref(&$msg), &[$((stringify!($k), ::std::string::ToString::to_string(&$v))),*]);
        }
    };
}
macro_rules! error {
    ($($t:tt)*) => { $crate::diag::event!($crate::diag::Level::Error, $($t)*) };
}
// `warn` itself would clash with the attribute: named apart, exported as `warn`.
macro_rules! warn_ {
    ($($t:tt)*) => { $crate::diag::event!($crate::diag::Level::Warn, $($t)*) };
}
macro_rules! info {
    ($($t:tt)*) => { $crate::diag::event!($crate::diag::Level::Info, $($t)*) };
}
macro_rules! debug {
    ($($t:tt)*) => { $crate::diag::event!($crate::diag::Level::Debug, $($t)*) };
}
#[allow(unused_imports)]
pub(crate) use {debug, error, event, info, warn_ as warn};

// ---------------------------------------------------------------- ops

thread_local! {
    /// The action this thread works for, if any: every line it writes
    /// carries it as `op`, and `client` passes it to the engine.
    static OP: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// A new op id, `run-7c2e`: what the window started, and 4 random hex
/// digits, so one action's lines (window and engine) can be picked out.
pub fn new_op(kind: &str) -> String {
    use std::hash::BuildHasher;
    let kind: String = kind
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect();
    let n = std::collections::hash_map::RandomState::new().hash_one(SystemTime::now()) as u16;
    format!("{}-{n:04x}", if kind.is_empty() { "op" } else { &kind })
}

/// An op id as it may arrive from elsewhere (a header, `KIRI_OP`).
pub fn valid_op(op: &str) -> bool {
    !op.is_empty() && op.len() <= 40 && op.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// This thread's op.
pub fn op() -> Option<String> {
    OP.with(|o| o.borrow().clone())
}

/// Puts the previous op back when dropped.
#[derive(Debug)]
pub struct OpScope(Option<String>);

impl Drop for OpScope {
    fn drop(&mut self) {
        let prev = self.0.take();
        OP.with(|o| *o.borrow_mut() = prev);
    }
}

/// Works for `op` on this thread until the scope ends.
pub fn enter_op(op: String) -> OpScope {
    OP.with(|o| OpScope(o.borrow_mut().replace(op)))
}

/// The header that carries this thread's op to the engine, or nothing.
pub fn op_header() -> String {
    op().map(|o| format!("X-Kiri-Op: {o}\r\n"))
        .unwrap_or_default()
}

/// `~/.kiri` (`~/.kiri-dev` for Kiri Dev).
pub fn state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(crate::flavor::STATE.get()))
}

pub fn logs_dir() -> Option<PathBuf> {
    state_dir().map(|d| d.join("logs"))
}

pub fn crashes_dir() -> Option<PathBuf> {
    state_dir().map(|d| d.join("crashes"))
}

/// Sets up a process: which part it is and the panic hook; the window and
/// the engine also log their stderr.
pub fn init(part: Part) {
    let _ = PART.set(part);
    // A command the window runs for an action (`kiri k8s start`) works for its op.
    if let Ok(op) = std::env::var("KIRI_OP")
        && valid_op(&op)
    {
        std::mem::forget(enter_op(op));
    }
    install_panic_hook();
    if part != Part::Cli {
        capture_stderr();
    }
}

/// Now, to the millisecond: `2026-10-28T09:41:07.123Z`.
pub fn now() -> String {
    millis(SystemTime::now())
}

fn millis(t: SystemTime) -> String {
    let s = crate::daemon::time::rfc3339(t);
    format!("{}Z", &s[..23])
}

/// Writes one event: the JSON line to this part's log, and a readable line
/// to the original stderr when [`capture_stderr`] took it. Never fails and
/// never writes to fd 2 (which may be the log's own pipe).
pub fn write(level: Level, msg: &str, fields: &[(&str, String)]) {
    let time = now();
    let part = part();
    let with_op;
    let fields = match op() {
        Some(op) if !fields.iter().any(|(k, _)| *k == "op") => {
            with_op = fields
                .iter()
                .cloned()
                .chain([("op", op)])
                .collect::<Vec<_>>();
            &with_op[..]
        }
        _ => fields,
    };
    let _ = append(part, &json_line(&time, level, part, msg, fields));
    let mut text = format!(
        "{time} {:<5} {}",
        level.name().to_ascii_uppercase(),
        redact::patterns(msg)
    );
    for (k, v) in fields {
        let _ = write!(text, " {k}={}", redact::patterns(v));
    }
    echo(&text);
}

/// One log line, `time`, `level`, `part`, `msg` then `fields`, in that order.
pub fn json_line(
    time: &str,
    level: Level,
    part: Part,
    msg: &str,
    fields: &[(&str, String)],
) -> String {
    let q = |s: &str| serde_json::Value::from(redact::patterns(s)).to_string();
    let mut line = format!(
        r#"{{"time":"{time}","level":"{}","part":"{}","msg":{}"#,
        level.name(),
        part.name(),
        q(msg)
    );
    if !fields.is_empty() {
        line.push_str(r#","fields":{"#);
        for (i, (k, v)) in fields.iter().enumerate() {
            let _ = write!(
                line,
                "{}{}:{}",
                if i == 0 { "" } else { "," },
                serde_json::Value::from(*k),
                q(v)
            );
        }
        line.push('}');
    }
    line.push('}');
    line
}

/// A log over this moves aside.
pub const MAX_LOG: u64 = 5 << 20;
/// How many moved-aside logs are kept: `x.log.1`, `x.log.2`.
pub const KEEP: usize = 2;

pub fn log_path(part: Part) -> Option<PathBuf> {
    logs_dir().map(|d| d.join(format!("{}.log", part.name())))
}

/// `daemon.log` → `daemon.log.1`.
pub fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

fn append(part: Part, line: &str) -> io::Result<()> {
    let path = log_path(part).ok_or_else(|| io::Error::other("HOME is not set"))?;
    append_to(&path, line, MAX_LOG)
}

/// Appends a line, first moving the file aside if the line would take it
/// past `max`. One write per line, in append mode: lines from several
/// processes (two `kiri` commands at once) don't interleave.
pub fn append_to(path: &Path, line: &str, max: u64) -> io::Result<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if std::fs::metadata(path).is_ok_and(|m| m.len() + line.len() as u64 + 1 > max) {
        for n in (1..KEEP).rev() {
            let _ = std::fs::rename(rotated(path, n), rotated(path, n + 1));
        }
        let _ = std::fs::rename(path, rotated(path, 1));
    }
    let open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
    };
    let mut f = match open() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)?;
            }
            open()?
        }
        r => r?,
    };
    f.write_all(format!("{line}\n").as_bytes())
}

/// A line to the original stderr, if [`capture_stderr`] took fd 2.
fn echo(text: &str) {
    let fd = ECHO.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    // SAFETY: `fd` is the dup of stderr this module owns and never closes;
    // ManuallyDrop keeps the File from closing it.
    let mut f = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    let _ = f.write_all(format!("{text}\n").as_bytes());
}

/// Sends fd 2 through a pipe: every line written to stderr (an
/// `eprintln!`, a library's complaint, a child's output) lands in this
/// part's log with a timestamp, and goes on to the original stderr with one.
pub fn capture_stderr() {
    if ECHO.load(Ordering::Relaxed) >= 0 {
        return;
    }
    let mut fds = [0i32; 2];
    // SAFETY: plain descriptor calls on descriptors this function owns; fd 2
    // is replaced only once the pipe and the copy of stderr both exist.
    let (read, orig) = unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
        let orig = libc::dup(2);
        if orig < 0 || libc::dup2(fds[1], 2) < 0 {
            libc::close(fds[0]);
            libc::close(fds[1]);
            if orig >= 0 {
                libc::close(orig);
            }
            return;
        }
        libc::close(fds[1]);
        // Children inherit fd 2 (the pipe), not these.
        libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(orig, libc::F_SETFD, libc::FD_CLOEXEC);
        (fds[0], orig)
    };
    ECHO.store(orig, Ordering::Relaxed);
    // SAFETY: the pipe's read end, owned from here by the thread.
    let reader = unsafe { File::from_raw_fd(read) };
    let spawned = std::thread::Builder::new()
        .name("kiri-stderr".into())
        .spawn(move || {
            // This thread starts before the daemon blocks SIGINT and SIGTERM for
            // its signal thread. Left open here, systemd's SIGTERM lands on this
            // thread and its default action ends the process at once, without the
            // daemon's shutdown: containers aren't stopped and kiri-init outlives
            // the unit, so the next start fails (219/CGROUP).
            // SAFETY: plain signal-mask calls on a local set, for this thread only.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGINT);
                libc::sigaddset(&mut set, libc::SIGTERM);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            }
            let mut r = BufReader::new(reader);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end();
                if line.is_empty() {
                    continue;
                }
                let time = now();
                echo(&format!("{time} {line}"));
                if enabled(Level::Info) {
                    let _ = append(
                        part(),
                        &json_line(
                            &time,
                            Level::Info,
                            part(),
                            line,
                            &[("src", "stderr".into())],
                        ),
                    );
                }
            }
        });
    if spawned.is_err() {
        // Nobody would drain the pipe: put stderr back.
        ECHO.store(-1, Ordering::Relaxed);
        // SAFETY: `orig` is still the copy of stderr.
        unsafe { libc::dup2(orig, 2) };
    }
}

// ---------------------------------------------------------------- crashes

/// The system as a crash report and an upload name it.
#[derive(Debug, Clone)]
pub struct System {
    /// `macOS 26.1`
    pub os: String,
    /// The OS build, `25B78`.
    pub build: String,
    /// `aarch64`
    pub arch: &'static str,
    /// The Mac's model, `Mac16,1`.
    pub model: String,
}

impl System {
    pub fn line(&self) -> String {
        let mut s = self.os.clone();
        if !self.build.is_empty() {
            let _ = write!(s, " ({})", self.build);
        }
        let _ = write!(s, ", {}", self.arch);
        if !self.model.is_empty() {
            let _ = write!(s, ", {}", self.model);
        }
        s
    }
}

#[cfg(target_os = "macos")]
fn sysctl(name: &str) -> Option<String> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut len = 0usize;
    // SAFETY: sysctlbyname with a valid name; the first call asks the size,
    // the second fills a buffer of that size.
    unsafe {
        if libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
            || len == 0
        {
            return None;
        }
        let mut buf = vec![0u8; len];
        if libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        buf.truncate(len);
        Some(
            String::from_utf8_lossy(&buf)
                .trim_end_matches('\0')
                .trim()
                .to_owned(),
        )
    }
}

pub fn system() -> System {
    #[cfg(target_os = "macos")]
    let (os, build, model) = (
        format!(
            "macOS {}",
            sysctl("kern.osproductversion").unwrap_or_default()
        )
        .trim()
        .to_owned(),
        sysctl("kern.osversion").unwrap_or_default(),
        sysctl("hw.model").unwrap_or_default(),
    );
    #[cfg(not(target_os = "macos"))]
    let (os, build, model) = {
        let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let pretty = release
            .lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
            .map(|v| v.trim_matches('"').to_owned());
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
            .to_owned();
        (
            pretty.unwrap_or_else(|| "Linux".into()),
            kernel,
            String::new(),
        )
    };
    System {
        os,
        build,
        arch: std::env::consts::ARCH,
        model,
    }
}

/// `kiri` or `kiri-dev`, as an upload names the flavor (a feature app
/// counts as Dev); crash files say the exact one.
pub fn flavor() -> &'static str {
    if crate::flavor::dev() {
        "kiri-dev"
    } else {
        "kiri"
    }
}

/// Writes a crash file (and an error line) on every panic, then lets the
/// previous hook print as before. With `panic = "abort"` the process ends
/// right after: everything here is synchronous.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let at = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        // A command whose output was closed under it (`kiri share | head`,
        // a closed terminal, the reader gone): not a crash. End quietly, as
        // a command killed by SIGPIPE would, with no crash file.
        if part() == Part::Cli && closed_output(&message) {
            write(Level::Info, "output closed", &[("message", message)]);
            std::process::exit(if cfg!(unix) { 141 } else { 1 });
        }
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        write(
            Level::Error,
            "panic",
            &[
                ("thread", thread.clone()),
                ("at", at.clone()),
                ("message", message.clone()),
            ],
        );
        let backtrace = std::backtrace::Backtrace::force_capture();
        let text = crash_text(
            SystemTime::now(),
            &thread,
            &at,
            &message,
            &backtrace.to_string(),
        );
        if let Some(dir) = crashes_dir() {
            let _ = save_crash(&dir, SystemTime::now(), part(), &text);
        }
        prev(info);
    }));
}

/// Whether a panic is Rust's `print!`/`eprint!` failing because the other
/// end of stdout or stderr went away: a broken pipe (EPIPE, os error 32 on
/// Unix; 109 on Windows) or Windows' "the pipe is being closed" (232).
fn closed_output(message: &str) -> bool {
    (message.starts_with("failed printing to stdout")
        || message.starts_with("failed printing to stderr"))
        && [
            "Broken pipe",
            "(os error 32)",
            "(os error 109)",
            "(os error 232)",
        ]
        .iter()
        .any(|m| message.contains(m))
}

/// What a crash file holds.
pub fn crash_text(t: SystemTime, thread: &str, at: &str, message: &str, backtrace: &str) -> String {
    // The subcommand only: arguments can carry values (`-e KEY=VALUE`).
    let command = std::env::args()
        .nth(1)
        .filter(|a| a.len() <= 20 && a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    let text = format!(
        "Kiri crash report\n\
         Version:  {app} {version}\n\
         Flavor:   {flavor}\n\
         Part:     {part}\n\
         Time:     {time}\n\
         Thread:   {thread}\n\
         Location: {at}\n\
         Message:  {message}\n\
         System:   {system}\n\
         Command:  kiri {command}\n\
         \n\
         Backtrace:\n{backtrace}\n",
        app = crate::flavor::APP,
        version = env!("CARGO_PKG_VERSION"),
        flavor = crate::flavor::CONTEXT,
        part = part().name(),
        time = crate::daemon::time::rfc3339(t),
        system = system().line(),
        command = command.unwrap_or_default(),
    );
    redact::Redactor::home_only().apply(&text)
}

/// `<dir>/<unix time>-<part>.txt`, readable only by this user.
pub fn save_crash(dir: &Path, t: SystemTime, part: Part, text: &str) -> io::Result<PathBuf> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut n = 0;
    loop {
        let name = if n == 0 {
            format!("{secs}-{}.txt", part.name())
        } else {
            format!("{secs}-{}-{n}.txt", part.name())
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 20 => n += 1,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_output_is_not_a_crash() {
        assert!(closed_output(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
        assert!(closed_output(
            "failed printing to stdout: The pipe is being closed. (os error 232)"
        ));
        assert!(closed_output(
            "failed printing to stdout: The pipe has been ended. (os error 109)"
        ));
        assert!(!closed_output(
            "failed printing to stdout: No space left on device (os error 28)"
        ));
        assert!(!closed_output(
            "called `Result::unwrap()` on an `Err` value: Broken pipe (os error 32)"
        ));
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kiri-diag-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_line_is_one_json_object_in_order() {
        let line = json_line(
            "2026-10-28T09:41:07.123Z",
            Level::Warn,
            Part::Daemon,
            "container \"web\" exited\n",
            &[("id", "3f2a".into()), ("exit_code", "137".into())],
        );
        assert_eq!(
            line,
            r#"{"time":"2026-10-28T09:41:07.123Z","level":"warn","part":"daemon","msg":"container \"web\" exited\n","fields":{"id":"3f2a","exit_code":"137"}}"#
        );
        assert!(!line.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["fields"]["exit_code"], "137");
        let bare = json_line("t", Level::Info, Part::Ui, "hi", &[]);
        assert_eq!(
            bare,
            r#"{"time":"t","level":"info","part":"ui","msg":"hi"}"#
        );
    }

    #[test]
    fn lines_are_redacted_on_the_way_in() {
        let line = json_line(
            "t",
            Level::Error,
            Part::Cli,
            "login failed: Authorization: Bearer abcdefgh12345678",
            &[("url", "https://x/?token=s3cr3tvalue&a=1".into())],
        );
        assert!(
            !line.contains("abcdefgh12345678") && !line.contains("s3cr3tvalue"),
            "{line}"
        );
        assert!(line.contains("a=1"));
    }

    #[test]
    fn ops_are_per_thread_and_scoped() {
        let op = new_op("run");
        assert!(
            op.starts_with("run-") && op.len() == 8 && valid_op(&op),
            "{op}"
        );
        assert!(!valid_op("a b") && !valid_op("") && !valid_op("x\r\nEvil: 1"));
        assert_eq!(super::op(), None);
        {
            let _outer = enter_op("pull-0001".into());
            {
                let _inner = enter_op("run-0002".into());
                assert_eq!(op_header(), "X-Kiri-Op: run-0002\r\n");
                std::thread::spawn(|| assert_eq!(super::op(), None))
                    .join()
                    .unwrap();
            }
            assert_eq!(super::op().as_deref(), Some("pull-0001"));
        }
        assert_eq!(op_header(), "");
    }

    #[test]
    fn times_have_milliseconds() {
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_792_800_000_123);
        assert_eq!(millis(t), "2026-10-24T00:00:00.123Z");
    }

    #[test]
    fn logs_rotate_by_size_and_keep_two() {
        let dir = temp("rotate");
        let path = dir.join("logs/daemon.log");
        let line = "x".repeat(99);
        // 100 bytes a line, 250 a file: two lines each.
        for i in 0..9 {
            append_to(&path, &format!("{i}{}", &line[1..]), 250).unwrap();
        }
        let read = |p: &Path| {
            std::fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|l| l[..1].to_owned())
                .collect::<Vec<_>>()
                .join("")
        };
        assert_eq!(read(&path), "8");
        assert_eq!(read(&rotated(&path, 1)), "67");
        assert_eq!(read(&rotated(&path, 2)), "45");
        assert!(!rotated(&path, 3).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn levels_order_and_parse() {
        assert!(Level::Error < Level::Warn && Level::Info < Level::Debug);
        assert_eq!(Level::parse(" DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse("loud"), None);
        assert_eq!(Part::parse("engine"), Some(Part::Daemon));
    }

    #[test]
    fn crash_files_are_named_by_time_and_part() {
        let dir = temp("crash");
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_792_800_000);
        let a = save_crash(&dir, t, Part::Daemon, "one").unwrap();
        let b = save_crash(&dir, t, Part::Daemon, "two").unwrap();
        assert_eq!(a.file_name().unwrap(), "1792800000-daemon.txt");
        assert_eq!(b.file_name().unwrap(), "1792800000-daemon-1.txt");
        let text = crash_text(t, "kiri-api", "src/x.rs:1:2", "boom", "0: main");
        assert!(
            text.contains("Message:  boom")
                && text.contains("Thread:   kiri-api")
                && text.contains("Backtrace:\n0: main"),
            "{text}"
        );
        assert!(text.contains(&format!("Version:  Kiri {}", env!("CARGO_PKG_VERSION"))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
