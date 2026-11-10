//! The Docker Engine API: the subset `docker run`, `ps`, `logs`, `stop`,
//! `rm`, `images`, `pull`, `rmi`, `version` and `info` use. Anything else
//! answers 404 with a message saying Kiri does not support it yet.

use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use kiri_image::{Client, Platform, Progress, Reference};
use kiri_proto::Stream;

use super::engine::{Container, Engine, Error, ExecConfig, RestartPolicy, Spec, Status};
use super::filters::Filters;
use super::http::{self, Request};
use super::time;

/// Serves one client connection until it closes.
pub fn serve(stream: UnixStream, engine: Arc<Engine>) -> io::Result<()> {
    let mut w = stream.try_clone()?;
    let mut r = BufReader::new(stream);
    while let Some(req) = http::read_request(&mut r, streams_body)? {
        // The window's action this request is part of: the engine's lines
        // for it carry the same op.
        let _op = req
            .header("X-Kiri-Op")
            .filter(|o| crate::diag::valid_op(o))
            .map(|o| crate::diag::enter_op(o.to_owned()));
        http::serving(&req.method, &req.path);
        // Kiri's own window checks in every few seconds; that isn't use, and
        // counting it would keep the VM from ever sleeping while it's open.
        if req.header("X-Kiri-Background").is_none() {
            engine.touch();
        }
        match route(&req, &engine, &mut w, &mut r) {
            Ok(Next::KeepAlive) => {}
            Ok(Next::Close) => return Ok(()),
            Err(e) => {
                // The response may be half written; closing is the only safe
                // answer. A client hanging up first (the docker CLI does, with
                // spare wait connections) is not worth a log line.
                if !matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) {
                    crate::diag::warn!(
                        "request failed midway",
                        method = req.method,
                        path = req.path,
                        error = e
                    );
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Requests whose bodies can be any size: archives and image tarballs.
fn streams_body(method: &str, path: &str) -> bool {
    matches!((method, path), ("PUT", p) if p.starts_with("/containers/") && p.ends_with("/archive"))
        || (method == "POST" && (path == "/images/load" || path == "/build"))
}

pub(super) enum Next {
    KeepAlive,
    Close,
}

pub(super) fn reply(w: &mut UnixStream, result: Result<Value, Error>, ok: u16) -> io::Result<Next> {
    match result {
        Ok(Value::Null) => http::no_content(w)?,
        Ok(v) => http::json(w, ok, &v.to_string())?,
        Err(e) => error(w, e)?,
    }
    Ok(Next::KeepAlive)
}

pub(super) fn error(w: &mut UnixStream, e: Error) -> io::Result<()> {
    match e {
        Error::NotFound(m) => http::error(w, 404, &m),
        Error::Conflict(m) => http::error(w, 409, &m),
        Error::BadRequest(m) => http::error(w, 400, &m),
        Error::Unauthorized(m) => http::error(w, 401, &m),
        Error::Io(e) => http::error(w, 500, &e.to_string()),
    }
}

fn route(
    req: &Request,
    engine: &Arc<Engine>,
    w: &mut UnixStream,
    r: &mut BufReader<UnixStream>,
) -> io::Result<Next> {
    if let Some(done) = super::ops::route(req, engine, w) {
        return done;
    }
    let p = req.path.as_str();
    let m = req.method.as_str();
    match (m, p) {
        ("GET" | "HEAD", "/_ping") => {
            // Builder-Version 1: the classic POST /build, which Kiri serves
            // with its managed BuildKit (builder.rs). "2" would tell older
            // CLIs and Compose to use BuildKit inside the engine (/session,
            // /grpc), which Kiri doesn't have.
            http::respond_with(
                w,
                200,
                "text/plain; charset=utf-8",
                "Builder-Version: 1\r\n",
                if m == "HEAD" { b"" } else { b"OK" },
            )?;
            return Ok(Next::KeepAlive);
        }
        ("GET", "/version") => return reply(w, Ok(version()), 200),
        ("POST", "/auth") => return reply(w, auth(req), 200),
        // Kiri's own: the window's Start and Stop.
        // Start: no longer stopped, and the VM up.
        ("POST", "/kiri/wake") => {
            engine.unstop();
            return reply(w, engine.vm().map(|_| Value::Null).map_err(Error::Io), 204);
        }
        // The window's Stop: off until Start, whatever asks for the VM meanwhile.
        ("POST", "/kiri/stop") => return reply(w, engine.stop_all().map(|()| Value::Null), 204),
        ("POST", "/kiri/sleep") => return reply(w, engine.sleep_now().map(|()| Value::Null), 204),
        ("GET", "/kiri/proxy") => return reply(w, Ok(super::proxy::report()), 200),
        ("POST", "/kiri/proxy") => return reply(w, set_proxy(req), 200),
        ("PUT", "/kiri/proxy/system") => return reply(w, set_host_proxy(req), 200),
        ("GET", "/kiri/daemon") => return reply(w, Ok(super::daemon_cfg::report()), 200),
        ("POST", "/kiri/daemon") => return reply(w, set_daemon(req), 200),
        ("GET", "/kiri/resources") => return reply(w, Ok(resources(engine)), 200),
        ("POST", "/kiri/resources") => {
            return reply(w, set_resources(req).map(|()| resources(engine)), 200);
        }
        ("POST", p)
            if p.strip_prefix("/kiri/containers/")
                .and_then(|r| r.strip_suffix("/ports"))
                .is_some_and(|id| !id.is_empty()) =>
        {
            let id = p
                .strip_prefix("/kiri/containers/")
                .and_then(|r| r.strip_suffix("/ports"))
                .unwrap_or_default();
            return reply(w, set_ports(req, engine, id), 204);
        }
        // Kiri's own: the TCP ports a container listens on, for publishing
        // ones its image never declared. `local` are the ones only it can
        // reach (bound to loopback).
        ("GET", p)
            if p.strip_prefix("/kiri/containers/")
                .and_then(|r| r.strip_suffix("/listening"))
                .is_some_and(|id| !id.is_empty()) =>
        {
            let id = p
                .strip_prefix("/kiri/containers/")
                .and_then(|r| r.strip_suffix("/listening"))
                .unwrap_or_default();
            return reply(
                w,
                engine
                    .listening(id)
                    .map(|(tcp, local)| json!({"tcp": tcp, "local": local})),
                200,
            );
        }
        // Kiri's own: a loopback port reaching a Kubernetes service or pod,
        // for Share. `{"ip": "10.43.0.5", "port": 80}` → `{"port": 50123}`.
        ("POST", "/kiri/k8s/forward") => return reply(w, k8s_forward(req, engine), 200),
        ("DELETE", p)
            if p.strip_prefix("/kiri/k8s/forward/")
                .is_some_and(|n| n.parse::<u16>().is_ok()) =>
        {
            engine.k8s_unforward(
                p.strip_prefix("/kiri/k8s/forward/")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0),
            );
            return reply(w, Ok(Value::Null), 204);
        }
        ("POST", p)
            if p.strip_prefix("/kiri/containers/")
                .and_then(|r| r.strip_suffix("/env"))
                .is_some_and(|id| !id.is_empty()) =>
        {
            let id = p
                .strip_prefix("/kiri/containers/")
                .and_then(|r| r.strip_suffix("/env"))
                .unwrap_or_default();
            return reply(w, set_env(req, engine, id), 204);
        }
        // An image's page for kiri-win, whose window can't read the store
        // inside WSL: its layers and its Grype scans (crate::scan), as the
        // Mac's window reads them itself.
        ("GET", "/kiri/images/layers") => {
            return reply(
                w,
                crate::builds::image_layers(req.query("name").unwrap_or_default())
                    .map_err(Error::Io),
                200,
            );
        }
        ("POST", "/kiri/images/scan") => return image_scan(req, w),
        ("GET", "/kiri/images/scans") => {
            return reply(
                w,
                crate::scan::history(req.query("name").unwrap_or_default()).map_err(Error::Io),
                200,
            );
        }
        ("GET", "/kiri/images/scan") => {
            let at = req.query("at").and_then(|a| a.parse().ok());
            return reply(
                w,
                crate::scan::open(req.query("name").unwrap_or_default(), at)
                    .map(Option::unwrap_or_default)
                    .map_err(Error::Io),
                200,
            );
        }
        ("GET", "/kiri/images/sbom") => {
            let at = req.query("at").and_then(|a| a.parse().ok()).unwrap_or(0);
            match crate::scan::sbom(req.query("name").unwrap_or_default(), at)
                .and_then(std::fs::read)
            {
                Ok(b) => http::respond(w, 200, "application/json", &b)?,
                Err(e) => error(w, Error::Io(e))?,
            }
            return Ok(Next::KeepAlive);
        }
        ("GET", "/info") => return reply(w, Ok(info(engine)), 200),
        ("POST", "/build") => return super::builder::build(req, engine, w, r),
        ("POST", "/build/cancel") => return reply(w, super::builder::cancel(req, engine), 200),
        ("POST", "/build/prune") => return reply(w, super::builder::prune(req, engine), 200),
        ("GET", "/kiri/builder") => {
            return reply(w, Ok(super::builder::settings_report(engine)), 200);
        }
        ("POST", "/kiri/builder") => {
            return reply(
                w,
                super::builder::set_settings(req).map(|()| super::builder::settings_report(engine)),
                200,
            );
        }
        ("GET", "/images/json") => return reply(w, images(req, engine), 200),
        ("POST", "/images/create") => return pull(req, engine, w),
        ("POST", "/images/load") => return image_load(req, engine, w, r),
        ("GET", "/images/get") => {
            let names: Vec<String> = req.queries("names").map(str::to_owned).collect();
            return image_save(engine, &names, w);
        }
        ("POST", "/containers/create") => {
            if let Some(made) = super::builder::create_for_buildx(engine, req.query("name")) {
                return reply(w, made, 201);
            }
            return reply(w, create(req, engine), 201);
        }
        ("GET", "/containers/json") => return reply(w, list(req, engine), 200),
        ("GET", "/networks") => return reply(w, networks(req, engine), 200),
        ("POST", "/networks/create") => return reply(w, network_create(req, engine), 201),
        ("POST", "/networks/prune") => return reply(w, network_prune(req, engine), 200),
        ("GET", "/volumes") => return reply(w, volumes(req, engine), 200),
        ("POST", "/volumes/create") => return reply(w, volume_create(req, engine), 201),
        ("POST", "/volumes/prune") => return reply(w, volume_prune(req, engine), 200),
        _ => {}
    }
    if let Some(rest) = p.strip_prefix("/images/") {
        if let Some(name) = rest.strip_suffix("/json") {
            if m == "GET" {
                return reply(w, image_inspect(engine, name), 200);
            }
        } else if let Some(name) = rest.strip_suffix("/get") {
            if m == "GET" {
                return image_save(engine, &[name.to_owned()], w);
            }
        } else if let Some(name) = rest.strip_suffix("/push") {
            if m == "POST" {
                return push(req, engine, name, w);
            }
        } else if let Some(name) = rest.strip_suffix("/tag") {
            if m == "POST" {
                return reply(w, image_tag(req, engine, name).map(|()| Value::Null), 201);
            }
        } else if m == "DELETE" {
            return reply(w, image_remove(engine, rest), 200);
        }
    }
    if let Some(rest) = p.strip_prefix("/containers/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        match (m, action) {
            ("GET", "json") => return reply(w, container_inspect(engine, id), 200),
            ("POST", "start") => {
                super::builder::before_start(engine, id);
                return reply(w, engine.start(id).map(|()| Value::Null), 204);
            }
            ("POST", "stop") => {
                let t = stop_timeout(req, engine, id);
                return reply(w, engine.stop(id, t).map(|()| Value::Null), 204);
            }
            ("POST", "restart") => {
                let t = stop_timeout(req, engine, id);
                let r = engine.stop(id, t).and_then(|()| {
                    // Wait for the exit to be recorded before starting again.
                    engine.wait(id, "not-running")?;
                    super::builder::before_start(engine, id);
                    engine.start(id)
                });
                return reply(w, r.map(|()| Value::Null), 204);
            }
            ("POST", "kill") => {
                let r = parse_signal(
                    req.query("signal")
                        .filter(|s| !s.is_empty())
                        .unwrap_or("KILL"),
                )
                .and_then(|sig| engine.kill(id, sig));
                return reply(w, r.map(|()| Value::Null), 204);
            }
            ("POST", "wait") => return wait(req, engine, id, w),
            ("POST", "attach") => return attach(req, engine, id, w, r),
            ("GET", "logs") => return logs(req, engine, id, w),
            ("POST", "resize") => {
                return reply(w, resize(req, engine, id).map(|()| json!({})), 200);
            }
            ("POST", "exec") => return reply(w, exec_create(req, engine, id), 201),
            ("HEAD" | "GET", "archive") => return archive_get(req, engine, id, w),
            ("PUT", "archive") => return archive_put(req, engine, id, w, r),
            ("DELETE", "") => {
                return reply(
                    w,
                    engine
                        .remove(id, req.flag("force"), req.flag("v"))
                        .map(|()| Value::Null),
                    204,
                );
            }
            _ => {}
        }
    }
    if let Some(name) = p.strip_prefix("/volumes/") {
        match m {
            "GET" => {
                return reply(
                    w,
                    engine
                        .volume_get(name)
                        .map(|v| volume_json(engine, &v))
                        .ok_or_else(|| Error::NotFound(format!("get {name}: no such volume"))),
                    200,
                );
            }
            "DELETE" => {
                return reply(
                    w,
                    engine
                        .volume_remove(name, req.flag("force"))
                        .map(|()| Value::Null),
                    204,
                );
            }
            _ => {}
        }
    }
    if let Some(rest) = p.strip_prefix("/networks/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        match (m, action) {
            ("GET", "") => return reply(w, network_inspect(engine, id), 200),
            ("DELETE", "") => {
                return reply(
                    w,
                    engine
                        .network_remove(id)
                        .map(|()| Value::Null)
                        .map_err(net_error),
                    204,
                );
            }
            _ => {}
        }
    }
    if let Some(rest) = p.strip_prefix("/exec/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        match (m, action) {
            ("POST", "start") => return exec_start(req, engine, id, w, r),
            ("POST", "resize") => {
                return reply(w, resize(req, engine, id).map(|()| json!({})), 200);
            }
            ("GET", "json") => return reply(w, engine.with_exec(id, exec_inspect), 200),
            _ => {}
        }
    }
    crate::diag::warn!("unsupported request", method = m, path = p);
    http::error(w, 404, &format!("Kiri does not support {m} {p} yet"))?;
    Ok(Next::KeepAlive)
}

/// The engine's architecture as Docker names it (arm64, amd64): the Mac's
/// VM is the Mac's own (arm64 on Apple Silicon, amd64 on Intel), a Linux
/// host or WSL2 is whatever it runs on.
fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        a => a,
    }
}

/// On an Intel Mac the VM is x86-64 and there is no Rosetta for Linux:
/// amd64 images run natively, and Kiri does not emulate Arm (no qemu), so
/// an image with no amd64 build is refused with the reason rather than
/// failing later with `exec format error`. `None` elsewhere, and when
/// `platforms` has a linux/amd64 one.
fn intel_mac_refusal(image: &str, platforms: &[String]) -> Option<String> {
    if !cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        return None;
    }
    amd64_only_refusal(image, platforms)
}

fn amd64_only_refusal(image: &str, platforms: &[String]) -> Option<String> {
    if platforms
        .iter()
        .any(|p| p == "linux/amd64" || p.starts_with("linux/amd64/"))
    {
        return None;
    }
    let built = if platforms.is_empty() {
        "no platform Kiri can run".to_owned()
    } else {
        platforms.join(", ")
    };
    Some(format!(
        "{image} is built for {built}, and this Intel Mac runs linux/amd64 images (Kiri does not emulate Arm): use an image with a linux/amd64 build"
    ))
}

fn version() -> Value {
    let v = env!("CARGO_PKG_VERSION");
    json!({
        "Platform": {"Name": "Kiri"},
        "Components": [{"Name": "Engine", "Version": v, "Details": {"ApiVersion": http::API_VERSION, "MinAPIVersion": "1.24", "Arch": go_arch(), "Os": "linux"}}],
        "Version": v,
        "ApiVersion": http::API_VERSION,
        "MinAPIVersion": "1.24",
        "Os": "linux",
        "Arch": go_arch(),
        "KernelVersion": "6.18-kiri",
        "GitCommit": "",
        "GoVersion": "n/a (Rust)",
    })
}

