//! The Mac window: AppKit and the system WebKit, driven directly through
//! objc2. The page talks to `super::handle` through
//! `webkit.messageHandlers.kiri`.

use std::cell::RefCell;
use std::io;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2::sel;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType, NSMenu, NSMenuDelegate, NSMenuItem,
    NSWindow, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use objc2_web_kit::{WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView, WKWebViewConfiguration};

use super::{Cmd, PAGE, diag, emit, engine, json_str, menu_state, prefs, reopen_after_exit, update_check};

thread_local! {
    /// The page, reachable from closures the engine thread schedules on the
    /// main queue. Only ever touched on the main thread.
    static WEBVIEW: RefCell<Option<Retained<WKWebView>>> = const { RefCell::new(None) };
    /// The window, so the page can ask to be dragged or zoomed.
    static WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    /// The menu bar item; AppKit removes it once it is released.
    static STATUS_ITEM: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
}

/// `autostart` boots the VM as soon as the page has loaded; `view` picks the
/// sidebar section to open on.
pub fn run(autostart: bool, view: Option<String>) -> io::Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| io::Error::other("kiri ui must run on the main thread"))?;
    // An update Later staged that the last quit didn't install: in now, and
    // the new app opens in this one's place.
    if crate::update::staged().is_some() {
        match crate::update::install_staged() {
            Ok(_) => {
                reopen_after_exit();
                return Ok(());
            }
            Err(e) => diag::error!("the staged update did not install", error = e),
        }
    }
    let (tx, rx) = mpsc::channel();
    let engine_tx = tx.clone();
    let engine = std::thread::Builder::new().name("kiri-engine".into()).spawn(move || engine(rx, engine_tx))?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    set_dock_icon(&app);
    let delegate = AppDelegate::new(mtm, tx.clone(), engine);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    set_appearance(prefs()["theme"].as_str().unwrap_or("system"));
    app.setMainMenu(Some(&main_menu(mtm)));
    status_item(mtm, &delegate);
    let window = make_window(mtm, tx, autostart, view)?;
    // SAFETY: the delegate lives as long as the app; it's removed from no centre.
    unsafe {
        let centre: Retained<AnyObject> = msg_send![objc2::class!(NSNotificationCenter), defaultCenter];
        for name in ["NSWindowDidResizeNotification", "NSWindowDidExitFullScreenNotification", "NSWindowDidBecomeKeyNotification", "NSWindowDidResignKeyNotification"] {
            let _: () = msg_send![&*centre, addObserver: &*delegate, selector: sel!(windowLaidOut:), name: &*NSString::from_str(name), object: &*window];
        }
    }
    window.center();
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    app.run();
    Ok(())
}

fn make_window(mtm: MainThreadMarker, tx: Sender<Cmd>, autostart: bool, view: Option<String>) -> io::Result<Retained<NSWindow>> {
    // 1280x800, wide enough for the lists' columns and buttons, and never
    // more than nine tenths of the screen, as on Windows.
    // SAFETY: NSScreen class methods on the main thread; a nil screen gives a zero frame.
    let screen: NSRect = unsafe {
        let main: *mut AnyObject = msg_send![objc2::class!(NSScreen), mainScreen];
        if main.is_null() { NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)) } else { msg_send![main, visibleFrame] }
    };
    let fit = |want: f64, room: f64| if room > 0.0 { want.min(room * 0.9) } else { want };
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(fit(1280.0, screen.size.width), fit(800.0, screen.size.height)));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;
    // SAFETY: standard window construction on the main thread. The window is
    // owned by the returned Retained, so AppKit must not also release it on close.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: plain property setter on a window we own.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(crate::flavor::APP.get()));
    window.setTitlebarAppearsTransparent(true);
    // The page draws its own header; the title only names the window in Mission Control.
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setMinSize(NSSize::new(760.0, 480.0));

    // SAFETY: WebKit objects created and configured on the main thread.
    let webview = unsafe {
        let config = WKWebViewConfiguration::new(mtm);
        let handler = MessageHandler::new(mtm, tx, autostart, view);
        config
            .userContentController()
            .addScriptMessageHandler_name(ProtocolObject::from_ref(&*handler), &NSString::from_str("kiri"));
        let webview = WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config);
        if cfg!(debug_assertions) {
            webview.setInspectable(true);
        }
        // Let the page paint its own background instead of white.
        let no: Retained<AnyObject> = msg_send![objc2::class!(NSNumber), numberWithBool: false];
        let _: () = msg_send![&webview, setValue: &*no, forKey: &*NSString::from_str("drawsBackground")];
        webview.loadHTMLString_baseURL(&NSString::from_str(&super::assemble::page(PAGE)), None);
        webview
    };
    window.setContentView(Some(&webview));
    place_lights(&window);
    WEBVIEW.with(|w| *w.borrow_mut() = Some(webview));
    WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));
    Ok(window)
}

