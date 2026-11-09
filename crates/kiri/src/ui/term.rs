//! The built-in terminal's shells on the Mac and Linux: each tab or pane is
//! a shell on its own pseudo-terminal, in the home folder, with Kiri's
//! tools first on the PATH. What goes to and from the page is in
//! termwire.rs; Windows does the same with a ConPTY (kiri-win's term.rs).

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::termwire::{self, Shell};

struct Session {
    /// The pseudo-terminal's master side: resized here, read by the
    /// session's reader and written by its writer.
    master: Arc<File>,
    /// The shell, which leads its own session and process group.
    pid: i32,
    input: Sender<Vec<u8>>,
    /// Set once the shell has exited and been reaped.
    done: Arc<AtomicBool>,
}

static SESSIONS: Mutex<Option<HashMap<String, Session>>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut HashMap<String, Session>) -> R) -> R {
    f(SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new))
}

/// One `term` command from the page.
pub fn handle(action: &str, id: &str, value: &Value, emit: fn(String)) {
    match action {
        "shells" => {
            let (list, default) = shells();
            emit(termwire::shells(&list, &default));
        }
        "open" if termwire::valid_id(id) => {
            let (cols, rows) = termwire::size(value);
            let asked = value["shell"].as_str().unwrap_or("");
            let (list, default) = shells();
            // Only a shell from the list: the page never names a program.
            let path = list
                .iter()
                .find(|s| s.id == asked)
                .map_or(default, |s| s.id.clone());
            if let Err(e) = open(id, Path::new(&path), cols, rows, emit) {
                emit(termwire::exit(id, None, Some(&e.to_string())));
            }
        }
        "input" => {
            if let Some(bytes) = value.as_str().and_then(termwire::unb64) {
                with(|s| s.get(id).map(|s| s.input.send(bytes)));
            }
        }
        "resize" => {
            let (cols, rows) = termwire::size(value);
            with(|s| s.get(id).map(|s| set_size(&s.master, cols, rows)));
        }
        "close" => {
            if let Some(s) = with(|s| s.remove(id)) {
                end(&s);
            }
        }
        // The page (re)loaded: nothing it had is on screen any more.
        "closeall" => {
            for (_, s) in with(std::mem::take) {
                end(&s);
            }
        }
        _ => {}
    }
}

/// Ends a closed session's shell: a hang-up, as closing a terminal window
/// sends, then SIGKILL to its process group if it is still there 2 seconds
/// later (a shell that traps SIGHUP, or a busy one).
fn end(s: &Session) {
    hang_up(s.pid);
    let (pid, done) = (s.pid, s.done.clone());
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        // Not after it was reaped: the number could belong to another process by then.
        if !done.load(Ordering::SeqCst) {
            // SAFETY: kill only sends a signal.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    });
}

/// SIGHUP to a shell's process group, so what it started in the background goes too.
fn hang_up(pid: i32) {
    // SAFETY: kill only sends a signal.
    unsafe { libc::kill(-pid, libc::SIGHUP) };
}

/// The shells in /etc/shells that a person would open, plus Homebrew's, and
/// which one is the user's own.
fn shells() -> (Vec<Shell>, String) {
    let mut paths: Vec<PathBuf> = std::fs::read_to_string("/etc/shells")
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    for dir in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/home/linuxbrew/.linuxbrew/bin",
    ] {
        for name in ["zsh", "bash", "fish", "nu", "pwsh"] {
            paths.push(Path::new(dir).join(name));
        }
    }
    let own = login_shell();
    paths.insert(0, own.clone());
    let mut seen = Vec::new();
    let mut list: Vec<Shell> = Vec::new();
    for p in paths {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        // Shells nobody opens a tab of, and the ones that only refuse logins.
        if name.is_empty()
            || [
                "nologin",
                "false",
                "true",
                "rbash",
                "git-shell",
                "screen",
                "tmux",
                "sh",
            ]
            .contains(&name.as_str())
            || !executable(&p)
        {
            continue;
        }
        // /bin/bash and /usr/bin/bash are one file where /bin is a link.
        let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen.contains(&real) {
            continue;
        }
        seen.push(real);
        let label = if list.iter().any(|s| s.label == name) {
            format!(
                "{name} ({})",
                p.parent()
                    .map_or(String::new(), |d| d.display().to_string())
            )
        } else {
            name
        };
        list.push(Shell {
            id: p.display().to_string(),
            label,
            group: "",
        });
    }
    let default = if list.iter().any(|s| Path::new(&s.id) == own) {
        own.display().to_string()
    } else {
        list.first()
            .map_or_else(|| "/bin/sh".to_owned(), |s| s.id.clone())
    };
    if list.is_empty() {
        list.push(Shell {
            id: default.clone(),
            label: "sh".into(),
            group: "",
        });
    }
    (list, default)
}