fn info(engine: &Engine) -> Value {
    let states = engine.each(|c| Some(c.status));
    let running = states.iter().filter(|s| **s == Status::Running).count();
    let images = engine.store.images().map(|v| v.len()).unwrap_or(0);
    let (mem_total, _) = engine.guest_memory().unwrap_or((0, 0));
    json!({
        "ID": "kiri",
        "Name": "kiri",
        "Containers": states.len(),
        "ContainersRunning": running,
        "ContainersPaused": 0,
        "ContainersStopped": states.len() - running,
        "Images": images,
        "Driver": "overlayfs",
        "ServerVersion": env!("CARGO_PKG_VERSION"),
        "OperatingSystem": "Kiri",
        "OSType": "linux",
        "Architecture": std::env::consts::ARCH,
        "KernelVersion": "6.18-kiri",
        "NCPU": engine.cpus(),
        "MemTotal": mem_total,
        "DockerRootDir": "/var/lib/kiri",
        "CgroupDriver": "cgroupfs",
        "CgroupVersion": "2",
        "DefaultRuntime": "kiri",
        "Runtimes": {"kiri": {"path": "kiri-init"}},
        "SecurityOptions": ["name=cgroupns"],
        // Not in Docker's /info (the ping's Builder-Version header is), but
        // tools print it: the classic builder API, served by builder.rs.
        "BuilderVersion": "1",
        "KiriAgentMemory": engine.agent_rss().unwrap_or(0),
        "KiriVmAwake": engine.is_awake(),
        "KiriStopped": engine.is_stopped(),
        "KiriMemoryCeiling": engine.memory().0,
        "KiriMemoryTarget": engine.memory().1,
        "KiriMemoryUsed": engine.memory_used(),
        "KiriMemoryStall": engine.memory_trouble().0,
        "KiriOomKills": engine.memory_trouble().1,
        "KiriCpuPercent": engine.cpu_percent(),
        // The Linux environment's IPv4 (Mac VM uplink, or WSL/Linux host eth0).
        "KiriVmIp": vm_ip(engine),
        "RegistryConfig": super::daemon_cfg::registry_config(),
        "Warnings": [],
    })
}

/// The address to show on the sidebar's VM panel: the guest's NAT uplink on
/// a Mac, else this host's outbound IPv4 (WSL2 / a Linux install).
fn vm_ip(engine: &Engine) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        engine
            .vm()
            .ok()
            .and_then(|vm| vm.uplink().ok())
            .map(|(ip, _)| super::network::ip_str(ip))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = engine;
        primary_outbound_ipv4()
    }
}

/// IPv4 of the interface used for outbound traffic (UDP connect trick).
#[cfg(not(target_os = "macos"))]
fn primary_outbound_ipv4() -> Option<String> {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // No packet needs to arrive; the kernel still picks a local source.
    sock.connect(SocketAddr::from((Ipv4Addr::new(1, 1, 1, 1), 80)))
        .ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip.to_string()),
        _ => None,
    }
}

pub(super) fn filters(req: &Request, known: &[&str]) -> Result<Filters, Error> {
    let f = Filters::parse(req.query("filters")).map_err(Error::BadRequest)?;
    f.only(known).map_err(Error::BadRequest)?;
    Ok(f)
}

/// Docker's reference filter: `alpine`, `alpine:3`, `library/*`, `*:latest`.
pub(super) fn reference_matches(pattern: &str, name: &str) -> bool {
    let (repo, tag) = name.rsplit_once(':').unwrap_or((name, "latest"));
    let glob = |p: &str, s: &str| match p.split_once('*') {
        Some((a, b)) => s.len() >= a.len() + b.len() && s.starts_with(a) && s.ends_with(b),
        None => p == s,
    };
    match pattern.rsplit_once(':') {
        Some((p, t)) if !p.contains('/') || !t.contains('/') => glob(p, repo) && glob(t, tag),
        _ => glob(pattern, repo) || glob(pattern, name),
    }
}

fn images(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = filters(req, &["reference", "dangling", "label"])?;
    let list = engine.store.images()?;
    // Every stored image is tagged, so there are no dangling ones.
    if f.bool("dangling") == Some(true) {
        return Ok(json!([]));
    }
    Ok(Value::Array(
        list.iter()
            .filter(|i| {
                f.any("reference", |p| reference_matches(p, &i.name)) && f.labels(&Value::Null)
            })
            .map(|i| {
                json!({
                    "Id": i.config_digest,
                    "ParentId": "",
                    "RepoTags": [i.name],
                    "RepoDigests": i.repo_digest().into_iter().collect::<Vec<_>>(),
                    "Created": time::parse_unix(&i.created),
                    "Size": i.size,
                    "SharedSize": -1,
                    "VirtualSize": i.size,
                    "Labels": null,
                    "Containers": -1,
                    // Not Docker's: when this image arrived here, for the window.
                    "Pulled": i.pulled_at,
                })
            })
            .collect(),
    ))
}

fn image_inspect(engine: &Engine, name: &str) -> Result<Value, Error> {
    let i = engine
        .image(name)?
        .ok_or_else(|| Error::NotFound(format!("No such image: {name}")))?;
    // ExposedPorts (and Healthcheck) live in the image config blob, not in
    // Kiri's slim image model — read them the same way create's EXPOSE path does.
    let cfg = image_config_json(engine, &i);
    Ok(json!({
        "Id": i.config_digest,
        "RepoTags": [i.name],
        "RepoDigests": i.repo_digest().into_iter().collect::<Vec<_>>(),
        "Created": i.created,
        "Size": i.size,
        "Architecture": i.platform.rsplit('/').next().unwrap_or(go_arch()),
        "Os": "linux",
        "Config": {
            "Env": i.config.env,
            "Cmd": i.config.cmd,
            "Entrypoint": if i.config.entrypoint.is_empty() { Value::Null } else { json!(i.config.entrypoint) },
            "WorkingDir": i.config.working_dir,
            "User": i.config.user,
            "ExposedPorts": cfg["config"]["ExposedPorts"].clone(),
            "Healthcheck": cfg["config"]["Healthcheck"].clone(),
            "Labels": cfg["config"]["Labels"].clone(),
            "StopSignal": cfg["config"]["StopSignal"].clone(),
        },
        "RootFS": {"Type": "layers", "Layers": i.layers.iter().map(|l| &l.digest).collect::<Vec<_>>()},
    }))
}

/// The OCI image config blob, or null when it is missing / unreadable.
fn image_config_json(engine: &Engine, i: &kiri_image::Image) -> Value {
    engine
        .store
        .read_config(&i.config_digest)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

fn image_remove(engine: &Engine, name: &str) -> Result<Value, Error> {
    let i = engine
        .image(name)?
        .ok_or_else(|| Error::NotFound(format!("No such image: {name}")))?;
    // With another tag on the same image, removing this one only untags it.
    let other_tags = engine
        .store
        .images()?
        .iter()
        .any(|o| o.config_digest == i.config_digest && o.name != i.name);
    let in_use = if other_tags {
        Vec::new()
    } else {
        engine.each(|c| (c.spec.image_id == i.config_digest).then(|| c.short_id().to_owned()))
    };
    if let Some(c) = in_use.first() {
        return Err(Error::Conflict(format!(
            "conflict: unable to remove repository reference \"{name}\" (must force) - container {c} is using its referenced image"
        )));
    }
    engine.store.remove_image(&i.name)?;
    engine.emit_image("untag", &i.config_digest, &i.name);
    if other_tags {
        return Ok(json!([{"Untagged": i.name}]));
    }
    engine.emit_image("delete", &i.config_digest, &i.name);
    Ok(json!([{"Untagged": i.name}, {"Deleted": i.config_digest}]))
}

/// `docker tag SOURCE TARGET`: another name for the same image.
fn image_tag(req: &Request, engine: &Engine, source: &str) -> Result<(), Error> {
    let i = engine
        .image(source)?
        .ok_or_else(|| Error::NotFound(format!("No such image: {source}")))?;
    let repo = req.query("repo").unwrap_or_default();
    if repo.is_empty() {
        return Err(Error::BadRequest(
            "repository name must have at least one component".into(),
        ));
    }
    let tag = req
        .query("tag")
        .filter(|t| !t.is_empty())
        .unwrap_or("latest");
    let full = format!("{repo}:{tag}");
    let name = Reference::parse(&full)
        .map(|r| r.short())
        .map_err(|e| Error::BadRequest(format!("invalid reference format: {e}")))?;
    engine.store.put_image(i.tagged(name.clone()))?;
    engine.emit_image("tag", &i.config_digest, &name);
    Ok(())
}

/// `docker load`: an image archive, streamed into the store.
fn image_load(
    req: &Request,
    engine: &Engine,
    w: &mut UnixStream,
    r: &mut BufReader<UnixStream>,
) -> io::Result<Next> {
    let loaded = kiri_image::archive::load(&engine.store, http::Body::new(req, r));
    match loaded {
        Ok(images) => {
            http::begin_stream(w, 200, "application/json")?;
            for i in &images {
                engine.emit_image("load", &i.config_digest, &i.name);
                let line = if i.name.starts_with("sha256:") {
                    format!("Loaded image ID: {}\n", i.name)
                } else {
                    format!("Loaded image: {}\n", i.name)
                };
                http::chunk(w, format!("{}\n", json!({"stream": line})).as_bytes())?;
            }
            http::end(w)?;
        }
        Err(e) => error(w, Error::BadRequest(e.to_string()))?,
    }
    // A failed load may leave part of the body unread.
    Ok(Next::Close)
}

/// `docker save`: the named images as one archive.
fn image_save(engine: &Engine, names: &[String], w: &mut UnixStream) -> io::Result<Next> {
    let mut images = Vec::new();
    for n in names {
        match engine.image(n) {
            Ok(Some(i)) => images.push(i),
            Ok(None) => return reply(w, Err(Error::NotFound(format!("No such image: {n}"))), 200),
            Err(e) => return reply(w, Err(Error::Io(e)), 200),
        }
    }
    http::begin_stream(w, 200, "application/x-tar")?;
    struct Chunked<'a>(&'a mut UnixStream);
    impl io::Write for Chunked<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            http::chunk(self.0, buf)?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            io::Write::flush(self.0)
        }
    }
    let out = io::BufWriter::with_capacity(256 * 1024, Chunked(w));
    kiri_image::archive::save(&engine.store, &images, out)?;
    http::end(w)?;
    Ok(Next::KeepAlive)
}

/// Scans an image with Grype, streamed as JSON lines: `{phase, pct}` as it
/// goes (crate::scan::scan's progress), then `{result}` or `{error}`.
fn image_scan(req: &Request, w: &mut UnixStream) -> io::Result<Next> {
    let line = |w: &mut UnixStream, v: Value| {
        let mut s = v.to_string();
        s.push('\n');
        http::chunk(w, s.as_bytes())
    };
    http::begin_stream(w, 200, "application/json")?;
    let r = crate::scan::scan(req.query("name").unwrap_or_default(), &mut |phase, pct| {
        let _ = line(w, json!({"phase": phase, "pct": pct}));
    });
    line(
        w,
        match r {
            Ok(res) => json!({"result": res}),
            Err(e) => json!({"error": e.to_string()}),
        },
    )?;
    http::end(w)?;
    Ok(Next::KeepAlive)
}

/// `docker pull`, streamed as JSON progress lines like dockerd sends.
/// `{"ports": [{"container_port": 80, "host_port": 8080, "proto": "tcp", "host_ip": ""}]}`;
/// host_port 0 picks a free one.
/// Settings → Resources: what the VM has now, what it boots with next, and
/// the limits of this Mac.
fn resources(engine: &Engine) -> Value {
    use super::engine as e;
    json!({
        "cpus": e::vm_cpus(),
        "memory_mib": e::memory_ceiling() >> 20,
        "running_cpus": engine.cpus(),
        "running_memory_mib": engine.memory().0 >> 20,
        "awake": engine.is_awake(),
        "max_cpus": e::max_cpus(),
        "max_memory_mib": e::physical_memory_bytes() >> 20,
        "overridden": std::env::var_os("KIRI_CPUS").is_some() || std::env::var_os("KIRI_MEMORY_MIB").is_some(),
    })
}

/// The host's system proxy, from kiri-win on Windows: `{http, https,
/// no_proxy: [..], pac}`, used while the mode is "system".
fn set_host_proxy(req: &Request) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let url = |k: &str| {
        body[k]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| {
                if v.contains("://") {
                    v.to_owned()
                } else {
                    format!("http://{v}")
                }
            })
    };
    let no_proxy = body["no_proxy"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .map(str::to_owned)
        .collect();
    super::proxy::set_host(super::proxy::Effective {
        http: url("http"),
        https: url("https"),
        no_proxy,
        pac: body["pac"].as_bool().unwrap_or(false),
    });
    Ok(super::proxy::report())
}

fn set_proxy(req: &Request) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let mode = body["mode"]
        .as_str()
        .filter(|m| ["system", "custom", "none"].contains(m))
        .ok_or_else(|| Error::BadRequest("mode must be system, custom or none".into()))?;
    let field = |k: &str| body[k].as_str().unwrap_or("").trim().to_owned();
    for k in ["http", "https"] {
        let v = field(k);
        if mode == "custom" && !v.is_empty() && ureq::Proxy::new(&v).is_err() {
            return Err(Error::BadRequest(format!(
                "{k} proxy: not a proxy address: {v}"
            )));
        }
    }
    super::proxy::save(&json!({"mode": mode, "http": field("http"), "https": field("https"), "no_proxy": field("no_proxy"), "containers": body["containers"].as_bool().unwrap_or(false)}))
        .map_err(Error::Io)?;
    Ok(super::proxy::report())
}

fn set_daemon(req: &Request) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    // Accept a full Docker-shaped `daemon.json` object (or just `{ "registry-mirrors": [...] }`).
    if !body.is_object() {
        return Err(Error::BadRequest(
            "daemon.json must be a JSON object".into(),
        ));
    }
    if let Some(raw) = body.get("registry-mirrors") {
        let arr = raw
            .as_array()
            .ok_or_else(|| Error::BadRequest("registry-mirrors must be an array of URLs".into()))?;
        for v in arr {
            let Some(s) = v.as_str() else {
                return Err(Error::BadRequest(
                    "registry-mirrors entries must be strings".into(),
                ));
            };
            if !s.trim().is_empty() && kiri_image::normalize_mirror(s).is_none() {
                return Err(Error::BadRequest(format!(
                    "registry mirror: not an http(s) URL: {s}"
                )));
            }
        }
    }
    if let Some(raw) = body.get("insecure-registries") {
        let arr = raw.as_array().ok_or_else(|| {
            Error::BadRequest("insecure-registries must be an array of hosts or CIDRs".into())
        })?;
        for v in arr {
            let Some(s) = v.as_str() else {
                return Err(Error::BadRequest(
                    "insecure-registries entries must be strings".into(),
                ));
            };
            if !s.trim().is_empty() && kiri_image::normalize_insecure(s).is_none() {
                return Err(Error::BadRequest(format!(
                    "insecure registry: not a host or CIDR: {s}"
                )));
            }
        }
    }
    for key in ["max-concurrent-downloads", "max-concurrent-uploads"] {
        if let Some(raw) = body.get(key) {
            let n = raw
                .as_u64()
                .ok_or_else(|| Error::BadRequest(format!("{key} must be a number from 1 to 16")))?;
            if !(1..=16).contains(&n) {
                return Err(Error::BadRequest(format!(
                    "{key} must be a number from 1 to 16"
                )));
            }
        }
    }
    super::daemon_cfg::save(&body).map_err(Error::Io)?;
    // Kubernetes (k3s) pulls through its own containerd: keep its
    // registries.yaml aligned with Settings → Network mirrors.
    if let Err(e) = crate::k8s::sync_registry_mirrors() {
        crate::diag::warn!("k8s registry mirrors not synced", error = e);
    }
    Ok(super::daemon_cfg::report())
}

