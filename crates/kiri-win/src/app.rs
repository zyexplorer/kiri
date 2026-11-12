//! Kiri's window on Windows: the Mac app's own page
//! (crates/kiri/src/ui/index.html) in WebView2, and Kiri's icon in the
//! notification area. Closing the window hides it and Kiri keeps serving
//! from the tray, as the Mac app stays in the menu bar.
//!
//! The page talks to its host through `webkit.messageHandlers.kiri`; a
//! script added before it loads forwards that to WebView2's
//! `chrome.webview.postMessage`, so the page itself is unchanged.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, PoisonError};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller, ICoreWebView2Environment, ICoreWebView2NavigationStartingEventArgs, ICoreWebView2NewWindowRequestedEventArgs, ICoreWebView2WebMessageReceivedEventArgs,
    ICoreWebView2_3, COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
};
use webview2_com::{
    AddScriptToExecuteOnDocumentCreatedCompletedHandler, CoTaskMemPWSTR, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, ExecuteScriptCompletedHandler, NavigationCompletedEventHandler, NavigationStartingEventHandler, NewWindowRequestedEventHandler,
    WebMessageReceivedEventHandler,
};
use windows::core::{w, GUID, Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, EnumFontFamiliesExW, GetDC, ReleaseDC, DEFAULT_CHARSET, LOGFONTW, TEXTMETRICW};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, ShellExecuteW, Shell_NotifyIconW, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, NIF_GUID,
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BringWindowToTop, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, FindWindowW, GetClientRect,
    GetCursorPos, GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindowThreadProcessId, IsWindowVisible, IsZoomed, KillTimer, SM_CXSCREEN, SM_CYSCREEN, SW_MAXIMIZE, SW_SHOWNORMAL, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, TrackPopupMenu, TranslateMessage, HICON, HWND_NOTOPMOST, HWND_TOPMOST, ICON_BIG, ICON_SMALL, IDC_ARROW, LR_DEFAULTCOLOR, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, MSG, SW_HIDE, SW_RESTORE, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU,
    WM_LBUTTONUP, WM_RBUTTONUP, WM_SETICON, WM_SIZE, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

pub const CLASS: PCWSTR = w!("KiriWindow");
/// Runs the jobs other threads queued for this one.
const WM_RUN: u32 = WM_APP + 1;
/// From the tray icon.
const WM_TRAY: u32 = WM_APP + 2;
/// From a second launch of kiri-win: show the window.
pub const WM_SHOW: u32 = WM_APP + 3;
/// Open-file dialog: must not run inside `with_ui` (modal Show nests the
/// message loop; holding Ui's RefCell then blanks the shell view and freezes).
const WM_PICK_FILE: u32 = WM_APP + 4;

const PAGE: &str = include_str!("../../kiri/src/ui/index.html");
/// The first-run setup (setup.rs), Windows only.
const SETUP_PAGE: &str = include_str!("setup.html");
/// The window opens on setup rather than Kiri.
static SETUP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the window is on the setup page (setup commands are live then).
pub fn in_setup() -> bool {
    SETUP.load(Ordering::SeqCst)
}

fn page_html() -> String {
    if SETUP.load(Ordering::SeqCst) { SETUP_PAGE.to_owned() } else { crate::assemble::page(PAGE) }
}

/// Virtual host for the assembled main page. NavigateToString is capped at
/// 2 MB of the wide string; personal i18n + xterm exceeds that and paints a
/// blank WebView2. Mapping a folder and Navigating to https://app.kiri.local/
/// has no such cap. Mac/Linux never take this path.
const PAGE_HOST: &str = "app.kiri.local";
const PAGE_URL: &str = "https://app.kiri.local/index.html";

fn main_page_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Kiri").join("webview"))
}

