//! Software USB devices for testing passthrough end to end without
//! hardware or root: `kiri usb fake serial|mouse` attaches one through the
//! daemon, exactly as `kiri usb serve` attaches a captured device, and the
//! guest's real USB stack and drivers take it from there.
//!
//! - `serial`: a CDC ACM serial port (the class of most Zigbee/Z-Wave
//!   sticks and boards) that echoes back whatever it is sent: bulk OUT and
//!   IN, line coding, and an interrupt endpoint that never fires (so every
//!   close has to unlink it).
//! - `mouse`: a HID boot mouse that moves right one step every 10 ms while
//!   something reads it, `--reports N` times.
//!
//! Faults, for the edge cases: `--unplug-after BYTES` (the device vanishes
//! mid-transfer once that much has been echoed), `--stall-once` (the first
//! bulk IN stalls, so the driver must clear the halt).

use std::collections::{HashSet, VecDeque};
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use super::usbip::{self, Backend, Done, Setup};

const VENDOR: u16 = 0x1209; // pid.codes' test vendor
const FULL_SPEED: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Serial,
    Mouse,
}

#[derive(Default)]
struct State {
    echo: VecDeque<u8>,
    echoed: usize,
    aborted: HashSet<u32>,
    closed: bool,
    stalled: bool,
    line_coding: Vec<u8>,
    reports_left: usize,
    halts_cleared: usize,
}

struct Fake {
    kind: Kind,
    st: Mutex<State>,
    cv: Condvar,
    unplug_after: Option<usize>,
    /// The session, to end from this side for `--unplug-after`.
    session: Mutex<Option<UnixStream>>,
}

fn device_descriptor(kind: Kind) -> Vec<u8> {
    let (class, product) = match kind {
        Kind::Serial => (0x02, 0x0001u16),
        Kind::Mouse => (0x00, 0x0002u16),
    };
    let mut d = vec![18, 1, 0x00, 0x02, class, 0, 0, 64];
    d.extend_from_slice(&VENDOR.to_le_bytes());
    d.extend_from_slice(&product.to_le_bytes());
    d.extend_from_slice(&[0x00, 0x01, 1, 2, 3, 1]);
    d
}

/// Configuration 1, with its interfaces, class descriptors and endpoints.
fn config_descriptor(kind: Kind) -> Vec<u8> {
    let body: Vec<u8> = match kind {
        Kind::Serial => [
            &[9, 4, 0, 0, 1, 0x02, 0x02, 0x01, 0][..], // communications interface, ACM
            &[5, 0x24, 0x00, 0x10, 0x01],             // CDC header
            &[5, 0x24, 0x01, 0x00, 0x01],             // call management
            &[4, 0x24, 0x02, 0x02],                   // ACM: line coding and serial state
            &[5, 0x24, 0x06, 0x00, 0x01],             // union: 0 controls 1
            &[7, 5, 0x81, 0x03, 8, 0, 16],            // notifications, interrupt IN
            &[9, 4, 1, 0, 2, 0x0a, 0, 0, 0],          // data interface
            &[7, 5, 0x82, 0x02, 64, 0, 0],            // bulk IN
            &[7, 5, 0x02, 0x02, 64, 0, 0],            // bulk OUT
        ]
        .concat(),
        Kind::Mouse => [
            &[9, 4, 0, 0, 1, 0x03, 0x01, 0x02, 0][..], // HID, boot, mouse
            &[9, 0x21, 0x11, 0x01, 0, 1, 0x22, REPORT.len() as u8, 0],
            &[7, 5, 0x81, 0x03, 4, 0, 10], // interrupt IN, 10 ms
        ]
        .concat(),
    };
    let interfaces = if kind == Kind::Serial { 2 } else { 1 };
    let total = (9 + body.len()) as u16;
    let mut d = vec![9, 2];
    d.extend_from_slice(&total.to_le_bytes());
    d.extend_from_slice(&[interfaces, 1, 0, 0x80, 50]);
    d.extend_from_slice(&body);
    d
}

/// Buttons (3 bits + padding), then X, Y and wheel as signed bytes.
const REPORT: [u8; 52] = [
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95,
    0x01, 0x75, 0x05, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, 0xc0, 0xc0,
];

fn string_descriptor(index: u8, kind: Kind) -> Option<Vec<u8>> {
    let text = match index {
        0 => return Some(vec![4, 3, 0x09, 0x04]),
        1 => "Kiri",
        2 if kind == Kind::Serial => "Kiri test serial",
        2 => "Kiri test mouse",
        3 => "KIRI0001",
        _ => return None,
    };
    let mut d = vec![0, 3];
    for u in text.encode_utf16() {
        d.extend_from_slice(&u.to_le_bytes());
    }
    d[0] = d.len() as u8;
    Some(d)
}

