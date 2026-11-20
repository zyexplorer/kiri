//! Updates on Windows: the newest release, downloaded, checked and installed,
//! as crates/kiri/src/update.rs does for the Mac.
//!
//! The release feed is one small JSON file, `{"version", "url", "sha256"}`
//! (and optionally `notes_url`, the page on what's new, and `size`), at
//! `KIRI_UPDATE_URL` (default: `download.runkiri.com/latest-windows-x86_64.json`,
//! or `latest-windows-arm64.json` for Windows on Arm's build);
//! `url` is the MSI. The download is checked against the SHA-256, then the
//! MSI itself is read (msi.dll) and must carry Kiri's own UpgradeCode, this
//! build's platform (x64 and arm64 share the UpgradeCode, so an x64 MSI
//! would otherwise replace an arm64 Kiri) and the version the feed
//! promised, newer than this build, before it is
//! staged in `%LOCALAPPDATA%\Kiri\update`. Installing hands the staged MSI
//! to msiexec once this process has quit (the files it replaces are in
//! use until then): `kiri.ps1 -Update` (contrib/windows, installed beside
//! the exe) waits for that, runs the per-user upgrade silently, and opens
//! the new Kiri.
//!
//! The window downloads in the background, then installs on Restart now,
//! or when it quits after Later (or at its next launch, if that quit never
//! happened). The engine in WSL keeps running the old binary until
//! `restart_engine` copies the new one in and restarts it, which the new
//! Kiri does once the engine answers (`catch_up_engine`); the page offers
//! it as "Finish updating" when that failed and the engine still lags.
//!
//! What this cannot do that the Mac does: pin the download to a signing
//! identity, since the MSI is not signed. The feed is fetched over HTTPS and
//! the file must match its SHA-256 from there.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[path = "../../kiri/src/ui/recheck.rs"]
mod recheck;

use serde_json::json;

use crate::engine::{emit, notice, trf};

#[cfg(not(target_arch = "aarch64"))]
const FEED: &str = "https://download.runkiri.com/latest-windows-x86_64.json";
#[cfg(target_arch = "aarch64")]
const FEED: &str = "https://download.runkiri.com/latest-windows-arm64.json";
/// The biggest download accepted (the MSI is about 55 MB).
const MAX_DOWNLOAD: u64 = 2 << 30;
/// installer/windows/kiri.wxs's UpgradeCode: what makes an MSI a Kiri one.
const UPGRADE_CODE: &str = "{7C2E4B9A-5D13-4F8E-9B6A-0E1D3C5A7F21}";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub notes_url: String,
    /// The MSI's size, for the progress bar when the server does not say.
    #[serde(default)]
    pub size: u64,
}

/// `1.2.10` > `1.2.9`; a missing part counts as 0.
pub fn newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.trim_start_matches('v').split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// %LOCALAPPDATA%\Kiri, where the MSI installs and kiri-win keeps its state.
fn dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Kiri")).unwrap_or_else(|| PathBuf::from("."))
}

/// Where a download is staged; it outlives the upgrade, which keeps the
/// user's folder.
fn stage_dir() -> PathBuf {
    dir().join("update")
}