fn write_main_page() -> std::io::Result<std::path::PathBuf> {
    let dir = main_page_dir().ok_or_else(|| std::io::Error::other("LOCALAPPDATA is not set"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("index.html");
    let html = crate::assemble::page(PAGE);
    let tmp = path.with_extension("html.tmp");
    std::fs::write(&tmp, html.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(dir)
}

/// Load setup via NavigateToString (small); the main page via a virtual host.
fn navigate_page(wv: &ICoreWebView2) {
    if SETUP.load(Ordering::SeqCst) {
        let page = CoTaskMemPWSTR::from(SETUP_PAGE);
        // SAFETY: a live webview, on its own thread.
        let _ = unsafe { wv.NavigateToString(*page.as_ref().as_pcwstr()) };
        return;
    }
    let Ok(dir) = write_main_page() else {
        crate::serve::log("webview: could not write the page; falling back to NavigateToString");
        let page = CoTaskMemPWSTR::from(page_html().as_str());
        let _ = unsafe { wv.NavigateToString(*page.as_ref().as_pcwstr()) };
        return;
    };
    // SAFETY: a live webview, on its own thread.
    let mapped = unsafe {
        wv.cast::<ICoreWebView2_3>().and_then(|wv3| {
            wv3.SetVirtualHostNameToFolderMapping(
                &HSTRING::from(PAGE_HOST),
                &HSTRING::from(dir.to_string_lossy().as_ref()),
                COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
            )
        })
    };
    if let Err(e) = mapped {
        crate::serve::log(&format!("webview virtual host: {e}; falling back to NavigateToString"));
        let page = CoTaskMemPWSTR::from(page_html().as_str());
        let _ = unsafe { wv.NavigateToString(*page.as_ref().as_pcwstr()) };
        return;
    }
    let uri = CoTaskMemPWSTR::from(PAGE_URL);
    if let Err(e) = unsafe { wv.Navigate(*uri.as_ref().as_pcwstr()) } {
        crate::serve::log(&format!("webview Navigate: {e}"));
    }
}
/// Kiri's icon cropped to its artwork for Windows (brand/windows-icons.ps1),
/// by size: the Mac's has Apple's margin and looks small in the taskbar.
const ICONS: [(i32, &[u8]); 8] = [
    (16, include_bytes!("../../../brand/icon/windows/kiri-16.png")),
    (20, include_bytes!("../../../brand/icon/windows/kiri-20.png")),
    (24, include_bytes!("../../../brand/icon/windows/kiri-24.png")),
    (32, include_bytes!("../../../brand/icon/windows/kiri-32.png")),
    (40, include_bytes!("../../../brand/icon/windows/kiri-40.png")),
    (48, include_bytes!("../../../brand/icon/windows/kiri-48.png")),
    (64, include_bytes!("../../../brand/icon/windows/kiri-64.png")),
    (256, include_bytes!("../../../brand/icon/windows/kiri-256.png")),
];

/// Before the page's own scripts: its `webkit.messageHandlers.kiri` on WebView2.
/// It can run before there is a root element (WebView2 122, as Windows 11
/// ships it), so the class waits for one. Script errors go to the log: a
/// page that fails to run shows no other sign of it. `KIRI_WSL` is the distro
/// name so volume Mount can show `\\wsl$\…` host paths.
fn shim() -> String {
    let distro = crate::setup::saved_distro()
        .or_else(crate::engine::distro)
        .unwrap_or_else(|| crate::setup::KIRI_DISTRO.to_owned());
    let distro = serde_json::to_string(&distro).unwrap_or_else(|_| "\"kiri\"".into());
    format!(
        r#"window.KIRI_OS = "windows";
window.KIRI_WSL = {distro};
window.webkit = {{ messageHandlers: {{ kiri: {{ postMessage: m =>
  window.chrome.webview.postMessage(typeof m === "string" ? m : JSON.stringify(m)) }} }} }};
if (document.documentElement) document.documentElement.classList.add("windows");
else new MutationObserver((_, o) => {{
  if (document.documentElement) {{ document.documentElement.classList.add("windows"); o.disconnect(); }}
}}).observe(document, {{ childList: true }});
window.addEventListener("error", e => window.chrome.webview.postMessage(JSON.stringify(
  {{ cmd: "jserror", name: `${{e.message}} (line ${{e.lineno}}:${{e.colno}})` }})));"#
    )
}

/// The tray menu: what the engine last reported, kept for the next right-click.
#[derive(Debug, Clone, Default)]
pub struct Menu {
    pub vm: String,
    /// Running containers: id, name.
    pub running: Vec<(String, String)>,
}

type Job = Box<dyn FnOnce(&mut Ui) + Send>;

static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());
static WINDOW: AtomicIsize = AtomicIsize::new(0);

struct PickFileReq {
    title: String,
    filter_label: String,
    filter_pattern: String,
    reply: std::sync::mpsc::Sender<Option<std::path::PathBuf>>,
}

static PICK_FILE: Mutex<Option<PickFileReq>> = Mutex::new(None);

/// What the rest of kiri-win uses to reach the window, from any thread.
#[derive(Debug, Clone, Copy)]
pub struct Handle;

impl Handle {
    pub fn run(self, job: impl FnOnce(&mut Ui) + Send + 'static) {
        JOBS.lock().unwrap_or_else(PoisonError::into_inner).push(Box::new(job));
        let hwnd = WINDOW.load(Ordering::SeqCst);
        if hwnd != 0 {
            // SAFETY: posting to our own window; a closed one just drops it.
            let _ = unsafe { PostMessageW(Some(HWND(hwnd as _)), WM_RUN, WPARAM(0), LPARAM(0)) };
        }
    }

    /// Runs `js` in the page (queued until it has loaded).
    pub fn eval(self, js: String) {
        self.run(move |ui| ui.eval(js));
    }

    pub fn set_menu(self, menu: Menu) {
        self.run(move |ui| ui.menu = menu);
    }
}

/// Tray menu commands.
const ID_OPEN: usize = 1;
const ID_START: usize = 2;
const ID_STOP: usize = 3;
const ID_QUIT: usize = 4;
const ID_UPDATE: usize = 5;
/// Then one per running container, from here.
const ID_CONTAINER: usize = 100;

pub struct Ui {
    hwnd: HWND,
    webview: Option<ICoreWebView2>,
    controller: Option<ICoreWebView2Controller>,
    /// Scripts waiting for the page to load.
    pending: Vec<String>,
    loaded: bool,
    /// The webview has been asked for.
    started: bool,
    menu: Menu,
    /// Every message the page sends; called outside the borrow of UI.
    on_message: Rc<dyn Fn(String)>,
}

impl std::fmt::Debug for Ui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ui").field("loaded", &self.loaded).finish_non_exhaustive()
    }
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// None when there is no window yet, or while it is busy: Win32 calls the
/// window procedure from inside calls made on it (ShowWindow sends WM_SIZE).
fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| u.try_borrow_mut().ok()?.as_mut().map(f))
}

