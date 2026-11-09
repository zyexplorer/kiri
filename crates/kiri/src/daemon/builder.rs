//! The managed builder: one BuildKit (moby/buildkit) in a privileged
//! container the daemon makes itself on first need, its cache in a named
//! volume. It starts when a build wants it and stops after a few idle
//! minutes, so between builds it costs nothing.
//!
//! It is the same container buildx's docker-container builder `kiri-build`
//! uses (`buildx_buildkit_kiri-build0`, cache volume `…_state`), so caches
//! from before carry over and there is one builder, not two: with buildx,
//! `docker build` reaches it through `buildctl dial-stdio` (builds.rs sets
//! that builder up); without buildx, the docker CLI, Compose and SDKs send
//! the classic `POST /build`, which [`build`] runs here with `buildctl`.
//! Either way BuildKit keeps the build in its history, which the window's
//! Builds view reads (through buildx, so only where buildx is installed).

use std::io::{self, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use kiri_image::{Client, Platform, Reference};
use kiri_proto::Stream;

use super::api::{self, Next};
use super::engine::{Engine, Error, ExecConfig, Status};
use super::http::{self, Request};

/// The image buildx's docker-container driver runs, by the tag it uses: an
/// image already pulled under it is used as it is.
pub const IMAGE: &str = "moby/buildkit:buildx-stable-1";
/// What a fresh pull fetches: that tag's index on 2026-10-30 (BuildKit
/// 0.2x), pinned so every Kiri builds with the same BuildKit. Stored under
/// [`IMAGE`]. Moving the pin changes nothing for an image already pulled.
const PINNED: &str =
    "moby/buildkit@sha256:cec9f139f45e93c5c69c60f8b07cfad9f43f4ef6b6a6cd917527fea5ff2e3dea";
/// Marks the container as Kiri's builder. It stays in `docker ps`, as
/// buildx's does on any engine.
const LABEL: &str = "dev.kiri.builder";
/// Per build: the context and registry logins, in the builder's own /tmp.
const WORK: &str = "/tmp/kiri-build";
/// Largest bare Dockerfile (a body that isn't an archive) read into memory.
const MAX_DOCKERFILE: u64 = 8 << 20;
const GB: u64 = 1 << 30;

/// Builds running now; the idle watch never stops the builder under one.
static BUILDS: AtomicUsize = AtomicUsize::new(0);
/// When the builder was last seen busy (Unix seconds; 0 = not seen running).
static LAST_USE: AtomicU64 = AtomicU64::new(0);
/// Builds with a `buildid`: (build id, builder container, work dir).
static CANCELLABLE: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());
/// Making, starting and stopping the builder, one at a time. Never held
/// across a pull of its image, which [`PULL`] keeps to one at a time.
static ENSURE: Mutex<()> = Mutex::new(());
/// Pulling the BuildKit image. Apart from [`ENSURE`]: on a slow network
/// the pull takes minutes, and buildx looking for its container (which it
/// gives 20 s) or the idle watch must not wait for it.
static PULL: Mutex<()> = Mutex::new(());
/// `buildx_buildkit_kiri-build0`: buildx's name for the BuildKit container
/// of builder `kiri-build`'s first node.
pub fn container_name() -> String {
    format!("buildx_buildkit_{}0", crate::builds::BUILDER.get())
}

fn volume_name() -> String {
    format!("{}_state", container_name())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The builder container, if there is one: (id, status).
fn find(engine: &Engine) -> Option<(String, Status)> {
    let name = container_name();
    engine
        .each(|c| (c.name == name).then(|| (c.id.clone(), c.status)))
        .into_iter()
        .next()
}

// --- Settings ----------------------------------------------------------

fn settings_path() -> Option<std::path::PathBuf> {
    super::state_dir().ok().map(|d| d.join("builder.json"))
}

fn settings() -> Value {
    settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// The disk the daemon's state is on, in bytes (0 if unknown).
fn disk_total() -> u64 {
    let Some(dir) = super::state_dir()
        .ok()
        .and_then(|d| std::ffi::CString::new(d.as_os_str().as_encoded_bytes()).ok())
    else {
        return 0;
    };
    // SAFETY: statvfs fills the zeroed struct for a valid C path.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(dir.as_ptr(), &mut s) != 0 {
            return 0;
        }
        (s.f_blocks as u64).saturating_mul(s.f_frsize as u64)
    }
}

/// The build cache's default cap: a tenth of the disk, at most 20 GB.
fn default_cache(disk: u64) -> u64 {
    if disk == 0 {
        20 * GB
    } else {
        (disk / 10).clamp(GB, 20 * GB)
    }
}

