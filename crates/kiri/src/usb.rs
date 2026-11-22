//! `kiri usb`: USB devices for containers (beta).
//!
//! On a Mac the containers run in a VM with no USB controller, so a device
//! has to be passed through: `kiri usb attach` captures it from macOS
//! (through the USB helper, `helper.rs`, installed on first use with one
//! administrator prompt) and hands it to the VM, where
//! it shows up like on any Linux machine (`/dev/ttyUSB0`, `/dev/input/…`,
//! `/dev/bus/usb/…`) for `docker run --device`. `kiri usb detach` gives it
//! back to macOS.
//!
//! On Linux the containers run on the host's own kernel and already see its
//! devices; on Windows, WSL 2 gets them through usbipd-win.

use std::io;

#[cfg(target_os = "macos")]
mod fake;
#[cfg(target_os = "macos")]
pub mod helper;
#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(target_os = "macos")]
mod usbip;
/// Where a device shows up in Linux (shared with the guest agent).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub mod sysfs;

const USAGE: &str = "usage: kiri usb [list] | attach DEVICE | detach DEVICE | helper remove\n\
    DEVICE is a location (0x02114000), vendor:product (1532:00d7) or part of the name";

pub fn run(args: &[String]) -> io::Result<()> {
    match (args.first().map(String::as_str), args.get(1)) {
        (None | Some("list" | "ls"), _) => list(),
        (Some("attach"), Some(d)) => attach(d),
        (Some("detach"), Some(d)) => detach(d),
        #[cfg(target_os = "macos")]
        (Some("serve"), Some(location)) => mac::serve(location, flag(args, "--fd"), flag(args, "--status")),
        #[cfg(target_os = "macos")]
        (Some("helper"), Some(r)) if r == "remove" => helper::remove(),
        #[cfg(target_os = "macos")]
        (Some("helper"), Some(label)) => helper::serve(label, args.get(2).map(String::as_str).unwrap_or("")),
        // Software devices for the end-to-end tests (tests/usb.sh).
        #[cfg(target_os = "macos")]
        (Some("fake"), _) => fake::run(&args[1..]),
        _ => Err(io::Error::other(USAGE)),
    }
}

#[cfg(target_os = "macos")]
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1))
}

#[cfg(target_os = "macos")]
fn list() -> io::Result<()> {
    let attached = attached();
    let waiting = waiting();
    let devices = mac::devices()?;
    if devices.is_empty() && waiting.is_empty() {
        println!("No USB devices.");
        return Ok(());
    }
    println!("{:<12} {:<10} {:<6} {:<10} {:<9} NAME", "LOCATION", "ID", "SPEED", "STATE", "KIND");
    for d in &devices {
        let state = if attached.iter().any(|a| a == &d.location()) {
            "in Kiri"
        } else if waiting.iter().any(|(l, _)| l == &d.location()) {
            "waiting"
        } else {
            "on the Mac"
        };
        let kind = match d.kind() { "" => "usb", k => k };
        println!("{:<12} {:04x}:{:04x}  {:<6} {:<10} {:<9} {}", d.location(), d.vendor, d.product, d.speed_name(), state, kind, d.name);
    }
    // Left the bus: Kiri attaches them again when they come back.
    for (l, name) in waiting.iter().filter(|(l, _)| !devices.iter().any(|d| &d.location() == l)) {
        println!("{:<12} {:<10} {:<6} {:<10} {:<9} {}", l, "-", "-", "waiting", "-", name);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn attached() -> Vec<String> {
    crate::client::get("/kiri/usb")
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|s| s["Waiting"] != true)
        .filter_map(|s| s["Location"].as_str().map(str::to_owned))
        .collect()
}

/// The devices that left and that Kiri attaches again when they come back,
/// as (location, name).
#[cfg(target_os = "macos")]
fn waiting() -> Vec<(String, String)> {
    crate::client::get("/kiri/usb")
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|s| s["Waiting"] == true)
        .filter_map(|s| Some((s["Location"].as_str()?.to_owned(), s["Name"].as_str().unwrap_or_default().to_owned())))
        .collect()
}

#[cfg(target_os = "macos")]
fn attach(which: &str) -> io::Result<()> {
    let d = mac::find(which)?;
    if attached().contains(&d.location()) {
        println!("{} is already in Kiri.", d.name);
        return Ok(());
    }
    attach_device(&d)?;
    println!("{} is in Kiri now. In a container: --device /dev/ttyUSB0 (serial), /dev/input (HID) or /dev/bus/usb (raw).", d.name);
    println!("It is gone from the Mac until `kiri usb detach {}`.", d.location());
    Ok(())
}

