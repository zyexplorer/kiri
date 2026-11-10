//! Running Windows tools safely. `Command::new("reg")` (and friends) search
//! the directory of the running exe before System32, so a `reg.exe`,
//! `powershell.exe` or DLL planted next to kiri-win.exe — e.g. in the
//! unzipped bundle in Downloads — would run in their place. Every system
//! tool is resolved to its real System32 path, and the DLL search path is
//! pinned to System32 for anything loaded at run time (WebView2's loader).
//!
//! PowerShell only ever runs script files installed beside kiri-win.exe
//! (contrib/windows), with `-File` and their parameters as arguments; never
//! script text this binary carries, encoded or not. A hidden PowerShell fed
//! an encoded script is what malware droppers do, and Defender's machine
//! learning quarantined a kiri-win.exe that did it (Trojan:Win32/Bearfoos.A!ml).

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::System::LibraryLoader::SetDefaultDllDirectories;
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetExitCodeProcess, IsWow64Process2, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, SW_SHOWNORMAL, WM_SETTINGCHANGE};

const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;

/// What every script kiri-win runs gets before `-File <script>`: no
/// profile, no prompts, and no execution policy in the way of Kiri's own
/// unsigned scripts.
pub const PS_FILE: [&str; 5] = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"];

/// `C:\Windows\System32`, from the API rather than any environment variable.
fn system32() -> PathBuf {
    let mut buf = [0u16; 260];
    // SAFETY: GetSystemDirectoryW fills `buf` with up to its length and
    // returns the count; 0 means it could not be read.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n > buf.len() {
        return PathBuf::from(r"C:\Windows\System32");
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..n]))
}

/// The full path of a Windows tool. A name with a path separator (the
/// bundled docker CLI) is returned as given; a bare name resolves under
/// System32.
pub fn resolve(name: &str) -> PathBuf {
    if name.contains('\\') || name.contains('/') {
        return PathBuf::from(name);
    }
    let dir = system32();
    if name.eq_ignore_ascii_case("powershell.exe") || name.eq_ignore_ascii_case("powershell") {
        return dir.join("WindowsPowerShell").join("v1.0").join("powershell.exe");
    }
    // Explorer lives in Windows\, not System32 — resolving it under System32
    // makes every Reveal / Open log file click fail silently (spawn ENOENT).
    if name.eq_ignore_ascii_case("explorer.exe") || name.eq_ignore_ascii_case("explorer") {
        return dir.parent().unwrap_or(dir.as_path()).join("explorer.exe");
    }
    let mut leaf = name.to_owned();
    if !leaf.to_ascii_lowercase().ends_with(".exe") {
        leaf.push_str(".exe");
    }
    let p = dir.join(&leaf);
    // wsl.exe is a System32 stub on Windows 10 2004+/11; fall back to PATH
    // only if it is somehow absent, so an old build still runs.
    if leaf.eq_ignore_ascii_case("wsl.exe") && !p.is_file() {
        return PathBuf::from("wsl.exe");
    }
    p
}

/// A `Command` for a Windows tool at its real path. Callers add their own
/// flags (e.g. no window).
pub fn tool(name: &str) -> Command {
    Command::new(resolve(name))
}

/// Runs a script file (contrib/windows/kiri-admin.ps1) in an elevated
/// PowerShell and waits for it: ShellExecuteEx's "runas", so Windows asks
/// for an administrator's approval first. Its exit code, or an error if it
/// did not start: ERROR_CANCELLED (1223) when the approval was declined.
/// Straight from here, with no PowerShell `Start-Process -Verb RunAs` in
/// between to carry it, and its console window shows (no
/// `-WindowStyle Hidden`): Windows' approval prompt does anyway.
pub fn elevated_script(script: &Path, args: &[&str]) -> io::Result<u32> {
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let script = script.to_string_lossy();
    let params = PS_FILE.iter().copied().chain(Some(&*script)).chain(args.iter().copied()).map(quote).collect::<Vec<_>>().join(" ");
    let (verb, file, params) = (wide("runas"), wide(&resolve("powershell.exe").to_string_lossy()), wide(&params));
    // SAFETY: the strings outlive the call and are NUL-terminated; the
    // process handle ShellExecuteExW returns (NOCLOSEPROCESS) is waited on,
    // read and closed here. COM is initialized as ShellExecuteEx asks, and
    // uninitialized only if this call did it.
    unsafe {
        let com = CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        let started = ShellExecuteExW(&mut info);
        let error = io::Error::last_os_error();
        if com >= 0 {
            CoUninitialize();
        }
        if started == 0 {
            return Err(error);
        }
        if info.hProcess == 0 {
            return Err(io::Error::other("Windows started no PowerShell"));
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let read = GetExitCodeProcess(info.hProcess, &mut code);
        let error = io::Error::last_os_error();
        CloseHandle(info.hProcess);
        if read == 0 {
            return Err(error);
        }
        Ok(code)
    }
}

/// One argument as Windows' command-line parsing (CommandLineToArgvW, and
/// PowerShell's) reads it back: quoted when it has a space, tab or quote,
/// with backslashes doubled only before a quote.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut s = String::from('"');
    let mut slashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        s.extend(std::iter::repeat_n('\\', if c == '"' { slashes * 2 + 1 } else { slashes }));
        slashes = 0;
        s.push(c);
    }
    s.extend(std::iter::repeat_n('\\', slashes * 2));
    s.push('"');
    s
}

