//! First run on Windows: from a PC with no WSL to `docker run` working, in
//! the window (setup.html; the protocol is in setup-protocol.md). Steps:
//! WSL (downloaded with progress, installed after one administrator
//! approval, then a restart), a Linux for Kiri (Kiri Linux, from the
//! bundle; or Ubuntu, downloaded), the engine, the Docker tools, ready.
//! Progress lives in %LOCALAPPDATA%\Kiri\setup.json, so a restart resumes.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::app::Handle;

/// A file setup downloads: what it is, from where, and what it must hash to.
#[derive(Debug)]
struct Pin {
    name: &'static str,
    url: &'static str,
    sha256: &'static str,
    size: u64,
    file: &'static str,
}

/// GitHub's asset digest for the release. WSL ships one MSI per
/// architecture; this build's own (see crate::win::arch_mismatch).
#[cfg(not(target_arch = "aarch64"))]
const WSL_MSI: Pin = Pin {
    name: "WSL 3.0.3",
    url: "https://github.com/microsoft/WSL/releases/download/3.0.3/wsl.3.0.3.0.x64.msi",
    sha256: "ae977286974255bd26bf925f00f40f5692c6b2084155cbc0563c09aea091f211",
    size: 367_575_040,
    file: "wsl.3.0.3.0.x64.msi",
};
#[cfg(target_arch = "aarch64")]
const WSL_MSI: Pin = Pin {
    name: "WSL 3.0.3",
    url: "https://github.com/microsoft/WSL/releases/download/3.0.3/wsl.3.0.3.0.arm64.msi",
    sha256: "2937c7ce840bb35ec6e97f052189bdfff27f8c3463bfdd231df9a958212787a3",
    size: 341_086_208,
    file: "wsl.3.0.3.0.arm64.msi",
};

