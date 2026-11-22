//! The Mac side of USB passthrough: finding devices in the I/O Registry,
//! and `kiri usb serve`, the root helper that captures one with Apple's
//! IOUSBHost framework and puts it behind the USB/IP server (`usbip.rs`)
//! for the guest's vhci_hcd. Transfers use the framework's synchronous
//! calls, one endpoint thread each; an unlink aborts the pipe.
//!
//! The device goes back to macOS (a reset, so macOS enumerates it again and
//! matches its own drivers) whenever the session ends: a detach, the guest
//! or VM going away, the daemon quitting, a signal to the helper. Unplugging
//! it ends the session from this side.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::{Block, RcBlock};
use objc2::encode::{Encode, Encoding};
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;

use super::usbip::{self, Done, Setup};

// --- I/O Registry --------------------------------------------------------------

type CFTypeRef = *const c_void;
const UTF8: u32 = 0x0800_0100;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(main: u32, matching: *mut c_void, existing: *mut u32) -> i32;
    fn IOIteratorNext(it: u32) -> u32;
    fn IOObjectRelease(o: u32) -> i32;
    fn IOObjectRetain(o: u32) -> i32;
    fn IOObjectConformsTo(o: u32, class: *const c_char) -> u32;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: CFTypeRef, alloc: CFTypeRef, options: u32) -> CFTypeRef;
    fn IORegistryEntryGetChildIterator(entry: u32, plane: *const c_char, it: *mut u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, enc: u32) -> CFTypeRef;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut u8, len: isize, enc: u32) -> u8;
    fn CFGetTypeID(t: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(n: CFTypeRef, kind: isize, out: *mut c_void) -> u8;
    fn CFRelease(t: CFTypeRef);
}

#[link(name = "IOUSBHost", kind = "framework")]
unsafe extern "C" {}

/// An io_object_t, released when dropped.
struct IoObj(u32);

impl Drop for IoObj {
    fn drop(&mut self) {
        // SAFETY: an object this value owns one reference to.
        unsafe { IOObjectRelease(self.0) };
    }
}

fn property(entry: u32, key: &CStr) -> Option<CFTypeRef> {
    // SAFETY: plain CF/IOKit calls with valid arguments; the key is released.
    unsafe {
        let k = CFStringCreateWithCString(ptr::null(), key.as_ptr(), UTF8);
        let v = IORegistryEntryCreateCFProperty(entry, k, ptr::null(), 0);
        CFRelease(k);
        (!v.is_null()).then_some(v)
    }
}

fn number(entry: u32, key: &CStr) -> Option<i64> {
    let v = property(entry, key)?;
    let mut n = 0i64;
    // SAFETY: v is a CF object we own; kCFNumberSInt64Type is 4.
    let ok = unsafe { CFGetTypeID(v) == CFNumberGetTypeID() && CFNumberGetValue(v, 4, (&raw mut n).cast()) != 0 };
    // SAFETY: releasing what IORegistryEntryCreateCFProperty returned.
    unsafe { CFRelease(v) };
    ok.then_some(n)
}

fn string(entry: u32, key: &CStr) -> Option<String> {
    let v = property(entry, key)?;
    let mut buf = [0u8; 256];
    // SAFETY: v is a CF object we own; the buffer's length is passed.
    let ok = unsafe { CFGetTypeID(v) == CFStringGetTypeID() && CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as isize, UTF8) != 0 };
    // SAFETY: releasing what IORegistryEntryCreateCFProperty returned.
    unsafe { CFRelease(v) };
    ok.then(|| CStr::from_bytes_until_nul(&buf).unwrap_or_default().to_string_lossy().into_owned())
}

