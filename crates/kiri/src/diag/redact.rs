//! Takes out of logs and reports what must not leave the computer: the home
//! folder's path (it names the person), in every form Windows writes it,
//! bearer and other `Authorization` values, the values of token, password,
//! secret and cookie keys in JSON, query strings, `KEY=value`, `key: value`
//! and `key = value` pairs and `--flag value`, passwords in URLs, private
//! keys, JWTs, and tokens that say what they are (`ks_…`, `ghp_…`,
//! `sk_live_…`, `xoxb-…`, `sk-ant-…`).
//!
//! Patterns, not parsing: it runs over any text (log lines, panic
//! messages, macOS crash reports), so it errs towards taking out too much.

const MARK: &str = "[redacted]";

#[derive(Debug, Default)]
pub struct Redactor {
    /// The home folder as it may be written, and whether to match it
    /// ignoring case (Windows paths).
    homes: Vec<(String, bool)>,
    /// Exact strings to take out wherever they appear (the signed-in token).
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new(home: Option<String>, secrets: Vec<String>) -> Redactor {
        let homes = home.map(|h| home_forms(&h)).unwrap_or_default();
        Redactor {
            homes,
            secrets: secrets.into_iter().filter(|s| s.len() >= 8).collect(),
        }
    }

    /// `$HOME` → `~`, and the patterns.
    pub fn home_only() -> Redactor {
        Redactor::new(std::env::var("HOME").ok(), Vec::new())
    }

    /// For a report: also the Kiri Cloud token, when signed in.
    pub fn for_user() -> Redactor {
        Redactor::new(
            std::env::var("HOME").ok(),
            crate::share::read_config().token.into_iter().collect(),
        )
    }

    pub fn apply(&self, text: &str) -> String {
        let mut s = text.to_owned();
        for secret in &self.secrets {
            s = s.replace(secret.as_str(), MARK);
        }
        for (home, any_case) in &self.homes {
            s = replace_home(&s, home, *any_case);
        }
        patterns(&s)
    }
}

/// The patterns alone (no home, no known secrets): cheap enough for every
/// log line.
pub fn patterns(s: &str) -> String {
    let s = private_keys(s);
    let s = url_userinfo(&s);
    let s = bearer(&s);
    let s = authorization(&s);
    let s = json_values(&s, "\"");
    // The same inside a JSON string: `{\"token\":\"…\"}`.
    let s = json_values(&s, "\\\"");
    let s = assignments(&s);
    let s = spaced_values(&s);
    let s = flags(&s);
    let s = jwts(&s);
    let s = emails(&s);
    TOKEN_PREFIXES
        .iter()
        .fold(s, |s, prefix| prefixed_tokens(&s, prefix))
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A key whose value is secret: `token`, `access_token`, `PASSWORD`,
/// `client_secret`, `API-KEY`, `PGPASS`, docker's `auth`, `identitytoken`…
fn sensitive(key: &str) -> bool {
    let k = key
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace(['-', '.'], "_");
    matches!(k.as_str(), "auth" | "authorization" | "pass" | "pgpass")
        || k.ends_with("_pass")
        || [
            "token",
            "password",
            "passwd",
            "passphrase",
            "secret",
            "apikey",
            "api_key",
            "access_key",
            "private_key",
            "credential",
            "crashreporterkey",
            "cookie",
            "session",
        ]
        .iter()
        .any(|w| k.contains(w))
}

/// The home folder as logs may write it. A Windows one (`C:\Users\Ada`) is
/// also matched doubled inside JSON (`C:\\Users\\Ada`), with forward
/// slashes, as WSL mounts it (`/mnt/c/Users/Ada`) and as the WSL home of the
/// same name (`/home/ada`), all ignoring case, as Windows does.
fn home_forms(home: &str) -> Vec<(String, bool)> {
    let home = home.trim_end_matches(['/', '\\']);
    let b = home.as_bytes();
    if b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        let parts: Vec<&str> = home[2..]
            .split(['\\', '/'])
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            return Vec::new();
        }
        let drive = &home[..2];
        let mut forms = vec![
            format!("{drive}\\\\{}", parts.join("\\\\")),
            format!("{drive}\\{}", parts.join("\\")),
            format!("{drive}/{}", parts.join("/")),
            format!(
                "/mnt/{}/{}",
                drive[..1].to_ascii_lowercase(),
                parts.join("/")
            ),
        ];
        if let Some(user) = parts.last().filter(|u| u.len() > 1) {
            forms.push(format!("/home/{user}"));
        }
        return forms.into_iter().map(|f| (f, true)).collect();
    }
    if home.starts_with('/') && home.len() > 1 {
        vec![(home.to_owned(), false)]
    } else {
        Vec::new()
    }
}

