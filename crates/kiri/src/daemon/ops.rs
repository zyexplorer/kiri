//! The Engine API beyond create/start/stop: stats, top, pause, update,
//! rename, diff, export, commit, prune, system df, history, search, events
//! and network connect.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::api::{Next, copy_error, error, filters, guest_frame, reply};
use super::engine::{Engine, Error, Status};
use super::http::{self, Request};
use super::time;

/// USER_HZ: /proc/stat counts in hundredths of a second.
const TICK_NS: u64 = 10_000_000;

/// Routes the requests this module serves; `None` if it serves none.
pub fn route(req: &Request, engine: &Arc<Engine>, w: &mut UnixStream) -> Option<io::Result<Next>> {
    let p = req.path.as_str();
    let m = req.method.as_str();
    Some(match (m, p) {
        ("GET", "/events") => events(req, engine, w),
        ("GET", "/system/df") => reply(w, system_df(engine), 200),
        ("POST", "/containers/prune") => reply(w, container_prune(req, engine), 200),
        ("POST", "/images/prune") => reply(w, image_prune(req, engine), 200),
        ("GET", "/images/search") => reply(w, search(req), 200),
        ("POST", "/commit") => reply(w, commit(req, engine), 201),
        _ => {
            if let Some(rest) = p.strip_prefix("/containers/") {
                let (id, action) = rest.split_once('/')?;
                return Some(match (m, action) {
                    ("GET", "stats") => stats(req, engine, id, w),
                    ("GET", "top") => reply(w, top(req, engine, id), 200),
                    ("POST", "pause") => reply(w, engine.pause(id, true).map(|()| Value::Null), 204),
                    ("POST", "unpause") => reply(w, engine.pause(id, false).map(|()| Value::Null), 204),
                    ("POST", "rename") => reply(w, engine.rename(id, req.query("name").unwrap_or_default()).map(|()| Value::Null), 204),
                    ("POST", "update") => reply(w, update(req, engine, id), 200),
                    ("GET", "changes") => reply(w, changes(engine, id), 200),
                    ("GET", "export") => export(engine, id, w),
                    _ => return None,
                });
            }
            if let Some(rest) = p.strip_prefix("/images/") {
                if let (Some(name), "GET") = (rest.strip_suffix("/history"), m) {
                    return Some(reply(w, history(engine, name), 200));
                }
                return None;
            }
            if let Some(rest) = p.strip_prefix("/networks/") {
                let (id, action) = rest.split_once('/')?;
                return Some(match (m, action) {
                    ("POST", "connect") => reply(w, connect(req, engine, id), 200),
                    ("POST", "disconnect") => reply(w, disconnect(req, engine, id), 200),
                    _ => return None,
                });
            }
            return None;
        }
    })
}

fn body(req: &Request) -> Result<Value, Error> {
    if req.body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))
}

/// One sample as Docker's stats JSON; `pre` is the previous one.
fn stats_json(id: &str, name: &str, s: &kiri_proto::ContainerStats, pre: Option<&kiri_proto::ContainerStats>, ceiling: u64) -> Value {
    let cpu = |s: &kiri_proto::ContainerStats| {
        json!({
            "cpu_usage": {"total_usage": s.cpu_usage_usec * 1000, "usage_in_kernelmode": s.cpu_system_usec * 1000, "usage_in_usermode": s.cpu_user_usec * 1000},
            "system_cpu_usage": s.system_ticks * TICK_NS,
            "online_cpus": s.online_cpus,
            "throttling_data": {"periods": 0, "throttled_periods": 0, "throttled_time": 0},
        })
    };
    let read = time::rfc3339(std::time::SystemTime::now());
    json!({
        "id": id,
        "name": format!("/{name}"),
        "read": read,
        "preread": if pre.is_some() { read.clone() } else { time::ZERO.to_owned() },
        "pids_stats": {"current": s.pids},
        "cpu_stats": cpu(s),
        "precpu_stats": pre.map_or_else(|| json!({"cpu_usage": {"total_usage": 0}, "throttling_data": {}}), cpu),
        "memory_stats": {
            "usage": s.mem_current,
            "limit": if s.mem_max == 0 { ceiling } else { s.mem_max },
            "stats": {"inactive_file": s.mem_inactive_file},
        },
        "networks": {"eth0": {"rx_bytes": s.rx_bytes, "rx_packets": s.rx_packets, "rx_errors": 0, "rx_dropped": 0, "tx_bytes": s.tx_bytes, "tx_packets": s.tx_packets, "tx_errors": 0, "tx_dropped": 0}},
        "blkio_stats": {"io_service_bytes_recursive": [
            {"major": 254, "minor": 0, "op": "read", "value": s.io_read_bytes},
            {"major": 254, "minor": 0, "op": "write", "value": s.io_write_bytes},
        ]},
        "num_procs": 0,
        "storage_stats": {},
    })
}