fn set_resources(req: &Request) -> Result<(), Error> {
    use super::engine as e;
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let max_mem = e::physical_memory_bytes() >> 20;
    let cpus = body["cpus"]
        .as_u64()
        .filter(|c| (1..=e::max_cpus() as u64).contains(c))
        .ok_or_else(|| Error::BadRequest(format!("cpus must be 1 to {}", e::max_cpus())))?;
    let mem = body["memory_mib"]
        .as_u64()
        .filter(|m| (1024..=max_mem).contains(m))
        .ok_or_else(|| Error::BadRequest(format!("memory_mib must be 1024 to {max_mem}")))?;
    let path = e::resources_path().ok_or_else(|| Error::Io(io::Error::other("HOME is not set")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json!({"cpus": cpus, "memory_mib": mem}).to_string())
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(Error::Io)
}

fn set_ports(req: &Request, engine: &Engine, id: &str) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let mut ports = Vec::new();
    for p in body["ports"].as_array().into_iter().flatten() {
        let container_port = p["container_port"]
            .as_u64()
            .filter(|v| (1..=65535).contains(v))
            .ok_or_else(|| Error::BadRequest("container_port must be 1 to 65535".into()))?
            as u16;
        let host_port = p["host_port"]
            .as_u64()
            .filter(|v| *v <= 65535)
            .ok_or_else(|| Error::BadRequest("host_port must be 0 to 65535".into()))?
            as u16;
        let proto = p["proto"].as_str().unwrap_or("tcp");
        if proto != "tcp" && proto != "udp" {
            return Err(Error::BadRequest(format!(
                "proto must be tcp or udp, not {proto}"
            )));
        }
        ports.push(super::ports::Binding {
            container_port,
            proto: proto.into(),
            host_ip: p["host_ip"].as_str().unwrap_or("").into(),
            host_port,
        });
    }
    engine.set_ports(id, ports)?;
    Ok(Value::Null)
}

/// Kiri's own: a container's whole environment, `NAME=value` each. A running
/// container restarts with it; its files are kept.
fn k8s_forward(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let ip: std::net::Ipv4Addr = body["ip"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::BadRequest("ip must be an IPv4 address".into()))?;
    let port = body["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| Error::BadRequest("port must be a port number".into()))?;
    Ok(json!({"port": engine.k8s_forward(ip.octets(), port)?}))
}

fn set_env(req: &Request, engine: &Engine, id: &str) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let env = strings(&body["env"])
        .ok_or_else(|| Error::BadRequest("env must be a list of NAME=value".into()))?;
    if let Some(bad) = env.iter().find(|e| {
        !e.split_once('=')
            .is_some_and(|(k, _)| !k.is_empty() && !k.contains(char::is_whitespace))
    }) {
        return Err(Error::BadRequest(format!("{bad:?} is not NAME=value")));
    }
    if engine.set_env(id, env)? {
        engine.stop(id, engine.stop_timeout(id))?;
        engine.wait(id, "not-running")?;
        engine.start(id)?;
    }
    Ok(Value::Null)
}

/// A registry client with the credentials the CLI sent, if any.
fn client_for(req: &Request) -> Client {
    Client::new().with_credentials(
        req.header("x-registry-auth")
            .and_then(kiri_image::Credentials::from_header),
    )
}

/// `docker login`: the CLI asks the daemon to check the password, then
/// stores it itself.
fn auth(req: &Request) -> Result<Value, Error> {
    let creds: kiri_image::Credentials = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let server = if creds.server_address.is_empty() {
        "https://index.docker.io/v1/".to_owned()
    } else {
        creds.server_address.clone()
    };
    match Client::new().with_credentials(Some(creds)).login(&server) {
        Ok(()) => Ok(json!({"Status": "Login Succeeded", "IdentityToken": ""})),
        Err(kiri_image::registry::Error::Status {
            status: 401 | 403,
            body,
            ..
        }) => Err(Error::Unauthorized(if body.is_empty() {
            "unauthorized: incorrect username or password".into()
        } else {
            body
        })),
        Err(e) => Err(Error::BadRequest(format!("login to {server} failed: {e}"))),
    }
}

/// `docker push`: progress as a JSON stream, like a pull.
fn push(req: &Request, engine: &Engine, name: &str, w: &mut UnixStream) -> io::Result<Next> {
    let tag = req.query("tag").unwrap_or_default();
    let full = if tag.is_empty() {
        name.to_owned()
    } else {
        format!("{name}:{tag}")
    };
    let (image, target) = match (engine.image(&full), Reference::parse(&full)) {
        (Ok(Some(i)), Ok(r)) => (i, r),
        (Ok(None), _) => {
            return reply(
                w,
                Err(Error::NotFound(format!(
                    "An image does not exist locally with the tag: {full}"
                ))),
                200,
            );
        }
        (Err(e), _) => return reply(w, Err(Error::Io(e)), 200),
        (_, Err(e)) => return reply(w, Err(Error::BadRequest(e.to_string())), 200),
    };
    http::begin_stream(w, 200, "application/json")?;
    let out = Mutex::new(w.try_clone()?);
    let line = |v: Value| {
        let mut s = v.to_string();
        s.push('\n');
        let _ = http::chunk(
            &mut *out.lock().unwrap_or_else(|e| e.into_inner()),
            s.as_bytes(),
        );
    };
    let short = |d: &str| {
        d.strip_prefix("sha256:")
            .unwrap_or(d)
            .chars()
            .take(12)
            .collect::<String>()
    };
    line(
        json!({"status": format!("The push refers to repository [{}]", if target.registry == "docker.io" { format!("docker.io/{}", target.repository) } else { format!("{}/{}", target.registry, target.repository) })}),
    );
    let result = client_for(req).push(&engine.store, &image, &target, &|p| match p {
        kiri_image::PushProgress::Exists { digest } => line(
            json!({"status": "Layer already exists", "progressDetail": {}, "id": short(&digest)}),
        ),
        kiri_image::PushProgress::Uploading { digest, .. } => {
            line(json!({"status": "Pushing", "progressDetail": {}, "id": short(&digest)}))
        }
        kiri_image::PushProgress::Pushed { digest } => {
            line(json!({"status": "Pushed", "progressDetail": {}, "id": short(&digest)}))
        }
    });
    match result {
        Ok((digest, size)) => {
            if let Err(e) = engine.store.set_pushed(&image.name, &image.config_digest, &digest) {
                crate::diag::warn!("pushed digest not recorded", image = image.name, error = e);
            }
            line(json!({"status": format!("{}: digest: {digest} size: {size}", target.reference)}));
            line(
                json!({"progressDetail": {}, "aux": {"Tag": target.reference, "Digest": digest, "Size": size}}),
            );
        }
        Err(e) => line(json!({"errorDetail": {"message": e.to_string()}, "error": e.to_string()})),
    }
    http::end(w)?;
    Ok(Next::KeepAlive)
}

/// `docker pull`, streamed as JSON progress lines like dockerd sends. A
/// pull of the same image already under way is followed rather than
/// started again, and one whose every client hangs up stops
/// (super::pulls).
fn pull(req: &Request, engine: &Engine, w: &mut UnixStream) -> io::Result<Next> {
    let from = req.query("fromImage").unwrap_or_default();
    let tag = req.query("tag").unwrap_or_default();
    let name = match tag {
        "" => from.to_owned(),
        t if t.starts_with("sha256:") => format!("{from}@{t}"),
        t => format!("{from}:{t}"),
    };
    let reference = match Reference::parse(&name) {
        Ok(r) => r,
        Err(e) => {
            http::error(w, 400, &e.to_string())?;
            return Ok(Next::KeepAlive);
        }
    };
    http::begin_stream(w, 200, "application/json")?;
    let key = super::pulls::key(
        &reference.short(),
        req.query("platform").unwrap_or_default(),
        req.header("x-registry-auth").unwrap_or_default(),
    );
    let (follower, lead) = super::pulls::join(&key);
    if !lead {
        crate::diag::info!("image pull joined one under way", image = reference.short());
        return Ok(if follower.stream(w)? {
            Next::KeepAlive
        } else {
            Next::Close
        });
    }
    let job = follower.job.clone();
    // This client follows on a thread of its own, as any other would, so
    // the pull carries on for the rest when it hangs up.
    std::thread::scope(|s| {
        let following = s.spawn(move || follower.stream(w));
        run_pull(req, engine, &reference, &job);
        job.finish();
        match following.join() {
            Ok(Ok(true)) => Ok(Next::KeepAlive),
            Ok(Ok(false)) => Ok(Next::Close),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(Next::Close),
        }
    })
}

/// The pull itself, its lines going to `job`'s followers; stops early when
/// they have all gone.
fn run_pull(req: &Request, engine: &Engine, reference: &Reference, job: &super::pulls::Job) {
    let line = |v: Value| job.send(v);
    let short = |d: &str| {
        d.strip_prefix("sha256:")
            .unwrap_or(d)
            .chars()
            .take(12)
            .collect::<String>()
    };
    line(
        json!({"status": format!("Pulling from {}", reference.repository), "id": reference.reference}),
    );
    let wanted = req
        .query("platform")
        .filter(|p| !p.is_empty())
        .and_then(Platform::parse)
        .unwrap_or_else(Platform::host_linux);
    let client = client_for(req);
    let on_progress = |p: Progress| match p {
        Progress::Resolved { .. } | Progress::Layer { cached: true, .. } => {}
        // Docker's own lines, so `docker pull` draws its bars; `size` is
        // extra, for the window's overall bar, and the CLI ignores it.
        Progress::Layer { digest, size, .. } => line(
            json!({"status": "Pulling fs layer", "progressDetail": {}, "id": short(&digest), "size": size}),
        ),
        Progress::Downloading { digest, got, size } => {
            line(
                json!({"status": "Downloading", "progressDetail": {"current": got, "total": size}, "id": short(&digest)}),
            );
        }
        Progress::LayerDone { digest, cached, .. } => {
            let status = if cached {
                "Already exists"
            } else {
                "Pull complete"
            };
            line(json!({"status": status, "progressDetail": {}, "id": short(&digest)}));
        }
    };
    let began = std::time::Instant::now();
    crate::diag::info!(
        "image pull started",
        image = reference.short(),
        platform = wanted
    );
    let mut result = client.pull_with(&engine.store, reference, &wanted, &on_progress, &job.cancel);
    // An image built only for amd64 runs through Rosetta, as on Docker Desktop.
    if let Err(kiri_image::registry::Error::NoPlatform { available, .. }) = &result {
        let amd64 = Platform {
            os: "linux".into(),
            architecture: "amd64".into(),
        };
        if req.query("platform").is_none_or(str::is_empty)
            && wanted != amd64
            && available.iter().any(|a| a == "linux/amd64")
            && kiri_vmm::rosetta_installed()
        {
            line(
                json!({"status": format!("{} has no {wanted} image; using linux/amd64 through Rosetta", reference.short())}),
            );
            result = client.pull_with(&engine.store, reference, &amd64, &on_progress, &job.cancel);
        }
    }
    match result {
        Ok(image) => {
            // Docker reports the digest it was asked for: the index's, if any.
            line(
                json!({"status": format!("Digest: {}", image.index_digest.as_ref().unwrap_or(&image.digest))}),
            );
            line(json!({"status": format!("Status: Downloaded newer image for {}", image.name)}));
            engine.emit_image("pull", &image.config_digest, &image.name);
            crate::diag::info!(
                "image pulled",
                image = reference.short(),
                ms = began.elapsed().as_millis()
            );
        }
        // Nobody is left to tell.
        Err(kiri_image::registry::Error::Cancelled) => {
            crate::diag::info!(
                "image pull stopped",
                image = reference.short(),
                ms = began.elapsed().as_millis()
            );
        }
        Err(e) => {
            crate::diag::warn!("image pull failed", image = reference.short(), error = e);
            let mut message = e.to_string();
            if let kiri_image::registry::Error::NoPlatform { available, .. } = &e {
                if let Some(why) = intel_mac_refusal(&reference.short(), available) {
                    message = why;
                }
            }
            line(json!({"errorDetail": {"message": message}, "error": message}))
        }
    }
}

/// `-p` (`HostConfig.PortBindings`) and `-P` (`PublishAllPorts`: every port
/// the image or `--expose` declares, on a free host port).
fn port_bindings(
    body: &Value,
    engine: &Engine,
    image: &kiri_image::Image,
) -> Result<Vec<super::ports::Binding>, Error> {
    let parse = |key: &str| -> Result<(u16, String), Error> {
        let (port, proto) = key.split_once('/').unwrap_or((key, "tcp"));
        let port = port
            .parse()
            .map_err(|_| Error::BadRequest(format!("invalid port {key}")))?;
        if proto != "tcp" && proto != "udp" {
            return Err(Error::BadRequest(format!(
                "Kiri forwards TCP and UDP ports, not {key}"
            )));
        }
        Ok((port, proto.to_owned()))
    };
    let mut out = Vec::new();
    if let Some(map) = body["HostConfig"]["PortBindings"].as_object() {
        for (key, list) in map {
            let (container_port, proto) = parse(key)?;
            for b in list.as_array().into_iter().flatten() {
                let host_port = match b["HostPort"].as_str().unwrap_or("") {
                    "" => 0,
                    p => p
                        .parse()
                        .map_err(|_| Error::BadRequest(format!("invalid host port {p}")))?,
                };
                out.push(super::ports::Binding {
                    container_port,
                    proto: proto.clone(),
                    host_ip: b["HostIp"].as_str().unwrap_or("").to_owned(),
                    host_port,
                });
            }
        }
    }
    if body["HostConfig"]["PublishAllPorts"].as_bool() == Some(true) {
        let mut exposed: Vec<String> = body["ExposedPorts"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        // The image's own EXPOSE lines live in its config blob.
        if let Ok(bytes) = engine.store.read_config(&image.config_digest) {
            if let Ok(cfg) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(m) = cfg["config"]["ExposedPorts"].as_object() {
                    exposed.extend(m.keys().cloned());
                }
            }
        }
        for key in exposed {
            let Ok((container_port, proto)) = parse(&key) else {
                continue;
            };
            if !out
                .iter()
                .any(|b| b.container_port == container_port && b.proto == proto)
            {
                out.push(super::ports::Binding {
                    container_port,
                    proto,
                    host_ip: String::new(),
                    host_port: 0,
                });
            }
        }
    }
    Ok(out)
}

/// The VM path containers mount to reach the Mac's SSH agent (Docker
/// Desktop's and OrbStack's name for it).
const SSH_AGENT_SOCK: &str = "/run/host-services/ssh-auth.sock";

/// A container's mounts, worked out at create time.
struct Mounts {
    /// For the guest: `source\ttarget\tflags`.
    guest: Vec<String>,
    /// Docker's `Mounts` for inspect.
    info: Vec<Value>,
    volumes: Vec<String>,
    anon: Vec<String>,
}

/// Where the Mac's filesystem appears in the VM (virtiofs). On a Linux host
/// the agent sees the host's own paths.
#[cfg(target_os = "macos")]
const MAC_ROOT: &str = "/mnt/mac";
#[cfg(not(target_os = "macos"))]
const MAC_ROOT: &str = "";

/// Bind sources under these come from the Mac (Docker Desktop's default file
/// shares; /tmp and /var/folders are the Mac's links into /private). Any
/// other path is the VM's own, as on Docker Desktop and OrbStack: kind's
/// `-v /lib/modules:/lib/modules:ro` must not try to make /lib/modules on
/// the Mac's read-only root.
const MAC_SHARES: &[&str] = &["/Users", "/Volumes", "/private", "/tmp", "/var/folders"];

