//! kiri-win: Kiri's Windows side. Serves the Docker API on a named pipe,
//! `\\.\pipe\kiri`, and carries each connection into WSL2 over one
//! long-lived `wsl.exe -u root -- kiri bridge`, multiplexed. TCP through
//! WSL's localhost relay loses half-closes, which `docker run` and
//! `attach` depend on; the pipe and the bridge keep them. It is also
//! Kiri's window and tray icon.
//!
//!   kiri-win [--background] [--distro NAME] [--pipe \\.\pipe\kiri] [--socket PATH] [--kiri PATH]
//!
//! `--background` (from the login shortcut) starts with the tray icon only.
//! Starting it again while it runs shows the running one's window.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
// The window's page, put together as on the Mac and Linux.
#[cfg(windows)]
#[path = "../../kiri/src/ui/assemble.rs"]
mod assemble;
#[cfg(windows)]
#[path = "../../kiri/src/ui/notes.rs"]
mod notes;
// notes.rs (shared with the Mac ui module) calls super::reveal; on Windows
// that resolves here at the crate root, not under app.
#[cfg(windows)]
pub(crate) use app::reveal;
#[cfg(windows)]
mod client;
#[cfg(windows)]
mod detail;
#[cfg(windows)]
mod dns;
#[cfg(windows)]
mod engine;
#[cfg(windows)]
mod pipe;
#[cfg(windows)]
mod serve;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod setup;
// The built-in terminal's shells, on ConPTY.
#[cfg(windows)]
mod term;
#[cfg(windows)]
mod hub;
#[cfg(windows)]
mod k8s;
#[cfg(windows)]
mod diag;
// The Builds view: BuildKit's history through `docker buildx history`, the
// Mac's own crates/kiri/src/builds.rs, and the time parsing it uses.
#[cfg(windows)]
#[path = "../../kiri/src/builds.rs"]
#[allow(dead_code)]
mod builds;
// Which builder plain `docker build` uses, Docker's or Kiri's: the Mac's
// and Linux's own crates/kiri/src/buildwith.rs.
#[cfg(windows)]
#[path = "../../kiri/src/buildwith.rs"]
mod buildwith;
// Kiri's program moved or deleted from where it started: the Mac's and
// Linux's own crates/kiri/src/appwatch.rs.
#[cfg(windows)]
#[path = "../../kiri/src/appwatch.rs"]
mod appwatch;
// An image's Layers and Security tabs, asked of the engine in WSL.
#[cfg(windows)]
mod scan;
// The Mac's crates/kiri/src/daemon/time.rs, as crate::daemon::time (what
// builds.rs calls it). Pathed at the top level: a #[path] inside an inline
// `mod daemon` resolves from src/daemon/, a folder that does not exist, and
// only Windows' lexical `..` forgives that.
#[cfg(windows)]
#[path = "../../kiri/src/daemon/time.rs"]
#[allow(dead_code)]
mod daemon_time;
#[cfg(windows)]
mod daemon {
    pub(crate) use super::daemon_time as time;
}
#[cfg(windows)]
mod wslconfig;
// Kiri Dev's names (the window on Windows is always Kiri) and Share, the
// Mac's own files: crates/kiri/src/flavor.rs and share.rs.
#[cfg(windows)]
#[path = "../../kiri/src/flavor.rs"]
mod flavor;
#[cfg(windows)]
#[path = "../../kiri/src/share.rs"]
#[allow(dead_code)]
mod share;
#[cfg(windows)]
mod update;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    // There is no console to print a panic to.
    std::panic::set_hook(Box::new(|info| serve::log(&format!("panic: {info}"))));
    // Load DLLs only from System32, not from the folder kiri-win runs from.
    win::harden_dll_search();
    // Uninstalling (contrib/windows/kiri.ps1): docker build's own builder
    // back as it was before Kiri changed it, and nothing else.
    if std::env::args().any(|a| a == "--uninstall-builds") {
        return match buildwith::places().and_then(|p| p.uninstall()) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                serve::log(&format!("uninstall: docker build's builder: {e}"));
                std::process::ExitCode::FAILURE
            }
        };
    }
    // Uninstalling, too: the cluster's `kiri` kubectl context out of
    // %USERPROFILE%\.kube\config, the rest of the file as it was.
    if std::env::args().any(|a| a == "--uninstall-kubeconfig") {
        return match k8s::forget() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                serve::log(&format!("uninstall: kubectl context: {e}"));
                std::process::ExitCode::FAILURE
            }
        };
    }
    // An update Later staged that the last quit didn't install: in now, and
    // the helper opens the new Kiri once this process has gone.
    // A staged update goes in before anything else, once: if the helper
    // that was left to install it failed, Kiri starts as it is and the
    // window offers the update again rather than quitting on every launch.
    if update::staged().is_some() && !update::attempted() {
        match update::install_staged() {
            Ok(_) => return std::process::ExitCode::SUCCESS,
            Err(e) => serve::log(&format!("update: the staged update did not install: {e}")),
        }
    }
    let cfg = serve::Config::from_args();
    let background = std::env::args().any(|a| a == "--background");
    // First run (or Kiri's Linux gone): the window opens on setup. Naming a
    // distro (--distro) is for development and skips it.
    let setup = !std::env::args().any(|a| a == "--distro") && setup::needed();
    if let Err(e) = serve::start(cfg.clone()) {
        // Another kiri-win already serves the pipe: show its window instead.
        if app::running() {
            if !background {
                app::show_running();
            }
            return std::process::ExitCode::SUCCESS;
        }
        // No Kiri to show: whatever holds the name is not Kiri, and docker
        // commands would talk to it as if it were. Say so and stop.
        let msg = format!("pipe: {e}; not starting");
        serve::log(&msg);
        eprintln!("kiri-win: {msg}");
        return std::process::ExitCode::FAILURE;
    }
    client::set_pipe(&cfg.pipe);
    if let Err(e) = dns::start() {
        serve::log(&format!("dns on 127.0.0.1:53: {e}"));
    }
    engine::start(engine::Env { distro: cfg.distro.clone(), pipe: cfg.pipe.clone() });
    // Its folder renamed, moved or deleted around it (Windows won't delete
    // a running program itself): docker build handed back as it was before
    // Kiri took it, the choice kept for when Kiri runs again, and Kiri quits.
    appwatch::watch(|exe| {
        serve::log(&format!("{} is gone from where Kiri started; handing docker build back and quitting", exe.display()));
        if let Err(e) = buildwith::places().and_then(|p| p.give_back()) {
            serve::log(&format!("docker build not handed back: {e}"));
        }
        app::Handle.run(|_| {
            // SAFETY: ends the window's message loop; run on its thread.
            unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) }
        });
        // No window to quit (it never opened): quit from here.
        std::thread::sleep(std::time::Duration::from_secs(5));
        std::process::exit(0);
    });
    // The engine starts when the page says `ready`, and with the window
    // hidden (--background, as from the login shortcut) no page loads: after
    // kiri-win restarted, docker got no answer until Start was pressed.
    // Start it now. ensure_daemon runs kiri-start only when the daemon does
    // not answer, and kiri-start does nothing while one runs, so a daemon
    // already up is left alone and never doubled.
    if background && !setup {
        engine::send(engine::Cmd::Start);
    }
    let result = app::run(
        !background || setup,
        setup,
        |msg| if app::in_setup() && setup::handles(&msg) { setup::on_message(&msg) } else { engine::on_message(&msg) },
        engine::on_tray,
    );
    // The window has quit (Restart now on the update card, or Quit after
    // Later): a staged update goes in now, from here only, so the helper
    // is left behind once (it waits for this process to end).
    if update::staged().is_some()
        && let Err(e) = update::install_staged()
    {
        serve::log(&format!("update: the staged update did not install: {e}"));
    }
    if let Err(e) = result {
        serve::log(&format!("window: {e}"));
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("kiri-win runs on Windows only");
    std::process::ExitCode::FAILURE
}
