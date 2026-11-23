//! `kiri bridge`: the WSL2 end of kiri-win.exe. The Windows docker CLI's
//! connections arrive multiplexed on stdin, each relayed to the daemon's
//! unix socket, and the replies go back on stdout. A half-close travels as
//! a frame of its own: WSL's localhost relay drops TCP half-closes, which
//! `docker run` and `attach` depend on, so the API is not served that way.
//!
//! Flow control: each side may have at most WINDOW bytes of a connection
//! in flight towards the other, and the other acknowledges (ACK) what it
//! has handed on. A sender out of credit waits, on that connection's own
//! thread, so a daemon that pauses one attach's stdin (the container is
//! not reading it) or a client that stops reading its output holds up
//! only that connection, and no queue grows past the window. The bridge
//! says it does this with an ACK for connection 0 when it starts, and
//! kiri-win asks for it per connection by sending 1 with OPEN; a kiri-win
//! from before this ignores the first and never sends the second, and its
//! connections get the old bounded queue. The hello's data, 1, says this
//! bridge answers a PING on connection 0 with one: how kiri-win tells a
//! wsl.exe that runs and relays nothing (as after Windows wakes) from a
//! bridge with nothing to say. An older kiri-win ignores the data and
//! never sends one.
//!
//! A daemon that is not there (its socket missing or refusing: WSL
//! restarted and nothing started it, or it crashed) is started here, by
//! kiri-start in Kiri Linux or `systemctl start kiri`, once for all the
//! connections that come meanwhile (see `Starter`). A connection it still
//! cannot reach is closed with the reason as the CLOSE's data, which
//! kiri-win tells the docker client; a kiri-win from before this ignores
//! CLOSE's data. Each start and how it went is also told to kiri-win
//! (NOTE) for its own log. A stopped engine still runs and answers ("Kiri
//! is stopped"), so it is never started here.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// A frame: kind, connection id (u32 BE), length (u32 BE), then the data.
/// The same as in crates/kiri-win.
const OPEN: u8 = 1;
const DATA: u8 = 2;
/// No more data from this side (a half-close).
const EOF: u8 = 3;
/// The connection is gone, both ways. From the bridge, its data (when
/// there is any) is why the daemon could not be reached, in UTF-8.
const CLOSE: u8 = 4;
/// Credit back: the data is how many bytes of this connection the sender
/// of the ACK has handed on (u32 BE). For connection 0, the bridge's hello.
const ACK: u8 = 5;
/// From the bridge, on connection 0: a line of what it did to reach the
/// engine (a start and how it went), in UTF-8, for kiri-win's own log, so
/// a wake shows in kiri-win.log and not only in /var/log/kiri.log. A
/// kiri-win from before this drops it: it has no connection 0.
const NOTE: u8 = 6;
/// On connection 0, from kiri-win: answered with the same, at once.
const PING: u8 = 7;

/// The largest frame either side accepts. Both send at most 64 KiB of
/// data per frame; without a bound, a length of up to 4 GiB was allocated
/// as read.
const MAX_FRAME: usize = 1 << 20;

/// Bytes of one connection one way that may be unacknowledged. Some MB/s
/// over a sub-millisecond round trip need far less; it is the most a
/// connection can hold in memory on each side.
const WINDOW: usize = 4 << 20;
/// Received bytes are acknowledged once this many have been handed on.
const ACK_EVERY: usize = WINDOW / 4;
/// A connection whose peer sends past its credit (or, without credit, one
/// the daemon has stopped reading) is closed once this much is queued.
const LIMIT: usize = 2 * WINDOW;

/// The most a start may take: kiri-start (or systemctl), then the wait for
/// the daemon's socket to take a connection. A plain kiri-start takes at
/// most 20 s (15 s for a dying daemon to go, 5 s for the new one's
/// socket), or about as long waiting on its lock behind another; 5 s more
/// so one that would just make it is not killed.
const START_LIMIT: Duration = Duration::from_secs(25);
/// After a start failed, connections within this get its reason at once,
/// rather than each starting the engine again. Doubled after each failed
/// start in a row, up to RETRY_MOST: an engine that exits as it starts
/// is not started every few seconds for as long as something asks.
const RETRY_AFTER: Duration = Duration::from_secs(5);
const RETRY_MOST: Duration = Duration::from_secs(300);
/// Why a start whose daemon took the connection and then answered nothing
/// (it exited as it booted) is held as failed.
const NO_ANSWER: &str = "it started, then stopped before answering; see /var/log/kiri.log";
/// Kiri Linux's start script (contrib/wsl/kiri-linux).
const KIRI_START: &str = "/usr/local/bin/kiri-start";
/// Where the daemon's own output goes in Kiri Linux (kiri-start) and in a
/// distro without systemd (contrib/wsl/install.sh).
const LOG: &str = "/var/log/kiri.log";

type Out = Arc<Mutex<dyn Write + Send>>;

pub fn run(args: &[String]) -> io::Result<()> {
    let socket = match args.iter().position(|a| a == "--socket").and_then(|i| args.get(i + 1)) {
        Some(s) => PathBuf::from(s),
        None => crate::daemon::socket_path()?,
    };
    let out: Out = Arc::new(Mutex::new(BufWriter::with_capacity(64 * 1024, io::stdout())));
    let mut starter = Starter::new(socket, Box::new(start_engine), PathBuf::from(LOG));
    starter.tell = Some(teller(out.clone()));
    let starter = Arc::new(starter);
    serve(BufReader::with_capacity(64 * 1024, io::stdin()), &out, &starter)
}