impl Fake {
    /// Waits until `ready` has something, or the URB is unlinked, or the
    /// session ends.
    fn wait<T>(&self, seq: u32, mut ready: impl FnMut(&mut State) -> Option<T>) -> Result<T, Done> {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if st.closed {
                return Err(Done::fail(usbip::ESHUTDOWN));
            }
            if st.aborted.remove(&seq) {
                return Err(Done::fail(usbip::ECONNRESET));
            }
            if let Some(v) = ready(&mut st) {
                return Ok(v);
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn unplug(&self) {
        if let Some(s) = self.session.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

impl Backend for Fake {
    fn control(&self, _seq: u32, s: &Setup, out: &[u8]) -> Done {
        let kind = self.kind;
        let (desc_type, desc_index) = ((s.value >> 8) as u8, s.value as u8);
        match (s.request_type, s.request) {
            (0x80, 6) => match desc_type {
                1 => Done::read(device_descriptor(kind)),
                2 => Done::read(config_descriptor(kind)),
                3 => string_descriptor(desc_index, kind).map_or(Done::fail(usbip::EPIPE), Done::read),
                _ => Done::fail(usbip::EPIPE),
            },
            (0x81, 6) if kind == Kind::Mouse && desc_type == 0x22 => Done::read(REPORT.to_vec()),
            (0x80, 0) | (0x81, 0) | (0x82, 0) => Done::read(vec![0, 0]),
            (0x80, 8) => Done::read(vec![1]),
            // HID: SET_IDLE, SET_PROTOCOL.
            (0x21, 0x0a) | (0x21, 0x0b) if kind == Kind::Mouse => Done::wrote(0),
            // CDC: SET_LINE_CODING, GET_LINE_CODING, SET_CONTROL_LINE_STATE, SEND_BREAK.
            (0x21, 0x20) if kind == Kind::Serial => {
                self.st.lock().unwrap_or_else(|e| e.into_inner()).line_coding = out.to_vec();
                Done::wrote(out.len())
            }
            (0xa1, 0x21) if kind == Kind::Serial => {
                let lc = self.st.lock().unwrap_or_else(|e| e.into_inner()).line_coding.clone();
                Done::read(if lc.len() == 7 { lc } else { vec![0x00, 0xc2, 0x01, 0x00, 0, 0, 8] })
            }
            (0x21, 0x22) | (0x21, 0x23) if kind == Kind::Serial => Done::wrote(0),
            _ => Done::fail(usbip::EPIPE),
        }
    }

    fn set_configuration(&self, value: u8) -> i32 {
        if value <= 1 { 0 } else { -usbip::EINVAL }
    }

    fn set_interface(&self, _interface: u8, alt: u8) -> i32 {
        if alt == 0 { 0 } else { -usbip::EINVAL }
    }

    fn clear_halt(&self, _endpoint: u8) -> i32 {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.halts_cleared += 1;
        0
    }

    fn transfer(&self, seq: u32, endpoint: u8, out: &[u8], len: usize) -> Done {
        match (self.kind, endpoint) {
            (Kind::Serial, 0x02) => {
                let unplug = {
                    let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
                    if st.closed {
                        return Done::fail(usbip::ESHUTDOWN);
                    }
                    st.echo.extend(out);
                    st.echoed += out.len();
                    self.cv.notify_all();
                    self.unplug_after.is_some_and(|n| st.echoed >= n)
                };
                if unplug {
                    self.unplug();
                    return Done::fail(usbip::ENODEV);
                }
                Done::wrote(out.len())
            }
            (Kind::Serial, 0x82) => {
                let stall = {
                    let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::replace(&mut st.stalled, false)
                };
                if stall {
                    return Done::fail(usbip::EPIPE);
                }
                match self.wait(seq, |st| (!st.echo.is_empty()).then(|| st.echo.drain(..len.min(st.echo.len())).collect::<Vec<u8>>())) {
                    Ok(data) => Done::read(data),
                    Err(d) => d,
                }
            }
            // Serial notifications: nothing to say, ever.
            (Kind::Serial, 0x81) => self.wait::<()>(seq, |_| None).err().unwrap_or_default(),
            (Kind::Mouse, 0x81) => {
                // One report per poll interval while any are left.
                let more = {
                    let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
                    st.reports_left > 0
                };
                if more {
                    std::thread::sleep(Duration::from_millis(10));
                }
                match self.wait(seq, |st| {
                    (st.reports_left > 0).then(|| {
                        st.reports_left -= 1;
                        vec![0, 1, 0, 0]
                    })
                }) {
                    Ok(r) => Done::read(r),
                    Err(d) => d,
                }
            }
            _ => Done::fail(usbip::EPIPE),
        }
    }

    fn abort(&self, seq: u32, _endpoint: u8) {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).aborted.insert(seq);
        self.cv.notify_all();
    }

    fn close(&self) {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        self.cv.notify_all();
    }
}

/// `kiri usb fake serial|mouse [--location L] [--unplug-after BYTES]
/// [--stall-once] [--reports N]`: runs until detached.
pub fn run(args: &[String]) -> io::Result<()> {
    let kind = match args.first().map(String::as_str) {
        Some("serial") => Kind::Serial,
        Some("mouse") => Kind::Mouse,
        _ => return Err(io::Error::other("usage: kiri usb fake serial|mouse [--location L] [--unplug-after BYTES] [--stall-once] [--reports N]")),
    };
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let location = opt("--location").unwrap_or_else(|| format!("fake-{}", std::process::id()));
    if location.is_empty() || !location.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(io::Error::other("--location takes letters, digits and dashes"));
    }
    let fake = std::sync::Arc::new(Fake {
        kind,
        st: Mutex::new(State {
            stalled: args.iter().any(|a| a == "--stall-once"),
            reports_left: opt("--reports").and_then(|n| n.parse().ok()).unwrap_or(200),
            ..State::default()
        }),
        cv: Condvar::new(),
        unplug_after: opt("--unplug-after").and_then(|n| n.parse().ok()),
        session: Mutex::new(None),
    });
    let name = if kind == Kind::Serial { "Kiri test serial" } else { "Kiri test mouse" };
    let (stream, reader) = usbip::connect(&crate::client::socket()?, &location, name, FULL_SPEED, 0x7f << 16 | (std::process::id() & 0x7f))?;
    *fake.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(stream.try_clone()?);
    eprintln!("kiri usb: {name} attached as {location}");
    let r = usbip::serve(fake.clone(), reader, stream);
    let st = fake.st.lock().unwrap_or_else(|e| e.into_inner());
    eprintln!("kiri usb: {name} detached ({} bytes echoed, {} halts cleared)", st.echoed, st.halts_cleared);
    r
}
