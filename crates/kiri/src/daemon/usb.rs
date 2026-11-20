//! USB passthrough, the daemon's part: a pipe between `kiri usb serve` (the
//! root helper that owns the captured device and speaks USB/IP for it) and
//! the guest's vhci_hcd, plus the list of what is attached.
//!
//! `POST /kiri/usb?location=…&name=…&speed=…&devid=…` opens the guest's USB
//! port, asks it to attach, then upgrades the connection and copies bytes
//! both ways until either side closes. `GET /kiri/usb` lists the attached
//! devices; `DELETE /kiri/usb/<location>` detaches one by closing its pipe,
//! which makes the helper give the device back to macOS.
//!
//! A device stays attached until it is detached: one that leaves the Mac's
//! bus on its own (a board that resets and enumerates again, a device the
//! Mac's sleep and wake reset, a cable pulled and put back) is attached
//! again when it comes back, found by vendor:product and serial number, or
//! by the port it was in when it has no serial number. See `left`.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::api::{Next, error, reply};
use super::engine::{Engine, Error};
use super::http::{self, Request};

struct Session {
    location: String,
    name: String,
    port: i32,
    /// When it was attached (Unix seconds), for the window's Attached column.
    since: i64,
    /// Both ends, so a detach (or the VM going away) can close them.
    helper: UnixStream,
    guest: OwnedFd,
    /// Set by `detach`: the session ends because someone asked.
    asked: Arc<AtomicBool>,
    /// Who the device is, on the Mac's bus (none for a test device).
    ident: Option<Ident>,
}

static SESSIONS: Mutex<Vec<Session>> = Mutex::new(Vec::new());
/// Locations being attached right now.
static PENDING: Mutex<std::collections::BTreeSet<String>> = Mutex::new(std::collections::BTreeSet::new());

/// Whether a device is attached: the VM must not sleep under it.
pub fn any_attached() -> bool {
    !SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
}

/// The attached devices, each with where it shows up in the VM (`Nodes`:
/// `[{Path, What}]`, empty while the guest is still probing it or with an
/// older agent).
pub fn list(engine: &Engine) -> Value {
    let rows: Vec<(String, String, i32, i64)> =
        SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|s| (s.location.clone(), s.name.clone(), s.port, s.since)).collect();
    // A device is attached only to a running VM: never boot one to ask.
    let lines = if rows.is_empty() { Vec::new() } else { engine.vm().and_then(|vm| vm.usb_nodes()).unwrap_or_default() };
    let mut out: Vec<Value> =
        rows.into_iter().map(|(location, name, port, since)| json!({"Location": location, "Name": name, "Port": port, "Since": since, "Nodes": nodes_on(&lines, port)})).collect();
    // Devices that left and are waited for: listed, so they can be let go
    // (`DELETE /kiri/usb/<location>`) before they come back.
    out.extend(waiting().into_iter().map(|(location, name)| json!({"Location": location, "Name": name, "Waiting": true, "Nodes": []})));
    Value::Array(out)
}

/// The devices waited for and not attached, as (location, name): where
/// each was when it left.
fn waiting() -> Vec<(String, String)> {
    let busy: Vec<Ident> = SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).iter().filter_map(|s| s.ident.clone()).collect();
    WANTED.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|x| x.is_waiting(&busy)).map(|x| (x.ident.location.clone(), x.name.clone())).collect()
}

/// The `{Path, What}` of the guest's `PORT\tPATH\tWHAT` lines for one port.
fn nodes_on(lines: &[String], port: i32) -> Vec<Value> {
    lines
        .iter()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let (p, path, what) = (f.next()?, f.next()?, f.next()?);
            (p.parse::<i32>().ok() == Some(port)).then(|| json!({"Path": path, "What": what}))
        })
        .collect()
}

pub fn detach(location: &str) -> Result<Value, Error> {
    let s = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(found) = s.iter().find(|s| s.location == location) else {
        // Waiting to come back: detaching it means it stays on the Mac.
        if forget(|w| w.ident.location == location) {
            return Ok(json!({"Location": location}));
        }
        return Err(Error::NotFound(format!("no USB device {location} is attached")));
    };
    found.asked.store(true, Ordering::SeqCst);
    if let Some(ident) = &found.ident {
        forget(|w| w.ident.same(ident));
    }
    let _ = found.helper.shutdown(Shutdown::Both);
    // SAFETY: shutting down a socket we own; the session's copy threads
    // see the end and remove it.
    unsafe { libc::shutdown(found.guest.as_raw_fd(), libc::SHUT_RDWR) };
    Ok(json!({"Location": location}))
}