/// The build cache's cap in bytes: `KIRI_BUILD_CACHE_GB`, else Settings,
/// else [`default_cache`]. BuildKit's garbage collector keeps the cache under it.
pub fn cache_limit() -> u64 {
    let gb = std::env::var("KIRI_BUILD_CACHE_GB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| settings()["cache_gb"].as_u64());
    gb.filter(|g| *g > 0)
        .map_or_else(|| default_cache(disk_total()), |g| g * GB)
}

/// Seconds without a build before the builder stops: `KIRI_BUILDER_IDLE_SECS`,
/// else Settings, else five minutes. 0 keeps it running.
pub fn idle_secs() -> u64 {
    std::env::var("KIRI_BUILDER_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| settings()["idle_secs"].as_u64())
        .unwrap_or(300)
}

/// buildkitd.toml, written at each start: the garbage collector's limits.
/// BuildKit 0.17+ keys: the cache may use up to `maxUsedSpace`, and GC never
/// takes it below `reservedSpace`.
/// `registries` is the mirror's part (mirror.rs): Kiri's own images for `FROM`.
fn config_toml(limit: u64, registries: &str) -> String {
    format!(
        "# Written by Kiri each time its builder starts (Settings: the build cache's size).\n[worker.oci]\n  gc = true\n  maxUsedSpace = {limit}\n  reservedSpace = {}\n{registries}",
        limit / 4
    )
}

/// `GET /kiri/builder`: the settings and the builder's state.
pub fn settings_report(engine: &Engine) -> Value {
    let s = settings();
    let state = find(engine).map(|(_, st)| st.as_str()).unwrap_or("none");
    json!({
        "cache_gb": s["cache_gb"], "default_cache_gb": default_cache(disk_total()) / GB, "cache_limit": cache_limit(),
        "idle_secs": idle_secs(), "image": IMAGE, "container": container_name(), "volume": volume_name(), "state": state,
        "overridden": std::env::var_os("KIRI_BUILD_CACHE_GB").is_some() || std::env::var_os("KIRI_BUILDER_IDLE_SECS").is_some(),
    })
}

/// `POST /kiri/builder` `{"cache_gb": 20 | null, "idle_secs": 300}`: null or
/// 0 is the default cap. Used from the builder's next start.
pub fn set_settings(req: &Request) -> Result<(), Error> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| Error::BadRequest(format!("invalid JSON: {e}")))?;
    let mut s = settings();
    if !s.is_object() {
        s = json!({});
    }
    match &body["cache_gb"] {
        Value::Null if body.get("cache_gb").is_some() => s["cache_gb"] = Value::Null,
        Value::Null => {}
        v => {
            let gb = v
                .as_u64()
                .filter(|g| *g <= 100_000)
                .ok_or_else(|| Error::BadRequest("cache_gb must be a whole number of GB".into()))?;
            s["cache_gb"] = if gb == 0 { Value::Null } else { gb.into() };
        }
    }
    if !body["idle_secs"].is_null() {
        s["idle_secs"] = body["idle_secs"]
            .as_u64()
            .filter(|v| *v <= 7 * 86400)
            .ok_or_else(|| Error::BadRequest("idle_secs must be 0 to 604800".into()))?
            .into();
    }
    let path = settings_path().ok_or_else(|| Error::Io(io::Error::other("HOME is not set")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, s.to_string())
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(Error::Io)
}

// --- The container -------------------------------------------------------

/// The BuildKit image, pulled (by [`PINNED`]) if it isn't here yet.
fn ensure_image(engine: &Engine, say: &mut dyn FnMut(&str)) -> Result<kiri_image::Image, Error> {
    if let Some(i) = engine.image(IMAGE)? {
        return Ok(i);
    }
    let _p = PULL.lock().unwrap_or_else(|e| e.into_inner());
    // Pulled meanwhile by whoever had the lock.
    if let Some(i) = engine.image(IMAGE)? {
        return Ok(i);
    }
    say(&format!("Pulling {IMAGE} for Kiri's builder"));
    let reference =
        Reference::parse(PINNED).map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
    let pulled = Client::new()
        .pull(&engine.store, &reference, &Platform::host_linux(), &|_| {})
        .map_err(|e| Error::Io(io::Error::other(format!("pulling {IMAGE}: {e}"))))?;
    let name = Reference::parse(IMAGE)
        .map(|r| r.short())
        .unwrap_or_else(|_| IMAGE.to_owned());
    engine.store.put_image(kiri_image::Image {
        name: name.clone(),
        ..pulled.clone()
    })?;
    // Known by its tag only: the blobs stay, the tagged record uses them.
    if pulled.name != name {
        let _ = engine.store.remove_image(&pulled.name);
    }
    engine.emit_image("pull", &pulled.config_digest, &name);
    engine
        .image(IMAGE)?
        .ok_or_else(|| Error::Io(io::Error::other(format!("{IMAGE} vanished after its pull"))))
}

/// The body of the builder's create: what buildx's docker-container driver
/// makes (privileged, the state volume at /var/lib/buildkit, the same
/// buildkitd flag), without a restart policy, since it starts on demand.
fn create_body() -> Value {
    json!({
        "Image": IMAGE,
        "Cmd": ["--allow-insecure-entitlement=network.host"],
        "Labels": {LABEL: "1"},
        "HostConfig": {
            "Privileged": true,
            "Mounts": [{"Type": "volume", "Source": volume_name(), "Target": "/var/lib/buildkit"}],
            "RestartPolicy": {"Name": "no"},
        },
    })
}

fn create(engine: &Engine, say: &mut dyn FnMut(&str)) -> Result<String, Error> {
    ensure_image(engine, say)?;
    let name = container_name();
    let req = Request::internal(
        "POST",
        "/containers/create",
        &[("name", &name)],
        create_body().to_string().into_bytes(),
    );
    let made = api::create(&req, engine)?;
    crate::diag::info!("builder created", name = name);
    let id = made["Id"].as_str().unwrap_or_default().to_owned();
    // Before its first start, whoever starts it (buildx, for one).
    write_config(engine, &id);
    Ok(id)
}

/// `POST /containers/<builder>/start` from buildx (its docker-container
/// driver starts the container itself): the config is written first, as
/// for the daemon's own starts, so the mirror address and the cache limit
/// are today's, not whatever the container was made with (an older Kiri's
/// mirror address, a bridge gateway that has since moved).
pub fn before_start(engine: &Engine, key: &str) {
    let Some((id, status)) = find(engine) else {
        return;
    };
    let key = key.trim_start_matches('/');
    if status == Status::Running
        || !(key == container_name() || (key.len() >= 6 && id.starts_with(key)))
    {
        return;
    }
    let _g = ENSURE.lock().unwrap_or_else(|e| e.into_inner());
    write_config(engine, &id);
}

/// The garbage collector's limits, for buildkitd's next start.
fn write_config(engine: &Engine, id: &str) {
    let archive = tar(&[
        ("buildkit/", None),
        (
            "buildkit/buildkitd.toml",
            Some(config_toml(cache_limit(), &super::mirror::config(engine)).as_bytes()),
        ),
    ]);
    if let Err(e) = copy_in(engine, id, "/etc", &mut io::Cursor::new(archive)) {
        crate::diag::warn!("builder config not written", error = format!("{e:?}"));
    }
    if kiri_vmm::rosetta_installed() {
        hide_amd64_qemu(engine, id);
    }
}

/// BuildKit's x86-64 emulator, which it runs amd64 build steps under when it
/// thinks the machine can't run them itself.
const AMD64_QEMU: &str = "buildkit-qemu-x86_64";

/// With Rosetta shared into the VM, amd64 build steps run through Rosetta,
/// as `docker run --platform linux/amd64` does. BuildKit decides whether
/// amd64 runs natively by starting a tiny amd64 program in an empty chroot,
/// where Rosetta can't start (it needs `/proc/self/exe`), so it would run
/// every amd64 step under its own QEMU instead: much slower, and some
/// programs crash there (Bun aborts with "MemoryExhaustion"). With no
/// executable emulator BuildKit runs the step directly, and the kernel
/// hands it to Rosetta, which works inside the step's container. Other
/// platforms (arm/v7, riscv64…) keep their emulators. An empty file, not
/// executable, takes the emulator's place, so builders made before this
/// change get it too.
fn hide_amd64_qemu(engine: &Engine, id: &str) {
    let archive = tar(&[(AMD64_QEMU, Some(&[]))]);
    if let Err(e) = copy_in(engine, id, "/usr/bin", &mut io::Cursor::new(archive)) {
        crate::diag::warn!("builder amd64 emulator not hidden", error = format!("{e:?}"));
    }
}

/// `POST /containers/create` for the builder's name, from buildx when it
/// first builds (its docker-container driver makes the container itself):
/// the daemon makes its own instead, as buildx would but without a restart
/// policy and with today's config, and buildx then starts and uses it.
/// Only a build makes it: buildx looking the container up (`docker buildx
/// ls`, `inspect`, the window's Builds view) makes and pulls nothing, so a
/// fresh Kiri has no builder, image or cache volume until the first build.
/// None for any other name, or when the builder exists (the usual 409).
pub fn create_for_buildx(engine: &Engine, name: Option<&str>) -> Option<Result<Value, Error>> {
    if name.map(|n| n.trim_start_matches('/')) != Some(container_name().as_str()) {
        return None;
    }
    let _g = ENSURE.lock().unwrap_or_else(|e| e.into_inner());
    if find(engine).is_some() {
        return None;
    }
    Some(create(engine, &mut |_| {}).map(|id| json!({"Id": id, "Warnings": []})))
}

/// The builder, made if need be, started and answering. `say` hears what
/// takes a while (a pull, a start).
fn ensure_running(engine: &Engine, say: &mut dyn FnMut(&str)) -> Result<String, Error> {
    if find(engine).is_none() {
        // Before the lock: a pull must not hold the builder up for others.
        ensure_image(engine, say)?;
    }
    let _g = ENSURE.lock().unwrap_or_else(|e| e.into_inner());
    let (id, status) = match find(engine) {
        Some(found) => found,
        None => (create(engine, say)?, Status::Created),
    };
    if status != Status::Running {
        write_config(engine, &id);
        say("Starting Kiri's builder (BuildKit)");
        engine.start(&id)?;
    } else if engine.is_paused(&id) {
        engine.pause(&id, false)?;
    }
    LAST_USE.store(now_secs(), Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok((0, _)) = engine.exec_capture(&id, &["buildctl", "debug", "workers"]) {
            return Ok(id);
        }
        if find(engine).is_none_or(|(_, s)| s != Status::Running) || Instant::now() > deadline {
            let text: Vec<u8> = engine
                .logs(&id, false)
                .ok()
                .flatten()
                .map(|l| l.past.tail(Some(5)).flat_map(|r| r.data).collect())
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&text);
            let tail: Vec<&str> = text.lines().collect();
            return Err(Error::Io(io::Error::other(format!(
                "Kiri's builder (BuildKit) did not start: {}",
                tail.join(" / ")
            ))));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Pauses the builder once it has been idle [`idle_secs`]: no build here,
/// no exec in it (buildx's dial-stdio while it builds). Never wakes the VM.
pub fn watch(engine: Arc<Engine>) {
    super::mirror::start(&engine);
    let me = Arc::downgrade(&engine);
    drop(engine);
    let _ = std::thread::Builder::new()
        .name("kiri-builder-idle".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(15));
                let Some(engine) = me.upgrade() else { return };
                let idle = idle_secs();
                let Some((id, Status::Running)) = find(&engine) else {
                    LAST_USE.store(0, Ordering::Relaxed);
                    continue;
                };
                // Paused already: once something resumes it, it gets a full idle
                // time again (counted from the first look at it running).
                if engine.is_paused(&id) {
                    LAST_USE.store(0, Ordering::Relaxed);
                    continue;
                }
                let now = now_secs();
                let busy = || BUILDS.load(Ordering::Relaxed) > 0 || engine.running_execs(&id) > 0;
                if idle == 0 || busy() || LAST_USE.load(Ordering::Relaxed) == 0 {
                    LAST_USE.store(now, Ordering::Relaxed);
                    continue;
                }
                if now.saturating_sub(LAST_USE.load(Ordering::Relaxed)) < idle {
                    continue;
                }
                let _g = ENSURE.lock().unwrap_or_else(|e| e.into_inner());
                if busy() {
                    continue;
                }
                crate::diag::info!("builder idle, pausing it", secs = idle);
                if let Err(e) = engine.pause_idle_builder(&id) {
                    crate::diag::warn!("builder did not pause", error = format!("{e:?}"));
                }
                LAST_USE.store(0, Ordering::Relaxed);
            }
        });
}

