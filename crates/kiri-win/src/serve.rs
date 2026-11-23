//! Accepts docker clients on the pipe and carries them over the bridge.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crate::pipe::Pipe;
use crate::refuse::{is_head, refusal, Arrival, REQUEST_WAIT};

/// A frame: kind, connection id (u32 BE), length (u32 BE), then the data.
/// The same as in crates/kiri/src/bridge.rs.
const OPEN: u8 = 1;
const DATA: u8 = 2;
/// No more data from this side (a half-close).
const EOF: u8 = 3;
/// The connection is gone, both ways. From a bridge, its data (when there
/// is any) is why it could not reach the engine.
const CLOSE: u8 = 4;
/// Credit back: how many bytes of this connection the ACK's sender has
/// handed on (u32 BE). For connection 0, the bridge's hello: it does flow
/// control (crates/kiri/src/bridge.rs says how), and with 1 as its data it
/// answers PING.
const ACK: u8 = 5;
/// From the bridge, on connection 0: a line of what it did to reach the
/// engine (started it, and how that went), for this side's log.
const NOTE: u8 = 6;
/// On connection 0: asks a bridge that said it answers for a PING back.
const PING: u8 = 7;

/// Bytes of one connection one way that may be unacknowledged; the same
/// as the bridge's.
const WINDOW: usize = 4 << 20;
const ACK_EVERY: usize = WINDOW / 4;
/// A connection with credit that is sent past it is closed once this much
/// is queued; one without (an older bridge) at what was 256 frames.
const LIMIT: usize = 2 * WINDOW;
const LIMIT_NO_CREDIT: usize = 256 * 64 * 1024;
/// How long a bridge may take to say hello after wsl.exe was started for
/// it. Every bridge says it as it starts, so one that has not by then is a
/// wsl.exe that hangs (as it can right after Windows wakes): it is ended and
/// a new one started. Long enough for WSL to start from cold.
const HELLO_WAIT: Duration = Duration::from_secs(20);
/// A bridge not heard from for this long is asked for a PING before a new
/// connection goes on it, and ended when none comes within PING_WAIT: a
/// wsl.exe that outlived what it was connected to (WSL restarted while
/// Windows slept) takes connections and never answers them.
const QUIET: Duration = Duration::from_secs(2);
const PING_WAIT: Duration = Duration::from_secs(10);
/// How many bridges in a row a connection finds not answering before its
/// client is told why.
const SILENT_MOST: u32 = 2;
/// After a client was told WSL is not answering, those that come within
/// this are told the same once the bridge they find has had TOLD_GRACE to
/// say hello and has not: docker asks twice for one command (its ping,
/// then the request), and the second is not made to wait as long again.
/// A WSL that answers again says hello well within the grace, so the
/// error does not outlast its cause.
const TOLD_HOLD: Duration = Duration::from_secs(15);
const TOLD_GRACE: Duration = Duration::from_millis(1500);
/// How long a new connection keeps starting a bridge that dies at once
/// (wsl.exe right after Windows wakes, while WSL itself starts again)
/// before the client is told why; one try every BRIDGE_RETRY.
const BRIDGE_WAIT: Duration = Duration::from_secs(10);
const BRIDGE_RETRY: Duration = Duration::from_millis(500);

/// wsl.exe without a console window flashing up.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone)]
pub struct Config {
    pub distro: Option<String>,
    pub pipe: String,
    pub socket: String,
    pub kiri: String,
}

impl Config {
    pub fn from_args() -> Config {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        Config {
            // The distro setup chose, unless one is named.
            distro: flag("--distro").or_else(crate::setup::distro),
            pipe: flag("--pipe").unwrap_or_else(|| r"\\.\pipe\kiri".into()),
            // Where contrib/wsl/install.sh runs the daemon.
            socket: flag("--socket").unwrap_or_else(|| "/var/lib/kiri-daemon/.kiri/run/docker.sock".into()),
            kiri: flag("--kiri").unwrap_or_else(|| "/usr/local/bin/kiri".into()),
        }
    }
}

static HUB: std::sync::OnceLock<Arc<Hub>> = std::sync::OnceLock::new();

/// Setup isn't finished: a docker command with no engine to answer it is
/// told so, rather than having its connection simply closed.
static SETUP_PENDING: AtomicBool = AtomicBool::new(false);

pub fn set_setup_pending(pending: bool) {
    SETUP_PENDING.store(pending, Ordering::SeqCst);
}

const SETUP_UNFINISHED: &str = "Kiri's setup isn't finished: open Kiri to finish it";

fn setup_pending() -> bool {
    SETUP_PENDING.load(Ordering::SeqCst)
}

/// What a client the engine cannot serve is told (see `refuse::message`),
/// unless setup is unfinished: then that is why.
fn message(why: Option<&str>) -> String {
    if setup_pending() { SETUP_UNFINISHED.to_owned() } else { crate::refuse::message(why) }
}

/// Answers a client the engine cannot serve. True if it answered.
fn refuse(pipe: &Pipe, head: bool, message: &str) -> bool {
    if pipe.write(&refusal(head, message)).is_err() {
        return false;
    }
    pipe.flush();
    let _ = pipe.write(&[]);
    !pipe.is_aborted()
}

/// `refuse`, for a client whose request has not been read: read first, as
/// a reply that comes before the request is one Go's HTTP client calls
/// unsolicited and drops.
fn refuse_new(pipe: &Pipe, message: &str) -> bool {
    let mut buf = vec![0u8; 64 * 1024];
    let n = pipe.read_within(&mut buf, Some(REQUEST_WAIT)).unwrap_or(0);
    refuse(pipe, is_head(&buf[..n]), message)
}

/// Docker's own pipe. docker-py, the Go and Java SDKs, Testcontainers and
/// IDE plugins open it without reading docker contexts, so Kiri serves it
/// too while no other engine does.
pub const DOCKER_ENGINE: &str = r"\\.\pipe\docker_engine";

/// Processes that serve docker_engine themselves: while one runs, the pipe
/// is theirs, and Kiri lets go of it (Docker Desktop cannot start its own
/// while any instance of Kiri's is open).
const OTHER_ENGINES: &[&str] = &["Docker Desktop.exe", "com.docker.backend.exe", "dockerd.exe", "Rancher Desktop.exe"];

/// Carries connections to `distro` from now on (setup has just made it);
/// the bridge to the old one is left to end.
pub fn set_distro(distro: &str) {
    if let Some(hub) = HUB.get() {
        hub.cfg.lock().unwrap_or_else(PoisonError::into_inner).distro = Some(distro.to_owned());
        hub.retire();
    }
}

/// The engine in WSL was replaced by this version's: connections from now
/// on go over a bridge run from the new one. The old bridge, started from
/// the engine that was there before, carries the connections it has to
/// their end.
pub fn renew_bridge() {
    if let Some(hub) = HUB.get()
        && hub.retire()
    {
        log("bridge: the engine in WSL was replaced; new connections get a bridge from the new one");
    }
}

/// Serves the pipe on threads of its own. Fails if another kiri-win
/// already serves it.
pub fn start(cfg: Config) -> io::Result<()> {
    let first = Pipe::create(&cfg.pipe, true)?;
    log(&format!("serving {} ({})", cfg.pipe, cfg.distro.as_deref().unwrap_or("default distro")));
    let pipe_name = cfg.pipe.clone();
    let hub = Arc::new(Hub::new(cfg, Box::new(Bridge::start), Limits::default()));
    let _ = HUB.set(hub.clone());
    let h = hub.clone();
    thread::Builder::new().name("kiri-docker-engine".into()).spawn(move || docker_engine(&h))?;
    // Start WSL now, so the first docker command does not wait for it.
    let h = hub.clone();
    thread::spawn(move || {
        if let Err(e) = h.bridge() {
            if e.kind() != io::ErrorKind::NotConnected {
                log(&format!("bridge: {e}"));
            }
        }
    });
    thread::Builder::new().name("kiri-pipe".into()).spawn(move || {
        let mut pipe = first;
        loop {
            if let Err(e) = pipe.connect() {
                log(&format!("connect: {e}"));
            } else {
                let hub = hub.clone();
                thread::spawn(move || serve(&hub, pipe));
            }
            // Every instance in use (ERROR_PIPE_BUSY) is not fatal: wait for a
            // client to go. Nothing here may take the whole Windows side down.
            let mut warned = false;
            pipe = loop {
                match Pipe::create(&pipe_name, false) {
                    Ok(p) => break p,
                    Err(e) => {
                        if !warned {
                            log(&format!("{pipe_name}: {e}; waiting for a free instance"));
                            warned = true;
                        }
                        thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            };
        }
    })?;
    Ok(())
}

/// Serves docker_engine whenever it is free, and hands it back when another
/// engine starts.
fn docker_engine(hub: &Arc<Hub>) {
    let mut said = String::new();
    let mut say = |msg: String| {
        if msg != said {
            log(&msg);
            said = msg;
        }
    };
    loop {
        if let Some(other) = other_engine() {
            say(format!("{DOCKER_ENGINE} left to {other}"));
            thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }
        let first = match Pipe::create(DOCKER_ENGINE, true) {
            Ok(p) => p,
            Err(e) => {
                say(format!("{DOCKER_ENGINE} left to whatever serves it: {e}"));
                thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };
        say(format!("serving {DOCKER_ENGINE} as well (no other engine has it)"));
        let (h, mut new) = (hub.clone(), NewEngines::default());
        serve_while_free(DOCKER_ENGINE, first, move || new.find(), move |p| serve_shared(&h, p));
    }
}

/// How often Kiri looks for another engine while it serves docker_engine.
/// Docker Desktop's backend creates the pipe about 0.4 s after Docker
/// Desktop starts, and gives up if an instance of Kiri's is open then.
const ENGINE_CHECK: Duration = Duration::from_millis(100);
/// How many fast ticks between PID-reuse scans (200 ms, within Docker
/// Desktop's ~0.4 s pipe-creation window).
const REUSE_CHECK_EVERY: u8 = 2;
/// How many fast ticks between periodic full snapshots.
const FULL_SNAPSHOT_EVERY: u8 = 50;
const INITIAL_PIDS: usize = 4096;
const MAX_PIDS: usize = 65536;
const SYSTEM_SID: &str = "S-1-5-18";
const ADMINS_SID: &str = "S-1-5-32-544";
const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x0000_2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Created {
    At(u64),
    Denied,
    Gone,
}

#[derive(Debug, PartialEq, Eq)]
enum ProcessInfo {
    /// Opened and inspected: an optional trusted engine under Program Files
    /// along with the process creation time when readable.
    Known { engine: Option<&'static str>, created: Option<u64> },
    /// Access denied: owner must be verified for this PID via SCM or WTS.
    Denied,
    /// Exited or a minimal kernel process with no executable path.
    Gone,
}

/// Process inspection callbacks, allowing `NewEngines` to be tested directly.
trait EngineProbe {
    fn list_pids(&mut self, buf: &mut [u32]) -> Option<usize>;
    fn inspect(&mut self, pid: u32) -> ProcessInfo;
    fn created(&mut self, pid: u32) -> Created;
    fn check_denied(&mut self, pids: &[u32]) -> Option<&'static str>;
    fn check_services(&mut self) -> Option<&'static str>;
    fn snapshot(&mut self) -> Option<&'static str>;
}

#[derive(Default)]
struct WinProbe {
    seen_services: HashSet<(u32, String)>,
}

impl EngineProbe for WinProbe {
    fn list_pids(&mut self, buf: &mut [u32]) -> Option<usize> {
        k32_enum_pids(buf)
    }

    fn inspect(&mut self, pid: u32) -> ProcessInfo {
        inspect_process(pid)
    }

    fn created(&mut self, pid: u32) -> Created {
        process_created(pid)
    }

    fn check_denied(&mut self, pids: &[u32]) -> Option<&'static str> {
        check_denied_pids(pids)
    }

    fn check_services(&mut self) -> Option<&'static str> {
        service_engine_for_pids(None, Some(&mut self.seen_services))
    }

    fn snapshot(&mut self) -> Option<&'static str> {
        other_engine()
    }
}

/// Lists running PIDs into `pids`, growing the buffer up to `MAX_PIDS` when
/// full before returning `None` so callers can fall back cleanly.
fn enum_pids_with(pids: &mut Vec<u32>, mut list: impl FnMut(&mut [u32]) -> Option<usize>) -> Option<usize> {
    if pids.len() < INITIAL_PIDS {
        pids.resize(INITIAL_PIDS, 0);
    }
    loop {
        let count = list(pids)?;
        if count < pids.len() {
            return Some(count);
        }
        if pids.len() >= MAX_PIDS {
            return None;
        }
        let next = (pids.len() * 2).min(MAX_PIDS);
        pids.resize(next, 0);
    }
}

fn k32_enum_pids(buf: &mut [u32]) -> Option<usize> {
    use windows_sys::Win32::System::ProcessStatus::K32EnumProcesses;
    let mut bytes = 0u32;
    let cap_bytes = std::mem::size_of_val(buf) as u32;
    // SAFETY: `buf` holds `buf.len()` u32s (`cap_bytes` bytes).
    let listed = unsafe { K32EnumProcesses(buf.as_mut_ptr(), cap_bytes, &mut bytes) } != 0;
    listed.then_some(bytes as usize / std::mem::size_of::<u32>())
}

/// Finds another engine among the processes started since it last looked.
/// Listing PIDs takes about 0.06 ms, so the watcher runs every 100 ms and
/// inspects only new PIDs (and checks creation times every 200 ms for PID
/// reuse). Only processes under Program Files owned by this user, SYSTEM, or
/// Administrators at medium integrity or above count; unreadable PIDs are
/// verified per PID as SYSTEM services or Session 0 SYSTEM processes via SCM
/// and WTS.
#[derive(Default)]
struct NewEngines {
    started: bool,
    reuse_ticks: u8,
    full_ticks: u8,
    pids: Vec<u32>,
    seen: HashMap<u32, Option<u64>>,
    next: HashMap<u32, Option<u64>>,
    probe: WinProbe,
}

impl NewEngines {
    fn find(&mut self) -> Option<&'static str> {
        let mut probe = std::mem::take(&mut self.probe);
        let found = self.find_with(&mut probe);
        self.probe = probe;
        found
    }

    fn find_with(&mut self, probe: &mut impl EngineProbe) -> Option<&'static str> {
        let mut pids = std::mem::take(&mut self.pids);
        let found = match enum_pids_with(&mut pids, |buf| probe.list_pids(buf)) {
            Some(count) => self.scan(&pids[..count], probe),
            None => probe.snapshot(),
        };
        self.pids = pids;
        found
    }

    fn scan(&mut self, pids: &[u32], probe: &mut impl EngineProbe) -> Option<&'static str> {
        if !self.started {
            self.started = true;
            self.seen.clear();
            for &p in pids {
                let created = match probe.created(p) {
                    Created::At(t) => Some(t),
                    Created::Denied | Created::Gone => None,
                };
                self.seen.insert(p, created);
            }
            // Snapshot taken after recording `seen`: a process started during
            // the snapshot is caught either here or on the next tick.
            return probe.snapshot();
        }
        self.reuse_ticks = (self.reuse_ticks + 1) % REUSE_CHECK_EVERY;
        self.full_ticks = (self.full_ticks + 1) % FULL_SNAPSHOT_EVERY;
        let check_reuse = self.reuse_ticks == 0;
        self.next.clear();
        let mut found = None;
        let mut denied = Vec::new();
        let mut had_unreadable = false;
        for &p in pids {
            if let Some(&prev_created) = self.seen.get(&p) {
                if !check_reuse || found.is_some() {
                    self.next.insert(p, prev_created);
                    continue;
                }
                match probe.created(p) {
                    Created::At(t) if prev_created == Some(t) => {
                        self.next.insert(p, Some(t));
                    }
                    Created::At(_) => match probe.inspect(p) {
                        ProcessInfo::Known { engine, created } => {
                            found = engine;
                            self.next.insert(p, created);
                        }
                        ProcessInfo::Denied => {
                            denied.push(p);
                            self.next.insert(p, None);
                        }
                        ProcessInfo::Gone => {}
                    },
                    Created::Denied => {
                        if prev_created.is_some() {
                            denied.push(p);
                        } else {
                            had_unreadable = true;
                        }
                        self.next.insert(p, None);
                    }
                    Created::Gone => {}
                }
            } else if found.is_none() {
                match probe.inspect(p) {
                    ProcessInfo::Known { engine, created } => {
                        found = engine;
                        self.next.insert(p, created);
                    }
                    ProcessInfo::Denied => {
                        denied.push(p);
                        self.next.insert(p, None);
                    }
                    ProcessInfo::Gone => {}
                }
            } else {
                self.next.insert(p, None);
            }
        }
        std::mem::swap(&mut self.seen, &mut self.next);
        if found.is_some() {
            return found;
        }
        if !denied.is_empty()
            && let Some(engine) = probe.check_denied(&denied)
        {
            return Some(engine);
        }
        if check_reuse && had_unreadable
            && let Some(engine) = probe.check_services()
        {
            return Some(engine);
        }
        if self.full_ticks == 0 && had_unreadable {
            return probe.snapshot();
        }
        None
    }
}