/// curl.exe, part of Windows: kiri-win has no HTTPS client of its own.
fn curl() -> Command {
    let mut c = crate::win::tool("curl.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut c, CREATE_NO_WINDOW);
    c.stdin(Stdio::null());
    c.arg("-A").arg(format!("kiri-win/{CURRENT}"));
    // Redirects stay on HTTPS too: by default curl follows one down to HTTP.
    c.args(["--proto", if cfg!(debug_assertions) { "=https,file" } else { "=https" }, "--proto-redir", "=https"]);
    c
}

/// Only HTTPS carries a release; in a debug build, `file:` is for trying a
/// feed on disk.
fn allowed_url(url: &str) -> io::Result<()> {
    if url.starts_with("https://") || (cfg!(debug_assertions) && url.starts_with("file:///")) {
        Ok(())
    } else {
        Err(io::Error::other(format!("the update feed points at {url}, which is not https")))
    }
}

fn first_line(bytes: &[u8], fallback: &str) -> String {
    String::from_utf8_lossy(bytes).lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(fallback).to_owned()
}

pub fn latest() -> io::Result<Release> {
    let url = std::env::var("KIRI_UPDATE_URL").unwrap_or_else(|_| FEED.to_owned());
    allowed_url(&url)?;
    let out = curl().args(["-fsSL", "--max-time", "20", "--max-filesize", "65536", "--"]).arg(&url).output()?;
    if !out.status.success() {
        return Err(io::Error::other(first_line(&out.stderr, "could not fetch the update feed")));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{url}: {e}")))
}

/// The size the server says the file is, if it says.
fn content_length(url: &str) -> Option<u64> {
    let out = curl().args(["-sIL", "--max-time", "20", "--"]).arg(url).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .filter_map(|(_, v)| v.trim().parse::<u64>().ok())
        .next_back()
}

/// Downloads `release`, checks it and stages it for `install_staged`.
/// `progress` gets the bytes so far and the total when known; setting
/// `cancel` stops the download.
pub fn download(release: &Release, progress: &mut dyn FnMut(u64, Option<u64>), cancel: &AtomicBool) -> io::Result<()> {
    if !newer(&release.version, CURRENT) {
        return Err(io::Error::other(format!("Kiri {} is not newer than this {CURRENT}", release.version)));
    }
    allowed_url(&release.url)?;
    let dir = stage_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let result = fetch_and_check(release, &dir, progress, cancel);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

fn fetch_and_check(release: &Release, dir: &Path, progress: &mut dyn FnMut(u64, Option<u64>), cancel: &AtomicBool) -> io::Result<()> {
    let part = dir.join("Kiri.msi.part");
    let total = if release.size > 0 { Some(release.size) } else { content_length(&release.url) };
    let mut child = curl()
        .args(["-fsSL", "--retry", "3", "--max-time", "3600", "--max-filesize", &MAX_DOWNLOAD.to_string(), "-o"])
        .arg(&part)
        .arg("--")
        .arg(&release.url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stderr = child.stderr.take();
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let got = std::fs::metadata(&part).map_or(0, |m| m.len());
        progress(got, total);
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                let mut err = Vec::new();
                if let Some(mut e) = stderr.take() {
                    let _ = e.read_to_end(&mut err);
                }
                return Err(io::Error::other(first_line(&err, "downloading the update failed: check the internet connection")));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let got = std::fs::metadata(&part)?.len();
    if got > MAX_DOWNLOAD {
        return Err(io::Error::other("the update is larger than any Kiri release"));
    }
    progress(got, Some(total.unwrap_or(got)));
    let sum = crate::setup::sha256(&part)?;
    if !sum.eq_ignore_ascii_case(release.sha256.trim()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("download does not match its SHA-256 (got {sum})")));
    }
    check(&part, &release.version)?;
    let msi = dir.join(format!("Kiri-{}.msi", release.version.trim_start_matches('v')));
    std::fs::rename(&part, &msi)?;
    std::fs::write(dir.join("version"), release.version.trim_start_matches('v'))?;
    // A fresh stage: an earlier helper's failure no longer counts against it.
    let _ = std::fs::remove_file(dir.join("attempted"));
    Ok(())
}

fn staged_msi(version: &str) -> PathBuf {
    stage_dir().join(format!("Kiri-{version}.msi"))
}

/// The staged update's version, when there is one newer than this build.
pub fn staged() -> Option<String> {
    let version = std::fs::read_to_string(stage_dir().join("version")).ok()?.trim().to_owned();
    (staged_msi(&version).is_file() && newer(&version, CURRENT)).then_some(version)
}

/// The staged MSI, checked again (it sat on disk since the download).
fn verify_staged() -> io::Result<(String, PathBuf)> {
    let Some(version) = staged() else {
        return Err(io::Error::other("no update is staged"));
    };
    let msi = staged_msi(&version);
    check(&msi, &version)?;
    Ok((version, msi))
}

/// A helper was left to install the staged update; it removes the stage
/// when it is done, so this still being here at a launch means it failed
/// (or has not run yet). Then Kiri starts as it is and the card offers
/// Restart now again, rather than quitting at every launch.
pub fn attempted() -> bool {
    stage_dir().join("attempted").is_file()
}

/// Checks the staged MSI again, then leaves a helper behind that installs
/// it as soon as this process has quit and opens the new Kiri. The caller
/// quits. Returns the version.
pub fn install_staged() -> io::Result<String> {
    let (version, msi) = verify_staged()?;
    std::fs::write(stage_dir().join("attempted"), "")?;
    // The helper is kiri.ps1 -Update, installed beside this exe: it waits
    // for this process, upgrades silently and opens the new Kiri, logging
    // to helper.log in the stage. A script file, not a script in this
    // binary: a hidden PowerShell fed an encoded script that runs msiexec
    // is what malware droppers do, and Defender's heuristics quarantined a
    // kiri-win.exe that carried one. Checked against this build's SHA-256
    // first, as setup checks the bundle's other files.
    let script = crate::setup::verified("kiri.ps1")?;
    // A hidden console of its own (CREATE_NO_WINDOW), not none: Windows
    // PowerShell does not come up without one. Out of any job this process
    // is in, if the job allows it (a launcher that kills its children when
    // it closes would take the helper with it).
    let spawn = |flags: u32| {
        let mut cmd = crate::win::tool("powershell.exe");
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, flags);
        cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .args(["-Update", "-Msi"])
            .arg(&msi)
            .arg("-WaitFor")
            .arg(std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    spawn(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(CREATE_NO_WINDOW))?;
    crate::serve::log(&format!("update: installing Kiri {version} after quit"));
    Ok(version)
}

/// Puts the engine binaries the MSI installed into the WSL distro and
/// restarts the daemon (and the bridge, which serve restarts on the new
/// binary). Running containers stop; those with a restart policy start again.
pub fn restart_engine() -> io::Result<()> {
    let distro = crate::setup::saved_distro().ok_or_else(|| io::Error::other("Kiri's Linux is not set up"))?;
    // The pkill patterns spell "kiri" as "[k]iri": `sh -c` runs with this
    // whole script on its command line, and a plain pattern would match
    // that too and kill the shell before it starts the daemon again.
    let script = if distro == crate::setup::KIRI_DISTRO {
        // Kiri Linux: busybox, no systemd; this version's engine, kiri-start
        // and wsl.conf go in, and kiri-start --restart stops the daemon,
        // waits for it to exit and starts it again, under its lock.
        format!("( {} ) >/dev/null || exit 1; pkill -f '/usr/local/bin/[k]iri bridge'; /usr/local/bin/kiri-start --restart", crate::wslsync::sync_script(&dir(), "", true))
    } else {
        // Ubuntu: install.sh installs the binaries and restarts the service.
        let install = crate::setup::to_wsl(&distro, &dir().join("install.sh"))?;
        format!("sh {} && pkill -f '/usr/local/bin/[k]iri bridge'; true", crate::wslsync::sh_quote(&install))
    };
    let mut cmd = crate::win::tool("wsl.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
    let out = output_within(cmd.args(crate::wslsync::wsl_sh(&distro, &script)), RESTART_LIMIT)?;
    if !out.status.success() {
        return Err(io::Error::other(first_line(&out.stderr, "the engine did not restart")));
    }
    // Done only when the engine answers as this version: a restart that
    // brought the old one back would ask again at the next check.
    let began = Instant::now();
    let mut last = None;
    while began.elapsed() < Duration::from_secs(60) {
        if let Some(v) = crate::client::request("GET", "/version", None).ok().filter(|r| r.ok()).and_then(|r| crate::client::kiri_version(&r.json()).map(str::to_owned)) {
            if v == CURRENT {
                return Ok(());
            }
            last = Some(v);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(io::Error::other(match last {
        Some(v) => format!("the engine came back on {v}, not {CURRENT}"),
        None => "the engine did not come back".to_owned(),
    }))
}

/// Whether `msi` is a Kiri installer of the version `promised`, newer than
/// this build: a swapped feed could otherwise point at any MSI, or at an
/// older, genuine one and its known bugs.
fn check(msi: &Path, promised: &str) -> io::Result<()> {
    let upgrade = msi_property(msi, "UpgradeCode")?;
    if !upgrade.trim().eq_ignore_ascii_case(UPGRADE_CODE) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the update is not a Kiri installer"));
    }
    let template = msi_template(msi)?;
    if !platform_matches(&template, crate::win::BUILD_ARCH) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("the update is Kiri for {}, not for this {} Kiri", template.split(';').next().unwrap_or("another platform"), crate::win::BUILD_ARCH)));
    }
    let version = msi_property(msi, "ProductVersion")?;
    let (version, promised) = (version.trim(), promised.trim_start_matches('v'));
    if version != promised {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("the update is Kiri {version}, but the feed promised {promised}")));
    }
    if !newer(version, CURRENT) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("the update is Kiri {version}, which is not newer than this {CURRENT}")));
    }
    Ok(())
}