/// A build in progress, for the idle watch.
struct Busy;

impl Busy {
    fn new() -> Busy {
        BUILDS.fetch_add(1, Ordering::Relaxed);
        Busy
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        LAST_USE.store(now_secs(), Ordering::Relaxed);
        BUILDS.fetch_sub(1, Ordering::Relaxed);
    }
}

// --- In and out of the container -------------------------------------------

fn internal_exec(argv: &[String], stdin: bool, env: Vec<String>) -> ExecConfig {
    ExecConfig {
        console_size: None,
        argv: argv.to_vec(),
        env,
        cwd: "/".into(),
        user: String::new(),
        tty: false,
        stdin,
        privileged: true,
        internal: true,
    }
}

/// A tar archive, as `docker cp` takes it, from (path, contents) entries; a
/// path ending in `/` with no contents is a directory.
fn tar(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        let mut h = [0u8; 512];
        let field = |h: &mut [u8; 512], at: usize, len: usize, v: &[u8]| {
            h[at..at + v.len().min(len)].copy_from_slice(&v[..v.len().min(len)])
        };
        field(&mut h, 0, 100, name.as_bytes());
        let size = data.map_or(0, <[u8]>::len);
        field(
            &mut h,
            100,
            8,
            if data.is_some() {
                b"0000644\0"
            } else {
                b"0000755\0"
            },
        );
        field(&mut h, 108, 8, b"0000000\0");
        field(&mut h, 116, 8, b"0000000\0");
        field(&mut h, 124, 12, format!("{size:011o}\0").as_bytes());
        field(&mut h, 136, 12, format!("{:011o}\0", now_secs()).as_bytes());
        field(&mut h, 148, 8, b"        ");
        h[156] = if data.is_some() { b'0' } else { b'5' };
        field(&mut h, 257, 8, b"ustar\x0000");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        field(&mut h, 148, 8, format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&h);
        if let Some(d) = data {
            out.extend_from_slice(d);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
    }
    out.resize(out.len() + 1024, 0);
    out
}

/// Unpacks a tar archive into a directory of the container, as `docker cp` does.
fn copy_in(engine: &Engine, id: &str, path: &str, archive: &mut dyn Read) -> Result<(), Error> {
    let stream = engine.copy(
        id,
        kiri_proto::copy_op::IN,
        path,
        kiri_proto::copy_flags::CHUNKED,
    )?;
    let mut to_guest = std::fs::File::from(stream.fd.try_clone()?);
    api::send_chunks(archive, &mut to_guest)?;
    let mut from_guest = std::fs::File::from(stream.fd.try_clone()?);
    let frame = api::guest_frame(&mut from_guest)?;
    match kiri_proto::decode(&frame) {
        Ok(kiri_proto::Msg::CopyDone) => Ok(()),
        Ok(kiri_proto::Msg::Error(m)) => Err(api::copy_error(m)),
        Ok(other) => Err(Error::Io(io::Error::other(format!(
            "unexpected copy reply: {other:?}"
        )))),
        Err(e) => Err(Error::Io(io::Error::other(format!(
            "bad copy reply: {e:?}"
        )))),
    }
}

