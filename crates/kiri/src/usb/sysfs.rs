//! Where a USB device shows up in Linux: its device nodes, read from sysfs.
//!
//! One file, shared: the guest agent (kiri-init) reads it for the devices
//! passed through to the VM, and the engine on a Linux host or in WSL reads
//! it for the machine's own devices. Only std, no unsafe.

use std::path::Path;

/// One node a container can be given with `--device` (or, for a network
/// interface, its name), and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub path: String,
    pub what: &'static str,
}

/// The nodes of the USB device at `dev` (a /sys/bus/usb/devices/<busid>
/// directory): serial ports, input and raw HID, sound, video, disks and
/// network interfaces its interfaces made, then its raw USB node.
pub fn nodes(dev: &Path) -> Vec<Node> {
    let mut out = Vec::new();
    walk(dev, "", 0, &mut out);
    out.sort_by(|a, b| rank(a.what).cmp(&rank(b.what)).then_with(|| natural(&a.path).cmp(&natural(&b.path))));
    out.dedup();
    let read = |f: &str| std::fs::read_to_string(dev.join(f)).map(|s| s.trim().to_owned()).unwrap_or_default();
    if let (Ok(bus), Ok(num)) = (read("busnum").parse::<u32>(), read("devnum").parse::<u32>()) {
        out.push(Node { path: format!("/dev/bus/usb/{bus:03}/{num:03}"), what: "raw USB" });
    }
    out
}

/// The node an entry named `name` under a directory named `parent` stands
/// for, if it is one.
fn node(parent: &str, name: &str) -> Option<Node> {
    let num = |prefix: &str| name.strip_prefix(prefix).filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if num("ttyUSB").is_some() || num("ttyACM").is_some() {
        return Some(Node { path: format!("/dev/{name}"), what: "serial port" });
    }
    if num("event").is_some() {
        return Some(Node { path: format!("/dev/input/{name}"), what: "input events" });
    }
    if parent == "hidraw" && num("hidraw").is_some() {
        return Some(Node { path: format!("/dev/{name}"), what: "raw HID" });
    }
    if let Some(n) = num("card").filter(|_| parent == "sound") {
        return Some(Node { path: format!("/dev/snd/controlC{n}"), what: "sound card" });
    }
    if parent == "video4linux" && num("video").is_some() {
        return Some(Node { path: format!("/dev/{name}"), what: "video" });
    }
    if parent == "block" && name.len() > 2 && name.starts_with("sd") && name[2..].bytes().all(|b| b.is_ascii_lowercase()) {
        return Some(Node { path: format!("/dev/{name}"), what: "disk" });
    }
    if parent == "net" && !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.') {
        return Some(Node { path: name.to_owned(), what: "network interface" });
    }
    None
}

/// Real directories only (sysfs is full of links back up the tree), and
/// not into another USB device (a hub's children are listed on their own).
fn walk(dir: &Path, parent: &str, depth: u32, out: &mut Vec<Node>) {
    if depth > 7 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(t) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(n) = node(parent, &name) {
            out.push(n);
        }
        if t.is_dir() && !t.is_symlink() && !is_usb_device(&name) {
            walk(&e.path(), &name, depth + 1, out);
        }
    }
}

/// `1-2` or `1-2.3`: a USB device, not one of its interfaces (`1-2:1.0`).
fn is_usb_device(name: &str) -> bool {
    let mut p = name.splitn(2, '-');
    let (Some(bus), Some(rest)) = (p.next(), p.next()) else { return false };
    !bus.is_empty() && bus.bytes().all(|b| b.is_ascii_digit()) && !rest.contains(':') && rest.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

/// What a container most likely wants first.
fn rank(what: &str) -> u8 {
    match what {
        "serial port" => 0,
        "network interface" => 1,
        "disk" => 2,
        "video" => 3,
        "sound card" => 4,
        "input events" => 5,
        "raw HID" => 6,
        _ => 7,
    }
}

/// `/dev/input/event10` after `event9`.
fn natural(s: &str) -> (String, u64) {
    let digits = s.bytes().rev().take_while(u8::is_ascii_digit).count();
    let (head, tail) = s.split_at(s.len() - digits);
    (head.to_owned(), tail.parse().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_each_kind_of_node_and_ignores_links_and_child_devices() {
        let root = std::env::temp_dir().join(format!("kiri-usb-nodes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dev = root.join("1-2");
        let mk = |p: &str| std::fs::create_dir_all(dev.join(p)).unwrap();
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("busnum"), "1\n").unwrap();
        std::fs::write(dev.join("devnum"), "7\n").unwrap();
        mk("1-2:1.0/ttyUSB0/tty/ttyUSB0");
        mk("1-2:1.1/0003:1532:00D6.0003/input/input12/event10");
        mk("1-2:1.1/0003:1532:00D6.0003/input/input11/event9");
        mk("1-2:1.1/0003:1532:00D6.0003/hidraw/hidraw2");
        mk("1-2:1.2/sound/card1/pcmC1D0c");
        mk("1-2:1.3/net/eth1");
        mk("1-2:1.4/host0/target0:0:0/0:0:0:0/block/sda/sda1");
        // A device behind a hub on this one: its nodes are its own.
        mk("1-2.1/1-2.1:1.0/ttyACM0");
        // A link back up the tree must not be followed.
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, dev.join("1-2:1.0/subsystem")).unwrap();
        let got: Vec<(String, &str)> = nodes(&dev).into_iter().map(|n| (n.path, n.what)).collect();
        assert_eq!(
            got,
            [
                ("/dev/ttyUSB0".into(), "serial port"),
                ("eth1".into(), "network interface"),
                ("/dev/sda".into(), "disk"),
                ("/dev/snd/controlC1".into(), "sound card"),
                ("/dev/input/event9".into(), "input events"),
                ("/dev/input/event10".into(), "input events"),
                ("/dev/hidraw2".into(), "raw HID"),
                ("/dev/bus/usb/001/007".into(), "raw USB"),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tells_devices_from_interfaces() {
        assert!(is_usb_device("1-2") && is_usb_device("3-1.4.2"));
        assert!(!is_usb_device("1-2:1.0") && !is_usb_device("usb1") && !is_usb_device("net"));
    }
}
