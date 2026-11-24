//! What the built-in terminal says between the page and the app, the same on
//! every system: the Mac and Linux run shells on a pseudo-terminal
//! (ui/term.rs), Windows on a ConPTY (kiri-win's term.rs, which includes
//! this file).
//!
//! The page sends `{"cmd":"term","name":ACTION,"id":SESSION,"value":…}`:
//! `shells`; `open` with `{shell, cols, rows}` and optional `cwd` (an existing
//! directory; else the home folder) and optional `env` (a small allow-listed
//! map for project terminals, e.g. `DATA_ROOT` / `PROJECT_SLUG`); `input` with
//! the bytes typed (base64); `resize` with `{cols, rows}`; `close`; `closeall`
//! when the page starts; `paste`; and `copy` with the text. The app answers
//! with events of type `term`: `shells`, `data`
//! (output, base64), `exit`, `paste`.
//!
//! Output and input go as base64: a read can end in the middle of a UTF-8
//! character, and programs send bytes that aren't text at all.

use std::path::PathBuf;

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

/// Optional working directory from `{cwd}`: must already exist as a directory.
/// Canonicalized so the page can't point at a missing or non-dir path.
/// On Windows, strips the `\\?\` verbatim prefix — otherwise PowerShell's
/// prompt becomes `Microsoft.PowerShell.Core\FileSystem::\\?\D:\…`.
pub fn cwd(v: &Value) -> Option<PathBuf> {
    let s = v.get("cwd").and_then(|x| x.as_str())?.trim();
    if s.is_empty() || s.len() > 4096 {
        return None;
    }
    let p = PathBuf::from(s);
    std::fs::canonicalize(&p).ok().filter(|c| c.is_dir()).map(plain_path)
}

/// Keys the page may set on `open` via `{env: {…}}`. Anything else is dropped
/// so a tab can't rewrite `PATH` / `LD_PRELOAD` / credentials.
const ENV_ALLOW: &[&str] = &[
    "DATA_ROOT",
    "DATA_ROOT_DOCKER",
    "PROJECT_SLUG",
    "COMPOSE_PROJECT_NAME",
    "KIRI_PROJECT_ID",
    "KIRI_PROJECT_CODE",
    "KIRI_PROJECTS_ROOT",
];

/// Optional environment from `{env}`: only allow-listed keys, no NULs, capped
/// length. Empty when absent or invalid.
pub fn env(v: &Value) -> Vec<(String, String)> {
    let Some(obj) = v.get("env").and_then(|x| x.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, val) in obj {
        if !ENV_ALLOW.iter().any(|a| a == k) {
            continue;
        }
        let Some(s) = val.as_str() else { continue };
        if s.is_empty() || s.len() > 1024 || s.contains('\0') {
            continue;
        }
        out.push((k.clone(), s.to_owned()));
    }
    out
}

/// Windows' canonical form `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share\x` →
/// `\\server\share\x`. Other platforms leave the path as-is.
fn plain_path(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            return PathBuf::from(rest);
        }
    }
    p
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

    #[test]
    fn cwd_only_takes_a_real_directory() {
        let dir = std::env::temp_dir();
        let got = cwd(&json!({"cwd": dir.to_string_lossy()}));
        assert!(got.as_ref().is_some_and(|p| p.is_dir()));
        // Windows canonicalize yields \\?\… — the shell must not see that.
        if let Some(p) = &got {
            let s = p.to_string_lossy();
            assert!(!s.starts_with(r"\\?\"), "{s}");
        }
        assert!(cwd(&json!({"cwd": "/no/such/kiri/dir/ever"})).is_none());
        assert!(cwd(&json!({})).is_none());
    }

    #[test]
    fn env_only_allows_project_keys() {
        let got = env(&json!({
            "env": {
                "DATA_ROOT": "/数字档案/data",
                "PROJECT_SLUG": "数字档案",
                "PATH": "/evil",
                "LD_PRELOAD": "x"
            }
        }));
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|(k, _)| ENV_ALLOW.contains(&k.as_str())));
        assert!(got.iter().any(|(k, v)| k == "DATA_ROOT" && v == "/数字档案/data"));
        assert!(got.iter().any(|(k, v)| k == "PROJECT_SLUG" && v == "数字档案"));
        assert!(env(&json!({})).is_empty());
        assert!(env(&json!({"env": {"DATA_ROOT": ""}})).is_empty());
    }

    #[test]
    fn plain_path_strips_windows_verbatim_prefix() {
        assert_eq!(plain_path(PathBuf::from(r"\\?\C:\Users\a")), PathBuf::from(r"C:\Users\a"));
        assert_eq!(plain_path(PathBuf::from(r"\\?\UNC\server\share\x")), PathBuf::from(r"\\server\share\x"));
        assert_eq!(plain_path(PathBuf::from(r"C:\Users\a")), PathBuf::from(r"C:\Users\a"));
        assert_eq!(plain_path(PathBuf::from("/tmp/x")), PathBuf::from("/tmp/x"));
    }
}