/// A PEM private key, whole; without its end line, everything after its
/// start, since the rest of the key may be in there.
fn private_keys(s: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const DASHES: &str = "-----";
    let (mut out, mut rest) = (String::with_capacity(s.len()), s);
    while let Some(i) = rest.find(BEGIN) {
        let after = &rest[i + BEGIN.len()..];
        let Some(label_len) = after.find(DASHES) else {
            break;
        };
        let label = &after[..label_len];
        if !label.ends_with("PRIVATE KEY") {
            out.push_str(&rest[..i + BEGIN.len()]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..i]);
        out.push_str(MARK);
        let body = &after[label_len + DASHES.len()..];
        let end = format!("-----END {label}-----");
        rest = body.find(&end).map_or("", |n| &body[n + end.len()..]);
    }
    out.push_str(rest);
    out
}

/// `postgres://app:hunter2@db/x` → `postgres://[redacted]@db/x`: all of the
/// part before the `@`, since a token can stand where the user name goes.
fn url_userinfo(s: &str) -> String {
    let (mut out, mut rest) = (String::with_capacity(s.len()), s);
    while let Some(i) = rest.find("://") {
        let start = i + 3;
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let end = after
            .find(|c: char| c.is_whitespace() || "/?#\"'<>\\".contains(c))
            .unwrap_or(after.len());
        match after[..end].rfind('@') {
            Some(at) if &after[..at] != MARK => {
                out.push_str(MARK);
                rest = &after[at..];
            }
            _ => rest = after,
        }
    }
    out.push_str(rest);
    out
}

/// `Authorization: Basic abc` → `Authorization: Basic [redacted]`, whatever
/// the scheme, and the value alone when it has none. `Basic` isn't matched
/// by itself as `Bearer` is: "basic authentication" is ordinary text.
fn authorization(s: &str) -> String {
    const KEY: &str = "authorization:";
    let lower = s.to_ascii_lowercase();
    let b = s.as_bytes();
    let skip = |mut j: usize| {
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        j
    };
    let word = |mut j: usize| {
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b"-._~+/=".contains(&b[j])) {
            j += 1;
        }
        j
    };
    let (mut out, mut last, mut from) = (String::with_capacity(s.len()), 0, 0);
    while let Some(i) = lower[from..].find(KEY).map(|i| i + from) {
        from = i + KEY.len();
        let first = skip(from);
        let first_end = word(first);
        let second = skip(first_end);
        let second_end = word(second);
        if s[second..].starts_with(MARK) {
            continue;
        }
        let (start, end) = if second_end > second {
            (second, second_end)
        } else {
            (first, first_end)
        };
        if end > start {
            out.push_str(&s[last..start]);
            out.push_str(MARK);
            last = end;
            from = end;
        }
    }
    out.push_str(&s[last..]);
    out
}

/// `/Users/ada/x` → `~/x`, but not `/Users/adam`. `any_case` matches
/// `c:\users\ADA` too.
fn replace_home(s: &str, home: &str, any_case: bool) -> String {
    // ASCII lower-casing keeps every byte where it was.
    let (hay, needle) = if any_case {
        (s.to_ascii_lowercase(), home.to_ascii_lowercase())
    } else {
        (s.to_owned(), home.to_owned())
    };
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(i) = hay[from..].find(&needle).map(|i| i + from) {
        let end = i + needle.len();
        let whole = s
            .as_bytes()
            .get(end)
            .is_none_or(|&b| !(is_word(b) || b == b'-' || b == b'.'));
        if whole {
            out.push_str(&s[last..i]);
            out.push('~');
            last = end;
        }
        from = end;
    }
    out.push_str(&s[last..]);
    out
}

