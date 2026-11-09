//! Updates: the newest release, downloaded, checked and installed.
//!
//! The release feed is one small JSON file, `{"version", "url", "sha256"}`
//! (and optionally `notes`, Markdown for the window's What's new, and
//! `notes_url`, a runkiri.com page on it), at
//! `KIRI_UPDATE_URL` (default: `download.runkiri.com/latest-mac-arm64.json`,
//! and `latest-mac-x86_64.json` for the Intel build: each Mac has its own app). The
//! download is checked against the SHA-256, the installed app's own signing
//! identity, notarization and the version the feed promised before it is
//! staged beside the app in `.kiri-update/`. Installing swaps the staged app
//! in (checking it again first) and keeps the old one until the new one is
//! in place.
//!
//! The window (ui) downloads in the background, then installs on Restart
//! now, or when it quits after Later (or at its next launch, if that quit
//! never happened). `kiri update` does it all at once; `--check` only
//! reports.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

/// Each Mac reads its own feed, so a release for one never reaches the other
/// (and check() refuses an app without code for this CPU anyway).
#[cfg(all(not(target_os = "linux"), not(target_arch = "x86_64")))]
const FEED: &str = "https://download.runkiri.com/latest-mac-arm64.json";
/// Intel Macs get the Intel app (`cargo xtask release --arch x86_64`).
#[cfg(all(not(target_os = "linux"), target_arch = "x86_64"))]
const FEED: &str = "https://download.runkiri.com/latest-mac-x86_64.json";
/// Linux reads only the version: its kiri is a .deb or .rpm installed from
/// runkiri.com, so nothing is downloaded from the feed.
#[cfg(target_os = "linux")]
const FEED: &str = "https://download.runkiri.com/latest.json";
/// The biggest download accepted (the app is about 60 MB packed).
const MAX_DOWNLOAD: u64 = 2 << 30;

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub notes_url: String,
}

