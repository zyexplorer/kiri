//! Builds: BuildKit's history of `docker build`s on Kiri, for the window's
//! Builds view, read through `docker buildx history` (ls, inspect, logs).
//!
//! Only builders whose BuildKit container already exists on Kiri's engine
//! are asked: asking any other would make buildx create one (and pull
//! moby/buildkit). Asking a stopped one starts it, which wakes the VM, so
//! the window asks only while the Builds view is open.

use std::collections::HashMap;
use std::io;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::flavor::{CONTEXT, Name};

/// Kiri's BuildKit builder, a docker-container one on Kiri's context.
pub const BUILDER: Name = Name("kiri-build", "kiri-dev-build");

fn buildx(args: &[&str]) -> io::Result<(String, String)> {
    let docker = crate::client::docker_cli()
        .ok_or_else(|| io::Error::other("no docker CLI found (Kiri.app bundles one)"))?;
    let mut cmd = Command::new(docker);
    // No console window flashing up from Kiri's window on Windows.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd
        .args(["--context", CONTEXT.get(), "buildx"])
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    if !out.status.success() {
        let msg = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("buildx failed")
            .trim_start_matches("ERROR: ")
            .to_owned();
        return Err(io::Error::other(msg));
    }
    Ok((stdout, stderr))
}

/// Every JSON value in `s`, whether one per line or run together.
fn values(s: &str) -> Vec<Value> {
    serde_json::Deserializer::from_str(s)
        .into_iter::<Value>()
        .map_while(Result::ok)
        .collect()
}

/// The docker-container builders on Kiri's context whose BuildKit container exists.
/// No buildx plugin (a distribution's plain docker CLI) means no builds yet, not an error.
fn builders() -> io::Result<Vec<String>> {
    // Without the plugin, docker reads `buildx` as an unknown command or flag.
    if buildx(&["version"]).is_err() {
        return Ok(Vec::new());
    }
    let (out, _) = buildx(&["ls", "--format", "json"])?;
    let have: Vec<String> = crate::client::get_background("/containers/json?all=1")?
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|c| {
            c["Names"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|n| n.as_str().map(|n| n.trim_start_matches('/').to_owned()))
        })
        .collect();
    Ok(values(&out)
        .into_iter()
        .filter(|b| b["Driver"] == "docker-container")
        .filter(|b| {
            b["Nodes"].as_array().into_iter().flatten().any(|n| {
                n["Endpoint"] == CONTEXT.get()
                    && n["Name"].as_str().is_some_and(|name| {
                        have.iter().any(|c| *c == format!("buildx_buildkit_{name}"))
                    })
            })
        })
        .filter_map(|b| b["Name"].as_str().map(str::to_owned))
        .collect())
}

/// The newest builds first, from every builder: {builder, id, name, status,
/// started, completed, steps, cached}.
pub fn list() -> io::Result<Vec<Value>> {
    let mut all = Vec::new();
    for b in builders()? {
        let Ok((out, _)) = buildx(&["history", "ls", "--builder", &b, "--format", "json"]) else {
            continue;
        };
        for r in values(&out) {
            let id = r["ref"]
                .as_str()
                .unwrap_or_default()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned();
            all.push(json!({
                "builder": b, "id": id, "name": r["name"], "status": r["status"],
                "started": r["created_at"], "completed": r["completed_at"],
                "steps": r["total_steps"], "done": r["completed_steps"], "cached": r["cached_steps"],
            }));
        }
    }
    all.sort_by(|a, b| b["started"].as_str().cmp(&a["started"].as_str()));
    all.truncate(300);
    Ok(all)
}