/// `Bearer <token>` (any case) → `Bearer [redacted]`.
fn bearer(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let b = s.as_bytes();
    let token_byte = |c: u8| c.is_ascii_alphanumeric() || b"-._~+/=".contains(&c);
    let (mut out, mut last, mut from) = (String::with_capacity(s.len()), 0, 0);
    while let Some(i) = lower[from..].find("bearer").map(|i| i + from) {
        let mut j = i + 6;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        let start = j;
        while j < b.len() && token_byte(b[j]) {
            j += 1;
        }
        if (i == 0 || !is_word(b[i - 1])) && start > i + 6 && j - start >= 8 {
            out.push_str(&s[last..start]);
            out.push_str(MARK);
            last = j;
        }
        from = j.max(i + 6);
    }
    out.push_str(&s[last..]);
    out
}

/// The end of a string value starting at `s[0]`: the next `q` (for a plain
/// quote, one not escaped).
fn close(s: &str, q: &str) -> Option<usize> {
    if q != "\"" {
        return s.find(q);
    }
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// `"token": "…"` → `"token": "[redacted]"`, with `q` as the quote.
fn json_values(s: &str, q: &str) -> String {
    let (mut out, mut last, mut from) = (String::with_capacity(s.len()), 0, 0);
    while let Some(i) = s[from..].find(q).map(|i| i + from) {
        let key_start = i + q.len();
        let Some(key_len) = s[key_start..].find(q) else {
            break;
        };
        let key = &s[key_start..key_start + key_len];
        let mut j = key_start + key_len + q.len();
        let skip = |j: usize| j + (s[j..].len() - s[j..].trim_start().len());
        j = skip(j);
        if !key.is_empty()
            && key.len() <= 64
            && !key.contains(['\\', '"'])
            && s[j..].starts_with(':')
        {
            j = skip(j + 1);
            if sensitive(key) && s[j..].starts_with(q) {
                let start = j + q.len();
                if let Some(len) = close(&s[start..], q) {
                    out.push_str(&s[last..start]);
                    out.push_str(MARK);
                    last = start + len;
                    from = last + q.len();
                    continue;
                }
            }
            from = j;
            continue;
        }
        // The closing quote may open the next key.
        from = key_start + key_len;
    }
    out.push_str(&s[last..]);
    out
}

/// `?token=…&`, `PASSWORD=…`, `--password=…`: the value after a sensitive key
/// and `=`. [`flags`] has `--password …`.
fn assignments(s: &str) -> String {
    let b = s.as_bytes();
    let key_byte = |c: u8| is_word(c) || c == b'-' || c == b'.';
    let (mut out, mut last) = (String::with_capacity(s.len()), 0);
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'=' || b.get(i + 1) == Some(&b'=') || i < last {
            i += 1;
            continue;
        }
        let mut k = i;
        while k > 0 && key_byte(b[k - 1]) {
            k -= 1;
        }
        let start = i + 1;
        let end = match b.get(start) {
            Some(&qc @ (b'"' | b'\'')) => s[start + 1..]
                .find(qc as char)
                .map_or(b.len(), |n| start + 1 + n + 1),
            _ => s[start..]
                .find(|c: char| c.is_whitespace() || "&\"',;)#<>".contains(c))
                .map_or(b.len(), |n| start + n),
        };
        if k < i && sensitive(&s[k..i]) && end > start && &s[start..end] != MARK {
            let quoted = matches!(b[start], b'"' | b'\'');
            out.push_str(&s[last..start]);
            if quoted {
                out.push(b[start] as char);
                out.push_str(MARK);
                out.push(b[start] as char);
            } else {
                out.push_str(MARK);
            }
            last = end;
            i = end;
        } else {
            // Not a secret: the value may hold one (`env=[DB_PASSWORD=x`).
            i += 1;
        }
    }
    out.push_str(&s[last..]);
    out
}

