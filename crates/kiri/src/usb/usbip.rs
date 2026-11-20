//! USB/IP, the device side: what the guest's vhci_hcd sends once a device
//! is attached, and what it expects back. Hardware-free, so it is tested on
//! its own; `mac.rs` puts a captured Mac device behind it, `fake.rs` a
//! software one for the end-to-end tests.
//!
//! After attach the guest sends two commands, both with a 48-byte header,
//! all integers big-endian:
//!
//! - CMD_SUBMIT: one URB (a control, bulk or interrupt transfer), followed
//!   by its data when it is an OUT. Answered with RET_SUBMIT: a status, the
//!   bytes transferred and, for an IN, the data.
//! - CMD_UNLINK: cancel the URB with seqnum `unlink_seqnum`. Answered with
//!   RET_UNLINK: -ECONNRESET if it was cancelled (and then it gets no
//!   RET_SUBMIT), 0 if it had already been answered.
//!
//! Each endpoint's URBs run in order, on a thread of their own; control
//! requests share one queue. Requests that change the device's state go to
//! the backend's own calls (SET_CONFIGURATION, SET_INTERFACE,
//! CLEAR_FEATURE(ENDPOINT_HALT)), as Linux's own usbip-host does.
//! Isochronous URBs are refused with -EINVAL.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CMD_SUBMIT: u32 = 1;
const CMD_UNLINK: u32 = 2;
const RET_SUBMIT: u32 = 3;
const RET_UNLINK: u32 = 4;
const DIR_IN: u32 = 1;

pub const EPIPE: i32 = 32;
pub const EINVAL: i32 = 22;
pub const ENODEV: i32 = 19;
pub const ESHUTDOWN: i32 = 108;
pub const ECONNRESET: i32 = 104;

/// The largest transfer accepted. Linux's own drivers stay far below; a
/// bigger one is a broken or hostile peer, and the session ends.
pub const MAX_TRANSFER: usize = 16 << 20;
/// Isochronous packets in one URB, at most (Linux allows 128 per URB on
/// full speed, 1024 on high): more is a broken peer.
const MAX_PACKETS: usize = 1024;

/// A USB setup packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    pub fn parse(s: &[u8; 8]) -> Setup {
        Setup {
            request_type: s[0],
            request: s[1],
            value: u16::from_le_bytes([s[2], s[3]]),
            index: u16::from_le_bytes([s[4], s[5]]),
            length: u16::from_le_bytes([s[6], s[7]]),
        }
    }
}

/// How a transfer ended: a status (0 or a negative errno), the data of an
/// IN, and the bytes transferred.
#[derive(Debug, Default)]
pub struct Done {
    pub status: i32,
    pub data: Vec<u8>,
    pub actual: usize,
}

impl Done {
    pub fn read(data: Vec<u8>) -> Done {
        Done { status: 0, actual: data.len(), data }
    }

    pub fn wrote(n: usize) -> Done {
        Done { status: 0, data: Vec::new(), actual: n }
    }

    pub fn fail(errno: i32) -> Done {
        Done { status: -errno.abs(), data: Vec::new(), actual: 0 }
    }
}

/// A device behind USB/IP. Every call may block; `abort` and `close` come
/// from another thread and must make a blocked call return.
pub trait Backend: Send + Sync + 'static {
    /// A control request on endpoint 0, other than the three below.
    fn control(&self, seq: u32, setup: &Setup, out: &[u8]) -> Done;
    fn set_configuration(&self, value: u8) -> i32;
    fn set_interface(&self, interface: u8, alt: u8) -> i32;
    fn clear_halt(&self, endpoint: u8) -> i32;
    /// A bulk or interrupt transfer: `out` for an OUT endpoint, `len` bytes
    /// at most for an IN one (address with 0x80).
    fn transfer(&self, seq: u32, endpoint: u8, out: &[u8], len: usize) -> Done;
    /// Cancel URB `seq`, running on `endpoint` (0 for control).
    fn abort(&self, seq: u32, endpoint: u8);
    /// The session ended: make every blocked call return.
    fn close(&self);
}

