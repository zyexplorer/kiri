//! Accepts docker clients on the pipe and carries them over the bridge.

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
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
/// control (crates/kiri/src/bridge.rs says how).
const ACK: u8 = 5;

/// Bytes of one connection one way that may be unacknowledged; the same
/// as the bridge's.
const WINDOW: usize = 4 << 20;
const ACK_EVERY: usize = WINDOW / 4;
/// A connection with credit that is sent past it is closed once this much
/// is queued; one without (an older bridge) at what was 256 frames.
const LIMIT: usize = 2 * WINDOW;
const LIMIT_NO_CREDIT: usize = 256 * 64 * 1024;
/// How long a new connection waits for a bridge just started to say it
/// does flow control; an older one never does.
const HELLO_WAIT: Duration = Duration::from_secs(5);

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
    true
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
        if let Some(b) = hub.current.lock().unwrap_or_else(PoisonError::into_inner).take() {
            b.dead.store(true, Ordering::SeqCst);
        }
    }
}

/// Serves the pipe on threads of its own. Fails if another kiri-win
/// already serves it.
pub fn start(cfg: Config) -> io::Result<()> {
    let first = Pipe::create(&cfg.pipe, true)?;
    log(&format!("serving {} ({})", cfg.pipe, cfg.distro.as_deref().unwrap_or("default distro")));
    let pipe_name = cfg.pipe.clone();
    let hub = Arc::new(Hub { cfg: Mutex::new(cfg), current: Mutex::new(None), next_id: AtomicU32::new(1) });
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
        serve_while_free(hub, first);
    }
}

/// Accepts clients on docker_engine until another engine starts. The
/// clients connected then keep their connections until they close.
fn serve_while_free(hub: &Arc<Hub>, first: Pipe) {
    let stop = Arc::new(AtomicBool::new(false));
    // The instance waiting for a client, for the watcher to cancel.
    let waiting: Arc<Mutex<Option<Arc<Pipe>>>> = Arc::new(Mutex::new(None));
    let (st, w) = (stop.clone(), waiting.clone());
    let watcher = thread::spawn(move || {
        while !st.load(Ordering::SeqCst) {
            if let Some(other) = other_engine() {
                log(&format!("{other} started: handing {DOCKER_ENGINE} back"));
                st.store(true, Ordering::SeqCst);
                if let Some(p) = w.lock().unwrap_or_else(PoisonError::into_inner).take() {
                    p.cancel();
                }
                return;
            }
            thread::sleep(std::time::Duration::from_secs(2));
        }
    });
    let mut pipe = Arc::new(first);
    'serve: loop {
        *waiting.lock().unwrap_or_else(PoisonError::into_inner) = Some(pipe.clone());
        let connected = pipe.connect();
        waiting.lock().unwrap_or_else(PoisonError::into_inner).take();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match connected {
            Ok(()) => {
                let hub = hub.clone();
                let p = pipe.clone();
                thread::spawn(move || serve_shared(&hub, p));
            }
            Err(e) => log(&format!("{DOCKER_ENGINE} connect: {e}")),
        }
        // Every instance in use: wait for a client to go, as for Kiri's pipe.
        pipe = loop {
            if stop.load(Ordering::SeqCst) {
                break 'serve;
            }
            match Pipe::create(DOCKER_ENGINE, false) {
                Ok(p) => break Arc::new(p),
                Err(_) => thread::sleep(std::time::Duration::from_millis(100)),
            }
        };
    }
    let _ = watcher.join();
}

/// The first of OTHER_ENGINES that is running.
fn other_engine() -> Option<&'static str> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    // SAFETY: a process snapshot read entry by entry into a zeroed,
    // correctly sized PROCESSENTRY32W, and closed once.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        let mut more = Process32FirstW(snap, &mut e) != 0;
        while more && found.is_none() {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]);
            found = OTHER_ENGINES.iter().copied().find(|o| o.eq_ignore_ascii_case(&name));
            more = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        found
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
            let (g, t) = self.cv.wait_timeout(s, Duration::from_millis(500)).unwrap_or_else(PoisonError::into_inner);
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

#[derive(Debug)]
struct Hub {
    /// Setup may change the distro while kiri-win runs.
    cfg: Mutex<Config>,
    /// The running bridge; a new one starts when it has died (WSL shut down).
    current: Mutex<Option<Arc<Bridge>>>,
    next_id: AtomicU32,
}

impl Hub {
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
        let b = Bridge::start(&cfg)?;
        *current = Some(b.clone());
        Ok(b)
    }
}

