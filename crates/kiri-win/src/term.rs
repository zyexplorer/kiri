//! The built-in terminal's shells on Windows: each tab or pane is a shell
//! on its own ConPTY (Windows' pseudo console), in the user's folder, with
//! Kiri's tools first on the PATH. PowerShell 7, Windows PowerShell, cmd,
//! and each WSL distro except Kiri's own. What goes to and from the page is
//! in termwire.rs, shared with the Mac and Linux (crates/kiri/src/ui/term.rs).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

#[path = "../../kiri/src/ui/termwire.rs"]
mod termwire;
use termwire::Shell;

pub use termwire::{COPY_MAX, paste as paste_event, web_link};

/// A pseudo console, closed once: by the page closing it, or after its
/// shell exits.
struct Console(Mutex<Option<HPCON>>);

impl Console {
    fn close(&self) {
        if let Some(h) = self.0.lock().unwrap_or_else(PoisonError::into_inner).take() {
            // SAFETY: h came from CreatePseudoConsole and is closed only here.
            unsafe { ClosePseudoConsole(h) };
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        if let Some(h) = *self.0.lock().unwrap_or_else(PoisonError::into_inner) {
            // SAFETY: h is open while it is in the mutex.
            unsafe { ResizePseudoConsole(h, COORD { X: cols as i16, Y: rows as i16 }) };
        }
    }
}

struct Session {
    console: Arc<Console>,
    process: Arc<OwnedHandle>,
    input: Sender<Vec<u8>>,
}

static SESSIONS: Mutex<Option<HashMap<String, Session>>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut HashMap<String, Session>) -> R) -> R {
    f(SESSIONS.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(HashMap::new))
}

/// One `term` command from the page (copy, paste and links are engine.rs's).
pub fn handle(action: &str, id: &str, value: &Value, emit: fn(String)) {
    match action {
        "shells" => {
            std::thread::spawn(move || {
                let list = shells();
                let default = list.first().map_or_else(String::new, |s| s.id.clone());
                emit(termwire::shells(&list, &default));
            });
        }
        "open" if termwire::valid_id(id) => {
            let (cols, rows) = termwire::size(value);
            let asked = value["shell"].as_str().unwrap_or("").to_owned();
            let id = id.to_owned();
            // Listing WSL's distros runs wsl.exe: not on the window's thread.
            std::thread::spawn(move || {
                let list = shells();
                // Only a shell from the list: the page never names a program.
                let Some(shell) = list.iter().find(|s| s.id == asked).or(list.first()) else {
                    return emit(termwire::exit(&id, None, Some("No shell found")));
                };
                if let Err(e) = open(&id, &command_line(&shell.id), cols, rows, emit) {
                    emit(termwire::exit(&id, None, Some(&e.to_string())));
                }
            });
        }
        "input" => {
            if let Some(bytes) = value.as_str().and_then(termwire::unb64) {
                with(|s| s.get(id).map(|s| s.input.send(bytes)));
            }
        }
        "resize" => {
            let (cols, rows) = termwire::size(value);
            with(|s| s.get(id).map(|s| s.console.resize(cols, rows)));
        }
        "close" => {
            if let Some(s) = with(|s| s.remove(id)) {
                end(s);
            }
        }
        "closeall" => {
            for (_, s) in with(std::mem::take) {
                end(s);
            }
        }
        _ => {}
    }
}

/// Closes a session's console, which ends what runs in it, and its shell.
fn end(s: Session) {
    // ClosePseudoConsole can wait for its output to be read: off the window's thread.
    std::thread::spawn(move || {
        s.console.close();
        // SAFETY: the process handle is open while s holds it.
        unsafe { TerminateProcess(s.process.as_raw_handle() as HANDLE, 1) };
    });
}

