//! Settings → Resources on Windows. Kiri's engine runs in WSL's one VM,
//! whose CPUs and memory are `processors` and `memory` under `[wsl2]` in
//! %USERPROFILE%\.wslconfig (memory is half of the PC's when unset). Saving
//! writes those two keys and keeps the rest of the file; they apply when
//! WSL restarts, which stops every WSL distro, not only Kiri's.
//!
//! Disk location is the WSL distro's `ext4.vhdx` folder (Lxss BasePath).
//! Setup chooses it before `wsl --import`; Resources can move it later with
//! `wsl --manage <distro> --move`, or reclaim host space by compacting the
//! VHDX (fstrim + diskpart) after deleted images leave holes inside Linux.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use crate::engine::{emit, notice, tr, trf};

fn path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".wslconfig"))
}

/// `32GB`, `8192MB`, `512m` → MiB.
fn mib(v: &str) -> Option<u64> {
    let v = v.trim().to_ascii_uppercase();
    let digits = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let n: u64 = v[..digits].parse().ok()?;
    match v[digits..].trim().trim_end_matches('B') {
        "T" => Some(n << 20),
        "G" => Some(n << 10),
        "M" => Some(n),
        "K" => Some(n >> 10),
        "" => Some(n >> 20),
        _ => None,
    }
}

/// A key's value under `[wsl2]`.
fn get(text: &str, key: &str) -> Option<String> {
    let mut in_wsl2 = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_wsl2 = l.eq_ignore_ascii_case("[wsl2]");
        } else if in_wsl2 && let Some((k, v)) = l.split_once('=') && k.trim().eq_ignore_ascii_case(key) {
            return Some(v.split('#').next().unwrap_or_default().trim().to_owned());
        }
    }
    None
}

/// The file with `key=value` under `[wsl2]`, replacing the key if it is
/// there and adding the section if it isn't.
fn set(text: &str, key: &str, value: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let (mut in_wsl2, mut seen_wsl2, mut done) = (false, false, false);
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            if in_wsl2 && !done {
                // The section's end, after its last non-blank line.
                let at = out.iter().rposition(|x| !x.trim().is_empty()).map_or(out.len(), |i| i + 1);
                out.insert(at, format!("{key}={value}"));
                done = true;
            }
            in_wsl2 = l.eq_ignore_ascii_case("[wsl2]");
            seen_wsl2 |= in_wsl2;
        } else if in_wsl2 && !done && l.split_once('=').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(key)) {
            out.push(format!("{key}={value}"));
            done = true;
            continue;
        }
        out.push(line.to_owned());
    }
    if !done {
        if !seen_wsl2 {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push("[wsl2]".into());
        }
        out.push(format!("{key}={value}"));
    }
    out.join("\r\n") + "\r\n"
}

fn physical_mib() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: a plain struct with its length set, filled by the call.
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) != 0 { m.ullTotalPhys >> 20 } else { 0 }
    }
}