/// The user's shell: $SHELL, else the account's, else zsh or bash.
fn login_shell() -> PathBuf {
    if let Some(s) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
        return s.into();
    }
    // SAFETY: getpwuid returns the calling thread's account record or null;
    // pw_shell is read at once, before another call could replace it.
    let from_passwd = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        (!pw.is_null() && !(*pw).pw_shell.is_null()).then(|| {
            CStr::from_ptr((*pw).pw_shell)
                .to_string_lossy()
                .into_owned()
        })
    };
    from_passwd.map_or_else(
        || {
            PathBuf::from(if cfg!(target_os = "macos") {
                "/bin/zsh"
            } else {
                "/bin/bash"
            })
        },
        PathBuf::from,
    )
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Kiri's own tools, put first on the shell's PATH: `kiri`, `docker` and
/// the rest of what Kiri installs, and the copies inside the app.
fn tool_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(
            PathBuf::from(home)
                .join(crate::flavor::STATE.get())
                .join("bin"),
        );
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        dirs.push(exe_dir.join("../Resources/bin"));
        dirs.push(exe_dir);
    }
    dirs.into_iter()
        .filter(|d| d.is_dir())
        .filter_map(|d| std::fs::canonicalize(d).ok())
        .collect()
}

/// A new pseudo-terminal: its master, and the path of its other side.
fn new_pty() -> std::io::Result<(OwnedFd, PathBuf)> {
    // SAFETY: plain libc calls on a descriptor this function owns from the
    // moment posix_openpt returns it.
    unsafe {
        let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master = OwnedFd::from_raw_fd(fd);
        if libc::grantpt(fd) != 0 || libc::unlockpt(fd) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok((master, slave_name(fd)?))
    }
}