struct Urb {
    seq: u32,
    dir_in: bool,
    ep: u8,
    len: usize,
    setup: [u8; 8],
    data: Vec<u8>,
}

impl Urb {
    fn address(&self) -> u8 {
        if self.ep == 0 { 0 } else { self.ep | if self.dir_in { 0x80 } else { 0 } }
    }
}

enum State {
    Queued,
    /// Running on this endpoint address.
    Running(u8),
    /// Unlinked while running: answer RET_UNLINK with this seqnum, not RET_SUBMIT.
    Unlinking(u32),
    /// Unlinked before it ran: already answered; never run it.
    Dropped,
}

struct Session<B: ?Sized> {
    backend: Arc<B>,
    states: Mutex<HashMap<u32, State>>,
    out: Mutex<Box<dyn Write + Send>>,
}

impl<B: Backend + ?Sized> Session<B> {
    fn send(&self, msg: &[u8]) {
        // A closed peer ends the read loop; nothing to do here.
        let _ = self.out.lock().unwrap_or_else(|e| e.into_inner()).write_all(msg);
    }

    fn ret_submit(&self, seq: u32, d: &Done) {
        let mut m = Vec::with_capacity(48 + d.data.len());
        for v in [RET_SUBMIT, seq, 0, 0, 0] {
            m.extend_from_slice(&v.to_be_bytes());
        }
        for v in [d.status, d.actual as i32, 0, 0, 0] {
            m.extend_from_slice(&v.to_be_bytes());
        }
        m.extend_from_slice(&[0; 8]);
        m.extend_from_slice(&d.data);
        self.send(&m);
    }

    fn ret_unlink(&self, seq: u32, status: i32) {
        let mut m = Vec::with_capacity(48);
        for v in [RET_UNLINK, seq, 0, 0, 0] {
            m.extend_from_slice(&v.to_be_bytes());
        }
        m.extend_from_slice(&status.to_be_bytes());
        m.extend_from_slice(&[0; 24]);
        self.send(&m);
    }

    fn run(&self, u: Urb) {
        {
            let mut st = self.states.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(st.get(&u.seq), Some(State::Dropped)) {
                st.remove(&u.seq);
                return;
            }
            st.insert(u.seq, State::Running(u.address()));
        }
        let mut d = self.dispatch(&u);
        if u.dir_in {
            d.data.truncate(u.len);
            d.actual = d.data.len();
        } else {
            d.data.clear();
        }
        let state = self.states.lock().unwrap_or_else(|e| e.into_inner()).remove(&u.seq);
        match state {
            Some(State::Unlinking(unlink_seq)) => self.ret_unlink(unlink_seq, -ECONNRESET),
            _ => self.ret_submit(u.seq, &d),
        }
    }

    fn dispatch(&self, u: &Urb) -> Done {
        let b = &self.backend;
        if u.ep != 0 {
            return b.transfer(u.seq, u.address(), &u.data, u.len);
        }
        let s = Setup::parse(&u.setup);
        let status = |code: i32| Done { status: code, ..Done::default() };
        match (s.request_type, s.request) {
            (0x00, 9) => status(b.set_configuration(s.value as u8)),
            (0x01, 11) => status(b.set_interface(s.index as u8, s.value as u8)),
            (0x02, 1) if s.value == 0 => status(b.clear_halt(s.index as u8)),
            _ => {
                let mut d = b.control(u.seq, &s, &u.data);
                if u.dir_in {
                    d.data.truncate(s.length as usize);
                }
                d
            }
        }
    }