/// Runs a command in the container with `input` as its stdin; returns its
/// exit code and output.
fn exec_with_input(
    engine: &Engine,
    id: &str,
    argv: &[String],
    input: &mut dyn Read,
) -> Result<(i32, String), Error> {
    let exec = engine.exec_create(id, internal_exec(argv, true, Vec::new()))?;
    let (_, rx) = engine.exec_start(&exec)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match input.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if engine.input(&exec, &buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = engine.close_stdin(&exec);
    let out: Vec<u8> = rx.into_iter().flat_map(|(_, d)| d).collect();
    let code = engine
        .with_exec(&exec, |e| e.exit_code.unwrap_or(-1))
        .unwrap_or(-1);
    engine.forget_exec(&exec);
    Ok((code, String::from_utf8_lossy(&out).into_owned()))
}

fn run_ok(engine: &Engine, id: &str, argv: &[&str]) -> Result<(), String> {
    match engine.exec_capture(id, argv) {
        Ok((0, _)) => Ok(()),
        Ok((code, out)) => Err(format!(
            "{} failed ({code}): {}",
            argv.join(" "),
            out.trim()
        )),
        Err(e) => Err(format!("{e:?}")),
    }
}

// --- POST /build -----------------------------------------------------------

/// What `POST /build` asks for, from its query.
#[derive(Debug, Default, PartialEq)]
struct Options {
    tags: Vec<String>,
    /// The Dockerfile's path in the context (or the remote).
    dockerfile: String,
    remote: Option<String>,
    target: String,
    build_args: Vec<(String, String)>,
    labels: Vec<(String, String)>,
    no_cache: bool,
    pull: bool,
    platform: String,
    /// `host` or `none`; empty for the default network.
    network: String,
    /// `name=ip`, as BuildKit's frontend takes them.
    extra_hosts: Vec<String>,
    shm_size: u64,
    ulimits: Vec<String>,
    cache_from: Vec<String>,
    quiet: bool,
    /// `buildid`, which `POST /build/cancel?id=` names.
    build_id: String,
}

fn json_query(req: &Request, key: &str) -> Result<Value, Error> {
    match req.query(key).filter(|v| !v.is_empty()) {
        None => Ok(Value::Null),
        Some(v) => {
            serde_json::from_str(v).map_err(|e| Error::BadRequest(format!("invalid {key}: {e}")))
        }
    }
}

/// `a/b/Dockerfile` stays; a leading `./` or `/` goes; `..` is refused.
fn clean_path(p: &str) -> Result<String, Error> {
    let parts: Vec<&str> = p
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(Error::BadRequest(format!(
            "the Dockerfile ({p}) must be within the build context"
        )));
    }
    Ok(parts.join("/"))
}

impl Options {
    fn parse(req: &Request) -> Result<Options, Error> {
        if req.query("version") == Some("2") {
            return Err(Error::BadRequest(
                "Kiri builds with the classic builder API (version 1): BuildKit sessions go to Kiri's builder through buildx (docker buildx use kiri-build)".into(),
            ));
        }
        let mut tags = Vec::new();
        for t in req.queries("t").filter(|t| !t.is_empty()) {
            let r = Reference::parse(t)
                .map_err(|_| Error::BadRequest(format!("invalid reference format: {t}")))?;
            if r.is_digest() {
                return Err(Error::BadRequest(format!(
                    "invalid reference format: {t}: a tag, not a digest"
                )));
            }
            tags.push(r.short());
        }
        let pairs = |v: Value| -> Vec<(String, String)> {
            let mut out: Vec<(String, String)> = v
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect();
            out.sort();
            out
        };
        let network = match req.query("networkmode").unwrap_or_default() {
            "" | "default" | "bridge" => String::new(),
            m @ ("host" | "none") => m.to_owned(),
            m => {
                return Err(Error::BadRequest(format!(
                    "network mode \"{m}\" not supported by buildkit"
                )));
            }
        };
        let mut extra_hosts = Vec::new();
        for h in req
            .queries("extrahosts")
            .flat_map(|h| h.split(','))
            .map(str::trim)
            .filter(|h| !h.is_empty())
        {
            let (name, ip) = h
                .split_once(':')
                .or_else(|| h.split_once('='))
                .ok_or_else(|| Error::BadRequest(format!("invalid extra host {h}: name:ip")))?;
            let ip = if ip == "host-gateway" {
                super::ports::HOST_ADDR
            } else {
                ip
            };
            extra_hosts.push(format!("{name}={ip}"));
        }
        let ulimits = json_query(req, "ulimits")?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|u| {
                Some(format!(
                    "{}={}:{}",
                    u["Name"].as_str()?,
                    u["Soft"].as_i64()?,
                    u["Hard"].as_i64()?
                ))
            })
            .collect();
        let cache_from = json_query(req, "cachefrom")?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_owned))
            .filter(|c| !c.is_empty())
            .collect();
        let dockerfile = clean_path(
            req.query("dockerfile")
                .filter(|d| !d.is_empty())
                .unwrap_or("Dockerfile"),
        )?;
        Ok(Options {
            tags,
            dockerfile: if dockerfile.is_empty() {
                "Dockerfile".into()
            } else {
                dockerfile
            },
            remote: req
                .query("remote")
                .filter(|r| !r.is_empty())
                .map(str::to_owned),
            target: req.query("target").unwrap_or_default().to_owned(),
            build_args: pairs(json_query(req, "buildargs")?),
            labels: pairs(json_query(req, "labels")?),
            no_cache: req.flag("nocache"),
            pull: req.flag("pull"),
            platform: req.query("platform").unwrap_or_default().to_lowercase(),
            network,
            extra_hosts,
            shm_size: req
                .query("shmsize")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            ulimits,
            cache_from,
            quiet: req.flag("q"),
            build_id: req.query("buildid").unwrap_or_default().to_owned(),
        })
    }

    /// `buildctl build …` for a context unpacked at `dir/context`.
    fn argv(&self, dir: &str) -> Vec<String> {
        let mut a: Vec<String> = [
            "buildctl",
            "build",
            "--progress",
            "plain",
            "--frontend",
            "dockerfile.v0",
        ]
        .map(str::to_owned)
        .to_vec();
        let mut opt = |s: String| {
            a.push("--opt".into());
            a.push(s);
        };
        match &self.remote {
            Some(url) => {
                opt(format!("context={url}"));
                opt(format!("filename={}", self.dockerfile));
            }
            None => opt(format!(
                "filename={}",
                self.dockerfile
                    .rsplit('/')
                    .next()
                    .unwrap_or(&self.dockerfile)
            )),
        }
        if !self.target.is_empty() {
            opt(format!("target={}", self.target));
        }
        for (k, v) in &self.build_args {
            opt(format!("build-arg:{k}={v}"));
        }
        for (k, v) in &self.labels {
            opt(format!("label:{k}={v}"));
        }
        if self.pull {
            opt("image-resolve-mode=pull".into());
        }
        if !self.platform.is_empty() {
            opt(format!("platform={}", self.platform));
        }
        if !self.network.is_empty() {
            opt(format!("force-network-mode={}", self.network));
        }
        if !self.extra_hosts.is_empty() {
            opt(format!("add-hosts={}", self.extra_hosts.join(",")));
        }
        if self.shm_size > 0 {
            opt(format!("shm-size={}", self.shm_size));
        }
        if !self.ulimits.is_empty() {
            opt(format!("ulimit={}", self.ulimits.join(",")));
        }
        if self.remote.is_none() {
            let (sub, _) = self.dockerfile.rsplit_once('/').unwrap_or(("", ""));
            a.extend([
                "--local".into(),
                format!("context={dir}/context"),
                "--local".into(),
            ]);
            a.push(if sub.is_empty() {
                format!("dockerfile={dir}/context")
            } else {
                format!("dockerfile={dir}/context/{sub}")
            });
        }
        if self.network == "host" {
            a.extend(["--allow".into(), "network.host".into()]);
        }
        if self.no_cache {
            a.push("--no-cache".into());
        }
        for c in &self.cache_from {
            a.extend(["--import-cache".into(), format!("type=registry,ref={c}")]);
        }
        // An image archive on stdout, which the daemon loads into Kiri's
        // store; the names are quoted, the CSV way, since there may be several.
        a.push("--output".into());
        a.push(if self.tags.is_empty() {
            "type=docker,dest=-".into()
        } else {
            format!("type=docker,\"name={}\",dest=-", self.tags.join(","))
        });
        a
    }
}

