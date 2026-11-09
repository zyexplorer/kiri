//! Containers, execs and images as the daemon tracks them.
//!
//! Container records are saved to `containers.json` on every change (except
//! `--rm` containers, which are gone by the time anyone could ask), so `docker
//! ps -a` survives a daemon restart. Every state change notifies one
//! condition variable, which is what `wait` blocks on.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use kiri_image::{Image, Store};
use kiri_proto::{Stream, run_flags};

use super::network::Networks;
use super::vmlink::{Event, VmLink};

/// Dynamic memory: the VM boots with a ceiling, and the balloon keeps the
/// guest at what it uses plus this much, returning the rest to macOS.
const MEMORY_HEADROOM: u64 = 256 << 20;
const MEMORY_FLOOR: u64 = 256 << 20;
/// How much more the guest gets at once when it reports pressure.
const MEMORY_GROW: u64 = 1 << 30;

/// What a request that needs the VM gets while Kiri is stopped.
pub const STOPPED: &str = "Kiri is stopped: press Start in the Kiri window to run containers again";

/// The file that keeps "stopped" across engine restarts.
fn stopped_marker() -> Option<std::path::PathBuf> {
    super::state_dir().ok().map(|d| d.join("stopped"))
}

/// The VM's memory ceiling: `KIRI_MEMORY_MIB`, else Settings → Resources,
/// else half the Mac's RAM up to 8 GiB.
pub fn memory_ceiling() -> u64 {
    if let Some(mib) = std::env::var("KIRI_MEMORY_MIB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return mib << 20;
    }
    if let Some(mib) = resources()["memory_mib"].as_u64() {
        return (mib << 20).clamp(1 << 30, physical_memory());
    }
    (physical_memory() / 2).clamp(2 << 30, 8 << 30)
}

/// The VM's CPUs: `KIRI_CPUS`, else Settings → Resources, else 2.
pub fn vm_cpus() -> usize {
    let max = max_cpus();
    std::env::var("KIRI_CPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| resources()["cpus"].as_u64().map(|c| c as usize))
        .unwrap_or(2)
        .clamp(1, max)
}

/// The Mac's CPUs, the most the VM can have.
pub fn max_cpus() -> usize {
    std::thread::available_parallelism().map_or(2, std::num::NonZero::get)
}

/// What Settings → Resources saved, read at each boot: `{"cpus", "memory_mib"}`.
pub fn resources_path() -> Option<std::path::PathBuf> {
    super::state_dir().ok().map(|d| d.join("vm.json"))
}

pub fn resources() -> serde_json::Value {
    resources_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(serde_json::Value::Null)
}

pub fn physical_memory_bytes() -> u64 {
    physical_memory()
}

#[cfg(target_os = "macos")]
fn physical_memory() -> u64 {
    let mut ram: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: sysctlbyname writes a u64 into `ram`.
    unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut ram as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    ram
}

/// This process's resident memory, in bytes: on a Linux host the daemon runs
/// beside its containers, so it counts in what Kiri holds.
#[cfg(not(target_os = "macos"))]
fn self_rss() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // SAFETY: sysconf takes no pointers.
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64
}

#[cfg(target_os = "macos")]
fn self_rss() -> u64 {
    0
}

#[cfg(not(target_os = "macos"))]
fn physical_memory() -> u64 {
    let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    info.lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(4 << 30, |k| k << 10)
}

/// Docker's restart backoff: 100 ms doubling, capped at one minute.
const RESTART_BASE: Duration = Duration::from_millis(100);
const RESTART_CAP: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Created,
    Running,
    Exited,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Created => "created",
            Status::Running => "running",
            Status::Exited => "exited",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartPolicy {
    /// `no`, `always`, `unless-stopped` or `on-failure`.
    pub name: String,
    pub max_retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub image: String,
    pub image_id: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub user: String,
    pub auto_remove: bool,
    pub network_mode: String,
    pub labels: serde_json::Value,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub open_stdin: bool,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Initial terminal size (rows, cols), from `HostConfig.ConsoleSize`.
    #[serde(default)]
    pub console_size: Option<(u16, u16)>,
    /// Extra names on its network (`--network-alias`, Compose service names).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// `--add-host` lines for /etc/hosts, already as `ip name`.
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    /// `-p` bindings.
    #[serde(default)]
    pub ports: Vec<super::ports::Binding>,
    /// TCP ports the image (or `--expose`) declares; picks the domain's port.
    #[serde(default)]
    pub exposed: Vec<u16>,
    /// Bind mounts as the guest takes them: `source\ttarget\tflags`, where
    /// `source` is a path inside the VM.
    #[serde(default)]
    pub mounts: Vec<String>,
    /// For `inspect`: Docker's `Mounts` entries.
    #[serde(default)]
    pub mount_info: Vec<serde_json::Value>,
    /// Named volumes it uses (kept while it exists), and the anonymous ones
    /// created for it (deleted with it on `--rm` or `rm -v`).
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub anon_volumes: Vec<String>,
    /// `--privileged`.
    #[serde(default)]
    pub privileged: bool,
    /// `--memory`, `--cpus`, `--cpu-shares`, `--pids-limit`; `docker update` changes them.
    #[serde(default)]
    pub limits: Limits,
    /// `--cap-add` / `--cap-drop` applied to Docker's default set, one bit
    /// per capability; 0 means the default set.
    #[serde(default)]
    pub caps: u64,
    /// `--cap-add`/`--cap-drop` as given, for inspect.
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub cap_drop: Vec<String>,
    /// `--security-opt` entries as given.
    #[serde(default)]
    pub security_opt: Vec<String>,
    /// `--read-only`.
    #[serde(default)]
    pub readonly_rootfs: bool,
    /// `--hostname`; empty means the short container id, as Docker does.
    #[serde(default)]
    pub hostname: String,
    /// `--stop-signal` (image STOPSIGNAL); SIGTERM if not set.
    #[serde(default)]
    pub stop_signal: Option<u32>,
    /// `--stop-timeout`, seconds (-1: wait for ever); the stop request's own wins.
    #[serde(default)]
    pub stop_timeout: Option<i64>,
    /// `Config.Healthcheck` merged over the image's, as inspect shows it.
    #[serde(default)]
    pub healthcheck: Option<super::health::Healthcheck>,
    /// `Config.Shell` (or the image's SHELL) for `CMD-SHELL` healthchecks.
    #[serde(default)]
    pub shell: Vec<String>,
    /// `--init` (`HostConfig.Init`): a small init as PID 1 that forwards
    /// signals and reaps zombies.
    #[serde(default)]
    pub init: Option<bool>,
    /// Networks beyond `network_mode` (`docker network connect`, Compose):
    /// name and aliases on it.
    #[serde(default)]
    pub extra_networks: Vec<ExtraNetwork>,
    /// `--log-driver` / `--log-opt`; containers from before it have the default.
    #[serde(default)]
    pub log_config: super::logfile::LogConfig,
    /// `--ip` / Compose `ipv4_address` on `network_mode`; empty for any.
    #[serde(default)]
    pub ipv4_address: String,
    /// `--ip6` / Compose `ipv6_address` on `network_mode`; empty for any.
    #[serde(default)]
    pub ipv6_address: String,
    /// `--mac-address` on `network_mode`; empty for the kernel's choice.
    #[serde(default)]
    pub mac_address: String,
    /// `--dns`, `--dns-search`, `--dns-option`.
    #[serde(default)]
    pub dns: Vec<String>,
    #[serde(default)]
    pub dns_search: Vec<String>,
    #[serde(default)]
    pub dns_options: Vec<String>,
    /// `--domainname`.
    #[serde(default)]
    pub domainname: String,
    /// `--link` / Compose `links`: /etc/hosts entries for these containers.
    #[serde(default)]
    pub links: Vec<Link>,
    /// HostConfig's Ulimits, Sysctls, Devices, DeviceCgroupRules and
    /// GroupAdd as given; see [`super::hostopts`].
    #[serde(default)]
    pub host_opts: serde_json::Value,
}

/// One `--link name:alias`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// The linked container's id, and its name when the link was made.
    pub id: String,
    pub name: String,
    pub alias: String,
}

/// cgroup limits as saved with a container; see [`kiri_proto::Limits`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub memory_max: u64,
    pub cpu_quota: u64,
    pub cpu_period: u32,
    pub cpu_weight: u32,
    pub pids_max: u32,
    #[serde(default)]
    pub shm_size: u64,
}

impl Limits {
    pub fn wire(&self) -> kiri_proto::Limits {
        kiri_proto::Limits {
            memory_max: self.memory_max,
            cpu_quota: self.cpu_quota,
            cpu_period: self.cpu_period,
            cpu_weight: self.cpu_weight,
            pids_max: self.pids_max,
            shm_size: self.shm_size,
        }
    }
}

impl Spec {
    /// The guest's `opts` lines for a start or an exec. `net_host`: it runs
    /// in the VM's (or host's) network namespace.
    pub fn guest_opts(&self, net_host: bool) -> Result<Vec<String>, Error> {
        super::hostopts::guest_lines(&self.host_opts, net_host).map_err(Error::BadRequest)
    }

    /// Whether `--sysctl net.ipv6.conf.{all,default,<ifname>}.disable_ipv6=1`
    /// turns IPv6 off on `ifname`: then it gets no IPv6 address (and no AAAA
    /// record), as with Docker.
    pub fn ipv6_disabled(&self, ifname: &str) -> bool {
        super::hostopts::ipv6_disabled(&self.host_opts, ifname)
    }

    /// The security flags for the guest: seccomp, no-new-privileges, read-only root.
    pub fn security_flags(&self) -> u32 {
        let mut f = 0;
        if self
            .security_opt
            .iter()
            .any(|o| o == "seccomp=unconfined" || o == "seccomp:unconfined")
        {
            f |= run_flags::NO_SECCOMP;
        }
        if self.security_opt.iter().any(|o| {
            o == "no-new-privileges"
                || o == "no-new-privileges:true"
                || o == "no-new-privileges=true"
        }) {
            f |= run_flags::NO_NEW_PRIVS;
        }
        if self.readonly_rootfs {
            f |= run_flags::READONLY_ROOTFS;
        }
        f
    }
}

/// A container on a network: (id, name, ip, IPv6 ip).
pub type Member = (String, String, Option<[u8; 4]>, Option<[u8; 16]>);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraNetwork {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// A fixed address on it (`IPAMConfig.IPv4Address`); empty for any.
    #[serde(default)]
    pub ipv4_address: String,
    /// A fixed IPv6 address on it (`IPAMConfig.IPv6Address`); empty for any.
    #[serde(default)]
    pub ipv6_address: String,
    #[serde(default)]
    pub mac_address: String,
}

#[derive(Debug)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub spec: Spec,
    pub layers: Vec<String>,
    pub created: SystemTime,
    pub status: Status,
    pub started: Option<SystemTime>,
    pub finished: Option<SystemTime>,
    pub exit_code: i32,
    /// Its last run ended with the kernel killing it for memory.
    pub oom_killed: bool,
    /// Its last run ended because the VM or the engine stopped under it, not
    /// because the app exited: Docker's exit code 255, but nothing failed.
    pub vm_stopped: bool,
    /// Set by `stop`/`kill`: no automatic restart until started again.
    pub stopped_by_user: bool,
    pub restart_count: u32,
    /// How many times it has exited; `wait?condition=next-exit` waits for this to grow.
    exits: u64,
    /// Terminal size asked for before the process existed.
    size: Option<(u16, u16)>,
    /// Its address while running on a bridge network.
    pub ip: Option<[u8; 4]>,
    /// Its first process while running, as the agent's kernel numbers it
    /// (the host's on a Linux host, the VM's otherwise); 0 when not running.
    pub pid: u32,
    /// Published ports while running.
    pub forwards: Option<super::ports::Forwards>,
    /// Frozen by `docker pause`.
    pub paused: bool,
    /// Addresses on `spec.extra_networks` while running, as (network, ip).
    pub extra_ips: Vec<(String, [u8; 4])>,
    /// IPv6 addresses while running: on `network_mode`, and on extra
    /// networks as (network, ip). Only networks with IPv6 give one.
    pub ip6: Option<[u8; 16]>,
    pub extra_ip6s: Vec<(String, [u8; 16])>,
    /// True while the guest has the process. Until then, input is held here:
    /// `docker run -i` attaches and sends stdin before it starts the container.
    live: bool,
    early_input: Vec<u8>,
    early_close: bool,
    /// `docker logs`: its output on disk, and who follows it.
    log: Arc<Mutex<super::logfile::Log>>,
    attached: Vec<Sender<(Stream, Vec<u8>)>>,
}

impl Container {
    pub fn argv(&self) -> Vec<String> {
        self.spec
            .entrypoint
            .iter()
            .chain(&self.spec.cmd)
            .cloned()
            .collect()
    }

    pub fn short_id(&self) -> &str {
        &self.id[..12]
    }
}

#[derive(Debug, Clone)]
pub struct ExecConfig {
    /// Initial terminal size (rows, cols).
    pub console_size: Option<(u16, u16)>,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub user: String,
    pub tty: bool,
    pub stdin: bool,
    /// `exec --privileged`, or any exec in a privileged container.
    pub privileged: bool,
    /// Kiri's own (the Kubernetes service watcher): no events.
    pub internal: bool,
}

#[derive(Debug)]
pub struct Exec {
    pub id: String,
    pub container_id: String,
    pub config: ExecConfig,
    pub running: bool,
    pub exit_code: Option<i32>,
    /// Its process while running; 0 otherwise.
    pub pid: u32,
    attached: Vec<Sender<(Stream, Vec<u8>)>>,
}

/// What `docker logs` reads: the stored part, then (with `follow`) what comes after it.
pub struct Logs {
    pub tty: bool,
    pub past: super::logfile::Snapshot,
    pub live: Option<Receiver<super::logfile::Record>>,
}

fn lock_log(l: &Mutex<super::logfile::Log>) -> MutexGuard<'_, super::logfile::Log> {
    l.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `containers.json` holds for one container.
#[derive(Serialize, Deserialize)]
struct Saved {
    id: String,
    name: String,
    spec: Spec,
    layers: Vec<String>,
    created_ns: u64,
    status: Status,
    started_ns: Option<u64>,
    finished_ns: Option<u64>,
    exit_code: i32,
    #[serde(default)]
    oom_killed: bool,
    #[serde(default)]
    vm_stopped: bool,
    stopped_by_user: bool,
    restart_count: u32,
    #[serde(default)]
    health: Option<super::health::Health>,
}

/// A container's healthcheck while it has one: its `State.Health`, and
/// whether a monitor thread is probing. `epoch` changes whenever the monitor
/// should stop, so a thread that sees a different one quits.
#[derive(Debug, Default)]
struct HealthRun {
    health: super::health::Health,
    epoch: u64,
    monitoring: bool,
}

#[derive(Default)]
struct State {
    containers: HashMap<String, Container>,
    execs: HashMap<String, Exec>,
    /// Exit codes of recently removed containers, for `wait?condition=removed`
    /// (that is how `docker run --rm` learns the exit status). Bounded.
    removed: VecDeque<(String, i32)>,
    /// `State.Health` of containers with a healthcheck, by id.
    health: HashMap<String, HealthRun>,
}

/// Set while the daemon shuts down: exits then are not restarts.
static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `docker stop`'s default wait, when neither the request nor the container sets one.
pub const DEFAULT_STOP_TIMEOUT: i64 = 10;

/// A stop timeout in seconds as a wait; negative means as long as it takes.
pub fn stop_wait(secs: i64) -> Duration {
    if secs < 0 {
        Duration::from_secs(365 * 86_400)
    } else {
        Duration::from_secs(secs as u64)
    }
}