/// One value from the MSI's Property table, read with Windows Installer
/// itself (msi.dll). `name` is one of our own constants, never input.
fn msi_property(msi: &Path, name: &str) -> io::Result<String> {
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
        MsiCloseHandle, MsiDatabaseOpenViewW, MsiOpenDatabaseW, MsiRecordGetStringW, MsiViewExecute, MsiViewFetch, MSIDBOPEN_READONLY, MSIHANDLE,
    };
    /// Closes an MSI handle when dropped, on every path out.
    struct Handle(MSIHANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: a handle this function opened and nothing else uses.
                unsafe { MsiCloseHandle(self.0) };
            }
        }
    }
    const ERROR_MORE_DATA: u32 = 234;
    const ERROR_NO_MORE_ITEMS: u32 = 259;
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let fail = |what: &str, code: u32| io::Error::other(format!("cannot read the installer ({what}: error {code})"));
    let path = wide(&msi.to_string_lossy());
    let query = wide(&format!("SELECT `Value` FROM `Property` WHERE `Property` = '{name}'"));
    let mut db = Handle(0);
    // SAFETY: valid NUL-terminated strings and out-pointers to our handles,
    // which live until this function returns and are closed by Drop.
    unsafe {
        let r = MsiOpenDatabaseW(path.as_ptr(), MSIDBOPEN_READONLY, &mut db.0);
        if r != 0 {
            return Err(fail("open", r));
        }
        let mut view = Handle(0);
        let r = MsiDatabaseOpenViewW(db.0, query.as_ptr(), &mut view.0);
        if r != 0 {
            return Err(fail("query", r));
        }
        let r = MsiViewExecute(view.0, 0);
        if r != 0 {
            return Err(fail("execute", r));
        }
        let mut rec = Handle(0);
        let r = MsiViewFetch(view.0, &mut rec.0);
        if r == ERROR_NO_MORE_ITEMS {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("the installer has no {name}")));
        }
        if r != 0 {
            return Err(fail("fetch", r));
        }
        // Ask for the size first (the buffer is too small for anything but
        // an empty value), then for the value.
        let mut len: u32 = 1;
        let mut buf: Vec<u16> = vec![0; 1];
        let r = MsiRecordGetStringW(rec.0, 1, buf.as_mut_ptr(), &mut len);
        if r == ERROR_MORE_DATA {
            buf = vec![0; len as usize + 1];
            len += 1;
            let r = MsiRecordGetStringW(rec.0, 1, buf.as_mut_ptr(), &mut len);
            if r != 0 {
                return Err(fail("read", r));
            }
        } else if r != 0 {
            return Err(fail("read", r));
        }
        Ok(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Whether an MSI's Template summary property (`Arm64;1033`, `x64;1033`:
/// the platform `wix build -arch` set, then the languages) is for `arch`.
fn platform_matches(template: &str, arch: &str) -> bool {
    template.split(';').next().is_some_and(|p| p.trim().eq_ignore_ascii_case(arch))
}

/// The MSI's Template summary property, read with msi.dll.
fn msi_template(msi: &Path) -> io::Result<String> {
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::{MsiCloseHandle, MsiGetSummaryInformationW, MsiSummaryInfoGetPropertyW, MSIHANDLE};
    struct Handle(MSIHANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: a handle this function opened and nothing else uses.
                unsafe { MsiCloseHandle(self.0) };
            }
        }
    }
    const PID_TEMPLATE: u32 = 7;
    let fail = |what: &str, code: u32| io::Error::other(format!("cannot read the installer ({what}: error {code})"));
    let path: Vec<u16> = msi.to_string_lossy().encode_utf16().chain([0]).collect();
    let mut info = Handle(0);
    // SAFETY: a NUL-terminated path and out-pointers to locals; the buffer's
    // length is passed with it, in characters, and the handle is closed by Drop.
    unsafe {
        let r = MsiGetSummaryInformationW(0, path.as_ptr(), 0, &mut info.0);
        if r != 0 {
            return Err(fail("summary", r));
        }
        let (mut kind, mut int, mut time) = (0u32, 0i32, std::mem::zeroed());
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        let r = MsiSummaryInfoGetPropertyW(info.0, PID_TEMPLATE, &mut kind, &mut int, &mut time, buf.as_mut_ptr(), &mut len);
        if r != 0 {
            return Err(fail("template", r));
        }
        Ok(String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]))
    }
}