/// The CLI's `X-Registry-Config` (base64 JSON: server → {username,
/// password, identitytoken, …}) as a docker config.json for buildctl.
fn registry_config(header: &str) -> Option<Vec<u8>> {
    let raw = kiri_image::b64::decode(header)?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    let mut auths = serde_json::Map::new();
    for (server, a) in v.as_object()? {
        let mut e = serde_json::Map::new();
        if let (Some(u), Some(p)) = (a["username"].as_str(), a["password"].as_str()) {
            e.insert(
                "auth".into(),
                kiri_image::b64::encode(format!("{u}:{p}").as_bytes()).into(),
            );
        } else if let Some(t) = a["auth"].as_str().filter(|t| !t.is_empty()) {
            e.insert("auth".into(), t.into());
        }
        if let Some(t) = a["identitytoken"].as_str().filter(|t| !t.is_empty()) {
            e.insert("identitytoken".into(), t.into());
        }
        if !e.is_empty() {
            auths.insert(server.clone(), Value::Object(e));
        }
    }
    (!auths.is_empty()).then(|| json!({"auths": auths}).to_string().into_bytes())
}

/// Docker's JSON progress lines, chunked to the client. A client that has
/// gone stops the writing, not the build's cleanup.
struct Out<'a> {
    w: &'a mut UnixStream,
    broken: bool,
}

impl Out<'_> {
    fn line(&mut self, v: Value) {
        if !self.broken && http::chunk(self.w, format!("{v}\r\n").as_bytes()).is_err() {
            self.broken = true;
        }
    }

    fn stream(&mut self, text: &str) {
        self.line(json!({"stream": text}));
    }
}