pub struct Engine {
    pub volumes: Mutex<super::volumes::Volumes>,
    /// The VM, while it is awake. It sleeps (powers off, freeing all its
    /// memory) when idle, and boots again on the next request.
    link: Mutex<Option<Arc<VmLink>>>,
    /// Unix seconds of the last API request; drives the idle sleep.
    last_activity: std::sync::atomic::AtomicU64,
    /// Set while the VM is being put to sleep, so its stop is not an error.
    sleeping: std::sync::atomic::AtomicBool,
    /// Stopped from the window: the VM stays off, whatever asks for it
    /// (a build, a docker command, a share), until Start. Unlike the idle
    /// sleep, which the next request wakes. Kept across engine restarts by
    /// a file in the state dir (`stopped`).
    stopped: std::sync::atomic::AtomicBool,
    pub store: Store,
    pub networks: Mutex<Networks>,
    state: Mutex<State>,
    changed: Condvar,
    /// Serialises sending layers, so two creates of a new image don't send
    /// the same layers twice.
    layers: Mutex<()>,
    records: PathBuf,
    /// Serialises writes of `containers.json`.
    saving: Mutex<()>,
    me: OnceLock<Weak<Engine>>,
    /// Memory ceiling and the balloon's current target, in bytes. The
    /// ceiling is read again at each boot, so Settings applies on restart.
    ceiling: std::sync::atomic::AtomicU64,
    /// CPUs the running VM has.
    cpus: std::sync::atomic::AtomicUsize,
    memory_target: Mutex<u64>,
    /// What the guest last used, in bytes (0 while asleep): the balloon's
    /// measure, without the pages the balloon itself holds.
    memory_used: std::sync::atomic::AtomicU64,
    /// How starved of memory the guest is: PSI "full avg10" in hundredths
    /// of a percent (0 while asleep), and processes killed for memory since it booted.
    memory_stall: std::sync::atomic::AtomicU64,
    oom_kills: std::sync::atomic::AtomicU64,
    /// How busy the VM was over the last couple of seconds, in hundredths of a
    /// percent of all its CPUs (0 while asleep).
    cpu_permyriad: std::sync::atomic::AtomicU32,
    /// `docker events`: recent ones for `--since`, and who is listening.
    events: Mutex<Events>,
    /// Kubernetes LoadBalancer services published on the Mac's localhost:
    /// (port, protocol) → its forward.
    k8s_lb: Mutex<HashMap<(u16, String), super::ports::Forwards>>,
    /// The cluster's pod and service ranges routed to the node, in which VM
    /// (its link's address) and through which node address.
    k8s_routes: Mutex<Option<(usize, [u8; 4])>>,
    /// Shares of Kubernetes services and pods: the Mac port → its forward
    /// to a cluster address. They go when the cluster does.
    k8s_fwd: Mutex<HashMap<u16, super::ports::Forwards>>,
}

/// k3s's pod and service ranges, which `kiri k8s` leaves at their defaults.
pub const K8S_RANGES: [([u8; 4], u8); 2] = [([10, 42, 0, 0], 16), ([10, 43, 0, 0], 16)];

/// Whether an address is in the cluster's pod or service range.
pub fn in_k8s_ranges(ip: [u8; 4]) -> bool {
    K8S_RANGES.iter().any(|(net, bits)| {
        let mask = u32::MAX << (32 - *bits as u32);
        u32::from_be_bytes(ip) & mask == u32::from_be_bytes(*net)
    })
}

/// Events kept for `docker events --since`.
const RECENT_EVENTS: usize = 512;
/// Events queued for a slow listener before it starts missing some.
const EVENT_QUEUE: usize = 256;

#[derive(Default)]
struct Events {
    recent: VecDeque<serde_json::Value>,
    listeners: Vec<std::sync::mpsc::SyncSender<serde_json::Value>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Engine")
    }
}

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Conflict(String),
    BadRequest(String),
    Unauthorized(String),
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