/// `password: hunter2`, `token = "abc"`, `aws_secret_access_key = …`,
/// `Cookie: a=b; c=d`: the value after a sensitive key, a `:` or an `=`
/// with spaces around it. A cookie header loses the rest of its line.
/// `Authorization` has its own pass, and `key=value` with no spaces is
/// [`assignments`]'.
fn spaced_values(s: &str) -> String {
    let b = s.as_bytes();
    let key_byte = |c: u8| is_word(c) || c == b'-' || c == b'.';
    let blank = |c: u8| c == b' ' || c == b'\t';
    let (mut out, mut last) = (String::with_capacity(s.len()), 0);
    let mut i = 0;
    while i < b.len() {
        let sep = b[i];
        if !(sep == b':' || sep == b'=') || i < last {
            i += 1;
            continue;
        }
        let mut j = i;
        while j > 0 && blank(b[j - 1]) {
            j -= 1;
        }
        let mut k = j;
        while k > 0 && key_byte(b[k - 1]) {
            k -= 1;
        }
        let mut start = i + 1;
        while start < b.len() && blank(b[start]) {
            start += 1;
        }
        let key = &s[k..j];
        let lower = key.to_ascii_lowercase();
        let spaced = j < i || start > i + 1;
        // `=` with no spaces is `assignments`; `==`, `!=` and `<=` compare;
        // `://` is a URL and `::` a path.
        let next = b.get(i + 1).copied();
        let prev = if i > 0 { Some(b[i - 1]) } else { None };
        let skip = k == j
            || (sep == b'='
                && (!spaced
                    || matches!(next, Some(b'='))
                    || matches!(prev, Some(b'=' | b'!' | b'<' | b'>'))))
            || (sep == b':' && (matches!(next, Some(b'/' | b':')) || prev == Some(b':')))
            || matches!(lower.as_str(), "authorization" | "proxy-authorization")
            || !sensitive(key)
            || start >= b.len()
            || b[start] == b'\n'
            || b[start] == b'\r';
        if skip {
            i += 1;
            continue;
        }
        let cookie = lower.ends_with("cookie");
        let quote = matches!(b[start], b'"' | b'\'').then_some(b[start] as char);
        let end = match quote {
            Some(q) => s[start + 1..]
                .find(q)
                .map_or(b.len(), |n| start + 1 + n + 1),
            None if cookie => s[start..].find(['\n', '\r']).map_or(b.len(), |n| start + n),
            None => s[start..]
                .find(|c: char| c.is_whitespace() || ",;&)<>\"'".contains(c))
                .map_or(b.len(), |n| start + n),
        };
        let value = &s[start..end];
        if end <= start || value.trim_matches(['"', '\'']).starts_with(MARK) {
            i += 1;
            continue;
        }
        out.push_str(&s[last..start]);
        match quote {
            Some(q) => {
                out.push(q);
                out.push_str(MARK);
                out.push(q);
            }
            None => out.push_str(MARK),
        }
        last = end;
        i = end;
    }
    out.push_str(&s[last..]);
    out
}

/// `ada@example.org`, whole: an email address names the person. Not
/// `git@github.com:…`, and not `image@sha256:…`.
fn emails(s: &str) -> String {
    let b = s.as_bytes();
    let local = |c: u8| c.is_ascii_alphanumeric() || b"._%+-".contains(&c);
    let domain = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'.';
    let (mut out, mut last, mut from) = (String::with_capacity(s.len()), 0, 0);
    while let Some(at) = s[from..].find('@').map(|i| i + from) {
        from = at + 1;
        let mut i = at;
        while i > last && local(b[i - 1]) {
            i -= 1;
        }
        let mut j = at + 1;
        while j < b.len() && domain(b[j]) {
            j += 1;
        }
        let host = s[at + 1..j].trim_end_matches('.');
        let j = at + 1 + host.len();
        let tld = host.rsplit('.').next().unwrap_or("");
        let ok = i < at
            && &s[i..at] != "git"
            && host.contains('.')
            && tld.len() >= 2
            && tld.bytes().all(|c| c.is_ascii_alphabetic());
        if ok {
            out.push_str(&s[last..i]);
            out.push_str(MARK);
            last = j;
            from = j;
        }
    }
    out.push_str(&s[last..]);
    out
}