/// `1.2.10` > `1.2.9`; a missing part counts as 0.
pub fn newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(10)))
        .user_agent(concat!("kiri/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

pub fn latest() -> io::Result<Release> {
    let url = std::env::var("KIRI_UPDATE_URL").unwrap_or_else(|_| FEED.to_owned());
    let release = (|| -> io::Result<Release> {
        let body = agent()
            .get(&url)
            .call()
            .map_err(io::Error::other)?
            .body_mut()
            .with_config()
            .limit(64 << 10)
            .read_to_vec()
            .map_err(io::Error::other)?;
        serde_json::from_slice(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{url}: {e}")))
    })();
    match &release {
        Ok(r) => crate::diag::info!(
            "update check",
            latest = r.version,
            current = env!("CARGO_PKG_VERSION")
        ),
        Err(e) => crate::diag::warn!("update check failed", error = e),
    }
    release
}

/// The running Kiri.app.
pub fn app() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let app = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf);
    app.ok_or_else(|| {
        io::Error::other("Kiri updates replace Kiri.app; this binary is not running from one")
    })
}

/// `.kiri-update/` beside the app: on the same volume, so installing is a
/// rename.
fn stage_dir(app: &Path) -> io::Result<PathBuf> {
    Ok(app
        .parent()
        .ok_or_else(|| io::Error::other("Kiri.app has no parent directory"))?
        .join(".kiri-update"))
}

/// Downloads `release`, checks it and stages it for `install_staged`.
/// `progress` gets the bytes so far and the total when the server says;
/// setting `cancel` stops the download.
pub fn download(
    release: &Release,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> io::Result<()> {
    let app = app()?;
    if !newer(&release.version, env!("CARGO_PKG_VERSION")) {
        return Err(io::Error::other(format!(
            "Kiri {} is not newer than this {}",
            release.version,
            env!("CARGO_PKG_VERSION")
        )));
    }
    let dir = stage_dir(&app)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    crate::diag::info!("update download started", version = release.version);
    let result = fetch_and_check(release, &app, &dir, progress, cancel);
    match &result {
        Ok(()) => crate::diag::info!("update downloaded and checked", version = release.version),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => {
            crate::diag::info!("update download cancelled", version = release.version)
        }
        Err(e) => crate::diag::warn!(
            "update download failed",
            version = release.version,
            error = e
        ),
    }
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

fn fetch_and_check(
    release: &Release,
    app: &Path,
    dir: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> io::Result<()> {
    // Download, hashing as it streams to disk.
    let tgz = dir.join("Kiri.tar.gz");
    let mut resp = agent().get(&release.url).call().map_err(io::Error::other)?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let mut body = resp.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let mut out = std::fs::File::create(&tgz)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        got += n as u64;
        progress(got, total);
    }
    out.sync_all()?;
    let sum: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if !sum.eq_ignore_ascii_case(release.sha256.trim()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("download does not match its SHA-256 (got {sum})"),
        ));
    }
    let unpacked = dir.join("unpacked");
    std::fs::create_dir_all(&unpacked)?;
    // A sparse-aware tar: the VM's disk template inside is mostly holes.
    if !Command::new("/usr/bin/tar")
        .args(["-xzf"])
        .arg(&tgz)
        .arg("-C")
        .arg(&unpacked)
        .status()?
        .success()
    {
        return Err(io::Error::other("could not unpack the update"));
    }
    let _ = std::fs::remove_file(&tgz);
    let new_app = unpacked.join("Kiri.app");
    check(&new_app, app, &release.version)?;
    std::fs::rename(&new_app, dir.join("Kiri.app"))?;
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::write(dir.join("version"), &release.version)?;
    Ok(())
}

/// The staged update's version, when there is one newer than this build.
pub fn staged() -> Option<String> {
    let dir = stage_dir(&app().ok()?).ok()?;
    let version = std::fs::read_to_string(dir.join("version"))
        .ok()?
        .trim()
        .to_owned();
    (dir.join("Kiri.app").is_dir() && newer(&version, env!("CARGO_PKG_VERSION"))).then_some(version)
}

/// Swaps the staged app in for the running one, checking it again first:
/// the folder sat on disk since the download. Returns its version.
pub fn install_staged() -> io::Result<String> {
    let app = app()?;
    let dir = stage_dir(&app)?;
    let Some(version) = staged() else {
        return Err(io::Error::other("no update is staged"));
    };
    let new_app = dir.join("Kiri.app");
    check(&new_app, &app, &version).inspect_err(|e| {
        crate::diag::error!(
            "staged update failed its checks",
            version = version,
            error = e
        )
    })?;
    // The old app moves aside, the new one takes its place, and the old one
    // is only deleted once the new one is there.
    let old = dir.join("Kiri.old.app");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(&app, &old)?;
    if let Err(e) = std::fs::rename(&new_app, &app) {
        let _ = std::fs::rename(&old, &app);
        crate::diag::error!(
            "update install failed; the old app is back",
            version = version,
            error = e
        );
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&dir);
    crate::diag::info!(
        "update installed",
        version = version,
        from = env!("CARGO_PKG_VERSION")
    );
    Ok(version)
}

/// Restarts the daemon on the app's current binary. Running containers stop;
/// those with a restart policy start again.
pub fn restart_engine() {
    crate::diag::info!("restarting the engine");
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let _ = Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &format!("gui/{uid}/dev.kiri.daemon")])
        .stderr(std::process::Stdio::null())
        .status();
}