// The window's side: what the page is told, and what its card asks for.

/// Set by the card's cancel: stops a download in progress.
static CANCEL: AtomicBool = AtomicBool::new(false);
/// A download is running; a second Download waits for it.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// About's update row: what the last check found. `state` is uptodate,
/// available, test or error.
fn update_state(state: &str, latest: Option<&str>, error: Option<String>) {
    let checked = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    emit(json!({"type": "updatestate", "state": state, "current": CURRENT, "latest": latest, "error": error, "checked": checked}).to_string());
}

/// Tells the page what there is: a staged update ("updateready"), else a
/// newer release ("update"); and whether the engine runs an older build than
/// the app ("enginestale"). `manual` (Check for Updates…, About's Check now)
/// also says when there is nothing new, and shows a skipped or snoozed
/// version again. With "Check for updates automatically" off, only a manual
/// check asks the feed.
pub fn check_now(manual: bool) {
    std::thread::spawn(move || {
        // An update went in and the engine in WSL still runs the old build:
        // it is moved over (once, catch_up_engine), rather than asking. The
        // card shows only if that fails, and on a manual check.
        // What it tells the page before the window has opened (Kiri started
        // in the background) waits in app.rs's queue, and shows when it opens.
        if !manual {
            catch_up_engine();
        } else if let Some(v) = engine_version()
            && newer(CURRENT, &v)
        {
            emit(json!({"type": "enginestale", "version": v}).to_string());
        }
        feed_check(manual);
    });
    // And every 6 hours while Kiri runs (recheck.rs), as on the Mac.
    recheck::start(|| feed_check(false));
}