    /// CMD_UNLINK. The state lock is held across the abort, so the target
    /// can't finish and let the next URB on its endpoint start meanwhile:
    /// the abort only ever hits the URB it was meant for.
    fn unlink(&self, seq: u32, target: u32) {
        let mut st = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match st.get(&target) {
            Some(State::Queued) => {
                st.insert(target, State::Dropped);
                drop(st);
                self.ret_unlink(seq, -ECONNRESET);
            }
            Some(&State::Running(address)) => {
                st.insert(target, State::Unlinking(seq));
                self.backend.abort(target, address);
            }
            // Already answered, or never seen.
            _ => {
                drop(st);
                self.ret_unlink(seq, 0);
            }
        }
    }
}

/// Serves `backend` on a USB/IP stream until the guest closes it (or the
/// stream breaks, or the guest breaks the protocol). On the way out the
/// backend is closed and its endpoint threads are given 2 s to finish.
pub fn serve<B: Backend + ?Sized>(backend: Arc<B>, mut input: impl Read, output: impl Write + Send + 'static) -> io::Result<()> {
    let session = Arc::new(Session { backend: backend.clone(), states: Mutex::new(HashMap::new()), out: Mutex::new(Box::new(output)) });
    let mut workers: HashMap<u8, Sender<Urb>> = HashMap::new();
    let (finished_tx, finished) = mpsc::channel::<()>();
    let result = read_loop(&session, &mut input, &mut workers, &finished_tx);
    backend.close();
    let n = workers.len();
    drop(workers);
    for _ in 0..n {
        if finished.recv_timeout(Duration::from_secs(2)).is_err() {
            break;
        }
    }
    result
}

fn read_loop<B: Backend + ?Sized>(
    session: &Arc<Session<B>>,
    input: &mut impl Read,
    workers: &mut HashMap<u8, Sender<Urb>>,
    finished: &Sender<()>,
) -> io::Result<()> {
    let mut head = [0u8; 48];
    loop {
        match input.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let be = |i: usize| u32::from_be_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let (cmd, seq) = (be(0), be(4));
        match cmd {
            CMD_SUBMIT => {
                let dir_in = be(12) == DIR_IN;
                let ep = be(16);
                let len = be(24) as i32;
                let packets = be(32) as i32;
                if ep > 15 || len < 0 || len as usize > MAX_TRANSFER || packets as usize > MAX_PACKETS {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad URB: ep {ep}, length {len}, {packets} packets")));
                }
                let mut setup = [0u8; 8];
                setup.copy_from_slice(&head[40..48]);
                let mut data = Vec::new();
                if !dir_in && len > 0 {
                    data.resize(len as usize, 0);
                    input.read_exact(&mut data)?;
                }
                // number_of_packets is 0 for non-isochronous URBs (Linux
                // sends 0; some peers send -1).
                if packets > 0 {
                    let mut iso = vec![0u8; packets as usize * 16];
                    input.read_exact(&mut iso)?;
                    session.ret_submit(seq, &Done::fail(EINVAL));
                    continue;
                }
                session.states.lock().unwrap_or_else(|e| e.into_inner()).insert(seq, State::Queued);
                let urb = Urb { seq, dir_in, ep: ep as u8, len: len as usize, setup, data };
                let key = urb.address();
                let tx = workers.entry(key).or_insert_with(|| {
                    let (tx, rx) = mpsc::channel::<Urb>();
                    let s = session.clone();
                    let done = finished.clone();
                    let spawned = std::thread::Builder::new().name(format!("kiri-usb-ep{key:02x}")).spawn(move || {
                        for u in rx {
                            s.run(u);
                        }
                        let _ = done.send(());
                    });
                    if let Err(e) = spawned {
                        eprintln!("kiri usb: no thread for endpoint {key:02x}: {e}");
                    }
                    tx
                });
                if tx.send(urb).is_err() {
                    session.states.lock().unwrap_or_else(|e| e.into_inner()).remove(&seq);
                    session.ret_submit(seq, &Done::fail(ESHUTDOWN));
                }
            }
            CMD_UNLINK => session.unlink(seq, be(20)),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown USB/IP command {cmd}"))),
        }
    }
}