/// A JWT (`eyJ….eyJ….sig`), whole.
fn jwts(s: &str) -> String {
    let b = s.as_bytes();
    let part = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    let (mut out, mut last, mut from) = (String::with_capacity(s.len()), 0, 0);
    while let Some(i) = s[from..].find("eyJ").map(|i| i + from) {
        let (mut j, mut dots) = (i, 0);
        while j < b.len() && (part(b[j]) || (b[j] == b'.' && dots < 2)) {
            dots += usize::from(b[j] == b'.');
            j += 1;
        }
        if (i == 0 || !is_word(b[i - 1]))
            && dots == 2
            && j - i >= 24
            && s[i..j]
                .split('.')
                .nth(1)
                .is_some_and(|p| p.starts_with("eyJ"))
        {
            out.push_str(&s[last..i]);
            out.push_str(MARK);
            last = j;
        }
        from = j.max(i + 3);
    }
    out.push_str(&s[last..]);
    out
}

/// `--password hunter2`, `--token "two words"`: the value after a sensitive
/// long flag and spaces. The next flag isn't a value.
fn flags(s: &str) -> String {
    let b = s.as_bytes();
    let key_byte = |c: u8| is_word(c) || c == b'-' || c == b'.';
    let (mut out, mut last, mut from) = (String::with_capacity(s.len()), 0, 0);
    while let Some(i) = s[from..].find("--").map(|i| i + from) {
        let mut k = i + 2;
        while k < b.len() && key_byte(b[k]) {
            k += 1;
        }
        from = k;
        let mut start = k;
        while start < b.len() && (b[start] == b' ' || b[start] == b'\t') {
            start += 1;
        }
        if (i > 0 && key_byte(b[i - 1])) || !sensitive(&s[i..k]) || start == k || start == b.len() {
            continue;
        }
        let quote = matches!(b[start], b'"' | b'\'').then_some(b[start] as char);
        let end = match quote {
            Some(q) => s[start + 1..]
                .find(q)
                .map_or(b.len(), |n| start + 1 + n + 1),
            None => s[start..]
                .find(char::is_whitespace)
                .map_or(b.len(), |n| start + n),
        };
        let value = &s[start..end];
        if value.starts_with('-') || value.trim_matches(['"', '\'']) == MARK {
            continue;
        }
        out.push_str(&s[last..start]);
        match quote {
            Some(q) => {
                out.push(q);
                out.push_str(MARK);
                out.push(q);
            }
            None => out.push_str(MARK),
        }
        last = end;
        from = end;
    }
    out.push_str(&s[last..]);
    out
}

/// What starts a token that says what it is: Kiri share's, GitHub's,
/// GitLab's, Docker Hub's, an AWS access key id, Stripe's, Slack's,
/// Anthropic's, OpenAI's, Google's, npm's and PyPI's.
const TOKEN_PREFIXES: [&str; 26] = [
    "ks_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    concat!("github", "_pat_"),
    "glpat-",
    "glptt-",
    "dckr_pat_",
    "AKIA",
    "ASIA",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "rk_test_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "sk-ant-",
    "sk-proj-",
    "sk-svcacct-",
    "AIza",
    "npm_",
    "pypi-",
];

