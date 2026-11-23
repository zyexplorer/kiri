//! A message-mode named pipe instance, as Docker's own `\\.\pipe\docker_engine`
//! is: go-winio (the docker CLI's pipe client) then reads an empty message
//! as EOF and sends one for CloseWrite, so half-closes work. Overlapped, so
//! one thread can read while another writes. The client end (`open`), for
//! kiri-win's own requests, is overlapped too, so `cancel` can end a read
//! that waits on a log stream.

use std::io;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_PIPE_NOT_CONNECTED, ERROR_MORE_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, SetNamedPipeHandleState, WaitNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE,
    PIPE_WAIT,
};
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

#[derive(Debug)]
pub struct Pipe {
    h: HANDLE,
    /// The server's end (disconnected when dropped), not a client's.
    server: bool,
    /// Abandoned on handover: stop new I/O and skip flushing on close.
    aborted: AtomicBool,
}

// SAFETY: a pipe handle may be used from any thread; each overlapped call
// below has its own OVERLAPPED and event, so a read and a write can run at
// once.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Pipe {
    /// A new instance of `name`. `first` fails if another process already
    /// serves it (another kiri-win).
    pub fn create(name: &str, first: bool) -> io::Result<Pipe> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let open = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let mode = PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        // Grant access only to this user, SYSTEM and administrators (the
        // default descriptor also lets Everyone open a read-only connection).
        // 255 instances (the most Windows allows) bounds open connections; a
        // compose stack holds one per service for its logs, so not fewer.
        let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: security_descriptor(), bInheritHandle: 0 };
        if sa.lpSecurityDescriptor.is_null() {
            // The default descriptor lets Everyone connect: refuse to serve
            // rather than fall back to it.
            return Err(io::Error::other("could not build the pipe's security descriptor"));
        }
        let sa_ptr: *const SECURITY_ATTRIBUTES = &sa;
        // SAFETY: a valid NUL-terminated name and, when non-null, a descriptor
        // that lives for the process (built once in a OnceLock).
        let h = unsafe { CreateNamedPipeW(wide.as_ptr(), open, mode, 255, 64 * 1024, 64 * 1024, 0, sa_ptr) };
        if h == INVALID_HANDLE_VALUE {
            let e = io::Error::last_os_error();
            // FILE_FLAG_FIRST_PIPE_INSTANCE answers with these when some
            // process already serves the name: another kiri-win, or anything
            // squatting it to pose as Kiri. Name the case for the caller.
            if first && e.raw_os_error().is_some_and(|c| c == ERROR_ACCESS_DENIED as i32 || c == ERROR_PIPE_BUSY as i32) {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("{name} is already served by another process ({e})")));
            }
            return Err(e);
        }
        Ok(Pipe { h, server: true, aborted: AtomicBool::new(false) })
    }

    /// A client connection to `name`, waiting while every instance is busy.
    pub fn open(name: &str) -> io::Result<Pipe> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        for _ in 0..50 {
            // SAFETY: a valid NUL-terminated name; no security attributes.
            // SECURITY_SQOS_PRESENT|SECURITY_IDENTIFICATION: if some other
            // account squatted the name, its server can identify but not
            // impersonate us.
            const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
            const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
            let flags = FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
            let h = unsafe { CreateFileW(wide.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, ptr::null(), OPEN_EXISTING, flags, 0) };
            if h != INVALID_HANDLE_VALUE {
                // Message reads, so the server's empty message reads as EOF.
                let mode = PIPE_READMODE_MESSAGE;
                // SAFETY: our new handle and a valid mode.
                unsafe { SetNamedPipeHandleState(h, &mode, ptr::null(), ptr::null()) };
                return Ok(Pipe { h, server: false, aborted: AtomicBool::new(false) });
            }
            // SAFETY: reads this thread's last error.
            if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: as above.
            unsafe { WaitNamedPipeW(wide.as_ptr(), 100) };
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, format!("{name} stays busy")))
    }

    /// Ends whatever read or write is waiting on this pipe, from any thread.
    pub fn cancel(&self) {
        // SAFETY: our handle; cancelling nothing is harmless.
        unsafe { CancelIoEx(self.h, ptr::null()) };
    }

    /// Abandons this instance: cancels waiting I/O, stops new I/O, and
    /// skips flushing unread replies when the handle is closed.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        self.cancel();
    }

    /// Whether `abort` has been called on this instance.
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Whether the client is still connected, without reading.
    pub fn alive(&self) -> bool {
        if self.is_aborted() {
            return false;
        }
        let mut avail = 0u32;
        // SAFETY: our handle; no buffer, only the count of bytes waiting.
        unsafe { PeekNamedPipe(self.h, ptr::null_mut(), 0, ptr::null_mut(), &mut avail, ptr::null_mut()) != 0 }
    }

    /// Waits until the client has read everything written so far.
    pub fn flush(&self) {
        if self.is_aborted() {
            return;
        }
        // SAFETY: our handle.
        unsafe { FlushFileBuffers(self.h) };
    }

    /// Waits for a client.
    pub fn connect(&self) -> io::Result<()> {
        if self.is_aborted() {
            return Err(io::Error::from_raw_os_error(ERROR_OPERATION_ABORTED as i32));
        }
        // SAFETY: the OVERLAPPED outlives the call, which overlapped() waits on.
        match overlapped(self.h, |ov| unsafe { ConnectNamedPipe(self.h, ov) }) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) => Ok(()),
            r => r.map(drop),
        }
    }

    /// One message, or part of one; 0 is the client's half-close.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_within(buf, None)
    }

    /// `read`, giving up (TimedOut) when nothing arrives within `timeout`.
    pub fn read_within(&self, buf: &mut [u8], timeout: Option<std::time::Duration>) -> io::Result<usize> {
        if self.is_aborted() {
            return Err(io::Error::from_raw_os_error(ERROR_OPERATION_ABORTED as i32));
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        // SAFETY: buf is valid for len bytes until overlapped() returns.
        match overlapped_within(self.h, timeout, |ov| unsafe { ReadFile(self.h, buf.as_mut_ptr(), len, ptr::null_mut(), ov) }) {
            // The rest of a longer message comes with the next read.
            Err(e) if e.raw_os_error() == Some(ERROR_MORE_DATA as i32) => Ok(buf.len()),
            r => r.map(|n| n as usize),
        }
    }

    /// Writes `data` as one message; an empty one tells the client EOF.
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut rest = data;
        loop {
            if self.is_aborted() {
                return Err(io::Error::from_raw_os_error(ERROR_OPERATION_ABORTED as i32));
            }
            let len = rest.len().min(u32::MAX as usize) as u32;
            // SAFETY: rest is valid for len bytes until overlapped() returns.
            let n = overlapped(self.h, |ov| unsafe { WriteFile(self.h, rest.as_ptr(), len, ptr::null_mut(), ov) })? as usize;
            rest = &rest[n..];
            if rest.is_empty() {
                return Ok(());
            }
        }
    }
}

