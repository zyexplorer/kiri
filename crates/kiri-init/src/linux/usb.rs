//! USB passthrough: the Mac captures a device and speaks USB/IP for it over
//! one vsock connection; the guest kernel's vhci_hcd turns that connection
//! into a USB bus, so the device shows up in the VM (`/dev/ttyUSB0`,
//! `/dev/input/event0`, `/dev/bus/usb/...`) and containers get it with
//! `--device` like on any Linux machine.
//!
//! Each connection opens with `speed u32 | devid u32` (little-endian; the
//! kernel's USB_SPEED_* and USB/IP's bus<<16|dev). The agent picks a free
//! vhci port, answers `port i32` (or a negative errno), then hands the socket
//! to the kernel through vhci's `attach` file and closes its own copy: from
//! then on the kernel reads and writes USB/IP on it. When either side closes
//! the connection, the kernel detaches the device.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use kiri_proto::USB_PORT;

/// Shared with the engine, which reads the same for a Linux host's devices.
#[path = "../../../kiri/src/usb/sysfs.rs"]
mod sysfs;

const VHCI: &str = "/sys/devices/platform/vhci_hcd.0";
/// USB_SPEED_SUPER: needs one of vhci's SuperSpeed ports.
const SPEED_SUPER: u32 = 5;
/// VDEV_ST_NULL: a port with nothing attached.
const PORT_FREE: u32 = 4;

pub fn listen() -> io::Result<OwnedFd> {
    super::relay::listen_vsock(USB_PORT)
}

/// Attaches every pending connection. A failed one is logged and closed;
/// the Mac sees the connection end.
pub fn accept(listener: &OwnedFd) {
    loop {
        // SAFETY: accept4 on a listening socket we own; the fd is owned immediately.
        let fd = unsafe { libc::accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if fd < 0 {
            return;
        }
        // SAFETY: a fresh descriptor from accept4.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        if let Err(e) = attach(sock) {
            eprintln!("kiri-init: usb: {e}");
        }
    }
}

fn attach(sock: OwnedFd) -> io::Result<()> {
    let mut stream = std::os::unix::net::UnixStream::from(sock);
    // The header follows the connect at once; never stall the agent on a
    // peer that sends nothing.
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut head = [0u8; 8];
    stream.read_exact(&mut head)?;
    let speed = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let devid = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    let port = match free_port(speed) {
        Ok(p) => p,
        Err(e) => {
            let _ = stream.write_all(&(-e.raw_os_error().unwrap_or(libc::EBUSY)).to_le_bytes());
            return Err(e);
        }
    };
    stream.set_read_timeout(None)?;
    // Answer before attaching: once attached, the kernel owns the stream.
    stream.write_all(&(port as i32).to_le_bytes())?;
    fs::write(format!("{VHCI}/attach"), format!("{port} {} {devid} {speed}", stream.as_raw_fd()))
        .map_err(|e| io::Error::new(e.kind(), format!("vhci attach on port {port}: {e}")))?;
    // The kernel holds its own reference to the socket now.
    Ok(())
}

/// The first free vhci port of the right kind: SuperSpeed devices need an
/// `ss` port, everything else an `hs` one.
fn free_port(speed: u32) -> io::Result<u32> {
    let status = fs::read_to_string(format!("{VHCI}/status")).map_err(|e| io::Error::new(e.kind(), format!("no USB/IP support in this kernel ({e})")))?;
    let hub = if speed == SPEED_SUPER { "ss" } else { "hs" };
    pick(&status, hub).ok_or_else(|| io::Error::from_raw_os_error(libc::EBUSY))
}

/// `PORT\tPATH\tWHAT` for every node of every attached device (see
/// `kiri_proto::Msg::UsbNodes`).
pub fn nodes() -> Vec<String> {
    let status = fs::read_to_string(format!("{VHCI}/status")).unwrap_or_default();
    attached(&status)
        .into_iter()
        .flat_map(|(port, busid)| {
            let dev = std::path::Path::new("/sys/bus/usb/devices").join(&busid);
            sysfs::nodes(&dev).into_iter().map(move |n| format!("{port}\t{}\t{}", n.path, n.what))
        })
        .collect()
}

/// (vhci port, sysfs name) of each attached device: `local_busid` is the
/// name, `0-0` on a free port.
fn attached(status: &str) -> Vec<(u32, String)> {
    status
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (port, busid) = (f.get(1)?.parse().ok()?, *f.get(6)?);
            (busid != "0-0" && busid.bytes().all(|c| c.is_ascii_digit() || c == b'-' || c == b'.')).then(|| (port, busid.to_owned()))
        })
        .collect()
}

fn pick(status: &str, hub: &str) -> Option<u32> {
    status.lines().skip(1).find_map(|line| {
        let mut f = line.split_whitespace();
        let (h, port, sta) = (f.next()?, f.next()?, f.next()?);
        (h == hub && sta.parse::<u32>().ok() == Some(PORT_FREE)).then(|| port.parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "hub port sta spd dev      sockfd local_busid\n\
        hs  0000 006 002 00010002 000005 1-1\n\
        hs  0001 004 000 00000000 000000 0-0\n\
        ss  0008 004 000 00000000 000000 0-0\n";

    #[test]
    fn picks_a_free_port_of_the_right_kind() {
        assert_eq!(pick(STATUS, "hs"), Some(1));
        assert_eq!(pick(STATUS, "ss"), Some(8));
        assert_eq!(pick("hub port sta\nhs 0000 006\n", "hs"), None);
    }

    #[test]
    fn lists_the_attached_devices() {
        assert_eq!(attached(STATUS), [(0, "1-1".to_owned())]);
    }
}