/// What the running VM has: CPUs, and memory in MiB as Linux reports it
/// (MemTotal; see `as_set`). None while Kiri's engine isn't running, so
/// that Settings never starts WSL.
pub(crate) fn running() -> Option<(u64, u64)> {
    if !crate::client::alive() {
        return None;
    }
    let mut cmd = crate::win::tool("wsl.exe");
    if let Some(d) = crate::engine::distro() {
        cmd.args(["-d", &d]);
    }
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.args(["-u", "root", "--", "sh", "-c", "nproc; head -1 /proc/meminfo"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines();
    let cpus: u64 = lines.next()?.trim().parse().ok()?;
    let kib: u64 = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    Some((cpus, kib >> 10))
}

/// The memory the VM runs with, as Settings shows it. Linux reports less
/// than WSL gave it: the kernel keeps some back, 1.2 GB of 64 GB, more than
/// rounding up to a whole GB makes good, and Settings said "up to 63 GB"
/// and offered a restart that changed nothing. Up to a sixteenth under
/// the setting is the setting; anything else is rounded up to a whole GB.
fn as_set(running_mib: u64, set_mib: u64) -> u64 {
    if running_mib <= set_mib && running_mib + set_mib / 16 >= set_mib {
        set_mib
    } else {
        running_mib.div_ceil(1024) * 1024
    }
}

/// CPUs and memory from `.wslconfig` (what Settings → Resources saved).
pub(crate) fn configured() -> (u64, u64) {
    let text = path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let max_cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
    let max_mib = physical_mib();
    let cpus = get(&text, "processors").and_then(|v| v.parse().ok()).unwrap_or(max_cpus).clamp(1, max_cpus);
    let memory = get(&text, "memory").and_then(|v| mib(&v)).unwrap_or(max_mib / 2);
    (cpus, memory)
}

fn disk_fields() -> (Option<String>, Option<u64>, bool) {
    let Some(name) = crate::setup::distro().or_else(crate::engine::distro) else {
        return (None, None, false);
    };
    let path = crate::setup::distro_base_path(&name).map(|p| p.to_string_lossy().into_owned());
    let bytes = crate::setup::distro_vhdx_bytes(&name);
    let movable = path.is_some();
    (path, bytes, movable)
}

fn event() -> Value {
    let (cpus, memory) = configured();
    let max_cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
    let max_mib = physical_mib();
    let now = running();
    let (disk_path, disk_bytes, disk_movable) = disk_fields();
    json!({
        "type": "resources",
        "cpus": cpus,
        "memory_mib": memory,
        "running_cpus": now.map_or(cpus, |r| r.0),
        "running_memory_mib": now.map_or(memory, |r| as_set(r.1, memory)),
        "awake": now.is_some(),
        "max_cpus": max_cpus,
        "max_memory_mib": max_mib,
        "overridden": false,
        "disk_path": disk_path,
        "disk_bytes": disk_bytes,
        "disk_movable": disk_movable,
        "disk_moving": disk_op() == DiskOp::Moving,
        "disk_compacting": disk_op() == DiskOp::Compacting,
        "restart_note": "Restarting restarts WSL: every WSL distro stops, not only Kiri's, and Kiri's containers start again as they're set to.",
    })
}

fn save(body: &Value) -> io::Result<()> {
    let p = path().ok_or_else(|| io::Error::other("USERPROFILE is not set"))?;
    let mut text = std::fs::read_to_string(&p).unwrap_or_default();
    if let Some(n) = body["cpus"].as_u64().filter(|&n| n >= 1) {
        text = set(&text, "processors", &n.to_string());
    }
    if let Some(m) = body["memory_mib"].as_u64().filter(|&m| m >= 1024) {
        text = set(&text, "memory", &if m % 1024 == 0 { format!("{}GB", m / 1024) } else { format!("{m}MB") });
    }
    std::fs::write(&p, text)
}

/// Reads (or saves, then reads) the CPUs and memory for Settings.
pub fn resources(body: Option<Value>) {
    std::thread::spawn(move || {
        let saved = body.map(|b| save(&b));
        let ev = event();
        match saved {
            // While Kiri's engine runs, the toast offers to restart WSL now.
            Some(Ok(())) => emit(json!({"type": "notice", "kind": "ok", "text": tr("Saved in .wslconfig. WSL uses these from its next start"), "act": if ev["awake"] == true { "restartvm" } else { "" }}).to_string()),
            Some(Err(e)) => emit(notice("error", &trf("Could not save .wslconfig: {error}", &[("error", &e)]))),
            None => {}
        }
        emit(ev.to_string());
    });
}

/// Restart to apply: WSL stops and Kiri's engine starts again.
pub fn restart() {
    if disk_busy() {
        return;
    }
    std::thread::spawn(|| {
        let mut cmd = crate::win::tool("wsl.exe");
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        let r = cmd.arg("--shutdown").status().and_then(|s| if s.success() { Ok(()) } else { Err(io::Error::other("wsl --shutdown failed")) }).and_then(|()| crate::engine::ensure_daemon());
        emit(match r {
            Ok(()) => notice("ok", &tr("WSL restarted with the new resources")),
            Err(e) => notice("error", &trf("Could not restart WSL: {error}", &[("error", &e)])),
        });
        emit(event().to_string());
    });
}

/// Move or reclaim holds the VHDX; only one may run, and Restart VM waits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DiskOp {
    None = 0,
    Moving = 1,
    Compacting = 2,
}

static DISK_OP: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn disk_op() -> DiskOp {
    match DISK_OP.load(Ordering::SeqCst) {
        1 => DiskOp::Moving,
        2 => DiskOp::Compacting,
        _ => DiskOp::None,
    }
}

fn disk_busy() -> bool {
    disk_op() != DiskOp::None
}

fn begin_disk_op(op: DiskOp) -> bool {
    DISK_OP
        .compare_exchange(DiskOp::None as u8, op as u8, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

fn end_disk_op() {
    DISK_OP.store(DiskOp::None as u8, Ordering::SeqCst);
}

fn quiet_wsl() -> std::process::Command {
    let mut c = crate::win::tool("wsl.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000);
    c.env("WSL_UTF8", "1");
    c
}

fn wsl_err(out: &std::process::Output) -> io::Error {
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    io::Error::other(text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("WSL failed").to_owned())
}

fn shutdown_wsl() -> io::Result<()> {
    let out = quiet_wsl().arg("--shutdown").output()?;
    if out.status.success() { Ok(()) } else { Err(wsl_err(&out)) }
}

/// `wsl --shutdown` returns before HCS always releases `ext4.vhdx`. True when
/// an exclusive open succeeds (disk is free to move) or the file is gone.
fn vhdx_unlocked(distro: &str) -> bool {
    let Some(base) = crate::setup::distro_base_path(distro) else {
        return true;
    };
    let vhdx = base.join("ext4.vhdx");
    if !vhdx.is_file() {
        return true;
    }
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(&vhdx).is_ok()
}

fn wait_vhdx_free(distro: &str, budget: std::time::Duration) {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if vhdx_unlocked(distro) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Errors that mean the VM/VHDX is still held — worth another shutdown + wait.
fn busy_move_err(err: &io::Error) -> bool {
    let s = err.to_string();
    let lower = s.to_ascii_lowercase();
    lower.contains("in use")
        || lower.contains("being used")
        || lower.contains("used by another")
        || lower.contains("sharing violation")
        || s.contains("使用")
        || s.contains("占用")
        || lower.contains("0x80070020")
        || lower.contains("0x8037011e")
}

/// Drop the bridge, shut WSL down, wait for the VHDX, then
/// `wsl --manage --move`. Retries when WSL still reports the VM in use.
///
/// Do **not** POST `/kiri/stop` here: that writes the persistent `stopped`
/// marker, and after the move `kiri-start` brings up a daemon that refuses
/// (or exits) until Start — which itself cannot clear the marker if the
/// daemon never answers. A plain `wsl --shutdown` releases the VHDX.
fn move_distro(distro: &str, dest: &Path) -> io::Result<()> {
    // Drop the bridge so a reconnect does not wake the distro mid-move.
    crate::serve::renew_bridge();

    const ATTEMPTS: u32 = 8;
    let mut last = io::Error::other("WSL move failed");
    for attempt in 0..ATTEMPTS {
        if let Err(e) = shutdown_wsl() {
            last = e;
            std::thread::sleep(std::time::Duration::from_millis(400));
            continue;
        }
        wait_vhdx_free(distro, std::time::Duration::from_secs(8));
        match quiet_wsl().args(["--manage", distro, "--move"]).arg(dest).output() {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => {
                last = wsl_err(&o);
                if !busy_move_err(&last) {
                    return Err(last);
                }
            }
            Err(e) => {
                last = e;
                if !busy_move_err(&last) {
                    return Err(last);
                }
            }
        }
        crate::serve::log(&format!(
            "disk move: attempt {} still busy ({last}); shutting down again",
            attempt + 1
        ));
        std::thread::sleep(std::time::Duration::from_millis(400 + 200 * u64::from(attempt)));
    }
    Err(last)
}

/// Settings → Resources → Move…: pick a parent folder, move the distro's
/// VHDX there with `wsl --manage --move`, then restart the engine.
pub fn move_disk() {
    if !begin_disk_op(DiskOp::Moving) {
        return;
    }
    std::thread::spawn(|| {
        let finish = |r: io::Result<()>| {
            end_disk_op();
            match r {
                Ok(()) => emit(notice("ok", &tr("Linux disk moved. WSL restarted with the new location"))),
                Err(e) => emit(notice("error", &trf("Could not move the Linux disk: {error}", &[("error", &e)]))),
            }
            emit(event().to_string());
        };
        emit(event().to_string());
        let Some(distro) = crate::setup::distro().or_else(crate::engine::distro) else {
            finish(Err(io::Error::other("Kiri's Linux is not set up yet")));
            return;
        };
        let Some(parent) = crate::app::pick_folder_blocking(&tr("Choose where the Linux disk should live")) else {
            end_disk_op();
            emit(event().to_string());
            return;
        };
        let dest = parent.join(&distro);
        if let Some(cur) = crate::setup::distro_base_path(&distro) {
            if same_dir(&cur, &dest) {
                finish(Ok(()));
                return;
            }
            if dest.exists() && !dir_empty(&dest) {
                finish(Err(io::Error::other(format!("{} is not empty", dest.display()))));
                return;
            }
        }
        if let Err(e) = std::fs::create_dir_all(&parent) {
            finish(Err(io::Error::other(format!("could not create {}: {e}", parent.display()))));
            return;
        }
        if let Err(e) = move_distro(&distro, &dest) {
            finish(Err(e));
            return;
        }
        crate::setup::set_wsl_root(&parent);
        // Disk is already at dest. Starting the engine after --move can take
        // longer than a normal restart; never report that as a failed move.
        match restart_engine_after_move(&distro) {
            Ok(()) => finish(Ok(())),
            Err(e) => {
                crate::serve::log(&format!("disk move: disk moved but engine did not start: {e}"));
                end_disk_op();
                emit(notice("ok", &tr("Linux disk moved. Start the engine to run containers again.")));
                emit(event().to_string());
            }
        }
    });
}

/// Settings → Resources → Reclaim…: fstrim, shut WSL down, compact
/// `ext4.vhdx` with diskpart, then restart the engine. Does not enable
/// experimental sparse VHD.
pub fn compact_disk() {
    if !begin_disk_op(DiskOp::Compacting) {
        return;
    }
    std::thread::spawn(|| {
        let finish_err = |e: io::Error| {
            end_disk_op();
            emit(notice("error", &trf("Could not reclaim Linux disk space: {error}", &[("error", &e)])));
            emit(event().to_string());
        };
        emit(event().to_string());
        let Some(distro) = crate::setup::distro().or_else(crate::engine::distro) else {
            finish_err(io::Error::other("Kiri's Linux is not set up yet"));
            return;
        };
        let Some(base) = crate::setup::distro_base_path(&distro) else {
            finish_err(io::Error::other("Linux disk path is not available yet"));
            return;
        };
        let vhdx = base.join("ext4.vhdx");
        if !vhdx.is_file() {
            finish_err(io::Error::other(format!("{} not found", vhdx.display())));
            return;
        }
        let before = std::fs::metadata(&vhdx).ok().map(|m| m.len()).unwrap_or(0);

        // Best-effort TRIM while the distro can still run; never /kiri/stop.
        try_fstrim(&distro);

        crate::serve::renew_bridge();
        if let Err(e) = shutdown_wsl() {
            finish_err(e);
            return;
        }
        wait_vhdx_free(&distro, std::time::Duration::from_secs(15));

        // Compact refuses NTFS-sparse VHDs; leave sparse off afterwards.
        if vhdx_reported_sparse(&distro, &vhdx) {
            let _ = set_distro_sparse(&distro, false);
            wait_vhdx_free(&distro, std::time::Duration::from_secs(8));
        }

        if let Err(e) = compact_vhdx_file(&vhdx) {
            crate::serve::log(&format!("disk compact failed ({e}); restarting engine anyway"));
            let _ = restart_engine_after_move(&distro);
            finish_err(e);
            return;
        }

        let after = std::fs::metadata(&vhdx).ok().map(|m| m.len()).unwrap_or(before);
        let reclaimed = before.saturating_sub(after);

        match restart_engine_after_move(&distro) {
            Ok(()) => {
                end_disk_op();
                if reclaimed == 0 {
                    emit(notice("ok", &tr("Linux disk compacted. The file size did not change.")));
                } else {
                    emit(notice(
                        "ok",
                        &trf("Linux disk compacted. Reclaimed {size}.", &[("size", &fmt_bytes(reclaimed))]),
                    ));
                }
                emit(event().to_string());
            }
            Err(e) => {
                crate::serve::log(&format!("disk compact: compacted but engine did not start: {e}"));
                end_disk_op();
                if reclaimed == 0 {
                    emit(notice("ok", &tr("Linux disk compacted. Start the engine to run containers again.")));
                } else {
                    emit(notice(
                        "ok",
                        &trf(
                            "Linux disk compacted. Reclaimed {size}. Start the engine to run containers again.",
                            &[("size", &fmt_bytes(reclaimed))],
                        ),
                    ));
                }
                emit(event().to_string());
            }
        }
    });
}

fn fmt_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{} MB", (n + MB / 2) / MB)
    } else if n >= KB {
        format!("{} KB", (n + KB / 2) / KB)
    } else {
        format!("{n} B")
    }
}

/// Issue discard so compact can see freed blocks. Best-effort only.
fn try_fstrim(distro: &str) {
    if !crate::client::alive() {
        return;
    }
    let mut cmd = quiet_wsl();
    cmd.args(["-d", distro, "-u", "root", "--", "fstrim", "-av"]);
    match crate::update::output_within(&mut cmd, std::time::Duration::from_secs(120)) {
        Ok(o) if o.status.success() => {}
        Ok(o) => crate::serve::log(&format!("disk compact: fstrim exited {}: {}", o.status, wsl_err(&o))),
        Err(e) => crate::serve::log(&format!("disk compact: fstrim skipped ({e})")),
    }
}

fn vhdx_reported_sparse(distro: &str, vhdx: &Path) -> bool {
    if let Ok(out) = quiet_wsl().args(["--manage", distro, "--get-sparse"]).output() {
        let t = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).to_ascii_lowercase();
        if t.contains("true") {
            return true;
        }
        if t.contains("false") {
            return false;
        }
    }
    let mut cmd = crate::win::tool("fsutil.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let Ok(out) = cmd.args(["sparse", "queryflag"]).arg(vhdx).output() else {
        return false;
    };
    let t = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    if t.contains("not set as sparse") || t.contains("is not set") {
        return false;
    }
    t.contains("set as sparse") || t.contains("is set")
}

fn set_distro_sparse(distro: &str, on: bool) -> io::Result<()> {
    let flag = if on { "true" } else { "false" };
    let out = quiet_wsl().args(["--manage", distro, "--set-sparse", flag]).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(wsl_err(&out))
    }
}

/// diskpart compact (Home-friendly). Non-ASCII BasePath uses `subst` first —
/// diskpart's script encoding is unreliable with Unicode paths.
fn compact_vhdx_file(vhdx: &Path) -> io::Result<()> {
    let abs = vhdx.canonicalize().unwrap_or_else(|_| vhdx.to_path_buf());
    let path = strip_unc_prefix(&abs);
    if path_has_non_ascii(&path) {
        return compact_via_subst(&path);
    }
    run_diskpart_compact(&path)
}

fn compact_via_subst(vhdx_path: &str) -> io::Result<()> {
    let letter = free_drive_letter().ok_or_else(|| io::Error::other("no free drive letter for subst"))?;
    let parent = Path::new(vhdx_path).parent().ok_or_else(|| io::Error::other("vhdx has no parent"))?;
    subst_run(letter, parent, true)?;
    let mapped = format!(
        "{letter}:\\{}",
        Path::new(vhdx_path).file_name().and_then(|n| n.to_str()).unwrap_or("ext4.vhdx")
    );
    let result = run_diskpart_compact(&mapped);
    let _ = subst_run(letter, parent, false);
    result
}

fn strip_unc_prefix(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).replace('/', "\\")
}

