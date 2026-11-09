//! The window's page as every system loads it: index.html with its icons
//! and the terminal emulator (xterm.js, vendored unmodified in `xterm/`)
//! spliced into their placeholders. The Mac and Linux window (crates/kiri)
//! and Windows (kiri-win includes this file) load the same string, and
//! scripts/ui-test.py builds it the same way.
//!
//! The page is loaded as a string, and WebView2's NavigateToString takes at
//! most 2 MB: with xterm.js it is about 1.3 MB.

const ICONS_JS: &str = include_str!("icons.js");
const XTERM_CSS: &str = include_str!("xterm/xterm.css");
/// xterm.js 6.0.0 and its fit, search and web-links add-ons (UMD builds,
/// each defining a global), in the order they load.
const XTERM_JS: [&str; 4] = [
    include_str!("xterm/xterm.js"),
    include_str!("xterm/addon-fit.js"),
    include_str!("xterm/addon-search.js"),
    include_str!("xterm/addon-web-links.js"),
];

/// `index` (index.html) with everything spliced in.
pub fn page(index: &str) -> String {
    let mut js = String::with_capacity(XTERM_JS.iter().map(|s| s.len() + 1).sum());
    for part in XTERM_JS {
        js.push_str(part);
        // Each ends in a source-map comment: the next must start on a line of its own.
        js.push('\n');
    }
    index
        .replacen("/*ICONS*/", ICONS_JS, 1)
        .replacen("/*XTERM_CSS*/", XTERM_CSS, 1)
        .replacen("/*XTERM_JS*/", &js, 1)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_placeholder_is_filled_once_and_the_page_fits_webview2() {
        let page = super::page(include_str!("index.html"));
        for p in ["/*ICONS*/", "/*XTERM_CSS*/", "/*XTERM_JS*/"] {
            assert!(!page.contains(p), "{p} left in the page");
        }
        assert!(page.contains(".xterm-viewport"), "xterm.css missing");
        // NavigateToString's limit is 2 MB; keep a margin for the page to grow.
        assert!(
            page.encode_utf16().count() < 1_800_000,
            "page is {} UTF-16 units",
            page.encode_utf16().count()
        );
    }
}