/// Tells kiri-win a line for its log (NOTE). Best effort: a kiri-win that
/// has gone has nobody to tell.
fn teller(out: Out) -> Box<dyn Fn(&str) + Send + Sync> {
    Box::new(move |msg: &str| {
        let msg = msg.as_bytes();
        let _ = send(&out, NOTE, 0, &msg[..msg.len().min(4096)]);
    })
}

/// One connection, as the frame reader sees it.
struct Conn {
    /// To its writer thread, which has its own unbounded queue: what bounds
    /// it is `queued`, which the credit keeps under WINDOW.
    tx: Sender<Op>,
    queued: Arc<AtomicUsize>,
    /// Credit for its replies, when kiri-win asked for flow control.
    window: Option<Arc<Window>>,
}

/// Relays the frames on `input` until it ends (kiri-win has gone; its
/// connections go too).
fn serve(mut input: impl Read, out: &Out, starter: &Arc<Starter>) -> io::Result<()> {
    // Hello: this bridge does flow control, and (1) answers PING.
    send(out, ACK, 0, &[1])?;
    let mut conns: HashMap<u32, Conn> = HashMap::new();
    while let Some((kind, id, data)) = read_frame(&mut input)? {
        match kind {
            // The connect is the writer's, on its own thread: one that waits
            // for the engine to start holds up no other connection. Input
            // that comes meanwhile waits in its queue.
            OPEN => {
                let credit = data.first() == Some(&1);
                let (tx, rx) = channel::<Op>();
                let queued = Arc::new(AtomicUsize::new(0));
                let window = credit.then(|| Arc::new(Window::default()));
                conns.insert(id, Conn { tx, queued: queued.clone(), window: window.clone() });
                let (out, starter) = (out.clone(), starter.clone());
                std::thread::Builder::new().name("kiri-bridge-w".into()).spawn(move || writer(id, &starter, &rx, &queued, &out, credit, window))?;
            }
            PING => send(out, PING, 0, &[])?,
            ACK => {
                if let (Some(w), Some(n)) = (conns.get(&id).and_then(|c| c.window.as_ref()), ack_count(&data)) {
                    w.ack(n);
                }
            }
            DATA | EOF | CLOSE => {
                let Some(c) = conns.get(&id) else { continue };
                let op = match kind {
                    DATA => Op::Data(data),
                    EOF => Op::Eof,
                    _ => Op::Close,
                };
                let len = if let Op::Data(d) = &op { d.len() } else { 0 };
                // Past the window: kiri-win sent beyond its credit, or (without
                // credit) the daemon is not taking this connection's input.
                // Dropping the sender ends its writer, which closes it.
                let overflow = c.queued.fetch_add(len, Ordering::SeqCst) + len > LIMIT;
                // Its writer has ended (it could not reach the daemon, and
                // has closed the connection with why): nothing is counted
                // or closed again.
                let ended = !overflow && c.tx.send(op).is_err();
                if overflow || ended || kind == CLOSE {
                    if let Some(c) = conns.remove(&id)
                        && let Some(w) = c.window {
                            w.close();
                        }
                    if overflow {
                        send(out, CLOSE, id, &[])?;
                    }
                }
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad bridge frame {kind}"))),
        }
    }
    for c in conns.into_values() {
        if let Some(w) = c.window {
            w.close();
        }
    }
    Ok(())
}

/// What one connection's writer is asked to do, in order.
enum Op {
    Data(Vec<u8>),
    /// kiri-win's client half-closed: no more input, replies may still flow.
    Eof,
    Close,
}

/// Connects to the daemon (starting it if it is not there) and starts the
/// thread copying its replies; then writes the connection's input to it,
/// acknowledging it to kiri-win (with `credit`) as it goes. Ends, closing
/// the connection, when asked to or when its queue's sender is dropped.
/// Once the daemon's side has gone, input is still taken and acknowledged,
/// unwritten, so kiri-win's sender never waits on credit that cannot come.
/// A daemon it cannot reach closes the connection with why.
fn writer(id: u32, starter: &Arc<Starter>, rx: &Receiver<Op>, queued: &AtomicUsize, out: &Out, credit: bool, window: Option<Arc<Window>>) {
    let reached = starter.connect().and_then(|s| {
        let reader = s.try_clone().map_err(|e| e.to_string())?;
        let (o, st) = (out.clone(), starter.clone());
        std::thread::Builder::new().name("kiri-bridge".into()).spawn(move || replies(id, reader, &o, window.as_deref(), &st)).map_err(|e| e.to_string())?;
        Ok(s)
    });
    let mut s = match reached {
        Ok(s) => s,
        Err(why) => {
            // Within one frame; kiri-win reads it lossily.
            let why = why.as_bytes();
            let _ = send(out, CLOSE, id, &why[..why.len().min(4096)]);
            return;
        }
    };
    let acks = credit.then_some(out);
    let mut broken = false;
    let mut done = 0usize;
    for op in rx {
        match op {
            Op::Data(d) => {
                if !broken && s.write_all(&d).is_err() {
                    broken = true;
                    let _ = s.shutdown(Shutdown::Both);
                }
                queued.fetch_sub(d.len(), Ordering::SeqCst);
                done += d.len();
                if let Some(out) = acks
                    && (done >= ACK_EVERY || broken) {
                        if send(out, ACK, id, &(done as u32).to_be_bytes()).is_err() {
                            break;
                        }
                        done = 0;
                    }
            }
            Op::Eof => {
                let _ = s.shutdown(Shutdown::Write);
            }
            Op::Close => break,
        }
    }
    let _ = s.shutdown(Shutdown::Both);
}

/// Copies the daemon's side of one connection to stdout, ending with EOF.
/// With a `window`, it waits for kiri-win's credit before each piece. The
/// first bytes tell `starter` the engine answers.
fn replies(id: u32, mut s: UnixStream, out: &Out, window: Option<&Window>, starter: &Starter) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut first = true;
    let end = loop {
        match s.read(&mut buf) {
            Ok(0) => break EOF,
            Ok(n) => {
                if std::mem::take(&mut first) {
                    starter.answered();
                }
                // kiri-win closed the connection while this waited.
                if window.is_some_and(|w| !w.take(n)) {
                    return;
                }
                if send(out, DATA, id, &buf[..n]).is_err() {
                    return;
                }
            }
            Err(_) => break CLOSE,
        }
    };
    let _ = send(out, end, id, &[]);
}