/// One build: the record, its steps in the order they ran (each with its
/// start, end, cached, error and output) and its warnings.
pub fn detail(builder: &str, id: &str) -> io::Result<Value> {
    if !id.bytes().all(|b| b.is_ascii_alphanumeric()) || builder.starts_with('-') {
        return Err(io::Error::other("not a build"));
    }
    let (info, _) = buildx(&[
        "history",
        "inspect",
        "--builder",
        builder,
        id,
        "--format",
        "json",
    ])?;
    let info: Value = serde_json::from_str(&info).map_err(io::Error::other)?;
    // The raw progress stream goes to stderr.
    let (_, raw) = buildx(&[
        "history",
        "logs",
        "--builder",
        builder,
        "--progress",
        "rawjson",
        id,
    ])
    .unwrap_or_default();

    let mut order: Vec<String> = Vec::new();
    let mut steps: HashMap<String, Value> = HashMap::new();
    let mut logs: HashMap<String, String> = HashMap::new();
    let mut warnings = Vec::new();
    for ev in values(&raw) {
        for v in ev["vertexes"].as_array().into_iter().flatten() {
            let Some(d) = v["digest"].as_str() else {
                continue;
            };
            let s = steps.entry(d.to_owned()).or_insert_with(|| json!({"name": v["name"], "started": null, "completed": null, "cached": false, "error": null}));
            if let Some(t) = v["started"].as_str()
                && (s["started"].is_null() || s["started"].as_str() > Some(t))
            {
                s["started"] = t.into();
                if !order.iter().any(|o| o == d) {
                    order.push(d.to_owned());
                }
            }
            if let Some(t) = v["completed"].as_str()
                && s["completed"].as_str() < Some(t)
            {
                s["completed"] = t.into();
            }
            if v["cached"] == true {
                s["cached"] = true.into();
            }
            if let Some(e) = v["error"].as_str() {
                s["error"] = e.into();
            }
        }
        for l in ev["logs"].as_array().into_iter().flatten() {
            if let (Some(d), Some(data)) = (l["vertex"].as_str(), l["data"].as_str()) {
                let text = logs.entry(d.to_owned()).or_default();
                if text.len() < 256 * 1024 {
                    text.push_str(&String::from_utf8_lossy(&unbase64(data)));
                }
            }
        }
        for w in ev["warnings"].as_array().into_iter().flatten() {
            let text = |v: &Value| {
                String::from_utf8_lossy(&unbase64(v.as_str().unwrap_or_default())).into_owned()
            };
            warnings.push(json!({
                "text": text(&w["short"]),
                "detail": w["detail"].as_array().into_iter().flatten().map(text).collect::<Vec<_>>().join(" "),
                "url": w["url"],
                "line": w["range"][0]["start"]["line"],
            }));
        }
    }
    // Steps that never started (skipped after an error) go last, in the definition's order.
    for d in steps.keys().cloned().collect::<Vec<_>>() {
        if !order.contains(&d) {
            order.push(d);
        }
    }
    let steps: Vec<Value> = order
        .iter()
        .filter_map(|d| {
            let mut s = steps.remove(d)?;
            s["log"] = logs.remove(d).unwrap_or_default().into();
            Some(s)
        })
        .collect();
    let error = &info["Error"];
    Ok(json!({
        "builder": builder, "id": id,
        "name": info["Name"], "status": info["Status"], "started": info["StartedAt"], "completed": info["CompletedAt"],
        "context": info["Context"], "dockerfile": info["Dockerfile"],
        "total": info["NumTotalSteps"], "done": info["NumCompletedSteps"], "cached": info["NumCachedSteps"],
        "error": if error.is_null() { Value::Null } else {
            json!({"message": error["Message"], "step": error["Name"], "source": String::from_utf8_lossy(&unbase64(error["Sources"].as_str().unwrap_or_default()))})
        },
        "steps": steps, "warnings": warnings,
    }))
}

/// The image a build made, per platform: its layers (compressed size, the
/// instruction that made each, whether it came from the base image) and its
/// config, read from the manifest and config BuildKit keeps with the record.
/// Builds that made no image (cache only, or failed) have none.
pub fn layers(builder: &str, id: &str) -> io::Result<Value> {
    if !id.bytes().all(|b| b.is_ascii_alphanumeric()) || builder.starts_with('-') {
        return Err(io::Error::other("not a build"));
    }
    let (info, _) = buildx(&[
        "history",
        "inspect",
        "--builder",
        builder,
        id,
        "--format",
        "json",
    ])?;
    let info: Value = serde_json::from_str(&info).map_err(io::Error::other)?;
    let attachment = |digest: &str| -> io::Result<Value> {
        let ok = digest
            .strip_prefix("sha256:")
            .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        if !ok {
            return Err(io::Error::other("not a digest"));
        }
        let (out, _) = buildx(&[
            "history",
            "inspect",
            "attachment",
            "--builder",
            builder,
            id,
            digest,
        ])?;
        serde_json::from_str(&out).map_err(io::Error::other)
    };
    let mut images = Vec::new();
    for a in info["Attachments"].as_array().into_iter().flatten() {
        if a["Type"] != "application/vnd.oci.image.manifest.v1+json"
            && a["Type"] != "application/vnd.docker.distribution.manifest.v2+json"
        {
            continue;
        }
        let Some(digest) = a["Digest"].as_str() else {
            continue;
        };
        let Ok(manifest) = attachment(digest) else {
            continue;
        };
        let config = manifest["config"]["digest"]
            .as_str()
            .and_then(|d| attachment(d).ok())
            .unwrap_or(Value::Null);
        images.push(image(&manifest, &config, a["Platform"].as_str()));
    }
    Ok(json!({"builder": builder, "id": id, "images": images, "materials": info["Materials"]}))
}