/// Strips a `\\?\` extended-length prefix only when followed by a drive path.
fn strip_win32_prefix(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
            return rest;
        }
    }
    path
}

/// Matches `path` only when it lies inside one of `roots` (Program Files) with
/// no `..` components and its file name is in `OTHER_ENGINES`.
fn engine_at_path(path: &str, roots: &[String]) -> Option<&'static str> {
    let norm = strip_win32_prefix(path).replace('/', "\\");
    if norm.split('\\').any(|c| c == "..") {
        return None;
    }
    let norm_lower = norm.to_lowercase();
    let under_root = roots.iter().any(|root| {
        let r = strip_win32_prefix(root).replace('/', "\\");
        let r = r.trim_end_matches('\\');
        if r.is_empty() {
            return false;
        }
        let mut prefix = r.to_lowercase();
        prefix.push('\\');
        norm_lower.starts_with(&prefix)
    });
    if !under_root {
        return None;
    }
    let file = norm.rsplit('\\').next()?;
    engine_named(file)
}

/// Extracts the executable path from a Windows service's `lpBinaryPathName`.
fn service_binary_path(cmd: &str) -> Option<&str> {
    let s = cmd.trim_start();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        let path = &rest[..end];
        return (!path.is_empty()).then_some(path);
    }
    let lower = s.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(rel) = lower[offset..].find(".exe") {
        let end = offset + rel + 4;
        if end == s.len() || s.as_bytes()[end].is_ascii_whitespace() {
            return Some(&s[..end]);
        }
        offset = end;
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    (!s[..end].is_empty()).then_some(&s[..end])
}

/// Whether a service's configured start account is LocalSystem (`SYSTEM`).
fn is_system_service_account(account: &str) -> bool {
    let a = account.trim();
    a.eq_ignore_ascii_case("LocalSystem")
        || a.eq_ignore_ascii_case(r".\LocalSystem")
        || a.eq_ignore_ascii_case(r"NT AUTHORITY\LocalSystem")
        || a.eq_ignore_ascii_case(r"NT AUTHORITY\SYSTEM")
}

/// Whether an opened process token belongs to this user, SYSTEM, or elevated
/// Administrators at Medium integrity or above.
fn is_trusted_token(user_sid: &str, owner_sid: Option<&str>, integrity_rid: u32, current_user_sid: Option<&str>) -> bool {
    if integrity_rid < SECURITY_MANDATORY_MEDIUM_RID {
        return false;
    }
    if current_user_sid.is_some_and(|me| user_sid.eq_ignore_ascii_case(me)) {
        return true;
    }
    user_sid.eq_ignore_ascii_case(SYSTEM_SID) || owner_sid.is_some_and(|o| o.eq_ignore_ascii_case(ADMINS_SID))
}

/// Whether a WTS entry's owner SID and session belong to SYSTEM in Session 0.
fn is_trusted_wts_owner(user_sid: &str, session_id: u32) -> bool {
    user_sid.eq_ignore_ascii_case(SYSTEM_SID) && session_id == 0
}

/// Reads a NUL-terminated UTF-16 string pointer, returning None when null.
///
/// # Safety
/// `ptr` must be null or point to a valid NUL-terminated UTF-16 string.
unsafe fn wide_to_string(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees `ptr` points to a NUL-terminated UTF-16 string.
    unsafe {
        let mut n = 0usize;
        while *ptr.add(n) != 0 {
            n += 1;
        }
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(ptr, n)))
    }
}

/// Trusted Program Files install directories from the Windows shell API.
fn program_files_dirs() -> &'static [String] {
    static DIRS: OnceLock<Vec<String>> = OnceLock::new();
    DIRS.get_or_init(|| {
        use windows_sys::Win32::System::Com::CoTaskMemFree;
        use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, SHGetKnownFolderPath};
        let mut dirs = Vec::new();
        for id in [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86] {
            let mut ptr: windows_sys::core::PWSTR = std::ptr::null_mut();
            // SAFETY: known folder GUIDs; `ptr` is freed with `CoTaskMemFree`.
            let hr = unsafe { SHGetKnownFolderPath(id, 0, 0, &mut ptr) };
            if hr == 0 && !ptr.is_null() {
                // SAFETY: `SHGetKnownFolderPath` returned a NUL-terminated path.
                if let Some(s) = unsafe { wide_to_string(ptr) }
                    && !s.is_empty()
                    && !dirs.iter().any(|d: &String| d.eq_ignore_ascii_case(&s))
                {
                    dirs.push(s);
                }
                // SAFETY: allocated by `SHGetKnownFolderPath`.
                unsafe { CoTaskMemFree(ptr.cast()) };
            }
        }
        dirs
    })
}

fn current_user_sid_cached() -> Option<&'static str> {
    static SID: OnceLock<Option<String>> = OnceLock::new();
    SID.get_or_init(crate::pipe::current_user_sid).as_deref()
}

/// Reads the creation time of an already-opened process handle as a 64-bit
/// FILETIME.
///
/// # Safety
/// `h` must be a valid process handle with `PROCESS_QUERY_LIMITED_INFORMATION`.
unsafe fn handle_created_at(h: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let mut creation = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut exit = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut kernel = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut user = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    // SAFETY: caller guarantees `h` is a valid process handle.
    if unsafe { GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some(((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64))
}

fn process_created(pid: u32) -> Created {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: opens, reads times from, and closes `h` in this function.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == 0 {
            return if GetLastError() == ERROR_ACCESS_DENIED { Created::Denied } else { Created::Gone };
        }
        let created = handle_created_at(h);
        CloseHandle(h);
        match created {
            Some(t) => Created::At(t),
            None => Created::Gone,
        }
    }
}

/// Reads the full Win32 executable path of `h`, growing the buffer for long paths.
///
/// # Safety
/// `h` must be a valid process handle with `PROCESS_QUERY_LIMITED_INFORMATION`.
unsafe fn handle_image_path(h: windows_sys::Win32::Foundation::HANDLE) -> Result<String, u32> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_INSUFFICIENT_BUFFER};
    use windows_sys::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};
    let mut cap = 1024usize;
    loop {
        let mut buf = vec![0u16; cap];
        let mut len = buf.len() as u32;
        // SAFETY: `h` is valid; `buf` holds `len` UTF-16 code units.
        if unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) } != 0 {
            let n = (len as usize).min(buf.len());
            return Ok(String::from_utf16_lossy(&buf[..n]));
        }
        // SAFETY: reads this thread's last Win32 error.
        let err = unsafe { GetLastError() };
        if err == ERROR_INSUFFICIENT_BUFFER && cap < 32768 {
            cap = (cap * 2).min(32768);
            continue;
        }
        return Err(err);
    }
}

/// Queries a variable-sized `GetTokenInformation` structure into an aligned buffer.
///
/// # Safety
/// `token` must be a valid token handle opened with `TOKEN_QUERY`.
unsafe fn token_info(token: windows_sys::Win32::Foundation::HANDLE, class: i32) -> Option<Vec<usize>> {
    use windows_sys::Win32::Security::GetTokenInformation;
    let mut len = 0u32;
    // SAFETY: size query with null output buffer.
    unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return None;
    }
    let words = (len as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buf = vec![0usize; words];
    let cap = (words * std::mem::size_of::<usize>()) as u32;
    // SAFETY: `buf` is usize-aligned and holds `cap >= len` bytes.
    let ok = unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), cap, &mut len) } != 0;
    ok.then_some(buf)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenTrust {
    Trusted,
    Untrusted,
    Denied,
    Gone,
}

/// Verifies `h`'s token owner/user SID and integrity level.
///
/// # Safety
/// `h` must be a valid process handle with `PROCESS_QUERY_LIMITED_INFORMATION`.
unsafe fn check_process_token(h: windows_sys::Win32::Foundation::HANDLE) -> TokenTrust {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, HANDLE};
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, TokenIntegrityLevel, TokenOwner, TokenUser, TOKEN_MANDATORY_LABEL, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    // SAFETY: opens, queries, and closes `token` within this block.
    unsafe {
        let mut token: HANDLE = 0;
        if OpenProcessToken(h, TOKEN_QUERY, &mut token) == 0 {
            return if GetLastError() == ERROR_ACCESS_DENIED { TokenTrust::Denied } else { TokenTrust::Gone };
        }
        let user_buf = token_info(token, TokenUser);
        let owner_buf = token_info(token, TokenOwner);
        let il_buf = token_info(token, TokenIntegrityLevel);
        CloseHandle(token);

        let Some(user_buf) = user_buf else { return TokenTrust::Untrusted };
        let user = &*(user_buf.as_ptr() as *const TOKEN_USER);
        let Some(user_sid) = crate::pipe::sid_to_string(user.User.Sid) else { return TokenTrust::Untrusted };

        let owner_sid = owner_buf.as_ref().and_then(|b| {
            let owner = &*(b.as_ptr() as *const TOKEN_OWNER);
            crate::pipe::sid_to_string(owner.Owner)
        });

        let Some(il_buf) = il_buf else { return TokenTrust::Untrusted };
        let label = &*(il_buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        if label.Label.Sid.is_null() {
            return TokenTrust::Untrusted;
        }
        let count_ptr = GetSidSubAuthorityCount(label.Label.Sid);
        if count_ptr.is_null() || *count_ptr == 0 {
            return TokenTrust::Untrusted;
        }
        let rid_ptr = GetSidSubAuthority(label.Label.Sid, (*count_ptr - 1) as u32);
        if rid_ptr.is_null() {
            return TokenTrust::Untrusted;
        }
        let rid = *rid_ptr;

        if is_trusted_token(&user_sid, owner_sid.as_deref(), rid, current_user_sid_cached()) {
            TokenTrust::Trusted
        } else {
            TokenTrust::Untrusted
        }
    }
}

/// Resolves the token check result for an opened Program Files engine process.
/// When token access is denied, only a process verified as SYSTEM in Session 0
/// matches; non-SYSTEM processes are treated as untrusted rather than routed to
/// a filename-only fallback.
fn resolve_engine_token(engine: &'static str, created: Option<u64>, trust: TokenTrust, is_system_session0: impl FnOnce() -> bool) -> ProcessInfo {
    match trust {
        TokenTrust::Trusted => ProcessInfo::Known { engine: Some(engine), created },
        TokenTrust::Untrusted => ProcessInfo::Known { engine: None, created },
        TokenTrust::Denied => ProcessInfo::Known { engine: is_system_session0().then_some(engine), created },
        TokenTrust::Gone => ProcessInfo::Gone,
    }
}

/// Inspects `pid`: checks Program Files path and, when the executable name
/// matches `OTHER_ENGINES`, checks token owner and integrity level.
fn inspect_process(pid: u32) -> ProcessInfo {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: handle opened, queried, and closed within this function.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == 0 {
            return if GetLastError() == ERROR_ACCESS_DENIED { ProcessInfo::Denied } else { ProcessInfo::Gone };
        }
        let created = handle_created_at(h);
        let info = match handle_image_path(h) {
            Ok(path) => match engine_at_path(&path, program_files_dirs()) {
                Some(engine) => resolve_engine_token(engine, created, check_process_token(h), || {
                    let targets = HashSet::from([pid]);
                    service_engine_for_pids(Some(&targets), None).is_some() || is_wts_system_session0(pid)
                }),
                None => ProcessInfo::Known { engine: None, created },
            },
            Err(err) if err == ERROR_ACCESS_DENIED => ProcessInfo::Denied,
            Err(_) => ProcessInfo::Gone,
        };
        CloseHandle(h);
        info
    }
}

/// Outcome of one `EnumServicesStatusExW` call.
enum ServiceEnumStep {
    /// All active services fit in the buffer (`count` entries written).
    Done(u32),
    /// Buffer too small; `remaining` is `pcbBytesNeeded` for entries that did
    /// not fit (only the remaining bytes when `cap > 0`).
    MoreData { remaining: u32 },
    /// Enumeration failed with an error other than `ERROR_MORE_DATA`.
    Error,
}

/// Allocates and fills an aligned buffer via `EnumServicesStatusExW`, growing
/// capacity by `cap + remaining` on `ERROR_MORE_DATA` so services added
/// mid-enumeration do not shrink the buffer on retry.
fn enum_services_with(mut call: impl FnMut(&mut [usize], u32) -> ServiceEnumStep) -> Option<(Vec<usize>, u32)> {
    let mut size = match call(&mut [], 0) {
        ServiceEnumStep::MoreData { remaining } if remaining > 0 => remaining as usize,
        _ => return None,
    };
    let mut buf: Vec<usize> = Vec::new();
    for _ in 0..4 {
        let words = size.div_ceil(std::mem::size_of::<usize>());
        let Ok(cap) = u32::try_from(words.saturating_mul(std::mem::size_of::<usize>())) else {
            return None;
        };
        buf.resize(words, 0);
        match call(&mut buf, cap) {
            ServiceEnumStep::Done(count) => return (count > 0).then_some((buf, count)),
            ServiceEnumStep::MoreData { remaining } if remaining > 0 => {
                size = (cap as usize).saturating_add(remaining as usize);
            }
            ServiceEnumStep::MoreData { .. } | ServiceEnumStep::Error => return None,
        }
    }
    None
}