/// Whether the feed has a newer release, and one already staged; the
/// window's check and the 6-hourly one.
fn feed_check(manual: bool) {
    {
        // A test build (0.1.8-dev.42, scripts/ci/test-version.sh) isn't
        // updated from the feed: it would "update" back to its release.
        if CURRENT.contains("-dev") {
            if manual {
                emit(notice("ok", &trf("Kiri {version} is a test build: it doesn't update itself", &[("version", &CURRENT)])));
            }
            update_state("test", None, None);
            return;
        }
        if let Some(v) = staged() {
            emit(json!({"type": "updateready", "version": v}).to_string());
            return;
        }
        if !manual && crate::engine::prefs()["updateAuto"] == false {
            return;
        }
        recheck::checked();
        match latest() {
            Ok(r) if newer(&r.version, CURRENT) => {
                update_state("available", Some(&r.version), None);
                emit(json!({"type": "update", "release": r, "current": CURRENT, "manual": manual}).to_string());
            }
            Ok(_) => {
                update_state("uptodate", None, None);
                if manual {
                    emit(notice("ok", &trf("Kiri {version} is the newest version", &[("version", &CURRENT)])));
                }
            }
            Err(e) => {
                update_state("error", None, Some(e.to_string()));
                if manual {
                    emit(notice("error", &trf("Couldn't check for updates: {error}", &[("error", &e)])));
                }
            }
        }
    }
}