/// `docker stats`: a sample a second while streaming, else one with a
/// short look back so the CPU figure means something.
fn stats(req: &Request, engine: &Engine, key: &str, w: &mut UnixStream) -> io::Result<Next> {
    let stream = req.query("stream").is_none_or(|v| v == "1" || v == "true");
    let one_shot = req.flag("one-shot");
    let ceiling = engine.memory().0;
    let first = match engine.stats(key) {
        Ok(v) => v,
        Err(e) => return reply(w, Err(e), 200),
    };
    if !stream {
        let (id, name, a) = first;
        if one_shot {
            return reply(w, Ok(stats_json(&id, &name, &a, None, ceiling)), 200);
        }
        std::thread::sleep(Duration::from_millis(500));
        let b = engine.stats(&id).map(|(_, _, s)| s).unwrap_or(a);
        return reply(w, Ok(stats_json(&id, &name, &b, Some(&a), ceiling)), 200);
    }
    http::begin_stream(w, 200, "application/json")?;
    let (id, name, mut prev) = first;
    http::chunk(w, format!("{}\n", stats_json(&id, &name, &prev, None, ceiling)).as_bytes())?;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        // A stopped container ends the stream; a gone client ends the write.
        let Ok((_, _, now)) = engine.stats(&id) else { break };
        http::chunk(w, format!("{}\n", stats_json(&id, &name, &now, Some(&prev), ceiling)).as_bytes())?;
        prev = now;
    }
    http::end(w)?;
    Ok(Next::KeepAlive)
}

fn top(req: &Request, engine: &Engine, key: &str) -> Result<Value, Error> {
    let ps_args = req.query("ps_args").unwrap_or_default();
    super::top::check(ps_args)?;
    super::top::answer(ps_args, &engine.top(key)?)
}

fn update(req: &Request, engine: &Engine, key: &str) -> Result<Value, Error> {
    let b = body(req)?;
    let base = engine.with(key, |c| c.spec.limits)?;
    let limits = super::api::limits_from(&b, base);
    let restart = b["RestartPolicy"]["Name"].as_str().filter(|n| !n.is_empty()).map(|n| super::engine::RestartPolicy {
        name: n.to_owned(),
        max_retries: b["RestartPolicy"]["MaximumRetryCount"].as_u64().unwrap_or(0) as u32,
    });
    engine.update(key, limits, restart)?;
    Ok(json!({"Warnings": []}))
}

/// Starts a copy-service job and checks it accepted: `CopyDone` first.
fn layer_job(engine: &Engine, key: &str, op: u8) -> Result<(kiri_vmm::VsockStream, std::fs::File), Error> {
    let stream = engine.copy(key, op, "/", 0)?;
    let mut f = std::fs::File::from(stream.fd.try_clone()?);
    let frame = guest_frame(&mut f)?;
    match kiri_proto::decode(&frame) {
        Ok(kiri_proto::Msg::CopyDone) => Ok((stream, f)),
        Ok(kiri_proto::Msg::Error(m)) => Err(copy_error(m)),
        other => Err(Error::Io(io::Error::other(format!("unexpected copy reply: {other:?}")))),
    }
}

/// `docker diff`: Kind 0 changed, 1 added, 2 deleted.
fn changes(engine: &Engine, key: &str) -> Result<Value, Error> {
    let (_stream, mut f) = layer_job(engine, key, kiri_proto::copy_op::DIFF)?;
    let mut text = String::new();
    f.read_to_string(&mut text)?;
    let list: Vec<Value> = text
        .lines()
        .filter_map(|l| {
            let (kind, path) = l.split_once(' ')?;
            let kind = match kind {
                "C" => 0,
                "A" => 1,
                "D" => 2,
                _ => return None,
            };
            Some(json!({"Path": path, "Kind": kind}))
        })
        .collect();
    Ok(if list.is_empty() { Value::Null } else { Value::Array(list) })
}

fn export(engine: &Engine, key: &str, w: &mut UnixStream) -> io::Result<Next> {
    let (stream, mut f) = match layer_job(engine, key, kiri_proto::copy_op::EXPORT) {
        Ok(v) => v,
        Err(e) => return reply(w, Err(e), 200),
    };
    http::begin_stream(w, 200, "application/x-tar")?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        http::chunk(w, &buf[..n])?;
    }
    http::end(w)?;
    drop(stream);
    Ok(Next::KeepAlive)
}

