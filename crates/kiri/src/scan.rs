//! Builds → Security: known vulnerabilities in an image, found by Grype
//! (Anchore's scanner) with the packages it finds as a CycloneDX SBOM.
//!
//! Grype is downloaded on the first scan, at the pinned version and
//! checksum (cli_pins.rs), with its vulnerability database (about 200 MB
//! to download, 3 GB unpacked). It reads the image from Kiri's own store,
//! through an OCI layout of hard links, so the VM isn't woken. Every scan
//! is kept, under `scans/<image ID>/<unix time>` with a summary for the
//! history list; the newest `KEEP` per image.
//!
//! On Windows the engine in WSL runs this, where the store is, for the
//! window (daemon/api.rs), with Grype's Linux build.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::cli_pins;

/// Grype's version and the SHA-256 of its download, and where it is.
#[cfg(target_os = "macos")]
const GRYPE: (&str, &str) = cli_pins::MacArch::HOST.grype();
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const GRYPE: (&str, &str) = cli_pins::LINUX_ARM64_GRYPE;
#[cfg(all(target_os = "linux", not(target_arch = "aarch64")))]
const GRYPE: (&str, &str) = cli_pins::LINUX_GRYPE;

fn grype_url(v: &str) -> String {
    if cfg!(target_os = "macos") {
        cli_pins::MacArch::HOST.grype_url(v)
    } else {
        cli_pins::linux_grype_url(
            v,
            if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "amd64"
            },
        )
    }
}

/// Images being scanned now, so a second click doesn't start another.
static RUNNING: Mutex<Option<HashSet<String>>> = Mutex::new(None);
/// Advisory pages from the scans shown so far: the only addresses `open_advisory` opens.
static LINKS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn root() -> io::Result<PathBuf> {
    kiri_image::Store::default_root()
}

fn grype() -> io::Result<PathBuf> {
    Ok(root()?.join("scanner/grype"))
}

/// Scans kept per image.
const KEEP: usize = 20;

/// An image's scans, one folder each: Grype's JSON report, the SBOM and a summary.
fn scan_dir(config_digest: &str) -> io::Result<PathBuf> {
    Ok(root()?
        .join("scans")
        .join(kiri_image::store::digest_hex(config_digest)?))
}

/// The image's scans, newest first, by their time. A scan kept before there
/// was a history (the files straight in the image's folder) moves into one.
fn scan_times(dir: &Path) -> Vec<u64> {
    if let Ok(m) = std::fs::metadata(dir.join("grype.json")) {
        let at = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        let to = dir.join(at.to_string());
        if std::fs::create_dir_all(&to).is_ok() {
            let _ = std::fs::rename(dir.join("grype.json"), to.join("grype.json"));
            let _ = std::fs::rename(dir.join("sbom.cdx.json"), to.join("sbom.cdx.json"));
        }
    }
    let mut at: Vec<u64> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("grype.json").is_file())
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    at.sort_unstable_by(|a, b| b.cmp(a));
    at
}

/// Grype's environment: its own database folder, no update nag, Kiri's proxy.
fn command(args: &[&str]) -> io::Result<Command> {
    let mut c = Command::new(grype()?);
    c.args(args)
        .env("GRYPE_DB_CACHE_DIR", root()?.join("scanner/db"))
        .env("GRYPE_CHECK_FOR_APP_UPDATE", "false");
    let p = crate::daemon::proxy::effective();
    if let Some(u) = &p.https {
        c.env("HTTPS_PROXY", u);
    }
    if let Some(u) = &p.http {
        c.env("HTTP_PROXY", u);
    }
    if !p.no_proxy.is_empty() {
        c.env("NO_PROXY", p.no_proxy.join(","));
    }
    Ok(c)
}

fn run(mut c: Command, what: &str) -> io::Result<()> {
    let out = c.output()?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // Grype's log lines carry colour codes; keep the last plain line.
    let line = err
        .lines()
        .rev()
        .map(|l| l.replace('\u{1b}', ""))
        .map(|l| {
            l.trim_start_matches(|c: char| c == '[' || c.is_ascii_digit() || c == ';' || c == 'm')
                .to_string()
        })
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default();
    Err(io::Error::other(format!("{what}: {}", line.trim())))
}