/// Download on the card: fetches, checks and stages the release in the
/// background, reporting progress a few times a second.
pub fn download_now() {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    CANCEL.store(false, Ordering::Relaxed);
    let _ = std::thread::Builder::new().name("kiri-update".into()).spawn(|| {
        let result = latest().and_then(|r| {
            let mut last = Instant::now() - Duration::from_secs(1);
            download(
                &r,
                &mut |got, total| {
                    if last.elapsed() >= Duration::from_millis(250) {
                        last = Instant::now();
                        emit(json!({"type": "updateprogress", "got": got, "total": total}).to_string());
                    }
                },
                &CANCEL,
            )
            .map(|()| r.version)
        });
        RUNNING.store(false, Ordering::SeqCst);
        emit(match result {
            Ok(v) => json!({"type": "updateready", "version": v}).to_string(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => r#"{"type":"updatecancelled"}"#.to_owned(),
            Err(e) => json!({"type": "updateerror", "error": e.to_string()}).to_string(),
        });
    });
}

pub fn cancel_download() {
    CANCEL.store(true, Ordering::Relaxed);
}

/// Restart now: the helper is left behind and this Kiri quits; the new one
/// opens once the MSI is in.
pub fn restart_now() {
    // The staged MSI is checked here, so the card hears of a bad one; the
    // helper is left behind by main once the window has quit.
    match verify_staged() {
        Ok(_) => crate::app::Handle.run(|_| {
            // SAFETY: ends this thread's message loop; run on the window's thread.
            unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) }
        }),
        Err(e) => emit(json!({"type": "updateerror", "error": e.to_string()}).to_string()),
    }
}

/// The card's "Finish updating": the engine in WSL moves to the installed
/// binary. Off the window's thread; the page hears when it is done.
/// Before the engine starts (engine::ensure_daemon), the engine binaries in
/// WSL are made the ones this app installed, when they differ, and in Kiri
/// Linux this version's kiri-start and wsl.conf (whose boot command starts
/// the engine when WSL boots the distro), so a distro an older Kiri set up
/// gets them too. Without it, declining "Restart engine" after an update
/// left the old engine in WSL, and every later start (each login, each
/// reboot) ran it again. Copying only: nothing running is stopped, so this
/// is for an engine that isn't up. One call into WSL, killed after
/// WSL_LIMIT. Returns whether anything was copied.
pub fn sync_engine() -> io::Result<bool> {
    if !dir().join("kiri").is_file() || !dir().join("kiri-init").is_file() {
        return Ok(false); // a development build run from elsewhere
    }
    let distro = crate::setup::saved_distro().ok_or_else(|| io::Error::other("Kiri's Linux is not set up"))?;
    let script = crate::wslsync::sync_script(&dir(), "", distro == crate::setup::KIRI_DISTRO);
    let mut cmd = crate::win::tool("wsl.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
    let out = output_within(cmd.args(crate::wslsync::wsl_sh(&distro, &script)), WSL_LIMIT)?;
    if !out.status.success() {
        return Err(io::Error::other(first_line(&out.stderr, "could not copy the engine into WSL")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).contains("copied"))
}

/// The most one call into WSL to sync or start the engine may take: two
/// binaries copied off the Windows disk, over WSL's file sharing, and a
/// start.
pub(crate) const WSL_LIMIT: Duration = Duration::from_secs(60);
/// The most a restart's call into WSL may take: the sync (WSL_LIMIT), then
/// kiri-start --restart: up to 25 s for its lock, 140 s for the old
/// daemon's stop (its shutdown waits up to 120 s for a container with a
/// long --stop-timeout, then for SIGKILL and its VM: about 132 s), 5 s for
/// the new one's socket. 60 + 25 + 140 + 5 = 230 s. Restarts run on
/// threads of their own.
const RESTART_LIMIT: Duration = Duration::from_secs(240);

/// `cmd`'s output, as `Command::output`, but killed (Err TimedOut) when it
/// runs past `limit`: a wsl.exe right after Windows wakes can hang.
pub(crate) fn output_within(cmd: &mut Command, limit: Duration) -> io::Result<std::process::Output> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let until = Instant::now() + limit;
    while child.try_wait()?.is_none() {
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("WSL did not finish in {} s", limit.as_secs())));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // It has exited: what it wrote (a line or two) is in the pipes.
    child.wait_with_output()
}

/// The engine's own Kiri version, when it answers.
fn engine_version() -> Option<String> {
    crate::client::request("GET", "/version", None).ok().filter(|r| r.ok()).and_then(|r| crate::client::kiri_version(&r.json()).map(str::to_owned))
}

static CATCH_UP: crate::wslsync::CatchUp = crate::wslsync::CatchUp::new();