/// Whether `new_app` is a genuine Kiri `promised`, newer than this build.
fn check(new_app: &Path, app: &Path, promised: &str) -> io::Result<()> {
    // A valid signature is not enough: `--verify` alone passes for any
    // self-consistent signature, including an attacker's own Developer ID or
    // an ad-hoc one. Pin the update to the SAME signing identity as the app
    // already installed (its designated requirement), so only a genuine Kiri
    // release is accepted, and require Gatekeeper/notarization acceptance.
    let dr = Command::new("/usr/bin/codesign")
        .args(["-d", "-r", "-"])
        .arg(app)
        .output()?;
    let requirement = String::from_utf8_lossy(&dr.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("designated => ").map(str::to_owned))
        .filter(|r| !r.trim().is_empty());
    let Some(requirement) = requirement else {
        return Err(io::Error::other(
            "cannot read the installed app's signing identity to check the update against",
        ));
    };
    let mut verify = Command::new("/usr/bin/codesign");
    verify
        .args(["--verify", "--deep", "--strict"])
        .arg(format!("-R={requirement}"));
    if !verify.arg(new_app).status()?.success() {
        return Err(io::Error::other(
            "the update is not signed by Kiri's own signing identity",
        ));
    }
    if !Command::new("/usr/sbin/spctl")
        .args(["-a", "-t", "exec"])
        .arg(new_app)
        .status()?
        .success()
    {
        return Err(io::Error::other("the update is not notarized"));
    }
    // A replaced feed could point at an older, genuinely signed release and
    // its known bugs: the bundle must be the version the feed promised, and
    // that must be newer than this one. Read after the signature checks, so
    // only a verified file is parsed.
    // A feed for the other kind of Mac would install an app that does not
    // open here.
    let mut head = vec![0u8; 4096];
    let n = std::fs::File::open(new_app.join("Contents/MacOS/kiri"))?.read(&mut head)?;
    if !runs_here(&head[..n]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the update is not built for this Mac ({})",
                std::env::consts::ARCH
            ),
        ));
    }
    let plist = std::fs::read_to_string(new_app.join("Contents/Info.plist"))?;
    let Some(version) = bundle_version(&plist) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the update's Info.plist has no CFBundleShortVersionString",
        ));
    };
    let promised = promised.trim_start_matches('v');
    if version != promised {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the update is Kiri {version}, but the feed promised {promised}"),
        ));
    }
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the update is Kiri {version}, which is not newer than this {}",
                env!("CARGO_PKG_VERSION")
            ),
        ));
    }
    Ok(())
}

/// Whether a Mach-O executable (its first bytes) has code for this CPU:
/// a thin binary of this architecture, or a universal one with a slice of it.
fn runs_here(head: &[u8]) -> bool {
    const ARM64: u32 = 0x0100_000c;
    const X86_64: u32 = 0x0100_0007;
    let want = if cfg!(target_arch = "x86_64") {
        X86_64
    } else {
        ARM64
    };
    has_cpu(head, want)
}