/// Downloads Grype if it isn't there yet, checked against its pinned SHA-256.
fn ensure_grype(progress: &mut dyn FnMut(u8)) -> io::Result<()> {
    let dest = grype()?;
    if dest.is_file() {
        return Ok(());
    }
    let dir = dest.parent().unwrap().to_owned();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".download-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let result = (|| {
        let (v, sum) = GRYPE;
        let part = tmp.join("grype.tar.gz");
        crate::tools::download(&crate::tools::agent(), &grype_url(v), &part, progress)?;
        crate::tools::verify(&part, sum)?;
        // busybox's tar in Kiri Linux; /bin is /usr/bin on Ubuntu.
        let st = Command::new(if cfg!(target_os = "macos") {
            "/usr/bin/tar"
        } else {
            "/bin/tar"
        })
        .arg("-xzf")
        .arg(&part)
        .arg("-C")
        .arg(&tmp)
        .arg("grype")
        .status()?;
        if !st.success() {
            return Err(io::Error::other("could not unpack Grype"));
        }
        std::fs::set_permissions(
            tmp.join("grype"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
        std::fs::rename(tmp.join("grype"), &dest)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// The image as an OCI layout Grype can read: hard links to the store's
/// blobs (a copy if they can't be linked) and a manifest written for it.
fn layout(store: &kiri_image::Store, i: &kiri_image::Image, to: &Path) -> io::Result<()> {
    let blobs = to.join("blobs/sha256");
    std::fs::create_dir_all(&blobs)?;
    let put = |digest: &str| -> io::Result<u64> {
        let from = store.blob_path(digest)?;
        let at = blobs.join(kiri_image::store::digest_hex(digest)?);
        if !at.exists() && std::fs::hard_link(&from, &at).is_err() {
            std::fs::copy(&from, &at)?;
        }
        Ok(std::fs::metadata(&at)?.len())
    };
    let config_size = put(&i.config_digest)?;
    let mut layers = Vec::new();
    for l in &i.layers {
        put(&l.digest)?;
        layers.push(json!({"mediaType": l.media_type, "digest": l.digest, "size": l.size}));
    }
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": i.config_digest, "size": config_size},
        "layers": layers,
    }))?;
    let digest = kiri_image::store::sha256_digest(&manifest);
    std::fs::write(
        blobs.join(kiri_image::store::digest_hex(&digest)?),
        &manifest,
    )?;
    std::fs::write(to.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    std::fs::write(
        to.join("index.json"),
        serde_json::to_vec(
            &json!({"schemaVersion": 2, "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": digest, "size": manifest.len()}]}),
        )?,
    )
}