/// Detaches every device and waits (up to 5 s) for their sessions to end:
/// for Stop and sleep, which would otherwise unplug them mid-transfer.
pub fn detach_all() {
    forget(|_| true);
    let locations: Vec<String> = SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|s| s.location.clone()).collect();
    for l in &locations {
        let _ = detach(l);
    }
    for _ in 0..50 {
        if !any_attached() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn attach(req: &Request, engine: &Arc<Engine>, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<Next> {
    let location = req.query("location").unwrap_or_default().to_owned();
    let name: String = req.query("name").unwrap_or_default().replace('+', " ").chars().filter(|c| !c.is_control()).take(80).collect();
    let speed: u32 = req.query("speed").and_then(|s| s.parse().ok()).unwrap_or(0);
    let devid: u32 = req.query("devid").and_then(|s| s.parse().ok()).unwrap_or(0);
    if location.is_empty() || location.len() > 64 || !location.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || !(1..=6).contains(&speed) {
        return reply(w, Err(Error::BadRequest("location (letters, digits, dashes) and speed (1-6) are required".into())), 200);
    }
    // Claimed before the guest is asked, so two attaches of one device
    // can't both get through. Check the live session first (no side
    // effects), then claim: inserting into PENDING must not happen
    // before any early return, or the location would be stuck pending.
    if SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|s| s.location == location) {
        return reply(w, Err(Error::Conflict(format!("USB device {location} is already attached"))), 200);
    }
    if !PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(location.clone()) {
        return reply(w, Err(Error::Conflict(format!("USB device {location} is already attached"))), 200);
    }
    let result = attach_claimed(&location, &name, speed, devid, engine, w, r);
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).remove(&location);
    result
}

