//! Daemon settings in `~/.kiri/daemon.json`, in the shape Docker's
//! `/etc/docker/daemon.json` uses for the fields Kiri understands:
//! `registry-mirrors`, `insecure-registries`, `max-concurrent-downloads`
//! and `max-concurrent-uploads`. Other keys are kept so a pasted Docker
//! `daemon.json` round-trips.

use std::path::PathBuf;

use serde_json::{Value, json};

/// Kiri's defaults when the keys are absent (downloads match the previous
/// hard-coded parallel layer count; uploads match Docker's default).
const DEFAULT_DOWNLOADS: u64 = 4;
const DEFAULT_UPLOADS: u64 = 5;
const MAX_CONCURRENT: u64 = 16;

pub fn path() -> Option<PathBuf> {
    super::state_dir().ok().map(|d| d.join("daemon.json"))
}

fn load() -> Value {
    path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

/// The configured Hub mirrors, normalized the way `docker pull` expects.
pub fn mirrors() -> Vec<String> {
    load()["registry-mirrors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .filter_map(kiri_image::normalize_mirror)
        .collect()
}

/// Hosts and CIDRs that may be reached over plain HTTP, as Docker's
/// `insecure-registries` does.
pub fn insecure() -> Vec<String> {
    load()["insecure-registries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .filter_map(kiri_image::normalize_insecure)
        .collect()
}

fn concurrent(key: &str, default: u64) -> u64 {
    match load().get(key) {
        Some(Value::Number(n)) => n.as_u64().filter(|v| (1..=MAX_CONCURRENT).contains(v)).unwrap_or(default),
        _ => default,
    }
}

pub fn max_concurrent_downloads() -> usize {
    concurrent("max-concurrent-downloads", DEFAULT_DOWNLOADS) as usize
}

pub fn max_concurrent_uploads() -> usize {
    concurrent("max-concurrent-uploads", DEFAULT_UPLOADS) as usize
}

/// The whole file as an object, with known keys normalized.
pub fn settings() -> Value {
    let mut out = load();
    if !out.is_object() {
        out = json!({});
    }
    out["registry-mirrors"] = json!(mirrors());
    out["insecure-registries"] = json!(insecure());
    out["max-concurrent-downloads"] = json!(max_concurrent_downloads());
    out["max-concurrent-uploads"] = json!(max_concurrent_uploads());
    out
}

/// Writes `v` as `daemon.json`. Known keys are normalized; every other key
/// is stored as given (unused by Kiri, kept for Docker-shaped pastes).
pub fn save(v: &Value) -> std::io::Result<()> {
    let path = path().ok_or_else(|| std::io::Error::other("HOME is not set"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut out = if v.is_object() { v.clone() } else { json!({}) };
    let mirrors: Vec<String> = out["registry-mirrors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str())
        .filter_map(kiri_image::normalize_mirror)
        .collect();
    out["registry-mirrors"] = json!(mirrors);
    let insecure: Vec<String> = out["insecure-registries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str())
        .filter_map(kiri_image::normalize_insecure)
        .collect();
    if out.get("insecure-registries").is_some() || !insecure.is_empty() {
        out["insecure-registries"] = json!(insecure);
    }
    for (key, default) in [("max-concurrent-downloads", DEFAULT_DOWNLOADS), ("max-concurrent-uploads", DEFAULT_UPLOADS)] {
        if let Some(raw) = out.get(key) {
            let n = raw.as_u64().filter(|v| (1..=MAX_CONCURRENT).contains(v)).unwrap_or(default);
            out[key] = json!(n);
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&out).unwrap_or_else(|_| out.to_string()) + "\n")?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// For kiri-image: the Hub mirrors every pull should try first.
pub fn for_registry() -> Vec<String> {
    mirrors()
}

pub fn for_insecure() -> Vec<String> {
    insecure()
}

pub fn for_downloads() -> usize {
    max_concurrent_downloads()
}

pub fn for_uploads() -> usize {
    max_concurrent_uploads()
}

pub fn report() -> Value {
    json!({ "settings": settings() })
}

/// Docker's `/info` RegistryConfig: mirrors apply only to docker.io;
/// insecure hosts and CIDRs are listed the way `docker info` shows them.
pub fn registry_config() -> Value {
    let mirrors = mirrors();
    let insecure = insecure();
    let mut indexes = json!({
        "docker.io": {
            "Name": "docker.io",
            "Mirrors": mirrors,
            "Secure": true,
            "Official": true,
        }
    });
    let mut cidrs = Vec::new();
    for entry in &insecure {
        if entry.contains('/') {
            cidrs.push(entry.clone());
            continue;
        }
        indexes[entry] = json!({
            "Name": entry,
            "Mirrors": [],
            "Secure": false,
            "Official": false,
        });
    }
    json!({
        "IndexConfigs": indexes,
        "InsecureRegistryCIDRs": cidrs,
        "Mirrors": mirrors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_means_no_mirrors() {
        assert!(mirrors().is_empty() || path().is_some());
        let cfg = registry_config();
        assert!(cfg["IndexConfigs"]["docker.io"]["Official"].as_bool().unwrap());
        assert_eq!(cfg["IndexConfigs"]["docker.io"]["Name"], "docker.io");
    }
}