/// The page's strip across the top (index.html's `.strip`), in points.
const STRIP: f64 = 44.0;

/// The traffic lights, moved into the page's strip: 20 points in, centred
/// in its height. AppKit lays them out again when the window resizes or
/// leaves full screen, so the delegate calls this then too.
fn place_lights(window: &NSWindow) {
    // SAFETY: AppKit's own window buttons and their superviews, read and
    // moved on the main thread; every object is retained while it's used.
    unsafe {
        let buttons: Vec<Retained<AnyObject>> = (0usize..3).filter_map(|kind| msg_send![window, standardWindowButton: kind]).collect();
        let Some(close) = buttons.first() else { return };
        let bar: Option<Retained<AnyObject>> = msg_send![close, superview];
        let Some(bar) = bar else { return };
        let container: Option<Retained<AnyObject>> = msg_send![&*bar, superview];
        let Some(container) = container else { return };
        let mut frame: NSRect = msg_send![&*container, frame];
        frame.size.height = STRIP;
        frame.origin.y = window.frame().size.height - STRIP;
        let _: () = msg_send![&*container, setFrame: frame];
        let _: () = msg_send![&*bar, setFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(frame.size.width, STRIP))];
        for (i, b) in buttons.iter().enumerate() {
            let f: NSRect = msg_send![&**b, frame];
            let _: () = msg_send![&**b, setFrameOrigin: NSPoint::new(20.0 + 20.0 * i as f64, ((STRIP - f.size.height) / 2.0).round())];
        }
    }
}

/// The brand icon, built in, so the Dock shows Kiri's logo even when the
/// window is started from a terminal rather than from Kiri.app.
const ICON_PNG: &[u8] = include_bytes!("../../../../brand/icon/png/kiri-app-icon-512.png");

fn set_dock_icon(app: &NSApplication) {
    // Kiri Dev always runs from its bundle, whose icon has the DEV band.
    if crate::flavor::dev() {
        return;
    }
    // SAFETY: NSData copies the bytes; NSImage reads them; both are released by Retained.
    unsafe {
        let data: Retained<AnyObject> =
            msg_send![objc2::class!(NSData), dataWithBytes: ICON_PNG.as_ptr().cast::<std::ffi::c_void>(), length: ICON_PNG.len()];
        let image: Option<Retained<AnyObject>> = msg_send![msg_send![objc2::class!(NSImage), alloc], initWithData: &*data];
        if let Some(image) = image {
            let _: () = msg_send![app, setApplicationIconImage: &*image];
        }
    }
}

const MENUBAR_PNG: &[u8] = include_bytes!("../../../../brand/icon/png/kiri-menubar-template@2x.png");