/// Applies one `--change` line (a Dockerfile instruction) to a config.
pub(super) fn apply_change(cfg: &mut Value, line: &str) -> Result<(), Error> {
    let (instr, arg) = line.trim().split_once(char::is_whitespace).unwrap_or((line.trim(), ""));
    let arg = arg.trim();
    // JSON form, or shell form run by /bin/sh -c.
    let words = |a: &str| -> Value {
        match serde_json::from_str::<Vec<String>>(a) {
            Ok(v) => json!(v),
            Err(_) => json!(["/bin/sh", "-c", a]),
        }
    };
    let c = &mut cfg["config"];
    match instr.to_ascii_uppercase().as_str() {
        "CMD" => c["Cmd"] = words(arg),
        "ENTRYPOINT" => c["Entrypoint"] = words(arg),
        "WORKDIR" => c["WorkingDir"] = arg.into(),
        "USER" => c["User"] = arg.into(),
        "ENV" => {
            let (k, v) = arg.split_once('=').or_else(|| arg.split_once(' ')).unwrap_or((arg, ""));
            let mut env: Vec<Value> = c["Env"].as_array().cloned().unwrap_or_default();
            env.retain(|e| e.as_str().and_then(|e| e.split('=').next()) != Some(k));
            env.push(format!("{k}={}", v.trim_matches('"')).into());
            c["Env"] = env.into();
        }
        "EXPOSE" => {
            for port in arg.split_whitespace() {
                let key = if port.contains('/') { port.to_owned() } else { format!("{port}/tcp") };
                if !c["ExposedPorts"].is_object() {
                    c["ExposedPorts"] = json!({});
                }
                c["ExposedPorts"][key] = json!({});
            }
        }
        "LABEL" => {
            let (k, v) = arg.split_once('=').unwrap_or((arg, ""));
            if !c["Labels"].is_object() {
                c["Labels"] = json!({});
            }
            c["Labels"][k.trim_matches('"')] = v.trim_matches('"').into();
        }
        "VOLUME" => {
            if !c["Volumes"].is_object() {
                c["Volumes"] = json!({});
            }
            for v in serde_json::from_str::<Vec<String>>(arg).unwrap_or_else(|_| arg.split_whitespace().map(str::to_owned).collect()) {
                c["Volumes"][v] = json!({});
            }
        }
        other => return Err(Error::BadRequest(format!("{other} is not a valid change command"))),
    }
    Ok(())
}