fn mac_shared(src: &str) -> bool {
    MAC_SHARES.iter().any(|s| {
        src.strip_prefix(s)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// Where the guest finds a host path given for a bind: the Mac's own under
/// the shares, else the VM's (on Linux, the host's).
fn guest_bind_source(src: &str) -> String {
    if cfg!(target_os = "macos") && !mac_shared(src) {
        src.to_owned()
    } else {
        format!("{MAC_ROOT}{src}")
    }
}

/// An NFS or CIFS server name as an address the guest kernel can use.
/// `host.docker.internal` is the Mac, as it is for containers.
fn resolve_server(host: &str) -> Option<String> {
    use std::net::ToSocketAddrs;
    if cfg!(target_os = "macos") && matches!(host, "host.docker.internal" | "host.kiri.internal") {
        return Some(super::ports::HOST_ADDR.to_owned());
    }
    let addrs: Vec<std::net::SocketAddr> = (host, 0).to_socket_addrs().ok()?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .map(|a| a.ip().to_string())
}

/// `-v`, `--mount`, `--tmpfs`, `-v /path` and the image's VOLUME lines.
fn mounts(body: &Value, engine: &Engine, image: &kiri_image::Image) -> Result<Mounts, Error> {
    let host = &body["HostConfig"];
    let mut out = Mounts {
        guest: Vec::new(),
        info: Vec::new(),
        volumes: Vec::new(),
        anon: Vec::new(),
    };
    let check_target = |t: &str| -> Result<(), Error> {
        if !t.starts_with('/') {
            return Err(Error::BadRequest(format!(
                "invalid mount target {t}: must be an absolute path"
            )));
        }
        Ok(())
    };
    // One entry: kind is "bind" | "volume" | "tmpfs"; for binds `create_missing`
    // says whether a missing source directory is made (-v) or an error (--mount).
    let add = |kind: &str,
               src: &str,
               dst: &str,
               ro: bool,
               extra: &str,
               create_missing: bool,
               out: &mut Mounts|
     -> Result<(), Error> {
        check_target(dst)?;
        let flag = |more: &str| {
            let mut f: Vec<&str> = more.split(',').filter(|x| !x.is_empty()).collect();
            if ro {
                f.push("ro");
            }
            f.join(",")
        };
        // The engine's own socket (Testcontainers' Ryuk, CI runners): the VM's
        // relay to this daemon on a Mac, or Kiri's socket on a Linux host.
        let engine_sock = if kind == "bind" {
            super::binds::engine_socket(src)
        } else {
            None
        };
        match kind {
            "bind" if engine_sock.is_some() => {
                out.guest.push(format!(
                    "{}\t{dst}\t{}",
                    engine_sock.as_deref().unwrap_or_default(),
                    flag("")
                ));
                out.info.push(json!({"Type": "bind", "Source": src, "Destination": dst, "Mode": "", "RW": !ro, "Propagation": "rprivate"}));
            }
            "bind" if src == SSH_AGENT_SOCK => {
                out.guest.push(format!("{src}\t{dst}\t{}", flag("")));
                out.info.push(json!({"Type": "bind", "Source": src, "Destination": dst, "Mode": "", "RW": !ro, "Propagation": "rprivate"}));
            }
            "bind" => {
                if !src.starts_with('/') {
                    return Err(Error::BadRequest(format!(
                        "invalid mount source {src}: must be an absolute path"
                    )));
                }
                if cfg!(target_os = "macos") && !mac_shared(src) {
                    // The guest makes a missing source itself, as Docker does on Linux.
                    out.guest.push(format!(
                        "{src}\t{dst}\t{}",
                        flag(if create_missing { "create" } else { "" })
                    ));
                    out.info.push(json!({"Type": "bind", "Source": src, "Destination": dst, "Mode": "", "RW": !ro, "Propagation": "rprivate"}));
                    return Ok(());
                }
                let p = std::path::Path::new(src);
                if !p.exists() {
                    if !create_missing {
                        return Err(Error::BadRequest(format!(
                            "invalid mount config for type \"bind\": bind source path does not exist: {src}"
                        )));
                    }
                    std::fs::create_dir_all(p).map_err(Error::Io)?;
                }
                out.guest
                    .push(format!("{MAC_ROOT}{src}\t{dst}\t{}", flag("")));
                out.info.push(json!({"Type": "bind", "Source": src, "Destination": dst, "Mode": "", "RW": !ro, "Propagation": "rprivate"}));
            }
            "volume" => {
                let (name, anonymous) = if src.is_empty() {
                    (super::engine::random_hex(32), true)
                } else {
                    (src.to_owned(), false)
                };
                let v = engine.volume_ensure(&name, json!({}), anonymous)?;
                let path = super::volumes::guest_path(&name);
                let copy = if extra.contains("nocopy") {
                    "create"
                } else {
                    "create,copyup"
                };
                // A volume with driver options (`type=nfs`, `o=bind` …) is
                // mounted at its path by the guest at start, then bound in.
                let vol = super::volumes::guest_mount_fields(
                    &v.options,
                    guest_bind_source,
                    resolve_server,
                );
                out.guest
                    .push(format!("{path}\t{dst}\t{}{vol}", flag(copy)));
                out.info.push(json!({"Type": "volume", "Name": name, "Source": path, "Destination": dst, "Driver": "local", "Mode": "z", "RW": !ro, "Propagation": ""}));
                if anonymous {
                    out.anon.push(name);
                } else {
                    out.volumes.push(name);
                }
            }
            "tmpfs" => {
                // Docker's tmpfs defaults; the options given come after, so
                // `exec` or `suid` there wins.
                out.guest.push(format!(
                    "tmpfs\t{dst}\t{}",
                    flag(&format!("tmpfs,noexec,nosuid,nodev,{extra}"))
                ));
                out.info.push(json!({"Type": "tmpfs", "Source": "", "Destination": dst, "Mode": "", "RW": !ro, "Propagation": ""}));
            }
            other => {
                return Err(Error::BadRequest(format!(
                    "Kiri does not support {other} mounts"
                )));
            }
        }
        Ok(())
    };
    // From the docker CLI on Windows (into WSL), `C:\src:/app` and the like.
    let wsl = super::binds::wsl();
    for b in strings(&host["Binds"]).unwrap_or_default() {
        let (src, dst, opts) = super::binds::split(&b, wsl)?;
        let (src, dst, opts) = (src.as_str(), dst.as_str(), opts.as_str());
        let ro = opts.split(',').any(|o| o == "ro");
        let kind = if src.starts_with('/') {
            "bind"
        } else {
            "volume"
        };
        let extra = if opts.split(',').any(|o| o == "nocopy") {
            "nocopy"
        } else {
            ""
        };
        add(kind, src, dst, ro, extra, true, &mut out)?;
    }
    for m in host["Mounts"].as_array().into_iter().flatten() {
        let kind = m["Type"].as_str().unwrap_or("bind");
        let src = m["Source"].as_str().unwrap_or("");
        let src = match wsl {
            Some(w) if kind == "bind" => super::binds::source(src, w)?,
            _ => src.to_owned(),
        };
        let src = src.as_str();
        let dst = m["Target"].as_str().unwrap_or("");
        let ro = m["ReadOnly"].as_bool().unwrap_or(false);
        let extra = match kind {
            "volume" if m["VolumeOptions"]["NoCopy"].as_bool() == Some(true) => "nocopy".to_owned(),
            "tmpfs" => {
                let mut o = Vec::new();
                if let Some(size) = m["TmpfsOptions"]["SizeBytes"].as_u64() {
                    o.push(format!("size={size}"));
                }
                if let Some(mode) = m["TmpfsOptions"]["Mode"].as_u64() {
                    o.push(format!("mode={mode:o}"));
                }
                // API 1.46+: `[["exec"], ["nr_inodes", "1000"]]`.
                for kv in m["TmpfsOptions"]["Options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let kv: Vec<&str> = kv
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    if !kv.is_empty() {
                        o.push(kv.join("="));
                    }
                }
                o.join(",")
            }
            _ => String::new(),
        };
        // `--mount type=volume,volume-opt=…` creates the volume with those
        // options when it does not exist yet, as Docker does.
        let driver_opts = &m["VolumeOptions"]["DriverConfig"];
        if kind == "volume"
            && !src.is_empty()
            && engine.volume_get(src).is_none()
            && !driver_opts.is_null()
        {
            match driver_opts["Name"].as_str().unwrap_or("") {
                "" | "local" => {}
                d => {
                    return Err(Error::BadRequest(format!(
                        "Kiri supports only local volumes, not {d}"
                    )));
                }
            }
            let options = super::volumes::local_options(&driver_opts["Options"])
                .map_err(Error::BadRequest)?;
            engine.volume_ensure_with(src, m["VolumeOptions"]["Labels"].clone(), options)?;
        }
        add(kind, src, dst, ro, &extra, false, &mut out)?;
    }
    if let Some(map) = host["Tmpfs"].as_object() {
        for (dst, opts) in map {
            // `--tmpfs /x:noexec,size=64m`: flags and options alike.
            let opts = opts
                .as_str()
                .unwrap_or("")
                .split(',')
                .filter(|o| !o.is_empty() && *o != "tmpfs")
                .collect::<Vec<_>>()
                .join(",");
            add("tmpfs", "", dst, false, &opts, false, &mut out)?;
        }
    }
    // `-v /path` (no source) arrives in `Volumes`; the image's VOLUME lines
    // are in its config. Each becomes an anonymous volume, unless something
    // else is already mounted there.
    let mut anonymous: Vec<String> = body["Volumes"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if let Ok(bytes) = engine.store.read_config(&image.config_digest) {
        if let Ok(cfg) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(m) = cfg["config"]["Volumes"].as_object() {
                anonymous.extend(m.keys().cloned());
            }
        }
    }
    for dst in anonymous {
        let taken = out.info.iter().any(|i| {
            i["Destination"].as_str().map(|d| d.trim_end_matches('/'))
                == Some(dst.trim_end_matches('/'))
        });
        if !taken {
            add("volume", "", &dst, false, "", false, &mut out)?;
        }
    }
    Ok(out)
}

fn volume_json(engine: &Engine, v: &super::volumes::Volume) -> Value {
    // Prefer a bind's host path when the local driver was given `device=…`
    // (Compose `driver_opts`); otherwise the volume's data directory in the
    // engine. UsageData.RefCount matches `docker system df -v` so the window
    // can mark unused volumes.
    let mountpoint = v
        .options
        .get("device")
        .filter(|d| !d.is_empty())
        .cloned()
        .unwrap_or_else(|| super::volumes::guest_path(&v.name));
    json!({
        "Name": v.name,
        "Driver": "local",
        "Mountpoint": mountpoint,
        "CreatedAt": time::rfc3339(std::time::UNIX_EPOCH + Duration::from_nanos(v.created_ns)),
        "Labels": if v.labels.is_object() { v.labels.clone() } else { json!({}) },
        "Scope": "local",
        "Options": v.options,
        "UsageData": {"Size": -1, "RefCount": engine.volume_users(&v.name).len()},
    })
}

fn volume_create(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    match body["Driver"].as_str().unwrap_or("local") {
        "" | "local" => {}
        d => {
            return Err(Error::BadRequest(format!(
                "Kiri supports only local volumes, not {d}"
            )));
        }
    }
    let name = match body["Name"].as_str().unwrap_or("") {
        "" => super::engine::random_hex(32),
        n => n.to_owned(),
    };
    let options = super::volumes::local_options(&body["DriverOpts"]).map_err(Error::BadRequest)?;
    let v = engine.volume_ensure_with(&name, body["Labels"].clone(), options)?;
    Ok(volume_json(engine, &v))
}

/// Docker's default prune: anonymous volumes no container uses.
/// `all=true` takes named ones too.
fn volume_prune(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = filters(req, &["label", "label!", "all"])?;
    let all = f.bool("all") == Some(true);
    let unused: Vec<String> = engine
        .volume_list()
        .into_iter()
        .filter(|v| {
            (all || v.anonymous) && f.labels(&v.labels) && engine.volume_users(&v.name).is_empty()
        })
        .map(|v| v.name)
        .collect();
    for n in &unused {
        let _ = engine.volume_remove(n, false);
    }
    Ok(json!({"VolumesDeleted": unused, "SpaceReclaimed": 0}))
}

fn volumes(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = filters(req, &["name", "label", "label!", "dangling", "driver"])?;
    let list: Vec<Value> = engine
        .volume_list()
        .iter()
        .filter(|v| {
            f.name(&v.name)
                && f.labels(&v.labels)
                && f.any("driver", |d| d == "local")
                && f.bool("dangling")
                    .is_none_or(|d| d == engine.volume_users(&v.name).is_empty())
        })
        .map(|v| volume_json(engine, v))
        .collect();
    Ok(json!({"Volumes": list, "Warnings": []}))
}

/// The TCP ports an image declares (EXPOSE) plus `--expose`.
fn exposed_ports(body: &Value, engine: &Engine, image: &kiri_image::Image) -> Vec<u16> {
    let mut keys: Vec<String> = body["ExposedPorts"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if let Ok(bytes) = engine.store.read_config(&image.config_digest) {
        if let Ok(cfg) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(m) = cfg["config"]["ExposedPorts"].as_object() {
                keys.extend(m.keys().cloned());
            }
        }
    }
    let mut ports: Vec<u16> = keys
        .iter()
        .filter(|k| k.ends_with("/tcp") || !k.contains('/'))
        .filter_map(|k| k.split('/').next()?.parse().ok())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// What is published right now: Docker's `Ports` list for `ps`.
fn ports_list(c: &Container) -> Value {
    let Some(fw) = &c.forwards else {
        return json!([]);
    };
    Value::Array(
        fw.bound
            .iter()
            .map(|(b, ip, port)| json!({"IP": ip, "PrivatePort": b.container_port, "PublicPort": port, "Type": b.proto}))
            .collect(),
    )
}

/// Docker's `NetworkSettings.Ports` map for inspect and `docker port`.
fn ports_map(c: &Container) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(fw) = &c.forwards {
        for (b, ip, port) in &fw.bound {
            let key = format!("{}/{}", b.container_port, b.proto);
            let entry = m.entry(key).or_insert_with(|| json!([]));
            if let Some(a) = entry.as_array_mut() {
                a.push(json!({"HostIp": ip, "HostPort": port.to_string()}));
            }
        }
    }
    Value::Object(m)
}

/// `[rows, cols]`, as Docker sends `ConsoleSize`; zeros mean unknown.
fn console_size(v: &Value) -> Option<(u16, u16)> {
    let a = v.as_array()?;
    let (h, w) = (a.first()?.as_u64()? as u16, a.get(1)?.as_u64()? as u16);
    (h > 0 && w > 0).then_some((h, w))
}

fn strings(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(a) => Some(
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect(),
        ),
        Value::String(s) => Some(vec![s.clone()]),
        _ => None,
    }
}

/// cgroup limits from a HostConfig (create) or an update body, on top of `base`.
/// Capability names as Docker writes them (`NET_ADMIN`, `CAP_NET_ADMIN`, `ALL`).
const CAPS: [&str; 41] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "FSETID",
    "KILL",
    "SETGID",
    "SETUID",
    "SETPCAP",
    "LINUX_IMMUTABLE",
    "NET_BIND_SERVICE",
    "NET_BROADCAST",
    "NET_ADMIN",
    "NET_RAW",
    "IPC_LOCK",
    "IPC_OWNER",
    "SYS_MODULE",
    "SYS_RAWIO",
    "SYS_CHROOT",
    "SYS_PTRACE",
    "SYS_PACCT",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_NICE",
    "SYS_RESOURCE",
    "SYS_TIME",
    "SYS_TTY_CONFIG",
    "MKNOD",
    "LEASE",
    "AUDIT_WRITE",
    "AUDIT_CONTROL",
    "SETFCAP",
    "MAC_OVERRIDE",
    "MAC_ADMIN",
    "SYSLOG",
    "WAKE_ALARM",
    "BLOCK_SUSPEND",
    "AUDIT_READ",
    "PERFMON",
    "BPF",
    "CHECKPOINT_RESTORE",
];

/// Docker's default set: what a container keeps without `--cap-add`.
const DEFAULT_CAPS: [&str; 14] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "KILL",
    "SETGID",
    "SETUID",
    "SETPCAP",
    "NET_BIND_SERVICE",
    "NET_RAW",
    "SYS_CHROOT",
    "MKNOD",
    "AUDIT_WRITE",
    "SETFCAP",
];

/// `--cap-add`/`--cap-drop` as a mask; 0 when they change nothing.
fn caps_from(host: &Value) -> Result<u64, Error> {
    let add = strings(&host["CapAdd"]).unwrap_or_default();
    let drop = strings(&host["CapDrop"]).unwrap_or_default();
    if add.is_empty() && drop.is_empty() {
        return Ok(0);
    }
    let bit = |name: &str| -> Result<u64, Error> {
        let n = name.trim().to_ascii_uppercase();
        let n = n.strip_prefix("CAP_").unwrap_or(&n);
        if n == "ALL" {
            return Ok((1u64 << CAPS.len()) - 1);
        }
        CAPS.iter()
            .position(|c| *c == n)
            .map(|i| 1u64 << i)
            .ok_or_else(|| Error::BadRequest(format!("unknown capability: {name:?}")))
    };
    let mut mask = DEFAULT_CAPS
        .iter()
        .map(|c| bit(c))
        .sum::<Result<u64, Error>>()?;
    for d in &drop {
        mask &= !bit(d)?;
    }
    for a in &add {
        mask |= bit(a)?;
    }
    // Dropping everything still has to be told apart from "default".
    Ok(if mask == 0 { 1u64 << 63 } else { mask })
}