/// Checks active Win32 services via the Service Control Manager, matching only
/// `LocalSystem` services whose executable lies under Program Files and is in
/// `OTHER_ENGINES`. When `target_pids` is `Some`, checks only those PIDs.
fn service_engine_for_pids(target_pids: Option<&HashSet<u32>>, seen_services: Option<&mut HashSet<(u32, String)>>) -> Option<&'static str> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA};
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, EnumServicesStatusExW, OpenSCManagerW, OpenServiceW, QueryServiceConfigW, ENUM_SERVICE_STATUS_PROCESSW, QUERY_SERVICE_CONFIGW, SC_ENUM_PROCESS_INFO,
        SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_ACTIVE, SERVICE_QUERY_CONFIG, SERVICE_WIN32,
    };
    // SAFETY: SCM and service handles are checked and closed; buffers are
    // usize-aligned and sized from the length queries.
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE);
        if scm == 0 {
            return None;
        }
        let Some((buf, count)) = enum_services_with(|buf, cap| {
            let mut needed = 0u32;
            let mut count = 0u32;
            let mut resume = 0u32;
            let ptr = if cap == 0 { std::ptr::null_mut() } else { buf.as_mut_ptr().cast() };
            if EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_ACTIVE, ptr, cap, &mut needed, &mut count, &mut resume, std::ptr::null()) != 0 {
                ServiceEnumStep::Done(count)
            } else if GetLastError() == ERROR_MORE_DATA {
                ServiceEnumStep::MoreData { remaining: needed }
            } else {
                ServiceEnumStep::Error
            }
        }) else {
            CloseServiceHandle(scm);
            return None;
        };
        let entries = std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize);
        let mut next_seen = seen_services.is_some().then(HashSet::new);
        let mut found = None;
        for e in entries {
            let pid = e.ServiceStatusProcess.dwProcessId;
            if pid == 0 {
                continue;
            }
            if let Some(targets) = target_pids
                && !targets.contains(&pid)
            {
                continue;
            }
            let Some(name) = wide_to_string(e.lpServiceName) else { continue };
            if let Some(next) = next_seen.as_mut() {
                next.insert((pid, name.clone()));
            }
            if let Some(seen) = seen_services.as_deref()
                && seen.contains(&(pid, name))
            {
                continue;
            }
            let svc = OpenServiceW(scm, e.lpServiceName, SERVICE_QUERY_CONFIG);
            if svc == 0 {
                continue;
            }
            let mut qn = 0u32;
            QueryServiceConfigW(svc, std::ptr::null_mut(), 0, &mut qn);
            if qn > 0 && GetLastError() == ERROR_INSUFFICIENT_BUFFER {
                let words = (qn as usize).div_ceil(std::mem::size_of::<usize>());
                let mut qbuf = vec![0usize; words];
                let qcap = (words * std::mem::size_of::<usize>()) as u32;
                if QueryServiceConfigW(svc, qbuf.as_mut_ptr().cast(), qcap, &mut qn) != 0 {
                    let cfg = &*(qbuf.as_ptr() as *const QUERY_SERVICE_CONFIGW);
                    if let (Some(account), Some(bin)) = (wide_to_string(cfg.lpServiceStartName), wide_to_string(cfg.lpBinaryPathName))
                        && is_system_service_account(&account)
                        && let Some(exe) = service_binary_path(&bin)
                        && let Some(engine) = engine_at_path(exe, program_files_dirs())
                    {
                        found = Some(engine);
                    }
                }
            }
            CloseServiceHandle(svc);
            if found.is_some() {
                break;
            }
        }
        if found.is_none()
            && let (Some(seen), Some(next)) = (seen_services, next_seen)
        {
            *seen = next;
        }
        CloseServiceHandle(scm);
        found
    }
}

/// Enumerates WTS process entries across all sessions, passing them to `f`
/// and freeing the returned buffer before returning.
fn with_wts_processes<R>(f: impl FnOnce(&[windows_sys::Win32::System::RemoteDesktop::WTS_PROCESS_INFOW]) -> R) -> Option<R> {
    use windows_sys::Win32::System::RemoteDesktop::{WTSEnumerateProcessesExW, WTSFreeMemoryExW, WTSTypeProcessInfoLevel0, WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW};
    // WTS_ANY_SESSION = (DWORD)-2 in wtsapi32.h.
    const WTS_ANY_SESSION: u32 = u32::MAX - 1;
    struct WtsMem(*mut std::ffi::c_void, u32);
    impl Drop for WtsMem {
        fn drop(&mut self) {
            // SAFETY: `self.0` was allocated by `WTSEnumerateProcessesExW` at level 0 with `self.1` entries.
            unsafe { WTSFreeMemoryExW(WTSTypeProcessInfoLevel0, self.0, self.1) };
        }
    }
    let mut level = 0u32;
    let mut ptr: windows_sys::core::PWSTR = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: queries level-0 process entries across all sessions and guards
    // the returned block with `WtsMem` so it is freed on return or unwind.
    unsafe {
        if WTSEnumerateProcessesExW(WTS_CURRENT_SERVER_HANDLE, &mut level, WTS_ANY_SESSION, &mut ptr, &mut count) == 0 || ptr.is_null() {
            return None;
        }
        let _guard = WtsMem(ptr.cast(), count);
        let entries = std::slice::from_raw_parts(ptr as *const WTS_PROCESS_INFOW, count as usize);
        Some(f(entries))
    }
}

/// Checks whether `pid` belongs to SYSTEM in Session 0 via WTS.
fn is_wts_system_session0(pid: u32) -> bool {
    with_wts_processes(|entries| {
        entries.iter().any(|e| {
            if e.ProcessId != pid {
                return false;
            }
            // SAFETY: `pUserSid` is null or a valid SID within the WTS buffer.
            let Some(sid) = (unsafe { crate::pipe::sid_to_string(e.pUserSid) }) else { return false };
            is_trusted_wts_owner(&sid, e.SessionId)
        })
    })
    .unwrap_or(false)
}

/// Checks specific denied PIDs via WTS, requiring a verified non-null owner
/// SID belonging to SYSTEM in Session 0.
fn wts_engine_for_pids(target_pids: &HashSet<u32>) -> Option<&'static str> {
    with_wts_processes(|entries| {
        for e in entries {
            if !target_pids.contains(&e.ProcessId) {
                continue;
            }
            // SAFETY: WTS entry pointers remain valid until `with_wts_processes` drops its guard.
            let Some(name) = (unsafe { wide_to_string(e.pProcessName) }) else { continue };
            let Some(engine) = engine_named(&name) else { continue };
            // SAFETY: `pUserSid` is null or a valid SID within the WTS buffer.
            let Some(sid) = (unsafe { crate::pipe::sid_to_string(e.pUserSid) }) else { continue };
            if is_trusted_wts_owner(&sid, e.SessionId) {
                return Some(engine);
            }
        }
        None
    })
    .flatten()
}

/// Verifies unreadable PIDs by checking those exact PIDs against active SYSTEM
/// services under Program Files and against Session 0 SYSTEM processes via WTS.
fn check_denied_pids(pids: &[u32]) -> Option<&'static str> {
    if pids.is_empty() {
        return None;
    }
    let targets: HashSet<u32> = pids.iter().copied().collect();
    service_engine_for_pids(Some(&targets), None).or_else(|| wts_engine_for_pids(&targets))
}

/// The one of OTHER_ENGINES an executable's file name is.
fn engine_named(file: &str) -> Option<&'static str> {
    OTHER_ENGINES.iter().copied().find(|o| o.eq_ignore_ascii_case(file))
}

/// How long the handover waits for the connected clients' instances to close.
const HANG_UP: Duration = Duration::from_secs(5);

struct Handover {
    stop: bool,
    /// The instance currently waiting in `Pipe::connect`, for the watcher to abort.
    waiting: Option<Arc<Pipe>>,
}

/// Decrements `active` when dropped, after a client handler has dropped its
/// `Pipe` and `Pipe::drop` has closed the handle.
struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Accepts clients on `name` until `other` finds another engine running,
/// then closes every instance, the connected ones too: Docker Desktop cannot
/// create the pipe while any is open. Its clients reconnect, to it.
fn serve_while_free<O, F>(name: &str, first: Pipe, mut other: O, serve: F)
where
    O: FnMut() -> Option<&'static str> + Send + 'static,
    F: Fn(Arc<Pipe>) + Clone + Send + 'static,
{
    let state = Arc::new((Mutex::new(Handover { stop: false, waiting: None }), Condvar::new()));
    let (st, n) = (state.clone(), name.to_owned());
    let watcher = thread::spawn(move || {
        let (lock, cv) = &*st;
        loop {
            if lock.lock().unwrap_or_else(PoisonError::into_inner).stop {
                return;
            }
            if let Some(engine) = other() {
                log(&format!("{engine} started: handing {n} back"));
                let mut h = lock.lock().unwrap_or_else(PoisonError::into_inner);
                h.stop = true;
                cv.notify_all();
                // Keep aborting until the accept loop leaves `connect()`: a
                // single cancel before `ConnectNamedPipe` enters the kernel
                // would leave it waiting indefinitely.
                loop {
                    let Some(p) = h.waiting.clone() else { return };
                    drop(h);
                    p.abort();
                    let h2 = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    if h2.waiting.is_none() {
                        return;
                    }
                    h = cv.wait_timeout(h2, Duration::from_millis(5)).unwrap_or_else(PoisonError::into_inner).0;
                }
            }
            let h = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if h.stop {
                return;
            }
            let _ = cv.wait_timeout(h, ENGINE_CHECK).unwrap_or_else(PoisonError::into_inner);
        }
    });
    let mut clients: Vec<Weak<Pipe>> = Vec::new();
    let active = Arc::new(AtomicUsize::new(0));
    let mut pipe = Arc::new(first);
    let (lock, cv) = &*state;
    'serve: loop {
        {
            let mut h = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if h.stop {
                break;
            }
            h.waiting = Some(pipe.clone());
        }
        let connected = pipe.connect();
        let stopping = {
            let mut h = lock.lock().unwrap_or_else(PoisonError::into_inner);
            h.waiting = None;
            cv.notify_all();
            h.stop
        };
        if stopping {
            break;
        }
        match connected {
            Ok(()) => {
                clients.retain(|p| p.strong_count() > 0);
                clients.push(Arc::downgrade(&pipe));
                active.fetch_add(1, Ordering::SeqCst);
                let (serve, p, done) = (serve.clone(), pipe.clone(), ActiveGuard(active.clone()));
                thread::spawn(move || {
                    let _done = done;
                    serve(p);
                });
            }
            Err(e) => log(&format!("{name} connect: {e}")),
        }
        // Every instance in use: wait for a client to go, as for Kiri's pipe.
        pipe = loop {
            if lock.lock().unwrap_or_else(PoisonError::into_inner).stop {
                break 'serve;
            }
            match Pipe::create(name, false) {
                Ok(p) => break Arc::new(p),
                Err(_) => {
                    let h = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    if h.stop {
                        break 'serve;
                    }
                    let _ = cv.wait_timeout(h, Duration::from_millis(20)).unwrap_or_else(PoisonError::into_inner);
                }
            }
        };
    }
    // Close our own instance before joining the watcher and hanging up clients.
    pipe.abort();
    drop(pipe);
    {
        let mut h = lock.lock().unwrap_or_else(PoisonError::into_inner);
        h.stop = true;
        cv.notify_all();
    }
    let _ = watcher.join();
    hang_up(name, &clients, &active);
}

/// Ends the connected clients of `name`, and waits for their instances to
/// close. A cancel ends only the I/O waiting at that moment, so it is
/// repeated until each client's threads have dropped the pipe and closed its
/// handle.
fn hang_up(name: &str, clients: &[Weak<Pipe>], active: &AtomicUsize) {
    let until = Instant::now() + HANG_UP;
    loop {
        let open: Vec<Arc<Pipe>> = clients.iter().filter_map(Weak::upgrade).collect();
        let left = active.load(Ordering::SeqCst);
        if open.is_empty() && left == 0 {
            return;
        }
        if Instant::now() > until {
            log(&format!("{name}: {} connection(s) still open after the handover", open.len().max(left)));
            return;
        }
        for p in &open {
            p.abort();
        }
        // Not held while waiting: the last reference closes the instance.
        drop(open);
        thread::sleep(Duration::from_millis(10));
    }
}

/// Enumerates running PIDs via a ToolHelp snapshot when `K32EnumProcesses` fails.
fn toolhelp_pids() -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    let mut pids = Vec::new();
    // SAFETY: a process snapshot read entry by entry into a zeroed,
    // correctly sized PROCESSENTRY32W, and closed once.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return pids;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap, &mut e) != 0;
        while more {
            pids.push(e.th32ProcessID);
            more = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    pids
}

/// The first verified process of OTHER_ENGINES that is running.
fn other_engine() -> Option<&'static str> {
    let mut pids = Vec::new();
    let pid_list = match enum_pids_with(&mut pids, k32_enum_pids) {
        Some(n) => {
            pids.truncate(n);
            pids
        }
        None => toolhelp_pids(),
    };
    let mut denied = Vec::new();
    for pid in pid_list {
        match inspect_process(pid) {
            ProcessInfo::Known { engine: Some(e), .. } => return Some(e),
            ProcessInfo::Denied => denied.push(pid),
            ProcessInfo::Known { engine: None, .. } | ProcessInfo::Gone => {}
        }
    }
    check_denied_pids(&denied)
}

/// The processes named `name` that `parent` started.
fn children(parent: u32, name: &str) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    let mut found = Vec::new();
    // SAFETY: a process snapshot read entry by entry into a zeroed,
    // correctly sized PROCESSENTRY32W, and closed once.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return found;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap, &mut e) != 0;
        while more {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            if e.th32ParentProcessID == parent && String::from_utf16_lossy(&e.szExeFile[..len]).eq_ignore_ascii_case(name) {
                found.push(e.th32ProcessID);
            }
            more = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    found
}