/// An image in Kiri's store, pulled or built, the same way: read from the
/// store's files, so the engine isn't asked and the VM isn't woken. Not on
/// Windows, whose store is inside WSL.
#[cfg(not(windows))]
pub fn image_layers(name: &str) -> io::Result<Value> {
    let store = kiri_image::Store::open(kiri_image::Store::default_root()?)?;
    let (i, manifest, config) = stored(&store, name)?;
    Ok(
        json!({"name": name, "images": [image(&manifest, &config, Some(&i.platform))], "materials": []}),
    )
}

/// An image in the store by name or ID, with a manifest made from its layers
/// and its config.
#[cfg(not(windows))]
pub(crate) fn stored(
    store: &kiri_image::Store,
    name: &str,
) -> io::Result<(kiri_image::Image, Value, Value)> {
    let found = match store.find(name)? {
        Some(i) => Some(i),
        // An untagged image goes by its ID, whole or the window's short form.
        None => {
            let id = name.trim_start_matches("sha256:");
            store.images()?.into_iter().find(|i| {
                !id.is_empty()
                    && i.config_digest
                        .trim_start_matches("sha256:")
                        .starts_with(id)
            })
        }
    };
    let i = found.ok_or_else(|| io::Error::other(format!("no image {name} in Kiri")))?;
    let manifest = json!({"config": {"digest": i.config_digest}, "layers": i.layers.iter().map(|l| json!({"size": l.size, "digest": l.digest})).collect::<Vec<_>>()});
    let config: Value =
        serde_json::from_slice(&store.read_config(&i.config_digest)?).map_err(io::Error::other)?;
    Ok((i, manifest, config))
}