#[cfg(target_os = "linux")]
fn slave_name(fd: i32) -> std::io::Result<PathBuf> {
    let mut buf = [0 as libc::c_char; 128];
    // SAFETY: ptsname_r writes at most buf.len() bytes, NUL included.
    if unsafe { libc::ptsname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: on success buf holds a NUL-terminated path.
    Ok(PathBuf::from(
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    ))
}

#[cfg(not(target_os = "linux"))]
fn slave_name(fd: i32) -> std::io::Result<PathBuf> {
    // ptsname shares one buffer: one caller at a time.
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the result is copied out before the lock is released.
    unsafe {
        let p = libc::ptsname(fd);
        if p.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(PathBuf::from(
            CStr::from_ptr(p).to_string_lossy().into_owned(),
        ))
    }
}

fn set_size(master: &File, cols: u16, rows: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads one winsize.
    unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
}

fn open(id: &str, shell: &Path, cols: u16, rows: u16, emit: fn(String)) -> std::io::Result<()> {
    // A tab the page reopens under the same name replaces the old one.
    if let Some(old) = with(|s| s.remove(id)) {
        end(&old);
    }
    let (master, slave_path) = new_pty()?;
    let master = Arc::new(File::from(master));
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags_noctty()
        .open(&slave_path)?;
    // After the other side is open: macOS forgets a size set before.
    set_size(&master, cols, rows);

    let name = shell.file_name().and_then(|n| n.to_str()).unwrap_or("sh");
    let mut cmd = Command::new(shell);
    // A login shell on the Mac, as Terminal starts one, so the profile sets
    // up the PATH; on Linux terminals start an interactive shell.
    if cfg!(target_os = "macos") {
        cmd.arg0(format!("-{name}"));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(tool_dirs().into_iter().chain(std::env::split_paths(&path)))
        .unwrap_or(path);
    cmd.env("PATH", path)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("TERM_PROGRAM", "Kiri")
        .env_remove("TERM_PROGRAM_VERSION");
    if std::env::var_os("LANG").is_none() {
        cmd.env("LANG", "en_US.UTF-8");
    }
    if let Some(home) = std::env::var_os("HOME") {
        cmd.current_dir(home);
    }
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: setsid and ioctl are async-signal-safe; they run in the child
    // after its stdio is the terminal, making it the controlling terminal
    // of the shell's own session.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // Dropping cmd closes the parent's copies of the terminal's other side,
    // so the reader sees the end once the shell and its children are gone.
    drop(cmd);
    let pid = child.id() as i32;

    let (input, typed) = channel::<Vec<u8>>();
    let done = Arc::new(AtomicBool::new(false));
    with(|s| {
        s.insert(
            id.to_owned(),
            Session {
                master: master.clone(),
                pid,
                input,
                done: done.clone(),
            },
        )
    });

    // Writes go through their own thread: a shell that stops reading can't
    // hold up the window.
    let writer = master.clone();
    std::thread::spawn(move || {
        for bytes in typed {
            if (&*writer).write_all(&bytes).is_err() {
                break;
            }
        }
    });

    // The shell ending: hang up whatever it left on the terminal, so the
    // reader reaches the end.
    let code = Arc::new(Mutex::new(None));
    let waiter = {
        let code = code.clone();
        std::thread::spawn(move || {
            let status = child.wait().ok();
            *code.lock().unwrap_or_else(|e| e.into_inner()) = status.and_then(|s| s.code());
            // Its group may outlive it (a job it started); the number is still ours to signal
            // until the reaped shell's group is empty.
            hang_up(pid);
            done.store(true, Ordering::SeqCst);
        })
    };

    let id = id.to_owned();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match (&*master).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => emit(termwire::data(&id, &buf[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // EIO: the other side is closed.
                Err(_) => break,
            }
        }
        let _ = waiter.join();
        // Only when it is still this session: a close or reopen replaced it.
        let mine = with(|s| {
            let mine = s.get(&id).is_some_and(|s| s.pid == pid);
            if mine {
                s.remove(&id);
            }
            mine
        });
        if mine {
            emit(termwire::exit(
                &id,
                *code.lock().unwrap_or_else(|e| e.into_inner()),
                None,
            ));
        }
    });
    Ok(())
}

/// Opening the terminal's other side without it becoming the window's
/// controlling terminal.
trait NoCtty {
    fn custom_flags_noctty(&mut self) -> &mut Self;
}

impl NoCtty for std::fs::OpenOptions {
    fn custom_flags_noctty(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.custom_flags(libc::O_NOCTTY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    // Every test's listener hears every event; each keeps its own session's.
    static EVENTS: Mutex<Vec<Sender<String>>> = Mutex::new(Vec::new());

    fn record(json: String) {
        for tx in EVENTS.lock().unwrap().iter() {
            let _ = tx.send(json.clone());
        }
    }

    fn listen() -> Receiver<String> {
        let (tx, rx) = channel();
        EVENTS.lock().unwrap().push(tx);
        rx
    }

    fn output_until(rx: &Receiver<String>, id: &str, want: &str) -> (String, Option<Value>) {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            let ev: Value = serde_json::from_str(&ev).unwrap();
            if ev["id"] != id {
                continue;
            }
            match ev["ev"].as_str() {
                Some("data") => {
                    out.extend(termwire::unb64(ev["b64"].as_str().unwrap()).unwrap());
                    if String::from_utf8_lossy(&out).contains(want) {
                        return (String::from_utf8_lossy(&out).into_owned(), None);
                    }
                }
                Some("exit") => return (String::from_utf8_lossy(&out).into_owned(), Some(ev)),
                _ => {}
            }
        }
        panic!("no {want:?} in {:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn a_shell_runs_on_a_real_terminal_and_says_when_it_ends() {
        let rx = listen();
        let (list, default) = shells();
        assert!(
            list.iter().any(|s| s.id == default),
            "default {default} not in the list"
        );

        handle(
            "open",
            "t1",
            &serde_json::json!({"shell": "/bin/bash", "cols": 91, "rows": 17}),
            record,
        );
        let typed = |s: &str| {
            handle(
                "input",
                "t1",
                &Value::String(termwire::b64(s.as_bytes())),
                record,
            )
        };
        // A terminal of the size asked for, Kiri's tools on the PATH, and
        // the TERM programs expect.
        typed("stty size; echo \"T=$TERM\"; test -t 0 && echo IS-A-TTY; echo DONE$((40+2))\n");
        let (out, _) = output_until(&rx, "t1", "DONE42");
        assert!(out.contains("17 91"), "{out}");
        assert!(
            out.contains("T=xterm-256color") && out.contains("IS-A-TTY"),
            "{out}"
        );

        handle(
            "resize",
            "t1",
            &serde_json::json!({"cols": 120, "rows": 30}),
            record,
        );
        typed("stty size; echo AGAIN$((1+1))\n");
        let (out, _) = output_until(&rx, "t1", "AGAIN2");
        assert!(out.contains("30 120"), "{out}");

        typed("exit 7\n");
        let (_, exit) = output_until(&rx, "t1", "\u{0}never");
        assert_eq!(exit.unwrap()["code"], 7);
        assert!(with(|s| !s.contains_key("t1")));
    }

    #[test]
    fn closing_hangs_up_the_shell_and_what_it_started() {
        let rx = listen();
        handle(
            "open",
            "t2",
            &serde_json::json!({"shell": "/bin/bash"}),
            record,
        );
        handle(
            "input",
            "t2",
            &Value::String(termwire::b64(b"sleep 600 & echo STARTED$((2*3))\n")),
            record,
        );
        output_until(&rx, "t2", "STARTED6");
        let pid = with(|s| s["t2"].pid);
        handle("close", "t2", &Value::Null, record);
        let deadline = Instant::now() + Duration::from_secs(5);
        // SAFETY: kill with signal 0 only checks the group exists.
        while unsafe { libc::kill(-pid, 0) } == 0 {
            assert!(Instant::now() < deadline, "process group {pid} still there");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_shell_that_ignores_the_hang_up_still_goes() {
        let rx = listen();
        handle(
            "open",
            "t3",
            &serde_json::json!({"shell": "/bin/bash"}),
            record,
        );
        handle(
            "input",
            "t3",
            &Value::String(termwire::b64(b"trap '' HUP; echo TRAPPED$((3*3))\n")),
            record,
        );
        output_until(&rx, "t3", "TRAPPED9");
        let pid = with(|s| s["t3"].pid);
        handle("close", "t3", &Value::Null, record);
        let deadline = Instant::now() + Duration::from_secs(6);
        // SAFETY: kill with signal 0 only checks the process exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < deadline, "shell {pid} survived its close");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_page_can_only_start_a_listed_shell() {
        let (list, default) = shells();
        assert!(
            !list
                .iter()
                .any(|s| s.id.ends_with("/nologin") || s.id.ends_with("/false"))
        );
        assert!(Path::new(&default).is_absolute());
    }
}