/// A client's reads: EOF at the server's empty message, when it has gone,
/// or when the read was cancelled.
impl io::Read for &Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Pipe::read_client(self, buf, None)
    }
}

impl Pipe {
    /// A client's read, as above, giving up (TimedOut) when nothing arrives
    /// within `timeout`.
    pub fn read_client(&self, buf: &mut [u8], timeout: Option<std::time::Duration>) -> io::Result<usize> {
        match self.read_within(buf, timeout) {
            Err(e) if matches!(e.raw_os_error(), Some(c) if c == ERROR_BROKEN_PIPE as i32 || c == ERROR_OPERATION_ABORTED as i32 || c == ERROR_PIPE_NOT_CONNECTED as i32) => Ok(0),
            r => r,
        }
    }
}

impl io::Write for &Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        Pipe::write(self, buf).map(|()| buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and closed once. Flushing is done by the
        // caller while an `Arc<Pipe>` is still held so handover can cancel an
        // in-flight flush via `abort()`; drop itself never blocks.
        unsafe {
            if self.server {
                DisconnectNamedPipe(self.h);
            }
            CloseHandle(self.h);
        }
    }
}

/// Starts an overlapped call and waits for it, with an event of its own.
fn overlapped(h: HANDLE, call: impl FnOnce(*mut OVERLAPPED) -> i32) -> io::Result<u32> {
    overlapped_within(h, None, call)
}