/// One platform's image: the config's history lines paired with the
/// manifest's layers (lines marked `empty_layer` add none).
pub(crate) fn image(manifest: &Value, config: &Value, platform: Option<&str>) -> Value {
    // BuildKit dates every history line a build writes with the image's own
    // time (cached steps too), the classic builder with each step's: a line
    // dated well before the image (an hour) came with the base image. The base's
    // lines come first, so the first newer line ends them: a COPY whose cache
    // dates from an older build is still this image's own.
    let made = crate::daemon::time::parse_unix(config["created"].as_str().unwrap_or_default());
    let mut sizes = manifest["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| (l["size"].as_u64().unwrap_or(0), l["digest"].clone()));
    let mut rows = Vec::new();
    let mut in_base = made > 0;
    for h in config["history"].as_array().into_iter().flatten() {
        let empty = h["empty_layer"] == true;
        let (size, digest) = if empty {
            (0, Value::Null)
        } else {
            sizes.next().unwrap_or((0, Value::Null))
        };
        let created = h["created"].as_str().unwrap_or_default();
        in_base = in_base && crate::daemon::time::parse_unix(created) < made - 3600;
        rows.push(json!({
            "by": h["created_by"].as_str().unwrap_or_default().trim_end_matches(" # buildkit"),
            "size": size, "digest": digest, "empty": empty, "created": created,
            "base": in_base,
        }));
    }
    // Layers the history doesn't describe (a config without history).
    for (size, digest) in sizes {
        rows.push(json!({"by": "", "size": size, "digest": digest, "empty": false, "created": "", "base": false}));
    }
    let c = &config["config"];
    json!({
        "platform": platform.map(str::to_owned).unwrap_or_else(|| format!("{}/{}", config["os"].as_str().unwrap_or("linux"), config["architecture"].as_str().unwrap_or("?"))),
        "size": manifest["layers"].as_array().into_iter().flatten().filter_map(|l| l["size"].as_u64()).sum::<u64>(),
        // The image's ID, to find it among Kiri's images.
        "config_digest": manifest["config"]["digest"],
        "layers": rows,
        "config": {
            "cmd": c["Cmd"], "entrypoint": c["Entrypoint"], "workdir": c["WorkingDir"], "user": c["User"],
            "env": c["Env"], "ports": c["ExposedPorts"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()),
            "labels": c["Labels"], "volumes": c["Volumes"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()),
            "healthcheck": c["Healthcheck"]["Test"], "stopsignal": c["StopSignal"],
        },
    })
}

/// Removes a build from the history.
pub fn remove(builder: &str, id: &str) -> io::Result<()> {
    if !id.bytes().all(|b| b.is_ascii_alphanumeric()) || builder.starts_with('-') {
        return Err(io::Error::other("not a build"));
    }
    buildx(&["history", "rm", "--builder", builder, id]).map(|_| ())
}

/// Standard base64, padding optional; anything else is skipped.
fn unbase64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::with_capacity(s.len() * 3 / 4), 0u32, 0);
    for v in s.bytes().filter_map(val) {
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// buildx's docker-container builder `kiri-build` on the Kiri context,
/// loading every build into Kiri. Its BuildKit is the daemon's managed
/// builder (daemon/builder.rs): the daemon makes that container when buildx
/// first looks for it, starts it on demand and stops it when idle, so this
/// only registers the name with buildx and makes it the builder docker uses
/// for Kiri. Without buildx there is nothing to do: `docker build` sends
/// the classic POST /build, which the daemon runs in the same BuildKit.
/// The window calls this once the daemon answers: the builder setup skips
/// when the daemon is not up yet.
pub fn ensure_builder() {
    let _ = builder();
}

pub fn builder() -> io::Result<bool> {
    let Some(docker) = crate::client::docker_cli() else {
        return Ok(false);
    };
    // buildx talks to the daemon, and can wait on it for good: give the
    // daemon (just started by the LaunchAgent or systemd) a few seconds, else skip.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !crate::client::alive() {
        if std::time::Instant::now() > deadline {
            return Err(io::Error::other("the Kiri daemon isn't answering yet"));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let quiet = || std::process::Stdio::null();
    if !Command::new(&docker)
        .args(["buildx", "version"])
        .stdout(quiet())
        .stderr(quiet())
        .status()?
        .success()
    {
        println!(
            "docker build: no buildx plugin here, so docker builds with Kiri's classic builder API (the same BuildKit, run by the daemon)"
        );
        return Ok(false);
    }
    let inspect = Command::new(&docker)
        .args(["buildx", "inspect", BUILDER.get()])
        .stderr(quiet())
        .output()?;
    let mut exists = inspect.status.success();
    // Without default-load a docker-container builder keeps what it builds
    // to itself: `docker build -t x .` builds and `docker run x` finds
    // nothing. One made without it is made again; --keep-state keeps its
    // cache volume, which the daemon's builder container reuses.
    if exists && !String::from_utf8_lossy(&inspect.stdout).contains("default-load") {
        exists = !Command::new(&docker)
            .args(["buildx", "rm", "--keep-state", BUILDER.get()])
            .stdout(quiet())
            .stderr(quiet())
            .status()?
            .success();
    }
    if !exists {
        // buildx create takes the Kiri context's name; without that context
        // (Linux makes it only in the optional "docker uses Kiri" step) buildx
        // reads the name as a host and fails on tcp://kiri:2375.
        ensure_context(&docker)?;
        let made = Command::new(&docker)
            .args([
                "buildx",
                "create",
                "--name",
                BUILDER.get(),
                "--driver",
                "docker-container",
                "--driver-opt",
                "default-load=true",
                CONTEXT.get(),
            ])
            .stdout(quiet())
            .output()?;
        if !made.status.success() {
            return Err(io::Error::other(failure(
                "docker buildx create failed",
                &made.stderr,
            )));
        }
    }
    // Which builder plain `docker build` uses, Docker's or Kiri's, as
    // chosen (crate::buildwith): it needs this builder to exist.
    if let Err(e) = crate::buildwith::apply() {
        println!("docker build: could not set which builder it uses ({e})");
    }
    Ok(true)
}

/// The docker context named CONTEXT, pointing at Kiri's socket, made when
/// missing. It is only made, never made current: which engine plain docker
/// uses stays the user's choice.
/// On Windows, setup (kiri.ps1) makes the context, on the pipe.
#[cfg(not(unix))]
fn ensure_context(_docker: impl AsRef<std::ffi::OsStr>) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn ensure_context(docker: impl AsRef<std::ffi::OsStr>) -> io::Result<()> {
    let docker = docker.as_ref();
    let quiet = || std::process::Stdio::null();
    if Command::new(docker)
        .args(["context", "inspect", CONTEXT.get()])
        .stdout(quiet())
        .stderr(quiet())
        .status()?
        .success()
    {
        return Ok(());
    }
    let host = format!("host=unix://{}", crate::client::socket()?.display());
    let out = Command::new(docker)
        .args([
            "context",
            "create",
            CONTEXT.get(),
            "--description",
            "Kiri",
            "--docker",
            &host,
        ])
        .stdout(quiet())
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(failure(
            "docker context create failed",
            &out.stderr,
        )))
    }
}