/// Scans the image (a name or an ID). `progress(phase, percent)`, with phase
/// "scanner" (downloading Grype), "database" or "scanning".
pub fn scan(name: &str, progress: &mut dyn FnMut(&str, u8)) -> io::Result<Value> {
    let store = kiri_image::Store::open(root()?)?;
    let (i, ..) = crate::builds::stored(&store, name)?;
    {
        let mut r = RUNNING.lock().unwrap();
        if !r
            .get_or_insert_with(HashSet::new)
            .insert(i.config_digest.clone())
        {
            return Err(io::Error::other("this image is being scanned already"));
        }
    }
    let result = (|| {
        ensure_grype(&mut |pct| progress("scanner", pct))?;
        progress("database", 0);
        let has_db = root()?
            .join("scanner/db")
            .read_dir()
            .is_ok_and(|mut d| d.next().is_some());
        // Offline with a database already here: scan with what's here, and
        // Grype says so itself if it's too old.
        if let Err(e) = run(
            command(&["db", "update", "-q"])?,
            "Could not update the vulnerability database",
        ) {
            if !has_db {
                return Err(e);
            }
        }
        progress("scanning", 0);
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let dir = scan_dir(&i.config_digest)?.join(at.to_string());
        std::fs::create_dir_all(&dir)?;
        let tmp = store.root().join("tmp").join(format!(
            "scan-{}-{}",
            std::process::id(),
            kiri_image::store::digest_hex(&i.config_digest)?
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        layout(&store, &i, &tmp)?;
        let report = dir.join("grype.json.part");
        let sbom = dir.join("sbom.cdx.json.part");
        let target = format!("oci-dir:{}", tmp.display());
        let out = (|| {
            let mut c = command(&[
                "-q",
                &target,
                "-o",
                &format!("json={}", report.display()),
                "-o",
                &format!("cyclonedx-json={}", sbom.display()),
            ])?;
            c.env("GRYPE_DB_AUTO_UPDATE", "false");
            run(c, "The scan failed")
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        if let Err(e) = out {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        std::fs::rename(&report, dir.join("grype.json"))?;
        std::fs::rename(&sbom, dir.join("sbom.cdx.json"))?;
        // Only the newest KEEP.
        let all = scan_dir(&i.config_digest)?;
        for old in scan_times(&all).into_iter().skip(KEEP) {
            let _ = std::fs::remove_dir_all(all.join(old.to_string()));
        }
        open(name, Some(at))?.ok_or_else(|| io::Error::other("the scan wrote no report"))
    })();
    RUNNING
        .lock()
        .unwrap()
        .as_mut()
        .map(|r| r.remove(&i.config_digest));
    result
}

/// The image's scans for the history list, newest first: when, the count
/// per severity, how many have a fix. Each summary is worked out once and kept.
pub fn history(name: &str) -> io::Result<Value> {
    let store = kiri_image::Store::open(root()?)?;
    let (i, ..) = crate::builds::stored(&store, name)?;
    let all = scan_dir(&i.config_digest)?;
    let mut list = Vec::new();
    for at in scan_times(&all) {
        let path = all.join(at.to_string()).join("summary.json");
        let summary = match std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        {
            Some(v) => v,
            None => {
                let Some(full) = open(name, Some(at))? else {
                    continue;
                };
                let mut counts = serde_json::Map::new();
                for v in full["vulns"].as_array().into_iter().flatten() {
                    let k = v["severity"].as_str().unwrap_or("Unknown").to_owned();
                    let n = counts.get(&k).and_then(Value::as_u64).unwrap_or(0);
                    counts.insert(k, json!(n + 1));
                }
                let vulns = full["vulns"].as_array().map_or(0, Vec::len);
                let fixable = full["vulns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|v| v["fixed"].as_array().is_some_and(|f| !f.is_empty()))
                    .count();
                let v = json!({"scanned": at, "counts": counts, "vulns": vulns, "fixable": fixable, "packages": full["packages"], "db": full["db"], "grype": full["grype"]});
                let _ = std::fs::write(&path, v.to_string());
                v
            }
        };
        list.push(summary);
    }
    Ok(json!({"id": i.config_digest, "scans": list}))
}

/// One of the image's scans in full (the newest when `at` is None), or None
/// if it has none.
pub fn open(name: &str, at: Option<u64>) -> io::Result<Option<Value>> {
    let store = kiri_image::Store::open(root()?)?;
    let (i, manifest, config) = crate::builds::stored(&store, name)?;
    let all = scan_dir(&i.config_digest)?;
    let Some(when) = at.or_else(|| scan_times(&all).first().copied()) else {
        return Ok(None);
    };
    let dir = all.join(when.to_string());
    let Ok(raw) = std::fs::read(dir.join("grype.json")) else {
        return Ok(None);
    };
    let report: Value = serde_json::from_slice(&raw).map_err(io::Error::other)?;
    let packages = std::fs::read(dir.join("sbom.cdx.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|s| s["components"].as_array().map(|c| c.len()))
        .unwrap_or(0);
    let s = summarize(
        &report,
        &image_lines(&manifest, &config, &i.platform),
        &config,
    );
    let mut links = LINKS.lock().unwrap();
    let links = links.get_or_insert_with(HashSet::new);
    for v in s["vulns"].as_array().into_iter().flatten() {
        if let Some(u) = v["url"].as_str() {
            links.insert(u.to_owned());
        }
    }
    Ok(Some(
        json!({"id": i.config_digest, "scanned": when, "packages": packages, "db": report["descriptor"]["db"]["status"]["built"], "grype": GRYPE.0, "vulns": s["vulns"]}),
    ))
}

/// The layer lines with a layer, in order, as builds::image pairs them.
fn image_lines(manifest: &Value, config: &Value, platform: &str) -> Vec<Value> {
    let im = crate::builds::image(manifest, config, Some(platform));
    im["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|l| l["empty"] != true)
        .cloned()
        .collect()
}

/// One row per vulnerability and package, worst first, each with the layer
/// it's in: Grype names the layer by its diff ID, the config's rootfs order.
fn summarize(report: &Value, lines: &[Value], config: &Value) -> Value {
    let diff_ids: Vec<&str> = config["rootfs"]["diff_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let rank = |s: &str| {
        ["Critical", "High", "Medium", "Low", "Negligible", "Unknown"]
            .iter()
            .position(|x| *x == s)
            .unwrap_or(9)
    };
    let mut seen = HashSet::new();
    let mut vulns: Vec<Value> = Vec::new();
    for m in report["matches"].as_array().into_iter().flatten() {
        let (v, a) = (&m["vulnerability"], &m["artifact"]);
        let key = format!("{}|{}|{}", v["id"], a["name"], a["version"]);
        if !seen.insert(key) {
            continue;
        }
        let layer = a["locations"][0]["layerID"]
            .as_str()
            .and_then(|d| diff_ids.iter().position(|x| *x == d));
        let line = layer.and_then(|n| lines.get(n));
        let url = v["dataSource"]
            .as_str()
            .filter(|u| u.starts_with("https://"))
            .unwrap_or_default();
        vulns.push(json!({
            "id": v["id"], "severity": v["severity"].as_str().unwrap_or("Unknown"),
            "package": a["name"], "version": a["version"], "kind": a["type"],
            "fixed": if v["fix"]["state"] == "fixed" { v["fix"]["versions"].clone() } else { json!([]) },
            "url": url, "description": v["description"],
            "layer": layer, "by": line.map(|l| l["by"].clone()).unwrap_or(Value::Null),
            "base": line.map(|l| l["base"] == true),
        }));
    }
    vulns.sort_by(|a, b| {
        let fix = |v: &Value| v["fixed"].as_array().is_none_or(|f| f.is_empty());
        rank(a["severity"].as_str().unwrap_or_default())
            .cmp(&rank(b["severity"].as_str().unwrap_or_default()))
            .then(fix(a).cmp(&fix(b)))
            .then(a["package"].as_str().cmp(&b["package"].as_str()))
    });
    json!({"vulns": vulns})
}

/// One scan's SBOM, where it's kept.
pub fn sbom(name: &str, at: u64) -> io::Result<PathBuf> {
    let store = kiri_image::Store::open(root()?)?;
    let (i, ..) = crate::builds::stored(&store, name)?;
    let from = scan_dir(&i.config_digest)?
        .join(at.to_string())
        .join("sbom.cdx.json");
    if !from.is_file() {
        return Err(io::Error::other("scan the image first"));
    }
    Ok(from)
}

/// Saves one scan's SBOM to Downloads as `<file>.cdx.json`, and returns where.
pub fn save_sbom(name: &str, at: u64, file: &str) -> io::Result<PathBuf> {
    let from = sbom(name, at)?;
    let stem: String = file
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem = if stem.trim_matches(['-', '.']).is_empty() {
        "image".to_owned()
    } else {
        stem
    };
    let downloads = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?
        .join("Downloads");
    let mut to = downloads.join(format!("{stem}.cdx.json"));
    let mut n = 2;
    while to.exists() {
        to = downloads.join(format!("{stem} {n}.cdx.json"));
        n += 1;
    }
    std::fs::copy(&from, &to)?;
    Ok(to)
}

/// Opens an advisory page, only if a scan listed it.
pub fn open_advisory(url: &str) {
    if LINKS
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|l| l.contains(url))
    {
        let _ = Command::new("/usr/bin/open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_name_their_layer_worst_first() {
        let report = json!({"matches": [
            {"vulnerability": {"id": "CVE-1", "severity": "Medium", "fix": {"state": "not-fixed"}, "dataSource": "https://x/1"},
             "artifact": {"name": "zlib", "version": "1", "type": "apk", "locations": [{"layerID": "sha256:d0"}]}},
            {"vulnerability": {"id": "CVE-2", "severity": "Critical", "fix": {"state": "fixed", "versions": ["2"]}, "dataSource": "http://plain"},
             "artifact": {"name": "app", "version": "1", "type": "npm", "locations": [{"layerID": "sha256:d1"}]}},
            {"vulnerability": {"id": "CVE-2", "severity": "Critical", "fix": {"state": "fixed", "versions": ["2"]}},
             "artifact": {"name": "app", "version": "1", "type": "npm", "locations": [{"layerID": "sha256:d1"}]}},
        ]});
        let lines = [
            json!({"by": "ADD rootfs /", "base": true}),
            json!({"by": "COPY . /app", "base": false}),
        ];
        let config = json!({"rootfs": {"diff_ids": ["sha256:d0", "sha256:d1"]}});
        let v = summarize(&report, &lines, &config)["vulns"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(v.len(), 2, "the same CVE in the same package once");
        assert_eq!(
            (
                v[0]["id"].as_str(),
                v[0]["base"].as_bool(),
                v[0]["by"].as_str()
            ),
            (Some("CVE-2"), Some(false), Some("COPY . /app"))
        );
        assert_eq!(v[0]["url"], "", "only https advisories");
        assert_eq!(
            (
                v[1]["layer"].as_u64(),
                v[1]["base"].as_bool(),
                v[1]["fixed"].as_array().map(Vec::len)
            ),
            (Some(0), Some(true), Some(0))
        );
    }

    /// A real scan of an image in a real store, Grype and its database downloaded on first use:
    /// `KIRI_LIVE_IMAGE=alpine:latest KIRI_DATA_DIR=... cargo test -p kiri scan_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn scan_live() {
        let name = std::env::var("KIRI_LIVE_IMAGE").unwrap();
        let r = scan(&name, &mut |phase, pct| {
            if pct % 25 == 0 {
                eprintln!("{phase} {pct}%")
            }
        })
        .unwrap();
        println!("{}", history(&name).unwrap());
        if let Some(out) = std::env::var_os("KIRI_LIVE_OUT") {
            std::fs::write(out, r.to_string()).unwrap();
        }
        let v = r["vulns"].as_array().unwrap();
        println!(
            "{} vulnerabilities, {} packages, database {}",
            v.len(),
            r["packages"],
            r["db"]
        );
        for x in v.iter().take(5) {
            println!(
                "  {} {} {} {} fixed {} layer {} base {}",
                x["severity"],
                x["id"],
                x["package"],
                x["version"],
                x["fixed"],
                x["layer"],
                x["base"]
            );
        }
    }
}