/// A program on its own pseudo console, for a sign-in that wants a terminal
/// (weblogin.rs): what it prints, what to type to it, a way to stop it, and
/// its exit code.
#[allow(clippy::type_complexity)]
pub fn captured(program: &std::path::Path, args: &[&str]) -> std::io::Result<(std::sync::mpsc::Receiver<Vec<u8>>, Sender<Vec<u8>>, Box<dyn Fn() + Send + Sync>, std::sync::mpsc::Receiver<Option<i32>>)> {
    let quote = |s: &str| if s.is_empty() || s.contains([' ', '\t', '"']) { format!("\"{}\"", s.replace('"', "\\\"")) } else { s.to_owned() };
    let command = std::iter::once(quote(&program.to_string_lossy())).chain(args.iter().map(|a| quote(a))).collect::<Vec<_>>().join(" ");
    let (con_in, typed_w) = pipe()?;
    let (screen_r, con_out) = pipe()?;
    let mut hpc: HPCON = 0;
    // Wide, so a long line (a code and its page) isn't wrapped.
    // SAFETY: the console duplicates both handles; ours are closed after.
    let hr = unsafe { CreatePseudoConsole(COORD { X: 240, Y: 40 }, con_in.as_raw_handle() as HANDLE, con_out.as_raw_handle() as HANDLE, 0, &mut hpc) };
    drop((con_in, con_out));
    if hr != S_OK {
        return Err(std::io::Error::other(format!("CreatePseudoConsole failed ({hr:#x})")));
    }
    let console = Arc::new(Console(Mutex::new(Some(hpc))));
    let process = match spawn(&command, hpc) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            console.close();
            return Err(e);
        }
    };
    let (input, typed) = channel::<Vec<u8>>();
    let mut writer = File::from(typed_w);
    std::thread::spawn(move || {
        for bytes in typed {
            if writer.write_all(&bytes).is_err() {
                break;
            }
        }
    });
    let (exit_tx, exit) = channel();
    {
        let (console, process) = (console.clone(), process.clone());
        std::thread::spawn(move || {
            let h = process.as_raw_handle() as HANDLE;
            let mut c = 0u32;
            // SAFETY: the handle stays open while process is held.
            unsafe {
                WaitForSingleObject(h, INFINITE);
                GetExitCodeProcess(h, &mut c);
            }
            console.close();
            let _ = exit_tx.send(Some(c as i32));
        });
    }
    let (out_tx, output) = channel();
    let mut screen = File::from(screen_r);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match screen.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let stop = Box::new(move || {
        // SAFETY: the process handle is open while this closure holds it.
        unsafe { TerminateProcess(process.as_raw_handle() as HANDLE, 1) };
    });
    Ok((output, input, stop, exit))
}

fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    root.join("System32").join(exe)
}

fn pwsh() -> Option<PathBuf> {
    let installed = std::env::var_os("ProgramFiles").map(|p| PathBuf::from(p).join(r"PowerShell\7\pwsh.exe")).filter(|p| p.is_file());
    installed.or_else(|| std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("pwsh.exe")).find(|p| p.is_file())))
}

/// The shells this PC has, the first one the default.
fn shells() -> Vec<Shell> {
    let mut list = Vec::new();
    if pwsh().is_some() {
        list.push(Shell { id: "pwsh".into(), label: "PowerShell 7".into(), group: "" });
    }
    list.push(Shell { id: "powershell".into(), label: "Windows PowerShell".into(), group: "" });
    list.push(Shell { id: "cmd".into(), label: "Command Prompt".into(), group: "" });
    // Kiri's own distro runs the engine; Docker Desktop's aren't for logging into.
    for d in crate::setup::distros() {
        let lower = d.to_ascii_lowercase();
        if !lower.starts_with("kiri") && !lower.starts_with("docker-desktop") && d.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
            list.push(Shell { id: format!("wsl:{d}"), label: format!("WSL · {d}"), group: "WSL" });
        }
    }
    list
}

/// The command line for a listed shell's id.
fn command_line(id: &str) -> String {
    let quoted = |p: PathBuf| format!("\"{}\"", p.display());
    match id {
        "pwsh" => pwsh().map_or_else(|| quoted(system32(r"WindowsPowerShell\v1.0\powershell.exe")), quoted) + " -NoLogo",
        "powershell" => quoted(system32(r"WindowsPowerShell\v1.0\powershell.exe")) + " -NoLogo",
        // Distro names were checked to be plain in shells().
        _ if id.starts_with("wsl:") => format!("{} -d {} --cd ~", quoted(system32("wsl.exe")), &id[4..]),
        _ => quoted(system32("cmd.exe")),
    }
}