fn ns(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn from_ns(n: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(n)
}

impl Engine {
    /// Boots the guest, loads saved containers and restarts the ones whose
    /// restart policy says so.
    pub fn boot(store: Store) -> io::Result<Arc<Engine>> {
        let records = store.root().join("containers.json");
        let ceiling = memory_ceiling();
        let networks = Mutex::new(Networks::load(store.root())?);
        let volumes = Mutex::new(super::volumes::Volumes::load(store.root())?);
        let engine = Arc::new(Engine {
            volumes,
            link: Mutex::new(None),
            last_activity: std::sync::atomic::AtomicU64::new(now_secs()),
            sleeping: std::sync::atomic::AtomicBool::new(false),
            stopped: std::sync::atomic::AtomicBool::new(
                stopped_marker().is_some_and(|f| f.exists()),
            ),
            networks,
            store,
            state: Mutex::default(),
            changed: Condvar::new(),
            layers: Mutex::new(()),
            records,
            saving: Mutex::new(()),
            me: OnceLock::new(),
            ceiling: std::sync::atomic::AtomicU64::new(ceiling),
            cpus: std::sync::atomic::AtomicUsize::new(vm_cpus()),
            memory_target: Mutex::new(ceiling),
            memory_used: std::sync::atomic::AtomicU64::new(0),
            memory_stall: std::sync::atomic::AtomicU64::new(0),
            oom_kills: std::sync::atomic::AtomicU64::new(0),
            cpu_permyriad: std::sync::atomic::AtomicU32::new(0),
            events: Mutex::new(Events::default()),
            k8s_lb: Mutex::new(HashMap::new()),
            k8s_routes: Mutex::new(None),
            k8s_fwd: Mutex::new(HashMap::new()),
        });
        let _ = engine.me.set(Arc::downgrade(&engine));
        engine.load()?;
        engine.start_balloon();
        engine.start_cpu_meter();
        engine.start_sleeper();
        engine.start_k8s_lb();
        Ok(engine)
    }

    /// The running VM, booting it first if it is asleep (about 0.1 s).
    pub fn vm(&self) -> io::Result<Arc<VmLink>> {
        let mut link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(vm) = link.as_ref() {
            return Ok(vm.clone());
        }
        if self.is_stopped() {
            return Err(io::Error::other(STOPPED));
        }
        let (ceiling, cpus) = (memory_ceiling(), vm_cpus());
        self.ceiling
            .store(ceiling, std::sync::atomic::Ordering::Relaxed);
        self.cpus.store(cpus, std::sync::atomic::Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        let guest = crate::guest::Guest::boot(&crate::guest::BootOptions {
            data_disk: true,
            memory_mib: Some(ceiling >> 20),
            cpus: Some(cpus),
            ..Default::default()
        })
        .inspect_err(|e| crate::diag::error!("VM did not boot", error = e))?;
        let (vm, conn) = guest.into_parts();
        let me = self.me.get().cloned().unwrap_or_default();
        let vm = Arc::new(VmLink::new(vm, conn, move |ev| {
            if let Some(engine) = me.upgrade() {
                engine.on_event(ev);
            }
        })?);
        // host.docker.internal: the guest dials back for each connection.
        vm.listen(
            super::ports::HOST_BOUND_PORT,
            Box::new(super::ports::serve_host_bound),
        )?;
        // SSH agent forwarding, for containers that mount /run/host-services/ssh-auth.sock.
        vm.listen(
            super::ports::SSH_AGENT_PORT,
            Box::new(super::ports::serve_ssh_agent),
        )?;
        // The engine's API, for containers that mount /var/run/docker.sock
        // (a Linux host's containers mount the daemon's socket itself).
        if cfg!(target_os = "macos") {
            vm.listen(
                super::ports::ENGINE_API_PORT,
                Box::new(super::ports::serve_engine_api),
            )?;
        }
        // Container lookups the VM's gateway resolver fails, asked of the
        // Mac's own (VM only: a Linux host's agent asks the host's itself).
        if cfg!(target_os = "macos") {
            vm.listen(
                kiri_proto::DNS_RELAY_PORT,
                Box::new(super::dns::serve_relay),
            )?;
        }
        // Everything else on the guest's disk belongs to containers that are gone.
        let keep: Vec<String> = self.lock().containers.keys().cloned().collect();
        vm.retain(&keep)?;
        // And every unpacked layer no image or container uses. Skipped if
        // the image list can't be read, rather than deleting everything.
        match self.store.images() {
            Ok(images) => {
                let mut layers: std::collections::BTreeSet<String> = images
                    .iter()
                    .flat_map(|i| i.layers.iter().map(|l| l.digest.clone()))
                    .collect();
                layers.extend(
                    self.lock()
                        .containers
                        .values()
                        .flat_map(|c| c.layers.iter().cloned()),
                );
                vm.retain_layers(&layers.into_iter().collect::<Vec<_>>())?;
            }
            Err(e) => crate::diag::warn!("kept every unpacked layer: images unreadable", error = e),
        }
        *self.memory_target.lock().unwrap_or_else(|e| e.into_inner()) = ceiling;
        self.sleeping
            .store(false, std::sync::atomic::Ordering::SeqCst);
        *link = Some(vm.clone());
        crate::diag::info!(
            "VM booted",
            ms = t0.elapsed().as_millis(),
            cpus = cpus,
            memory_mib = ceiling >> 20
        );
        Ok(vm)
    }

    /// The VM if it is awake; never boots it.
    fn awake(&self) -> Option<Arc<VmLink>> {
        self.link.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn touch(&self) {
        self.last_activity
            .store(now_secs(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Powers the VM off after `KIRI_IDLE_SECS` (default 60; 0 = never)
    /// with no running containers or execs and no API requests. A VM that
    /// boots in a tenth of a second can afford to sleep, and sleeping is the
    /// one thing that gives all its memory back to macOS: Apple's balloon
    /// device takes pages from the guest but does not release them.
    /// Runs a command in a running container and collects its output.
    pub fn exec_capture(&self, key: &str, argv: &[&str]) -> Result<(i32, String), Error> {
        let config = ExecConfig {
            console_size: None,
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            env: Vec::new(),
            cwd: "/".into(),
            user: String::new(),
            tty: false,
            stdin: false,
            privileged: false,
            internal: true,
        };
        let id = self.exec_create(key, config)?;
        let (_, rx) = self.exec_start(&id)?;
        let mut out = Vec::new();
        for (_, data) in rx {
            out.extend_from_slice(&data);
        }
        let code = self
            .with_exec(&id, |e| e.exit_code.unwrap_or(-1))
            .unwrap_or(-1);
        self.lock().execs.remove(&id);
        Ok((code, String::from_utf8_lossy(&out).into_owned()))
    }

    /// `kiri k8s`: every LoadBalancer service gets its port on the Mac's
    /// localhost, as OrbStack and Docker Desktop do. k3s's service load
    /// balancer already serves the port on the node, so a forward to the
    /// node container is all it takes. Checked every few seconds while the
    /// cluster runs; forwards go when services or the cluster do.
    fn start_k8s_lb(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        // `kiri k8s set --lan on` puts them on every interface; a change rebinds them.
        let mut bound_on = String::new();
        let _ = std::thread::Builder::new().name("kiri-k8s-lb".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_secs(3));
            let Some(engine) = me.upgrade() else { return };
            let node = engine.lock().containers.values().find(|c| c.status == Status::Running && c.spec.labels.get("dev.kiri.k8s").is_some()).map(|c| (c.id.clone(), c.ip));
            let Some((id, Some(ip))) = node else {
                engine.k8s_lb.lock().unwrap_or_else(|e| e.into_inner()).clear();
                engine.k8s_fwd.lock().unwrap_or_else(|e| e.into_inner()).clear();
                if let Some((_, via)) = engine.k8s_routes.lock().unwrap_or_else(|e| e.into_inner()).take()
                    && let Some(vm) = engine.awake()
                {
                    for (net, bits) in K8S_RANGES {
                        let _ = vm.route(net, bits, via, false);
                    }
                }
                continue;
            };
            engine.route_k8s(ip);
            let query = r#"jsonpath={range .items[?(@.spec.type=="LoadBalancer")]}{range .spec.ports[*]}{.port}/{.protocol} {end}{end}"#;
            let Ok((0, out)) = engine.exec_capture(&id, &["timeout", "-s", "KILL", "20", "kubectl", "get", "svc", "-A", "-o", query]) else { continue };
            let wanted: Vec<(u16, String)> = out
                .split_whitespace()
                .filter_map(|p| {
                    let (port, proto) = p.split_once('/')?;
                    Some((port.parse().ok()?, proto.to_ascii_lowercase()))
                })
                .collect();
            let Some(vm) = engine.awake() else { continue };
            let host_ip = if crate::k8s::config().lan { "0.0.0.0" } else { "127.0.0.1" };
            let mut lb = engine.k8s_lb.lock().unwrap_or_else(|e| e.into_inner());
            if bound_on != host_ip {
                lb.clear();
                bound_on = host_ip.to_owned();
            }
            lb.retain(|k, _| wanted.contains(k));
            for (port, proto) in wanted {
                if lb.contains_key(&(port, proto.clone())) {
                    continue;
                }
                let b = super::ports::Binding { container_port: port, proto: proto.clone(), host_ip: host_ip.into(), host_port: port };
                match super::ports::start(vm.clone(), &format!("lb-{port}-{proto}"), &[b], ip) {
                    Ok(fw) => {
                        crate::diag::info!("Kubernetes service published", at = format!("{host_ip}:{port}/{proto}"));
                        lb.insert((port, proto), fw);
                    }
                    Err(e) => crate::diag::warn!("Kubernetes service port not published", port = format!("{port}/{proto}"), error = e),
                }
            }
        });
    }

    /// Routes the cluster's pod and service ranges to the node, so the
    /// relay can reach a ClusterIP or a pod as it reaches a container: the
    /// node's kube-proxy takes it from there. Again after the VM wakes (a new
    /// VM has none) or the node's address changes.
    fn route_k8s(&self, node: [u8; 4]) {
        let Some(vm) = self.awake() else { return };
        let key = (Arc::as_ptr(&vm) as usize, node);
        let mut routes = self.k8s_routes.lock().unwrap_or_else(|e| e.into_inner());
        if *routes == Some(key) {
            return;
        }
        match K8S_RANGES
            .iter()
            .try_for_each(|(net, bits)| vm.route(*net, *bits, node, true))
        {
            Ok(()) => *routes = Some(key),
            // An older guest can't; shares of services just can't connect.
            Err(e) => {
                if routes.is_none() {
                    crate::diag::warn!(
                        "Kubernetes ranges not routed; sharing a service needs a newer guest",
                        error = e
                    );
                }
                *routes = Some((0, node));
            }
        }
    }

    /// Kiri's own: a port on the Mac's loopback that reaches `ip:port` in
    /// the cluster (a service's ClusterIP or a pod), for sharing it.
    pub fn k8s_forward(&self, ip: [u8; 4], port: u16) -> Result<u16, Error> {
        if !in_k8s_ranges(ip) || port == 0 {
            return Err(Error::BadRequest(
                "not an address in the Kubernetes cluster".into(),
            ));
        }
        let node = self
            .lock()
            .containers
            .values()
            .find(|c| c.status == Status::Running && c.spec.labels.get("dev.kiri.k8s").is_some())
            .and_then(|c| c.ip);
        let Some(node) = node else {
            return Err(Error::Conflict("Kubernetes isn't running".into()));
        };
        self.route_k8s(node);
        let vm = self.vm()?;
        let b = super::ports::Binding {
            container_port: port,
            proto: "tcp".into(),
            host_ip: "127.0.0.1".into(),
            host_port: 0,
        };
        let fw = super::ports::start(vm, &format!("k8s-share-{port}"), &[b], ip)?;
        let mac = fw.bound.first().map(|(_, _, p)| *p).unwrap_or(0);
        crate::diag::info!(
            "Kubernetes address forwarded for sharing",
            to = format!("{}:{port}", std::net::Ipv4Addr::from(ip)),
            at = format!("127.0.0.1:{mac}")
        );
        self.k8s_fwd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(mac, fw);
        Ok(mac)
    }

    /// Closes a forward `k8s_forward` opened.
    pub fn k8s_unforward(&self, mac: u16) {
        self.k8s_fwd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&mac);
    }

    fn start_sleeper(self: &Arc<Self>) {
        let idle: u64 = std::env::var("KIRI_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        if idle == 0 {
            return;
        }
        let builder_idle: u64 = std::env::var("KIRI_BUILDER_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("kiri-sleeper".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(5));
                    let Some(engine) = me.upgrade() else { return };
                    let quiet_for = now_secs().saturating_sub(
                        engine
                            .last_activity
                            .load(std::sync::atomic::Ordering::Relaxed),
                    );
                    // BuildKit containers (buildx's docker-container driver) run until
                    // paused, which keeps the VM's state without using CPU. Between
                    // builds they are paused; the next build resumes them.
                    let idle_builders: Vec<String> = {
                        let st = engine.lock();
                        let building = st.execs.values().any(|e| e.running);
                        let running: Vec<&Container> = st
                            .containers
                            .values()
                            .filter(|c| c.status == Status::Running)
                            .collect();
                        if !building
                            && quiet_for >= builder_idle
                            && !running.is_empty()
                            && running.iter().all(|c| is_builder(c))
                        {
                            running
                                .iter()
                                .filter(|c| !c.paused)
                                .map(|c| c.id.clone())
                                .collect()
                        } else {
                            Vec::new()
                        }
                    };
                    for id in &idle_builders {
                        if engine.pause_idle_builder(id).is_ok() {
                            crate::diag::info!(
                                "paused an idle BuildKit container; the next build resumes it",
                                id = &id[..12]
                            );
                        }
                    }
                    // A paused BuildKit container is no reason to stay awake.
                    let (busy, parked) = {
                        let st = engine.lock();
                        let parked: Vec<String> = st
                            .containers
                            .values()
                            .filter(|c| c.status == Status::Running && c.paused && is_builder(c))
                            .map(|c| c.id.clone())
                            .collect();
                        let busy =
                            st.containers.values().any(|c| {
                                c.status == Status::Running && !(c.paused && is_builder(c))
                            }) || st.execs.values().any(|e| e.running);
                        (busy, parked)
                    };
                    let quiet = quiet_for >= idle;
                    if busy || !quiet {
                        continue;
                    }
                    // Paused, they would keep the VM up for good: stopped before it
                    // sleeps, as they were before pausing; the next build starts them.
                    for id in &parked {
                        if let Err(e) = engine.stop(id, Duration::from_secs(10)) {
                            crate::diag::warn!(
                                "paused BuildKit container did not stop",
                                id = &id[..12],
                                error = format!("{e:?}")
                            );
                        }
                    }
                    if engine.sleep_if_idle() {
                        crate::diag::info!(
                            "VM asleep: idle; the next request wakes it",
                            idle_s = idle
                        );
                    }
                }
            });
    }

    /// Powers the VM off if nothing runs in it. True if it went to sleep.
    fn sleep_if_idle(&self) -> bool {
        let mut link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        // Check under the lock: a request may have woken it.
        let idle = !self
            .lock()
            .containers
            .values()
            .any(|c| c.status == Status::Running);
        let Some(vm) = link.take().filter(|_| idle) else {
            return false;
        };
        self.sleeping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.memory_used
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.memory_stall
            .store(0, std::sync::atomic::Ordering::Relaxed);
        vm.shutdown();
        // Let the guest flush its disk and power off before the VM is dropped.
        if !vm.wait_stopped(Duration::from_secs(10)) {
            crate::diag::warn!("the VM did not power off in 10 s; stopped it");
        }
        // Dropping the last reference stops the VM and frees the data disk
        // before anyone may boot a new one (they wait on `link`).
        drop(vm);
        true
    }

    /// The window's Stop: Kiri off until Start. Marked stopped first, so
    /// nothing that comes in meanwhile boots it again; then every container
    /// stops and the VM powers off.
    pub fn stop_all(&self) -> Result<(), Error> {
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(f) = stopped_marker() {
            let _ = std::fs::write(f, b"");
        }
        crate::diag::info!("Kiri stopped: the VM stays off until Start");
        self.sleep_now()
    }

    /// Start: no longer stopped (the VM boots on the next request, or now
    /// for `/kiri/wake`).
    pub fn unstop(&self) {
        if self
            .stopped
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            crate::diag::info!("Kiri started");
        }
        if let Some(f) = stopped_marker() {
            let _ = std::fs::remove_file(f);
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Stop every container, then let the VM sleep (it wakes on the next
    /// request). The window's Stop is stop_all; restarting the engine is this.
    pub fn sleep_now(&self) -> Result<(), Error> {
        let running = self.each(|c| (c.status == Status::Running).then(|| c.id.clone()));
        for id in running {
            self.stop(&id, self.stop_timeout(&id))?;
        }
        if self.sleep_if_idle() {
            crate::diag::info!("VM stopped on request");
        }
        Ok(())
    }

    fn ceiling(&self) -> u64 {
        self.ceiling.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// CPUs the VM has (or will boot with).
    pub fn cpus(&self) -> usize {
        self.cpus.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the VM is running right now.
    pub fn is_awake(&self) -> bool {
        self.awake().is_some()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Restores saved containers. Ones that were running when the daemon
    /// stopped are recorded as exited, as Docker does without live restore.
    fn load(&self) -> io::Result<()> {
        let saved: Vec<Saved> = match std::fs::read(&self.records) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                crate::diag::error!(
                    "ignoring unreadable container records",
                    file = self.records.display(),
                    error = e
                );
                Vec::new()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let now = SystemTime::now();
        let mut restart = Vec::new();
        {
            let mut st = self.lock();
            for s in saved {
                let was_running = s.status == Status::Running;
                let c = Container {
                    id: s.id.clone(),
                    name: s.name,
                    layers: s.layers,
                    created: from_ns(s.created_ns),
                    status: if was_running {
                        Status::Exited
                    } else {
                        s.status
                    },
                    started: s.started_ns.map(from_ns),
                    finished: if was_running {
                        Some(now)
                    } else {
                        s.finished_ns.map(from_ns)
                    },
                    exit_code: if was_running { 255 } else { s.exit_code },
                    oom_killed: !was_running && s.oom_killed,
                    vm_stopped: was_running || s.vm_stopped,
                    stopped_by_user: s.stopped_by_user,
                    restart_count: s.restart_count,
                    exits: 0,
                    size: None,
                    ip: None,
                    pid: 0,
                    forwards: None,
                    live: false,
                    early_input: Vec::new(),
                    early_close: false,
                    paused: false,
                    extra_ips: Vec::new(),
                    ip6: None,
                    extra_ip6s: Vec::new(),
                    log: Arc::new(Mutex::new(super::logfile::Log::new(
                        super::logfile::dir(self.store.root(), &s.id),
                        &s.spec.log_config,
                    ))),
                    attached: Vec::new(),
                    spec: s.spec,
                };
                if let Some(mut health) = s.health {
                    // Its monitor stopped with the daemon; Docker reports that as unhealthy.
                    if was_running {
                        health.status = super::health::UNHEALTHY.into();
                    }
                    st.health.insert(
                        c.id.clone(),
                        HealthRun {
                            health,
                            epoch: 0,
                            monitoring: false,
                        },
                    );
                }
                let policy = c.spec.restart.name.as_str();
                if (policy == "always" || (policy == "unless-stopped" && !c.stopped_by_user))
                    && !c.spec.auto_remove
                {
                    restart.push(c.id.clone());
                }
                st.containers.insert(s.id, c);
            }
            // Logs of containers that are gone (`--rm` ones are never saved).
            if let Ok(dirs) = std::fs::read_dir(self.store.root().join("containers")) {
                for d in dirs.flatten() {
                    if !d
                        .file_name()
                        .to_str()
                        .is_some_and(|n| st.containers.contains_key(n))
                    {
                        let _ = std::fs::remove_dir_all(d.path());
                    }
                }
            }
            // Containers that join another's network or link to others start
            // after those, as Docker orders them.
            restart.sort_by_key(|id| {
                st.containers.get(id).is_some_and(|c| {
                    c.spec.network_mode.starts_with("container:") || !c.spec.links.is_empty()
                })
            });
            drop(st);
            // Boot now only when containers restart; otherwise the engine
            // starts asleep (Ready · Sleeping) and the first request boots
            // the VM in about 0.1 s. Every boot tells the guest which
            // stopped containers to keep (vm()).
            if !restart.is_empty() {
                self.vm()?;
            }
        }
        for id in restart {
            if let Err(e) = self.start_inner(&id, false) {
                crate::diag::warn!(
                    "container did not restart",
                    id = &id[..12],
                    error = format!("{e:?}")
                );
            }
        }
        Ok(())
    }

    /// Writes `containers.json`. `--rm` containers are left out.
    fn save(&self) {
        let json = {
            let st = self.lock();
            let saved: Vec<Saved> = st
                .containers
                .values()
                .filter(|c| !c.spec.auto_remove)
                .map(|c| Saved {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    spec: c.spec.clone(),
                    layers: c.layers.clone(),
                    created_ns: ns(c.created),
                    status: c.status,
                    started_ns: c.started.map(ns),
                    finished_ns: c.finished.map(ns),
                    exit_code: c.exit_code,
                    oom_killed: c.oom_killed,
                    vm_stopped: c.vm_stopped,
                    stopped_by_user: c.stopped_by_user,
                    restart_count: c.restart_count,
                    health: st.health.get(&c.id).map(|h| h.health.clone()),
                })
                .collect();
            serde_json::to_vec(&saved)
        };
        let Ok(json) = json else { return };
        let _one_writer = self.saving.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = self.records.with_extension("json.tmp");
        if let Err(e) =
            std::fs::write(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &self.records))
        {
            crate::diag::error!("saving containers failed", error = e);
        }
    }

    fn on_event(&self, ev: Event<'_>) {
        match ev {
            Event::Output { id, stream, data } => {
                let mut st = self.lock();
                if let Some(c) = st.containers.get_mut(id) {
                    c.attached
                        .retain(|tx| tx.send((stream, data.to_vec())).is_ok());
                    // Written outside the engine's lock; holding the log's
                    // keeps the order `docker logs -f` sees.
                    let log = c.log.clone();
                    let mut log = lock_log(&log);
                    drop(st);
                    log.append(stream, ns(SystemTime::now()), data);
                } else if let Some(e) = st.execs.get_mut(id) {
                    e.attached
                        .retain(|tx| tx.send((stream, data.to_vec())).is_ok());
                }
            }
            Event::Exited { id, code, oom } => {
                let mut st = self.lock();
                if let Some(e) = st.execs.get_mut(id) {
                    e.running = false;
                    e.exit_code = Some(code);
                    e.pid = 0;
                    e.attached.clear();
                    let (container, exec_id, internal) =
                        (e.container_id.clone(), e.id.clone(), e.config.internal);
                    drop(st);
                    self.changed.notify_all();
                    if !internal {
                        self.emit_container(
                            "exec_die",
                            &container,
                            &[("execID", exec_id), ("exitCode", code.to_string())],
                        );
                    }
                    return;
                }
                let mut restart_in = None;
                let mut persist = false;
                let mut closed_ports = None;
                let actor = st
                    .containers
                    .get(id)
                    .map(|c| (c.spec.image.clone(), container_attrs(c)));
                // Docker stops a container's healthcheck when it exits.
                if let Some(h) = st.health.get_mut(id) {
                    close_monitor(h);
                }
                let remove = match st.containers.get_mut(id) {
                    Some(c) => {
                        c.status = Status::Exited;
                        c.exit_code = code;
                        c.pid = 0;
                        c.oom_killed = oom;
                        // Stopped by the daemon's shutdown, not by the app.
                        c.vm_stopped = SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst);
                        if oom {
                            crate::diag::warn!(
                                "container killed for memory",
                                name = c.name.clone(),
                                exit_code = code
                            );
                        }
                        c.exits += 1;
                        c.live = false;
                        if let Some(ip) = c.ip.take() {
                            self.release_ip(&c.spec.network_mode, ip);
                        }
                        for (net, ip) in std::mem::take(&mut c.extra_ips) {
                            self.release_ip(&net, ip);
                        }
                        self.release_ip6s(c);
                        c.paused = false;
                        closed_ports = c.forwards.take();
                        c.finished = Some(SystemTime::now());
                        // Dropping the senders ends every attach stream.
                        c.attached.clear();
                        lock_log(&c.log).end_follow();
                        persist = !c.spec.auto_remove;
                        let p = &c.spec.restart;
                        let wants = match p.name.as_str() {
                            "always" | "unless-stopped" => !c.stopped_by_user,
                            "on-failure" => {
                                code != 0
                                    && !c.stopped_by_user
                                    && (p.max_retries == 0 || c.restart_count < p.max_retries)
                            }
                            _ => false,
                        };
                        if wants
                            && !c.spec.auto_remove
                            && !SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
                        {
                            let backoff = RESTART_BASE
                                .saturating_mul(1 << c.restart_count.min(10))
                                .min(RESTART_CAP);
                            c.restart_count += 1;
                            restart_in = Some(backoff);
                        }
                        c.spec.auto_remove
                    }
                    None => false,
                };
                let mut anon = Vec::new();
                let mut removed_log = None;
                if remove {
                    if let Some(c) = st.containers.remove(id) {
                        anon = c.spec.anon_volumes;
                        removed_log = Some(c.log);
                    }
                    note_removed(&mut st, id, code);
                }
                drop(st);
                if let Some((image, attrs)) = actor {
                    self.emit_container_with(
                        "die",
                        id,
                        &image,
                        attrs.clone(),
                        &[("exitCode", code.to_string())],
                    );
                    if remove {
                        self.emit_container_with("destroy", id, &image, attrs, &[]);
                    }
                }
                // Closing listeners joins their threads: never under the lock.
                drop(closed_ports);
                if let Some(log) = removed_log {
                    lock_log(&log).remove();
                }
                // `--rm` takes the container's anonymous volumes with it.
                for v in anon {
                    self.delete_volume_data(&v);
                }
                self.changed.notify_all();
                if persist {
                    self.save();
                }
                if let (Some(delay), Some(me)) = (restart_in, self.me.get().and_then(Weak::upgrade))
                {
                    let id = id.to_owned();
                    let _ = std::thread::Builder::new()
                        .name("kiri-restart".into())
                        .spawn(move || {
                            std::thread::sleep(delay);
                            // Only if nobody stopped or removed it in the meantime.
                            let still_wanted =
                                me.lock().containers.get(&id).is_some_and(|c| {
                                    c.status == Status::Exited && !c.stopped_by_user
                                });
                            if still_wanted {
                                if let Err(e) = me.start_inner(&id, false) {
                                    crate::diag::warn!(
                                        "container did not restart",
                                        id = &id[..12],
                                        error = format!("{e:?}")
                                    );
                                }
                            }
                        });
                }
            }
            Event::Pressure => self.grow_memory(),
            Event::Lost => {
                let mut st = self.lock();
                let mut closed = Vec::new();
                for c in st.containers.values_mut() {
                    closed.extend(c.forwards.take());
                    if let Some(ip) = c.ip.take() {
                        self.release_ip(&c.spec.network_mode, ip);
                    }
                    for (net, ip) in std::mem::take(&mut c.extra_ips) {
                        self.release_ip(&net, ip);
                    }
                    self.release_ip6s(c);
                    c.paused = false;
                    c.pid = 0;
                    if c.status == Status::Running {
                        c.status = Status::Exited;
                        c.exits += 1;
                        c.exit_code = 255;
                        c.vm_stopped = true;
                        c.finished = Some(SystemTime::now());
                        c.attached.clear();
                        lock_log(&c.log).end_follow();
                    }
                }
                for e in st.execs.values_mut() {
                    e.running = false;
                    e.attached.clear();
                }
                for h in st.health.values_mut() {
                    close_monitor(h);
                }
                drop(st);
                drop(closed);
                self.changed.notify_all();
                if !self.sleeping.load(std::sync::atomic::Ordering::SeqCst) {
                    self.save();
                    crate::diag::error!(
                        "the VM stopped unexpectedly; the next request boots a new one"
                    );
                    // A crashed VM: forget it, so the next request boots a fresh one.
                    if let Ok(mut link) = self.link.try_lock() {
                        *link = None;
                    }
                }
            }
        }
    }

    /// Every second: read what the guest holds and, on the Mac, give it that
    /// plus headroom through the balloon, the rest back to macOS. Pressure
    /// events grow it at once in between. On a Linux host there is no
    /// balloon and nothing to give back: the reading is all, and it takes in
    /// this process too, which runs beside the containers there.
    fn start_balloon(self: &Arc<Self>) {
        if std::env::var_os("KIRI_NO_BALLOON").is_some() {
            return;
        }
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("kiri-balloon".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let Some(engine) = me.upgrade() else { return };
                    let Some(vm) = engine.awake() else { continue };
                    let Ok(stats) = vm.stats() else { continue };
                    let mut target = engine
                        .memory_target
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let ceiling = engine.ceiling();
                    let used = if stats.mem_used_kib > 0 {
                        // The agent's count: anonymous, shared and kernel memory
                        // from the cgroup, which neither the page cache nor the
                        // balloon's own pages get into.
                        stats.mem_used_kib * 1024
                            + if cfg!(target_os = "macos") {
                                0
                            } else {
                                self_rss()
                            }
                    } else {
                        // An agent from before that count: MemTotal minus
                        // MemAvailable, with the balloon taken back out. Apple's
                        // balloon offers deflate-on-OOM, so Linux counts its pages
                        // as used rather than shrinking MemTotal. The balloon is
                        // guessed at ceiling - target, which it only is once the
                        // guest has inflated that far: short of that, the guess
                        // exceeds the use and the result sits at zero.
                        let apparent =
                            (stats.mem_total_kib.saturating_sub(stats.mem_available_kib)) * 1024;
                        apparent.saturating_sub(ceiling - *target)
                    };
                    engine
                        .memory_used
                        .store(used, std::sync::atomic::Ordering::Relaxed);
                    engine
                        .memory_stall
                        .store(stats.mem_stall, std::sync::atomic::Ordering::Relaxed);
                    engine
                        .oom_kills
                        .store(stats.oom_kills, std::sync::atomic::Ordering::Relaxed);
                    if !cfg!(target_os = "macos") {
                        continue;
                    }
                    let want = (used + MEMORY_HEADROOM).clamp(MEMORY_FLOOR, ceiling);
                    // Shrink in steps (so the guest's caches go gradually), grow at once.
                    let next = if want < *target {
                        want.max(target.saturating_sub(256 << 20))
                    } else {
                        want
                    };
                    if next.abs_diff(*target) >= 64 << 20 {
                        *target = next;
                        vm.set_memory_target(next);
                    }
                }
            });
    }

    fn grow_memory(&self) {
        let mut target = self.memory_target.lock().unwrap_or_else(|e| e.into_inner());
        let next = (*target + MEMORY_GROW).min(self.ceiling());
        if next != *target {
            *target = next;
            if let Some(vm) = self.awake() {
                vm.set_memory_target(next);
            }
        }
    }

    /// (ceiling, current target) in bytes. On a Linux host there is no VM
    /// of Kiri's to bound: both are the machine's memory (in WSL, the memory
    /// .wslconfig gives it), so the window shows use against what is there.
    pub fn memory(&self) -> (u64, u64) {
        if cfg!(target_os = "macos") {
            (
                self.ceiling(),
                *self.memory_target.lock().unwrap_or_else(|e| e.into_inner()),
            )
        } else {
            let ram = physical_memory();
            (ram, ram)
        }
    }

    /// What Kiri's Linux holds now, in bytes; 0 while asleep.
    pub fn memory_used(&self) -> u64 {
        self.memory_used.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The share of the last 10 s the guest spent stalled on memory, in
    /// percent, and the processes it has killed for memory since it booted.
    pub fn memory_trouble(&self) -> (f64, u64) {
        let stall = self.memory_stall.load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0;
        (
            stall,
            self.oom_kills.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// How busy the VM is, in percent of all its CPUs; 0 while asleep.
    pub fn cpu_percent(&self) -> f64 {
        f64::from(
            self.cpu_permyriad
                .load(std::sync::atomic::Ordering::Relaxed),
        ) / 100.0
    }

    /// Reads the VM's CPU time every two seconds, for `cpu_percent`.
    fn start_cpu_meter(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("kiri-cpu".into())
            .spawn(move || {
                let mut last: Option<(u64, std::time::Instant)> = None;
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    let Some(engine) = me.upgrade() else { return };
                    let now = engine
                        .awake()
                        .and_then(|_| super::vmcpu::cpu_ns())
                        .map(|ns| (ns, std::time::Instant::now()));
                    let permyriad = match (last, now) {
                        (Some((a, t0)), Some((b, t1))) if b >= a => {
                            let wall = t1.duration_since(t0).as_nanos() as f64
                                * engine.cpus().max(1) as f64;
                            ((b - a) as f64 / wall * 10000.0)
                                .round()
                                .clamp(0.0, 10000.0) as u32
                        }
                        _ => 0,
                    };
                    engine
                        .cpu_permyriad
                        .store(permyriad, std::sync::atomic::Ordering::Relaxed);
                    last = now;
                }
            });
    }

    /// Finds an image by the name a client used.
    pub fn image(&self, name: &str) -> io::Result<Option<Image>> {
        Ok(image_named(self.store.images()?, name))
    }

    /// Records a container and makes sure its layers are in the guest, so
    /// `start` has nothing left to do but run it.
    pub fn create(&self, name: Option<String>, spec: Spec, image: &Image) -> Result<String, Error> {
        {
            let _one_at_a_time = self.layers.lock().unwrap_or_else(|e| e.into_inner());
            for layer in &image.layers {
                if !self.vm()?.has_layer(&layer.digest)? {
                    self.vm()?.send_layer(
                        &layer.digest,
                        &layer.media_type,
                        &self.store.blob_path(&layer.digest)?,
                    )?;
                }
            }
        }
        let id = random_hex(32);
        let persist = !spec.auto_remove;
        let (log_dir, log_config) = (
            super::logfile::dir(self.store.root(), &id),
            spec.log_config.clone(),
        );
        let mut st = self.lock();
        let name = match name.filter(|n| !n.is_empty()) {
            Some(n) => {
                let n = n.trim_start_matches('/').to_owned();
                if !valid_container_name(&n) {
                    return Err(Error::BadRequest(format!(
                        "Invalid container name ({n}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"
                    )));
                }
                if st.containers.values().any(|c| c.name == n) {
                    return Err(Error::Conflict(format!(
                        "Conflict. The container name \"/{n}\" is already in use. You have to remove (or rename) that container to be able to reuse that name."
                    )));
                }
                n
            }
            None => loop {
                let n = super::names::random();
                if !st.containers.values().any(|c| c.name == n) {
                    break n;
                }
            },
        };
        st.containers.insert(
            id.clone(),
            Container {
                id: id.clone(),
                name,
                layers: image
                    .layers
                    .iter()
                    .rev()
                    .map(|l| l.digest.clone())
                    .collect(),
                spec,
                created: SystemTime::now(),
                status: Status::Created,
                started: None,
                finished: None,
                exit_code: 0,
                oom_killed: false,
                vm_stopped: false,
                stopped_by_user: false,
                restart_count: 0,
                exits: 0,
                size: None,
                ip: None,
                pid: 0,
                forwards: None,
                live: false,
                early_input: Vec::new(),
                early_close: false,
                paused: false,
                extra_ips: Vec::new(),
                ip6: None,
                extra_ip6s: Vec::new(),
                log: Arc::new(Mutex::new(super::logfile::Log::new(log_dir, &log_config))),
                attached: Vec::new(),
            },
        );
        drop(st);
        if persist {
            self.save();
        }
        self.emit_container("create", &id, &[]);
        Ok(id)
    }

    /// Resolves an id, an id prefix or a name to the full id.
    pub fn resolve(&self, key: &str) -> Result<String, Error> {
        let st = self.lock();
        resolve(&st, key)
    }

    /// `docker start`: also clears a previous `stop`, as Docker does.
    pub fn start(&self, key: &str) -> Result<(), Error> {
        self.start_inner(key, true)
    }

    fn start_inner(&self, key: &str, by_user: bool) -> Result<(), Error> {
        let (id, layers, argv, spec, size) = {
            let mut st = self.lock();
            let id = resolve(&st, key)?;
            let c = st.containers.get_mut(&id).expect("resolved");
            match c.status {
                Status::Running => return Ok(()),
                Status::Exited if c.spec.auto_remove => {
                    return Err(Error::Conflict("container is being removed".into()));
                }
                _ => {}
            }
            let argv = c.argv();
            if argv.is_empty() {
                return Err(Error::BadRequest("no command specified".into()));
            }
            c.status = Status::Running;
            c.started = Some(SystemTime::now());
            c.finished = None;
            c.oom_killed = false;
            c.vm_stopped = false;
            // `docker run --rm` without -d: its client is reading the output
            // and the log goes with the container, so it stays in memory.
            if c.spec.auto_remove && !c.attached.is_empty() {
                lock_log(&c.log).keep_in_memory();
            }
            if by_user {
                c.stopped_by_user = false;
                c.restart_count = 0;
            }
            (
                id,
                c.layers.clone(),
                argv,
                c.spec.clone(),
                c.size.or(c.spec.console_size),
            )
        };
        let mut hostname = if spec.hostname.is_empty() {
            id[..12].to_owned()
        } else {
            spec.hostname.clone()
        };
        let mut domainname = spec.domainname.clone();
        // `--network container:<id>`: that container's namespace, hostname
        // and /etc files, as with Docker.
        let mut joined_mode = None;
        let joined = match spec.network_mode.strip_prefix("container:") {
            Some(target) => match self.network_container(&id, target) {
                Ok((target, h, d, mode)) => {
                    (hostname, domainname) = (h, d);
                    joined_mode = Some(mode);
                    Some(target)
                }
                Err(e) => {
                    self.fail_start(&id, &spec);
                    return Err(e);
                }
            },
            None => None,
        };
        let mut flags = 0;
        let net = self.lock_networks().find(&spec.network_mode).cloned();
        let links = match net
            .as_ref()
            .map(|n| self.link_hosts(&id, &spec, n))
            .transpose()
        {
            Ok(l) => l.unwrap_or_default(),
            Err(e) => {
                self.fail_start(&id, &spec);
                return Err(e);
            }
        };
        let mut dns_names: Vec<String> = Vec::new();
        let mut net6_opts: Vec<String> = Vec::new();
        let (bridge, ip, prefix, gateway) = match net.as_ref().map(|n| n.driver.as_str()) {
            Some("host") => {
                flags |= run_flags::NET_HOST;
                (String::new(), [0; 4], 0, [0; 4])
            }
            Some("bridge") => {
                let n = net.as_ref().expect("matched");
                let ip = match super::network::parse_ip(&spec.ipv4_address) {
                    Some(want) => self
                        .lock_networks()
                        .reserve(&n.name, want)
                        .map(|()| want)
                        .map_err(Error::Conflict),
                    None => {
                        let avoid = self.wanted_addresses(&n.name);
                        self.lock_networks()
                            .allocate_avoiding(&n.name, &avoid)
                            .ok_or_else(|| {
                                Error::Conflict(format!(
                                    "no free address left on network {}",
                                    n.name
                                ))
                            })
                    }
                };
                let ip = match ip {
                    Ok(ip) => ip,
                    Err(e) => {
                        self.fail_start(&id, &spec);
                        return Err(e);
                    }
                };
                let ip6 = if spec.ipv6_disabled("eth0") {
                    Ok(None)
                } else {
                    self.allocate_ip6(n, &spec.ipv6_address)
                };
                match ip6 {
                    Ok(ip6) => {
                        if let Some(c) = self.lock().containers.get_mut(&id) {
                            c.ip6 = ip6;
                        }
                        if let (Some(a), Some(v)) = (ip6, &n.v6) {
                            net6_opts.push(format!(
                                "{}={}/{}",
                                kiri_proto::net_opts::IP6,
                                super::network::ip6_str(a),
                                v.prefix
                            ));
                            net6_opts.push(format!(
                                "{}={}",
                                kiri_proto::net_opts::GW6,
                                super::network::ip6_str(v.gateway)
                            ));
                        }
                        if n.internal {
                            net6_opts.push(format!("{}=1", kiri_proto::net_opts::INTERNAL));
                        }
                        if n.no_ipv4 {
                            net6_opts.push(format!("{}=1", kiri_proto::net_opts::NO_IPV4));
                        }
                    }
                    Err(e) => {
                        self.lock_networks().release(&n.name, ip);
                        self.fail_start(&id, &spec);
                        return Err(e);
                    }
                }
                // User-defined networks resolve container names, as Docker's do;
                // the default bridge does not.
                if !n.builtin() {
                    flags |= run_flags::DNS_GATEWAY;
                    let name = self
                        .lock()
                        .containers
                        .get(&id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    dns_names.push(name);
                    dns_names.extend(spec.aliases.iter().cloned());
                    dns_names.push(hostname.clone());
                }
                (n.bridge.clone(), ip, n.prefix, n.gateway)
            }
            Some(_) => {
                flags |= run_flags::NET_NONE;
                (String::new(), [0; 4], 0, [0; 4])
            }
            None if joined.is_some() => (String::new(), [0; 4], 0, [0; 4]),
            None => {
                self.fail_start(&id, &spec);
                return Err(Error::NotFound(format!(
                    "network {} not found",
                    spec.network_mode
                )));
            }
        };
        if ip != [0; 4] {
            if let Some(c) = self.lock().containers.get_mut(&id) {
                c.ip = Some(ip);
            }
        }
        // A restarted container keeps its writable layer, so only --rm deletes it.
        if spec.auto_remove {
            flags |= run_flags::REMOVE;
        }
        if spec.tty {
            flags |= run_flags::TTY;
        }
        if spec.open_stdin {
            flags |= run_flags::STDIN;
        }
        if spec.privileged {
            flags |= run_flags::PRIVILEGED;
        }
        if spec.init == Some(true) {
            flags |= run_flags::INIT;
        }
        flags |= spec.security_flags();
        // Every container can reach the Mac, as with Docker Desktop and OrbStack.
        let mut hosts = vec![
            format!(
                "{}\thost.docker.internal host.kiri.internal",
                super::ports::HOST_ADDR
            ),
            format!("{}\tgateway.docker.internal", super::ports::HOST_ADDR),
        ];
        hosts.extend(spec.extra_hosts.iter().cloned());
        hosts.extend(links);
        let mut opts = net_opts(&spec, joined.as_deref(), &domainname);
        opts.extend(net6_opts);
        let net_args = kiri_proto::Net {
            bridge: &bridge,
            ip,
            prefix,
            gateway,
            extra_hosts: &hosts,
            dns_names: &dns_names,
            opts: &opts,
        };
        // Joining a host-network container is the host's namespace too: its
        // net.* sysctls would change the whole VM (or Linux host).
        let joined_host = joined_mode.is_some_and(|m| {
            self.lock_networks()
                .find(&m)
                .is_some_and(|n| n.driver == "host")
        });
        let host_opts = match spec.guest_opts(flags & run_flags::NET_HOST != 0 || joined_host) {
            Ok(o) => o,
            Err(e) => {
                self.fail_start(&id, &spec);
                return Err(e);
            }
        };
        let vm = match self.vm() {
            Ok(vm) => vm,
            Err(e) => {
                self.fail_start(&id, &spec);
                return Err(Error::Io(e));
            }
        };
        match vm.run(
            &id,
            &layers,
            &argv,
            &spec.env,
            &spec.cwd,
            &spec.user,
            &hostname,
            flags,
            net_args,
            &spec.mounts,
            &spec.limits.wire(),
            spec.caps,
            &host_opts,
        ) {
            Ok(pid) => {
                if let Some(c) = self
                    .lock()
                    .containers
                    .get_mut(&id)
                    .filter(|c| c.status == Status::Running)
                {
                    c.pid = pid;
                }
            }
            Err(e) => {
                crate::diag::warn!(
                    "container did not start",
                    id = &id[..12],
                    image = spec.image,
                    error = e
                );
                self.fail_start(&id, &spec);
                return Err(Error::Io(e));
            }
        }
        if let (true, Some((rows, cols))) = (spec.tty, size) {
            let _ = vm.resize(&id, rows, cols);
        }
        if flags & (run_flags::NET_HOST | run_flags::NET_NONE) == 0 && joined.is_none() {
            for (i, extra) in spec.extra_networks.iter().enumerate() {
                if let Err(e) = self.attach_extra(&vm, &id, i as u32 + 1, extra) {
                    crate::diag::warn!(
                        "container not attached to a network",
                        id = &id[..12],
                        network = extra.name,
                        error = format!("{e:?}")
                    );
                }
            }
        }
        if !spec.ports.is_empty() {
            let target = if flags & run_flags::NET_HOST != 0 {
                [127, 0, 0, 1]
            } else {
                ip
            };
            match super::ports::start(vm.clone(), &id, &spec.ports, target) {
                Ok(fw) => {
                    if let Some(c) = self.lock().containers.get_mut(&id) {
                        c.forwards = Some(fw);
                    }
                }
                Err(e) => {
                    crate::diag::warn!(
                        "ports not published; container stopped",
                        id = &id[..12],
                        error = e
                    );
                    // Docker refuses to run a container whose ports clash.
                    let _ = self.vm()?.kill(&id, libc::SIGKILL as u32);
                    return Err(Error::Conflict(e.to_string()));
                }
            }
        }
        // The guest has the process now: deliver input that arrived early.
        // It is drained before the container goes live, so input still
        // arriving meanwhile (and its CloseStdin) queues behind it rather
        // than overtaking it.
        loop {
            let (early, close) = {
                let mut st = self.lock();
                let Some(c) = st.containers.get_mut(&id) else {
                    break;
                };
                let early = std::mem::take(&mut c.early_input);
                if !early.is_empty() {
                    (early, false)
                } else {
                    c.live = true;
                    (early, std::mem::take(&mut c.early_close))
                }
            };
            if early.is_empty() {
                if close {
                    let _ = self.vm()?.close_stdin(&id);
                }
                break;
            }
            let _ = self.vm()?.input(&id, &early);
        }
        self.health_start(&id);
        if !spec.auto_remove {
            self.save();
        }
        self.emit_container("start", &id, &[]);
        Ok(())
    }

    /// Sends an event to every listener and keeps it for `--since`.
    pub fn emit(
        &self,
        typ: &str,
        action: &str,
        actor: &str,
        attributes: serde_json::Map<String, serde_json::Value>,
        legacy: Option<(&str, &str)>,
    ) {
        log_event(typ, action, actor, &attributes);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut ev = serde_json::json!({
            "Type": typ,
            "Action": action,
            "Actor": {"ID": actor, "Attributes": attributes},
            "scope": "local",
            "time": now.as_secs(),
            "timeNano": now.as_nanos() as u64,
        });
        // Old clients read `status`, `id` and `from` on container events.
        if let Some((id, from)) = legacy {
            ev["status"] = action.into();
            ev["id"] = id.into();
            ev["from"] = from.into();
        }
        let mut e = self.events.lock().unwrap_or_else(|e| e.into_inner());
        e.recent.push_back(ev.clone());
        while e.recent.len() > RECENT_EVENTS {
            e.recent.pop_front();
        }
        e.listeners.retain(|tx| {
            !matches!(
                tx.try_send(ev.clone()),
                Err(std::sync::mpsc::TrySendError::Disconnected(_))
            )
        });
    }

    /// Kept events, and a receiver for the ones to come.
    pub fn subscribe(&self) -> (Vec<serde_json::Value>, Receiver<serde_json::Value>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(EVENT_QUEUE);
        let mut e = self.events.lock().unwrap_or_else(|e| e.into_inner());
        e.listeners.push(tx);
        (e.recent.iter().cloned().collect(), rx)
    }

    /// A container event, with its name, image and labels as attributes.
    pub fn emit_container(&self, action: &str, id: &str, extra: &[(&str, String)]) {
        let Some((image, attrs)) = self
            .lock()
            .containers
            .get(id)
            .map(|c| (c.spec.image.clone(), container_attrs(c)))
        else {
            return;
        };
        self.emit_container_with(action, id, &image, attrs, extra);
    }

    fn emit_container_with(
        &self,
        action: &str,
        id: &str,
        image: &str,
        mut attrs: serde_json::Map<String, serde_json::Value>,
        extra: &[(&str, String)],
    ) {
        for (k, v) in extra {
            attrs.insert((*k).to_owned(), v.clone().into());
        }
        self.emit("container", action, id, attrs, Some((id, image)));
    }

    /// An image event, as Docker sends one: `pull`, `tag`, `untag`,
    /// `delete`, `load`. Open windows refresh their image list on it.
    pub fn emit_image(&self, action: &str, id: &str, name: &str) {
        let mut attrs = serde_json::Map::new();
        attrs.insert("name".into(), name.into());
        self.emit("image", action, id, attrs, None);
    }

    /// A volume event: `create` or `destroy`.
    fn emit_volume(&self, action: &str, name: &str) {
        let mut attrs = serde_json::Map::new();
        attrs.insert("driver".into(), "local".into());
        self.emit("volume", action, name, attrs, None);
    }

    fn emit_network(&self, action: &str, net: &super::network::Network, container: &str) {
        let mut attrs = serde_json::Map::new();
        attrs.insert("container".into(), container.into());
        attrs.insert("name".into(), net.name.clone().into());
        attrs.insert("type".into(), net.driver.clone().into());
        self.emit("network", action, &net.id, attrs, None);
    }

    /// Fixed addresses containers not running ask for on a network: an
    /// automatic address passes over them, so they are free when those start.
    fn wanted_addresses(&self, network: &str) -> Vec<[u8; 4]> {
        let st = self.lock();
        let mut out = Vec::new();
        for c in st
            .containers
            .values()
            .filter(|c| c.status != Status::Running)
        {
            if c.spec.network_mode == network {
                out.extend(super::network::parse_ip(&c.spec.ipv4_address));
            }
            out.extend(
                c.spec
                    .extra_networks
                    .iter()
                    .filter(|x| x.name == network)
                    .filter_map(|x| super::network::parse_ip(&x.ipv4_address)),
            );
        }
        out
    }

    /// For `--network container:<target>`: the target's id, hostname and
    /// domain name. It has to be running, as with Docker.
    /// Also the network mode the namespace finally belongs to (following
    /// `container:` chains), so the caller can tell a host-network one.
    fn network_container(
        &self,
        id: &str,
        target: &str,
    ) -> Result<(String, String, String, String), Error> {
        let st = self.lock();
        let target = resolve(&st, target)?;
        if target == id {
            return Err(Error::BadRequest("cannot join own network".into()));
        }
        let c = st.containers.get(&target).expect("resolved");
        if c.status != Status::Running {
            return Err(Error::Conflict(format!(
                "cannot join network of a non running container: {target}"
            )));
        }
        let hostname = if c.spec.hostname.is_empty() {
            c.short_id().to_owned()
        } else {
            c.spec.hostname.clone()
        };
        let mut mode = c.spec.network_mode.clone();
        for _ in 0..8 {
            let Some(next) = mode
                .strip_prefix("container:")
                .and_then(|t| resolve(&st, t).ok())
                .and_then(|t| st.containers.get(&t))
            else {
                break;
            };
            mode = next.spec.network_mode.clone();
        }
        Ok((target, hostname, c.spec.domainname.clone(), mode))
    }

    /// `--link` lines for /etc/hosts, as Docker writes them: `ip alias
    /// hostname name`. Linked containers must be running; on the default
    /// bridge (which has no DNS) they must be on it too. The lines are fixed
    /// at start: a linked container that restarts with another address is
    /// not followed (Docker rewrites them; restart this one).
    fn link_hosts(
        &self,
        id: &str,
        spec: &Spec,
        net: &super::network::Network,
    ) -> Result<Vec<String>, Error> {
        let st = self.lock();
        let me = st
            .containers
            .get(id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let mut out = Vec::new();
        for l in &spec.links {
            let c = st.containers.get(&l.id).ok_or_else(|| {
                Error::NotFound(format!("could not get container for {}", l.name))
            })?;
            if c.status != Status::Running {
                return Err(Error::Conflict(format!(
                    "Cannot link to a non running container: /{} AS /{me}/{}",
                    c.name, l.alias
                )));
            }
            let ip = if c.spec.network_mode == net.name {
                c.ip
            } else {
                c.extra_ips
                    .iter()
                    .find(|(n, _)| *n == net.name)
                    .map(|(_, ip)| *ip)
            };
            let Some(ip) = ip else {
                if net.name == "bridge" {
                    return Err(Error::BadRequest(format!(
                        "Cannot link to /{}, as it does not belong to the default network",
                        c.name
                    )));
                }
                continue;
            };
            let hostname = if c.spec.hostname.is_empty() {
                c.short_id()
            } else {
                c.spec.hostname.as_str()
            };
            // A user network's DNS already answers the container's own names
            // (Compose's `links: [db]`) and follows it across restarts; a
            // fixed line would not.
            if net.name != "bridge" {
                let aliases: &[String] = if c.spec.network_mode == net.name {
                    &c.spec.aliases
                } else {
                    c.spec
                        .extra_networks
                        .iter()
                        .find(|x| x.name == net.name)
                        .map_or(&[], |x| &x.aliases)
                };
                if l.alias == c.name || l.alias == hostname || aliases.contains(&l.alias) {
                    continue;
                }
            }
            let mut names = format!("{} {hostname}", l.alias);
            if l.alias != c.name {
                names.push(' ');
                names.push_str(&c.name);
            }
            out.push(format!("{}\t{names}", super::network::ip_str(ip)));
        }
        Ok(out)
    }

    /// Records a start that never reached the guest as an immediate exit.
    fn fail_start(&self, id: &str, spec: &Spec) {
        let mut st = self.lock();
        let actor = st
            .containers
            .get(id)
            .map(|c| (c.spec.image.clone(), container_attrs(c)));
        if let Some(c) = st.containers.get_mut(id) {
            c.status = Status::Exited;
            c.exits += 1;
            c.exit_code = 127;
            c.finished = Some(SystemTime::now());
            c.attached.clear();
            lock_log(&c.log).end_follow();
            if let Some(ip) = c.ip.take() {
                self.release_ip(&c.spec.network_mode, ip);
            }
            self.release_ip6s(c);
        }
        if let Some(h) = st.health.get_mut(id) {
            close_monitor(h);
        }
        // `--rm`: Docker removes a container whose start failed, and an
        // attached `docker run --rm` waits for exactly that (condition
        // "removed") before it reports the error; without it, it hung.
        let removed = if spec.auto_remove {
            st.containers.remove(id)
        } else {
            None
        };
        if removed.is_some() {
            note_removed(&mut st, id, 127);
        }
        drop(st);
        self.changed.notify_all();
        if let Some(c) = removed {
            if let Some(vm) = self.awake() {
                let _ = vm.remove(id);
            }
            lock_log(&c.log).remove();
            for v in c.spec.anon_volumes {
                self.delete_volume_data(&v);
            }
            if let Some((image, attrs)) = actor {
                self.emit_container_with("destroy", id, &image, attrs, &[]);
            }
        }
        if !spec.auto_remove {
            self.save();
        }
    }

    pub fn lock_net(&self) -> MutexGuard<'_, Networks> {
        self.lock_networks()
    }

    fn lock_networks(&self) -> MutexGuard<'_, Networks> {
        self.networks.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn release_ip(&self, network: &str, ip: [u8; 4]) {
        let mut nets = self.lock_networks();
        let name = nets.find(network).map(|n| n.name.clone());
        if let Some(name) = name {
            nets.release(&name, ip);
        }
    }

    /// Gives back a stopped container's IPv6 addresses.
    fn release_ip6s(&self, c: &mut Container) {
        let mut nets = self.lock_networks();
        if let Some(ip) = c.ip6.take() {
            if let Some(name) = nets.find(&c.spec.network_mode).map(|n| n.name.clone()) {
                nets.release6(&name, ip);
            }
        }
        for (net, ip) in std::mem::take(&mut c.extra_ip6s) {
            nets.release6(&net, ip);
        }
    }

    /// Fixed IPv6 addresses containers not running ask for on a network.
    fn wanted_addresses6(&self, network: &str) -> Vec<[u8; 16]> {
        let st = self.lock();
        let mut out = Vec::new();
        for c in st
            .containers
            .values()
            .filter(|c| c.status != Status::Running)
        {
            if c.spec.network_mode == network {
                out.extend(super::network::parse_ip6(&c.spec.ipv6_address));
            }
            out.extend(
                c.spec
                    .extra_networks
                    .iter()
                    .filter(|x| x.name == network)
                    .filter_map(|x| super::network::parse_ip6(&x.ipv6_address)),
            );
        }
        out
    }

    /// The container's IPv6 address on `n` if it has IPv6: the one asked
    /// for (`want`), or the next free one.
    fn allocate_ip6(
        &self,
        n: &super::network::Network,
        want: &str,
    ) -> Result<Option<[u8; 16]>, Error> {
        if n.v6.is_none() && want.is_empty() {
            return Ok(None);
        }
        let avoid = self.wanted_addresses6(&n.name);
        self.lock_networks()
            .allocate6(&n.name, super::network::parse_ip6(want), &avoid)
            .map_err(Error::Conflict)
    }

    /// Where `<host>` (a `*.kiri.localhost` name) should go: the container
    /// and the ports that could be its web port (domains.rs picks one).
    /// Names are the container name, or `<service>.<project>` for Compose
    /// services.
    pub fn route(&self, host: &str) -> Option<super::domains::Target> {
        let host = host.split(':').next()?.to_ascii_lowercase();
        let name = host
            .strip_suffix(".kiri.localhost")
            .or_else(|| host.strip_suffix(".kiri.local"))?
            .to_owned();
        let st = self.lock();
        let c = st.containers.values().find(|c| {
            c.status == Status::Running && {
                let label = |k: &str| c.spec.labels.get(k).and_then(|v| v.as_str()).unwrap_or("");
                let service = format!(
                    "{}.{}",
                    label("com.docker.compose.service"),
                    label("com.docker.compose.project")
                );
                // `dev.kiri.domain=api.myapp` (comma-separated for several) picks the names.
                let chosen = label(DOMAIN_LABEL)
                    .split(',')
                    .map(|d| own_name(d.trim()))
                    .any(|d| !d.is_empty() && d.eq_ignore_ascii_case(&name));
                chosen
                    || c.name.eq_ignore_ascii_case(&name)
                    || service.eq_ignore_ascii_case(&name)
                    || (name.len() >= 12 && c.id.starts_with(&name))
            }
        })?;
        let ip = if c.spec.network_mode == "host" {
            [127, 0, 0, 1]
        } else {
            c.ip?
        };
        Some(super::domains::Target {
            ip,
            id: c.id.clone(),
            label: c
                .spec
                .labels
                .get("dev.kiri.http.port")
                .and_then(|v| v.as_str())
                .and_then(|p| p.parse().ok()),
            published: c
                .spec
                .ports
                .iter()
                .filter(|b| b.proto != "udp")
                .map(|b| b.container_port)
                .collect(),
            exposed: c.spec.exposed.clone(),
        })
    }

    /// For the Finder view (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn fs_stat(&self, id: &str, path: &str) -> io::Result<super::vmlink::FileInfo> {
        self.vm()?.fs_stat(id, path)
    }

    /// For the Finder view (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn fs_list(&self, id: &str, path: &str) -> io::Result<Vec<super::vmlink::FileInfo>> {
        self.vm()?.fs_list(id, path)
    }

    /// For the Finder view (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn fs_read(&self, id: &str, path: &str, offset: u64, len: u32) -> io::Result<Vec<u8>> {
        self.vm()?.fs_read(id, path, offset, len)
    }

    /// A stream into the guest relay towards `ip:port`.
    pub fn relay_to(&self, ip: [u8; 4], port: u16) -> io::Result<kiri_vmm::VsockStream> {
        let vm = self.vm()?;
        super::ports::open_relay(&vm, ip, port)
    }

    /// Opens a `docker cp` session with the guest for a container, running
    /// or not: the request frame is sent, the caller does the rest.
    pub fn copy(
        &self,
        key: &str,
        op: u8,
        path: &str,
        flags: u32,
    ) -> Result<kiri_vmm::VsockStream, Error> {
        let (id, layers) = {
            let st = self.lock();
            let id = resolve(&st, key)?;
            let layers = st
                .containers
                .get(&id)
                .map(|c| c.layers.clone())
                .unwrap_or_default();
            (id, layers)
        };
        let vm = self.vm()?;
        let stream = vm.connect(kiri_proto::COPY_PORT)?;
        let mut out = Vec::new();
        kiri_proto::encode_copy(&mut out, op, &id, path, flags, &layers);
        std::io::Write::write_all(&mut std::fs::File::from(stream.fd.try_clone()?), &out)?;
        Ok(stream)
    }

    /// Running containers' domains, for the router's "not found" page.
    pub fn domains(&self) -> Vec<String> {
        let suffix = if super::dns::installed() {
            "kiri.local"
        } else {
            "kiri.localhost"
        };
        let mut v = self.each(|c| {
            let chosen = c
                .spec
                .labels
                .get(DOMAIN_LABEL)
                .and_then(|v| v.as_str())
                .map(|d| own_name(d.split(',').next().unwrap_or("").trim()).to_owned());
            (c.status == Status::Running).then(|| {
                format!(
                    "{}.{suffix}",
                    chosen
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(|| c.name.clone())
                )
            })
        });
        v.sort();
        v
    }

    /// Containers attached to a network.
    pub fn network_members(&self, network: &str) -> Vec<Member> {
        let canonical = self
            .lock_networks()
            .find(network)
            .map(|n| n.name.clone())
            .unwrap_or_default();
        self.each(|c| {
            let theirs = self
                .lock_networks()
                .find(&c.spec.network_mode)
                .map(|n| n.name.clone())
                .unwrap_or_default();
            if theirs == canonical {
                return Some((c.id.clone(), c.name.clone(), c.ip, c.ip6));
            }
            c.spec
                .extra_networks
                .iter()
                .any(|x| x.name == canonical)
                .then(|| {
                    let ip = c
                        .extra_ips
                        .iter()
                        .find(|(n, _)| *n == canonical)
                        .map(|(_, ip)| *ip);
                    let ip6 = c
                        .extra_ip6s
                        .iter()
                        .find(|(n, _)| *n == canonical)
                        .map(|(_, ip)| *ip);
                    (c.id.clone(), c.name.clone(), ip, ip6)
                })
        })
    }

    /// `DELETE /networks/{id}`: the network goes, and with it its bridge in
    /// the guest (addresses, routes, DNS, the IPv6 NAT rule), if the VM is up.
    pub fn network_remove(&self, key: &str) -> Result<(), String> {
        let n = self.lock_networks().find(key).cloned();
        self.lock_networks().remove(key)?;
        if let Some(n) = n.filter(|n| !n.bridge.is_empty()) {
            if let Some(vm) = self.awake() {
                let _ = vm.remove_bridge(&n.bridge);
            }
        }
        Ok(())
    }

    /// Subscribes to a container's output from now on. Returns the full id
    /// and whether it runs on a terminal (raw stream, no multiplexing).
    pub fn attach(&self, key: &str) -> Result<(String, bool, Receiver<(Stream, Vec<u8>)>), Error> {
        let mut st = self.lock();
        let id = resolve(&st, key)?;
        let (tx, rx) = mpsc::channel();
        let c = st.containers.get_mut(&id).expect("resolved");
        if c.status != Status::Exited || !c.spec.auto_remove {
            c.attached.push(tx);
        }
        Ok((id, c.spec.tty, rx))
    }

    /// Output so far, plus a live subscription if `follow` and still
    /// running; `None` with `--log-driver none`, which cannot be read.
    pub fn logs(&self, key: &str, follow: bool) -> Result<Option<Logs>, Error> {
        let st = self.lock();
        let id = resolve(&st, key)?;
        let c = st.containers.get(&id).expect("resolved");
        if !c.spec.log_config.readable() {
            return Ok(None);
        }
        let (tty, running) = (c.spec.tty, c.status != Status::Exited);
        // Under the engine's lock, so an exit can't end followers between
        // the status check and joining them.
        let log = c.log.clone();
        let mut log = lock_log(&log);
        drop(st);
        let (tx, live) = if follow && running {
            let (tx, rx) = mpsc::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let past = log.snapshot(tx);
        Ok(Some(Logs { tty, past, live }))
    }

    /// Where a container's log is, for inspect's `LogPath`.
    pub fn log_path(c: &Container) -> String {
        lock_log(&c.log).path()
    }

    pub fn input(&self, id: &str, data: &[u8]) -> io::Result<()> {
        {
            let mut st = self.lock();
            if let Some(c) = st.containers.get_mut(id) {
                if !c.live {
                    if c.status == Status::Exited {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "container has exited",
                        ));
                    }
                    c.early_input.extend_from_slice(data);
                    return Ok(());
                }
            }
        }
        self.vm()?.input(id, data)
    }

    pub fn close_stdin(&self, id: &str) -> io::Result<()> {
        {
            let mut st = self.lock();
            if let Some(c) = st.containers.get_mut(id) {
                if !c.live {
                    c.early_close = true;
                    return Ok(());
                }
            }
        }
        self.vm()?.close_stdin(id)
    }

    /// Resizes a container's (or exec's) terminal; remembered for a container
    /// that has not started yet.
    pub fn resize(&self, key: &str, rows: u16, cols: u16) -> Result<(), Error> {
        let mut st = self.lock();
        if st.execs.contains_key(key) {
            drop(st);
            return Ok(self.vm()?.resize(key, rows, cols)?);
        }
        let id = resolve(&st, key)?;
        let c = st.containers.get_mut(&id).expect("resolved");
        c.size = Some((rows, cols));
        let running = c.status == Status::Running;
        drop(st);
        if running {
            self.vm()?.resize(&id, rows, cols)?;
        }
        Ok(())
    }

    /// Blocks until the condition holds. Returns the exit code.
    /// `condition`: `not-running` (default), `next-exit` or `removed`.
    pub fn wait(&self, key: &str, condition: &str) -> Result<i32, Error> {
        let mut st = self.lock();
        let id = resolve(&st, key)?;
        let exits_before = st.containers.get(&id).map_or(0, |c| c.exits);
        loop {
            match st.containers.get(&id) {
                None => {
                    let code = st
                        .removed
                        .iter()
                        .rev()
                        .find(|(r, _)| *r == id)
                        .map_or(0, |(_, c)| *c);
                    return Ok(code);
                }
                Some(c) => match condition {
                    "removed" => {}
                    "next-exit" if c.exits > exits_before => return Ok(c.exit_code),
                    "next-exit" => {}
                    _ if c.status != Status::Running => return Ok(c.exit_code),
                    _ => {}
                },
            }
            st = self.changed.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// SIGTERM, then SIGKILL after `timeout` if it is still running.
    /// Plugs a running container into one more network as `eth{n}`.
    fn attach_extra(
        &self,
        vm: &VmLink,
        id: &str,
        n: u32,
        extra: &ExtraNetwork,
    ) -> Result<(), Error> {
        let net = self
            .lock_networks()
            .find(&extra.name)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("network {} not found", extra.name)))?;
        if net.driver != "bridge" {
            return Err(Error::BadRequest(format!(
                "only bridge networks can be connected, not {}",
                net.driver
            )));
        }
        let ip = match super::network::parse_ip(&extra.ipv4_address) {
            Some(want) => self
                .lock_networks()
                .reserve(&net.name, want)
                .map(|()| want)
                .map_err(Error::Conflict)?,
            None => {
                let avoid = self.wanted_addresses(&net.name);
                self.lock_networks()
                    .allocate_avoiding(&net.name, &avoid)
                    .ok_or_else(|| {
                        Error::Conflict(format!("no free address left on network {}", net.name))
                    })?
            }
        };
        let off6 = self
            .lock()
            .containers
            .get(id)
            .is_some_and(|c| c.spec.ipv6_disabled(&format!("eth{n}")));
        let ip6 = match if off6 {
            Ok(None)
        } else {
            self.allocate_ip6(&net, &extra.ipv6_address)
        } {
            Ok(a) => a,
            Err(e) => {
                self.lock_networks().release(&net.name, ip);
                return Err(e);
            }
        };
        let (name, hostname) = self
            .lock()
            .containers
            .get(id)
            .map(|c| (c.name.clone(), c.short_id().to_owned()))
            .unwrap_or_default();
        let mut names = vec![name, hostname];
        names.extend(extra.aliases.iter().cloned());
        let mut opts: Vec<String> = if extra.mac_address.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "{}={}",
                kiri_proto::net_opts::MAC,
                extra.mac_address
            )]
        };
        if let (Some(a), Some(v)) = (ip6, &net.v6) {
            opts.push(format!(
                "{}={}/{}",
                kiri_proto::net_opts::IP6,
                super::network::ip6_str(a),
                v.prefix
            ));
            opts.push(format!(
                "{}={}",
                kiri_proto::net_opts::GW6,
                super::network::ip6_str(v.gateway)
            ));
        }
        if net.internal {
            opts.push(format!("{}=1", kiri_proto::net_opts::INTERNAL));
        }
        if net.no_ipv4 {
            opts.push(format!("{}=1", kiri_proto::net_opts::NO_IPV4));
        }
        if let Err(e) = vm.net_attach(
            id,
            n,
            &net.bridge,
            ip,
            net.prefix,
            net.gateway,
            &names,
            &opts,
        ) {
            let mut nets = self.lock_networks();
            nets.release(&net.name, ip);
            if let Some(a) = ip6 {
                nets.release6(&net.name, a);
            }
            return Err(Error::Io(e));
        }
        if let Some(c) = self.lock().containers.get_mut(id) {
            c.extra_ips.push((net.name.clone(), ip));
            if let Some(a) = ip6 {
                c.extra_ip6s.push((net.name.clone(), a));
            }
        }
        Ok(())
    }

    /// `docker network connect`.
    pub fn connect(
        &self,
        network: &str,
        key: &str,
        aliases: Vec<String>,
        ipv4_address: String,
        ipv6_address: String,
        mac_address: String,
    ) -> Result<(), Error> {
        let id = self.resolve(key)?;
        let net = self
            .lock_networks()
            .find(network)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("network {network} not found")))?;
        if let Some(ip) = super::network::parse_ip(&ipv4_address) {
            net.check_static(ip).map_err(Error::BadRequest)?;
        }
        if let Some(ip) = super::network::parse_ip6(&ipv6_address) {
            net.check_static6(ip).map_err(Error::BadRequest)?;
        }
        let (n, running) = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).expect("resolved");
            if c.spec.network_mode == net.name
                || c.spec.extra_networks.iter().any(|x| x.name == net.name)
            {
                return Err(Error::Conflict(format!(
                    "endpoint with name {} already exists in network {}",
                    c.name, net.name
                )));
            }
            if matches!(c.spec.network_mode.as_str(), "host" | "none") {
                return Err(Error::BadRequest(format!(
                    "container cannot be connected to {} while on {}",
                    net.name, c.spec.network_mode
                )));
            }
            if c.spec.network_mode.starts_with("container:") {
                return Err(Error::BadRequest("container sharing network namespace with another container or host cannot be connected to any other network".into()));
            }
            c.spec.extra_networks.push(ExtraNetwork {
                name: net.name.clone(),
                aliases,
                ipv4_address,
                ipv6_address,
                mac_address,
            });
            (
                c.spec.extra_networks.len() as u32,
                c.status == Status::Running,
            )
        };
        self.save();
        if running {
            let extra = self
                .lock()
                .containers
                .get(&id)
                .and_then(|c| c.spec.extra_networks.last().cloned())
                .expect("just added");
            let vm = self.vm()?;
            if let Err(e) = self.attach_extra(&vm, &id, n, &extra) {
                if let Some(c) = self.lock().containers.get_mut(&id) {
                    c.spec.extra_networks.retain(|x| x.name != net.name);
                }
                return Err(e);
            }
        }
        self.emit_network("connect", &net, &id);
        Ok(())
    }

    /// `docker network disconnect`.
    pub fn disconnect(&self, network: &str, key: &str) -> Result<(), Error> {
        let id = self.resolve(key)?;
        let net = self
            .lock_networks()
            .find(network)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("network {network} not found")))?;
        let (n, ip) = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).expect("resolved");
            if c.spec.network_mode == net.name {
                return Err(Error::BadRequest(format!(
                    "Kiri cannot disconnect a container from its first network ({})",
                    net.name
                )));
            }
            let Some(pos) = c
                .spec
                .extra_networks
                .iter()
                .position(|x| x.name == net.name)
            else {
                return Err(Error::NotFound(format!(
                    "container {} is not connected to network {}",
                    &id[..12],
                    net.name
                )));
            };
            c.spec.extra_networks.remove(pos);
            let ip = c
                .extra_ips
                .iter()
                .position(|(x, _)| *x == net.name)
                .map(|i| c.extra_ips.remove(i).1);
            if let Some(i) = c.extra_ip6s.iter().position(|(x, _)| *x == net.name) {
                let (_, a) = c.extra_ip6s.remove(i);
                self.lock_networks().release6(&net.name, a);
            }
            (pos as u32 + 1, ip)
        };
        self.save();
        if let Some(ip) = ip {
            if let Some(vm) = self.awake() {
                vm.net_detach(&id, n, &net.bridge, ip)?;
            }
            self.release_ip(&net.name, ip);
        }
        self.emit_network("disconnect", &net, &id);
        Ok(())
    }

    /// `docker pause` / `docker unpause`.
    pub fn pause(&self, key: &str, paused: bool) -> Result<(), Error> {
        let id = self.resolve(key)?;
        {
            let st = self.lock();
            let c = st.containers.get(&id).expect("resolved");
            if c.status != Status::Running {
                return Err(Error::Conflict(format!(
                    "Container {} is not running",
                    &id[..12]
                )));
            }
            if c.paused == paused {
                return Err(Error::Conflict(format!(
                    "Container {} is {}paused",
                    &id[..12],
                    if paused { "already " } else { "not " }
                )));
            }
        }
        self.vm()?.freeze(&id, paused)?;
        if let Some(c) = self.lock().containers.get_mut(&id) {
            c.paused = paused;
        }
        // Docker probes only running, unpaused containers.
        if paused {
            if let Some(h) = self.lock().health.get_mut(&id) {
                close_monitor(h);
            }
            self.changed.notify_all();
        } else {
            self.health_resume(&id);
        }
        self.emit_container(if paused { "pause" } else { "unpause" }, &id, &[]);
        Ok(())
    }

    /// `docker update`: new limits now, and at every later start.
    pub fn update(
        &self,
        key: &str,
        limits: Limits,
        restart: Option<RestartPolicy>,
    ) -> Result<(), Error> {
        let id = self.resolve(key)?;
        let running = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).expect("resolved");
            c.spec.limits = limits;
            if let Some(r) = restart {
                c.spec.restart = r;
            }
            c.status == Status::Running
        };
        self.save();
        if running {
            self.vm()?.update(&id, limits.wire())?;
        }
        self.emit_container("update", &id, &[]);
        Ok(())
    }

    /// One `docker stats` sample of a running container.
    pub fn stats(&self, key: &str) -> Result<(String, String, kiri_proto::ContainerStats), Error> {
        let id = self.resolve(key)?;
        let name = self
            .lock()
            .containers
            .get(&id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!(
                "Container {} is not running",
                &id[..12]
            )));
        }
        Ok((id.clone(), name, self.vm()?.container_stats(&id)?))
    }

    /// `docker top`.
    pub fn top(&self, key: &str) -> Result<Vec<String>, Error> {
        let id = self.resolve(key)?;
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!(
                "Container {} is not running",
                &id[..12]
            )));
        }
        Ok(self.vm()?.top(&id)?)
    }

    /// Kiri's own: the TCP ports a running container listens on, split into
    /// those a published port would reach and those it would not.
    pub fn listening(&self, key: &str) -> Result<(Vec<u16>, Vec<u16>), Error> {
        let id = self.resolve(key)?;
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!(
                "Container {} is not running",
                &id[..12]
            )));
        }
        let mode = self
            .lock()
            .containers
            .get(&id)
            .map(|c| c.spec.network_mode.clone())
            .unwrap_or_default();
        let lines = self.vm()?.listening(&id)?;
        let (reach, local) = super::ports::listening(&lines, mode == "host");
        // With no network at all (or another container's), nothing can be published.
        if mode == "none" || mode.starts_with("container:") {
            return Ok((Vec::new(), [reach, local].concat()));
        }
        Ok((reach, local))
    }

    /// `docker rename`.
    /// Kiri's own: new published ports for a container. A running one keeps
    /// running: its ports are relays on the Mac, so only they are replaced.
    /// If the new ones cannot be bound, the old ones are put back.
    pub fn set_ports(&self, key: &str, ports: Vec<super::ports::Binding>) -> Result<(), Error> {
        let id = self.resolve(key)?;
        let (old_ports, old_fw, running, target) = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).expect("resolved");
            if c.spec.network_mode == "none" && !ports.is_empty() {
                return Err(Error::BadRequest(
                    "a container with --network none cannot publish ports".into(),
                ));
            }
            if c.spec.network_mode.starts_with("container:") && !ports.is_empty() {
                return Err(Error::BadRequest(
                    "conflicting options: port publishing and the container type network mode"
                        .into(),
                ));
            }
            let target = if c.spec.network_mode == "host" {
                Some([127, 0, 0, 1])
            } else {
                c.ip
            };
            let old = std::mem::replace(&mut c.spec.ports, ports.clone());
            (old, c.forwards.take(), c.status == Status::Running, target)
        };
        // Closing listeners joins their threads: never under the lock.
        drop(old_fw);
        if running && !ports.is_empty() {
            let vm = self.vm()?;
            let target = target.unwrap_or([127, 0, 0, 1]);
            match super::ports::start(vm.clone(), &id, &ports, target) {
                Ok(fw) => {
                    if let Some(c) = self.lock().containers.get_mut(&id) {
                        c.forwards = Some(fw);
                    }
                }
                Err(e) => {
                    crate::diag::warn!(
                        "new ports not published; the old ones are kept",
                        id = &id[..12],
                        error = e
                    );
                    let back = super::ports::start(vm, &id, &old_ports, target).ok();
                    if let Some(c) = self.lock().containers.get_mut(&id) {
                        c.spec.ports = old_ports;
                        c.forwards = back;
                    }
                    return Err(Error::Conflict(e.to_string()));
                }
            }
        }
        self.save();
        self.emit_container("update", &id, &[]);
        Ok(())
    }

    /// Kiri's own: a container's environment. Takes effect at its next start;
    /// says whether it is running now, so the caller can restart it.
    pub fn set_env(&self, key: &str, env: Vec<String>) -> Result<bool, Error> {
        let id = self.resolve(key)?;
        let running = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).expect("resolved");
            c.spec.env = env;
            c.status == Status::Running
        };
        self.save();
        self.emit_container("update", &id, &[]);
        Ok(running)
    }

    pub fn rename(&self, key: &str, new: &str) -> Result<(), Error> {
        let new = new.trim_start_matches('/');
        if !valid_container_name(new) {
            return Err(Error::BadRequest(format!(
                "Invalid container name ({new}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"
            )));
        }
        let id = {
            let mut st = self.lock();
            let id = resolve(&st, key)?;
            if st.containers.values().any(|c| c.name == new && c.id != id) {
                return Err(Error::Conflict(format!(
                    "Conflict. The container name \"/{new}\" is already in use by another container."
                )));
            }
            let c = st.containers.get_mut(&id).expect("resolved");
            if c.name == new {
                return Err(Error::Conflict(format!(
                    "Renaming a container with the same name as its current name"
                )));
            }
            c.name = new.to_owned();
            id
        };
        self.save();
        self.emit_container("rename", &id, &[]);
        Ok(())
    }

    /// Pauses an idle BuildKit container, and resumes it at once if an exec
    /// started in it meanwhile: exec_start marks its exec running before it
    /// looks for the pause, so one of the two sees the other.
    pub fn pause_idle_builder(&self, id: &str) -> Result<(), Error> {
        self.pause(id, true)?;
        if self.running_execs(id) > 0 {
            self.pause(id, false)?;
            return Err(Error::Conflict("an exec started in it".into()));
        }
        Ok(())
    }

    /// Whether a container is frozen.
    pub fn is_paused(&self, id: &str) -> bool {
        self.lock().containers.get(id).is_some_and(|c| c.paused)
    }

    pub fn stop(&self, key: &str, timeout: Duration) -> Result<(), Error> {
        let id = self.resolve(key)?;
        self.thaw_for_signal(&id);
        self.mark_stopped(&id);
        if !self.is_running(&id) {
            return Ok(());
        }
        self.emit_container("kill", &id, &[("signal", (libc::SIGTERM).to_string())]);
        let sig = self
            .lock()
            .containers
            .get(&id)
            .and_then(|c| c.spec.stop_signal)
            .unwrap_or(libc::SIGTERM as u32);
        self.vm()?.kill(&id, sig)?;
        let deadline = std::time::Instant::now() + timeout;
        let mut st = self.lock();
        while st
            .containers
            .get(&id)
            .is_some_and(|c| c.status == Status::Running)
        {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                drop(st);
                self.vm()?.kill(&id, libc::SIGKILL as u32)?;
                self.emit_container("stop", &id, &[]);
                return Ok(());
            }
            st = self
                .changed
                .wait_timeout(st, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        drop(st);
        self.emit_container("stop", &id, &[]);
        Ok(())
    }

    /// A frozen process cannot handle a signal; Docker thaws it first.
    fn thaw_for_signal(&self, id: &str) {
        if self.is_paused(id) {
            if let Ok(vm) = self.vm() {
                if vm.freeze(id, false).is_ok() {
                    if let Some(c) = self.lock().containers.get_mut(id) {
                        c.paused = false;
                    }
                    self.health_resume(id);
                }
            }
        }
    }

    pub fn kill(&self, key: &str, signal: u32) -> Result<(), Error> {
        let id = self.resolve(key)?;
        self.thaw_for_signal(&id);
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!(
                "Container {} is not running",
                &id[..12]
            )));
        }
        if signal == libc::SIGKILL as u32 || signal == libc::SIGTERM as u32 {
            self.mark_stopped(&id);
        }
        self.emit_container("kill", &id, &[("signal", signal.to_string())]);
        self.vm()?.kill(&id, signal)?;
        // Like Docker: a SIGKILL returns only once the exit is recorded, so
        // an immediate inspect, start or rm sees the container stopped.
        if signal == libc::SIGKILL as u32 {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut st = self.lock();
            while st
                .containers
                .get(&id)
                .is_some_and(|c| c.status == Status::Running)
            {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                st = self
                    .changed
                    .wait_timeout(st, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        Ok(())
    }

    pub fn remove(&self, key: &str, force: bool, with_volumes: bool) -> Result<(), Error> {
        let id = self.resolve(key)?;
        if self.is_running(&id) {
            if !force {
                return Err(Error::Conflict(format!(
                    "cannot remove container \"{}\": container is running: stop the container before removing or force remove",
                    &id[..12]
                )));
            }
            self.mark_stopped(&id);
            self.vm()?.kill(&id, libc::SIGKILL as u32)?;
            let mut st = self.lock();
            while st
                .containers
                .get(&id)
                .is_some_and(|c| c.status == Status::Running)
            {
                st = self
                    .changed
                    .wait_timeout(st, Duration::from_secs(5))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        self.emit_container("destroy", &id, &[]);
        let mut st = self.lock();
        let removed = st.containers.remove(&id);
        let code = removed.as_ref().map_or(0, |c| c.exit_code);
        note_removed(&mut st, &id, code);
        st.execs.retain(|_, e| e.container_id != id);
        drop(st);
        self.changed.notify_all();
        let anon = removed
            .as_ref()
            .map(|c| c.spec.anon_volumes.clone())
            .unwrap_or_default();
        if let Some(c) = &removed {
            lock_log(&c.log).remove();
        }
        if removed.is_some_and(|c| !c.spec.auto_remove) {
            self.save();
        }
        // Its writable layer is still on the guest's disk.
        self.vm()?.remove(&id)?;
        if with_volumes {
            for v in anon {
                self.delete_volume_data(&v);
            }
        }
        Ok(())
    }

    fn lock_volumes(&self) -> MutexGuard<'_, super::volumes::Volumes> {
        self.volumes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a volume (creating it if needed). Its directory appears in
    /// the guest on first use.
    pub fn volume_ensure(
        &self,
        name: &str,
        labels: serde_json::Value,
        anonymous: bool,
    ) -> Result<super::volumes::Volume, Error> {
        let new = self.volume_get(name).is_none();
        let v = self
            .lock_volumes()
            .ensure(name, labels, anonymous)
            .map_err(Error::BadRequest)?;
        if new {
            self.emit_volume("create", name);
        }
        Ok(v)
    }

    /// [`Engine::volume_ensure`] with the local driver's options, which a
    /// new volume keeps (an existing one keeps its own).
    pub fn volume_ensure_with(
        &self,
        name: &str,
        labels: serde_json::Value,
        options: std::collections::BTreeMap<String, String>,
    ) -> Result<super::volumes::Volume, Error> {
        let new = self.volume_get(name).is_none();
        let v = self
            .lock_volumes()
            .ensure_with(name, labels, false, options)
            .map_err(Error::BadRequest)?;
        if new {
            self.emit_volume("create", name);
        }
        Ok(v)
    }

    /// Containers using a volume, by name.
    pub fn volume_users(&self, name: &str) -> Vec<String> {
        self.each(|c| {
            (c.spec
                .volumes
                .iter()
                .chain(&c.spec.anon_volumes)
                .any(|v| v == name))
            .then(|| c.name.clone())
        })
    }

    pub fn volume_list(&self) -> Vec<super::volumes::Volume> {
        self.lock_volumes().all().to_vec()
    }

    pub fn volume_get(&self, name: &str) -> Option<super::volumes::Volume> {
        self.lock_volumes().get(name).cloned()
    }

    pub fn volume_remove(&self, name: &str, force: bool) -> Result<(), Error> {
        if self.volume_get(name).is_none() {
            return Err(Error::NotFound(format!("get {name}: no such volume")));
        }
        let users = self.volume_users(name);
        if !users.is_empty() && !force {
            return Err(Error::Conflict(format!(
                "remove {name}: volume is in use - [{}]",
                users.join(", ")
            )));
        }
        self.delete_volume_data(name);
        Ok(())
    }

    /// Forgets a volume and deletes its data in the guest.
    fn delete_volume_data(&self, name: &str) {
        if self.lock_volumes().forget(name) {
            if let Ok(vm) = self.vm() {
                let _ = vm.remove_volume(name);
            }
            self.emit_volume("destroy", name);
        }
    }

    /// `docker exec` step one: record what to run.
    pub fn exec_create(&self, key: &str, config: ExecConfig) -> Result<String, Error> {
        let mut st = self.lock();
        let container_id = resolve(&st, key)?;
        if st
            .containers
            .get(&container_id)
            .is_none_or(|c| c.status != Status::Running)
        {
            return Err(Error::Conflict(format!(
                "container {} is not running",
                &container_id[..12]
            )));
        }
        // A paused BuildKit container is only idle: the exec resumes it. If the
        // idle pause comes back before the exec starts, exec_start resumes it.
        let builder = st
            .containers
            .get(&container_id)
            .is_some_and(|c| c.paused && is_builder(c));
        if builder {
            drop(st);
            match self.pause(&container_id, false) {
                Ok(()) | Err(Error::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
            st = self.lock();
        }
        if !builder && st.containers.get(&container_id).is_some_and(|c| c.paused) {
            return Err(Error::Conflict(format!(
                "Container {} is paused, unpause the container before exec",
                &container_id[..12]
            )));
        }
        if config.argv.is_empty() {
            return Err(Error::BadRequest("no exec command specified".into()));
        }
        let id = random_hex(32);
        let cmd = config.argv.join(" ");
        let internal = config.internal;
        st.execs.insert(
            id.clone(),
            Exec {
                id: id.clone(),
                container_id: container_id.clone(),
                config,
                running: false,
                exit_code: None,
                pid: 0,
                attached: Vec::new(),
            },
        );
        drop(st);
        if !internal {
            self.emit_container(
                &format!("exec_create: {cmd}"),
                &container_id,
                &[("execID", id.clone())],
            );
        }
        Ok(id)
    }

    /// `docker exec` step two: start it. Output arrives on the receiver
    /// until the process exits.
    pub fn exec_start(&self, exec_id: &str) -> Result<(bool, Receiver<(Stream, Vec<u8>)>), Error> {
        let (tx, rx) = mpsc::channel();
        let exec_security;
        let (container_id, config, env) = {
            let mut st = self.lock();
            let e = st
                .execs
                .get_mut(exec_id)
                .ok_or_else(|| Error::NotFound(format!("No such exec instance: {exec_id}")))?;
            if e.running || e.exit_code.is_some() {
                return Err(Error::Conflict("exec has already run".into()));
            }
            e.running = true;
            // Subscribe before the process exists, so no output is missed.
            e.attached.push(tx);
            let (container_id, config) = (e.container_id.clone(), e.config.clone());
            let (security, caps) = st
                .containers
                .get(&container_id)
                .map(|c| {
                    (
                        c.spec.security_flags() & !run_flags::READONLY_ROOTFS,
                        c.spec.caps,
                    )
                })
                .unwrap_or_default();
            // Execs get the container's ulimits and groups, as with runc.
            let opts = st
                .containers
                .get(&container_id)
                .map(|c| c.spec.guest_opts(false).unwrap_or_default())
                .unwrap_or_default();
            exec_security = (security, caps, opts);
            let mut env = st
                .containers
                .get(&container_id)
                .map(|c| c.spec.env.clone())
                .unwrap_or_default();
            for e in &config.env {
                let key = e.split('=').next().unwrap_or_default();
                env.retain(|x| x.split('=').next() != Some(key));
                env.push(e.clone());
            }
            (container_id, config, env)
        };
        // Paused by the idle watch since the exec was made: started frozen, it
        // would hang. Marked running first, so pause_idle_builder sees it if
        // it pauses after this look.
        if self
            .lock()
            .containers
            .get(&container_id)
            .is_some_and(|c| c.paused && is_builder(c))
            && let Err(e) = self.pause(&container_id, false)
            && !matches!(e, Error::Conflict(_))
        {
            crate::diag::warn!(
                "BuildKit container not resumed for an exec",
                error = format!("{e:?}")
            );
        }
        let mut flags = 0;
        if config.tty {
            flags |= run_flags::TTY;
        }
        if config.stdin {
            flags |= run_flags::STDIN;
        }
        if config.privileged {
            flags |= run_flags::PRIVILEGED;
        }
        flags |= exec_security.0;
        let console = config.console_size;
        let started = self.vm()?.exec(
            &container_id,
            exec_id,
            &config.argv,
            &env,
            &config.cwd,
            &config.user,
            flags,
            exec_security.1,
            &exec_security.2,
        );
        if let Ok(pid) = started {
            if let Some(x) = self
                .lock()
                .execs
                .get_mut(exec_id)
                .filter(|x| x.exit_code.is_none())
            {
                x.pid = pid;
            }
        }
        if let Err(e) = started {
            let mut st = self.lock();
            if let Some(x) = st.execs.get_mut(exec_id) {
                x.running = false;
                x.exit_code = Some(126);
                x.attached.clear();
            }
            drop(st);
            self.changed.notify_all();
            return Err(Error::Io(e));
        }
        if let (true, Some((rows, cols))) = (config.tty, console) {
            let _ = self.vm()?.resize(exec_id, rows, cols);
        }
        if !config.internal {
            self.emit_container(
                &format!("exec_start: {}", config.argv.join(" ")),
                &container_id,
                &[("execID", exec_id.to_owned())],
            );
        }
        Ok((config.tty, rx))
    }

    /// Sets an exec's initial terminal size, given at start.
    pub fn exec_console(&self, exec_id: &str, size: (u16, u16)) {
        if let Some(e) = self.lock().execs.get_mut(exec_id) {
            e.config.console_size = Some(size);
        }
    }

    pub fn with_exec<T>(&self, exec_id: &str, f: impl FnOnce(&Exec) -> T) -> Result<T, Error> {
        let st = self.lock();
        st.execs
            .get(exec_id)
            .map(f)
            .ok_or_else(|| Error::NotFound(format!("No such exec instance: {exec_id}")))
    }

    /// Execs running in a container now (buildx's `buildctl dial-stdio`
    /// while it builds, for one): the managed builder isn't idle while any are.
    pub fn running_execs(&self, container_id: &str) -> usize {
        self.lock()
            .execs
            .values()
            .filter(|e| e.container_id == container_id && e.running)
            .count()
    }

    /// Drops a finished exec's record: Kiri's own, which nobody inspects.
    pub fn forget_exec(&self, exec_id: &str) {
        self.lock().execs.remove(exec_id);
    }

    fn is_running(&self, id: &str) -> bool {
        self.lock()
            .containers
            .get(id)
            .is_some_and(|c| c.status == Status::Running)
    }

    fn mark_stopped(&self, id: &str) {
        let persist = {
            let mut st = self.lock();
            match st.containers.get_mut(id) {
                Some(c) => {
                    c.stopped_by_user = true;
                    !c.spec.auto_remove
                }
                None => false,
            }
        };
        if persist {
            self.save();
        }
    }

    /// Runs `f` over every container, under the lock.
    pub fn each<T>(&self, mut f: impl FnMut(&Container) -> Option<T>) -> Vec<T> {
        self.lock().containers.values().filter_map(&mut f).collect()
    }

    pub fn with<T>(&self, key: &str, f: impl FnOnce(&Container) -> T) -> Result<T, Error> {
        let st = self.lock();
        let id = resolve(&st, key)?;
        Ok(f(st.containers.get(&id).expect("resolved")))
    }

    pub fn guest_memory(&self) -> Option<(u64, u64)> {
        self.awake()?
            .stats()
            .ok()
            .map(|s| (s.mem_total_kib * 1024, s.mem_available_kib * 1024))
    }

    /// Resident memory of the guest agent, in bytes.
    pub fn agent_rss(&self) -> Option<u64> {
        self.awake()?.stats().ok().map(|s| s.agent_rss_kib * 1024)
    }

    /// Stops everything for a daemon shutdown, as Docker does: each running
    /// container gets its stop signal and its own stop timeout, then
    /// SIGKILL. They are not stopped by the user, so restart policies bring
    /// them back when the daemon next starts.
    pub fn shutdown(&self) {
        crate::diag::info!("VM stopping for shutdown");
        SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
        self.save();
        for log in self.each(|c| Some(c.log.clone())) {
            lock_log(&log).flush();
        }
        let ids: Vec<String> = self.each(|c| (c.status == Status::Running).then(|| c.id.clone()));
        if let Some(vm) = self.awake() {
            self.stop_all_for_shutdown(&vm, &ids);
        }
        if let Some(vm) = self.awake() {
            vm.shutdown();
            if !vm.wait_stopped(Duration::from_secs(10)) {
                crate::diag::warn!("the VM did not power off in 10 s; stopped it");
            }
        }
    }
}

/// A Docker event in Kiri's log: containers and images coming and going,
/// with name, image and exit code; never labels (they can hold anything).
/// A BuildKit container's exec is a build session (`docker build`).
/// The `key=value` network settings the guest takes with `Run`.
fn net_opts(spec: &Spec, joined: Option<&str>, domainname: &str) -> Vec<String> {
    use kiri_proto::net_opts as k;
    let mut opts = Vec::new();
    if let Some(target) = joined {
        opts.push(format!("{}={target}", k::NETNS));
    }
    if !spec.mac_address.is_empty() {
        opts.push(format!("{}={}", k::MAC, spec.mac_address));
    }
    opts.extend(spec.dns.iter().map(|d| format!("{}={d}", k::NAMESERVER)));
    opts.extend(spec.dns_search.iter().map(|d| format!("{}={d}", k::SEARCH)));
    opts.extend(
        spec.dns_options
            .iter()
            .map(|d| format!("{}={d}", k::OPTION)),
    );
    if !domainname.is_empty() {
        opts.push(format!("{}={domainname}", k::DOMAINNAME));
    }
    opts
}

fn log_event(
    typ: &str,
    action: &str,
    actor: &str,
    attrs: &serde_json::Map<String, serde_json::Value>,
) {
    let attr = |k: &str| {
        attrs
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned()
    };
    let id: String = actor
        .strip_prefix("sha256:")
        .unwrap_or(actor)
        .chars()
        .take(12)
        .collect();
    let (name, image) = (attr("name"), attr("image"));
    match (typ, action) {
        ("container", "die") => {
            let code = attr("exitCode");
            if code == "0" {
                crate::diag::info!(
                    "container exited",
                    id = id,
                    name = name,
                    image = image,
                    exit_code = code
                );
            } else {
                crate::diag::warn!(
                    "container exited",
                    id = id,
                    name = name,
                    image = image,
                    exit_code = code
                );
            }
        }
        ("container", "exec_die") if name.starts_with("buildx_buildkit_") => {
            crate::diag::info!(
                "build session ended",
                builder = name,
                exit_code = attr("exitCode")
            );
        }
        ("container", "create" | "start" | "stop" | "destroy" | "kill" | "rename" | "commit") => {
            crate::diag::info!(
                format!("container {action}"),
                id = id,
                name = name,
                image = image
            );
        }
        ("image", _) => crate::diag::info!(format!("image {action}"), id = id, name = attr("name")),
        _ => crate::diag::debug!(format!("{typ} {action}"), id = id, name = name),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What a container event carries: labels, then name and image.
fn container_attrs(c: &Container) -> serde_json::Map<String, serde_json::Value> {
    let mut attrs = c.spec.labels.as_object().cloned().unwrap_or_default();
    attrs.insert("name".into(), c.name.clone().into());
    attrs.insert("image".into(), c.spec.image.clone().into());
    attrs
}

fn note_removed(st: &mut State, id: &str, code: i32) {
    // A removed container's monitor sees its record gone and quits.
    st.health.remove(id);
    st.removed.push_back((id.to_owned(), code));
    while st.removed.len() > 1024 {
        st.removed.pop_front();
    }
}

/// A BuildKit daemon that buildx runs for its docker-container driver.
fn is_builder(c: &Container) -> bool {
    c.name.starts_with("buildx_buildkit_")
}

fn resolve(st: &State, key: &str) -> Result<String, Error> {
    let key = key.trim_start_matches('/');
    if st.containers.contains_key(key) {
        return Ok(key.to_owned());
    }
    if let Some(c) = st.containers.values().find(|c| c.name == key) {
        return Ok(c.id.clone());
    }
    let mut matches = st
        .containers
        .keys()
        .filter(|id| !key.is_empty() && id.starts_with(key));
    match (matches.next(), matches.next()) {
        (Some(id), None) => Ok(id.clone()),
        (Some(_), Some(_)) => Err(Error::NotFound(format!("multiple containers match {key}"))),
        _ => Err(Error::NotFound(format!("No such container: {key}"))),
    }
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| io::Read::read_exact(&mut f, &mut b))
        .is_err()
    {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, x) in b.iter_mut().enumerate() {
            *x = (t >> ((i % 16) * 8)) as u8 ^ i as u8;
        }
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The label a container chooses its local names with.
pub const DOMAIN_LABEL: &str = "dev.kiri.domain";

/// `api.myapp`, from any of `api.myapp`, `api.myapp.kiri.local` or
/// `api.myapp.kiri.localhost`.
fn own_name(d: &str) -> &str {
    d.strip_suffix(".kiri.localhost")
        .or_else(|| d.strip_suffix(".kiri.local"))
        .unwrap_or(d)
}

/// Docker's container-name rule: non-empty, `[a-zA-Z0-9_.-]` only. A name
/// is a DNS label on user networks and part of `<name>.kiri.localhost`, so
/// it must not carry spaces, slashes or control characters.
fn valid_container_name(n: &str) -> bool {
    !n.is_empty()
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// The image `name` means: a name, an ID, or `name@digest` and
/// `name:tag@digest` by the platform manifest's digest or the index's it was
/// pulled by.
fn image_named(images: Vec<Image>, name: &str) -> Option<Image> {
    let reference = kiri_image::Reference::parse(name).ok();
    let short = reference
        .as_ref()
        .map(|r| r.short())
        .unwrap_or_else(|| name.to_owned());
    images.into_iter().find(|i| {
        i.name == short
            || i.name == name
            || reference.as_ref().is_some_and(|r| i.matches(r))
            || i.config_digest == name
            || i.config_digest
                .strip_prefix("sha256:")
                .is_some_and(|h| name.len() >= 12 && h.starts_with(name))
    })
}

#[cfg(test)]
mod image_lookup_tests {
    use kiri_image::{Image, RunConfig};

    const INDEX: &str = "sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5";
    const ARM64: &str = "sha256:6a3345f517a7df05185c648f83d18d464f2a94ea4a69eaee39b7e1de02611e59";

    fn image(name: &str, index: Option<&str>) -> Image {
        Image {
            name: name.into(),
            digest: ARM64.into(),
            index_digest: index.map(str::to_owned),
            config_digest: format!("sha256:{}", "c".repeat(64)),
            layers: Vec::new(),
            size: 0,
            platform: "linux/arm64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        }
    }

    fn found(images: &[Image], name: &str) -> bool {
        super::image_named(images.to_vec(), name).is_some()
    }

    #[test]
    fn a_pull_by_index_digest_is_found_by_either_digest() {
        // What `kind create cluster` pulls, then creates the node from.
        let pulled = [image(&format!("kindest/node@{INDEX}"), Some(INDEX))];
        for name in [
            format!("kindest/node@{INDEX}"),
            format!("kindest/node:v1.37.0@{INDEX}"),
            format!("docker.io/kindest/node:v1.37.0@{INDEX}"),
            format!("kindest/node@{ARM64}"),
            format!("kindest/node:v1.37.0@{ARM64}"),
        ] {
            assert!(found(&pulled, &name), "{name}");
        }
        assert_eq!(pulled[0].repo_digest(), format!("kindest/node@{INDEX}"));
    }

    #[test]
    fn a_digest_only_finds_images_of_its_repository() {
        let pulled = [image("kindest/node:v1.37.0", Some(INDEX))];
        assert!(found(&pulled, &format!("kindest/node@{INDEX}")));
        assert!(found(&pulled, "kindest/node:v1.37.0"));
        assert!(!found(&pulled, &format!("kindest/haproxy@{INDEX}")));
        assert!(!found(&pulled, &format!("ghcr.io/kindest/node@{INDEX}")));
        assert!(!found(
            &pulled,
            &format!("kindest/node@sha256:{}", "b".repeat(64))
        ));
        assert!(!found(&pulled, "kindest/node:v1.36.0"));
    }

    #[test]
    fn an_image_without_an_index_shows_its_manifest_digest() {
        let pulled = image("alpine:latest", None);
        assert_eq!(pulled.repo_digest(), format!("alpine@{ARM64}"));
        assert!(found(&[pulled], &format!("alpine:3@{ARM64}")));
    }
}

#[cfg(test)]
mod k8s_range_tests {
    #[test]
    fn only_the_clusters_pod_and_service_ranges_can_be_forwarded() {
        assert!(super::in_k8s_ranges([10, 43, 42, 10]));
        assert!(super::in_k8s_ranges([10, 42, 0, 17]));
        assert!(!super::in_k8s_ranges([10, 44, 0, 1]));
        assert!(!super::in_k8s_ranges([172, 17, 0, 4]));
        assert!(!super::in_k8s_ranges([127, 0, 0, 1]));
        assert!(!super::in_k8s_ranges([192, 168, 64, 1]));
    }
}

/// Docker's `CloseMonitorChannel`: the monitor stops, and a container whose
/// monitor has stopped reads as unhealthy.
fn close_monitor(h: &mut HealthRun) {
    if h.monitoring {
        h.monitoring = false;
        h.epoch += 1;
        h.health.status = super::health::UNHEALTHY.into();
    }
}

fn error_text(e: &Error) -> String {
    match e {
        Error::NotFound(m) | Error::Conflict(m) | Error::BadRequest(m) | Error::Unauthorized(m) => {
            m.clone()
        }
        Error::Io(e) => e.to_string(),
    }
}

/// The longest a daemon shutdown waits for one container, whatever its
/// `--stop-timeout` says.
const SHUTDOWN_WAIT_CAP: Duration = Duration::from_secs(120);

impl Engine {
    /// How long `docker stop` waits when the request does not say: the
    /// container's `--stop-timeout`, else 10 s.
    pub fn stop_timeout(&self, key: &str) -> Duration {
        stop_wait(
            self.with(key, |c| c.spec.stop_timeout)
                .ok()
                .flatten()
                .unwrap_or(DEFAULT_STOP_TIMEOUT),
        )
    }

    /// Daemon shutdown: every running container gets its stop signal and its
    /// own stop timeout, as `docker stop` would, then SIGKILL.
    fn stop_all_for_shutdown(&self, vm: &VmLink, ids: &[String]) {
        let t0 = std::time::Instant::now();
        // (id, when to SIGKILL it, when it was SIGKILLed)
        let mut pending: Vec<(String, std::time::Instant, Option<std::time::Instant>)> = Vec::new();
        for id in ids {
            let Some((sig, secs)) = self.lock().containers.get(id).map(|c| {
                (
                    c.spec.stop_signal.unwrap_or(libc::SIGTERM as u32),
                    c.spec.stop_timeout.unwrap_or(DEFAULT_STOP_TIMEOUT),
                )
            }) else {
                continue;
            };
            self.thaw_for_signal(id);
            let _ = vm.kill(id, sig);
            pending.push((
                id.clone(),
                t0 + stop_wait(secs).min(SHUTDOWN_WAIT_CAP),
                None,
            ));
        }
        let mut st = self.lock();
        loop {
            let now = std::time::Instant::now();
            pending.retain(|(id, _, _)| {
                st.containers
                    .get(id)
                    .is_some_and(|c| c.status == Status::Running)
            });
            for (id, deadline, killed) in &mut pending {
                if *deadline <= now && killed.is_none() {
                    let _ = vm.kill(id, libc::SIGKILL as u32);
                    *killed = Some(now);
                }
            }
            // A SIGKILLed container's exit is quick; don't hang on one that isn't.
            if pending
                .iter()
                .all(|(_, _, k)| k.is_some_and(|k| now.duration_since(k) > Duration::from_secs(2)))
            {
                break;
            }
            let next = pending
                .iter()
                .filter(|(_, _, k)| k.is_none())
                .map(|(_, d, _)| d.saturating_duration_since(now))
                .min()
                .unwrap_or(Duration::from_millis(100));
            st = self
                .changed
                .wait_timeout(
                    st,
                    next.clamp(Duration::from_millis(10), Duration::from_millis(250)),
                )
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// `State.Health` of a container with a healthcheck.
    pub fn health(&self, id: &str) -> Option<super::health::Health> {
        self.lock().health.get(id).map(|h| h.health.clone())
    }

    /// Health status by container id, for `docker ps`.
    pub fn health_statuses(&self) -> HashMap<String, super::health::Health> {
        self.lock()
            .health
            .iter()
            .map(|(id, h)| (id.clone(), h.health.clone()))
            .collect()
    }

    /// Docker's `initHealthMonitor` at every start: status back to
    /// starting, failing streak 0, the log kept, and a monitor probing.
    fn health_start(&self, id: &str) {
        {
            let mut st = self.lock();
            let wanted = st.containers.get(id).is_some_and(|c| {
                c.spec
                    .healthcheck
                    .as_ref()
                    .and_then(|h| h.probe(&c.spec.shell))
                    .is_some()
            });
            if !wanted {
                return;
            }
            let run = st.health.entry(id.to_owned()).or_default();
            close_monitor(run);
            run.health.status = super::health::STARTING.into();
            run.health.failing_streak = 0;
        }
        self.health_resume(id);
    }

    /// Docker's `updateHealthMonitor`: a monitor while the container runs
    /// unpaused (after `unpause`, its status stays until the next probe).
    fn health_resume(&self, id: &str) {
        let Some(me) = self.me.get().and_then(Weak::upgrade) else {
            return;
        };
        let epoch = {
            let mut st = self.lock();
            let wanted = st.containers.get(id).is_some_and(|c| {
                c.status == Status::Running
                    && !c.paused
                    && c.spec
                        .healthcheck
                        .as_ref()
                        .and_then(|h| h.probe(&c.spec.shell))
                        .is_some()
            });
            let Some(run) = st.health.get_mut(id) else {
                return;
            };
            if !wanted || run.monitoring {
                return;
            }
            run.monitoring = true;
            run.epoch += 1;
            run.epoch
        };
        let id = id.to_owned();
        let spawned = std::thread::Builder::new()
            .name("kiri-health".into())
            .spawn(move || me.monitor(&id, epoch));
        if let Err(e) = spawned {
            crate::diag::error!("healthcheck not started", error = e);
        }
    }

    /// Probes a container every interval until its monitor is stopped
    /// (exit, pause, removal: `epoch` moves on).
    fn monitor(&self, id: &str, epoch: u64) {
        let current = |st: &State| st.health.get(id).is_some_and(|h| h.epoch == epoch);
        loop {
            let (check, argv, cwd, user, started, wait) = {
                let st = self.lock();
                let (Some(c), Some(run)) = (st.containers.get(id), st.health.get(id)) else {
                    return;
                };
                if run.epoch != epoch {
                    return;
                }
                let Some(check) = c.spec.healthcheck.clone() else {
                    return;
                };
                let Some(argv) = check.probe(&c.spec.shell) else {
                    return;
                };
                let started = c.started.unwrap_or_else(SystemTime::now);
                let since = started.elapsed().unwrap_or_default();
                let wait = run.health.next_wait(since, &check);
                (
                    check,
                    argv,
                    c.spec.cwd.clone(),
                    c.spec.user.clone(),
                    started,
                    wait,
                )
            };
            let deadline = std::time::Instant::now() + wait;
            {
                let mut st = self.lock();
                loop {
                    if !current(&st) {
                        return;
                    }
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    st = self
                        .changed
                        .wait_timeout(st, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
            let start = SystemTime::now();
            let (exit_code, output) = self.probe(id, argv, cwd, user, check.timeout());
            let end = SystemTime::now();
            let changed = {
                let mut st = self.lock();
                let auto_remove = st.containers.get(id).is_some_and(|c| c.spec.auto_remove);
                let Some(run) = st.health.get_mut(id).filter(|h| h.epoch == epoch) else {
                    return;
                };
                // Stopped meanwhile: Docker drops the result.
                let before = run.health.status.clone();
                let since = start.duration_since(started).unwrap_or_default();
                let probe = super::health::Probe {
                    start: super::time::rfc3339(start),
                    end: super::time::rfc3339(end),
                    exit_code,
                    output,
                };
                run.health.record(probe, since, &check);
                (run.health.status != before).then(|| (run.health.status.clone(), auto_remove))
            };
            if let Some((status, auto_remove)) = changed {
                self.changed.notify_all();
                self.emit_container(&format!("health_status: {status}"), id, &[]);
                if !auto_remove {
                    self.save();
                }
            }
        }
    }

    /// One healthcheck probe: an exec in the container, as Docker runs it
    /// (the container's user and working directory, no terminal). Returns
    /// the exit code (-1 when it could not run or timed out) and output.
    fn probe(
        &self,
        id: &str,
        argv: Vec<String>,
        cwd: String,
        user: String,
        timeout: Duration,
    ) -> (i64, String) {
        use super::health::{OUTPUT_LIMIT, cap_output, go_duration};
        let config = ExecConfig {
            console_size: None,
            argv,
            env: Vec::new(),
            cwd,
            user,
            tty: false,
            stdin: false,
            privileged: false,
            internal: false,
        };
        let exec_id = match self.exec_create(id, config) {
            Ok(e) => e,
            Err(e) => return (-1, error_text(&e)),
        };
        let rx = match self.exec_start(&exec_id) {
            Ok((_, rx)) => rx,
            Err(e) => {
                self.lock().execs.remove(&exec_id);
                return (-1, error_text(&e));
            }
        };
        let deadline = std::time::Instant::now() + timeout;
        let mut out = Vec::new();
        let mut timed_out = false;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok((_, data)) => {
                    // One byte past the limit is enough to know it was cut.
                    let room = (OUTPUT_LIMIT + 1).saturating_sub(out.len());
                    out.extend_from_slice(&data[..data.len().min(room)]);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    timed_out = true;
                    break;
                }
            }
        }
        if timed_out {
            // Not left running: probes would pile up in a stuck container.
            if let Some(vm) = self.awake() {
                let _ = vm.kill(&exec_id, libc::SIGKILL as u32);
            }
            let until = std::time::Instant::now() + Duration::from_secs(5);
            while rx
                .recv_timeout(until.saturating_duration_since(std::time::Instant::now()))
                .is_ok()
            {}
        }
        let code = self.with_exec(&exec_id, |e| e.exit_code).ok().flatten();
        self.lock().execs.remove(&exec_id);
        if timed_out {
            return (
                -1,
                format!("Health check exceeded timeout ({})", go_duration(timeout)),
            );
        }
        (code.map_or(-1, i64::from), cap_output(&out))
    }
}

#[cfg(test)]
mod net_opts_tests {
    #[test]
    fn dns_mac_and_joined_namespace_reach_the_guest() {
        let spec: super::Spec = serde_json::from_value(serde_json::json!({
            "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": ["sh"], "env": [], "cwd": "", "user": "",
            "auto_remove": false, "network_mode": "bridge", "labels": {},
            "mac_address": "02:42:ac:11:00:09", "dns": ["8.8.8.8", "1.1.1.1"], "dns_search": ["corp.example"], "dns_options": ["ndots:2"],
        }))
        .unwrap();
        assert_eq!(
            super::net_opts(&spec, Some("abc"), "example.com"),
            [
                "netns=abc",
                "mac=02:42:ac:11:00:09",
                "nameserver=8.8.8.8",
                "nameserver=1.1.1.1",
                "search=corp.example",
                "option=ndots:2",
                "domainname=example.com"
            ]
        );
        // Records saved before these settings existed still load.
        let old: super::Spec = serde_json::from_value(serde_json::json!({
            "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": ["sh"], "env": [], "cwd": "", "user": "",
            "auto_remove": false, "network_mode": "bridge", "labels": {},
        }))
        .unwrap();
        assert!(super::net_opts(&old, None, "").is_empty());
    }
}