fn path_has_non_ascii(p: &str) -> bool {
    !p.is_ascii()
}

fn free_drive_letter() -> Option<char> {
    // SAFETY: GetLogicalDrives returns a bitmask; no pointers.
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    for c in (b'F'..=b'Z').rev() {
        let bit = 1u32 << (c - b'A');
        if mask & bit == 0 {
            return Some(c as char);
        }
    }
    None
}

fn subst_run(letter: char, path: &Path, add: bool) -> io::Result<()> {
    let mut cmd = crate::win::tool("subst.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = if add {
        cmd.arg(format!("{letter}:")).arg(path).output()?
    } else {
        cmd.arg(format!("{letter}:")).arg("/d").output()?
    };
    if out.status.success() {
        Ok(())
    } else {
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        Err(io::Error::other(text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("subst failed").to_owned()))
    }
}

const COMPACT_LIMIT: std::time::Duration = std::time::Duration::from_secs(45 * 60);

fn run_diskpart_compact(vhdx_path: &str) -> io::Result<()> {
    let escaped = vhdx_path.replace('"', "");
    let script = format!(
        "select vdisk file=\"{escaped}\"\r\nattach vdisk readonly\r\ncompact vdisk\r\ndetach vdisk\r\n"
    );
    let dir = std::env::temp_dir();
    let file = dir.join(format!("kiri-compact-{}.txt", std::process::id()));
    std::fs::write(&file, script.as_bytes())?;
    let mut cmd = crate::win::tool("diskpart.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    cmd.arg("/s").arg(&file);
    let out = crate::update::output_within(&mut cmd, COMPACT_LIMIT);
    let _ = std::fs::remove_file(&file);
    let out = out?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    let lower = text.to_ascii_lowercase();
    // Prefer an explicit success line; diskpart may still exit 0 on failure.
    if lower.contains("successfully compacted") || lower.contains("100 percent completed") {
        return Ok(());
    }
    let bad = !out.status.success()
        || lower.contains("access is denied")
        || lower.contains("is not valid")
        || lower.contains("virtual disk system limitation")
        || lower.contains("must not be sparse")
        || lower.contains("cannot be compacted")
        || lower.contains("failed");
    if bad {
        let detail = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.chars().all(|c| c.is_ascii_digit() || c == '%' || c.is_whitespace()))
            .unwrap_or("diskpart compact failed");
        return Err(io::Error::other(detail.to_owned()));
    }
    // Quiet success with exit 0 and no known failure string.
    if out.status.success() {
        return Ok(());
    }
    Err(io::Error::other("diskpart compact failed"))
}