/// As with_ui, but if the window is busy the job runs once it is not.
fn with_ui_soon(f: impl FnOnce(&mut Ui) + Send + 'static) {
    let busy = UI.with(|u| u.try_borrow_mut().is_err());
    if busy {
        Handle.run(f);
    } else {
        with_ui(f);
    }
}

/// The timer that gives show() its second try at the foreground.
const TIMER_FOREGROUND: usize = 1;

/// Brings the window to the front. Windows lets a process take the
/// foreground only while the user is working in it; to any other it
/// flashes the taskbar button and leaves the window behind, which is how
/// Kiri came up behind the installer's window and the shell. For a window
/// the user asked for, sharing the input queue of the thread that is
/// foreground makes the call ours to make; failing even that, the window
/// at least goes on top of the others.
///
/// # Safety
/// `hwnd` is this thread's window.
unsafe fn foreground(hwnd: HWND) {
    // SAFETY: the caller's window; the thread ids come from Windows itself.
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        if GetForegroundWindow() == hwnd {
            return;
        }
        let other = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let me = GetCurrentThreadId();
        let attached = other != 0 && other != me && AttachThreadInput(other, me, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(other, me, false);
        }
        if GetForegroundWindow() != hwnd {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

/// Shows the window, making its webview the first time. The webview is
/// made outside the borrow of UI: its callbacks borrow it too.
fn show() {
    if let Some(Some(hwnd)) = with_ui(Ui::show) {
        if let Err(e) = start_webview(hwnd) {
            crate::serve::log(&format!("webview: {e}"));
        }
    }
}

impl Ui {
    pub fn eval(&mut self, js: String) {
        match (&self.webview, self.loaded) {
            (Some(w), true) => {
                let js = CoTaskMemPWSTR::from(js.as_str());
                let done = ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(())));
                // SAFETY: a live webview, on its own thread.
                let _ = unsafe { w.ExecuteScript(*js.as_ref().as_pcwstr(), &done) };
            }
            _ => self.pending.push(js),
        }
    }

    /// Shows the window; the window to make a webview for, the first time.
    fn show(&mut self) -> Option<HWND> {
        // SAFETY: our window, on its thread.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_RESTORE);
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            foreground(self.hwnd);
            // Opened from the installer's last page (or a launcher), whose
            // own window closes just after this and hands the focus to
            // whatever was behind it: once more, shortly.
            SetTimer(Some(self.hwnd), TIMER_FOREGROUND, 400, None);
        }
        (!std::mem::replace(&mut self.started, true)).then_some(self.hwnd)
    }

    pub fn hide(&mut self) {
        // SAFETY: our window, on its thread.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    pub fn visible(&self) -> bool {
        // SAFETY: our window.
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }

    /// Setup is done: the window becomes Kiri's.
    pub fn navigate_main(&mut self) {
        SETUP.store(false, Ordering::SeqCst);
        self.loaded = false;
        self.pending.clear();
        if let Some(w) = &self.webview {
            navigate_page(w);
        }
    }

    /// Maximizes or restores, for a double-click on the page's toolbar.
    pub fn toggle_maximized(&mut self) {
        // SAFETY: our window, on its thread.
        unsafe {
            let _ = ShowWindow(self.hwnd, if IsZoomed(self.hwnd).as_bool() { SW_RESTORE } else { SW_MAXIMIZE });
        }
    }

    /// "dark", "light" or "" (follow Windows): the page's colours and the
    /// title bar's.
    pub fn set_theme(&mut self, theme: &str) {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2_13, COREWEBVIEW2_PREFERRED_COLOR_SCHEME_AUTO, COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK, COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT,
        };
        let dark = match theme {
            "dark" => true,
            "light" => false,
            _ => windows_dark(),
        };
        // SAFETY: our window and webview, on their thread.
        unsafe {
            let flag = windows::core::BOOL::from(dark);
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&raw const flag).cast(), std::mem::size_of_val(&flag) as u32);
            if let Some(c) = &self.controller {
                set_backdrop(c, dark);
            }
            if let Some(w) = self.webview.as_ref().and_then(|w| w.cast::<ICoreWebView2_13>().ok()) {
                let scheme = match theme {
                    "dark" => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK,
                    "light" => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT,
                    _ => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_AUTO,
                };
                if let Ok(p) = w.Profile() {
                    let _ = p.SetPreferredColorScheme(scheme);
                }
            }
        }
    }

    fn resize(&self) {
        if let Some(c) = &self.controller {
            let mut r = RECT::default();
            // SAFETY: our window and its controller, on their thread.
            unsafe {
                let _ = GetClientRect(self.hwnd, &mut r);
                let _ = c.SetBounds(r);
            }
        }
    }
}

