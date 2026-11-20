//! The USB helper: capturing a device from macOS takes root, so the first
//! attach installs a small LaunchDaemon (one administrator prompt, once),
//! and every attach after that just asks it.
//!
//! The helper is a root-owned copy of this binary in
//! /Library/PrivilegedHelperTools, run by launchd as `kiri usb helper`. It
//! listens on a socket in /var/run and takes requests only from the user
//! who installed it. A request is a device's location plus a connection,
//! passed as a file descriptor: the caller (`kiri usb attach`, the window)
//! makes the connection to its own daemon, so the helper never opens a path
//! a user named. For each device the helper starts `kiri usb serve`, which
//! captures it and serves it over that connection until either side lets
//! go, then gives it back to macOS.
//!
//! The helper is replaced when it differs from the Kiri asking (an update):
//! that is one more prompt.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::flavor::Name;

/// The LaunchDaemon's label, and the name of everything it installs.
pub const LABEL: Name = Name("dev.kiri.usb", "dev.kiri.usb.dev");

/// Devices one helper serves at once; vhci has 8 ports per speed.
const MAX_SESSIONS: usize = 32;

fn binary() -> PathBuf {
    PathBuf::from(format!("/Library/PrivilegedHelperTools/{LABEL}"))
}
fn plist() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{LABEL}.plist"))
}
fn socket() -> PathBuf {
    PathBuf::from(format!("/var/run/{LABEL}.sock"))
}

/// The Kiri build the installed helper was copied from, written beside it.
fn source() -> PathBuf {
    PathBuf::from(format!("/Library/PrivilegedHelperTools/{LABEL}.source"))
}