#[allow(clippy::too_many_arguments)]
fn attach_claimed(location: &str, name: &str, speed: u32, devid: u32, engine: &Arc<Engine>, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<Next> {
    let (location, name) = (location.to_owned(), name.to_owned());
    let opened = (|| -> io::Result<(kiri_vmm::VsockStream, i32)> {
        let vm = engine.vm()?;
        // A VM this request just woke may not have its agent listening yet.
        let mut tries = 0;
        let stream = loop {
            match vm.connect(kiri_proto::USB_PORT) {
                Ok(s) => break s,
                Err(_) if tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(e),
            }
        };
        let mut f = File::from(stream.fd.try_clone()?);
        let mut head = [0u8; 8];
        head[..4].copy_from_slice(&speed.to_le_bytes());
        head[4..].copy_from_slice(&devid.to_le_bytes());
        f.write_all(&head)?;
        let mut answer = [0u8; 4];
        set_timeout(&stream.fd, Some(Duration::from_secs(5)));
        f.read_exact(&mut answer)?;
        set_timeout(&stream.fd, None);
        let port = i32::from_le_bytes(answer);
        if port < 0 {
            return Err(io::Error::other(format!("the VM could not attach the device: {}", io::Error::from_raw_os_error(-port))));
        }
        Ok((stream, port))
    })();
    let (stream, port) = match opened {
        Ok(v) => v,
        Err(e) => {
            // The helper reads the answer to the end: close it.
            error(w, Error::Io(e))?;
            return Ok(Next::Close);
        }
    };
    http::upgrade(w)?;
    engine.touch();
    crate::diag::info!("USB device attached", location = location, name = name, port = port);
    let asked = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let ident = Ident::of(&location);
    {
        let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        sessions.push(Session {
            ident: ident.clone(),
            location: location.clone(),
            name: name.clone(),
            port,
            since: super::time::unix(std::time::SystemTime::now()),
            helper: w.try_clone()?,
            guest: stream.fd.try_clone()?,
            asked: asked.clone(),
        });
        // Back in Kiri: no longer waited for. Marked here, as the session
        // starts and before it can end, so a board that leaves again at once
        // is waited for again (`note_left` clears it) rather than lost.
        if let Some(ident) = &ident {
            mark_back(&mut WANTED.lock().unwrap_or_else(|e| e.into_inner()), ident);
        }
    }

    // Which side ended the session first, for the log: the guest's end is
    // marked before it shuts the helper's down, so the helper's copy can
    // tell whether it saw its own side close or the guest's.
    let guest_ended = Arc::new(AtomicBool::new(false));
    // Anything the helper sent right behind the request is already buffered.
    let early = r.buffer().to_vec();
    let mut to_guest = File::from(stream.fd.try_clone()?);
    let mut from_helper = r.get_ref().try_clone()?;
    let ended = guest_ended.clone();
    let up = std::thread::Builder::new().name("kiri-usb-up".into()).spawn(move || {
        let _ = to_guest.write_all(&early).and_then(|()| io::copy(&mut from_helper, &mut to_guest));
        let helper_first = !ended.load(Ordering::SeqCst);
        // SAFETY: shutting down a socket we own.
        unsafe { libc::shutdown(to_guest.as_raw_fd(), libc::SHUT_RDWR) };
        let _ = from_helper.shutdown(Shutdown::Both);
        helper_first
    })?;
    let mut from_guest = File::from(stream.fd.try_clone()?);
    let _ = io::copy(&mut from_guest, w);
    guest_ended.store(true, Ordering::SeqCst);
    let _ = w.shutdown(Shutdown::Both);
    // SAFETY: shutting down a socket we own.
    unsafe { libc::shutdown(from_guest.as_raw_fd(), libc::SHUT_RDWR) };
    let helper_first = up.join().unwrap_or(false);
    drop(stream);
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).retain(|s| s.location != location);
    let asked = asked.load(Ordering::SeqCst);
    let why = end_reason(asked, helper_first);
    crate::diag::info!("USB device detached", location = location, name = name, why = why, after_s = started.elapsed().as_secs());
    match ident {
        Some(ident) if helper_first && !asked && !engine.is_stopped() => left(ident, &name, started.elapsed()),
        // Ended any other way (detached, the VM closed its side): no longer
        // waited for, even if it came back once before.
        Some(ident) => {
            forget(|w| w.ident.same(&ident));
        }
        None => {}
    }
    Ok(Next::Close)
}

// --- Coming back ------------------------------------------------------------------

/// Who a device is, to know it again when it comes back.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ident {
    vendor: u16,
    product: u16,
    serial: String,
    /// The port it was in: what tells two alike devices with no serial
    /// number apart.
    location: String,
}

impl Ident {
    /// The device at `location` on the Mac's bus.
    fn of(location: &str) -> Option<Ident> {
        crate::usb::mac::devices().ok()?.iter().find(|d| d.location() == location).map(Ident::from)
    }

    /// The same device: by serial number when it has one (it may come back
    /// in another port), else by the port.
    fn same(&self, o: &Ident) -> bool {
        self.vendor == o.vendor && self.product == o.product && if self.serial.is_empty() { o.serial.is_empty() && self.location == o.location } else { self.serial == o.serial }
    }
}

impl From<&crate::usb::mac::Device> for Ident {
    fn from(d: &crate::usb::mac::Device) -> Ident {
        Ident { vendor: d.vendor, product: d.product, serial: d.serial.clone(), location: d.location() }
    }
}

/// A device that left while attached, until it is attached again.
struct Wanted {
    ident: Ident,
    /// Its name, for the list while it is away.
    name: String,
    /// Came back and was attached again. Kept while that session lasts, so
    /// that leaving again right away still counts towards `MAX_QUICK`, but
    /// not waited for.
    back: bool,
    /// Sessions in a row that ended within `QUICK` of starting.
    quick: u32,
    /// Failed attaches in a row since it came back, and when to try next.
    failures: u32,
    next: Instant,
}

/// A session shorter than this that ends on its own counts against the
/// device: one that leaves as soon as it is attached (a board in a boot
/// loop, a device that can't work in the VM) is not attached forever.
impl Wanted {
    /// Waited for: away, or back but not attached yet.
    fn is_waiting(&self, busy: &[Ident]) -> bool {
        !self.back && !busy.iter().any(|b| b.same(&self.ident))
    }
}