/// Microsoft's WSL distribution list gives the hash (DistributionInfo.json
/// in microsoft/WSL: Amd64Url and Arm64Url).
#[cfg(not(target_arch = "aarch64"))]
const UBUNTU: Pin = Pin {
    name: "Ubuntu 26.04",
    url: "https://releases.ubuntu.com/26.04.1/ubuntu-26.04.1-wsl-amd64.wsl",
    sha256: "48d56724b5c8e60f24893e83e73bbb58c60b3ca22fba3da977075420acd54104",
    size: 418_495_746,
    file: "ubuntu-26.04.1-wsl-amd64.wsl",
};
#[cfg(target_arch = "aarch64")]
const UBUNTU: Pin = Pin {
    name: "Ubuntu 26.04",
    url: "https://cdimages.ubuntu.com/releases/26.04.1/release/ubuntu-26.04.1-wsl-arm64.wsl",
    sha256: "c7d893d7ccb291009e6096c5a946937337b6fd4c426f11b6541f6cd2bf894f7d",
    size: 412_840_386,
    file: "ubuntu-26.04.1-wsl-arm64.wsl",
};
const UBUNTU_DISTRO: &str = "Ubuntu-26.04";
pub(crate) const KIRI_DISTRO: &str = "kiri";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Saved {
    done: bool,
    /// "kiri" or "ubuntu".
    linux: Option<String>,
    distro: Option<String>,
    /// Unix seconds of the WSL install that needs a restart.
    restart_after: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Task {
    label: String,
    state: &'static str,
    detail: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct State {
    step: &'static str,
    linux: Option<String>,
    sizes: Value,
    /// Direct download URLs for the pinned WSL MSI and Ubuntu `.wsl`
    /// (this build's architecture), so the page can open them in a browser.
    urls: Value,
    existing_ubuntu: bool,
    other_docker: bool,
    /// The engine besides Kiri, by name (crate::buildwith), for the build question.
    other_engine: Option<&'static str>,
    make_default: bool,
    /// Builds on Kiri: the answer to "Keep building with Docker / Build
    /// with Kiri", asked only with another engine here.
    build_kiri: bool,
    autostart: bool,
    progress: Option<Value>,
    tasks: Vec<Task>,
    log: Vec<String>,
    error: Option<String>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static BUSY: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

fn dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Kiri")).unwrap_or_else(|| PathBuf::from("."))
}

fn saved() -> Saved {
    std::fs::read(dir().join("setup.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The WSL distro setup put the engine in, if it has.
pub(crate) fn saved_distro() -> Option<String> {
    saved().distro
}

fn save(s: &Saved) {
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(dir().join("setup.json"), serde_json::to_vec_pretty(s).unwrap_or_default());
}

/// The distro setup put Kiri in, for the bridge.
pub fn distro() -> Option<String> {
    saved().distro
}

/// The SHA-256 of each bundle file, embedded at build time by
/// `cargo xtask windows` (`name=hash;…`). A dev build has none.
const BUNDLE_SHA256: Option<&str> = option_env!("KIRI_BUNDLE_SHA256");

/// A file from the bundle, checked against its embedded SHA-256 so a file
/// swapped in the unzipped folder (a drive-by download, another process)
/// is refused rather than imported into WSL or installed on PATH. With no
/// embedded hashes (a dev build) the file is used as is.
pub(crate) fn verified(rel: &str) -> io::Result<PathBuf> {
    let path = bundle().join(rel);
    let Some(want) = BUNDLE_SHA256.and_then(|m| m.split(';').find_map(|e| e.strip_prefix(rel).and_then(|r| r.strip_prefix('=')))) else {
        // A debug build (plain `cargo build`) has no manifest and uses the
        // file as is. A release must have one: otherwise building kiri-win on
        // its own would quietly skip every bundle check.
        if cfg!(debug_assertions) {
            return Ok(path);
        }
        return Err(io::Error::other(format!("this build carries no checksum for {rel}; build the bundle with `cargo xtask windows`")));
    };
    if !path.is_file() {
        return Err(io::Error::other(format!("{rel} is missing from Kiri's folder")));
    }
    let got = sha256(&path)?;
    if got != want {
        return Err(io::Error::other(format!("{rel} in Kiri's folder does not match this build (SHA-256 {got}); download Kiri again")));
    }
    Ok(path)
}

/// Where kiri-win was started from: the unzipped bundle on first run. Debug
/// builds run from target/debug use the bundle made by `cargo xtask windows-dev`.
fn bundle() -> PathBuf {
    let local = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_default();
    #[cfg(debug_assertions)]
    if !local.join("kiri-linux.tar.gz").is_file() {
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../target/kiri-windows-{}", std::env::consts::ARCH));
        if bundled.join("kiri-linux.tar.gz").is_file() {
            return bundled;
        }
    }
    local
}

fn quiet(program: &str) -> Command {
    let mut c = crate::win::tool(program);
    std::os::windows::process::CommandExt::creation_flags(&mut c, CREATE_NO_WINDOW);
    c.stdin(Stdio::null());
    c
}

/// WSL's own output is UTF-16.
fn wsl_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.iter().skip(1).step_by(2).take(8).all(|&b| b == 0) {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units).replace('\0', "")
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn wsl_installed() -> bool {
    quiet("wsl.exe").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

pub(crate) fn distros() -> Vec<String> {
    quiet("wsl.exe").args(["-l", "-q"]).output().map(|o| wsl_text(&o.stdout).lines().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect()).unwrap_or_default()
}

fn existing_ubuntu() -> Option<String> {
    distros().into_iter().find(|d| d.starts_with("Ubuntu"))
}

/// Another Docker on this PC: Docker Desktop, or a docker CLI on PATH that is not Kiri's.
fn other_docker() -> bool {
    let desktop = std::env::var_os("ProgramFiles").is_some_and(|p| Path::new(&p).join(r"Docker\Docker\Docker Desktop.exe").is_file());
    let ours = dir().join("bin");
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d != ours && d.join("docker.exe").is_file()));
    desktop || on_path
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Whether Windows has restarted since `at`.
fn restarted_since(at: u64) -> bool {
    // SAFETY: reads the system's uptime.
    let uptime = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() } / 1000;
    now_secs().saturating_sub(uptime) > at
}

/// Whether the window should open on setup rather than Kiri.
pub fn needed() -> bool {
    let s = saved();
    !s.done || s.distro.as_deref().is_none_or(|d| !distros().iter().any(|x| x == d))
}

/// The step to show now, from what is on this PC.
fn current_step(s: &Saved) -> &'static str {
    if !wsl_installed() {
        return "welcome";
    }
    if let Some(at) = s.restart_after {
        if !restarted_since(at) {
            return "restart";
        }
    }
    match &s.distro {
        Some(d) if distros().iter().any(|x| x == d) => {
            if crate::client::alive() {
                "tools"
            } else {
                "engine"
            }
        }
        _ => "choose",
    }
}

fn fresh_state(step: &'static str, s: &Saved) -> State {
    let other = other_docker();
    State {
        step,
        linux: s.linux.clone(),
        sizes: json!({"wsl": WSL_MSI.size, "ubuntu": UBUNTU.size, "kiri": std::fs::metadata(bundle().join("kiri-linux.tar.gz")).map_or(0, |m| m.len())}),
        urls: json!({"wsl": WSL_MSI.url, "ubuntu": UBUNTU.url}),
        existing_ubuntu: existing_ubuntu().is_some(),
        other_docker: other,
        other_engine: crate::buildwith::other_engine(),
        make_default: !other,
        build_kiri: !other,
        autostart: true,
        progress: None,
        tasks: Vec::new(),
        log: Vec::new(),
        error: None,
    }
}

fn update(f: impl FnOnce(&mut State)) {
    let json = {
        let mut st = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(s) = st.as_mut() else { return };
        f(s);
        let mut v = serde_json::to_value(&*s).unwrap_or(Value::Null);
        v["type"] = json!("setup");
        v.to_string()
    };
    Handle.eval(format!("window.kiri && kiri.on({json})"));
}

fn tasks(labels: &[&str]) -> Vec<Task> {
    labels.iter().map(|l| Task { label: (*l).to_owned(), state: "todo", detail: String::new() }).collect()
}

fn task(i: usize, state: &'static str, detail: &str) {
    update(|s| {
        if let Some(t) = s.tasks.get_mut(i) {
            t.state = state;
            t.detail = detail.to_owned();
        }
        // An unsized step's progress follows its tasks.
        let n = s.tasks.len().max(1);
        let done = s.tasks.iter().filter(|t| t.state == "done").count();
        if s.progress.as_ref().is_none_or(|p| p["total"].is_null()) {
            let label = s.tasks.iter().find(|t| t.state == "active").map_or(String::new(), |t| t.label.clone());
            s.progress = Some(json!({"pct": done * 100 / n, "done": null, "total": null, "rate": null, "eta": null, "label": label}));
        }
    });
}

fn log_line(line: &str) {
    let line = line.trim().to_owned();
    if !line.is_empty() {
        update(|s| {
            s.log.push(line);
            let n = s.log.len();
            if n > 8 {
                s.log.drain(..n - 8);
            }
        });
    }
}

/// Runs a step's work on its own thread; an error stays on its screen with "Try again".
fn run(step: &'static str, work: impl FnOnce() -> io::Result<()> + Send + 'static) {
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    CANCEL.store(false, Ordering::SeqCst);
    update(|s| {
        s.step = step;
        s.error = None;
        s.progress = None;
        s.log.clear();
    });
    std::thread::spawn(move || {
        let r = work();
        BUSY.store(false, Ordering::SeqCst);
        if let Err(e) = r {
            crate::serve::log(&format!("setup {step}: {e}"));
            let cancelled = CANCEL.load(Ordering::SeqCst);
            update(|s| {
                if cancelled {
                    s.step = if step == "wsl" { "welcome" } else if step == "linux" { "choose" } else { step };
                    s.progress = None;
                    s.tasks.clear();
                } else {
                    s.error = Some(e.to_string());
                    for t in &mut s.tasks {
                        if t.state == "active" {
                            t.state = "error";
                        }
                    }
                }
            });
        }
    });
}

/// Bytes in MB as the page shows them (1 MB = 1,000,000 bytes, rounded),
/// so a task's "4 / 368 MB" agrees with the bar's "4 of 368 MB".
fn mb(bytes: u64) -> u64 {
    (bytes + 500_000) / 1_000_000
}

/// Copies a person-chosen file into %LOCALAPPDATA%\Kiri\downloads as
/// `pin.file` after SHA-256 matches. `Ok(false)` means the open dialog was
/// cancelled (leave the current error alone).
fn stage_local(pin: &Pin, title: &str, filter_label: &str, filter_pattern: &str) -> io::Result<bool> {
    if let Some(e) = crate::win::arch_mismatch() {
        return Err(io::Error::other(e));
    }
    let Some(src) = crate::app::pick_file(title, filter_label, filter_pattern) else {
        return Ok(false);
    };
    if !src.is_file() {
        return Err(io::Error::other(format!("could not read {}", src.display())));
    }
    let got = sha256(&src)?;
    if got != pin.sha256 {
        return Err(io::Error::other(format!("{} did not match its SHA-256 (got {got})", pin.name)));
    }
    let dl = dir().join("downloads");
    std::fs::create_dir_all(&dl)?;
    let dest = dl.join(pin.file);
    if src != dest {
        let part = dl.join(format!("{}.part", pin.file));
        let _ = std::fs::remove_file(&part);
        std::fs::copy(&src, &part).map_err(|e| io::Error::other(format!("could not copy {}: {e}", src.display())))?;
        let got = sha256(&part)?;
        if got != pin.sha256 {
            let _ = std::fs::remove_file(&part);
            return Err(io::Error::other(format!("{} did not match its SHA-256 after copy (got {got})", pin.name)));
        }
        std::fs::rename(&part, &dest)?;
    }
    Ok(true)
}

/// Downloads `pin` into %LOCALAPPDATA%\Kiri\downloads with curl.exe (part
/// of Windows), showing bytes, speed and time left, then checks its SHA-256.
fn download(pin: &Pin, task_index: usize) -> io::Result<PathBuf> {
    let dl = dir().join("downloads");
    std::fs::create_dir_all(&dl)?;
    let dest = dl.join(pin.file);
    if dest.is_file() && sha256(&dest)? == pin.sha256 {
        task(task_index, "done", &format!("{} MB · already here", mb(pin.size)));
        return Ok(dest);
    }
    let part = dl.join(format!("{}.part", pin.file));
    let _ = std::fs::remove_file(&part);
    let label = format!("Downloading {} from {}", pin.name, if pin.url.contains("github.com/microsoft") { "Microsoft" } else { "ubuntu.com" });
    task(task_index, "active", &format!("0 / {} MB", mb(pin.size)));
    let mut child = quiet("curl.exe").args(["-fsSL", "--retry", "3", "-o"]).arg(&part).arg(pin.url).spawn()?;
    let (start, mut last, mut rate) = (Instant::now(), (Instant::now(), 0u64), 0f64);
    loop {
        if CANCEL.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = std::fs::remove_file(&part);
            return Err(io::Error::other("cancelled"));
        }
        let finished = child.try_wait()?;
        let got = std::fs::metadata(&part).map_or(0, |m| m.len());
        let dt = last.0.elapsed().as_secs_f64();
        if dt >= 0.25 {
            let now = (got - last.1.min(got)) as f64 / dt;
            rate = if rate == 0.0 { now } else { rate * 0.7 + now * 0.3 };
            last = (Instant::now(), got);
        }
        let eta = if rate > 0.0 { ((pin.size.saturating_sub(got)) as f64 / rate).ceil() as u64 } else { 0 };
        update(|s| {
            s.progress = Some(json!({"pct": got * 100 / pin.size.max(1), "done": got, "total": pin.size, "rate": rate as u64, "eta": eta, "label": label}));
            if let Some(t) = s.tasks.get_mut(task_index) {
                t.detail = format!("{} / {} MB", mb(got), mb(pin.size));
            }
        });
        if let Some(status) = finished {
            if !status.success() {
                return Err(io::Error::other(format!("downloading {} failed: check the internet connection", pin.name)));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let got = sha256(&part)?;
    if got != pin.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(io::Error::other(format!("{} did not match its SHA-256 (got {got})", pin.name)));
    }
    std::fs::rename(&part, &dest)?;
    task(task_index, "done", &format!("{} MB in {} s", mb(pin.size), start.elapsed().as_secs()));
    Ok(dest)
}

pub(crate) fn sha256(path: &Path) -> io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Runs kiri-admin.ps1 (contrib/windows, installed beside this exe) with
/// `args` in an elevated PowerShell (Windows asks for approval) and returns
/// its exit code; a refusal is "cancelled". A script file checked against
/// this build's SHA-256, not script text in this binary run with
/// -EncodedCommand: an elevated, hidden PowerShell fed an encoded script is
/// what malware droppers do, and Defender quarantined kiri-win.exe 0.1.7 as
/// Trojan:Win32/Bearfoos.A!ml for carrying it.
fn elevated(args: &[&str]) -> io::Result<i32> {
    let script = verified("kiri-admin.ps1")?;
    match crate::win::elevated_script(&script, args) {
        // As before: anything that kept it from starting reads as declined.
        Err(e) => {
            crate::serve::log(&format!("setup: the elevated PowerShell did not start ({e})"));
            Err(io::Error::other("cancelled: Windows' approval was declined"))
        }
        Ok(1223) => Err(io::Error::other("cancelled: Windows' approval was declined")),
        Ok(c) => Ok(c as i32),
    }
}

/// Step 1: download WSL, then in one elevated run turn on Virtual Machine
/// Platform and install WSL. Exit 3010 from either means a restart.
fn install_wsl() -> io::Result<()> {
    update(|s| s.tasks = tasks(&["Download WSL", "Administrator approval", "Turn on Virtual Machine Platform", "Install WSL"]));
    // WSL_MSI is this build's architecture's; on another PC it would not install.
    if let Some(e) = crate::win::arch_mismatch() {
        return Err(io::Error::other(e));
    }
    let msi = download(&WSL_MSI, 0)?;
    update(|s| s.progress = Some(json!({"pct": null, "done": null, "total": null, "rate": null, "eta": null, "label": "Waiting for Windows' approval"})));
    task(1, "active", "Windows is asking");
    // The downloaded MSI lives in a user-writable folder, so a process at
    // the user's own integrity could swap it between its SHA-256 check above
    // and the elevated install. kiri-admin.ps1 -Wsl re-verifies it, as admin,
    // on a copy the user cannot touch, and requires it to be Microsoft-signed,
    // before running it; its exit codes are read below.
    let msi = msi.to_string_lossy();
    let code = elevated(&["-Wsl", "-Msi", &msi, "-Sha256", WSL_MSI.sha256])?;
    task(1, "done", "approved");
    match code {
        22 => return Err(io::Error::other("the downloaded WSL installer changed on disk; try again")),
        23 => return Err(io::Error::other("the WSL installer is not validly signed by Microsoft")),
        20 => {
            task(2, "error", "");
            return Err(io::Error::other("Windows could not turn on Virtual Machine Platform (is virtualization on in the BIOS?)"));
        }
        21 => {
            task(2, "done", "");
            task(3, "error", "");
            return Err(io::Error::other("the WSL installer failed"));
        }
        _ => {}
    }
    task(2, "done", "");
    task(3, "done", "");
    if code == 3010 || !wsl_installed() {
        let mut s = saved();
        s.restart_after = Some(now_secs());
        save(&s);
        // After the restart, Kiri opens once by itself and carries on.
        if let Ok(exe) = std::env::current_exe() {
            let registered = quiet("reg.exe")
                .args(["add", r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce", "/v", "KiriSetup", "/t", "REG_SZ", "/d", &format!("\"{}\"", exe.display()), "/f"])
                .status()
                .is_ok_and(|s| s.success());
                if !registered {
                    return Err(io::Error::other("Windows would not register Kiri to carry on after the restart"));
                }
        }
        update(|s| {
            s.step = "restart";
            s.progress = None;
        });
    } else {
        update(|s| {
            s.step = "choose";
            s.progress = None;
        });
    }
    Ok(())
}

/// Step 2: Kiri Linux from the bundle, or Ubuntu (already here, or downloaded).
fn install_linux(kind: &str) -> io::Result<()> {
    // The engine and Kiri Linux in the bundle are this build's architecture.
    if let Some(e) = crate::win::arch_mismatch() {
        return Err(io::Error::other(e));
    }
    let base = dir().join("wsl");
    std::fs::create_dir_all(&base)?;
    let distro = if kind == "kiri" {
        update(|s| s.tasks = tasks(&["Unpack Kiri Linux", "Add it to WSL", "Start it"]));
        task(0, "active", "");
        let tar = verified("kiri-linux.tar.gz")?;
        task(0, "done", &format!("{} MB", std::fs::metadata(&tar)?.len().div_ceil(1_000_000)));
        task(1, "active", "");
        if !distros().iter().any(|d| d == KIRI_DISTRO) {
            log_line(&format!("wsl --import {KIRI_DISTRO} {} kiri-linux.tar.gz --version 2", base.join(KIRI_DISTRO).display()));
            wsl_ok(quiet("wsl.exe").args(["--import", KIRI_DISTRO]).arg(base.join(KIRI_DISTRO)).arg(&tar).args(["--version", "2"]))?;
        }
        task(1, "done", "");
        task(2, "active", "");
        wsl_ok(quiet("wsl.exe").args(["-d", KIRI_DISTRO, "-u", "root", "--", "/usr/local/bin/kiri-start"]))?;
        task(2, "done", "");
        KIRI_DISTRO.to_owned()
    } else {
        let existing = existing_ubuntu();
        let distro = match existing {
            Some(d) => {
                update(|s| s.tasks = tasks(&["Use the Ubuntu already here", "Install Kiri's engine in it", "Start it"]));
                task(0, "done", &d);
                d
            }
            None => {
                update(|s| s.tasks = tasks(&["Download Ubuntu", "Unpack it into WSL", "Install Kiri's engine in it", "Start it"]));
                let file = download(&UBUNTU, 0)?;
                update(|s| s.progress = None);
                task(1, "active", "≈ 1.6 GB");
                // A real Ubuntu, in Start too; older WSL lacks --from-file.
                let installed = quiet("wsl.exe").args(["--install", "--from-file"]).arg(&file).args(["--name", UBUNTU_DISTRO, "--no-launch"]).status().is_ok_and(|s| s.success());
                if !installed {
                    wsl_ok(quiet("wsl.exe").args(["--import", UBUNTU_DISTRO]).arg(base.join(UBUNTU_DISTRO)).arg(&file).args(["--version", "2"]))?;
                }
                task(1, "done", "");
                update(|s| {
                    s.tasks.remove(0);
                });
                UBUNTU_DISTRO.to_owned()
            }
        };
        task(1, "active", "");
        // The engine binaries install.sh copies are checked too.
        verified("kiri")?;
        verified("kiri-init")?;
        let install = to_wsl(&distro, &verified("install.sh")?)?;
        wsl_ok(quiet("wsl.exe").args(["-d", &distro, "-u", "root", "--", "sh", &install]))?;
        task(1, "done", "");
        task(2, "done", "");
        distro
    };
    let mut s = saved();
    s.linux = Some(kind.to_owned());
    s.distro = Some(distro.clone());
    save(&s);
    crate::serve::set_distro(&distro);
    update(|st| st.linux = Some(kind.to_owned()));
    engine()
}

/// A Windows path as `distro` sees it.
pub(crate) fn to_wsl(distro: &str, path: &Path) -> io::Result<String> {
    let out = quiet("wsl.exe").args(["-d", distro, "--", "wslpath", "-a"]).arg(path.to_string_lossy().replace('\\', "/")).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn wsl_ok(cmd: &mut Command) -> io::Result<()> {
    let out = cmd.output()?;
    if out.status.success() {
        return Ok(());
    }
    let text = wsl_text(&out.stdout) + &wsl_text(&out.stderr);
    Err(io::Error::other(text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("WSL failed").to_owned()))
}

fn docker() -> PathBuf {
    let ours = dir().join("bin").join("docker.exe");
    if ours.is_file() { ours } else { bundle().join("bin").join("docker.exe") }
}

/// Ensures `~/.kiri/daemon.json` in the engine distro has Hub mirrors when
/// the file is absent (DaoCloud / 1ms.run / xuanyuan). Best-effort: a
/// failure only means the test pull may hit docker.io directly.
fn seed_hub_mirror() {
    let distro = saved().distro.unwrap_or_else(|| KIRI_DISTRO.to_owned());
    let script = r#"d=/var/lib/kiri-daemon/.kiri; mkdir -p "$d"; f="$d/daemon.json"; [ -f "$f" ] || printf '%s\n' '{"registry-mirrors":["https://docker.m.daocloud.io","https://docker.1ms.run","https://docker.xuanyuan.me"],"max-concurrent-downloads":4,"max-concurrent-uploads":5}' >"$f""#;
    let _ = quiet("wsl.exe").args(["-d", &distro, "-u", "root", "--", "sh", "-c", script]).status();
}

/// Step 3: the engine answers through the pipe and runs a container.
fn engine() -> io::Result<()> {
    update(|s| {
        s.step = "engine";
        s.progress = None;
        s.tasks = tasks(&["Start the engine", "Connect Windows to it", "Run a test container"]);
    });
    task(0, "active", "");
    let t0 = Instant::now();
    while !crate::client::alive() {
        if CANCEL.load(Ordering::SeqCst) {
            return Err(io::Error::other("cancelled"));
        }
        if t0.elapsed() > Duration::from_secs(60) {
            return Err(io::Error::other("Kiri's engine did not answer within a minute"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    task(0, "done", &format!("answered in {} ms", t0.elapsed().as_millis()));
    task(1, "done", r"\\.\pipe\kiri");
    task(2, "active", "pulling alpine");
    log_line("$ docker run --rm alpine echo hello");
    // Hub is often unreachable from mainland China; seed DaoCloud as a
    // registry-mirror when daemon.json is still missing (engine reloads it
    // on each pull). Does not overwrite Settings the person already saved.
    seed_hub_mirror();
    // Before the installed copy exists, this runs the bundle's docker.exe:
    // check it first, as the tools step does for the copy it installs.
    if !dir().join("bin").join("docker.exe").is_file() {
        verified("bin/docker.exe")?;
    }
    let out = quiet(&docker().to_string_lossy()).args(["--host", "npipe:////./pipe/kiri", "run", "--rm", "alpine", "echo", "hello"]).output()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    for l in text.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev() {
        log_line(l);
    }
    if !out.status.success() || !text.contains("hello") {
        return Err(io::Error::other("the test container did not run"));
    }
    task(2, "done", "hello");
    tools()
}

/// Step 4: the Docker tools (setup-tools.ps1), task by task.
fn tools() -> io::Result<()> {
    update(|s| {
        s.step = "tools";
        s.progress = None;
        s.tasks = tasks(&["docker 29.9.2", "Compose v5.6.1", "buildx v0.38.1", "docker build on Kiri", "Make Kiri the default"]);
    });
    task(0, "active", "");
    for f in ["bin/docker.exe", "bin/kiri.exe", "bin/cli-plugins/docker-compose.exe", "bin/cli-plugins/docker-buildx.exe"] {
        verified(f)?;
    }
    let make_default = STATE.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|s| s.make_default);
    // setup-tools.ps1 (contrib/windows), checked like the bundle's other
    // files and run from its file with its parameters as arguments. Not a
    // copy in this binary run through [scriptblock]::Create from Base64:
    // that is the textbook dropper, and Defender's heuristics flagged
    // kiri-win.exe 0.1.7 for it (Trojan:Win32/Bearfoos.A!ml).
    let script = verified("setup-tools.ps1")?;
    let mut cmd = quiet("powershell.exe");
    cmd.args(crate::win::PS_FILE).arg(&script).arg("-Bundle").arg(bundle());
    if make_default {
        cmd.arg("-Default");
    }
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no output"))?;
    // stderr is drained as it comes: docker writes progress there, and a
    // full pipe would stall the script. It is only a fallback for the error.
    let stderr = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = e.read_to_string(&mut s);
            s
        })
    });
    // The script reports its own failure as "ERROR <why>" on stdout, so
    // the message is the real one, not whatever docker said on stderr first.
    let mut error = None;
    for line in io::BufRead::lines(io::BufReader::new(stdout)).map_while(Result::ok) {
        let line = line.trim();
        if let Some(n) = line.strip_prefix("TASK ").and_then(|n| n.parse::<usize>().ok()) {
            let detail = if n == 4 && !make_default { "skipped" } else { "" };
            task(n, "done", detail);
            task(n + 1, "active", "");
        } else if let Some(why) = line.strip_prefix("ERROR ") {
            error = Some(why.trim().to_owned());
        }
    }
    let status = child.wait()?;
    let err = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
    if !status.success() {
        let why = error
            .filter(|e| !e.is_empty())
            .or_else(|| err.lines().rev().find(|l| !l.trim().is_empty() && !l.contains("Successfully")).map(|l| l.trim().to_owned()))
            .unwrap_or_else(|| "installing the Docker tools failed".to_owned());
        return Err(io::Error::other(why));
    }
    // From here on Kiri runs from %LOCALAPPDATA%\Kiri, not the download folder.
    if let Ok(exe) = std::env::current_exe() {
        let installed = dir().join("kiri-win.exe");
        if exe != installed {
            std::fs::copy(&exe, &installed).map_err(|e| io::Error::other(format!("could not install kiri-win.exe to {}: {e}", installed.display())))?;
        }
    }
    update(|s| {
        s.step = "ready";
        s.progress = None;
    });
    Ok(())
}

/// Ready → Open Kiri: remember setup is done, start at sign-in if asked.
fn finish() {
    let autostart = STATE.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_none_or(|s| s.autostart);
    // Which builder docker build uses: asked with another engine here, Kiri's without.
    let asked = STATE.lock().unwrap_or_else(PoisonError::into_inner).as_ref().filter(|s| s.other_docker).map(|s| s.build_kiri);
    let built = match asked {
        Some(kiri) => crate::buildwith::choose(if kiri { crate::buildwith::With::Kiri } else { crate::buildwith::With::Docker }),
        None => crate::buildwith::apply(),
    };
    if let Err(e) = built {
        crate::serve::log(&format!("docker build: could not set which builder it uses ({e})"));
    }
    let mut s = saved();
    s.done = true;
    s.restart_after = None;
    save(&s);
    let exe = dir().join("kiri-win.exe");
    let run_key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    if autostart {
        if exe.is_file() {
            let _ = quiet("reg.exe").args(["add", run_key, "/v", "Kiri", "/t", "REG_SZ", "/d", &format!("\"{}\" --background", exe.display()), "/f"]).status();
        } else {
            crate::serve::log("not set to start at sign-in: kiri-win.exe is not installed");
        }
    } else {
        let _ = quiet("reg.exe").args(["delete", run_key, "/v", "Kiri", "/f"]).status();
    }
    let _ = quiet("reg.exe").args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce", "/v", "KiriSetup", "/f"]).status();
    Handle.run(|ui| ui.navigate_main());
}

fn copy_to_clipboard(text: &str) {
    if let Ok(mut c) = quiet("clip.exe").stdin(Stdio::piped()).spawn() {
        if let Some(mut i) = c.stdin.take() {
            let _ = io::Write::write_all(&mut i, text.as_bytes());
        }
        let _ = c.wait();
    }
}

/// Whether a page message is setup's.
pub fn handles(msg: &str) -> bool {
    serde_json::from_str::<Value>(msg).ok().and_then(|v| v["cmd"].as_str().map(|c| c.starts_with("setup-") || c == "copy" || c == "open")).unwrap_or(false)
}

fn setup_open_url(url: &str) -> bool {
    url == "https://learn.microsoft.com/windows/wsl/about" || url == WSL_MSI.url || url == UBUNTU.url
}

pub fn on_message(msg: &str) {
    let Ok(v) = serde_json::from_str::<Value>(msg) else { return };
    match v["cmd"].as_str().unwrap_or("") {
        // This arrives on the window's thread. Working out the step asks
        // wsl.exe twice and pings the engine through the pipe, which on a
        // cold WSL can take long (and the ping has no deadline): off the
        // window's thread, so it keeps drawing and answering.
        "setup-ready" => {
            std::thread::spawn(|| {
                let s = saved();
                let step = current_step(&s);
                let fresh = fresh_state(step, &s);
                *STATE.lock().unwrap_or_else(PoisonError::into_inner) = Some(fresh);
                update(|_| {});
                // A restart finished WSL, or the engine is there already: carry on by itself.
                match step {
                    "engine" => run("engine", engine),
                    "tools" => run("tools", tools),
                    _ => {}
                }
            });
        }
        "setup-install-wsl" => run("wsl", install_wsl),
        "setup-restart" => {
            let _ = quiet("shutdown.exe").args(["/r", "/t", "5", "/c", "Kiri: finishing Windows Subsystem for Linux"]).status();
        }
        "setup-later" => Handle.run(crate::app::Ui::hide),
        "setup-choose" => {
            let kind = if v["value"] == "ubuntu" { "ubuntu" } else { "kiri" };
            update(|s| s.linux = Some(kind.to_owned()));
            run("linux", move || install_linux(kind));
        }
        "setup-switch" => {
            CANCEL.store(true, Ordering::SeqCst);
            std::thread::spawn(|| {
                while BUSY.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                update(|s| s.linux = Some("kiri".into()));
                run("linux", || install_linux("kiri"));
            });
        }
        "setup-cancel" => CANCEL.store(true, Ordering::SeqCst),
        "setup-retry" => {
            let step = STATE.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map_or("welcome", |s| s.step);
            let linux = STATE.lock().unwrap_or_else(PoisonError::into_inner).as_ref().and_then(|s| s.linux.clone()).unwrap_or_else(|| "kiri".into());
            match step {
                "wsl" => run("wsl", install_wsl),
                "linux" => run("linux", move || install_linux(&linux)),
                "engine" => run("engine", engine),
                "tools" => run("tools", tools),
                _ => update(|s| s.error = None),
            }
        }
        // Offline / flaky download: pick the pinned MSI or .wsl, verify it,
        // stage it where download() looks, then continue the step.
        "setup-use-local" => {
            if BUSY.load(Ordering::SeqCst) {
                return;
            }
            let (step, linux) = {
                let g = STATE.lock().unwrap_or_else(PoisonError::into_inner);
                let s = g.as_ref();
                (s.map_or("welcome", |s| s.step), s.and_then(|s| s.linux.clone()))
            };
            let which = match v["which"].as_str() {
                Some("ubuntu") => "ubuntu",
                Some("wsl") => "wsl",
                _ if step == "linux" && linux.as_deref() == Some("ubuntu") => "ubuntu",
                _ => "wsl",
            };
            std::thread::spawn(move || {
                let staged = if which == "ubuntu" {
                    stage_local(&UBUNTU, "Choose the Ubuntu WSL image", "Ubuntu WSL (*.wsl)", "*.wsl")
                } else {
                    stage_local(&WSL_MSI, "Choose the WSL installer", "WSL installer (*.msi)", "*.msi")
                };
                match staged {
                    Ok(false) => {}
                    Ok(true) if which == "ubuntu" => {
                        update(|s| s.linux = Some("ubuntu".into()));
                        run("linux", || install_linux("ubuntu"));
                    }
                    Ok(true) => run("wsl", install_wsl),
                    Err(e) => {
                        crate::serve::log(&format!("setup use-local: {e}"));
                        update(|s| s.error = Some(e.to_string()));
                    }
                }
            });
        }
        "setup-default" => update(|s| s.make_default = v["value"] == true),
        "setup-autostart" => update(|s| s.autostart = v["value"] == true),
        "setup-build" => update(|s| s.build_kiri = v["value"] == "kiri"),
        "setup-open" => finish(),
        // SAFETY: ends this thread's message loop; run on the window's thread.
        "setup-quit" => Handle.run(|_| unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) }),
        "copy" => copy_to_clipboard(v["value"].as_str().unwrap_or("")),
        // Welcome / error links: only the pinned downloads and the WSL docs.
        "open" => {
            if let Some(url) = v["name"].as_str().filter(|u| setup_open_url(u)) {
                crate::app::open_url(url);
            }
        }
        _ => {}
    }
}