/// This binary's SHA-256, as hex.
fn digest(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    Ok(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether the installed helper was copied from this Kiri. (Not compared
/// byte for byte: the copy is signed again on its own, see `install`.)
fn current() -> bool {
    if !binary().exists() || !plist().exists() {
        return false;
    }
    let (Ok(me), Ok(installed)) = (std::env::current_exe(), std::fs::read_to_string(source())) else { return false };
    digest(&me).is_ok_and(|d| d == installed.trim())
}

/// Installs (or replaces) the helper: the one administrator prompt.
fn install() -> io::Result<()> {
    let me = std::env::current_exe()?.canonicalize()?;
    let (bin, plist, label) = (binary(), plist(), LABEL.get());
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;");
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>usb</string><string>helper</string><string>{label}</string><string>{uid}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>/var/log/{label}.log</string>
</dict>
</plist>
"#,
        esc(&bin.to_string_lossy())
    );
    let tmp = std::env::temp_dir().join(format!("{label}.{}.plist", std::process::id()));
    std::fs::write(&tmp, body)?;
    let sum = digest(&me)?;
    let q = |p: &Path| sh_quote(&p.to_string_lossy());
    // The copy is root's: a helper that ran a binary in the user's app
    // bundle would run whatever replaced it, as root. Kiri's own signature
    // seals the app's Info.plist, which the copy is without, so macOS
    // wouldn't run it: it gets a signature of its own (ad hoc, no
    // entitlements; it needs none). Then which Kiri it came from.
    let command = format!(
        "set -e; mkdir -p /Library/PrivilegedHelperTools; cp {me} {bin}.new; \
         /usr/bin/codesign --force --sign - --identifier {label} {bin}.new; \
         chown root:wheel {bin}.new; chmod 755 {bin}.new; mv -f {bin}.new {bin}; echo {sum} > {src}; chmod 644 {src}; \
         cp {tmp} {plist}; chown root:wheel {plist}; chmod 644 {plist}; \
         launchctl bootout system/{label} 2>/dev/null || true; launchctl bootstrap system {plist}",
        me = q(&me),
        bin = q(&bin),
        src = q(&source()),
        tmp = q(&tmp),
        plist = q(&plist)
    );
    let r = crate::setup::as_admin(&command, "Kiri wants to install its USB helper, so it can connect USB devices to its Linux VM. It asks once.");
    let _ = std::fs::remove_file(&tmp);
    r
}

/// Removes the helper (`kiri usb helper remove`, and uninstalling Kiri).
pub fn remove() -> io::Result<()> {
    if !plist().exists() && !binary().exists() {
        return Ok(());
    }
    let (bin, plist) = (binary(), plist());
    let command = format!(
        "launchctl bootout system/{} 2>/dev/null; rm -f {} {} {} {}",
        LABEL.get(),
        sh_quote(&plist.to_string_lossy()),
        sh_quote(&bin.to_string_lossy()),
        sh_quote(&source().to_string_lossy()),
        sh_quote(&socket().to_string_lossy())
    );
    crate::setup::as_admin(&command, "Kiri wants to remove its USB helper.")?;
    println!("removed the USB helper");
    Ok(())
}

/// A connection to the helper, installing or replacing it first if it
/// isn't this Kiri.
fn connect() -> io::Result<UnixStream> {
    if !current() {
        install()?;
    }
    // launchd starts it on bootstrap; give it a moment to listen.
    let mut last = None;
    for _ in 0..50 {
        match UnixStream::connect(socket()) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::other(format!("the USB helper isn't answering ({})", last.map(|e| e.to_string()).unwrap_or_default())))
}

/// Attaches the device at `location`: the connection to the daemon is made
/// here and handed to the helper, which captures the device and serves it.
/// Returns once the device is captured, or with why it couldn't be.
pub fn attach(location: &str, name: &str, speed: u32, devid: u32) -> io::Result<()> {
    attach_through(connect()?, location, name, speed, devid)
}

/// The same, for the daemon attaching a device again when it comes back:
/// never the administrator prompt, so only through a helper that is
/// already this Kiri's.
pub fn reattach(location: &str, name: &str, speed: u32, devid: u32) -> io::Result<()> {
    if !current() {
        return Err(io::Error::other("the USB helper is not this Kiri's (attach it once from the window to update it)"));
    }
    attach_through(UnixStream::connect(socket())?, location, name, speed, devid)
}

fn attach_through(helper: UnixStream, location: &str, name: &str, speed: u32, devid: u32) -> io::Result<()> {
    let (stream, _reader) = super::usbip::connect(&crate::client::socket()?, location, name, speed, devid)?;
    send_fd(&helper, format!("attach {location}\n").as_bytes(), stream.as_raw_fd())?;
    // The helper and its server hold the connection now.
    drop(stream);
    helper.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut line = String::new();
    BufReader::new(&helper).read_line(&mut line)?;
    match line.trim_end() {
        "ok" => Ok(()),
        "" => Err(io::Error::other("the USB helper stopped before it took the device")),
        l => Err(io::Error::other(l.strip_prefix("error: ").unwrap_or(l).to_owned())),
    }
}

// --- The helper itself (root, under launchd) ----------------------------------------

/// `kiri usb helper LABEL UID`: serves attach requests from UID until
/// launchd stops it.
pub fn serve(label: &str, uid: &str) -> io::Result<()> {
    // SAFETY: getuid has no preconditions.
    if unsafe { libc::getuid() } != 0 {
        return Err(io::Error::other("the USB helper runs as root, under launchd"));
    }
    let uid: libc::uid_t = uid.parse().map_err(|_| io::Error::other("bad uid"))?;
    if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return Err(io::Error::other("bad label"));
    }
    let path = PathBuf::from(format!("/var/run/{label}.sock"));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // Anyone may connect; only `uid` (and root) is answered.
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
    // Children are reaped by their own threads; a dead one's device has
    // already gone back to macOS.
    let exe = std::env::current_exe()?;
    let sessions = Arc::new(AtomicUsize::new(0));
    super::mac::log(&format!("helper listening for uid {uid}"));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (exe, sessions) = (exe.clone(), sessions.clone());
        std::thread::spawn(move || {
            if let Err(e) = request(&conn, uid, &exe, &sessions) {
                let _ = writeln!(&conn, "error: {e}");
            }
        });
    }
    Ok(())
}

