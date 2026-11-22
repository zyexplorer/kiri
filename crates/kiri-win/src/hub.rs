//! Pull image's suggestions: Docker Hub's search and each image's tags,
//! as crates/kiri/src/ui/hub.rs does for the Mac. Answers are kept in
//! %LOCALAPPDATA%\Kiri\hub.json, so a search seen before shows at once,
//! offline too; one older than a day is shown and then fetched again, and
//! the page swaps in the fresh answer.
//!
//! Search goes through the engine (`GET /images/search`, what
//! `docker search` uses), which already reaches Docker Hub from WSL. Tags
//! come from Hub's own API, which the engine does not serve, through
//! curl.exe (part of Windows): kiri-win has no HTTPS client of its own.
//! A name with a registry in front (ghcr.io/…, public.ecr.aws/…) is asked
//! of that registry instead, as the Mac asks kiri-image's client.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

// Following a tag list's pages and putting it newest first: kiri-image's own file.
#[path = "../../kiri-image/src/tags.rs"]
mod tagorder;

/// How long an answer is used without asking Docker Hub again.
const FRESH: Duration = Duration::from_secs(24 * 60 * 60);
/// Entries kept per kind; the oldest go first.
const KEEP: usize = 300;
/// curl's whole-request limit, so a Hub that does not answer cannot pin a thread.
const TIMEOUT_SECS: &str = "20";

/// One writer at a time for the cache file.
static CACHE: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
pub enum Kind {
    Search,
    Tags,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Search => "search",
            Kind::Tags => "tags",
        }
    }
}

fn path() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("Kiri").join("hub.json"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn load() -> Value {
    path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({}))
}

fn cached(kind: Kind, key: &str) -> Option<(u64, Value)> {
    let _guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let entry = load()[kind.key()][key].clone();
    Some((entry["at"].as_u64()?, entry["items"].clone()))
}