/// `prefix…`, a token of that kind, wherever it appears.
fn prefixed_tokens(s: &str, prefix: &str) -> String {
    let b = s.as_bytes();
    let tail = |c: u8| is_word(c) || c == b'-';
    let (mut out, mut last, mut from) = (String::with_capacity(s.len()), 0, 0);
    while let Some(i) = s[from..].find(prefix).map(|i| i + from) {
        let mut j = i + prefix.len();
        while j < b.len() && tail(b[j]) {
            j += 1;
        }
        if (i == 0 || !is_word(b[i - 1])) && j - (i + prefix.len()) >= 12 {
            out.push_str(&s[last..i + prefix.len()]);
            out.push_str(MARK);
            last = j;
        }
        from = j;
    }
    out.push_str(&s[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> Redactor {
        Redactor::new(
            Some("/Users/ada/".into()),
            vec!["s3cret-signed-in-token".into(), "short".into()],
        )
    }

    #[test]
    fn home_becomes_tilde_but_not_a_longer_name() {
        let out = r().apply("open /Users/ada/.kiri/logs and /Users/adam/x and /Users/ada");
        assert_eq!(out, "open ~/.kiri/logs and /Users/adam/x and ~");
    }

    #[test]
    fn known_secrets_go_wherever_they_are() {
        assert_eq!(
            r().apply("sent s3cret-signed-in-token twice: s3cret-signed-in-token"),
            "sent [redacted] twice: [redacted]"
        );
        // Too short to be worth matching blindly.
        assert_eq!(r().apply("a short note"), "a short note");
    }

    #[test]
    fn bearer_tokens() {
        assert_eq!(
            patterns("Authorization: Bearer eyJhbGciOi.J9x-y_z"),
            "Authorization: Bearer [redacted]"
        );
        assert_eq!(
            patterns("authorization: bearer abcdefgh1234"),
            "authorization: bearer [redacted]"
        );
        // Words, not tokens.
        assert_eq!(patterns("the bearer of bad news"), "the bearer of bad news");
        assert_eq!(patterns("Bearer [redacted]"), "Bearer [redacted]");
    }

    #[test]
    fn json_values_of_secret_keys() {
        assert_eq!(
            patterns(
                r#"{"login":"ada","token":"ks_abc","password" : "p\"w","access_token":"x","auths":{"r":{"auth":"YWRhOnB3"}}}"#
            ),
            r#"{"login":"ada","token":"[redacted]","password" : "[redacted]","access_token":"[redacted]","auths":{"r":{"auth":"[redacted]"}}}"#
        );
        // Escaped, as inside a log line's message.
        assert_eq!(
            patterns(r#"body {\"secret\":\"hunter2\",\"n\":1}"#),
            r#"body {\"secret\":\"[redacted]\",\"n\":1}"#
        );
        // Not a key: a value that happens to say token.
        assert_eq!(
            patterns(r#"{"msg":"token","n":"1"}"#),
            r#"{"msg":"token","n":"1"}"#
        );
    }

    #[test]
    fn query_values_and_assignments() {
        assert_eq!(
            patterns("GET /cb?code=1&token=abc123&x=2 HTTP/1.1"),
            "GET /cb?code=1&token=[redacted]&x=2 HTTP/1.1"
        );
        assert_eq!(
            patterns("https://x/?password=hunter2"),
            "https://x/?password=[redacted]"
        );
        assert_eq!(
            patterns("-e DB_PASSWORD=hunter2 -e MODE=dev"),
            "-e DB_PASSWORD=[redacted] -e MODE=dev"
        );
        assert_eq!(
            patterns("--password='two words' next"),
            "--password='[redacted]' next"
        );
        assert_eq!(
            patterns("KIRI_SHARE_TOKEN=\"abc def\""),
            "KIRI_SHARE_TOKEN=\"[redacted]\""
        );
        assert_eq!(patterns("if token == other"), "if token == other");
        assert_eq!(patterns("a=b token="), "a=b token=");
    }

    #[test]
    fn share_tokens_anywhere() {
        assert_eq!(
            patterns("signed in with ks_0123456789abcdefXYZ, ok"),
            "signed in with ks_[redacted], ok"
        );
        // Too short, or part of a word.
        assert_eq!(
            patterns("ks_x and thanks_0123456789abcdef"),
            "ks_x and thanks_0123456789abcdef"
        );
    }

    #[test]
    fn passwords_in_urls() {
        assert_eq!(
            patterns("DATABASE_URL=postgres://app:hunter2@db.internal/prod"),
            "DATABASE_URL=postgres://[redacted]@db.internal/prod"
        );
        assert_eq!(
            patterns("proxy failed: http://bob:p@ss@proxy.corp:3128, retrying"),
            "proxy failed: http://[redacted]@proxy.corp:3128, retrying"
        );
        assert_eq!(
            patterns("https://ghp_x@github.com/a/b.git"),
            "https://[redacted]@github.com/a/b.git"
        );
        // No password in these.
        for text in [
            "https://example.com/a@b",
            "git@github.com:a/b.git",
            "http://localhost:8080/?who=a@b.c",
            "see https://runkiri.com.",
        ] {
            assert_eq!(patterns(text), text);
        }
    }

    #[test]
    fn authorization_values() {
        assert_eq!(
            patterns("Authorization: Basic Ym9iOmh1bnRlcjI="),
            "Authorization: Basic [redacted]"
        );
        assert_eq!(
            patterns("proxy-authorization: Basic Ym9i, next"),
            "proxy-authorization: Basic [redacted], next"
        );
        assert_eq!(
            patterns("Authorization: abc123"),
            "Authorization: [redacted]"
        );
        assert_eq!(
            patterns(r#"{"Authorization":"Basic Ym9i"}"#),
            r#"{"Authorization":"[redacted]"}"#
        );
        assert_eq!(
            patterns("basic authentication failed"),
            "basic authentication failed"
        );
    }

    #[test]
    fn private_key_blocks() {
        let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END RSA PRIVATE KEY-----";
        assert_eq!(
            patterns(&format!("before {key} after")),
            "before [redacted] after"
        );
        assert_eq!(
            patterns("x -----BEGIN PRIVATE KEY----- MIIEvQIBADANBg"),
            "x [redacted]"
        );
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        assert_eq!(patterns(cert), cert);
    }

    #[test]
    fn secret_flags() {
        assert_eq!(
            patterns("--password hunter2 --verbose"),
            "--password [redacted] --verbose"
        );
        assert_eq!(
            patterns("login --token 'two words' x"),
            "login --token '[redacted]' x"
        );
        assert_eq!(patterns("--api-key abc"), "--api-key [redacted]");
        // The next flag, or nothing, isn't a value; other flags are left.
        for text in [
            "--password --verbose",
            "--password",
            "--name web",
            "pre--token x",
        ] {
            assert_eq!(patterns(text), text);
        }
    }

    #[test]
    fn more_secret_keys() {
        for key in [
            "PGPASS",
            "MYSQL_PASS",
            "API-KEY",
            "X.API.KEY",
            "SSH_PRIVATE_KEY",
            "AWS_ACCESS_KEY_ID",
            "GPG_PASSPHRASE",
        ] {
            assert_eq!(
                patterns(&format!("{key}=abc123")),
                format!("{key}=[redacted]")
            );
        }
        for text in ["bypass=1", "compass=north", "passed=3", "MONKEY=1"] {
            assert_eq!(patterns(text), text);
        }
    }

    #[test]
    fn tokens_that_say_what_they_are() {
        assert_eq!(
            patterns("pushed with ghp_abcdefghijklmnopqrstuvwxyz0123456789 ok"),
            "pushed with ghp_[redacted] ok"
        );
        assert_eq!(
            patterns("github_pat_11ABCDEFG0abcdefghij_klmn"),
            "github_pat_[redacted]"
        );
        assert_eq!(patterns("glpat-abcdef123456xyz"), "glpat-[redacted]");
        assert_eq!(patterns("dckr_pat_abcdefghijklmnop"), "dckr_pat_[redacted]");
        assert_eq!(patterns("key AKIAIOSFODNN7EXAMPLE"), "key AKIA[redacted]");
        assert_eq!(
            patterns("ghp_short and aghp_abcdefghijklmnop"),
            "ghp_short and aghp_abcdefghijklmnop"
        );
    }

    #[test]
    fn redacting_twice_changes_nothing() {
        let text = r#"Bearer abcdefghijkl token=abc {"token":"x"} ks_0123456789abcdef /Users/ada/x postgres://a:b@h/d Authorization: Basic Ym9i --password hunter2 --token "two words" ghp_abcdefghijklmnop -----BEGIN PRIVATE KEY-----k-----END PRIVATE KEY----- end"#;
        let once = r().apply(text);
        assert_eq!(r().apply(&once), once);
        assert!(
            !once.contains("abcdefghijkl")
                && !once.contains("0123456789abcdef")
                && !once.contains("/Users/ada"),
            "{once}"
        );
    }

    #[test]
    fn windows_home_in_every_form() {
        let r = Redactor::new(Some(r"C:\Users\Alex".into()), Vec::new());
        for (text, want) in [
            (r"opened C:\Users\Alex\proj", r"opened ~\proj"),
            (
                r#"{"path":"C:\\Users\\Alex\\proj"}"#,
                r#"{"path":"~\\proj"}"#,
            ),
            ("C:/Users/Alex/proj", "~/proj"),
            (r"c:\users\ALEX\proj", r"~\proj"),
            ("/mnt/c/Users/Alex/proj", "~/proj"),
            ("/home/alex/.docker/config.json", "~/.docker/config.json"),
        ] {
            assert_eq!(r.apply(text), want, "{text}");
        }
        // A longer name is someone else.
        assert_eq!(r.apply(r"C:\Users\Alexandra\x"), r"C:\Users\Alexandra\x");
        assert_eq!(
            Redactor::new(Some(r"C:\".into()), Vec::new()).apply(r"C:\x"),
            r"C:\x"
        );
    }

    #[test]
    fn colon_and_spaced_values() {
        for (text, want) in [
            ("password: hunter2", "password: [redacted]"),
            ("token : abc123 next", "token : [redacted] next"),
            (r#"token: "two words""#, r#"token: "[redacted]""#),
            (
                "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG",
                "aws_secret_access_key = [redacted]",
            ),
            (
                r#"Config { password: "x", port: 5432 }"#,
                r#"Config { password: "[redacted]", port: 5432 }"#,
            ),
            ("X-Api-Key: abcdef0123456789", "X-Api-Key: [redacted]"),
            (
                "Cookie: session=abc; theme=dark\nnext",
                "Cookie: [redacted]\nnext",
            ),
            (
                "Set-Cookie: sg_session=abc; Path=/",
                "Set-Cookie: [redacted]",
            ),
        ] {
            assert_eq!(patterns(text), want, "{text}");
        }
        for text in [
            "error: could not connect",
            "at 12:30: done",
            "see https://runkiri.com/token",
            "token refresh failed",
            "port = 5432",
            "password:\nnext",
        ] {
            assert_eq!(patterns(text), text);
        }
    }

    #[test]
    fn a_secret_inside_another_value() {
        assert_eq!(
            patterns("env=[DB_PASSWORD=hunter2 STRIPE=1]"),
            "env=[DB_PASSWORD=[redacted] STRIPE=1]"
        );
        assert_eq!(
            patterns("args=--password=hunter2"),
            "args=--password=[redacted]"
        );
    }

    #[test]
    fn email_addresses() {
        assert_eq!(
            patterns("logged in as ada.lovelace@example.org."),
            "logged in as [redacted]."
        );
        for text in [
            "git@github.com:a/b.git",
            "nginx@sha256:abcd",
            "a @ b",
            "x@localhost",
        ] {
            assert_eq!(patterns(text), text);
        }
    }

    #[test]
    fn jwts_whole() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdefghijklmn";
        assert_eq!(patterns(&format!("got {jwt} ok")), "got [redacted] ok");
        assert_eq!(patterns("eyJustAWord and eyJx.y"), "eyJustAWord and eyJx.y");
    }

    #[test]
    fn more_tokens_that_say_what_they_are() {
        for (text, want) in [
            ("sk_live_abcdefghijklmnopqrstu", "sk_live_[redacted]"),
            ("xoxb-123456789012-abcdefghijkl", "xoxb-[redacted]"),
            (
                "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
                "sk-ant-[redacted]",
            ),
            (
                "sk-proj-abcdefghijklmnopqrstuvwxyz012345",
                "sk-proj-[redacted]",
            ),
            ("AIzaSyA1234567890abcdefghijklmnop", "AIza[redacted]"),
            ("npm_abcdefghijklmnopqrstuvwxyz0123456789", "npm_[redacted]"),
        ] {
            assert_eq!(patterns(text), want, "{text}");
        }
    }

    #[test]
    fn redacting_the_new_forms_twice_changes_nothing() {
        let text = r#"password: hunter2 Cookie: a=b eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdefghijklmn sk_live_abcdefghijklmnop env=[DB_PASSWORD=x ada@example.org C:/Users/Ada/x"#;
        let r = Redactor::new(Some(r"C:\Users\Ada".into()), Vec::new());
        let once = r.apply(text);
        assert_eq!(r.apply(&once), once);
        assert!(
            !once.contains("hunter2") && !once.contains("ada@") && !once.contains("Users/Ada"),
            "{once}"
        );
    }

    #[test]
    fn leaves_other_text_and_unicode_alone() {
        let text = "VM booted in 112 ms → 4 CPUs, café=ok, a = b";
        assert_eq!(patterns(text), text);
    }
}