/// Creates the window (hidden unless `show`) and the tray icon, and runs
/// the message loop until Quit. `on_message` gets each message the page
/// sends; `on_tray` each tray command ("start", "stop", "container:<id>").
pub fn run(show: bool, setup: bool, on_message: impl Fn(String) + 'static, on_tray: impl Fn(&str) + 'static) -> windows::core::Result<()> {
    SETUP.store(setup, Ordering::SeqCst);
    // SAFETY: once, on the thread that owns the window and the webview.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    TRAY.with(|t| *t.borrow_mut() = Some(Box::new(on_tray)));
    // SAFETY: plain Win32 window creation with a static class name.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None)?;
        // The page's background from the first frame, so the window never shows white before it loads.
        let (r, g, b) = backdrop(windows_dark());
        let brush = CreateSolidBrush(COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16));
        let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance.into(), lpszClassName: CLASS, hCursor: LoadCursorW(None, IDC_ARROW)?, hbrBackground: brush, ..Default::default() };
        RegisterClassW(&class);
        // 1280x840 at 100%, scaled to the display, centred, and never
        // larger than the screen.
        let scale = |v: i32| v * GetDpiForSystem() as i32 / 96;
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let (w, h) = (scale(1280).min(sw * 9 / 10), scale(840).min(sh * 9 / 10));
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, w!("Kiri"), WS_OVERLAPPEDWINDOW, (sw - w) / 2, (sh - h) / 2, w, h, None, None, Some(instance.into()), None)?;
        let flag = windows::core::BOOL::from(windows_dark());
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&raw const flag).cast(), std::mem::size_of_val(&flag) as u32);
        hwnd
    };
    WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
    // The sizes Windows draws at this display's scale: 16 and 32 at 100%.
    let (small, big) = (icon(16), icon(32));
    // SAFETY: our window; the icons live as long as the process.
    unsafe {
        if let Some(i) = big {
            SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(i.0 as isize)));
        }
        if let Some(i) = small {
            SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(i.0 as isize)));
        }
    }
    UI.with(|u| *u.borrow_mut() = Some(Ui { hwnd, webview: None, controller: None, pending: Vec::new(), loaded: false, started: false, menu: Menu::default(), on_message: Rc::new(on_message) }));
    // GUID identity: drop any ghost left by a previous process that was killed
    // mid-rebuild, then add ours. HWND+uID alone cannot remove another process's icon.
    tray_install(hwnd, small);
    if show {
        self::show();
    }
    // Jobs queued before the window existed.
    run_jobs();
    let mut msg = MSG::default();
    // SAFETY: the thread's ordinary message loop.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    tray(hwnd, None, NIM_DELETE);
    Ok(())
}

/// Whether a kiri-win is running: its window exists, shown or not.
pub fn running() -> bool {
    // SAFETY: looking up a window by our class name.
    unsafe { FindWindowW(CLASS, None).is_ok_and(|h| !h.is_invalid()) }
}

/// Asks the kiri-win already running to show its window. False if there is none.
pub fn show_running() -> bool {
    // SAFETY: looking up a window by our class name and posting to it.
    unsafe {
        match FindWindowW(CLASS, None) {
            Ok(h) if !h.is_invalid() => PostMessageW(Some(h), WM_SHOW, WPARAM(0), LPARAM(0)).is_ok(),
            _ => false,
        }
    }
}

thread_local! {
    static TRAY: RefCell<Option<Box<dyn Fn(&str)>>> = const { RefCell::new(None) };
}