const QUICK: Duration = Duration::from_secs(5);
const MAX_QUICK: u32 = 5;
/// Attaches that fail in a row before Kiri leaves the device to the Mac.
const MAX_FAILURES: u32 = 5;

static WANTED: Mutex<Vec<Wanted>> = Mutex::new(Vec::new());
static WATCHING: AtomicBool = AtomicBool::new(false);

/// Stops waiting for the devices `which` picks. True if it was waiting for any.
fn forget(which: impl Fn(&Wanted) -> bool) -> bool {
    let mut w = WANTED.lock().unwrap_or_else(|e| e.into_inner());
    let before = w.len();
    w.retain(|x| !which(x));
    w.len() != before
}

/// A device left the Mac's bus while attached, without anyone detaching
/// it: wait for it to come back, and attach it again then.
fn left(ident: Ident, name: &str, lasted: Duration) {
    let mut w = WANTED.lock().unwrap_or_else(|e| e.into_inner());
    if !note_left(&mut w, ident, name, lasted) {
        crate::diag::warn!("USB device keeps leaving as soon as it is attached; it stays on the Mac", name = name);
        return;
    }
    drop(w);
    crate::diag::info!("USB device left the Mac's bus while attached; Kiri attaches it again when it comes back", name = name);
    watch();
}

/// Records a device that left. False when it has left too often right
/// after being attached, and is no longer waited for.
fn note_left(w: &mut Vec<Wanted>, ident: Ident, name: &str, lasted: Duration) -> bool {
    let quick = u32::from(lasted < QUICK);
    let i = match w.iter().position(|x| x.ident.same(&ident)) {
        Some(i) => {
            let x = &mut w[i];
            x.quick = if quick == 1 { x.quick + 1 } else { 0 };
            (x.failures, x.next, x.ident, x.back) = (0, Instant::now(), ident, false);
            i
        }
        None => {
            w.push(Wanted { ident, name: name.to_owned(), back: false, quick, failures: 0, next: Instant::now() });
            w.len() - 1
        }
    };
    if w[i].quick >= MAX_QUICK {
        w.remove(i);
        return false;
    }
    true
}

/// A device's session started: not waited for while it lasts, but kept, so
/// that leaving again right away still counts towards `MAX_QUICK`.
fn mark_back(w: &mut [Wanted], ident: &Ident) {
    if let Some(x) = w.iter_mut().find(|x| x.ident.same(ident)) {
        (x.back, x.failures) = (true, 0);
    }
}

/// The devices to attach now: waited for, back on the bus, not attached
/// or being attached, and not waiting out a failed try. (index in `w`,
/// the device as it is now.)
fn due<'a>(w: &[Wanted], seen: &'a [crate::usb::mac::Device], busy: &[Ident], pending: &[String], now: Instant) -> Vec<(usize, &'a crate::usb::mac::Device)> {
    w.iter()
        .enumerate()
        .filter(|(_, x)| x.next <= now && x.is_waiting(busy))
        .filter_map(|(i, x)| seen.iter().find(|d| x.ident.same(&Ident::from(*d)) && !pending.contains(&d.location())).map(|d| (i, d)))
        .collect()
}

/// Watches the Mac's bus while a device is waited for and not attached:
/// twice a second, so a board that resets is back in its container about
/// as fast as on a Linux machine.
fn watch() {
    if WATCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiri-usb-rejoin".into()).spawn(|| {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let busy: Vec<Ident> = SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).iter().filter_map(|s| s.ident.clone()).collect();
            let waiting = WANTED.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|x| x.is_waiting(&busy));
            if !waiting {
                WATCHING.store(false, Ordering::SeqCst);
                // One may have left meanwhile, its `watch` seeing this one
                // still running: keep going for it.
                let again = WANTED.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|x| x.is_waiting(&busy));
                if !again || WATCHING.swap(true, Ordering::SeqCst) {
                    return;
                }
                continue;
            }
            let Ok(seen) = crate::usb::mac::devices() else { continue };
            let pending: Vec<String> = PENDING.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
            let todo: Vec<(Ident, String, u32, u32, String)> = {
                let w = WANTED.lock().unwrap_or_else(|e| e.into_inner());
                due(&w, &seen, &busy, &pending, Instant::now()).into_iter().map(|(_, d)| (Ident::from(d), d.location(), d.linux_speed(), d.devid(), d.name.clone())).collect()
            };
            for (ident, location, speed, devid, name) in todo {
                match crate::usb::helper::reattach(&location, &name, speed, devid) {
                    // The session marked it back as it started (`attach`).
                    Ok(()) => crate::diag::info!("USB device came back and is attached again", name = name, location = location),
                    Err(e) => {
                        let mut w = WANTED.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(i) = w.iter().position(|x| x.ident.same(&ident)) else { continue };
                        w[i].failures += 1;
                        if w[i].failures >= MAX_FAILURES {
                            w.remove(i);
                            crate::diag::warn!("USB device came back but could not be attached again; it stays on the Mac", name = name, error = e);
                        } else {
                            // 1, 2, 3, 4 s: the Mac's own drivers may still be taking it.
                            w[i].next = Instant::now() + Duration::from_secs(u64::from(w[i].failures));
                        }
                    }
                }
            }
        }
    });
    if let Err(e) = spawned {
        WATCHING.store(false, Ordering::SeqCst);
        crate::diag::warn!("cannot watch for USB devices coming back", error = e);
    }
}