/// `overlapped`, cancelling the call when it has not completed within
/// `timeout` (then TimedOut).
fn overlapped_within(h: HANDLE, timeout: Option<std::time::Duration>, call: impl FnOnce(*mut OVERLAPPED) -> i32) -> io::Result<u32> {
    // SAFETY: a manual-reset, unnamed event, closed below.
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: an all-zero OVERLAPPED is valid; it and the event live until
    // GetOverlappedResult has waited for the call.
    let result = unsafe {
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = event;
        let started = call(&mut ov) != 0 || matches!(GetLastError(), ERROR_IO_PENDING | ERROR_MORE_DATA);
        let mut n = 0u32;
        let mut timed_out = false;
        if started && let Some(t) = timeout {
            let ms = t.as_millis().min(u32::MAX as u128 - 1) as u32;
            if WaitForSingleObject(event, ms) == WAIT_TIMEOUT {
                timed_out = true;
                // The call then completes as aborted, which the wait below sees.
                CancelIoEx(h, &ov);
            }
        }
        if !started {
            Err(io::Error::last_os_error())
        } else if GetOverlappedResult(h, &ov, &mut n, 1) == 0 {
            let e = io::Error::last_os_error();
            if timed_out { Err(io::Error::new(io::ErrorKind::TimedOut, "Kiri did not answer in time")) } else { Err(e) }
        } else {
            Ok(n)
        }
    };
    // SAFETY: the event is ours and no call uses it any more.
    unsafe { CloseHandle(event) };
    result
}

/// A security descriptor granting full access only to this user, SYSTEM and
/// the Administrators group, and nobody else. Built once; null on failure, in
/// which case the pipe falls back to the default descriptor.
fn security_descriptor() -> *mut std::ffi::c_void {
    static SD: OnceLock<usize> = OnceLock::new();
    *SD.get_or_init(|| build_sd().map(|p| p as usize).unwrap_or(0)) as *mut std::ffi::c_void
}

fn build_sd() -> Option<*mut std::ffi::c_void> {
    let sid = current_user_sid()?;
    // D:P = a protected DACL with only these ACEs (no Everyone, no inherit).
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})").encode_utf16().chain([0]).collect();
    let mut sd: *mut std::ffi::c_void = ptr::null_mut();
    // SAFETY: a valid NUL-terminated SDDL string; on success `sd` is a
    // LocalAlloc'd descriptor kept for the life of the process.
    let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1 as u32, &mut sd, ptr::null_mut()) };
    (ok != 0 && !sd.is_null()).then_some(sd)
}

/// Converts a Windows SID pointer into its S-1-… string form.
///
/// # Safety
/// `sid` must be null or point to a valid Windows SID.
pub(crate) unsafe fn sid_to_string(sid: *mut std::ffi::c_void) -> Option<String> {
    if sid.is_null() {
        return None;
    }
    let mut str_ptr: windows_sys::core::PWSTR = ptr::null_mut();
    // SAFETY: caller guarantees `sid` is a valid SID pointer; `str_ptr` is
    // freed with `LocalFree` before returning.
    unsafe {
        if ConvertSidToStringSidW(sid, &mut str_ptr) == 0 || str_ptr.is_null() {
            return None;
        }
        let mut n = 0;
        while *str_ptr.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(str_ptr, n));
        LocalFree(str_ptr.cast());
        Some(s)
    }
}

/// The current process user's SID as an S-1-… string.
pub(crate) fn current_user_sid() -> Option<String> {
    // SAFETY: a sequence of token calls, each checked; buffers sized from the
    // first (length-query) call.
    unsafe {
        let mut token: HANDLE = 0;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len);
        if len == 0 {
            CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let got = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        CloseHandle(token);
        if got == 0 {
            return None;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_to_string(user.User.Sid)
    }
}