pub(super) fn limits_from(h: &Value, base: super::engine::Limits) -> super::engine::Limits {
    let mut l = base;
    if let Some(m) = h["Memory"].as_u64().filter(|m| *m > 0) {
        l.memory_max = m;
    }
    if let Some(n) = h["NanoCpus"].as_u64().filter(|n| *n > 0) {
        // --cpus 1.5 → 150 ms of CPU time every 100 ms.
        l.cpu_period = 100_000;
        l.cpu_quota = n * 100_000 / 1_000_000_000;
    }
    if let Some(q) = h["CpuQuota"].as_i64().filter(|q| *q > 0) {
        l.cpu_quota = q as u64;
        l.cpu_period = h["CpuPeriod"]
            .as_u64()
            .filter(|p| *p > 0)
            .unwrap_or(100_000) as u32;
    }
    if let Some(shares) = h["CpuShares"].as_u64().filter(|s| *s > 0) {
        // runc's conversion from v1 shares (2–262144) to v2 weight (1–10000).
        let shares = shares.clamp(2, 262_144);
        l.cpu_weight = (1 + (shares - 2) * 9999 / 262_142) as u32;
    }
    if let Some(p) = h["PidsLimit"].as_i64().filter(|p| *p > 0) {
        l.pids_max = p as u32;
    }
    l
}