/// This process's environment with Kiri's tools first on the PATH, as a
/// CreateProcessW block: `name=value` UTF-16 strings, sorted, each ending
/// in a NUL, and one more NUL at the end.
fn environment() -> Vec<u16> {
    let ours = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"Kiri\bin")).filter(|d| d.is_dir());
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf()));
    let mut vars: Vec<(String, String)> = std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect();
    let path_at = vars.iter().position(|(k, _)| k.eq_ignore_ascii_case("PATH"));
    let old = path_at.map(|i| vars[i].1.clone()).unwrap_or_default();
    let dirs: Vec<PathBuf> = ours.into_iter().chain(exe_dir.map(|d| d.join("bin")).filter(|d| d.is_dir())).chain(std::env::split_paths(&old)).collect();
    let path = std::env::join_paths(dirs).map(|p| p.to_string_lossy().into_owned()).unwrap_or(old);
    match path_at {
        Some(i) => vars[i].1 = path,
        None => vars.push(("PATH".into(), path)),
    }
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in vars {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

fn pipe() -> std::io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut r, mut w): (HANDLE, HANDLE) = (0, 0);
    // SAFETY: CreatePipe fills both handles on success; each is owned once.
    unsafe {
        if CreatePipe(&mut r, &mut w, std::ptr::null(), 0) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((OwnedHandle::from_raw_handle(r as _), OwnedHandle::from_raw_handle(w as _)))
    }
}

fn open(id: &str, command: &str, cols: u16, rows: u16, emit: fn(String)) -> std::io::Result<()> {
    if let Some(old) = with(|s| s.remove(id)) {
        end(old);
    }
    // The console reads what's typed from one pipe and writes the screen to the other.
    let (con_in, typed_w) = pipe()?;
    let (screen_r, con_out) = pipe()?;
    let mut hpc: HPCON = 0;
    // SAFETY: the console duplicates both handles; ours are closed after.
    let hr = unsafe { CreatePseudoConsole(COORD { X: cols as i16, Y: rows as i16 }, con_in.as_raw_handle() as HANDLE, con_out.as_raw_handle() as HANDLE, 0, &mut hpc) };
    drop((con_in, con_out));
    if hr != S_OK {
        return Err(std::io::Error::other(format!("CreatePseudoConsole failed ({hr:#x})")));
    }
    let console = Arc::new(Console(Mutex::new(Some(hpc))));

    let process = match spawn(command, hpc) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            console.close();
            return Err(e);
        }
    };

    let (input, typed) = channel::<Vec<u8>>();
    with(|s| s.insert(id.to_owned(), Session { console: console.clone(), process: process.clone(), input }));

    let mut writer = File::from(typed_w);
    std::thread::spawn(move || {
        for bytes in typed {
            if writer.write_all(&bytes).is_err() {
                break;
            }
        }
    });

    // The shell ending: close its console, which ends the screen pipe.
    let code = Arc::new(Mutex::new(None));
    let waiter = {
        let (code, console, process) = (code.clone(), console.clone(), process.clone());
        std::thread::spawn(move || {
            let h = process.as_raw_handle() as HANDLE;
            let mut c = 0u32;
            // SAFETY: the handle stays open while process is held.
            unsafe {
                WaitForSingleObject(h, INFINITE);
                GetExitCodeProcess(h, &mut c);
            }
            *code.lock().unwrap_or_else(PoisonError::into_inner) = Some(c as i32);
            console.close();
        })
    };

    let id = id.to_owned();
    let mut screen = File::from(screen_r);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match screen.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => emit(termwire::data(&id, &buf[..n])),
            }
        }
        let _ = waiter.join();
        let mine = with(|s| {
            let mine = s.get(&id).is_some_and(|s| Arc::ptr_eq(&s.process, &process));
            if mine {
                s.remove(&id);
            }
            mine
        });
        if mine {
            emit(termwire::exit(&id, *code.lock().unwrap_or_else(PoisonError::into_inner), None));
        }
    });
    Ok(())
}

/// Starts `command` attached to the pseudo console `hpc`, in the user's folder.
fn spawn(command: &str, hpc: HPCON) -> std::io::Result<OwnedHandle> {
    let mut cmdline: Vec<u16> = command.encode_utf16().chain([0]).collect();
    let cwd: Option<Vec<u16>> = std::env::var_os("USERPROFILE").map(|p| p.to_string_lossy().encode_utf16().chain([0]).collect());
    let env = environment();
    // SAFETY: the attribute list is sized by the first call and lives in
    // `list` until after CreateProcessW; every pointer passed outlives the
    // call that takes it.
    unsafe {
        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        let mut list = vec![0u8; size];
        let attrs = list.as_mut_ptr().cast();
        if InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let ok = UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize, hpc as *const _, std::mem::size_of::<HPCON>(), std::ptr::null_mut(), std::ptr::null());
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            DeleteProcThreadAttributeList(attrs);
            return Err(e);
        }
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // No standard handles of the window's: the shell's are the console's.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.lpAttributeList = attrs;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr().cast(),
            cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        );
        let e = std::io::Error::last_os_error();
        DeleteProcThreadAttributeList(attrs);
        if ok == 0 {
            return Err(e);
        }
        CloseHandle(pi.hThread);
        Ok(OwnedHandle::from_raw_handle(pi.hProcess as _))
    }
}