/// Once the engine answers (engine::ensure_daemon, on a thread of its own,
/// and the window's update check), an engine older than this app is
/// restarted on this one's, once per run of kiri-win (wslsync::CatchUp):
/// after an update, Kiri Linux's boot command or the bridge starts the old
/// engine before anything here can copy the new one in, and with no window
/// open nothing else would move it over. Waits for that restart; one asked
/// while another runs returns at once. Never on the engine thread.
pub fn catch_up_engine() {
    if !dir().join("kiri").is_file() {
        return; // a development build run from elsewhere: nothing to copy
    }
    CATCH_UP.run(
        engine_version,
        |v| newer(CURRENT, v),
        |v| {
            crate::serve::log(&format!("update: the engine runs {v}, this Kiri is {CURRENT}; restarting it"));
            emit(json!({"type": "state", "state": "booting"}).to_string());
            restart_and_report();
        },
    );
}

pub fn restart_engine_now() {
    std::thread::spawn(restart_and_report);
}

/// `restart_engine`, with the page and the log told how it went.
fn restart_and_report() {
    let began = Instant::now();
    match restart_engine() {
        Ok(()) => {
            emit(r#"{"type":"enginestale","version":null}"#.to_owned());
            // The page's engine toast, as the Mac's.
            emit(json!({"type": "enginerestarted", "version": CURRENT, "ms": began.elapsed().as_millis() as u64}).to_string());
        }
        Err(e) => {
            crate::serve::log(&format!("engine: did not restart ({e})"));
            emit(notice("error", &trf("Could not restart the engine: {error}", &[("error", &e)])));
            if let Some(v) = engine_version()
                && newer(CURRENT, &v)
            {
                emit(json!({"type": "enginestale", "version": v}).to_string());
            }
        }
    }
    crate::engine::send(crate::engine::Cmd::Refresh);
}

#[cfg(test)]
mod tests {
    use super::{newer, Release};

    #[test]
    fn compares_versions() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("1.2.10", "1.2.9"));
        assert!(newer("v1.0.1", "1.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn reads_feeds_with_and_without_extras() {
        let r: Release = serde_json::from_str(r#"{"version":"0.1.1","url":"https://x/K.msi","sha256":"ab"}"#).unwrap();
        assert_eq!((r.notes_url.as_str(), r.size), ("", 0));
        let r: Release = serde_json::from_str(r#"{"version":"0.1.1","url":"u","sha256":"ab","notes_url":"https://runkiri.com/","size":5}"#).unwrap();
        assert_eq!((r.notes_url.as_str(), r.size), ("https://runkiri.com/", 5));
    }

    #[test]
    fn only_https_feeds() {
        assert!(super::allowed_url("https://download.runkiri.com/latest-windows-x86_64.json").is_ok());
        assert_eq!(super::allowed_url("file:///C:/t/latest.json").is_ok(), cfg!(debug_assertions));
        assert!(super::allowed_url("http://download.runkiri.com/latest-windows-x86_64.json").is_err());
        assert!(super::allowed_url("ftp://x/y").is_err());
    }

    #[test]
    fn reads_this_installer_when_built() {
        // The MSI cargo xtask msi built, if there is one next to this build.
        let name = if cfg!(target_arch = "aarch64") { "arm64" } else { "x86_64" };
        let msi = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target").join(format!("Kiri-{}-{name}.msi", env!("CARGO_PKG_VERSION")));
        if msi.is_file() {
            assert_eq!(super::msi_property(&msi, "ProductVersion").unwrap(), env!("CARGO_PKG_VERSION"));
            assert!(super::msi_property(&msi, "UpgradeCode").unwrap().eq_ignore_ascii_case(super::UPGRADE_CODE));
            assert!(super::platform_matches(&super::msi_template(&msi).unwrap(), crate::win::BUILD_ARCH));
        }
    }

    #[test]
    fn only_this_platforms_installer() {
        assert!(super::platform_matches("x64;1033", "x64"));
        assert!(super::platform_matches("Arm64;1033", "arm64"));
        assert!(!super::platform_matches("x64;1033", "arm64"));
        assert!(!super::platform_matches("Arm64;1033", "x64"));
        assert!(!super::platform_matches("Intel;1033", "x64"));
    }
}