/// What failed, with the tool's own last error line when it gave one.
fn failure(what: &str, stderr: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr);
    match err.lines().map(str::trim).filter(|l| !l.is_empty()).last() {
        Some(line) => format!("{what}: {}", line.trim_start_matches("ERROR: ")),
        None => what.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_carries_the_tools_last_error_line() {
        assert_eq!(
            failure("docker buildx create failed", b""),
            "docker buildx create failed"
        );
        assert_eq!(
            failure("docker buildx create failed", b"Warning: x\nERROR: failed to initialize builder kiri-build (kiri-build0): permission denied\n\n"),
            "docker buildx create failed: failed to initialize builder kiri-build (kiri-build0): permission denied"
        );
    }

    #[test]
    fn pairs_history_with_layers() {
        let manifest = json!({"layers": [{"size": 4000, "digest": "sha256:a"}, {"size": 100, "digest": "sha256:b"}, {"size": 7, "digest": "sha256:c"}]});
        let config = json!({"architecture": "arm64", "os": "linux", "created": "2026-10-28T02:40:31+01:00", "config": {"Cmd": ["sh"]}, "history": [
            {"created": "2026-04-16T23:53:24Z", "created_by": "ADD rootfs.tar.gz / # buildkit"},
            {"created": "2026-04-16T23:53:24Z", "created_by": "CMD [\"/bin/sh\"]", "empty_layer": true},
            {"created": "2026-10-28T01:40:31Z", "created_by": "COPY /app /app # buildkit"},
            {"created": "2026-04-16T23:53:24Z", "created_by": "COPY old.sh / # buildkit"},
        ]});
        let i = image(&manifest, &config, None);
        assert_eq!(i["platform"], "linux/arm64");
        assert_eq!(i["size"], 4107);
        let l = i["layers"].as_array().unwrap();
        assert_eq!(
            (
                l[0]["size"].as_u64(),
                l[0]["base"].as_bool(),
                l[0]["by"].as_str()
            ),
            (Some(4000), Some(true), Some("ADD rootfs.tar.gz /"))
        );
        assert_eq!(
            (l[1]["empty"].as_bool(), l[1]["size"].as_u64()),
            (Some(true), Some(0))
        );
        assert_eq!(
            (l[2]["size"].as_u64(), l[2]["base"].as_bool()),
            (Some(100), Some(false))
        );
        // Old, but after this image's own lines: still its own.
        assert_eq!(l[3]["base"].as_bool(), Some(false));
    }

    #[test]
    fn decodes_base64() {
        assert_eq!(unbase64("YnVpbGRpbmcK"), b"building\n");
        assert_eq!(unbase64("YQ=="), b"a");
        assert_eq!(unbase64("YWI="), b"ab");
    }

    #[test]
    fn reads_values_run_together_or_by_line() {
        assert_eq!(values("{\"a\":1}\n{\"a\":2}").len(), 2);
        assert_eq!(values("{\"a\":1}{\"a\":2}").len(), 2);
    }
}

/// An image in a real store:
/// `KIRI_LIVE_IMAGE=name KIRI_DATA_DIR=... cargo test -p kiri image_layers_live -- --ignored --nocapture`.
/// Not on Windows, where kiri-win includes this file without `image_layers`.
#[cfg(not(windows))]
#[test]
#[ignore]
fn image_layers_live() {
    let name = std::env::var("KIRI_LIVE_IMAGE").unwrap();
    println!(
        "{}",
        serde_json::to_string_pretty(&image_layers(&name).unwrap()).unwrap()
    );
}