fn each_service(class: &CStr, mut f: impl FnMut(&IoObj)) -> io::Result<()> {
    let mut it = 0u32;
    // SAFETY: IOServiceGetMatchingServices consumes the matching dictionary.
    let kr = unsafe { IOServiceGetMatchingServices(0, IOServiceMatching(class.as_ptr()), &mut it) };
    if kr != 0 {
        return Err(io::Error::other(format!("the I/O Registry did not answer (0x{kr:x})")));
    }
    let it = IoObj(it);
    loop {
        // SAFETY: a valid iterator; each object returned is ours to release.
        let o = unsafe { IOIteratorNext(it.0) };
        if o == 0 {
            return Ok(());
        }
        f(&IoObj(o));
    }
}

pub struct Device {
    pub location: u32,
    pub vendor: u16,
    pub product: u16,
    /// The I/O Registry's "Device Speed": 0 low, 1 full, 2 high, 3 super, 4 super+.
    pub speed: u8,
    pub address: u8,
    pub name: String,
    /// The maker and serial number the device reports, when it does.
    pub maker: String,
    pub serial: String,
}

impl Device {
    pub fn location(&self) -> String {
        format!("0x{:08x}", self.location)
    }

    pub fn speed_name(&self) -> &'static str {
        ["low", "full", "high", "super", "super+"].get(self.speed as usize).copied().unwrap_or("?")
    }

    /// The kernel's USB_SPEED_*, for vhci: no SuperSpeed+ there.
    pub fn linux_speed(&self) -> u32 {
        match self.speed {
            0 => 1,
            1 => 2,
            2 => 3,
            _ => 5,
        }
    }

    /// What it is, for the window's icon: from its interfaces.
    pub fn kind(&self) -> &'static str {
        let mut ifs = Vec::new();
        let mut class = 0u8;
        let _ = each_service(c"IOUSBHostDevice", |o| {
            if number(o.0, c"locationID") != Some(i64::from(self.location)) {
                return;
            }
            class = number(o.0, c"bDeviceClass").unwrap_or(0) as u8;
            let mut it = 0u32;
            // SAFETY: a valid registry entry; the iterator is ours.
            if unsafe { IORegistryEntryGetChildIterator(o.0, c"IOService".as_ptr(), &mut it) } != 0 {
                return;
            }
            let it = IoObj(it);
            loop {
                // SAFETY: a valid iterator; each object is ours to release.
                let c = unsafe { IOIteratorNext(it.0) };
                if c == 0 {
                    break;
                }
                let c = IoObj(c);
                // SAFETY: a valid object and a class name.
                if unsafe { IOObjectConformsTo(c.0, c"IOUSBHostInterface".as_ptr()) } == 0 {
                    continue;
                }
                let n = |k: &CStr| number(c.0, k).unwrap_or(0) as u8;
                ifs.push((n(c"bInterfaceNumber"), n(c"bInterfaceClass"), n(c"bInterfaceSubClass"), n(c"bInterfaceProtocol")));
            }
        });
        super::kind_of(class, &ifs, self.vendor, &self.name)
    }

    /// The bus number, as Linux and System Information count it.
    pub fn bus(&self) -> u32 {
        (self.location >> 24) & 0xff
    }

    /// USB/IP's bus << 16 | device address.
    pub fn devid(&self) -> u32 {
        ((self.location >> 24) & 0xff) << 16 | u32::from(self.address)
    }
}

fn read_device(o: &IoObj) -> Option<Device> {
    // Hubs pass nothing useful through.
    if number(o.0, c"bDeviceClass") == Some(9) {
        return None;
    }
    Some(Device {
        location: number(o.0, c"locationID")? as u32,
        vendor: number(o.0, c"idVendor").unwrap_or(0) as u16,
        product: number(o.0, c"idProduct").unwrap_or(0) as u16,
        speed: number(o.0, c"Device Speed").unwrap_or(1) as u8,
        address: number(o.0, c"USB Address").unwrap_or(0) as u8,
        name: string(o.0, c"USB Product Name").or_else(|| string(o.0, c"kUSBProductString")).unwrap_or_else(|| "USB device".into()),
        maker: string(o.0, c"USB Vendor Name").or_else(|| string(o.0, c"kUSBVendorString")).unwrap_or_default().trim().to_owned(),
        serial: string(o.0, c"USB Serial Number").or_else(|| string(o.0, c"kUSBSerialNumberString")).unwrap_or_default().trim().to_owned(),
    })
}