/// Runs queued jobs; if the window is busy (a modal loop inside a call on
/// it), they wait for the next WM_RUN.
fn run_jobs() {
    let jobs = std::mem::take(&mut *JOBS.lock().unwrap_or_else(PoisonError::into_inner));
    let mut jobs = jobs.into_iter();
    while let Some(job) = jobs.next() {
        let busy = UI.with(|u| u.try_borrow_mut().is_err());
        if busy {
            let mut q = JOBS.lock().unwrap_or_else(PoisonError::into_inner);
            let later = std::mem::take(&mut *q);
            q.push(job);
            q.extend(jobs);
            q.extend(later);
            drop(q);
            let hwnd = WINDOW.load(Ordering::SeqCst);
            // SAFETY: posting to our own window.
            let _ = unsafe { PostMessageW(Some(HWND(hwnd as _)), WM_RUN, WPARAM(0), LPARAM(0)) };
            return;
        }
        with_ui(job);
    }
}

/// The icon at `size` pixels at 100%, scaled to the display: drawn from the
/// smallest PNG at least that big, which Windows takes as an icon image.
fn icon(size: i32) -> Option<HICON> {
    // SAFETY: plain system query.
    let px = size * unsafe { GetDpiForSystem() } as i32 / 96;
    let png = ICONS.iter().find(|(s, _)| *s >= px).unwrap_or(&ICONS[ICONS.len() - 1]).1;
    // SAFETY: the bytes are a complete PNG image.
    unsafe { CreateIconFromResourceEx(png, true, 0x0003_0000, px, px, LR_DEFAULTCOLOR).ok() }
}

/// Former GUID tray identity (personal fix for rebuild ghosts). Windows binds
/// a GUID to the **exe path** on first NIM_ADD; unsigned moves (portable →
/// %LOCALAPPDATA%\Kiri, or debug → release) make later NIM_ADD fail and the
/// tray vanishes. Kept only so startup can best-effort DELETE an old GUID icon.
const TRAY_GUID: GUID = GUID::from_u128(0x4b49_5249_5769_6e54_7261_7949_636f_6e01);

fn tray_nid(hwnd: HWND, icon: Option<HICON>, guid: bool) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    if guid {
        nid.uFlags |= NIF_GUID;
        nid.guidItem = TRAY_GUID;
    }
    if let Some(i) = icon {
        nid.hIcon = i;
        nid.uFlags |= NIF_ICON;
    }
    for (d, s) in nid.szTip.iter_mut().zip("Kiri".encode_utf16()) {
        *d = s;
    }
    nid
}

fn tray(hwnd: HWND, icon: Option<HICON>, op: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE) -> bool {
    tray_op(hwnd, icon, op, false)
}

fn tray_op(hwnd: HWND, icon: Option<HICON>, op: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE, guid: bool) -> bool {
    let nid = tray_nid(hwnd, icon, guid);
    // SAFETY: a filled-in NOTIFYICONDATAW for our window.
    unsafe { Shell_NotifyIconW(op, &nid).as_bool() }
}

/// Install the tray icon with HWND+uID (survives unsigned exe moves). Also
/// tries to drop any leftover GUID icon from an older build.
fn tray_install(hwnd: HWND, icon: Option<HICON>) {
    let _ = tray_op(hwnd, None, NIM_DELETE, true);
    let _ = tray_op(hwnd, None, NIM_DELETE, false);
    if !tray_op(hwnd, icon, NIM_ADD, false) {
        crate::serve::log("tray: NIM_ADD failed; trying NIM_MODIFY");
        if !tray_op(hwnd, icon, NIM_MODIFY, false) {
            crate::serve::log("tray: NIM_MODIFY failed; icon may be hidden");
        }
    }
    let mut nid = tray_nid(hwnd, None, false);
    // SAFETY: union write of uVersion for NIM_SETVERSION.
    unsafe {
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    }
}

/// Language for native tray strings: Settings → Appearance, else the Windows UI language.
fn tray_lang() -> &'static str {
    match crate::engine::prefs()["lang"].as_str().unwrap_or("system") {
        "zh" | "zh-CN" => "zh",
        "zh-TW" => "zh-TW",
        "system" | "" => {
            // SAFETY: plain system query.
            let id = unsafe { GetUserDefaultUILanguage() };
            match id {
                0x0804 | 0x1004 => "zh",             // zh-CN, zh-SG
                0x0404 | 0x0c04 | 0x1404 => "zh-TW", // zh-TW, zh-HK, zh-MO
                _ => "en",
            }
        }
        // Tray catalog is zh / zh-TW only; en and other UI langs keep English labels.
        _ => "en",
    }
}