#[derive(Debug)]
struct Bridge {
    input: Mutex<ChildStdin>,
    conns: Mutex<HashMap<u32, Conn>>,
    dead: AtomicBool,
    /// The bridge said hello: it does flow control.
    credit: AtomicBool,
    started: Instant,
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
        let bridge = Arc::new(Bridge { input: Mutex::new(input), conns: Mutex::new(HashMap::new()), dead: AtomicBool::new(false), credit: AtomicBool::new(false), started: Instant::now(), said: Mutex::new(None) });
        log("bridge started");
        let b = bridge.clone();
        thread::spawn(move || b.replies(output, child));
        Ok(bridge)
    }

    /// Hands each reply to its connection, until the bridge exits.
    fn replies(&self, output: impl Read, mut child: Child) {
        let mut r = BufReader::with_capacity(64 * 1024, output);
        while let Ok(Some((kind, id, data))) = read_frame(&mut r) {
            if kind == ACK {
                if id == 0 {
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
        // Every connection still open ends now.
        for (_, c) in self.conns.lock().unwrap_or_else(PoisonError::into_inner).drain() {
            c.close();
        }
        let status = child.wait();
        log(&format!("bridge exited: {status:?}"));
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

    /// Whether this bridge does flow control, waiting for its hello while
    /// it has only just started.
    fn credit(&self) -> bool {
        while !self.credit.load(Ordering::SeqCst) {
            if self.dead.load(Ordering::SeqCst) || self.started.elapsed() > HELLO_WAIT {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }
}

/// One docker client: its requests go over the bridge, on this thread, and
/// the replies come back on another.
fn serve(hub: &Hub, pipe: Pipe) {
    serve_shared(hub, Arc::new(pipe));
}

fn serve_shared(hub: &Hub, pipe: Arc<Pipe>) {
    // A bridge can die between hub.bridge() and the insert below: while WSL
    // is still booting, wsl.exe exits at once. `replies` ends every
    // connection it finds when that happens, but one registered after that
    // sweep sat in the dead bridge's map for good: no thread would ever
    // write its reply or cancel its read, so the client waited forever,
    // with the window's engine thread (and every Start click) behind it.
    // So the insert checks `dead` under the lock the sweep takes, and a
    // bridge found dead is left for the next one.
    let mut tries = 0;
    let (bridge, id, window, rx, queued) = loop {
        let bridge = match hub.bridge() {
            Ok(b) => b,
            Err(e) => {
                let answered = if e.kind() == io::ErrorKind::NotConnected {
                    setup_pending() && refuse_new(&pipe, SETUP_UNFINISHED)
                } else {
                    log(&format!("bridge: {e}"));
                    refuse_new(&pipe, &message(Some(&format!("WSL did not start Kiri's Linux ({e})"))))
                };
                if answered {
                    linger(&pipe);
                }
                return;
            }
        };
        let id = hub.next_id.fetch_add(1, Ordering::SeqCst);
        // The client's input waits for the bridge's credit (the daemon holds
        // back an attach's stdin until the container reads it), and its
        // replies are bounded by the credit this side gives (see `to_client`).
        let window = bridge.credit().then(|| Arc::new(Window::default()));
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
            break (bridge, id, window, rx, queued);
        }
        bridge.conns.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
        tries += 1;
        if tries == 3 {
            log("bridge: died three times while a connection opened; giving up on it");
            if refuse_new(&pipe, &message(Some("Kiri's Linux in WSL stopped three times as the command came in"))) {
                linger(&pipe);
            }
            return;
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
    let _ = bridge.send(end, id, &[]);
    let replies_ended = writer.join().unwrap_or(false);
    if end == EOF && replies_ended {
        linger(&pipe);
    }
    if let Some(c) = bridge.conns.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
        c.close();
    }
}

/// Writes the bridge's replies to the client, acknowledging them to the
/// bridge (`credit`) as they go when it does flow control. True if they
/// ended with EOF, or the client was told why the engine could not serve
/// it (once its `request` had come).
fn to_client(pipe: &Pipe, rx: &Receiver<Reply>, queued: &AtomicUsize, bridge: &Bridge, credit: Option<(u32, &Window)>, request: &Arrival) -> bool {
    let mut done = 0usize;
    let mut answered = false;
    for reply in rx {
        match reply {
            Reply::Data(d) => {
                if pipe.write(&d).is_err() {
                    return false;
                }
                if !answered {
                    bridge.told(None);
                }
                answered = true;
                queued.fetch_sub(d.len(), Ordering::SeqCst);
                done += d.len();
                if let Some((id, _)) = credit
                    && done >= ACK_EVERY {
                        let _ = bridge.send(ACK, id, &(done as u32).to_be_bytes());
                        done = 0;
                    }
            }
            // The docker CLI reads in byte mode, where the empty message
            // that is EOF reads as one only with no data queued ahead of
            // it; behind data it is merged away and the client waits on.
            Reply::Eof => {
                pipe.flush();
                return pipe.write(&[]).is_ok();
            }
            // Closed before a word: the bridge could not reach the engine
            // (and says why), the daemon closed it unanswered (no reason),
            // or setup is unfinished. Said to a client still there, after
            // its request.
            Reply::Close(why) if !answered && pipe.alive() => {
                let message = message(why.as_deref());
                if refuse(pipe, request.head(REQUEST_WAIT), &message) {
                    bridge.told(Some(&message));
                    return true;
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
    #[test]
    fn frames_round_trip() {
        let mut wire = super::header(super::DATA, 7, 3).to_vec();
        wire.extend_from_slice(b"abc");
        let mut r = &wire[..];
        assert_eq!(super::read_frame(&mut r).unwrap(), Some((super::DATA, 7, b"abc".to_vec())));
        assert_eq!(super::read_frame(&mut r).unwrap(), None);
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
}