/// Asks the daemon at `socket` to attach a device, then returns the
/// upgraded connection that carries its USB/IP. `speed` is the kernel's
/// USB_SPEED_*, `devid` USB/IP's bus << 16 | address.
pub fn connect(socket: &Path, location: &str, name: &str, speed: u32, devid: u32) -> io::Result<(UnixStream, BufReader<UnixStream>)> {
    let stream = UnixStream::connect(socket)?;
    // Never hold a device on a daemon that doesn't answer.
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '+' }).collect();
    write!(
        &stream,
        "POST /kiri/usb?location={location}&name={name}&speed={speed}&devid={devid} HTTP/1.1\r\nHost: kiri\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 0\r\n\r\n"
    )?;
    let mut r = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut length = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim_end().is_empty() {
            break;
        }
        if let Some(v) = h.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.trim().to_owned()) {
            length = v.parse().unwrap_or(0);
        }
    }
    if status != 101 {
        let mut body = vec![0u8; length.min(64 * 1024)];
        let _ = r.read_exact(&mut body);
        let text = String::from_utf8_lossy(&body);
        let msg = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["message"].as_str().map(str::to_owned)).unwrap_or_else(|| text.trim().to_owned());
        return Err(io::Error::other(format!("Kiri did not take the device: {msg}")));
    }
    stream.set_read_timeout(None)?;
    Ok((stream, r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::time::Instant;

    /// Endpoint 0x81 blocks until aborted; 0x02 records what it gets; 0x82
    /// answers with it; control IN answers with its own setup bytes.
    #[derive(Default)]
    struct Toy {
        inner: Mutex<ToyState>,
        cv: Condvar,
    }

    #[derive(Default)]
    struct ToyState {
        aborted: Vec<u32>,
        closed: bool,
        written: Vec<Vec<u8>>,
        calls: Vec<String>,
    }

    impl Backend for Toy {
        fn control(&self, _: u32, s: &Setup, out: &[u8]) -> Done {
            self.inner.lock().unwrap().calls.push(format!("control {:02x} {:02x} out={}", s.request_type, s.request, out.len()));
            if s.request_type & 0x80 != 0 { Done::read(vec![s.request_type, s.request, 1, 2, 3, 4, 5, 6, 7, 8]) } else { Done::wrote(out.len()) }
        }
        fn set_configuration(&self, v: u8) -> i32 {
            self.inner.lock().unwrap().calls.push(format!("config {v}"));
            0
        }
        fn set_interface(&self, i: u8, a: u8) -> i32 {
            self.inner.lock().unwrap().calls.push(format!("interface {i} {a}"));
            0
        }
        fn clear_halt(&self, ep: u8) -> i32 {
            self.inner.lock().unwrap().calls.push(format!("clear {ep:02x}"));
            0
        }
        fn transfer(&self, seq: u32, ep: u8, out: &[u8], len: usize) -> Done {
            let mut st = self.inner.lock().unwrap();
            st.calls.push(format!("transfer {seq}"));
            match ep {
                0x02 => {
                    st.written.push(out.to_vec());
                    Done::wrote(out.len())
                }
                0x82 => Done::read(vec![7; len]),
                _ => loop {
                    if st.closed {
                        return Done::fail(ESHUTDOWN);
                    }
                    if let Some(i) = st.aborted.iter().position(|s| *s == seq) {
                        st.aborted.remove(i);
                        return Done::fail(ECONNRESET);
                    }
                    st = self.cv.wait(st).unwrap();
                },
            }
        }
        fn abort(&self, seq: u32, _: u8) {
            self.inner.lock().unwrap().aborted.push(seq);
            self.cv.notify_all();
        }
        fn close(&self) {
            self.inner.lock().unwrap().closed = true;
            self.cv.notify_all();
        }
    }

    struct Peer {
        s: UnixStream,
        toy: Arc<Toy>,
        server: Option<std::thread::JoinHandle<io::Result<()>>>,
    }

    fn start() -> Peer {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let toy = Arc::new(Toy::default());
        let t = toy.clone();
        let w = a.try_clone().unwrap();
        let server = std::thread::spawn(move || serve(t, a, w));
        Peer { s: b, toy, server: Some(server) }
    }

    fn header(words: [u32; 10], setup: [u8; 8]) -> Vec<u8> {
        let mut m: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        m.extend_from_slice(&setup);
        m
    }

    impl Peer {
        fn submit(&mut self, seq: u32, dir_in: bool, ep: u32, len: u32, setup: [u8; 8], data: &[u8]) {
            let mut m = header([CMD_SUBMIT, seq, 0x10002, dir_in as u32, ep, 0, len, 0, 0, 0], setup);
            m.extend_from_slice(data);
            self.s.write_all(&m).unwrap();
        }
        fn unlink(&mut self, seq: u32, target: u32) {
            self.s.write_all(&header([CMD_UNLINK, seq, 0x10002, 0, 0, target, 0, 0, 0, 0], [0; 8])).unwrap();
        }
        /// (command, seq, status, actual, data); `has_data` for an IN's RET_SUBMIT.
        fn reply_in(&mut self, has_data: bool) -> (u32, u32, i32, usize, Vec<u8>) {
            let mut h = [0u8; 48];
            self.s.read_exact(&mut h).unwrap();
            let be = |i: usize| u32::from_be_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
            let (cmd, seq, status, actual) = (be(0), be(4), be(20) as i32, be(24) as usize);
            let mut data = Vec::new();
            if cmd == RET_SUBMIT && actual > 0 && has_data {
                data.resize(actual, 0);
                self.s.read_exact(&mut data).unwrap();
            }
            (cmd, seq, status, actual, data)
        }
        fn expect_quiet(&mut self) {
            self.s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
            let mut b = [0u8; 1];
            assert!(self.s.read(&mut b).is_err(), "expected no reply");
            self.s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        }
    }

    impl Peer {
        fn reply(&mut self) -> (u32, u32, i32, usize, Vec<u8>) {
            self.reply_in(false)
        }
    }

    const GET_DESC: [u8; 8] = [0x80, 6, 0, 1, 0, 0, 18, 0];

    #[test]
    fn control_in_answers_with_data_truncated_to_wlength() {
        let mut p = start();
        p.submit(1, true, 0, 64, [0x80, 6, 0, 1, 0, 0, 4, 0], &[]);
        let (cmd, seq, status, actual, data) = p.reply_in(true);
        assert_eq!((cmd, seq, status, actual), (RET_SUBMIT, 1, 0, 4));
        assert_eq!(data, vec![0x80, 6, 1, 2]);
    }

    #[test]
    fn state_changing_requests_go_to_the_backend_calls() {
        let mut p = start();
        p.submit(1, false, 0, 0, [0x00, 9, 1, 0, 0, 0, 0, 0], &[]);
        p.submit(2, false, 0, 0, [0x01, 11, 2, 0, 3, 0, 0, 0], &[]);
        p.submit(3, false, 0, 0, [0x02, 1, 0, 0, 0x81, 0, 0, 0], &[]);
        p.submit(4, false, 0, 2, [0x21, 0x22, 3, 0, 0, 0, 2, 0], &[9, 9]);
        for want in 1..=4 {
            let (cmd, seq, status, _, _) = p.reply();
            assert_eq!((cmd, seq, status), (RET_SUBMIT, want, 0));
        }
        assert_eq!(p.toy.inner.lock().unwrap().calls, vec!["config 1", "interface 3 2", "clear 81", "control 21 22 out=2"]);
        assert_eq!(p.toy.inner.lock().unwrap().written.len(), 0);
    }

    #[test]
    fn unlinking_a_running_urb_answers_ret_unlink_and_no_ret_submit() {
        let mut p = start();
        p.submit(5, true, 1, 8, [0; 8], &[]);
        std::thread::sleep(Duration::from_millis(50));
        p.unlink(6, 5);
        let (cmd, seq, status, _, _) = p.reply();
        assert_eq!((cmd, seq, status), (RET_UNLINK, 6, -ECONNRESET));
        p.expect_quiet();
        // The endpoint still works afterwards.
        p.submit(7, true, 1, 8, [0; 8], &[]);
        std::thread::sleep(Duration::from_millis(50));
        p.unlink(8, 7);
        assert_eq!(p.reply().1, 8);
    }

    #[test]
    fn unlinking_a_queued_urb_answers_at_once_and_never_runs_it() {
        let mut p = start();
        p.submit(1, true, 1, 8, [0; 8], &[]); // blocks the endpoint
        p.submit(2, true, 1, 8, [0; 8], &[]); // queued behind it
        std::thread::sleep(Duration::from_millis(50));
        p.unlink(3, 2);
        let (cmd, seq, status, _, _) = p.reply();
        assert_eq!((cmd, seq, status), (RET_UNLINK, 3, -ECONNRESET));
        p.unlink(4, 1);
        let (cmd, seq, status, _, _) = p.reply();
        assert_eq!((cmd, seq, status), (RET_UNLINK, 4, -ECONNRESET));
        p.expect_quiet();
        assert!(!p.toy.inner.lock().unwrap().calls.contains(&"transfer 2".to_owned()));
    }

    #[test]
    fn unlinking_an_answered_urb_answers_zero() {
        let mut p = start();
        p.submit(1, true, 0, 18, GET_DESC, &[]);
        assert_eq!(p.reply_in(true).0, RET_SUBMIT);
        p.unlink(2, 1);
        let (cmd, seq, status, _, _) = p.reply();
        assert_eq!((cmd, seq, status), (RET_UNLINK, 2, 0));
    }

    #[test]
    fn bulk_out_keeps_its_order() {
        let mut p = start();
        for i in 0..200u32 {
            p.submit(i + 1, false, 2, 4, [0; 8], &i.to_le_bytes());
        }
        for i in 0..200u32 {
            let (cmd, seq, status, actual, _) = p.reply();
            assert_eq!((cmd, seq, status, actual), (RET_SUBMIT, i + 1, 0, 4));
        }
        let written = p.toy.inner.lock().unwrap().written.clone();
        assert_eq!(written, (0..200u32).map(|i| i.to_le_bytes().to_vec()).collect::<Vec<_>>());
    }

    #[test]
    fn isochronous_is_refused_and_the_stream_stays_in_step() {
        let mut p = start();
        let mut m = header([CMD_SUBMIT, 1, 0x10002, 0, 3, 0, 4, 0, 2, 0], [0; 8]);
        m.extend_from_slice(&[1, 2, 3, 4]); // data
        m.extend_from_slice(&[0; 32]); // two packet descriptors
        p.s.write_all(&m).unwrap();
        let (cmd, seq, status, _, _) = p.reply();
        assert_eq!((cmd, seq, status), (RET_SUBMIT, 1, -EINVAL));
        p.submit(2, true, 0, 18, GET_DESC, &[]);
        assert_eq!(p.reply_in(true).1, 2);
    }

    #[test]
    fn a_huge_length_ends_the_session() {
        let mut p = start();
        p.s.write_all(&header([CMD_SUBMIT, 1, 0, 0, 2, 0, 0x7fff_ffff, 0, 0, 0], [0; 8])).unwrap();
        let r = p.server.take().unwrap().join().unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn eof_closes_the_backend_and_returns_with_a_transfer_blocked() {
        let mut p = start();
        p.submit(1, true, 1, 8, [0; 8], &[]);
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        p.s.shutdown(std::net::Shutdown::Both).unwrap();
        p.server.take().unwrap().join().unwrap().unwrap();
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(p.toy.inner.lock().unwrap().closed);
    }
}
