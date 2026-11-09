//! The Linux window: GTK 3 and the system's WebKitGTK (4.1), the same
//! WebKit the Mac uses. The page is unchanged: it posts to
//! `webkit.messageHandlers.kiri`, which WebKitGTK has too, and gets events
//! through `kiri.on(...)`.
//!
//! The window keeps the desktop's own title bar, as on Windows. Closing it
//! quits the window only: the engine is a system service and keeps running.

use std::cell::RefCell;
use std::io;
use std::sync::mpsc;

use gtk::gdk_pixbuf::{Pixbuf, PixbufLoader};
use gtk::glib;
use gtk::prelude::*;
use javascriptcore::ValueExt;
use webkit2gtk::{
    SettingsExt, UserContentInjectedFrames, UserContentManager, UserContentManagerExt, UserScript,
    UserScriptInjectionTime, WebView, WebViewExt,
};

use super::{Cmd, PAGE, engine, prefs};

thread_local! {
    /// The page and its window. Only ever touched on GTK's main thread.
    static WEBVIEW: RefCell<Option<WebView>> = const { RefCell::new(None) };
    static WINDOW: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
}

/// Before the page's own scripts: which system this is.
const SHIM: &str = r#"window.KIRI_OS = "linux";"#;

const ICON_PNG: &[u8] = include_bytes!("../../../../brand/icon/png/kiri-app-icon-512.png");

/// `autostart` starts the engine as soon as the page has loaded; `view` picks
/// the sidebar section to open on.
pub fn run(autostart: bool, view: Option<String>) -> io::Result<()> {
    // The desktop file's name, before GTK starts: on Wayland it's the window's
    // app id, which is how the dock finds Kiri's launcher and icon.
    glib::set_prgname(Some("kiri"));
    gtk::init().map_err(|e| io::Error::other(format!("no display to open the window on ({e})")))?;
    let (tx, rx) = mpsc::channel();
    let engine_tx = tx.clone();
    let engine = std::thread::Builder::new()
        .name("kiri-engine".into())
        .spawn(move || engine(rx, engine_tx))?;

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title(crate::flavor::APP.get());
    if let Some(icon) = icon() {
        window.set_icon(Some(&icon));
    }
    // 1280x800, never more than nine tenths of the screen, as on the Mac and Windows.
    let (mut w, mut h) = (1280, 800);
    if let Some(area) = gtk::gdk::Display::default()
        .and_then(|d| d.primary_monitor().or_else(|| d.monitor(0)))
        .map(|m| m.workarea())
    {
        w = w.min(area.width() * 9 / 10);
        h = h.min(area.height() * 9 / 10);
    }
    window.set_default_size(w, h);
    window.set_size_request(760, 480);
    window.set_position(gtk::WindowPosition::Center);

    let content = UserContentManager::new();
    content.add_script(&UserScript::new(
        SHIM,
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    content.register_script_message_handler("kiri");
    let page_tx = tx.clone();
    content.connect_script_message_received(Some("kiri"), move |_, message| {
        if let Some(text) = message.js_value().map(|v| v.to_str()) {
            super::handle(&text, &page_tx, autostart, view.as_deref());
        }
    });
    let webview = WebView::with_user_content_manager(&content);
    if let Some(settings) = WebViewExt::settings(&webview) {
        settings.set_enable_developer_extras(cfg!(debug_assertions));
        // Drawn on the CPU, not through OpenGL: a page of lists needs no GPU,
        // and WebKit's GL compositing redraws the whole window for every
        // animation, which cost 26% of a core idle where there is no GPU
        // (a VM; software GL), against under 1.5% without it.
        settings.set_hardware_acceleration_policy(webkit2gtk::HardwareAccelerationPolicy::Never);
        settings.set_enable_write_console_messages_to_stdout(cfg!(debug_assertions));
    }
    // The page paints its own background: no white flash before it does.
    webview.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    webview.load_html(&super::assemble::page(PAGE), None);
    window.add(&webview);

    set_appearance(prefs()["theme"].as_str().unwrap_or("system"));
    let quit_tx = tx;
    window.connect_delete_event(move |_, _| {
        let _ = quit_tx.send(Cmd::Quit);
        gtk::main_quit();
        glib::Propagation::Proceed
    });
    window.show_all();
    WEBVIEW.with(|v| *v.borrow_mut() = Some(webview));
    WINDOW.with(|v| *v.borrow_mut() = Some(window));
    gtk::main();
    let _ = engine.join();
    Ok(())
}

fn icon() -> Option<Pixbuf> {
    let loader = PixbufLoader::new();
    loader.write(ICON_PNG).ok()?;
    loader.close().ok()?;
    loader.pixbuf()
}

/// Runs JavaScript in the page. Callable from any thread.
pub(super) fn eval(js: String) {
    glib::MainContext::default().invoke(move || {
        WEBVIEW.with(|v| {
            if let Some(webview) = v.borrow().as_ref() {
                webview.evaluate_javascript(
                    &js,
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            }
        });
    });
}

/// Closes the window, from any thread.
pub(super) fn quit() {
    glib::MainContext::default().invoke(|| {
        WINDOW.with(|v| {
            if let Some(window) = v.borrow().as_ref() {
                window.close();
            }
        });
    });
}

/// The desktop's own title bar moves the window; the page's strip is not a handle here.
pub(super) fn drag() {}

/// A double-click on the page's strip: maximise, or back.
pub(super) fn zoom() {
    glib::MainContext::default().invoke(|| {
        WINDOW.with(|v| {
            if let Some(window) = v.borrow().as_ref() {
                if window.is_maximized() {
                    window.unmaximize()
                } else {
                    window.maximize()
                }
            }
        });
    });
}

/// Settings → Appearance → Theme: GTK's dark variant for the title bar; the
/// page follows through `prefers-color-scheme`. "system" follows the desktop.
pub(super) fn set_appearance(theme: &str) {
    let theme = theme.to_owned();
    glib::MainContext::default().invoke(move || {
        if let Some(settings) = gtk::Settings::default() {
            match theme.as_str() {
                "dark" => settings.set_gtk_application_prefer_dark_theme(true),
                "light" => settings.set_gtk_application_prefer_dark_theme(false),
                _ => settings.set_gtk_application_prefer_dark_theme(desktop_dark()),
            }
        }
    });
}

/// GNOME's (and most desktops') dark preference, through the settings portal's key.
fn desktop_dark() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("dark"))
}

/// The page can't write the clipboard itself.
pub(super) fn copy(text: &str) {
    let text = text.to_owned();
    glib::MainContext::default().invoke(move || {
        let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
        clipboard.set_text(&text);
        // Kept after the window quits, where the desktop has a clipboard manager.
        clipboard.store();
    });
}

/// The clipboard's text, handed to `done` once GTK has it.
pub(super) fn paste(done: impl FnOnce(String) + Send + 'static) {
    glib::MainContext::default().invoke(move || {
        gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD)
            .request_text(move |_, text| done(text.map(|t| t.to_string()).unwrap_or_default()));
    });
}

/// The installed font families, sorted; `mono` keeps the fixed-pitch ones.
pub(super) fn font_families(mono: bool) -> Vec<String> {
    let spacing = if mono { ":spacing=mono" } else { ":" };
    let Ok(out) = std::process::Command::new("fc-list")
        .args([spacing, "family"])
        .output()
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        // "DejaVu Sans,DejaVu Sans Condensed": the first name is the family.
        .filter_map(|l| {
            l.split(',')
                .next()
                .map(str::trim)
                .filter(|n| !n.is_empty() && !n.starts_with('.'))
                .map(str::to_owned)
        })
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}
