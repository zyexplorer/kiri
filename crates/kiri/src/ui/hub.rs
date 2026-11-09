//! Pull image's suggestions: Docker Hub's search and each image's tags.
//! Answers are kept in ~/Library/Caches/Kiri/hub.json, so a search seen
//! before shows at once, offline too; one older than a day is shown and
//! then fetched again, and the page swaps in the fresh answer.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// How long an answer is used without asking Docker Hub again.
const FRESH: Duration = Duration::from_secs(24 * 60 * 60);
/// Entries kept per kind; the oldest go first.
const KEEP: usize = 300;

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
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Caches/Kiri/hub.json"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn load() -> Value {
    path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}))
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
    let Some(entries) = all[kind.key()].as_object_mut() else {
        return;
    };
    entries.insert(key.to_owned(), json!({"at": now(), "items": items}));
    if entries.len() > KEEP {
        let mut ages: Vec<(u64, String)> = entries
            .iter()
            .map(|(k, v)| (v["at"].as_u64().unwrap_or(0), k.clone()))
            .collect();
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

fn fetch(kind: Kind, key: &str) -> Result<Value, String> {
    let client = kiri_image::Client::new();
    match kind {
        Kind::Search => {
            let mut rows = client.search(key, 50).map_err(|e| e.to_string())?;
            // Hub's own order puts empty namesakes first: official images
            // lead, then the most starred.
            rows.sort_by_key(|r| {
                (
                    !r["is_official"].as_bool().unwrap_or(false),
                    std::cmp::Reverse(r["star_count"].as_u64().unwrap_or(0)),
                )
            });
            Ok(rows
                .iter()
                .map(|r| {
                    let name = r["name"].as_str().unwrap_or_default();
                    json!({
                        "name": name,
                        "description": r["description"],
                        "stars": r["star_count"],
                        "official": r["is_official"],
                        "icon": super::brand(name),
                    })
                })
                .collect())
        }
        Kind::Tags => client
            .hub_tags(key, 100)
            .map(|t| json!(order_tags(t)))
            .map_err(|e| e.to_string()),
    }
}

/// `latest` first, then the newest tags, with pre-releases (`19rc1`,
/// `unstable`, `nightly`) last: Hub lists them by push date, so those lead.
fn order_tags(mut tags: Vec<String>) -> Vec<String> {
    let rank = |t: &str| {
        let pre = [
            "rc", "beta", "alpha", "unstable", "nightly", "dev", "edge", "preview",
        ]
        .iter()
        .any(|p| t.contains(p));
        if t == "latest" {
            0
        } else if pre {
            2
        } else {
            1
        }
    };
    // Stable, so each group keeps Hub's newest-first order.
    tags.sort_by_key(|t| rank(t));
    tags
}

#[cfg(test)]
mod tests {
    #[test]
    fn latest_first_prereleases_last() {
        let tags = ["19rc1", "17-alpine", "unstable", "latest", "16"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            super::order_tags(tags),
            ["latest", "17-alpine", "16", "19rc1", "unstable"]
        );
    }
}

/// Answers the page with the cached answer, if any, then with a fresh one
/// when the cached one is missing or stale. Runs on its own thread.
pub fn ask(kind: Kind, key: String, emit: fn(String)) {
    std::thread::spawn(move || {
        let reply = |items: &Value, error: Option<&str>| {
            json!({"type": "hub", "kind": kind.key(), "key": key, "items": items, "error": error})
                .to_string()
        };
        let old = cached(kind, &key);
        if let Some((_, items)) = &old {
            emit(reply(items, None));
        }
        if old
            .as_ref()
            .is_some_and(|(at, _)| now().saturating_sub(*at) < FRESH.as_secs())
        {
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