fn store(kind: Kind, key: &str, items: &Value) {
    let Some(path) = path() else { return };
    let _guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = load();
    if !all[kind.key()].is_object() {
        all[kind.key()] = Value::Object(Map::new());
    }
    let Some(entries) = all[kind.key()].as_object_mut() else { return };
    entries.insert(key.to_owned(), json!({"at": now(), "items": items}));
    if entries.len() > KEEP {
        let mut ages: Vec<(u64, String)> = entries.iter().map(|(k, v)| (v["at"].as_u64().unwrap_or(0), k.clone())).collect();
        ages.sort();
        for (_, k) in ages.into_iter().take(entries.len() - KEEP) {
            entries.remove(&k);
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Written aside and renamed, so a reader never sees half a file.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, all.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Docker Hub's search, through the engine.
fn search(term: &str) -> Result<Value, String> {
    let r = crate::client::request("GET", &format!("/images/search?term={}&limit=50", crate::engine::path_escape(term)), None).map_err(|e| e.to_string())?;
    if !r.ok() {
        return Err(r.error());
    }
    let mut rows = r.json().as_array().cloned().unwrap_or_default();
    // Hub's own order puts empty namesakes first: official images lead,
    // then the most starred.
    rows.sort_by_key(|r| (!r["is_official"].as_bool().unwrap_or(false), std::cmp::Reverse(r["star_count"].as_u64().unwrap_or(0))));
    Ok(rows
        .iter()
        .map(|r| {
            let name = r["name"].as_str().unwrap_or_default();
            json!({
                "name": name,
                "description": r["description"],
                "stars": r["star_count"],
                "official": r["is_official"],
                "icon": crate::engine::brand(name),
            })
        })
        .collect())
}

/// A Hub repository's tags, most recently pushed first (`postgres` gives
/// `latest`, `17-alpine`, `17`…), from Hub's API through curl.exe.
fn tags(repo: &str) -> Result<Vec<String>, String> {
    // A repository name as the page sends it: no way to smuggle a different
    // URL, or a curl option, in.
    if repo.is_empty() || repo.starts_with('-') || repo.contains("..") || !repo.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-/".contains(&b)) {
        return Err("not a Docker Hub repository name".to_owned());
    }
    let repo = if repo.contains('/') { repo.to_owned() } else { format!("library/{repo}") };
    let url = format!("https://hub.docker.com/v2/repositories/{repo}/tags?page_size=100&ordering=last_updated");
    let mut cmd = crate::win::tool("curl.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.args(["-fsSL", "--max-time", TIMEOUT_SECS, "--"]).arg(&url).stdin(Stdio::null()).stderr(Stdio::piped()).output().map_err(|e| format!("curl.exe: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("Docker Hub did not answer").trim().to_owned());
    }
    let body: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    Ok(order_tags(body["results"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect()))
}

/// `latest` first, then the newest tags, with pre-releases (`19rc1`,
/// `unstable`, `nightly`) last: Hub lists them by push date, so those lead.
fn order_tags(mut tags: Vec<String>) -> Vec<String> {
    let rank = |t: &str| {
        let pre = ["rc", "beta", "alpha", "unstable", "nightly", "dev", "edge", "preview"].iter().any(|p| t.contains(p));
        if t == "latest" { 0 } else if pre { 2 } else { 1 }
    };
    // Stable, so each group keeps Hub's newest-first order.
    tags.sort_by_key(|t| rank(t));
    tags
}

/// A name with a registry host first (`ghcr.io/o/r`, `public.ecr.aws/o/r`),
/// as the Mac's hub.rs tells them from Hub names.
fn on_registry(name: &str) -> bool {
    name.split_once('/').is_some_and(|(first, _)| first.contains('.') || first.contains(':') || first == "localhost")
}

/// A registry's tags, newest version first, as the distribution API lists
/// them: asked anonymously; a Bearer challenge is answered with the token
/// its realm gives; the list is read a page at a time (up to 20).
fn registry_tags(name: &str) -> Result<Vec<String>, String> {
    let (host, repo) = name.split_once('/').ok_or("not a registry image name")?;
    let host_ok = !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b));
    let repo_ok = !repo.is_empty() && !repo.contains("..") && !repo.starts_with(['-', '/']) && repo.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-/".contains(&b));
    if !host_ok || !repo_ok {
        return Err("not a registry image name".to_owned());
    }
    let base = format!("https://{host}");
    let mut url = format!("{base}/v2/{repo}/tags/list?n=1000");
    let mut token: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut pages = 0;
    while pages < 20 {
        let (status, headers, body) = curl(&url, token.as_deref())?;
        if status == 401 && token.is_none() {
            token = Some(bearer_token(header(&headers, "www-authenticate").ok_or_else(|| format!("{host} wants a sign-in"))?)?);
            continue;
        }
        if status != 200 {
            return Err(match status {
                401 | 403 => format!("{host} wants a sign-in for {repo}"),
                404 => format!("{host} has no image called {repo}"),
                s => format!("{host} answered {s}"),
            });
        }
        let body: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        tags.extend(body["tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).filter(|t| !t.starts_with("sha256-")).map(str::to_owned));
        pages += 1;
        // Only a next page on the same registry is followed.
        match header(&headers, "link").and_then(tagorder::next_link) {
            Some(n) if n.starts_with("/v2/") => url = format!("{base}{n}"),
            _ => break,
        }
    }
    tags.sort_by(|a, b| tagorder::version_order(b, a));
    let mut tags = order_tags(tags);
    tags.truncate(100);
    Ok(tags)
}

/// GET through curl.exe: the status, the headers and the body.
fn curl(url: &str, token: Option<&str>) -> Result<(u16, String, Vec<u8>), String> {
    let mut cmd = crate::win::tool("curl.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    cmd.args(["-sS", "-i", "--max-time", TIMEOUT_SECS]);
    if let Some(t) = token {
        cmd.arg("-H").arg(format!("Authorization: Bearer {t}"));
    }
    let out = cmd.arg("--").arg(url).stdin(Stdio::null()).stderr(Stdio::piped()).output().map_err(|e| format!("curl.exe: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("the registry did not answer").trim().to_owned());
    }
    split_response(&out.stdout).ok_or_else(|| "the registry's answer couldn't be read".to_owned())
}

/// `curl -i`'s output: the last header block (after any `100 Continue`) and the body.
fn split_response(raw: &[u8]) -> Option<(u16, String, Vec<u8>)> {
    let mut rest = raw;
    loop {
        let end = rest.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end + 4..];
        let status: u16 = head.split_whitespace().nth(1)?.parse().ok()?;
        if status != 100 {
            return Some((status, head, rest.to_vec()));
        }
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
}

/// The anonymous token a Bearer challenge's realm hands out
/// (`Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:o/r:pull"`).
fn bearer_token(challenge: &str) -> Result<String, String> {
    let rest = challenge.get(..7).filter(|b| b.eq_ignore_ascii_case("bearer ")).map(|_| &challenge[7..]).ok_or("the registry wants a password")?;
    let param = |k: &str| rest.split(',').find_map(|p| p.trim().strip_prefix(k).and_then(|v| v.strip_prefix('='))).map(|v| v.trim_matches('"').to_owned());
    let realm = param("realm").filter(|r| r.starts_with("https://")).ok_or("the registry's sign-in isn't https")?;
    let query: Vec<String> = ["service", "scope"].iter().filter_map(|k| param(k).map(|v| format!("{k}={}", v.replace(' ', "%20")))).collect();
    let url = if query.is_empty() { realm } else { format!("{realm}{}{}", if realm.contains('?') { "&" } else { "?" }, query.join("&")) };
    let (status, _, body) = curl(&url, None)?;
    if status != 200 {
        return Err(format!("the registry's token service answered {status}"));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    v["token"].as_str().or(v["access_token"].as_str()).map(str::to_owned).ok_or_else(|| "the registry gave no token".to_owned())
}

fn fetch(kind: Kind, key: &str) -> Result<Value, String> {
    match kind {
        Kind::Search => search(key),
        Kind::Tags if on_registry(key) => registry_tags(key).map(|t| json!(t)),
        Kind::Tags => tags(key).map(|t| json!(t)),
    }
}

/// Answers the page with the cached answer, if any, then with a fresh one
/// when the cached one is missing or stale. Runs on its own thread.
pub fn ask(kind: Kind, key: String, emit: fn(String)) {
    std::thread::spawn(move || {
        let reply = |items: &Value, error: Option<&str>| json!({"type": "hub", "kind": kind.key(), "key": key, "items": items, "error": error}).to_string();
        let old = cached(kind, &key);
        if let Some((_, items)) = &old {
            emit(reply(items, None));
        }
        if old.as_ref().is_some_and(|(at, _)| now().saturating_sub(*at) < FRESH.as_secs()) {
            return;
        }
        match fetch(kind, &key) {
            Ok(items) => {
                store(kind, &key, &items);
                emit(reply(&items, None));
            }
            // Offline with an old answer: keep showing that one.
            Err(_) if old.is_some() => {}
            Err(e) => emit(reply(&json!([]), Some(&e))),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_with_a_host_is_asked_of_that_registry() {
        assert!(on_registry("ghcr.io/home-assistant/home-assistant"));
        assert!(on_registry("public.ecr.aws/nginx/nginx"));
        assert!(on_registry("localhost:5000/app"));
        assert!(!on_registry("library/postgres"));
        assert!(!on_registry("postgres"));
    }

    #[test]
    fn curls_answer_is_split_after_any_continue() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/2 401\r\nwww-authenticate: Bearer realm=\"https://ghcr.io/token\"\r\nlink: </v2/o/r/tags/list?last=b&n=1000>; rel=\"next\"\r\n\r\n{\"x\":1}";
        let (status, head, body) = split_response(raw).unwrap();
        assert_eq!((status, body.as_slice()), (401, b"{\"x\":1}".as_slice()));
        assert_eq!(header(&head, "WWW-Authenticate"), Some("Bearer realm=\"https://ghcr.io/token\""));
        assert_eq!(header(&head, "link").and_then(tagorder::next_link), Some("/v2/o/r/tags/list?last=b&n=1000"));
    }

    #[test]
    fn names_that_could_carry_another_url_are_refused() {
        assert!(registry_tags("ghcr.io/../x").is_err());
        assert!(registry_tags("evil.example/a b").is_err());
        assert!(registry_tags("ghcr.io/-o/r").is_err());
        assert!(registry_tags("ghcr.io@evil/x").is_err());
    }

    #[test]
    #[ignore = "asks the real ghcr.io and public.ecr.aws through curl.exe"]
    fn live_github_and_ecr_list_their_tags_newest_first() {
        for name in ["ghcr.io/home-assistant/home-assistant", "public.ecr.aws/nginx/nginx"] {
            let tags = registry_tags(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(tags.len() > 5, "{name}: {tags:?}");
            eprintln!("{name}: {:?}", &tags[..5]);
        }
        assert!(registry_tags("public.ecr.aws/no-such-owner-kiri/none").is_err());
    }

    #[test]
    fn latest_first_prereleases_last() {
        let tags = ["19rc1", "17-alpine", "unstable", "latest", "16"].map(String::from).to_vec();
        assert_eq!(super::order_tags(tags), ["latest", "17-alpine", "16", "19rc1", "unstable"]);
    }

    #[test]
    fn tags_refuse_what_is_not_a_repository() {
        for bad in ["", "-o", "a b", "x?y=1", "../z", "a&b"] {
            assert!(super::tags(bad).is_err(), "{bad:?} was accepted");
        }
    }
}