pub(super) fn create(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let image_name = body["Image"].as_str().unwrap_or_default();
    if image_name.is_empty() {
        return Err(Error::BadRequest("no image specified".into()));
    }
    let host = &body["HostConfig"];
    let network_mode = match host["NetworkMode"].as_str().unwrap_or("default") {
        "" | "default" => "bridge".to_owned(),
        m => m.to_owned(),
    };
    let network = match network_mode.strip_prefix("container:") {
        Some(target) => Some(container_network_mode(&body, engine, target)?),
        None => engine
            .lock_net()
            .find(&network_mode)
            .map(|n| n.name.clone()),
    };
    let Some(network_mode) = network else {
        return Err(Error::NotFound(format!("network {network_mode} not found")));
    };
    // One endpoint per network (API 1.44+ allows several at create, which
    // Compose uses); the one named by NetworkMode is the primary.
    let mut aliases = Vec::new();
    let mut extra_networks = Vec::new();
    // `--ip`, `--mac-address` (Config.MacAddress before API 1.44).
    let mut ipv4_address = String::new();
    let mut ipv6_address = String::new();
    let mut mac_address = body["MacAddress"].as_str().unwrap_or_default().to_owned();
    if let Some(eps) = body["NetworkingConfig"]["EndpointsConfig"].as_object() {
        for (net, ep) in eps {
            if net.starts_with("container:") {
                continue;
            }
            if network_mode.starts_with("container:") {
                return Err(Error::BadRequest("container sharing network namespace with another container or host cannot be connected to any other network".into()));
            }
            let names = strings(&ep["Aliases"]).unwrap_or_default();
            let found = engine
                .lock_net()
                .find(net)
                .cloned()
                .ok_or_else(|| Error::NotFound(format!("network {net} not found")))?;
            let canonical = found.name.clone();
            let (ip, ip6, mac) = endpoint_address(ep)?;
            if let Some(a) = super::network::parse_ip(&ip) {
                found.check_static(a).map_err(Error::BadRequest)?;
            }
            if let Some(a) = super::network::parse_ip6(&ip6) {
                found.check_static6(a).map_err(Error::BadRequest)?;
            }
            if canonical == network_mode {
                aliases.extend(names);
                ipv4_address = ip;
                ipv6_address = ip6;
                if !mac.is_empty() {
                    mac_address = mac;
                }
            } else if !extra_networks
                .iter()
                .any(|x: &super::engine::ExtraNetwork| x.name == canonical)
            {
                extra_networks.push(super::engine::ExtraNetwork {
                    name: canonical,
                    aliases: names,
                    ipv4_address: ip,
                    ipv6_address: ip6,
                    mac_address: mac,
                });
            }
        }
    }
    if !mac_address.is_empty() && super::network::parse_mac(&mac_address).is_none() {
        return Err(Error::BadRequest(format!(
            "invalid MAC address: {mac_address}"
        )));
    }
    if !mac_address.is_empty() && network_mode == "host" {
        return Err(Error::BadRequest(
            "conflicting options: mac-address and the network mode".into(),
        ));
    }
    let links = links_from(&strings(&host["Links"]).unwrap_or_default(), engine)?;
    if !links.is_empty() && network_mode == "host" {
        return Err(Error::BadRequest("conflicting options: host type networking can't be used with links. This would result in undefined behavior".into()));
    }
    // `--add-host name:ip` arrives as "name:ip"; /etc/hosts wants "ip name".
    let mut extra_hosts = Vec::new();
    for h in strings(&host["ExtraHosts"]).unwrap_or_default() {
        let Some((name, ip)) = h.split_once(':') else {
            continue;
        };
        let ip = if ip == "host-gateway" {
            super::ports::HOST_ADDR
        } else {
            ip
        };
        extra_hosts.push(format!("{ip}\t{name}"));
    }
    let warnings: Vec<&str> = Vec::new();
    // --ulimit, --sysctl, --device, --device-cgroup-rule, --group-add:
    // refused now with Docker's error rather than at start.
    let host_opts = super::hostopts::from_host(host);
    super::hostopts::guest_lines(&host_opts, network_mode == "host").map_err(Error::BadRequest)?;
    super::hostopts::check_device_requests(host).map_err(Error::BadRequest)?;
    if let Some(s) = body["StopSignal"].as_str().filter(|s| !s.is_empty()) {
        parse_signal(s)?;
    }

    let image = engine
        .image(image_name)?
        .ok_or_else(|| Error::NotFound(format!("No such image: {image_name}")))?;
    if let Some(why) = intel_mac_refusal(image_name, std::slice::from_ref(&image.platform)) {
        return Err(Error::BadRequest(why));
    }
    // A local image for another platform is not the one asked for: the CLI
    // pulls the right one when told there is none.
    if let Some(p) = req
        .query("platform")
        .filter(|p| !p.is_empty())
        .and_then(Platform::parse)
    {
        if image.platform != p.to_string() {
            return Err(Error::NotFound(format!(
                "No such image: {image_name} for {p}"
            )));
        }
    }
    // The image's own config, for what Kiri's image model does not keep.
    let image_cfg: Value = engine
        .store
        .read_config(&image.config_digest)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    // Checked before mounts, which may create volumes.
    let healthcheck = healthcheck_from(&body, &image_cfg)?;
    let mounts = mounts(&body, engine, &image)?;
    let ports = port_bindings(&body, engine, &image)?;
    if !ports.is_empty() && network_mode == "none" {
        return Err(Error::BadRequest(
            "a container with --network none cannot publish ports".into(),
        ));
    }
    // Docker's rules: an explicit Entrypoint drops the image's Cmd; an
    // explicit Cmd replaces the image's Cmd but keeps its Entrypoint.
    let entry_override = strings(&body["Entrypoint"]);
    let entrypoint = entry_override
        .clone()
        .unwrap_or_else(|| image.config.entrypoint.clone());
    let cmd = strings(&body["Cmd"]).unwrap_or_else(|| {
        if entry_override.is_some() {
            Vec::new()
        } else {
            image.config.cmd.clone()
        }
    });
    let mut env = image.config.env.clone();
    // Settings → Network → "Pass to containers": the proxy, unless set here.
    let given = strings(&body["Env"]).unwrap_or_default();
    for p in super::proxy::container_env() {
        let key = p.split('=').next().unwrap_or_default();
        if !given
            .iter()
            .chain(&env)
            .any(|e| e.split('=').next() == Some(key))
        {
            env.push(p);
        }
    }
    for e in given {
        let key = e.split('=').next().unwrap_or_default().to_owned();
        env.retain(|x| x.split('=').next() != Some(key.as_str()));
        env.push(e);
    }
    let pick = |field: &str, fallback: &str| match body[field].as_str() {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => fallback.to_owned(),
    };
    let spec = Spec {
        image: image_name.to_owned(),
        image_id: image.config_digest.clone(),
        entrypoint,
        cmd,
        env,
        cwd: pick("WorkingDir", &image.config.working_dir),
        user: pick("User", &image.config.user),
        auto_remove: host["AutoRemove"].as_bool().unwrap_or(false),
        privileged: host["Privileged"].as_bool().unwrap_or(false),
        limits: super::engine::Limits {
            shm_size: host["ShmSize"].as_u64().unwrap_or(0),
            ..limits_from(host, super::engine::Limits::default())
        },
        caps: caps_from(host)?,
        cap_add: strings(&host["CapAdd"]).unwrap_or_default(),
        cap_drop: strings(&host["CapDrop"]).unwrap_or_default(),
        security_opt: strings(&host["SecurityOpt"]).unwrap_or_default(),
        readonly_rootfs: host["ReadonlyRootfs"].as_bool().unwrap_or(false),
        hostname: body["Hostname"].as_str().unwrap_or_default().to_owned(),
        stop_signal: body["StopSignal"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(signal)
            .or_else(|| {
                // The image's STOPSIGNAL.
                let cfg: Value =
                    serde_json::from_slice(&engine.store.read_config(&image.config_digest).ok()?)
                        .ok()?;
                cfg["config"]["StopSignal"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(signal)
            }),
        stop_timeout: body["StopTimeout"].as_i64(),
        healthcheck,
        shell: strings(&body["Shell"])
            .filter(|s| !s.is_empty())
            .or_else(|| strings(&image_cfg["config"]["Shell"]))
            .unwrap_or_default(),
        init: host["Init"].as_bool(),
        extra_networks,
        log_config: super::logfile::LogConfig::from_host(&host["LogConfig"])
            .map_err(Error::BadRequest)?,
        network_mode,
        ipv4_address,
        ipv6_address,
        mac_address,
        dns: strings(&host["Dns"]).unwrap_or_default(),
        dns_search: strings(&host["DnsSearch"]).unwrap_or_default(),
        dns_options: strings(&host["DnsOptions"]).unwrap_or_default(),
        domainname: body["Domainname"].as_str().unwrap_or_default().to_owned(),
        links,
        labels: body["Labels"].clone(),
        tty: body["Tty"].as_bool().unwrap_or(false),
        open_stdin: body["OpenStdin"].as_bool().unwrap_or(false),
        console_size: console_size(&host["ConsoleSize"]),
        aliases,
        extra_hosts,
        ports,
        exposed: exposed_ports(&body, engine, &image),
        host_opts,
        mounts: mounts.guest,
        mount_info: mounts.info,
        volumes: mounts.volumes,
        anon_volumes: mounts.anon,
        restart: RestartPolicy {
            name: match host["RestartPolicy"]["Name"].as_str() {
                Some("") | None => "no".to_owned(),
                Some(n) => n.to_owned(),
            },
            max_retries: host["RestartPolicy"]["MaximumRetryCount"]
                .as_u64()
                .unwrap_or(0) as u32,
        },
    };
    if !matches!(
        spec.restart.name.as_str(),
        "no" | "always" | "unless-stopped" | "on-failure"
    ) {
        return Err(Error::BadRequest(format!(
            "invalid restart policy {}",
            spec.restart.name
        )));
    }
    if spec.auto_remove && spec.restart.name != "no" {
        return Err(Error::BadRequest(
            "can't create 'AutoRemove' container with restart policy".into(),
        ));
    }
    let id = engine.create(req.query("name").map(str::to_owned), spec, &image)?;
    Ok(json!({"Id": id, "Warnings": warnings}))
}

/// `--network container:<target>`: Docker's checks, then the mode as it is
/// kept, `container:<full id>`. The target must exist now and run at start.
fn container_network_mode(body: &Value, engine: &Engine, target: &str) -> Result<String, Error> {
    container_mode_allowed(body, target)?;
    let id = engine.resolve(target)?;
    Ok(format!("container:{id}"))
}

/// What Docker refuses beside `--network container:`, in its order and words.
fn container_mode_allowed(body: &Value, target: &str) -> Result<(), Error> {
    let host = &body["HostConfig"];
    let given = |v: &Value| match v {
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::String(s) => !s.is_empty(),
        _ => false,
    };
    let conflict = |what: &str| Err(Error::BadRequest(format!("conflicting options: {what}")));
    if target.is_empty() {
        return Err(Error::BadRequest(
            "Invalid network mode: invalid container format container:<name|id>".into(),
        ));
    }
    if given(&body["Hostname"]) {
        return conflict("hostname and the network mode");
    }
    if given(&host["Links"]) {
        return conflict(
            "container type network can't be used with links. This would result in undefined behavior",
        );
    }
    if given(&host["Dns"]) {
        return conflict("dns and the network mode");
    }
    if given(&host["ExtraHosts"]) {
        return conflict("custom host-to-IP mapping and the network mode");
    }
    if given(&body["MacAddress"]) {
        return conflict("mac-address and the network mode");
    }
    if given(&host["PortBindings"]) || host["PublishAllPorts"].as_bool() == Some(true) {
        return conflict("port publishing and the container type network mode");
    }
    if given(&body["ExposedPorts"]) {
        return conflict("port exposing and the container type network mode");
    }
    Ok(())
}

/// An endpoint's `IPAMConfig.IPv4Address`, `IPv6Address` and `MacAddress`,
/// checked for form.
pub(super) fn endpoint_address(ep: &Value) -> Result<(String, String, String), Error> {
    let ip = ep["IPAMConfig"]["IPv4Address"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if !ip.is_empty() && super::network::parse_ip(&ip).is_none() {
        return Err(Error::BadRequest(format!("invalid IPv4 address: {ip}")));
    }
    let ip6 = ep["IPAMConfig"]["IPv6Address"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if !ip6.is_empty() && super::network::parse_ip6(&ip6).is_none() {
        return Err(Error::BadRequest(format!("invalid IPv6 address: {ip6}")));
    }
    let mac = ep["MacAddress"].as_str().unwrap_or_default().to_owned();
    if !mac.is_empty() && super::network::parse_mac(&mac).is_none() {
        return Err(Error::BadRequest(format!("invalid MAC address: {mac}")));
    }
    Ok((ip, ip6, mac))
}

/// `HostConfig.Links`: `name:alias`, `name` or Docker's own `/name:/me/alias`.
pub(super) fn parse_link(l: &str) -> Option<(&str, &str)> {
    let (name, alias) = l.split_once(':').unwrap_or((l, l));
    let name = name.trim_start_matches('/');
    let alias = alias.rsplit('/').next().unwrap_or(alias);
    (!name.is_empty() && !alias.is_empty()).then_some((name, alias))
}

fn links_from(list: &[String], engine: &Engine) -> Result<Vec<super::engine::Link>, Error> {
    let mut out = Vec::new();
    for l in list {
        let (name, alias) =
            parse_link(l).ok_or_else(|| Error::BadRequest(format!("invalid link: {l}")))?;
        let id = engine
            .resolve(name)
            .map_err(|_| Error::NotFound(format!("could not get container for {name}")))?;
        let name = engine.with(&id, |c| c.name.clone())?;
        out.push(super::engine::Link {
            id,
            name,
            alias: alias.to_owned(),
        });
    }
    Ok(out)
}

fn status_text(c: &Container) -> String {
    let ago = |t: Option<std::time::SystemTime>| {
        let s = t.and_then(|t| t.elapsed().ok()).map_or(0, |d| d.as_secs());
        match s {
            0..=1 => "Less than a second".to_owned(),
            2..=59 => format!("{s} seconds"),
            60..=3599 => format!("{} minutes", s / 60),
            3600..=86399 => format!("{} hours", s / 3600),
            _ => format!("{} days", s / 86400),
        }
    };
    match c.status {
        Status::Created => "Created".into(),
        Status::Running if c.paused => format!("Up {} (Paused)", ago(c.started)),
        Status::Running => format!("Up {}", ago(c.started)),
        Status::Exited => format!("Exited ({}) {} ago", c.exit_code, ago(c.finished)),
    }
}

/// `t` on stop and restart; without it, the container's `--stop-timeout`, else 10 s.
fn stop_timeout(req: &Request, engine: &Engine, id: &str) -> Duration {
    match req.query("t").and_then(|v| v.parse::<i64>().ok()) {
        Some(t) => super::engine::stop_wait(t),
        None => engine.stop_timeout(id),
    }
}

/// `Config.Healthcheck` merged over the image's HEALTHCHECK, as Docker does at create.
fn healthcheck_from(
    body: &Value,
    image_cfg: &Value,
) -> Result<Option<super::health::Healthcheck>, Error> {
    use super::health::Healthcheck;
    let mine = Healthcheck::from_json(&body["Healthcheck"]);
    if let Some(h) = &mine {
        h.validate().map_err(Error::BadRequest)?;
    }
    Ok(super::health::merge(
        mine,
        Healthcheck::from_json(&image_cfg["config"]["Healthcheck"]),
    ))
}

/// `docker ps`'s Status: "Up 2 minutes (healthy)" while running unpaused
/// with a healthcheck.
fn ps_status(c: &Container, health: Option<&super::health::Health>) -> String {
    let s = status_text(c);
    match health {
        Some(h) if c.status == Status::Running && !c.paused => format!("{s} ({})", h.ps_word()),
        _ => s,
    }
}

/// Docker's `NetworkSettings.Networks`, the same in inspect and in list
/// rows: Compose compares each `NetworkID` with the network's id (and list
/// `Mounts` by volume name) to decide whether a container is up to date.
/// Addresses are only there while it runs, as in Docker.
fn endpoints(c: &Container, networks: &[super::network::Network]) -> Value {
    // Another container's namespace: no endpoints of its own, as Docker
    // shows it (`{}`), not an entry named `container:<id>`.
    if c.spec.network_mode.starts_with("container:") {
        return json!({});
    }
    let running = c.status == Status::Running;
    let mut out = serde_json::Map::new();
    let mut add = |name: &str,
                   ip: Option<[u8; 4]>,
                   ip6: Option<[u8; 16]>,
                   aliases: &[String],
                   want4: &str,
                   want6: &str| {
        let net = networks.iter().find(|n| n.name == name || n.id == name);
        let ip = ip.filter(|_| running && !net.is_some_and(|n| n.no_ipv4));
        let ip6 = ip6.filter(|_| running);
        let gateway = match (ip, net) {
            (Some(_), Some(n)) if n.gateway != [0; 4] => super::network::ip_str(n.gateway),
            _ => String::new(),
        };
        let v6 = net.and_then(|n| n.v6.as_ref()).filter(|_| ip6.is_some());
        out.insert(
            name.to_owned(),
            json!({
                "IPAMConfig": ipam_config(want4, want6),
                "Links": null,
                "Aliases": if aliases.is_empty() { Value::Null } else { json!(aliases) },
                "MacAddress": "",
                "DriverOpts": null,
                "NetworkID": net.map(|n| n.id.clone()).unwrap_or_default(),
                "EndpointID": if running { c.id.as_str() } else { "" },
                "Gateway": gateway,
                "IPAddress": ip.map(super::network::ip_str).unwrap_or_default(),
                "IPPrefixLen": if ip.is_some() { net.map_or(16, |n| n.prefix) } else { 0 },
                "IPv6Gateway": v6.map(|v| super::network::ip6_str(v.gateway)).unwrap_or_default(),
                "GlobalIPv6Address": ip6.map(super::network::ip6_str).unwrap_or_default(),
                "GlobalIPv6PrefixLen": v6.map_or(0, |v| v.prefix),
                "DNSNames": null,
            }),
        );
    };
    add(
        &c.spec.network_mode,
        c.ip,
        c.ip6,
        &c.spec.aliases,
        &c.spec.ipv4_address,
        &c.spec.ipv6_address,
    );
    for x in &c.spec.extra_networks {
        let ip6 = c
            .extra_ip6s
            .iter()
            .find(|(n, _)| *n == x.name)
            .map(|(_, ip)| *ip);
        add(
            &x.name,
            c.extra_ips
                .iter()
                .find(|(n, _)| *n == x.name)
                .map(|(_, ip)| *ip),
            ip6,
            &x.aliases,
            &x.ipv4_address,
            &x.ipv6_address,
        );
    }
    Value::Object(out)
}

/// An endpoint's `IPAMConfig`: the fixed addresses it asked for, as Docker
/// shows them (null when none).
fn ipam_config(want4: &str, want6: &str) -> Value {
    let mut m = serde_json::Map::new();
    if !want4.is_empty() {
        m.insert("IPv4Address".into(), want4.into());
    }
    if !want6.is_empty() {
        m.insert("IPv6Address".into(), want6.into());
    }
    if m.is_empty() {
        Value::Null
    } else {
        Value::Object(m)
    }
}

/// `GET /containers/{id}/json`: [`inspect`] plus what needs the engine's
/// networks and healthcheck state.
fn container_inspect(engine: &Engine, key: &str) -> Result<Value, Error> {
    let id = engine.resolve(key)?;
    let networks: Vec<super::network::Network> = engine.lock_net().all().to_vec();
    let health = engine.health(&id);
    engine.with(&id, |c| {
        let mut v = inspect(c, &networks);
        v["NetworkSettings"]["Networks"] = endpoints(c, &networks);
        // The old top-level fields describe the default bridge only.
        if c.spec.network_mode == "bridge" {
            let ep = v["NetworkSettings"]["Networks"]["bridge"].clone();
            for k in ["EndpointID", "Gateway", "IPPrefixLen", "MacAddress"] {
                v["NetworkSettings"][k] = ep[k].clone();
            }
        }
        if let Some(h) = &health {
            v["State"]["Health"] = json!(h);
        }
        if let Some(hc) = &c.spec.healthcheck {
            v["Config"]["Healthcheck"] = hc.to_json();
        }
        if let Some(t) = c.spec.stop_timeout {
            v["Config"]["StopTimeout"] = t.into();
        }
        if !c.spec.shell.is_empty() {
            v["Config"]["Shell"] = json!(c.spec.shell);
        }
        if let Some(init) = c.spec.init {
            v["HostConfig"]["Init"] = init.into();
        }
        v
    })
}

fn list(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = filters(
        req,
        &[
            "id", "name", "label", "label!", "status", "ancestor", "network", "exited", "volume",
            "health",
        ],
    )?;
    if let Some(bad) = f
        .values("health")
        .and_then(|v| v.iter().find(|v| !super::health::valid_filter(v)))
    {
        return Err(Error::BadRequest(format!(
            "Unrecognised filter value for health: {bad}"
        )));
    }
    let health = engine.health_statuses();
    // A status filter lists stopped containers without --all, as Docker does.
    let all = req.flag("all") || f.values("status").is_some() || f.values("exited").is_some();
    let networks: Vec<super::network::Network> = engine.lock_net().all().to_vec();
    let mut rows = engine.each(|c| {
        if !all && c.status != Status::Running {
            return None;
        }
        let labels = if c.spec.labels.is_object() {
            c.spec.labels.clone()
        } else {
            json!({})
        };
        let net = networks.iter().find(|n| {
            n.name == c.spec.network_mode
                || n.id == c.spec.network_mode
                || (c.spec.network_mode == "default" && n.name == "bridge")
        });
        let keep = f.any("id", |v| c.id.starts_with(v))
            && f.name(&c.name)
            && f.labels(&labels)
            && f.any("status", |v| v == state_word(c))
            && f.any("exited", |v| {
                c.status == Status::Exited && v.parse() == Ok(c.exit_code)
            })
            && f.any("ancestor", |v| {
                reference_matches(v, &c.spec.image)
                    || c.spec.image == v
                    || c.spec
                        .image_id
                        .trim_start_matches("sha256:")
                        .starts_with(v.trim_start_matches("sha256:"))
            })
            && f.any("network", |v| {
                net.is_some_and(|n| n.name == v || n.id.starts_with(v))
            })
            && f.any("volume", |v| {
                c.spec
                    .volumes
                    .iter()
                    .chain(&c.spec.anon_volumes)
                    .any(|n| n == v)
                    || c.spec
                        .mount_info
                        .iter()
                        .any(|m| m["Destination"] == v || m["Source"] == v)
            })
            && f.any("health", |v| {
                v == health.get(&c.id).map_or("none", |h| h.status.as_str())
            });
        if !keep {
            return None;
        }
        Some((
            time::unix(c.created),
            json!({
                "Id": c.id,
                "Names": [format!("/{}", c.name)],
                "Image": c.spec.image,
                "ImageID": c.spec.image_id,
                "Command": c.argv().join(" "),
                "Created": time::unix(c.created),
                "State": state_word(c),
                "Status": ps_status(c, health.get(&c.id)),
                // Kiri's own: the window counts "Up 3 s" from these every second.
                // Docker clients ignore fields they don't know.
                "KiriStartedAt": c.started.map(time::unix),
                "KiriFinishedAt": c.finished.map(time::unix),
                "KiriExitCode": c.exit_code,
                "KiriOOMKilled": c.oom_killed,
                "KiriVmStopped": c.vm_stopped,
                "Ports": ports_list(c),
                "Labels": if c.spec.labels.is_object() { c.spec.labels.clone() } else { json!({}) },
                "HostConfig": {"NetworkMode": c.spec.network_mode},
                "NetworkSettings": {"Networks": endpoints(c, &networks)},
                "Mounts": c.spec.mount_info,
            }),
        ))
    });
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    Ok(Value::Array(rows.into_iter().map(|(_, v)| v).collect()))
}

fn inspect(c: &Container, nets: &[super::network::Network]) -> Value {
    let when =
        |t: Option<std::time::SystemTime>| t.map_or_else(|| time::ZERO.to_owned(), time::rfc3339);
    let argv = c.argv();
    json!({
        "Id": c.id,
        "Created": time::rfc3339(c.created),
        "Path": argv.first().cloned().unwrap_or_default(),
        "Args": argv.iter().skip(1).collect::<Vec<_>>(),
        "State": {
            "Status": state_word(c),
            "Running": c.status == Status::Running,
            "Paused": c.paused,
            "Restarting": false,
            "OOMKilled": c.oom_killed,
            "Dead": false,
            "Pid": c.pid,
            "ExitCode": c.exit_code,
            "Error": "",
            "StartedAt": when(c.started),
            "FinishedAt": when(c.finished),
        },
        "Image": c.spec.image_id,
        "Name": format!("/{}", c.name),
        "RestartCount": c.restart_count,
        "LogPath": Engine::log_path(c),
        "Driver": "overlayfs",
        "Platform": "linux",
        "Config": {
            "Hostname": if c.spec.hostname.is_empty() { c.short_id() } else { c.spec.hostname.as_str() },
            "Domainname": c.spec.domainname,
            "Image": c.spec.image,
            "Entrypoint": if c.spec.entrypoint.is_empty() { Value::Null } else { json!(c.spec.entrypoint) },
            "Cmd": c.spec.cmd,
            "Env": c.spec.env,
            "WorkingDir": c.spec.cwd,
            "User": c.spec.user,
            "Tty": c.spec.tty,
            "OpenStdin": c.spec.open_stdin,
            "AttachStdout": true,
            "AttachStderr": true,
            "Labels": if c.spec.labels.is_object() { c.spec.labels.clone() } else { json!({}) },
        },
        "HostConfig": {
            "AutoRemove": c.spec.auto_remove,
            "Privileged": c.spec.privileged,
            "CapAdd": c.spec.cap_add,
            "CapDrop": c.spec.cap_drop,
            "SecurityOpt": c.spec.security_opt,
            "ReadonlyRootfs": c.spec.readonly_rootfs,
            "ShmSize": if c.spec.limits.shm_size > 0 { c.spec.limits.shm_size } else { 64 << 20 },
            "Memory": c.spec.limits.memory_max,
            "NanoCpus": if c.spec.limits.cpu_period > 0 { c.spec.limits.cpu_quota * 1_000_000_000 / u64::from(c.spec.limits.cpu_period) } else { 0 },
            "CpuQuota": c.spec.limits.cpu_quota,
            "CpuPeriod": c.spec.limits.cpu_period,
            "CpuShares": if c.spec.limits.cpu_weight > 0 { 2 + (u64::from(c.spec.limits.cpu_weight) - 1) * 262_142 / 9999 } else { 0 },
            "PidsLimit": if c.spec.limits.pids_max > 0 { Value::from(c.spec.limits.pids_max) } else { Value::Null },
            "NetworkMode": c.spec.network_mode,
            "Dns": c.spec.dns,
            "DnsSearch": c.spec.dns_search,
            "DnsOptions": c.spec.dns_options,
            "Links": if c.spec.links.is_empty() { Value::Null } else { json!(c.spec.links.iter().map(|l| format!("/{}:/{}/{}", l.name, c.name, l.alias)).collect::<Vec<_>>()) },
            "RestartPolicy": {"Name": c.spec.restart.name, "MaximumRetryCount": c.spec.restart.max_retries},
            "LogConfig": {"Type": c.spec.log_config.kind, "Config": c.spec.log_config.config},
            "Ulimits": super::hostopts::inspect(&c.spec.host_opts, "Ulimits"),
            "Sysctls": super::hostopts::inspect(&c.spec.host_opts, "Sysctls"),
            "Devices": super::hostopts::inspect(&c.spec.host_opts, "Devices"),
            "DeviceCgroupRules": super::hostopts::inspect(&c.spec.host_opts, "DeviceCgroupRules"),
            "GroupAdd": super::hostopts::inspect(&c.spec.host_opts, "GroupAdd"),
        },
        "NetworkSettings": network_settings(c, nets),
        "Mounts": c.spec.mount_info,
    })
}

fn network_settings(c: &Container, nets: &[super::network::Network]) -> Value {
    // Another container's namespace: nothing of its own, as Docker shows it.
    if c.spec.network_mode.starts_with("container:") {
        return json!({"IPAddress": "", "IPPrefixLen": 0, "MacAddress": "", "Ports": {}, "Networks": {}});
    }
    let prefix_of = |name: &str, has_ip: bool| {
        if has_ip {
            nets.iter()
                .find(|n| n.name == name)
                .map_or(16, |n| n.prefix)
        } else {
            0
        }
    };
    // An IPv6-only network's IPv4 addresses are Kiri's own, not shown.
    let v4_on = |name: &str| !nets.iter().any(|n| n.name == name && n.no_ipv4);
    let ip =
        c.ip.filter(|_| v4_on(&c.spec.network_mode))
            .map(super::network::ip_str)
            .unwrap_or_default();
    let v6 = |name: &str, ip6: Option<[u8; 16]>| {
        let n = nets
            .iter()
            .find(|n| n.name == name)
            .and_then(|n| n.v6.as_ref())
            .filter(|_| ip6.is_some());
        (
            ip6.map(super::network::ip6_str).unwrap_or_default(),
            n.map_or(0, |v| v.prefix),
            n.map(|v| super::network::ip6_str(v.gateway))
                .unwrap_or_default(),
        )
    };
    let mut networks = serde_json::Map::new();
    let (ip6, prefix6, gateway6) = v6(&c.spec.network_mode, c.ip6);
    networks.insert(
        c.spec.network_mode.clone(),
        json!({"IPAddress": ip, "IPPrefixLen": prefix_of(&c.spec.network_mode, !ip.is_empty()), "IPAMConfig": ipam_config(&c.spec.ipv4_address, &c.spec.ipv6_address), "Aliases": c.spec.aliases, "NetworkID": "", "MacAddress": c.spec.mac_address, "GlobalIPv6Address": ip6, "GlobalIPv6PrefixLen": prefix6, "IPv6Gateway": gateway6}),
    );
    for x in &c.spec.extra_networks {
        let ip = c
            .extra_ips
            .iter()
            .find(|(n, _)| *n == x.name)
            .filter(|_| v4_on(&x.name))
            .map(|(_, ip)| super::network::ip_str(*ip))
            .unwrap_or_default();
        let prefix = prefix_of(&x.name, !ip.is_empty());
        let (xip6, xprefix6, xgateway6) = v6(
            &x.name,
            c.extra_ip6s
                .iter()
                .find(|(n, _)| *n == x.name)
                .map(|(_, a)| *a),
        );
        networks.insert(x.name.clone(), json!({"IPAddress": ip, "IPPrefixLen": prefix, "IPAMConfig": ipam_config(&x.ipv4_address, &x.ipv6_address), "Aliases": x.aliases, "NetworkID": "", "MacAddress": x.mac_address, "GlobalIPv6Address": xip6, "GlobalIPv6PrefixLen": xprefix6, "IPv6Gateway": xgateway6}));
    }
    json!({"IPAddress": ip, "IPPrefixLen": prefix_of(&c.spec.network_mode, !ip.is_empty()), "MacAddress": c.spec.mac_address, "GlobalIPv6Address": ip6, "GlobalIPv6PrefixLen": prefix6, "IPv6Gateway": gateway6, "Ports": ports_map(c), "Networks": networks})
}

/// Docker's state word: `paused` is a running container that is frozen.
fn state_word(c: &Container) -> &'static str {
    if c.paused {
        "paused"
    } else {
        c.status.as_str()
    }
}

fn net_error(m: String) -> Error {
    if m.contains("not found") {
        Error::NotFound(m)
    } else if m.contains("active endpoints")
        || m.contains("already exists")
        || m.contains("pre-defined")
        || m.contains("Pool overlaps")
    {
        Error::Conflict(m)
    } else {
        Error::BadRequest(m)
    }
}

fn network_json(engine: &Engine, n: &super::network::Network, with_members: bool) -> Value {
    let mut containers = serde_json::Map::new();
    if with_members {
        for (id, name, ip, ip6) in engine.network_members(&n.name) {
            containers.insert(
                id.clone(),
                json!({
                    "Name": name,
                    "EndpointID": id,
                    "MacAddress": "",
                    "IPv4Address": ip.filter(|_| !n.no_ipv4).map(|a| format!("{}/{}", super::network::ip_str(a), n.prefix)).unwrap_or_default(),
                    "IPv6Address": match (ip6, &n.v6) {
                        (Some(a), Some(v)) => format!("{}/{}", super::network::ip6_str(a), v.prefix),
                        _ => String::new(),
                    },
                }),
            );
        }
    }
    let ipam = if n.bridge.is_empty() {
        json!([])
    } else {
        let mut c = json!({"Subnet": n.subnet_str(), "Gateway": super::network::ip_str(n.gateway)});
        if let Some(r) = n.ip_range_str() {
            c["IPRange"] = r.into();
        }
        if !n.aux.is_empty() {
            c["AuxiliaryAddresses"] = n
                .aux
                .iter()
                .map(|(k, ip)| (k.clone(), Value::from(super::network::ip_str(*ip))))
                .collect::<serde_json::Map<_, _>>()
                .into();
        }
        // An IPv6-only network's IPv4 subnet is Kiri's own, not shown.
        let mut configs = if n.no_ipv4 { Vec::new() } else { vec![c] };
        if let Some(v) = &n.v6 {
            let mut c =
                json!({"Subnet": v.subnet_str(), "Gateway": super::network::ip6_str(v.gateway)});
            if let Some(r) = v.ip_range_str() {
                c["IPRange"] = r.into();
            }
            if !v.aux.is_empty() {
                c["AuxiliaryAddresses"] = v
                    .aux
                    .iter()
                    .map(|(k, ip)| (k.clone(), Value::from(super::network::ip6_str(*ip))))
                    .collect::<serde_json::Map<_, _>>()
                    .into();
            }
            configs.push(c);
        }
        Value::Array(configs)
    };
    json!({
        "Name": n.name,
        "Id": n.id,
        "Created": time::rfc3339(std::time::UNIX_EPOCH + Duration::from_nanos(n.created_ns)),
        "Scope": "local",
        "Driver": n.driver,
        "EnableIPv4": !n.no_ipv4,
        "EnableIPv6": n.v6.is_some(),
        "IPAM": {"Driver": "default", "Options": if n.ipam_options.is_object() { n.ipam_options.clone() } else { Value::Null }, "Config": ipam},
        "Internal": n.internal,
        "Attachable": n.attachable,
        "Ingress": false,
        "ConfigOnly": false,
        "Containers": containers,
        "Options": if n.options.is_object() { n.options.clone() } else { json!({}) },
        "Labels": if n.labels.is_object() { n.labels.clone() } else { json!({}) },
    })
}

fn network_matches(f: &Filters, engine: &Engine, n: &super::network::Network) -> bool {
    let labels = if n.labels.is_object() {
        n.labels.clone()
    } else {
        json!({})
    };
    f.name(&n.name)
        && f.any("id", |v| n.id.starts_with(v))
        && f.any("driver", |v| v == n.driver)
        && f.labels(&labels)
        && f.any("scope", |v| v == "local")
        && f.any("type", |v| (v == "custom") != n.builtin())
        && f.bool("dangling")
            .is_none_or(|d| d == (!n.builtin() && engine.network_members(&n.name).is_empty()))
}

const NETWORK_FILTERS: &[&str] = &[
    "name", "id", "driver", "label", "label!", "scope", "type", "dangling",
];

fn networks(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = filters(req, NETWORK_FILTERS)?;
    let list: Vec<super::network::Network> = engine.lock_net().all().to_vec();
    Ok(Value::Array(
        list.iter()
            .filter(|n| network_matches(&f, engine, n))
            .map(|n| network_json(engine, n, false))
            .collect(),
    ))
}

fn network_inspect(engine: &Engine, key: &str) -> Result<Value, Error> {
    let n = engine
        .lock_net()
        .find(key)
        .cloned()
        .ok_or_else(|| Error::NotFound(format!("network {key} not found")))?;
    Ok(network_json(engine, &n, true))
}

fn network_create(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let name = body["Name"].as_str().unwrap_or_default();
    match body["Driver"].as_str().unwrap_or("bridge") {
        "" | "bridge" => {}
        d => {
            return Err(Error::BadRequest(format!(
                "Kiri supports only bridge networks, not {d}"
            )));
        }
    }
    let (ipam, ipam6) =
        super::network::parse_ipam_both(&body["IPAM"]).map_err(Error::BadRequest)?;
    let settings = super::network::Settings {
        labels: body["Labels"].clone(),
        internal: body["Internal"].as_bool().unwrap_or(false),
        attachable: body["Attachable"].as_bool().unwrap_or(false),
        options: body["Options"].clone(),
        ipam_options: body["IPAM"]["Options"].clone(),
        ipam,
        ipv6: body["EnableIPv6"].as_bool() == Some(true),
        no_ipv4: body["EnableIPv4"].as_bool() == Some(false),
        ipam6,
    };
    let n = engine
        .lock_net()
        .create(name, settings)
        .map_err(net_error)?;
    Ok(json!({"Id": n.id, "Warning": ""}))
}

fn network_prune(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = filters(req, &["label", "label!"])?;
    let nets = engine.lock_net();
    let unused: Vec<String> = nets
        .all()
        .iter()
        .filter(|n| {
            !n.builtin()
                && !nets.in_use(&n.name)
                && f.labels(if n.labels.is_object() {
                    &n.labels
                } else {
                    &Value::Null
                })
        })
        .map(|n| n.name.clone())
        .collect();
    drop(nets);
    for n in &unused {
        let _ = engine.network_remove(n);
    }
    Ok(json!({"NetworksDeleted": unused}))
}

fn wait(req: &Request, engine: &Engine, id: &str, w: &mut UnixStream) -> io::Result<Next> {
    let id = match engine.resolve(id) {
        Ok(id) => id,
        Err(e) => {
            error(w, e)?;
            return Ok(Next::KeepAlive);
        }
    };
    // Headers go out first: clients take them as "the wait is registered"
    // and only then start the container.
    http::begin_stream(w, 200, "application/json")?;
    let condition = req.query("condition").unwrap_or("not-running");
    let body = match engine.wait(&id, condition) {
        Ok(code) => json!({"StatusCode": code, "Error": null}),
        Err(_) => json!({"StatusCode": 0, "Error": null}),
    };
    http::chunk(w, format!("{body}\n").as_bytes())?;
    http::end(w)?;
    Ok(Next::KeepAlive)
}

fn attach(
    req: &Request,
    engine: &Arc<Engine>,
    key: &str,
    w: &mut UnixStream,
    r: &mut BufReader<UnixStream>,
) -> io::Result<Next> {
    let (want_out, want_err, want_in) = (req.flag("stdout"), req.flag("stderr"), req.flag("stdin"));
    let (id, tty, rx) = match engine.attach(key) {
        Ok(v) => v,
        Err(e) => {
            error(w, e)?;
            return Ok(Next::KeepAlive);
        }
    };
    http::upgrade(w)?;
    if want_in {
        forward_stdin(engine.clone(), id, r)?;
    }
    pump(rx, tty, want_out, want_err, w)?;
    let _ = w.shutdown(std::net::Shutdown::Write);
    Ok(Next::Close)
}

/// Copies the client's side of a hijacked connection into the process's
/// stdin, on its own thread, ending with CloseStdin at EOF.
fn forward_stdin(engine: Arc<Engine>, id: String, r: &mut BufReader<UnixStream>) -> io::Result<()> {
    // Anything the client sent right behind the request is already buffered.
    let early = r.buffer().to_vec();
    let mut stream = r.get_ref().try_clone()?;
    std::thread::Builder::new()
        .name("kiri-stdin".into())
        .spawn(move || {
            if !early.is_empty() && engine.input(&id, &early).is_err() {
                return;
            }
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match io::Read::read(&mut stream, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if engine.input(&id, &buf[..n]).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = engine.close_stdin(&id);
        })?;
    Ok(())
}

/// Sends output to an attached client: raw for a terminal, Docker's
/// multiplexed frames otherwise. Ends when the process exits.
fn pump(
    rx: std::sync::mpsc::Receiver<(Stream, Vec<u8>)>,
    tty: bool,
    want_out: bool,
    want_err: bool,
    w: &mut UnixStream,
) -> io::Result<()> {
    for (stream, data) in rx {
        let wanted = match stream {
            Stream::Stdout => want_out,
            Stream::Stderr => want_err,
        };
        if !wanted {
            continue;
        }
        if tty {
            io::Write::write_all(w, &data)?;
        } else {
            http::mux_frame(w, stream as u8, &data)?;
        }
    }
    Ok(())
}

fn resize(req: &Request, engine: &Engine, key: &str) -> Result<(), Error> {
    let n = |k: &str| {
        req.query(k)
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0)
    };
    let (rows, cols) = (n("h"), n("w"));
    if rows == 0 || cols == 0 {
        return Ok(());
    }
    engine.resize(key, rows, cols)
}

fn exec_create(req: &Request, engine: &Engine, key: &str) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let config = ExecConfig {
        console_size: console_size(&body["ConsoleSize"]),
        argv: strings(&body["Cmd"]).unwrap_or_default(),
        env: strings(&body["Env"]).unwrap_or_default(),
        cwd: body["WorkingDir"].as_str().unwrap_or_default().to_owned(),
        user: body["User"].as_str().unwrap_or_default().to_owned(),
        tty: body["Tty"].as_bool().unwrap_or(false),
        stdin: body["AttachStdin"].as_bool().unwrap_or(false),
        privileged: body["Privileged"].as_bool().unwrap_or(false),
        internal: false,
    };
    // An exec with no working directory or user inherits the container's.
    let config = engine.with(key, |c| ExecConfig {
        cwd: if config.cwd.is_empty() {
            c.spec.cwd.clone()
        } else {
            config.cwd.clone()
        },
        user: if config.user.is_empty() {
            c.spec.user.clone()
        } else {
            config.user.clone()
        },
        privileged: config.privileged || c.spec.privileged,
        ..config.clone()
    })?;
    let id = engine.exec_create(key, config)?;
    Ok(json!({"Id": id}))
}

fn exec_start(
    req: &Request,
    engine: &Arc<Engine>,
    id: &str,
    w: &mut UnixStream,
    r: &mut BufReader<UnixStream>,
) -> io::Result<Next> {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let detach = body["Detach"].as_bool().unwrap_or(false);
    if let Some(size) = console_size(&body["ConsoleSize"]) {
        engine.exec_console(id, size);
    }
    let stdin = engine.with_exec(id, |e| e.config.stdin).unwrap_or(false);
    let (tty, rx) = match engine.exec_start(id) {
        Ok(v) => v,
        Err(e) => {
            error(w, e)?;
            return Ok(Next::KeepAlive);
        }
    };
    if detach {
        // Nobody will read the output; let it drain on its own thread.
        std::thread::spawn(move || for _ in rx {});
        http::no_content(w)?;
        return Ok(Next::KeepAlive);
    }
    http::upgrade(w)?;
    if stdin {
        forward_stdin(engine.clone(), id.to_owned(), r)?;
    }
    pump(rx, tty, true, true, w)?;
    let _ = w.shutdown(std::net::Shutdown::Write);
    Ok(Next::Close)
}

fn exec_inspect(e: &super::engine::Exec) -> Value {
    json!({
        "ID": e.id,
        "ContainerID": e.container_id,
        "Running": e.running,
        "ExitCode": e.exit_code,
        "Pid": e.pid,
        "OpenStdin": e.config.stdin,
        "OpenStdout": true,
        "OpenStderr": true,
        "ProcessConfig": {
            "entrypoint": e.config.argv.first(),
            "arguments": e.config.argv.iter().skip(1).collect::<Vec<_>>(),
            "tty": e.config.tty,
            "user": e.config.user,
            "privileged": false,
        },
    })
}

fn logs(req: &Request, engine: &Engine, id: &str, w: &mut UnixStream) -> io::Result<Next> {
    use super::logfile::{Record, parse_tail, parse_time};
    let (want_out, want_err, stamps) = (
        req.flag("stdout"),
        req.flag("stderr"),
        req.flag("timestamps"),
    );
    if !want_out && !want_err {
        error(
            w,
            Error::BadRequest("Bad parameters: you must choose at least one stream".into()),
        )?;
        return Ok(Next::KeepAlive);
    }
    let tail = parse_tail(req.query("tail"));
    let since = req.query("since").and_then(parse_time);
    let until = req.query("until").and_then(parse_time);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    // An `until` already past ends the answer with what is stored.
    let follow = req.flag("follow") && until.is_none_or(|u| u > now);
    let logs = match engine.logs(id, follow) {
        Ok(Some(l)) => l,
        Ok(None) => {
            http::error(w, 501, "configured logging driver does not support reading")?;
            return Ok(Next::KeepAlive);
        }
        Err(e) => {
            error(w, e)?;
            return Ok(Next::KeepAlive);
        }
    };
    let tty = logs.tty;
    let content_type = if tty {
        "application/vnd.docker.raw-stream"
    } else {
        "application/vnd.docker.multiplexed-stream"
    };
    http::begin_stream(w, 200, content_type)?;
    // Frames are gathered into chunks of up to 32 KiB: one write per line
    // would make a long log slow to read.
    let mut out = Vec::with_capacity(40 << 10);
    let mut line = Vec::new();
    let mut put = |r: &Record, out: &mut Vec<u8>| -> io::Result<()> {
        let wanted = match r.stream {
            Stream::Stdout => want_out,
            Stream::Stderr => want_err,
        };
        if !wanted || since.is_some_and(|s| r.time < s) {
            return Ok(());
        }
        let data = if stamps {
            line.clear();
            line.extend_from_slice(
                time::rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(r.time))
                    .as_bytes(),
            );
            line.push(b' ');
            line.extend_from_slice(&r.data);
            &line[..]
        } else {
            &r.data[..]
        };
        if tty {
            out.extend_from_slice(data)
        } else {
            http::mux_frame(out, r.stream as u8, data)?
        }
        Ok(())
    };
    let past_until = |r: &Record| until.is_some_and(|u| r.time > u);
    let mut ended = false;
    for r in logs.past.tail(tail) {
        if past_until(&r) {
            ended = true;
            break;
        }
        put(&r, &mut out)?;
        if out.len() >= 32 << 10 {
            http::chunk(w, &out)?;
            out.clear();
        }
    }
    http::chunk(w, &out)?;
    out.clear();
    if let (Some(rx), false) = (logs.live, ended) {
        'live: while let Ok(r) = rx.recv() {
            let mut next = Some(r);
            // Whatever else is already waiting goes in the same chunk.
            while let Some(r) = next {
                if past_until(&r) {
                    http::chunk(w, &out)?;
                    break 'live;
                }
                put(&r, &mut out)?;
                next = if out.len() < 32 << 10 {
                    rx.try_recv().ok()
                } else {
                    None
                };
            }
            http::chunk(w, &out)?;
            out.clear();
        }
    }
    http::end(w)?;
    Ok(Next::KeepAlive)
}