/// Why a session ended, in the log's words.
fn end_reason(asked: bool, helper_first: bool) -> &'static str {
    match (asked, helper_first) {
        (true, _) => "detached on request",
        // The helper's side closes when the device leaves the Mac's bus
        // (unplugged, or it reset and enumerated again), or the helper died.
        (false, true) => "the device left the Mac's bus or its helper ended",
        (false, false) => "the VM closed its side",
    }
}

fn set_timeout(fd: &OwnedFd, t: Option<Duration>) {
    let tv = t.map_or(libc::timeval { tv_sec: 0, tv_usec: 0 }, |d| libc::timeval { tv_sec: d.as_secs() as _, tv_usec: 0 });
    // SAFETY: setsockopt with a valid timeval on a socket we own.
    unsafe {
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVTIMEO, (&raw const tv).cast(), size_of::<libc::timeval>() as libc::socklen_t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_are_matched_to_their_port() {
        let lines = ["0\t/dev/ttyUSB0\tserial port", "0\t/dev/bus/usb/003/002\traw USB", "1\t/dev/hidraw0\traw HID", "bad"].map(String::from);
        assert_eq!(nodes_on(&lines, 0), [json!({"Path": "/dev/ttyUSB0", "What": "serial port"}), json!({"Path": "/dev/bus/usb/003/002", "What": "raw USB"})]);
        assert_eq!(nodes_on(&lines, 1).len(), 1);
        assert!(nodes_on(&lines, 7).is_empty());
    }

    fn device(location: u32, vendor: u16, product: u16, serial: &str) -> crate::usb::mac::Device {
        crate::usb::mac::Device { location, vendor, product, speed: 1, address: 3, name: "USB JTAG/serial debug unit".into(), maker: String::new(), serial: serial.into() }
    }

    fn ident(location: u32, vendor: u16, product: u16, serial: &str) -> Ident {
        Ident::from(&device(location, vendor, product, serial))
    }

    #[test]
    fn a_device_is_known_again_by_serial_else_by_port() {
        let esp = ident(0x0110_0000, 0x303a, 0x1001, "F4:12:FA:00:11:22");
        // Back in another port: the same board.
        assert!(esp.same(&ident(0x0120_0000, 0x303a, 0x1001, "F4:12:FA:00:11:22")));
        // Another board of the same kind is not.
        assert!(!esp.same(&ident(0x0110_0000, 0x303a, 0x1001, "F4:12:FA:00:11:33")));
        assert!(!esp.same(&ident(0x0110_0000, 0x303a, 0x1002, "F4:12:FA:00:11:22")));
        // No serial number: only in the same port.
        let ch340 = ident(0x0110_0000, 0x1a86, 0x7523, "");
        assert!(ch340.same(&ident(0x0110_0000, 0x1a86, 0x7523, "")));
        assert!(!ch340.same(&ident(0x0120_0000, 0x1a86, 0x7523, "")));
    }

    #[test]
    fn a_device_that_keeps_leaving_right_away_is_let_go() {
        let mut w = Vec::new();
        let esp = ident(0x0110_0000, 0x303a, 0x1001, "A");
        // A long session that ends (a reset during flashing): waited for.
        assert!(note_left(&mut w, esp.clone(), "esp", Duration::from_secs(600)));
        assert_eq!((w.len(), w[0].quick), (1, 0));
        for n in 1..MAX_QUICK {
            assert!(note_left(&mut w, esp.clone(), "esp", Duration::from_secs(1)), "quick drop {n}");
        }
        // A longer one in between starts the count again.
        assert!(note_left(&mut w, esp.clone(), "esp", Duration::from_secs(60)));
        assert_eq!(w[0].quick, 0);
        for _ in 1..MAX_QUICK {
            assert!(note_left(&mut w, esp.clone(), "esp", Duration::from_secs(1)));
        }
        assert!(!note_left(&mut w, esp, "esp", Duration::from_secs(1)));
        assert!(w.is_empty());
    }

    #[test]
    fn only_devices_back_on_the_bus_and_free_are_attached_again() {
        let now = Instant::now();
        let want = |i: Ident| Wanted { ident: i, name: String::new(), back: false, quick: 0, failures: 0, next: now };
        let w = vec![want(ident(0x0110_0000, 0x303a, 0x1001, "A")), want(ident(0x0120_0000, 0x1a86, 0x7523, "")), want(ident(0x0130_0000, 0x10c4, 0xea60, "C"))];
        // A back in another port; the CH340 not back; C back but attached already.
        let seen = [device(0x0140_0000, 0x303a, 0x1001, "A"), device(0x0130_0000, 0x10c4, 0xea60, "C"), device(0x0150_0000, 0x1a86, 0x7523, "")];
        let busy = [ident(0x0130_0000, 0x10c4, 0xea60, "C")];
        let got: Vec<(usize, String)> = due(&w, &seen, &busy, &[], now).into_iter().map(|(i, d)| (i, d.location())).collect();
        assert_eq!(got, [(0, "0x01400000".to_owned())]);
        // Being attached already, or waiting out a failed try: not yet.
        assert!(due(&w, &seen, &busy, &["0x01400000".to_owned()], now).is_empty());
        let mut later = w;
        later[0].next = now + Duration::from_secs(2);
        assert!(due(&later, &seen, &busy, &[], now).is_empty());
        assert_eq!(due(&later, &seen, &busy, &[], now + Duration::from_secs(3)).len(), 1);
        // Back and attached again: not waited for, though still listed.
        later[0].back = true;
        assert!(due(&later, &seen, &busy, &[], now + Duration::from_secs(3)).is_empty());
        assert!(!later[0].is_waiting(&[]));
    }

    #[test]
    fn a_session_that_ends_before_the_watcher_looks_is_waited_for_again() {
        // The order that lost a device: re-attached (the session marks it
        // back as it starts), the board leaves again at once (note_left),
        // then the watcher gets its "ok". It must still be waited for.
        let mut w = Vec::new();
        let esp = ident(0x0110_0000, 0x303a, 0x1001, "A");
        assert!(note_left(&mut w, esp.clone(), "esp", Duration::from_secs(600)));
        mark_back(&mut w, &esp);
        assert!(!w[0].is_waiting(&[]));
        assert!(note_left(&mut w, esp.clone(), "esp", Duration::from_secs(1)));
        assert!(w[0].is_waiting(&[]), "waited for again");
        let seen = [device(0x0110_0000, 0x303a, 0x1001, "A")];
        assert_eq!(due(&w, &seen, &[], &[], Instant::now()).len(), 1, "due to attach");
    }

    #[test]
    fn leaving_again_after_coming_back_waits_again() {
        let mut w = Vec::new();
        let esp = ident(0x0110_0000, 0x303a, 0x1001, "A");
        assert!(note_left(&mut w, esp.clone(), "esp", Duration::from_secs(1)));
        w[0].back = true;
        assert!(note_left(&mut w, esp, "esp", Duration::from_secs(1)));
        assert_eq!((w.len(), w[0].back, w[0].quick), (1, false, 2));
    }

    #[test]
    fn the_log_says_why_a_session_ended() {
        assert_eq!(end_reason(true, true), "detached on request");
        assert_eq!(end_reason(false, true), "the device left the Mac's bus or its helper ended");
        assert_eq!(end_reason(false, false), "the VM closed its side");
    }
}