fn has_cpu(head: &[u8], want: u32) -> bool {
    let word = |at: usize, be: bool| -> Option<u32> {
        let b: [u8; 4] = head.get(at..at + 4)?.try_into().ok()?;
        Some(if be {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    match word(0, true) {
        // MH_MAGIC_64, little-endian on disk: cputype follows.
        Some(0xcffa_edfe) => word(4, false) == Some(want),
        // FAT_MAGIC / FAT_MAGIC_64: big-endian, nfat_arch, then fat_arch
        // entries (20 or 32 bytes) that start with the cputype.
        Some(magic @ (0xcafe_babe | 0xcafe_babf)) => {
            let step = if magic == 0xcafe_babe { 20 } else { 32 };
            let count = word(4, true).unwrap_or(0).min(64) as usize;
            (0..count).any(|i| word(8 + i * step, true) == Some(want))
        }
        _ => false,
    }
}

pub fn run(args: &[String]) -> io::Result<()> {
    if crate::flavor::dev() {
        return Err(io::Error::other(format!(
            "{} doesn't update itself: rebuild it with `cargo xtask app-dev`",
            crate::flavor::APP
        )));
    }
    let current = env!("CARGO_PKG_VERSION");
    let release = latest()?;
    if !newer(&release.version, current) {
        println!("Kiri {current} is up to date");
        return Ok(());
    }
    println!(
        "Kiri {} is available (this is {current}){}",
        release.version,
        if release.notes.is_empty() {
            String::new()
        } else {
            format!(": {}", release.notes)
        }
    );
    if args.iter().any(|a| a == "--check") {
        return Ok(());
    }
    if cfg!(target_os = "linux") {
        return Err(io::Error::other(
            "download the new .deb or .rpm from https://runkiri.com/releases/ and install it as before",
        ));
    }
    download(&release, &mut |_, _| {}, &AtomicBool::new(false))?;
    install_staged()?;
    // The daemon runs from the app: restart it on the new binary.
    restart_engine();
    println!("updated to Kiri {}", release.version);
    Ok(())
}

/// `CFBundleShortVersionString` from an Info.plist: the key, then the next
/// `<string>`. The file is our own (xtask writes it as XML), so a scan does.
fn bundle_version(plist: &str) -> Option<String> {
    const KEY: &str = "<key>CFBundleShortVersionString</key>";
    let after = &plist[plist.find(KEY)? + KEY.len()..];
    let start = after.find("<string>")? + "<string>".len();
    let end = start + after[start..].find("</string>")?;
    Some(after[start..end].trim().to_owned()).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{Release, bundle_version, has_cpu, newer, runs_here};

    #[test]
    fn compares_versions() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("1.2.10", "1.2.9"));
        assert!(newer("v1.0.1", "1.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn reads_the_bundle_version() {
        let plist = "<plist><dict>\n  <key>CFBundleVersion</key><string>7</string>\n  <key>CFBundleShortVersionString</key>\n  <string> 0.3.1 </string>\n</dict></plist>";
        assert_eq!(bundle_version(plist).as_deref(), Some("0.3.1"));
        assert_eq!(
            bundle_version(
                "<plist><dict><key>CFBundleVersion</key><string>7</string></dict></plist>"
            ),
            None
        );
        assert_eq!(
            bundle_version("<key>CFBundleShortVersionString</key><string></string>"),
            None
        );
    }

    #[test]
    fn checks_the_update_is_for_this_mac() {
        let thin = |cpu: u32| {
            [0xcfu8, 0xfa, 0xed, 0xfe]
                .into_iter()
                .chain(cpu.to_le_bytes())
                .chain([0; 8])
                .collect::<Vec<u8>>()
        };
        assert!(has_cpu(&thin(0x0100_000c), 0x0100_000c));
        assert!(!has_cpu(&thin(0x0100_0007), 0x0100_000c));
        // Universal: x86_64 then arm64.
        let mut fat = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2];
        for cpu in [0x0100_0007u32, 0x0100_000c] {
            fat.extend(cpu.to_be_bytes());
            fat.extend([0; 16]);
        }
        assert!(has_cpu(&fat, 0x0100_000c) && has_cpu(&fat, 0x0100_0007));
        assert!(!has_cpu(&fat[..12], 0x0100_000c));
        assert!(!has_cpu(b"#!/bin/sh\n", 0x0100_000c));
        // This test binary itself.
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(cfg!(not(target_os = "macos")) || runs_here(&me[..4096]));
    }

    #[test]
    fn reads_feeds_with_and_without_notes_url() {
        let r: Release =
            serde_json::from_str(r#"{"version":"0.1.1","url":"https://x/K.tar.gz","sha256":"ab"}"#)
                .unwrap();
        assert_eq!(r.notes_url, "");
        let r: Release = serde_json::from_str(
            r#"{"version":"0.1.1","url":"u","sha256":"ab","notes_url":"https://runkiri.com/"}"#,
        )
        .unwrap();
        assert_eq!(r.notes_url, "https://runkiri.com/");
        let r: Release = serde_json::from_str(
            r###"{"version":"0.1.5","url":"u","sha256":"ab","notes":"## New\n- a line"}"###,
        )
        .unwrap();
        assert_eq!(r.notes, "## New\n- a line");
    }
}