/// `docker commit`: the container's writable layer on top of its image.
fn commit(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let key = req.query("container").unwrap_or_default();
    let (id, spec, running) = engine.with(key, |c| (c.id.clone(), c.spec.clone(), c.status == Status::Running))?;
    let base = engine
        .store
        .images()?
        .into_iter()
        .find(|i| i.config_digest == spec.image_id)
        .ok_or_else(|| Error::NotFound(format!("the image of container {} is gone", &id[..12])))?;
    let mut cfg: Value = serde_json::from_slice(&engine.store.read_config(&base.config_digest)?).map_err(|e| Error::Io(io::Error::other(e)))?;
    // What the container ran with becomes the image's config.
    cfg["config"]["Cmd"] = json!(spec.cmd);
    cfg["config"]["Entrypoint"] = if spec.entrypoint.is_empty() { Value::Null } else { json!(spec.entrypoint) };
    cfg["config"]["Env"] = json!(spec.env);
    cfg["config"]["WorkingDir"] = spec.cwd.clone().into();
    cfg["config"]["User"] = spec.user.clone().into();
    if spec.labels.is_object() {
        cfg["config"]["Labels"] = spec.labels.clone();
    }
    if let Some(h) = &spec.healthcheck {
        cfg["config"]["Healthcheck"] = h.to_json();
    }
    if let Value::Object(overrides) = body(req)? {
        for (k, v) in overrides {
            if !v.is_null() {
                cfg["config"][k] = v;
            }
        }
    }
    for change in req.queries("changes") {
        for line in change.lines().filter(|l| !l.trim().is_empty()) {
            apply_change(&mut cfg, line)?;
        }
    }
    // Docker pauses a running container so the layer is consistent.
    let pause = running && req.query("pause").is_none_or(|v| v != "0" && v != "false") && !engine.is_paused(&id);
    if pause {
        engine.pause(&id, true)?;
    }
    let layer = (|| -> Result<(String, u64), Error> {
        let (_stream, mut f) = layer_job(engine, &id, kiri_proto::copy_op::COMMIT)?;
        let mut w = engine.store.ingest()?;
        io::copy(&mut f, &mut w)?;
        Ok(w.finish()?)
    })();
    if pause {
        let _ = engine.pause(&id, false);
    }
    // An uncompressed layer: its digest is its diff id.
    let (digest, size) = layer?;
    if !cfg["rootfs"]["diff_ids"].is_array() {
        cfg["rootfs"] = json!({"type": "layers", "diff_ids": []});
    }
    cfg["rootfs"]["diff_ids"].as_array_mut().expect("array").push(digest.clone().into());
    let now = time::rfc3339(std::time::SystemTime::now());
    let comment = req.query("comment").unwrap_or_default();
    let mut hist = cfg["history"].as_array().cloned().unwrap_or_default();
    hist.push(json!({"created": now, "created_by": format!("docker commit {}", &id[..12]), "comment": comment}));
    cfg["history"] = hist.into();
    cfg["created"] = now.into();
    if let Some(a) = req.query("author").filter(|a| !a.is_empty()) {
        cfg["author"] = a.into();
    }
    let bytes = serde_json::to_vec(&cfg).map_err(|e| Error::Io(io::Error::other(e)))?;
    let mut w = engine.store.ingest()?;
    io::Write::write_all(&mut w, &bytes)?;
    let (config_digest, _) = w.finish()?;
    let mut layers = base.layers.clone();
    layers.push(kiri_image::Layer { digest, size, media_type: "application/vnd.oci.image.layer.v1.tar".into() });
    let repo = req.query("repo").unwrap_or_default();
    let name = if repo.is_empty() {
        config_digest.clone()
    } else {
        let tag = req.query("tag").filter(|t| !t.is_empty()).unwrap_or("latest");
        kiri_image::Reference::parse(&format!("{repo}:{tag}")).map(|r| r.short()).map_err(|e| Error::BadRequest(format!("invalid reference format: {e}")))?
    };
    let image = kiri_image::archive::from_config(name, &config_digest, &cfg, layers);
    engine.store.put_image(image)?;
    engine.emit_container("commit", &id, &[]);
    Ok(json!({"Id": config_digest}))
}

/// A prune's `until` filter, as Docker's daemon reads it (moby
/// `getUntilFromPruneFilters`): one value, a duration, date or Unix time
/// (see `time::docker_timestamp`); anything else is refused, never ignored.
/// The cut-off in Unix seconds: only what was created at or before it goes.
pub(super) fn prune_until(f: &super::filters::Filters, key: &str) -> Result<Option<f64>, Error> {
    let Some(v) = f.values(key) else { return Ok(None) };
    let mut v = v.to_vec();
    v.sort();
    v.dedup();
    if v.len() > 1 {
        return Err(Error::BadRequest(format!("more than one {key} filter specified")));
    }
    match time::docker_timestamp(&v[0], std::time::SystemTime::now()) {
        Some(t) => Ok(Some(t)),
        None => Err(Error::BadRequest(format!("failed to parse value as time or duration: {:?}", v[0]))),
    }
}

/// Created at or before the `until` cut-off (Docker skips what was created after it).
pub(super) fn before(until: Option<f64>, created_secs: f64) -> bool {
    until.is_none_or(|u| created_secs <= u)
}

fn container_prune(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = filters(req, &["until", "label", "label!"])?;
    let until = prune_until(&f, "until")?;
    let doomed = engine.each(|c| {
        let labels = if c.spec.labels.is_object() { c.spec.labels.clone() } else { json!({}) };
        let created = c.created.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        (c.status != Status::Running && f.labels(&labels) && before(until, created)).then(|| c.id.clone())
    });
    let mut deleted = Vec::new();
    for id in doomed {
        if engine.remove(&id, false, false).is_ok() {
            deleted.push(id);
        }
    }
    Ok(json!({"ContainersDeleted": deleted, "SpaceReclaimed": 0}))
}