/// Tells running programs (Explorer, so new terminals) that the user's
/// environment variables changed, as [Environment]::SetEnvironmentVariable
/// does after it writes them.
pub fn environment_changed() {
    let what: Vec<u16> = "Environment".encode_utf16().chain(Some(0)).collect();
    let mut result = 0usize;
    // SAFETY: a broadcast with a NUL-terminated string that outlives the
    // call (SendMessageTimeout waits, at most 5 s per window).
    unsafe {
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, what.as_ptr() as isize, SMTO_ABORTIFHUNG, 5000, &mut result);
    }
}

/// Pins run-time DLL loading to System32, so a DLL dropped next to the exe is
/// not loaded in place of a system one. Call once, first thing in `main`.
/// (Statically imported DLLs are handled by the `/DEPENDENTLOADFLAG` and
/// static-CRT link flags in .cargo/config.toml.)
pub fn harden_dll_search() {
    // SAFETY: a process-wide setting with a constant flag.
    unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }
}

/// This build's architecture as Windows names it: "x64" or "arm64".
pub const BUILD_ARCH: &str = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };

/// The PC's own processor, as Windows reports it even to a program running
/// under emulation (an x64 Kiri on Windows on Arm): "arm64", "x64", or
/// None when Windows does not say.
pub fn native_arch() -> Option<&'static str> {
    let (mut process, mut native) = (0u16, 0u16);
    // SAFETY: the current-process pseudo handle and two out-pointers to
    // locals; the call only writes them.
    if unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) } == 0 {
        return None;
    }
    match native {
        IMAGE_FILE_MACHINE_ARM64 => Some("arm64"),
        IMAGE_FILE_MACHINE_AMD64 => Some("x64"),
        _ => None,
    }
}

/// Why this Kiri cannot set up on this PC, if it cannot: its Linux half is
/// built for one architecture, and WSL2 runs the PC's own (an arm64
/// kernel on Windows on Arm, with no x86-64 emulation inside its distros),
/// and WSL's installer is per architecture too. An x64 Kiri does start on
/// an Arm PC, under Windows' emulation, so it says which to install.
pub fn arch_mismatch() -> Option<String> {
    mismatch(BUILD_ARCH, native_arch()?)
}

fn mismatch(build: &str, native: &str) -> Option<String> {
    (build != native).then(|| {
        let v = env!("CARGO_PKG_VERSION");
        format!("This is Kiri for {build} PCs, and this PC is {native}: its Linux cannot run Kiri's {build} engine. Install Kiri-{v}-{}.msi instead.", if native == "arm64" { "arm64" } else { "x86_64" })
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn quotes_arguments_as_windows_reads_them() {
        assert_eq!(super::quote("-Dns"), "-Dns");
        assert_eq!(super::quote(""), "\"\"");
        assert_eq!(super::quote(r"C:\Users\A B\kiri-admin.ps1"), r#""C:\Users\A B\kiri-admin.ps1""#);
        assert_eq!(super::quote(r"C:\A B\"), r#""C:\A B\\""#);
        assert_eq!(super::quote(r#"a "b""#), r#""a \"b\"""#);
        assert_eq!(super::quote(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn names_the_installer_to_use() {
        assert_eq!(super::mismatch("x64", "x64"), None);
        assert_eq!(super::mismatch("arm64", "arm64"), None);
        let m = super::mismatch("x64", "arm64").unwrap();
        assert!(m.contains(&format!("Kiri-{}-arm64.msi", env!("CARGO_PKG_VERSION"))), "{m}");
        assert!(super::mismatch("arm64", "x64").unwrap().ends_with("-x86_64.msi instead."));
    }

    #[test]
    fn explorer_resolves_beside_windows_not_system32() {
        let p = super::resolve("explorer.exe");
        assert!(p.ends_with(r"Windows\explorer.exe"), "{}", p.display());
        assert!(p.is_file(), "missing {}", p.display());
    }
}