/// Tray / menu-bar strings, keyed like the page's `tr()` catalog (zh / zh-TW inline).
fn tray_tr(en: &str) -> String {
    let s = match (tray_lang(), en) {
        ("zh", "Open Kiri") => "打开 Kiri",
        ("zh", "Start") => "启动",
        ("zh", "Stop") => "停止",
        ("zh", "Running") => "运行中",
        ("zh", "Stopped") => "已停止",
        ("zh", "Asleep") => "休眠",
        ("zh", "Not running") => "未运行",
        ("zh", "Check for Updates…") => "检查更新…",
        ("zh", "Quit Kiri") => "退出 Kiri",
        ("zh-TW", "Open Kiri") => "打開 Kiri",
        ("zh-TW", "Start") => "啟動",
        ("zh-TW", "Stop") => "停止",
        ("zh-TW", "Running") => "運行中",
        ("zh-TW", "Stopped") => "已停止",
        ("zh-TW", "Asleep") => "休眠",
        ("zh-TW", "Not running") => "未運行",
        ("zh-TW", "Check for Updates…") => "檢查更新…",
        ("zh-TW", "Quit Kiri") => "結束 Kiri",
        _ => en,
    };
    s.to_owned()
}

/// The tray's right-click menu, as the Mac's menu bar menu: the VM's state,
/// running containers (click one to open it), Start/Stop, Quit.
fn tray_menu(hwnd: HWND) {
    let mut menu = Menu::default();
    with_ui(|ui| menu = ui.menu.clone());
    let vm = if menu.vm.is_empty() { "…".to_owned() } else { tray_tr(&menu.vm) };
    // SAFETY: a popup menu made, shown and destroyed here, for our window.
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let item = |flags, id: usize, text: &str| {
            let _ = AppendMenuW(m, flags, id, &HSTRING::from(text));
        };
        item(MF_STRING, ID_OPEN, &tray_tr("Open Kiri"));
        item(MF_SEPARATOR, 0, "");
        item(MF_STRING | MF_GRAYED, 0, &format!("Kiri: {vm}"));
        for (i, (_, name)) in menu.running.iter().enumerate().take(20) {
            item(MF_STRING, ID_CONTAINER + i, &format!("    {name}"));
        }
        item(MF_SEPARATOR, 0, "");
        item(MF_STRING, ID_START, &tray_tr("Start"));
        item(MF_STRING, ID_STOP, &tray_tr("Stop"));
        item(MF_SEPARATOR, 0, "");
        item(MF_STRING, ID_UPDATE, &tray_tr("Check for Updates…"));
        item(MF_STRING, ID_QUIT, &tray_tr("Quit Kiri"));
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        // The menu closes when clicking elsewhere only if we are foreground.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, Some(0), hwnd, None).0 as usize;
        let _ = DestroyMenu(m);
        let tray_cmd = |c: &str| TRAY.with(|t| if let Some(f) = t.borrow().as_ref() { f(c) });
        match chosen {
            ID_OPEN => show(),
            ID_START => tray_cmd("start"),
            ID_STOP => tray_cmd("stop"),
            ID_UPDATE => tray_cmd("update"),
            ID_QUIT => PostQuitMessage(0),
            n if n >= ID_CONTAINER => {
                if let Some((id, _)) = menu.running.get(n - ID_CONTAINER) {
                    show();
                    tray_cmd(&format!("container:{id}"));
                }
            }
            _ => {}
        }
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_RUN => run_jobs(),
        WM_PICK_FILE => run_pick_file(hwnd),
        WM_SHOW => show(),
        WM_TIMER if wparam.0 == TIMER_FOREGROUND => {
            // SAFETY: our window, on its thread.
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_FOREGROUND);
                if IsWindowVisible(hwnd).as_bool() {
                    foreground(hwnd);
                }
            }
        }
        WM_SIZE => with_ui_soon(|ui| ui.resize()),
        WM_CLOSE => with_ui_soon(Ui::hide),
        WM_TRAY => match (lparam.0 & 0xffff) as u32 {
            WM_LBUTTONUP => {
                if with_ui(|ui| ui.visible()) == Some(true) {
                    with_ui(Ui::hide);
                } else {
                    show();
                }
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => tray_menu(hwnd),
            _ => {}
        },
        m if m == taskbar_created() => tray_install(hwnd, icon(16)),
        // SAFETY: the default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

/// Explorer restarted: the tray icon has to be added again.
fn taskbar_created() -> u32 {
    thread_local!(static ID: u32 = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) });
    ID.with(|id| *id)
}

/// WebView2's data (cache, local storage) lives in %LOCALAPPDATA%\Kiri.
fn data_dir() -> HSTRING {
    let dir = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Kiri").join("WebView2")).unwrap_or_default();
    HSTRING::from(dir.as_os_str())
}

fn start_webview(hwnd: HWND) -> windows::core::Result<()> {
    let env_done = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, env: Option<ICoreWebView2Environment>| {
        result?;
        let Some(env) = env else { return Ok(()) };
        let ctl_done = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, ctl: Option<ICoreWebView2Controller>| {
            result?;
            if let Some(ctl) = ctl {
                webview_ready(hwnd, ctl)?;
            }
            Ok(())
        }));
        // SAFETY: our window, on its thread.
        unsafe { env.CreateCoreWebView2Controller(hwnd, &ctl_done) }
    }));
    // SAFETY: creates the environment; the handler runs on this thread's loop.
    unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &data_dir(), None, &env_done) }
}