/// Starts the engine when its socket is not there or refuses (the daemon
/// is gone), once for every connection that comes while it starts: they
/// wait for that start, up to START_LIMIT, then connect. One that failed
/// is not tried again within its back-off (RETRY_AFTER, doubling to
/// RETRY_MOST); connections then get its reason. Once the back-off is at
/// RETRY_MOST, a start that fails as the last did is not logged again.
///
/// A start counts as one that worked, and the back-off is reset, only once
/// the engine answers a connection (`answered`), started here or not: a
/// daemon that creates its socket and then exits as it boots takes the
/// connections the kernel queues for it, and is not started again on each.
/// Until then, a start whose daemon took the connection is held as failed,
/// with NO_ANSWER as its reason.
struct Starter {
    socket: PathBuf,
    /// Starts the daemon, by the deadline given; Err when it cannot start.
    /// Ok when it may be starting: the wait for its socket then decides.
    start: Box<dyn Fn(Instant) -> Result<(), String> + Send + Sync>,
    /// Where starts and their outcomes are logged.
    log: PathBuf,
    /// Also told each of those lines: kiri-win, over NOTE frames.
    tell: Option<Box<dyn Fn(&str) + Send + Sync>>,
    /// The most one start takes (START_LIMIT).
    limit: Duration,
    state: Mutex<StartState>,
    cv: Condvar,
}

#[derive(Default)]
struct StartState {
    running: bool,
    /// Starts ended so far.
    ended: u64,
    /// When the last start failed, and why (NO_ANSWER: its daemon took
    /// the connection, and nothing has answered since).
    failed: Option<(Instant, String)>,
    /// Whether the last start ended with a connection to the daemon: those
    /// that waited for it connect too.
    connected: bool,
    /// How long after a failed start the next waits.
    backoff: Duration,
}

impl Starter {
    fn new(socket: PathBuf, start: Box<dyn Fn(Instant) -> Result<(), String> + Send + Sync>, log: PathBuf) -> Starter {
        Starter { socket, start, log, tell: None, limit: START_LIMIT, state: Mutex::new(StartState { backoff: RETRY_AFTER, ..StartState::default() }), cv: Condvar::new() }
    }

