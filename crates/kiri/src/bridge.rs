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
//! connections get the old bounded queue.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

/// A frame: kind, connection id (u32 BE), length (u32 BE), then the data.
/// The same as in crates/kiri-win.
const OPEN: u8 = 1;
const DATA: u8 = 2;
/// No more data from this side (a half-close).
const EOF: u8 = 3;
/// The connection is gone, both ways.
const CLOSE: u8 = 4;
/// Credit back: the data is how many bytes of this connection the sender
/// of the ACK has handed on (u32 BE). For connection 0, the bridge's hello.
const ACK: u8 = 5;

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

type Out = Arc<Mutex<dyn Write + Send>>;

pub fn run(args: &[String]) -> io::Result<()> {
    let socket = match args
        .iter()
        .position(|a| a == "--socket")
        .and_then(|i| args.get(i + 1))
    {
        Some(s) => PathBuf::from(s),
        None => crate::daemon::socket_path()?,
    };
    let out: Out = Arc::new(Mutex::new(BufWriter::with_capacity(
        64 * 1024,
        io::stdout(),
    )));
    serve(
        BufReader::with_capacity(64 * 1024, io::stdin()),
        &out,
        &socket,
    )
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
fn serve(mut input: impl Read, out: &Out, socket: &Path) -> io::Result<()> {
    // Hello: this bridge does flow control.
    send(out, ACK, 0, &[])?;
    let mut conns: HashMap<u32, Conn> = HashMap::new();
    while let Some((kind, id, data)) = read_frame(&mut input)? {
        match kind {
            OPEN => match UnixStream::connect(socket) {
                Ok(s) => {
                    let reader = s.try_clone()?;
                    let credit = data.first() == Some(&1);
                    let (tx, rx) = channel::<Op>();
                    let queued = Arc::new(AtomicUsize::new(0));
                    let window = credit.then(|| Arc::new(Window::default()));
                    conns.insert(
                        id,
                        Conn {
                            tx,
                            queued: queued.clone(),
                            window: window.clone(),
                        },
                    );
                    let acks = credit.then(|| out.clone());
                    std::thread::Builder::new()
                        .name("kiri-bridge-w".into())
                        .spawn(move || writer(id, s, &rx, &queued, acks.as_ref()))?;
                    let out = out.clone();
                    std::thread::Builder::new()
                        .name("kiri-bridge".into())
                        .spawn(move || replies(id, reader, &out, window.as_deref()))?;
                }
                Err(_) => send(out, CLOSE, id, &[])?,
            },
            ACK => {
                if let (Some(w), Some(n)) = (
                    conns.get(&id).and_then(|c| c.window.as_ref()),
                    ack_count(&data),
                ) {
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
                if !overflow {
                    let _ = c.tx.send(op);
                }
                if overflow || kind == CLOSE {
                    if let Some(c) = conns.remove(&id)
                        && let Some(w) = c.window
                    {
                        w.close();
                    }
                    if overflow {
                        send(out, CLOSE, id, &[])?;
                    }
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad bridge frame {kind}"),
                ));
            }
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

/// Writes one connection's input to the daemon, acknowledging it to
/// kiri-win (`acks`) as it goes. Ends, closing the connection, when asked
/// to or when its queue's sender is dropped. Once the daemon's side has
/// gone, input is still taken and acknowledged, unwritten, so kiri-win's
/// sender never waits on credit that cannot come.
fn writer(id: u32, mut s: UnixStream, rx: &Receiver<Op>, queued: &AtomicUsize, acks: Option<&Out>) {
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
                    && (done >= ACK_EVERY || broken)
                {
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
/// With a `window`, it waits for kiri-win's credit before each piece.
fn replies(id: u32, mut s: UnixStream, out: &Out, window: Option<&Window>) {
    let mut buf = vec![0u8; 64 * 1024];
    let end = loop {
        match s.read(&mut buf) {
            Ok(0) => break EOF,
            Ok(n) => {
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes"),
        ));
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
        assert_eq!(
            super::read_frame(&mut r).unwrap(),
            Some((super::DATA, 7, b"abc".to_vec()))
        );
        assert_eq!(
            super::read_frame(&mut r).unwrap(),
            Some((super::EOF, 7, vec![]))
        );
        assert_eq!(super::read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut wire = super::header(super::DATA, 7, super::MAX_FRAME).to_vec();
        wire.resize(wire.len() + super::MAX_FRAME, 0);
        assert_eq!(
            super::read_frame(&mut &wire[..])
                .unwrap()
                .map(|f| f.2.len()),
            Some(super::MAX_FRAME)
        );
        // 4 GiB claimed, nothing behind it: an error at once, not an allocation.
        let huge = super::header(super::DATA, 7, u32::MAX as usize);
        assert_eq!(
            super::read_frame(&mut &huge[..]).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    /// A bridge between socket pairs, to a "daemon" on its own socket that
    /// `daemon` serves (one connection). Returns kiri-win's ends: what it
    /// writes to the bridge, and what it reads from it.
    fn bridge(
        name: &str,
        daemon: impl FnOnce(UnixStream) + Send + 'static,
    ) -> (UnixStream, UnixStream) {
        let dir = std::env::temp_dir().join(format!("kiri-bridge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("d.sock");
        let l = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || daemon(l.accept().unwrap().0));
        let (to_bridge, input) = UnixStream::pair().unwrap();
        let (output, from_bridge) = UnixStream::pair().unwrap();
        let out: Out = Arc::new(Mutex::new(output));
        std::thread::spawn(move || {
            let _ = serve(input, &out, &sock);
            let _ = std::fs::remove_dir_all(&dir);
        });
        (to_bridge, from_bridge)
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
        let (mut to_bridge, mut from_bridge) = bridge("stdin", |mut s| {
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
        let (mut to_bridge, mut from_bridge) = bridge("stdout", |mut s| {
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
                        frame(
                            &mut to_bridge,
                            ACK,
                            1,
                            &((got - acked) as u32).to_be_bytes(),
                        );
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
        let (mut to_bridge, mut from_bridge) = bridge("legacy", |mut s| {
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