pub fn devices() -> io::Result<Vec<Device>> {
    let mut v = Vec::new();
    each_service(c"IOUSBHostDevice", |o| v.extend(read_device(o)))?;
    v.sort_by_key(|d| d.location);
    Ok(v)
}

/// A device by location, vendor:product or part of its name.
pub fn find(which: &str) -> io::Result<Device> {
    let all = devices()?;
    let w = which.to_ascii_lowercase();
    let by_id = |d: &Device| format!("{:04x}:{:04x}", d.vendor, d.product) == w;
    let by_loc = |d: &Device| d.location() == w || u32::from_str_radix(w.trim_start_matches("0x"), 16).is_ok_and(|l| l == d.location);
    let mut found: Vec<Device> = all.into_iter().filter(|d| by_loc(d) || by_id(d) || d.name.to_ascii_lowercase().contains(&w)).collect();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(io::Error::new(io::ErrorKind::NotFound, format!("no USB device matches {which:?} (see kiri usb list)"))),
        _ => Err(io::Error::other(format!(
            "{which:?} matches {} devices: {}",
            found.len(),
            found.iter().map(|d| format!("{} ({})", d.name, d.location())).collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn service_at(location: u32) -> io::Result<IoObj> {
    let mut found = None;
    each_service(c"IOUSBHostDevice", |o| {
        if found.is_none() && number(o.0, c"locationID") == Some(i64::from(location)) {
            // SAFETY: keep a reference past the iterator's.
            unsafe { IOObjectRetain(o.0) };
            found = Some(IoObj(o.0));
        }
    })?;
    found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no USB device at 0x{location:08x}")))
}

// --- IOUSBHost ------------------------------------------------------------------

/// IOUSBHost's IOUSBDeviceRequest: a USB setup packet.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct DeviceRequest {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
}

// SAFETY: the same fields and layout as the C struct.
unsafe impl Encode for DeviceRequest {
    const ENCODING: Encoding = Encoding::Struct("IOUSBDeviceRequest", &[u8::ENCODING, u8::ENCODING, u16::ENCODING, u16::ENCODING, u16::ENCODING]);
}

/// An IOUSBHost object. The framework's objects may be used from any
/// thread; each endpoint's transfers run on one thread at a time.
#[derive(Clone)]
struct Obj(Retained<AnyObject>);
// SAFETY: see the type's documentation.
unsafe impl Send for Obj {}
// SAFETY: see the type's documentation.
unsafe impl Sync for Obj {}

type InterestHandler = Block<dyn Fn(NonNull<AnyObject>, u32, *mut c_void)>;

const CAPTURE: usize = 1;

/// The IOReturn in an NSError, or a generic error code.
fn ioreturn(err: *mut AnyObject) -> u32 {
    if err.is_null() {
        return 0xe00002bc; // kIOReturnError
    }
    // SAFETY: a valid NSError from the framework.
    let code: isize = unsafe { msg_send![&*err, code] };
    code as u32
}

fn describe(err: *mut AnyObject) -> String {
    if err.is_null() {
        return "unknown error".into();
    }
    // SAFETY: a valid NSError; its description is an NSString.
    let d: Option<Retained<objc2_foundation::NSString>> = unsafe { msg_send![&*err, localizedDescription] };
    d.map_or_else(|| format!("0x{:x}", ioreturn(err)), |s| s.to_string())
}

fn open(class: &str, service: u32, options: usize, handler: Option<&InterestHandler>) -> Result<Obj, String> {
    let cls = AnyClass::get(&CString::new(class).unwrap_or_default()).ok_or_else(|| format!("{class} is not available on this Mac"))?;
    let mut err: *mut AnyObject = ptr::null_mut();
    let queue: Option<&AnyObject> = None;
    // SAFETY: the framework's designated initialiser; the service is valid
    // and the framework takes its own reference.
    let obj: Option<Retained<AnyObject>> = unsafe {
        let a: Allocated<AnyObject> = msg_send![cls, alloc];
        msg_send![a, initWithIOService: service, options: options, queue: queue, error: &mut err, interestHandler: handler]
    };
    obj.map(Obj).ok_or_else(|| describe(err))
}

fn mutable_data(len: usize) -> Retained<AnyObject> {
    // SAFETY: NSMutableData's class constructor.
    unsafe { msg_send![objc2::class!(NSMutableData), dataWithLength: len] }
}

fn data_bytes(d: &AnyObject, n: usize) -> Vec<u8> {
    // SAFETY: NSMutableData's buffer holds at least its length, and n is
    // capped to it.
    unsafe {
        let len: usize = msg_send![d, length];
        let p: *const u8 = msg_send![d, mutableBytes];
        std::slice::from_raw_parts(p, n.min(len)).to_vec()
    }
}

fn data_with(bytes: &[u8]) -> Retained<AnyObject> {
    let d = mutable_data(bytes.len());
    // SAFETY: the buffer is bytes.len() long.
    unsafe {
        let p: *mut u8 = msg_send![&*d, mutableBytes];
        ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    }
    d
}


// --- The captured device ----------------------------------------------------------

/// kIOMessageServiceIsTerminated: the device was unplugged (or reset away).
const TERMINATED: u32 = 0xe000_0010;
/// IOUSBHostAbortOptionSynchronous: return once the aborted IO completed.
const ABORT_SYNC: usize = 1;

/// The kernel's errno for an IOReturn, as URB status (positive).
fn errno(kr: u32) -> i32 {
    match kr {
        0xe0005000 | 0xe000404f => usbip::EPIPE, // stalled
        0xe00002eb => usbip::ECONNRESET,         // aborted
        0xe00002d6 => 110,                       // timeout: ETIMEDOUT
        0xe00002e8 => 75,                        // overrun: EOVERFLOW
        0xe00002c0 | 0xe00002ed | 0xe00002d8 | 0xe00002c7 => usbip::ENODEV, // no device, not responding, not attached, unsupported
        _ => 71,                                 // EPROTO
    }
}

/// kIOReturnUnderrun: a short IN, which USB/IP reports as success.
const UNDERRUN: u32 = 0xe00002e7;

struct HostDevice {
    device: Obj,
    service: u32,
    /// Open IOUSBHostInterfaces by interface number.
    interfaces: Mutex<HashMap<u8, Obj>>,
    /// Pipes by endpoint address, with their interface number.
    pipes: Mutex<HashMap<u8, (u8, Obj)>>,
    gone: Arc<AtomicBool>,
}

impl HostDevice {
    /// Opens the current configuration's interfaces (they appear in the
    /// registry a moment after a configure).
    fn open_interfaces(&self) {
        let mut ifs = self.interfaces.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..40 {
            let mut it = 0u32;
            // SAFETY: a valid registry entry; the iterator is ours.
            if unsafe { IORegistryEntryGetChildIterator(self.service, c"IOService".as_ptr(), &mut it) } == 0 {
                let it = IoObj(it);
                loop {
                    // SAFETY: a valid iterator.
                    let o = unsafe { IOIteratorNext(it.0) };
                    if o == 0 {
                        break;
                    }
                    let o = IoObj(o);
                    // SAFETY: a valid object and a class name.
                    if unsafe { IOObjectConformsTo(o.0, c"IOUSBHostInterface".as_ptr()) } == 0 {
                        continue;
                    }
                    let Some(n) = number(o.0, c"bInterfaceNumber") else { continue };
                    if ifs.contains_key(&(n as u8)) {
                        continue;
                    }
                    match open("IOUSBHostInterface", o.0, 0, None) {
                        Ok(i) => {
                            ifs.insert(n as u8, i);
                        }
                        Err(e) => log(&format!("interface {n}: {e}")),
                    }
                }
            }
            if !ifs.is_empty() || self.gone.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn close_interfaces(&self) {
        self.pipes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        for (_, i) in self.interfaces.lock().unwrap_or_else(|e| e.into_inner()).drain() {
            // SAFETY: destroy on an object we opened.
            let _: () = unsafe { msg_send![&*i.0, destroy] };
        }
    }

    fn pipe(&self, address: u8) -> Option<Obj> {
        if let Some((_, p)) = self.pipes.lock().unwrap_or_else(|e| e.into_inner()).get(&address) {
            return Some(p.clone());
        }
        if self.interfaces.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            self.open_interfaces();
        }
        let ifs: Vec<(u8, Obj)> = self.interfaces.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(n, i)| (*n, i.clone())).collect();
        for (n, i) in ifs {
            let mut err: *mut AnyObject = ptr::null_mut();
            // SAFETY: a "copy" method: the pipe comes back retained.
            let p: Option<Retained<AnyObject>> = unsafe { msg_send![&*i.0, copyPipeWithAddress: address as usize, error: &mut err] };
            if let Some(p) = p {
                let p = Obj(p);
                self.pipes.lock().unwrap_or_else(|e| e.into_inner()).insert(address, (n, p.clone()));
                return Some(p);
            }
        }
        None
    }

    fn status(ok: Bool, err: *mut AnyObject) -> i32 {
        if ok.as_bool() { 0 } else { -errno(ioreturn(err)) }
    }

    fn finish(ok: Bool, err: *mut AnyObject, data: Option<&AnyObject>, done: usize, dir_in: bool) -> Done {
        let kr = if ok.as_bool() { 0 } else { ioreturn(err) };
        if kr != 0 && kr != UNDERRUN {
            return Done::fail(errno(kr));
        }
        match (data, dir_in) {
            (Some(d), true) => Done::read(data_bytes(d, done)),
            _ => Done::wrote(done),
        }
    }
}

impl usbip::Backend for HostDevice {
    fn control(&self, _seq: u32, s: &Setup, out: &[u8]) -> Done {
        if self.gone.load(Ordering::SeqCst) {
            return Done::fail(usbip::ENODEV);
        }
        autoreleasepool(|_| {
            let req = DeviceRequest { request_type: s.request_type, request: s.request, value: s.value, index: s.index, length: s.length };
            let dir_in = s.request_type & 0x80 != 0;
            let data = if dir_in { Some(mutable_data(s.length as usize)) } else { (!out.is_empty()).then(|| data_with(out)) };
            let (mut done, mut err) = (0usize, ptr::null_mut::<AnyObject>());
            let d: Option<&AnyObject> = data.as_deref();
            // SAFETY: a synchronous request on the default control endpoint.
            let ok: Bool = unsafe { msg_send![&*self.device.0, sendDeviceRequest: req, data: d, bytesTransferred: &mut done, completionTimeout: 5.0f64, error: &mut err] };
            Self::finish(ok, err, d, done, dir_in)
        })
    }

    fn set_configuration(&self, value: u8) -> i32 {
        self.close_interfaces();
        let mut err: *mut AnyObject = ptr::null_mut();
        // SAFETY: configureWithValue on the captured device; no macOS
        // drivers on its interfaces.
        let ok: Bool = unsafe { msg_send![&*self.device.0, configureWithValue: value as usize, matchInterfaces: false, error: &mut err] };
        if ok.as_bool() && value != 0 {
            self.open_interfaces();
        }
        Self::status(ok, err)
    }

    fn set_interface(&self, interface: u8, alt: u8) -> i32 {
        if !self.interfaces.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&interface) {
            self.open_interfaces();
        }
        let Some(i) = self.interfaces.lock().unwrap_or_else(|e| e.into_inner()).get(&interface).cloned() else {
            return -usbip::EPIPE;
        };
        // Its pipes change with the setting.
        self.pipes.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, (n, _)| *n != interface);
        let mut err: *mut AnyObject = ptr::null_mut();
        // SAFETY: selectAlternateSetting on an interface we opened.
        let ok: Bool = unsafe { msg_send![&*i.0, selectAlternateSetting: alt as usize, error: &mut err] };
        Self::status(ok, err)
    }

    fn clear_halt(&self, endpoint: u8) -> i32 {
        let Some(p) = self.pipe(endpoint) else { return -usbip::EPIPE };
        let mut err: *mut AnyObject = ptr::null_mut();
        // SAFETY: clearStall on a pipe we hold.
        let ok: Bool = unsafe { msg_send![&*p.0, clearStallWithError: &mut err] };
        Self::status(ok, err)
    }

    fn transfer(&self, _seq: u32, endpoint: u8, out: &[u8], len: usize) -> Done {
        if self.gone.load(Ordering::SeqCst) {
            return Done::fail(usbip::ENODEV);
        }
        let Some(p) = self.pipe(endpoint) else { return Done::fail(usbip::EPIPE) };
        autoreleasepool(|_| {
            let dir_in = endpoint & 0x80 != 0;
            let data = if dir_in { Some(mutable_data(len)) } else { (!out.is_empty()).then(|| data_with(out)) };
            let (mut done, mut err) = (0usize, ptr::null_mut::<AnyObject>());
            let d: Option<&AnyObject> = data.as_deref();
            // SAFETY: a synchronous transfer; 0 is no timeout, as interrupt
            // pipes require. An unlink aborts it.
            let ok: Bool = unsafe { msg_send![&*p.0, sendIORequestWithData: d, bytesTransferred: &mut done, completionTimeout: 0.0f64, error: &mut err] };
            Self::finish(ok, err, d, done, dir_in)
        })
    }

    fn abort(&self, _seq: u32, endpoint: u8) {
        let mut err: *mut AnyObject = ptr::null_mut();
        if endpoint == 0 {
            // SAFETY: abort on the device we hold.
            let _: Bool = unsafe { msg_send![&*self.device.0, abortDeviceRequestsWithOption: ABORT_SYNC, error: &mut err] };
        } else if let Some((_, p)) = self.pipes.lock().unwrap_or_else(|e| e.into_inner()).get(&endpoint) {
            // SAFETY: abort on a pipe we hold.
            let _: Bool = unsafe { msg_send![&*p.0, abortWithOption: ABORT_SYNC, error: &mut err] };
        }
    }

    fn close(&self) {
        let pipes: Vec<Obj> = self.pipes.lock().unwrap_or_else(|e| e.into_inner()).values().map(|(_, p)| p.clone()).collect();
        for p in pipes {
            let mut err: *mut AnyObject = ptr::null_mut();
            // SAFETY: abort on a pipe we hold.
            let _: Bool = unsafe { msg_send![&*p.0, abortWithOption: ABORT_SYNC, error: &mut err] };
        }
        let mut err: *mut AnyObject = ptr::null_mut();
        // SAFETY: abort on the device we hold.
        let _: Bool = unsafe { msg_send![&*self.device.0, abortDeviceRequestsWithOption: ABORT_SYNC, error: &mut err] };
    }
}

/// The session's socket, for ending it from a signal handler or the
/// unplug notification (shutdown(2) is async-signal-safe).
static SESSION_FD: AtomicI32 = AtomicI32::new(-1);
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
    let fd = SESSION_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: shutdown on a socket; async-signal-safe.
        unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
    }
}