    /// A connection to the daemon, or why there is none.
    fn connect(&self) -> Result<UnixStream, String> {
        let ended = self.state.lock().unwrap_or_else(|e| e.into_inner()).ended;
        match UnixStream::connect(&self.socket) {
            // Its first reply ends a back-off (`answered`), not this: a
            // daemon dying as it boots still takes connections.
            Ok(s) => return Ok(s),
            Err(e) if !gone(&e) => return Err(format!("the bridge could not reach its socket ({e})")),
            Err(_) => {}
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st = self.cv.wait_while(st, |s| s.running).unwrap_or_else(|e| e.into_inner());
        // A start ended since the try above (this one waited for it, or it
        // ended just after): its outcome is this connection's too.
        if st.ended != ended {
            if let Some((_, why)) = st.failed.as_ref().filter(|_| !st.connected) {
                return Err(why.clone());
            }
            drop(st);
            return UnixStream::connect(&self.socket).map_err(|e| format!("it started, then did not answer ({e})"));
        }
        if let Some((at, why)) = &st.failed
            && at.elapsed() < st.backoff
        {
            return Err(why.clone());
        }
        st.running = true;
        // The same failure again at the longest back-off: not logged.
        let last = st.failed.as_ref().map(|(_, why)| why.clone()).filter(|_| st.backoff >= RETRY_MOST);
        // The last start's daemon took a connection and has gone: said here,
        // as nothing could say it when it went.
        let unanswered = last.is_none() && st.failed.as_ref().is_some_and(|(_, why)| why == NO_ANSWER);
        drop(st);
        if unanswered {
            self.note(&format!("engine did not start: {NO_ANSWER}"));
        }
        // Records how the start ended and wakes those waiting above, when
        // dropped: also on a panic where panics unwind (tests; a release
        // build aborts on one, panic = "abort").
        let mut ending = Ending { starter: self, failed: Some("the bridge's start of the engine ended unexpectedly".to_owned()) };
        let r = self.start_and_connect(last.as_deref());
        ending.failed = r.as_ref().err().cloned();
        r
    }

    /// Starts the daemon and connects to it; `last` is the failure the
    /// log already has, when a repeat of it is not to be logged.
    fn start_and_connect(&self, last: Option<&str>) -> Result<UnixStream, String> {
        let until = Instant::now() + self.limit;
        if last.is_none() {
            self.note("engine not answering on its socket; starting it");
        }
        let failed = |why: String| {
            if last != Some(why.as_str()) {
                self.note(&format!("engine did not start: {why}"));
            }
            Err(why)
        };
        if let Err(why) = (self.start)(until) {
            return failed(why);
        }
        loop {
            match UnixStream::connect(&self.socket) {
                Ok(s) => {
                    if last != Some(NO_ANSWER) {
                        self.note("engine started");
                    }
                    return Ok(s);
                }
                Err(e) if Instant::now() >= until => return failed(format!("it did not answer within {} s ({e}); see {LOG}", self.limit.as_secs())),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    /// The engine answered a connection: a failure before is over, so the
    /// next one is logged and waits RETRY_AFTER.
    fn answered(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.failed = None;
        st.backoff = RETRY_AFTER;
    }

    /// One line in the log, as the daemon's are stamped (UTC, to the ms),
    /// and the same told to kiri-win for kiri-win.log.
    fn note(&self, msg: &str) {
        let t = crate::daemon::time::rfc3339(SystemTime::now());
        let line = format!("{}Z kiri bridge: {msg}\n", t.get(..23).unwrap_or(&t));
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.log) {
            let _ = f.write_all(line.as_bytes());
        }
        if let Some(tell) = &self.tell {
            tell(msg);
        }
    }
}

/// The end of one start, recorded when dropped: `running` off, its outcome
/// (`failed`: why, or None when it connected, held as NO_ANSWER until the
/// engine answers) and the next back-off set, and everyone waiting on it
/// woken.
struct Ending<'a> {
    starter: &'a Starter,
    failed: Option<String>,
}

impl Drop for Ending<'_> {
    fn drop(&mut self) {
        let s = self.starter;
        let mut st = s.state.lock().unwrap_or_else(|e| e.into_inner());
        st.running = false;
        st.ended += 1;
        st.connected = self.failed.is_none();
        let why = self.failed.take().unwrap_or_else(|| NO_ANSWER.to_owned());
        let was = st.backoff;
        st.backoff = if st.failed.is_some() { (was * 2).min(RETRY_MOST) } else { RETRY_AFTER };
        if st.backoff == RETRY_MOST && was < RETRY_MOST {
            s.note(&format!("engine keeps failing to start; trying again every {} s, logging only a new reason", RETRY_MOST.as_secs()));
        }
        st.failed = Some((Instant::now(), why));
        s.cv.notify_all();
    }
}

/// Whether a connect's error means no daemon is there: the socket file is
/// missing, or left behind by one that has gone.
fn gone(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
}

/// Starts the daemon as this distro does: kiri-start in Kiri Linux,
/// systemd's `kiri` service in a distro with systemd (contrib/wsl/install.sh).
/// Killed at `until`. kiri-start's exit 1 (no socket yet) may still end
/// with the daemon up, so it is not a failure here.
fn start_engine(until: Instant) -> Result<(), String> {
    let kiri_start = Path::new(KIRI_START).exists();
    let mut cmd = if kiri_start {
        Command::new(KIRI_START)
    } else if Path::new("/run/systemd/system").exists() {
        let mut c = Command::new("systemctl");
        c.args(["start", "kiri"]);
        c
    } else {
        return Err("nothing in this Linux starts it (no kiri-start, no systemd)".to_owned());
    };
    let what = if kiri_start { "kiri-start" } else { "systemctl start kiri" };
    // stdin and stdout are kiri-win's frames: never handed on. Its own
    // process group, so nothing sent to the bridge's reaches the daemon.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| format!("could not run {what} ({e})"))?;
    // Read on a thread of its own: the daemon does not keep it open
    // (kiri-start sends its output to the log), but nothing here may wait on that.
    let (tx, rx) = channel();
    if let Some(mut err) = child.stderr.take() {
        // No thread to be had: its last line is not read, and the start goes on.
        let _ = std::thread::Builder::new().name("kiri-start-err".into()).spawn(move || {
            let mut said = String::new();
            let _ = err.read_to_string(&mut said);
            let _ = tx.send(said);
        });
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= until => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what} did not finish in {} s", START_LIMIT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("{what}: {e}")),
        }
    };
    if status.success() || (kiri_start && status.code() == Some(1)) {
        return Ok(());
    }
    // Within the deadline too: a child it left may hold stderr open.
    let said = rx.recv_timeout(until.saturating_duration_since(Instant::now()).min(Duration::from_secs(1))).unwrap_or_default();
    Err(said.lines().map(str::trim).rfind(|l| !l.is_empty()).map_or_else(|| format!("{what} failed ({status})"), str::to_owned))
}

/// The credit one way on one connection: bytes sent and not yet acknowledged.
#[derive(Default)]
struct Window {
    state: Mutex<(usize, bool)>,
    cv: Condvar,
}

impl Window {
    /// Waits while a whole window is unacknowledged, then counts `n` more;
    /// false once the connection has closed.
    fn take(&self, n: usize) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while s.0 >= WINDOW && !s.1 {
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if s.1 {
            return false;
        }
        s.0 += n;
        true
    }

    fn ack(&self, n: usize) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.0 = s.0.saturating_sub(n);
        self.cv.notify_all();
    }

    fn close(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.cv.notify_all();
    }
}

fn ack_count(data: &[u8]) -> Option<usize> {
    Some(u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize)
}