/// Reads what arrives on a channel, in order, until its senders are gone.
struct ChanReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    at: usize,
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.at == self.buf.len() {
            match self.rx.recv() {
                Ok(b) => (self.buf, self.at) = (b, 0),
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// What a context body is, from its first bytes.
#[derive(Debug, PartialEq)]
enum Context {
    Tar,
    /// tar's flag to read it: `-z`, `-j`, `-J`.
    Compressed(&'static str),
    Zstd,
    /// Not an archive: the Dockerfile itself (`docker build - < Dockerfile`
    /// from an SDK), as Docker takes it.
    Dockerfile,
}

fn sniff(head: &[u8]) -> Context {
    match head {
        [0x1f, 0x8b, ..] => Context::Compressed("-z"),
        [b'B', b'Z', b'h', ..] => Context::Compressed("-j"),
        [0xfd, b'7', b'z', b'X', b'Z', 0, ..] => Context::Compressed("-J"),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Context::Zstd,
        // Every tar Docker clients write is ustar or PAX (`ustar` at 257);
        // an empty body is an empty context.
        h if h.is_empty() || h.get(257..262) == Some(b"ustar") => Context::Tar,
        _ => Context::Dockerfile,
    }
}

/// `POST /build`: the classic builder API, run in the managed BuildKit.
pub fn build(
    req: &Request,
    engine: &Arc<Engine>,
    w: &mut UnixStream,
    r: &mut BufReader<UnixStream>,
) -> io::Result<Next> {
    let mut o = match Options::parse(req) {
        Ok(o) => o,
        Err(e) => {
            api::error(w, e)?;
            return Ok(Next::Close);
        }
    };
    // `host-gateway` was parsed as the Mac's address; on a Linux host it's
    // the bridge gateway, as `docker run` gives (kiri-init maps it there).
    let host = super::mirror::host_addr(engine);
    if host != super::ports::HOST_ADDR {
        let mac = format!("={}", super::ports::HOST_ADDR);
        for h in o.extra_hosts.iter_mut().filter(|h| h.ends_with(&mac)) {
            *h = format!("{}={host}", &h[..h.len() - mac.len()]);
        }
    }
    let _busy = Busy::new();
    http::begin_stream(w, 200, "application/json")?;
    let probe = w.try_clone()?;
    let began = Instant::now();
    let mut out = Out { w, broken: false };
    let result = run(req, engine, &mut o, r, &mut out, &probe);
    match result {
        Ok(images) => {
            let id = images[0].config_digest.clone();
            let short: String = id.trim_start_matches("sha256:").chars().take(12).collect();
            out.line(json!({"aux": {"ID": id}}));
            if o.quiet {
                out.stream(&format!("{id}\n"));
            } else {
                out.stream(&format!("Successfully built {short}\n"));
            }
            for i in images.iter().filter(|i| !i.name.starts_with("sha256:")) {
                engine.emit_image("tag", &i.config_digest, &i.name);
                if !o.quiet {
                    out.stream(&format!("Successfully tagged {}\n", i.name));
                }
            }
            crate::diag::info!(
                "image built",
                tags = o.tags.join(","),
                ms = began.elapsed().as_millis()
            );
        }
        Err(m) => {
            crate::diag::warn!("build failed", error = m);
            out.line(json!({"errorDetail": {"message": m}, "error": m}));
        }
    }
    if !out.broken {
        http::end(out.w)?;
    }
    // The body may be partly unread after a failure.
    Ok(Next::Close)
}

fn run(
    req: &Request,
    engine: &Engine,
    o: &mut Options,
    r: &mut BufReader<UnixStream>,
    out: &mut Out<'_>,
    probe: &UnixStream,
) -> Result<Vec<kiri_image::Image>, String> {
    let quiet = o.quiet;
    let id = ensure_running(engine, &mut |m| {
        if !quiet {
            out.stream(&format!("{m}\n"));
        }
    })
    .map_err(|e| match e {
        Error::NotFound(m) | Error::Conflict(m) | Error::BadRequest(m) | Error::Unauthorized(m) => {
            m
        }
        Error::Io(e) => e.to_string(),
    })?;
    let dir = format!("{WORK}/{}", super::engine::random_hex(8));
    run_ok(engine, &id, &["mkdir", "-p", &format!("{dir}/context")])?;
    let built = (|| {
        let env = match req.header("X-Registry-Config").and_then(registry_config) {
            Some(config) => {
                copy_in(
                    engine,
                    &id,
                    &dir,
                    &mut io::Cursor::new(tar(&[
                        ("docker/", None),
                        ("docker/config.json", Some(&config)),
                    ])),
                )
                .map_err(|e| format!("{e:?}"))?;
                vec![format!("DOCKER_CONFIG={dir}/docker")]
            }
            None => Vec::new(),
        };
        if o.remote.is_none() {
            send_context(engine, &id, &dir, req, r, o)?;
        }
        buildctl(engine, &id, &dir, o, env, out, probe)
    })();
    let _ = engine.exec_capture(&id, &["rm", "-rf", &dir]);
    built
}

/// The request body, unpacked into `dir/context` in the builder.
fn send_context(
    engine: &Engine,
    id: &str,
    dir: &str,
    req: &Request,
    r: &mut BufReader<UnixStream>,
    o: &mut Options,
) -> Result<(), String> {
    let mut body = http::Body::new(req, r);
    let mut head = Vec::with_capacity(512);
    (&mut body)
        .take(512)
        .read_to_end(&mut head)
        .map_err(|e| format!("reading the build context: {e}"))?;
    let ctx = format!("{dir}/context");
    match sniff(&head) {
        // Two zero blocks after the body end an archive whose last entry
        // isn't padded (Compose's classic builder sends those).
        Context::Tar => copy_in(
            engine,
            id,
            &ctx,
            &mut io::Cursor::new(head)
                .chain(body)
                .chain(io::repeat(0).take(1024)),
        )
        .map_err(|e| format!("unpacking the build context: {e:?}")),
        Context::Compressed(flag) => {
            let argv: Vec<String> = ["tar", "-x", flag, "-f", "-", "-C", &ctx]
                .map(str::to_owned)
                .to_vec();
            match exec_with_input(engine, id, &argv, &mut io::Cursor::new(head).chain(body)) {
                Ok((0, _)) => Ok(()),
                Ok((_, m)) => Err(format!("unpacking the build context: {}", m.trim())),
                Err(e) => Err(format!("unpacking the build context: {e:?}")),
            }
        }
        Context::Zstd => Err(
            "a zstd-compressed build context isn't supported: send it as tar, gzip, bzip2 or xz"
                .into(),
        ),
        Context::Dockerfile => {
            let mut text = head;
            body.take(MAX_DOCKERFILE)
                .read_to_end(&mut text)
                .map_err(|e| format!("reading the Dockerfile: {e}"))?;
            o.dockerfile = "Dockerfile".into();
            copy_in(
                engine,
                id,
                &ctx,
                &mut io::Cursor::new(tar(&[("Dockerfile", Some(&text))])),
            )
            .map_err(|e| format!("{e:?}"))
        }
    }
}

/// Runs the build: its progress (stderr) to the client as it comes, the
/// image archive (stdout) into Kiri's store as it comes.
fn buildctl(
    engine: &Engine,
    id: &str,
    dir: &str,
    o: &Options,
    env: Vec<String>,
    out: &mut Out<'_>,
    probe: &UnixStream,
) -> Result<Vec<kiri_image::Image>, String> {
    let argv = o.argv(dir);
    if !o.build_id.is_empty() {
        CANCELLABLE.lock().unwrap_or_else(|e| e.into_inner()).push((
            o.build_id.clone(),
            id.to_owned(),
            dir.to_owned(),
        ));
    }
    let built = buildctl_run(engine, id, dir, o, argv, env, out, probe);
    CANCELLABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(_, _, d)| d != dir);
    built
}

/// `POST /build/cancel?id=`: interrupts that build (BuildKit cancels on SIGINT).
pub fn cancel(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let id = req.query("id").unwrap_or_default();
    let found = CANCELLABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(b, _, _)| !id.is_empty() && b == id)
        .cloned();
    if let Some((_, container, dir)) = found {
        let _ = engine.exec_capture(&container, &["pkill", "-INT", "-f", &dir]);
    }
    Ok(Value::Null)
}

#[allow(clippy::too_many_arguments)]
fn buildctl_run(
    engine: &Engine,
    id: &str,
    dir: &str,
    o: &Options,
    argv: Vec<String>,
    env: Vec<String>,
    out: &mut Out<'_>,
    probe: &UnixStream,
) -> Result<Vec<kiri_image::Image>, String> {
    let exec = engine
        .exec_create(id, internal_exec(&argv, false, env))
        .map_err(|e| format!("{e:?}"))?;
    let (_, rx) = engine.exec_start(&exec).map_err(|e| format!("{e:?}"))?;
    let mut cancelled = false;
    let mut tail: Vec<String> = Vec::new();
    let loaded = std::thread::scope(|s| {
        let (tx, lrx) = mpsc::sync_channel::<Vec<u8>>(64);
        let loader = s.spawn(move || {
            kiri_image::archive::load(
                &engine.store,
                ChanReader {
                    rx: lrx,
                    buf: Vec::new(),
                    at: 0,
                },
            )
        });
        let mut tx = Some(tx);
        let mut partial = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok((Stream::Stdout, data)) => {
                    // A loader that stopped (a bad archive) is reported below.
                    if tx.as_ref().is_some_and(|t| t.send(data).is_err()) {
                        tx = None;
                    }
                }
                Ok((Stream::Stderr, data)) => {
                    partial.extend_from_slice(&data);
                    while let Some(nl) = partial.iter().position(|b| *b == b'\n') {
                        let line = String::from_utf8_lossy(&partial[..=nl]).into_owned();
                        partial.drain(..=nl);
                        if !o.quiet {
                            out.stream(&line);
                        }
                        tail.push(line.trim_end().to_owned());
                        if tail.len() > 40 {
                            tail.remove(0);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // `docker build` interrupted: BuildKit cancels on SIGINT.
            if !cancelled && (out.broken || super::ops::gone(probe)) {
                cancelled = true;
                out.broken = true;
                let _ = engine.exec_capture(id, &["pkill", "-INT", "-f", dir]);
            }
        }
        if !partial.is_empty() {
            let line = String::from_utf8_lossy(&partial).into_owned();
            if !o.quiet {
                out.stream(&format!("{line}\n"));
            }
            tail.push(line);
        }
        drop(tx);
        loader
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("the image loader panicked")))
    });
    let code = engine
        .with_exec(&exec, |e| e.exit_code.unwrap_or(-1))
        .unwrap_or(-1);
    engine.forget_exec(&exec);
    if cancelled {
        return Err("build cancelled".into());
    }
    if code != 0 {
        return Err(failure(&tail, code));
    }
    match loaded {
        Ok(images) if !images.is_empty() => Ok(images),
        Ok(_) => Err("BuildKit sent no image".into()),
        Err(e) => Err(format!("loading the built image into Kiri: {e}")),
    }
}

/// buildctl's own last word on a failed build (`error: failed to solve: …`).
fn failure(tail: &[String], code: i32) -> String {
    tail.iter()
        .rev()
        .find_map(|l| {
            l.strip_prefix("error: ")
                .or_else(|| l.strip_prefix("ERROR: "))
        })
        .map(str::to_owned)
        .or_else(|| tail.iter().rev().find(|l| !l.trim().is_empty()).cloned())
        .unwrap_or_else(|| format!("buildctl exited with code {code}"))
}

// --- Build cache -------------------------------------------------------------

/// `POST /build/prune`: BuildKit's prune, if the builder exists (started
/// for it if stopped). `{CachesDeleted, SpaceReclaimed}`.
pub fn prune(req: &Request, engine: &Engine) -> Result<Value, Error> {
    let f = api::filters(
        req,
        &[
            "until",
            "unused-for",
            "id",
            "parent",
            "type",
            "description",
            "inuse",
            "shared",
            "private",
        ],
    )?;
    let argv = prune_argv(req, &f)?;
    if find(engine).is_none() {
        return Ok(json!({"CachesDeleted": [], "SpaceReclaimed": 0}));
    }
    let _busy = Busy::new();
    let id = ensure_running(engine, &mut |_| {})?;
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let (code, text) = engine.exec_capture(&id, &argv)?;
    if code != 0 {
        let last = text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("buildctl prune failed");
        return Err(Error::Io(io::Error::other(
            last.trim_start_matches("error: ").to_owned(),
        )));
    }
    let (ids, space) = parse_prune(&text);
    Ok(json!({"CachesDeleted": ids, "SpaceReclaimed": space}))
}

fn prune_argv(req: &Request, f: &super::filters::Filters) -> Result<Vec<String>, Error> {
    let mut a: Vec<String> = ["buildctl", "prune", "--format", "{{json .}}"]
        .map(str::to_owned)
        .to_vec();
    if req.flag("all") {
        a.push("--all".into());
    }
    // Docker's sizes are bytes; buildctl's are MB (10^6).
    let mb = |key: &str| {
        req.query(key)
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .map(|v| format!("{}", v as f64 / 1e6))
    };
    for (key, flag) in [
        ("keep-storage", "--keep-storage"),
        ("max-used-space", "--keep-storage"),
        ("reserved-space", "--keep-storage-min"),
        ("min-free-space", "--free-storage"),
    ] {
        if let Some(v) = mb(key)
            && !a.iter().any(|x| x == flag)
        {
            a.extend([flag.to_owned(), v]);
        }
    }
    for key in ["until", "unused-for"] {
        if let Some(v) = f.values(key).and_then(|v| v.first()) {
            let ok = v.len() > 1
                && v.ends_with(['h', 'm', 's'])
                && v[..v.len() - 1]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b"hms.".contains(&b));
            if !ok {
                return Err(Error::BadRequest(format!(
                    "{key}: give a duration such as 24h"
                )));
            }
            a.extend(["--keep-duration".into(), v.clone()]);
        }
    }
    for (key, as_) in [
        ("id", "id~="),
        ("parent", "parents=="),
        ("type", "type=="),
        ("description", "description=="),
    ] {
        for v in f.values(key).into_iter().flatten() {
            a.extend(["--filter".into(), format!("{as_}{v}")]);
        }
    }
    for key in ["inuse", "shared", "private"] {
        if f.bool(key) == Some(true)
            || f.values(key)
                .is_some_and(|v| v.iter().any(String::is_empty))
        {
            a.extend(["--filter".into(), key.to_owned()]);
        }
    }
    Ok(a)
}