/// Ends the process `pid`, if this user may.
fn terminate(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    // SAFETY: a handle opened here, used once and closed; 0 is no handle.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h != 0 {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

/// What the bridge sends back for one connection.
#[derive(Debug)]
enum Reply {
    Data(Vec<u8>),
    Eof,
    /// With the bridge's reason, when it gave one.
    Close(Option<String>),
}

/// One connection, as the bridge's reply reader sees it.
#[derive(Debug)]
struct Conn {
    /// To the thread writing to the client. Unbounded, but `queued` is kept
    /// under the window by the credit the bridge waits for.
    tx: Sender<Reply>,
    queued: Arc<AtomicUsize>,
    /// Credit for the client's input, when the bridge does flow control.
    window: Option<Arc<Window>>,
}

impl Conn {
    fn close(&self) {
        if let Some(w) = &self.window {
            w.close();
        }
    }
}

/// The credit one way on one connection: bytes sent and not yet acknowledged.
#[derive(Debug, Default)]
struct Window {
    state: Mutex<(usize, bool)>,
    cv: Condvar,
}

impl Window {
    /// Waits while a whole window is unacknowledged, then counts `n` more;
    /// false once the connection has closed or `alive` says the client has gone.
    fn take(&self, n: usize, alive: impl Fn() -> bool) -> bool {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        while s.0 >= WINDOW && !s.1 {
            let (g, t) = self.cv.wait_timeout(s, Duration::from_millis(50)).unwrap_or_else(PoisonError::into_inner);
            s = g;
            if t.timed_out() && !alive() {
                return false;
            }
        }
        if s.1 {
            return false;
        }
        s.0 += n;
        true
    }

    fn ack(&self, n: usize) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.0 = s.0.saturating_sub(n);
        self.cv.notify_all();
    }

    fn close(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        self.cv.notify_all();
    }
}

/// Starts a bridge: wsl.exe, or a stand-in in tests.
type Spawn = Box<dyn Fn(&Config) -> io::Result<Arc<Bridge>> + Send + Sync>;

/// How long a bridge is waited for; shorter in tests.
#[derive(Debug, Clone, Copy)]
struct Limits {
    hello: Duration,
    quiet: Duration,
    ping: Duration,
    told_hold: Duration,
    told_grace: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { hello: HELLO_WAIT, quiet: QUIET, ping: PING_WAIT, told_hold: TOLD_HOLD, told_grace: TOLD_GRACE }
    }
}

struct Hub {
    /// Setup may change the distro while kiri-win runs.
    cfg: Mutex<Config>,
    /// The running bridge; a new one starts when it has died (WSL shut down).
    current: Mutex<Option<Arc<Bridge>>>,
    next_id: AtomicU32,
    spawn: Spawn,
    limits: Limits,
    /// When a client was last told WSL is not answering, and in what words.
    silent: Mutex<Option<(Instant, String)>>,
}

impl Hub {
    fn new(cfg: Config, spawn: Spawn, limits: Limits) -> Hub {
        Hub { cfg: Mutex::new(cfg), current: Mutex::new(None), next_id: AtomicU32::new(1), spawn, limits, silent: Mutex::new(None) }
    }

    /// What a client was told within `told_hold`: WSL is not answering.
    fn told_silent(&self) -> Option<String> {
        self.silent.lock().unwrap_or_else(PoisonError::into_inner).as_ref().filter(|(at, _)| at.elapsed() < self.limits.told_hold).map(|(_, why)| why.clone())
    }

    /// Takes the running bridge out of use for new connections; true if
    /// there was one. It ends once the connections it has are over.
    fn retire(&self) -> bool {
        let Some(b) = self.current.lock().unwrap_or_else(PoisonError::into_inner).take() else { return false };
        b.retired.store(true, Ordering::SeqCst);
        b.dead.store(true, Ordering::SeqCst);
        b.end_if_unused();
        true
    }

    fn bridge(&self) -> io::Result<Arc<Bridge>> {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(b) = current.as_ref().filter(|b| !b.dead.load(Ordering::SeqCst)) {
            return Ok(b.clone());
        }
        let cfg = self.cfg.lock().unwrap_or_else(PoisonError::into_inner).clone();
        // Until setup has made Kiri's Linux there is nothing to bridge to.
        if cfg.distro.is_none() {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "Kiri is not set up yet"));
        }
        // The last one has gone (WSL restarted, Windows slept): a docker
        // client is waiting, so a new one starts now, and the log says why;
        // once, not again for each try while new ones die as they start.
        if current.as_ref().is_some_and(|b| b.started.elapsed() > 2 * BRIDGE_RETRY) {
            log("bridge: the last one has gone; starting a new one for a docker client");
        }
        let b = (self.spawn)(&cfg)?;
        *current = Some(b.clone());
        Ok(b)
    }
}

/// What runs a bridge: wsl.exe, or a stand-in in tests.
trait Process: Send {
    fn kill(&mut self);
    /// How it exited, once it has.
    fn exited(&mut self) -> Option<String>;
}

/// wsl.exe as kiri-win starts it: System32's, which runs the real one
/// (under Program Files) as its child and relays to it.
struct Wsl(Child);

impl Process for Wsl {
    /// The child first: it is the one that hangs, and it outlives the one
    /// kiri-win started when it is not reading what that relays.
    fn kill(&mut self) {
        for pid in children(self.0.id(), "wsl.exe") {
            terminate(pid);
        }
        let _ = self.0.kill();
    }

    fn exited(&mut self) -> Option<String> {
        match self.0.try_wait() {
            Ok(None) => None,
            status => Some(format!("{status:?}")),
        }
    }
}

/// Whether a bridge can take a new connection.
#[derive(Debug, PartialEq)]
enum Answer {
    Yes,
    /// It has exited, or is on its way out.
    Gone,
    /// It runs and says nothing: how that showed.
    No(String),
}

struct Bridge {
    input: Mutex<Box<dyn Write + Send>>,
    process: Mutex<Box<dyn Process>>,
    conns: Mutex<HashMap<u32, Conn>>,
    dead: AtomicBool,
    /// Ended here (`kill`), not by exiting.
    killed: AtomicBool,
    /// The log has said that this one, started after one that did not
    /// answer, does.
    announced: AtomicBool,
    /// Out of use for new connections (`Hub::retire`), and ended once the
    /// ones it has are over.
    retired: AtomicBool,
    /// The bridge said hello: it does flow control.
    credit: AtomicBool,
    /// Its hello said it answers PING.
    pings: AtomicBool,
    started: Instant,
    /// When its last frame came, in ms after `started`.
    heard: AtomicU64,
    /// What clients were last told the engine could not do, logged once
    /// until a client is served again.
    said: Mutex<Option<String>>,
}

impl Bridge {
    fn start(cfg: &Config) -> io::Result<Arc<Bridge>> {
        let mut cmd = crate::win::tool("wsl.exe");
        if let Some(d) = &cfg.distro {
            cmd.args(["-d", d]);
        }
        let mut child = cmd
            .args(["-u", "root", "--", &cfg.kiri, "bridge", "--socket", &cfg.socket])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else { return Err(io::Error::other("wsl.exe has no stdio")) };
        log("bridge started");
        Ok(Bridge::over(Box::new(input), output, Box::new(Wsl(child))))
    }

    /// A bridge over `process`'s input and output.
    fn over(input: Box<dyn Write + Send>, output: impl Read + Send + 'static, process: Box<dyn Process>) -> Arc<Bridge> {
        let bridge = Arc::new(Bridge {
            input: Mutex::new(input),
            process: Mutex::new(process),
            conns: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
            killed: AtomicBool::new(false),
            announced: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            credit: AtomicBool::new(false),
            pings: AtomicBool::new(false),
            started: Instant::now(),
            heard: AtomicU64::new(0),
            said: Mutex::new(None),
        });
        let b = bridge.clone();
        thread::spawn(move || b.replies(output));
        bridge
    }

    /// Milliseconds since it started.
    fn clock(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Hands each reply to its connection, until the bridge exits.
    fn replies(&self, output: impl Read) {
        let mut r = BufReader::with_capacity(64 * 1024, output);
        while let Ok(Some((kind, id, data))) = read_frame(&mut r) {
            self.heard.store(self.clock(), Ordering::SeqCst);
            if kind == PING {
                continue;
            }
            if kind == NOTE {
                if id == 0 {
                    log(&format!("engine in WSL: {}", String::from_utf8_lossy(&data)));
                }
                continue;
            }
            if kind == ACK {
                if id == 0 {
                    self.pings.store(data.first() == Some(&1), Ordering::SeqCst);
                    self.credit.store(true, Ordering::SeqCst);
                } else if let Some(n) = data.get(..4).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes)
                    && let Some(w) = self.conns.lock().unwrap_or_else(PoisonError::into_inner).get(&id).and_then(|c| c.window.clone())
                {
                    w.ack(n as usize);
                }
                continue;
            }
            let reply = match kind {
                DATA => Reply::Data(data),
                EOF => Reply::Eof,
                _ => Reply::Close((kind == CLOSE && !data.is_empty()).then(|| String::from_utf8_lossy(&data).into_owned())),
            };
            let mut conns = self.conns.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(c) = conns.get(&id) else { continue };
            // Never block here: this thread serves every connection. With
            // credit the bridge never sends past the window; without it, a
            // client that has stopped reading is dropped, so it cannot stall
            // the others.
            let len = if let Reply::Data(d) = &reply { d.len() } else { 0 };
            let limit = if c.window.is_some() { LIMIT } else { LIMIT_NO_CREDIT };
            if c.queued.fetch_add(len, Ordering::SeqCst) + len > limit {
                log(&format!("connection {id}: client stopped reading; closing it"));
                if let Some(c) = conns.remove(&id) {
                    c.close();
                }
                drop(conns);
                let _ = self.send(CLOSE, id, &[]);
            } else {
                let _ = c.tx.send(reply);
            }
        }
        self.dead.store(true, Ordering::SeqCst);
        self.close_all();
        // Its output has ended: it has exited, or does within a moment. One
        // that still runs after that is of no use, and is not left behind.
        let until = Instant::now() + Duration::from_secs(2);
        let status = loop {
            let mut p = self.process.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(status) = p.exited() {
                break status;
            }
            if Instant::now() >= until {
                p.kill();
                break "still running with its output closed; ended".to_owned();
            }
            drop(p);
            thread::sleep(Duration::from_millis(20));
        };
        if !self.killed.load(Ordering::SeqCst) {
            log(&format!("bridge exited: {status}"));
        }
    }

    /// Every connection still open ends now.
    fn close_all(&self) {
        for (_, c) in self.conns.lock().unwrap_or_else(PoisonError::into_inner).drain() {
            c.close();
        }
    }

    /// Ends a bridge whose wsl.exe runs and does nothing (`why`), with the
    /// connections on it: nothing else would. Its exit never comes, so
    /// nothing waits for one; a write stuck in its full input ends as it goes.
    fn kill(&self, why: &str) {
        if self.killed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.dead.store(true, Ordering::SeqCst);
        log(&format!("bridge: {why}; ending its wsl.exe"));
        self.process.lock().unwrap_or_else(PoisonError::into_inner).kill();
        self.close_all();
    }

    /// Ends a retired bridge that has no connections left.
    fn end_if_unused(&self) {
        if self.retired.load(Ordering::SeqCst) && self.conns.lock().unwrap_or_else(PoisonError::into_inner).is_empty() && !self.killed.swap(true, Ordering::SeqCst) {
            self.process.lock().unwrap_or_else(PoisonError::into_inner).kill();
        }
    }