fn send(out: &Out, kind: u8, id: u32, data: &[u8]) -> io::Result<()> {
    let mut w = out.lock().unwrap_or_else(|e| e.into_inner());
    w.write_all(&header(kind, id, data.len()))?;
    w.write_all(data)?;
    w.flush()
}

fn header(kind: u8, id: u32, len: usize) -> [u8; 9] {
    let mut h = [0u8; 9];
    h[0] = kind;
    h[1..5].copy_from_slice(&id.to_be_bytes());
    h[5..].copy_from_slice(&(len as u32).to_be_bytes());
    h
}

/// The next frame, or None when the other side has gone.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    #[test]
    fn frames_round_trip() {
        let mut wire = super::header(super::DATA, 7, 3).to_vec();
        wire.extend_from_slice(b"abc");
        wire.extend_from_slice(&super::header(super::EOF, 7, 0));
        let mut r = &wire[..];
        assert_eq!(super::read_frame(&mut r).unwrap(), Some((super::DATA, 7, b"abc".to_vec())));
        assert_eq!(super::read_frame(&mut r).unwrap(), Some((super::EOF, 7, vec![])));
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

    /// A bridge between socket pairs, to a "daemon" on its own socket that
    /// `daemon` serves (one connection). Returns kiri-win's ends: what it
    /// writes to the bridge, and what it reads from it.
    fn bridge(name: &str, daemon: impl FnOnce(UnixStream) + Send + 'static) -> (UnixStream, UnixStream, PathBuf) {
        let dir = temp(name);
        let sock = dir.join("d.sock");
        let l = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || daemon(l.accept().unwrap().0));
        // The daemon is up: nothing is ever started.
        let starter = Arc::new(Starter::new(sock, Box::new(|_| panic!("started a daemon that was up")), dir.join("log")));
        let (to_bridge, from_bridge) = bridge_to(starter);
        (to_bridge, from_bridge, dir)
    }

    /// A bridge using `starter`; kiri-win's ends, as `bridge`.
    fn bridge_to(starter: Arc<Starter>) -> (UnixStream, UnixStream) {
        let (to_bridge, input) = UnixStream::pair().unwrap();
        let (output, from_bridge) = UnixStream::pair().unwrap();
        let out: Out = Arc::new(Mutex::new(output));
        std::thread::spawn(move || {
            let _ = serve(input, &out, &starter);
        });
        (to_bridge, from_bridge)
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kiri-bridge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An echo daemon on `sock`, for every connection.
    fn echo_daemon(sock: &Path) {
        let l = UnixListener::bind(sock).unwrap();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                std::thread::spawn(move || {
                    let mut req = Vec::new();
                    let _ = s.read_to_end(&mut req);
                    let _ = s.write_all(&req);
                });
            }
        });
    }

    /// Each connection's reply until its EOF, or its CLOSE's reason.
    fn outcomes(from_bridge: &mut UnixStream, n: usize) -> HashMap<u32, Result<Vec<u8>, String>> {
        let mut got: HashMap<u32, Vec<u8>> = HashMap::new();
        let mut done = HashMap::new();
        while done.len() < n {
            let (kind, id, data) = read_frame(from_bridge).unwrap().expect("the bridge ended");
            match kind {
                ACK | NOTE => {}
                DATA => got.entry(id).or_default().extend_from_slice(&data),
                EOF => {
                    done.insert(id, Ok(got.remove(&id).unwrap_or_default()));
                }
                CLOSE => {
                    done.insert(id, Err(String::from_utf8(data).unwrap()));
                }
                other => panic!("unexpected frame {other}"),
            }
        }
        done
    }

    /// The daemon has gone (a socket file left, nothing on it): the first
    /// connection starts it, the others that come meanwhile wait for that
    /// one start, and all are served. The start is logged.
    #[test]
    fn a_gone_daemon_is_started_once() {
        let dir = temp("start");
        let sock = dir.join("d.sock");
        // A socket file nobody listens on: ECONNREFUSED, as after WSL restarted.
        drop(UnixListener::bind(&sock).unwrap());
        let starts = Arc::new(AtomicUsize::new(0));
        let (s, n) = (sock.clone(), starts.clone());
        let starter = Arc::new(Starter::new(
            sock.clone(),
            Box::new(move |_| {
                n.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(300));
                let _ = std::fs::remove_file(&s);
                echo_daemon(&s);
                Ok(())
            }),
            dir.join("log"),
        ));
        let (mut to_bridge, mut from_bridge) = bridge_to(starter);
        for id in 1..=4 {
            frame(&mut to_bridge, OPEN, id, &[1]);
            frame(&mut to_bridge, DATA, id, format!("ping {id}").as_bytes());
            frame(&mut to_bridge, EOF, id, &[]);
        }
        let done = outcomes(&mut from_bridge, 4);
        for id in 1..=4 {
            assert_eq!(done[&id], Ok(format!("ping {id}").into_bytes()), "connection {id}");
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert!(log.contains("kiri bridge: engine not answering on its socket; starting it") && log.contains("kiri bridge: engine started"), "{log}");
        // Up now: a later connection starts nothing.
        frame(&mut to_bridge, OPEN, 5, &[1]);
        frame(&mut to_bridge, DATA, 5, b"again");
        frame(&mut to_bridge, EOF, 5, &[]);
        assert_eq!(outcomes(&mut from_bridge, 1)[&5], Ok(b"again".to_vec()));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each start and how it went is told to kiri-win too, as NOTE frames
    /// on connection 0, for kiri-win.log (#93: a wake left no trace there).
    #[test]
    fn starts_are_told_to_kiri_win() {
        let dir = temp("tell");
        let sock = dir.join("d.sock");
        let s = sock.clone();
        let mut starter = Starter::new(
            sock,
            Box::new(move |_| {
                echo_daemon(&s);
                Ok(())
            }),
            dir.join("log"),
        );
        let (output, mut from_bridge) = UnixStream::pair().unwrap();
        starter.tell = Some(teller(Arc::new(Mutex::new(output))));
        assert!(starter.connect().is_ok());
        let mut notes = Vec::new();
        for _ in 0..2 {
            let (kind, id, data) = read_frame(&mut from_bridge).unwrap().unwrap();
            assert_eq!((kind, id), (NOTE, 0));
            notes.push(String::from_utf8(data).unwrap());
        }
        assert_eq!(notes, ["engine not answering on its socket; starting it", "engine started"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hello says this bridge answers PING, and it does, while a
    /// connection waits on a daemon that says nothing.
    #[test]
    fn a_ping_is_answered() {
        let (mut to_bridge, mut from_bridge, dir) = bridge("ping", |s| {
            std::thread::sleep(Duration::from_secs(2));
            drop(s);
        });
        assert_eq!(read_frame(&mut from_bridge).unwrap(), Some((ACK, 0, vec![1])));
        frame(&mut to_bridge, OPEN, 1, &[1]);
        frame(&mut to_bridge, DATA, 1, b"GET /events HTTP/1.1\r\n\r\n");
        frame(&mut to_bridge, PING, 0, &[]);
        assert_eq!(read_frame(&mut from_bridge).unwrap(), Some((PING, 0, vec![])));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A start that fails: the connection is closed with why, and those
    /// within RETRY_AFTER get that at once, without another start.
    #[test]
    fn a_failed_start_is_said_and_not_retried_at_once() {
        let dir = temp("fail");
        let starts = Arc::new(AtomicUsize::new(0));
        let n = starts.clone();
        let starter = Arc::new(Starter::new(
            dir.join("none.sock"),
            Box::new(move |_| {
                n.fetch_add(1, Ordering::SeqCst);
                Err("kiri daemon exited as it started; see /var/log/kiri.log".to_owned())
            }),
            dir.join("log"),
        ));
        let (mut to_bridge, mut from_bridge) = bridge_to(starter.clone());
        frame(&mut to_bridge, OPEN, 1, &[1]);
        frame(&mut to_bridge, DATA, 1, b"GET /_ping HTTP/1.1\r\n\r\n");
        assert_eq!(outcomes(&mut from_bridge, 1)[&1], Err("kiri daemon exited as it started; see /var/log/kiri.log".to_owned()));
        for id in 2..=3 {
            frame(&mut to_bridge, OPEN, id, &[]);
        }
        let done = outcomes(&mut from_bridge, 2);
        assert!(done.values().all(|r| r.as_ref().is_err_and(|e| e.contains("exited as it started"))), "{done:?}");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        // After RETRY_AFTER, the next connection tries again.
        starter.state.lock().unwrap().failed.as_mut().unwrap().0 -= RETRY_AFTER;
        frame(&mut to_bridge, OPEN, 4, &[1]);
        assert!(outcomes(&mut from_bridge, 1)[&4].is_err());
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert_eq!(log.matches("engine did not start: kiri daemon exited as it started").count(), 2, "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A start that says it worked but leaves no daemon: the wait for the
    /// socket ends at the deadline it was given, with why.
    #[test]
    fn a_start_with_no_daemon_ends_at_the_deadline() {
        let dir = temp("nodaemon");
        let mut starter = Starter::new(dir.join("none.sock"), Box::new(|_| Ok(())), dir.join("log"));
        starter.limit = Duration::from_secs(1);
        let began = Instant::now();
        let why = starter.connect().unwrap_err();
        assert!(why.starts_with("it did not answer within 1 s"), "{why}");
        assert!(began.elapsed() >= starter.limit && began.elapsed() < starter.limit + Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Failed starts in a row wait longer each time, 5 s doubling to 5 min,
    /// and one that works resets that. At the longest, the same failure is
    /// not logged again; a new reason is.
    #[test]
    fn failed_starts_back_off_and_are_logged_boundedly() {
        let dir = temp("backoff");
        let why = Arc::new(Mutex::new(Err::<(), String>("kiri daemon exited as it started".to_owned())));
        let (w, sock) = (why.clone(), dir.join("d.sock"));
        let s = sock.clone();
        let mut starter = Starter::new(
            sock,
            Box::new(move |_| {
                let r = w.lock().unwrap().clone();
                if r.is_ok() {
                    echo_daemon(&s);
                }
                r
            }),
            dir.join("log"),
        );
        starter.limit = Duration::from_millis(200);
        // Each try once its back-off has passed.
        let retry = |s: &Starter| {
            let mut st = s.state.lock().unwrap();
            let b = st.backoff;
            if let Some((at, _)) = st.failed.as_mut() {
                *at = Instant::now().checked_sub(b).unwrap();
            }
        };
        let mut backoffs = Vec::new();
        for _ in 0..9 {
            retry(&starter);
            assert!(starter.connect().is_err());
            backoffs.push(starter.state.lock().unwrap().backoff.as_secs());
        }
        assert_eq!(backoffs, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
        // Within the back-off: its reason, with no start.
        assert_eq!(starter.connect().unwrap_err(), "kiri daemon exited as it started");
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        // Seven tries logged, then the two at the longest back-off not.
        assert_eq!(log.matches("starting it").count(), 7, "{log}");
        assert_eq!(log.matches("engine did not start: kiri daemon exited as it started").count(), 7, "{log}");
        assert_eq!(log.matches("trying again every 300 s").count(), 1, "{log}");
        // A new reason is logged.
        *why.lock().unwrap() = Err("could not run kiri-start".to_owned());
        retry(&starter);
        assert!(starter.connect().is_err());
        assert!(std::fs::read_to_string(dir.join("log")).unwrap().contains("engine did not start: could not run kiri-start"));
        // A start that works resets the back-off once the engine answers.
        *why.lock().unwrap() = Ok(());
        retry(&starter);
        assert!(starter.connect().is_ok());
        assert!(starter.state.lock().unwrap().failed.as_ref().is_some_and(|(_, w)| w == NO_ANSWER));
        starter.answered();
        let st = starter.state.lock().unwrap();
        assert!(st.failed.is_none() && st.backoff == RETRY_AFTER);
        drop(st);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A daemon something else started (the boot command, kiri-win) ends
    /// the back-off: the next failure is logged again and waits 5 s, not 5 min.
    #[test]
    fn a_daemon_that_answers_resets_the_back_off() {
        let dir = temp("answers");
        let sock = dir.join("d.sock");
        let starter = Arc::new(Starter::new(sock.clone(), Box::new(|_| panic!("started a daemon that answers")), dir.join("log")));
        {
            let mut st = starter.state.lock().unwrap();
            st.failed = Some((Instant::now(), "kiri daemon exited as it started".to_owned()));
            st.backoff = RETRY_MOST;
        }
        echo_daemon(&sock);
        let (mut to_bridge, mut from_bridge) = bridge_to(starter.clone());
        frame(&mut to_bridge, OPEN, 1, &[1]);
        frame(&mut to_bridge, DATA, 1, b"ping");
        frame(&mut to_bridge, EOF, 1, &[]);
        assert_eq!(outcomes(&mut from_bridge, 1)[&1], Ok(b"ping".to_vec()));
        let st = starter.state.lock().unwrap();
        assert!(st.failed.is_none() && st.backoff == RETRY_AFTER);
        drop(st);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A daemon that creates its socket and exits as it boots: the start's
    /// connection is taken, then closed unanswered. That start is held as
    /// failed, so the connections after it, once it has gone, get NO_ANSWER
    /// and wait its back-off rather than each starting it again.
    #[test]
    fn a_daemon_that_dies_as_it_boots_is_backed_off() {
        let dir = temp("dies");
        let sock = dir.join("d.sock");
        let starts = Arc::new(AtomicUsize::new(0));
        let (s, n) = (sock.clone(), starts.clone());
        let starter = Arc::new(Starter::new(
            sock.clone(),
            Box::new(move |_| {
                n.fetch_add(1, Ordering::SeqCst);
                let _ = std::fs::remove_file(&s);
                let l = UnixListener::bind(&s).unwrap();
                // Its socket stays, nobody on it: ECONNREFUSED after.
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(200));
                    drop(l);
                });
                Ok(())
            }),
            dir.join("log"),
        ));
        let (mut to_bridge, mut from_bridge) = bridge_to(starter);
        let mut done = Vec::new();
        for id in 1..=4 {
            frame(&mut to_bridge, OPEN, id, &[1]);
            frame(&mut to_bridge, DATA, id, b"GET /_ping HTTP/1.1\r\n\r\n");
            frame(&mut to_bridge, EOF, id, &[]);
            done.push(outcomes(&mut from_bridge, 1).remove(&id).unwrap());
            std::thread::sleep(Duration::from_millis(400));
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1, "{done:?}");
        assert!(done[0].as_ref().is_ok_and(Vec::is_empty) || done[0].is_err(), "{done:?}");
        assert!(done[1..].iter().all(|r| r.as_ref().is_err_and(|e| e == NO_ANSWER)), "{done:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A start that panics still ends where panics unwind (tests, debug
    /// builds): those waiting on it are not left waiting forever, and get a
    /// reason. A release build aborts on a panic (panic = "abort").
    #[test]
    fn a_start_that_panics_still_ends() {
        let dir = temp("panic");
        let starter = Arc::new(Starter::new(
            dir.join("none.sock"),
            Box::new(|_| {
                std::thread::sleep(Duration::from_millis(300));
                panic!("no thread to be had")
            }),
            dir.join("log"),
        ));
        let s = starter.clone();
        let first = std::thread::spawn(move || s.connect());
        std::thread::sleep(Duration::from_millis(100));
        assert!(starter.state.lock().unwrap().running);
        // Waits for the first's start, which panics.
        let s = starter.clone();
        let second = std::thread::spawn(move || s.connect());
        assert!(first.join().is_err());
        assert_eq!(second.join().unwrap().unwrap_err(), "the bridge's start of the engine ended unexpectedly");
        assert!(!starter.state.lock().unwrap().running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A connection the bridge closed with why (no daemon) is forgotten:
    /// what kiri-win sends for it meanwhile is not counted towards its
    /// limit, and no second, empty CLOSE follows.
    #[test]
    fn a_connection_that_could_not_connect_is_closed_once() {
        let dir = temp("once");
        let starter = Arc::new(Starter::new(dir.join("none.sock"), Box::new(|_| Err("no engine here".to_owned())), dir.join("log")));
        let (mut to_bridge, mut from_bridge) = bridge_to(starter);
        // Without credit (a kiri-win from before flow control).
        frame(&mut to_bridge, OPEN, 1, &[]);
        assert_eq!(outcomes(&mut from_bridge, 1)[&1], Err("no engine here".to_owned()));
        // Past LIMIT, had it been counted.
        let chunk = vec![0u8; 512 << 10];
        for _ in 0..(LIMIT / chunk.len() + 4) {
            frame(&mut to_bridge, DATA, 1, &chunk);
        }
        frame(&mut to_bridge, OPEN, 2, &[]);
        // The next frame is connection 2's.
        let (kind, id, _) = loop {
            let f = read_frame(&mut from_bridge).unwrap().unwrap();
            if f.0 != ACK {
                break f;
            }
        };
        assert_eq!((kind, id), (CLOSE, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn frame(w: &mut impl Write, kind: u8, id: u32, data: &[u8]) {
        w.write_all(&header(kind, id, data.len())).unwrap();
        w.write_all(data).unwrap();
    }

    /// `docker run -i … wc -c < 200 MB` where the container reads slower
    /// than input comes: the input waits for credit, all of it arrives, and
    /// the connection stays open (before, the bridge closed it 64 frames in).
    #[test]
    fn slow_stdin_waits_for_credit() {
        const TOTAL: usize = 200 << 20;
        let (mut to_bridge, mut from_bridge, _dir) = bridge("stdin", |mut s| {
            let mut buf = vec![0u8; 256 << 10];
            let mut n = 0;
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => n += k,
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            s.write_all(n.to_string().as_bytes()).unwrap();
        });
        let window = Arc::new(Window::default());
        let w = window.clone();
        let reader = std::thread::spawn(move || {
            let (mut hello, mut reply, mut acks) = (false, Vec::new(), 0usize);
            while let Some((kind, id, data)) = read_frame(&mut from_bridge).unwrap() {
                match (kind, id) {
                    (ACK, 0) => hello = true,
                    (ACK, 1) => {
                        let n = ack_count(&data).unwrap();
                        acks += n;
                        w.ack(n);
                    }
                    (DATA, 1) => reply.extend_from_slice(&data),
                    (EOF, 1) => break,
                    other => panic!("unexpected frame {other:?}"),
                }
            }
            (hello, reply, acks)
        });
        frame(&mut to_bridge, OPEN, 1, &[1]);
        let chunk = vec![7u8; 64 << 10];
        let mut sent = 0;
        while sent < TOTAL {
            assert!(window.take(chunk.len()), "closed after {sent} bytes");
            frame(&mut to_bridge, DATA, 1, &chunk);
            sent += chunk.len();
        }
        frame(&mut to_bridge, EOF, 1, &[]);
        let (hello, reply, acks) = reader.join().unwrap();
        assert!(hello);
        assert_eq!(String::from_utf8(reply).unwrap(), TOTAL.to_string());
        // Everything acknowledged but the last part under ACK_EVERY.
        assert!(acks + ACK_EVERY > TOTAL && acks <= TOTAL, "{acks}");
    }

    /// The other way: a client that reads its output slowly holds the
    /// bridge to a window of it in flight.
    #[test]
    fn slow_reader_holds_output_to_the_window() {
        const TOTAL: usize = 64 << 20;
        let (mut to_bridge, mut from_bridge, _dir) = bridge("stdout", |mut s| {
            let chunk = vec![1u8; 64 << 10];
            for _ in 0..TOTAL / chunk.len() {
                s.write_all(&chunk).unwrap();
            }
        });
        frame(&mut to_bridge, OPEN, 1, &[1]);
        let (mut got, mut acked, mut most) = (0usize, 0usize, 0usize);
        while let Some((kind, id, data)) = read_frame(&mut from_bridge).unwrap() {
            match (kind, id) {
                (ACK, 0) => {}
                (DATA, 1) => {
                    got += data.len();
                    most = most.max(got - acked);
                    // Handed on slowly, and acknowledged as kiri-win does.
                    std::thread::sleep(Duration::from_micros(200));
                    if got - acked >= ACK_EVERY {
                        frame(&mut to_bridge, ACK, 1, &((got - acked) as u32).to_be_bytes());
                        acked = got;
                    }
                }
                (EOF, 1) => break,
                other => panic!("unexpected frame {other:?}"),
            }
        }
        assert_eq!(got, TOTAL);
        assert!(most <= WINDOW + (64 << 10), "{most} bytes in flight");
    }

    /// A kiri-win from before flow control: no credit asked, no ACKs sent
    /// for its connections, and they work as before.
    #[test]
    fn connections_without_credit() {
        let (mut to_bridge, mut from_bridge, _dir) = bridge("legacy", |mut s| {
            let mut req = Vec::new();
            s.read_to_end(&mut req).unwrap();
            s.write_all(&req).unwrap();
        });
        frame(&mut to_bridge, OPEN, 1, &[]);
        frame(&mut to_bridge, DATA, 1, b"ping");
        frame(&mut to_bridge, EOF, 1, &[]);
        let mut reply = Vec::new();
        while let Some((kind, id, data)) = read_frame(&mut from_bridge).unwrap() {
            match (kind, id) {
                (ACK, 0) => {}
                (DATA, 1) => reply.extend_from_slice(&data),
                (EOF, 1) => break,
                other => panic!("unexpected frame {other:?}"),
            }
        }
        assert_eq!(reply, b"ping");
    }
}
