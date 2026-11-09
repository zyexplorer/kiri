//! What the built-in terminal says between the page and the app, the same on
//! every system: the Mac and Linux run shells on a pseudo-terminal
//! (ui/term.rs), Windows on a ConPTY (kiri-win's term.rs, which includes
//! this file).
//!
//! The page sends `{"cmd":"term","name":ACTION,"id":SESSION,"value":…}`:
//! `shells`; `open` with `{shell, cols, rows}`; `input` with the bytes typed
//! (base64); `resize` with `{cols, rows}`; `close`; `closeall` when the page
//! starts; `paste`; and `copy` with the text. The app answers with events of
//! type `term`: `shells`, `data` (output, base64), `exit`, `paste`.
//!
//! Output and input go as base64: a read can end in the middle of a UTF-8
//! character, and programs send bytes that aren't text at all.

use serde_json::{Value, json};

/// A shell the terminal can start: `id` is what the page asks for, so a
/// page can only start a shell from this list, never a program it names.
pub struct Shell {
    pub id: String,
    pub label: String,
    /// What the new-tab menu groups it under ("" for none).
    pub group: &'static str,
}

/// A session's name from the page: short, letters and digits only.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 16 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Columns and rows from `{cols, rows}`, kept to what a terminal can be.
pub fn size(v: &Value) -> (u16, u16) {
    let get = |k: &str, d: u64| v[k].as_u64().unwrap_or(d).clamp(2, 1000) as u16;
    (get("cols", 80), get("rows", 24))
}

pub fn shells(list: &[Shell], default: &str) -> String {
    let list: Vec<Value> = list
        .iter()
        .map(|s| json!({"id": s.id, "label": s.label, "group": s.group}))
        .collect();
    json!({"type": "term", "ev": "shells", "shells": list, "default": default}).to_string()
}

pub fn data(id: &str, bytes: &[u8]) -> String {
    json!({"type": "term", "ev": "data", "id": id, "b64": b64(bytes)}).to_string()
}

/// The session ended: `code` when the shell exited by itself, `error` when
/// it couldn't start.
pub fn exit(id: &str, code: Option<i32>, error: Option<&str>) -> String {
    json!({"type": "term", "ev": "exit", "id": id, "code": code, "error": error}).to_string()
}

pub fn paste(id: &str, text: &str) -> String {
    json!({"type": "term", "ev": "paste", "id": id, "text": text}).to_string()
}

/// A link the terminal may open: http or https with a host, and nothing
/// that would read as more than one address.
pub fn web_link(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
        && url.len() <= 2048
        && url.chars().all(|c| c.is_ascii_graphic())
}

/// The most a copy may put on the clipboard: a whole screen of scrollback.
pub const COPY_MAX: usize = 4 << 20;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() {
                B64[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=').as_bytes();
    let val = |b: u8| B64.iter().position(|&x| x == b).map(|p| p as u32);
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.chunks(4) {
        if c.len() == 1 {
            return None;
        }
        let mut n = 0;
        for (i, &b) in c.iter().enumerate() {
            n |= val(b)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if c.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if c.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_goes_both_ways_at_every_length() {
        for n in 0..70u8 {
            let bytes: Vec<u8> = (0..n)
                .map(|i| i.wrapping_mul(37).wrapping_add(200))
                .collect();
            assert_eq!(
                unb64(&b64(&bytes)).as_deref(),
                Some(&bytes[..]),
                "{n} bytes"
            );
        }
        assert_eq!(b64(b"Kiri"), "S2lyaQ==");
        assert_eq!(unb64("not base64!"), None);
    }

    #[test]
    fn a_page_cant_name_a_session_that_isnt_plain() {
        assert!(valid_id("t12"));
        assert!(
            !valid_id("") && !valid_id("../x") && !valid_id("a b") && !valid_id(&"a".repeat(17))
        );
    }

    #[test]
    fn only_web_links_open() {
        assert!(web_link("https://example.com/a?b=c") && web_link("http://localhost:3000"));
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://",
            "http:///x",
            "https://a b",
            "vscode://x",
            "ssh://h",
        ] {
            assert!(!web_link(bad), "{bad}");
        }
    }

    #[test]
    fn sizes_are_kept_sane() {
        assert_eq!(size(&json!({"cols": 120, "rows": 40})), (120, 40));
        assert_eq!(size(&json!({"cols": 0, "rows": 99999})), (2, 1000));
        assert_eq!(size(&json!({})), (80, 24));
    }
}