/// Kiri in the menu bar: a template image (macOS tints it for light and dark
/// menu bars) and a menu the delegate fills each time it opens.
fn status_item(mtm: MainThreadMarker, delegate: &AppDelegate) {
    // SAFETY: AppKit's status bar API on the main thread; every object is
    // retained by Retained, and the item is kept for the app's life.
    unsafe {
        let bar: Retained<AnyObject> = msg_send![objc2::class!(NSStatusBar), systemStatusBar];
        let item: Retained<AnyObject> = msg_send![&*bar, statusItemWithLength: -1.0f64];
        let data: Retained<AnyObject> =
            msg_send![objc2::class!(NSData), dataWithBytes: MENUBAR_PNG.as_ptr().cast::<std::ffi::c_void>(), length: MENUBAR_PNG.len()];
        let image: Option<Retained<AnyObject>> = msg_send![msg_send![objc2::class!(NSImage), alloc], initWithData: &*data];
        let button: Option<Retained<AnyObject>> = msg_send![&*item, button];
        if let (Some(image), Some(button)) = (image, button) {
            let _: () = msg_send![&*image, setSize: NSSize::new(22.0, 18.0)];
            let _: () = msg_send![&*image, setTemplate: true];
            let _: () = msg_send![&*button, setImage: &*image];
            let _: () = msg_send![&*button, setToolTip: &*NSString::from_str(crate::flavor::APP.get())];
        }
        let menu = NSMenu::new(mtm);
        menu.setDelegate(Some(ProtocolObject::from_ref(delegate)));
        let _: () = msg_send![&*item, setMenu: &*menu];
        STATUS_ITEM.with(|s| *s.borrow_mut() = Some(item));
    }
}

/// Settings → Appearance → Theme: Kiri's own macOS appearance, so the
/// window, its title-bar buttons and the page (through
/// `prefers-color-scheme`) switch together. "system" follows macOS.
pub(super) fn set_appearance(theme: &str) {
    let name = match theme {
        "dark" => Some("NSAppearanceNameDarkAqua"),
        "light" => Some("NSAppearanceNameAqua"),
        _ => None,
    };
    // SAFETY: main-thread AppKit calls; a nil appearance means "follow the system".
    unsafe {
        let app: Retained<AnyObject> = msg_send![objc2::class!(NSApplication), sharedApplication];
        let appearance: Option<Retained<AnyObject>> = match name {
            Some(n) => msg_send![objc2::class!(NSAppearance), appearanceNamed: &*NSString::from_str(n)],
            None => None,
        };
        let _: () = msg_send![&*app, setAppearance: appearance.as_deref()];
    }
}

/// Shows the window (after it was closed or hidden) and brings Kiri forward,
/// putting the Dock icon back that closing the window took away.
fn show_window(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Regular);
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            window.makeKeyAndOrderFront(None);
        }
    });
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

/// The app menu, so the standard shortcuts work: ⌘Q quits (the daemon keeps
/// running), ⌘W closes, ⌘H hides, ⌘, opens Settings.
fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let item = |title: &str, action, key: &str| {
        // SAFETY: standard menu item construction on the main thread.
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        }
    };
    let app_menu = NSMenu::new(mtm);
    app_menu.addItem(&item("Check for Updates…", sel!(checkForUpdates:), ""));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item("Settings…", sel!(openSettings:), ","));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item(&format!("Hide {}", crate::flavor::APP), sel!(hide:), "h"));
    // ⌘W asks the page first: in the built-in terminal it closes a pane.
    app_menu.addItem(&item("Close Window", sel!(closeKey:), "w"));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item(&format!("Quit {}", crate::flavor::APP), sel!(terminate:), "q"));
    let app_item = NSMenuItem::new(mtm);
    app_item.setSubmenu(Some(&app_menu));
    // Edit: the standard Cut/Copy/Paste/Select All so ⌘X/C/V/A reach the
    // WebView's focused field (inputs, textareas). Without this menu AppKit
    // never dispatches those key equivalents into WKWebView.
    let edit_menu = NSMenu::new(mtm);
    edit_menu.setTitle(&NSString::from_str("Edit"));
    edit_menu.addItem(&item("Cut", sel!(cut:), "x"));
    edit_menu.addItem(&item("Copy", sel!(copy:), "c"));
    edit_menu.addItem(&item("Paste", sel!(paste:), "v"));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&item("Select All", sel!(selectAll:), "a"));
    let edit_item = NSMenuItem::new(mtm);
    edit_item.setSubmenu(Some(&edit_menu));
    // View: ⌘+ / ⌘− / ⌘0 step the text size Settings → Appearance sets.
    let view_menu = NSMenu::new(mtm);
    view_menu.setTitle(&NSString::from_str("View"));
    view_menu.addItem(&item("Actual Size", sel!(textActualSize:), "0"));
    view_menu.addItem(&item("Zoom In", sel!(textBigger:), "+"));
    view_menu.addItem(&item("Zoom Out", sel!(textSmaller:), "-"));
    let view_item = NSMenuItem::new(mtm);
    view_item.setSubmenu(Some(&view_menu));
    let bar = NSMenu::new(mtm);
    bar.addItem(&app_item);
    bar.addItem(&edit_item);
    bar.addItem(&view_item);
    bar
}