fn webview_ready(hwnd: HWND, ctl: ICoreWebView2Controller) -> windows::core::Result<()> {
    // SAFETY: the new controller and its webview, on the window's thread.
    unsafe {
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        ctl.SetBounds(r)?;
        set_backdrop(&ctl, windows_dark());
        ctl.SetIsVisible(true)?;
        // Block dropping files or links onto the window: that is the only way
        // foreign content could load into the page and reach the host commands.
        if let Ok(c4) = ctl.cast::<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller4>() {
            let _ = c4.SetAllowExternalDrop(false);
        }
        let wv = ctl.CoreWebView2()?;
        let settings = wv.Settings()?;
        settings.SetIsStatusBarEnabled(false)?;
        settings.SetAreDevToolsEnabled(cfg!(debug_assertions) || std::env::var_os("KIRI_DEVTOOLS").is_some())?;
        settings.SetAreDefaultContextMenusEnabled(cfg!(debug_assertions))?;
        settings.SetIsZoomControlEnabled(false)?;

        let mut token = 0;
        wv.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(|_, args: Option<ICoreWebView2WebMessageReceivedEventArgs>| {
                let Some(args) = args else { return Ok(()) };
                let mut s = PWSTR::null();
                if args.TryGetWebMessageAsString(&mut s).is_ok() {
                    let s = CoTaskMemPWSTR::from(s).to_string();
                    if let Some(f) = UI.with(|u| u.try_borrow().ok()?.as_ref().map(|ui| ui.on_message.clone())) {
                        f(s);
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        // The page is the only document this window shows: popups are
        // refused and any navigation away (a dropped link, a slip in the
        // page) is cancelled, so no other content can reach the host commands.
        wv.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(|_, args: Option<ICoreWebView2NewWindowRequestedEventArgs>| {
                if let Some(a) = args {
                    let _ = a.SetHandled(true);
                }
                Ok(())
            })),
            &mut token,
        )?;
        wv.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(|_, args: Option<ICoreWebView2NavigationStartingEventArgs>| {
                if let Some(a) = args {
                    let mut uri = PWSTR::null();
                    let ours = a.Uri(&mut uri).is_ok() && {
                        let u = CoTaskMemPWSTR::from(uri).to_string();
                        // about:/data: for NavigateToString (setup); our virtual host for the main page.
                        u.is_empty()
                            || u.starts_with("about:")
                            || u.starts_with("data:")
                            || u.starts_with("https://app.kiri.local/")
                    };
                    if !ours {
                        let _ = a.SetCancel(true);
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        wv.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(|_, _| {
                with_ui(|ui| {
                    ui.loaded = true;
                    for js in std::mem::take(&mut ui.pending) {
                        ui.eval(js);
                    }
                });
                Ok(())
            })),
            &mut token,
        )?;
        let shim = CoTaskMemPWSTR::from(shim().as_str());
        wv.AddScriptToExecuteOnDocumentCreated(*shim.as_ref().as_pcwstr(), &AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(|_, _| Ok(()))))?;
        navigate_page(&wv);
        let theme = crate::engine::prefs()["theme"].as_str().unwrap_or("").to_owned();
        with_ui(|ui| {
            ui.controller = Some(ctl.clone());
            ui.webview = Some(wv);
            ui.set_theme(&theme);
        });
    }
    Ok(())
}

/// Whether Windows is in dark mode for apps.
/// The page's --bg, dark or light: what shows before it paints.
fn backdrop(dark: bool) -> (u8, u8, u8) {
    if dark { (0x0F, 0x14, 0x19) } else { (0xF5, 0xF7, 0xFA) }
}

/// WebView2 paints white until the page does; paint the page's background instead.
fn set_backdrop(ctl: &ICoreWebView2Controller, dark: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Controller2, COREWEBVIEW2_COLOR};
    let (r, g, b) = backdrop(dark);
    if let Ok(c2) = ctl.cast::<ICoreWebView2Controller2>() {
        // SAFETY: the window's controller, on its thread.
        let _ = unsafe { c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: r, G: g, B: b }) };
    }
}

fn windows_dark() -> bool {
    let mut cmd = crate::win::tool("reg");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "/v", "AppsUseLightTheme"]).output();
    out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("0x0"))
}