/// Clear a leftover window-Stop marker inside the distro so `kiri-start`
/// can bring the daemon up after a move (or a prior aborted Stop).
fn clear_stopped_marker(distro: &str) {
    let mut cmd = quiet_wsl();
    cmd.args([
        "-d",
        distro,
        "-u",
        "root",
        "--",
        "rm",
        "-f",
        "/var/lib/kiri-daemon/.kiri/stopped",
    ]);
    let _ = cmd.output();
}

/// After `wsl --manage --move`, the distro's first start is often slower than
/// `ensure_daemon`'s usual wait — retry with a short pause between attempts.
fn restart_engine_after_move(distro: &str) -> io::Result<()> {
    crate::serve::renew_bridge();
    // Brief settle: Lxss may still be updating BasePath right after --move.
    std::thread::sleep(std::time::Duration::from_secs(1));
    clear_stopped_marker(distro);
    let mut last = io::Error::other("engine did not start after disk move");
    for attempt in 0..5 {
        match crate::engine::ensure_daemon() {
            Ok(()) => {
                let _ = crate::client::request("POST", "/kiri/wake", None);
                return Ok(());
            }
            Err(e) => {
                last = e;
                crate::serve::log(&format!(
                    "disk move: engine start attempt {} failed ({last})",
                    attempt + 1
                ));
                clear_stopped_marker(distro);
                crate::serve::renew_bridge();
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    }
    Err(last)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let a = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let b = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    a == b
}

fn dir_empty(path: &Path) -> bool {
    std::fs::read_dir(path).map(|mut d| d.next().is_none()).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_running_as_set_shows_its_setting() {
        // WSL given 64 GB: Linux's MemTotal is 65843312 kB.
        assert_eq!(as_set(65843312 >> 10, 65536), 65536);
        assert_eq!(as_set(8000, 8192), 8192);
        // Given less, or more, than the setting now says: what it has.
        assert_eq!(as_set(65843312 >> 10, 32768), 63 * 1024);
        assert_eq!(as_set(16000, 65536), 16 * 1024);
    }

    #[test]
    fn reads_sizes() {
        assert_eq!(mib("32GB"), Some(32768));
        assert_eq!(mib("8192MB"), Some(8192));
        assert_eq!(mib("4g"), Some(4096));
        assert_eq!(mib("lots"), None);
    }

    #[test]
    fn sets_keys_under_wsl2_and_keeps_the_rest() {
        let text = "[wsl2]\r\nmemory=32GB\r\n\r\n[experimental]\r\nsparseVhd=true\r\n";
        assert_eq!(get(text, "memory").as_deref(), Some("32GB"));
        let t = set(&set(text, "memory", "16GB"), "processors", "8");
        assert_eq!(t, "[wsl2]\r\nmemory=16GB\r\nprocessors=8\r\n\r\n[experimental]\r\nsparseVhd=true\r\n");
        assert_eq!(get(&t, "processors").as_deref(), Some("8"));
        assert_eq!(set("", "memory", "4GB"), "[wsl2]\r\nmemory=4GB\r\n");
        assert_eq!(set("[boot]\r\nx=1\r\n", "memory", "4GB"), "[boot]\r\nx=1\r\n\r\n[wsl2]\r\nmemory=4GB\r\n");
    }
}