/// Runs JavaScript in the page. Callable from any thread.
pub(super) fn eval(js: String) {
    DispatchQueue::main().exec_async(move || {
        WEBVIEW.with(|w| {
            if let Some(webview) = w.borrow().as_ref() {
                let js = NSString::from_str(&js);
                // SAFETY: on the main thread (main queue), with a live webview.
                unsafe { webview.evaluateJavaScript_completionHandler(&js, None) };
            }
        });
    });
}

/// Quits the app, from any thread.
pub(super) fn quit() {
    DispatchQueue::main().exec_async(|| {
        if let Some(mtm) = MainThreadMarker::new() {
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    });
}

/// The page covers the title bar, and WebKit ignores CSS drag regions, so
/// the page asks for the drag itself on mouse-down over empty chrome. The
/// message lands after the mouse-down: if the button is already up (a quick
/// click), AppKit's current event is the mouse-up, and a drag started then
/// waits for a release that already happened, holding the window until the
/// next click.
pub(super) fn drag() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    WINDOW.with(|w| {
        if let (Some(window), Some(event)) = (w.borrow().as_ref(), NSApplication::sharedApplication(mtm).currentEvent()) {
            if event.r#type() == objc2_app_kit::NSEventType::LeftMouseDown && objc2_app_kit::NSEvent::pressedMouseButtons() & 1 != 0 {
                window.performWindowDragWithEvent(&event);
            }
        }
    });
}

/// Closes the window, as the menu's Close Window did before ⌘W went to the page.
pub(super) fn close_window() {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            window.performClose(None);
        }
    });
}

pub(super) fn zoom() {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            window.zoom(None);
        }
    });
}

/// The page can't write the clipboard itself.
pub(super) fn copy(text: &str) {
    use std::io::Write;
    if let Ok(mut c) = std::process::Command::new("/usr/bin/pbcopy").stdin(std::process::Stdio::piped()).spawn() {
        let _ = c.stdin.take().map(|mut i| i.write_all(text.as_bytes()));
        let _ = c.wait();
    }
}

/// The clipboard's text, handed to `done` off the main thread.
pub(super) fn paste(done: impl FnOnce(String) + Send + 'static) {
    std::thread::spawn(move || {
        if let Ok(out) = std::process::Command::new("/usr/bin/pbpaste").output() {
            done(String::from_utf8_lossy(&out.stdout).into_owned());
        }
    });
}

/// The installed font families, sorted; `mono` keeps the fixed-pitch ones.
pub(super) fn font_families(mono: bool) -> Vec<String> {
    use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString, CFType};
    use objc2_core_text::{CTFontCollection, CTFontDescriptor, CTFontSymbolicTraits, kCTFontFamilyNameAttribute, kCTFontSymbolicTrait, kCTFontTraitsAttribute};
    // Descriptors, not fonts: opening each fixed-pitch font to ask its family
    // took seconds on first use.
    // SAFETY: CoreText font queries are thread-safe, and every value passed
    // is the type its key takes.
    let descriptors = unsafe {
        let all = if mono {
            let traits = CFDictionary::<CFString, CFType>::from_slices(&[kCTFontSymbolicTrait], &[&CFNumber::new_i32(CTFontSymbolicTraits::TraitMonoSpace.0 as i32)]);
            let attrs = CFDictionary::<CFString, CFType>::from_slices(&[kCTFontTraitsAttribute], &[&traits]);
            let query = CTFontDescriptor::with_attributes(attrs.as_opaque());
            CTFontCollection::with_font_descriptors(Some(CFArray::from_objects(&[&*query]).as_opaque()), None)
        } else {
            CTFontCollection::from_available_fonts(None)
        };
        all.matching_font_descriptors()
    };
    let Some(descriptors) = descriptors else { return Vec::new() };
    // SAFETY: the collection's matches are font descriptors.
    let descriptors = unsafe { descriptors.cast_unchecked::<CTFontDescriptor>() };
    let mut names: Vec<String> = descriptors
        .iter()
        // SAFETY: the family name attribute is a CFString.
        .filter_map(|d| unsafe { d.attribute(kCTFontFamilyNameAttribute) })
        .filter_map(|n| n.downcast::<CFString>().ok().map(|n| n.to_string()))
        .collect();
    // Hidden system faces start with a dot.
    names.retain(|n| !n.starts_with('.'));
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}