fn image_prune(req: &Request, engine: &Engine) -> Result<Value, Error> {
    // While the container list may be missing containers, "unused" can't
    // be told; say so rather than quietly pruning less.
    if let Some(why) = engine.refusal("Images were not pruned, because the container list may be missing containers that use them") {
        return Err(Error::Conflict(why));
    }
    let f = filters(req, &["dangling", "until", "label", "label!"])?;
    // Without `-a` only untagged images go; Kiri keeps none of those around.
    let dangling_only = f.bool("dangling").unwrap_or(true);
    let until = prune_until(&f, "until")?;
    let used: Vec<String> = engine.each(|c| Some(c.spec.image_id.clone()));
    let mut deleted = Vec::new();
    let mut reclaimed = 0u64;
    for i in engine.store.images()? {
        if used.contains(&i.config_digest) || (dangling_only && !i.name.starts_with("sha256:")) {
            continue;
        }
        // Docker's rules (moby daemon/images/image_prune.go): the image
        // config's labels must match `label` and not `label!`, and with
        // `until` an image without a creation time stays.
        let cfg = super::api::image_config_json(engine, &i.config_digest);
        let created = cfg["created"].as_str().and_then(|c| time::docker_timestamp(c, std::time::SystemTime::now()));
        if !f.labels(&super::api::image_labels_from(&cfg)) || (until.is_some() && !created.is_some_and(|c| before(until, c))) {
            continue;
        }
        if engine.store.remove_image(&i.name)?.is_some() {
            reclaimed += i.size;
            deleted.push(json!({"Untagged": i.name}));
            engine.emit_image("untag", &i.config_digest, &i.name);
            if !engine.store.images()?.iter().any(|o| o.config_digest == i.config_digest) {
                deleted.push(json!({"Deleted": i.config_digest}));
                engine.emit_image("delete", &i.config_digest, &i.name);
            }
        }
    }
    Ok(json!({"ImagesDeleted": if deleted.is_empty() { Value::Null } else { Value::Array(deleted) }, "SpaceReclaimed": reclaimed}))
}

/// What the guest's storage takes on disk, by entry (`Msg::Usage`).
#[derive(Default)]
struct DiskUsage {
    /// Unpacked layers by blob hex, half-done unpacks included.
    layers: HashMap<String, u64>,
    /// Writable layers by container id.
    containers: HashMap<String, u64>,
    volumes: HashMap<String, u64>,
}

impl DiskUsage {
    fn parse(lines: &[String]) -> DiskUsage {
        let mut du = DiskUsage::default();
        for l in lines {
            let mut f = l.split(' ');
            let (Some(kind), Some(name), Some(Ok(bytes)), None) = (f.next(), f.next(), f.next().map(str::parse::<u64>), f.next()) else { continue };
            let map = match kind {
                "l" => &mut du.layers,
                "c" => &mut du.containers,
                "v" => &mut du.volumes,
                _ => continue,
            };
            map.insert(name.to_owned(), bytes);
        }
        du
    }
}

/// `docker system df`: sizes as they are on the engine's disk, measured by
/// the guest. A layer not unpacked yet counts at its download size; if the
/// guest doesn't answer, so does every layer, and containers and volumes go
/// unmeasured, as before.
fn system_df(engine: &Engine) -> Result<Value, Error> {
    let images = engine.store.images()?;
    let used: Vec<String> = engine.each(|c| Some(c.spec.image_id.clone()));
    let measured = engine.vm().ok().and_then(|vm| vm.usage(std::time::Duration::from_secs(120)));
    let du = measured.as_deref().map(DiskUsage::parse);
    let layer_size = |l: &kiri_image::Layer| du.as_ref().and_then(|d| d.layers.get(l.digest.trim_start_matches("sha256:")).copied()).unwrap_or(l.size);
    // How many images use each layer, for each image's shared size.
    let mut users: HashMap<&str, usize> = HashMap::new();
    for i in &images {
        let mut mine: Vec<&str> = i.layers.iter().map(|l| l.digest.as_str()).collect();
        mine.dedup();
        for d in mine {
            *users.entry(d).or_default() += 1;
        }
    }
    // Every image layer once, as `layer_size` counts it; with the guest's
    // numbers, plus the rest of the layer store: layers only stopped
    // containers still use, and leftovers the next VM start clears.
    let mut seen = HashSet::new();
    let mut layers_size: u64 = images.iter().flat_map(|i| &i.layers).filter(|l| seen.insert(l.digest.trim_start_matches("sha256:"))).map(layer_size).sum();
    if let Some(d) = &du {
        layers_size += d.layers.iter().filter(|(hex, _)| !seen.contains(hex.as_str())).map(|(_, b)| b).sum::<u64>();
    }
    let image_size = |i: &kiri_image::Image| -> u64 { i.layers.iter().map(layer_size).sum() };
    let labels = engine.image_labels(&images);
    let image_rows: Vec<Value> = images
        .iter()
        .zip(labels)
        .map(|(i, labels)| {
            let size = image_size(i);
            let shared: u64 = i.layers.iter().filter(|l| users.get(l.digest.as_str()).is_some_and(|n| *n > 1)).map(layer_size).sum();
            json!({
                "Id": i.config_digest,
                "ParentId": "",
                "RepoTags": [i.name],
                "RepoDigests": [],
                "Created": time::parse_unix(&i.created),
                "Size": size,
                "SharedSize": shared,
                "VirtualSize": size,
                "Labels": labels,
                "Containers": used.iter().filter(|u| **u == i.config_digest).count(),
            })
        })
        .collect();
    let containers = engine.each(|c| {
        let rw = du.as_ref().and_then(|d| d.containers.get(&c.id).copied()).unwrap_or(0);
        let image = images.iter().find(|i| i.config_digest == c.spec.image_id).map_or(0, image_size);
        Some(json!({
            "Id": c.id,
            "Names": [format!("/{}", c.name)],
            "Image": c.spec.image,
            "ImageID": c.spec.image_id,
            "Command": c.argv().join(" "),
            "Created": time::unix(c.created),
            "State": c.status.as_str(),
            "Status": c.status.as_str(),
            "SizeRw": rw,
            "SizeRootFs": rw + image,
            "Labels": c.spec.labels,
        }))
    });
    let volumes: Vec<Value> = engine
        .volume_list()
        .iter()
        .map(|v| {
            let size = du.as_ref().and_then(|d| d.volumes.get(&v.name)).map_or(-1, |b| *b as i64);
            json!({
                "Name": v.name,
                "Driver": "local",
                "Mountpoint": super::volumes::guest_path(&v.name),
                "Labels": v.labels,
                "Scope": "local",
                "Options": {},
                "UsageData": {"Size": size, "RefCount": engine.volume_users(&v.name).len()},
            })
        })
        .collect();
    Ok(json!({"LayersSize": layers_size, "Images": image_rows, "Containers": containers, "Volumes": volumes, "BuildCache": super::builder::cache(engine)}))
}