/// Opens a link in the default browser.
pub fn open_url(url: &str) {
    // SAFETY: plain strings; no window is needed.
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(url), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Modal open-file dialog on the window thread. `None` if the person
/// cancelled or the dialog could not be shown. Call from a worker thread
/// (not the window's): it posts `WM_PICK_FILE` and waits — never via
/// `Handle.run` / `with_ui`, so Show's nested loop does not hold Ui.
pub fn pick_file(title: &str, filter_label: &str, filter_pattern: &str) -> Option<std::path::PathBuf> {
    let hwnd = WINDOW.load(Ordering::SeqCst);
    if hwnd == 0 {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let mut slot = PICK_FILE.lock().unwrap_or_else(PoisonError::into_inner);
        // One at a time: a second request cancels as if the person closed it.
        if slot.is_some() {
            let _ = tx.send(None);
            return None;
        }
        *slot = Some(PickFileReq {
            title: title.to_owned(),
            filter_label: filter_label.to_owned(),
            filter_pattern: filter_pattern.to_owned(),
            reply: tx,
        });
    }
    // SAFETY: our window; handler runs on its thread without borrowing Ui.
    let _ = unsafe { PostMessageW(Some(HWND(hwnd as _)), WM_PICK_FILE, WPARAM(0), LPARAM(0)) };
    rx.recv().ok().flatten()
}

fn run_pick_file(hwnd: HWND) {
    let Some(req) = PICK_FILE.lock().unwrap_or_else(PoisonError::into_inner).take() else {
        return;
    };
    // WebView2 as a child of the owner can leave the common-item dialog a
    // blank skeleton; hide it for the modal Show, then put it back.
    with_ui(|ui| {
        if let Some(c) = ui.controller.as_ref() {
            // SAFETY: controller for this window, on its thread.
            let _ = unsafe { c.SetIsVisible(false) };
        }
    });
    let path = unsafe { file_open_dialog(hwnd, &req.title, &req.filter_label, &req.filter_pattern) };
    with_ui(|ui| {
        if let Some(c) = ui.controller.as_ref() {
            // SAFETY: as above.
            let _ = unsafe { c.SetIsVisible(true) };
        }
    });
    let _ = req.reply.send(path);
}

/// # Safety
/// `owner` is this thread's window (or null). Must not be called while `Ui`
/// is borrowed — the modal loop dispatches window messages.
unsafe fn file_open_dialog(owner: HWND, title: &str, filter_label: &str, filter_pattern: &str) -> Option<std::path::PathBuf> {
    // SAFETY: COM is already initialised on the window thread; the dialog
    // is in-process and freed when `dialog` drops.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let _ = dialog.SetTitle(&HSTRING::from(title));
        let name: Vec<u16> = filter_label.encode_utf16().chain(Some(0)).collect();
        let spec: Vec<u16> = filter_pattern.encode_utf16().chain(Some(0)).collect();
        let filters = [COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(spec.as_ptr()) }];
        let _ = dialog.SetFileTypes(&filters);
        let opts = dialog.GetOptions().unwrap_or_default() | FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST | FOS_FORCEFILESYSTEM;
        let _ = dialog.SetOptions(opts);
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let pwstr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        if pwstr.is_null() {
            return None;
        }
        let path = pwstr.to_string().ok().map(std::path::PathBuf::from);
        CoTaskMemFree(Some(pwstr.0 as *const _));
        path
    }
}

/// Shows a file selected in Explorer, as the Mac reveals it in Finder.
/// Returns whether Explorer was started; callers may toast on failure.
pub fn reveal(path: &std::path::Path) -> bool {
    let mut cmd = crate::win::tool("explorer.exe");
    std::os::windows::process::CommandExt::raw_arg(&mut cmd, format!("/select,\"{}\"", path.display()));
    cmd.spawn().is_ok()
}

/// The installed font families, sorted; `mono` only the fixed-pitch ones.
pub fn font_families(mono: bool) -> Vec<String> {
    unsafe extern "system" fn each(lf: *const LOGFONTW, _tm: *const TEXTMETRICW, _kind: u32, data: LPARAM) -> i32 {
        // SAFETY: GDI passes a valid LOGFONTW, and our tuple through `data`.
        unsafe {
            let (lf, out) = (&*lf, &mut *(data.0 as *mut (bool, Vec<String>)));
            let end = lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(lf.lfFaceName.len());
            let name = String::from_utf16_lossy(&lf.lfFaceName[..end]);
            if !name.starts_with('@') && (!out.0 || lf.lfPitchAndFamily & 3 == 1) {
                out.1.push(name);
            }
        }
        1
    }
    let mut out: (bool, Vec<String>) = (mono, Vec::new());
    // SAFETY: the screen DC, released below; the callback runs only during the call.
    unsafe {
        let dc = GetDC(None);
        let lf = LOGFONTW { lfCharSet: DEFAULT_CHARSET, ..Default::default() };
        EnumFontFamiliesExW(dc, &lf, Some(each), LPARAM(&raw mut out as isize), 0);
        ReleaseDC(None, dc);
    }
    let mut v = out.1;
    v.sort_by_key(|n| n.to_lowercase());
    v.dedup();
    v
}