    /// Whether it has said hello, waiting until it is `grace` old or `stop`.
    fn hello_within(&self, grace: Duration, stop: impl Fn() -> bool) -> bool {
        while !self.credit.load(Ordering::SeqCst) {
            if stop() || self.dead.load(Ordering::SeqCst) || self.started.elapsed() > grace {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// Whether this bridge can take a new connection: it has said hello
    /// (waited for while it has only just started), and when it has been
    /// quiet, it answers a PING now. An engine from before PING is taken at
    /// its hello.
    fn answers(self: &Arc<Self>, limits: &Limits, stop: impl Fn() -> bool) -> Answer {
        while !self.credit.load(Ordering::SeqCst) {
            if stop() || self.dead.load(Ordering::SeqCst) {
                return Answer::Gone;
            }
            if self.started.elapsed() > limits.hello {
                return Answer::No(format!("wsl.exe was started {} s ago and Kiri's Linux has not answered", self.started.elapsed().as_secs()));
            }
            thread::sleep(Duration::from_millis(5));
        }
        let asked = self.clock();
        if !self.pings.load(Ordering::SeqCst) || asked.saturating_sub(self.heard.load(Ordering::SeqCst)) < limits.quiet.as_millis() as u64 {
            return Answer::Yes;
        }
        // On a thread of its own: a wsl.exe that has stopped reading leaves
        // the write waiting, until `kill`.
        let b = self.clone();
        thread::spawn(move || b.send(PING, 0, &[]));
        let began = Instant::now();
        while self.heard.load(Ordering::SeqCst) < asked {
            if stop() || self.dead.load(Ordering::SeqCst) {
                return Answer::Gone;
            }
            if began.elapsed() > limits.ping {
                return Answer::No(format!("its wsl.exe runs, but Kiri's Linux did not answer in {} s", limits.ping.as_secs()));
            }
            thread::sleep(Duration::from_millis(2));
        }
        Answer::Yes
    }

    fn send(&self, kind: u8, id: u32, data: &[u8]) -> io::Result<()> {
        let mut w = self.input.lock().unwrap_or_else(PoisonError::into_inner);
        let r = w.write_all(&header(kind, id, data.len())).and_then(|()| w.write_all(data)).and_then(|()| w.flush());
        if r.is_err() {
            self.dead.store(true, Ordering::SeqCst);
        }
        r
    }

    /// Logs what a client was told, unless it is what the last one was.
    /// None: a client was served, so the next refusal is logged again.
    fn told(&self, message: Option<&str>) {
        let mut said = self.said.lock().unwrap_or_else(PoisonError::into_inner);
        if said.as_deref() != message {
            if let Some(m) = message {
                log(&format!("engine: told docker clients: {m}"));
            }
            *said = message.map(str::to_owned);
        }
    }

}

/// Flushes a server pipe before dropping the last `Arc<Pipe>` so handover's
/// `Weak::upgrade()` can still reach the instance and cancel an in-flight flush.
struct FlushOnDrop(Arc<Pipe>);

impl Drop for FlushOnDrop {
    fn drop(&mut self) {
        self.0.flush();
    }
}

/// One docker client: its requests go over the bridge, on this thread, and
/// the replies come back on another.
fn serve(hub: &Hub, pipe: Pipe) {
    serve_shared(hub, Arc::new(pipe));
}

fn serve_shared(hub: &Hub, pipe: Arc<Pipe>) {
    let flush = FlushOnDrop(pipe);
    let pipe = &flush.0;
    // A bridge can die between hub.bridge() and the insert below: while WSL
    // is still booting, wsl.exe exits at once. `replies` ends every
    // connection it finds when that happens, but one registered after that
    // sweep sat in the dead bridge's map for good: no thread would ever
    // write its reply or cancel its read, so the client waited forever,
    // with the window's engine thread (and every Start click) behind it.
    // So the insert checks `dead` under the lock the sweep takes, and a
    // bridge found dead is left for the next one.
    //
    // A bridge that runs and says nothing (a wsl.exe that hangs after
    // Windows wakes) is ended and the connection goes on the next one: its
    // request has not been read yet, so nothing is lost. After SILENT_MOST
    // of those in a row the client is told why.
    let began = Instant::now();
    // Since the last bridge that was ended for saying nothing: what the
    // wait for one that dies as it starts is counted from.
    let mut dying_since = began;
    let mut silent = 0;
    let (bridge, id, window, rx, queued) = loop {
        if pipe.is_aborted() {
            return;
        }
        let bridge = match hub.bridge() {
            Ok(b) => b,
            Err(e) => {
                let answered = if e.kind() == io::ErrorKind::NotConnected {
                    setup_pending() && refuse_new(pipe, SETUP_UNFINISHED)
                } else {
                    log(&format!("bridge: {e}"));
                    refuse_new(pipe, &message(Some(&format!("WSL did not start Kiri's Linux ({e})"))))
                };
                if answered {
                    linger(pipe);
                }
                return;
            }
        };
        // A client was just told WSL is not answering. A bridge started
        // back then that has said nothing is ended for a new one, and one
        // that says nothing in the moment a working one needs means this
        // client is told too, without waiting as long again.
        let age = bridge.started.elapsed();
        if let Some(why) = hub.told_silent()
            && !bridge.hello_within(hub.limits.told_grace, || pipe.is_aborted())
        {
            if pipe.is_aborted() {
                return;
            }
            if age > hub.limits.told_grace {
                bridge.kill("it was started while WSL was not answering, and has said nothing");
                continue;
            }
            if refuse_new(pipe, &message(Some(&why))) {
                linger(pipe);
            }
            return;
        }
        if let Answer::No(why) = bridge.answers(&hub.limits, || pipe.is_aborted()) {
            bridge.kill(&why);
        }
        if pipe.is_aborted() {
            return;
        }
        // Ended for saying nothing, here or by another connection that
        // waited on it too: not one that dies as it starts, so the next is
        // started at once.
        if bridge.killed.load(Ordering::SeqCst) && !bridge.retired.load(Ordering::SeqCst) {
            silent += 1;
            dying_since = Instant::now();
            if silent >= SILENT_MOST {
                let why = format!("WSL is not answering (wsl.exe ran {silent} times, {} s in all, and Kiri's Linux said nothing)", began.elapsed().as_secs());
                // Logged once for the clients that waited together.
                let mut told = hub.silent.lock().unwrap_or_else(PoisonError::into_inner);
                if !told.as_ref().is_some_and(|(at, _)| at.elapsed() < hub.limits.told_hold) {
                    log(&format!("bridge: {silent} in a row did not answer, over {} s; told the docker client", began.elapsed().as_secs()));
                }
                *told = Some((Instant::now(), why.clone()));
                drop(told);
                if refuse_new(pipe, &message(Some(&why))) {
                    linger(pipe);
                }
                return;
            }
            continue;
        }
        let id = hub.next_id.fetch_add(1, Ordering::SeqCst);
        // The client's input waits for the bridge's credit (the daemon holds
        // back an attach's stdin until the container reads it), and its
        // replies are bounded by the credit this side gives (see `to_client`).
        let window = bridge.credit.load(Ordering::SeqCst).then(|| Arc::new(Window::default()));
        let (tx, rx) = channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let registered = {
            let mut conns = bridge.conns.lock().unwrap_or_else(PoisonError::into_inner);
            if bridge.dead.load(Ordering::SeqCst) {
                false
            } else {
                conns.insert(id, Conn { tx, queued: queued.clone(), window: window.clone() });
                true
            }
        };
        if registered && bridge.send(OPEN, id, if window.is_some() { &[1] } else { &[] }).is_ok() {
            if silent > 0 && !bridge.announced.swap(true, Ordering::SeqCst) {
                log(&format!("bridge: the new one answers; the docker client that waited {} s is served on it", began.elapsed().as_secs()));
            }
            break (bridge, id, window, rx, queued);
        }
        bridge.conns.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
        if gave_up(dying_since.elapsed()) {
            log(&format!("bridge: kept stopping as soon as it started, for {} s; told the docker client", BRIDGE_WAIT.as_secs()));
            if refuse_new(pipe, &message(Some("Kiri's Linux in WSL kept stopping as the command came in"))) {
                linger(pipe);
            }
            return;
        }
        let retry_until = Instant::now() + BRIDGE_RETRY;
        while Instant::now() < retry_until {
            if pipe.is_aborted() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    };
    // Whether the client's request has come, and is a HEAD: a refusal
    // waits for it (see `refuse_new`).
    let request = Arc::new(Arrival::default());
    let (p, b, w, r) = (pipe.clone(), bridge.clone(), window.clone(), request.clone());
    let writer = thread::spawn(move || to_client(&p, &rx, &queued, &b, w.as_deref().map(|w| (id, w)), &r));
    let mut buf = vec![0u8; 64 * 1024];
    let end = loop {
        match pipe.read(&mut buf) {
            Ok(0) => break EOF,
            Ok(n) => {
                request.set(is_head(&buf[..n]));
                // Out of credit with the client gone (nothing to read its
                // reply), or the connection closed: stop.
                if window.as_ref().is_some_and(|w| !w.take(n, || pipe.alive())) {
                    break CLOSE;
                }
                if bridge.send(DATA, id, &buf[..n]).is_err() {
                    break CLOSE;
                }
            }
            // The client closed the pipe.
            Err(_) => break CLOSE,
        }
    };
    // Gone before a word: nothing for a refusal to wait for.
    request.set(false);
    let end = if pipe.is_aborted() { CLOSE } else { end };
    if end == CLOSE {
        // Drop the reply channel before joining `writer`: a frozen or
        // credit-stalled bridge may never send CLOSE back, and no further
        // replies can be written to this client.
        if let Some(c) = bridge.conns.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
            c.close();
        }
        pipe.cancel();
    } else if !bridge.dead.load(Ordering::SeqCst) {
        // Send EOF off the client thread so a full bridge input pipe cannot
        // hold this instance open during a handover.
        let b = bridge.clone();
        let _ = thread::Builder::new().spawn(move || {
            let _ = b.send(EOF, id, &[]);
        });
    }
    let replies_ended = writer.join().unwrap_or(false);
    let aborted = pipe.is_aborted();
    if end == EOF && replies_ended && !aborted {
        linger(pipe);
    }
    let removed = if end != CLOSE {
        if let Some(c) = bridge.conns.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
            c.close();
            true
        } else {
            false
        }
    } else {
        false
    };
    let send_close = end == CLOSE || ((aborted || !replies_ended) && removed);
    // Flush (unless aborted) while the Arc is still reachable by `hang_up`,
    // then close the pipe handle before notifying the bridge so a stalled
    // bridge never blocks handover or delays `ActiveGuard`.
    drop(flush);
    bridge.end_if_unused();
    if send_close && !bridge.dead.load(Ordering::SeqCst) {
        let b = bridge.clone();
        let _ = thread::Builder::new().spawn(move || {
            let _ = b.send(CLOSE, id, &[]);
        });
    }
}

/// Whether a connection whose bridge keeps dying as it starts has waited
/// long enough (`waited`) to be told why: about BRIDGE_WAIT, rather than
/// three tries within milliseconds while WSL was still coming back.
fn gave_up(waited: Duration) -> bool {
    waited + BRIDGE_RETRY > BRIDGE_WAIT
}

/// Writes the bridge's replies to the client, acknowledging them to the
/// bridge (`credit`) as they go when it does flow control. True if they
/// ended with EOF, or the client was told why the engine could not serve
/// it (once its `request` had come).
fn to_client(pipe: &Pipe, rx: &Receiver<Reply>, queued: &AtomicUsize, bridge: &Bridge, credit: Option<(u32, &Window)>, request: &Arrival) -> bool {
    let mut done = 0usize;
    let mut answered = false;
    loop {
        if pipe.is_aborted() {
            break;
        }
        let reply = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(reply) => reply,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if pipe.is_aborted() {
            break;
        }
        match reply {
            Reply::Data(d) => {
                if pipe.write(&d).is_err() {
                    break;
                }
                if !answered {
                    bridge.told(None);
                }
                answered = true;
                queued.fetch_sub(d.len(), Ordering::SeqCst);
                done += d.len();
                if let Some((id, _)) = credit
                    && done >= ACK_EVERY
                {
                    let _ = bridge.send(ACK, id, &(done as u32).to_be_bytes());
                    done = 0;
                }
            }
            // The docker CLI reads in byte mode, where the empty message
            // that is EOF reads as one only with no data queued ahead of
            // it; behind data it is merged away and the client waits on.
            Reply::Eof => {
                pipe.flush();
                return !pipe.is_aborted() && pipe.write(&[]).is_ok();
            }
            // Closed before a word: the bridge could not reach the engine
            // (and says why), the daemon closed it unanswered (no reason),
            // or setup is unfinished. Said to a client still there, after
            // its request.
            Reply::Close(why) if !answered && pipe.alive() => {
                let message = message(why.as_deref());
                if refuse(pipe, request.head(REQUEST_WAIT), &message) {
                    bridge.told(Some(&message));
                    return !pipe.is_aborted();
                }
                break;
            }
            Reply::Close(_) => break,
        }
    }
    // The daemon closed the connection, or the bridge died with it open
    // (the engine restarting): end the client's side too. Its reader above
    // waits on the client, and a client waiting for its reply (the window's
    // own requests, a docker command) would otherwise wait forever, with
    // the window's engine thread behind it. Its input may be waiting for
    // credit rather than on the client: that ends too.
    if let Some((_, w)) = credit {
        w.close();
    }
    pipe.cancel();
    false
}

/// Waits a while for a client that has had both half-closes to close its
/// end. Disconnecting the instance discards what the client has not read
/// yet, and FlushFileBuffers does not wait for the empty message that is
/// its EOF: a client that had not read it yet (often at the end of 200 MB
/// of `docker run` output) then read ERROR_PIPE_NOT_CONNECTED, "No process
/// is on the other end of the pipe", instead of EOF, about one run in twenty.
fn linger(pipe: &Pipe) {
    let until = Instant::now() + Duration::from_secs(2);
    while pipe.alive() && Instant::now() < until {
        thread::sleep(Duration::from_millis(2));
    }
}

/// The largest frame either side accepts. Both send at most 64 KiB of
/// data per frame; without a bound, a length of up to 4 GiB was allocated
/// as read.
const MAX_FRAME: usize = 1 << 20;

fn header(kind: u8, id: u32, len: usize) -> [u8; 9] {
    let mut h = [0u8; 9];
    h[0] = kind;
    h[1..5].copy_from_slice(&id.to_be_bytes());
    h[5..].copy_from_slice(&(len as u32).to_be_bytes());
    h
}

fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, u32, Vec<u8>)>> {
    let mut h = [0u8; 9];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let id = u32::from_be_bytes([h[1], h[2], h[3], h[4]]);
    let len = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) as usize;
    // The length comes off the wire: refuse it before allocating it.
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes")));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok(Some((h[0], id, data)))
}