/// `buildctl prune --format '{{json .}}'`: one record per line.
fn parse_prune(text: &str) -> (Vec<String>, u64) {
    let mut ids = Vec::new();
    let mut space = 0;
    for v in text
        .lines()
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        if let Some(id) = v["ID"].as_str() {
            ids.push(id.to_owned());
        }
        space += v["Size"].as_u64().unwrap_or(0);
    }
    (ids, space)
}

/// `system df`'s BuildCache: BuildKit's records while the builder runs
/// (asking a stopped one would start it); none otherwise.
pub fn cache(engine: &Engine) -> Value {
    let Some((id, Status::Running)) = find(engine) else {
        return json!([]);
    };
    let _busy = Busy::new();
    match engine.exec_capture(&id, &["buildctl", "du", "--format", "{{json .}}"]) {
        Ok((0, text)) => du_rows(&text),
        _ => json!([]),
    }
}

/// buildctl's usage records as Docker's BuildCache entries.
fn du_rows(text: &str) -> Value {
    let start = text.find('[').unwrap_or(text.len());
    let records: Vec<Value> = serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Vec<Value>>()
        .next()
        .and_then(Result::ok)
        .unwrap_or_default();
    Value::Array(
        records
            .iter()
            .map(|r| {
                json!({
                    "ID": r["ID"], "Parent": r["Parents"][0], "Parents": r["Parents"], "Type": r["RecordType"],
                    "Description": r["Description"], "InUse": r["InUse"].as_bool().unwrap_or(false), "Shared": r["Shared"].as_bool().unwrap_or(false),
                    "Size": r["Size"].as_i64().unwrap_or(0), "CreatedAt": r["CreatedAt"], "LastUsedAt": r["LastUsedAt"], "UsageCount": r["UsageCount"].as_i64().unwrap_or(0),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BuildKit looks the emulator up by name and skips it when it isn't
    /// executable, so the file taking its place must be a plain 0644 file.
    #[test]
    fn hidden_amd64_emulator_is_not_executable() {
        let archive = tar(&[(AMD64_QEMU, Some(&[]))]);
        assert_eq!(&archive[..AMD64_QEMU.len() + 1], format!("{AMD64_QEMU}\0").as_bytes());
        assert_eq!(&archive[100..108], b"0000644\0");
        assert_eq!(archive[156], b'0');
        assert_eq!(&archive[124..136], b"00000000000\0");
    }

    fn req(query: &[(&str, &str)]) -> Request {
        Request::internal("POST", "/build", query, Vec::new())
    }

    #[test]
    fn reads_the_build_query() {
        let o = Options::parse(&req(&[
            ("t", "app"),
            ("t", "ghcr.io/o/app:1"),
            ("dockerfile", "./docker/Dockerfile.dev"),
            ("buildargs", r#"{"B":"2","A":"1","UNSET":null}"#),
            ("labels", r#"{"team":"core"}"#),
            ("nocache", "1"),
            ("pull", "1"),
            ("q", "1"),
            ("platform", "linux/AMD64"),
            ("networkmode", "host"),
            ("extrahosts", "db:10.0.0.2"),
            ("extrahosts", "host.docker.internal:host-gateway"),
            ("shmsize", "67108864"),
            ("cachefrom", r#"["ghcr.io/o/app:cache"]"#),
            ("ulimits", r#"[{"Name":"nofile","Soft":1024,"Hard":2048}]"#),
            ("target", "dev"),
            ("rm", "1"),
            ("forcerm", "0"),
        ]))
        .unwrap();
        assert_eq!(o.tags, ["app:latest", "ghcr.io/o/app:1"]);
        assert_eq!(o.dockerfile, "docker/Dockerfile.dev");
        assert_eq!(
            o.build_args,
            [
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "2".to_owned())
            ]
        );
        assert!(o.no_cache && o.pull && o.quiet);
        assert_eq!(o.platform, "linux/amd64");
        assert_eq!(
            o.extra_hosts,
            [
                "db=10.0.0.2".to_owned(),
                format!("host.docker.internal={}", super::super::ports::HOST_ADDR)
            ]
        );
        let a = o.argv("/tmp/kiri-build/x");
        let has = |pair: &[&str]| a.windows(pair.len()).any(|w| w == pair);
        assert!(has(&["--opt", "filename=Dockerfile.dev"]));
        assert!(has(&[
            "--local",
            "context=/tmp/kiri-build/x/context",
            "--local",
            "dockerfile=/tmp/kiri-build/x/context/docker"
        ]));
        assert!(
            has(&["--opt", "build-arg:A=1"])
                && has(&["--opt", "label:team=core"])
                && has(&["--opt", "target=dev"])
        );
        assert!(
            has(&["--opt", "image-resolve-mode=pull"])
                && has(&["--no-cache"])
                && has(&["--opt", "platform=linux/amd64"])
        );
        assert!(has(&["--opt", "force-network-mode=host"]) && has(&["--allow", "network.host"]));
        assert!(has(&["--opt", "shm-size=67108864"]) && has(&["--opt", "ulimit=nofile=1024:2048"]));
        assert!(has(&[
            "--import-cache",
            "type=registry,ref=ghcr.io/o/app:cache"
        ]));
        assert_eq!(
            a.last().unwrap(),
            "type=docker,\"name=app:latest,ghcr.io/o/app:1\",dest=-"
        );
        assert!(!a.iter().any(|x| x.contains("UNSET")));
    }

    #[test]
    fn plain_build_and_refusals() {
        let o = Options::parse(&req(&[])).unwrap();
        assert_eq!(o.dockerfile, "Dockerfile");
        let a = o.argv("/d");
        assert!(
            a.windows(2)
                .any(|w| w == ["--local", "dockerfile=/d/context"])
        );
        assert_eq!(a.last().unwrap(), "type=docker,dest=-");
        assert!(Options::parse(&req(&[("t", "Bad:Tag!")])).is_err());
        assert!(Options::parse(&req(&[("dockerfile", "../Dockerfile")])).is_err());
        assert!(Options::parse(&req(&[("networkmode", "mynet")])).is_err());
        assert!(Options::parse(&req(&[("version", "2")])).is_err());
        let remote = Options::parse(&req(&[("remote", "https://github.com/o/r.git#main")]))
            .unwrap()
            .argv("/d");
        assert!(
            remote
                .windows(2)
                .any(|w| w == ["--opt", "context=https://github.com/o/r.git#main"])
        );
        assert!(!remote.iter().any(|x| x == "--local"));
    }

    #[test]
    fn sniffs_the_context() {
        let t = tar(&[("Dockerfile", Some(b"FROM alpine\n"))]);
        assert_eq!(sniff(&t[..512]), Context::Tar);
        assert_eq!(sniff(&[0x1f, 0x8b, 8]), Context::Compressed("-z"));
        assert_eq!(sniff(b"BZh91AY"), Context::Compressed("-j"));
        assert_eq!(
            sniff(&[0xfd, b'7', b'z', b'X', b'Z', 0]),
            Context::Compressed("-J")
        );
        assert_eq!(sniff(b"FROM alpine\nRUN true\n"), Context::Dockerfile);
        assert_eq!(sniff(b""), Context::Tar);
        // The archive reads back with the tar crate (as the guest's would).
        let mut a = ::tar::Archive::new(io::Cursor::new(t));
        let mut e = a.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(e.path().unwrap().to_str(), Some("Dockerfile"));
        let mut s = String::new();
        e.read_to_string(&mut s).unwrap();
        assert_eq!(s, "FROM alpine\n");
    }

    #[test]
    fn registry_config_becomes_docker_config() {
        let header = kiri_image::b64::encode(br#"{"https://index.docker.io/v1/":{"username":"u","password":"p"},"ghcr.io":{"identitytoken":"tok"},"empty":{}}"#);
        let v: Value = serde_json::from_slice(&registry_config(&header).unwrap()).unwrap();
        assert_eq!(
            v["auths"]["https://index.docker.io/v1/"]["auth"],
            kiri_image::b64::encode(b"u:p")
        );
        assert_eq!(v["auths"]["ghcr.io"]["identitytoken"], "tok");
        assert!(v["auths"].get("empty").is_none());
    }

    #[test]
    fn prune_and_du() {
        let r = Request::internal(
            "POST",
            "/build/prune",
            &[
                ("all", "1"),
                ("keep-storage", "5000000000"),
                ("filters", r#"{"until":["24h"],"type":["regular"]}"#),
            ],
            Vec::new(),
        );
        let f = api::filters(&r, &["until", "type"]).unwrap();
        let a = prune_argv(&r, &f).unwrap();
        let has = |pair: &[&str]| a.windows(pair.len()).any(|w| w == pair);
        assert!(
            has(&["--all"])
                && has(&["--keep-storage", "5000"])
                && has(&["--keep-duration", "24h"])
                && has(&["--filter", "type==regular"])
        );
        let bad = Request::internal(
            "POST",
            "/build/prune",
            &[("filters", r#"{"until":["2026-01-01"]}"#)],
            Vec::new(),
        );
        assert!(prune_argv(&bad, &api::filters(&bad, &["until"]).unwrap()).is_err());
        let (ids, space) =
            parse_prune("{\"ID\":\"a\",\"Size\":10}\n{\"ID\":\"b\",\"Size\":5}\nTotal: 15B\n");
        assert_eq!((ids, space), (vec!["a".to_owned(), "b".to_owned()], 15));
        let rows = du_rows(
            r#"[{"ID":"x","Mutable":false,"InUse":true,"Size":42,"Parents":["p"],"CreatedAt":"2026-10-30T10:00:00Z","LastUsedAt":null,"UsageCount":3,"Description":"mount / from exec","RecordType":"regular","Shared":false}]"#,
        );
        assert_eq!(rows[0]["ID"], "x");
        assert_eq!(rows[0]["Parent"], "p");
        assert_eq!(rows[0]["Type"], "regular");
        assert_eq!(rows[0]["Size"], 42);
        assert_eq!(rows[0]["InUse"], true);
    }

    #[test]
    fn cache_cap_and_config() {
        assert_eq!(default_cache(0), 20 * GB);
        assert_eq!(default_cache(100 * GB), 10 * GB);
        assert_eq!(default_cache(2000 * GB), 20 * GB);
        assert!(config_toml(8 * GB, "").contains(&format!("maxUsedSpace = {}", 8 * GB)));
        assert_eq!(
            failure(
                &[
                    "#5 ERROR: boom".into(),
                    "error: failed to solve: boom".into()
                ],
                1
            ),
            "failed to solve: boom"
        );
        assert_eq!(failure(&[], 2), "buildctl exited with code 2");
    }
}