/// Hands a device to the VM through the USB helper (installing it the
/// first time: the one administrator prompt). The window uses this too.
#[cfg(target_os = "macos")]
pub fn attach_device(d: &mac::Device) -> io::Result<()> {
    // Wake the engine first, so the attach doesn't wait on a boot.
    crate::client::request("POST", "/kiri/wake", None)?;
    helper::attach(&d.location(), &d.name, d.linux_speed(), d.devid()).map_err(|e| io::Error::other(format!("{} was not attached: {e}", d.name)))
}

/// Gives a device back to macOS.
#[cfg(target_os = "macos")]
pub fn detach_location(location: &str) -> io::Result<()> {
    let r = crate::client::request("DELETE", &format!("/kiri/usb/{location}"), None)?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn detach(which: &str) -> io::Result<()> {
    let d = match mac::find(which) {
        Ok(d) => d,
        // Away from the bus and waited for: `kiri usb ls` gives its location.
        Err(e) => {
            return match waiting().into_iter().find(|(l, _)| l == which) {
                Some((l, name)) => detach_location(&l).map(|()| println!("Kiri stops waiting for {name}; it stays on the Mac when it comes back.")),
                None => Err(e),
            };
        }
    };
    detach_location(&d.location())?;
    println!("{} is back on the Mac.", d.name);
    Ok(())
}

// Linux: the containers share the host's kernel, so there is nothing to
// pass through. Windows runs this in WSL 2, where usbipd-win attaches.
#[cfg(not(target_os = "macos"))]
fn list() -> io::Result<()> {
    let wsl = std::fs::read_to_string("/proc/version").is_ok_and(|v| v.contains("microsoft"));
    let dir = std::path::Path::new("/sys/bus/usb/devices");
    // WSL has no USB bus at all until usbipd-win attaches a device.
    let Ok(entries) = std::fs::read_dir(dir) else {
        if wsl {
            println!("No USB devices are attached to WSL. In Windows: usbipd list, then usbipd attach --wsl --busid <BUSID>.");
        } else {
            println!("No USB devices.");
        }
        return Ok(());
    };
    drop(entries);
    let devices = sysfs_devices(dir);
    println!("{:<22} {:<10} {:<14} NAME", "DEVICE", "ID", "TTY");
    for d in devices {
        println!("{:<22} {:<10} {:<14} {}", d.node, d.id, d.tty.as_deref().unwrap_or("-"), d.name);
    }
    println!("\nContainers already see these devices: docker run --device /dev/ttyUSB0 …");
    if wsl {
        println!("To add one from Windows: usbipd attach --wsl --busid <BUSID>");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn attach(_: &str) -> io::Result<()> {
    Err(io::Error::other(
        "nothing to attach here: containers already see this machine's USB devices (docker run --device /dev/ttyUSB0 …). Under Windows, attach the device to WSL with usbipd-win first.",
    ))
}

#[cfg(not(target_os = "macos"))]
fn detach(_: &str) -> io::Result<()> {
    attach("")
}

/// What a device is, for its icon in the window: from its interfaces'
/// (number, class, subclass, protocol), then its name and vendor.
/// "mouse", "keyboard", "network", "serial", "radio", "storage", "audio",
/// "camera" or "" (a USB device).
pub fn kind_of(device_class: u8, interfaces: &[(u8, u8, u8, u8)], vendor: u16, name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    // Radios that speak serial: named for what they are.
    if has(&["zigbee", "z-wave", "zwave", "thread", "lora", "conbee", "skyconnect", "sonoff"]) {
        return "radio";
    }
    let mut ifs = interfaces.to_vec();
    ifs.sort();
    // The first HID boot interface is what a combo device mostly is (a
    // mouse with macro keys, a keyboard with a media knob).
    for &(_, class, sub, proto) in &ifs {
        if class == 3 && sub == 1 && proto == 2 {
            return "mouse";
        }
        if class == 3 && sub == 1 && proto == 1 {
            return "keyboard";
        }
    }
    let class = |c: u8| ifs.iter().any(|i| i.1 == c) || device_class == c;
    if class(14) {
        return "camera";
    }
    if class(1) {
        return "audio";
    }
    if class(8) {
        return "storage";
    }
    // CDC: ECM/NCM/EEM networking, else a serial port (ACM).
    if ifs.iter().any(|i| i.1 == 2 && matches!(i.2, 6 | 12 | 13)) || class(0xe0) && has(&["ethernet", "lan", "rndis"]) {
        return "network";
    }
    if has(&["ethernet", " lan", "lan ", "gbe", "network adapter", "2.5g", "1g", "10/100"]) {
        return "network";
    }
    // Serial bridges: CP210x, CH34x, FTDI, PL2303, and CDC ACM.
    if matches!(vendor, 0x10c4 | 0x1a86 | 0x0403 | 0x067b) || class(2) || has(&["uart", "serial", "arduino", "esp32", "ttl"]) {
        return "serial";
    }
    if has(&["mouse"]) {
        return "mouse";
    }
    if has(&["keyboard"]) {
        return "keyboard";
    }
    if class(3) {
        return "";
    }
    ""
}

#[cfg(test)]
mod kind_tests {
    use super::kind_of;

    #[test]
    fn devices_get_the_right_kind() {
        // Razer Basilisk: interface 0 a boot mouse, 1 a keyboard for its macro keys.
        assert_eq!(kind_of(0, &[(1, 3, 1, 1), (0, 3, 1, 2), (2, 3, 0, 0)], 0x1532, "Razer Basilisk V3 Pro 35K"), "mouse");
        // ROG AZOTH: a keyboard, whatever its name says.
        assert_eq!(kind_of(0, &[(0, 3, 1, 1), (1, 3, 0, 0)], 0x0b05, "ROG AZOTH"), "keyboard");
        assert_eq!(kind_of(0, &[(0, 0xff, 0, 0)], 0x10c4, "Sonoff Zigbee 3.0 USB Dongle Plus"), "radio");
        assert_eq!(kind_of(0, &[(0, 0xff, 0, 0)], 0x10c4, "CP2102 USB to UART Bridge Controller"), "serial");
        assert_eq!(kind_of(2, &[(0, 2, 2, 1), (1, 10, 0, 0)], 0x2341, "Uno"), "serial");
        assert_eq!(kind_of(0, &[(0, 0xff, 0xff, 0)], 0x0bda, "USB 10_100_1G_2_5G LAN"), "network");
        assert_eq!(kind_of(0, &[(0, 2, 13, 0), (1, 10, 0, 1)], 0x0bda, "Adapter"), "network");
        assert_eq!(kind_of(0, &[(0, 1, 1, 0), (1, 1, 2, 0), (2, 3, 0, 0)], 0x352f, "DM30 USB Microphone"), "audio");
        assert_eq!(kind_of(0, &[(0, 14, 1, 0), (1, 14, 2, 0)], 0x046d, "C920"), "camera");
        assert_eq!(kind_of(0, &[(0, 8, 6, 80)], 0x0781, "Ultra"), "storage");
        assert_eq!(kind_of(0, &[(0, 3, 0, 0)], 0x0bda, "USB2.0 HID"), "");
    }
}

// --- The window's USB view -----------------------------------------------------------
//
// One shape on every system: { type: "usb", devices: [{ key, name, id,
// speed, state, path, kind, maker, serial, bus, dev, busid, since }], note }.
// maker, serial, bus/dev (or Windows' busid) and since (Unix seconds, when
// it went into Kiri) are there when the system knows them. `state` is "kiri" (in the VM), "host" (on
// this computer, can be attached) or "shared" (containers see it as it is).

/// The devices for the window's USB view.
#[cfg(target_os = "macos")]
pub fn ui_list() -> serde_json::Value {
    let daemon: Vec<serde_json::Value> = crate::client::get("/kiri/usb").ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let devices = match mac::devices() {
        Ok(d) => d,
        Err(e) => return serde_json::json!({"type": "usb", "devices": [], "error": e.to_string()}),
    };
    for d in &devices {
        let loc = d.location();
        let inside = daemon.iter().find(|a| a["Location"].as_str() == Some(&loc));
        rows.push(serde_json::json!({
            "key": loc, "name": d.name, "id": format!("{:04x}:{:04x}", d.vendor, d.product),
            "speed": d.speed_name(), "state": state_of(inside), "path": "", "kind": d.kind(),
            "maker": d.maker, "serial": d.serial, "bus": d.bus(), "dev": d.address,
            "since": inside.and_then(|a| a["Since"].as_i64()), "paths": inside.map(paths).unwrap_or_default(),
        }));
    }
    // In the VM but not on the Mac's bus (a test device), or away and waited
    // for: listed all the same.
    for a in &daemon {
        let loc = a["Location"].as_str().unwrap_or_default();
        if !loc.is_empty() && !rows.iter().any(|r| r["key"] == loc) {
            rows.push(serde_json::json!({"key": loc, "name": a["Name"], "id": "", "speed": "", "state": state_of(Some(a)), "path": "", "since": a["Since"], "paths": paths(a)}));
        }
    }
    serde_json::json!({"type": "usb", "devices": rows, "can": "attach",
        "note": "A device in Kiri is gone from the Mac until you detach it. In a container: --device /dev/ttyUSB0 (serial), /dev/input (HID) or /dev/bus/usb (raw)."})
}

/// A device's `state` from its row in the daemon's list: "waiting" when it
/// left and Kiri attaches it again when it comes back.
#[cfg(target_os = "macos")]
fn state_of(inside: Option<&serde_json::Value>) -> &'static str {
    match inside {
        Some(a) if a["Waiting"] == true => "waiting",
        Some(_) => "kiri",
        None => "host",
    }
}

/// The daemon's `Nodes` as the page's `paths`: `[{path, what}]`.
#[cfg(target_os = "macos")]
fn paths(a: &serde_json::Value) -> Vec<serde_json::Value> {
    a["Nodes"].as_array().into_iter().flatten().map(|n| serde_json::json!({"path": n["Path"], "what": n["What"]})).collect()
}

#[cfg(target_os = "macos")]
pub fn ui_attach(key: &str) -> io::Result<String> {
    let d = mac::find(key)?;
    attach_device(&d)?;
    Ok(format!("{} is in Kiri", d.name))
}

#[cfg(target_os = "macos")]
pub fn ui_detach(key: &str) -> io::Result<String> {
    detach_location(key)?;
    Ok("Given back to the Mac".into())
}

/// Linux: the host's devices, which containers already see.
#[cfg(not(target_os = "macos"))]
pub fn ui_list() -> serde_json::Value {
    let rows: Vec<serde_json::Value> = sysfs_devices(std::path::Path::new("/sys/bus/usb/devices"))
        .into_iter()
        .map(|d| serde_json::json!({"key": d.node, "name": d.name, "id": d.id, "speed": d.speed, "state": "shared", "kind": d.kind, "path": d.tty.unwrap_or(d.node), "maker": d.maker, "serial": d.serial, "bus": d.bus, "dev": d.dev, "paths": d.paths}))
        .collect();
    serde_json::json!({"type": "usb", "devices": rows, "can": "none",
        "note": "Containers run on this computer's own kernel, so they see its USB devices as they are: docker run --device with the path shown."})
}

/// The engine's `GET /kiri/usb/nodes` on Linux and in WSL: each device's
/// id, serial number and nodes, so the Windows window can say where a
/// device it attached to WSL shows up.
#[cfg(not(target_os = "macos"))]
pub fn host_nodes() -> serde_json::Value {
    serde_json::Value::Array(
        sysfs_devices(std::path::Path::new("/sys/bus/usb/devices"))
            .into_iter()
            .map(|d| serde_json::json!({"id": d.id, "serial": d.serial, "busid": d.busid, "paths": d.paths}))
            .collect(),
    )
}

#[cfg(not(target_os = "macos"))]
pub fn ui_attach(_: &str) -> io::Result<String> {
    attach("").map(|_| String::new())
}

#[cfg(not(target_os = "macos"))]
pub fn ui_detach(_: &str) -> io::Result<String> {
    attach("").map(|_| String::new())
}

#[cfg(not(target_os = "macos"))]
struct SysDevice {
    node: String,
    id: String,
    name: String,
    speed: String,
    tty: Option<String>,
    kind: &'static str,
    maker: String,
    serial: String,
    bus: u32,
    dev: u32,
    /// Its name in sysfs (`1-2`).
    busid: String,
    /// `[{path, what}]`, from `sysfs::nodes`.
    paths: Vec<serde_json::Value>,
}

/// The USB devices under a sysfs bus directory, hubs left out.
#[cfg(not(target_os = "macos"))]
fn sysfs_devices(dir: &std::path::Path) -> Vec<SysDevice> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let read = |f: &str| std::fs::read_to_string(p.join(f)).map(|s| s.trim().to_owned()).unwrap_or_default();
        let (vid, pid) = (read("idVendor"), read("idProduct"));
        if vid.is_empty() || read("bDeviceClass") == "09" {
            continue;
        }
        let maker = read("manufacturer");
        let name = [maker.clone(), read("product")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
        let speed = match read("speed").as_str() {
            "1.5" => "low",
            "12" => "full",
            "480" => "high",
            "5000" => "super",
            "10000" | "20000" => "super+",
            _ => "",
        };
        let hex = |v: String| u8::from_str_radix(&v, 16).unwrap_or(0);
        let mut ifs = Vec::new();
        for i in std::fs::read_dir(&p).into_iter().flatten().flatten() {
            let ip = i.path();
            let r = |f: &str| std::fs::read_to_string(ip.join(f)).map(|s| s.trim().to_owned()).unwrap_or_default();
            if ip.join("bInterfaceClass").exists() {
                ifs.push((hex(r("bInterfaceNumber")), hex(r("bInterfaceClass")), hex(r("bInterfaceSubClass")), hex(r("bInterfaceProtocol"))));
            }
        }
        let kind = kind_of(hex(read("bDeviceClass")), &ifs, u16::from_str_radix(&vid, 16).unwrap_or(0), &name);
        out.push(SysDevice {
            kind,
            node: format!("/dev/bus/usb/{:0>3}/{:0>3}", read("busnum"), read("devnum")),
            id: format!("{vid}:{pid}"),
            name: if name.is_empty() { "USB device".into() } else { name },
            speed: speed.into(),
            tty: tty_of(&p),
            serial: read("serial"),
            bus: read("busnum").parse().unwrap_or(0),
            dev: read("devnum").parse().unwrap_or(0),
            busid: e.file_name().to_string_lossy().into_owned(),
            paths: sysfs::nodes(&p).into_iter().map(|n| serde_json::json!({"path": n.path, "what": n.what})).collect(),
            maker,
        });
    }
    out.sort_by(|a, b| a.node.cmp(&b.node));
    out
}

/// A serial device's tty (/dev/ttyUSB0, /dev/ttyACM0), from its interfaces.
#[cfg(not(target_os = "macos"))]
fn tty_of(dev: &std::path::Path) -> Option<String> {
    for iface in std::fs::read_dir(dev).ok()?.flatten() {
        let ip = iface.path();
        // cdc-acm: <iface>/tty/ttyACM0; usb-serial: <iface>/ttyUSB0.
        let tty_dir = std::fs::read_dir(ip.join("tty")).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()).map(|e| e.file_name());
        let direct = std::fs::read_dir(&ip).ok().and_then(|d| d.flatten().map(|e| e.file_name()).find(|n| n.to_string_lossy().starts_with("ttyUSB")));
        if let Some(n) = tty_dir.or(direct) {
            return Some(format!("/dev/{}", n.to_string_lossy()));
        }
    }
    None
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn sysfs_lists_devices_not_hubs_with_their_tty() {
        let d = std::env::temp_dir().join(format!("kiri-usb-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let dev = |name: &str, files: &[(&str, &str)]| {
            let p = d.join(name);
            std::fs::create_dir_all(&p).unwrap();
            for (f, v) in files {
                std::fs::write(p.join(f), format!("{v}\n")).unwrap();
            }
            p
        };
        dev("usb1", &[("idVendor", "1d6b"), ("idProduct", "0002"), ("bDeviceClass", "09"), ("busnum", "1"), ("devnum", "1")]);
        let s = dev("1-1", &[("idVendor", "10c4"), ("idProduct", "ea60"), ("bDeviceClass", "00"), ("busnum", "1"), ("devnum", "3"), ("product", "CP2102"), ("speed", "12"), ("serial", "0001")]);
        std::fs::create_dir_all(s.join("1-1:1.0/ttyUSB0")).unwrap();
        dev("1-1:1.0", &[]);
        let got = sysfs_devices(&d);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].node, "/dev/bus/usb/001/003");
        assert_eq!(got[0].id, "10c4:ea60");
        assert_eq!(got[0].speed, "full");
        assert_eq!(got[0].tty.as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!((got[0].bus, got[0].dev, got[0].serial.as_str()), (1, 3, "0001"));
        assert_eq!(got[0].busid, "1-1");
        let paths: Vec<&str> = got[0].paths.iter().filter_map(|p| p["path"].as_str()).collect();
        assert_eq!(paths, ["/dev/ttyUSB0", "/dev/bus/usb/001/003"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