/// The clipboard's text, handed to `done` on a thread of its own.
pub fn paste(done: impl FnOnce(String) + Send + 'static) {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    std::thread::spawn(move || {
        // SAFETY: the clipboard is opened, read and closed here; the locked
        // memory is a NUL-terminated UTF-16 string while it stays locked.
        let text = unsafe {
            // Another program may have it open for a moment.
            let mut opened = false;
            for _ in 0..10 {
                if OpenClipboard(0) != 0 {
                    opened = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if !opened {
                return;
            }
            let h = GetClipboardData(CF_UNICODETEXT);
            let mut text = String::new();
            if h != 0 {
                let p = GlobalLock(h as _) as *const u16;
                if !p.is_null() {
                    let len = (0..).take_while(|&i| *p.add(i) != 0).count();
                    text = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                    GlobalUnlock(h as _);
                }
            }
            CloseClipboard();
            text
        };
        done(text);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    static EVENTS: Mutex<Vec<Sender<String>>> = Mutex::new(Vec::new());

    #[test]
    fn a_captured_program_has_a_console_reads_what_is_typed_and_says_its_exit_code() {
        let cmd = system32("cmd.exe");
        let (output, input, _stop, exit) = captured(&cmd, &["/d", "/q", "/c", "echo code: ABCD-EFGH& set /p x=& call echo got:%x%& exit 3"]).unwrap();
        input.send(b"yes\r".to_vec()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = String::new();
        while let Ok(b) = output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            seen.push_str(&String::from_utf8_lossy(&b));
            if seen.contains("got:yes") {
                break;
            }
        }
        assert!(seen.contains("code: ABCD-EFGH") && seen.contains("got:yes"), "{seen:?}");
        assert_eq!(exit.recv_timeout(Duration::from_secs(10)).unwrap(), Some(3));
    }

    #[test]
    fn a_captured_program_stops_when_asked() {
        let (_output, _input, stop, exit) = captured(&system32("ping.exe"), &["-n", "30", "127.0.0.1"]).unwrap();
        stop();
        assert!(exit.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    #[ignore = "runs the real docker login (Kiri's docker.exe), which reaches Docker's sign-in service"]
    fn live_docker_login_shows_its_device_code_on_a_captured_console() {
        let docker = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join(r"Kiri\bin\docker.exe");
        let dir = std::env::temp_dir().join(format!("kiri-weblogin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        // SAFETY: an ignored test, run on its own.
        unsafe { std::env::set_var("DOCKER_CONFIG", &dir) };
        let (output, _input, stop, _exit) = captured(&docker, &["login"]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = String::new();
        while let Ok(b) = output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            seen.push_str(&String::from_utf8_lossy(&b));
            if seen.contains("login.docker.com/activate") || seen.contains("Login Succeeded") {
                break;
            }
        }
        stop();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.contains("Login Succeeded") || (seen.contains("device confirmation code") && seen.contains("login.docker.com/activate")), "{seen:?}");
    }


    fn record(json: String) {
        for tx in EVENTS.lock().unwrap().iter() {
            let _ = tx.send(json.clone());
        }
    }

    fn output_until(rx: &Receiver<String>, id: &str, want: &str) -> (String, Option<Value>) {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
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
    fn cmd_runs_on_a_pseudo_console_and_says_when_it_ends() {
        let (tx, rx) = channel();
        EVENTS.lock().unwrap().push(tx);
        handle("open", "w1", &serde_json::json!({"shell": "cmd", "cols": 100, "rows": 20}), record);
        let typed = |s: &str| handle("input", "w1", &Value::String(termwire::b64(s.as_bytes())), record);
        // cmd prints its prompt once it's ready for input.
        output_until(&rx, "w1", ">");
        typed("set /a 40+2 & echo. & echo DONE\r");
        let (out, _) = output_until(&rx, "w1", "DONE");
        assert!(out.contains("42"), "{out}");
        typed("exit 7\r");
        let (_, exit) = output_until(&rx, "w1", "\u{0}never");
        assert_eq!(exit.unwrap()["code"], 7);
    }

    #[test]
    fn the_list_never_offers_kiris_own_distro() {
        let list = shells();
        assert!(list.iter().any(|s| s.id == "cmd"));
        assert!(!list.iter().any(|s| s.id.to_ascii_lowercase().starts_with("wsl:kiri")));
        assert!(command_line("wsl:Ubuntu").ends_with("-d Ubuntu --cd ~"));
    }
}