/// `KILL`, `SIGTERM`, `9` … as a Linux signal number: it is delivered in
/// the guest, and macOS numbers several signals differently.
/// One frame from a guest copy connection.
pub(super) fn guest_frame(f: &mut std::fs::File) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = vec![0u8; 4];
    f.read_exact(&mut buf)?;
    let len = kiri_proto::frame_len(buf[..4].try_into().expect("4 bytes"))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
    buf.resize(len, 0);
    f.read_exact(&mut buf[4..])?;
    Ok(buf)
}

/// Sends a copy-in archive as `copy_flags::CHUNKED` chunks, then the empty
/// one that ends it. An error is the archive's own source failing; if the
/// guest stops reading (it refused, and says why) sending just stops.
pub(super) fn send_chunks(
    archive: &mut dyn io::Read,
    to_guest: &mut impl io::Write,
) -> io::Result<()> {
    let mut buf = vec![0u8; 4 + (256 << 10)];
    loop {
        let n = match archive.read(&mut buf[4..]) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        buf[..4].copy_from_slice(&(n as u32).to_le_bytes());
        if to_guest.write_all(&buf[..4 + n]).is_err() || n == 0 {
            return Ok(());
        }
    }
}

/// The guest says why a copy failed; Docker's status codes follow from it.
pub(super) fn copy_error(m: &str) -> Error {
    if let Some(m) = m.strip_prefix("notfound: ") {
        Error::NotFound(m.to_owned())
    } else if let Some(m) = m.strip_prefix("notdir: ") {
        Error::BadRequest(m.to_owned())
    } else if m.contains("no such container") {
        Error::NotFound(m.to_owned())
    } else {
        Error::BadRequest(m.to_owned())
    }
}

