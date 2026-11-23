//! The Docker Engine API: the subset `docker run`, `ps`, `logs`, `stop`,
//! `rm`, `images`, `pull`, `rmi`, `version` and `info` use. Anything else
//! answers 404 with a message saying Kiri does not support it yet.

use std::io::{self, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use kiri_image::{Client, Platform, Progress, Reference};
use kiri_proto::Stream;

use super::caller::Caller;
use super::engine::{Container, Engine, Error, ExecConfig, RestartPolicy, Spec, Status};
use super::filters::Filters;
use super::http::{self, Request};
use super::time;

/// Serves one client connection until it closes. `caller`: where it
/// arrived (caller.rs), which decides whether Kiri's settings are open to it.
pub fn serve(stream: UnixStream, engine: Arc<Engine>, caller: Caller) -> io::Result<()> {
    let mut w = stream.try_clone()?;
    let mut r = BufReader::new(stream);
    while let Some(req) = http::read_request(&mut r, streams_body)? {
        // A body too large to hold: answered and ended, never routed.
        if req.too_large() {
            return http::refuse_too_large(&req, &mut w, &mut r).or(Ok(()));
        }
        // The window's action this request is part of: the engine's lines
        // for it carry the same op.
        let _op = req.header("X-Kiri-Op").filter(|o| crate::diag::valid_op(o)).map(|o| crate::diag::enter_op(o.to_owned()));
        http::serving(&req);
        // Kiri's own window checks in every few seconds; that isn't use, and
        // counting it would keep the VM from ever sleeping while it's open.
        if req.header("X-Kiri-Background").is_none() {
            engine.touch();
        }
        match route(&req, &engine, caller, &mut w, &mut r) {
            // `Connection: close` or HTTP/1.0: the client reads to the end of
            // the stream (a 204 has no length to read by), which `close`
            // gives it cleanly.
            Ok(Next::KeepAlive) if req.wants_close() => {
                http::close(&w, &mut r);
                return Ok(());
            }
            // A body its handler left unread that can't be read out would be
            // taken for the next request: the connection ends here instead.
            Ok(Next::KeepAlive) if !http::finish_body(&req, &mut r) => {
                http::close(&w, &mut r);
                return Ok(());
            }
            Ok(Next::KeepAlive) => {}
            // An upload answered before its body was read to the end (a
            // refusal, an archive's padding): the client may still be sending.
            Ok(Next::Close) if req.body_left() => {
                http::close(&w, &mut r);
                return Ok(());
            }
            Ok(Next::Close) => return Ok(()),
            Err(e) => {
                // The response may be half written; closing is the only safe
                // answer. A client hanging up first (the docker CLI does, with
                // spare wait connections) is not worth a log line.
                if !matches!(e.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset) {
                    crate::diag::warn!("request failed midway", method = req.method, path = req.path, error = e);
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Requests whose bodies can be any size: archives and image tarballs
/// (`/images/create` is `docker import`'s; a pull's, when a client sends
/// one, is read out after it, by `http::finish_body`).
fn streams_body(method: &str, path: &str) -> bool {
    matches!((method, path), ("PUT", p) if p.starts_with("/containers/") && p.ends_with("/archive")) || (method == "POST" && matches!(path, "/images/load" | "/images/create" | "/build"))
}

pub(super) enum Next {
    KeepAlive,
    Close,
}

pub(super) fn reply(w: &mut UnixStream, result: Result<Value, Error>, ok: u16) -> io::Result<Next> {
    match result {
        // An empty answer keeps the status Docker gives it: 204 for most,
        // 201 for a tag, 200 for a network connect or disconnect. Client
        // libraries that check the exact status need it.
        Ok(Value::Null) => http::empty(w, ok)?,
        Ok(v) => http::json(w, ok, &v.to_string())?,
        Err(e) => error(w, e)?,
    }
    Ok(Next::KeepAlive)
}

pub(super) fn error(w: &mut UnixStream, e: Error) -> io::Result<()> {
    let (status, m) = status_of(e);
    http::error(w, status, &m)
}

/// `error` for a request whose connection closes after it (`http::CLOSE`).
pub(super) fn error_closing(w: &mut UnixStream, e: Error) -> io::Result<()> {
    let (status, m) = status_of(e);
    http::error_closing(w, status, &m)
}

fn status_of(e: Error) -> (u16, String) {
    match e {
        Error::NotFound(m) => (404, m),
        Error::Conflict(m) => (409, m),
        Error::BadRequest(m) => (400, m),
        Error::Unauthorized(m) => (401, m),
        Error::Io(e) => (500, e.to_string()),
    }
}

/// Kiri's own endpoints (`/kiri/…`) that change something and that a
/// container given the engine's socket may still call: starting the VM it
/// already runs in (`kiri start` in a container's shell). Every other one
/// that is not a GET (the settings, Stop, Sleep, ports, USB, the builder's
/// limits, and any added later) is for this machine's programs only, as
/// Docker's API has nothing that changes the engine's own settings.
const CONTAINER_MAY: &[(&str, &str)] = &[("POST", "/kiri/wake")];

/// Whether a container's connection may make this request.
fn container_may(method: &str, path: &str) -> bool {
    let kiri = path == "/kiri" || path.starts_with("/kiri/");
    !kiri || matches!(method, "GET" | "HEAD") || CONTAINER_MAY.contains(&(method, path))
}

fn route(req: &Request, engine: &Arc<Engine>, caller: Caller, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<Next> {
    if req.api_version.is_some_and(|v| v < http::MIN_API_VERSION) {
        let (major, minor) = req.api_version.unwrap_or_default();
        let (min_major, min_minor) = http::MIN_API_VERSION;
        return reply(w, Err(Error::BadRequest(format!("client version {major}.{minor} is too old. Minimum supported API version is {min_major}.{min_minor}, please upgrade your client to a newer version"))), 200);
    }
    let p = req.path.as_str();
    let m = req.method.as_str();
    // Kiri's own endpoints that change something are for this machine's
    // programs (caller.rs), not for a container given the socket.
    if caller == Caller::Container && !container_may(m, p) {
        // Closed after the answer: a body the request carried is not left
        // on the connection to be read as the next request.
        http::error(w, 403, "this is changed in Kiri's window or with the kiri command on this machine, not from a container")?;
        return Ok(Next::Close);
    }
    if let Some(done) = super::ops::route(req, engine, w) {
        return done;
    }
    match (m, p) {
        ("GET" | "HEAD", "/_ping") => {
            // Builder-Version 1: the classic POST /build, which Kiri serves
            // with its managed BuildKit (builder.rs). "2" would tell older
            // CLIs and Compose to use BuildKit inside the engine (/session,
            // /grpc), which Kiri doesn't have.
            http::respond_with(w, 200, "text/plain; charset=utf-8", "Builder-Version: 1\r\n", if m == "HEAD" { b"" } else { b"OK" })?;
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
        ("GET", "/kiri/daemon") => return reply(w, Ok(super::daemon_cfg::report()), 200),
        ("POST", "/kiri/daemon") => return reply(w, set_daemon(req), 200),
        ("GET", "/kiri/proxy") => return reply(w, Ok(super::proxy::report()), 200),
        ("POST", "/kiri/proxy") => return reply(w, set_proxy(req), 200),
        ("PUT", "/kiri/proxy/system") => return reply(w, set_host_proxy(req), 200),
        // daemon.json's registry keys: Settings → Registries, `kiri registry`.
        ("GET", "/kiri/registries") => return reply(w, Ok(super::daemon_json::report()), 200),
        ("POST", "/kiri/registries") => return reply(w, set_registries(req, engine), 200),
        ("GET", "/kiri/resources") => return reply(w, Ok(resources(engine)), 200),
        // Settings → Domains: the container-domain routers' ports.
        ("GET", "/kiri/domains") => return reply(w, Ok(super::domains::report()), 200),
        ("POST", "/kiri/domains") => return reply(w, set_domain_ports(req, engine), 200),
        // Back to the ports a start gives (a test copy's routers stop).
        ("DELETE", "/kiri/domains") => return reply(w, super::domains::reset(engine).map_err(Error::Conflict), 200),
        // USB passthrough (macOS: the root helper `kiri usb serve` connects here).
        #[cfg(target_os = "macos")]
        ("POST", "/kiri/usb") => return super::usb::attach(req, engine, w, r),
        #[cfg(target_os = "macos")]
        ("GET", "/kiri/usb") => return reply(w, Ok(super::usb::list(engine)), 200),
        // Linux and WSL: where each of this machine's USB devices shows up.
        #[cfg(not(target_os = "macos"))]
        ("GET", "/kiri/usb/nodes") => return reply(w, Ok(crate::usb::host_nodes()), 200),
        #[cfg(target_os = "macos")]
        ("DELETE", p) if p.strip_prefix("/kiri/usb/").is_some_and(|l| !l.is_empty()) => {
            return reply(w, super::usb::detach(p.strip_prefix("/kiri/usb/").unwrap_or_default()), 200);
        }
        ("POST", "/kiri/resources") => return reply(w, set_resources(req).map(|()| resources(engine)), 200),
        ("POST", p) if p.strip_prefix("/kiri/containers/").and_then(|r| r.strip_suffix("/ports")).is_some_and(|id| !id.is_empty()) => {
            let id = p.strip_prefix("/kiri/containers/").and_then(|r| r.strip_suffix("/ports")).unwrap_or_default();
            return reply(w, set_ports(req, engine, id), 204);
        }
        // Kiri's own: the TCP ports a container listens on, for publishing
        // ones its image never declared. `local` are the ones only it can
        // reach (bound to loopback).
        ("GET", p) if p.strip_prefix("/kiri/containers/").and_then(|r| r.strip_suffix("/listening")).is_some_and(|id| !id.is_empty()) => {
            let id = p.strip_prefix("/kiri/containers/").and_then(|r| r.strip_suffix("/listening")).unwrap_or_default();
            return reply(w, engine.listening(id).map(|(tcp, local)| json!({"tcp": tcp, "local": local})), 200);
        }
        // Kiri's own: a loopback port reaching a Kubernetes service or pod,
        // for Share. `{"ip": "10.43.0.5", "port": 80}` → `{"port": 50123}`.
        ("POST", "/kiri/k8s/forward") => return reply(w, k8s_forward(req, engine), 200),
        ("DELETE", p) if p.strip_prefix("/kiri/k8s/forward/").is_some_and(|n| n.parse::<u16>().is_ok()) => {
            engine.k8s_unforward(p.strip_prefix("/kiri/k8s/forward/").and_then(|n| n.parse().ok()).unwrap_or(0));
            return reply(w, Ok(Value::Null), 204);
        }
        ("POST", p) if p.strip_prefix("/kiri/containers/").and_then(|r| r.strip_suffix("/env")).is_some_and(|id| !id.is_empty()) => {
            let id = p.strip_prefix("/kiri/containers/").and_then(|r| r.strip_suffix("/env")).unwrap_or_default();
            return reply(w, set_env(req, engine, id), 204);
        }
        // An image's page for kiri-win, whose window can't read the store
        // inside WSL: its layers and its Grype scans (crate::scan), as the
        // Mac's window reads them itself.
        ("GET", "/kiri/images/layers") => return reply(w, crate::builds::image_layers(req.query("name").unwrap_or_default()).map_err(Error::Io), 200),
        ("POST", "/kiri/images/scan") => return image_scan(req, w),
        ("GET", "/kiri/images/scans") => return reply(w, crate::scan::history(req.query("name").unwrap_or_default()).map_err(Error::Io), 200),
        ("GET", "/kiri/images/scan") => {
            let at = req.query("at").and_then(|a| a.parse().ok());
            return reply(w, crate::scan::open(req.query("name").unwrap_or_default(), at).map(Option::unwrap_or_default).map_err(Error::Io), 200);
        }
        ("GET", "/kiri/images/sbom") => {
            let at = req.query("at").and_then(|a| a.parse().ok()).unwrap_or(0);
            match crate::scan::sbom(req.query("name").unwrap_or_default(), at).and_then(std::fs::read) {
                Ok(b) => http::respond(w, 200, "application/json", &b)?,
                Err(e) => error(w, Error::Io(e))?,
            }
            return Ok(Next::KeepAlive);
        }
        ("GET", "/info") => return reply(w, Ok(info(engine)), 200),
        ("POST", "/build") => return super::builder::build(req, engine, w, r),
        ("POST", "/build/cancel") => return reply(w, super::builder::cancel(req, engine), 200),
        ("POST", "/build/prune") => return reply(w, super::builder::prune(req, engine), 200),
        ("GET", "/kiri/builder") => return reply(w, Ok(super::builder::settings_report(engine)), 200),
        ("POST", "/kiri/builder") => return reply(w, super::builder::set_settings(req).map(|()| super::builder::settings_report(engine)), 200),
        ("GET", "/images/json") => return reply(w, images(req, engine), 200),
        ("POST", "/images/create") if req.query("fromSrc").is_some_and(|s| !s.is_empty()) => return image_import(req, engine, w, r),
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
    if let Some(name) = p.strip_prefix("/distribution/").and_then(|r| r.strip_suffix("/json")) {
        if m == "GET" {
            return reply(w, distribution(req, name), 200);
        }
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
            return reply(w, image_remove(engine, rest, req.flag("force")), 200);
        }
    }
    if let Some(rest) = p.strip_prefix("/containers/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        match (m, action) {
            ("GET", "json") => return reply(w, container_inspect(engine, id), 200),
            ("POST", "start") => {
                // An idle BuildKit container that Kiri paused itself counts
                // as running: the next build resumes it.
                match engine.with(id, |c| (c.status == Status::Running, c.paused && !super::engine::is_builder(c))) {
                    Ok((_, true)) => return reply(w, Err(Error::Conflict("cannot start a paused container, try unpause instead".into())), 204),
                    Ok((true, _)) => {
                        http::respond(w, 304, "", b"")?;
                        return Ok(Next::KeepAlive);
                    }
                    Err(e) => return reply(w, Err(e), 204),
                    _ => {}
                }
                let started = super::builder::start(engine, id).unwrap_or_else(|| engine.start(id));
                return reply(w, started.map(|()| Value::Null), 204);
            }
            ("POST", "stop") => {
                let t = stop_timeout(req, engine, id);
                // The engine decides: a stop that only cancels a pending
                // restart still changed something.
                return match engine.stop(id, t) {
                    Ok(false) => {
                        http::respond(w, 304, "", b"")?;
                        Ok(Next::KeepAlive)
                    }
                    r => reply(w, r.map(|_| Value::Null), 204),
                };
            }
            ("POST", "restart") => {
                let t = stop_timeout(req, engine, id);
                let r = engine.stop(id, t).and_then(|_| {
                    // Wait for the exit to be recorded before starting again.
                    engine.wait(id, "not-running")?;
                    super::builder::start(engine, id).unwrap_or_else(|| engine.start(id))
                });
                return reply(w, r.map(|()| Value::Null), 204);
            }
            ("POST", "kill") => {
                let r = parse_signal(req.query("signal").filter(|s| !s.is_empty()).unwrap_or("KILL")).and_then(|sig| engine.kill(id, sig));
                return reply(w, r.map(|()| Value::Null), 204);
            }
            ("POST", "wait") => return wait(req, engine, id, w),
            ("POST", "attach") => return attach(req, engine, id, w, r),
            ("GET", "logs") => return logs(req, engine, id, w),
            ("POST", "resize") => return reply(w, resize(req, engine, id).map(|()| json!({})), 200),
            ("POST", "exec") => {
                super::builder::before_exec(engine, id);
                return reply(w, exec_create(req, engine, id), 201);
            }
            ("HEAD" | "GET", "archive") => return archive_get(req, engine, id, w),
            ("PUT", "archive") => return archive_put(req, engine, id, w, r),
            ("DELETE", "") => return reply(w, engine.remove(id, req.flag("force"), req.flag("v")).map(|()| Value::Null), 204),
            _ => {}
        }
    }
    if let Some(name) = p.strip_prefix("/volumes/") {
        match m {
            "GET" => return reply(w, engine.volume_get(name).map(|v| volume_json(&v)).ok_or_else(|| Error::NotFound(format!("get {name}: no such volume"))), 200),
            "DELETE" => return reply(w, engine.volume_remove(name, req.flag("force")).map(|()| Value::Null), 204),
            _ => {}
        }
    }
    if let Some(rest) = p.strip_prefix("/networks/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        match (m, action) {
            ("GET", "") => return reply(w, network_inspect(engine, id), 200),
            ("DELETE", "") => return reply(w, engine.network_remove(id).map(|()| Value::Null).map_err(net_error), 204),
            _ => {}
        }
    }
    if let Some(rest) = p.strip_prefix("/exec/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        match (m, action) {
            ("POST", "start") => return exec_start(req, engine, id, w, r),
            ("POST", "resize") => return reply(w, resize(req, engine, id).map(|()| json!({})), 200),
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
    if platforms.iter().any(|p| p == "linux/amd64" || p.starts_with("linux/amd64/")) {
        return None;
    }
    let built = if platforms.is_empty() { "no platform Kiri can run".to_owned() } else { platforms.join(", ") };
    Some(format!(
        "{image} is built for {built}, and this Intel Mac runs linux/amd64 images (Kiri does not emulate Arm): use an image with a linux/amd64 build"
    ))
}

/// The kernel containers run on, as `uname -r` prints it. On Linux (native
/// and WSL 2) the engine runs straight on the host kernel, so that is read;
/// on macOS it is Kiri's own guest kernel in the VM.
fn kernel_version() -> String {
    #[cfg(target_os = "linux")]
    if let Some(release) = linux_kernel_release() {
        return release;
    }
    GUEST_KERNEL.to_owned()
}

const GUEST_KERNEL: &str = "6.18-kiri";

/// The release of the kernel containers run on, when the daemon can read it:
/// the host's on Linux, nothing on macOS (the VM agent does not report one).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn kernel_release() -> Option<String> {
    #[cfg(target_os = "linux")]
    return linux_kernel_release();
    #[cfg(not(target_os = "linux"))]
    None
}

#[cfg(target_os = "linux")]
fn linux_kernel_release() -> Option<String> {
    parse_release(&std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_release(raw: &str) -> Option<String> {
    let release = raw.trim();
    (!release.is_empty()).then(|| release.to_owned())
}

/// The Docker Engine release whose API Kiri serves (`http::API_VERSION`,
/// 1.47: Docker 27.3 to 27.5), reported as the engine's `Version`. Clients
/// gate on it as a Docker version (Testcontainers wants at least 1.6.0), so
/// Kiri's own version there read as an ancient engine. Kiri's own version is
/// `KiriVersion`, and a component named "Kiri". Moves with `API_VERSION`.
pub const ENGINE_VERSION: &str = "27.5.1";

fn version() -> Value {
    let kiri = env!("CARGO_PKG_VERSION");
    json!({
        "Platform": {"Name": "Kiri"},
        "Components": [
            {"Name": "Engine", "Version": ENGINE_VERSION, "Details": {"ApiVersion": http::API_VERSION, "MinAPIVersion": "1.24", "Arch": go_arch(), "Os": "linux", "KernelVersion": kernel_version(), "GoVersion": "n/a (Rust)", "GitCommit": ""}},
            {"Name": "Kiri", "Version": kiri, "Details": {}},
        ],
        "Version": ENGINE_VERSION,
        "KiriVersion": kiri,
        "ApiVersion": http::API_VERSION,
        "MinAPIVersion": "1.24",
        "Os": "linux",
        "Arch": go_arch(),
        "KernelVersion": kernel_version(),
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
        "ServerVersion": ENGINE_VERSION,
        "KiriVersion": env!("CARGO_PKG_VERSION"),
        "OperatingSystem": "Kiri",
        "OSType": "linux",
        "Architecture": std::env::consts::ARCH,
        "KernelVersion": kernel_version(),
        "NCPU": engine.cpus(),
        "MemTotal": mem_total,
        "DockerRootDir": "/var/lib/kiri",
        "CgroupDriver": "cgroupfs",
        "CgroupVersion": "2",
        "DefaultRuntime": "kiri",
        "Runtimes": {"kiri": {"path": "kiri-init"}},
        "SecurityOptions": ["name=seccomp,profile=builtin", "name=cgroupns"],
        // Compose needs it to attach in `up` and `logs -f` (#346).
        "SystemTime": time::rfc3339(std::time::SystemTime::now()),
        // Not in Docker's /info (the ping's Builder-Version header is), but
        // tools print it: the classic builder API, served by builder.rs.
        "BuilderVersion": "1",
        // `docker info`'s Registry, Insecure Registries and Registry Mirrors:
        // daemon.json's, as Docker reports its own.
        "IndexServerAddress": "https://index.docker.io/v1/",
        "RegistryConfig": super::daemon_json::info(),
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
        "Warnings": engine.records_damage().into_iter().chain(engine.image_damage()).chain(engine.host_warnings()).collect::<Vec<_>>(),
    })
}

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
/// A colon followed by a `/` is a registry's port (`localhost:5000/app`),
/// not a tag, in the pattern and in the name alike.
pub(super) fn reference_matches(pattern: &str, name: &str) -> bool {
    fn tagged(s: &str) -> Option<(&str, &str)> {
        s.rsplit_once(':').filter(|(_, t)| !t.contains('/'))
    }
    let (repo, tag) = tagged(name).unwrap_or((name, "latest"));
    let glob = |p: &str, s: &str| match p.split_once('*') {
        Some((a, b)) => s.len() >= a.len() + b.len() && s.starts_with(a) && s.ends_with(b),
        None => p == s,
    };
    match tagged(pattern) {
        Some((p, t)) => glob(p, repo) && glob(t, tag),
        None => glob(pattern, repo) || glob(pattern, name),
    }
}

pub(super) fn image_config_json(engine: &Engine, digest: &str) -> Value {
    engine.store.read_config(digest).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

pub(super) fn image_labels_from(cfg: &Value) -> Value {
    match &cfg["config"]["Labels"] {
        Value::Object(m) if !m.is_empty() => Value::Object(m.clone()),
        _ => Value::Null,
    }
}

fn merge_labels(image_cfg: &Value, body_labels: &Value) -> Value {
    let mut merged = serde_json::Map::new();
    if let Value::Object(img) = &image_cfg["config"]["Labels"] {
        for (k, v) in img {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Value::Object(req) = body_labels {
        for (k, v) in req {
            merged.insert(k.clone(), v.clone());
        }
    }
    if merged.is_empty() && !body_labels.is_object() {
        Value::Null
    } else {
        Value::Object(merged)
    }
}

fn images(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = filters(req, &["reference", "dangling", "label", "label!", "before", "since", "until"])?;
    let mut list = engine.store.images()?;
    // Newest first, as Docker lists them (#364).
    list.sort_by_key(|i| std::cmp::Reverse(time::parse_unix(&i.created)));
    let now = std::time::SystemTime::now();
    let created = |i: &kiri_image::Image| time::docker_timestamp(&i.created, now);
    // Docker's rules (moby daemon/images/image_list.go): `dangling` is a
    // strict boolean; `before`/`since` name images (the oldest `before`,
    // the newest `since`, an unknown name is an error); `until` is a time
    // and narrows `before`; `reference` never lists an untagged image.
    let dangling = image_list_dangling(&f)?;
    let mut before: Option<f64> = None;
    let mut since: Option<f64> = None;
    for (key, slot) in [("before", &mut before), ("since", &mut since)] {
        for name in f.values(key).unwrap_or_default() {
            let i = engine.image(name)?.ok_or_else(|| Error::NotFound(format!("No such image: {name}")))?;
            if let Some(t) = created(&i) {
                *slot = Some(match (*slot, key) {
                    (None, _) => t,
                    (Some(s), "before") => s.min(t),
                    (Some(s), _) => s.max(t),
                });
            }
        }
    }
    for v in f.values("until").unwrap_or_default() {
        let t = time::docker_timestamp(v, now).ok_or_else(|| Error::BadRequest(format!("failed to parse value as time or duration: {v:?}")))?;
        before = Some(before.map_or(t, |b| b.min(t)));
    }
    let labels = engine.image_labels(&list);
    Ok(Value::Array(
        list.iter()
            .zip(labels)
            .filter_map(|(i, labels)| {
                // Kiri keeps an untagged image under its ID.
                let untagged = i.name.starts_with("sha256:") && !list.iter().any(|o| o.config_digest == i.config_digest && !o.name.starts_with("sha256:"));
                if i.name.starts_with("sha256:") && !untagged {
                    return None;
                }
                if dangling.is_some_and(|d| d != untagged) {
                    return None;
                }
                if (untagged && f.values("reference").is_some()) || (!untagged && !f.any("reference", |p| reference_matches(p, &i.name))) {
                    return None;
                }
                if !f.labels(&labels) {
                    return None;
                }
                let t = created(i);
                if before.is_some_and(|b| !t.is_some_and(|t| t < b)) || since.is_some_and(|s| !t.is_some_and(|t| t > s)) {
                    return None;
                }
                // Untagged: no names; before API 1.43 Docker wrote `<none>` ones.
                let (tags, digests) = match (untagged, req.api_version.is_some_and(|v| v < (1, 43))) {
                    (false, _) => (json!([i.name]), json!(i.repo_digest().into_iter().collect::<Vec<_>>())),
                    (true, false) => (json!([]), json!([])),
                    (true, true) => (json!(["<none>:<none>"]), json!(["<none>@<none>"])),
                };
                Some(json!({
                    "Id": i.config_digest,
                    "ParentId": "",
                    "RepoTags": tags,
                    "RepoDigests": digests,
                    "Created": time::parse_unix(&i.created),
                    "Size": i.size,
                    "SharedSize": -1,
                    "VirtualSize": i.size,
                    "Labels": labels,
                    "Containers": -1,
                    // Not Docker's: when this image arrived here, for the window.
                    "Pulled": i.pulled_at,
                }))
            })
            .collect(),
    ))
}

/// The image list's `dangling` filter as Docker's `GetBoolOrDefault` reads
/// it: `true`/`1` or `false`/`0`; both, or anything else, is refused.
fn image_list_dangling(f: &Filters) -> Result<Option<bool>, Error> {
    let Some(v) = f.values("dangling") else { return Ok(None) };
    let yes = v.iter().any(|s| s == "1" || s == "true");
    let no = v.iter().any(|s| s == "0" || s == "false");
    if yes == no {
        return Err(Error::BadRequest(format!("invalid filter 'dangling=[{}]'", v.join(" "))));
    }
    Ok(Some(yes))
}

fn image_inspect(engine: &Engine, name: &str) -> Result<Value, Error> {
    let i = engine.image(name)?.ok_or_else(|| Error::NotFound(format!("No such image: {name}")))?;
    let cfg = image_config_json(engine, &i.config_digest);
    // Docker's RootFS.Layers are the image config's uncompressed diff_ids,
    // not the manifest's compressed blob digests (scanners like Trivy match
    // them against the tar layers from `docker save`).
    let rootfs_layers: Vec<Value> = match cfg["rootfs"]["diff_ids"].as_array() {
        Some(ids) => ids.clone(),
        None => i.layers.iter().map(|l| Value::String(l.digest.clone())).collect(),
    };
    // An untagged image (kept under its ID) has no names, as in Docker.
    let untagged = i.name.starts_with("sha256:");
    Ok(json!({
        "Id": i.config_digest,
        "RepoTags": if untagged { json!([]) } else { json!([i.name]) },
        "RepoDigests": if untagged { json!([]) } else { json!(i.repo_digest().into_iter().collect::<Vec<_>>()) },
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
            "Labels": image_labels_from(&cfg),
            "Healthcheck": cfg["config"]["Healthcheck"].clone(),
        },
        "RootFS": {"Type": cfg["rootfs"]["type"].as_str().unwrap_or("layers"), "Layers": rootfs_layers},
    }))
}

fn image_remove(engine: &Engine, name: &str, force: bool) -> Result<Value, Error> {
    let i = engine.image(name)?.ok_or_else(|| Error::NotFound(format!("No such image: {name}")))?;
    // With another tag on the same image, removing this one only untags it.
    let other_tags = engine.store.images()?.iter().any(|o| o.config_digest == i.config_digest && o.name != i.name);
    let in_use = if other_tags { Vec::new() } else { engine.each(|c| (c.spec.image_id == i.config_digest).then(|| c.short_id().to_owned())) };
    if let Some(c) = in_use.first() {
        return Err(Error::Conflict(format!(
            "conflict: unable to remove repository reference \"{name}\" (must force) - container {c} is using its referenced image"
        )));
    }
    // The last tag takes the image: no listed container uses it, but while
    // the list may be missing some, only a forced removal goes ahead.
    if !other_tags
        && !force
        && let Some(why) = engine.refusal(&format!("Image {name} was not removed, because the container list may be missing a container that uses it; `docker rmi --force` removes it anyway"))
    {
        return Err(Error::Conflict(why));
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
    let i = engine.image(source)?.ok_or_else(|| Error::NotFound(format!("No such image: {source}")))?;
    let repo = req.query("repo").unwrap_or_default();
    if repo.is_empty() {
        return Err(Error::BadRequest("repository name must have at least one component".into()));
    }
    let tag = req.query("tag").filter(|t| !t.is_empty()).unwrap_or("latest");
    let full = format!("{repo}:{tag}");
    let name = Reference::parse(&full).map(|r| r.short()).map_err(|e| Error::BadRequest(format!("invalid reference format: {e}")))?;
    engine.store.put_image(i.tagged(name.clone()))?;
    engine.emit_image("tag", &i.config_digest, &name);
    Ok(())
}

/// `docker load`: an image archive, plain or gzip- or zstd-compressed as Docker
/// accepts it, streamed into the store.
fn image_load(req: &Request, engine: &Engine, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<Next> {
    use std::io::Read;
    let mut body = http::Body::new(req, r);
    let mut head = Vec::with_capacity(6);
    (&mut body).take(6).read_to_end(&mut head)?;
    let loaded = upload_reader(head, &mut body).map(|archive| kiri_image::archive::load(&engine.store, archive));
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(e) => {
            // Refused from its first bytes: read the rest first, or the
            // client, still sending, gets a broken pipe and not the reason.
            let _ = io::copy(&mut body, &mut io::sink());
            error_closing(w, e)?;
            return Ok(Next::Close);
        }
    };
    match loaded {
        Ok(images) => {
            http::begin_stream_with(w, 200, "application/json", http::CLOSE)?;
            for i in &images {
                engine.emit_image("load", &i.config_digest, &i.name);
                let line = if i.name.starts_with("sha256:") { format!("Loaded image ID: {}\n", i.name) } else { format!("Loaded image: {}\n", i.name) };
                http::chunk(w, format!("{}\n", json!({"stream": line})).as_bytes())?;
            }
            http::end(w)?;
        }
        Err(e) => error_closing(w, Error::BadRequest(e.to_string()))?,
    }
    // A load may leave part of the body unread (padding after the archive's end).
    Ok(Next::Close)
}

/// `docker import`: a root filesystem tarball, the body (`fromSrc=-`) or a
/// URL, plain, gzip or zstd, becomes a one-layer image as Docker makes it.
fn image_import(req: &Request, engine: &Engine, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<Next> {
    let mut body = http::Body::new(req, r);
    match import(req, engine, &mut body) {
        Ok(id) => {
            http::begin_stream_with(w, 200, "application/json", http::CLOSE)?;
            http::chunk(w, format!("{}\n", json!({"status": id})).as_bytes())?;
            http::end(w)?;
        }
        Err(e) => {
            // The CLI is still sending the archive: read it out, or it gets
            // a broken pipe instead of the message.
            let _ = io::copy(&mut body, &mut io::sink());
            error_closing(w, e)?;
        }
    }
    Ok(Next::Close)
}

fn import(req: &Request, engine: &Engine, stdin: &mut dyn io::Read) -> Result<String, Error> {
    use std::io::Read;
    let name = import_name(req.query("repo").unwrap_or_default(), req.query("tag").unwrap_or_default())?;
    let platform = match req.query("platform").filter(|p| !p.is_empty()) {
        Some(p) => Platform::parse(p).ok_or_else(|| Error::BadRequest(format!("invalid platform: {p}")))?,
        None => Platform::host_linux(),
    };
    let src = req.query("fromSrc").unwrap_or_default();
    let now = time::rfc3339(std::time::SystemTime::now());
    let comment = req.query("message").filter(|m| !m.is_empty()).map_or_else(|| format!("Imported from {src}"), str::to_owned);
    let mut cfg = json!({
        "architecture": platform.architecture,
        "os": platform.os,
        "created": now,
        "config": {},
        "rootfs": {"type": "layers", "diff_ids": []},
        "history": [{"created": now, "comment": comment}],
    });
    // Checked before the body is read, so a bad change costs no upload.
    for change in req.queries("changes") {
        for line in change.lines().filter(|l| !l.trim().is_empty()) {
            super::ops::apply_change(&mut cfg, line)?;
        }
    }
    let mut body: Box<dyn Read + '_> = if src == "-" {
        Box::new(stdin)
    } else {
        // Docker takes `host/path` as http.
        let url = if src.contains("://") { src.to_owned() } else { format!("http://{src}") };
        let resp = crate::tools::agent().get(&url).call().map_err(|e| Error::BadRequest(format!("{url}: {e}")))?;
        Box::new(resp.into_body().into_reader())
    };
    let mut head = Vec::with_capacity(6);
    (&mut body).take(6).read_to_end(&mut head)?;
    let gzip = upload_compression(&head) == Upload::Gzip;
    let mut tar = upload_reader(head, body)?;
    let mut block = Vec::with_capacity(512);
    (&mut tar).take(512).read_to_end(&mut block).map_err(|e| upload_error(gzip, e))?;
    if !tar_header(&block) {
        return Err(Error::BadRequest("the archive isn't a tar archive (plain, gzip or zstd)".into()));
    }
    // An uncompressed layer: its digest is its diff id.
    let mut layer = engine.store.ingest()?;
    io::copy(&mut io::Cursor::new(block).chain(tar), &mut layer).map_err(|e| upload_error(gzip, e))?;
    let (digest, size) = layer.finish()?;
    cfg["rootfs"]["diff_ids"] = json!([digest]);
    let bytes = serde_json::to_vec(&cfg).map_err(|e| Error::Io(io::Error::other(e)))?;
    let mut w = engine.store.ingest()?;
    io::Write::write_all(&mut w, &bytes)?;
    let (config_digest, _) = w.finish()?;
    let layers = vec![kiri_image::Layer { digest, size, media_type: "application/vnd.oci.image.layer.v1.tar".into() }];
    let name = name.unwrap_or_else(|| config_digest.clone());
    engine.store.put_image(kiri_image::archive::from_config(name.clone(), &config_digest, &cfg, layers))?;
    engine.emit_image("import", &config_digest, &name);
    Ok(config_digest)
}

/// The name `docker import` gives its image. The CLI sends the tag in
/// `repo` (`repo=my/rootfs:1`, no `tag`); no `repo` is an untagged image.
fn import_name(repo: &str, tag: &str) -> Result<Option<String>, Error> {
    if repo.is_empty() {
        return Ok(None);
    }
    let name = if tag.is_empty() { repo.to_owned() } else { format!("{repo}:{tag}") };
    if name.contains('@') {
        return Err(Error::BadRequest(format!("cannot import digest reference: {name}")));
    }
    Reference::parse(&name).map(|r| Some(r.short())).map_err(|e| Error::BadRequest(format!("invalid reference format: {e}")))
}

/// Whether `block` begins a tar archive: a header whose checksum adds up,
/// or the zero block that ends an empty archive.
fn tar_header(block: &[u8]) -> bool {
    if block.len() < 512 {
        return false;
    }
    if block.iter().all(|&b| b == 0) {
        return true;
    }
    let want = std::str::from_utf8(&block[148..156]).ok().and_then(|s| u32::from_str_radix(s.trim_matches([' ', '\0']), 8).ok());
    // The checksum is taken with its own field as spaces.
    let sum: u32 = block[..512].iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u32::from(b) }).sum();
    want == Some(sum)
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
    // Held from here to the end of the stream; a blob already gone is an
    // error reply, not an archive cut short.
    let _held = match kiri_image::archive::hold(&engine.store, &images) {
        Ok(p) => p,
        Err(e) => return reply(w, Err(Error::Io(e)), 200),
    };
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
    line(w, match r {
        Ok(res) => json!({"result": res}),
        Err(e) => json!({"error": e.to_string()}),
    })?;
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
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let url = |k: &str| body[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(|v| if v.contains("://") { v.to_owned() } else { format!("http://{v}") });
    let no_proxy = body["no_proxy"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(str::to_owned).collect();
    super::proxy::set_host(super::proxy::Effective { http: url("http"), https: url("https"), no_proxy, pac: body["pac"].as_bool().unwrap_or(false) });
    // kiri-win's relay to Windows' loopback: {"7892": 41234}. A kiri-win
    // without one sends none, and proxies are dialled as set.
    if let Some(relay) = body["relay"].as_object() {
        super::proxy::set_relay(relay.iter().filter_map(|(k, v)| Some((k.parse().ok()?, u16::try_from(v.as_u64()?).ok()?))).collect());
    }
    Ok(super::proxy::report())
}

fn set_daemon(req: &Request) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
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
            if !s.trim().is_empty() && kiri_image::Mirror::parse(s).is_err() {
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
            if !s.trim().is_empty() && kiri_image::Insecure::parse(s).is_err() {
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
    if let Err(e) = crate::k8s::sync_registry_mirrors() {
        crate::diag::warn!("k8s registry mirrors not synced", error = e);
    }
    Ok(super::daemon_cfg::report())
}

fn set_proxy(req: &Request) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let mode = body["mode"].as_str().filter(|m| ["system", "custom", "none"].contains(m)).ok_or_else(|| Error::BadRequest("mode must be system, custom or none".into()))?;
    let field = |k: &str| body[k].as_str().unwrap_or("").trim().to_owned();
    for k in ["http", "https"] {
        let v = field(k);
        if mode == "custom" && !v.is_empty() && ureq::Proxy::new(&v).is_err() {
            return Err(Error::BadRequest(format!("{k} proxy: not a proxy address: {v}")));
        }
    }
    super::proxy::save(&json!({"mode": mode, "http": field("http"), "https": field("https"), "no_proxy": field("no_proxy"), "containers": body["containers"].as_bool().unwrap_or(false)}))
        .map_err(Error::Io)?;
    Ok(super::proxy::report())
}

fn set_registries(req: &Request, engine: &Engine) -> Result<Value, Error> {
    set_registries_with(req, super::daemon_json::set, || super::builder::refresh(engine))
}

fn set_registries_with(
    req: &Request,
    save: impl FnOnce(&Value) -> Result<(Value, bool), String>,
    refresh: impl FnOnce(),
) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let (report, changed) = save(&body).map_err(Error::BadRequest)?;
    // BuildKit reads its registries from buildkitd.toml at each start; an
    // unchanged save leaves the builder alone. A stopped builder gets the
    // new file now if nothing holds ENSURE; a running one is left to the
    // next build or the idle watch rather than stopped from a detached
    // thread after this save has answered.
    if changed {
        refresh();
    }
    Ok(report)
}

fn set_domain_ports(req: &Request, engine: &Arc<Engine>) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let port = |k: &str| body[k].as_u64().and_then(|p| u16::try_from(p).ok()).filter(|&p| p != 0).ok_or_else(|| Error::BadRequest(format!("{k} must be a port from 1 to 65535")));
    super::domains::set_ports(engine, port("http")?, port("https")?).map_err(Error::Conflict)
}

fn set_resources(req: &Request) -> Result<(), Error> {
    use super::engine as e;
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let max_mem = e::physical_memory_bytes() >> 20;
    let cpus = body["cpus"].as_u64().filter(|c| (1..=e::max_cpus() as u64).contains(c)).ok_or_else(|| Error::BadRequest(format!("cpus must be 1 to {}", e::max_cpus())))?;
    let mem = body["memory_mib"].as_u64().filter(|m| (1024..=max_mem).contains(m)).ok_or_else(|| Error::BadRequest(format!("memory_mib must be 1024 to {max_mem}")))?;
    let path = e::resources_path().ok_or_else(|| Error::Io(io::Error::other("HOME is not set")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json!({"cpus": cpus, "memory_mib": mem}).to_string()).and_then(|()| std::fs::rename(&tmp, &path)).map_err(Error::Io)
}

fn set_ports(req: &Request, engine: &Engine, id: &str) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let mut ports = Vec::new();
    for p in body["ports"].as_array().into_iter().flatten() {
        let container_port = p["container_port"].as_u64().filter(|v| (1..=65535).contains(v)).ok_or_else(|| Error::BadRequest("container_port must be 1 to 65535".into()))? as u16;
        let host_port = p["host_port"].as_u64().filter(|v| *v <= 65535).ok_or_else(|| Error::BadRequest("host_port must be 0 to 65535".into()))? as u16;
        let proto = p["proto"].as_str().unwrap_or("tcp");
        if proto != "tcp" && proto != "udp" {
            return Err(Error::BadRequest(format!("proto must be tcp or udp, not {proto}")));
        }
        ports.push(super::ports::Binding { container_port, proto: proto.into(), host_ip: p["host_ip"].as_str().unwrap_or("").into(), host_port });
    }
    engine.set_ports(id, ports)?;
    Ok(Value::Null)
}

/// Kiri's own: a container's whole environment, `NAME=value` each. A running
/// container restarts with it; its files are kept.
fn k8s_forward(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let ip: std::net::Ipv4Addr = body["ip"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::BadRequest("ip must be an IPv4 address".into()))?;
    let port = body["port"].as_u64().and_then(|p| u16::try_from(p).ok()).ok_or_else(|| Error::BadRequest("port must be a port number".into()))?;
    Ok(json!({"port": engine.k8s_forward(ip.octets(), port)?}))
}

fn set_env(req: &Request, engine: &Engine, id: &str) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let env = strings(&body["env"]).ok_or_else(|| Error::BadRequest("env must be a list of NAME=value".into()))?;
    if let Some(bad) = env.iter().find(|e| !e.split_once('=').is_some_and(|(k, _)| !k.is_empty() && !k.contains(char::is_whitespace))) {
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
    Client::new().with_credentials(req.header("x-registry-auth").and_then(kiri_image::Credentials::from_header))
}

/// `GET /distribution/{name}/json`: what the registry serves for an image
/// reference now (its digest and platforms), without pulling it. Update
/// checkers (Watchtower, Arcane) and `docker manifest inspect` ask this (#257).
fn distribution(req: &Request, name: &str) -> Result<Value, Error> {
    let reference = kiri_image::Reference::parse(name).map_err(|e| Error::BadRequest(format!("invalid reference format: {e}")))?;
    client_for(req).distribution(&reference).map_err(|e| match e {
        kiri_image::registry::Error::NotFound(m) => Error::NotFound(m),
        kiri_image::registry::Error::Status { status: 401 | 403, .. } => Error::Unauthorized(format!("{name}: authentication required")),
        e => Error::Io(io::Error::other(e.to_string())),
    })
}

/// `docker login`: the CLI asks the daemon to check the password, then
/// stores it itself.
fn auth(req: &Request) -> Result<Value, Error> {
    let creds: kiri_image::Credentials = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let server = if creds.server_address.is_empty() { "https://index.docker.io/v1/".to_owned() } else { creds.server_address.clone() };
    match Client::new().with_credentials(Some(creds)).login(&server) {
        Ok(()) => Ok(json!({"Status": "Login Succeeded", "IdentityToken": ""})),
        Err(kiri_image::registry::Error::Status { status: 401 | 403, body, .. }) => {
            Err(Error::Unauthorized(if body.is_empty() { "unauthorized: incorrect username or password".into() } else { body }))
        }
        Err(e) => Err(Error::BadRequest(format!("login to {server} failed: {e}"))),
    }
}

/// `docker push`: progress as a JSON stream, like a pull.
fn push(req: &Request, engine: &Engine, name: &str, w: &mut UnixStream) -> io::Result<Next> {
    let tag = req.query("tag").unwrap_or_default();
    let full = if tag.is_empty() { name.to_owned() } else { format!("{name}:{tag}") };
    let (image, target) = match (engine.image(&full), Reference::parse(&full)) {
        (Ok(Some(i)), Ok(r)) => (i, r),
        (Ok(None), _) => return reply(w, Err(Error::NotFound(format!("An image does not exist locally with the tag: {full}"))), 200),
        (Err(e), _) => return reply(w, Err(Error::Io(e)), 200),
        (_, Err(e)) => return reply(w, Err(Error::BadRequest(e.to_string())), 200),
    };
    http::begin_stream(w, 200, "application/json")?;
    let out = Mutex::new(w.try_clone()?);
    let line = |v: Value| {
        let mut s = v.to_string();
        s.push('\n');
        let _ = http::chunk(&mut *out.lock().unwrap_or_else(|e| e.into_inner()), s.as_bytes());
    };
    let short = |d: &str| d.strip_prefix("sha256:").unwrap_or(d).chars().take(12).collect::<String>();
    line(json!({"status": format!("The push refers to repository [{}]", if target.registry == "docker.io" { format!("docker.io/{}", target.repository) } else { format!("{}/{}", target.registry, target.repository) })}));
    let result = client_for(req).push(&engine.store, &image, &target, &|p| match p {
        kiri_image::PushProgress::Exists { digest } => line(json!({"status": "Layer already exists", "progressDetail": {}, "id": short(&digest)})),
        kiri_image::PushProgress::Uploading { digest, .. } => line(json!({"status": "Pushing", "progressDetail": {}, "id": short(&digest)})),
        kiri_image::PushProgress::Pushed { digest } => line(json!({"status": "Pushed", "progressDetail": {}, "id": short(&digest)})),
    });
    match result {
        Ok((digest, size)) => {
            if let Err(e) = engine.store.set_pushed(&image.name, &image.config_digest, &digest) {
                crate::diag::warn!("pushed digest not recorded", image = image.name, error = e);
            }
            line(json!({"status": format!("{}: digest: {digest} size: {size}", target.reference)}));
            line(json!({"progressDetail": {}, "aux": {"Tag": target.reference, "Digest": digest, "Size": size}}));
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
    let key = super::pulls::key(&reference.short(), req.query("platform").unwrap_or_default(), req.header("x-registry-auth").unwrap_or_default());
    let (follower, lead) = super::pulls::join(&key);
    if !lead {
        crate::diag::info!("image pull joined one under way", image = reference.short());
        return Ok(if follower.stream(w)? { Next::KeepAlive } else { Next::Close });
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
    let short = |d: &str| d.strip_prefix("sha256:").unwrap_or(d).chars().take(12).collect::<String>();
    line(json!({"status": format!("Pulling from {}", reference.repository), "id": reference.reference}));
    let wanted = req.query("platform").filter(|p| !p.is_empty()).and_then(Platform::parse).unwrap_or_else(Platform::host_linux);
    let client = client_for(req);
    let on_progress = |p: Progress| match p {
        Progress::Resolved { .. } | Progress::Layer { cached: true, .. } => {}
        // A mirror that did not have the image: say which, and carry on.
        Progress::MirrorFallback { mirror, reason } => {
            let reason: String = reason.chars().take(120).collect();
            line(json!({"status": format!("{mirror}: {reason}; trying the next registry")}));
        }
        Progress::FromMirror { mirror } => line(json!({"status": format!("Pulled from the mirror {mirror}")})),
        // Docker's own lines, so `docker pull` draws its bars; `size` is
        // extra, for the window's overall bar, and the CLI ignores it.
        Progress::Layer { digest, size, .. } => line(json!({"status": "Pulling fs layer", "progressDetail": {}, "id": short(&digest), "size": size})),
        Progress::Downloading { digest, got, size } => {
            line(json!({"status": "Downloading", "progressDetail": {"current": got, "total": size}, "id": short(&digest)}));
        }
        Progress::LayerDone { digest, cached, .. } => {
            let status = if cached { "Already exists" } else { "Pull complete" };
            line(json!({"status": status, "progressDetail": {}, "id": short(&digest)}));
        }
    };
    let began = std::time::Instant::now();
    crate::diag::info!("image pull started", image = reference.short(), platform = wanted);
    let mut result = client.pull_with(&engine.store, reference, &wanted, &on_progress, &job.cancel);
    // An image built only for amd64 runs through Rosetta, as on Docker Desktop.
    if let Err(kiri_image::registry::Error::NoPlatform { available, .. }) = &result {
        let amd64 = Platform { os: "linux".into(), architecture: "amd64".into() };
        if req.query("platform").is_none_or(str::is_empty) && wanted != amd64 && available.iter().any(|a| a == "linux/amd64") && amd64_runs() {
            line(json!({"status": format!("{} has no {wanted} image; using linux/amd64 through Rosetta", reference.short())}));
            result = client.pull_with(&engine.store, reference, &amd64, &on_progress, &job.cancel);
        }
    }
    match result {
        Ok(image) => {
            // Docker reports the digest it was asked for: the index's, if any.
            line(json!({"status": format!("Digest: {}", image.index_digest.as_ref().unwrap_or(&image.digest))}));
            line(json!({"status": format!("Status: Downloaded newer image for {}", image.name)}));
            engine.emit_image("pull", &image.config_digest, &image.name);
            crate::diag::info!("image pulled", image = reference.short(), ms = began.elapsed().as_millis());
        }
        // Nobody is left to tell.
        Err(kiri_image::registry::Error::Cancelled) => {
            crate::diag::info!("image pull stopped", image = reference.short(), ms = began.elapsed().as_millis());
        }
        Err(e) => {
            crate::diag::warn!("image pull failed", image = reference.short(), error = e);
            let mut message = e.to_string();
            if let kiri_image::registry::Error::NoPlatform { available, .. } = &e {
                if let Some(why) = intel_mac_refusal(&reference.short(), available) {
                    message = why;
                }
            }
            let proxy = super::proxy::shown();
            if let Some(why) = local_network_hint(&e, &reference.registry, proxy.is_some(), &crate::flavor::APP.to_string()) {
                message = format!("{message}. {why}");
            } else if let Some(why) = pull_hint(&e, &reference.registry, proxy.as_deref()) {
                message = format!("{why} ({message})");
            }
            line(json!({"errorDetail": {"message": message}, "error": message}))
        }
    }
}

/// macOS answers "No route to host" (EHOSTUNREACH, os error 65) to an app
/// that may not use the local network, though the Mac itself reaches it:
/// for a pull from a registry at a private or link-local address, the
/// sentence that says where that is allowed. None anywhere else, for
/// another error, a public registry, or a pull through a proxy.
fn local_network_hint(e: &kiri_image::registry::Error, registry: &str, proxied: bool, app: &str) -> Option<String> {
    let kiri_image::registry::Error::Http(m) = e else { return None };
    if !cfg!(target_os = "macos") || proxied || !m.contains("os error 65") {
        return None;
    }
    // Looked up only now, on this one error: the address the pull failed to reach.
    registry_addrs(registry).iter().any(|a| private_addr(*a)).then(|| {
        format!("{registry} is on the local network: {app} not allowed under Local Network in Privacy & Security (System Settings) usually causes this")
    })
}

/// The registry host's addresses: `host`, `host:port`, an IP or `[v6]:port`.
fn registry_addrs(registry: &str) -> Vec<std::net::IpAddr> {
    use std::net::ToSocketAddrs;
    if let Ok(ip) = registry.parse::<std::net::IpAddr>() {
        return vec![ip];
    }
    let with_port = if registry.parse::<std::net::SocketAddr>().is_ok() || registry.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        registry.to_owned()
    } else {
        format!("{registry}:443")
    };
    with_port.to_socket_addrs().map(|a| a.map(|s| s.ip()).collect()).unwrap_or_default()
}

/// RFC 1918 and IPv4 link-local, IPv6 unique-local (fc00::/7) and link-local (fe80::/10).
fn private_addr(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80 || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_private() || v4.is_link_local())
        }
    }
}

/// What a failed pull most likely means, in words that say what to fix: the
/// network, the proxy, a login or the registry's rate limit. None when the
/// error says it well enough itself.
fn pull_hint(e: &kiri_image::registry::Error, registry: &str, proxy: Option<&str>) -> Option<String> {
    use kiri_image::registry::Error;
    let name = if registry == "docker.io" || registry.ends_with(".docker.io") { "Docker Hub".to_owned() } else { registry.to_owned() };
    match e {
        Error::Http(m) => {
            let m = m.to_ascii_lowercase();
            let refused = m.contains("refused") || m.contains("reset") || m.contains("unexpected eof") || m.contains("broken pipe");
            let timeout = m.contains("timeout") || m.contains("timed out");
            let lookup = ["lookup", "resolve", "dns", "not known", "host not found", "no address"].iter().any(|w| m.contains(w));
            Some(match proxy {
                Some(p) if refused => format!("the proxy {p} refused the connection or closed it: is it running?"),
                Some(p) if timeout => format!("no answer from the proxy {p}"),
                Some(p) if lookup => format!("couldn't look up the proxy {p}"),
                Some(p) => format!("couldn't reach {name} through the proxy {p}"),
                None if lookup => format!("couldn't look up {name}: check the network or DNS, or set a proxy in Settings → Network"),
                None if refused || timeout || m.contains("unreachable") => format!("couldn't reach {name}: check the network, or set a proxy in Settings → Network"),
                None => return None,
            })
        }
        Error::Status { status: 429, .. } => Some(format!("{name} limits how many pulls it serves: docker login raises the limit, or try again later")),
        Error::Status { status: 401 | 403, .. } => Some(format!("{name} asked for a login: docker login{}", if name == "Docker Hub" { String::new() } else { format!(" {registry}") })),
        Error::Status { status: 407, .. } => Some(match proxy {
            Some(p) => format!("the proxy {p} asked for a login: put it in the proxy address, http://user:password@host:port"),
            None => "a proxy asked for a login".to_owned(),
        }),
        _ => None,
    }
}

/// `-p` (`HostConfig.PortBindings`) and `-P` (`PublishAllPorts`: every port
/// the image or `--expose` declares, on a free host port).
fn port_bindings(body: &Value, engine: &Engine, image: &kiri_image::Image) -> Result<Vec<super::ports::Binding>, Error> {
    let parse = |key: &str| -> Result<(u16, String), Error> {
        let (port, proto) = key.split_once('/').unwrap_or((key, "tcp"));
        let port = port.parse().map_err(|_| Error::BadRequest(format!("invalid port {key}")))?;
        if proto != "tcp" && proto != "udp" {
            return Err(Error::BadRequest(format!("Kiri forwards TCP and UDP ports, not {key}")));
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
                    p => p.parse().map_err(|_| Error::BadRequest(format!("invalid host port {p}")))?,
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
        let mut exposed: Vec<String> = body["ExposedPorts"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
        // The image's own EXPOSE lines live in its config blob.
        if let Ok(bytes) = engine.store.read_config(&image.config_digest) {
            if let Ok(cfg) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(m) = cfg["config"]["ExposedPorts"].as_object() {
                    exposed.extend(m.keys().cloned());
                }
            }
        }
        for key in exposed {
            let Ok((container_port, proto)) = parse(&key) else { continue };
            if !out.iter().any(|b| b.container_port == container_port && b.proto == proto) {
                out.push(super::ports::Binding { container_port, proto, host_ip: String::new(), host_port: 0 });
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
/// shares; /tmp and /var/folders are the Mac's links into /private, so they
/// resolve to a path under it). Any other path is the VM's own, as on Docker
/// Desktop and OrbStack: kind's `-v /lib/modules:/lib/modules:ro` must not
/// try to make /lib/modules on the Mac's read-only root.
///
/// Only the folders these resolve to are shared with the guest — see
/// `crate::guest::mac_share_dirs` — so this list cannot widen the share.
const MAC_SHARES: &[&str] = &["/Users", "/Volumes", "/private", "/tmp", "/var/folders"];

fn mac_shared(src: &str) -> bool {
    MAC_SHARES.iter().any(|s| src.strip_prefix(s).is_some_and(|rest| rest.is_empty() || rest.starts_with('/')))
}

/// A bind source that is the Mac's, as the path the Mac really keeps it at;
/// None for one that is the VM's own.
///
/// Links are followed here, on the Mac, as Docker follows a bind source's:
/// the guest sees the Mac under /mnt/mac, where a link to an absolute path
/// (/private/etc/localtime, a project folder linked from elsewhere) leads
/// nowhere. A path whose links leave the shares is left as given.
///
/// /etc is the Mac's link to /private/etc, and Compose files written for a
/// Linux host mount paths in it:
/// - /etc/passwd, /etc/os-release and the others in `hostetc::FILES` are
///   expected to be Linux's, so they are the ones Kiri keeps; and /etc
///   itself is the folder of them.
/// - anything else the Mac has there (/etc/hosts, /etc/localtime) is the
///   Mac's.
/// - what the Mac lacks stays the VM's.
fn mac_source(src: &str) -> Option<String> {
    let real = std::fs::canonicalize(src).ok().and_then(|p| p.into_os_string().into_string().ok()).filter(|p| mac_shared(p));
    if mac_shared(src) {
        return Some(real.unwrap_or_else(|| src.to_owned()));
    }
    let kept = match src.strip_prefix("/etc/") {
        Some(name) if super::hostetc::FILES.contains(&name) => Some(super::hostetc::file(name)),
        None if src == "/etc" => Some(super::hostetc::all()),
        _ => None,
    };
    if let Some(kept) = kept {
        // Kiri's data folder is in the person's home, which is shared. If
        // it cannot be written, the path is the VM's, as it was.
        return kept.ok().and_then(|p| p.into_os_string().into_string().ok()).filter(|p| mac_shared(p));
    }
    if src.strip_prefix("/etc").is_some_and(|rest| rest.is_empty() || rest.starts_with('/')) { real } else { None }
}

/// Where the guest finds a host path given for a bind: the Mac's own under
/// the shares, else the VM's (on Linux, the host's).
fn guest_bind_source(src: &str) -> String {
    if !cfg!(target_os = "macos") {
        return src.to_owned();
    }
    mac_source(src).map_or_else(|| src.to_owned(), |real| format!("{MAC_ROOT}{real}"))
}

/// The guest mount lines with each Mac bind source followed again from the
/// path the person gave (kept in `info`, one entry per line). Links are
/// resolved when the container is made; this redoes it at start, so a link
/// re-pointed since then leads to its new target. On a Linux host (and
/// WSL) only a bind of the engine's socket is looked at again, so a
/// container made before Kiri gave containers a socket of their own gets
/// it now: by the path the person gave, or else by the path mounted when
/// it was made, every line, whether or not `info` has one for it. Other
/// lines stay as they are.
pub(super) fn fresh_bind_sources(guest: &[String], info: &[Value]) -> Vec<String> {
    fresh_bind_sources_with(guest, info, super::binds::engine_socket, cfg!(target_os = "macos"))
}

fn fresh_bind_sources_with(guest: &[String], info: &[Value], engine_socket: impl Fn(&str) -> Option<String>, mac: bool) -> Vec<String> {
    let paired = guest.len() == info.len();
    if mac && !paired {
        return guest.to_vec();
    }
    guest
        .iter()
        .enumerate()
        .map(|(idx, line)| {
            let Some((old, rest)) = line.split_once('\t') else { return line.clone() };
            let src = info.get(idx).filter(|i| paired && i["Type"] == "bind").and_then(|i| i["Source"].as_str());
            if !mac {
                let given = src.and_then(&engine_socket).or_else(|| old.starts_with('/').then(|| engine_socket(old)).flatten());
                return given.map_or_else(|| line.clone(), |given| format!("{given}\t{rest}"));
            }
            let Some(src) = src else { return line.clone() };
            if !old.strip_prefix(MAC_ROOT).is_some_and(|p| p.starts_with('/')) || engine_socket(src).is_some() {
                return line.clone();
            }
            format!("{}\t{rest}", guest_bind_source(src))
        })
        .collect()
}

/// An NFS or CIFS server name as an address the guest kernel can use.
/// `host.docker.internal` is the Mac, as it is for containers.
fn resolve_server(host: &str) -> Option<String> {
    use std::net::ToSocketAddrs;
    if cfg!(target_os = "macos") && matches!(host, "host.docker.internal" | "host.kiri.internal") {
        return Some(super::ports::HOST_ADDR.to_owned());
    }
    let addrs: Vec<std::net::SocketAddr> = (host, 0).to_socket_addrs().ok()?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).map(|a| a.ip().to_string())
}

/// Whether `src` names the mount point the guest keeps the Mac's folders at.
///
/// `root` is empty on a Linux host, where the agent sees the host's own
/// paths and there is no share to name.
fn guest_share_path(src: &str, root: &str) -> bool {
    // Compared as the guest kernel will read it: `/mnt//mac`, `/mnt/./mac`
    // and `/mnt/x/../mac` all name the share.
    let src = lexical_path(src);
    root.starts_with('/') && (src == root || src.strip_prefix(root).is_some_and(|rest| rest.starts_with('/')))
}

/// An absolute path with empty and `.` components dropped and `..` applied.
fn lexical_path(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    format!("/{}", out.join("/"))
}

/// Refuses volume driver options that would mount the VM's own shares.
///
/// The guest mounts a volume's `device` itself, as root, before the container
/// starts, so a `type=virtiofs` volume (`device=kiri-mac`) or a bind of
/// `/mnt/mac` would hand the container the share without `-v` naming it.
fn check_volume_device(options: &std::collections::BTreeMap<String, String>) -> Result<(), Error> {
    let kind = options.get("type").map_or("", String::as_str);
    if kind.eq_ignore_ascii_case("virtiofs") {
        return Err(Error::BadRequest("invalid volume option type=virtiofs: the VM's shares cannot be mounted as a volume".into()));
    }
    let device = options.get("device").map_or("", String::as_str);
    let (flags, _) = super::volumes::mount_options(options.get("o").map_or("", String::as_str));
    if flags & 4096 != 0 && guest_share_path(device, MAC_ROOT) {
        return Err(Error::BadRequest(format!(
            "invalid volume option device={device}: {MAC_ROOT} exists only inside the VM; name the Mac's own path, such as /Users/…"
        )));
    }
    Ok(())
}

/// Refuses text that would forge a field in the tab-separated mount line the
/// guest parses.
///
/// Each mount reaches the guest as `source\ttarget\tflags`, and a volume with
/// driver options adds `\tkind\tdevice\tflags\tdata`. Targets come from the
/// image's `VOLUME` lines and volume options from its driver config, so a tab
/// (or a newline, or NUL) in one is an untrusted field separator: the fields
/// after it would be read as a mount type, device and flags instead.
fn wire_text(what: &str, s: &str) -> Result<(), Error> {
    let Some(b) = s.bytes().find(|b| matches!(b, b'\t' | b'\n' | b'\r' | 0)) else { return Ok(()) };
    let named = match b {
        b'\t' => "a tab",
        b'\n' => "a newline",
        b'\r' => "a carriage return",
        _ => "NUL",
    };
    Err(Error::BadRequest(format!("invalid {what} {s:?}: {named} is not allowed")))
}

/// A hostname as the guest writes it into `/etc/hostname` and `/etc/hosts`.
/// Docker takes nearly anything here (`my_app`, `-db`, `a..b`), and Compose
/// files rely on it, so only what would add a line or a field of its own is
/// refused: a newline, carriage return, tab, NUL or other control character.
fn valid_hostname(name: &str) -> bool {
    !name.is_empty() && !name.bytes().any(|b| b.is_ascii_control())
}

/// A `--add-host` name, one field of a hosts line. Docker takes any name
/// here too (`my_app.local`, even `a b`), so, as for a hostname, only a
/// control character, which would add a line or a field of its own, is refused.
fn valid_hosts_name(name: &str) -> bool {
    !name.is_empty() && !name.bytes().any(|b| b.is_ascii_control())
}

/// One `--volumes-from` entry, `container[:mode]`, as the container and
/// whether the mounts are made read-only. Docker's modes: `ro` or `rw`, with
/// `z`, `Z` or `nocopy` beside them.
fn parse_volumes_from(spec: &str) -> Result<(&str, bool), Error> {
    if spec.is_empty() {
        return Err(Error::BadRequest("volumes-from specification cannot be an empty string".into()));
    }
    let (name, mode) = spec.split_once(':').unwrap_or((spec, ""));
    let parts: Vec<&str> = mode.split(',').filter(|m| !m.is_empty()).collect();
    let rw_modes = parts.iter().filter(|m| matches!(**m, "ro" | "rw")).count();
    if name.is_empty() || rw_modes > 1 || parts.iter().any(|m| !matches!(*m, "ro" | "rw" | "z" | "Z" | "nocopy")) || (!mode.is_empty() && parts.is_empty()) {
        return Err(Error::BadRequest(format!("invalid mode: {mode}")));
    }
    Ok((name, parts.contains(&"ro")))
}

/// The mounts `--volumes-from` takes from a container: its guest lines and
/// inspect entries (one per line), without tmpfs mounts, which Docker does
/// not share either. With `ro` each becomes read-only; otherwise each keeps
/// its own mode, so `:rw` does not make a read-only mount writable.
fn volumes_from_mounts(guest: &[String], info: &[Value], ro: bool) -> Vec<(String, Value)> {
    guest
        .iter()
        .zip(info)
        .filter(|(_, i)| i["Type"] != "tmpfs")
        .map(|(line, i)| {
            let mut i = i.clone();
            if !ro {
                return (line.clone(), i);
            }
            // `source\ttarget\tflags[\t…]`: "ro" joins the flags.
            let mut fields: Vec<String> = line.split('\t').map(str::to_owned).collect();
            if fields.len() < 3 {
                fields.resize(3, String::new());
            }
            if !fields[2].split(',').any(|f| f == "ro") {
                fields[2] = if fields[2].is_empty() { "ro".to_owned() } else { format!("{},ro", fields[2]) };
            }
            i["RW"] = json!(false);
            (fields.join("\t"), i)
        })
        .collect()
}

/// `-v`, `--mount`, `--tmpfs`, `--volumes-from`, `-v /path` and the image's
/// VOLUME lines.
fn mounts(body: &Value, engine: &Engine, image: &kiri_image::Image) -> Result<Mounts, Error> {
    let host = &body["HostConfig"];
    let mut out = Mounts { guest: Vec::new(), info: Vec::new(), volumes: Vec::new(), anon: Vec::new() };
    let check_target = |t: &str| -> Result<(), Error> {
        if !t.starts_with('/') {
            return Err(Error::BadRequest(format!("invalid mount target {t}: must be an absolute path")));
        }
        wire_text("mount target", t)?;
        Ok(())
    };
    // One entry: kind is "bind" | "volume" | "tmpfs"; for binds `create_missing`
    // says whether a missing source directory is made (-v) or an error (--mount).
    let add = |kind: &str, src: &str, dst: &str, ro: bool, extra: &str, create_missing: bool, out: &mut Mounts| -> Result<(), Error> {
        check_target(dst)?;
        // A source and a target are joined with tabs for the guest; so are
        // tmpfs options. Reject a separator or NUL inside any of them.
        wire_text("mount source", src)?;
        wire_text("tmpfs option", extra)?;
        // `/mnt/mac` is the guest's view of the Mac's folders, not a path on
        // the Mac: a bind of it would hand the container the share itself.
        if guest_share_path(src, MAC_ROOT) {
            return Err(Error::BadRequest(format!(
                "invalid mount source {src}: {MAC_ROOT} exists only inside the VM; name the Mac's own path, such as /Users/…"
            )));
        }
        let flag = |more: &str| {
            let mut f: Vec<&str> = more.split(',').filter(|x| !x.is_empty()).collect();
            if ro {
                f.push("ro");
            }
            f.join(",")
        };
        // The engine's own socket (Testcontainers' Ryuk, CI runners): the VM's
        // relay to this daemon on a Mac, or Kiri's socket on a Linux host.
        let engine_sock = if kind == "bind" { super::binds::engine_socket(src) } else { None };
        match kind {
            "bind" if engine_sock.is_some() => {
                out.guest.push(format!("{}\t{dst}\t{}", engine_sock.as_deref().unwrap_or_default(), flag("")));
                out.info.push(json!({"Type": "bind", "Source": src, "Destination": dst, "Mode": "", "RW": !ro, "Propagation": "rprivate"}));
            }
            "bind" if src == SSH_AGENT_SOCK => {
                out.guest.push(format!("{src}\t{dst}\t{}", flag("")));
                out.info.push(json!({"Type": "bind", "Source": src, "Destination": dst, "Mode": "", "RW": !ro, "Propagation": "rprivate"}));
            }
            "bind" => {
                if !src.starts_with('/') {
                    return Err(Error::BadRequest(format!("invalid mount source {src}: must be an absolute path")));
                }
                // Where the source is on this machine: itself, or on a Mac
                // the path it leads to (a file Kiri keeps for /etc/passwd).
                let here = if cfg!(target_os = "macos") { mac_source(src) } else { Some(src.to_owned()) };
                let Some(here) = here else {
                    // The guest makes a missing source itself, as Docker does on Linux.
                    out.guest.push(format!("{src}\t{dst}\t{}", flag(if create_missing { "create" } else { "" })));
                    out.info.push(json!({"Type": "bind", "Source": src, "Destination": dst, "Mode": "", "RW": !ro, "Propagation": "rprivate"}));
                    return Ok(());
                };
                let p = std::path::Path::new(&here);
                if !p.exists() {
                    if !create_missing {
                        return Err(Error::BadRequest(format!("invalid mount config for type \"bind\": bind source path does not exist: {src}")));
                    }
                    super::binds::make_source(p).map_err(Error::Io)?;
                }
                // After a missing source was made, so its links are followed too.
                out.guest.push(format!("{}\t{dst}\t{}", guest_bind_source(src), flag("")));
                out.info.push(json!({"Type": "bind", "Source": src, "Destination": dst, "Mode": "", "RW": !ro, "Propagation": "rprivate"}));
            }
            "volume" => {
                let (name, anonymous) = if src.is_empty() { (super::engine::random_hex(32), true) } else { (src.to_owned(), false) };
                let v = engine.volume_ensure(&name, json!({}), anonymous)?;
                // `type`, `device` and `o` reach the guest in the same
                // tab-separated line, so neither may carry a separator.
                for (k, val) in &v.options {
                    wire_text(&format!("volume option {k}"), val)?;
                }
                // The guest mounts a volume's device as root, so the share
                // must not be reachable this way either: not as a virtiofs
                // tag, and not as a bind of /mnt/mac.
                check_volume_device(&v.options)?;
                let path = super::volumes::guest_path(&name);
                let copy = if extra.contains("nocopy") { "create" } else { "create,copyup" };
                // A volume with driver options (`type=nfs`, `o=bind` …) is
                // mounted at its path by the guest at start, then bound in.
                let vol = super::volumes::guest_mount_fields(&v.options, guest_bind_source, resolve_server);
                out.guest.push(format!("{path}\t{dst}\t{}{vol}", flag(copy)));
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
                out.guest.push(format!("tmpfs\t{dst}\t{}", flag(&format!("tmpfs,noexec,nosuid,nodev,{extra}"))));
                out.info.push(json!({"Type": "tmpfs", "Source": "", "Destination": dst, "Mode": "", "RW": !ro, "Propagation": ""}));
            }
            other => return Err(Error::BadRequest(format!("Kiri does not support {other} mounts"))),
        }
        Ok(())
    };
    // From the docker CLI on Windows (into WSL), `C:\src:/app` and the like.
    let wsl = super::binds::wsl();
    for b in strings(&host["Binds"]).unwrap_or_default() {
        let (src, dst, opts) = super::binds::split(&b, wsl)?;
        let (src, dst, opts) = (src.as_str(), dst.as_str(), opts.as_str());
        let ro = opts.split(',').any(|o| o == "ro");
        let kind = if src.starts_with('/') { "bind" } else { "volume" };
        let extra = if opts.split(',').any(|o| o == "nocopy") { "nocopy" } else { "" };
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
                for kv in m["TmpfsOptions"]["Options"].as_array().into_iter().flatten() {
                    let kv: Vec<&str> = kv.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
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
        if kind == "volume" && !src.is_empty() && engine.volume_get(src).is_none() && !driver_opts.is_null() {
            match driver_opts["Name"].as_str().unwrap_or("") {
                "" | "local" => {}
                d => return Err(Error::BadRequest(format!("Kiri supports only local volumes, not {d}"))),
            }
            let options = super::volumes::local_options(&driver_opts["Options"]).map_err(Error::BadRequest)?;
            engine.volume_ensure_with(src, m["VolumeOptions"]["Labels"].clone(), options)?;
        }
        add(kind, src, dst, ro, &extra, false, &mut out)?;
    }
    if let Some(map) = host["Tmpfs"].as_object() {
        for (dst, opts) in map {
            // `--tmpfs /x:noexec,size=64m`: flags and options alike.
            let opts = opts.as_str().unwrap_or("").split(',').filter(|o| !o.is_empty() && *o != "tmpfs").collect::<Vec<_>>().join(",");
            add("tmpfs", "", dst, false, &opts, false, &mut out)?;
        }
    }
    // `--volumes-from other[:ro]`: every volume and bind of the other
    // container (running or not) at the same path. A mount given here at
    // the same path wins, as in Docker.
    for spec in strings(&host["VolumesFrom"]).unwrap_or_default() {
        let (name, ro) = parse_volumes_from(&spec)?;
        let id = engine.resolve(name).map_err(|e| missing_container(e, ""))?;
        let (guest, info) = engine.with(&id, |c| (c.spec.mounts.clone(), c.spec.mount_info.clone()))?;
        for (line, i) in volumes_from_mounts(&guest, &info, ro) {
            let dst = i["Destination"].as_str().unwrap_or_default().trim_end_matches('/').to_owned();
            if out.info.iter().any(|m| m["Destination"].as_str().map(|d| d.trim_end_matches('/')) == Some(dst.as_str())) {
                continue;
            }
            // The volume is the other container's too: kept, never taken
            // with this one on `--rm`.
            if let Some(v) = i["Name"].as_str().filter(|_| i["Type"] == "volume")
                && !out.volumes.iter().any(|x| x == v)
            {
                out.volumes.push(v.to_owned());
            }
            out.guest.push(line);
            out.info.push(i);
        }
    }
    // `-v /path` (no source) arrives in `Volumes`; the image's VOLUME lines
    // are in its config. Each becomes an anonymous volume, unless something
    // else is already mounted there.
    let mut anonymous: Vec<String> = body["Volumes"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
    if let Ok(bytes) = engine.store.read_config(&image.config_digest) {
        if let Ok(cfg) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(m) = cfg["config"]["Volumes"].as_object() {
                anonymous.extend(m.keys().cloned());
            }
        }
    }
    for dst in anonymous {
        let taken = out.info.iter().any(|i| i["Destination"].as_str().map(|d| d.trim_end_matches('/')) == Some(dst.trim_end_matches('/')));
        if !taken {
            add("volume", "", &dst, false, "", false, &mut out)?;
        }
    }
    Ok(out)
}

fn volume_json(v: &super::volumes::Volume) -> Value {
    json!({
        "Name": v.name,
        "Driver": "local",
        "Mountpoint": super::volumes::guest_path(&v.name),
        "CreatedAt": time::rfc3339(std::time::UNIX_EPOCH + Duration::from_nanos(v.created_ns)),
        "Labels": if v.labels.is_object() { v.labels.clone() } else { json!({}) },
        "Scope": "local",
        "Options": v.options,
    })
}

fn volume_create(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    match body["Driver"].as_str().unwrap_or("local") {
        "" | "local" => {}
        d => return Err(Error::BadRequest(format!("Kiri supports only local volumes, not {d}"))),
    }
    let name = match body["Name"].as_str().unwrap_or("") {
        "" => super::engine::random_hex(32),
        n => n.to_owned(),
    };
    let options = super::volumes::local_options(&body["DriverOpts"]).map_err(Error::BadRequest)?;
    let v = engine.volume_ensure_with(&name, body["Labels"].clone(), options)?;
    Ok(volume_json(&v))
}

/// Docker's default prune: anonymous volumes no container uses.
/// `all=true` takes named ones too.
fn volume_prune(req: &Request, engine: &Engine) -> Result<Value, Error> {
    // While the container list may be missing containers, "unused" can't
    // be told; say so rather than quietly pruning less.
    if let Some(why) = engine.refusal("Volumes were not pruned, because the container list may be missing containers that use them") {
        return Err(Error::Conflict(why));
    }
    let f = filters(req, &["label", "label!", "all"])?;
    // `all` as Docker's withPrune reads it (strconv.ParseBool, one value).
    let all = match f.values("all") {
        None => false,
        Some([v]) => match v.as_str() {
            "1" | "t" | "T" | "TRUE" | "true" | "True" => true,
            "0" | "f" | "F" | "FALSE" | "false" | "False" => false,
            _ => return Err(Error::BadRequest(format!("invalid filter 'all': strconv.ParseBool: parsing {v:?}: invalid syntax"))),
        },
        Some(v) => return Err(Error::BadRequest(format!("invalid filter 'all=[{}]': only one value is expected", v.join(" ")))),
    };
    // Docker never prunes a volume made with driver options (a bind or
    // tmpfs `--opt`): its prune takes only those with none.
    let unused: Vec<String> = engine
        .volume_list()
        .into_iter()
        .filter(|v| (all || v.anonymous) && v.options.is_empty() && f.labels(&v.labels) && engine.volume_users(&v.name).is_empty())
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
                && f.bool("dangling").is_none_or(|d| d == engine.volume_users(&v.name).is_empty())
        })
        .map(volume_json)
        .collect();
    Ok(json!({"Volumes": list, "Warnings": []}))
}

/// The TCP ports an image declares (EXPOSE) plus `--expose`.
fn exposed_ports(body: &Value, engine: &Engine, image: &kiri_image::Image) -> Vec<u16> {
    let mut keys: Vec<String> = body["ExposedPorts"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
    if let Ok(bytes) = engine.store.read_config(&image.config_digest) {
        if let Ok(cfg) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(m) = cfg["config"]["ExposedPorts"].as_object() {
                keys.extend(m.keys().cloned());
            }
        }
    }
    let mut ports: Vec<u16> = keys.iter().filter(|k| k.ends_with("/tcp") || !k.contains('/')).filter_map(|k| k.split('/').next()?.parse().ok()).collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// What is published right now: Docker's `Ports` list for `ps`.
fn ports_list(c: &Container) -> Value {
    let Some(fw) = &c.forwards else { return json!([]) };
    Value::Array(
        fw.bound
            .iter()
            .map(|(b, ip, port)| json!({"IP": ip, "PrivatePort": b.container_port, "PublicPort": port, "Type": b.proto}))
            .collect(),
    )
}

/// What `-p` asked for, as Docker's inspect keeps it in HostConfig: the
/// container port and protocol, each with the host address and port given
/// (an empty HostPort for "any free port"). Empty, not null, with none.
fn inspect_bindings(ports: &[super::ports::Binding]) -> Value {
    let mut m = serde_json::Map::new();
    for b in ports {
        let entry = m.entry(format!("{}/{}", b.container_port, b.proto)).or_insert_with(|| json!([]));
        if let Some(a) = entry.as_array_mut() {
            a.push(json!({"HostIp": b.host_ip, "HostPort": if b.host_port == 0 { String::new() } else { b.host_port.to_string() }}));
        }
    }
    Value::Object(m)
}

/// Config.ExposedPorts: the image's and `--expose`'s ports, and every
/// published one, as Docker lists them (`"80/tcp": {}`); null with none,
/// as Docker leaves it out.
fn inspect_exposed(ports: &[super::ports::Binding], exposed: &[u16]) -> Value {
    let mut m = serde_json::Map::new();
    for b in ports {
        m.insert(format!("{}/{}", b.container_port, b.proto), json!({}));
    }
    for p in exposed {
        m.entry(format!("{p}/tcp")).or_insert_with(|| json!({}));
    }
    if m.is_empty() { Value::Null } else { Value::Object(m) }
}

/// Docker's `NetworkSettings.Ports` map for inspect and `docker port`.
fn ports_map(c: &Container) -> Value {
    let bound: &[(super::ports::Binding, String, u16)] = c.forwards.as_ref().map_or(&[], |fw| &fw.bound);
    // Docker lists the exposed ports only while the container has its own
    // network endpoint: running, and not on host, none or another's network.
    let mode = c.spec.network_mode.as_str();
    let endpoint = c.status == Status::Running && mode != "host" && mode != "none" && !mode.starts_with("container:");
    ports_map_of(bound, if endpoint { Some((&c.spec.ports, &c.spec.exposed)) } else { None })
}

/// The published ports with their host addresses, then (when `exposed` is
/// given) every exposed or `-p` port that has none as a key with a null value
/// (`"80/tcp": null`), as Docker shows them. Traefik's docker provider picks
/// a container's port from these keys alone.
fn ports_map_of(bound: &[(super::ports::Binding, String, u16)], exposed: Option<(&[super::ports::Binding], &[u16])>) -> Value {
    let mut m = serde_json::Map::new();
    for (b, ip, port) in bound {
        let key = format!("{}/{}", b.container_port, b.proto);
        let entry = m.entry(key).or_insert_with(|| json!([]));
        if let Some(a) = entry.as_array_mut() {
            a.push(json!({"HostIp": ip, "HostPort": port.to_string()}));
        }
    }
    if let Some((asked, exposed)) = exposed {
        for b in asked {
            m.entry(format!("{}/{}", b.container_port, b.proto)).or_insert(Value::Null);
        }
        for p in exposed {
            m.entry(format!("{p}/tcp")).or_insert(Value::Null);
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

/// A create's Entrypoint and Cmd with the image's, by Docker's rules: a
/// non-empty Entrypoint drops the image's Cmd; otherwise an empty or missing
/// Cmd takes the image's (Testcontainers sends `"Cmd": []`), and a missing
/// Entrypoint the image's (`[]` clears it). `--entrypoint=` and
/// `--entrypoint ""` reach here as `[""]`: Docker counts that as given, so the
/// image's Cmd is dropped, then clears it, so only the request's Cmd runs, and
/// with none there is nothing to run. Kept as `[""]`, the empty name was looked
/// up on PATH and came back as the first PATH directory.
fn command(entry: Option<Vec<String>>, cmd: Option<Vec<String>>, image_entry: &[String], image_cmd: &[String]) -> Result<(Vec<String>, Vec<String>), Error> {
    let cmd = cmd.unwrap_or_default();
    Ok(match entry {
        Some(e) if e.len() == 1 && e[0].is_empty() => {
            if cmd.is_empty() {
                return Err(Error::BadRequest("no command specified".into()));
            }
            (vec![], cmd)
        }
        Some(e) if !e.is_empty() => (e, cmd),
        e => (e.unwrap_or_else(|| image_entry.to_vec()), if cmd.is_empty() { image_cmd.to_vec() } else { cmd }),
    })
}

/// The primary network when NetworkMode is empty or `default`: the first
/// resolved endpoint's name, unless bridge is listed. No match keeps bridge.
/// Unknown endpoints are left for create to reject. `keys` come from the
/// request's JSON map, so they are sorted and "first" is the same every time.
/// `resolve` gives a key's network name and whether it is internal. Only the
/// primary network gets the default route, so a network that reaches outside
/// is preferred, as Docker picks its gateway: an internal one is primary only
/// when every listed network is internal.
fn endpoints_primary<'a>(keys: impl Iterator<Item = &'a str>, resolve: impl Fn(&str) -> Option<(String, bool)>) -> Option<String> {
    let (mut outside, mut internal) = (None, None);
    for (name, is_internal) in keys.filter(|key| !key.starts_with("container:")).filter_map(resolve) {
        if name == "bridge" {
            return None;
        }
        let slot = if is_internal { &mut internal } else { &mut outside };
        if slot.is_none() {
            *slot = Some(name);
        }
    }
    outside.or(internal)
}

fn strings(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(a) => Some(a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()),
        Value::String(s) => Some(vec![s.clone()]),
        _ => None,
    }
}

/// cgroup limits from a HostConfig (create) or an update body, on top of `base`.
/// Capability names as Docker writes them (`NET_ADMIN`, `CAP_NET_ADMIN`, `ALL`).
pub(super) const CAPS: [&str; 41] = [
    "CHOWN", "DAC_OVERRIDE", "DAC_READ_SEARCH", "FOWNER", "FSETID", "KILL", "SETGID", "SETUID", "SETPCAP", "LINUX_IMMUTABLE",
    "NET_BIND_SERVICE", "NET_BROADCAST", "NET_ADMIN", "NET_RAW", "IPC_LOCK", "IPC_OWNER", "SYS_MODULE", "SYS_RAWIO", "SYS_CHROOT",
    "SYS_PTRACE", "SYS_PACCT", "SYS_ADMIN", "SYS_BOOT", "SYS_NICE", "SYS_RESOURCE", "SYS_TIME", "SYS_TTY_CONFIG", "MKNOD", "LEASE",
    "AUDIT_WRITE", "AUDIT_CONTROL", "SETFCAP", "MAC_OVERRIDE", "MAC_ADMIN", "SYSLOG", "WAKE_ALARM", "BLOCK_SUSPEND", "AUDIT_READ",
    "PERFMON", "BPF", "CHECKPOINT_RESTORE",
];

/// Docker's default set: what a container keeps without `--cap-add`.
const DEFAULT_CAPS: [&str; 14] =
    ["CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "KILL", "SETGID", "SETUID", "SETPCAP", "NET_BIND_SERVICE", "NET_RAW", "SYS_CHROOT", "MKNOD", "AUDIT_WRITE", "SETFCAP"];

/// The capabilities a container keeps, one bit each: `caps` as stored (0 is
/// Docker's default set).
pub(super) fn effective_caps(caps: u64) -> u64 {
    if caps != 0 {
        return caps;
    }
    DEFAULT_CAPS.iter().filter_map(|d| CAPS.iter().position(|c| c == d)).fold(0, |m, i| m | 1u64 << i)
}

/// The capabilities a profile's `includes`/`excludes` see: all of them for a
/// privileged container, as in moby.
pub(super) fn profile_caps(caps: u64, privileged: bool) -> u64 {
    if privileged { (1u64 << CAPS.len()) - 1 } else { effective_caps(caps) }
}

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
        CAPS.iter().position(|c| *c == n).map(|i| 1u64 << i).ok_or_else(|| Error::BadRequest(format!("unknown capability: {name:?}")))
    };
    let mut mask = DEFAULT_CAPS.iter().map(|c| bit(c)).sum::<Result<u64, Error>>()?;
    for d in &drop {
        mask &= !bit(d)?;
    }
    for a in &add {
        mask |= bit(a)?;
    }
    // Dropping everything still has to be told apart from "default".
    Ok(if mask == 0 { 1u64 << 63 } else { mask })
}

/// Whether this machine runs amd64 programs: Rosetta on a Mac; on a Linux
/// host (and Windows' engine in WSL 2) an emulator the kernel has been told
/// to use for them. Then a repository with no image for this machine gives
/// its amd64 one, as on the Mac.
fn amd64_runs() -> bool {
    if cfg!(target_os = "linux") {
        let Ok(entries) = std::fs::read_dir("/proc/sys/fs/binfmt_misc") else { return false };
        return entries.flatten().filter_map(|e| std::fs::read_to_string(e.path()).ok()).any(|rule| binfmt_runs_amd64(&rule));
    }
    kiri_vmm::rosetta_installed()
}

/// One binfmt_misc rule, as the kernel prints it: enabled, and its magic is
/// an x86-64 ELF header (class 64, little-endian, machine 0x3e).
fn binfmt_runs_amd64(rule: &str) -> bool {
    rule.lines().next() == Some("enabled") && rule.lines().any(|l| l.strip_prefix("magic ").is_some_and(|m| m.starts_with("7f454c460201") && m.get(36..40) == Some("3e00")))
}

/// cgroup v1 CPU shares (2–262144) as a v2 weight (1–10000), on the curve
/// runc and crun use since 2025: 1024, Docker's default, is 100, the
/// kernel's default weight. The older straight line made it 39, less than
/// a container that asked for nothing.
pub(super) fn cpu_weight(shares: u64) -> u32 {
    if shares <= 2 {
        return 1;
    }
    if shares >= 262_144 {
        return 10_000;
    }
    let l = (shares as f64).log2();
    // Rounded to 9 places first: the exact powers of ten (1024 → 100) must
    // not be pushed over by the last bit of the float.
    let w = 10f64.powf((l * l + 125.0 * l) / 612.0 - 7.0 / 34.0);
    ((w * 1e9).round() / 1e9).ceil() as u32
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
        l.cpu_period = h["CpuPeriod"].as_u64().filter(|p| *p > 0).unwrap_or(100_000) as u32;
    }
    if let Some(shares) = h["CpuShares"].as_u64().filter(|s| *s > 0) {
        l.cpu_shares = shares;
        l.cpu_weight = cpu_weight(shares);
    }
    if let Some(p) = h["PidsLimit"].as_i64().filter(|p| *p > 0) {
        l.pids_max = p as u32;
    }
    l
}

pub(super) fn create(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let image_name = body["Image"].as_str().unwrap_or_default();
    if image_name.is_empty() {
        return Err(Error::BadRequest("no image specified".into()));
    }
    let host = &body["HostConfig"];
    // A seccomp profile Kiri cannot apply fails the create, before anything
    // (volumes) is made for it.
    if let super::seccomp::Choice::Profile(json) = super::seccomp::choice(&strings(&host["SecurityOpt"]).unwrap_or_default()).map_err(Error::BadRequest)? {
        super::seccomp::compile(json, profile_caps(caps_from(host)?, host["Privileged"].as_bool().unwrap_or(false))).map_err(Error::BadRequest)?;
    }
    let network_mode = match host["NetworkMode"].as_str().unwrap_or("default") {
        "" | "default" => body["NetworkingConfig"]["EndpointsConfig"].as_object()
            .and_then(|eps| endpoints_primary(eps.keys().map(String::as_str), |key| engine.lock_net().find(key).map(|n| (n.name.clone(), n.internal))))
            .unwrap_or_else(|| "bridge".to_owned()),
        m => m.to_owned(),
    };
    let network = match network_mode.strip_prefix("container:") {
        Some(target) => Some(container_network_mode(&body, engine, target)?),
        None => engine.lock_net().find(&network_mode).map(|n| n.name.clone()),
    };
    // A network that doesn't exist is refused at start, as Docker does. A
    // 404 here would be read by the docker CLI as a missing image, which it
    // then pulls before trying again.
    let network_mode = network.unwrap_or(network_mode);
    // One endpoint per network (API 1.44+ allows several at create, which
    // Compose uses); NetworkMode or the endpoints above pick the primary.
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
            let Some(found) = engine.lock_net().find(net).cloned() else {
                if *net == network_mode {
                    // The missing primary network: start refuses it.
                    aliases.extend(names);
                    continue;
                }
                return Err(Error::NotFound(format!("network {net} not found")));
            };
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
            } else if !extra_networks.iter().any(|x: &super::engine::ExtraNetwork| x.name == canonical) {
                extra_networks.push(super::engine::ExtraNetwork { name: canonical, aliases: names, ipv4_address: ip, ipv6_address: ip6, mac_address: mac });
            }
        }
    }
    if !mac_address.is_empty() && super::network::parse_mac(&mac_address).is_none() {
        return Err(Error::BadRequest(format!("invalid MAC address: {mac_address}")));
    }
    if !mac_address.is_empty() && network_mode == "host" {
        return Err(Error::BadRequest("conflicting options: mac-address and the network mode".into()));
    }
    let pid_mode = pid_mode(host["PidMode"].as_str().unwrap_or(""), engine)?;
    let links = links_from(&strings(&host["Links"]).unwrap_or_default(), engine)?;
    if !links.is_empty() && network_mode == "host" {
        return Err(Error::BadRequest("conflicting options: host type networking can't be used with links. This would result in undefined behavior".into()));
    }
    // `--add-host name:ip` arrives as "name:ip"; /etc/hosts wants "ip name".
    // Both are written into the container's /etc/hosts by the guest, so a
    // newline in either would add lines of its own.
    let mut extra_hosts = Vec::new();
    for h in strings(&host["ExtraHosts"]).unwrap_or_default() {
        let Some((name, ip)) = h.split_once(':') else { continue };
        let ip = if ip == "host-gateway" { super::ports::HOST_ADDR } else { ip };
        if ip.parse::<std::net::IpAddr>().is_err() {
            return Err(Error::BadRequest(format!("invalid IP address: {ip}")));
        }
        if !valid_hosts_name(name) {
            return Err(Error::BadRequest(format!("invalid host name ({name})")));
        }
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

    let image = engine.image(image_name)?.ok_or_else(|| Error::NotFound(format!("No such image: {image_name}")))?;
    if let Some(why) = intel_mac_refusal(image_name, std::slice::from_ref(&image.platform)) {
        return Err(Error::BadRequest(why));
    }
    // A local image for another platform is not the one asked for: the CLI
    // pulls the right one when told there is none.
    if let Some(p) = req.query("platform").filter(|p| !p.is_empty()).and_then(Platform::parse) {
        if image.platform != p.to_string() {
            return Err(Error::NotFound(format!("No such image: {image_name} for {p}")));
        }
    }
    // The image's own config, for what Kiri's image model does not keep.
    let image_cfg = image_config_json(engine, &image.config_digest);
    // Checked before mounts, which may create volumes.
    let healthcheck = healthcheck_from(&body, &image_cfg)?;
    let (entrypoint, cmd) = command(strings(&body["Entrypoint"]), strings(&body["Cmd"]), &image.config.entrypoint, &image.config.cmd)?;
    let mounts = mounts(&body, engine, &image)?;
    let ports = port_bindings(&body, engine, &image)?;
    if !ports.is_empty() && network_mode == "none" {
        return Err(Error::BadRequest("a container with --network none cannot publish ports".into()));
    }
    let mut env = image.config.env.clone();
    // Settings → Network → "Pass to containers": the proxy, unless set here.
    let given = strings(&body["Env"]).unwrap_or_default();
    for p in super::proxy::container_env() {
        let key = p.split('=').next().unwrap_or_default();
        if !given.iter().chain(&env).any(|e| e.split('=').next() == Some(key)) {
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
    // The guest writes it to the container's /etc/hostname and /etc/hosts.
    let hostname = pick("Hostname", "");
    if !hostname.is_empty() && !valid_hostname(&hostname) {
        return Err(Error::BadRequest(format!("invalid hostname ({hostname})")));
    }
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
        limits: super::engine::Limits { shm_size: host["ShmSize"].as_u64().unwrap_or(0), ..limits_from(host, super::engine::Limits::default()) },
        caps: caps_from(host)?,
        cap_add: strings(&host["CapAdd"]).unwrap_or_default(),
        cap_drop: strings(&host["CapDrop"]).unwrap_or_default(),
        security_opt: strings(&host["SecurityOpt"]).unwrap_or_default(),
        readonly_rootfs: host["ReadonlyRootfs"].as_bool().unwrap_or(false),
        hostname,
        stop_signal: body["StopSignal"].as_str().filter(|s| !s.is_empty()).map(signal).or_else(|| {
            image_cfg["config"]["StopSignal"].as_str().filter(|s| !s.is_empty()).map(signal)
        }),
        stop_timeout: body["StopTimeout"].as_i64(),
        healthcheck,
        shell: strings(&body["Shell"]).filter(|s| !s.is_empty()).or_else(|| strings(&image_cfg["config"]["Shell"])).unwrap_or_default(),
        init: host["Init"].as_bool(),
        extra_networks,
        log_config: super::logfile::LogConfig::from_host(&host["LogConfig"]).map_err(Error::BadRequest)?,
        network_mode,
        ipv4_address,
        ipv6_address,
        mac_address,
        left_all_networks: false,
        dns: strings(&host["Dns"]).unwrap_or_default(),
        dns_search: strings(&host["DnsSearch"]).unwrap_or_default(),
        dns_options: strings(&host["DnsOptions"]).unwrap_or_default(),
        domainname: body["Domainname"].as_str().unwrap_or_default().to_owned(),
        links,
        labels: merge_labels(&image_cfg, &body["Labels"]),
        tty: body["Tty"].as_bool().unwrap_or(false),
        open_stdin: body["OpenStdin"].as_bool().unwrap_or(false),
        console_size: console_size(&host["ConsoleSize"]),
        aliases,
        extra_hosts,
        ports,
        exposed: exposed_ports(&body, engine, &image),
        host_opts,
        volumes_from: strings(&host["VolumesFrom"]).unwrap_or_default(),
        pid_mode,
        mounts: mounts.guest,
        mount_info: mounts.info,
        volumes: mounts.volumes,
        anon_volumes: mounts.anon,
        restart: RestartPolicy {
            name: match host["RestartPolicy"]["Name"].as_str() {
                Some("") | None => "no".to_owned(),
                Some(n) => n.to_owned(),
            },
            max_retries: host["RestartPolicy"]["MaximumRetryCount"].as_u64().unwrap_or(0) as u32,
        },
    };
    if !matches!(spec.restart.name.as_str(), "no" | "always" | "unless-stopped" | "on-failure") {
        return Err(Error::BadRequest(format!("invalid restart policy {}", spec.restart.name)));
    }
    if spec.auto_remove && spec.restart.name != "no" {
        return Err(Error::BadRequest("can't create 'AutoRemove' container with restart policy".into()));
    }
    let id = engine.create(req.query("name").map(str::to_owned), spec, &image)?;
    Ok(json!({"Id": id, "Warnings": warnings}))
}

/// A container that create's options name (`--volumes-from`, `--pid
/// container:`, `--link`, `--network container:`) and that does not exist:
/// a 400, as Docker answers. A 404 at create tells the docker CLI the image
/// is missing, so it pulled the image and tried again before showing this.
fn missing_container(e: Error, context: &str) -> Error {
    match e {
        Error::NotFound(m) => Error::BadRequest(format!("{context}{m}")),
        e => e,
    }
}

/// `--pid`: empty, `host`, or `container:<target>`, kept as
/// `container:<full id>` as Docker does. The target must exist now and run
/// at start.
fn pid_mode(mode: &str, engine: &Engine) -> Result<String, Error> {
    match mode.split_once(':') {
        None if mode.is_empty() || mode == "host" => Ok(mode.to_owned()),
        Some(("container", target)) if !target.is_empty() => Ok(format!("container:{}", engine.resolve(target).map_err(|e| missing_container(e, ""))?)),
        _ => Err(Error::BadRequest(format!("invalid PID mode: {mode}"))),
    }
}

/// `--network container:<target>`: Docker's checks, then the mode as it is
/// kept, `container:<full id>`. The target must exist now and run at start.
fn container_network_mode(body: &Value, engine: &Engine, target: &str) -> Result<String, Error> {
    container_mode_allowed(body, target)?;
    let id = engine.resolve(target).map_err(|e| missing_container(e, "joining network namespace of container: "))?;
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
        return Err(Error::BadRequest("Invalid network mode: invalid container format container:<name|id>".into()));
    }
    if given(&body["Hostname"]) {
        return conflict("hostname and the network mode");
    }
    if given(&host["Links"]) {
        return conflict("container type network can't be used with links. This would result in undefined behavior");
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
    let ip = ep["IPAMConfig"]["IPv4Address"].as_str().unwrap_or_default().to_owned();
    if !ip.is_empty() && super::network::parse_ip(&ip).is_none() {
        return Err(Error::BadRequest(format!("invalid IPv4 address: {ip}")));
    }
    let ip6 = ep["IPAMConfig"]["IPv6Address"].as_str().unwrap_or_default().to_owned();
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
        let (name, alias) = parse_link(l).ok_or_else(|| Error::BadRequest(format!("invalid link: {l}")))?;
        let id = engine.resolve(name).map_err(|e| missing_container(e, &format!("could not get container for {name}: ")))?;
        let name = engine.with(&id, |c| c.name.clone())?;
        out.push(super::engine::Link { id, name, alias: alias.to_owned() });
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
fn healthcheck_from(body: &Value, image_cfg: &Value) -> Result<Option<super::health::Healthcheck>, Error> {
    use super::health::Healthcheck;
    let mine = Healthcheck::from_json(&body["Healthcheck"]);
    if let Some(h) = &mine {
        h.validate().map_err(Error::BadRequest)?;
    }
    Ok(super::health::merge(mine, Healthcheck::from_json(&image_cfg["config"]["Healthcheck"])))
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
    // It left every network it had: none at all, as Docker shows it, not
    // the `none` it is kept on.
    if c.spec.network_mode.starts_with("container:") || c.spec.left_all_networks {
        return json!({});
    }
    let running = c.status == Status::Running;
    let mut out = serde_json::Map::new();
    let mut add = |name: &str, ip: Option<[u8; 4]>, ip6: Option<[u8; 16]>, aliases: &[String], want4: &str, want6: &str| {
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
    add(&c.spec.network_mode, c.ip, c.ip6, &c.spec.aliases, &c.spec.ipv4_address, &c.spec.ipv6_address);
    for x in &c.spec.extra_networks {
        let ip6 = c.extra_ip6s.iter().find(|(n, _)| *n == x.name).map(|(_, ip)| *ip);
        add(&x.name, c.extra_ips.iter().find(|(n, _)| *n == x.name).map(|(_, ip)| *ip), ip6, &x.aliases, &x.ipv4_address, &x.ipv6_address);
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
    if m.is_empty() { Value::Null } else { Value::Object(m) }
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
    let f = filters(req, &["id", "name", "label", "label!", "status", "ancestor", "network", "exited", "volume", "health"])?;
    if let Some(bad) = f.values("health").and_then(|v| v.iter().find(|v| !super::health::valid_filter(v))) {
        return Err(Error::BadRequest(format!("Unrecognised filter value for health: {bad}")));
    }
    let health = engine.health_statuses();
    // Docker matches the image each `ancestor` value names, by ID (#442).
    let ancestors = f.values("ancestor").map(|v| engine.ancestor_ids(v)).transpose()?;
    // A status filter lists stopped containers without --all, as Docker does.
    let all = req.flag("all") || f.values("status").is_some() || f.values("exited").is_some();
    let networks: Vec<super::network::Network> = engine.lock_net().all().to_vec();
    let mut rows = engine.each(|c| {
        if !all && c.status != Status::Running {
            return None;
        }
        let labels = if c.spec.labels.is_object() { c.spec.labels.clone() } else { json!({}) };
        let net = networks.iter().find(|n| n.name == c.spec.network_mode || n.id == c.spec.network_mode || (c.spec.network_mode == "default" && n.name == "bridge"));
        let keep = f.any("id", |v| c.id.starts_with(v))
            && f.name(&c.name)
            && f.labels(&labels)
            && f.any("status", |v| v == state_word(c))
            && f.any("exited", |v| c.status == Status::Exited && v.parse() == Ok(c.exit_code))
            && ancestors.as_ref().is_none_or(|ids| ids.contains(&c.spec.image_id))
            && f.any("network", |v| net.is_some_and(|n| n.name == v || n.id.starts_with(v)))
            && f.any("volume", |v| c.spec.volumes.iter().chain(&c.spec.anon_volumes).any(|n| n == v) || c.spec.mount_info.iter().any(|m| m["Destination"] == v || m["Source"] == v))
            && f.any("health", |v| v == health.get(&c.id).map_or("none", |h| h.status.as_str()));
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
    let when = |t: Option<std::time::SystemTime>| t.map_or_else(|| time::ZERO.to_owned(), time::rfc3339);
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
            "ExposedPorts": inspect_exposed(&c.spec.ports, &c.spec.exposed),
        },
        "HostConfig": {
            "PortBindings": inspect_bindings(&c.spec.ports),
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
            // Containers saved before the shares were kept have only the
            // weight, from the old straight-line conversion.
            "CpuShares": if c.spec.limits.cpu_shares > 0 {
                c.spec.limits.cpu_shares
            } else if c.spec.limits.cpu_weight > 0 {
                2 + (u64::from(c.spec.limits.cpu_weight) - 1) * 262_142 / 9999
            } else {
                0
            },
            "PidsLimit": if c.spec.limits.pids_max > 0 { Value::from(c.spec.limits.pids_max) } else { Value::Null },
            "NetworkMode": c.spec.network_mode,
            "PidMode": c.spec.pid_mode,
            "VolumesFrom": if c.spec.volumes_from.is_empty() { Value::Null } else { json!(c.spec.volumes_from) },
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
    if c.spec.network_mode.starts_with("container:") || c.spec.left_all_networks {
        return json!({"IPAddress": "", "IPPrefixLen": 0, "MacAddress": "", "Ports": {}, "Networks": {}});
    }
    let prefix_of = |name: &str, has_ip: bool| if has_ip { nets.iter().find(|n| n.name == name).map_or(16, |n| n.prefix) } else { 0 };
    // An IPv6-only network's IPv4 addresses are Kiri's own, not shown.
    let v4_on = |name: &str| !nets.iter().any(|n| n.name == name && n.no_ipv4);
    let ip = c.ip.filter(|_| v4_on(&c.spec.network_mode)).map(super::network::ip_str).unwrap_or_default();
    let v6 = |name: &str, ip6: Option<[u8; 16]>| {
        let n = nets.iter().find(|n| n.name == name).and_then(|n| n.v6.as_ref()).filter(|_| ip6.is_some());
        (ip6.map(super::network::ip6_str).unwrap_or_default(), n.map_or(0, |v| v.prefix), n.map(|v| super::network::ip6_str(v.gateway)).unwrap_or_default())
    };
    let mut networks = serde_json::Map::new();
    let (ip6, prefix6, gateway6) = v6(&c.spec.network_mode, c.ip6);
    networks.insert(
        c.spec.network_mode.clone(),
        json!({"IPAddress": ip, "IPPrefixLen": prefix_of(&c.spec.network_mode, !ip.is_empty()), "IPAMConfig": ipam_config(&c.spec.ipv4_address, &c.spec.ipv6_address), "Aliases": c.spec.aliases, "NetworkID": "", "MacAddress": c.spec.mac_address, "GlobalIPv6Address": ip6, "GlobalIPv6PrefixLen": prefix6, "IPv6Gateway": gateway6}),
    );
    for x in &c.spec.extra_networks {
        let ip = c.extra_ips.iter().find(|(n, _)| *n == x.name).filter(|_| v4_on(&x.name)).map(|(_, ip)| super::network::ip_str(*ip)).unwrap_or_default();
        let prefix = prefix_of(&x.name, !ip.is_empty());
        let (xip6, xprefix6, xgateway6) = v6(&x.name, c.extra_ip6s.iter().find(|(n, _)| *n == x.name).map(|(_, a)| *a));
        networks.insert(x.name.clone(), json!({"IPAddress": ip, "IPPrefixLen": prefix, "IPAMConfig": ipam_config(&x.ipv4_address, &x.ipv6_address), "Aliases": x.aliases, "NetworkID": "", "MacAddress": x.mac_address, "GlobalIPv6Address": xip6, "GlobalIPv6PrefixLen": xprefix6, "IPv6Gateway": xgateway6}));
    }
    json!({"IPAddress": ip, "IPPrefixLen": prefix_of(&c.spec.network_mode, !ip.is_empty()), "MacAddress": c.spec.mac_address, "GlobalIPv6Address": ip6, "GlobalIPv6PrefixLen": prefix6, "IPv6Gateway": gateway6, "Ports": ports_map(c), "Networks": networks})
}

/// Docker's state word: `paused` is a running container that is frozen.
fn state_word(c: &Container) -> &'static str {
    if c.paused { "paused" } else { c.status.as_str() }
}

fn net_error(m: String) -> Error {
    if m.contains("not found") {
        Error::NotFound(m)
    } else if m.contains("active endpoints") || m.contains("already exists") || m.contains("pre-defined") || m.contains("Pool overlaps") {
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
            c["AuxiliaryAddresses"] = n.aux.iter().map(|(k, ip)| (k.clone(), Value::from(super::network::ip_str(*ip)))).collect::<serde_json::Map<_, _>>().into();
        }
        // An IPv6-only network's IPv4 subnet is Kiri's own, not shown.
        let mut configs = if n.no_ipv4 { Vec::new() } else { vec![c] };
        if let Some(v) = &n.v6 {
            let mut c = json!({"Subnet": v.subnet_str(), "Gateway": super::network::ip6_str(v.gateway)});
            if let Some(r) = v.ip_range_str() {
                c["IPRange"] = r.into();
            }
            if !v.aux.is_empty() {
                c["AuxiliaryAddresses"] = v.aux.iter().map(|(k, ip)| (k.clone(), Value::from(super::network::ip6_str(*ip)))).collect::<serde_json::Map<_, _>>().into();
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
        "Options": super::network::shown_options(n),
        "Labels": if n.labels.is_object() { n.labels.clone() } else { json!({}) },
    })
}

fn network_matches(f: &Filters, engine: &Engine, n: &super::network::Network) -> bool {
    let labels = if n.labels.is_object() { n.labels.clone() } else { json!({}) };
    f.name(&n.name)
        && f.network_id(&n.id)
        && f.any("driver", |v| v == n.driver)
        && f.labels(&labels)
        && f.any("scope", |v| v == "local")
        && f.any("type", |v| (v == "custom") != n.builtin())
        && f.bool("dangling").is_none_or(|d| d == (!n.builtin() && engine.network_members(&n.name).is_empty()))
}

const NETWORK_FILTERS: &[&str] = &["name", "id", "driver", "label", "label!", "scope", "type", "dangling"];

fn networks(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = filters(req, NETWORK_FILTERS)?;
    let list: Vec<super::network::Network> = engine.lock_net().all().to_vec();
    Ok(Value::Array(list.iter().filter(|n| network_matches(&f, engine, n)).map(|n| network_json(engine, n, false)).collect()))
}

fn network_inspect(engine: &Engine, key: &str) -> Result<Value, Error> {
    let n = engine.lock_net().find(key).cloned().ok_or_else(|| Error::NotFound(format!("network {key} not found")))?;
    Ok(network_json(engine, &n, true))
}

fn network_create(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let name = body["Name"].as_str().unwrap_or_default();
    match body["Driver"].as_str().unwrap_or("bridge") {
        "" | "bridge" => {}
        d => return Err(Error::BadRequest(format!("Kiri supports only bridge networks, not {d}"))),
    }
    let (ipam, ipam6) = super::network::parse_ipam_both(&body["IPAM"]).map_err(Error::BadRequest)?;
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
    let n = engine.lock_net().create(name, settings).map_err(net_error)?;
    Ok(json!({"Id": n.id, "Warning": ""}))
}

fn network_prune(req: &Request, engine: &Engine) -> Result<Value, Error> {
    // While the container list may be missing containers, "unused" can't
    // be told; say so rather than quietly pruning less.
    if let Some(why) = engine.refusal("Networks were not pruned, because the container list may be missing containers that use them") {
        return Err(Error::Conflict(why));
    }
    // Docker takes `until` here too (moby daemon/prune.go); `docker system
    // prune --filter until=…` sends it.
    let f = filters(req, &["label", "label!", "until"])?;
    let until = super::ops::prune_until(&f, "until")?;
    let nets = engine.lock_net();
    let unused: Vec<String> = nets
        .all()
        .iter()
        .filter(|n| {
            !n.builtin()
                && !nets.in_use(&n.name)
                && f.labels(if n.labels.is_object() { &n.labels } else { &Value::Null })
                && super::ops::before(until, n.created_ns as f64 / 1e9)
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

fn attach(req: &Request, engine: &Arc<Engine>, key: &str, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<Next> {
    let (want_out, want_err, want_in) = (req.flag("stdout"), req.flag("stderr"), req.flag("stdin"));
    let (id, tty, rx) = match engine.attach(key) {
        Ok(v) => v,
        Err(e) => {
            error(w, e)?;
            return Ok(Next::KeepAlive);
        }
    };
    http::upgrade_if_asked(req, w)?;
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
    std::thread::Builder::new().name("kiri-stdin".into()).spawn(move || {
        if !early.is_empty() && engine.input(&id, &early).is_err() {
            return;
        }
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            wait_readable(&stream);
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

/// Waits until a read of `stream` will not block. macOS can leave a thread
/// blocked in recv on a unix socket asleep through the peer's half-close when
/// it closely follows data, though the next read would see the end. For
/// `docker run -i` that was a process waiting forever for the end of its input,
/// in 1 to 5 runs in 100. poll is woken for it.
fn wait_readable(stream: &UnixStream) {
    let mut fd = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd. Any other failure leaves the read to block or to
    // report it, as without this.
    while unsafe { libc::poll(&mut fd, 1, -1) } == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {}
}

/// Sends output to an attached client: raw for a terminal, Docker's
/// multiplexed frames otherwise. Ends when the process exits.
fn pump(rx: std::sync::mpsc::Receiver<(Stream, Vec<u8>)>, tty: bool, want_out: bool, want_err: bool, w: &mut UnixStream) -> io::Result<()> {
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
    let n = |k: &str| req.query(k).and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
    let (rows, cols) = (n("h"), n("w"));
    if rows == 0 || cols == 0 {
        return Ok(());
    }
    engine.resize(key, rows, cols)
}

fn exec_create(req: &Request, engine: &Engine, key: &str) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
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
        cwd: if config.cwd.is_empty() { c.spec.cwd.clone() } else { config.cwd.clone() },
        user: if config.user.is_empty() { c.spec.user.clone() } else { config.user.clone() },
        privileged: config.privileged || c.spec.privileged,
        ..config.clone()
    })?;
    let id = engine.exec_create(key, config)?;
    Ok(json!({"Id": id}))
}

fn exec_start(req: &Request, engine: &Arc<Engine>, id: &str, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<Next> {
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
        // 200 with no body, as Docker answers a detached start.
        http::empty(w, 200)?;
        return Ok(Next::KeepAlive);
    }
    http::upgrade_if_asked(req, w)?;
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
    let (want_out, want_err, stamps) = (req.flag("stdout"), req.flag("stderr"), req.flag("timestamps"));
    if !want_out && !want_err {
        error(w, Error::BadRequest("Bad parameters: you must choose at least one stream".into()))?;
        return Ok(Next::KeepAlive);
    }
    let tail = parse_tail(req.query("tail"));
    let since = req.query("since").and_then(parse_time);
    let until = req.query("until").and_then(parse_time);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
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
    let content_type = if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" };
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
            line.extend_from_slice(time::rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(r.time)).as_bytes());
            line.push(b' ');
            line.extend_from_slice(&r.data);
            &line[..]
        } else {
            &r.data[..]
        };
        if tty { out.extend_from_slice(data) } else { http::mux_frame(out, r.stream as u8, data)? }
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
                next = if out.len() < 32 << 10 { rx.try_recv().ok() } else { None };
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
    let len = kiri_proto::frame_len(buf[..4].try_into().expect("4 bytes")).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
    buf.resize(len, 0);
    f.read_exact(&mut buf[4..])?;
    Ok(buf)
}

/// Sends a copy-in archive as `copy_flags::CHUNKED` chunks, then the empty
/// one that ends it. An error is the archive's own source failing; if the
/// guest stops reading (it refused, and says why) sending just stops.
pub(super) fn send_chunks(archive: &mut dyn io::Read, to_guest: &mut impl io::Write) -> io::Result<()> {
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
    format!("X-Docker-Container-Path-Stat: {}\r\n", kiri_image::b64::encode(stat.to_string().as_bytes()))
}

/// `docker cp` out of a container (GET), or the stat it starts with (HEAD).
fn archive_get(req: &Request, engine: &Engine, id: &str, w: &mut UnixStream) -> io::Result<Next> {
    use std::io::Read;
    let path = req.query("path").unwrap_or("/");
    let op = if req.method == "HEAD" { kiri_proto::copy_op::STAT } else { kiri_proto::copy_op::OUT };
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
                let status = match copy_error(m) { Error::NotFound(_) => 404, _ => 400 };
                http::head_only(w, status, "")?;
                return Ok(Next::KeepAlive);
            }
            return reply(w, Err(copy_error(m)), 200);
        }
        other => return reply(w, Err(Error::Io(io::Error::other(format!("unexpected copy reply: {other:?}")))), 200),
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
#[derive(Debug, PartialEq)]
enum Upload {
    Tar,
    Gzip,
    Zstd,
    Unsupported(&'static str),
}

/// What an archive upload (`docker cp` in, `docker load`) is, from its first
/// bytes. Docker accepts tar, gzip, bzip2, xz and zstd here; anything that
/// isn't compressed is tar.
fn upload_compression(head: &[u8]) -> Upload {
    match head {
        [0x1f, 0x8b, ..] => Upload::Gzip,
        [b'B', b'Z', b'h', ..] => Upload::Unsupported("bzip2"),
        [0xfd, b'7', b'z', b'X', b'Z', 0, ..] => Upload::Unsupported("xz"),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Upload::Zstd,
        _ => Upload::Tar,
    }
}

/// A read of the upload failed. A damaged gzip or zstd archive is the
/// client's input, as a damaged tar is (the guest refuses that one with 400).
fn upload_error(gzip: bool, e: io::Error) -> Error {
    if gzip {
        Error::BadRequest(format!("the gzip archive is damaged or cut short: {e}"))
    } else if e.get_ref().is_some_and(|inner| inner.is::<ZstdDamaged>()) {
        Error::BadRequest(e.to_string())
    } else {
        Error::Io(e)
    }
}

/// What `ZstdFrames` fails a read with, so `upload_error` knows the archive
/// and not the connection was at fault.
#[derive(Debug)]
struct ZstdDamaged(String);

impl std::fmt::Display for ZstdDamaged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the zstd archive is damaged or cut short: {}", self.0)
    }
}

impl std::error::Error for ZstdDamaged {}

/// A zstd upload, decompressed: every frame of it, one after the other, as
/// `zstd -d` reads a file (`pzstd` and `zstd` on several inputs write more
/// than one frame). The decoder reads one frame and stops.
struct ZstdFrames<R: io::BufRead> {
    /// None between frames and at the end.
    frame: Option<ruzstd::decoding::StreamingDecoder<R, ruzstd::decoding::FrameDecoder>>,
    /// The source, while no frame holds it.
    source: Option<R>,
}

impl<R: io::BufRead> ZstdFrames<R> {
    fn new(source: R) -> ZstdFrames<R> {
        ZstdFrames { frame: None, source: Some(source) }
    }
}

impl<R: io::BufRead> io::Read for ZstdFrames<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let damaged = |e: String| io::Error::new(io::ErrorKind::InvalidData, ZstdDamaged(e));
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(frame) = &mut self.frame {
                let n = frame.read(buf).map_err(|e| damaged(e.to_string()))?;
                if n > 0 {
                    return Ok(n);
                }
                // This frame is done: the source goes back for the next one.
                self.source = self.frame.take().map(ruzstd::decoding::StreamingDecoder::into_inner);
            }
            let Some(mut source) = self.source.take() else { return Ok(0) };
            if source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            self.frame = Some(ruzstd::decoding::StreamingDecoder::new(source).map_err(|e| damaged(e.to_string()))?);
        }
    }
}

/// The upload as tar: `head` (its first bytes, already read) then `rest`.
fn upload_reader<'a>(head: Vec<u8>, rest: impl io::Read + 'a) -> Result<Box<dyn io::Read + 'a>, Error> {
    use std::io::Read;
    match upload_compression(&head) {
        Upload::Tar => Ok(Box::new(io::Cursor::new(head).chain(rest))),
        Upload::Gzip => Ok(Box::new(flate2::read::MultiGzDecoder::new(io::Cursor::new(head).chain(rest)))),
        Upload::Zstd => Ok(Box::new(ZstdFrames::new(io::BufReader::new(io::Cursor::new(head).chain(rest))))),
        Upload::Unsupported(kind) => Err(Error::BadRequest(format!("a {kind}-compressed archive isn't supported yet: send it as tar, gzip or zstd"))),
    }
}

fn archive_put(req: &Request, engine: &Engine, id: &str, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<Next> {
    use kiri_proto::copy_flags;
    let path = req.query("path").unwrap_or("/");
    let mut flags = copy_flags::CHUNKED;
    if req.flag("copyUIDGID") {
        flags |= copy_flags::KEEP_OWNERS;
    }
    if req.flag("noOverwriteDirNonDir") {
        flags |= copy_flags::NO_OVERWRITE_DIR_NON_DIR;
    }
    // Docker takes the archive plain or compressed; the guest unpacks tar.
    use std::io::Read;
    let mut body = http::Body::new(req, r);
    let mut head = Vec::with_capacity(6);
    (&mut body).take(6).read_to_end(&mut head)?;
    let gzip = upload_compression(&head) == Upload::Gzip;
    let mut archive = match upload_reader(head, body) {
        Ok(a) => a,
        Err(e) => {
            error_closing(w, e)?;
            return Ok(Next::Close);
        }
    };
    let stream = match engine.copy(id, kiri_proto::copy_op::IN, path, flags) {
        Ok(s) => s,
        Err(e) => {
            error_closing(w, e)?;
            return Ok(Next::Close);
        }
    };
    let mut to_guest = std::fs::File::from(stream.fd.try_clone()?);
    if let Err(e) = send_chunks(&mut archive, &mut to_guest) {
        // The body broke off or isn't valid gzip: dropping the stream ends
        // the guest's copy.
        error_closing(w, upload_error(gzip, e))?;
        return Ok(Next::Close);
    }
    let mut from_guest = std::fs::File::from(stream.fd.try_clone()?);
    let result = match guest_frame(&mut from_guest) {
        Ok(frame) => match kiri_proto::decode(&frame) {
            Ok(kiri_proto::Msg::CopyDone) => Ok(()),
            Ok(kiri_proto::Msg::Error(m)) => Err(copy_error(m)),
            Ok(other) => Err(Error::Io(io::Error::other(format!("unexpected copy reply: {other:?}")))),
            Err(e) => Err(Error::Io(io::Error::other(format!("bad copy reply: {e:?}")))),
        },
        Err(e) => Err(Error::Io(e)),
    };
    // Docker answers 200 with no body. The archive was read to its end, so
    // the connection serves the client's next request (docker-java pools it
    // and sends `start` next); after a refusal the body may be partly unread.
    match result {
        // Whatever the guest left unread is read here (normally nothing).
        Ok(()) if io::copy(&mut archive, &mut io::sink()).is_ok() => {
            http::respond(w, 200, "text/plain", b"")?;
            Ok(Next::KeepAlive)
        }
        Ok(()) => {
            error_closing(w, Error::BadRequest("the archive could not be read to its end".into()))?;
            Ok(Next::Close)
        }
        Err(e) => {
            error_closing(w, e)?;
            Ok(Next::Close)
        }
    }
}

/// A signal by name or number for a stop signal Docker has already
/// accepted; one it could not parse is SIGTERM, Docker's default.
pub(super) fn signal(name: &str) -> u32 {
    parse_signal(name).unwrap_or(15)
}

/// Linux's signals by name, as Docker's `signal.SignalMap` has them.
const SIGNALS: [(&str, u32); 34] = [
    ("HUP", 1), ("INT", 2), ("QUIT", 3), ("ILL", 4), ("TRAP", 5), ("ABRT", 6), ("IOT", 6), ("BUS", 7),
    ("FPE", 8), ("KILL", 9), ("USR1", 10), ("SEGV", 11), ("USR2", 12), ("PIPE", 13), ("ALRM", 14), ("TERM", 15),
    ("STKFLT", 16), ("CHLD", 17), ("CLD", 17), ("CONT", 18), ("STOP", 19), ("TSTP", 20), ("TTIN", 21), ("TTOU", 22),
    ("URG", 23), ("XCPU", 24), ("XFSZ", 25), ("VTALRM", 26), ("PROF", 27), ("WINCH", 28), ("IO", 29), ("POLL", 29),
    ("PWR", 30), ("SYS", 31),
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
            _ => Err(Error::BadRequest(format!("the linux daemon does not support signal {n}"))),
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
    if let Some(n) = name.strip_prefix("RTMAX").and_then(|r| offset(r, '-')).filter(|n| *n <= 14) {
        return Ok(64 - n);
    }
    Err(invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback registry that takes only `Basic me:pw`.
    fn basic_registry() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let want = format!("Basic {}", kiri_image::b64::encode(b"me:pw"));
        std::thread::spawn(move || {
            for c in listener.incoming() {
                let Ok(mut c) = c else { break };
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && c.read(&mut b).unwrap_or(0) == 1 {
                    head.push(b[0]);
                }
                let ok = String::from_utf8_lossy(&head).lines().any(|l| l.split_once(':').is_some_and(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.trim() == want));
                let status = if ok { "200 OK" } else { "401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"kiri\"" };
                let _ = write!(c, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        host
    }

    /// docker-java sends every AuthConfig field, unused ones as JSON null,
    /// in `X-Registry-Auth` and in `POST /auth`; Docker reads null as empty (#654).
    #[test]
    fn registry_auth_with_null_fields_signs_in() {
        let host = basic_registry();
        let json = format!(r#"{{"username":"me","password":"pw","serveraddress":"{host}","identitytoken":null,"registrytoken":null,"email":null,"auth":null,"stackOrchestrator":null}}"#);
        let header = kiri_image::b64::encode(json.as_bytes()).replace('+', "-").replace('/', "_").trim_end_matches('=').to_owned();
        let pull = Request::internal("POST", "/images/create", &[], Vec::new()).with_header("X-Registry-Auth", &header);
        client_for(&pull).login(&host).expect("pull client signs in with the header's password");
        assert!(client_for(&Request::internal("POST", "/images/create", &[], Vec::new())).login(&host).is_err(), "no header is anonymous");

        let login = auth(&Request::internal("POST", "/auth", &[], json.into_bytes())).expect("POST /auth with null fields");
        assert_eq!(login["Status"], "Login Succeeded");
        let wrong = format!(r#"{{"username":"me","password":"nope","serveraddress":"{host}","identitytoken":null,"email":null}}"#);
        assert!(matches!(auth(&Request::internal("POST", "/auth", &[], wrong.into_bytes())), Err(Error::Unauthorized(_))));
        assert!(matches!(auth(&Request::internal("POST", "/auth", &[], b"not json".to_vec())), Err(Error::BadRequest(_))));
    }

    /// A null server address, or every field null, in `X-Registry-Auth`.
    #[test]
    fn registry_auth_header_with_null_address_or_all_null() {
        let host = basic_registry();
        let hdr = |json: String| kiri_image::b64::encode(json.as_bytes()).replace('+', "-").replace('/', "_").trim_end_matches('=').to_owned();
        let pull = |h: &str| client_for(&Request::internal("POST", "/images/create", &[], Vec::new()).with_header("X-Registry-Auth", h));
        let null_addr = hdr(r#"{"username":"me","password":"pw","serveraddress":null,"identitytoken":null}"#.into());
        pull(&null_addr).login(&host).expect("a null server address still signs in");
        let all_null = hdr(r#"{"username":null,"password":null,"auth":null,"serveraddress":null,"identitytoken":null,"registrytoken":null}"#.into());
        assert!(pull(&all_null).login(&host).is_err(), "every field null is anonymous");
        let addr_only = hdr(format!(r#"{{"serveraddress":"{host}","username":null,"password":null,"auth":null,"identitytoken":null,"registrytoken":null}}"#));
        assert!(pull(&addr_only).login(&host).is_err(), "an address with no credentials is anonymous");
    }

    /// One prune command sends its filters to every prune: volumes and
    /// networks go only when their labels match, as in Docker; a volume
    /// with driver options is never pruned; networks take `until`.
    #[test]
    fn volume_and_network_prune_follow_labels() {
        let root = std::env::temp_dir().join(format!("kiri-prune-labels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let engine = super::super::engine::boot_without_vm(&root);
        engine.volume_ensure_with("v-keep", json!({"keep": "1"}), Default::default()).unwrap();
        engine.volume_ensure_with("v-plain", json!({}), Default::default()).unwrap();
        engine.volume_ensure_with("v-opts", json!({}), [("type".to_owned(), "tmpfs".to_owned()), ("device".to_owned(), "tmpfs".to_owned())].into()).unwrap();
        let vp = |f: &str| volume_prune(&Request::internal("POST", "/volumes/prune", &[("filters", f)], Vec::new()), &engine);
        let names = |v: Result<Value, Error>, key: &str| -> Vec<String> { serde_json::from_value(v.unwrap()[key].clone()).unwrap() };
        assert!(names(vp(r#"{"all":["true"],"label":["nope=1"]}"#), "VolumesDeleted").is_empty());
        assert!(names(vp(r#"{"label!":["keep=1"]}"#), "VolumesDeleted").is_empty(), "named volumes need all");
        assert!(matches!(vp(r#"{"all":["yes"]}"#), Err(Error::BadRequest(_))));
        assert_eq!(names(vp(r#"{"all":["True"],"label!":["keep=1"]}"#), "VolumesDeleted"), ["v-plain"]);
        assert!(engine.volume_get("v-opts").is_some() && engine.volume_get("v-keep").is_some());

        let settings = |labels: Value| super::super::network::Settings { labels, ..Default::default() };
        engine.lock_net().create("n-keep", settings(json!({"keep": "1"}))).unwrap();
        engine.lock_net().create("n-plain", settings(Value::Null)).unwrap();
        let np = |f: &str| network_prune(&Request::internal("POST", "/networks/prune", &[("filters", f)], Vec::new()), &engine);
        assert!(names(np(r#"{"label":["nope=1"]}"#), "NetworksDeleted").is_empty());
        assert!(names(np(r#"{"until":["2000-01-01T00:00:00Z"]}"#), "NetworksDeleted").is_empty(), "made after the cut-off");
        assert!(matches!(np(r#"{"until":["soon"]}"#), Err(Error::BadRequest(_))));
        assert!(matches!(np(r#"{"until":["1h","2h"]}"#), Err(Error::BadRequest(_))));
        assert_eq!(names(np(r#"{"until":["2999-01-01"],"label":["keep=1"]}"#), "NetworksDeleted"), ["n-keep"]);
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// An engine whose container list was damaged (a kept copy is there),
    /// with a named volume and a network no listed container uses: they may
    /// be a lost container's, so prune removes neither and says why.
    #[test]
    fn prune_removes_nothing_while_the_container_list_is_damaged() {
        let root = std::env::temp_dir().join(format!("kiri-prune-damaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("containers.json.corrupt-1-0"), b"[{\"id\":").unwrap();
        let engine = super::super::engine::boot_without_vm(&root);
        engine.volume_ensure_with("lost-data", json!({}), Default::default()).unwrap();
        engine.lock_net().create("lost-net", Default::default()).unwrap();
        let req = Request::internal("POST", "/volumes/prune", &[("filters", r#"{"all":["true"]}"#)], Vec::new());
        match volume_prune(&req, &engine) {
            Err(Error::Conflict(why)) => assert!(why.starts_with("Volumes were not pruned") && why.contains("containers.json.corrupt-1-0"), "{why}"),
            other => panic!("volume prune: {other:?}"),
        }
        assert!(engine.volume_get("lost-data").is_some(), "the volume is kept");
        match network_prune(&Request::internal("POST", "/networks/prune", &[], Vec::new()), &engine) {
            Err(Error::Conflict(why)) => assert!(why.starts_with("Networks were not pruned") && why.contains("containers.json.corrupt-1-0"), "{why}"),
            other => panic!("network prune: {other:?}"),
        }
        assert!(engine.lock_net().find("lost-net").is_some(), "the network is kept");
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    fn engine_in(name: &str, damaged: bool) -> (std::path::PathBuf, std::sync::Arc<Engine>) {
        let root = std::env::temp_dir().join(format!("kiri-rm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        if damaged {
            std::fs::write(root.join("containers.json.corrupt-1-0"), b"[{\"id\":").unwrap();
        }
        let engine = super::super::engine::boot_without_vm(&root);
        (root, engine)
    }

    fn test_image(name: &str) -> kiri_image::Image {
        kiri_image::Image { mirror: None,
            name: name.into(), digest: "sha256:0123".into(), index_digest: None, config_digest: "sha256:0123".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        }
    }

    /// A container using `volume` and `image`, in the list.
    fn listed_user(engine: &Engine, volume: &str, image: &kiri_image::Image) {
        let spec: super::super::engine::Spec = serde_json::from_value(json!({
            "image": image.name, "image_id": image.config_digest, "entrypoint": [], "cmd": ["true"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "restart": {"name": "no", "max_retries": 0}, "volumes": [volume]
        })).unwrap();
        engine.create(Some("user".into()), spec, image).unwrap();
    }

    /// On a good list removal by name is as before: one a listed container
    /// uses is refused, one none uses goes.
    #[test]
    fn removal_by_name_on_a_good_list_is_unchanged() {
        let (root, engine) = engine_in("good", false);
        let image = test_image("used:1");
        engine.store.put_image(image.clone()).unwrap();
        engine.volume_ensure_with("used", json!({}), Default::default()).unwrap();
        engine.volume_ensure_with("free", json!({}), Default::default()).unwrap();
        listed_user(&engine, "used", &image);
        assert!(matches!(engine.volume_remove("used", false), Err(Error::Conflict(m)) if m.contains("volume is in use")));
        engine.volume_remove("free", false).unwrap();
        assert!(engine.volume_get("free").is_none());
        assert!(matches!(image_remove(&engine, "used:1", false), Err(Error::Conflict(m)) if m.contains("is using its referenced image")));
        engine.store.put_image(test_image("free:1")).unwrap();
        engine.store.put_image(kiri_image::Image { config_digest: "sha256:4567".into(), ..test_image("other:1") }).unwrap();
        image_remove(&engine, "other:1", false).unwrap();
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// On a damaged list a volume or image no listed container uses may be
    /// a lost container's (the window's Delete sends exactly this request):
    /// refused unless forced, and still there. Forced, it goes.
    #[test]
    fn removal_by_name_on_a_damaged_list_needs_force() {
        let (root, engine) = engine_in("damaged", true);
        engine.volume_ensure_with("lost-data", json!({}), Default::default()).unwrap();
        match engine.volume_remove("lost-data", false) {
            Err(Error::Conflict(why)) => assert!(why.starts_with("Volume lost-data was not removed") && why.contains("containers.json.corrupt-1-0"), "{why}"),
            other => panic!("volume rm: {other:?}"),
        }
        assert!(engine.volume_get("lost-data").is_some(), "the volume is kept");
        engine.volume_remove("lost-data", true).unwrap();
        assert!(engine.volume_get("lost-data").is_none(), "forced, it goes");
        engine.store.put_image(test_image("lost:1")).unwrap();
        match image_remove(&engine, "lost:1", false) {
            Err(Error::Conflict(why)) => assert!(why.starts_with("Image lost:1 was not removed") && why.contains("containers.json.corrupt-1-0"), "{why}"),
            other => panic!("rmi: {other:?}"),
        }
        assert_eq!(engine.store.images().unwrap().len(), 1, "the image is kept");
        // Untagging, with another tag left, deletes nothing: allowed.
        engine.store.put_image(test_image("lost:2")).unwrap();
        image_remove(&engine, "lost:2", false).unwrap();
        image_remove(&engine, "lost:1", true).unwrap();
        assert!(engine.store.images().unwrap().is_empty(), "forced, it goes");
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `--volumes-from` entries: the container and its mode, as Docker parses them.
    #[test]
    fn volumes_from_takes_a_container_and_a_mode() {
        assert_eq!(parse_volumes_from("db").unwrap(), ("db", false));
        assert_eq!(parse_volumes_from("db:ro").unwrap(), ("db", true));
        assert_eq!(parse_volumes_from("db:rw").unwrap(), ("db", false));
        assert_eq!(parse_volumes_from("db:").unwrap(), ("db", false), "an empty mode is rw, as in Docker");
        assert_eq!(parse_volumes_from("db:ro,z").unwrap(), ("db", true));
        for bad in ["", "db:rx", "db:ro,rw", ":ro", "db:,"] {
            assert!(matches!(parse_volumes_from(bad), Err(Error::BadRequest(_))), "{bad:?}");
        }
        assert!(matches!(parse_volumes_from("db:rx"), Err(Error::BadRequest(m)) if m == "invalid mode: rx"));
    }

    /// The other container's volumes and binds come at their paths, tmpfs
    /// mounts do not; `ro` makes each read-only, and without it a read-only
    /// mount stays read-only.
    #[test]
    fn volumes_from_copies_volumes_and_binds() {
        let guest = vec![
            "/var/lib/kiri/volumes/named\t/vol\tcreate,copyup".to_owned(),
            "/host/dir\t/data\tro".to_owned(),
            "tmpfs\t/scratch\ttmpfs,noexec".to_owned(),
            "/var/lib/kiri/volumes/x\t/nfs\tcreate\tnfs\t:/export\t0\taddr=1".to_owned(),
        ];
        let info = vec![
            json!({"Type": "volume", "Name": "named", "Destination": "/vol", "RW": true}),
            json!({"Type": "bind", "Source": "/host/dir", "Destination": "/data", "RW": false}),
            json!({"Type": "tmpfs", "Destination": "/scratch", "RW": true}),
            json!({"Type": "volume", "Name": "x", "Destination": "/nfs", "RW": true}),
        ];
        let rw = volumes_from_mounts(&guest, &info, false);
        assert_eq!(rw.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), [guest[0].as_str(), guest[1].as_str(), guest[3].as_str()]);
        assert_eq!(rw[1].1["RW"], false, "a read-only mount stays so");
        let ro = volumes_from_mounts(&guest, &info, true);
        assert_eq!(ro[0].0, "/var/lib/kiri/volumes/named\t/vol\tcreate,copyup,ro");
        assert_eq!(ro[1].0, "/host/dir\t/data\tro", "not twice");
        assert_eq!(ro[2].0, "/var/lib/kiri/volumes/x\t/nfs\tcreate,ro\tnfs\t:/export\t0\taddr=1", "the volume's own fields stay");
        assert!(ro.iter().all(|(_, i)| i["RW"] == false));
    }

    /// Through `mounts`: an unknown container is Docker's error; a known one
    /// (never started) lends its volumes, and a mount given at the same path
    /// wins. Its anonymous volume outlives it while the new one uses it.
    #[test]
    fn volumes_from_mounts_another_containers_volumes() {
        let (root, engine) = engine_in("volumes-from", false);
        let image = test_image("vf:1");
        engine.store.put_image(image.clone()).unwrap();
        engine.volume_ensure("named", json!({}), false).unwrap();
        engine.volume_ensure("anon1", json!({}), true).unwrap();
        let spec: super::super::engine::Spec = serde_json::from_value(json!({
            "image": image.name, "image_id": image.config_digest, "entrypoint": [], "cmd": ["true"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "restart": {"name": "no", "max_retries": 0}, "volumes": ["named"], "anon_volumes": ["anon1"],
            "mounts": ["/v/named\t/vol\tcreate,copyup", "/v/anon1\t/anon\tcreate,copyup", "/v/named\t/other\tcreate"],
            "mount_info": [
                {"Type": "volume", "Name": "named", "Destination": "/vol", "RW": true},
                {"Type": "volume", "Name": "anon1", "Destination": "/anon", "RW": true},
                {"Type": "volume", "Name": "named", "Destination": "/other", "RW": true},
            ],
        })).unwrap();
        engine.create(Some("src".into()), spec, &image).unwrap();

        let body = json!({"HostConfig": {"VolumesFrom": ["nosuch"]}});
        assert!(matches!(mounts(&body, &engine, &image), Err(Error::BadRequest(m)) if m == "No such container: nosuch"));

        let body = json!({"HostConfig": {"VolumesFrom": ["src:ro"], "Tmpfs": {"/other": ""}}});
        let m = mounts(&body, &engine, &image).unwrap();
        let dsts: Vec<&str> = m.info.iter().filter_map(|i| i["Destination"].as_str()).collect();
        assert_eq!(dsts, ["/other", "/vol", "/anon"]);
        assert_eq!(m.info[0]["Type"], "tmpfs", "the mount given here wins");
        assert_eq!(m.guest[1], "/v/named\t/vol\tcreate,copyup,ro");
        assert_eq!(m.volumes, ["named", "anon1"]);
        assert!(m.anon.is_empty(), "nothing of the other's is removed with this one");

        let user: super::super::engine::Spec = serde_json::from_value(json!({
            "image": image.name, "image_id": image.config_digest, "entrypoint": [], "cmd": ["true"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "restart": {"name": "no", "max_retries": 0}, "volumes": m.volumes, "mounts": m.guest, "mount_info": m.info,
            "volumes_from": ["src:ro"],
        })).unwrap();
        engine.create(Some("dst".into()), user, &image).unwrap();
        engine.remove("src", true, true).unwrap();
        assert!(engine.volume_get("anon1").is_some(), "dst still uses it");
        engine.remove("dst", true, true).unwrap();
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `--pid`: empty and `host` as given, `container:<name>` kept with the
    /// full id; anything else, or an unknown container, refused.
    #[test]
    fn pid_mode_is_kept_or_refused() {
        let (root, engine) = engine_in("pid-mode", false);
        let image = test_image("pid:1");
        engine.store.put_image(image.clone()).unwrap();
        listed_user(&engine, "v", &image);
        let id = engine.resolve("user").unwrap();
        assert_eq!(pid_mode("", &engine).unwrap(), "");
        assert_eq!(pid_mode("host", &engine).unwrap(), "host");
        assert_eq!(pid_mode("container:user", &engine).unwrap(), format!("container:{id}"));
        assert_eq!(pid_mode(&format!("container:{}", &id[..12]), &engine).unwrap(), format!("container:{id}"));
        assert!(matches!(pid_mode("container:nosuch", &engine), Err(Error::BadRequest(m)) if m == "No such container: nosuch"));
        for bad in ["private", "container:", "container", "hostx", "ns:/proc/1/ns/pid"] {
            assert!(matches!(pid_mode(bad, &engine), Err(Error::BadRequest(m)) if m == format!("invalid PID mode: {bad}")), "{bad}");
        }
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn kernel_release_is_trimmed_and_empty_is_none() {
        assert_eq!(parse_release("7.0.0-38-generic\n").as_deref(), Some("7.0.0-38-generic"));
        assert_eq!(parse_release(" \n"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reports_the_host_kernel() {
        let host = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
        assert_eq!(version()["KernelVersion"], host.trim());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn vm_platforms_report_the_guest_kernel() {
        assert_eq!(version()["KernelVersion"], GUEST_KERNEL);
    }

    /// Testcontainers gates on the engine version as a Docker version.
    #[test]
    fn version_is_a_docker_engine_version_and_kiri_is_beside_it() {
        let v = version();
        let major: u32 = v["Version"].as_str().unwrap().split('.').next().unwrap().parse().unwrap();
        assert!(major >= 25, "{v}");
        assert_eq!(v["Version"], ENGINE_VERSION);
        assert_eq!(v["Components"][0]["Name"], "Engine");
        assert_eq!(v["Components"][0]["Version"], ENGINE_VERSION);
        assert_eq!(v["KiriVersion"], env!("CARGO_PKG_VERSION"));
        assert!(v["Components"].as_array().unwrap().iter().any(|c| c["Name"] == "Kiri" && c["Version"] == env!("CARGO_PKG_VERSION")));
        // The docker CLI reads every component's Details as strings.
        for c in v["Components"].as_array().unwrap() {
            assert!(c["Details"].as_object().unwrap().values().all(Value::is_string), "{c}");
        }
        assert_eq!(crate::client::kiri_version(&v), Some(env!("CARGO_PKG_VERSION")));
        // An engine from before KiriVersion: its Version was Kiri's.
        assert_eq!(crate::client::kiri_version(&json!({"Version": "0.2.0"})), Some("0.2.0"));
    }

    /// Docker's merge of a create's Entrypoint and Cmd with the image's.
    #[test]
    fn create_command_follows_docker() {
        let s = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let (ie, ic) = (s(&["/entry"]), s(&["/bin/ryuk"]));
        // Testcontainers (Java, .NET): "Cmd": [] means the image's Cmd.
        assert_eq!(command(None, Some(vec![]), &ie, &ic).ok(), Some((ie.clone(), ic.clone())));
        assert_eq!(command(None, None, &ie, &ic).ok(), Some((ie.clone(), ic.clone())));
        assert_eq!(command(None, Some(s(&["x"])), &ie, &ic).ok(), Some((ie.clone(), s(&["x"]))));
        // A non-empty Entrypoint drops the image's Cmd.
        assert_eq!(command(Some(s(&["sh"])), None, &ie, &ic).ok(), Some((s(&["sh"]), vec![])));
        assert_eq!(command(Some(s(&["sh"])), Some(vec![]), &ie, &ic).ok(), Some((s(&["sh"]), vec![])));
        assert_eq!(command(Some(s(&["sh"])), Some(s(&["-c", "x"])), &ie, &ic).ok(), Some((s(&["sh"]), s(&["-c", "x"]))));
        // "Entrypoint": [] clears the image's and keeps its Cmd.
        assert_eq!(command(Some(vec![]), None, &ie, &ic).ok(), Some((vec![], ic.clone())));
        // `--entrypoint=` sends [""]: no entrypoint, and the image's Cmd is dropped too.
        assert_eq!(command(Some(s(&[""])), Some(s(&["/bin/sh", "-c", "echo ok"])), &ie, &ic).ok(), Some((vec![], s(&["/bin/sh", "-c", "echo ok"]))));
        assert!(matches!(command(Some(s(&[""])), None, &ie, &ic), Err(Error::BadRequest(m)) if m == "no command specified"));
        assert!(matches!(command(Some(s(&[""])), Some(vec![]), &ie, &ic), Err(Error::BadRequest(_))));
    }

    // #461: the CLI sends `docker import - my/rootfs:1` as `repo=my/rootfs:1`.
    #[test]
    fn import_names_take_the_tag_from_repo_or_tag() {
        let name = |repo, tag| import_name(repo, tag).ok().flatten();
        assert_eq!(name("my/rootfs:1", ""), Some("my/rootfs:1".into()));
        assert_eq!(name("my/rootfs", "1"), Some("my/rootfs:1".into()));
        assert_eq!(name("rootfs", ""), Some("rootfs:latest".into()));
        assert_eq!(name("", ""), None);
        for bad in ["My/Rootfs", "my/rootfs@sha256:0000000000000000000000000000000000000000000000000000000000000000", "a b"] {
            assert!(matches!(import_name(bad, ""), Err(Error::BadRequest(_))), "{bad}");
        }
    }

    #[test]
    fn import_takes_only_tar_archives() {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_ustar();
        h.set_path("etc/hostname").unwrap();
        h.set_size(4);
        h.set_cksum();
        b.append(&h, &b"kiri"[..]).unwrap();
        let archive = b.into_inner().unwrap();
        assert!(tar_header(&archive[..512]));
        // An empty archive is its end blocks.
        assert!(tar_header(&[0; 512]));
        let mut broken = archive[..512].to_vec();
        broken[0] ^= 1;
        assert!(!tar_header(&broken));
        assert!(!tar_header(b"not a tar archive"));
        assert!(!tar_header(&[b'x'; 512]));
    }

    #[test]
    fn endpoints_alone_choose_the_primary_network() {
        let asked = std::cell::RefCell::new(Vec::new());
        let resolve = |key: &str| {
            asked.borrow_mut().push(key.to_owned());
            match key {
                "a" | "b" | "n" | "bridge" => Some((key.to_owned(), false)),
                // Internal networks (`network create --internal`).
                "back" | "inner" => Some((key.to_owned(), true)),
                "0123abc" => Some(("n".to_owned(), false)),
                "4567def" => Some(("bridge".to_owned(), false)),
                _ => None,
            }
        };
        let cases: &[(&[&str], Option<&str>)] = &[
            (&[], None),
            (&["n"], Some("n")),
            (&["0123abc"], Some("n")),
            (&["a", "b"], Some("a")),
            (&["bridge"], None),
            (&["n", "bridge"], None),
            (&["bridge", "n"], None),
            (&["4567def"], None),
            (&["n", "4567def"], None),
            (&["container:x"], None),
            (&["container:x", "n"], Some("n")),
            (&["n", "container:x"], Some("n")),
            (&["missing"], None),
            (&["missing", "n"], Some("n")),
            // An internal network sorted first doesn't take the default
            // route from one that reaches outside.
            (&["back", "n"], Some("n")),
            (&["back", "inner", "n"], Some("n")),
            (&["back"], Some("back")),
            (&["back", "inner"], Some("back")),
            (&["back", "bridge"], None),
        ];
        for (keys, expected) in cases {
            assert_eq!(endpoints_primary(keys.iter().copied(), resolve).as_deref(), *expected, "{keys:?}");
        }
        assert!(asked.borrow().iter().all(|k| !k.starts_with("container:")), "a container: key was looked up as a network");
    }

    #[test]
    fn archive_uploads_are_sniffed() {
        assert_eq!(upload_compression(&[0x1f, 0x8b, 8, 0, 0, 0]), Upload::Gzip);
        assert_eq!(upload_compression(b"BZh91A"), Upload::Unsupported("bzip2"));
        assert_eq!(upload_compression(&[0xfd, b'7', b'z', b'X', b'Z', 0]), Upload::Unsupported("xz"));
        assert_eq!(upload_compression(&[0x28, 0xb5, 0x2f, 0xfd, 0, 0]), Upload::Zstd);
        assert_eq!(upload_compression(b"etc/ho"), Upload::Tar);
        assert_eq!(upload_compression(b""), Upload::Tar);
    }

    /// A zstd upload comes out as the tar it holds: one frame, several
    /// frames one after the other (`pzstd`), wherever the bytes already
    /// read (enough to know it by) end and the rest begins.
    #[test]
    fn zstd_uploads_are_decompressed_frame_after_frame() {
        use std::io::Read;
        let zstd = |data: &[u8]| ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest);
        let first: Vec<u8> = (0..200_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let second = b"the second frame".to_vec();
        let one = zstd(&first);
        let two = [one.clone(), zstd(&second)].concat();
        for (what, sent, want) in [("one frame", &one, first.clone()), ("two frames", &two, [first.clone(), second].concat())] {
            for split in [4, 6, sent.len()] {
                let mut got = Vec::new();
                upload_reader(sent[..split].to_vec(), &sent[split..]).unwrap().read_to_end(&mut got).unwrap();
                assert!(got == want, "{what}, split at {split}");
            }
        }
        // Cut short, and not zstd after its first four bytes: the client's
        // input, a 400 that says so.
        for (what, sent) in [("cut short", &one[..one.len() / 2]), ("damaged", &[&one[..4], &[0xffu8; 64][..]].concat()[..])] {
            let mut got = Vec::new();
            let e = upload_reader(sent[..6].to_vec(), &sent[6..]).unwrap().read_to_end(&mut got).expect_err(what);
            assert!(matches!(upload_error(false, e), Error::BadRequest(m) if m.starts_with("the zstd archive is damaged")), "{what}");
        }
    }

    /// DDEV's CopyIntoVolume sends a gzipped tar: the guest gets the tar.
    #[test]
    fn gzip_uploads_reach_the_guest_as_tar() {
        use std::io::{Read, Write};
        let mut t = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_ustar();
        h.set_size(5);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        t.append_data(&mut h, "traefik/static_config.yaml", &b"hello"[..]).unwrap();
        let plain = t.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&plain).unwrap();
        let gz = gz.finish().unwrap();
        for upload in [&gz, &plain] {
            let (head, rest) = upload.split_at(6);
            let mut got = Vec::new();
            upload_reader(head.to_vec(), rest).ok().unwrap().read_to_end(&mut got).unwrap();
            assert_eq!(got, plain);
        }
        assert!(matches!(upload_reader(b"BZh91A".to_vec(), &b""[..]), Err(Error::BadRequest(m)) if m.contains("bzip2")));
    }

    /// A request that asks for the close (`Connection: close`, or HTTP/1.0
    /// without keep-alive) is answered and then the client reads the end of
    /// the stream, as Docker.DotNet waits to; one that does not keeps the
    /// connection for the next request.
    #[test]
    fn the_connection_closes_when_the_request_asks() {
        use std::io::{Read, Write};
        let (root, engine) = engine_in("conn-close", false);
        // What the client reads within two seconds, and whether that ended
        // with the stream's end.
        let exchange = |requests: &str| {
            let (mut client, server) = UnixStream::pair().unwrap();
            let served = std::thread::spawn({
                let engine = engine.clone();
                move || serve(server, engine, Caller::Host)
            });
            client.write_all(requests.as_bytes()).unwrap();
            client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let (mut got, mut buf) = (Vec::new(), [0u8; 4096]);
            let ended = loop {
                match client.read(&mut buf) {
                    Ok(0) => break true,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(_) => break false,
                }
            };
            drop(client);
            served.join().unwrap().unwrap();
            (String::from_utf8_lossy(&got).into_owned(), ended)
        };
        let (got, ended) = exchange("GET /v1.44/_ping HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n");
        assert!(ended && got.starts_with("HTTP/1.1 200 ") && got.contains("Connection: close\r\n") && got.ends_with("OK"), "{got}");
        let (got, ended) = exchange("GET /_ping HTTP/1.0\r\n\r\n");
        assert!(ended && got.starts_with("HTTP/1.1 200 ") && got.contains("Connection: close\r\n"), "{got}");
        // An error answer (with a body) closes the same way.
        let (got, ended) = exchange("POST /v1.44/containers/nothing-here/start HTTP/1.1\r\nHost: d\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
        assert!(ended && got.starts_with("HTTP/1.1 404 ") && got.contains("Connection: close\r\n"), "{got}");
        // Kept alive: both answered, neither says close, and the connection waits for a third.
        let (got, ended) = exchange("GET /_ping HTTP/1.1\r\nHost: d\r\n\r\nGET /_ping HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
        assert!(!ended && got.matches("HTTP/1.1 200 ").count() == 2 && !got.contains("Connection:"), "{got}");
        // A close asked for on the second request ends it after that one.
        let (got, ended) = exchange("GET /_ping HTTP/1.1\r\nHost: d\r\n\r\nGET /_ping HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n");
        assert!(ended && got.matches("HTTP/1.1 200 ").count() == 2 && got.matches("Connection: close\r\n").count() == 1, "{got}");
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// The `(method, path)` routes a `route` function's body matches, read
    /// from its source: single or `|`-separated methods, a literal
    /// `/kiri…` path or a `p.strip_prefix("/kiri/…")` guard (with an
    /// optional `strip_suffix`). Any other line naming a `/kiri` path (an
    /// arm split over lines, a method in a variable) fails the test rather
    /// than going unseen; a handler repeating its arm's `strip_prefix` is
    /// fine.
    fn kiri_routes(body: &str) -> Vec<(String, String)> {
        let mut routes: Vec<(String, String)> = Vec::new();
        let mut prefixes: Vec<String> = Vec::new();
        for line in body.lines().map(str::trim).filter(|l| !l.starts_with("//")) {
            let names_kiri = line.contains("\"/kiri");
            let arm = line.strip_prefix('(').filter(|r| r.starts_with('"')).and_then(|r| {
                let (methods, rest) = r.split_once(", ")?;
                let methods: Option<Vec<&str>> = methods.split('|').map(|m| m.trim().strip_prefix('"')?.strip_suffix('"')).collect();
                Some((methods?, rest))
            });
            let path = arm.as_ref().and_then(|(_, rest)| {
                if let Some(lit) = rest.strip_prefix("\"/kiri") {
                    Some(format!("/kiri{}", lit.split('"').next()?))
                } else if let Some(i) = rest.find("strip_prefix(\"/kiri/") {
                    let prefix = rest[i + "strip_prefix(\"".len()..].split('"').next()?;
                    let suffix = rest.split("strip_suffix(\"").nth(1).map_or("", |s| s.split('"').next().unwrap_or_default());
                    Some(format!("{prefix}1{suffix}"))
                } else {
                    None
                }
            });
            match (arm, path) {
                (Some((methods, _)), Some(path)) => {
                    if let Some(i) = line.find("strip_prefix(\"/kiri/") {
                        prefixes.push(line[i..].split('"').nth(1).unwrap_or_default().to_owned());
                    }
                    routes.extend(methods.iter().map(|m| ((*m).to_owned(), path.clone())));
                }
                _ if names_kiri => {
                    let repeats = line.find("strip_prefix(\"/kiri/").is_some_and(|i| prefixes.iter().any(|p| line[i..].split('"').nth(1) == Some(p.as_str())));
                    assert!(repeats, "a /kiri path the route reader cannot read: {line}");
                }
                _ => {}
            }
        }
        routes
    }

    /// Every one of Kiri's own endpoints that changes something is refused
    /// on a container's connection, unless listed in `CONTAINER_MAY`; GETs
    /// and HEADs stay open. The routes are read from `route` itself and
    /// from `ops::route` (which it asks first), so one added later is
    /// covered too.
    #[test]
    fn a_container_changes_nothing_through_kiris_own_endpoints() {
        let body_of = |src: &'static str, start: &str| {
            let body = &src[src.find(start).unwrap()..];
            &body[..body.find("\n}\n").unwrap()]
        };
        let mut routes = kiri_routes(body_of(include_str!("api.rs"), "\nfn route("));
        routes.extend(kiri_routes(body_of(include_str!("ops.rs"), "\npub fn route(")));
        for known in [("POST", "/kiri/stop"), ("POST", "/kiri/sleep"), ("POST", "/kiri/registries"), ("PUT", "/kiri/proxy/system"), ("POST", "/kiri/builder"), ("POST", "/kiri/containers/1/ports"), ("DELETE", "/kiri/k8s/forward/1")] {
            assert!(routes.iter().any(|(m, p)| (m.as_str(), p.as_str()) == known), "{known:?} not found in route(): {routes:?}");
        }
        for (m, p) in &routes {
            let open = matches!(m.as_str(), "GET" | "HEAD") || CONTAINER_MAY.contains(&(m.as_str(), p.as_str()));
            assert_eq!(container_may(m, p), open, "{m} {p}");
        }
        assert!(!container_may("PATCH", "/kiri/anything/new"));
        assert!(!container_may("POST", "/kiri"));
        assert!(container_may("POST", "/containers/create") && container_may("GET", "/kiri/registries") && container_may("HEAD", "/kiri/registries"));
        // The reader itself: arms of several methods are read as each, and
        // a /kiri path it cannot read stops the test.
        let read = kiri_routes("        (\"POST\" | \"DELETE\", \"/kiri/x\") => {}\n        (\"GET\" | \"HEAD\", p) if p.strip_prefix(\"/kiri/y/\").is_some() => {\n            go(p.strip_prefix(\"/kiri/y/\"));\n        }");
        assert_eq!(read, [("POST".to_owned(), "/kiri/x".to_owned()), ("DELETE".into(), "/kiri/x".into()), ("GET".into(), "/kiri/y/1".into()), ("HEAD".into(), "/kiri/y/1".into())]);
        for unreadable in ["        (m, \"/kiri/z\") => {}", "        (\"POST\",\n            \"/kiri/z\") => {}"] {
            assert!(std::panic::catch_unwind(|| kiri_routes(unreadable)).is_err(), "{unreadable}");
        }
    }

    /// Through the API itself: a container's Stop and settings are refused,
    /// and the engine stays as it was.
    #[test]
    fn a_containers_stop_and_settings_are_refused() {
        use std::io::{Read, Write};
        let (root, engine) = engine_in("container-gate", false);
        for req in ["POST /v1.47/kiri/stop", "POST /kiri/sleep", "POST /kiri/registries", "POST /kiri/proxy", "POST /kiri/resources", "POST /kiri/domains", "DELETE /kiri/domains"] {
            let (mut client, server) = UnixStream::pair().unwrap();
            let served = std::thread::spawn({
                let engine = engine.clone();
                move || serve(server, engine, Caller::Container)
            });
            client.write_all(format!("{req} HTTP/1.1\r\nHost: d\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{{}}").as_bytes()).unwrap();
            let mut got = String::new();
            client.read_to_string(&mut got).unwrap();
            served.join().unwrap().unwrap();
            assert!(got.starts_with("HTTP/1.1 403 "), "{req}: {got}");
        }
        // Refused, the connection ends after the answer even when the
        // client asked to keep it: the body is not read as a request.
        let (mut client, server) = UnixStream::pair().unwrap();
        let served = std::thread::spawn({
            let engine = engine.clone();
            move || serve(server, engine, Caller::Container)
        });
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client.write_all(b"POST /kiri/registries HTTP/1.1\r\nHost: d\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        let mut got = String::new();
        client.read_to_string(&mut got).unwrap();
        served.join().unwrap().unwrap();
        assert!(got.starts_with("HTTP/1.1 403 "), "{got}");
        // What a container may do still answers: the registry lists are
        // readable from one.
        let (mut client, server) = UnixStream::pair().unwrap();
        let served = std::thread::spawn({
            let engine = engine.clone();
            move || serve(server, engine, Caller::Container)
        });
        client.write_all(b"GET /kiri/registries HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n").unwrap();
        let mut got = String::new();
        client.read_to_string(&mut got).unwrap();
        served.join().unwrap().unwrap();
        assert!(got.starts_with("HTTP/1.1 200 "), "{got}");
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `POST /kiri/registries` refreshes synchronously on the request thread
    /// only when the save changed settings, never from a detached thread after
    /// answering.
    #[test]
    fn set_registries_refreshes_synchronously_without_a_detached_thread() {
        let req = Request::internal("POST", "/kiri/registries", &[], b"{}".to_vec());
        let caller = std::thread::current().id();
        let ran = std::sync::Arc::new(std::sync::Mutex::new(None));
        let ran2 = ran.clone();
        set_registries_with(&req, |_| Ok((json!({}), true)), move || {
            *ran2.lock().unwrap() = Some(std::thread::current().id());
        })
        .unwrap();
        assert_eq!(*ran.lock().unwrap(), Some(caller), "refresh must finish on the caller thread before answering");

        let ran = std::sync::Arc::new(std::sync::Mutex::new(false));
        let ran2 = ran.clone();
        set_registries_with(&req, |_| Ok((json!({}), false)), move || {
            *ran2.lock().unwrap() = true;
        })
        .unwrap();
        assert!(!*ran.lock().unwrap(), "unchanged save must not refresh");
    }

    /// A pull sent with a body, as docker-modem (so Testcontainers for Node)
    /// sends its options as JSON, doesn't leave that body to be read as the
    /// start of the next request on the connection: it used to answer
    /// `Kiri does not support {"fromImage":...}GET /containers/json yet`.
    /// A body too large to read out closes the connection after the answer.
    #[test]
    fn a_body_the_handler_leaves_unread_is_not_the_next_request() {
        use std::io::{Read, Write};
        let (root, engine) = engine_in("unread-body", false);
        let exchange = |requests: &[u8]| {
            let (mut client, server) = UnixStream::pair().unwrap();
            let served = std::thread::spawn({
                let engine = engine.clone();
                move || serve(server, engine, Caller::Host)
            });
            client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            // Written from a thread: a body larger than the socket buffer
            // would otherwise stop the test before it reads anything.
            let sent = std::thread::spawn({
                let (mut client, requests) = (client.try_clone().unwrap(), requests.to_vec());
                move || client.write_all(&requests)
            });
            let (mut got, mut buf) = (Vec::new(), [0u8; 4096]);
            let ended = loop {
                match client.read(&mut buf) {
                    Ok(0) => break true,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(_) => break false,
                }
            };
            drop(client);
            let sent = sent.join().unwrap().is_ok();
            served.join().unwrap().unwrap();
            (String::from_utf8_lossy(&got).into_owned(), ended, sent)
        };
        // No image name, so the pull is refused before it reaches a registry.
        let json = r#"{"fromImage":"","tag":"1.36.1"}"#;
        let ping = "GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n";
        let sized = format!("POST /v1.44/images/create?fromImage=&tag=1.36.1 HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}{ping}", json.len());
        let chunked = format!("POST /images/create?fromImage= HTTP/1.1\r\nHost: d\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{json}\r\n0\r\n\r\n{ping}", json.len());
        for requests in [sized, chunked] {
            let (got, ended, _) = exchange(requests.as_bytes());
            assert!(!ended && got.starts_with("HTTP/1.1 400 ") && got.contains("HTTP/1.1 200 OK") && !got.contains("does not support"), "{got}");
        }
        // Past what is read out (256 KB, as Go's server reads): answered, then
        // closed cleanly. The client's read ends, not "connection reset" (what
        // Linux gives when a socket closes with unread data in it), and a
        // client still sending its body is not cut off with a broken pipe.
        for size in [300 << 10, 8 << 20] {
            let mut requests = format!("POST /images/create?fromImage= HTTP/1.1\r\nHost: d\r\nContent-Length: {size}\r\n\r\n").into_bytes();
            requests.resize(requests.len() + size, b' ');
            requests.extend_from_slice(ping.as_bytes());
            let (got, ended, sent) = exchange(&requests);
            assert!(ended && sent && got.starts_with("HTTP/1.1 400 ") && got.matches("HTTP/1.1 ").count() == 1, "{size}: ended {ended}, sent {sent}: {got}");
        }
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A body past what a request read whole may send (8 MiB) gets a real
    /// answer, 413 in Docker's error shape, and a clean end: before, the
    /// connection was dropped unanswered and the client, still sending, got
    /// a broken pipe. Content-Length and chunked; a client that never stops
    /// sending is cut off after the drain's bounds.
    #[test]
    fn a_body_too_large_to_hold_is_answered_and_the_connection_ends() {
        use std::io::{Read, Write};
        let (root, engine) = engine_in("too-large-body", false);
        let ping = "GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n";
        // What the client got, whether its read ended cleanly, whether its
        // write went through, and how long the server took to let go.
        type Writes = Box<dyn FnOnce(&mut UnixStream) -> std::io::Result<()> + Send>;
        let exchange = |send: Writes| {
            let (mut client, server) = UnixStream::pair().unwrap();
            let t = std::time::Instant::now();
            let served = std::thread::spawn({
                let engine = engine.clone();
                move || (serve(server, engine, Caller::Host), t.elapsed())
            });
            client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let sent = std::thread::spawn({
                let mut client = client.try_clone().unwrap();
                move || send(&mut client).is_ok()
            });
            let (mut got, mut buf) = (Vec::new(), [0u8; 4096]);
            let ended = loop {
                match client.read(&mut buf) {
                    Ok(0) => break true,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(_) => break false,
                }
            };
            drop(client);
            let sent = sent.join().unwrap();
            let (result, took) = served.join().unwrap();
            result.unwrap();
            (String::from_utf8_lossy(&got).into_owned(), ended, sent, took)
        };
        let answered = |got: &str| got.starts_with("HTTP/1.1 413 ") && got.contains("Connection: close\r\n") && got.contains("request body too large") && got.matches("HTTP/1.1 ").count() == 1;
        let size = 9usize << 20;
        let sized = Box::new(move |c: &mut UnixStream| {
            write!(c, "POST /v1.44/containers/no-such-container/kill HTTP/1.1\r\nHost: d\r\nContent-Length: {size}\r\n\r\n")?;
            c.write_all(&vec![b'x'; size])?;
            c.write_all(ping.as_bytes())
        });
        let chunked = Box::new(move |c: &mut UnixStream| {
            write!(c, "POST /no/such/endpoint HTTP/1.1\r\nHost: d\r\nTransfer-Encoding: chunked\r\n\r\n")?;
            let chunk = vec![b'x'; 1 << 20];
            for _ in 0..9 {
                write!(c, "{:x}\r\n", chunk.len())?;
                c.write_all(&chunk)?;
                c.write_all(b"\r\n")?;
            }
            write!(c, "0\r\n\r\n{ping}")
        });
        for (what, send) in [("sized", sized as Writes), ("chunked", chunked)] {
            let (got, ended, sent, _) = exchange(send);
            assert!(answered(&got) && ended && sent, "{what}: ended {ended}, sent {sent}: {got}");
        }
        // Never stops sending: answered all the same, and the connection is
        // closed once the drain's bounds (16 MiB, 1 s) are reached.
        let (got, _, sent, took) = exchange(Box::new(|c: &mut UnixStream| {
            write!(c, "POST /containers/x/kill HTTP/1.1\r\nHost: d\r\nContent-Length: {}\r\n\r\n", 1u64 << 40)?;
            let block = vec![b'x'; 64 << 10];
            loop {
                c.write_all(&block)?;
            }
        }));
        assert!(answered(&got) && !sent && took < Duration::from_secs(4), "endless: sent {sent}, took {took:?}: {got}");
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `docker save | gzip | docker load` loads, as on Docker (issue 528):
    /// the body goes through the same detection as `docker cp` uploads.
    /// Each archive is sent to `POST /images/load` as the CLI sends it.
    #[test]
    fn image_load_takes_gzip_archives() {
        use std::io::{Read, Write};
        let (src, engine) = engine_in("load-gzip-src", false);
        let put = |bytes: &[u8]| {
            let mut w = engine.store.ingest().unwrap();
            w.write_all(bytes).unwrap();
            w.finish().unwrap()
        };
        let config = json!({"os": "linux", "architecture": "amd64", "created": "2026-01-01T00:00:00Z", "config": {"Cmd": ["echo", "hi"]}});
        let (cfg, _) = put(&serde_json::to_vec(&config).unwrap());
        let (layer, size) = put(b"\x1f\x8bnot really gzip");
        let layers = vec![kiri_image::Layer { digest: layer, size, media_type: "application/vnd.docker.image.rootfs.diff.tar.gzip".into() }];
        let image = kiri_image::archive::from_config("demo:gz".into(), &cfg, &config, layers);
        let mut plain = Vec::new();
        kiri_image::archive::save(&engine.store, &[image], &mut plain).unwrap();
        drop(engine);
        let _ = std::fs::remove_dir_all(src);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&plain).unwrap();
        let gz = gz.finish().unwrap();

        let load = |name: &str, body: &[u8]| {
            let (root, engine) = engine_in(name, false);
            let (mut client, server) = UnixStream::pair().unwrap();
            let served = std::thread::spawn({
                let engine = engine.clone();
                move || serve(server, engine, Caller::Host)
            });
            write!(client, "POST /images/load?quiet=0 HTTP/1.1\r\nHost: docker\r\nContent-Type: application/x-tar\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
            client.write_all(body).unwrap();
            let mut answer = String::new();
            client.read_to_string(&mut answer).unwrap();
            served.join().unwrap().unwrap();
            let found = engine.store.find("demo:gz").unwrap().map(|i| i.config_digest);
            drop(engine);
            let _ = std::fs::remove_dir_all(root);
            (answer, found)
        };
        for (name, body) in [("load-gzip", &gz), ("load-plain", &plain)] {
            let (answer, found) = load(name, body);
            assert!(answer.starts_with("HTTP/1.1 200"), "{name}: {answer}");
            assert!(answer.contains("Loaded image: demo:gz"), "{name}: {answer}");
            assert_eq!(found.as_deref(), Some(cfg.as_str()), "{name}");
        }
        // Formats Docker takes that Kiri doesn't yet say so, not "cksum",
        // and the client gets that answer after sending all of a big body.
        let mut bzip2 = b"BZh91AY&SY".to_vec();
        bzip2.resize(4 << 20, 0x5a);
        let (answer, found) = load("load-bzip2", &bzip2);
        assert!(answer.starts_with("HTTP/1.1 400") && answer.contains("bzip2-compressed archive isn't supported yet"), "{answer}");
        assert_eq!(found, None);
    }

    /// Every damaged gzip is a 400, including the ones Docker 29 accepts
    /// with 200 because it never reads the trailer: a wrong CRC or length,
    /// no trailer at all, and a byte flipped mid-stream (Docker writes that
    /// file corrupted). Truncated is a 400 here too (500 on Docker).
    #[test]
    fn damaged_gzip_uploads_are_bad_requests() {
        use std::io::Write;
        // Half incompressible, so a flipped byte lands in literal data and
        // the stream still decodes, to the wrong bytes.
        let mut data: Vec<u8> = (0..1u32 << 20).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        data.extend(std::iter::repeat_n(b'x', 1 << 20));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&data).unwrap();
        let good = gz.finish().unwrap();
        let n = good.len();
        let flip = |at: usize| {
            let mut d = good.clone();
            d[at] ^= 0xff;
            d
        };
        let cases = [
            ("truncated", good[..n / 2].to_vec()),
            ("wrong CRC", flip(n - 8)),
            ("wrong length", flip(n - 1)),
            ("no trailer", good[..n - 8].to_vec()),
            ("flipped mid-stream", flip(n / 4)),
        ];
        for (what, upload) in cases {
            let (head, rest) = upload.split_at(6);
            let mut archive = upload_reader(head.to_vec(), rest).ok().unwrap();
            let e = send_chunks(&mut archive, &mut io::sink()).expect_err(what);
            assert!(matches!(upload_error(true, e), Error::BadRequest(m) if m.starts_with("the gzip archive is damaged")), "{what}");
        }
        // The undamaged one reads through.
        let (head, rest) = good.split_at(6);
        send_chunks(&mut upload_reader(head.to_vec(), rest).ok().unwrap(), &mut io::sink()).unwrap();
        // A plain body that breaks off stays the server-side error it was.
        assert!(matches!(upload_error(false, io::Error::other("cut")), Error::Io(_)));
    }

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
        assert_eq!(amd64_only_refusal("x", &p(&["linux/arm64", "linux/amd64/v3"])), None);
        let why = amd64_only_refusal("arm-only:1", &p(&["linux/arm64", "linux/arm/v7"])).unwrap();
        assert!(why.starts_with("arm-only:1 is built for linux/arm64, linux/arm/v7, and this Intel Mac runs linux/amd64"), "{why}");
        // Everywhere but an Intel Mac this never refuses.
        if !cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            assert_eq!(intel_mac_refusal("arm-only", &p(&["linux/arm64"])), None);
        }
    }

    #[test]
    fn mac_shares_and_vm_paths() {
        for p in ["/Users", "/Users/me/src", "/Volumes/ext", "/private/tmp/x", "/private/etc/hosts", "/tmp", "/tmp/kind", "/var/folders/ab/T"] {
            assert!(mac_shared(p), "{p} should come from the Mac");
        }
        for p in ["/lib/modules", "/etc/hostname", "/var/lib/x", "/var", "/run/foo", "/Users2", "/tmpx", "/privatefoo", "/var/folders2", "/"] {
            assert!(!mac_shared(p), "{p} should be the VM's");
        }
    }

    /// Bind sources are followed through the Mac's links before the guest
    /// gets them, and the Mac's /etc files are the Mac's (issue 170).
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_sources_follow_links_and_etc_is_the_macs() {
        // /etc -> private/etc: -v /etc/hosts:/etc/hosts is the Mac's hosts file.
        assert_eq!(mac_source("/etc/hosts").as_deref(), Some("/private/etc/hosts"));
        assert_eq!(guest_bind_source("/etc/hosts"), "/mnt/mac/private/etc/hosts");
        // /etc/localtime links to an absolute path, which the guest could not follow.
        let tz = mac_source("/etc/localtime").expect("the Mac has a time zone");
        assert!(tz.starts_with("/private/var/db/timezone/"), "{tz}");
        assert_eq!(mac_source("/private/etc/localtime").as_deref(), Some(tz.as_str()));
        // What the Mac lacks, and paths outside the shares, stay the VM's.
        for p in ["/etc/fstab", "/etc/kiri-no-such-file", "/lib/modules", "/var/lib/docker", "/var/run", "/etcetera"] {
            assert_eq!(mac_source(p), None, "{p} should be the VM's");
            assert_eq!(guest_bind_source(p), p);
        }
        // A link inside a share to another place in the shares is followed;
        // a missing path under a share is passed as given.
        let dir = std::env::temp_dir().join(format!("kiri-mac-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let real = std::fs::canonicalize(dir.join("real")).unwrap();
        assert_eq!(mac_source(dir.join("link").to_str().unwrap()).as_deref(), real.to_str());
        assert_eq!(mac_source("/tmp/kiri-no-such-path").as_deref(), Some("/tmp/kiri-no-such-path"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// On a Linux host a bind of the engine's socket made before Kiri gave
    /// containers a socket of their own gets it at the next start; other
    /// binds are mounted as they were made.
    #[test]
    fn a_socket_bind_made_earlier_is_given_the_container_socket_at_start() {
        let guest = vec!["/run/kiri/docker.sock\t/var/run/docker.sock\t".to_owned(), "/srv/app\t/x\tro".to_owned(), "tmpfs\t/t\ttmpfs".to_owned()];
        let info = vec![json!({"Type": "bind", "Source": "/var/run/docker.sock"}), json!({"Type": "bind", "Source": "/srv/app"}), json!({"Type": "tmpfs", "Source": ""})];
        let sock = |s: &str| matches!(s, "/var/run/docker.sock" | "/run/kiri/docker.sock").then(|| "/run/kiri/docker.container.sock".to_owned());
        let given = ["/run/kiri/docker.container.sock\t/var/run/docker.sock\t", "/srv/app\t/x\tro", "tmpfs\t/t\ttmpfs"];
        assert_eq!(fresh_bind_sources_with(&guest, &info, sock, false), given);
        // The path the person gave no longer leads to the socket (a link
        // removed or re-pointed): the path mounted when it was made does.
        let moved = vec![json!({"Type": "bind", "Source": "/home/me/sock-link"}), info[1].clone(), info[2].clone()];
        assert_eq!(fresh_bind_sources_with(&guest, &moved, sock, false), given);
        // Records that don't pair one to one with the mount lines: each
        // line is still looked at by the path it mounts.
        assert_eq!(fresh_bind_sources_with(&guest, &info[1..], sock, false), given);
        assert_eq!(fresh_bind_sources_with(&guest, &[], sock, false), given);
        // A Mac follows binds from the records only, so it leaves unpaired
        // lines as they are.
        assert_eq!(fresh_bind_sources_with(&guest, &info[1..], sock, true), guest);
    }

    /// A linked bind source is followed again at each start: re-pointing the
    /// link (a `current` release link) mounts the new target.
    #[cfg(target_os = "macos")]
    #[test]
    fn bind_links_are_followed_again_at_start() {
        let dir = std::env::temp_dir().join(format!("kiri-fresh-binds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for r in ["r1", "r2"] {
            std::fs::create_dir_all(dir.join(r)).unwrap();
        }
        let link = dir.join("current");
        std::os::unix::fs::symlink("r1", &link).unwrap();
        let src = link.to_str().unwrap().to_owned();
        let at = |r: &str| format!("/mnt/mac{}\t/app\t", std::fs::canonicalize(dir.join(r)).unwrap().display());
        let made = format!("{}\t/app\t", guest_bind_source(&src));
        assert_eq!(made, at("r1"));
        let guest = vec![made, "tmpfs\t/t\tnoexec".to_owned()];
        let info = vec![json!({"Type": "bind", "Source": src}), json!({"Type": "tmpfs", "Source": ""})];
        // Unchanged link: the same lines.
        assert_eq!(fresh_bind_sources(&guest, &info), guest);
        // Re-pointed: the new target; other mounts as they were.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("r2", &link).unwrap();
        assert_eq!(fresh_bind_sources(&guest, &info), [at("r2"), guest[1].clone()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn signals_like_docker() {
        for (raw, n) in [
            ("KILL", 9), ("SIGKILL", 9), ("term", 15), ("SigTerm", 15), ("ALRM", 14), ("PWR", 30), ("SIGPWR", 30), ("TSTP", 20),
            ("PIPE", 13), ("CHLD", 17), ("CLD", 17), ("IOT", 6), ("WINCH", 28), ("9", 9), ("15", 15), ("64", 64),
            ("RTMIN", 34), ("SIGRTMIN+3", 37), ("RTMIN+15", 49), ("RTMAX", 64), ("RTMAX-14", 50), ("rtmax-1", 63),
        ] {
            assert_eq!(parse_signal(raw).ok(), Some(n), "{raw}");
        }
        for raw in ["FOO", "SIG", "", "RTMIN+16", "RTMAX-15", "RTMIN-1", "SIGSIGKILL"] {
            assert!(matches!(parse_signal(raw), Err(Error::BadRequest(m)) if m == format!("Invalid signal: {raw}")), "{raw}");
        }
        assert!(matches!(parse_signal("0"), Err(Error::BadRequest(m)) if m == "Invalid signal: 0"));
        for raw in ["32", "65", "-9"] {
            assert!(matches!(parse_signal(raw), Err(Error::BadRequest(m)) if m.starts_with("the linux daemon does not support signal")), "{raw}");
        }
        // A stop signal Docker would not parse stops with SIGTERM.
        assert_eq!(signal("BOGUS"), 15);
    }

    /// The image list's filters as Docker's: an untagged image is listed
    /// with no names (`<none>:<none>` in the CLI), `dangling` picks it or
    /// leaves it out, and `before`, `since` and `until` work by creation time.
    #[test]
    fn image_list_filters_follow_docker() {
        let root = std::env::temp_dir().join(format!("kiri-image-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let engine = super::super::engine::boot_without_vm(&root);
        let mut untagged = String::new();
        for (tag, created, labels) in [(Some("demo:new"), "2026-11-16T00:00:00Z", json!({"keep": "1"})), (None, "2020-01-01T00:00:00Z", json!({"keep": "1"})), (Some("demo:old"), "2019-01-01T00:00:00Z", Value::Null)] {
            let cfg = json!({"created": created, "architecture": "arm64", "os": "linux", "config": {"Labels": labels}});
            let mut w = engine.store.ingest().unwrap();
            std::io::Write::write_all(&mut w, &serde_json::to_vec(&cfg).unwrap()).unwrap();
            let (digest, _) = w.finish().unwrap();
            if tag.is_none() {
                untagged = digest.clone();
            }
            engine.store.put_image(kiri_image::archive::from_config(tag.map_or(digest.clone(), str::to_owned), &digest, &cfg, Vec::new())).unwrap();
        }
        let list = |f: &str, v: Option<(u32, u32)>| {
            let mut r = Request::internal("GET", "/images/json", &[("filters", f)], Vec::new());
            r.api_version = v;
            super::images(&r, &engine)
        };
        // Rows by name, the untagged one as its ID.
        let rows = |f: &str| -> Vec<String> {
            list(f, None).unwrap().as_array().unwrap().iter().map(|r| r["RepoTags"][0].as_str().map_or_else(|| r["Id"].as_str().unwrap().to_owned(), str::to_owned)).collect()
        };
        let u = untagged.as_str();
        assert_eq!(rows(""), ["demo:new", u, "demo:old"]);
        let all = list("", None).unwrap();
        let row = all.as_array().unwrap().iter().find(|r| r["Id"] == u).unwrap();
        assert_eq!((row["RepoTags"].clone(), row["RepoDigests"].clone()), (json!([]), json!([])));
        let old = list("", Some((1, 41))).unwrap();
        let row = old.as_array().unwrap().iter().find(|r| r["Id"] == u).unwrap();
        assert_eq!((row["RepoTags"].clone(), row["RepoDigests"].clone()), (json!(["<none>:<none>"]), json!(["<none>@<none>"])));
        assert_eq!(rows(r#"{"dangling":["true"]}"#), [u]);
        assert_eq!(rows(r#"{"dangling":{"1":true}}"#), [u]);
        assert_eq!(rows(r#"{"dangling":["false"]}"#), ["demo:new", "demo:old"]);
        assert_eq!(rows(r#"{"dangling":["true"],"label":["keep=1"]}"#), [u]);
        assert!(matches!(list(r#"{"dangling":["yes"]}"#, None), Err(Error::BadRequest(m)) if m == "invalid filter 'dangling=[yes]'"));
        assert!(matches!(list(r#"{"dangling":["true","false"]}"#, None), Err(Error::BadRequest(_))));
        assert_eq!(rows(r#"{"reference":["demo*"]}"#), ["demo:new", "demo:old"]);
        assert_eq!(rows(r#"{"label":["keep=1"]}"#), ["demo:new", u]);
        assert_eq!(rows(r#"{"before":["demo:new"]}"#), [u, "demo:old"]);
        assert_eq!(rows(r#"{"since":["demo:old"]}"#), ["demo:new", u]);
        assert_eq!(rows(r#"{"since":["demo:old"],"before":["demo:new"]}"#), [u]);
        assert!(matches!(list(r#"{"before":["nope:1"]}"#, None), Err(Error::NotFound(_))));
        assert_eq!(rows(r#"{"until":["2021-01-01T00:00:00Z"]}"#), [u, "demo:old"]);
        assert!(matches!(list(r#"{"until":["someday"]}"#, None), Err(Error::BadRequest(_))));
        let inspected = super::image_inspect(&engine, u).unwrap();
        assert_eq!((inspected["RepoTags"].clone(), inspected["RepoDigests"].clone()), (json!([]), json!([])));
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `docker images REPO` sends `{"reference":{"REPO":true}}`; a registry
    /// with a port must not have its port read as a tag.
    #[test]
    fn reference_filter_keeps_a_registry_port() {
        let listed = |pattern: &str| -> Vec<String> {
            let filters = json!({"reference": {pattern: true}}).to_string();
            let req = Request::internal("GET", "/images/json", &[("filters", &filters)], Vec::new());
            let f = super::filters(&req, &["reference", "dangling", "label"]).unwrap();
            ["alpine", "bitnami/redis:7", "localhost:5000/app", "registry.example.com:5000/team/app:1"]
                .iter()
                .map(|n| Reference::parse(n).unwrap().short())
                .filter(|n| f.any("reference", |p| reference_matches(p, n)))
                .collect()
        };
        assert_eq!(listed("alpine"), ["alpine:latest"]);
        assert_eq!(listed("bitnami/redis:7"), ["bitnami/redis:7"]);
        assert_eq!(listed("localhost:5000/app:latest"), ["localhost:5000/app:latest"]);
        assert_eq!(listed("localhost:5000/app"), ["localhost:5000/app:latest"]);
        assert_eq!(listed("localhost:5000/*"), ["localhost:5000/app:latest"]);
        assert_eq!(listed("registry.example.com:5000/team/app"), ["registry.example.com:5000/team/app:1"]);
        assert!(listed("localhost:5000/other").is_empty());
        // `docker ps --filter ancestor=`: the image as it was typed at create.
        assert!(reference_matches("localhost:5000/app:latest", "localhost:5000/app"));
        assert!(reference_matches("localhost:5000/app", "localhost:5000/app"));
        assert!(!reference_matches("localhost:5000/app:2", "localhost:5000/app"));
    }

    /// Reads a socket as `forward_stdin` does, reporting each read's length.
    fn read_to_the_end(mut server: UnixStream) -> std::sync::mpsc::Receiver<usize> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8];
            loop {
                wait_readable(&server);
                let n = io::Read::read(&mut server, &mut buf).unwrap();
                tx.send(n).unwrap();
                if n == 0 {
                    break;
                }
            }
        });
        rx
    }

    // This pins what wait_readable must do. The lost wake-up itself is too rare
    // for a unit test: the SLOW check in tests/e2e.sh is what catches that.
    #[test]
    fn wait_readable_wakes_for_input_and_its_end_and_not_while_idle() {
        let (idle, limit) = (Duration::from_millis(200), Duration::from_secs(10));
        let (mut client, server) = UnixStream::pair().unwrap();
        let rx = read_to_the_end(server);
        assert!(rx.recv_timeout(idle).is_err());
        io::Write::write_all(&mut client, b"in").unwrap();
        assert_eq!(rx.recv_timeout(limit), Ok(2));
        assert!(rx.recv_timeout(idle).is_err());
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(rx.recv_timeout(limit), Ok(0));

        // A client that dies, with no half-close, ends it too.
        let (client, server) = UnixStream::pair().unwrap();
        let rx = read_to_the_end(server);
        drop(client);
        assert_eq!(rx.recv_timeout(limit), Ok(0));
    }

    // Falling through to the read on a signal would bring the lost wake-up back.
    #[test]
    fn wait_readable_keeps_waiting_through_a_signal() {
        use std::os::unix::thread::JoinHandleExt;
        extern "C" fn nothing(_: libc::c_int) {}
        // SAFETY: the handler does nothing, and with no SA_RESTART poll sees EINTR.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = nothing as *const () as libc::sighandler_t;
            libc::sigaction(libc::SIGUSR2, &action, std::ptr::null_mut());
        }
        let (mut client, server) = UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            wait_readable(&server);
            tx.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: the thread is running, as nothing has woken it yet.
        assert_eq!(unsafe { libc::pthread_kill(waiter.as_pthread_t() as libc::pthread_t, libc::SIGUSR2) }, 0);
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        io::Write::write_all(&mut client, b"in").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok());
    }

    /// Docker's `RootFS.Layers` are the image config's uncompressed `diff_ids`,
    /// not the manifest's compressed layer digests (scanners like Trivy look
    /// up layers by `DiffID` against the tar from `docker save`).
    #[test]
    fn image_inspect_reports_uncompressed_diff_ids_in_rootfs() {
        use std::io::Write;
        let (root, engine) = engine_in("inspect-diff-ids", false);
        let cfg = json!({
            "architecture": "amd64",
            "os": "linux",
            "created": "2026-11-18T00:00:00Z",
            "config": {
                "Env": ["PATH=/usr/local/bin:/usr/bin"],
                "Healthcheck": {"Test": ["CMD-SHELL", "true"]}
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": [
                    "sha256:9bd858237f92a7b58be7f21bc17a45578f2ba486be6968fe4bd975a312142db1",
                    "sha256:af695414f7353af49c3ce40f3084457fe507c665b297ecfa2fe791eeac451dbb"
                ]
            }
        });
        let mut w = engine.store.ingest().unwrap();
        w.write_all(&serde_json::to_vec(&cfg).unwrap()).unwrap();
        let (cfg_digest, _) = w.finish().unwrap();

        let layers = vec![
            kiri_image::Layer {
                digest: "sha256:7a48b8f0c8f6814ce77ef58a6d6e23c948fd30a793ce9d03c2a8b136c1317a49".into(),
                size: 1024,
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            },
            kiri_image::Layer {
                digest: "sha256:aa57db58390911503d469fd9510d6f16ab424c25a8322d669a030e44ef73d421".into(),
                size: 2048,
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            },
        ];
        let image = kiri_image::archive::from_config("demo:diffids".into(), &cfg_digest, &cfg, layers.clone());
        engine.store.put_image(image).unwrap();

        let inspected = image_inspect(&engine, "demo:diffids").unwrap();
        assert_eq!(
            inspected["RootFS"],
            json!({
                "Type": "layers",
                "Layers": [
                    "sha256:9bd858237f92a7b58be7f21bc17a45578f2ba486be6968fe4bd975a312142db1",
                    "sha256:af695414f7353af49c3ce40f3084457fe507c665b297ecfa2fe791eeac451dbb"
                ]
            })
        );
        assert_eq!(inspected["Config"]["Healthcheck"]["Test"], json!(["CMD-SHELL", "true"]));

        // When the config blob is absent, fall back to the manifest's layer digests.
        let mut fallback = test_image("demo:no-cfg");
        fallback.layers = layers;
        engine.store.put_image(fallback).unwrap();
        let fallback_inspected = image_inspect(&engine, "demo:no-cfg").unwrap();
        assert_eq!(
            fallback_inspected["RootFS"]["Layers"],
            json!([
                "sha256:7a48b8f0c8f6814ce77ef58a6d6e23c948fd30a793ce9d03c2a8b136c1317a49",
                "sha256:aa57db58390911503d469fd9510d6f16ab424c25a8322d669a030e44ef73d421"
            ])
        );

        drop(engine);
        let _ = std::fs::remove_dir_all(root);
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
    fn an_enabled_x86_64_binfmt_rule_means_amd64_runs() {
        let qemu = "enabled\ninterpreter /usr/libexec/qemu-binfmt/x86_64-binfmt-P\nflags: POCF\noffset 0\nmagic 7f454c4602010100000000000000000002003e00\nmask fffffffffffefefcfffffffffffffffffeffffff\n";
        assert!(super::binfmt_runs_amd64(qemu));
        assert!(!super::binfmt_runs_amd64(&qemu.replace("enabled", "disabled")));
        // aarch64 (machine b7) and 32-bit x86 (class 01, machine 03) are not it.
        assert!(!super::binfmt_runs_amd64(&qemu.replace("02003e00", "0200b700")));
        assert!(!super::binfmt_runs_amd64("enabled\nmagic 7f454c4601010100000000000000000002000300\n"));
        assert!(!super::binfmt_runs_amd64("enabled\nextension .exe\n"));
    }

    #[test]
    fn cpu_shares_become_the_weight_runc_gives() {
        // Docker 29 / runc 1.3.2: 1024 is the kernel's default weight.
        for (shares, weight) in [(2, 1), (512, 59), (1024, 100), (4096, 303), (262_144, 10_000), (1, 1), (999_999, 10_000)] {
            assert_eq!(super::cpu_weight(shares), weight, "{shares}");
        }
        let l = super::limits_from(&json!({"CpuShares": 1024}), Default::default());
        assert_eq!((l.cpu_shares, l.cpu_weight), (1024, 100));
        // Rises with the shares, never backwards.
        let mut last = 0;
        for s in 2..5000 {
            let w = super::cpu_weight(s);
            assert!(w >= last, "{s}");
            last = w;
        }
    }

    #[test]
    fn container_network_mode_refuses_what_docker_refuses() {
        assert!(container_mode_allowed(&json!({"HostConfig": {"NetworkMode": "container:web"}}), "web").is_ok());
        // inspect reports the bindings asked for, as Docker does.
        let b = |container_port, proto: &str, host_ip: &str, host_port| super::super::ports::Binding { container_port, proto: proto.into(), host_ip: host_ip.into(), host_port };
        let ports = [b(80, "tcp", "", 8080), b(80, "tcp", "127.0.0.1", 0), b(53, "udp", "", 5353)];
        assert_eq!(inspect_bindings(&ports), json!({"80/tcp": [{"HostIp": "", "HostPort": "8080"}, {"HostIp": "127.0.0.1", "HostPort": ""}], "53/udp": [{"HostIp": "", "HostPort": "5353"}]}));
        assert_eq!(inspect_bindings(&[]), json!({}));
        assert_eq!(inspect_exposed(&[], &[]), Value::Null);
        assert_eq!(inspect_exposed(&ports, &[80, 9000]), json!({"80/tcp": {}, "53/udp": {}, "9000/tcp": {}}));
        // NetworkSettings.Ports: published ports with their addresses, exposed
        // ones without as null keys (#492); none at all while not running.
        let bound = [(b(80, "tcp", "", 8080), "0.0.0.0".to_string(), 8080)];
        assert_eq!(ports_map_of(&[], Some((&[], &[80]))), json!({"80/tcp": null}));
        assert_eq!(
            ports_map_of(&bound, Some((&ports, &[80, 9000]))),
            json!({"80/tcp": [{"HostIp": "0.0.0.0", "HostPort": "8080"}], "53/udp": null, "9000/tcp": null})
        );
        assert_eq!(ports_map_of(&[], None), json!({}));
        // Compose sends empty lists and maps for what a service leaves out.
        assert!(container_mode_allowed(&json!({"Hostname": "", "ExposedPorts": {}, "HostConfig": {"PortBindings": {}, "Dns": [], "Links": null}}), "web").is_ok());
        assert!(container_mode_allowed(&json!({}), "").is_err());
        assert_eq!(refused(json!({"HostConfig": {"PortBindings": {"80/tcp": [{"HostPort": "8080"}]}}})), "conflicting options: port publishing and the container type network mode");
        assert_eq!(refused(json!({"HostConfig": {"PublishAllPorts": true}})), "conflicting options: port publishing and the container type network mode");
        assert_eq!(refused(json!({"Hostname": "x"})), "conflicting options: hostname and the network mode");
        assert_eq!(refused(json!({"HostConfig": {"Dns": ["1.1.1.1"]}})), "conflicting options: dns and the network mode");
        assert_eq!(refused(json!({"HostConfig": {"ExtraHosts": ["a:1.2.3.4"]}})), "conflicting options: custom host-to-IP mapping and the network mode");
        assert_eq!(refused(json!({"ExposedPorts": {"80/tcp": {}}})), "conflicting options: port exposing and the container type network mode");
        assert!(refused(json!({"HostConfig": {"Links": ["db:db"]}})).contains("links"));
    }

    #[test]
    fn endpoint_addresses_are_checked_for_form() {
        let ok = endpoint_address(&json!({"IPAMConfig": {"IPv4Address": "10.5.0.9"}, "MacAddress": "02:42:0a:05:00:09"})).unwrap();
        let v6 = endpoint_address(&json!({"IPAMConfig": {"IPv6Address": "fd00::9"}})).unwrap();
        assert_eq!(v6.1, "fd00::9");
        assert!(endpoint_address(&json!({"IPAMConfig": {"IPv6Address": "fd00::zz"}})).is_err());
        assert_eq!(ok, ("10.5.0.9".to_owned(), String::new(), "02:42:0a:05:00:09".to_owned()));
        assert_eq!(endpoint_address(&json!({})).unwrap(), (String::new(), String::new(), String::new()));
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

    #[test]
    fn failed_pulls_say_what_to_fix() {
        use kiri_image::registry::Error;
        let http = |m: &str| Error::Http(m.into());
        let p = Some("http://127.0.0.1:7892");
        assert_eq!(super::pull_hint(&http("io: Connection refused (os error 111)"), "docker.io", p).unwrap(), "the proxy http://127.0.0.1:7892 refused the connection or closed it: is it running?");
        assert_eq!(super::pull_hint(&http("timeout: connect"), "docker.io", p).unwrap(), "no answer from the proxy http://127.0.0.1:7892");
        assert_eq!(super::pull_hint(&http("timeout: connect"), "docker.io", None).unwrap(), "couldn't reach Docker Hub: check the network, or set a proxy in Settings → Network");
        assert!(super::pull_hint(&http("io: failed to lookup address information"), "ghcr.io", None).unwrap().starts_with("couldn't look up ghcr.io"));
        let status = |status| Error::Status { url: "u".into(), status, body: String::new() };
        assert!(super::pull_hint(&status(429), "docker.io", None).unwrap().starts_with("Docker Hub limits"));
        assert_eq!(super::pull_hint(&status(401), "ghcr.io", None).unwrap(), "ghcr.io asked for a login: docker login ghcr.io");
        assert_eq!(super::pull_hint(&status(401), "docker.io", None).unwrap(), "Docker Hub asked for a login: docker login");
        assert_eq!(super::pull_hint(&status(500), "docker.io", None), None);
        assert_eq!(super::pull_hint(&Error::NotFound("no such image".into()), "docker.io", p), None);
    }

    /// macOS's "No route to host" from a registry on the local network says
    /// where Local Network is allowed; nothing for anything else.
    #[test]
    fn no_route_to_a_registry_on_the_local_network_points_at_local_network() {
        use kiri_image::registry::Error;
        let e65 = Error::Http("io: No route to host (os error 65)".into());
        let hint = |e: &Error, reg: &str, proxied: bool| super::local_network_hint(e, reg, proxied, "Kiri Repro");
        let want = |reg: &str| format!("{reg} is on the local network: Kiri Repro not allowed under Local Network in Privacy & Security (System Settings) usually causes this");
        for reg in ["192.168.1.20", "10.1.2.3:5000", "172.20.0.9", "169.254.10.1", "fd12::1", "[fe80::1]:5000"] {
            assert_eq!(hint(&e65, reg, false), cfg!(target_os = "macos").then(|| want(reg)), "{reg}");
        }
        // A public registry, another error, a pull through a proxy: as before.
        assert_eq!(hint(&e65, "8.8.8.8", false), None);
        assert_eq!(hint(&e65, "[2001:4860::8888]:443", false), None);
        assert_eq!(hint(&Error::Http("io: Connection refused (os error 61)".into()), "192.168.1.20", false), None);
        assert_eq!(hint(&Error::NotFound("no such image".into()), "192.168.1.20", false), None);
        assert_eq!(hint(&e65, "192.168.1.20", true), None);
        assert!(super::private_addr("::ffff:192.168.1.1".parse().unwrap()));
        assert!(!super::private_addr("172.32.0.1".parse().unwrap()));
    }

    /// A container named in create's options that does not exist is a 400
    /// with Docker's words, so the docker CLI does not take it for a missing
    /// image (a 404) and pull. A missing image is still a 404.
    #[test]
    fn a_missing_container_at_create_is_a_400_and_a_missing_image_a_404() {
        let dir = std::env::temp_dir().join(format!("kiri-missing-ctr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let engine = super::super::engine::boot_without_vm(&dir);
        let cfg = json!({"created": "2026-11-16T00:00:00Z", "architecture": "arm64", "os": "linux", "config": {"Cmd": ["true"]}});
        let mut w = engine.store.ingest().unwrap();
        io::Write::write_all(&mut w, &serde_json::to_vec(&cfg).unwrap()).unwrap();
        let (digest, _) = w.finish().unwrap();
        engine.store.put_image(kiri_image::archive::from_config("demo:local".into(), &digest, &cfg, Vec::new())).unwrap();
        let create = |host: Value, image: &str| {
            let body = json!({"Image": image, "HostConfig": host});
            match super::create(&Request::internal("POST", "/containers/create", &[], serde_json::to_vec(&body).unwrap()), &engine) {
                Ok(_) => panic!("the create was not refused"),
                Err(e) => super::status_of(e),
            }
        };
        assert_eq!(create(json!({"VolumesFrom": ["nope"]}), "demo:local"), (400, "No such container: nope".to_owned()));
        assert_eq!(create(json!({"PidMode": "container:nope"}), "demo:local"), (400, "No such container: nope".to_owned()));
        assert_eq!(create(json!({"Links": ["nope:db"]}), "demo:local"), (400, "could not get container for nope: No such container: nope".to_owned()));
        assert_eq!(
            create(json!({"NetworkMode": "container:nope"}), "demo:local"),
            (400, "joining network namespace of container: No such container: nope".to_owned())
        );
        let (status, message) = create(json!({}), "demo:nosuch");
        assert_eq!(status, 404);
        assert!(message.starts_with("No such image: demo:nosuch"));
        drop(engine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_and_container_labels_follow_docker() {
        let dir = std::env::temp_dir().join(format!("kiri-labels-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = kiri_image::Store::open(&dir).unwrap();
        let put = |name: &str, cfg: Value| {
            let bytes = serde_json::to_vec(&cfg).unwrap();
            let mut w = store.ingest().unwrap();
            io::Write::write_all(&mut w, &bytes).unwrap();
            let (digest, _) = w.finish().unwrap();
            let img = kiri_image::archive::from_config(name.into(), &digest, &cfg, Vec::new());
            store.put_image(img).unwrap();
        };
        put(
            "demo:labeled",
            json!({
                "created": "2026-11-16T00:00:00Z",
                "architecture": "arm64",
                "os": "linux",
                "config": {
                    "Labels": {
                        "cn.shopcider.safeguards.revision": "0a5b41cadacdafe94742fc8401d682347a392739",
                        "keep": "image"
                    }
                }
            }),
        );
        put(
            "demo:plain",
            json!({"created": "2026-11-16T00:00:00Z", "architecture": "arm64", "os": "linux", "config": {}}),
        );

        let engine = Engine::boot(store).unwrap();
        let labeled = super::image_inspect(&engine, "demo:labeled").unwrap();
        assert_eq!(
            labeled["Config"]["Labels"],
            json!({"cn.shopcider.safeguards.revision": "0a5b41cadacdafe94742fc8401d682347a392739", "keep": "image"})
        );
        let plain = super::image_inspect(&engine, "demo:plain").unwrap();
        assert!(plain["Config"].as_object().unwrap().contains_key("Labels"));
        assert!(plain["Config"]["Labels"].is_null());

        // The list shows `{}` for an image without labels, where inspect
        // shows `null`, as Docker does.
        let all = super::images(&Request::internal("GET", "/images/json", &[], Vec::new()), &engine).unwrap();
        let row = |name: &str| all.as_array().unwrap().iter().find(|r| r["RepoTags"][0] == name).unwrap()["Labels"].clone();
        assert_eq!(row("demo:plain"), json!({}));
        assert_eq!(row("demo:labeled")["keep"], "image");

        let filtered = super::images(
            &Request::internal("GET", "/images/json", &[("filters", r#"{"label":["keep=image"]}"#)], Vec::new()),
            &engine,
        )
        .unwrap();
        let rows = filtered.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["RepoTags"], json!(["demo:labeled"]));
        assert_eq!(rows[0]["Labels"]["keep"], "image");

        let negated = super::images(
            &Request::internal("GET", "/images/json", &[("filters", r#"{"label!":["keep=image"]}"#)], Vec::new()),
            &engine,
        )
        .unwrap();
        assert_eq!(negated.as_array().unwrap().len(), 1);
        assert_eq!(negated[0]["RepoTags"], json!(["demo:plain"]));

        let body = json!({
            "Image": "demo:labeled",
            "Labels": {"keep": "container", "extra": "1"},
            "HostConfig": {"NetworkMode": "none"}
        });
        let created = super::create(
            &Request::internal("POST", "/containers/create", &[("name", "c1")], serde_json::to_vec(&body).unwrap()),
            &engine,
        )
        .unwrap();
        let id = created["Id"].as_str().unwrap();
        let ctr = super::container_inspect(&engine, id).unwrap();
        assert_eq!(
            ctr["Config"]["Labels"],
            json!({
                "cn.shopcider.safeguards.revision": "0a5b41cadacdafe94742fc8401d682347a392739",
                "keep": "container",
                "extra": "1"
            })
        );

        drop(engine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `docker images` with 200 labelled images, 50 times: prints the median.
    /// A measure, not a check (cargo test -p kiri images_listing_speed --
    /// --ignored --nocapture).
    #[test]
    #[ignore]
    fn images_listing_speed() {
        let dir = std::env::temp_dir().join(format!("kiri-images-speed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = kiri_image::Store::open(&dir).unwrap();
        for n in 0..200 {
            let cfg = json!({
                "created": "2026-11-16T00:00:00Z", "architecture": "arm64", "os": "linux",
                "config": {"Env": ["PATH=/usr/bin"], "Cmd": ["sh"], "Labels": {"maintainer": "someone", "n": n.to_string(), "org.opencontainers.image.source": "https://example.com/r"}}
            });
            let bytes = serde_json::to_vec(&cfg).unwrap();
            let mut w = store.ingest().unwrap();
            io::Write::write_all(&mut w, &bytes).unwrap();
            let (digest, _) = w.finish().unwrap();
            store.put_image(kiri_image::archive::from_config(format!("speed/img{n}:latest"), &digest, &cfg, Vec::new())).unwrap();
        }
        let engine = Engine::boot(store).unwrap();
        let req = Request::internal("GET", "/images/json", &[], Vec::new());
        assert_eq!(super::images(&req, &engine).unwrap().as_array().unwrap().len(), 200);
        let mut times: Vec<_> = (0..50)
            .map(|_| {
                let t = std::time::Instant::now();
                std::hint::black_box(super::images(&req, &engine).unwrap());
                t.elapsed()
            })
            .collect();
        times.sort();
        eprintln!("images() with 200 images: median {:?}, min {:?}, max {:?}", times[25], times[0], times[49]);
        drop(engine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A mount target reaches the guest in a tab-separated line; a tab inside
    /// it would be read as the start of the volume's driver options, and the
    /// fields after it as a mount type, device and flags.
    #[test]
    fn mount_fields_cannot_carry_a_separator() {
        assert!(super::wire_text("mount target", "/app").is_ok());
        for bad in ["/app\tx", "/app\nx", "/app\rx", "/app\0x"] {
            let e = super::wire_text("mount target", bad).expect_err("refused");
            assert!(matches!(e, super::Error::BadRequest(m) if m.contains("is not allowed")), "{bad:?}");
        }
    }

    /// `/mnt/mac` is the guest's own name for the Mac's folders, not a path
    /// on the Mac; a bind of it would be a bind of the share.
    #[test]
    fn the_guest_share_cannot_be_named_as_a_bind_source() {
        assert!(super::guest_share_path("/mnt/mac", "/mnt/mac"));
        assert!(super::guest_share_path("/mnt/mac/Users/me", "/mnt/mac"));
        assert!(!super::guest_share_path("/mnt/macintosh/x", "/mnt/mac"));
        assert!(!super::guest_share_path("/Users/me", "/mnt/mac"));
        assert!(!super::guest_share_path("/mnt/mac", ""), "a Linux host has no share");
        // Other spellings of the same path name the share too.
        for p in ["/mnt//mac", "/mnt/./mac", "/mnt/x/../mac/Users", "//mnt/mac/", "/mnt/mac/."] {
            assert!(super::guest_share_path(p, "/mnt/mac"), "{p}");
        }
        assert!(!super::guest_share_path("/mnt/mac/../macintosh", "/mnt/mac"));
    }

    /// A volume's device is mounted by the guest as root, so it must not
    /// reach the share either.
    #[test]
    fn a_volume_cannot_mount_the_share() {
        let opts = |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<std::collections::BTreeMap<_, _>>();
        assert!(super::check_volume_device(&opts(&[("type", "virtiofs"), ("device", "kiri-mac")])).is_err());
        assert!(super::check_volume_device(&opts(&[("type", "VirtioFS"), ("device", "rosetta")])).is_err());
        if cfg!(target_os = "macos") {
            assert!(super::check_volume_device(&opts(&[("type", "none"), ("o", "bind"), ("device", "/mnt/mac")])).is_err());
            assert!(super::check_volume_device(&opts(&[("type", "none"), ("o", "bind"), ("device", "/mnt//mac/Users")])).is_err());
        }
        // Ordinary volumes are untouched.
        assert!(super::check_volume_device(&opts(&[("type", "none"), ("o", "bind"), ("device", "/Users/me/data")])).is_ok());
        assert!(super::check_volume_device(&opts(&[("type", "nfs"), ("o", "addr=10.0.0.1"), ("device", ":/export")])).is_ok());
        assert!(super::check_volume_device(&opts(&[("type", "tmpfs"), ("device", "tmpfs")])).is_ok());
        assert!(super::check_volume_device(&opts(&[])).is_ok());
    }

    /// `--add-host name:ip` and `--hostname` are written into the container's
    /// /etc/hosts by the guest, so neither may add a line of its own.
    #[test]
    fn host_names_are_checked_before_they_are_written() {
        assert!(super::valid_hostname("db"));
        assert!(super::valid_hostname("host.docker.internal"));
        // What Docker takes, Kiri takes: Compose files name hosts like these.
        for ok in ["my_app", "My.Host_1", "-db", "db-", "a..b", "db."] {
            assert!(super::valid_hostname(ok), "{ok}");
        }
        for bad in ["db\n1.2.3.4 evil", "db\r", "db\tx", "db\0", ""] {
            assert!(!super::valid_hostname(bad), "{bad:?}");
        }
        // A hosts name is as free as Docker leaves it, short of a control character.
        assert!(super::valid_hosts_name("my_app.local"));
        assert!(super::valid_hosts_name("a b"), "Docker takes it: two names for one address");
        for bad in ["db\t1.2.3.4 evil", "db\n", "db\r", "db\0", ""] {
            assert!(!super::valid_hosts_name(bad), "{bad:?}");
        }
    }
}

#[cfg(test)]
mod leave_first_network_tests {
    use super::*;

    /// After the .NET Aspire sequence (created on bridge, connected to a
    /// session network, disconnected from bridge) inspect and the list show
    /// only the session network; after leaving that too, no network at all,
    /// as Docker shows it, not the `none` Kiri keeps it on.
    #[test]
    fn inspect_shows_only_the_networks_it_still_has() {
        let root = std::env::temp_dir().join(format!("kiri-leave-first-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let engine = super::super::engine::boot_without_vm(&root);
        engine.lock_net().create("probe-net", Default::default()).unwrap();
        let image = kiri_image::Image { mirror: None,
            name: "test:1".into(), digest: "sha256:0123".into(), index_digest: None, config_digest: "sha256:0123".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let spec: super::super::engine::Spec = serde_json::from_value(json!({
            "image": "test:1", "image_id": "sha256:0123", "entrypoint": [], "cmd": ["true"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "bridge", "labels": {},
            "restart": {"name": "no", "max_retries": 0}
        })).unwrap();
        let id = engine.create(Some("probe".into()), spec, &image).unwrap();
        engine.connect("probe-net", &id, Vec::new(), String::new(), String::new(), String::new()).unwrap();
        engine.disconnect("bridge", &id).unwrap();
        let names = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        let networks: Vec<super::super::network::Network> = engine.lock_net().all().to_vec();
        assert_eq!(names(&container_inspect(&engine, &id).unwrap()["NetworkSettings"]["Networks"]), ["probe-net"]);
        assert_eq!(names(&engine.with(&id, |c| network_settings(c, &networks)).unwrap()["Networks"]), ["probe-net"]);
        engine.disconnect("probe-net", &id).unwrap();
        assert!(names(&container_inspect(&engine, &id).unwrap()["NetworkSettings"]["Networks"]).is_empty());
        assert!(names(&engine.with(&id, |c| network_settings(c, &networks)).unwrap()["Networks"]).is_empty());
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }
}