/// `docker history`: the config's history, newest first, with each
/// layer-making step's size.
fn history(engine: &Engine, name: &str) -> Result<Value, Error> {
    let i = engine.image(name)?.ok_or_else(|| Error::NotFound(format!("No such image: {name}")))?;
    let cfg: Value = serde_json::from_slice(&engine.store.read_config(&i.config_digest)?).unwrap_or(Value::Null);
    let mut layers = i.layers.iter();
    let mut rows: Vec<Value> = cfg["history"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| {
            let empty = h["empty_layer"].as_bool().unwrap_or(false);
            let size = if empty { 0 } else { layers.next().map_or(0, |l| l.size) };
            json!({
                "Id": "<missing>",
                "Created": time::parse_unix(h["created"].as_str().unwrap_or_default()),
                "CreatedBy": h["created_by"].as_str().unwrap_or_default(),
                "Tags": null,
                "Size": size,
                "Comment": h["comment"].as_str().unwrap_or_default(),
            })
        })
        .collect();
    rows.reverse();
    if let Some(top) = rows.first_mut() {
        top["Id"] = i.config_digest.clone().into();
        top["Tags"] = json!([i.name]);
    }
    Ok(Value::Array(rows))
}

fn search(req: &Request) -> Result<Value, Error> {
    let term = req.query("term").unwrap_or_default();
    let limit = req.query("limit").and_then(|l| l.parse().ok()).unwrap_or(25);
    let results = kiri_image::Client::new().search(term, limit).map_err(|e| Error::BadRequest(format!("search failed: {e}")))?;
    let f = filters(req, &["is-official", "is-automated", "stars"])?;
    let rows: Vec<Value> = results
        .into_iter()
        .filter(|r| {
            f.bool("is-official").is_none_or(|o| r["is_official"].as_bool().unwrap_or(false) == o)
                && f.any("stars", |s| r["star_count"].as_u64().unwrap_or(0) >= s.parse().unwrap_or(0))
        })
        .map(|r| {
            json!({
                "name": r["name"],
                "description": r["description"],
                "star_count": r["star_count"],
                "is_official": r["is_official"],
                "is_automated": r["is_automated"],
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

fn connect(req: &Request, engine: &Engine, network: &str) -> Result<Value, Error> {
    let b = body(req)?;
    let container = b["Container"].as_str().unwrap_or_default();
    let aliases = b["EndpointConfig"]["Aliases"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
    let (ip, ip6, mac) = super::api::endpoint_address(&b["EndpointConfig"])?;
    engine.connect(network, container, aliases, ip, ip6, mac)?;
    Ok(Value::Null)
}

fn disconnect(req: &Request, engine: &Engine, network: &str) -> Result<Value, Error> {
    let b = body(req)?;
    engine.disconnect(network, b["Container"].as_str().unwrap_or_default())?;
    Ok(Value::Null)
}

/// Seconds since the epoch, from `--since`/`--until` (the CLI sends
/// `1700000000.000000000`).
fn epoch(v: Option<&str>) -> Option<f64> {
    v.filter(|s| !s.is_empty()).and_then(|s| s.parse().ok())
}

fn event_matches(f: &super::filters::Filters, ev: &Value) -> bool {
    let typ = ev["Type"].as_str().unwrap_or_default();
    let action = ev["Action"].as_str().unwrap_or_default();
    let actor = ev["Actor"]["ID"].as_str().unwrap_or_default();
    let attrs = &ev["Actor"]["Attributes"];
    let name = attrs["name"].as_str().unwrap_or_default();
    f.any("type", |v| v == typ)
        && f.any("event", |v| action == v || action.split(':').next() == Some(v))
        && f.any("container", |v| typ == "container" && (actor.starts_with(v) || name == v))
        && f.any("image", |v| attrs["image"].as_str() == Some(v) || (typ == "image" && actor == v))
        && f.any("network", |v| typ == "network" && (actor.starts_with(v) || name == v))
        && f.any("volume", |v| typ == "volume" && actor == v)
        && f.labels(attrs)
}

/// The client hung up: a readable socket that reads nothing.
pub(super) fn gone(w: &UnixStream) -> bool {
    let mut pfd = libc::pollfd { fd: w.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd, no wait.
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    if n <= 0 {
        return false;
    }
    let mut b = [0u8; 1];
    // SAFETY: peeking one byte into a local buffer.
    let r = unsafe { libc::recv(w.as_raw_fd(), b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    r == 0 || pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

/// `docker events`: kept events since `since`, then live ones until `until`.
fn events(req: &Request, engine: &Engine, w: &mut UnixStream) -> io::Result<Next> {
    let f = match filters(req, &["type", "event", "container", "image", "network", "volume", "label", "label!", "daemon", "scope"]) {
        Ok(f) => f,
        Err(e) => {
            error(w, e)?;
            return Ok(Next::KeepAlive);
        }
    };
    let since = epoch(req.query("since"));
    let until = epoch(req.query("until"));
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let (recent, rx) = engine.subscribe();
    http::begin_stream(w, 200, "application/json")?;
    let send = |w: &mut UnixStream, ev: &Value| -> io::Result<()> { http::chunk(w, format!("{ev}\n").as_bytes()) };
    let at = |ev: &Value| ev["timeNano"].as_u64().map_or(0.0, |n| n as f64 / 1e9);
    if let Some(s) = since {
        for ev in recent.iter().filter(|ev| at(ev) >= s && until.is_none_or(|u| at(ev) <= u) && event_matches(&f, ev)) {
            send(w, ev)?;
        }
    }
    loop {
        if until.is_some_and(|u| now() >= u) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => {
                if until.is_some_and(|u| at(&ev) > u) {
                    break;
                }
                if event_matches(&f, &ev) {
                    send(w, &ev)?;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if gone(w) {
                    return Ok(Next::Close);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    http::end(w)?;
    Ok(Next::KeepAlive)
}

#[cfg(test)]
mod disk_usage_tests {
    use super::DiskUsage;

    /// While the container list is damaged an image no listed container
    /// uses may be a lost container's: image prune removes none.
    #[test]
    fn image_prune_removes_nothing_while_the_container_list_is_damaged() {
        use super::{Error, Request};
        let root = std::env::temp_dir().join(format!("kiri-image-prune-damaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("containers.json.corrupt-1-0"), b"[{\"id\":").unwrap();
        let engine = super::super::engine::boot_without_vm(&root);
        let image = kiri_image::Image { mirror: None,
            name: "sha256:0123".into(), digest: "sha256:0123".into(), index_digest: None, config_digest: "sha256:0123".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        engine.store.put_image(image).unwrap();
        match super::image_prune(&Request::internal("POST", "/images/prune", &[], Vec::new()), &engine) {
            Err(Error::Conflict(why)) => assert!(why.starts_with("Images were not pruned") && why.contains("containers.json.corrupt-1-0"), "{why}"),
            other => panic!("image prune: {other:?}"),
        }
        assert_eq!(engine.store.images().unwrap().len(), 1, "the image is kept");
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Image prune keeps what Docker keeps: an image's config labels must
    /// match `label` and not `label!`, and `until` keeps newer images.
    #[test]
    fn image_prune_follows_labels_and_until() {
        use super::{Error, Request};
        use serde_json::{Value, json};
        let root = std::env::temp_dir().join(format!("kiri-image-prune-labels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let engine = super::super::engine::boot_without_vm(&root);
        let mut ids = Vec::new();
        for (tag, created, labels) in [(None, "2026-11-16T00:00:00Z", json!({"keep": "1"})), (None, "2020-01-01T00:00:00Z", Value::Null), (Some("tagged:1"), "2020-01-01T00:00:00Z", Value::Null)] {
            let cfg = json!({"created": created, "architecture": "arm64", "os": "linux", "config": {"Labels": labels}});
            let mut w = engine.store.ingest().unwrap();
            std::io::Write::write_all(&mut w, &serde_json::to_vec(&cfg).unwrap()).unwrap();
            let (digest, _) = w.finish().unwrap();
            engine.store.put_image(kiri_image::archive::from_config(tag.map_or(digest.clone(), str::to_owned), &digest, &cfg, Vec::new())).unwrap();
            ids.push(digest);
        }
        let prune = |f: &str| super::image_prune(&Request::internal("POST", "/images/prune", &[("filters", f)], Vec::new()), &engine);
        let names = || -> Vec<String> { engine.store.images().unwrap().into_iter().map(|i| i.name).collect() };
        let left = |want: &[&str]| {
            let mut got = names();
            got.sort();
            let mut want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            want.sort();
            assert_eq!(got, want);
        };
        let (keep, old) = (ids[0].as_str(), ids[1].as_str());
        prune(r#"{"label":["nope=1"]}"#).unwrap();
        left(&[keep, old, "tagged:1"]);
        prune(r#"{"dangling":["false"],"label":["nope=1"]}"#).unwrap();
        left(&[keep, old, "tagged:1"]);
        assert!(matches!(prune(r#"{"until":["someday"]}"#), Err(Error::BadRequest(_))));
        assert!(matches!(prune(r#"{"until":["1h","2h"]}"#), Err(Error::BadRequest(_))));
        prune(r#"{"until":["2021-01-01T00:00:00Z"]}"#).unwrap();
        left(&[keep, "tagged:1"]);
        prune(r#"{"label!":["keep=1"]}"#).unwrap();
        left(&[keep, "tagged:1"]);
        prune(r#"{"label":["keep=1"]}"#).unwrap();
        left(&["tagged:1"]);
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `system df` image rows carry the image's labels, and `{}` (not
    /// `null`) for an image without any, as Docker's do.
    #[test]
    fn system_df_image_rows_carry_labels() {
        use serde_json::{Value, json};
        let root = std::env::temp_dir().join(format!("kiri-df-labels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let engine = super::super::engine::boot_without_vm(&root);
        for (name, labels) in [("demo:labeled", json!({"maintainer": "someone"})), ("demo:plain", Value::Null)] {
            let cfg = json!({"created": "2026-11-16T00:00:00Z", "architecture": "arm64", "os": "linux", "config": {"Labels": labels}});
            let mut w = engine.store.ingest().unwrap();
            std::io::Write::write_all(&mut w, &serde_json::to_vec(&cfg).unwrap()).unwrap();
            let (digest, _) = w.finish().unwrap();
            engine.store.put_image(kiri_image::archive::from_config(name.into(), &digest, &cfg, Vec::new())).unwrap();
        }
        let df = super::system_df(&engine).unwrap();
        let row = |name: &str| df["Images"].as_array().unwrap().iter().find(|r| r["RepoTags"][0] == name).unwrap()["Labels"].clone();
        assert_eq!(row("demo:labeled"), json!({"maintainer": "someone"}));
        assert_eq!(row("demo:plain"), json!({}));
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reads_each_kind_and_skips_bad_lines() {
        let lines: Vec<String> = ["l ab12 4096", "c c1 8192", "v data 12288", "v bad", "x odd 1", "l cd34 notanumber", "c c2 1 extra"].map(str::to_owned).to_vec();
        let du = DiskUsage::parse(&lines);
        assert_eq!(du.layers.get("ab12"), Some(&4096));
        assert_eq!(du.containers.get("c1"), Some(&8192));
        assert_eq!(du.volumes.get("data"), Some(&12288));
        assert_eq!((du.layers.len(), du.containers.len(), du.volumes.len()), (1, 1, 1));
    }
}