struct HandlerIvars {
    tx: Sender<Cmd>,
    autostart: bool,
    view: Option<String>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this type has no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KiriMessageHandler"]
    #[ivars = HandlerIvars]
    struct MessageHandler;

    unsafe impl NSObjectProtocol for MessageHandler {}

    unsafe impl WKScriptMessageHandler for MessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            // SAFETY: reading the body of a message WebKit just delivered.
            let body = unsafe { message.body() };
            let Ok(text) = body.downcast::<NSString>() else { return };
            let ivars = self.ivars();
            super::handle(&text.to_string(), &ivars.tx, ivars.autostart, ivars.view.as_deref());
        }
    }
);

impl MessageHandler {
    fn new(mtm: MainThreadMarker, tx: Sender<Cmd>, autostart: bool, view: Option<String>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HandlerIvars { tx, autostart, view });
        // SAFETY: NSObject's init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

struct AppIvars {
    tx: Sender<Cmd>,
    engine: RefCell<Option<JoinHandle<()>>>,
    /// The containers the menu bar menu lists, by item tag.
    shown: RefCell<Vec<(String, Option<String>)>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this type has no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KiriAppDelegate"]
    #[ivars = AppIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        /// Closing the window leaves Kiri in the menu bar only, as the tray
        /// does on Windows: the Dock icon goes until the window opens again.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window(&self, app: &NSApplication) -> bool {
            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            false
        }

