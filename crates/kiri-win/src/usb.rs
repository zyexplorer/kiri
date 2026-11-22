//! The window's USB view on Windows: WSL 2, where Kiri's containers run,
//! gets USB devices through usbipd-win (Microsoft's recommended tool). A
//! device is shared once (`usbipd bind`, which Windows asks an
//! administrator to approve) and then attached to WSL (`usbipd attach
//! --wsl`), where it shows up like on any Linux machine. The page is the
//! same as on a Mac (crates/kiri/src/ui/index.html, renderUsb).

use std::io;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};

use crate::engine::{emit, notice};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn usbipd_path() -> Option<PathBuf> {
    let installed = std::env::var_os("ProgramFiles").map(|p| PathBuf::from(p).join("usbipd-win").join("usbipd.exe"));
    if let Some(p) = installed.filter(|p| p.is_file()) {
        return Some(p);
    }
    // On PATH, from another install.
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join("usbipd.exe")).find(|p| p.is_file()))
}

fn run(args: &[&str]) -> io::Result<String> {
    let exe = usbipd_path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "usbipd-win is not installed"))?;
    let mut c = Command::new(exe);
    c.args(args);
    std::os::windows::process::CommandExt::creation_flags(&mut c, CREATE_NO_WINDOW);
    let out = c.output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let text = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout).into_owned() } else { err.into_owned() };
        // usbipd says "usbipd: error: …".
        return Err(io::Error::other(text.trim().trim_start_matches("usbipd: ").trim_start_matches("error: ").to_owned()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// When this app attached each device (bus id → Unix seconds), for the
/// page's Attached column. usbipd doesn't keep it: one attached before the
/// app started shows no time.
static ATTACHED: std::sync::Mutex<Vec<(String, i64)>> = std::sync::Mutex::new(Vec::new());

fn attached_at(bus: &str) -> Option<i64> {
    ATTACHED.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(b, _)| b == bus).map(|(_, t)| *t)
}

fn set_attached(bus: &str, on: bool) {
    let mut a = ATTACHED.lock().unwrap_or_else(|e| e.into_inner());
    a.retain(|(b, _)| b != bus);
    if on {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        a.push((bus.to_owned(), now));
    }
}

/// The page's rows from `usbipd state`'s JSON.
fn rows(state: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for d in state["Devices"].as_array().into_iter().flatten() {
        // Remembered but not plugged in: nothing to attach.
        let Some(bus) = d["BusId"].as_str().filter(|b| !b.is_empty()) else { continue };
        let instance = d["InstanceId"].as_str().unwrap_or("").to_ascii_uppercase();
        let hex = |key: &str| instance.split(['\\', '&']).find_map(|p| p.strip_prefix(key)).map(|v| v.to_ascii_lowercase());
        let id = match (hex("VID_"), hex("PID_")) {
            (Some(v), Some(p)) => format!("{v}:{p}"),
            _ => String::new(),
        };
        let inside = !d["ClientIPAddress"].is_null();
        // USB\VID_…&PID_…\<serial>: the last part is the device's own serial
        // number, unless Windows made one up (it has an `&` in it then).
        let serial = d["InstanceId"].as_str().and_then(|i| i.rsplit('\\').next()).filter(|s| !s.contains('&') && !s.starts_with("VID_")).unwrap_or("");
        out.push(json!({
            "key": bus, "name": d["Description"].as_str().unwrap_or("USB device"), "id": id, "speed": "",
            "state": if inside { "kiri" } else { "host" }, "path": "", "shared": !d["PersistedGuid"].is_null(),
            "serial": serial, "busid": bus, "since": if inside { attached_at(bus) } else { None },
        }));
    }
    out
}

/// Where each device in WSL shows up there (`paths`), from the engine's
/// `GET /kiri/usb/nodes`. Bus ids differ between Windows and WSL, so a
/// device is found by vendor:product and serial number, or by
/// vendor:product alone when only one device in WSL has it.
fn with_paths(mut rows: Vec<Value>) -> Vec<Value> {
    if !rows.iter().any(|r| r["state"] == "kiri") {
        return rows;
    }
    let wsl = crate::client::get_background("/kiri/usb/nodes").ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    for r in rows.iter_mut().filter(|r| r["state"] == "kiri") {
        if let Some(paths) = paths_for(r, &wsl) {
            r["paths"] = paths;
        }
    }
    rows
}

fn paths_for(row: &Value, wsl: &[Value]) -> Option<Value> {
    let id = row["id"].as_str().filter(|i| !i.is_empty())?;
    let same: Vec<&Value> = wsl.iter().filter(|d| d["id"] == id).collect();
    let serial = row["serial"].as_str().unwrap_or("");
    let found = match same.as_slice() {
        [one] if serial.is_empty() || one["serial"] == serial => Some(*one),
        _ if !serial.is_empty() => same.iter().find(|d| d["serial"] == serial).copied(),
        _ => None,
    };
    found.map(|d| d["paths"].clone())
}

pub fn list() -> Value {
    if usbipd_path().is_none() {
        return json!({"type": "usb", "devices": [], "missing": true});
    }
    match run(&["state"]).and_then(|s| serde_json::from_str::<Value>(&s).map_err(io::Error::other)) {
        Ok(state) => json!({"type": "usb", "devices": with_paths(rows(&state)), "can": "attach",
            "note": crate::engine::tr("A device in Kiri is gone from Windows until you detach it. In a container: --device /dev/ttyUSB0 (serial), /dev/input (HID) or /dev/bus/usb (raw).")}),
        Err(e) => json!({"type": "usb", "devices": [], "error": e.to_string()}),
    }
}

fn valid(bus: &str) -> bool {
    bus.len() <= 16 && bus.split('-').count() == 2 && bus.split('-').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn attach(bus: &str) -> io::Result<String> {
    if !valid(bus) {
        return Err(io::Error::other(format!("bad bus id {bus:?}")));
    }
    let state: Value = serde_json::from_str(&run(&["state"])?).map_err(io::Error::other)?;
    let row = rows(&state).into_iter().find(|r| r["key"] == bus).ok_or_else(|| io::Error::other("that device is gone"))?;
    let name = row["name"].as_str().unwrap_or("The device").to_owned();
    // Sharing a device is an administrator's call, once per device.
    if row["shared"] != true {
        let exe = usbipd_path().ok_or_else(|| io::Error::other("usbipd-win is not installed"))?;
        match crate::win::elevated(&exe, &["bind", "--busid", bus]) {
            Ok(0) => {}
            Ok(code) => return Err(io::Error::other(format!("usbipd could not share {name} (exit {code})"))),
            Err(e) if e.raw_os_error() == Some(1223) => return Err(io::Error::other("cancelled")),
            Err(e) => return Err(e),
        }
    }
    run(&["attach", "--wsl", "--busid", bus]).map_err(|e| io::Error::other(format!("{name} was not attached: {e}")))?;
    set_attached(bus, true);
    Ok(crate::engine::trf("{name} is in Kiri", &[("name", &name)]))
}

fn detach(bus: &str) -> io::Result<String> {
    if !valid(bus) {
        return Err(io::Error::other(format!("bad bus id {bus:?}")));
    }
    run(&["detach", "--busid", bus])?;
    set_attached(bus, false);
    Ok(crate::engine::tr("Given back to Windows"))
}

/// winget installs it; its installer asks for approval itself.
fn install() -> io::Result<()> {
    let mut c = crate::win::tool("winget.exe");
    c.args(["install", "--exact", "--id", "dorssel.usbipd-win", "--silent", "--accept-package-agreements", "--accept-source-agreements"]);
    std::os::windows::process::CommandExt::creation_flags(&mut c, CREATE_NO_WINDOW);
    let out = c.output().map_err(|e| io::Error::other(format!("winget did not start ({e}): install usbipd-win from github.com/dorssel/usbipd-win")))?;
    if !out.status.success() && usbipd_path().is_none() {
        let text = String::from_utf8_lossy(&out.stdout);
        let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_owned();
        return Err(io::Error::other(format!("usbipd-win was not installed{}", if last.is_empty() { String::new() } else { format!(": {last}") })));
    }
    Ok(())
}

/// The page's USB commands.
pub fn handle(cmd: &'static str, key: String) {
    std::thread::spawn(move || {
        match cmd {
            "usbattach" | "usbdetach" => {
                let r = if cmd == "usbattach" { attach(&key) } else { detach(&key) };
                emit(match r {
                    Ok(text) => json!({"type": "usbdone", "key": key, "text": text}),
                    Err(e) if e.to_string() == "cancelled" => json!({"type": "usbdone", "key": key}),
                    Err(e) => json!({"type": "usbdone", "key": key, "error": e.to_string()}),
                }
                .to_string());
            }
            "usbinstall" => match install() {
                Ok(()) => emit(notice("ok", &crate::engine::tr("usbipd-win is installed"))),
                Err(e) => emit(notice("error", &e.to_string())),
            },
            _ => {}
        }
        emit(list().to_string());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_from_usbipd_state() {
        let state: Value = serde_json::from_str(r#"{"Devices":[
            {"BusId":"1-3","ClientIPAddress":null,"Description":"CP210x USB to UART Bridge (COM3)","InstanceId":"USB\\VID_10C4&PID_EA60\\0001","IsForced":false,"PersistedGuid":null,"StubInstanceId":null},
            {"BusId":"2-1","ClientIPAddress":"172.20.0.2","Description":"USB Input Device","InstanceId":"USB\\VID_046D&PID_C077\\5&2A","IsForced":false,"PersistedGuid":"6a1c1d1e-0000-0000-0000-000000000000","StubInstanceId":"x"},
            {"BusId":null,"ClientIPAddress":null,"Description":"Unplugged","InstanceId":"USB\\VID_1111&PID_2222\\9","PersistedGuid":"7b"}]}"#).unwrap();
        let r = rows(&state);
        assert_eq!(r.len(), 2);
        assert_eq!((r[0]["key"].as_str(), r[0]["id"].as_str(), r[0]["state"].as_str(), r[0]["shared"].as_bool()), (Some("1-3"), Some("10c4:ea60"), Some("host"), Some(false)));
        assert_eq!((r[1]["id"].as_str(), r[1]["state"].as_str(), r[1]["shared"].as_bool()), (Some("046d:c077"), Some("kiri"), Some(true)));
        // The serial number when the device has one; none when Windows made the id up.
        assert_eq!((r[0]["serial"].as_str(), r[1]["serial"].as_str(), r[0]["busid"].as_str()), (Some("0001"), Some(""), Some("1-3")));
    }

    #[test]
    fn paths_come_from_the_device_in_wsl() {
        let wsl = [
            json!({"id": "10c4:ea60", "serial": "0001", "paths": [{"path": "/dev/ttyUSB0", "what": "serial port"}]}),
            json!({"id": "10c4:ea60", "serial": "0002", "paths": [{"path": "/dev/ttyUSB1", "what": "serial port"}]}),
            json!({"id": "046d:c077", "serial": "", "paths": [{"path": "/dev/input/event3", "what": "input events"}]}),
        ];
        let p = |id: &str, serial: &str| paths_for(&json!({"id": id, "serial": serial}), &wsl).map(|v| v[0]["path"].clone());
        assert_eq!(p("10c4:ea60", "0002"), Some(json!("/dev/ttyUSB1")));
        assert_eq!(p("046d:c077", ""), Some(json!("/dev/input/event3")));
        // Two alike with no serial number to tell them apart: none rather than the wrong one.
        assert_eq!(p("10c4:ea60", ""), None);
        assert_eq!(p("1111:2222", ""), None);
    }

    #[test]
    fn bus_ids_are_checked() {
        assert!(valid("1-3") && valid("12-10"));
        assert!(!valid("") && !valid("1-3 & calc") && !valid("--busid"));
    }
}