/// `kiri usb serve LOCATION --fd N --status M`, started by the helper as
/// root: capture the device and serve it over fd N, the connection its
/// requester made to the daemon, until either side lets go. "ok" (or
/// "error: …") goes to fd M once the device is captured.
pub fn serve(location: &str, fd: Option<&String>, status: Option<&String>) -> io::Result<()> {
    use std::os::fd::FromRawFd;
    let parse = |v: Option<&String>, what: &str| v.and_then(|s| s.parse::<i32>().ok()).filter(|&n| n > 2).ok_or_else(|| io::Error::other(format!("{what} is required")));
    let (fd, status) = (parse(fd, "--fd")?, parse(status, "--status")?);
    // SAFETY: the helper passed these two descriptors to this process.
    let (stream, mut status) = unsafe { (UnixStream::from_raw_fd(fd), UnixStream::from_raw_fd(status)) };
    let r = capture_and_serve(location, stream, &mut status);
    if let Err(e) = &r {
        let _ = writeln!(status, "error: {e}");
    }
    r
}

fn capture_and_serve(location: &str, stream: UnixStream, status: &mut UnixStream) -> io::Result<()> {
    // SAFETY: getuid has no preconditions.
    if unsafe { libc::getuid() } != 0 {
        return Err(io::Error::other("kiri usb serve needs root (the USB helper runs it)"));
    }
    let loc = u32::from_str_radix(location.trim_start_matches("0x"), 16).map_err(|_| io::Error::other(format!("bad location {location:?}")))?;
    let svc = service_at(loc)?;
    let info = read_device(&svc).ok_or_else(|| io::Error::other("that is a hub; pass a device through instead"))?;

    // A signal (logout, shutdown, kill) ends the session the normal way, so
    // the device still goes back to macOS.
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: installing a handler that only does async-signal-safe work.
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }

    // Unplugged: end the session from this side.
    let gone = Arc::new(AtomicBool::new(false));
    let g = gone.clone();
    let handler = RcBlock::new(move |_obj: NonNull<AnyObject>, message: u32, _arg: *mut c_void| {
        if message == TERMINATED {
            g.store(true, Ordering::SeqCst);
            on_signal(0);
        }
    });

    // Take the device from macOS: every driver on it is terminated.
    let device = open("IOUSBHostDevice", svc.0, CAPTURE, Some(&handler)).map_err(|e| io::Error::other(format!("could not capture {}: {e}", info.name)))?;
    let host = Arc::new(HostDevice { device: device.clone(), service: svc.0, interfaces: Mutex::new(HashMap::new()), pipes: Mutex::new(HashMap::new()), gone: gone.clone() });

    let result = (|| -> io::Result<()> {
        if STOP.load(Ordering::SeqCst) {
            return Ok(());
        }
        {
            use std::os::fd::AsRawFd;
            SESSION_FD.store(stream.as_raw_fd(), Ordering::SeqCst);
        }
        if STOP.load(Ordering::SeqCst) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let _ = writeln!(status, "ok");
        let _ = status.shutdown(std::net::Shutdown::Both);
        log(&format!("{} ({:04x}:{:04x}) attached", info.name, info.vendor, info.product));
        let reader = std::io::BufReader::new(stream.try_clone()?);
        let r = usbip::serve(host.clone(), reader, stream);
        SESSION_FD.store(-1, Ordering::SeqCst);
        r
    })();
    host.close_interfaces();

    // Hand the device back: a reset makes macOS enumerate it again and
    // match its own drivers. An unplugged device has nothing to reset.
    let mut err: *mut AnyObject = ptr::null_mut();
    // SAFETY: calls on the device we captured, then its destroy.
    unsafe {
        if !gone.load(Ordering::SeqCst) {
            let _: Bool = msg_send![&*device.0, resetWithError: &mut err];
        }
        let _: () = msg_send![&*device.0, destroy];
    }
    if gone.load(Ordering::SeqCst) {
        log(&format!("{} left the bus (unplugged, or it reset and enumerated again)", info.name));
    } else {
        log(&format!("{} given back to macOS", info.name));
    }
    result
}

/// A line in the helper's log, with the time: when a device left matters
/// as much as that it did.
pub(super) fn log(msg: &str) {
    eprintln!("{} kiri usb: {msg}", crate::daemon::time::rfc3339(std::time::SystemTime::now()));
}