/// Appends to %LOCALAPPDATA%\Kiri\kiri-win.log: there is no console.
pub fn log(msg: &str) {
    let Some(dir) = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Kiri")) else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("kiri-win.log")) {
        // UTC to the millisecond, as the engine's /var/log/kiri.log writes
        // it, so a moment reads the same in both.
        let t = crate::daemon::time::rfc3339(std::time::SystemTime::now());
        let stamp = format!("{}Z", t.get(..23).unwrap_or(&t));
        // One write per line: separate writes from two threads interleave
        // in the file and Diagnostics then cannot parse the line.
        let _ = f.write_all(format!("{stamp} {msg}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    #[test]
    fn frames_round_trip() {
        let mut wire = super::header(super::DATA, 7, 3).to_vec();
        wire.extend_from_slice(b"abc");
        let mut r = &wire[..];
        assert_eq!(super::read_frame(&mut r).unwrap(), Some((super::DATA, 7, b"abc".to_vec())));
        assert_eq!(super::read_frame(&mut r).unwrap(), None);
    }

    /// A bridge that dies as it starts (WSL coming back after Windows
    /// slept) is started again for about BRIDGE_WAIT, not given up on
    /// after three tries within milliseconds (#93).
    #[test]
    fn a_dying_bridge_is_retried_for_a_bounded_while() {
        use std::time::Duration;
        assert!(!super::gave_up(Duration::from_millis(30)));
        assert!(!super::gave_up(super::BRIDGE_WAIT / 2));
        assert!(super::gave_up(super::BRIDGE_WAIT));
        // About twenty tries, not three.
        assert!(super::BRIDGE_WAIT.as_millis() / super::BRIDGE_RETRY.as_millis() >= 10);
    }

    use super::{header, read_frame, Answer, Bridge, Config, Hub, Limits, Process, ACK, DATA, EOF, PING};
    use crate::pipe::Pipe;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// What stands in for wsl.exe: ended by `kill`, which also closes the
    /// output it holds, as the real one's exit does.
    struct Stub {
        killed: Arc<AtomicBool>,
        output: Option<std::io::PipeWriter>,
    }

    impl Process for Stub {
        fn kill(&mut self) {
            self.killed.store(true, Ordering::SeqCst);
            self.output.take();
        }

        fn exited(&mut self) -> Option<String> {
            self.killed.load(Ordering::SeqCst).then(|| "killed".to_owned())
        }
    }

    /// A wsl.exe that runs and never says a word, hello included, as one
    /// can right after Windows wakes. Returns whether it was killed.
    fn silent() -> (Arc<Bridge>, Arc<AtomicBool>) {
        let (output, held) = std::io::pipe().unwrap();
        let killed = Arc::new(AtomicBool::new(false));
        let b = Bridge::over(Box::new(std::io::sink()), output, Box::new(Stub { killed: killed.clone(), output: Some(held) }));
        (b, killed)
    }

    /// A bridge that says hello, answers PING, and answers each request
    /// with `name`, until `frozen`: then it takes what it is sent and says
    /// nothing, as a wsl.exe that outlived its WSL.
    fn answering(name: &'static str, frozen: Arc<AtomicBool>) -> (Arc<Bridge>, Arc<AtomicBool>) {
        let (output, mut out) = std::io::pipe().unwrap();
        let (mut input, to_bridge) = std::io::pipe().unwrap();
        let killed = Arc::new(AtomicBool::new(false));
        let held = out.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut say = |kind, id, data: &[u8]| {
                let _ = out.write_all(&header(kind, id, data.len()));
                let _ = out.write_all(data);
            };
            say(ACK, 0, &[1]);
            while let Ok(Some((kind, id, _))) = read_frame(&mut input) {
                if frozen.load(Ordering::SeqCst) {
                    continue;
                }
                match kind {
                    PING => say(PING, 0, &[]),
                    DATA => {
                        say(DATA, id, format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{name}", name.len()).as_bytes());
                        say(EOF, id, &[]);
                    }
                    _ => {}
                }
            }
        });
        let b = Bridge::over(Box::new(to_bridge), output, Box::new(Stub { killed: killed.clone(), output: Some(held) }));
        (b, killed)
    }

    fn limits() -> Limits {
        Limits { hello: Duration::from_millis(300), quiet: Duration::from_millis(50), ping: Duration::from_millis(300), told_hold: Duration::from_secs(10), told_grace: Duration::from_millis(100) }
    }

    /// A hub whose bridges come from `next`, and how many it has started.
    fn hub(next: impl Fn(usize) -> Arc<Bridge> + Send + Sync + 'static) -> (Arc<Hub>, Arc<AtomicUsize>) {
        let started = Arc::new(AtomicUsize::new(0));
        let n = started.clone();
        let cfg = Config { distro: Some("test".into()), pipe: String::new(), socket: String::new(), kiri: String::new() };
        (Arc::new(Hub::new(cfg, Box::new(move |_| Ok(next(n.fetch_add(1, Ordering::SeqCst)))), limits())), started)
    }

    /// One docker client's request through `hub`, as over Kiri's pipe:
    /// what it read back, and how long that took. Panics after 20 s
    /// without an end, where the client once waited for good.
    fn request(hub: &Arc<Hub>) -> (String, Duration) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let name = format!(r"\\.\pipe\kiri-test-serve-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst));
        let server = Pipe::create(&name, true).unwrap();
        let h = hub.clone();
        std::thread::spawn(move || {
            server.connect().unwrap();
            super::serve(&h, server);
        });
        let began = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let client = Pipe::open(&name).unwrap();
            client.write(b"GET /_ping HTTP/1.1\r\nHost: kiri\r\n\r\n").unwrap();
            let mut reply = Vec::new();
            let _ = (&client).read_to_end(&mut reply);
            let _ = tx.send(String::from_utf8_lossy(&reply).into_owned());
        });
        let reply = rx.recv_timeout(Duration::from_secs(20)).expect("the docker client was left waiting on a bridge that never answers");
        (reply, began.elapsed())
    }

    /// A wsl.exe that hangs without a word (#93, after Windows wakes): the
    /// bridge is ended once its hello is overdue, and the client that
    /// waited on it is served by the next one.
    #[test]
    fn a_bridge_that_never_says_hello_is_replaced() {
        let first_killed = Arc::new(Mutex::new(None));
        let k = first_killed.clone();
        let (hub, started) = hub(move |n| {
            if n == 0 {
                let (b, killed) = silent();
                *k.lock().unwrap() = Some(killed);
                b
            } else {
                answering("second", Arc::default()).0
            }
        });
        let (reply, took) = request(&hub);
        assert!(reply.starts_with("HTTP/1.1 200 OK") && reply.ends_with("second"), "{reply}");
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert!(first_killed.lock().unwrap().as_ref().unwrap().load(Ordering::SeqCst), "the silent wsl.exe was left running");
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    /// A bridge that served, then stopped answering (its wsl.exe outlived
    /// the WSL it was connected to): the next connection asks it for a
    /// PING, ends it when none comes, and is served by a new one.
    #[test]
    fn a_bridge_that_stops_answering_is_replaced() {
        let frozen = Arc::new(AtomicBool::new(false));
        let first_killed = Arc::new(Mutex::new(None));
        let (f, k) = (frozen.clone(), first_killed.clone());
        let (hub, started) = hub(move |n| {
            if n == 0 {
                let (b, killed) = answering("first", f.clone());
                *k.lock().unwrap() = Some(killed);
                b
            } else {
                answering("second", Arc::default()).0
            }
        });
        assert!(request(&hub).0.ends_with("first"));
        // Quiet, and still answering: asked, and kept.
        std::thread::sleep(limits().quiet * 2);
        assert!(request(&hub).0.ends_with("first"));
        assert_eq!(started.load(Ordering::SeqCst), 1);
        frozen.store(true, Ordering::SeqCst);
        std::thread::sleep(limits().quiet * 2);
        let (reply, took) = request(&hub);
        assert!(reply.ends_with("second"), "{reply}");
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert!(first_killed.lock().unwrap().as_ref().unwrap().load(Ordering::SeqCst), "the frozen wsl.exe was left running");
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    /// WSL that answers nothing however often wsl.exe is started: the
    /// client is told so after two, rather than left waiting.
    #[test]
    fn a_client_is_told_when_no_bridge_answers() {
        let (hub, started) = hub(|_| silent().0);
        let (reply, took) = request(&hub);
        assert!(reply.starts_with("HTTP/1.1 503 ") && reply.contains("WSL is not answering"), "{reply}");
        assert!(reply.contains("kiri start"), "{reply}");
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert!(took < Duration::from_secs(8), "{took:?}");
        // docker's next request for the same command is told at once,
        // not made to wait as long again.
        let (again, took) = request(&hub);
        assert_eq!(again, reply);
        assert!(took < Duration::from_millis(250), "{took:?}");
    }

    /// The error does not outlast its cause: once WSL answers again, the
    /// next client is served, though one was just told it is not answering.
    #[test]
    fn a_client_is_served_as_soon_as_a_bridge_answers_again() {
        let back = Arc::new(AtomicBool::new(false));
        let b = back.clone();
        let (hub, _) = hub(move |_| if b.load(Ordering::SeqCst) { answering("back", Arc::default()).0 } else { silent().0 });
        assert!(request(&hub).0.contains("WSL is not answering"));
        // One started while it was not answering, as another client's
        // would be, and still there when WSL answers again.
        assert!(request(&hub).0.contains("WSL is not answering"));
        std::thread::sleep(limits().told_grace * 3);
        back.store(true, Ordering::SeqCst);
        let (reply, took) = request(&hub);
        assert!(reply.ends_with("back"), "{reply}");
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    /// The real wsl.exe is a child of the one kiri-win starts: found by
    /// its parent and name, so both are ended.
    #[test]
    fn a_process_s_children_are_found_by_name() {
        let mut cmd = crate::win::tool("cmd.exe");
        let mut child = cmd.args(["/c", "ping -n 30 127.0.0.1 >NUL"]).spawn().unwrap();
        let began = Instant::now();
        let found = loop {
            let found = super::children(child.id(), "PING.EXE");
            if !found.is_empty() || began.elapsed() > Duration::from_secs(5) {
                break found;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(found.len(), 1);
        assert!(super::children(child.id(), "wsl.exe").is_empty());
        super::terminate(found[0]);
        // Its only child gone, cmd.exe ends by itself.
        assert!(child.wait().is_ok());
        assert!(began.elapsed() < Duration::from_secs(10));
    }

    /// An engine from before PING (its hello has no data) is never asked
    /// for one, and is taken at its hello.
    #[test]
    fn a_bridge_from_before_ping_is_not_asked() {
        let (output, mut out) = std::io::pipe().unwrap();
        let b = Bridge::over(Box::new(std::io::sink()), output, Box::new(Stub { killed: Arc::default(), output: None }));
        out.write_all(&header(ACK, 0, 0)).unwrap();
        std::thread::sleep(limits().quiet * 3);
        assert_eq!(b.answers(&limits(), || false), Answer::Yes);
    }

    /// The engine in WSL was replaced: the bridge run from the old one is
    /// out of use for new connections, and ended as nothing is on it.
    #[test]
    fn a_retired_bridge_is_ended_and_replaced() {
        let killed = Arc::new(Mutex::new(Vec::new()));
        let k = killed.clone();
        let (hub, started) = hub(move |_| {
            let (b, was) = answering("any", Arc::default());
            k.lock().unwrap().push(was);
            b
        });
        assert!(request(&hub).0.ends_with("any"));
        assert!(hub.retire());
        // At once, or as the connection just served leaves it.
        let began = Instant::now();
        while !killed.lock().unwrap()[0].load(Ordering::SeqCst) {
            assert!(began.elapsed() < Duration::from_secs(5), "the retired bridge was left running");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(request(&hub).0.ends_with("any"));
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert!(!killed.lock().unwrap()[1].load(Ordering::SeqCst));
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut wire = super::header(super::DATA, 7, super::MAX_FRAME).to_vec();
        wire.resize(wire.len() + super::MAX_FRAME, 0);
        assert_eq!(super::read_frame(&mut &wire[..]).unwrap().map(|f| f.2.len()), Some(super::MAX_FRAME));
        // 4 GiB claimed, nothing behind it: an error at once, not an allocation.
        let huge = super::header(super::DATA, 7, u32::MAX as usize);
        assert_eq!(super::read_frame(&mut &huge[..]).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn engines_are_known_by_file_name() {
        let super::ProcessInfo::Known { engine, created } = super::inspect_process(std::process::id()) else {
            panic!("could not inspect current process");
        };
        assert_eq!(engine, None);
        assert!(created.is_some());
        assert_eq!(super::engine_named("docker desktop.EXE"), Some("Docker Desktop.exe"));
        assert_eq!(super::engine_named("docker.exe"), None);
        // This test's process is not new to a second look.
        let mut new = super::NewEngines::default();
        new.find();
        assert!(new.seen.contains_key(&std::process::id()));
    }

    /// Another engine can create the pipe as its first instance soon after
    /// it starts, though a client of Kiri's is connected and never closes.
    #[test]
    fn hands_the_pipe_back_with_a_client_connected() {
        let name = format!(r"\\.\pipe\kiri-test-handover-{}", std::process::id());
        let first = Pipe::create(&name, true).unwrap();
        let other = Arc::new(AtomicBool::new(false));
        let (n, o) = (name.clone(), other.clone());
        // Served as serve_shared does: reading until the pipe ends.
        let server = thread::spawn(move || {
            super::serve_while_free(
                &n,
                first,
                move || o.load(Ordering::SeqCst).then_some("test engine"),
                |p: Arc<Pipe>| {
                    let mut buf = [0u8; 64];
                    while p.read(&mut buf).is_ok_and(|n| n > 0) {}
                },
            )
        });
        let client = Pipe::open(&name).unwrap();
        client.write(b"GET /events").unwrap();
        thread::sleep(Duration::from_millis(300));
        other.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let theirs = loop {
            match Pipe::create(&name, true) {
                Ok(p) => break p,
                Err(e) => {
                    assert!(started.elapsed() < Duration::from_secs(1), "{name} still in use 1 s after the handover: {e}");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        server.join().unwrap();
        drop((theirs, client));
    }

    /// With no client connected, the handover completes well before another
    /// engine creates its first pipe instance.
    #[test]
    fn hands_the_pipe_back_promptly_with_no_client() {
        let name = format!(r"\\.\pipe\kiri-test-handover-idle-{}", std::process::id());
        let first = Pipe::create(&name, true).unwrap();
        let other = Arc::new(AtomicBool::new(false));
        let (n, o) = (name.clone(), other.clone());
        let server = thread::spawn(move || {
            super::serve_while_free(
                &n,
                first,
                move || o.load(Ordering::SeqCst).then_some("test engine"),
                |_p: Arc<Pipe>| {},
            )
        });
        // Past the first watcher check, so we measure the polling interval.
        thread::sleep(Duration::from_millis(200));
        other.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let theirs = loop {
            match Pipe::create(&name, true) {
                Ok(p) => break p,
                Err(e) => {
                    assert!(started.elapsed() < Duration::from_millis(500), "{name} still in use 500 ms after the handover: {e}");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        server.join().unwrap();
        drop(theirs);
    }

    /// Real `serve_shared` with a streaming client (`docker events`-style):
    /// even when the server has written unread bytes to the pipe and the
    /// bridge never answers CLOSE, handover disconnects and closes the
    /// instance without blocking on FlushFileBuffers or the bridge.
    #[test]
    fn hands_the_pipe_back_with_a_streaming_client_and_unread_reply() {
        let (output, mut out) = std::io::pipe().unwrap();
        let (mut input, to_bridge) = std::io::pipe().unwrap();
        let killed = Arc::new(AtomicBool::new(false));
        let held = out.try_clone().unwrap();
        let replied = Arc::new(AtomicBool::new(false));
        let rep = replied.clone();
        thread::spawn(move || {
            let mut say = |kind, id, data: &[u8]| {
                let _ = out.write_all(&header(kind, id, data.len()));
                let _ = out.write_all(data);
            };
            say(ACK, 0, &[1]);
            while let Ok(Some((kind, id, _))) = read_frame(&mut input) {
                match kind {
                    PING => say(PING, 0, &[]),
                    // Reply with headers and stay silent forever (ignoring CLOSE).
                    DATA => {
                        say(DATA, id, b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n");
                        rep.store(true, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        });
        let b = Bridge::over(Box::new(to_bridge), output, Box::new(Stub { killed, output: Some(held) }));
        let (hub, _) = hub(move |_| b.clone());
        let name = format!(r"\\.\pipe\kiri-test-handover-stream-{}", std::process::id());
        let first = Pipe::create(&name, true).unwrap();
        let other = Arc::new(AtomicBool::new(false));
        let (h, n, o) = (hub.clone(), name.clone(), other.clone());
        let server = thread::spawn(move || {
            super::serve_while_free(
                &n,
                first,
                move || o.load(Ordering::SeqCst).then_some("test engine"),
                move |p| super::serve_shared(&h, p),
            )
        });
        let client = Pipe::open(&name).unwrap();
        client.write(b"GET /events HTTP/1.1\r\nHost: kiri\r\n\r\n").unwrap();
        // Wait until the bridge wrote the reply (buffered in the pipe, unread by client).
        let wait = Instant::now();
        while !replied.load(Ordering::SeqCst) {
            assert!(wait.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(100));
        other.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let theirs = loop {
            match Pipe::create(&name, true) {
                Ok(p) => break p,
                Err(e) => {
                    assert!(started.elapsed() < Duration::from_secs(1), "{name} still in use 1 s after the handover: {e}");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        server.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "handover took {:?}", started.elapsed());
        drop((theirs, client));
    }

    /// A client connected while a bridge has not yet said hello still lets
    /// go of the pipe on handover, without waiting for HELLO_WAIT.
    #[test]
    fn hands_the_pipe_back_while_client_waits_for_bridge() {
        let cfg = Config { distro: Some("test".into()), pipe: String::new(), socket: String::new(), kiri: String::new() };
        let slow = Limits { hello: Duration::from_secs(10), ..limits() };
        let hub = Arc::new(Hub::new(cfg, Box::new(|_| Ok(silent().0)), slow));
        let name = format!(r"\\.\pipe\kiri-test-handover-wait-{}", std::process::id());
        let first = Pipe::create(&name, true).unwrap();
        let other = Arc::new(AtomicBool::new(false));
        let (h, n, o) = (hub.clone(), name.clone(), other.clone());
        let server = thread::spawn(move || {
            super::serve_while_free(
                &n,
                first,
                move || o.load(Ordering::SeqCst).then_some("test engine"),
                move |p| super::serve_shared(&h, p),
            )
        });
        let client = Pipe::open(&name).unwrap();
        thread::sleep(Duration::from_millis(200));
        other.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let theirs = loop {
            match Pipe::create(&name, true) {
                Ok(p) => break p,
                Err(e) => {
                    assert!(started.elapsed() < Duration::from_secs(1), "{name} still in use 1 s after the handover: {e}");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        server.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "handover waited on bridge: {:?}", started.elapsed());
        drop((theirs, client));
    }

    /// A half-closed streaming client (write end closed via empty message,
    /// e.g. `docker logs -f` or `docker run -i < NUL`) still releases the pipe
    /// promptly on handover and notifies the bridge with CLOSE.
    #[test]
    fn hands_the_pipe_back_with_a_half_closed_streaming_client() {
        let (output, mut out) = std::io::pipe().unwrap();
        let (mut input, to_bridge) = std::io::pipe().unwrap();
        let killed = Arc::new(AtomicBool::new(false));
        let held = out.try_clone().unwrap();
        let saw_eof = Arc::new(AtomicBool::new(false));
        let saw_close = Arc::new(AtomicBool::new(false));
        let (eof_flag, close_flag) = (saw_eof.clone(), saw_close.clone());
        thread::spawn(move || {
            let mut say = |kind, id, data: &[u8]| {
                let _ = out.write_all(&header(kind, id, data.len()));
                let _ = out.write_all(data);
            };
            say(ACK, 0, &[1]);
            while let Ok(Some((kind, id, _))) = read_frame(&mut input) {
                match kind {
                    PING => say(PING, 0, &[]),
                    DATA => {
                        say(DATA, id, b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n");
                    }
                    EOF => {
                        eof_flag.store(true, Ordering::SeqCst);
                    }
                    super::CLOSE => {
                        close_flag.store(true, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        });
        let b = Bridge::over(Box::new(to_bridge), output, Box::new(Stub { killed, output: Some(held) }));
        let (hub, _) = hub(move |_| b.clone());
        let name = format!(r"\\.\pipe\kiri-test-handover-halfclose-{}", std::process::id());
        let first = Pipe::create(&name, true).unwrap();
        let other = Arc::new(AtomicBool::new(false));
        let (h, n, o) = (hub.clone(), name.clone(), other.clone());
        let server = thread::spawn(move || {
            super::serve_while_free(
                &n,
                first,
                move || o.load(Ordering::SeqCst).then_some("test engine"),
                move |p| super::serve_shared(&h, p),
            )
        });
        let client = Pipe::open(&name).unwrap();
        client.write(b"GET /logs?follow=1 HTTP/1.1\r\nHost: kiri\r\n\r\n").unwrap();
        // Half-close client write end (go-winio CloseWrite): reader exits with EOF.
        client.write(&[]).unwrap();
        let wait = Instant::now();
        while !saw_eof.load(Ordering::SeqCst) {
            assert!(wait.elapsed() < Duration::from_secs(5), "bridge did not see client EOF");
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(100));
        other.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let theirs = loop {
            match Pipe::create(&name, true) {
                Ok(p) => break p,
                Err(e) => {
                    assert!(started.elapsed() < Duration::from_secs(1), "{name} still in use 1 s after handover: {e}");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        server.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "handover took {:?}", started.elapsed());
        let wait_close = Instant::now();
        while !saw_close.load(Ordering::SeqCst) {
            assert!(wait_close.elapsed() < Duration::from_secs(2), "bridge did not receive CLOSE for aborted half-closed client");
            thread::sleep(Duration::from_millis(10));
        }
        drop((theirs, client));
    }

    /// A writer whose writes block whenever `gate` is set, standing in for a
    /// bridge whose stdin pipe buffer is full.
    struct BlockingWriter {
        inner: std::io::PipeWriter,
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, cv) = &*self.gate;
            let mut blocked = lock.lock().unwrap();
            while *blocked {
                blocked = cv.wait(blocked).unwrap();
            }
            drop(blocked);
            self.inner.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    /// Handover releases the pipe and ActiveGuard without waiting on a bridge
    /// whose stdin is blocked.
    #[test]
    fn hands_the_pipe_back_when_bridge_input_is_blocked() {
        let (output, mut out) = std::io::pipe().unwrap();
        let (mut input, to_bridge) = std::io::pipe().unwrap();
        let killed = Arc::new(AtomicBool::new(false));
        let held = out.try_clone().unwrap();
        let saw_data = Arc::new(AtomicBool::new(false));
        let data_flag = saw_data.clone();
        thread::spawn(move || {
            let mut say = |kind, id, data: &[u8]| {
                let _ = out.write_all(&header(kind, id, data.len()));
                let _ = out.write_all(data);
            };
            say(ACK, 0, &[1]);
            while let Ok(Some((kind, id, _))) = read_frame(&mut input) {
                match kind {
                    PING => say(PING, 0, &[]),
                    DATA => {
                        say(DATA, id, b"HTTP/1.1 200 OK\r\n\r\n");
                        data_flag.store(true, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        });
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let writer = BlockingWriter { inner: to_bridge, gate: gate.clone() };
        let b = Bridge::over(Box::new(writer), output, Box::new(Stub { killed, output: Some(held) }));
        let (hub, _) = hub(move |_| b.clone());
        let name = format!(r"\\.\pipe\kiri-test-handover-blocked-bridge-{}", std::process::id());
        let first = Pipe::create(&name, true).unwrap();
        let other = Arc::new(AtomicBool::new(false));
        let (h, n, o) = (hub.clone(), name.clone(), other.clone());
        let server = thread::spawn(move || {
            super::serve_while_free(
                &n,
                first,
                move || o.load(Ordering::SeqCst).then_some("test engine"),
                move |p| super::serve_shared(&h, p),
            )
        });
        let client = Pipe::open(&name).unwrap();
        client.write(b"GET /events HTTP/1.1\r\nHost: kiri\r\n\r\n").unwrap();
        let wait = Instant::now();
        while !saw_data.load(Ordering::SeqCst) {
            assert!(wait.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(10));
        }
        // Freeze bridge writes before handover triggers CLOSE.
        *gate.0.lock().unwrap() = true;
        other.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let theirs = loop {
            match Pipe::create(&name, true) {
                Ok(p) => break p,
                Err(e) => {
                    assert!(started.elapsed() < Duration::from_secs(1), "{name} still in use 1 s after handover with blocked bridge: {e}");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        server.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "handover waited on blocked bridge: {:?}", started.elapsed());
        // Release the blocked writer so the detached CLOSE sender can finish.
        *gate.0.lock().unwrap() = false;
        gate.1.notify_all();
        drop((theirs, client));
    }

    /// Dropping a server `Pipe` does not block in `FlushFileBuffers` when a
    /// client has not read what was written.
    #[test]
    fn pipe_drop_does_not_block_on_unread_data() {
        let name = format!(r"\\.\pipe\kiri-test-drop-nonblock-{}", std::process::id());
        let server = Pipe::create(&name, true).unwrap();
        let client = Pipe::open(&name).unwrap();
        server.connect().unwrap();
        server.write(b"unread payload").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop(server);
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(1)).expect("Pipe::drop blocked waiting for unread data");
        drop(client);
    }

    /// When a client handler has finished its reply and is flushing unread data
    /// to a stalled client, handover can still reach the `Arc<Pipe>` and cancel
    /// the in-flight flush.
    #[test]
    fn hands_the_pipe_back_while_flushing_unread_reply() {
        let (output, mut out) = std::io::pipe().unwrap();
        let (mut input, to_bridge) = std::io::pipe().unwrap();
        let killed = Arc::new(AtomicBool::new(false));
        let held = out.try_clone().unwrap();
        let sent_close = Arc::new(AtomicBool::new(false));
        let sc = sent_close.clone();
        thread::spawn(move || {
            let mut say = |kind, id, data: &[u8]| {
                let _ = out.write_all(&header(kind, id, data.len()));
                let _ = out.write_all(data);
            };
            say(ACK, 0, &[1]);
            while let Ok(Some((kind, id, _))) = read_frame(&mut input) {
                match kind {
                    PING => say(PING, 0, &[]),
                    DATA => {
                        // Send reply followed by CLOSE (not EOF): writer exits
                        // immediately and handler enters FlushOnDrop while data
                        // is still unread in the pipe buffer.
                        say(DATA, id, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                        say(super::CLOSE, id, &[]);
                        sc.store(true, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        });
        let b = Bridge::over(Box::new(to_bridge), output, Box::new(Stub { killed, output: Some(held) }));
        let (hub, _) = hub(move |_| b.clone());
        let name = format!(r"\\.\pipe\kiri-test-handover-flush-{}", std::process::id());
        let first = Pipe::create(&name, true).unwrap();
        let other = Arc::new(AtomicBool::new(false));
        let (h, n, o) = (hub.clone(), name.clone(), other.clone());
        let server = thread::spawn(move || {
            super::serve_while_free(
                &n,
                first,
                move || o.load(Ordering::SeqCst).then_some("test engine"),
                move |p| super::serve_shared(&h, p),
            )
        });
        let client = Pipe::open(&name).unwrap();
        client.write(b"GET /info HTTP/1.1\r\nHost: kiri\r\n\r\n").unwrap();
        let wait = Instant::now();
        while !sent_close.load(Ordering::SeqCst) {
            assert!(wait.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(10));
        }
        // Let writer exit and serve_shared block inside FlushOnDrop::drop.
        thread::sleep(Duration::from_millis(200));
        other.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let theirs = loop {
            match Pipe::create(&name, true) {
                Ok(p) => break p,
                Err(e) => {
                    assert!(started.elapsed() < Duration::from_secs(1), "{name} still in use 1 s after handover during flush: {e}");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        server.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "handover stalled in flush: {:?}", started.elapsed());
        drop((theirs, client));
    }

    /// Without a handover, `FlushOnDrop` still flushes a reply followed by
    /// CLOSE (no EOF) so a slightly slow client receives the full body.
    #[test]
    fn normal_reply_without_eof_flushes_before_close() {
        let (output, mut out) = std::io::pipe().unwrap();
        let (mut input, to_bridge) = std::io::pipe().unwrap();
        let killed = Arc::new(AtomicBool::new(false));
        let held = out.try_clone().unwrap();
        thread::spawn(move || {
            let mut say = |kind, id, data: &[u8]| {
                let _ = out.write_all(&header(kind, id, data.len()));
                let _ = out.write_all(data);
            };
            say(ACK, 0, &[1]);
            while let Ok(Some((kind, id, _))) = read_frame(&mut input) {
                if kind == DATA {
                    say(DATA, id, b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello");
                    say(super::CLOSE, id, &[]);
                }
            }
        });
        let b = Bridge::over(Box::new(to_bridge), output, Box::new(Stub { killed, output: Some(held) }));
        let (hub, _) = hub(move |_| b.clone());
        let name = format!(r"\\.\pipe\kiri-test-flush-deliver-{}", std::process::id());
        let server = Pipe::create(&name, true).unwrap();
        let h = hub.clone();
        let srv = thread::spawn(move || {
            server.connect().unwrap();
            super::serve(&h, server);
        });
        let client = Pipe::open(&name).unwrap();
        client.write(b"GET /info HTTP/1.1\r\nHost: kiri\r\n\r\n").unwrap();
        // Pause so the server finishes writing and enters FlushOnDrop before we read.
        thread::sleep(Duration::from_millis(100));
        let mut reply = Vec::new();
        let _ = (&client).read_to_end(&mut reply);
        srv.join().unwrap();
        assert_eq!(String::from_utf8_lossy(&reply), "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello");
    }

    struct MockProbe {
        pids: Vec<u32>,
        fail_list: bool,
        list_calls: usize,
        inspect: Box<dyn FnMut(u32) -> super::ProcessInfo>,
        created: Box<dyn FnMut(u32) -> super::Created>,
        denied_calls: Vec<Vec<u32>>,
        denied_result: Option<&'static str>,
        service_calls: usize,
        service_result: Option<&'static str>,
        snapshot_calls: usize,
        snapshot_result: Option<&'static str>,
    }

    impl MockProbe {
        fn new(pids: Vec<u32>) -> Self {
            Self {
                pids,
                fail_list: false,
                list_calls: 0,
                inspect: Box::new(|_| super::ProcessInfo::Known { engine: None, created: Some(1) }),
                created: Box::new(|p| super::Created::At(p as u64)),
                denied_calls: Vec::new(),
                denied_result: None,
                service_calls: 0,
                service_result: None,
                snapshot_calls: 0,
                snapshot_result: None,
            }
        }
    }

    impl super::EngineProbe for MockProbe {
        fn list_pids(&mut self, buf: &mut [u32]) -> Option<usize> {
            self.list_calls += 1;
            if self.fail_list {
                return None;
            }
            let n = self.pids.len().min(buf.len());
            buf[..n].copy_from_slice(&self.pids[..n]);
            Some(n)
        }

        fn inspect(&mut self, pid: u32) -> super::ProcessInfo {
            (self.inspect)(pid)
        }

        fn created(&mut self, pid: u32) -> super::Created {
            (self.created)(pid)
        }

        fn check_denied(&mut self, pids: &[u32]) -> Option<&'static str> {
            self.denied_calls.push(pids.to_vec());
            self.denied_result
        }

        fn check_services(&mut self) -> Option<&'static str> {
            self.service_calls += 1;
            self.service_result
        }

        fn snapshot(&mut self) -> Option<&'static str> {
            self.snapshot_calls += 1;
            self.snapshot_result
        }
    }

    /// Engine matching requires a Program Files install path and a trusted
    /// token owner/integrity level (and trusted session/owner for WTS and SCM).
    #[test]
    fn engine_detection_requires_program_files_and_trusted_owner() {
        let roots = vec![r"C:\Program Files".to_owned(), r"C:\Program Files (x86)".to_owned()];
        // Trusted Program Files paths match.
        assert_eq!(super::engine_at_path(r"C:\Program Files\Docker\Docker\Docker Desktop.exe", &roots), Some("Docker Desktop.exe"));
        assert_eq!(super::engine_at_path(r"c:\program files\docker\dockerd.exe", &roots), Some("dockerd.exe"));
        assert_eq!(super::engine_at_path(r"\\?\C:\Program Files\Docker\Docker\resources\com.docker.backend.exe", &roots), Some("com.docker.backend.exe"));
        assert_eq!(super::engine_at_path(r"C:\Program Files (x86)\Rancher Desktop\Rancher Desktop.exe", &roots), Some("Rancher Desktop.exe"));
        // Non-Program-Files, prefix-lookalike, traversal, UNC, and non-engine paths do not match.
        assert_eq!(super::engine_at_path(r"C:\Users\other\dockerd.exe", &roots), None);
        assert_eq!(super::engine_at_path(r"C:\Temp\Docker Desktop.exe", &roots), None);
        assert_eq!(super::engine_at_path(r"C:\Program Files Evil\Docker Desktop.exe", &roots), None);
        assert_eq!(super::engine_at_path(r"C:\Program Files\..\Users\other\dockerd.exe", &roots), None);
        assert_eq!(super::engine_at_path(r"\\server\share\Program Files\Docker\dockerd.exe", &roots), None);
        assert_eq!(super::engine_at_path(r"\\?\UNC\server\share\Program Files\Docker\dockerd.exe", &roots), None);
        assert_eq!(super::engine_at_path(r"C:\Program Files\Docker\docker.exe", &roots), None);

        // Token trust: current user, SYSTEM, or elevated Administrators at >= Medium IL.
        let me = Some("S-1-5-21-1-2-3-1000");
        let other = "S-1-5-21-1-2-3-1001";
        assert!(super::is_trusted_token("S-1-5-21-1-2-3-1000", Some("S-1-5-21-1-2-3-1000"), 0x2000, me));
        assert!(super::is_trusted_token("S-1-5-21-1-2-3-1000", Some(super::ADMINS_SID), 0x3000, me));
        assert!(super::is_trusted_token(super::SYSTEM_SID, Some(super::SYSTEM_SID), 0x4000, me));
        assert!(super::is_trusted_token(other, Some(super::ADMINS_SID), 0x3000, me));
        assert!(!super::is_trusted_token(other, Some(other), 0x2000, me));
        assert!(!super::is_trusted_token("S-1-5-21-1-2-3-1000", Some("S-1-5-21-1-2-3-1000"), 0x1000, me));
        assert!(!super::is_trusted_token("S-1-5-21-1-2-3-1000", Some("S-1-5-21-1-2-3-1000"), 0x0000, me));
        assert!(!super::is_trusted_token("S-1-5-19", Some("S-1-5-19"), 0x4000, me));

        // WTS owner/session trust: only SYSTEM in Session 0.
        assert!(super::is_trusted_wts_owner(super::SYSTEM_SID, 0));
        assert!(!super::is_trusted_wts_owner(super::SYSTEM_SID, 1));
        assert!(!super::is_trusted_wts_owner("S-1-5-21-1-2-3-1000", 0));
        assert!(!super::is_trusted_wts_owner("S-1-5-21-1-2-3-1000", 1));
        assert!(!super::is_trusted_wts_owner(other, 1));
        assert!(!super::is_trusted_wts_owner(other, 2));

        // SCM service command-line and account parsing.
        assert_eq!(super::service_binary_path(r#""C:\Program Files\Docker\dockerd.exe" --run-service"#), Some(r"C:\Program Files\Docker\dockerd.exe"));
        assert_eq!(super::service_binary_path(r"C:\Program Files\Docker\dockerd.exe --run-service"), Some(r"C:\Program Files\Docker\dockerd.exe"));
        assert_eq!(super::service_binary_path(r"C:\Program Files\Other\wrap.exe C:\Program Files\Docker\dockerd.exe"), Some(r"C:\Program Files\Other\wrap.exe"));
        assert_eq!(super::service_binary_path(r#""C:\Program Files\Docker\dockerd.exe"#), None);
        assert!(super::is_system_service_account("LocalSystem"));
        assert!(super::is_system_service_account(r"NT AUTHORITY\SYSTEM"));
        assert!(!super::is_system_service_account(r".\otheruser"));

        // Live Windows queries succeed in this process.
        assert!(!super::program_files_dirs().is_empty());
        assert!(super::current_user_sid_cached().is_some());
    }

    /// Unreadable same-user processes and non-SYSTEM processes with denied
    /// token access never match through WTS filename fallback.
    #[test]
    fn unreadable_user_process_and_denied_token_do_not_match() {
        let me = super::current_user_sid_cached().expect("current user SID");
        assert!(!super::is_trusted_wts_owner(me, 0));
        assert!(!super::is_trusted_wts_owner(me, 1));
        assert!(!super::is_trusted_wts_owner(super::ADMINS_SID, 0));
        assert!(!super::is_trusted_wts_owner("S-1-5-19", 0));
        assert!(!super::is_trusted_wts_owner("S-1-5-20", 0));
        assert!(super::is_trusted_wts_owner(super::SYSTEM_SID, 0));
        assert!(!super::is_trusted_wts_owner(super::SYSTEM_SID, 1));
        assert!(!super::is_wts_system_session0(std::process::id()));

        // TokenTrust::Denied for a non-SYSTEM process is treated as untrusted
        // rather than ProcessInfo::Denied (no routing to WTS fallback).
        assert_eq!(
            super::resolve_engine_token("Docker Desktop.exe", Some(42), super::TokenTrust::Denied, || false),
            super::ProcessInfo::Known { engine: None, created: Some(42) },
        );
        assert_eq!(
            super::resolve_engine_token("dockerd.exe", Some(42), super::TokenTrust::Denied, || true),
            super::ProcessInfo::Known { engine: Some("dockerd.exe"), created: Some(42) },
        );
        assert_eq!(
            super::resolve_engine_token("Docker Desktop.exe", Some(42), super::TokenTrust::Trusted, || unreachable!()),
            super::ProcessInfo::Known { engine: Some("Docker Desktop.exe"), created: Some(42) },
        );
        assert_eq!(
            super::resolve_engine_token("Docker Desktop.exe", Some(42), super::TokenTrust::Untrusted, || unreachable!()),
            super::ProcessInfo::Known { engine: None, created: Some(42) },
        );
        assert_eq!(
            super::resolve_engine_token("Docker Desktop.exe", Some(42), super::TokenTrust::Gone, || unreachable!()),
            super::ProcessInfo::Gone,
        );

        // NewEngines does not route a non-SYSTEM denied-token process to check_denied.
        let mut new = super::NewEngines::default();
        let mut probe = MockProbe::new(vec![10]);
        assert_eq!(new.find_with(&mut probe), None);
        probe.pids = vec![10, 77];
        probe.inspect = Box::new(|p| {
            assert_eq!(p, 77);
            super::resolve_engine_token("Docker Desktop.exe", Some(770), super::TokenTrust::Denied, || false)
        });
        probe.denied_result = Some("Docker Desktop.exe");
        assert_eq!(new.find_with(&mut probe), None);
        assert!(probe.denied_calls.is_empty());
        assert_eq!(new.seen.get(&77), Some(&Some(770)));
    }

    /// Unreadable new PIDs are checked per PID via `check_denied` (owner check)
    /// rather than triggering a global name-only snapshot.
    #[test]
    fn denied_fallback_checks_only_denied_pid_owner() {
        let mut new = super::NewEngines::default();
        let mut probe = MockProbe::new(vec![10, 20]);
        // Initial scan seeds `seen` and takes one initial snapshot.
        assert_eq!(new.find_with(&mut probe), None);
        assert_eq!(probe.snapshot_calls, 1);

        // Unchanged PIDs on fast tick do not inspect or take snapshots.
        probe.inspect = Box::new(|_| unreachable!());
        assert_eq!(new.find_with(&mut probe), None);
        assert_eq!(probe.snapshot_calls, 1);
        assert!(probe.denied_calls.is_empty());

        // An exited new PID does not trigger denied checks or snapshots.
        probe.pids = vec![10, 20, 30];
        probe.inspect = Box::new(|_| super::ProcessInfo::Gone);
        assert_eq!(new.find_with(&mut probe), None);
        assert_eq!(probe.snapshot_calls, 1);
        assert!(probe.denied_calls.is_empty());

        // A new unreadable PID whose owner is not trusted does not hand back.
        probe.pids = vec![10, 20, 50];
        probe.created = Box::new(|p| if p >= 50 { super::Created::Denied } else { super::Created::At(p as u64) });
        probe.inspect = Box::new(|p| if p == 50 { super::ProcessInfo::Denied } else { unreachable!() });
        probe.denied_result = None;
        assert_eq!(new.find_with(&mut probe), None);
        assert_eq!(probe.denied_calls, vec![vec![50]]);
        assert_eq!(probe.snapshot_calls, 1);

        // A new unreadable PID whose owner is verified (e.g. SYSTEM dockerd.exe) matches.
        probe.pids = vec![10, 20, 50, 60];
        probe.inspect = Box::new(|p| if p == 60 { super::ProcessInfo::Denied } else { unreachable!() });
        probe.denied_result = Some("dockerd.exe");
        assert_eq!(new.find_with(&mut probe), Some("dockerd.exe"));
        assert_eq!(probe.denied_calls, vec![vec![50], vec![60]]);
        assert_eq!(probe.snapshot_calls, 1);
    }

    /// When running processes exceed the initial 4096-entry PID buffer,
    /// `NewEngines` grows the buffer up to `MAX_PIDS` and retries instead of
    /// permanently falling back to `other_engine()`.
    #[test]
    fn new_engines_grows_pid_buffer_before_falling_back() {
        let mut new = super::NewEngines::default();
        let pids: Vec<u32> = (1..=5000).collect();
        let mut probe = MockProbe::new(pids.clone());
        assert_eq!(new.find_with(&mut probe), None);
        // 4096 filled on first call, grew to 8192 and succeeded on second call.
        assert_eq!(probe.list_calls, 2);
        assert_eq!(new.pids.len(), 8192);
        assert_eq!(new.seen.len(), 5000);
        assert_eq!(probe.snapshot_calls, 1);

        // Next tick reuses the 8192 buffer in a single call and detects a new PID.
        probe.pids.push(5001);
        probe.inspect = Box::new(|p| {
            assert_eq!(p, 5001);
            super::ProcessInfo::Known { engine: Some("Docker Desktop.exe"), created: Some(99) }
        });
        assert_eq!(new.find_with(&mut probe), Some("Docker Desktop.exe"));
        assert_eq!(probe.list_calls, 3);
        assert_eq!(probe.snapshot_calls, 1);

        // Filling even MAX_PIDS cleanly falls back to snapshot.
        let mut buf = Vec::new();
        let mut calls = 0usize;
        let res = super::enum_pids_with(&mut buf, |b| {
            calls += 1;
            Some(b.len())
        });
        assert_eq!(res, None);
        assert_eq!(buf.len(), super::MAX_PIDS);
        assert_eq!(calls, 5); // 4096 -> 8192 -> 16384 -> 32768 -> 65536
    }

    /// A new engine process that reuses an already-seen PID is detected within
    /// `REUSE_CHECK_EVERY` ticks (well inside Docker Desktop's ~0.4 s window).
    #[test]
    fn new_engines_detects_reused_pid_within_handover_window() {
        assert!(super::ENGINE_CHECK * u32::from(super::REUSE_CHECK_EVERY) <= Duration::from_millis(300));

        // Case 1: readable PID 10 reused with a newer creation time.
        let mut new = super::NewEngines::default();
        let mut probe = MockProbe::new(vec![10, 20]);
        probe.created = Box::new(|p| super::Created::At(100 + p as u64));
        assert_eq!(new.find_with(&mut probe), None);

        // PID 10 exits and Docker Desktop reuses PID 10 in the same tick.
        probe.created = Box::new(|p| if p == 10 { super::Created::At(999) } else { super::Created::At(100 + p as u64) });
        probe.inspect = Box::new(|p| {
            if p == 10 {
                super::ProcessInfo::Known { engine: Some("Docker Desktop.exe"), created: Some(999) }
            } else {
                super::ProcessInfo::Known { engine: None, created: Some(100 + p as u64) }
            }
        });
        let mut found = None;
        for _ in 0..super::REUSE_CHECK_EVERY {
            found = new.find_with(&mut probe);
            if found.is_some() {
                break;
            }
        }
        assert_eq!(found, Some("Docker Desktop.exe"));

        // Case 2: readable PID 20 reused by an unreadable SYSTEM engine service.
        let mut new2 = super::NewEngines::default();
        let mut probe2 = MockProbe::new(vec![10, 20]);
        probe2.created = Box::new(|p| super::Created::At(100 + p as u64));
        assert_eq!(new2.find_with(&mut probe2), None);

        probe2.created = Box::new(|p| if p == 20 { super::Created::Denied } else { super::Created::At(100 + p as u64) });
        probe2.denied_result = Some("dockerd.exe");
        let mut found2 = None;
        for _ in 0..super::REUSE_CHECK_EVERY {
            found2 = new2.find_with(&mut probe2);
            if found2.is_some() {
                break;
            }
        }
        assert_eq!(found2, Some("dockerd.exe"));
        assert_eq!(probe2.denied_calls, vec![vec![20]]);

        // Case 3: unreadable PID 30 reused by an active SYSTEM service detected via check_services.
        let mut new3 = super::NewEngines::default();
        let mut probe3 = MockProbe::new(vec![30]);
        probe3.created = Box::new(|_| super::Created::Denied);
        assert_eq!(new3.find_with(&mut probe3), None);
        probe3.service_result = Some("dockerd.exe");
        let mut found3 = None;
        for _ in 0..super::REUSE_CHECK_EVERY {
            found3 = new3.find_with(&mut probe3);
            if found3.is_some() {
                break;
            }
        }
        assert_eq!(found3, Some("dockerd.exe"));
        assert_eq!(probe3.service_calls, 1);
    }

    /// When a new service starts between the initial `EnumServicesStatusExW`
    /// size query and the fill call, `pcbBytesNeeded` reports only the
    /// remaining bytes that did not fit; retrying must grow the buffer by
    /// `cap + remaining` rather than shrinking it to the remainder.
    #[test]
    fn enum_services_grows_buffer_on_more_data() {
        // Initial query reports 4096 bytes; a new service starts before the
        // first fill so total active services now need 4352 bytes (17 entries
        // of 256 bytes). On a partial fill, EnumServicesStatusExW sets
        // pcbBytesNeeded to only the unwritten remainder (total - written).
        let entry_size = 256u32;
        let mut caps = Vec::new();
        let mut total = 4096u32;
        let res = super::enum_services_with(|_buf, cap| {
            caps.push(cap);
            if cap == 0 {
                let initial = total;
                total = 4352;
                return super::ServiceEnumStep::MoreData { remaining: initial };
            }
            if cap >= total {
                return super::ServiceEnumStep::Done(total / entry_size);
            }
            let written = (cap / entry_size) * entry_size;
            super::ServiceEnumStep::MoreData { remaining: total - written }
        });
        let (buf, count) = res.expect("enumeration should succeed after growing buffer");
        assert_eq!(count, 17);
        assert!(buf.len() * std::mem::size_of::<usize>() >= 4352);
        assert_eq!(caps, vec![0, 4096, 4352]);

        // Sequential growth across two mid-enumeration service starts also converges.
        let mut caps2 = Vec::new();
        let mut step = 0usize;
        let res2 = super::enum_services_with(|_buf, cap| {
            caps2.push(cap);
            step += 1;
            let total = match step {
                1 => 1024u32,
                2 => 1200u32,
                _ => 1400u32,
            };
            if cap == 0 {
                return super::ServiceEnumStep::MoreData { remaining: total };
            }
            if cap >= total {
                return super::ServiceEnumStep::Done(5);
            }
            let written = (cap / 200) * 200;
            super::ServiceEnumStep::MoreData { remaining: total - written }
        });
        assert_eq!(res2.map(|(_, c)| c), Some(5));
        assert!(caps2.windows(2).all(|w| w[1] > w[0]), "buffer capacities did not grow: {caps2:?}");

        // Non-ERROR_MORE_DATA failures and empty results return None cleanly.
        assert!(super::enum_services_with(|_, _| super::ServiceEnumStep::Error).is_none());
        assert!(super::enum_services_with(|_, cap| if cap == 0 { super::ServiceEnumStep::MoreData { remaining: 64 } } else { super::ServiceEnumStep::Done(0) }).is_none());

        // Live SCM enumeration runs cleanly against the current PID (not a service).
        let mine = std::collections::HashSet::from([std::process::id()]);
        assert_eq!(super::service_engine_for_pids(Some(&mine), None), None);
    }
}
