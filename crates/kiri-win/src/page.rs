//! Where the window's page comes from on Windows: one fixed address that
//! the webview answers from memory (app.rs's WebResourceRequested handler),
//! so the page can be any size.
//!
//! It used to go in through NavigateToString. Its documentation says 2 MB
//! (2 * 1024 * 1024 bytes); measured on WebView2 157, the string becomes a
//! base64 `data:` address, which may be at most 2,097,152 characters: about
//! 1.57 MB of UTF-8. The assembled page is about 1.5 MB in English and more
//! in most other languages, past that the call failed with E_INVALIDARG,
//! and the window stayed blank (#495). NavigateToString now loads only the
//! small error page (error_page()).
//!
//! The host is `kiri.invalid`. RFC 6761 reserves `.invalid` so that it
//! never resolves, which WebView2's documentation suggests for names an app
//! serves itself: if the handler ever missed a request, the request would
//! fail rather than reach a server. Not `.localhost`, which Chromium sends
//! to the loopback, where any local program could answer, and not a real
//! domain. https makes the page a secure context, as `about:blank` was not.
//! The handler answers only PAGE_URL, with the page, and everything else on
//! the host with 404; nothing is written to disk, and the address exists
//! only inside this webview, so no other program or web page can reach it.

/// The page's address.
pub const PAGE_URL: &str = "https://kiri.invalid/";
/// The WebResourceRequested filter: the whole host, so the handler sees
/// (and refuses) whatever else is asked of it.
pub const FILTER: &str = "https://kiri.invalid/*";
/// The page's headers: UTF-8 HTML, and kept out of the HTTP cache.
pub const PAGE_HEADERS: &str = "Content-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff";
pub const NOT_FOUND_HEADERS: &str = "Cache-Control: no-store";

/// What the handler answers a request for `uri` with.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// 200 with the page.
    Page,
    /// 404, nothing else.
    NotFound,
}

pub fn answer(method: &str, uri: &str) -> Answer {
    if method.eq_ignore_ascii_case("GET") && uri == PAGE_URL { Answer::Page } else { Answer::NotFound }
}

/// Whether the window may go to `uri`. Only the page; and the error page,
/// which NavigateToString starts as `about:blank` or a `data:` address,
/// only when the host itself is showing it (`string_load`). Anything else
/// (a dropped link, a slip in the page) is cancelled.
pub fn may_navigate(uri: &str, string_load: bool) -> bool {
    uri == PAGE_URL || (string_load && (uri.is_empty() || uri == "about:blank" || uri.starts_with("data:")))
}

/// Whether a web message came from the page (and not from the error page
/// or anything else).
pub fn from_page(source: &str) -> bool {
    source == PAGE_URL
}

/// The window's page when Kiri's own could not load: the reason, in a page
/// small enough for NavigateToString (a few hundred bytes and the reason).
pub fn error_page(reason: &str) -> String {
    let esc: String = reason
        .chars()
        .take(2000)
        .map(|c| match c {
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '&' => "&amp;".to_owned(),
            '"' => "&quot;".to_owned(),
            c => c.to_string(),
        })
        .collect();
    format!(
        "<!doctype html><meta charset=utf-8><title>Kiri</title>\
<style>body{{font:14px 'Segoe UI',sans-serif;margin:40px;color:#1b1f24;background:#f5f7fa}}\
@media (prefers-color-scheme:dark){{body{{color:#e6e9ee;background:#0f1419}}}}code{{white-space:pre-wrap}}</style>\
<h1 style=\"font-size:18px\">Kiri's window could not load</h1><p><code>{esc}</code></p>\
<p>This is also in %LOCALAPPDATA%\\Kiri\\kiri-win.log. Quit Kiri from the tray icon and open it again; \
if this comes back, please report it with that log.</p>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_page_address_is_answered_with_the_page() {
        assert_eq!(answer("GET", PAGE_URL), Answer::Page);
        assert_eq!(answer("get", PAGE_URL), Answer::Page);
        for uri in ["https://kiri.invalid/index.html", "https://kiri.invalid/?x=1", "https://kiri.invalid/favicon.ico", "http://kiri.invalid/", "https://kiri.invalid.example/", "https://other.invalid/"] {
            assert_eq!(answer("GET", uri), Answer::NotFound, "{uri}");
        }
        assert_eq!(answer("POST", PAGE_URL), Answer::NotFound);
        assert!(FILTER.starts_with(PAGE_URL) && FILTER.ends_with("/*"));
        assert!(PAGE_HEADERS.contains("text/html; charset=utf-8") && PAGE_HEADERS.contains("no-store") && NOT_FOUND_HEADERS.contains("no-store"));
    }

    #[test]
    fn the_host_never_resolves() {
        // RFC 6761's .invalid: never a real host, never the loopback.
        let host = PAGE_URL.trim_start_matches("https://").trim_end_matches('/');
        assert!(host.ends_with(".invalid"), "{host}");
        assert!(PAGE_URL.starts_with("https://"), "a secure context");
    }

    #[test]
    fn navigation_stays_on_the_page() {
        assert!(may_navigate(PAGE_URL, false));
        for uri in ["https://example.com/", "http://kiri.invalid/", "https://kiri.invalid/x", "file:///C:/Windows/win.ini", "data:text/html,<script>1</script>", "about:blank", "", "javascript:1"] {
            assert!(!may_navigate(uri, false), "{uri}");
        }
        // The error page, shown by the host itself.
        assert!(may_navigate("about:blank", true) && may_navigate("data:text/html;charset=utf-8,x", true));
        assert!(!may_navigate("https://example.com/", true));
        assert!(from_page(PAGE_URL) && !from_page("about:blank") && !from_page("https://example.com/"));
    }

    #[test]
    fn the_error_page_shows_the_reason_and_fits_navigate_to_string() {
        let p = error_page("Navigate failed: <0x80070057> & more");
        assert!(p.contains("&lt;0x80070057&gt; &amp; more"), "{p}");
        assert!(!p.contains("<0x8"));
        // Well under NavigateToString's 2 MB (bytes of UTF-16), whatever the reason.
        assert!(error_page(&"\u{20ac}".repeat(1_000_000)).encode_utf16().count() * 2 < 64 * 1024);
    }
}