        /// Clicking the Dock icon brings the window back.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            show_window(self.mtm());
            true
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _note: &NSNotification) {
            // The daemon keeps running (and sleeps when idle); only the
            // engine thread stops.
            let _ = self.ivars().tx.send(Cmd::Quit);
            if let Some(engine) = self.ivars().engine.borrow_mut().take() {
                let _ = engine.join();
            }
            // Later: a downloaded update goes in as the window quits. The
            // engine moves to it the next time it starts.
            if crate::update::staged().is_some()
                && let Err(e) = crate::update::install_staged()
            {
                diag::error!("the update did not install", error = e);
            }
        }
    }

    unsafe impl NSMenuDelegate for AppDelegate {
        /// The menu bar menu, rebuilt each time it opens.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.fill_menu(menu);
        }
    }

    impl AppDelegate {
        /// The window resized, left full screen or changed focus: AppKit put
        /// the traffic lights back, so they go into the strip again.
        #[unsafe(method(windowLaidOut:))]
        fn window_laid_out(&self, _note: &NSNotification) {
            WINDOW.with(|w| {
                if let Some(w) = w.borrow().as_ref() {
                    place_lights(w);
                }
            });
        }

        #[unsafe(method(openWindow:))]
        fn open_window(&self, _sender: Option<&AnyObject>) {
            show_window(self.mtm());
        }

        /// Settings… (⌘,), in the app menu and the menu bar icon's menu.
        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            show_window(self.mtm());
            emit(r#"{"type":"view","view":"settings"}"#.to_owned());
        }

        /// Kiri → Check for Updates…: asks now, and shows the card again
        /// even for a version that was skipped or snoozed.
        #[unsafe(method(checkForUpdates:))]
        fn check_for_updates(&self, _sender: Option<&AnyObject>) {
            show_window(self.mtm());
            std::thread::spawn(|| update_check(true));
        }

        /// ⌘W: the page closes the terminal's pane when it has the focus,
        /// and otherwise asks for the window to close (`close_window`).
        #[unsafe(method(closeKey:))]
        fn close_key(&self, _sender: Option<&AnyObject>) {
            emit(r#"{"type":"closekey"}"#.to_owned());
        }

        #[unsafe(method(textBigger:))]
        fn text_bigger(&self, _sender: Option<&AnyObject>) {
            emit(r#"{"type":"textsize","step":1}"#.to_owned());
        }

        #[unsafe(method(textSmaller:))]
        fn text_smaller(&self, _sender: Option<&AnyObject>) {
            emit(r#"{"type":"textsize","step":-1}"#.to_owned());
        }

        #[unsafe(method(textActualSize:))]
        fn text_actual_size(&self, _sender: Option<&AnyObject>) {
            emit(r#"{"type":"textsize","step":0}"#.to_owned());
        }

        #[unsafe(method(toggleVm:))]
        fn toggle_vm(&self, _sender: Option<&AnyObject>) {
            let running = menu_state().vm == "Running";
            let _ = self.ivars().tx.send(if running { Cmd::Stop } else { Cmd::Start });
        }

        /// A running container: its local address in the browser, or its
        /// page in the window when it has no web port.
        #[unsafe(method(openContainer:))]
        fn open_container(&self, sender: Option<&AnyObject>) {
            // SAFETY: the sender is the NSMenuItem the action came from.
            let tag: isize = sender.map_or(-1, |s| unsafe { msg_send![s, tag] });
            let Some((id, url)) = usize::try_from(tag).ok().and_then(|i| self.ivars().shown.borrow().get(i).cloned()) else { return };
            match url {
                Some(url) => {
                    let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
                }
                None => {
                    show_window(self.mtm());
                    let mut ev = String::from(r#"{"type":"detail","id":"#);
                    json_str(&mut ev, &id);
                    ev.push('}');
                    emit(ev);
                }
            }
        }
    }
);

impl AppDelegate {
    fn fill_menu(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        menu.removeAllItems();
        let state = menu_state();
        let add = |title: &str, action: Option<objc2::runtime::Sel>, key: &str, tag: isize| {
            // SAFETY: standard menu item construction on the main thread; the
            // target is this delegate, which lives for the app's life.
            unsafe {
                let item = NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), action, &NSString::from_str(key));
                if action.is_some() {
                    item.setTarget(Some(self));
                } else {
                    item.setEnabled(false);
                }
                item.setTag(tag);
                menu.addItem(&item);
            }
        };
        add(&format!("Linux VM: {}", state.vm), None, "", 0);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let mut shown = self.ivars().shown.borrow_mut();
        shown.clear();
        if state.running.is_empty() {
            add("No containers running", None, "", 0);
        }
        for (i, (id, name, url)) in state.running.iter().take(12).enumerate() {
            let title = match url {
                Some(u) => format!("{name}  ·  {}", u.trim_start_matches("http://")),
                None => name.clone(),
            };
            add(&title, Some(sel!(openContainer:)), "", i as isize);
            shown.push((id.clone(), url.clone()));
        }
        if state.running.len() > 12 {
            add(&format!("and {} more…", state.running.len() - 12), Some(sel!(openWindow:)), "", 0);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        add("Open Kiri", Some(sel!(openWindow:)), "o", 0);
        add("Settings…", Some(sel!(openSettings:)), ",", 0);
        add(if state.vm == "Running" { "Stop Linux VM" } else { "Start Linux VM" }, Some(sel!(toggleVm:)), "", 0);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        // SAFETY: standard menu item construction; terminate: goes to the app.
        let quit = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str("Quit Kiri"), Some(sel!(terminate:)), &NSString::from_str("q")) };
        menu.addItem(&quit);
    }
}

impl AppDelegate {
    fn new(mtm: MainThreadMarker, tx: Sender<Cmd>, engine: JoinHandle<()>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppIvars { tx, engine: RefCell::new(Some(engine)), shown: RefCell::new(Vec::new()) });
        // SAFETY: NSObject's init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}