fn request(conn: &UnixStream, uid: libc::uid_t, exe: &Path, sessions: &Arc<AtomicUsize>) -> io::Result<()> {
    let (mut peer_uid, mut peer_gid) = (0, 0);
    // SAFETY: getpeereid on a connected socket we own, writing two ids.
    if unsafe { libc::getpeereid(conn.as_raw_fd(), &mut peer_uid, &mut peer_gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if peer_uid != uid && peer_uid != 0 {
        return Err(io::Error::other("not your USB helper"));
    }
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (line, fd) = recv_fd(conn)?;
    let line = String::from_utf8_lossy(&line).into_owned();
    let Some(location) = line.trim_end().strip_prefix("attach ") else { return Err(io::Error::other("unknown request")) };
    let Some(fd) = fd else { return Err(io::Error::other("no connection came with the request")) };
    let valid = location.strip_prefix("0x").is_some_and(|h| !h.is_empty() && h.len() <= 8 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    if !valid {
        return Err(io::Error::other(format!("bad location {location:?}")));
    }
    // The connection must be a socket: the server speaks USB/IP over it.
    // SAFETY: fstat into a zeroed stat.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 || st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(io::Error::other("the connection isn't a socket"));
    }
    if sessions.fetch_add(1, Ordering::SeqCst) >= MAX_SESSIONS {
        sessions.fetch_sub(1, Ordering::SeqCst);
        return Err(io::Error::other("too many devices attached"));
    }
    let status = conn.try_clone()?;
    let result = spawn(exe, location, fd, status);
    let mut child = match result {
        Ok(c) => c,
        Err(e) => {
            sessions.fetch_sub(1, Ordering::SeqCst);
            return Err(e);
        }
    };
    let _ = child.wait();
    sessions.fetch_sub(1, Ordering::SeqCst);
    Ok(())
}

/// `kiri usb serve LOCATION --fd 3 --status 4`, with the USB/IP
/// connection as fd 3 and the requester's connection as fd 4 for its "ok".
fn spawn(exe: &Path, location: &str, conn: OwnedFd, status: UnixStream) -> io::Result<std::process::Child> {
    let (a, b) = (conn.as_raw_fd(), status.as_raw_fd());
    let mut cmd = Command::new(exe);
    cmd.args(["usb", "serve", location, "--fd", "3", "--status", "4"])
        .env_clear()
        .env("HOME", "/var/root")
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    // SAFETY: only dup2 between fork and exec. Through a spare number
    // first, so neither lands on the other.
    unsafe {
        cmd.pre_exec(move || {
            let (ta, tb) = (libc::dup(a), libc::dup(b));
            if ta < 0 || tb < 0 || libc::dup2(ta, 3) < 0 || libc::dup2(tb, 4) < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::close(ta);
            libc::close(tb);
            Ok(())
        });
    }
    let child = cmd.spawn();
    drop(conn);
    drop(status);
    child
}

// --- Descriptor passing ------------------------------------------------------------

fn send_fd(sock: &UnixStream, data: &[u8], fd: RawFd) -> io::Result<()> {
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
    // SAFETY: a msghdr over buffers that outlive the call, with one
    // SCM_RIGHTS header written inside `control`.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
        if libc::sendmsg(sock.as_raw_fd(), &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// One request line and the descriptor sent with it, if any.
fn recv_fd(sock: &UnixStream) -> io::Result<(Vec<u8>, Option<OwnedFd>)> {
    let mut buf = vec![0u8; 256];
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut fd = None;
    // SAFETY: recvmsg into buffers we own; only SCM_RIGHTS headers that fit
    // are read, and each descriptor received is owned from here on.
    let n = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let raw = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>().add(i));
                    let owned = OwnedFd::from_raw_fd(raw);
                    // One is expected; any more are closed.
                    if fd.is_none() {
                        fd = Some(owned);
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::other("the request carried too many descriptors"));
        }
        n as usize
    };
    buf.truncate(n);
    // The rest of the line, if it came in pieces (it doesn't, in practice).
    if !buf.ends_with(b"\n") && buf.len() < 256 {
        let mut more = [0u8; 1];
        let mut s = sock;
        while buf.len() < 256 && s.read(&mut more)? == 1 {
            buf.push(more[0]);
            if more[0] == b'\n' {
                break;
            }
        }
    }
    Ok((buf, fd))
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_descriptor_crosses_with_its_line() {
        let (a, b) = UnixStream::pair().unwrap();
        let (x, mut y) = UnixStream::pair().unwrap();
        send_fd(&a, b"attach 0x01100000\n", x.as_raw_fd()).unwrap();
        drop(x);
        let (line, fd) = recv_fd(&b).unwrap();
        assert_eq!(line, b"attach 0x01100000\n");
        let mut got = UnixStream::from(fd.unwrap());
        got.write_all(b"hi").unwrap();
        let mut buf = [0u8; 2];
        y.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[test]
    fn a_line_without_a_descriptor() {
        let (a, b) = UnixStream::pair().unwrap();
        (&a).write_all(b"attach 0x1\n").unwrap();
        let (line, fd) = recv_fd(&b).unwrap();
        assert_eq!(line, b"attach 0x1\n");
        assert!(fd.is_none());
    }
}
