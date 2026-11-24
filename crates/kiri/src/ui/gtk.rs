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
    webview.load_html(&super::assemble::page(PAGE, super::prefs()["lang"].as_str()), None);
    window.add(&webview);
    // Before set_appearance, which gives it its background.
    WEBVIEW.with(|v| *v.borrow_mut() = Some(webview));

    set_appearance(prefs()["theme"].as_str().unwrap_or("system"));
    let quit_tx = tx;
    window.connect_delete_event(move |_, _| {
        let _ = quit_tx.send(Cmd::Quit);
        gtk::main_quit();
        glib::Propagation::Proceed
    });
    window.show_all();
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
/// The language setting changed: the page redraws its own words, and the
/// GTK window draws none of its own.
pub(super) fn relang() {}

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
        let dark = match theme.as_str() {
            "dark" => true,
            "light" => false,
            _ => desktop_dark(),
        };
        if let Some(settings) = gtk::Settings::default() {
            settings.set_gtk_application_prefer_dark_theme(dark);
        }
        WEBVIEW.with(|v| {
            if let Some(webview) = v.borrow().as_ref() {
                webview.set_background_color(&page_background(dark));
            }
        });
    });
}

/// The view's background under the page: index.html's `--bg` for the scheme,
/// so there is no white flash before the page paints. Opaque on purpose: with
/// a transparent view and no GPU compositing (see `run`), WebKitGTK 2.54 keeps
/// only the regions repainted in each frame, and the rest of the window
/// flickers through to whatever is behind it.
fn page_background(dark: bool) -> gtk::gdk::RGBA {
    let (r, g, b) = if dark { (0x0F, 0x14, 0x19) } else { (0xF5, 0xF7, 0xFA) };
    gtk::gdk::RGBA::new(f64::from(r) / 255.0, f64::from(g) / 255.0, f64::from(b) / 255.0, 1.0)
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

/// Open a compose file…: GTK's own file chooser (the desktop's portal when
/// there is one), on GTK's main thread. Compose's file names first, any
/// YAML file second. `done` gets the path, or None when cancelled.
pub(super) fn pick_compose_file(title: &str, filters: &[String; 2], done: impl FnOnce(Option<std::path::PathBuf>) + Send + 'static) {
    let (title, filters) = (title.to_owned(), filters.clone());
    glib::MainContext::default().invoke(move || {
        let parent = WINDOW.with(|v| v.borrow().clone());
        let dialog = gtk::FileChooserNative::new(Some(&title), parent.as_ref(), gtk::FileChooserAction::Open, None, None);
        let names = gtk::FileFilter::new();
        names.set_name(Some(&filters[0]));
        for p in super::compose::FILE_NAMES {
            names.add_pattern(p);
        }
        let yaml = gtk::FileFilter::new();
        yaml.set_name(Some(&filters[1]));
        yaml.add_pattern("*.yaml");
        yaml.add_pattern("*.yml");
        dialog.add_filter(names);
        dialog.add_filter(yaml);
        let picked = if dialog.run() == gtk::ResponseType::Accept { dialog.filename() } else { None };
        dialog.destroy();
        done(picked);
    });
}

pub(super) fn pick_project_dir(title: &str, done: impl FnOnce(Option<std::path::PathBuf>) + Send + 'static) {
    let title = title.to_owned();
    glib::MainContext::default().invoke(move || {
        let parent = WINDOW.with(|v| v.borrow().clone());
        let dialog = gtk::FileChooserNative::new(Some(&title), parent.as_ref(), gtk::FileChooserAction::SelectFolder, None, None);
        let picked = if dialog.run() == gtk::ResponseType::Accept { dialog.filename() } else { None };
        dialog.destroy();
        done(picked);
    });
}

pub(super) fn pick_project_files(title: &str, done: impl FnOnce(Vec<std::path::PathBuf>) + Send + 'static) {
    let title = title.to_owned();
    glib::MainContext::default().invoke(move || {
        let parent = WINDOW.with(|v| v.borrow().clone());
        let dialog = gtk::FileChooserNative::new(Some(&title), parent.as_ref(), gtk::FileChooserAction::Open, None, None);
        dialog.set_select_multiple(true);
        let picked = if dialog.run() == gtk::ResponseType::Accept {
            dialog.filenames().into_iter().collect()
        } else {
            Vec::new()
        };
        dialog.destroy();
        done(picked);
    });
}

pub(super) fn pick_kit_file(title: &str, done: impl FnOnce(Option<std::path::PathBuf>) + Send + 'static) {
    let title = title.to_owned();
    glib::MainContext::default().invoke(move || {
        let parent = WINDOW.with(|v| v.borrow().clone());
        let dialog = gtk::FileChooserNative::new(Some(&title), parent.as_ref(), gtk::FileChooserAction::Open, None, None);
        let kit = gtk::FileFilter::new();
        kit.set_name(Some("Kiri kits"));
        kit.add_pattern("*.kirikit");
        kit.add_pattern("*.zip");
        dialog.add_filter(kit);
        let picked = if dialog.run() == gtk::ResponseType::Accept { dialog.filename() } else { None };
        dialog.destroy();
        done(picked);
    });
}

pub(super) fn pick_kit_save(title: &str, suggested: &str, done: impl FnOnce(Option<std::path::PathBuf>) + Send + 'static) {
    pick_save(title, suggested, "Kiri kits", "*.kirikit", done);
}

pub(super) fn pick_tar_save(title: &str, suggested: &str, done: impl FnOnce(Option<std::path::PathBuf>) + Send + 'static) {
    pick_save(title, suggested, "Tar archive", "*.tar", move |path| {
        done(path.map(|p| {
            if p.extension().and_then(|e| e.to_str()) == Some("tar") {
                p
            } else {
                p.with_extension("tar")
            }
        }));
    });
}

fn pick_save(
    title: &str,
    suggested: &str,
    filter_name: &str,
    pattern: &str,
    done: impl FnOnce(Option<std::path::PathBuf>) + Send + 'static,
) {
    let (title, suggested, filter_name, pattern) = (
        title.to_owned(),
        suggested.to_owned(),
        filter_name.to_owned(),
        pattern.to_owned(),
    );
    glib::MainContext::default().invoke(move || {
        let parent = WINDOW.with(|v| v.borrow().clone());
        let dialog = gtk::FileChooserNative::new(Some(&title), parent.as_ref(), gtk::FileChooserAction::Save, None, None);
        dialog.set_current_name(&suggested);
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(&filter_name));
        filter.add_pattern(&pattern);
        dialog.add_filter(filter);
        let picked = if dialog.run() == gtk::ResponseType::Accept { dialog.filename() } else { None };
        dialog.destroy();
        done(picked);
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