/// Go's `os.FileMode` bits, which the CLI reads from the stat header.
fn go_file_mode(mode: u32) -> u32 {
    let mut m = mode & 0o777;
    m |= match mode & 0o170000 {
        0o040000 => 1 << 31,
        0o120000 => 1 << 27,
        0o060000 => 1 << 26,
        0o020000 => (1 << 26) | (1 << 21),
        0o010000 => 1 << 25,
        0o140000 => 1 << 24,
        _ => 0,
    };
    if mode & 0o4000 != 0 {
        m |= 1 << 23;
    }
    if mode & 0o2000 != 0 {
        m |= 1 << 22;
    }
    if mode & 0o1000 != 0 {
        m |= 1 << 20;
    }
    m
}

fn path_stat_header(e: &kiri_proto::FsEntry<'_>, link: &str) -> String {
    let mtime = std::time::UNIX_EPOCH + Duration::new(e.mtime.max(0) as u64, e.mtime_nsec);
    let stat = json!({"name": e.name, "size": e.size, "mode": go_file_mode(e.mode), "mtime": time::rfc3339(mtime), "linkTarget": link});
    format!(
        "X-Docker-Container-Path-Stat: {}\r\n",
        kiri_image::b64::encode(stat.to_string().as_bytes())
    )
}

/// `docker cp` out of a container (GET), or the stat it starts with (HEAD).
fn archive_get(req: &Request, engine: &Engine, id: &str, w: &mut UnixStream) -> io::Result<Next> {
    use std::io::Read;
    let path = req.query("path").unwrap_or("/");
    let op = if req.method == "HEAD" {
        kiri_proto::copy_op::STAT
    } else {
        kiri_proto::copy_op::OUT
    };
    let stream = match engine.copy(id, op, path, 0) {
        Ok(s) => s,
        Err(e) => return reply(w, Err(e), 200),
    };
    let mut f = std::fs::File::from(stream.fd.try_clone()?);
    let frame = guest_frame(&mut f)?;
    let header = match kiri_proto::decode(&frame) {
        Ok(kiri_proto::Msg::CopyStat { entry, link }) => path_stat_header(&entry, link),
        Ok(kiri_proto::Msg::Error(m)) => {
            // HEAD answers carry no body, so the reason goes nowhere; the CLI
            // reports "no such file" from the status alone.
            if req.method == "HEAD" {
                let status = match copy_error(m) {
                    Error::NotFound(_) => 404,
                    _ => 400,
                };
                http::head_only(w, status, "")?;
                return Ok(Next::KeepAlive);
            }
            return reply(w, Err(copy_error(m)), 200);
        }
        other => {
            return reply(
                w,
                Err(Error::Io(io::Error::other(format!(
                    "unexpected copy reply: {other:?}"
                )))),
                200,
            );
        }
    };
    if req.method == "HEAD" {
        http::head_only(w, 200, &header)?;
        return Ok(Next::KeepAlive);
    }
    http::begin_stream_with(w, 200, "application/x-tar", &header)?;
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

/// `docker cp` into a container: the body is a tar archive, streamed to the
/// guest as it arrives.
fn archive_put(
    req: &Request,
    engine: &Engine,
    id: &str,
    w: &mut UnixStream,
    r: &mut BufReader<UnixStream>,
) -> io::Result<Next> {
    use kiri_proto::copy_flags;
    let path = req.query("path").unwrap_or("/");
    let mut flags = copy_flags::CHUNKED;
    if req.flag("copyUIDGID") {
        flags |= copy_flags::KEEP_OWNERS;
    }
    if req.flag("noOverwriteDirNonDir") {
        flags |= copy_flags::NO_OVERWRITE_DIR_NON_DIR;
    }
    let stream = match engine.copy(id, kiri_proto::copy_op::IN, path, flags) {
        Ok(s) => s,
        Err(e) => {
            reply(w, Err(e), 200)?;
            return Ok(Next::Close);
        }
    };
    let mut to_guest = std::fs::File::from(stream.fd.try_clone()?);
    if let Err(e) = send_chunks(&mut http::Body::new(req, r), &mut to_guest) {
        // The client's body broke off: dropping the stream ends the guest's copy.
        error(w, Error::Io(e))?;
        return Ok(Next::Close);
    }
    let mut from_guest = std::fs::File::from(stream.fd.try_clone()?);
    let result = match guest_frame(&mut from_guest) {
        Ok(frame) => match kiri_proto::decode(&frame) {
            Ok(kiri_proto::Msg::CopyDone) => Ok(()),
            Ok(kiri_proto::Msg::Error(m)) => Err(copy_error(m)),
            Ok(other) => Err(Error::Io(io::Error::other(format!(
                "unexpected copy reply: {other:?}"
            )))),
            Err(e) => Err(Error::Io(io::Error::other(format!(
                "bad copy reply: {e:?}"
            )))),
        },
        Err(e) => Err(Error::Io(e)),
    };
    // Docker answers 200 with no body.
    match result {
        Ok(()) => http::respond(w, 200, "text/plain", b"")?,
        Err(e) => error(w, e)?,
    }
    // The body may be partly unread after a refusal: this connection is done.
    Ok(Next::Close)
}

/// A signal by name or number for a stop signal Docker has already
/// accepted; one it could not parse is SIGTERM, Docker's default.
pub(super) fn signal(name: &str) -> u32 {
    parse_signal(name).unwrap_or(15)
}

/// Linux's signals by name, as Docker's `signal.SignalMap` has them.
const SIGNALS: [(&str, u32); 34] = [
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("IOT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("KILL", 9),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("STKFLT", 16),
    ("CHLD", 17),
    ("CLD", 17),
    ("CONT", 18),
    ("STOP", 19),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("POLL", 29),
    ("PWR", 30),
    ("SYS", 31),
];

/// Docker's `signal.ParseSignal` plus its platform check: a number, or a
/// name with or without `SIG` in any case (`term`, `SIGPWR`, `RTMIN+3`,
/// `RTMAX-2`). Anything else is Docker's 400, never a guess.
pub(super) fn parse_signal(raw: &str) -> Result<u32, Error> {
    let invalid = || Error::BadRequest(format!("Invalid signal: {raw}"));
    if let Ok(n) = raw.parse::<i64>() {
        return match n {
            0 => Err(invalid()),
            1..=31 | 34..=64 => Ok(n as u32),
            _ => Err(Error::BadRequest(format!(
                "the linux daemon does not support signal {n}"
            ))),
        };
    }
    let upper = raw.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    if let Some(&(_, n)) = SIGNALS.iter().find(|(s, _)| *s == name) {
        return Ok(n);
    }
    // Real-time signals: RTMIN is 34, RTMAX 64; Docker names RTMIN+1…+15
    // and RTMAX-14…-1.
    let offset = |rest: &str, sign: char| -> Option<u32> {
        if rest.is_empty() {
            return Some(0);
        }
        let n: u32 = rest.strip_prefix(sign)?.parse().ok()?;
        (1..=15).contains(&n).then_some(n)
    };
    if let Some(n) = name.strip_prefix("RTMIN").and_then(|r| offset(r, '+')) {
        return Ok(34 + n);
    }
    if let Some(n) = name
        .strip_prefix("RTMAX")
        .and_then(|r| offset(r, '-'))
        .filter(|n| *n <= 14)
    {
        return Ok(64 - n);
    }
    Err(invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_in_archives_go_as_chunks_with_an_empty_one_last() {
        let archive = vec![7u8; 300 << 10];
        let mut wire = Vec::new();
        send_chunks(&mut &archive[..], &mut wire).unwrap();
        let (mut at, mut got) = (0, Vec::new());
        loop {
            let len = u32::from_le_bytes(wire[at..at + 4].try_into().unwrap()) as usize;
            at += 4;
            if len == 0 {
                break;
            }
            got.extend_from_slice(&wire[at..at + len]);
            at += len;
        }
        assert_eq!((got, at), (archive, wire.len()));
    }

    #[test]
    fn intel_macs_refuse_images_without_amd64() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(amd64_only_refusal("alpine", &p(&["linux/amd64"])), None);
        assert_eq!(
            amd64_only_refusal("x", &p(&["linux/arm64", "linux/amd64/v3"])),
            None
        );
        let why = amd64_only_refusal("arm-only:1", &p(&["linux/arm64", "linux/arm/v7"])).unwrap();
        assert!(why.starts_with("arm-only:1 is built for linux/arm64, linux/arm/v7, and this Intel Mac runs linux/amd64"), "{why}");
        // Everywhere but an Intel Mac this never refuses.
        if !cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            assert_eq!(intel_mac_refusal("arm-only", &p(&["linux/arm64"])), None);
        }
    }

    #[test]
    fn mac_shares_and_vm_paths() {
        for p in [
            "/Users",
            "/Users/me/src",
            "/Volumes/ext",
            "/private/tmp/x",
            "/private/etc/hosts",
            "/tmp",
            "/tmp/kind",
            "/var/folders/ab/T",
        ] {
            assert!(mac_shared(p), "{p} should come from the Mac");
        }
        for p in [
            "/lib/modules",
            "/etc/hostname",
            "/var/lib/x",
            "/var",
            "/run/foo",
            "/Users2",
            "/tmpx",
            "/privatefoo",
            "/var/folders2",
            "/",
        ] {
            assert!(!mac_shared(p), "{p} should be the VM's");
        }
    }

    #[test]
    fn signals_like_docker() {
        for (raw, n) in [
            ("KILL", 9),
            ("SIGKILL", 9),
            ("term", 15),
            ("SigTerm", 15),
            ("ALRM", 14),
            ("PWR", 30),
            ("SIGPWR", 30),
            ("TSTP", 20),
            ("PIPE", 13),
            ("CHLD", 17),
            ("CLD", 17),
            ("IOT", 6),
            ("WINCH", 28),
            ("9", 9),
            ("15", 15),
            ("64", 64),
            ("RTMIN", 34),
            ("SIGRTMIN+3", 37),
            ("RTMIN+15", 49),
            ("RTMAX", 64),
            ("RTMAX-14", 50),
            ("rtmax-1", 63),
        ] {
            assert_eq!(parse_signal(raw).ok(), Some(n), "{raw}");
        }
        for raw in [
            "FOO",
            "SIG",
            "",
            "RTMIN+16",
            "RTMAX-15",
            "RTMIN-1",
            "SIGSIGKILL",
        ] {
            assert!(
                matches!(parse_signal(raw), Err(Error::BadRequest(m)) if m == format!("Invalid signal: {raw}")),
                "{raw}"
            );
        }
        assert!(matches!(parse_signal("0"), Err(Error::BadRequest(m)) if m == "Invalid signal: 0"));
        for raw in ["32", "65", "-9"] {
            assert!(
                matches!(parse_signal(raw), Err(Error::BadRequest(m)) if m.starts_with("the linux daemon does not support signal")),
                "{raw}"
            );
        }
        // A stop signal Docker would not parse stops with SIGTERM.
        assert_eq!(signal("BOGUS"), 15);
    }
}

#[cfg(test)]
mod network_option_tests {
    use super::*;

    fn refused(body: Value) -> String {
        match container_mode_allowed(&body, "web") {
            Err(Error::BadRequest(m)) => m,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn container_network_mode_refuses_what_docker_refuses() {
        assert!(
            container_mode_allowed(
                &json!({"HostConfig": {"NetworkMode": "container:web"}}),
                "web"
            )
            .is_ok()
        );
        // Compose sends empty lists and maps for what a service leaves out.
        assert!(container_mode_allowed(&json!({"Hostname": "", "ExposedPorts": {}, "HostConfig": {"PortBindings": {}, "Dns": [], "Links": null}}), "web").is_ok());
        assert!(container_mode_allowed(&json!({}), "").is_err());
        assert_eq!(
            refused(json!({"HostConfig": {"PortBindings": {"80/tcp": [{"HostPort": "8080"}]}}})),
            "conflicting options: port publishing and the container type network mode"
        );
        assert_eq!(
            refused(json!({"HostConfig": {"PublishAllPorts": true}})),
            "conflicting options: port publishing and the container type network mode"
        );
        assert_eq!(
            refused(json!({"Hostname": "x"})),
            "conflicting options: hostname and the network mode"
        );
        assert_eq!(
            refused(json!({"HostConfig": {"Dns": ["1.1.1.1"]}})),
            "conflicting options: dns and the network mode"
        );
        assert_eq!(
            refused(json!({"HostConfig": {"ExtraHosts": ["a:1.2.3.4"]}})),
            "conflicting options: custom host-to-IP mapping and the network mode"
        );
        assert_eq!(
            refused(json!({"ExposedPorts": {"80/tcp": {}}})),
            "conflicting options: port exposing and the container type network mode"
        );
        assert!(refused(json!({"HostConfig": {"Links": ["db:db"]}})).contains("links"));
    }

    #[test]
    fn endpoint_addresses_are_checked_for_form() {
        let ok = endpoint_address(
            &json!({"IPAMConfig": {"IPv4Address": "10.5.0.9"}, "MacAddress": "02:42:0a:05:00:09"}),
        )
        .unwrap();
        let v6 = endpoint_address(&json!({"IPAMConfig": {"IPv6Address": "fd00::9"}})).unwrap();
        assert_eq!(v6.1, "fd00::9");
        assert!(endpoint_address(&json!({"IPAMConfig": {"IPv6Address": "fd00::zz"}})).is_err());
        assert_eq!(
            ok,
            (
                "10.5.0.9".to_owned(),
                String::new(),
                "02:42:0a:05:00:09".to_owned()
            )
        );
        assert_eq!(
            endpoint_address(&json!({})).unwrap(),
            (String::new(), String::new(), String::new())
        );
        assert!(endpoint_address(&json!({"IPAMConfig": {"IPv4Address": "10.5.0.300"}})).is_err());
        assert!(endpoint_address(&json!({"MacAddress": "02:42:0a"})).is_err());
    }

    #[test]
    fn links_parse_in_every_form() {
        assert_eq!(parse_link("db:database"), Some(("db", "database")));
        assert_eq!(parse_link("db"), Some(("db", "db")));
        assert_eq!(parse_link("/db:/web/database"), Some(("db", "database")));
        assert_eq!(parse_link(":x"), None);
    }
}
