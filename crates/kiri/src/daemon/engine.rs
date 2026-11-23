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
const MEMORY_SHRINK_STEP: u64 = 256 << 20;
const MEMORY_DEADBAND: u64 = 64 << 20;

/// The balloon's next target, in bytes, or None to leave it.
///
/// The guest keeps `MEMORY_HEADROOM` of usable memory (`MemAvailable`) beyond
/// everything Linux cannot hand out: the boot-time kernel reserve (`ceiling`
/// less `MemTotal`, mostly `struct page`), zone reclaim watermarks and
/// `lowmem_reserve`, uncharged kernel memory, container memory and the share
/// of page cache Linux excludes from `MemAvailable` (up to `wmark_low`). On
/// an 8 GiB VM the boot reserve alone is ~173 MiB, and on a 24 GiB VM zone
/// watermarks, `lowmem_reserve` and uncharged kernel memory add another
/// ~265 MiB — enough to leave `MemAvailable` at zero even when idle if only
/// `used` and the boot reserve are counted (#509).
///
/// When the guest reports its live balloon size (`balloon`), `ceiling` less
/// `balloon + mem_available` is that exact non-available total, independent
/// of inflation or deflation lag while `mem_available` stays above zero.
/// Whenever `mem_available` is below `MEMORY_HEADROOM` (such as right after
/// `grow_memory()`, before the balloon finishes deflating, or with an older
/// agent that reports no balloon size), `target` is never shrunk. Without a
/// balloon reading, fall back to `used + (ceiling - mem_total)` and never
/// shrink past `MEMORY_HEADROOM` of `mem_available`. Shrinks in steps, so the
/// guest's caches go gradually, and grows at once.
fn next_memory_target(
    used: u64,
    mem_total: u64,
    mem_available: u64,
    balloon: Option<u64>,
    ceiling: u64,
    target: u64,
) -> Option<u64> {
    let reserved = if mem_total > 0 { ceiling.saturating_sub(mem_total) } else { 0 };
    let floor_need = used.saturating_add(reserved);
    let need = match balloon {
        Some(b) if mem_total > 0 => ceiling.saturating_sub(b.saturating_add(mem_available)).max(floor_need),
        _ => floor_need,
    };
    let want = need.saturating_add(MEMORY_HEADROOM).clamp(MEMORY_FLOOR.min(ceiling), ceiling);
    let want = if mem_total > 0 {
        let min_target = match balloon {
            Some(_) if mem_available < MEMORY_HEADROOM => target.min(ceiling),
            Some(_) => 0,
            None => target.min(ceiling).saturating_sub(mem_available.saturating_sub(MEMORY_HEADROOM)),
        };
        want.max(min_target)
    } else {
        want
    };
    let next = if want < target { want.max(target.saturating_sub(MEMORY_SHRINK_STEP)) } else { want };
    (next.abs_diff(target) >= MEMORY_DEADBAND).then_some(next)
}

/// How much memory the guest holds, in bytes, from a `Stats` sample.
fn estimate_used(stats: &kiri_proto::Stats, ceiling: u64, target: u64, host_rss: u64) -> u64 {
    if stats.mem_used_kib > 0 {
        // The agent's count: anonymous, shared and kernel memory from the
        // cgroup, which neither the page cache nor the balloon's own pages
        // get into.
        stats.mem_used_kib.saturating_mul(1024).saturating_add(host_rss)
    } else {
        // An agent from before that count: MemTotal minus MemAvailable, with
        // the balloon taken back out. Apple's balloon offers deflate-on-OOM,
        // so Linux counts its pages as used rather than shrinking MemTotal.
        // Without a live balloon reading, the balloon is guessed at
        // ceiling - target, which it only is once the guest has caught up.
        let apparent = stats.mem_total_kib.saturating_sub(stats.mem_available_kib).saturating_mul(1024);
        let balloon = stats.mem_balloon_kib.map_or_else(|| ceiling.saturating_sub(target), |k| k.saturating_mul(1024));
        apparent.saturating_sub(balloon)
    }
}

const FALLBACK_SHRINK_SETTLE_TICKS: u8 = 5;

enum PendingFallback {
    /// Waiting for an in-flight grow to show up in `MemAvailable` before the
    /// legacy (`mem_used_kib == 0`, no `mem_balloon_kib`) fallback may grow
    /// again.
    Grow { avail: u64, peak: u64 },
    /// Waiting for an in-flight shrink to show up in `MemAvailable` before the
    /// legacy fallback may shrink again.
    Shrink { avail: u64, expected_avail: u64, ticks: u8 },
}

#[derive(Default)]
struct BalloonState {
    last_target: Option<u64>,
    pending: Option<PendingFallback>,
}

impl BalloonState {
    fn reset(&mut self) {
        self.last_target = None;
        self.pending = None;
    }

    fn next_target(&mut self, stats: &kiri_proto::Stats, used: u64, ceiling: u64, target: u64) -> Option<u64> {
        let mem_total = stats.mem_total_kib.saturating_mul(1024);
        let mem_available = stats.mem_available_kib.saturating_mul(1024);
        let balloon = stats.mem_balloon_kib.map(|k| k.saturating_mul(1024));
        let legacy_fallback = stats.mem_used_kib == 0 && balloon.is_none() && mem_total > 0;

        if !legacy_fallback {
            self.pending = None;
            let next = next_memory_target(used, mem_total, mem_available, balloon, ceiling, target);
            self.last_target = Some(next.unwrap_or(target));
            return next;
        }

        // An external grow (e.g. `grow_memory()` on pressure) raised `target`
        // since the last poll: wait for deflation to show up in `MemAvailable`
        // before allowing the target-derived fallback to grow again.
        if self.last_target.is_some_and(|prev| target > prev) {
            self.pending = Some(PendingFallback::Grow { avail: mem_available, peak: mem_available });
        }

        match self.pending {
            Some(PendingFallback::Grow { avail, mut peak }) => {
                if mem_available > peak {
                    peak = mem_available;
                }
                let recovered = mem_available >= MEMORY_HEADROOM
                    || (peak >= avail.saturating_add(MEMORY_DEADBAND)
                        && mem_available.saturating_add(MEMORY_DEADBAND) <= peak);
                self.pending = (!recovered).then_some(PendingFallback::Grow { avail, peak });
            }
            Some(PendingFallback::Shrink { avail, expected_avail, ticks }) => {
                let settled = mem_available < MEMORY_HEADROOM
                    || mem_available <= expected_avail.saturating_add(MEMORY_DEADBAND)
                    || mem_available >= avail.saturating_add(MEMORY_DEADBAND)
                    || ticks >= FALLBACK_SHRINK_SETTLE_TICKS;
                self.pending = (!settled).then_some(PendingFallback::Shrink {
                    avail,
                    expected_avail,
                    ticks: ticks.saturating_add(1),
                });
            }
            None => {}
        }

        let next = next_memory_target(used, mem_total, mem_available, None, ceiling, target);
        let next = match (next, &self.pending) {
            (Some(n), Some(PendingFallback::Grow { .. })) if n > target => None,
            (Some(n), Some(PendingFallback::Shrink { .. })) if n < target => None,
            (next, _) => next,
        };

        if let Some(n) = next {
            self.pending = if n > target {
                Some(PendingFallback::Grow { avail: mem_available, peak: mem_available })
            } else {
                Some(PendingFallback::Shrink {
                    avail: mem_available,
                    expected_avail: mem_available.saturating_sub(target.saturating_sub(n)),
                    ticks: 0,
                })
            };
        }
        self.last_target = Some(next.unwrap_or(target));
        next
    }
}

/// What a request that needs the VM gets while Kiri is stopped (Stop in
/// the window, the tray or the menu bar): only Start ends that.
pub const STOPPED: &str = "Kiri is stopped: run `kiri start`, or press Start in the Kiri window, to run containers again";
/// What one gets while the daemon shuts down (WSL or Windows stopping it,
/// a restart, an update): not a Stop, so nothing to press; the next
/// command reaches the engine that comes up after it (#93).
pub const SHUTTING: &str = "Kiri's engine is shutting down: run the command again in a moment";

/// The file that keeps "stopped" across engine restarts.
fn stopped_marker() -> Option<std::path::PathBuf> {
    super::state_dir().ok().map(|d| d.join("stopped"))
}

/// The VM's memory ceiling: `KIRI_MEMORY_MIB`, else Settings → Resources,
/// else half the Mac's RAM up to 8 GiB.
pub fn memory_ceiling() -> u64 {
    if let Some(mib) = std::env::var("KIRI_MEMORY_MIB").ok().and_then(|v| v.parse::<u64>().ok()) {
        return mib << 20;
    }
    if let Some(mib) = resources()["memory_mib"].as_u64() {
        return (mib << 20).clamp(1 << 30, physical_memory());
    }
    (physical_memory() / 2).clamp(2 << 30, 8 << 30)
}

/// The VM's CPUs: `KIRI_CPUS`, else Settings → Resources, else 2. On
/// Linux there is no VM: containers run on the host's kernel, on every CPU
/// the engine may use, and that is the count (`docker info` NCPU).
pub fn vm_cpus() -> usize {
    let max = max_cpus();
    if cfg!(target_os = "linux") {
        return max;
    }
    std::env::var("KIRI_CPUS").ok().and_then(|v| v.parse().ok()).or_else(|| resources()["cpus"].as_u64().map(|c| c as usize)).unwrap_or(2).clamp(1, max)
}

/// The Mac's CPUs, the most the VM can have. On Linux the CPUs the engine
/// may run on, counted as dockerd counts them for NCPU (Go's
/// runtime.NumCPU: the affinity mask, not a cgroup's CPU quota).
pub fn max_cpus() -> usize {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: cpu_set_t is plain data; sched_getaffinity fills it.
        let n = unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 { libc::CPU_COUNT(&set) } else { 0 }
        };
        if n > 0 {
            return n as usize;
        }
    }
    std::thread::available_parallelism().map_or(2, std::num::NonZero::get)
}

/// What Settings → Resources saved, read at each boot: `{"cpus", "memory_mib"}`.
pub fn resources_path() -> Option<std::path::PathBuf> {
    super::state_dir().ok().map(|d| d.join("vm.json"))
}

pub fn resources() -> serde_json::Value {
    resources_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(serde_json::Value::Null)
}

pub fn physical_memory_bytes() -> u64 {
    physical_memory()
}

#[cfg(target_os = "macos")]
fn physical_memory() -> u64 {
    let mut ram: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: sysctlbyname writes a u64 into `ram`.
    unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut ram as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
    ram
}

/// This process's resident memory, in bytes: on a Linux host the daemon runs
/// beside its containers, so it counts in what Kiri holds.
#[cfg(not(target_os = "macos"))]
fn self_rss() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
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
    info.lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).map_or(4 << 30, |k| k << 10)
}

/// Docker's restart backoff: 100 ms doubling, capped at one minute.
const RESTART_BASE: Duration = Duration::from_millis(100);
const RESTART_CAP: Duration = Duration::from_secs(60);

fn restart_backoff(attempt: u32) -> Duration {
    RESTART_BASE.saturating_mul(1 << attempt.min(10)).min(RESTART_CAP)
}

/// A VM up this long before it crashed starts the crash backoff over, as
/// Docker resets a container's restart delay after 10 s of running.
const VM_STABLE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    User,
    Restart,
    DaemonRestart,
    /// Held by Stop, or by a VM that did not boot: Start or `retry_held`.
    Held,
    VmRecovery,
}

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
    /// `network_mode` is `none` because the container left every network it
    /// had (`docker network disconnect`), not because it was made with
    /// `--network none`: it starts with no network as such a one does, but
    /// unlike it can be connected again, and the network it is connected to
    /// then becomes its first. Records from before it read false.
    #[serde(default)]
    pub left_all_networks: bool,
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
    /// `--volumes-from` as given, for inspect; the mounts it brought are in
    /// `mounts` and `mount_info` with the others.
    #[serde(default)]
    pub volumes_from: Vec<String>,
    /// `--pid`: empty for a PID namespace of its own, `host`, or
    /// `container:<full id>`.
    #[serde(default)]
    pub pid_mode: String,
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
    /// `--cpu-shares` as asked for, which `inspect` reports back; the
    /// kernel gets `cpu_weight`. 0 when not set, or saved before 0.2.3.
    #[serde(default)]
    pub cpu_shares: u64,
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
    /// Whether `network` (a network's name) is the first network, kept in
    /// `network_mode`; the API stores the name, but `default` is bridge.
    pub fn first_network_is(&self, network: &str) -> bool {
        self.network_mode == network || (network == "bridge" && matches!(self.network_mode.as_str(), "default" | ""))
    }

    /// Leaves the first network of a container that is not running, as
    /// `docker network disconnect` does on Docker for any of its networks.
    /// Kiri keeps the first network apart (`network_mode`, `aliases`,
    /// `ipv4_address`, `ipv6_address`, `mac_address`) because only it gives
    /// the default route and the main address, so one of the others takes
    /// its place, with its own aliases, fixed addresses and MAC: none of the
    /// left network's settings follow, or a fixed `--ip` would be asked for
    /// on a network it does not belong to. The one picked is the one Docker
    /// gives the default route to: a network that reaches outside before an
    /// `internal` one (which gives no default route), then by name, as
    /// libnetwork orders a container's endpoints when none has a gateway
    /// priority (Kiri keeps none), and as create picks the primary of
    /// several endpoints. `internal` says whether a network is internal.
    /// With no other network it is left with none, as with `--network none`.
    pub fn leave_first_network(&mut self, internal: impl Fn(&str) -> bool) {
        let next = self.extra_networks.iter().enumerate().min_by(|(_, a), (_, b)| internal(&a.name).cmp(&internal(&b.name)).then_with(|| a.name.cmp(&b.name))).map(|(i, _)| i);
        match next {
            Some(i) => {
                let x = self.extra_networks.remove(i);
                self.join_first_network(x);
            }
            None => {
                self.network_mode = "none".into();
                self.left_all_networks = true;
                self.aliases.clear();
                self.ipv4_address.clear();
                self.ipv6_address.clear();
                self.mac_address.clear();
            }
        }
    }

    /// Makes `x` the first network, with its settings in place of the old
    /// first network's.
    fn join_first_network(&mut self, x: ExtraNetwork) {
        self.network_mode = x.name;
        self.left_all_networks = false;
        self.aliases = x.aliases;
        self.ipv4_address = x.ipv4_address;
        self.ipv6_address = x.ipv6_address;
        self.mac_address = x.mac_address;
    }

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

    /// The program for `--security-opt seccomp=<profile>`, compiled for this
    /// container's capabilities; empty for the default filter and for
    /// `unconfined`. `--privileged` only drops the default filter: an
    /// explicit profile still applies, as in moby's `WithSeccomp`. A profile
    /// Kiri cannot apply is an error, never the default.
    pub fn seccomp_program(&self) -> Result<Vec<u8>, Error> {
        let super::seccomp::Choice::Profile(json) = super::seccomp::choice(&self.security_opt).map_err(Error::BadRequest)? else {
            return Ok(Vec::new());
        };
        super::seccomp::compile(json, super::api::profile_caps(self.caps, self.privileged)).map_err(Error::BadRequest)
    }

    /// The security flags for the guest: seccomp, no-new-privileges, read-only root.
    pub fn security_flags(&self) -> u32 {
        let mut f = 0;
        if matches!(super::seccomp::choice(&self.security_opt), Ok(super::seccomp::Choice::Unconfined)) {
            f |= run_flags::NO_SECCOMP;
        }
        if self.security_opt.iter().any(|o| o == "no-new-privileges" || o == "no-new-privileges:true" || o == "no-new-privileges=true") {
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

/// Shut while a container's start is under way: from when it is marked
/// Running until the guest has answered the start, which waits until the
/// container's root is in place (a new volume filled). An exec or a copy
/// sent meanwhile waits here, then reaches a container that can be
/// entered, as it did when the guest answered nothing else meanwhile.
///
/// Every start has a gate of its own (`Container::begin_start` replaces the
/// container's gate with a new one), so a gate is shut once in its life and
/// opened once, by its start's guard. Whoever holds a gate waits for the
/// one start that was under way when it took it, never for a later start
/// of the same container: else two starts could wait for each other (A
/// took B's gate; B starts again, and its new start takes A's).
///
/// Only those wait, and they hold no lock while they do. A start waits
/// only for the gates that were already shut when it shut its own, each of
/// a start that began earlier, which itself waits only for still earlier
/// ones: no wait can be part of a cycle. The start holds its gate across
/// the waits and `VmLink::run`, which waits for nothing but the guest.
#[derive(Debug, Default)]
struct StartGate {
    shut: Mutex<bool>,
    opened: Condvar,
    /// Set by `Engine::await_start`: a stop, kill or `rm -f` found this start
    /// stuck (`START_GRACE`, `REMOVE_GRACE`), or Kiri itself is stopping.
    /// `VmLink::run` reads it.
    cancel: std::sync::atomic::AtomicBool,
    /// Why it was cancelled (`Cancel`), for the start's answer.
    cancel_by: std::sync::atomic::AtomicU8,
}

/// Why a start was cancelled (`StartGate::cancel_by`); the larger wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Cancel {
    /// A stop or kill that waited `START_GRACE` for it, or Kiri's own
    /// stop (Stop, the VM's sleep).
    Stopped = 1,
    /// `rm -f`, after `REMOVE_GRACE`.
    Removed = 2,
}

/// How long a `docker stop` or `kill` that comes while the container is
/// starting waits for that start, before it takes the start as stuck and
/// cancels it (see `Engine::await_start`). Docker's waits for as long as
/// the start takes (its start holds the container's lock throughout); a
/// healthy start here takes milliseconds, a queue of them a few seconds, and
/// a start filling a new volume is let finish the fill even when cancelled.
#[cfg(not(test))]
const START_GRACE: Duration = Duration::from_secs(10);
// Tests: wide enough apart that a slow, shared machine can't push one wait
// into the other's window (300 and 400 ms did, at 436 ms).
#[cfg(test)]
const START_GRACE: Duration = Duration::from_secs(2);
/// The same for `rm -f`, which is expected to be prompt: a stuck start it
/// cancels sooner, and that start answers with Docker's words for a start
/// a removal got to first ("marked for removal").
#[cfg(not(test))]
const REMOVE_GRACE: Duration = Duration::from_secs(2);
#[cfg(test)]
const REMOVE_GRACE: Duration = Duration::from_secs(1);

/// What a cancelled start answers (see `Cancel`), as a 409 each time.
fn cancelled_start_error(by: Option<Cancel>, id: &str) -> Option<Error> {
    match by? {
        // Docker's answer to a start that a removal got to first.
        Cancel::Removed => Some(Error::Conflict("container is marked for removal and cannot be started".into())),
        Cancel::Stopped => Some(Error::Conflict(format!("container {} was stopped before it finished starting", &id[..12.min(id.len())]))),
    }
}

impl StartGate {
    fn shut(self: &Arc<Self>) -> StartGuard {
        *self.shut.lock().unwrap_or_else(|e| e.into_inner()) = true;
        StartGuard(self.clone())
    }

    fn is_shut(&self) -> bool {
        *self.shut.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait(&self) {
        let mut shut = self.shut.lock().unwrap_or_else(|e| e.into_inner());
        while *shut {
            shut = self.opened.wait(shut).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits at most `limit` for the gate to open. True when it is open.
    fn wait_for(&self, limit: Duration) -> bool {
        let deadline = std::time::Instant::now() + limit;
        let mut shut = self.shut.lock().unwrap_or_else(|e| e.into_inner());
        while *shut {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            shut = self.opened.wait_timeout(shut, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    fn cancelled_by(&self) -> Option<Cancel> {
        match self.cancel_by.load(std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            1 => Some(Cancel::Stopped),
            _ => Some(Cancel::Removed),
        }
    }
}

/// See `Engine::take_early`.
#[derive(Debug, PartialEq, Eq)]
enum Early {
    Input(Vec<u8>),
    Live { close: bool },
    Gone,
}

/// Opens the gate again when dropped, whichever way the start ends.
struct StartGuard(Arc<StartGate>);

impl Drop for StartGuard {
    fn drop(&mut self) {
        *self.0.shut.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.0.opened.notify_all();
    }
}

#[derive(Debug)]
pub struct Container {
    /// Serializes start with stop/kill/remove through guest creation.
    lifecycle: Arc<Mutex<u64>>,
    /// Orders the changes to the container's DNS names in the guest: a
    /// rename, a network joined and one left each read the names and tell
    /// the guest under it, so none is applied over another's.
    names_sync: Arc<Mutex<()>>,
    /// See `StartGate`.
    start_gate: Arc<StartGate>,
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
    /// Exited, with its restart policy about to start it again.
    restart_pending: bool,
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
    /// The interface each extra network is plugged in as while running,
    /// as (network, n) for `eth{n}`. A disconnect leaves a gap, so the
    /// number can't be worked out from the network's place in the list.
    pub extra_ifs: Vec<(String, u32)>,
    /// IPv6 addresses while running: on `network_mode`, and on extra
    /// networks as (network, ip). Only networks with IPv6 give one.
    pub ip6: Option<[u8; 16]>,
    pub extra_ip6s: Vec<(String, [u8; 16])>,
    /// True while the guest has the process. Until then, input is held here:
    /// `docker run -i` attaches and sends stdin before it starts the container.
    live: bool,
    early_input: Vec<u8>,
    early_close: bool,
    /// This run's `start` event is still to be sent: by the start once it
    /// makes the container live, or by its exit, if that came first (a
    /// start that waited for a new volume to fill can be overtaken by its
    /// exit), so `start` always comes before `die`.
    start_owed: bool,
    /// `docker logs`: its output on disk, and who follows it.
    log: Arc<Mutex<super::logfile::Log>>,
    attached: Vec<Sender<(Stream, Vec<u8>)>>,
}

impl Container {
    /// Begins a start: a new gate (see `StartGate`), shut. Under the engine
    /// lock, with the container's lifecycle lock held, which serialises the
    /// starts of one container: the old gate is open by then, its start's
    /// guard having dropped before that start returned.
    fn begin_start(&mut self) -> StartGuard {
        self.start_gate = Arc::default();
        self.start_gate.shut()
    }

    pub fn argv(&self) -> Vec<String> {
        self.spec.entrypoint.iter().chain(&self.spec.cmd).cloned().collect()
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

impl Exec {
    /// The VM stopped under it. A running exec gets an exit code (255, as its
    /// container does) before its output stream closes: docker-java
    /// (Testcontainers' `execInContainer`) inspects the exec as soon as the
    /// stream ends and fails on a finished exec with a null `ExitCode` (#551).
    /// One never started keeps none, as in Docker.
    fn end_with_vm(&mut self) {
        if self.running && self.exit_code.is_none() {
            self.exit_code = Some(255);
        }
        self.running = false;
        self.pid = 0;
        self.attached.clear();
    }
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

/// What `load` reads of `containers.json`.
struct Records {
    saved: Vec<Saved>,
    damage: Damage,
    /// The damaged file's bytes when no copy of them could be kept: the
    /// next save keeps a copy before it replaces the file.
    pending_copy: Option<Vec<u8>>,
    /// What is in `containers.json` after the read ([`fingerprint`]): what
    /// was read, or the recovered list written over the damaged one.
    on_disk: Option<(usize, u64)>,
}

/// Length and hash of a file's bytes, enough to tell whether it is the one
/// this engine last read or wrote.
fn fingerprint(bytes: &[u8]) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    (bytes.len(), h.finish())
}

/// How to restart the engine itself on this system, for the warning: the
/// window's Stop and Quit leave it running.
fn restart_hint() -> String {
    if cfg!(target_os = "macos") {
        format!("`launchctl kickstart -k gui/$(id -u)/{}`", crate::flavor::AGENT.get())
    } else if std::path::Path::new("/run/systemd/system").exists() {
        "`sudo systemctl restart kiri`".into()
    } else if std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|r| r.to_ascii_lowercase().contains("microsoft")) {
        "`wsl -d kiri -u root kiri-start --restart` in a Windows terminal".into()
    } else {
        "stop and start `kiri daemon`".into()
    }
}

/// Why the container list may be missing containers. While any of it holds,
/// nothing is deleted for not being in the list.
///
/// A list that could not be read in full says nothing about which folders
/// belong to containers that are gone: the ones cut from it are still the
/// person's. So this lasts as long as a `containers.json.corrupt-*` copy is
/// kept beside the file, not just for the start that found the damage: a
/// later clean start with a readable but short list would otherwise delete
/// the same folders. Removing the copies (after putting back what they
/// held, or deciding it is not needed) and restarting is what lets the
/// engine tidy up again. Orphaned folders cost disk; deleting them on a
/// guess could cost someone's data.
#[derive(Clone, Debug, Default)]
struct Damage {
    /// Copies of damaged lists beside the file, oldest first.
    copies: Vec<PathBuf>,
    /// Why no copy of the damaged list could be kept: saves wait for one.
    unsaved: Option<String>,
    /// Why the folder could not be listed: copies there can't be ruled out.
    unlistable: Option<String>,
}

impl Damage {
    fn any(&self) -> bool {
        !self.copies.is_empty() || self.unsaved.is_some() || self.unlistable.is_some()
    }

    /// One message for `docker info` and for what is refused because of
    /// it: what happened, where the copies are, what is kept and refused,
    /// and what ends it. True in each state; each copy named once.
    fn message(&self, records: &std::path::Path) -> Option<String> {
        if !self.any() {
            return None;
        }
        let dir = records.parent().map(|d| d.display().to_string()).unwrap_or_default();
        let mut m = Vec::new();
        if let Some(e) = &self.unsaved {
            m.push(format!("The container list in {dir} is damaged and no copy of it could be kept ({e}), so changes to containers are not saved until Kiri can write to that folder."));
        }
        let copies = self.copies.iter().map(|c| c.display().to_string()).collect::<Vec<_>>().join(", ");
        match (self.copies.len(), self.unsaved.is_some()) {
            (0, _) => {}
            (1, false) => m.push(format!("The container list was damaged; a copy is kept at {copies}.")),
            (_, false) => m.push(format!("The container list was damaged; copies are kept at {copies}.")),
            (1, true) => m.push(format!("A copy of an earlier damaged list is kept at {copies}.")),
            (_, true) => m.push(format!("Copies of earlier damaged lists are kept at {copies}.")),
        }
        if let Some(e) = &self.unlistable {
            m.push(format!("Kiri cannot list {dir} ({e}), so it cannot tell whether a damaged container list was kept there."));
        }
        if self.unsaved.is_some() || !self.copies.is_empty() {
            m.push("Containers missing from `docker ps -a` cannot be listed again, but their volumes are still there and their files and logs are kept on disk.".into());
        }
        let refuses = "Kiri deletes nothing on its own, and refuses volume, image and network prunes and removals of volumes and images without --force";
        let mut until = Vec::new();
        match self.copies.len() {
            0 => {}
            1 => until.push("the copy is removed"),
            _ => until.push("the copies are removed"),
        }
        if self.unlistable.is_some() {
            until.push("that folder can be listed");
        }
        if until.is_empty() {
            m.push(format!("{refuses} until a copy can be kept."));
        } else {
            m.push(format!("{refuses} until {} and the engine itself is restarted ({}); then it tidies up and removes the files and logs of containers not in the list.", until.join(" and "), restart_hint()));
        }
        Some(m.join(" "))
    }
}

/// The `docker info` warning for a repaired image list (#440): where the
/// damaged copy is, which images it names that the list lost, and that
/// their unpacked layers are kept until the copy is removed. `Err`: the
/// list or its folder could not be read, so lost images cannot be ruled out.
fn image_damage_message(damage: &io::Result<Option<kiri_image::store::IndexDamage>>, root: &std::path::Path) -> Option<String> {
    let d = match damage {
        Ok(None) => return None,
        Ok(Some(d)) => d,
        Err(e) => return Some(format!("Kiri cannot read the image list in {} ({e}), so it cannot tell whether images are missing from it; it keeps every unpacked layer meanwhile.", root.display())),
    };
    let copies = d.copies.iter().map(|c| c.display().to_string()).collect::<Vec<_>>().join(", ");
    let (one, kept) = if d.copies.len() == 1 { ("a copy of it is", "the copy is") } else { ("copies are", "the copies are") };
    let missing = match d.missing.len() {
        0 => "Every image it names is listed again.".to_owned(),
        n => format!("{n} image(s) it names are missing from `docker images`: {}. Pulled ones can be pulled again; built or committed ones are in the copy.", d.missing.join(", ")),
    };
    Some(format!("The image list was damaged and repaired; {one} kept at {copies}. {missing} Kiri keeps every unpacked layer until {kept} removed."))
}

/// Reads `containers.json`. One that does not parse is kept as a
/// `containers.json.corrupt-*` copy, then replaced in one rename by its
/// complete leading records, which are what is loaded: the list file is
/// never missing. A missing file is an empty list; copies beside it still
/// count as damage. Only a file that cannot be read at all is an error.
fn read_records(path: &std::path::Path) -> io::Result<Records> {
    use kiri_image::jsonfile;
    jsonfile::remove_stale_temps(path);
    let mut damage = Damage::default();
    let mut pending_copy = None;
    let mut made = None;
    let mut on_disk = None;
    let copies = jsonfile::kept_copies(path);
    let saved = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(saved) => {
                on_disk = Some(fingerprint(&bytes));
                saved
            }
            Err(e) => {
                let saved: Vec<Saved> = jsonfile::salvage(&bytes);
                match jsonfile::keep_copy(path, &bytes) {
                    Ok(copy) => {
                        crate::diag::error!("container records unreadable: kept a copy and loaded what could be read",
                            file = path.display(), copy = copy.display(), recovered = saved.len(), error = e);
                        let written = serde_json::to_vec(&saved).map_err(io::Error::other)
                            .and_then(|json| jsonfile::replace(path, &json, jsonfile::Durable::Now).map(|()| json));
                        match written {
                            Ok(json) => on_disk = Some(fingerprint(&json)),
                            // Still the damaged bytes, which the copy holds.
                            Err(err) => {
                                on_disk = Some(fingerprint(&bytes));
                                crate::diag::error!("the recovered container records were not written; the next save writes them", file = path.display(), error = err);
                            }
                        }
                        made = Some(copy);
                    }
                    Err(err) => {
                        crate::diag::error!("container records unreadable, and no copy could be kept: loaded what could be read; the list is not saved until a copy is kept",
                            file = path.display(), recovered = saved.len(), error = e, copy_error = err);
                        damage.unsaved = Some(err.to_string());
                        pending_copy = Some(bytes);
                    }
                }
                saved
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    match copies {
        Ok(copies) => damage.copies = copies,
        Err(e) => damage.unlistable = Some(e.to_string()),
    }
    damage.copies.extend(made);
    Ok(Records { saved, damage, pending_copy, on_disk })
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
    vm_recovery_at: Option<std::time::Instant>,
    /// VM crashes in a row (none lasted VM_STABLE): the recovery delay.
    vm_crashes: u32,
    /// When the running VM booted.
    vm_booted_at: Option<std::time::Instant>,
    vm_recovery_running: bool,
    vm_recovery_generation: u64,
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

/// What a request that would boot the VM is told, if anything: a Stop
/// says so, and wins; a shutdown says to try again.
fn boot_refusal(stopped: bool, shutting_down: bool) -> Option<&'static str> {
    if stopped { Some(STOPPED) } else if shutting_down { Some(SHUTTING) } else { None }
}

/// `docker stop`'s default wait, when neither the request nor the container sets one.
pub const DEFAULT_STOP_TIMEOUT: i64 = 10;

/// A stop timeout in seconds as a wait; negative means as long as it takes.
pub fn stop_wait(secs: i64) -> Duration {
    if secs < 0 { Duration::from_secs(365 * 86_400) } else { Duration::from_secs(secs as u64) }
}

/// One network's DNS names for a renamed container: (bridge, IPv4 key, IPv6,
/// whether the network has IPv4 addresses, names).
type RenamedDns = (String, [u8; 4], Option<[u8; 16]>, bool, Vec<String>);

/// The names a `DnsRename` carries: each once (the guest's DNS names are
/// lowercase), none empty, in order (the container's name first) and no
/// more than the frame takes (`kiri_proto::MAX_DNS_NAMES`).
fn rename_names(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names.into_iter().filter(|n| !n.is_empty() && seen.insert(n.to_ascii_lowercase())).take(kiri_proto::MAX_DNS_NAMES).collect()
}

pub struct Engine {
    pub volumes: Mutex<super::volumes::Volumes>,
    /// The VM, while it is awake. It sleeps (powers off, freeing all its
    /// memory) when idle, and boots again on the next request.
    link: Mutex<Option<Arc<VmLink>>>,
    /// Serialises retirement and boot without blocking access to the link.
    waking: Mutex<()>,
    #[cfg(test)]
    boot_attempts: std::sync::atomic::AtomicUsize,
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
    /// Serialises writes of `containers.json`. Taken before the engine's
    /// state lock, never while holding it.
    saving: Mutex<()>,
    /// Wakes the thread that syncs `containers.json` to disk after a save
    /// (`start_syncer`); none once `shutdown` has stopped it.
    syncer: Mutex<Option<Sender<()>>>,
    /// Syncs of `containers.json` the syncer thread has made.
    syncer_syncs: Arc<std::sync::atomic::AtomicU64>,
    /// Why the container list may be missing containers, found by `load`
    /// (or by a save that kept a copy later): what `docker info` warns
    /// about. While any of it holds nothing is deleted for not being in the
    /// list (see [`Damage`]).
    records_damage: Mutex<Damage>,
    /// Length and hash of what is on disk as `containers.json` as far as
    /// this engine knows (what it read at start, then what it wrote), kept
    /// only while the list may be damaged: a save then keeps a file it did
    /// not write as a copy before replacing it (`keep_foreign_copy`).
    on_disk: Mutex<Option<(usize, u64)>>,
    /// The damaged list's bytes while no copy of them could be kept: `save`
    /// keeps them aside first, and replaces nothing until it could.
    pending_copy: Mutex<Option<Vec<u8>>>,
    me: OnceLock<Weak<Engine>>,
    /// Containers that come back on Start: the ones whose restart policy
    /// would have started them when the daemon came up, held because Kiri
    /// was stopped (Start starts them) or because the VM did not boot
    /// (`retry_held` tries it again), and the ones Stop (or the VM's sleep on
    /// request) stopped, which `always` and `unless-stopped` bring back.
    held_restarts: Mutex<Vec<String>>,
    /// Taken by Stop (`sleep_now`) for its whole run and by `start_held`
    /// while it starts held containers. A Stop during a Start waits for the
    /// one container being started, then stops it with the rest, and the
    /// ones not started yet stay held; a Start during a Stop starts them
    /// once the Stop is done.
    power: Mutex<()>,
    /// Set while `sleep_now` stops containers: those exits are the engine's,
    /// not ends the restart policy answers (Start brings them back).
    parking: std::sync::atomic::AtomicBool,
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
    /// Each image's config Labels by config digest, for `docker images` and
    /// `system df`, so a listing does not read every config from disk.
    /// Configs are content-addressed, so an entry never goes stale; entries
    /// of images no longer stored are dropped on the next listing.
    image_labels: Mutex<HashMap<String, serde_json::Value>>,
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
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// A container the VM's crash stopped, that its restart policy brings back
/// as soon as a new VM is up (`recover_vm`).
fn recover_after_vm_loss(c: &Container) -> bool {
    c.vm_stopped && c.restart_pending && c.status == Status::Exited && !c.stopped_by_user && !c.spec.auto_remove
        && matches!(c.spec.restart.name.as_str(), "always" | "unless-stopped")
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
            waking: Mutex::new(()),
            #[cfg(test)]
            boot_attempts: std::sync::atomic::AtomicUsize::new(0),
            last_activity: std::sync::atomic::AtomicU64::new(now_secs()),
            sleeping: std::sync::atomic::AtomicBool::new(false),
            stopped: std::sync::atomic::AtomicBool::new(stopped_marker().is_some_and(|f| f.exists())),
            networks,
            store,
            state: Mutex::default(),
            changed: Condvar::new(),
            layers: Mutex::new(()),
            records,
            saving: Mutex::new(()),
            records_damage: Mutex::new(Damage::default()),
            pending_copy: Mutex::new(None),
            on_disk: Mutex::new(None),
            syncer: Mutex::new(None),
            syncer_syncs: Arc::default(),
            me: OnceLock::new(),
            held_restarts: Mutex::new(Vec::new()),
            power: Mutex::new(()),
            parking: std::sync::atomic::AtomicBool::new(false),
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
            image_labels: Mutex::new(HashMap::new()),
        });
        let _ = engine.me.set(Arc::downgrade(&engine));
        engine.start_syncer();
        engine.load()?;
        engine.start_balloon();
        engine.start_cpu_meter();
        engine.start_sleeper();
        engine.start_clock();
        engine.start_k8s_lb();
        #[cfg(target_os = "macos")]
        super::watch::start(Arc::downgrade(&engine));
        Ok(engine)
    }

    /// Why the container list may be missing containers, for `docker info`.
    pub fn records_damage(&self) -> Vec<String> {
        self.records_damage.lock().unwrap_or_else(|e| e.into_inner()).message(&self.records).into_iter().collect()
    }

    /// What the agent found wrong with a Linux host, for `docker info`
    /// (`VmLink::host_warnings`).
    pub fn host_warnings(&self) -> Vec<String> {
        self.awake().map(|vm| vm.host_warnings()).unwrap_or_default()
    }

    /// Why unpacked layers may belong to images the image list lost, for
    /// `docker info`.
    pub fn image_damage(&self) -> Option<String> {
        image_damage_message(&self.store.index_damage(), self.store.root())
    }

    /// Whether nothing is deleted for not being in the container list.
    fn keeps_everything(&self) -> bool {
        self.records_damage.lock().unwrap_or_else(|e| e.into_inner()).any()
    }

    /// Why something must not be removed now that is removed for being
    /// unused by every listed container (a prune, or a removal by name
    /// without force, which the window sends for "Delete"): the list may be
    /// missing containers that use it. `what` says what was not done, so
    /// the text is true for the request it answers; the rest is the
    /// `docker info` message. A forced removal is the person's call.
    pub fn refusal(&self, what: &str) -> Option<String> {
        let m = self.records_damage.lock().unwrap_or_else(|e| e.into_inner()).message(&self.records)?;
        Some(format!("{what}. {m}"))
    }

    /// The containers whose folders the guest keeps at boot, everything
    /// else on its disk being deleted; `None` when it must delete nothing
    /// (the list may be missing containers).
    fn guest_keeps(&self) -> Option<Vec<String>> {
        (!self.keeps_everything()).then(|| self.lock().containers.keys().cloned().collect())
    }

    /// The running VM, booting it first if it is asleep (about 0.1 s).
    pub fn vm(&self) -> io::Result<Arc<VmLink>> {
        self.vm_inner(false)
    }

    fn vm_inner(&self, recovery: bool) -> io::Result<Arc<VmLink>> {
        let waking = self.waking.lock().unwrap_or_else(|e| e.into_inner());
        let old = self.link.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(vm) = old.as_ref().filter(|vm| !vm.is_lost()) {
            return Ok(vm.clone());
        }
        if let Some(why) = self.boot_refused() {
            return Err(io::Error::other(why));
        }
        if recovery {
            let mut st = self.lock();
            if !st.containers.values().any(recover_after_vm_loss)
                || st.vm_recovery_at.is_some_and(|at| at > std::time::Instant::now()) {
                // A deferred boot is still pending recovery work.
                st.vm_recovery_generation = st.vm_recovery_generation.saturating_add(1);
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "VM recovery is backing off"));
            }
        }
        if let Some(vm) = old && let Err(e) = vm.retire() {
            let mut st = self.lock();
            let delay = restart_backoff(st.vm_crashes.saturating_sub(1));
            st.vm_recovery_at = st.containers.values().any(recover_after_vm_loss).then(|| std::time::Instant::now() + delay);
            drop(st);
            self.recover_vm();
            return Err(e);
        }
        // Retained old links are inert; successful retirement has released
        // their VM and completed cleanup before the replacement opens disk.
        *self.link.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(why) = self.boot_refused() {
            return Err(io::Error::other(why));
        }
        if recovery && !self.lock().containers.values().any(recover_after_vm_loss) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "VM recovery was canceled"));
        }
        let (ceiling, cpus) = (memory_ceiling(), vm_cpus());
        self.ceiling.store(ceiling, std::sync::atomic::Ordering::Relaxed);
        self.cpus.store(cpus, std::sync::atomic::Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        #[cfg(test)]
        self.boot_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let guest = crate::guest::Guest::boot(&crate::guest::BootOptions {
            data_disk: true,
            console_log: crate::diag::console::path().ok(),
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
        vm.listen(super::ports::HOST_BOUND_PORT, Box::new(super::ports::serve_host_bound))?;
        // SSH agent forwarding, for containers that mount /run/host-services/ssh-auth.sock.
        vm.listen(super::ports::SSH_AGENT_PORT, Box::new(super::ports::serve_ssh_agent))?;
        // The engine's API, for containers that mount /var/run/docker.sock
        // (a Linux host's containers mount the daemon's socket itself).
        if cfg!(target_os = "macos") {
            let engine = self.me.get().cloned().unwrap_or_default();
            vm.listen(super::ports::ENGINE_API_PORT, Box::new(move |s| super::ports::serve_engine_api(s, engine.clone())))?;
        }
        // Container lookups the VM's gateway resolver fails, asked of the
        // Mac's own (VM only: a Linux host's agent asks the host's itself).
        if cfg!(target_os = "macos") {
            vm.listen(kiri_proto::DNS_RELAY_PORT, Box::new(super::dns::serve_relay))?;
        }
        // Everything else on the guest's disk belongs to containers that are
        // gone, unless the container list was damaged: then a container
        // missing from it may still be someone's, and so may the layers only
        // it uses (its image may have been removed since).
        let keep = self.guest_keeps();
        match &keep {
            Some(keep) => vm.retain(keep)?,
            None => crate::diag::warn!("kept every container's files and layers: the container list was damaged"),
        }
        // And every unpacked layer no image or container uses. Skipped if
        // the image list can't be read, rather than deleting everything, and
        // while a damaged copy of it is kept: images cut from the list may
        // still be someone's (#440).
        let image_damage = self.store.index_damage();
        match self.store.images() {
            _ if keep.is_none() => {}
            _ if !matches!(image_damage, Ok(None)) => {
                crate::diag::warn!("kept every unpacked layer: the image list was damaged", detail = image_damage_message(&image_damage, self.store.root()).unwrap_or_default());
            }
            Ok(images) => {
                let mut layers: std::collections::BTreeSet<String> = images.iter().flat_map(|i| i.layers.iter().map(|l| l.digest.clone())).collect();
                layers.extend(self.lock().containers.values().flat_map(|c| c.layers.iter().cloned()));
                vm.retain_layers(&layers.into_iter().collect::<Vec<_>>())?;
            }
            Err(e) => crate::diag::warn!("kept every unpacked layer: images unreadable", error = e),
        }
        *self.memory_target.lock().unwrap_or_else(|e| e.into_inner()) = ceiling;
        self.sleeping.store(false, std::sync::atomic::Ordering::SeqCst);
        self.lock().vm_booted_at = Some(std::time::Instant::now());
        *self.link.lock().unwrap_or_else(|e| e.into_inner()) = Some(vm.clone());
        crate::diag::info!("VM booted", ms = t0.elapsed().as_millis(), cpus = cpus, memory_mib = ceiling >> 20);
        drop(waking);
        self.recover_vm();
        Ok(vm)
    }

    /// The VM if it is awake; never boots it.
    fn awake(&self) -> Option<Arc<VmLink>> {
        self.link.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|vm| !vm.is_disconnected()).cloned()
    }

    fn vm_restart_candidates(&self) -> Vec<String> {
        let st = self.lock();
        let mut ids: Vec<String> = st.containers.values().filter(|c| recover_after_vm_loss(c)).map(|c| c.id.clone()).collect();
        ids.sort_by_key(|id| st.containers.get(id).is_some_and(joins_others));
        ids
    }

    fn recover_vm(&self) {
        let Some(me) = self.me.get().and_then(Weak::upgrade) else { return };
        let mut st = self.lock();
        if self.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
            || !st.containers.values().any(recover_after_vm_loss)
        {
            return;
        }
        st.vm_recovery_generation = st.vm_recovery_generation.saturating_add(1);
        if st.vm_recovery_running {
            return;
        }
        st.vm_recovery_running = true;
        drop(st);
        // vm() also serves start requests. Recover on another thread so
        // it cannot recursively start the caller's own container.
        let worker = std::thread::Builder::new().name("kiri-vm-recovery".into()).spawn(move || me.run_vm_recovery());
        if let Err(e) = worker {
            self.lock().vm_recovery_running = false;
            crate::diag::error!("VM recovery worker did not start", error = e);
        }
    }

    fn run_vm_recovery(&self) {
        // VMs that did not boot in a row: each waits twice as long, from
        // 1.6 s to a minute, as retry_held does when the daemon comes up.
        let mut boot_failures = 0u32;
        loop {
            let mut st = self.lock();
            if self.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
                || !st.containers.values().any(recover_after_vm_loss)
            {
                st.vm_recovery_running = false;
                return;
            }
            let delay = st.vm_recovery_at.map_or(Duration::ZERO, |at| at.saturating_duration_since(std::time::Instant::now()));
            if !delay.is_zero() {
                drop(self.changed.wait_timeout(st, delay).unwrap_or_else(|e| e.into_inner()));
                continue;
            }
            let generation = st.vm_recovery_generation;
            drop(st);
            let mut failed = false;
            for id in self.vm_restart_candidates() {
                if self.lock().vm_recovery_generation != generation {
                    break;
                }
                if SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) || self.is_stopped() {
                    break;
                }
                // As start_held: a Stop meanwhile waits for this one start,
                // then stops it with the rest, and holds what is left.
                let started = {
                    let _power = self.power.lock().unwrap_or_else(|e| e.into_inner());
                    self.start_inner(&id, Start::VmRecovery)
                };
                if let Err(e) = started {
                    failed = true;
                    if !matches!(&e, Error::Io(e) if e.kind() == io::ErrorKind::WouldBlock) {
                        crate::diag::warn!("container did not restart after VM stop", id = &id[..12], error = format!("{e:?}"));
                    }
                    if self.awake().is_none() {
                        break;
                    }
                }
            }
            let mut st = self.lock();
            // A new loss or a successful request-driven boot leaves this
            // worker a retry (the generation moved on).
            if failed && st.vm_recovery_generation == generation {
                if self.awake().is_none() {
                    // No VM came up: try again later, not only on the next
                    // request, or `always` containers stay down.
                    boot_failures = boot_failures.saturating_add(1);
                    st.vm_recovery_at = Some(std::time::Instant::now() + restart_backoff(boot_failures.saturating_add(3)));
                    continue;
                }
                st.vm_recovery_running = false;
                return;
            }
            if !failed {
                boot_failures = 0;
            }
        }
    }

    pub fn touch(&self) {
        self.last_activity.store(now_secs(), std::sync::atomic::Ordering::Relaxed);
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
        let code = self.with_exec(&id, |e| e.exit_code.unwrap_or(-1)).unwrap_or(-1);
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
            if vm.is_disconnected() {
                continue;
            }
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
        match K8S_RANGES.iter().try_for_each(|(net, bits)| vm.route(*net, *bits, node, true)) {
            Ok(()) => *routes = Some(key),
            // An older guest can't; shares of services just can't connect.
            Err(e) => {
                if routes.is_none() {
                    crate::diag::warn!("Kubernetes ranges not routed; sharing a service needs a newer guest", error = e);
                }
                *routes = Some((0, node));
            }
        }
    }

    /// Kiri's own: a port on the Mac's loopback that reaches `ip:port` in
    /// the cluster (a service's ClusterIP or a pod), for sharing it.
    pub fn k8s_forward(&self, ip: [u8; 4], port: u16) -> Result<u16, Error> {
        if !in_k8s_ranges(ip) || port == 0 {
            return Err(Error::BadRequest("not an address in the Kubernetes cluster".into()));
        }
        let node = self.lock().containers.values().find(|c| c.status == Status::Running && c.spec.labels.get("dev.kiri.k8s").is_some()).and_then(|c| c.ip);
        let Some(node) = node else { return Err(Error::Conflict("Kubernetes isn't running".into())) };
        self.route_k8s(node);
        let vm = self.vm()?;
        let b = super::ports::Binding { container_port: port, proto: "tcp".into(), host_ip: "127.0.0.1".into(), host_port: 0 };
        let fw = super::ports::start(vm.clone(), &format!("k8s-share-{port}"), &[b], ip)?;
        let mac = fw.bound.first().map(|(_, _, p)| *p).unwrap_or(0);
        crate::diag::info!("Kubernetes address forwarded for sharing", to = format!("{}:{port}", std::net::Ipv4Addr::from(ip)), at = format!("127.0.0.1:{mac}"));
        let mut forwards = self.k8s_fwd.lock().unwrap_or_else(|e| e.into_inner());
        if vm.is_disconnected() {
            drop(forwards);
            return Err(Error::Io(io::Error::other("the VM stopped")));
        }
        forwards.insert(mac, fw);
        Ok(mac)
    }

    /// Closes a forward `k8s_forward` opened.
    pub fn k8s_unforward(&self, mac: u16) {
        self.k8s_fwd.lock().unwrap_or_else(|e| e.into_inner()).remove(&mac);
    }

    fn start_sleeper(self: &Arc<Self>) {
        let idle: u64 = std::env::var("KIRI_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
        if idle == 0 {
            return;
        }
        let builder_idle: u64 = std::env::var("KIRI_BUILDER_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("kiri-sleeper".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            let Some(engine) = me.upgrade() else { return };
            let quiet_for = now_secs().saturating_sub(engine.last_activity.load(std::sync::atomic::Ordering::Relaxed));
            // BuildKit containers (buildx's docker-container driver) run until
            // paused, which keeps the VM's state without using CPU. Between
            // builds they are paused; the next build resumes them.
            let idle_builders: Vec<String> = {
                let st = engine.lock();
                let building = st.execs.values().any(|e| e.running);
                let running: Vec<&Container> = st.containers.values().filter(|c| c.status == Status::Running).collect();
                if !building && quiet_for >= builder_idle && !running.is_empty() && running.iter().all(|c| is_builder(c)) {
                    running.iter().filter(|c| !c.paused).map(|c| c.id.clone()).collect()
                } else {
                    Vec::new()
                }
            };
            for id in &idle_builders {
                if engine.pause_idle_builder(id).is_ok() {
                    crate::diag::info!("paused an idle BuildKit container; the next build resumes it", id = &id[..12]);
                }
            }
            // A paused BuildKit container is no reason to stay awake.
            let (busy, parked) = {
                let st = engine.lock();
                let parked: Vec<String> = st.containers.values().filter(|c| c.status == Status::Running && c.paused && is_builder(c)).map(|c| c.id.clone()).collect();
                let busy = st.containers.values().any(|c| c.status == Status::Running && !(c.paused && is_builder(c))) || st.execs.values().any(|e| e.running) || usb_attached();
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
                    crate::diag::warn!("paused BuildKit container did not stop", id = &id[..12], error = format!("{e:?}"));
                }
            }
            if engine.sleep_if_idle() {
                crate::diag::info!("VM asleep: idle; the next request wakes it", idle_s = idle);
            }
        });
    }

    /// Powers the VM off if nothing runs in it. True if it went to sleep.
    fn sleep_if_idle(&self) -> bool {
        let _waking = self.waking.lock().unwrap_or_else(|e| e.into_inner());
        // Check under the lock: a request may have woken it.
        let idle = !self.lock().containers.values().any(|c| c.status == Status::Running) && !usb_attached();
        // Not `link.take().filter(..)`: that takes the VM first and drops it
        // when busy, which powers it off under whatever just started.
        if !idle {
            return false;
        }
        let Some(vm) = self.link.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return false };
        self.sleeping.store(true, std::sync::atomic::Ordering::SeqCst);
        self.memory_used.store(0, std::sync::atomic::Ordering::Relaxed);
        self.memory_stall.store(0, std::sync::atomic::Ordering::Relaxed);
        vm.shutdown();
        // Let the guest flush its disk and power off before the VM is dropped.
        if let Err(e) = vm.retire() {
            self.sleeping.store(false, std::sync::atomic::Ordering::SeqCst);
            crate::diag::warn!("the VM did not finish stopping", error = e);
            return false;
        }
        *self.link.lock().unwrap_or_else(|e| e.into_inner()) = None;
        true
    }

    /// The window's Stop: Kiri off until Start. Marked stopped first, so
    /// nothing that comes in meanwhile boots it again; then every container
    /// stops and the VM powers off.
    pub fn stop_all(&self) -> Result<(), Error> {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(f) = stopped_marker() {
            let _ = std::fs::write(f, b"");
        }
        crate::diag::info!("Kiri stopped: the VM stays off until Start");
        self.sleep_now()
    }

    /// Start: no longer stopped (the VM boots on the next request, or now
    /// for `/kiri/wake`).
    pub fn unstop(&self) {
        if self.stopped.swap(false, std::sync::atomic::Ordering::SeqCst) {
            crate::diag::info!("Kiri started");
        }
        if let Some(f) = stopped_marker() {
            let _ = std::fs::remove_file(f);
        }
        // What the daemon would have restarted when it came up, had Kiri
        // not been stopped then. Off this thread: the caller boots the VM.
        if let Some(me) = self.me.get().and_then(Weak::upgrade) {
            std::thread::spawn(move || me.start_held());
        }
    }

    /// Starts the held containers (`held_restarts`), one at a time, each
    /// taken from the list as it starts: Start, or `retry_held` once the VM
    /// boots. A Stop meanwhile ends it: the rest stay held for the next
    /// Start, and the one being started is stopped with the others.
    fn start_held(&self) {
        let _power = self.power.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let id = {
                let mut held = self.held_restarts.lock().unwrap_or_else(|e| e.into_inner());
                if held.is_empty() {
                    return;
                }
                held.remove(0)
            };
            if let Err(e) = self.start_inner(&id, Start::Held) {
                // Stopped before it reached the VM: still held, as if not taken.
                let rehold = self.is_stopped() && {
                    let mut st = self.lock();
                    let c = st.containers.get_mut(&id).filter(|c| c.status != Status::Running);
                    c.map(|c| c.restart_pending = true).is_some()
                };
                if rehold {
                    self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()).insert(0, id);
                    return;
                }
                crate::diag::warn!("container did not restart", id = &id[..12], error = format!("{e:?}"));
            }
        }
    }

    /// The VM did not boot when the daemon came up, and containers wait for
    /// it. Tries again after 10 s, then twice as long each time, up to every
    /// 5 minutes: what launchd and systemd did by starting a daemon that
    /// exited, without the exits. Ends when the containers have started, when
    /// Start took them, or when Kiri is stopped (then Start starts them).
    fn retry_held(&self) {
        let Some(me) = self.me.get().and_then(Weak::upgrade) else { return };
        std::thread::spawn(move || {
            let mut delay = Duration::from_secs(10);
            loop {
                std::thread::sleep(delay);
                let none = me.held_restarts.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
                if none || me.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                if me.vm().is_ok() {
                    return me.start_held();
                }
                delay = (delay * 2).min(Duration::from_secs(300));
            }
        });
    }

    /// Kiri is stopped, parking the VM or shutting down: every one of
    /// them is set before the running containers are signalled.
    fn halting(&self) -> bool {
        self.is_stopped() || self.parking.load(std::sync::atomic::Ordering::SeqCst) || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Why the VM may not boot now: Kiri stopped (STOPPED), or the daemon
    /// shutting down (SHUTTING). Only a Stop says to press Start.
    fn boot_refused(&self) -> Option<&'static str> {
        boot_refusal(self.is_stopped(), SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Stop every container, then let the VM sleep (it wakes on the next
    /// request). The window's Stop is stop_all; restarting the VM is this,
    /// then Start. Neither is a stop by the user, as stopping Docker's daemon
    /// isn't: the containers that `always` and `unless-stopped` keep running
    /// are held, and Start (`unstop`) brings them back; a `docker stop` of
    /// one meanwhile cancels that, as it cancels any pending restart.
    pub fn sleep_now(&self) -> Result<(), Error> {
        // After a Start's restarts that are under way: what they started is
        // stopped here too.
        let _power = self.power.lock().unwrap_or_else(|e| e.into_inner());
        let mut held = std::mem::take(&mut *self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()));
        {
            let mut st = self.lock();
            for c in st.containers.values_mut() {
                if (c.status == Status::Running || c.restart_pending) && comes_back(c) {
                    c.restart_pending = true;
                    if !held.contains(&c.id) {
                        held.push(c.id.clone());
                    }
                }
            }
            held.sort_by_key(|id| st.containers.get(id).is_some_and(joins_others));
        }
        *self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()) = held;
        let running = self.each(|c| (c.status == Status::Running).then(|| c.id.clone()));
        // All at once, as for a daemon shutdown, each with its own stop
        // signal and timeout: seven `sleep 1d`s take one timeout, not seven.
        if let Some(vm) = self.awake().filter(|_| !running.is_empty()) {
            self.parking.store(true, std::sync::atomic::Ordering::SeqCst);
            let left = self.stop_together(&vm, &running, Stopping::Engine);
            self.parking.store(false, std::sync::atomic::Ordering::SeqCst);
            if let Some(id) = left.first() {
                return Err(Error::Io(io::Error::other(format!("cannot stop container: {}: it did not exit after SIGKILL", &id[..12]))));
            }
        }
        // Passed-through USB devices go back to the Mac, as containers stop.
        detach_usb();
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

    /// The Mac folders and files that running containers bind-mount, as
    /// the Mac keeps them (the guest sees them under /mnt/mac).
    #[cfg(target_os = "macos")]
    pub fn mac_bind_sources(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .lock()
            .containers
            .values()
            .filter(|c| c.status == Status::Running)
            .flat_map(|c| c.spec.mounts.iter())
            .filter_map(|m| m.split('\t').next()?.strip_prefix("/mnt/mac").filter(|p| p.starts_with('/')).map(str::to_owned))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The VM's link while it runs, without waking it.
    #[cfg(target_os = "macos")]
    pub fn awake_link(&self) -> Option<Arc<VmLink>> {
        self.awake()
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
        let records = read_records(&self.records)?;
        let keep_everything = records.damage.any();
        *self.records_damage.lock().unwrap_or_else(|e| e.into_inner()) = records.damage;
        *self.pending_copy.lock().unwrap_or_else(|e| e.into_inner()) = records.pending_copy;
        *self.on_disk.lock().unwrap_or_else(|e| e.into_inner()) = records.on_disk.filter(|_| keep_everything);
        let saved = records.saved;
        let now = SystemTime::now();
        let mut restart = Vec::new();
        {
            let mut st = self.lock();
            for s in saved {
                let was_running = s.status == Status::Running;
                let mut c = Container {
                    lifecycle: Arc::new(Mutex::new(0)),
                    names_sync: Arc::default(),
                    start_gate: Arc::default(),
                    id: s.id.clone(),
                    name: s.name,
                    layers: s.layers,
                    created: from_ns(s.created_ns),
                    status: if was_running { Status::Exited } else { s.status },
                    started: s.started_ns.map(from_ns),
                    finished: if was_running { Some(now) } else { s.finished_ns.map(from_ns) },
                    exit_code: if was_running { 255 } else { s.exit_code },
                    oom_killed: !was_running && s.oom_killed,
                    vm_stopped: was_running || s.vm_stopped,
                    stopped_by_user: s.stopped_by_user,
                    restart_count: s.restart_count,
                    restart_pending: false,
                    exits: 0,
                    size: None,
                ip: None,
                pid: 0,
                forwards: None,
                live: false,
                early_input: Vec::new(),
                early_close: false,
                start_owed: false,
                paused: false,
                extra_ips: Vec::new(),
                extra_ifs: Vec::new(),
                ip6: None,
                extra_ip6s: Vec::new(),
                    log: Arc::new(Mutex::new(super::logfile::Log::new(super::logfile::dir(self.store.root(), &s.id), &s.spec.log_config))),
                    attached: Vec::new(),
                    spec: s.spec,
                };
                if let Some(mut health) = s.health {
                    // Its monitor stopped with the daemon; Docker reports that as unhealthy.
                    if was_running {
                        health.status = super::health::UNHEALTHY.into();
                    }
                    st.health.insert(c.id.clone(), HealthRun { health, epoch: 0, monitoring: false });
                }
                if comes_back(&c) {
                    // Pending until it starts, also while held: a stop in the meantime cancels it.
                    c.restart_pending = true;
                    restart.push(c.id.clone());
                }
                st.containers.insert(s.id, c);
            }
            // Logs of containers that are gone (`--rm` ones are never saved).
            // Not while the list may be missing some (`keep_everything`).
            if keep_everything {
                crate::diag::warn!("kept every container's logs: the container list was damaged");
            } else if let Ok(dirs) = std::fs::read_dir(self.store.root().join("containers")) {
                for d in dirs.flatten() {
                    if !d.file_name().to_str().is_some_and(|n| st.containers.contains_key(n)) {
                        let _ = std::fs::remove_dir_all(d.path());
                    }
                }
            }
            // Containers that join another's network or link to others start
            // after those, as Docker orders them.
            restart.sort_by_key(|id| st.containers.get(id).is_some_and(joins_others));
            drop(st);
            // Boot now only when containers restart; otherwise the engine
            // starts asleep (Ready · Sleeping) and the first request boots
            // the VM in about 0.1 s. Every boot tells the guest which
            // stopped containers to keep (vm()).
            if self.is_stopped() {
                // Stopped (the window's Stop, kept across restarts): the
                // daemon still comes up, so Start has something to talk to
                // and launchd has nothing to start again and again.
                *self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut restart);
            } else if !restart.is_empty() && self.vm().is_err() {
                // vm() has said why. The daemon stays up, since exiting only
                // had launchd start one that failed the same way every 10 s;
                // the containers wait and the VM is tried again (retry_held).
                crate::diag::warn!("the VM did not boot: the containers that restart wait while it is tried again", containers = restart.len());
                *self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut restart);
                self.retry_held();
            }
        }
        for id in restart {
            if let Err(e) = self.start_inner(&id, Start::DaemonRestart) {
                crate::diag::warn!("container did not restart", id = &id[..12], error = format!("{e:?}"));
            }
        }
        Ok(())
    }

    /// Writes `containers.json`. `--rm` containers are left out.
    fn save(&self) {
        // Taken before the snapshot, so saves land in the order their
        // snapshots were taken: two saves racing could otherwise write an
        // older snapshot last and undo a newer one.
        let _one_writer = self.saving.lock().unwrap_or_else(|e| e.into_inner());
        if !self.keep_damaged_copy() {
            return;
        }
        let watching = self.keeps_everything();
        if watching && !self.keep_foreign_copy() {
            return;
        }
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
        if let Err(e) = kiri_image::jsonfile::replace(&self.records, &json, kiri_image::jsonfile::Durable::Later) {
            crate::diag::error!("saving containers failed", error = e);
            return;
        }
        if watching {
            *self.on_disk.lock().unwrap_or_else(|e| e.into_inner()) = Some(fingerprint(&json));
        }
        // The file is complete and current now; making it durable (an fsync,
        // about 5 ms on a Mac's disk) is left to the syncer, so container
        // starts and stops never wait for the disk.
        let syncer = self.syncer.lock().unwrap_or_else(|e| e.into_inner());
        if syncer.as_ref().is_none_or(|tx| tx.send(()).is_err()) {
            drop(syncer);
            self.sync_records();
        }
    }

    /// Before a save replaces a damaged list no copy of which could be kept
    /// at the start, keeps it now. False while it still cannot: the save
    /// then writes nothing, as replacing the file would lose the list.
    /// Called with the save lock held.
    fn keep_damaged_copy(&self) -> bool {
        let mut pending = self.pending_copy.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bytes) = pending.as_ref() else { return true };
        match kiri_image::jsonfile::keep_copy(&self.records, bytes) {
            Ok(copy) => {
                crate::diag::warn!("kept a copy of the damaged container records", copy = copy.display());
                let mut damage = self.records_damage.lock().unwrap_or_else(|e| e.into_inner());
                damage.unsaved = None;
                damage.copies.push(copy);
                *self.on_disk.lock().unwrap_or_else(|e| e.into_inner()) = Some(fingerprint(bytes));
                *pending = None;
                true
            }
            Err(e) => {
                self.records_damage.lock().unwrap_or_else(|e| e.into_inner()).unsaved = Some(e.to_string());
                crate::diag::error!("container records not saved: the damaged list they would replace could not be kept aside", file = self.records.display(), error = e);
                false
            }
        }
    }

    /// While the list may be damaged, a `containers.json` this engine did
    /// not write (a person put a list back while it ran) is kept as a copy
    /// before the save replaces it, so its content is never lost and the
    /// next start still keeps everything. Called with the save lock held.
    /// False when it could not be kept: the save then writes nothing.
    fn keep_foreign_copy(&self) -> bool {
        let Ok(bytes) = std::fs::read(&self.records) else { return true };
        let on_disk = self.on_disk.lock().unwrap_or_else(|e| e.into_inner());
        if *on_disk == Some(fingerprint(&bytes)) {
            return true;
        }
        match kiri_image::jsonfile::keep_copy(&self.records, &bytes) {
            Ok(copy) => {
                crate::diag::warn!("the container list was changed outside Kiri: kept it as a copy before saving", copy = copy.display());
                self.records_damage.lock().unwrap_or_else(|e| e.into_inner()).copies.push(copy);
                true
            }
            Err(e) => {
                crate::diag::error!("container records not saved: the list was changed outside Kiri and no copy of it could be kept", file = self.records.display(), error = e);
                false
            }
        }
    }

    /// Syncs `containers.json` to disk.
    fn sync_records(&self) {
        if let Err(e) = kiri_image::jsonfile::sync(&self.records) {
            crate::diag::error!("syncing containers to disk failed", error = e);
        }
    }

    /// The thread that syncs `containers.json` after saves. It waits for a
    /// quiet moment, `SYNC_QUIET` with no save, but no longer than
    /// `SYNC_AT_MOST` while saves keep coming: on a Mac's disk a save made
    /// while a sync runs waits for it, so syncing between saves of a burst
    /// would slow every one of them. One sync covers every save made before
    /// it starts. It ends when `shutdown` drops the sender (or the engine
    /// goes), and `shutdown` then syncs one last time itself.
    fn start_syncer(&self) {
        const SYNC_QUIET: Duration = Duration::from_millis(100);
        const SYNC_AT_MOST: Duration = Duration::from_secs(1);
        let (tx, rx) = mpsc::channel::<()>();
        let path = self.records.clone();
        let syncs = self.syncer_syncs.clone();
        let spawned = std::thread::Builder::new().name("kiri-sync".into()).spawn(move || {
            while rx.recv().is_ok() {
                let first = std::time::Instant::now();
                loop {
                    let left = SYNC_AT_MOST.saturating_sub(first.elapsed());
                    if left.is_zero() || !matches!(rx.recv_timeout(SYNC_QUIET.min(left)), Ok(())) {
                        break;
                    }
                }
                if let Err(e) = kiri_image::jsonfile::sync(&path) {
                    crate::diag::error!("syncing containers to disk failed", error = e);
                }
                syncs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        // Without the thread, save() syncs on the caller's thread.
        if spawned.is_ok() {
            *self.syncer.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        }
    }

    fn on_event(&self, ev: Event<'_>) {
        match ev {
            Event::Output { id, stream, data } => {
                let mut st = self.lock();
                if let Some(c) = st.containers.get_mut(id) {
                    c.attached.retain(|tx| tx.send((stream, data.to_vec())).is_ok());
                    // Written outside the engine's lock; holding the log's
                    // keeps the order `docker logs -f` sees.
                    let log = c.log.clone();
                    let mut log = lock_log(&log);
                    drop(st);
                    log.append(stream, ns(SystemTime::now()), data);
                } else if let Some(e) = st.execs.get_mut(id) {
                    e.attached.retain(|tx| tx.send((stream, data.to_vec())).is_ok());
                }
            }
            Event::Exited { id, code, oom } => {
                let mut st = self.lock();
                if let Some(e) = st.execs.get_mut(id) {
                    e.running = false;
                    e.exit_code = Some(code);
                    e.pid = 0;
                    e.attached.clear();
                    let (container, exec_id, internal) = (e.container_id.clone(), e.id.clone(), e.config.internal);
                    drop(st);
                    self.changed.notify_all();
                    if !internal {
                        self.emit_container("exec_die", &container, &[("execID", exec_id), ("exitCode", code.to_string())]);
                    }
                    return;
                }
                let mut restart_in = None;
                let mut persist = false;
                let mut closed_ports = None;
                let actor = st.containers.get(id).map(|c| (c.spec.image.clone(), container_attrs(c)));
                // Exited before its start made it live: its `start` first.
                let start_owed = st.containers.get_mut(id).is_some_and(|c| std::mem::take(&mut c.start_owed));
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
                            crate::diag::warn!("container killed for memory", name = c.name.clone(), exit_code = code);
                        }
                        c.exits += 1;
                        c.live = false;
                        if let Some(ip) = c.ip.take() {
                            self.release_ip(&c.spec.network_mode, ip);
                        }
                        for (net, ip) in std::mem::take(&mut c.extra_ips) {
                            self.release_ip(&net, ip);
                        }
                        c.extra_ifs.clear();
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
                            "on-failure" => code != 0 && !c.stopped_by_user && (p.max_retries == 0 || c.restart_count < p.max_retries),
                            _ => false,
                        };
                        // Not for the engine's own stop (Stop, the VM's sleep on
                        // request): Start brings those back.
                        let engine_stop = SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) || self.parking.load(std::sync::atomic::Ordering::SeqCst) || self.is_stopped();
                        if wants && !c.spec.auto_remove && !engine_stop {
                            let backoff = restart_backoff(c.restart_count);
                            c.restart_count = c.restart_count.saturating_add(1);
                            c.restart_pending = true;
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
                    if start_owed {
                        self.emit_container_with("start", id, &image, attrs.clone(), &[]);
                    }
                    self.emit_container_with("die", id, &image, attrs.clone(), &[("exitCode", code.to_string())]);
                    if remove {
                        self.emit_container_with("destroy", id, &image, attrs, &[]);
                    }
                }
                // Closing listeners joins their threads: never under the lock.
                drop(closed_ports);
                if let Some(log) = removed_log {
                    lock_log(&log).remove();
                }
                // `--rm` takes the container's anonymous volumes with it: gone
                // from the list now, their data off this thread (the VM's
                // reader), since that asks the VM, and vm() waits for this
                // reader when the VM was just lost.
                let forgotten: Vec<String> = self.unshared(anon).into_iter().filter(|v| self.forget_volume(v)).collect();
                if !forgotten.is_empty() && let Some(me) = self.me.get().and_then(Weak::upgrade) {
                    let _ = std::thread::Builder::new().name("kiri-volume-rm".into()).spawn(move || {
                        if let Ok(vm) = me.vm() {
                            for v in forgotten {
                                let _ = vm.remove_volume(&v);
                            }
                        }
                    });
                }
                self.changed.notify_all();
                if persist {
                    self.save();
                }
                if let (Some(delay), Some(me)) = (restart_in, self.me.get().and_then(Weak::upgrade)) {
                    let id = id.to_owned();
                    let _ = std::thread::Builder::new().name("kiri-restart".into()).spawn(move || {
                        std::thread::sleep(delay);
                        // Only if nobody stopped or removed it in the meantime.
                        // start_inner checks again, under its lock, that the
                        // restart is still pending: a stop between here and
                        // there cancels it.
                        let still_wanted = !me.is_stopped() && me.lock().containers.get(&id).is_some_and(|c| c.status == Status::Exited && !c.stopped_by_user);
                        if still_wanted {
                            match me.start_inner(&id, Start::Restart) {
                                // Kiri is stopping: still pending, Start brings it back.
                                Err(Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                                Err(e) => crate::diag::warn!("container did not restart", id = &id[..12], error = format!("{e:?}")),
                                Ok(()) => {}
                            }
                        }
                    });
                }
            }
            Event::Pressure => self.grow_memory(),
            Event::Lost => {
                let end = vm_end(self.sleeping.load(std::sync::atomic::Ordering::SeqCst), SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst));
                let crashed = end == VmEnd::Crashed;
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
                    c.extra_ifs.clear();
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
                        // A crash, not a stop: what the restart policy keeps
                        // running comes back once a new VM is up. A `docker
                        // stop` meanwhile takes this away (mark_stopped).
                        if crashed && comes_back(c) {
                            c.restart_pending = true;
                        }
                    }
                    if crashed && recover_after_vm_loss(c) {
                        c.restart_count = c.restart_count.saturating_add(1);
                    }
                }
                if crashed {
                    if st.vm_booted_at.is_some_and(|at| at.elapsed() >= VM_STABLE) {
                        st.vm_crashes = 0;
                    }
                    st.vm_crashes = st.vm_crashes.saturating_add(1);
                }
                st.vm_booted_at = None;
                let delay = restart_backoff(st.vm_crashes.saturating_sub(1));
                st.vm_recovery_at = st.containers.values().any(recover_after_vm_loss).then(|| std::time::Instant::now() + delay);
                for e in st.execs.values_mut() {
                    e.end_with_vm();
                }
                for h in st.health.values_mut() {
                    close_monitor(h);
                }
                drop(st);
                drop(closed);
                self.k8s_lb.lock().unwrap_or_else(|e| e.into_inner()).clear();
                self.k8s_fwd.lock().unwrap_or_else(|e| e.into_inner()).clear();
                self.k8s_routes.lock().unwrap_or_else(|e| e.into_inner()).take();
                self.changed.notify_all();
                match end {
                    // sleep_if_idle says why it slept.
                    VmEnd::Slept => {}
                    // The daemon's own shutdown (`systemctl stop kiri`, quitting Kiri): asked for.
                    VmEnd::ShutDown => {
                        self.save();
                        crate::diag::info!("the VM stopped");
                    }
                    VmEnd::Crashed => {
                        self.save();
                        crate::diag::error!("the VM stopped unexpectedly", console = crate::diag::console::path().map(|p| p.display().to_string()).unwrap_or_default());
                        // vm() releases this dead link after the reader has
                        // finished exit accounting. Never wait for it here.
                        self.recover_vm();
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
        let _ = std::thread::Builder::new().name("kiri-balloon".into()).spawn(move || {
            let mut balloon_state = BalloonState::default();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Some(engine) = me.upgrade() else { return };
                let Some(vm) = engine.awake() else {
                    balloon_state.reset();
                    continue;
                };
                let Ok(stats) = vm.stats() else { continue };
                let mut target = engine.memory_target.lock().unwrap_or_else(|e| e.into_inner());
                let ceiling = engine.ceiling();
                let used = estimate_used(&stats, ceiling, *target, if cfg!(target_os = "macos") { 0 } else { self_rss() });
                engine.memory_used.store(used, std::sync::atomic::Ordering::Relaxed);
                engine.memory_stall.store(stats.mem_stall, std::sync::atomic::Ordering::Relaxed);
                engine.oom_kills.store(stats.oom_kills, std::sync::atomic::Ordering::Relaxed);
                if !cfg!(target_os = "macos") {
                    continue;
                }
                if let Some(next) = balloon_state.next_target(&stats, used, ceiling, *target) {
                    *target = next;
                    vm.set_memory_target(next);
                }
            }
        });
    }

    fn grow_memory(&self) {
        let mut target = self.memory_target.lock().unwrap_or_else(|e| e.into_inner());
        let next = target.saturating_add(MEMORY_GROW).min(self.ceiling());
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
            (self.ceiling(), *self.memory_target.lock().unwrap_or_else(|e| e.into_inner()))
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
        (stall, self.oom_kills.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// How busy the VM is, in percent of all its CPUs; 0 while asleep.
    pub fn cpu_percent(&self) -> f64 {
        f64::from(self.cpu_permyriad.load(std::sync::atomic::Ordering::Relaxed)) / 100.0
    }

    /// Keeps the VM's clock on the Mac's. The guest is told the time at
    /// boot, but its clock stops while the Mac sleeps, so after each sleep it
    /// fell further behind: an hour behind after a few nights, and apt
    /// refused Debian's signatures as made in the future. Sent again within
    /// a few seconds of a wake (the wall clock moved on further than the
    /// monotonic one, which stops in sleep too), and once a minute anyway;
    /// the agent only steps its clock when it is off.
    fn start_clock(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("kiri-clock".into()).spawn(move || {
            let (mut wall, mut mono) = (SystemTime::now(), std::time::Instant::now());
            let mut sent = std::time::Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let Some(engine) = me.upgrade() else { return };
                let (w, m) = (SystemTime::now(), std::time::Instant::now());
                let slept = w.duration_since(wall).unwrap_or_default().saturating_sub(m.duration_since(mono)) > Duration::from_secs(2);
                (wall, mono) = (w, m);
                if !slept && sent.elapsed() < Duration::from_secs(60) {
                    continue;
                }
                if let Some(vm) = engine.awake()
                    && vm.set_time().is_ok()
                {
                    sent = m;
                }
            }
        });
    }

    /// Reads the VM's CPU time every two seconds, for `cpu_percent`.
    fn start_cpu_meter(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("kiri-cpu".into()).spawn(move || {
            let mut last: Option<(u64, std::time::Instant)> = None;
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let Some(engine) = me.upgrade() else { return };
                let now = engine.awake().and_then(|_| super::vmcpu::cpu_ns()).map(|ns| (ns, std::time::Instant::now()));
                let permyriad = match (last, now) {
                    (Some((a, t0)), Some((b, t1))) if b >= a => {
                        let wall = t1.duration_since(t0).as_nanos() as f64 * engine.cpus().max(1) as f64;
                        ((b - a) as f64 / wall * 10000.0).round().clamp(0.0, 10000.0) as u32
                    }
                    _ => 0,
                };
                engine.cpu_permyriad.store(permyriad, std::sync::atomic::Ordering::Relaxed);
                last = now;
            }
        });
    }

    /// Finds an image by the name a client used.
    pub fn image(&self, name: &str) -> io::Result<Option<Image>> {
        Ok(image_named(self.store.images()?, name))
    }

    /// The config Labels of each of `images`, which must be the whole image
    /// list (`store.images()`): a config is read from disk only the first
    /// time it is seen, and entries for images not in the list are dropped.
    /// An image without labels has `{}`, as Docker's image list and
    /// `system df` rows show it (image inspect shows `null`).
    pub fn image_labels(&self, images: &[Image]) -> Vec<serde_json::Value> {
        let mut cache = self.image_labels.lock().unwrap_or_else(|e| e.into_inner());
        // Rebuilt from the list each time, which drops removed images.
        let mut old = std::mem::take(&mut *cache);
        images
            .iter()
            .map(|i| {
                let digest = &i.config_digest;
                if let Some(labels) = cache.get(digest) {
                    return labels.clone();
                }
                let labels = match old.remove(digest) {
                    Some(labels) => labels,
                    None => {
                        // Not kept when the config could not be read, so a
                        // passing read error does not stick.
                        let Some(cfg) = self.store.read_config(digest).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else {
                            return serde_json::json!({});
                        };
                        match super::api::image_labels_from(&cfg) {
                            serde_json::Value::Null => serde_json::json!({}),
                            labels => labels,
                        }
                    }
                };
                cache.insert(digest.clone(), labels.clone());
                labels
            })
            .collect()
    }

    /// The IDs of the images that `docker ps --filter ancestor=` values name.
    pub fn ancestor_ids(&self, values: &[String]) -> io::Result<Vec<String>> {
        Ok(ancestor_ids(&self.store.images()?, values))
    }

    /// Sends the guest every layer of these (digest, media type) it doesn't
    /// have unpacked, from the blob store.
    fn ensure_layers<'a>(&self, vm: &VmLink, layers: impl Iterator<Item = (&'a str, &'a str)>) -> io::Result<()> {
        let _one_at_a_time = self.layers.lock().unwrap_or_else(|e| e.into_inner());
        for (digest, media_type) in layers {
            if !vm.has_layer(digest)? {
                vm.send_layer(digest, media_type, &self.store.blob_path(digest)?)?;
            }
        }
        Ok(())
    }

    /// A container's layers back in the guest before it runs again: the
    /// guest deletes an unpacked layer at boot when it isn't whole (an unpack
    /// cut short, or one from before layers were marked whole), and a
    /// container created before that would otherwise fail to start with
    /// "layer ... is not unpacked". The media type comes from any image with
    /// the layer; a layer no image has any more can't be sent again.
    fn restore_layers(&self, vm: &VmLink, layers: &[String]) -> Result<(), Error> {
        let mut missing = Vec::new();
        for digest in layers {
            if !vm.has_layer(digest)? {
                missing.push(digest);
            }
        }
        // The usual case: nothing to send, and no need to read the image index.
        if missing.is_empty() {
            return Ok(());
        }
        let images = self.store.images()?;
        let mut wanted = Vec::with_capacity(missing.len());
        for digest in missing {
            let Some(l) = images.iter().flat_map(|i| &i.layers).find(|l| &l.digest == digest) else {
                return Err(Error::Conflict(format!("layer {digest} is gone from the engine and no image has it any more: remove this container and create it again")));
            };
            wanted.push((l.digest.as_str(), l.media_type.as_str()));
        }
        Ok(self.ensure_layers(vm, wanted.into_iter())?)
    }

    /// Records a container and makes sure its layers are in the guest, so
    /// `start` has nothing left to do but run it.
    pub fn create(&self, name: Option<String>, spec: Spec, image: &Image) -> Result<String, Error> {
        if !image.layers.is_empty() {
            let vm = self.vm()?;
            self.ensure_layers(&vm, image.layers.iter().map(|l| (l.digest.as_str(), l.media_type.as_str())))?;
        }
        let id = random_hex(32);
        let persist = !spec.auto_remove;
        let (log_dir, log_config) = (super::logfile::dir(self.store.root(), &id), spec.log_config.clone());
        let mut st = self.lock();
        let name = match name.filter(|n| !n.is_empty()) {
            Some(n) => {
                let n = n.trim_start_matches('/').to_owned();
                if !valid_container_name(&n) {
                    return Err(Error::BadRequest(format!("Invalid container name ({n}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed")));
                }
                if let Some(c) = st.containers.values().find(|c| c.name == n) {
                    return Err(name_in_use(&n, &c.id));
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
                lifecycle: Arc::new(Mutex::new(0)),
                names_sync: Arc::default(),
                start_gate: Arc::default(),
                id: id.clone(),
                name,
                layers: image.layers.iter().rev().map(|l| l.digest.clone()).collect(),
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
                restart_pending: false,
                exits: 0,
                size: None,
                ip: None,
                pid: 0,
                forwards: None,
                live: false,
                early_input: Vec::new(),
                early_close: false,
                start_owed: false,
                paused: false,
                extra_ips: Vec::new(),
                extra_ifs: Vec::new(),
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

    /// The lock that orders a container's DNS name changes (see `names_sync`).
    fn names_sync(&self, id: &str) -> Option<Arc<Mutex<()>>> {
        self.lock().containers.get(id).map(|c| c.names_sync.clone())
    }

    fn lifecycle(&self, key: &str) -> Result<Arc<Mutex<u64>>, Error> {
        let st = self.lock();
        let id = resolve(&st, key)?;
        Ok(st.containers[&id].lifecycle.clone())
    }

    /// `docker start`: also clears a previous `stop`, as Docker does.
    pub fn start(&self, key: &str) -> Result<(), Error> {
        self.start_inner(key, Start::User)
    }

    fn start_inner(&self, key: &str, reason: Start) -> Result<(), Error> {
        let lifecycle = self.lifecycle(key)?;
        let _starting = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let generation = *_starting;
        {
            let st = self.lock();
            let id = resolve(&st, key)?;
            let c = &st.containers[&id];
            // A start by the restart policy only while it is still pending:
            // nothing to boot a VM for otherwise.
            if c.status == Status::Running || (reason == Start::VmRecovery && !recover_after_vm_loss(c))
                || (reason == Start::Restart && c.stopped_by_user)
                || (reason != Start::User && !c.restart_pending)
            {
                return Ok(());
            }
            if c.argv().is_empty() {
                return Err(Error::BadRequest("no command specified".into()));
            }
            // Checked again in start_on_vm, under the lock Stop's holds take.
            if reason != Start::User && (self.is_stopped() || self.parking.load(std::sync::atomic::Ordering::SeqCst)) {
                return Err(Error::Io(io::Error::new(io::ErrorKind::WouldBlock, STOPPED)));
            }
        }
        drop(_starting);
        // Retirement and boot own no container lock. A manual action can
        // invalidate this attempt while the previous VM is stopping.
        let prepared = self.vm_inner(reason == Start::VmRecovery);
        let starting = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if *starting != generation {
            // Removed meanwhile: "No such container", as on Docker.
            self.resolve(key)?;
            return Err(Error::Conflict("container start was canceled".into()));
        }
        let vm = match prepared {
            Ok(vm) => vm,
            Err(e) => {
                if reason != Start::VmRecovery {
                    let (id, spec) = self.with(key, |c| (c.id.clone(), c.spec.clone()))?;
                    self.fail_start(&id, &spec, &e);
                }
                return Err(Error::Io(e));
            }
        };
        self.start_on_vm(key, reason, vm, starting)
    }

    /// `starting` is the container's lifecycle lock, held by start_inner:
    /// this attempt keeps it until the guest has the process, so a stop,
    /// kill or remove acts on it only then, and lets go before handing
    /// over input sent ahead of the start (see below).
    fn start_on_vm(&self, key: &str, reason: Start, vm: Arc<VmLink>, starting: MutexGuard<'_, u64>) -> Result<(), Error> {
        let mut starting = Some(starting);
        let (id, layers, argv, mut spec, size, gate) = {
            let mut st = self.lock();
            let id = resolve(&st, key)?;
            // Under the same lock as Lost: either this claim belongs to
            // the live VM, or Lost will observe it as Running and end it.
            if vm.is_disconnected() {
                let spec = st.containers[&id].spec.clone();
                drop(st);
                let _ = vm.is_lost();
                if reason != Start::VmRecovery {
                    self.fail_start(&id, &spec, &"the VM stopped");
                }
                return Err(Error::Io(io::Error::other("the VM stopped")));
            }
            // Kiri stopped or stopping (Stop, the VM's sleep on request): the
            // restart policy waits for Start, still pending, so Stop holds it.
            if reason != Start::User && (self.is_stopped() || self.parking.load(std::sync::atomic::Ordering::SeqCst)) {
                return Err(Error::Io(io::Error::new(io::ErrorKind::WouldBlock, STOPPED)));
            }
            let c = st.containers.get_mut(&id).expect("resolved");
            if (reason == Start::VmRecovery && !recover_after_vm_loss(c)) || (reason == Start::Restart && c.stopped_by_user) {
                return Ok(());
            }
            match c.status {
                Status::Running => return Ok(()),
                Status::Exited if c.spec.auto_remove => return Err(Error::Conflict("container is being removed".into())),
                _ => {}
            }
            // A start by the restart policy: only while it is still pending.
            // A `stop` takes that away (mark_stopped) and answered as if it
            // ended the container, so it must not start after all.
            if reason != Start::User && !std::mem::take(&mut c.restart_pending) {
                return Ok(());
            }
            let argv = c.argv();
            if argv.is_empty() {
                return Err(Error::BadRequest("no command specified".into()));
            }
            c.status = Status::Running;
            c.restart_pending = false;
            c.started = Some(SystemTime::now());
            c.finished = None;
            c.oom_killed = false;
            c.vm_stopped = false;
            // `docker run --rm` without -d: its client is reading the output
            // and the log goes with the container, so it stays in memory.
            if c.spec.auto_remove && !c.attached.is_empty() {
                lock_log(&c.log).keep_in_memory();
            }
            if reason == Start::User {
                c.stopped_by_user = false;
                c.restart_count = 0;
            }
            // Shut under the same lock that marks it Running: an exec or
            // copy that sees Running also finds the gate shut.
            let gate = c.begin_start();
            // This run's exit, once reported, counts one more.
            let exits = c.exits;
            c.start_owed = true;
            let out = (id.clone(), c.layers.clone(), argv, c.spec.clone(), c.size.or(c.spec.console_size), (gate, exits));
            // A volume is filled from the image by the first start that
            // finds it empty, while that start is under way. Collected
            // here, under the lock that shut the gate just above, and only
            // of gates shut already by other containers: every wait below
            // is for a start that began earlier than this one, and each of
            // those gates belongs to that one start alone (see `StartGate`),
            // so no two starts can wait for each other. (A container that
            // shares nothing with an earlier start has no wait at all.)
            let earlier = shared_volume_gates(&st, &id, &out.3);
            let halted = self.halting();
            (out.0, out.1, out.2, out.3, out.4, (out.5.0, out.5.1, earlier, halted))
        };
        let (gate, exits, earlier, halted_at_claim) = gate;
        // A bind source that is a link is followed again at every start, as
        // Docker does: re-pointing it between runs (a `current` release
        // link) mounts the new target.
        spec.mounts = super::api::fresh_bind_sources(&spec.mounts, &spec.mount_info);
        let mut hostname = if spec.hostname.is_empty() { id[..12].to_owned() } else { spec.hostname.clone() };
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
                    self.fail_start(&id, &spec, &e);
                    return Err(e);
                }
            },
            None => None,
        };
        // `--pid container:<id>`: that container's PID namespace; it has to
        // be running, as with Docker.
        let pid_joined = match spec.pid_mode.strip_prefix("container:").map(|t| self.pid_container(&id, t)) {
            Some(Ok(target)) => Some(target),
            Some(Err(e)) => {
                self.fail_start(&id, &spec, &e);
                return Err(e);
            }
            None => None,
        };
        let mut flags = 0;
        if spec.pid_mode == "host" {
            flags |= run_flags::PID_HOST;
        }
        // The network's subnet, checked against the host's once more before
        // its first container (#489).
        let net = {
            let mut nets = self.lock_networks();
            match nets.settle(&spec.network_mode) {
                Ok(()) => nets.find(&spec.network_mode).cloned(),
                Err(e) => {
                    drop(nets);
                    let e = Error::Conflict(e);
                    self.fail_start(&id, &spec, &e);
                    return Err(e);
                }
            }
        };
        let links = match net.as_ref().map(|n| self.link_hosts(&id, &spec, n)).transpose() {
            Ok(l) => l.unwrap_or_default(),
            Err(e) => {
                self.fail_start(&id, &spec, &e);
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
                    Some(want) => self.lock_networks().reserve(&n.name, want).map(|()| want).map_err(Error::Conflict),
                    None => {
                        let avoid = self.wanted_addresses(&n.name);
                        self.lock_networks().allocate_avoiding(&n.name, &avoid).ok_or_else(|| Error::Conflict(format!("no free address left on network {}", n.name)))
                    }
                };
                let ip = match ip {
                    Ok(ip) => ip,
                    Err(e) => {
                        self.fail_start(&id, &spec, &e);
                        return Err(e);
                    }
                };
                let ip6 = if spec.ipv6_disabled("eth0") { Ok(None) } else { self.allocate_ip6(n, &spec.ipv6_address) };
                match ip6 {
                    Ok(ip6) => {
                        if let Some(c) = self.lock().containers.get_mut(&id) {
                            c.ip6 = ip6;
                        }
                        if let (Some(a), Some(v)) = (ip6, &n.v6) {
                            net6_opts.push(format!("{}={}/{}", kiri_proto::net_opts::IP6, super::network::ip6_str(a), v.prefix));
                            net6_opts.push(format!("{}={}", kiri_proto::net_opts::GW6, super::network::ip6_str(v.gateway)));
                        }
                        if n.internal {
                            net6_opts.push(format!("{}=1", kiri_proto::net_opts::INTERNAL));
                        }
                        if n.no_ipv4 {
                            net6_opts.push(format!("{}=1", kiri_proto::net_opts::NO_IPV4));
                        }
                        net6_opts.push(format!("{}={}", kiri_proto::net_opts::NETWORK, n.name));
                    }
                    Err(e) => {
                        self.lock_networks().release(&n.name, ip);
                        self.fail_start(&id, &spec, &e);
                        return Err(e);
                    }
                }
                // User-defined networks resolve container names, as Docker's do;
                // the default bridge does not.
                if !n.builtin() {
                    flags |= run_flags::DNS_GATEWAY;
                    let name = self.lock().containers.get(&id).map(|c| c.name.clone()).unwrap_or_default();
                    dns_names.push(name);
                    dns_names.extend(spec.aliases.iter().cloned());
                    dns_names.push(hostname.clone());
                } else {
                    // On the default bridge and connected to a user network
                    // before it started (k3d's load balancer): its other
                    // networks are plugged in only once it runs, so the
                    // embedded DNS is its resolver from the start, and answers
                    // their names from the start, as on Docker, not after the
                    // program has looked them up (#449). Its own name is not
                    // registered on the bridge.
                    let user = self.user_network_bridges(&spec);
                    if !user.is_empty() {
                        flags |= run_flags::DNS_GATEWAY;
                        net6_opts.extend(user.iter().map(|b| format!("{}={b}", kiri_proto::net_opts::DNS_BRIDGE)));
                    }
                }
                (n.bridge.clone(), ip, n.prefix, n.gateway)
            }
            Some(_) => {
                flags |= run_flags::NET_NONE;
                (String::new(), [0; 4], 0, [0; 4])
            }
            None if joined.is_some() => (String::new(), [0; 4], 0, [0; 4]),
            None => {
                let e = Error::NotFound(format!("network {} not found", spec.network_mode));
                self.fail_start(&id, &spec, &e);
                return Err(e);
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
        // --add-host and links come first: /etc/hosts takes the first match,
        // so `--add-host host.docker.internal:IP` overrides Kiri's, as on Docker.
        let mut hosts = spec.extra_hosts.clone();
        hosts.extend(links);
        hosts.push(format!("{}\thost.docker.internal host.kiri.internal", super::ports::HOST_ADDR));
        hosts.push(format!("{}\tgateway.docker.internal", super::ports::HOST_ADDR));
        let mut opts = net_opts(&spec, joined.as_deref(), &domainname);
        opts.extend(net6_opts);
        opts.extend(pid_joined.map(|t| format!("{}={t}", kiri_proto::net_opts::PIDNS)));
        let net_args = kiri_proto::Net { bridge: &bridge, ip, prefix, gateway, extra_hosts: &hosts, dns_names: &dns_names, opts: &opts };
        // Joining a host-network container is the host's namespace too: its
        // net.* sysctls would change the whole VM (or Linux host).
        let joined_host = joined_mode.is_some_and(|m| self.lock_networks().find(&m).is_some_and(|n| n.driver == "host"));
        let host_opts = match spec.guest_opts(flags & run_flags::NET_HOST != 0 || joined_host) {
            Ok(o) => o,
            Err(e) => {
                self.fail_start(&id, &spec, &e);
                return Err(e);
            }
        };
        if let Err(e) = self.restore_layers(&vm, &layers) {
            self.fail_start(&id, &spec, &e);
            return Err(e);
        }
        let seccomp = match spec.seccomp_program() {
            Ok(p) => p,
            Err(e) => {
                self.fail_start(&id, &spec, &e);
                return Err(e);
            }
        };
        // Waits for the fill of a volume this container shares with a start
        // still under way, as it did when the guest's one thread did the
        // fill: else it would run on a half-filled volume. No lock is held.
        // If that start failed, its gate is open too and this one finds the
        // volume still empty and fills it; if that container was removed,
        // its gate (an Arc kept here) was opened by its start's guard all
        // the same.
        for g in &earlier {
            g.wait();
        }
        // The guest knows nothing of this container while it waits, so a
        // stop signal sent meanwhile (Kiri's Stop, the VM's sleep, the
        // daemon's shutdown: each sets its flag before it signals) was
        // dropped there. Looked at again under the lock Stop reads the
        // containers with: running it now would leave it running after Stop.
        // It ends as a start that failed, which Stop sees as no longer Running.
        if !earlier.is_empty()
            && let Some(e) = self.refuse_waited_start(&id, &spec, reason, halted_at_claim, || vm.is_disconnected())
        {
            return Err(e);
        }
        let cancelled = || gate.0.cancel.load(std::sync::atomic::Ordering::SeqCst);
        let started = if cancelled() {
            Err(io::Error::other("cancelled before it reached the guest"))
        } else {
            vm.run(&id, &layers, &argv, &spec.env, &spec.cwd, &spec.user, &hostname, flags, net_args, &spec.mounts, &spec.limits.wire(), spec.caps, &host_opts, &seccomp, &gate.0.cancel)
        };
        if let Ok(pid) = started
            && let Some(c) = self.lock().containers.get_mut(&id).filter(|c| c.status == Status::Running)
        {
            c.pid = pid;
        }
        // A failed start is cleaned up before the gate opens, as on every
        // earlier failure above: an exec or a copy waiting on it then finds
        // the container as the failure left it, not half-way there.
        if let Err(e) = started {
            crate::diag::warn!("container did not start", id = &id[..12], image = spec.image, error = e);
            match gate.0.cancelled_by() {
                // Docker refuses a start that a removal got to first before
                // the start touches the container: its exit code stays 0,
                // which is what a waiter on the removed container gets.
                Some(Cancel::Removed) => self.fail_start_code(&id, &spec, 0),
                // A stuck start that a stop or kill ended: its first process
                // was killed, as the stop or kill would have killed it.
                Some(Cancel::Stopped) => self.fail_start_code(&id, &spec, 128 + libc::SIGKILL),
                None => self.fail_start(&id, &spec, &e),
            }
            return Err(cancelled_start_error(gate.0.cancelled_by(), &id).unwrap_or(Error::Io(e)));
        }
        // Before anything below, which may itself exec (a health probe),
        // and after the pid is set.
        drop(gate);
        if let (true, Some((rows, cols))) = (spec.tty, size) {
            let _ = vm.resize(&id, rows, cols);
        }
        if flags & (run_flags::NET_HOST | run_flags::NET_NONE) == 0 && joined.is_none() {
            for (i, extra) in spec.extra_networks.iter().enumerate() {
                if let Err(e) = self.attach_extra(&vm, &id, i as u32 + 1, extra) {
                    crate::diag::warn!("container not attached to a network", id = &id[..12], network = extra.name, error = format!("{e:?}"));
                }
            }
        }
        // A container that left all its networks keeps its `-p` bindings
        // but has no address for them, as on Docker, where it publishes none.
        if !spec.ports.is_empty() && flags & run_flags::NET_NONE == 0 {
            let target = if flags & run_flags::NET_HOST != 0 { [127, 0, 0, 1] } else { ip };
            match super::ports::start(vm.clone(), &id, &spec.ports, target) {
                Ok(fw) => {
                    let mut st = self.lock();
                    if let Some(c) = st.containers.get_mut(&id).filter(|c| c.status == Status::Running && !vm.is_disconnected()) {
                        c.forwards = Some(fw);
                    } else {
                        drop(st);
                        drop(fw);
                    }
                }
                Err(e) => {
                    crate::diag::warn!("ports not published; container stopped", id = &id[..12], error = e);
                    // Docker refuses to run a container whose ports clash:
                    // no `start` for it.
                    if let Some(c) = self.lock().containers.get_mut(&id) {
                        c.start_owed = false;
                    }
                    let _ = vm.kill(&id, libc::SIGKILL as u32);
                    return Err(Error::Conflict(e.to_string()));
                }
            }
        }
        if vm.is_disconnected() {
            self.fail_start(&id, &spec, &"the VM stopped");
            return Err(Error::Io(io::Error::other("the VM stopped")));
        }
        // The guest has the process now: deliver input that arrived early.
        // It is drained before the container goes live, so input still
        // arriving meanwhile (and its CloseStdin) queues behind it rather
        // than overtaking it.
        loop {
            let (early, close) = match self.take_early(&id, exits) {
                Early::Input(early) => (early, false),
                Early::Live { close } => (Vec::new(), close),
                // Exited already (its exit overtook the end of a start that
                // waited for its root): not live, and no `start` after its
                // `die`.
                Early::Gone => return Ok(()),
            };
            if early.is_empty() {
                if close {
                    let _ = vm.close_stdin(&id);
                }
                break;
            }
            // The guest takes no more of this than the program reads, which
            // may be never: a stop or kill must not wait behind it for the
            // container's lock. The process is there to signal now, and its
            // exit ends this hand-over (its window closes) and the loop.
            drop(starting.take());
            let _ = vm.input(&id, &early);
        }
        self.health_start(&id);
        if !spec.auto_remove {
            self.save();
        }
        Ok(())
    }

    /// The next step in making a started container live (see
    /// `start_on_vm`), under the lock: input that arrived early to deliver
    /// first, or live now (and whether its stdin closed early), its `start`
    /// sent, or nothing, when this run (`exits`: its count of exits when it
    /// started) has already exited. Its exit is reported apart from the
    /// start, and with a start that waits for a new volume to fill it can
    /// come first: the container is then not made live again, which would
    /// send the next run's early input straight to a guest that has no
    /// process for it; what was early for this run goes with it, and its
    /// exit sent the `start` before its `die`.
    fn take_early(&self, id: &str, exits: u64) -> Early {
        let mut st = self.lock();
        let Some(c) = st.containers.get_mut(id) else { return Early::Gone };
        if c.status != Status::Running || c.exits != exits {
            c.early_input.clear();
            c.early_close = false;
            return Early::Gone;
        }
        let early = std::mem::take(&mut c.early_input);
        if !early.is_empty() {
            return Early::Input(early);
        }
        c.live = true;
        if std::mem::take(&mut c.start_owed) {
            // Under the lock, so its exit, reported under it too, sends its
            // `die` only after this.
            let (image, attrs) = (c.spec.image.clone(), container_attrs(c));
            self.emit_container_with("start", id, &image, attrs, &[]);
        }
        Early::Live { close: std::mem::take(&mut c.early_close) }
    }

    /// Sends an event to every listener and keeps it for `--since`.
    pub fn emit(&self, typ: &str, action: &str, actor: &str, attributes: serde_json::Map<String, serde_json::Value>, legacy: Option<(&str, &str)>) {
        log_event(typ, action, actor, &attributes);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
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
        e.listeners.retain(|tx| !matches!(tx.try_send(ev.clone()), Err(std::sync::mpsc::TrySendError::Disconnected(_))));
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
        let Some((image, attrs)) = self.lock().containers.get(id).map(|c| (c.spec.image.clone(), container_attrs(c))) else { return };
        self.emit_container_with(action, id, &image, attrs, extra);
    }

    fn emit_container_with(&self, action: &str, id: &str, image: &str, mut attrs: serde_json::Map<String, serde_json::Value>, extra: &[(&str, String)]) {
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

    /// The guest bridges of the container's other networks that are
    /// user-defined, whose names its embedded DNS resolves.
    fn user_network_bridges(&self, spec: &Spec) -> Vec<String> {
        let nets = self.lock_networks();
        spec.extra_networks.iter().filter_map(|x| nets.find(&x.name).filter(|n| n.driver == "bridge" && !n.builtin() && !n.bridge.is_empty()).map(|n| n.bridge.clone())).collect()
    }

    /// Fixed addresses containers not running ask for on a network: an
    /// automatic address passes over them, so they are free when those start.
    fn wanted_addresses(&self, network: &str) -> Vec<[u8; 4]> {
        let st = self.lock();
        let mut out = Vec::new();
        for c in st.containers.values().filter(|c| c.status != Status::Running) {
            if c.spec.network_mode == network {
                out.extend(super::network::parse_ip(&c.spec.ipv4_address));
            }
            out.extend(c.spec.extra_networks.iter().filter(|x| x.name == network).filter_map(|x| super::network::parse_ip(&x.ipv4_address)));
        }
        out
    }

    /// For `--network container:<target>`: the target's id, hostname and
    /// domain name. It has to be running, as with Docker.
    /// Also the network mode the namespace finally belongs to (following
    /// `container:` chains), so the caller can tell a host-network one.
    fn network_container(&self, id: &str, target: &str) -> Result<(String, String, String, String), Error> {
        let st = self.lock();
        let target = resolve(&st, target)?;
        if target == id {
            return Err(Error::BadRequest("cannot join own network".into()));
        }
        let c = st.containers.get(&target).expect("resolved");
        if c.status != Status::Running {
            return Err(Error::Conflict(format!("cannot join network of a non running container: {target}")));
        }
        let hostname = if c.spec.hostname.is_empty() { c.short_id().to_owned() } else { c.spec.hostname.clone() };
        let mut mode = c.spec.network_mode.clone();
        for _ in 0..8 {
            let Some(next) = mode.strip_prefix("container:").and_then(|t| resolve(&st, t).ok()).and_then(|t| st.containers.get(&t)) else { break };
            mode = next.spec.network_mode.clone();
        }
        Ok((target, hostname, c.spec.domainname.clone(), mode))
    }

    /// For `--pid container:<target>`: the target's id. It has to be
    /// running, as with Docker.
    fn pid_container(&self, id: &str, target: &str) -> Result<String, Error> {
        let st = self.lock();
        let target = resolve(&st, target)?;
        if target == id {
            return Err(Error::BadRequest("cannot join own PID namespace".into()));
        }
        if st.containers.get(&target).is_none_or(|c| c.status != Status::Running) {
            return Err(Error::Conflict(format!("cannot join PID of a non running container: {target}")));
        }
        Ok(target)
    }

    /// `--link` lines for /etc/hosts, as Docker writes them: `ip alias
    /// hostname name`. Linked containers must be running; on the default
    /// bridge (which has no DNS) they must be on it too. The lines are fixed
    /// at start: a linked container that restarts with another address is
    /// not followed (Docker rewrites them; restart this one).
    fn link_hosts(&self, id: &str, spec: &Spec, net: &super::network::Network) -> Result<Vec<String>, Error> {
        let st = self.lock();
        let me = st.containers.get(id).map(|c| c.name.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for l in &spec.links {
            let c = st.containers.get(&l.id).ok_or_else(|| Error::NotFound(format!("could not get container for {}", l.name)))?;
            if c.status != Status::Running {
                return Err(Error::Conflict(format!("Cannot link to a non running container: /{} AS /{me}/{}", c.name, l.alias)));
            }
            let ip = if c.spec.network_mode == net.name { c.ip } else { c.extra_ips.iter().find(|(n, _)| *n == net.name).map(|(_, ip)| *ip) };
            let Some(ip) = ip else {
                if net.name == "bridge" {
                    return Err(Error::BadRequest(format!("Cannot link to /{}, as it does not belong to the default network", c.name)));
                }
                continue;
            };
            let hostname = if c.spec.hostname.is_empty() { c.short_id() } else { c.spec.hostname.as_str() };
            // A user network's DNS already answers the container's own names
            // (Compose's `links: [db]`) and follows it across restarts; a
            // fixed line would not.
            if net.name != "bridge" {
                let aliases: &[String] = if c.spec.network_mode == net.name { &c.spec.aliases } else { c.spec.extra_networks.iter().find(|x| x.name == net.name).map_or(&[], |x| &x.aliases) };
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

    /// The look after a start's wait (see the call): `Some(error)` when the
    /// start must end instead of running, already failed (`fail_start`).
    /// `vm_gone` is read under the engine lock. A start by the restart
    /// policy or recovery took `restart_pending` at its claim; refused
    /// because Kiri began to stop, it puts it back under that same lock, so
    /// Stop's hold keeps the container (Exited, pending) whether its loop
    /// ran before or after this, and Start brings it back, as the same error
    /// at the top of `start_on_vm` leaves it. Not when the VM is gone
    /// (recovery and Lost own that) and not for `--rm` (`comes_back`).
    fn refuse_waited_start(&self, id: &str, spec: &Spec, reason: Start, halted_at_claim: bool, vm_gone: impl FnOnce() -> bool) -> Option<Error> {
        let (gone, refuse) = {
            let mut st = self.lock();
            let gone = vm_gone();
            let refuse = refuse_after_wait(halted_at_claim, self.halting(), gone);
            if refuse && !gone && reason != Start::User
                && let Some(c) = st.containers.get_mut(id)
            {
                c.restart_pending = true;
            }
            (gone, refuse)
        };
        if !refuse {
            return None;
        }
        let e = if gone { Error::Io(io::Error::other("the VM stopped")) } else { Error::Io(io::Error::new(io::ErrorKind::WouldBlock, STOPPED)) };
        self.fail_start(id, spec, &e);
        Some(e)
    }

    /// Records a start that never reached the guest as an immediate exit,
    /// with the exit code Docker gives that failure (see `start_exit_code`).
    /// A start the VM's crash ended keeps the crash's exit (255).
    fn fail_start(&self, id: &str, spec: &Spec, err: &dyn std::fmt::Debug) {
        self.fail_start_code(id, spec, start_exit_code(&format!("{err:?}")));
    }

    /// `fail_start` with the exit code given.
    fn fail_start_code(&self, id: &str, spec: &Spec, code: i32) {
        let vm = self.link.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(vm) = vm {
            let _ = vm.is_lost();
        }
        let mut st = self.lock();
        // Lost may have run before this start finished allocating addresses.
        // Reclaim those even when preserving the crash's exit accounting.
        if let Some(c) = st.containers.get_mut(id) {
            // A start that failed sends no `start`.
            c.start_owed = false;
            if let Some(ip) = c.ip.take() {
                self.release_ip(&c.spec.network_mode, ip);
            }
            for (net, ip) in std::mem::take(&mut c.extra_ips) {
                self.release_ip(&net, ip);
            }
            self.release_ip6s(c);
        }
        if !spec.auto_remove && st.containers.get(id).is_some_and(|c| c.vm_stopped) {
            return;
        }
        let actor = st.containers.get(id).map(|c| (c.spec.image.clone(), container_attrs(c)));
        if let Some(c) = st.containers.get_mut(id) {
            c.status = Status::Exited;
            c.exits += 1;
            c.exit_code = code;
            c.finished = Some(SystemTime::now());
            c.attached.clear();
            lock_log(&c.log).end_follow();
        }
        if let Some(h) = st.health.get_mut(id) {
            close_monitor(h);
        }
        // `--rm`: Docker removes a container whose start failed, and an
        // attached `docker run --rm` waits for exactly that (condition
        // "removed") before it reports the error; without it, it hung.
        let removed = if spec.auto_remove { st.containers.remove(id) } else { None };
        if removed.is_some() {
            note_removed(&mut st, id, code);
        }
        drop(st);
        self.changed.notify_all();
        if let Some(c) = removed {
            if let Some(vm) = self.awake() {
                let _ = vm.remove(id);
            }
            lock_log(&c.log).remove();
            for v in self.unshared(c.spec.anon_volumes) {
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
        for c in st.containers.values().filter(|c| c.status != Status::Running) {
            if c.spec.network_mode == network {
                out.extend(super::network::parse_ip6(&c.spec.ipv6_address));
            }
            out.extend(c.spec.extra_networks.iter().filter(|x| x.name == network).filter_map(|x| super::network::parse_ip6(&x.ipv6_address)));
        }
        out
    }

    /// The container's IPv6 address on `n` if it has IPv6: the one asked
    /// for (`want`), or the next free one.
    fn allocate_ip6(&self, n: &super::network::Network, want: &str) -> Result<Option<[u8; 16]>, Error> {
        if n.v6.is_none() && want.is_empty() {
            return Ok(None);
        }
        let avoid = self.wanted_addresses6(&n.name);
        self.lock_networks().allocate6(&n.name, super::network::parse_ip6(want), &avoid).map_err(Error::Conflict)
    }

    /// Where `<host>` (a `*.kiri.localhost` name) should go: the container
    /// and the ports that could be its web port (domains.rs picks one).
    /// Names are the container name, or `<service>.<project>` for Compose
    /// services.
    pub fn route(&self, host: &str) -> Option<super::domains::Target> {
        let host = host.split(':').next()?.to_ascii_lowercase();
        let name = host.strip_suffix(".kiri.localhost").or_else(|| host.strip_suffix(".kiri.local"))?.to_owned();
        let st = self.lock();
        let c = st.containers.values().find(|c| {
            c.status == Status::Running && {
                let label = |k: &str| c.spec.labels.get(k).and_then(|v| v.as_str()).unwrap_or("");
                let service = format!("{}.{}", label("com.docker.compose.service"), label("com.docker.compose.project"));
                // `dev.kiri.domain=api.myapp` (comma-separated for several) picks the names.
                let chosen = label(DOMAIN_LABEL).split(',').map(|d| own_name(d.trim())).any(|d| !d.is_empty() && d.eq_ignore_ascii_case(&name));
                chosen || c.name.eq_ignore_ascii_case(&name) || service.eq_ignore_ascii_case(&name) || (name.len() >= 12 && c.id.starts_with(&name))
            }
        })?;
        let ip = if c.spec.network_mode == "host" { [127, 0, 0, 1] } else { c.ip? };
        Some(super::domains::Target {
            ip,
            id: c.id.clone(),
            label: c.spec.labels.get("dev.kiri.http.port").and_then(|v| v.as_str()).and_then(|p| p.parse().ok()),
            published: c.spec.ports.iter().filter(|b| b.proto != "udp").map(|b| b.container_port).collect(),
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

    /// Waits for a start of the container (see `StartGate`) and gives its
    /// id, or the not-found error when it is gone by then: a `--rm`
    /// container whose start failed, or that exited, while the call waited.
    fn resolve_after_start(&self, key: &str) -> Result<String, Error> {
        let id = self.resolve(key)?;
        self.wait_for_start(&id);
        resolve(&self.lock(), &id)
    }

    /// Waits for a start of this container still under way (see
    /// `StartGate`). Holds no lock while it waits.
    fn wait_for_start(&self, id: &str) {
        let gate = self.lock().containers.get(id).map(|c| c.start_gate.clone());
        if let Some(gate) = gate {
            gate.wait();
        }
    }

    /// Opens a `docker cp` session with the guest for a container, running
    /// or not: the request frame is sent, the caller does the rest.
    pub fn copy(&self, key: &str, op: u8, path: &str, flags: u32) -> Result<kiri_vmm::VsockStream, Error> {
        self.wait_for_start(&self.resolve(key)?);
        // Looked up after the wait: a `--rm` start that failed meanwhile has
        // removed the container, and a copy must not then write into a
        // folder made again for a container that no longer exists.
        let (id, layers) = {
            let st = self.lock();
            let id = resolve(&st, key)?;
            let layers = st.containers.get(&id).map(|c| c.layers.clone()).unwrap_or_default();
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
        let suffix = if super::dns::installed() { "kiri.local" } else { "kiri.localhost" };
        let mut v = self.each(|c| {
            let chosen = c.spec.labels.get(DOMAIN_LABEL).and_then(|v| v.as_str()).map(|d| own_name(d.split(',').next().unwrap_or("").trim()).to_owned());
            (c.status == Status::Running).then(|| format!("{}.{suffix}", chosen.filter(|d| !d.is_empty()).unwrap_or_else(|| c.name.clone())))
        });
        v.sort();
        v
    }

    /// Containers attached to a network.
    pub fn network_members(&self, network: &str) -> Vec<Member> {
        let canonical = self.lock_networks().find(network).map(|n| n.name.clone()).unwrap_or_default();
        self.each(|c| {
            let theirs = self.lock_networks().find(&c.spec.network_mode).map(|n| n.name.clone()).unwrap_or_default();
            if theirs == canonical {
                return Some((c.id.clone(), c.name.clone(), c.ip, c.ip6));
            }
            c.spec.extra_networks.iter().any(|x| x.name == canonical).then(|| {
                let ip = c.extra_ips.iter().find(|(n, _)| *n == canonical).map(|(_, ip)| *ip);
                let ip6 = c.extra_ip6s.iter().find(|(n, _)| *n == canonical).map(|(_, ip)| *ip);
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
                        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "container has exited"));
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
                    // Its first exit is the real one: ids are never reused.
                    let code = st.removed.iter().find(|(r, _)| *r == id).map_or(0, |(_, c)| *c);
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
    fn attach_extra(&self, vm: &VmLink, id: &str, n: u32, extra: &ExtraNetwork) -> Result<(), Error> {
        // Held from the name read to the network being recorded, so a rename
        // lands before (and the guest then has the new name) or after (and
        // renames this network too).
        let sync = self.names_sync(id);
        let _order = sync.as_ref().map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        let net = {
            let mut nets = self.lock_networks();
            nets.settle(&extra.name).map_err(Error::Conflict)?;
            nets.find(&extra.name).cloned().ok_or_else(|| Error::NotFound(format!("network {} not found", extra.name)))?
        };
        if net.driver != "bridge" {
            return Err(Error::BadRequest(format!("only bridge networks can be connected, not {}", net.driver)));
        }
        let ip = match super::network::parse_ip(&extra.ipv4_address) {
            Some(want) => self.lock_networks().reserve(&net.name, want).map(|()| want).map_err(Error::Conflict)?,
            None => {
                let avoid = self.wanted_addresses(&net.name);
                self.lock_networks().allocate_avoiding(&net.name, &avoid).ok_or_else(|| Error::Conflict(format!("no free address left on network {}", net.name)))?
            }
        };
        let off6 = self.lock().containers.get(id).is_some_and(|c| c.spec.ipv6_disabled(&format!("eth{n}")));
        let ip6 = match if off6 { Ok(None) } else { self.allocate_ip6(&net, &extra.ipv6_address) } {
            Ok(a) => a,
            Err(e) => {
                self.lock_networks().release(&net.name, ip);
                return Err(e);
            }
        };
        let (name, hostname) = self.lock().containers.get(id).map(|c| (c.name.clone(), c.short_id().to_owned())).unwrap_or_default();
        let mut names = vec![name, hostname];
        names.extend(extra.aliases.iter().cloned());
        let mut opts: Vec<String> = if extra.mac_address.is_empty() { Vec::new() } else { vec![format!("{}={}", kiri_proto::net_opts::MAC, extra.mac_address)] };
        if let (Some(a), Some(v)) = (ip6, &net.v6) {
            opts.push(format!("{}={}/{}", kiri_proto::net_opts::IP6, super::network::ip6_str(a), v.prefix));
            opts.push(format!("{}={}", kiri_proto::net_opts::GW6, super::network::ip6_str(v.gateway)));
        }
        if net.internal {
            opts.push(format!("{}=1", kiri_proto::net_opts::INTERNAL));
        }
        if net.no_ipv4 {
            opts.push(format!("{}=1", kiri_proto::net_opts::NO_IPV4));
        }
        opts.push(format!("{}={}", kiri_proto::net_opts::NETWORK, net.name));
        if let Err(e) = vm.net_attach(id, n, &net.bridge, ip, net.prefix, net.gateway, &names, &opts) {
            let mut nets = self.lock_networks();
            nets.release(&net.name, ip);
            if let Some(a) = ip6 {
                nets.release6(&net.name, a);
            }
            return Err(Error::Io(e));
        }
        if let Some(c) = self.lock().containers.get_mut(id) {
            c.extra_ips.push((net.name.clone(), ip));
            c.extra_ifs.push((net.name.clone(), n));
            if let Some(a) = ip6 {
                c.extra_ip6s.push((net.name.clone(), a));
            }
        }
        Ok(())
    }

    /// `docker network connect`.
    pub fn connect(&self, network: &str, key: &str, aliases: Vec<String>, ipv4_address: String, ipv6_address: String, mac_address: String) -> Result<(), Error> {
        // After the start, as when the guest answered nothing else during
        // it, so this and the start's own networks are numbered in turn.
        let id = self.resolve_after_start(key)?;
        let net = self.lock_networks().find(network).cloned().ok_or_else(|| Error::NotFound(format!("network {network} not found")))?;
        if let Some(ip) = super::network::parse_ip(&ipv4_address) {
            net.check_static(ip).map_err(Error::BadRequest)?;
        }
        if let Some(ip) = super::network::parse_ip6(&ipv6_address) {
            net.check_static6(ip).map_err(Error::BadRequest)?;
        }
        let (n, running) = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).ok_or_else(|| gone(&id))?;
            if c.spec.first_network_is(&net.name) || c.spec.extra_networks.iter().any(|x| x.name == net.name) {
                return Err(Error::Conflict(format!("endpoint with name {} already exists in network {}", c.name, net.name)));
            }
            // It left every network it had: the one it joins becomes its
            // first, which a start sets up as such. A running one has only
            // loopback and its resolver set for no network, which a plug-in
            // alone would not change.
            if c.spec.left_all_networks {
                if c.status == Status::Running {
                    return Err(Error::Conflict(format!("Kiri cannot connect a running container that left all its networks to {}; stop the container first", net.name)));
                }
                c.spec.join_first_network(ExtraNetwork { name: net.name.clone(), aliases, ipv4_address, ipv6_address, mac_address });
                drop(st);
                self.save();
                self.emit_network("connect", &net, &id);
                return Ok(());
            }
            if matches!(c.spec.network_mode.as_str(), "host" | "none") {
                return Err(Error::BadRequest(format!("container cannot be connected to {} while on {}", net.name, c.spec.network_mode)));
            }
            if c.spec.network_mode.starts_with("container:") {
                return Err(Error::BadRequest("container sharing network namespace with another container or host cannot be connected to any other network".into()));
            }
            c.spec.extra_networks.push(ExtraNetwork { name: net.name.clone(), aliases, ipv4_address, ipv6_address, mac_address });
            // The lowest eth number not in use: a disconnect may have freed
            // one below the top.
            let n = (1..).find(|n| !c.extra_ifs.iter().any(|(_, x)| x == n)).expect("a free number");
            (n, c.status == Status::Running)
        };
        self.save();
        if running {
            let extra = self.lock().containers.get(&id).and_then(|c| c.spec.extra_networks.last().cloned()).ok_or_else(|| gone(&id))?;
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
        // As for connect.
        let id = self.resolve_after_start(key)?;
        // A rename sees the network gone, or is done before it goes: never
        // names put back on an address the container has left.
        let sync = self.names_sync(&id);
        let _order = sync.as_ref().map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        let net = self.lock_networks().find(network).cloned().ok_or_else(|| Error::NotFound(format!("network {network} not found")))?;
        let (n, ip) = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).ok_or_else(|| gone(&id))?;
            if c.spec.first_network_is(&net.name) && matches!(net.name.as_str(), "host" | "none") {
                if c.spec.left_all_networks {
                    return Err(Error::NotFound(format!("container {} is not connected to network {}", &id[..12], net.name)));
                }
                return Err(Error::BadRequest(format!("container cannot be disconnected from {} network", net.name)));
            }
            if c.spec.first_network_is(&net.name) {
                // A running container's first network holds its default
                // route, its main address (which published ports go to) and
                // its resolver: moving those to another network while it
                // runs needs the guest to do more than unplug it.
                if c.status == Status::Running {
                    return Err(Error::Conflict(format!("Kiri cannot disconnect a running container from its first network ({}); stop the container first", net.name)));
                }
                let left = (c.ip.take(), c.ip6.take());
                let nets = self.lock_networks();
                c.spec.leave_first_network(|name| nets.find(name).is_some_and(|n| n.internal));
                drop(nets);
                drop(st);
                // Not running, it holds no address; one left from a run that
                // did not hand it back goes back to the pool all the same.
                if let Some(ip) = left.0 {
                    self.release_ip(&net.name, ip);
                }
                if let Some(a) = left.1 {
                    self.lock_networks().release6(&net.name, a);
                }
                self.save();
                self.emit_network("disconnect", &net, &id);
                return Ok(());
            }
            let Some(pos) = c.spec.extra_networks.iter().position(|x| x.name == net.name) else {
                return Err(Error::NotFound(format!("container {} is not connected to network {}", &id[..12], net.name)));
            };
            c.spec.extra_networks.remove(pos);
            let ip = c.extra_ips.iter().position(|(x, _)| *x == net.name).map(|i| c.extra_ips.remove(i).1);
            if let Some(i) = c.extra_ip6s.iter().position(|(x, _)| *x == net.name) {
                let (_, a) = c.extra_ip6s.remove(i);
                self.lock_networks().release6(&net.name, a);
            }
            let n = c.extra_ifs.iter().position(|(x, _)| *x == net.name).map(|i| c.extra_ifs.remove(i).1);
            (n, ip)
        };
        self.save();
        if let Some(ip) = ip {
            let unplugged = match (self.awake(), n) {
                (Some(vm), Some(n)) => vm.net_detach(&id, n, &net.bridge, ip),
                _ => Ok(()),
            };
            // The network is gone from the container either way: its
            // address goes back too, or the network can never be removed.
            self.release_ip(&net.name, ip);
            unplugged?;
        }
        self.emit_network("disconnect", &net, &id);
        Ok(())
    }

    /// `docker pause` / `docker unpause`.
    pub fn pause(&self, key: &str, paused: bool) -> Result<(), Error> {
        // A container still filling a new volume is not frozen: its start
        // waits for the fill, which a frozen child never finishes, and a
        // `docker commit` (which pauses first) would wait for that start
        // forever, and stop, kill and rm behind it. So pause and unpause
        // wait for the start, as exec and copy do.
        let id = self.resolve_after_start(key)?;
        {
            let st = self.lock();
            let c = st.containers.get(&id).ok_or_else(|| gone(&id))?;
            if c.status != Status::Running {
                return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
            }
            if c.paused == paused {
                return Err(Error::Conflict(format!("Container {} is {}paused", &id[..12], if paused { "already " } else { "not " })));
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
    pub fn update(&self, key: &str, limits: Limits, restart: Option<RestartPolicy>) -> Result<(), Error> {
        // New limits reach the container once its start is done, as when
        // the guest answered nothing else during a start: a memory limit
        // cut while a new volume fills could end the fill half way.
        let id = self.resolve_after_start(key)?;
        let running = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).ok_or_else(|| gone(&id))?;
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
        let name = self.lock().containers.get(&id).map(|c| c.name.clone()).unwrap_or_default();
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
        }
        Ok((id.clone(), name, self.vm()?.container_stats(&id)?))
    }

    /// `docker top`.
    pub fn top(&self, key: &str) -> Result<Vec<String>, Error> {
        let id = self.resolve(key)?;
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
        }
        Ok(self.vm()?.top(&id)?)
    }

    /// Kiri's own: the TCP ports a running container listens on, split into
    /// those a published port would reach and those it would not.
    pub fn listening(&self, key: &str) -> Result<(Vec<u16>, Vec<u16>), Error> {
        let id = self.resolve(key)?;
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
        }
        let mode = self.lock().containers.get(&id).map(|c| c.spec.network_mode.clone()).unwrap_or_default();
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
            let c = st.containers.get_mut(&id).ok_or_else(|| gone(&id))?;
            if c.spec.network_mode == "none" && !ports.is_empty() {
                return Err(Error::BadRequest("a container with --network none cannot publish ports".into()));
            }
            if c.spec.network_mode.starts_with("container:") && !ports.is_empty() {
                return Err(Error::BadRequest("conflicting options: port publishing and the container type network mode".into()));
            }
            let target = if c.spec.network_mode == "host" { Some([127, 0, 0, 1]) } else { c.ip };
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
                    crate::diag::warn!("new ports not published; the old ones are kept", id = &id[..12], error = e);
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
            let c = st.containers.get_mut(&id).ok_or_else(|| gone(&id))?;
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
            return Err(Error::BadRequest(format!("Invalid container name ({new}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed")));
        }
        // After a start under way: its names are registered in the guest
        // only once it is done, so a rename in between would be undone by it.
        let key = self.resolve_after_start(key)?;
        // A start reads the name and sets the pid under the container's
        // lifecycle lock, as a stop+start does: held from the name change to
        // the guest being told, so a start that comes meanwhile registers the
        // new name, and one under way is done (its pid set) when the names
        // are read below. Taken before `names_sync`, in the order a start's
        // `attach_extra` takes them.
        let lifecycle = self.lifecycle(&key)?;
        let _life = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        // One change of this container's guest names at a time (a network
        // joined or left, another rename), each told to the guest in the
        // order it was made.
        let sync = self.names_sync(&key).ok_or_else(|| gone(&key))?;
        let _order = sync.lock().unwrap_or_else(|e| e.into_inner());
        let id = {
            let mut st = self.lock();
            let id = resolve(&st, &key)?;
            if let Some(c) = st.containers.values().find(|c| c.name == new && c.id != id) {
                return Err(name_in_use(new, &c.id));
            }
            let c = st.containers.get_mut(&id).expect("resolved");
            if c.name == new {
                return Err(Error::Conflict(format!("Renaming a container with the same name as its current name")));
            }
            c.name = new.to_owned();
            id
        };
        self.save();
        // The guest's DNS names are registered when a container starts. A
        // rename has to replace them too, or the new name never resolves and
        // the old one keeps answering to the same address.
        let updates = self.dns_names_after_rename(&id);
        // No update for a stopped container, and never a VM woken for one.
        if !updates.is_empty()
            && let Some(vm) = self.awake()
        {
            for (bridge, ip, ip6, v4, names) in updates {
                if let Err(e) = vm.dns_rename(&id, &bridge, ip, ip6, v4, &names) {
                    crate::diag::warn!("the guest's DNS names were not renamed", id = &id[..12], network = bridge, error = e);
                }
            }
        }
        self.emit_container("rename", &id, &[]);
        Ok(())
    }

    /// The DNS names a running container should answer to after a rename,
    /// one set per attached user network: (bridge, IPv4, IPv6, names). The
    /// same names it was given there: at start on its first network (name,
    /// aliases, hostname), on `attach_extra` for the others (name, short ID,
    /// aliases). The default bridge has no embedded DNS and is left out.
    fn dns_names_after_rename(&self, id: &str) -> Vec<RenamedDns> {
        let st = self.lock();
        let Some(c) = st.containers.get(id) else { return Vec::new() };
        if c.pid == 0 {
            return Vec::new();
        }
        let networks = self.lock_networks();
        let mut out = Vec::new();
        if let Some(ip) = c.ip
            && let Some(n) = networks.find(&c.spec.network_mode)
            && !n.builtin()
        {
            let hostname = if c.spec.hostname.is_empty() { c.short_id() } else { c.spec.hostname.as_str() };
            let mut names = vec![c.name.clone(), hostname.to_owned()];
            names.extend(c.spec.aliases.iter().cloned());
            out.push((n.bridge.clone(), ip, c.ip6, !n.no_ipv4, rename_names(names)));
        }
        for (network, ip) in &c.extra_ips {
            let Some(n) = networks.find(network).filter(|n| !n.builtin()) else { continue };
            let ip6 = c.extra_ip6s.iter().find(|(name, _)| name == network).map(|(_, ip)| *ip);
            let mut names = vec![c.name.clone(), c.short_id().to_owned()];
            names.extend(c.spec.extra_networks.iter().find(|x| &x.name == network).into_iter().flat_map(|x| x.aliases.iter().cloned()));
            out.push((n.bridge.clone(), *ip, ip6, !n.no_ipv4, rename_names(names)));
        }
        out
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

    /// `docker stop`. False when there was nothing to stop: the container
    /// was not running and its restart policy was not about to start it.
    pub fn stop(&self, key: &str, timeout: Duration) -> Result<bool, Error> {
        self.stop_as(key, timeout, true)
    }

    /// `docker stop` (`by_user`), or the engine's own stop of a container
    /// (Stop, the VM's sleep on request), which is not recorded as the
    /// user's: `unless-stopped` brings it back on Start.
    fn stop_as(&self, key: &str, timeout: Duration, by_user: bool) -> Result<bool, Error> {
        let ended_start = if by_user {
            self.await_start(key, START_GRACE, Cancel::Stopped)
        } else {
            // Kiri stopping, or its VM going to sleep: not held up by a
            // start, as before.
            self.cancel_start(key, Cancel::Stopped)
        };
        let lifecycle = self.lifecycle(key)?;
        let mut stopping = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let id = self.resolve(key)?;
        *stopping = stopping.wrapping_add(1);
        self.thaw_for_signal(&id);
        let cancelled = by_user && self.mark_stopped(&id);
        if !self.is_running(&id) {
            // A start this stop ended is something it stopped: 204, not
            // "already stopped" (304).
            return Ok(cancelled || ended_start);
        }
        self.emit_container("kill", &id, &[("signal", (libc::SIGTERM).to_string())]);
        let sig = self.lock().containers.get(&id).and_then(|c| c.spec.stop_signal).unwrap_or(libc::SIGTERM as u32);
        if let Some(vm) = self.awake() {
            vm.kill(&id, sig)?;
        }
        let deadline = std::time::Instant::now() + timeout;
        let mut st = self.lock();
        while st.containers.get(&id).is_some_and(|c| c.status == Status::Running) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                drop(st);
                // Never boots a VM: a lost one has already ended it.
                if let Some(vm) = self.awake() {
                    vm.kill(&id, libc::SIGKILL as u32)?;
                }
                // Docker's stop response means the process has exited, even
                // with t=0. Wait for its exit (or auto-removal) to be recorded,
                // as long as `kill` does: the exit may never be reported.
                let killed = std::time::Instant::now() + Duration::from_secs(10);
                let mut exited = self.lock();
                while exited.containers.get(&id).is_some_and(|c| c.status == Status::Running) {
                    let left = killed.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err(Error::Io(io::Error::other(format!("cannot stop container: {}: it did not exit after SIGKILL", &id[..12]))));
                    }
                    exited = self.changed.wait_timeout(exited, left).unwrap_or_else(|e| e.into_inner()).0;
                }
                drop(exited);
                self.emit_container("stop", &id, &[]);
                return Ok(true);
            }
            st = self.changed.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        drop(st);
        self.emit_container("stop", &id, &[]);
        Ok(true)
    }

    /// A frozen process cannot handle a signal; Docker thaws it first.
    fn thaw_for_signal(&self, id: &str) {
        if self.is_paused(id) {
            if let Some(vm) = self.awake() {
                if vm.freeze(id, false).is_ok() {
                    if let Some(c) = self.lock().containers.get_mut(id) {
                        c.paused = false;
                    }
                    self.health_resume(id);
                }
            }
        }
    }

    /// A `docker stop`, `kill` or `rm -f` that comes while the container is
    /// starting waits for that start, as Docker's does (its start holds the
    /// container's lock throughout, and its stop, kill and removal take it):
    /// the start answers as it would have (204 once the process runs), and
    /// the signal then reaches the process. Before, a kill sent now found no
    /// process in the guest and was dropped; then (#621) the three cancelled
    /// the start, which answered 409 where Docker answers 204, after inspect
    /// had already shown the container running.
    ///
    /// A start that has not finished after `limit` is taken as stuck (its
    /// first process never got its root in place) and cancelled
    /// (`cancel_gate`): it holds the container's lifecycle lock until the
    /// guest says the root is in place, so without this the stop, kill or
    /// removal would queue behind it for good. True when it was cancelled.
    fn await_start(&self, key: &str, limit: Duration, by: Cancel) -> bool {
        let (id, gate) = {
            let st = self.lock();
            let Ok(id) = resolve(&st, key) else { return false };
            let gate = st.containers[&id].start_gate.clone();
            (id, gate)
        };
        if gate.wait_for(limit) {
            return false;
        }
        crate::diag::warn!("start not finished; cancelled", id = &id[..12], by = format!("{by:?}"), waited_ms = limit.as_millis());
        self.cancel_gate(&id, &gate, by)
    }

    /// Kiri's own stop of a container whose start is still under way: the
    /// start is cancelled at once (see `cancel_gate`). True when a start was
    /// under way.
    fn cancel_start(&self, key: &str, by: Cancel) -> bool {
        self.await_start(key, Duration::ZERO, by)
    }

    /// Cancels the start that shut `gate`, if it is still under way. That
    /// start holds the container's lifecycle lock until the guest says its
    /// root is in place, which a child stuck before then never does; so
    /// before waiting for that lock, the start is told
    /// (`StartGate::cancel`, which its wait reads) and so is the guest
    /// (`Msg::CancelStart`): a child still making its root is killed, the
    /// start fails, and the lock is free. A child filling a volume is left
    /// to finish (a slow fill is no reason to fail), then killed; a child
    /// whose root is in place is not touched, and its start goes on.
    fn cancel_gate(&self, id: &str, gate: &StartGate, by: Cancel) -> bool {
        if !gate.is_shut() {
            return false;
        }
        gate.cancel_by.fetch_max(by as u8, std::sync::atomic::Ordering::SeqCst);
        gate.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        // Never boots a VM: a start under way has one.
        if let Some(vm) = self.awake() {
            let _ = vm.cancel_start(id);
        }
        true
    }

    pub fn kill(&self, key: &str, signal: u32) -> Result<(), Error> {
        let cancelled = self.await_start(key, START_GRACE, Cancel::Stopped);
        let lifecycle = self.lifecycle(key)?;
        let mut killing = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let id = self.resolve(key)?;
        self.thaw_for_signal(&id);
        if !self.is_running(&id) {
            if (signal == libc::SIGKILL as u32 || signal == libc::SIGTERM as u32)
                && self.with(&id, recover_after_vm_loss)?
            {
                *killing = killing.wrapping_add(1);
                self.mark_stopped(&id);
                self.save();
                return Ok(());
            }
            if cancelled {
                // Its start was under way, and ended because of this kill.
                return Ok(());
            }
            return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
        }
        if signal == libc::SIGKILL as u32 || signal == libc::SIGTERM as u32 {
            *killing = killing.wrapping_add(1);
            self.mark_stopped(&id);
        }
        self.emit_container("kill", &id, &[("signal", signal.to_string())]);
        if let Some(vm) = self.awake() {
            vm.kill(&id, signal)?;
        }
        // Like Docker: a SIGKILL returns only once the exit is recorded, so
        // an immediate inspect, start or rm sees the container stopped.
        if signal == libc::SIGKILL as u32 {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut st = self.lock();
            while st.containers.get(&id).is_some_and(|c| c.status == Status::Running) {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                st = self.changed.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
            }
        }
        Ok(())
    }

    pub fn remove(&self, key: &str, force: bool, with_volumes: bool) -> Result<(), Error> {
        if force {
            self.await_start(key, REMOVE_GRACE, Cancel::Removed);
        }
        let lifecycle = self.lifecycle(key)?;
        let mut removing = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let id = self.resolve(key)?;
        if self.is_running(&id) {
            if !force {
                return Err(Error::Conflict(format!(
                    "cannot remove container \"{}\": container is running: stop the container before removing or force remove",
                    &id[..12]
                )));
            }
            self.mark_stopped(&id);
            if let Some(vm) = self.awake() {
                vm.kill(&id, libc::SIGKILL as u32)?;
            }
            let mut st = self.lock();
            while st.containers.get(&id).is_some_and(|c| c.status == Status::Running) {
                st = self.changed.wait_timeout(st, Duration::from_secs(5)).unwrap_or_else(|e| e.into_inner()).0;
            }
        }
        *removing = removing.wrapping_add(1);
        self.emit_container("destroy", &id, &[]);
        let mut st = self.lock();
        let removed = st.containers.remove(&id);
        // A `--rm` container that exited while the kill above waited has
        // removed itself and noted its code (137 when this kill ended it);
        // noting it again, with no record to read, told its waiters 0.
        if let Some(c) = &removed {
            note_removed(&mut st, &id, c.exit_code);
        }
        st.execs.retain(|_, e| e.container_id != id);
        drop(st);
        self.changed.notify_all();
        let anon = self.unshared(removed.as_ref().map(|c| c.spec.anon_volumes.clone()).unwrap_or_default());
        if let Some(c) = &removed {
            lock_log(&c.log).remove();
        }
        if removed.as_ref().is_some_and(|c| !c.spec.auto_remove) {
            self.save();
        }
        // Its writable layer is on the guest's disk, whether it ran in this
        // boot, the one before a crash, or never started. Removal must not
        // boot a replacement or wait for a retiring VM: while none is awake,
        // retain() prunes the layer on the next boot.
        // The container is already gone: a link that broke a moment ago
        // doesn't make rm fail, retain() takes the layer on the next boot.
        if removed.is_some()
            && let Some(vm) = self.awake()
            && let Err(e) = vm.remove(&id)
        {
            crate::diag::warn!("writable layer left for the next boot", id = id, error = e);
        }
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
    pub fn volume_ensure(&self, name: &str, labels: serde_json::Value, anonymous: bool) -> Result<super::volumes::Volume, Error> {
        let new = self.volume_get(name).is_none();
        let v = self.lock_volumes().ensure(name, labels, anonymous).map_err(Error::BadRequest)?;
        if new {
            self.emit_volume("create", name);
        }
        Ok(v)
    }

    /// [`Engine::volume_ensure`] with the local driver's options, which a
    /// new volume keeps (an existing one keeps its own).
    pub fn volume_ensure_with(&self, name: &str, labels: serde_json::Value, options: std::collections::BTreeMap<String, String>) -> Result<super::volumes::Volume, Error> {
        let new = self.volume_get(name).is_none();
        let v = self.lock_volumes().ensure_with(name, labels, false, options).map_err(Error::BadRequest)?;
        if new {
            self.emit_volume("create", name);
        }
        Ok(v)
    }

    /// Containers using a volume, by name.
    pub fn volume_users(&self, name: &str) -> Vec<String> {
        self.each(|c| (c.spec.volumes.iter().chain(&c.spec.anon_volumes).any(|v| v == name)).then(|| c.name.clone()))
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
            return Err(Error::Conflict(format!("remove {name}: volume is in use - [{}]", users.join(", "))));
        }
        if !force && let Some(why) = self.refusal(&format!("Volume {name} was not removed, because the container list may be missing a container that uses it; `docker volume rm --force` removes it anyway")) {
            return Err(Error::Conflict(why));
        }
        self.delete_volume_data(name);
        Ok(())
    }

    /// Forgets a volume and deletes its data in the guest.
    /// The anonymous volumes of a removed container that no other container
    /// uses: one made with `--volumes-from` keeps them, as in Docker.
    fn unshared(&self, anon: Vec<String>) -> Vec<String> {
        if anon.is_empty() {
            return anon;
        }
        anon.into_iter().filter(|v| self.volume_users(v).is_empty()).collect()
    }

    fn delete_volume_data(&self, name: &str) {
        if self.forget_volume(name) {
            if let Ok(vm) = self.vm() {
                let _ = vm.remove_volume(name);
            }
        }
    }

    /// Drops the volume's record and says so; its data stays on the guest's
    /// disk for the caller to remove. False if there was no such volume.
    fn forget_volume(&self, name: &str) -> bool {
        let forgot = self.lock_volumes().forget(name);
        if forgot {
            self.emit_volume("destroy", name);
        }
        forgot
    }

    /// `docker exec` step one: record what to run.
    pub fn exec_create(&self, key: &str, config: ExecConfig) -> Result<String, Error> {
        let mut st = self.lock();
        let container_id = resolve(&st, key)?;
        if st.containers.get(&container_id).is_none_or(|c| c.status != Status::Running) {
            return Err(Error::Conflict(format!("container {} is not running", &container_id[..12])));
        }
        // A paused BuildKit container is only idle: the exec resumes it. If the
        // idle pause comes back before the exec starts, exec_start resumes it.
        let builder = st.containers.get(&container_id).is_some_and(|c| c.paused && is_builder(c));
        if builder {
            drop(st);
            match self.pause(&container_id, false) {
                Ok(()) | Err(Error::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
            st = self.lock();
        }
        if !builder && st.containers.get(&container_id).is_some_and(|c| c.paused) {
            return Err(Error::Conflict(format!("Container {} is paused, unpause the container before exec", &container_id[..12])));
        }
        if config.argv.is_empty() {
            return Err(Error::BadRequest("no exec command specified".into()));
        }
        let id = random_hex(32);
        let cmd = config.argv.join(" ");
        let internal = config.internal;
        st.execs.insert(id.clone(), Exec { id: id.clone(), container_id: container_id.clone(), config, running: false, exit_code: None, pid: 0, attached: Vec::new() });
        drop(st);
        if !internal {
            self.emit_container(&format!("exec_create: {cmd}"), &container_id, &[("execID", id.clone())]);
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
            let e = st.execs.get_mut(exec_id).ok_or_else(|| Error::NotFound(format!("No such exec instance: {exec_id}")))?;
            if e.running || e.exit_code.is_some() {
                return Err(Error::Conflict("exec has already run".into()));
            }
            e.running = true;
            // Subscribe before the process exists, so no output is missed.
            e.attached.push(tx);
            let (container_id, config) = (e.container_id.clone(), e.config.clone());
            let (security, caps) = st.containers.get(&container_id).map(|c| (c.spec.security_flags() & !run_flags::READONLY_ROOTFS, c.spec.caps)).unwrap_or_default();
            // Execs get the container's ulimits and groups, as with runc.
            let opts = st.containers.get(&container_id).map(|c| c.spec.guest_opts(false).unwrap_or_default()).unwrap_or_default();
            exec_security = (security, caps, opts);
            let mut env = st.containers.get(&container_id).map(|c| c.spec.env.clone()).unwrap_or_default();
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
        if self.lock().containers.get(&container_id).is_some_and(|c| c.paused && is_builder(c))
            && let Err(e) = self.pause(&container_id, false)
            && !matches!(e, Error::Conflict(_))
        {
            crate::diag::warn!("BuildKit container not resumed for an exec", error = format!("{e:?}"));
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
        self.wait_for_start(&container_id);
        let started = self.vm()?.exec(&container_id, exec_id, &config.argv, &env, &config.cwd, &config.user, flags, exec_security.1, &exec_security.2);
        if let Ok(pid) = started {
            if let Some(x) = self.lock().execs.get_mut(exec_id).filter(|x| x.exit_code.is_none()) {
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
            self.emit_container(&format!("exec_start: {}", config.argv.join(" ")), &container_id, &[("execID", exec_id.to_owned())]);
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
        st.execs.get(exec_id).map(f).ok_or_else(|| Error::NotFound(format!("No such exec instance: {exec_id}")))
    }

    /// Execs running in a container now (buildx's `buildctl dial-stdio`
    /// while it builds, for one): the managed builder isn't idle while any are.
    pub fn running_execs(&self, container_id: &str) -> usize {
        self.lock().execs.values().filter(|e| e.container_id == container_id && e.running).count()
    }

    /// Drops a finished exec's record: Kiri's own, which nobody inspects.
    pub fn forget_exec(&self, exec_id: &str) {
        self.lock().execs.remove(exec_id);
    }

    fn is_running(&self, id: &str) -> bool {
        self.lock().containers.get(id).is_some_and(|c| c.status == Status::Running)
    }

    /// Records a stop asked for by the user. True when that cancelled a
    /// restart its policy had pending.
    fn mark_stopped(&self, id: &str) -> bool {
        let (persist, cancelled) = {
            let mut st = self.lock();
            match st.containers.get_mut(id) {
                Some(c) => {
                    c.stopped_by_user = true;
                    (!c.spec.auto_remove, std::mem::take(&mut c.restart_pending))
                }
                None => (false, false),
            }
        };
        self.changed.notify_all();
        if persist {
            self.save();
        }
        cancelled
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
        self.awake()?.stats().ok().map(|s| (s.mem_total_kib * 1024, s.mem_available_kib * 1024))
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
        // A boot under way finishes first, so its VM is stopped below too;
        // none starts after this (vm_inner checks SHUTTING_DOWN under it).
        drop(self.waking.lock().unwrap_or_else(|e| e.into_inner()));
        self.save();
        for log in self.each(|c| Some(c.log.clone())) {
            lock_log(&log).flush();
        }
        let ids: Vec<String> = self.each(|c| (c.status == Status::Running).then(|| c.id.clone()));
        if let Some(vm) = self.awake() {
            self.stop_together(&vm, &ids, Stopping::Shutdown);
        }
        if let Some(vm) = self.awake() {
            vm.shutdown();
            if !vm.wait_stopped(Duration::from_secs(10)) {
                crate::diag::warn!("the VM did not power off in 10 s; stopped it");
            }
        }
        // The syncer stops; the last saves are synced here, and any save
        // after this syncs on its own thread.
        self.syncer.lock().unwrap_or_else(|e| e.into_inner()).take();
        let _one_writer = self.saving.lock().unwrap_or_else(|e| e.into_inner());
        self.sync_records();
    }
}

/// The `--dns` servers a container uses, as with Docker.
const MAX_DNS_SERVERS: usize = 3;

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
    // The embedded DNS asks the first three IPv4 `--dns` servers, as Docker
    // does: later IPv4 ones are left out. IPv6 ones (for resolv.conf, which
    // the embedded DNS doesn't ask) don't take an IPv4 server's place, and
    // are bounded to three as well.
    let is_v4 = |d: &&String| d.parse::<std::net::Ipv4Addr>().is_ok();
    let v4 = spec.dns.iter().filter(is_v4).count();
    let other = spec.dns.len() - v4;
    if v4 > MAX_DNS_SERVERS || other > MAX_DNS_SERVERS {
        crate::diag::warn!("more than three IPv4 or other --dns servers: only the first three of each are used", given = spec.dns.len());
    }
    let (mut kept4, mut kept_other) = (0, 0);
    for d in &spec.dns {
        let kept = if is_v4(&d) { &mut kept4 } else { &mut kept_other };
        if *kept == MAX_DNS_SERVERS {
            continue;
        }
        *kept += 1;
        opts.push(format!("{}={d}", k::NAMESERVER));
    }
    opts.extend(spec.dns_search.iter().map(|d| format!("{}={d}", k::SEARCH)));
    opts.extend(spec.dns_options.iter().map(|d| format!("{}={d}", k::OPTION)));
    if !domainname.is_empty() {
        opts.push(format!("{}={domainname}", k::DOMAINNAME));
    }
    opts
}

fn log_event(typ: &str, action: &str, actor: &str, attrs: &serde_json::Map<String, serde_json::Value>) {
    let attr = |k: &str| attrs.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let id: String = actor.strip_prefix("sha256:").unwrap_or(actor).chars().take(12).collect();
    let (name, image) = (attr("name"), attr("image"));
    match (typ, action) {
        ("container", "die") => {
            let code = attr("exitCode");
            if code == "0" {
                crate::diag::info!("container exited", id = id, name = name, image = image, exit_code = code);
            } else {
                crate::diag::warn!("container exited", id = id, name = name, image = image, exit_code = code);
            }
        }
        ("container", "exec_die") if name.starts_with("buildx_buildkit_") => {
            crate::diag::info!("build session ended", builder = name, exit_code = attr("exitCode"));
        }
        ("container", "create" | "start" | "stop" | "destroy" | "kill" | "rename" | "commit") => {
            crate::diag::info!(format!("container {action}"), id = id, name = name, image = image);
        }
        ("image", _) => crate::diag::info!(format!("image {action}"), id = id, name = attr("name")),
        _ => crate::diag::debug!(format!("{typ} {action}"), id = id, name = name),
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// How the VM came to an end, for what the log says about it.
#[derive(Debug, PartialEq, Eq)]
enum VmEnd {
    /// Put to sleep: idle, Stop, or a restart of the VM.
    Slept,
    /// Stopped with the daemon, which was asked to stop.
    ShutDown,
    /// Nobody asked: an error.
    Crashed,
}

fn vm_end(sleeping: bool, shutting_down: bool) -> VmEnd {
    if sleeping {
        VmEnd::Slept
    } else if shutting_down {
        VmEnd::ShutDown
    } else {
        VmEnd::Crashed
    }
}

/// Whether the container's restart policy starts it when the daemon comes
/// up, or on Start after Stop: `always`, and `unless-stopped` unless the
/// user stopped it (`docker stop`), as Docker's daemon does.
fn comes_back(c: &Container) -> bool {
    let policy = c.spec.restart.name.as_str();
    (policy == "always" || (policy == "unless-stopped" && !c.stopped_by_user)) && !c.spec.auto_remove
}

/// Containers that join another's network or PID namespace, or link to
/// others, start after those, as Docker orders them.
fn joins_others(c: &Container) -> bool {
    c.spec.network_mode.starts_with("container:") || c.spec.pid_mode.starts_with("container:") || !c.spec.links.is_empty()
}

/// What a container event carries: labels, then name and image.
fn container_attrs(c: &Container) -> serde_json::Map<String, serde_json::Value> {
    let mut attrs = c.spec.labels.as_object().cloned().unwrap_or_default();
    attrs.insert("name".into(), c.name.clone().into());
    attrs.insert("image".into(), c.spec.image.clone().into());
    attrs
}

/// The exit code Docker records for a container that never started: 127
/// when the command (or a file it needs) is missing, or a mount puts a
/// directory on a file; 126 when it may not be run; and 128 for anything
/// else, such as a network that could not be set up.
fn start_exit_code(err: &str) -> i32 {
    let err = err.to_lowercase();
    if ["executable file not found", "no such file or directory", "system cannot find the file specified", "not a directory"].iter().any(|m| err.contains(m)) {
        127
    } else if err.contains("permission denied") {
        126
    } else {
        128
    }
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
pub fn is_builder(c: &Container) -> bool {
    c.name.starts_with("buildx_buildkit_")
}

/// Every volume (named, anonymous or `--mount`) a spec uses.
fn volume_names(spec: &Spec) -> Vec<&str> {
    let mut v: Vec<&str> = spec.volumes.iter().chain(&spec.anon_volumes).map(String::as_str).collect();
    v.extend(spec.mount_info.iter().filter(|m| m["Type"] == "volume").filter_map(|m| m["Name"].as_str()));
    v
}

/// The start gates already shut of other containers that share a volume
/// with `spec` (of container `id`). Bind and tmpfs mounts are not volumes.
fn shared_volume_gates(st: &State, id: &str, spec: &Spec) -> Vec<Arc<StartGate>> {
    let mine = volume_names(spec);
    if mine.is_empty() {
        return Vec::new();
    }
    st.containers
        .values()
        .filter(|c| c.id != id && c.start_gate.is_shut() && volume_names(&c.spec).iter().any(|v| mine.contains(v)))
        .map(|c| c.start_gate.clone())
        .collect()
}

/// Whether a start that waited for other starts must not run its container
/// after all: Kiri began to stop meanwhile (not already when the start was
/// claimed: Stop's signals reach the guest only for containers it knows,
/// which this one was not), or the VM is gone.
fn refuse_after_wait(halted_at_claim: bool, halting: bool, vm_gone: bool) -> bool {
    (halting && !halted_at_claim) || vm_gone
}

/// The error for a container removed while a call waited for its start.
fn gone(id: &str) -> Error {
    Error::NotFound(format!("No such container: {id}"))
}

fn resolve(st: &State, key: &str) -> Result<String, Error> {
    let key = key.trim_start_matches('/');
    if st.containers.contains_key(key) {
        return Ok(key.to_owned());
    }
    if let Some(c) = st.containers.values().find(|c| c.name == key) {
        return Ok(c.id.clone());
    }
    let mut matches = st.containers.keys().filter(|id| !key.is_empty() && id.starts_with(key));
    match (matches.next(), matches.next()) {
        (Some(id), None) => Ok(id.clone()),
        (Some(_), Some(_)) => Err(Error::NotFound(format!("multiple containers match {key}"))),
        _ => Err(Error::NotFound(format!("No such container: {key}"))),
    }
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| io::Read::read_exact(&mut f, &mut b)).is_err() {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
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
    d.strip_suffix(".kiri.localhost").or_else(|| d.strip_suffix(".kiri.local")).unwrap_or(d)
}

/// Docker's container-name rule: `[a-zA-Z0-9][a-zA-Z0-9_.-]*`. A name is a
/// DNS label on user networks and part of `<name>.kiri.localhost`, so it
/// must not carry spaces, slashes or control characters, nor start with a
/// dot, dash or underscore.
fn valid_container_name(n: &str) -> bool {
    n.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// Docker's words for a name another container has, on create and rename,
/// naming that container. Clients match on them: Testcontainers retries its
/// shared reaper's create only when the message says "already in use by",
/// so without it parallel test packages fail on a reaper another one made.
fn name_in_use(name: &str, holder: &str) -> Error {
    Error::Conflict(format!(
        "Conflict. The container name \"/{name}\" is already in use by container \"{holder}\". You have to remove (or rename) that container to be able to reuse that name."
    ))
}

/// The image `name` means: a name, an ID, or `name@digest` and
/// `name:tag@digest` by the platform manifest's digest or the index's it was
/// pulled by.
fn image_named(images: Vec<Image>, name: &str) -> Option<Image> {
    let reference = kiri_image::Reference::parse(name).ok();
    let short = reference.as_ref().map(|r| r.short()).unwrap_or_else(|| name.to_owned());
    images.into_iter().find(|i| {
        i.name == short
            || i.name == name
            || reference.as_ref().is_some_and(|r| i.matches(r))
            || i.config_digest == name
            || i.config_digest.strip_prefix("sha256:").is_some_and(|h| name.len() >= 12 && h.starts_with(name))
    })
}

/// The image each `ancestor` filter value names, as Docker resolves it: a
/// name (`alpine` is `alpine:latest`, not every tag of it), a reference, or
/// an ID or ID prefix of any length, with or without `sha256:`. A value that
/// names no image adds nothing, so it matches no container, as in Docker
/// (#442).
fn ancestor_ids(images: &[Image], values: &[String]) -> Vec<String> {
    let by_id = |v: &str| {
        let hex = v.strip_prefix("sha256:").unwrap_or(v);
        let is_id = !hex.is_empty() && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        images.iter().find(|i| is_id && i.config_digest.strip_prefix("sha256:").is_some_and(|h| h.starts_with(hex))).cloned()
    };
    values.iter().filter_map(|v| image_named(images.to_vec(), v).or_else(|| by_id(v))).map(|i| i.config_digest).collect()
}

#[cfg(test)]
mod refusal_tests {
    /// A daemon shutting down (WSL or Windows stopping it, a restart) is
    /// not a Stop: it says to try again, never to press Start (#93). A
    /// Stop says what ends it, from the command line too.
    #[test]
    fn only_a_stop_says_to_start() {
        assert_eq!(super::boot_refusal(false, false), None);
        let shutting = super::boot_refusal(false, true).unwrap();
        assert!(!shutting.contains("Start") && !shutting.contains("stopped"), "{shutting}");
        assert!(shutting.contains("again"), "{shutting}");
        assert_eq!(super::boot_refusal(true, false), Some(super::STOPPED));
        assert_eq!(super::boot_refusal(true, true), Some(super::STOPPED));
        assert!(super::STOPPED.starts_with("Kiri is stopped") && super::STOPPED.contains("`kiri start`"), "{}", super::STOPPED);
    }
}

#[cfg(test)]
mod image_lookup_tests {
    use kiri_image::{Image, RunConfig};

    const INDEX: &str = "sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5";
    const ARM64: &str = "sha256:6a3345f517a7df05185c648f83d18d464f2a94ea4a69eaee39b7e1de02611e59";

    fn image(name: &str, index: Option<&str>) -> Image {
        Image { mirror: None,
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
        assert_eq!(pulled[0].repo_digest(), Some(format!("kindest/node@{INDEX}")));
    }

    #[test]
    fn a_digest_only_finds_images_of_its_repository() {
        let pulled = [image("kindest/node:v1.37.0", Some(INDEX))];
        assert!(found(&pulled, &format!("kindest/node@{INDEX}")));
        assert!(found(&pulled, "kindest/node:v1.37.0"));
        assert!(!found(&pulled, &format!("kindest/haproxy@{INDEX}")));
        assert!(!found(&pulled, &format!("ghcr.io/kindest/node@{INDEX}")));
        assert!(!found(&pulled, &format!("kindest/node@sha256:{}", "b".repeat(64))));
        assert!(!found(&pulled, "kindest/node:v1.36.0"));
    }

    #[test]
    fn an_image_without_an_index_shows_its_manifest_digest() {
        let pulled = image("alpine:latest", None);
        assert_eq!(pulled.repo_digest(), Some(format!("alpine@{ARM64}")));
        assert!(found(&[pulled], &format!("alpine:3@{ARM64}")));
    }

    #[test]
    fn the_ancestor_filter_names_one_image_like_docker() {
        // #442: `alpine` is `alpine:latest`, not every tag of alpine.
        let with_id = |name: &str, c: char| Image { config_digest: format!("sha256:{}", c.to_string().repeat(64)), ..image(name, None) };
        let images = [with_id("alpine:latest", 'a'), with_id("alpine:3.21", 'b'), with_id("web:1", 'c')];
        let ids = |v: &str| super::ancestor_ids(&images, &[v.to_owned()]);
        let latest = vec![format!("sha256:{}", "a".repeat(64))];
        for v in ["alpine", "alpine:latest", "library/alpine", "docker.io/library/alpine:latest", "aaaa", "sha256:aaaaaaaaaaaa", &latest[0]] {
            assert_eq!(ids(v), latest, "{v}");
        }
        assert_eq!(ids("alpine:3.21"), vec![format!("sha256:{}", "b".repeat(64))]);
        // Not a pattern, and nothing for a name no image has.
        for v in ["alp*", "alpine:*", "nosuchimage", "AAAA", ""] {
            assert!(ids(v).is_empty(), "{v}");
        }
        // Values OR together.
        assert_eq!(super::ancestor_ids(&images, &["web:1".into(), "alpine:3.21".into()]).len(), 2);
    }
}

#[cfg(test)]
mod name_in_use_tests {
    #[test]
    fn a_taken_name_says_which_container_has_it_as_docker_does() {
        let id = "4f1c".repeat(16);
        let super::Error::Conflict(m) = super::name_in_use("reaper_1", &id) else { panic!("not a conflict") };
        assert_eq!(
            m,
            format!("Conflict. The container name \"/reaper_1\" is already in use by container \"{id}\". You have to remove (or rename) that container to be able to reuse that name.")
        );
        // What Testcontainers for Go matches before it retries its reaper.
        assert!(m.contains("The container name \"/reaper_1\" is already in use by "));
    }
}

#[cfg(test)]
mod memory_target_tests {
    use super::{estimate_used, next_memory_target, BalloonState, MEMORY_FLOOR, MEMORY_GROW, MEMORY_HEADROOM};

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    /// An 8 GiB VM's guest, idle (#509): the MemTotal it reports, and
    /// what its cgroups hold.
    const CEILING: u64 = 8 * GIB;
    const MEM_TOTAL: u64 = 8_211_244 * 1024;
    const USED: u64 = 100 * MIB;

    /// Runs the balloon loop from `target` until it stops moving (older agent
    /// without a balloon reading).
    fn settle(used: u64, mem_total: u64, mut target: u64) -> u64 {
        loop {
            let balloon = CEILING.saturating_sub(target);
            let mem_available = mem_total.saturating_sub(balloon).saturating_sub(used);
            let Some(next) = next_memory_target(used, mem_total, mem_available, None, CEILING, target) else {
                return target;
            };
            target = next;
        }
    }

    /// Runs the balloon loop against a simulated guest where `unavailable`
    /// bytes of `mem_total` are held by processes, uncharged kernel memory
    /// and zone reserves (`wmark_low + lowmem_reserve`), and `balloon` tracks
    /// `ceiling - target`.
    fn settle_guest(ceiling: u64, mem_total: u64, used: u64, unavailable: u64) -> u64 {
        let mut target = ceiling;
        loop {
            let balloon = ceiling - target;
            let mem_available = mem_total.saturating_sub(balloon).saturating_sub(unavailable);
            let Some(next) = next_memory_target(used, mem_total, mem_available, Some(balloon), ceiling, target) else {
                return target;
            };
            target = next;
        }
    }

    /// The guest keeps the headroom above what it holds and what its kernel
    /// took at boot, so it isn't left below its reclaim watermarks.
    #[test]
    fn the_guest_keeps_its_headroom_beyond_its_kernels_reserve() {
        let target = settle(USED, MEM_TOTAL, CEILING);
        let reserved = CEILING - MEM_TOTAL;
        assert!(reserved > 150 * MIB, "an 8 GiB guest's kernel keeps about 173 MiB");
        assert_eq!(target, USED + reserved + MEMORY_HEADROOM);
        // Settled: nothing left to move.
        assert_eq!(next_memory_target(USED, MEM_TOTAL, MEMORY_HEADROOM, None, CEILING, target), None);
    }

    /// On a larger VM (24 GiB), zone watermarks, lowmem_reserve (~153 MiB)
    /// and uncharged kernel memory (~110 MiB) exceed MEMORY_HEADROOM by
    /// themselves. With the guest's live balloon and MemAvailable, the guest
    /// still settles with at least MEMORY_HEADROOM of MemAvailable.
    #[test]
    fn large_vm_keeps_mem_available_headroom_beyond_zone_reserves() {
        let ceiling = 24 * GIB;
        let mem_total = 24_656_160 * 1024;
        let used = 200 * MIB;
        // cgroup-held memory + zone watermarks & lowmem_reserve (~153 MiB)
        // + uncharged kernel memory & active file cache (~115 MiB).
        let unavailable = used + 268 * MIB;
        let target = settle_guest(ceiling, mem_total, used, unavailable);
        let balloon = ceiling - target;
        let mem_available = mem_total.saturating_sub(balloon).saturating_sub(unavailable);
        assert!(mem_available >= MEMORY_HEADROOM, "MemAvailable settled at {} MiB", mem_available / MIB);
        assert!(mem_available < MEMORY_HEADROOM + 64 * MIB, "kept too much headroom: {} MiB", mem_available / MIB);
        assert_eq!(next_memory_target(used, mem_total, mem_available, Some(balloon), ceiling, target), None);
    }

    /// When the guest's balloon lags behind `ceiling - target` while
    /// `MemAvailable` stays above headroom (during shrink or non-clamped
    /// deflation), `balloon + mem_available` stays constant and the next
    /// target does not overshoot or oscillate.
    #[test]
    fn target_is_invariant_to_balloon_lag() {
        let ceiling = 24 * GIB;
        let mem_total = 24_656_160 * 1024;
        let unavailable = 300 * MIB;
        let target = 2 * GIB;
        let caught_up_balloon = ceiling - target;
        let caught_up_avail = mem_total - caught_up_balloon - unavailable;
        let shrink_lag_balloon = caught_up_balloon - 180 * MIB;
        let shrink_lag_avail = mem_total - shrink_lag_balloon - unavailable;
        let deflate_lag_balloon = caught_up_balloon + 180 * MIB;
        let deflate_lag_avail = mem_total - deflate_lag_balloon - unavailable;
        let expected = next_memory_target(100 * MIB, mem_total, caught_up_avail, Some(caught_up_balloon), ceiling, target);
        assert_eq!(expected, next_memory_target(100 * MIB, mem_total, shrink_lag_avail, Some(shrink_lag_balloon), ceiling, target));
        assert_eq!(expected, next_memory_target(100 * MIB, mem_total, deflate_lag_avail, Some(deflate_lag_balloon), ceiling, target));
    }

    /// Cold page cache is counted in MemAvailable by Linux, so the balloon
    /// still reclaims it rather than hoarding gigabytes after a large read.
    #[test]
    fn cold_page_cache_is_reclaimed() {
        let ceiling = 24 * GIB;
        let mem_total = 24_656_160 * 1024;
        let target = 5 * GIB;
        let balloon = ceiling - target;
        // 300 MiB committed + 4 GiB clean page cache (counted in MemAvailable).
        let mem_available = mem_total - balloon - 300 * MIB;
        assert_eq!(
            next_memory_target(20 * MIB, mem_total, mem_available, Some(balloon), ceiling, target),
            Some(target - 256 * MIB),
        );
    }

    /// If MemAvailable drops to zero under pressure, the target grows at once.
    #[test]
    fn zero_mem_available_under_pressure_grows_target() {
        let ceiling = 24 * GIB;
        let mem_total = 24_656_160 * 1024;
        let target = GIB;
        let balloon = ceiling - target;
        assert_eq!(
            next_memory_target(200 * MIB, mem_total, 0, Some(balloon), ceiling, target),
            Some(target + MEMORY_HEADROOM),
        );
    }

    #[test]
    fn it_shrinks_in_steps_and_grows_at_once() {
        assert_eq!(next_memory_target(USED, MEM_TOTAL, MEM_TOTAL - USED, None, CEILING, CEILING), Some(CEILING - 256 * MIB));
        let target = settle(USED, MEM_TOTAL, CEILING);
        let more = USED + GIB;
        assert_eq!(next_memory_target(more, MEM_TOTAL, 0, None, CEILING, target), Some(more + (CEILING - MEM_TOTAL) + MEMORY_HEADROOM));
    }

    #[test]
    fn small_changes_leave_it_alone() {
        let target = settle(USED, MEM_TOTAL, CEILING);
        assert_eq!(next_memory_target(USED + 32 * MIB, MEM_TOTAL, MEMORY_HEADROOM - 32 * MIB, None, CEILING, target), None);
    }

    /// No MemTotal (an agent that sent none): no reserve to add.
    #[test]
    fn without_a_mem_total_only_the_headroom_is_added() {
        assert_eq!(settle(USED, 0, CEILING), USED + MEMORY_HEADROOM);
    }

    #[test]
    fn it_stays_between_the_floor_and_the_ceiling() {
        assert_eq!(settle(0, CEILING, CEILING), MEMORY_FLOOR);
        assert_eq!(settle(CEILING, MEM_TOTAL, CEILING / 2), CEILING);
        assert_eq!(next_memory_target(u64::MAX, u64::MAX, u64::MAX, Some(u64::MAX), CEILING, CEILING / 2), Some(CEILING));
    }

    /// Right after a pressure growth (`MEMORY_GROW`), the balloon is still
    /// deflating (`balloon > ceiling - target`) while `MemAvailable` is clamped
    /// at zero (or still below headroom). The poll loop must hold `target`
    /// steady instead of shrinking it back down and fighting the deflation.
    #[test]
    fn deflation_lag_after_growth_does_not_shrink_target() {
        let ceiling = 24 * GIB;
        let mem_total = 24_656_160 * 1024;
        let old_target = GIB;
        let target = old_target + MEMORY_GROW;
        let unstarted_balloon = ceiling - old_target;
        assert_eq!(next_memory_target(200 * MIB, mem_total, 0, Some(unstarted_balloon), ceiling, target), None);
        // Partial deflation that has not yet restored MEMORY_HEADROOM still holds.
        let partial_balloon = unstarted_balloon - 128 * MIB;
        assert_eq!(next_memory_target(200 * MIB, mem_total, 64 * MIB, Some(partial_balloon), ceiling, target), None);
        // Real demand beyond `target` can still grow immediately.
        assert!(next_memory_target(2 * GIB, mem_total, 0, Some(unstarted_balloon), ceiling, target).is_some_and(|n| n > target));
        // Once deflation restores ample MemAvailable, normal shrinking resumes.
        let caught_up_balloon = ceiling - target;
        let caught_up_avail = mem_total - caught_up_balloon - 300 * MIB;
        assert_eq!(
            next_memory_target(200 * MIB, mem_total, caught_up_avail, Some(caught_up_balloon), ceiling, target),
            Some(target - 256 * MIB),
        );
    }

    /// An older agent that omits `Balloon` still reports `MemTotal` and
    /// `MemAvailable`. Even when zone reserves and uncharged kernel memory
    /// exceed `MEMORY_HEADROOM`, the balloon must not shrink past
    /// `MEMORY_HEADROOM` of `MemAvailable` or undo a pressure growth.
    #[test]
    fn missing_balloon_reading_preserves_mem_available_headroom() {
        let ceiling = 24 * GIB;
        let mem_total: u64 = 24_656_160 * 1024;
        let used = 200 * MIB;
        let unavailable = used + 268 * MIB;
        let mut target = ceiling;
        loop {
            let balloon = ceiling - target;
            let mem_available = mem_total.saturating_sub(balloon).saturating_sub(unavailable);
            let Some(next) = next_memory_target(used, mem_total, mem_available, None, ceiling, target) else {
                break;
            };
            target = next;
        }
        let mem_available = mem_total.saturating_sub(ceiling - target).saturating_sub(unavailable);
        assert!(mem_available >= MEMORY_HEADROOM, "MemAvailable settled at {} MiB", mem_available / MIB);
        assert!(mem_available < MEMORY_HEADROOM + 64 * MIB, "kept too much headroom: {} MiB", mem_available / MIB);
        // After a pressure growth with MemAvailable still at 0, do not shrink.
        assert_eq!(next_memory_target(used, mem_total, 0, None, ceiling, target + MEMORY_GROW), None);
    }

    /// A ceiling below MEMORY_FLOOR (e.g. a small KIRI_MEMORY_MIB) must clamp
    /// cleanly instead of panicking in `u64::clamp`.
    #[test]
    fn ceiling_below_floor_does_not_panic() {
        let small_ceiling = 128 * MIB;
        let small_total = 120 * MIB;
        assert_eq!(next_memory_target(32 * MIB, small_total, 64 * MIB, Some(0), small_ceiling, small_ceiling), None);
        assert_eq!(next_memory_target(32 * MIB, small_total, 64 * MIB, None, small_ceiling, 0), Some(small_ceiling));
        assert_eq!(next_memory_target(0, 0, 0, None, 0, 0), None);
    }

    /// An older agent reporting neither `mem_used_kib` nor `mem_balloon_kib`
    /// derives `used` from `ceiling - target`. Under deflation lag (either
    /// after a fallback grow or after `grow_memory()`), repeated polls with
    /// `MemAvailable == 0` must not ratchet `target` toward `ceiling`, and
    /// under shrink lag it must not overshoot downward before inflation lands.
    #[test]
    fn legacy_fallback_does_not_ratchet_or_overshoot_under_lag() {
        let ceiling = 24 * GIB;
        let mem_total_kib = 24_656_160;
        let legacy_stats = |mem_available_kib| kiri_proto::Stats {
            uptime_ns: 0,
            mem_total_kib,
            mem_available_kib,
            agent_rss_kib: 0,
            mem_used_kib: 0,
            mem_stall: 0,
            oom_kills: 0,
            mem_balloon_kib: None,
        };

        // 1. Deflation lag after a fallback grow: grows once, then holds while
        //    MemAvailable stays 0 (or climbs partway during deflation).
        let mut state = BalloonState::default();
        let mut target = GIB;
        let st = legacy_stats(0);
        let used = estimate_used(&st, ceiling, target, 0);
        let first = state.next_target(&st, used, ceiling, target);
        assert_eq!(first, Some(GIB + MEMORY_HEADROOM));
        target = first.unwrap();
        for _ in 0..20 {
            let used = estimate_used(&st, ceiling, target, 0);
            assert_eq!(state.next_target(&st, used, ceiling, target), None);
        }
        // Partial deflation still below headroom does not double-grow.
        let partial = legacy_stats(128 * 1024);
        let used = estimate_used(&partial, ceiling, target, 0);
        assert_eq!(state.next_target(&partial, used, ceiling, target), None);
        // Once deflation restores ample headroom, normal shrinking resumes.
        let recovered = legacy_stats(512 * 1024);
        let used = estimate_used(&recovered, ceiling, target, 0);
        assert_eq!(state.next_target(&recovered, used, ceiling, target), Some(target - 256 * MIB));

        // 2. Deflation lag right after an external `grow_memory()` bump:
        //    zero additional fallback grows while MemAvailable remains 0.
        let mut state = BalloonState::default();
        let settled = legacy_stats(MEMORY_HEADROOM / 1024);
        let used = estimate_used(&settled, ceiling, GIB, 0);
        assert_eq!(state.next_target(&settled, used, ceiling, GIB), None);
        let grown = GIB + MEMORY_GROW;
        for _ in 0..10 {
            let used = estimate_used(&st, ceiling, grown, 0);
            assert_eq!(state.next_target(&st, used, ceiling, grown), None);
        }

        // 3. Shrink lag: balloon has not inflated yet after a 256 MiB step,
        //    so MemAvailable stays at 512 MiB. Must hold until inflation lands
        //    instead of stepping past equilibrium into MemAvailable == 0.
        let mut state = BalloonState::default();
        let reserved = ceiling - mem_total_kib * 1024;
        let unavailable = 300 * MIB;
        let eq_target = reserved + unavailable + MEMORY_HEADROOM;
        let start = eq_target + 256 * MIB;
        let high_avail = legacy_stats((512 * MIB) / 1024);
        let used = estimate_used(&high_avail, ceiling, start, 0);
        let shrunk = state.next_target(&high_avail, used, ceiling, start);
        assert_eq!(shrunk, Some(eq_target));
        // Two lagging polls before the balloon finishes inflating: no overshoot.
        for _ in 0..2 {
            let used = estimate_used(&high_avail, ceiling, eq_target, 0);
            assert_eq!(state.next_target(&high_avail, used, ceiling, eq_target), None);
        }
        // Once inflation catches up, MemAvailable settles at MEMORY_HEADROOM.
        let used = estimate_used(&settled, ceiling, eq_target, 0);
        assert_eq!(state.next_target(&settled, used, ceiling, eq_target), None);
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
        Error::NotFound(m) | Error::Conflict(m) | Error::BadRequest(m) | Error::Unauthorized(m) => m.clone(),
        Error::Io(e) => e.to_string(),
    }
}

/// The longest a daemon shutdown waits for one container, whatever its
/// `--stop-timeout` says.
const SHUTDOWN_WAIT_CAP: Duration = Duration::from_secs(120);

/// Who stops every container: the daemon shutting down (each wait capped,
/// no events), or Kiri's Stop (`docker stop`'s waits and events).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stopping {
    Shutdown,
    Engine,
}

impl Engine {
    /// How long `docker stop` waits when the request does not say: the
    /// container's `--stop-timeout`, else 10 s.
    pub fn stop_timeout(&self, key: &str) -> Duration {
        stop_wait(self.with(key, |c| c.spec.stop_timeout).ok().flatten().unwrap_or(DEFAULT_STOP_TIMEOUT))
    }

    /// Daemon shutdown and Kiri's Stop: every running container gets its
    /// stop signal and its own stop timeout, as `docker stop` would, then
    /// SIGKILL, all at once. Returns the ones still running after SIGKILL.
    fn stop_together(&self, vm: &VmLink, ids: &[String], how: Stopping) -> Vec<String> {
        let engine = how == Stopping::Engine;
        let t0 = std::time::Instant::now();
        // (id, when to SIGKILL it, when it was SIGKILLed)
        let mut pending: Vec<(String, std::time::Instant, Option<std::time::Instant>)> = Vec::new();
        for id in ids {
            let Some((sig, secs)) = self.lock().containers.get(id).map(|c| (c.spec.stop_signal.unwrap_or(libc::SIGTERM as u32), c.spec.stop_timeout.unwrap_or(DEFAULT_STOP_TIMEOUT)))
            else {
                continue;
            };
            self.thaw_for_signal(id);
            if engine {
                self.emit_container("kill", id, &[("signal", sig.to_string())]);
            }
            let _ = vm.kill(id, sig);
            let wait = if engine { stop_wait(secs) } else { stop_wait(secs).min(SHUTDOWN_WAIT_CAP) };
            pending.push((id.clone(), t0 + wait, None));
        }
        // A SIGKILLed container's exit is quick; don't hang on one that
        // isn't. Kiri's Stop waits as long as `docker stop` does.
        let grace = if engine { Duration::from_secs(10) } else { Duration::from_secs(2) };
        let mut exited = Vec::new();
        let mut st = self.lock();
        loop {
            let now = std::time::Instant::now();
            pending.retain(|(id, _, _)| {
                let running = st.containers.get(id).is_some_and(|c| c.status == Status::Running);
                if !running {
                    exited.push(id.clone());
                }
                running
            });
            for (id, deadline, killed) in &mut pending {
                if *deadline <= now && killed.is_none() {
                    let _ = vm.kill(id, libc::SIGKILL as u32);
                    *killed = Some(now);
                }
            }
            if pending.iter().all(|(_, _, k)| k.is_some_and(|k| now.duration_since(k) > grace)) {
                break;
            }
            let next = pending.iter().filter(|(_, _, k)| k.is_none()).map(|(_, d, _)| d.saturating_duration_since(now)).min().unwrap_or(Duration::from_millis(100));
            st = self.changed.wait_timeout(st, next.clamp(Duration::from_millis(10), Duration::from_millis(250))).unwrap_or_else(|e| e.into_inner()).0;
        }
        drop(st);
        if engine {
            for id in &exited {
                self.emit_container("stop", id, &[]);
            }
        }
        pending.into_iter().map(|(id, _, _)| id).collect()
    }

    /// `State.Health` of a container with a healthcheck.
    pub fn health(&self, id: &str) -> Option<super::health::Health> {
        self.lock().health.get(id).map(|h| h.health.clone())
    }

    /// Health status by container id, for `docker ps`.
    pub fn health_statuses(&self) -> HashMap<String, super::health::Health> {
        self.lock().health.iter().map(|(id, h)| (id.clone(), h.health.clone())).collect()
    }

    /// Docker's `initHealthMonitor` at every start: status back to
    /// starting, failing streak 0, the log kept, and a monitor probing.
    fn health_start(&self, id: &str) {
        {
            let mut st = self.lock();
            let wanted = st.containers.get(id).is_some_and(|c| c.spec.healthcheck.as_ref().and_then(|h| h.probe(&c.spec.shell)).is_some());
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
        let Some(me) = self.me.get().and_then(Weak::upgrade) else { return };
        let epoch = {
            let mut st = self.lock();
            let wanted = st
                .containers
                .get(id)
                .is_some_and(|c| c.status == Status::Running && !c.paused && c.spec.healthcheck.as_ref().and_then(|h| h.probe(&c.spec.shell)).is_some());
            let Some(run) = st.health.get_mut(id) else { return };
            if !wanted || run.monitoring {
                return;
            }
            run.monitoring = true;
            run.epoch += 1;
            run.epoch
        };
        let id = id.to_owned();
        let spawned = std::thread::Builder::new().name("kiri-health".into()).spawn(move || me.monitor(&id, epoch));
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
                let (Some(c), Some(run)) = (st.containers.get(id), st.health.get(id)) else { return };
                if run.epoch != epoch {
                    return;
                }
                let Some(check) = c.spec.healthcheck.clone() else { return };
                let Some(argv) = check.probe(&c.spec.shell) else { return };
                let started = c.started.unwrap_or_else(SystemTime::now);
                let since = started.elapsed().unwrap_or_default();
                let wait = run.health.next_wait(since, &check);
                (check, argv, c.spec.cwd.clone(), c.spec.user.clone(), started, wait)
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
                    st = self.changed.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
            let start = SystemTime::now();
            let (exit_code, output) = self.probe(id, argv, cwd, user, check.timeout());
            let end = SystemTime::now();
            let changed = {
                let mut st = self.lock();
                let auto_remove = st.containers.get(id).is_some_and(|c| c.spec.auto_remove);
                let Some(run) = st.health.get_mut(id).filter(|h| h.epoch == epoch) else { return };
                // Stopped meanwhile: Docker drops the result.
                let before = run.health.status.clone();
                let since = start.duration_since(started).unwrap_or_default();
                let probe = super::health::Probe { start: super::time::rfc3339(start), end: super::time::rfc3339(end), exit_code, output };
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
    fn probe(&self, id: &str, argv: Vec<String>, cwd: String, user: String, timeout: Duration) -> (i64, String) {
        use super::health::{OUTPUT_LIMIT, cap_output, go_duration};
        let config = ExecConfig { console_size: None, argv, env: Vec::new(), cwd, user, tty: false, stdin: false, privileged: false, internal: false };
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
            while rx.recv_timeout(until.saturating_duration_since(std::time::Instant::now())).is_ok() {}
        }
        let code = self.with_exec(&exec_id, |e| e.exit_code).ok().flatten();
        self.lock().execs.remove(&exec_id);
        if timed_out {
            return (-1, format!("Health check exceeded timeout ({})", go_duration(timeout)));
        }
        (code.map_or(-1, i64::from), cap_output(&out))
    }
}

#[cfg(test)]
mod vm_recovery_tests {
    use super::*;

    /// Run only in an isolated HOME and KIRI_DATA_DIR containing an Alpine
    /// image and no containers, with a signed test binary and guest assets.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires an isolated store, signed binary and a real VM"]
    fn vm_loss_live_recovers_without_a_request() {
        assert_eq!(std::env::var("KIRI_LIVE_VM_RECOVERY").as_deref(), Ok("1"));
        let root = PathBuf::from(std::env::var_os("KIRI_DATA_DIR").expect("isolated KIRI_DATA_DIR"));
        let records = root.join("containers.json");
        if records.exists() {
            let saved: Vec<Saved> = serde_json::from_slice(&std::fs::read(records).unwrap()).unwrap();
            assert!(saved.is_empty(), "the live test requires an empty container store");
        }
        let store = Store::open(&root).unwrap();
        let image = store.images().unwrap().into_iter().find(|i| i.name == "alpine:latest").expect("pull alpine:latest into the isolated store first");
        let engine = Engine::boot(store).unwrap();
        let mut ids = Vec::new();
        for (name, policy) in [("live-always", "always"), ("live-unless", "unless-stopped"), ("live-no", "no"), ("live-stopped", "unless-stopped"), ("live-failure", "on-failure")] {
            let spec: Spec = serde_json::from_value(serde_json::json!({
                "image": image.name, "image_id": image.digest, "entrypoint": [], "cmd": ["sleep", "600"],
                "env": [], "cwd": "/", "user": "", "auto_remove": false,
                "network_mode": if name == "live-always" { "bridge" } else { "none" },
                "labels": if name == "live-always" { serde_json::json!({"dev.kiri.k8s": "true"}) } else { serde_json::json!({}) },
                "restart": {"name": policy, "max_retries": 0}
            })).unwrap();
            let id = engine.create(Some(name.into()), spec, &image).unwrap();
            engine.start(&id).unwrap();
            ids.push(id);
        }
        engine.stop(&ids[3], Duration::ZERO).unwrap();
        for count in 1..=2 {
            let shared = engine.k8s_forward([10, 42, 0, 2], 8080).unwrap();
            let vm = engine.awake().unwrap();
            assert!(!vm.wait_stopped(Duration::ZERO), "force-stop this test's VM");
            drop(vm);
            let until = std::time::Instant::now() + Duration::from_secs(10);
            let mut st = engine.lock();
            while !ids[..2].iter().all(|id| st.containers[id].status == Status::Running && st.containers[id].restart_count == count) {
                let left = until.saturating_duration_since(std::time::Instant::now());
                assert!(!left.is_zero(), "policy containers stayed down without a new VM request");
                st = engine.changed.wait_timeout(st, left.min(Duration::from_millis(100))).unwrap().0;
            }
            for id in &ids[2..] {
                assert_eq!(st.containers[id].status, Status::Exited);
                assert_eq!(st.containers[id].restart_count, 0);
            }
            assert!(engine.k8s_fwd.lock().unwrap().is_empty(), "VM loss must release old Kubernetes shares");
            let _rebound = std::net::TcpListener::bind(("127.0.0.1", shared)).expect("the old share's listening socket must be released");
            println!("PASS: actual VM loss {count} recovered without a boot request; RestartCount={count}; excluded policies stayed down");
        }
        // A layer blob missing on the host fails that send alone: nothing
        // was sent yet, so the VM and its containers carry on.
        {
            let vm = engine.awake().unwrap();
            let missing = root.join("retirement-missing.blob");
            assert!(vm.send_layer("sha256:0000000000000000000000000000000000000000000000000000000000000000", "application/vnd.oci.image.layer.v1.tar+gzip", &missing).is_err());
            assert!(!vm.is_disconnected(), "a missing blob must not cost the VM");
            assert!(vm.stats().is_ok(), "the link still answers after a missing blob");
            println!("PASS: missing layer blob fails before the header; the VM carries on");
        }
        for (fault, count) in [("truncated", 3)] {
            let mut canceled = Vec::new();
            for action in ["stop", "kill", "remove"] {
                let mut spec = engine.with(&ids[1], |c| c.spec.clone()).unwrap();
                spec.restart.name = "always".into();
                let id = engine.create(Some(format!("retire-{fault}-{action}")), spec, &image).unwrap();
                engine.start(&id).unwrap();
                canceled.push(id);
            }
            let old = engine.awake().unwrap();
            let held = old.hold_retirement();
            let attempts = engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst);
            let blob = root.join(format!("retirement-{fault}.blob"));
            if fault == "truncated" {
                std::fs::write(&blob, b"").unwrap();
            }
            assert!(old.interrupted_layer(&blob).is_err());
            let _ = old.is_lost();
            let requested_engine = engine.clone();
            let (sent, received) = std::sync::mpsc::channel();
            let request = std::thread::spawn(move || sent.send(requested_engine.vm()).unwrap());
            assert!(received.recv_timeout(Duration::from_millis(200)).is_err(), "fallback must wait for retirement");
            assert_eq!(engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), attempts, "neither recovery nor fallback may open replacement storage before retirement");
            let stopping = std::time::Instant::now();
            engine.stop(&canceled[0], Duration::ZERO).unwrap();
            engine.kill(&canceled[1], libc::SIGKILL as u32).unwrap();
            engine.remove(&canceled[2], false, false).unwrap();
            assert!(stopping.elapsed() < Duration::from_secs(2), "retirement must not hold container lifecycle locks");
            drop(held);
            let replacement = received.recv_timeout(Duration::from_secs(15)).unwrap().unwrap();
            request.join().unwrap();
            let until = std::time::Instant::now() + Duration::from_secs(15);
            let mut st = engine.lock();
            while !ids[..2].iter().all(|id| st.containers[id].status == Status::Running && st.containers[id].restart_count == count) {
                let left = until.saturating_duration_since(std::time::Instant::now());
                assert!(!left.is_zero(), "retirement must allow recovery after confirmed stop");
                st = engine.changed.wait_timeout(st, left.min(Duration::from_millis(100))).unwrap().0;
            }
            assert!(st.containers[&canceled[0]].stopped_by_user);
            assert_eq!(st.containers[&canceled[0]].status, Status::Exited);
            assert_eq!(st.containers[&canceled[1]].status, Status::Exited);
            assert!(!st.containers.contains_key(&canceled[2]));
            drop(st);
            old.retire().unwrap();
            drop(old);
            assert!(replacement.stats().is_ok(), "late destruction of the old generation must not affect its successor");
            if blob.exists() { std::fs::remove_file(blob).unwrap(); }
            println!("PASS: {fault} layer after header; no replacement before retirement; retained old handle inert; stop/kill/remove cancel");
        }
        {
            // Ten crashes in a row, the last VM just booted: the next waits a minute.
            let mut st = engine.lock();
            st.vm_crashes = 10;
            st.vm_booted_at = Some(std::time::Instant::now());
        }
        let vm = engine.awake().unwrap();
        assert!(!vm.wait_stopped(Duration::ZERO));
        let lost_vm = vm;
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let mut st = engine.lock();
        while ids[..2].iter().any(|id| st.containers[id].status == Status::Running) {
            let left = until.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "the lost VM must finish exit accounting");
            st = engine.changed.wait_timeout(st, left).unwrap().0;
        }
        assert!(st.vm_recovery_at.unwrap() > std::time::Instant::now() + Duration::from_secs(50));
        let spec = st.containers[&ids[0]].spec.clone();
        drop(st);
        let lifecycle = engine.lifecycle(&ids[0]).unwrap();
        let held = lifecycle.lock().unwrap();
        assert!(engine.start_on_vm(&ids[0], Start::VmRecovery, lost_vm, held).is_err(), "a start borrowing the old VM cannot claim Running after Lost");
        assert_eq!(engine.with(&ids[0], |c| (c.status, c.vm_stopped, c.exit_code)).unwrap(), (Status::Exited, true, 255));
        engine.fail_start(&ids[0], &spec, &"the VM stopped");
        assert_eq!(engine.with(&ids[0], |c| c.exit_code).unwrap(), 255, "a late start failure must preserve VM-loss accounting");
        assert!(!engine.is_awake(), "a disconnected VM must not be reported as awake");
        assert!(matches!(engine.start_inner(&ids[1], Start::VmRecovery), Err(Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock), "a remaining candidate must not reboot during backoff");
        let stopping = std::time::Instant::now();
        for id in &ids[..2] {
            engine.stop(id, Duration::ZERO).unwrap();
        }
        assert!(stopping.elapsed() < Duration::from_secs(2), "backoff must not hold a lifecycle lock");
        let mut st = engine.lock();
        while st.vm_recovery_running {
            let left = until.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "stopping the candidates cancels the recovery worker");
            st = engine.changed.wait_timeout(st, left.min(Duration::from_millis(100))).unwrap().0;
        }
        drop(st);
        assert!(!engine.is_awake(), "no eligible containers means the VM stays down");
        println!("PASS: a crash loop backs off for 60 s; manual stops cancel recovery without waiting for the delay");
        engine.shutdown();
    }

    /// A start whose child never says its root is in place holds the
    /// container's lifecycle lock for as long as it waits. `kill` and
    /// `stop` wait `START_GRACE` for it, `rm -f` `REMOVE_GRACE`
    /// (`await_start`), then cancel it, instead of queueing behind it for
    /// good. The start here is modelled as `VmLink::run`'s wait: it ends,
    /// failed, once it sees the cancel.
    #[test]
    fn rm_kill_and_stop_cancel_a_start_that_never_finishes() {
        let dir = std::env::temp_dir().join(format!("kiri-cancel-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::boot(Store::open(&dir).unwrap()).unwrap();
        // No real VM: the start below stands in for the guest's answers.
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let image = Image { mirror: None,
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        for op in ["rm -f", "kill", "kill TERM", "stop"] {
            let spec: Spec = serde_json::from_value(serde_json::json!({
                "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["sleep", "60"],
                "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            })).unwrap();
            let id = engine.create(Some(format!("stuck-{}", op.replace(' ', ""))), spec.clone(), &image).unwrap();
            let (why, reason) = std::sync::mpsc::channel();
            // Claimed as start_on_vm claims it: Running, gate shut, the
            // lifecycle lock held across the wait.
            let lifecycle = engine.lifecycle(&id).unwrap();
            let (held, holding) = std::sync::mpsc::channel();
            let start = {
                let (engine, id) = (engine.clone(), id.clone());
                std::thread::spawn(move || {
                    let _starting = lifecycle.lock().unwrap();
                    let (gate, guard) = {
                        let mut st = engine.lock();
                        let c = st.containers.get_mut(&id).unwrap();
                        c.status = Status::Running;
                        let guard = c.begin_start();
                        (c.start_gate.clone(), guard)
                    };
                    held.send(()).unwrap();
                    // The guest never says the root is in place; only a
                    // cancel ends the wait, as a failed start.
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while !gate.cancel.load(std::sync::atomic::Ordering::SeqCst) {
                        assert!(std::time::Instant::now() < deadline, "{op}: never cancelled");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    why.send(gate.cancelled_by()).unwrap();
                    engine.fail_start(&id, &spec, &"cancelled");
                    drop(guard);
                })
            };
            holding.recv().unwrap();
            let t = std::time::Instant::now();
            let (done, finished) = std::sync::mpsc::channel();
            let worker = {
                let (engine, id) = (engine.clone(), id.clone());
                std::thread::spawn(move || {
                    let r = match op {
                        "rm -f" => engine.remove(&id, true, false),
                        "kill" => engine.kill(&id, libc::SIGKILL as u32),
                        "kill TERM" => engine.kill(&id, libc::SIGTERM as u32),
                        // The stop ended the start: 204, not 304.
                        _ => engine.stop(&id, Duration::from_secs(1)).and_then(|stopped| if stopped { Ok(()) } else { Err(Error::Conflict("answered 304".into())) }),
                    };
                    done.send(r.map_err(|e| format!("{e:?}"))).unwrap();
                })
            };
            let got = finished.recv_timeout(START_GRACE * 3);
            let took = t.elapsed();
            start.join().unwrap();
            worker.join().unwrap();
            assert_eq!(got, Ok(Ok(())), "{op} of a container whose start never finishes");
            match op {
                "rm -f" => {
                    assert!(took >= REMOVE_GRACE && took < START_GRACE, "rm -f waits for the start, briefly: {took:?}");
                    assert_eq!(reason.recv().unwrap(), Some(Cancel::Removed));
                }
                _ => {
                    assert!(took >= START_GRACE, "{op} waits for the start first: {took:?}");
                    assert_eq!(reason.recv().unwrap(), Some(Cancel::Stopped), "{op}");
                }
            }
            let st = engine.lock();
            match op {
                "rm -f" => assert!(!st.containers.contains_key(&id), "removed"),
                _ => assert_eq!(st.containers[&id].status, Status::Exited, "{op}: no longer running"),
            }
        }
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A `kill` (any signal), `stop` or `rm -f` that comes while a healthy
    /// start is under way waits for it, as Docker's does: the start is not
    /// cancelled (it answers 204), and the signal goes to the process once
    /// the guest has it. Before, the kill was dropped (no process yet), then
    /// (#621) all three cancelled the start, which answered 409. The start is modelled:
    /// the guest has the process after 150 ms (gate opened, pid set), and
    /// the process exits once a stop or kill reaches it.
    #[test]
    fn a_stop_or_kill_during_a_healthy_start_waits_for_it() {
        let dir = std::env::temp_dir().join(format!("kiri-signal-during-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::boot(Store::open(&dir).unwrap()).unwrap();
        // No real VM: the threads below stand in for the guest.
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let image = Image { mirror: None,
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let start_takes = Duration::from_millis(150);
        assert!(start_takes < REMOVE_GRACE && REMOVE_GRACE < START_GRACE);
        for op in ["kill", "kill TERM", "kill USR1", "stop", "rm -f"] {
            let spec: Spec = serde_json::from_value(serde_json::json!({
                "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["sleep", "60"],
                "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            })).unwrap();
            let id = engine.create(Some(format!("healthy-{}", op.replace(' ', ""))), spec, &image).unwrap();
            let lifecycle = engine.lifecycle(&id).unwrap();
            let (held, holding) = std::sync::mpsc::channel();
            let start = {
                let (engine, id) = (engine.clone(), id.clone());
                std::thread::spawn(move || {
                    let starting = lifecycle.lock().unwrap();
                    let (gate, guard) = {
                        let mut st = engine.lock();
                        let c = st.containers.get_mut(&id).unwrap();
                        c.status = Status::Running;
                        let guard = c.begin_start();
                        (c.start_gate.clone(), guard)
                    };
                    held.send(()).unwrap();
                    std::thread::sleep(start_takes);
                    let cancelled = gate.cancel.load(std::sync::atomic::Ordering::SeqCst);
                    // The guest has the process: the pid is set, the gate
                    // opens, the lifecycle lock goes.
                    engine.lock().containers.get_mut(&id).unwrap().pid = 42;
                    let started_at = std::time::Instant::now();
                    drop(guard);
                    drop(starting);
                    (cancelled, started_at)
                })
            };
            holding.recv().unwrap();
            // The process: it exits once a stop or a kill that ends it is
            // recorded (both mark the container stopped by the user).
            let exits = op != "kill USR1";
            let process = {
                let (engine, id) = (engine.clone(), id.clone());
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + if exits { Duration::from_secs(5) } else { Duration::from_millis(500) };
                    let mut st = engine.lock();
                    while !st.containers[&id].stopped_by_user {
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        if left.is_zero() {
                            return;
                        }
                        st = engine.changed.wait_timeout(st, left).unwrap().0;
                    }
                    let c = st.containers.get_mut(&id).unwrap();
                    c.status = Status::Exited;
                    c.exit_code = 137;
                    c.exits += 1;
                    drop(st);
                    engine.changed.notify_all();
                })
            };
            let r = match op {
                "kill" => engine.kill(&id, libc::SIGKILL as u32),
                "kill TERM" => engine.kill(&id, libc::SIGTERM as u32),
                "kill USR1" => engine.kill(&id, libc::SIGUSR1 as u32),
                "rm -f" => engine.remove(&id, true, false),
                _ => engine.stop(&id, Duration::from_secs(1)).map(drop),
            };
            let answered = std::time::Instant::now();
            let (cancelled, started_at) = start.join().unwrap();
            process.join().unwrap();
            assert!(r.is_ok(), "{op}: {r:?}");
            assert!(!cancelled, "{op} must not cancel a start that finishes");
            assert!(answered >= started_at, "{op} answered before the start finished");
            let st = engine.lock();
            if op == "rm -f" {
                assert!(!st.containers.contains_key(&id), "removed once it had started");
                continue;
            }
            let c = &st.containers[&id];
            match exits {
                true => assert_eq!((c.status, c.exit_code), (Status::Exited, 137), "{op}: delivered once the process existed"),
                false => assert_eq!(c.status, Status::Running, "{op}: a signal that does not end it"),
            }
        }
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What a cancelled start answers: Docker's own words for a start that
    /// a removal got to first; a 409 for a stuck start that a stop or kill
    /// ended; nothing special for a start that was not cancelled.
    #[test]
    fn a_cancelled_start_answers_as_docker_does() {
        let id = "0123456789abcdef0123";
        let msg = |by| match cancelled_start_error(by, id) {
            Some(Error::Conflict(m)) => Some(m),
            other => other.map(|e| format!("not a 409: {e:?}")),
        };
        assert_eq!(msg(Some(Cancel::Removed)).as_deref(), Some("container is marked for removal and cannot be started"));
        assert_eq!(msg(Some(Cancel::Stopped)).as_deref(), Some("container 0123456789ab was stopped before it finished starting"));
        assert_eq!(msg(None), None);
        // A removal and a stop both cancelling: the removal's answer.
        let gate = StartGate::default();
        gate.cancel_by.fetch_max(Cancel::Stopped as u8, std::sync::atomic::Ordering::SeqCst);
        gate.cancel_by.fetch_max(Cancel::Removed as u8, std::sync::atomic::Ordering::SeqCst);
        gate.cancel_by.fetch_max(Cancel::Stopped as u8, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(gate.cancelled_by(), Some(Cancel::Removed));
    }

    #[test]
    fn a_policy_start_waits_while_kiri_stops() {
        let dir = std::env::temp_dir().join(format!("kiri-vm-parking-{}", std::process::id()));
        let engine = Engine::boot(Store::open(&dir).unwrap()).unwrap();
        let image = Image { mirror: None,
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["sleep", "60"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "restart": {"name": "always", "max_retries": 0}
        })).unwrap();
        let id = engine.create(Some("parked".into()), spec, &image).unwrap();
        {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.status = Status::Exited;
            c.vm_stopped = true;
            c.restart_pending = true;
        }
        // Stopping (the VM being put to sleep by Stop), then stopped: no
        // policy start boots a VM or takes the pending restart.
        for (parking, stopped) in [(true, false), (false, true)] {
            engine.parking.store(parking, std::sync::atomic::Ordering::SeqCst);
            engine.stopped.store(stopped, std::sync::atomic::Ordering::SeqCst);
            for reason in [Start::Restart, Start::Held, Start::DaemonRestart, Start::VmRecovery] {
                let r = engine.start_inner(&id, reason);
                assert!(matches!(r, Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock), "{r:?}");
            }
        }
        engine.stopped.store(false, std::sync::atomic::Ordering::SeqCst);
        let st = engine.lock();
        assert_eq!(st.containers[&id].status, Status::Exited, "nothing starts while Kiri stops");
        assert!(st.containers[&id].restart_pending, "the restart stays pending, for Start to bring back");
        drop(st);
        engine.parking.store(false, std::sync::atomic::Ordering::SeqCst);
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn vm_loss_recovers_running_policies_but_preserves_user_stops() {
        let dir = std::env::temp_dir().join(format!("kiri-vm-recovery-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let engine = Engine::boot(store).unwrap();
        // Prevent any real VM boot: this test supplies the guest events.
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let image = Image { mirror: None,
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let cases = [
            ("always-live", "always", Status::Running, false),
            ("unless-live", "unless-stopped", Status::Running, false),
            ("always-stopped", "always", Status::Exited, true),
            ("unless-stopped", "unless-stopped", Status::Exited, true),
            ("always-created", "always", Status::Created, false),
            ("unless-created", "unless-stopped", Status::Created, false),
            ("no-live", "no", Status::Running, false),
            ("failure-live", "on-failure", Status::Running, false),
        ];
        let mut ids = HashMap::new();
        for (name, policy, status, stopped_by_user) in cases {
            let spec: Spec = serde_json::from_value(serde_json::json!({
                "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["sleep", "60"],
                "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
                "restart": {"name": policy, "max_retries": 0}
            })).unwrap();
            let id = engine.create(Some(name.into()), spec, &image).unwrap();
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.status = status;
            c.stopped_by_user = stopped_by_user;
            c.started = (status != Status::Created).then(SystemTime::now);
            ids.insert(name, id);
        }
        let lost_at = std::time::Instant::now();
        *engine.k8s_routes.lock().unwrap() = Some((1, [10, 99, 0, 2]));
        engine.on_event(Event::Lost);
        let accounted_at = std::time::Instant::now();
        assert!(engine.k8s_routes.lock().unwrap().is_none(), "the replacement VM must install fresh Kubernetes routes");
        // A VM preparation wait must not own a container's lifecycle.
        // The stopped engine prevents this synthetic test booting a VM.
        let link = engine.link.lock().unwrap();
        let starting_engine = engine.clone();
        let starting_id = ids["always-live"].clone();
        let start = std::thread::spawn(move || starting_engine.start(&starting_id));
        std::thread::sleep(Duration::from_millis(50));
        let stopping_engine = engine.clone();
        let stopping_id = ids["always-live"].clone();
        let (sent, received) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || sent.send(stopping_engine.stop(&stopping_id, Duration::ZERO)).unwrap());
        let canceled_during_preparation = received.recv_timeout(Duration::from_millis(200)).is_ok();
        drop(link);
        let started = start.join().unwrap();
        stop.join().unwrap();
        assert!(canceled_during_preparation, "stop must finish while VM preparation is blocked");
        assert!(matches!(started, Err(Error::Conflict(_))), "a newer manual stop invalidates the prepared user start");
        // Undo that stop, back to what the crash left: not the user's, pending.
        {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&ids["always-live"]).unwrap();
            c.stopped_by_user = false;
            c.restart_pending = true;
        }
        // A start can finish reserving its address after Lost has already
        // reclaimed the previous run. Its failure must reclaim that too.
        let ipam = super::super::network::parse_ipam(&serde_json::json!({"Config": [{"Subnet": "10.99.0.0/24"}]})).unwrap();
        let net = engine.lock_networks().create("late-start", super::super::network::Settings { ipam, ..Default::default() }).unwrap();
        let mut late_ip = net.subnet;
        late_ip[3] = 42;
        engine.lock_networks().reserve("late-start", late_ip).unwrap();
        let late_spec = {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&ids["always-live"]).unwrap();
            c.spec.network_mode = "late-start".into();
            c.ip = Some(late_ip);
            c.spec.clone()
        };
        engine.fail_start(&ids["always-live"], &late_spec, &"the VM stopped");
        assert_eq!(engine.with(&ids["always-live"], |c| c.exit_code).unwrap(), 255);
        engine.lock_networks().reserve("late-start", late_ip).expect("failed recovery must release a late fixed-IP reservation");
        engine.lock_networks().release("late-start", late_ip);
        let retry_at = engine.lock().vm_recovery_at.expect("a VM loss schedules recovery with backoff");
        assert!(retry_at >= lost_at + RESTART_BASE);
        assert!(retry_at <= accounted_at + RESTART_BASE);
        let mut wanted = engine.vm_restart_candidates();
        wanted.sort();
        let mut expected = vec![ids["always-live"].clone(), ids["unless-live"].clone()];
        expected.sort();
        assert_eq!(wanted, expected);
        let saved: Vec<Saved> = serde_json::from_slice(&std::fs::read(&engine.records).unwrap()).unwrap();
        let live = saved.iter().find(|c| c.name == "always-live").unwrap();
        assert_eq!(live.status, Status::Exited);
        assert_eq!(live.exit_code, 255);
        assert!(live.vm_stopped);
        assert_eq!(live.restart_count, 1, "a VM recovery counts as a restart");
        assert_eq!(saved.iter().find(|c| c.name == "unless-live").unwrap().restart_count, 1);
        for name in ["always-stopped", "unless-stopped", "always-created", "unless-created", "no-live", "failure-live"] {
            assert_eq!(saved.iter().find(|c| c.name == name).unwrap().restart_count, 0, "{name} must not restart");
        }
        // A replacement VM can die before any container has started.
        let before = std::time::Instant::now();
        engine.on_event(Event::Lost);
        let after = std::time::Instant::now();
        {
            let st = engine.lock();
            assert_eq!(st.containers[&ids["always-live"]].restart_count, 2);
            let at = st.vm_recovery_at.unwrap();
            assert!(at >= before + Duration::from_millis(200) && at <= after + Duration::from_millis(200));
        }
        // A successful request-driven boot while the worker is still
        // unwinding a failure must leave a retry for that worker to see.
        let generation = {
            let mut st = engine.lock();
            st.vm_recovery_running = true;
            st.vm_recovery_generation
        };
        engine.stopped.store(false, std::sync::atomic::Ordering::SeqCst);
        engine.recover_vm();
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let mut st = engine.lock();
            assert!(st.vm_recovery_generation > generation, "a boot racing an active recovery must not lose its retry");
            st.vm_recovery_running = false;
        }
        // Crashes in a row back off, doubling to a minute; a VM that ran
        // VM_STABLE first starts over. RestartCount counts every one.
        let stable = std::time::Instant::now().checked_sub(VM_STABLE + Duration::from_secs(1));
        for (previous, booted, delay) in [(1, None, Duration::from_millis(200)), (2, None, Duration::from_millis(400)), (10, None, Duration::from_secs(60)), (u32::MAX, None, Duration::from_secs(60)), (10, stable, RESTART_BASE)] {
            {
                let mut st = engine.lock();
                st.vm_crashes = previous;
                st.vm_booted_at = booted;
                for name in ["always-live", "unless-live"] {
                    let c = st.containers.get_mut(&ids[name]).unwrap();
                    c.status = Status::Running;
                    c.restart_count = previous;
                }
            }
            let before = std::time::Instant::now();
            engine.on_event(Event::Lost);
            let after = std::time::Instant::now();
            let st = engine.lock();
            let at = st.vm_recovery_at.unwrap();
            assert!(at >= before + delay && at <= after + delay);
            assert_eq!(st.containers[&ids["always-live"]].restart_count, previous.saturating_add(1));
        }
        // A stop that arrives between the crash and the next boot wins.
        let lifecycle = engine.lifecycle(&ids["unless-live"]).unwrap();
        let starting = lifecycle.lock().unwrap();
        let stop_engine = engine.clone();
        let stop_id = ids["unless-live"].clone();
        let (sent, received) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || {
            sent.send(stop_engine.stop(&stop_id, Duration::ZERO)).unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err(), "stop must wait for an in-flight start");
        drop(starting);
        received.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        stop.join().unwrap();
        assert_eq!(engine.vm_restart_candidates(), vec![ids["always-live"].clone()]);
        engine.start_inner(&ids["unless-live"], Start::VmRecovery).unwrap();
        engine.start_inner(&ids["unless-live"], Start::Restart).unwrap();
        assert_eq!(engine.lock().containers[&ids["unless-live"]].status, Status::Exited);
        assert!(engine.lock().containers[&ids["unless-live"]].stopped_by_user);
        assert!(matches!(engine.kill(&ids["always-live"], libc::SIGUSR1 as u32), Err(Error::Conflict(_))));
        engine.kill(&ids["always-live"], libc::SIGTERM as u32).expect("a terminal kill cancels a pending VM recovery");
        engine.on_event(Event::Lost);
        assert!(engine.vm_restart_candidates().is_empty());
        assert!(engine.lock().vm_recovery_at.is_none(), "no eligible containers means no automatic boot");
        let mut spec = engine.lock().containers[&ids["always-live"]].spec.clone();
        spec.auto_remove = true;
        spec.restart.name = "no".into();
        let failed = engine.create(Some("failed-auto-remove".into()), spec, &image).unwrap();
        let waiting = engine.clone();
        let waiting_id = failed.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || sent.send(waiting.wait(&waiting_id, "removed")).unwrap());
        assert!(matches!(engine.start(&failed), Err(Error::Io(_))));
        assert!(engine.resolve(&failed).is_err(), "a failed boot must remove a --rm container and wake its waiter");
        assert!(matches!(received.recv_timeout(Duration::from_secs(2)).unwrap(), Ok(128) | Err(Error::NotFound(_))));
        waiter.join().unwrap();
        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
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
            ["netns=abc", "mac=02:42:ac:11:00:09", "nameserver=8.8.8.8", "nameserver=1.1.1.1", "search=corp.example", "option=ndots:2", "domainname=example.com"]
        );
        // Records saved before these settings existed still load.
        let old: super::Spec = serde_json::from_value(serde_json::json!({
            "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": ["sh"], "env": [], "cwd": "", "user": "",
            "auto_remove": false, "network_mode": "bridge", "labels": {},
        }))
        .unwrap();
        assert!(super::net_opts(&old, None, "").is_empty());
    }

    /// The cap on `--dns` servers counts IPv4 ones: IPv6 servers ahead of
    /// them don't push an IPv4 server out, and a fourth IPv4 one is left out.
    #[test]
    fn the_first_three_ipv4_dns_servers_are_kept() {
        let spec: super::Spec = serde_json::from_value(serde_json::json!({
            "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": ["sh"], "env": [], "cwd": "", "user": "",
            "auto_remove": false, "network_mode": "bridge", "labels": {},
            "dns": ["2001:db8::1", "2001:db8::2", "2001:db8::3", "10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4"],
        }))
        .unwrap();
        assert_eq!(
            super::net_opts(&spec, None, ""),
            [
                "nameserver=2001:db8::1",
                "nameserver=2001:db8::2",
                "nameserver=2001:db8::3",
                "nameserver=10.0.0.1",
                "nameserver=10.0.0.2",
                "nameserver=10.0.0.3"
            ]
        );
    }

    /// IPv6 `--dns` entries are bounded to three as well.
    #[test]
    fn at_most_three_ipv6_dns_servers_are_kept() {
        let spec: super::Spec = serde_json::from_value(serde_json::json!({
            "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": ["sh"], "env": [], "cwd": "", "user": "",
            "auto_remove": false, "network_mode": "bridge", "labels": {},
            "dns": ["2001:db8::1", "2001:db8::2", "2001:db8::3", "2001:db8::4", "10.0.0.1"],
        }))
        .unwrap();
        assert_eq!(
            super::net_opts(&spec, None, ""),
            ["nameserver=2001:db8::1", "nameserver=2001:db8::2", "nameserver=2001:db8::3", "nameserver=10.0.0.1"]
        );
    }
}

/// A passed-through USB device keeps the VM awake: sleeping would unplug it.
#[cfg(target_os = "macos")]
fn usb_attached() -> bool {
    super::usb::any_attached()
}

#[cfg(not(target_os = "macos"))]
fn usb_attached() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn detach_usb() {
    super::usb::detach_all();
}

#[cfg(not(target_os = "macos"))]
fn detach_usb() {}

#[cfg(test)]
mod vm_end_tests {
    use super::{VmEnd, vm_end};

    /// A stop that was asked for (the daemon's shutdown, `systemctl stop
    /// kiri`) is no error; only a VM that ended on its own is.
    #[test]
    fn only_an_unrequested_vm_end_is_an_error() {
        assert_eq!(vm_end(false, true), VmEnd::ShutDown);
        assert_eq!(vm_end(true, false), VmEnd::Slept);
        assert_eq!(vm_end(true, true), VmEnd::Slept);
        assert_eq!(vm_end(false, false), VmEnd::Crashed);
    }
}

#[cfg(test)]
mod start_exit_tests {
    #[test]
    fn a_failed_start_gets_dockers_exit_code() {
        let io = |m: &str| format!("{:?}", super::Error::Io(std::io::Error::other(m.to_owned())));
        assert_eq!(super::start_exit_code(&io("dns: No such file or directory (os error 2)")), 127);
        assert_eq!(super::start_exit_code(&io("exec: \"nope\": executable file not found in $PATH")), 127);
        assert_eq!(super::start_exit_code(&io("mount etc/hosts: mount /proc/self/fd/28: Not a directory (os error 20)")), 127);
        assert_eq!(super::start_exit_code(&io("exec /app: Permission denied (os error 13)")), 126);
        assert_eq!(super::start_exit_code(&format!("{:?}", super::Error::Conflict("no free address left on network n".into()))), 128);
        assert_eq!(super::start_exit_code(&format!("{:?}", super::Error::NotFound("network x not found".into()))), 128);
    }
}

/// An engine that never boots a VM, for tests outside this file.
#[cfg(test)]
pub fn boot_without_vm(root: &std::path::Path) -> Arc<Engine> {
    let engine = Engine::boot(Store::open(root).unwrap()).unwrap();
    engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
    engine
}

#[cfg(test)]
mod records_tests {
    use super::*;
    use std::path::Path;

    fn image() -> Image {
        Image { mirror: None,
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        }
    }

    fn boot(root: &Path) -> Arc<Engine> {
        let engine = Engine::boot(Store::open(root).unwrap()).unwrap();
        // Nothing here may boot a real VM.
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        engine
    }

    fn listed(engine: &Engine) -> Vec<String> {
        let mut ids: Vec<String> = engine.lock().containers.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// A store with three stopped containers, each with a log folder, and a
    /// log folder no record names. Returns the root, the ids in the file's
    /// order and the good file's bytes.
    fn fixture(name: &str) -> (PathBuf, Vec<String>, Vec<u8>) {
        let root = std::env::temp_dir().join(format!("kiri-records-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let engine = boot(&root);
        for n in ["c1", "c2", "c3"] {
            let spec: Spec = serde_json::from_value(serde_json::json!({
                "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["true"],
                "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
                "restart": {"name": "no", "max_retries": 0}
            })).unwrap();
            let id = engine.create(Some(n.into()), spec, &image()).unwrap();
            engine.lock().containers.get_mut(&id).unwrap().status = Status::Exited;
        }
        engine.save();
        drop(engine);
        let bytes = std::fs::read(root.join("containers.json")).unwrap();
        let saved: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let ids: Vec<String> = saved.iter().map(|s| s["id"].as_str().unwrap().to_owned()).collect();
        for id in ids.iter().map(String::as_str).chain(["orphan"]) {
            let d = super::super::logfile::dir(&root, id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("log"), "line-one\n").unwrap();
        }
        (root, ids, bytes)
    }

    fn logs_kept(root: &Path, ids: &[String]) -> bool {
        ids.iter().map(String::as_str).chain(["orphan"]).all(|id| super::super::logfile::dir(root, id).join("log").exists())
    }

    /// Where each record ends in a file written by `save`: `[r1,r2,r3]`.
    fn record_ends(bytes: &[u8]) -> Vec<usize> {
        let saved: Vec<serde_json::Value> = serde_json::from_slice(bytes).unwrap();
        let mut at = 1;
        saved.iter().map(|r| { at += serde_json::to_vec(r).unwrap().len() + 1; at - 1 }).collect()
    }

    #[test]
    fn a_good_file_round_trips_and_tidies_up() {
        let (root, ids, _) = fixture("good");
        let engine = boot(&root);
        let mut want = ids.clone();
        want.sort();
        assert_eq!(listed(&engine), want);
        let mut names: Vec<String> = ids.iter().map(|id| engine.lock().containers[id].name.clone()).collect();
        names.sort();
        assert_eq!(names, ["c1", "c2", "c3"]);
        assert!(ids.iter().all(|id| engine.lock().containers[id].status == Status::Exited));
        assert!(!engine.keeps_everything());
        assert!(engine.records_damage().is_empty());
        assert!(kiri_image::jsonfile::kept_copies(&engine.records).unwrap().is_empty());
        // A log folder no record names is a container that is gone.
        assert!(!super::super::logfile::dir(&root, "orphan").exists());
        assert!(ids.iter().all(|id| super::super::logfile::dir(&root, id).exists()));
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    fn names_on_disk(root: &Path) -> Vec<String> {
        let saved: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(root.join("containers.json")).unwrap()).unwrap();
        let mut names: Vec<String> = saved.iter().map(|s| s["name"].as_str().unwrap().to_owned()).collect();
        names.sort();
        names
    }

    /// One message, true in each state, listing the copies once, never
    /// advising to put a list back, and saying how the engine itself is
    /// restarted here.
    /// A repaired image list is shown in `docker info` with its copy and the
    /// images it lost (#440).
    #[test]
    fn a_repaired_image_list_is_a_warning() {
        use kiri_image::store::IndexDamage;
        let root = std::path::Path::new("/s");
        assert_eq!(image_damage_message(&Ok(None), root), None);
        let one = Ok(Some(IndexDamage { copies: vec!["/s/images.json.corrupt-1-0".into()], missing: vec!["local/img:1".into(), "web:2".into()] }));
        let m = image_damage_message(&one, root).unwrap();
        for want in ["image list was damaged", "a copy of it is kept at /s/images.json.corrupt-1-0", "2 image(s)", "local/img:1, web:2", "keeps every unpacked layer until the copy is removed"] {
            assert!(m.contains(want), "{want:?} in {m}");
        }
        let back = Ok(Some(IndexDamage { copies: vec!["/s/a".into(), "/s/b".into()], missing: Vec::new() }));
        let m = image_damage_message(&back, root).unwrap();
        assert!(m.contains("copies are kept at /s/a, /s/b") && m.contains("listed again") && m.contains("until the copies are removed"), "{m}");
        let unlistable = Err(io::Error::other("denied"));
        assert!(image_damage_message(&unlistable, root).unwrap().contains("cannot read the image list in /s (denied)"));
    }

    #[test]
    fn the_warning_says_what_happened_and_what_ends_it() {
        let records = Path::new("/s/containers.json");
        let restart = restart_hint();
        assert_eq!(Damage::default().message(records), None);
        let refused = "refuses volume, image and network prunes and removals of volumes and images without --force";
        let copies = Damage { copies: vec!["/s/containers.json.corrupt-1-0".into(), "/s/containers.json.corrupt-2-0".into()], ..Default::default() };
        let m = copies.message(records).unwrap();
        for want in ["copies are kept at /s/containers.json.corrupt-1-0, /s/containers.json.corrupt-2-0", "cannot be listed again", "their volumes are still there", "files and logs are kept on disk", refused, "until the copies are removed and the engine itself is restarted", &restart, "removes the files and logs of containers not in the list"] {
            assert!(m.contains(want), "{want:?} in {m}");
        }
        assert_eq!(m.matches("corrupt-1-0").count(), 1, "each copy once: {m}");
        assert!(!m.contains("back"), "no advice to put a list back: {m}");
        let unsaved = Damage { unsaved: Some("Permission denied".into()), ..Default::default() };
        let m = unsaved.message(records).unwrap();
        assert!(m.contains("changes to containers are not saved until Kiri can write to that folder") && m.contains("Permission denied") && m.contains("until a copy can be kept") && !m.contains("tidies up"), "{m}");
        let unlistable = Damage { unlistable: Some("Permission denied".into()), ..Default::default() };
        let m = unlistable.message(records).unwrap();
        assert!(m.contains("cannot list /s") && m.contains("until that folder can be listed and the engine itself is restarted") && !m.contains("cannot be listed again"), "{m}");
        if cfg!(target_os = "macos") {
            assert!(restart.contains("launchctl kickstart -k"), "{restart}");
        }
    }

    /// The reviewer's sequence: damage, start, a person moves another list
    /// onto containers.json while the engine runs, a save happens, the
    /// engine starts again. The person's file is kept as a copy, everything
    /// is still kept, and nothing is swept.
    #[test]
    fn a_list_put_back_while_the_engine_runs_is_kept_as_a_copy() {
        let (root, ids, good) = fixture("putback");
        let file = root.join("containers.json");
        std::fs::write(&file, &good[..good.len() / 2]).unwrap();
        let engine = boot(&root);
        assert!(engine.keeps_everything());
        assert_eq!(kiri_image::jsonfile::kept_copies(&file).unwrap().len(), 1);
        // Put back by hand: written beside it, moved over it.
        let theirs = root.join("theirs.json");
        std::fs::write(&theirs, &good).unwrap();
        std::fs::rename(&theirs, &file).unwrap();
        engine.save();
        let copies = kiri_image::jsonfile::kept_copies(&file).unwrap();
        assert_eq!(copies.len(), 2, "the file put back is kept as a copy");
        assert!(copies.iter().any(|c| std::fs::read(c).unwrap() == good), "with its content");
        // A save of the engine's own file keeps nothing more.
        engine.save();
        assert_eq!(kiri_image::jsonfile::kept_copies(&file).unwrap().len(), 2);
        drop(engine);
        let engine = boot(&root);
        assert!(engine.keeps_everything());
        assert!(logs_kept(&root, &ids), "nothing swept at the next start");
        assert_eq!(engine.guest_keeps(), None);
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_guest_deletes_nothing_while_the_list_is_damaged() {
        let (root, ids, good) = fixture("guest");
        let engine = boot(&root);
        let mut keep = engine.guest_keeps().expect("a good list: the guest tidies up");
        keep.sort();
        let mut want = ids.clone();
        want.sort();
        assert_eq!(keep, want);
        drop(engine);
        std::fs::write(root.join("containers.json"), &good[..good.len() / 2]).unwrap();
        let engine = boot(&root);
        assert_eq!(engine.guest_keeps(), None, "a damaged list: the guest keeps every folder and layer");
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_temporary_files_go_at_start() {
        let (root, _, _) = fixture("temps");
        let stale = root.join("containers.json.4242.7.tmp");
        std::fs::write(&stale, b"[").unwrap();
        let engine = boot(&root);
        assert!(!stale.exists());
        assert!(!engine.keeps_everything());
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// No list file, but copies beside it: an empty list with everything
    /// kept, never the copies' containers (an old copy would bring back
    /// containers removed long ago).
    #[test]
    fn with_no_list_but_a_copy_nothing_is_loaded_and_nothing_deleted() {
        let (root, ids, good) = fixture("copyonly");
        let file = root.join("containers.json");
        std::fs::write(root.join("containers.json.corrupt-100-0"), &good).unwrap();
        std::fs::remove_file(&file).unwrap();
        let engine = boot(&root);
        assert!(listed(&engine).is_empty(), "the copy is not loaded");
        assert!(engine.keeps_everything());
        assert!(logs_kept(&root, &ids));
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Damage found at this start keeps everything even when the folder
    /// cannot be listed (mode 0300: entries can be made and renamed, not
    /// read), and a folder that cannot be listed counts as damage itself.
    #[cfg(unix)]
    #[test]
    fn an_unlistable_folder_still_keeps_everything() {
        use std::os::unix::fs::PermissionsExt;
        let (root, ids, good) = fixture("unlistable");
        let file = root.join("containers.json");
        let mode = |m: u32| std::fs::set_permissions(&root, std::fs::Permissions::from_mode(m)).unwrap();
        mode(0o300);
        if std::fs::read_dir(&root).is_ok() {
            mode(0o755);
            eprintln!("skipped: permissions do not apply to this user");
            return;
        }
        // (b) a good list, but no way to look for copies: damage.
        let engine = boot(&root);
        assert!(engine.keeps_everything(), "an unlistable folder counts as damage");
        drop(engine);
        // (a) a damaged list moved aside at this very start.
        std::fs::write(&file, &good[..10]).unwrap();
        let engine = boot(&root);
        assert!(engine.keeps_everything());
        assert!(engine.records_damage().iter().any(|w| w.contains("containers.json.corrupt-")), "{:?}", engine.records_damage());
        drop(engine);
        mode(0o755);
        assert!(logs_kept(&root, &ids));
        let _ = std::fs::remove_dir_all(root);
    }

    /// A folder that refuses changes at the first start (mode 0500): the
    /// damaged list can be neither moved nor copied. Nothing replaces it
    /// until a copy is kept; once the folder takes changes again, the next
    /// save keeps the copy first, then writes the list.
    #[cfg(unix)]
    #[test]
    fn a_damaged_list_is_never_replaced_before_a_copy_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let (root, ids, good) = fixture("readonly");
        let ends = record_ends(&good);
        let file = root.join("containers.json");
        let damaged = good[..ends[0] + 5].to_vec();
        std::fs::write(&file, &damaged).unwrap();
        let mode = |m: u32| std::fs::set_permissions(&root, std::fs::Permissions::from_mode(m)).unwrap();
        mode(0o500);
        if std::fs::write(root.join("probe"), b"").is_ok() {
            mode(0o755);
            eprintln!("skipped: permissions do not apply to this user");
            return;
        }
        let engine = boot(&root);
        assert_eq!(listed(&engine), vec![ids[0].clone()], "the readable record is loaded");
        assert!(engine.keeps_everything());
        engine.lock().containers.get_mut(&ids[0]).unwrap().name = "renamed".into();
        engine.save();
        assert_eq!(std::fs::read(&file).unwrap(), damaged, "no save replaces the only copy");
        mode(0o755);
        engine.save();
        let copies = kiri_image::jsonfile::kept_copies(&file).unwrap();
        assert_eq!(copies.len(), 1);
        assert_eq!(std::fs::read(&copies[0]).unwrap(), damaged, "the copy holds the damaged list");
        assert_eq!(names_on_disk(&root), ["renamed"], "then the list is written");
        assert!(engine.records_damage().iter().any(|w| w.contains(&*copies[0].to_string_lossy())));
        assert!(logs_kept(&root, &ids));
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    /// The list on disk is current as soon as save returns, with the sync
    /// left to the syncer thread, and also once that thread is gone (as
    /// after shutdown, which is not called here: it ends every engine in
    /// the process).
    #[test]
    fn saves_are_current_at_once_with_or_without_the_syncer() {
        let (root, ids, _) = fixture("syncer");
        let engine = boot(&root);
        let names = || -> Vec<String> {
            let saved: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(root.join("containers.json")).unwrap()).unwrap();
            saved.iter().map(|s| s["name"].as_str().unwrap().to_owned()).collect()
        };
        assert!(engine.syncer.lock().unwrap().is_some(), "the syncer runs");
        engine.lock().containers.get_mut(&ids[0]).unwrap().name = "renamed".into();
        engine.save();
        assert!(names().contains(&"renamed".to_owned()));
        engine.syncer.lock().unwrap().take();
        engine.lock().containers.get_mut(&ids[0]).unwrap().name = "after".into();
        engine.save();
        assert!(names().contains(&"after".to_owned()));
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_damaged_file_is_kept_recovered_and_deletes_nothing() {
        let (root, ids, good) = fixture("damaged");
        let ends = record_ends(&good);
        let file = root.join("containers.json");
        // (bytes, how many records can still be read)
        let cases: Vec<(Vec<u8>, usize)> = vec![
            (Vec::new(), 0),
            (good[..10].to_vec(), 0),
            (good[..ends[0]].to_vec(), 1),
            (good[..ends[0] + 7].to_vec(), 1),
            (good[..ends[1] + 1].to_vec(), 2),
            (good[..good.len() / 2].to_vec(), ends.iter().filter(|e| **e <= good.len() / 2).count()),
            (good[..good.len() - 1].to_vec(), 3),
            (b"\0\0\0garbage".to_vec(), 0),
        ];
        for (bytes, recovered) in cases {
            std::fs::write(&file, &bytes).unwrap();
            let engine = boot(&root);
            let copies = kiri_image::jsonfile::kept_copies(&file).unwrap();
            assert_eq!(copies.len(), 1, "one copy kept");
            assert_eq!(std::fs::read(&copies[0]).unwrap(), bytes, "the copy holds the damaged bytes");
            let mut want = ids[..recovered].to_vec();
            want.sort();
            assert_eq!(listed(&engine), want, "cut at {}: the complete leading records are loaded", bytes.len());
            assert!(engine.keeps_everything());
            let damage = engine.records_damage();
            assert!(damage.iter().any(|w| w.contains(&*copies[0].to_string_lossy())), "{damage:?}");
            assert!(logs_kept(&root, &ids), "no log folder is deleted");
            // What was recovered is written back, readable.
            let rewritten: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            assert_eq!(rewritten.len(), recovered);
            drop(engine);

            // The next start reads a good, shorter list: still nothing goes,
            // since the copy says containers may be missing from it.
            let engine = boot(&root);
            assert!(engine.keeps_everything());
            assert!(logs_kept(&root, &ids), "no log folder is deleted at the next clean start");
            drop(engine);
            std::fs::remove_file(&copies[0]).unwrap();
        }
        // With the copies removed, the engine tidies up again.
        std::fs::write(&file, &good).unwrap();
        let engine = boot(&root);
        assert!(!engine.keeps_everything());
        assert!(!super::super::logfile::dir(&root, "orphan").exists());
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod image_labels_cache_tests {
    use super::*;

    fn put(store: &Store, name: &str, labels: serde_json::Value) -> String {
        let cfg = serde_json::json!({"created": "2026-11-16T00:00:00Z", "architecture": "arm64", "os": "linux", "config": {"Labels": labels}});
        let mut w = store.ingest().unwrap();
        io::Write::write_all(&mut w, &serde_json::to_vec(&cfg).unwrap()).unwrap();
        let (digest, _) = w.finish().unwrap();
        store.put_image(kiri_image::archive::from_config(name.into(), &digest, &cfg, Vec::new())).unwrap();
        digest
    }

    /// A listing reads each image's config once; later ones use what was
    /// read, a removed image's entry goes, and a new image is read.
    #[test]
    fn image_labels_are_read_once_and_follow_the_image_list() {
        let root = std::env::temp_dir().join(format!("kiri-label-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let a = put(&store, "demo:a", serde_json::json!({"k": "a"}));
        put(&store, "demo:b", serde_json::json!({}));
        let engine = Engine::boot(store).unwrap();
        let labels = |e: &Engine| e.image_labels(&e.store.images().unwrap());
        assert_eq!(labels(&engine), vec![serde_json::json!({"k": "a"}), serde_json::json!({})]);

        // Read from memory now: gone from disk, the labels are still there.
        std::fs::remove_file(engine.store.blob_path(&a).unwrap()).unwrap();
        assert_eq!(labels(&engine)[0], serde_json::json!({"k": "a"}));

        engine.store.remove_image("demo:a").unwrap();
        let c = put(&engine.store, "demo:c", serde_json::json!({"k": "c"}));
        assert_eq!(labels(&engine), vec![serde_json::json!({}), serde_json::json!({"k": "c"})]);
        let cache = engine.image_labels.lock().unwrap();
        assert!(!cache.contains_key(&a), "a removed image's labels are dropped");
        assert!(cache.contains_key(&c));
        drop(cache);
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod save_speed_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Many saves back to back from several threads (a compose up, a burst
    /// of exits) never wait for the disk: no save syncs the file itself
    /// (an fsync, 5 ms or more on a Mac's disk, so a save that waits for one
    /// is many times slower than the write), and the syncer thread syncs
    /// once per quiet moment, at most once a second during the burst, not
    /// once per save. Counted, not timed, so neither a busy machine nor a
    /// disk where syncing is free (tmpfs) changes the answer.
    #[test]
    fn back_to_back_saves_leave_syncing_to_the_syncer() {
        let root = std::env::temp_dir().join(format!("kiri-save-speed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let engine = Engine::boot(Store::open(&root).unwrap()).unwrap();
        engine.stopped.store(true, Ordering::SeqCst);
        let image = Image { mirror: None,
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        for i in 0..20 {
            let spec: Spec = serde_json::from_value(serde_json::json!({
                "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["true"],
                "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
                "restart": {"name": "no", "max_retries": 0}
            })).unwrap();
            engine.create(Some(format!("speed{i}")), spec, &image).unwrap();
        }
        // The creates' own sync, once it has happened: the burst starts quiet.
        let quiet = |n: u64| {
            let t = std::time::Instant::now();
            while engine.syncer_syncs.load(Ordering::SeqCst) == n && t.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(10));
            }
            engine.syncer_syncs.load(Ordering::SeqCst)
        };
        let before = quiet(0);
        let (threads, each) = (8, 100);
        let t = std::time::Instant::now();
        let inline: Vec<u64> = std::thread::scope(|s| {
            let saving: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| {
                        let start = kiri_image::jsonfile::syncs_here();
                        (0..each).for_each(|_| engine.save());
                        kiri_image::jsonfile::syncs_here() - start
                    })
                })
                .collect();
            saving.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let took = t.elapsed();
        let during = engine.syncer_syncs.load(Ordering::SeqCst) - before;
        eprintln!("{} saves from {threads} threads in {took:?}: synced by the saves {inline:?}, by the syncer {during}", threads * each);
        assert!(inline.iter().all(|&n| n == 0), "saves synced the file themselves: {inline:?} syncs per thread");
        // At most one a second while saves keep coming (`SYNC_AT_MOST`), one
        // more for a gap, however slow a busy machine makes the burst.
        assert!(during <= took.as_secs() + 2, "the syncer synced {during} times during {} saves in {took:?}", threads * each);
        // And the last save is made durable soon after the burst.
        assert!(quiet(before + during) > before + during, "the syncer never synced after the burst");
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod start_gate_tests {
    use super::StartGate;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn an_exec_during_a_start_waits_for_it_and_no_longer() {
        let gate = Arc::new(StartGate::default());
        // Open: no wait at all.
        let t = Instant::now();
        gate.wait();
        assert!(t.elapsed() < Duration::from_millis(50));
        // Shut by a start: a waiter goes on only once the start is done,
        // however it ended (the guard opens it when dropped).
        let guard = gate.shut();
        let waiter = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                let t = Instant::now();
                gate.wait();
                t.elapsed()
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "an exec waits while the start is under way");
        drop(guard);
        let waited = waiter.join().unwrap();
        assert!(waited >= Duration::from_millis(90) && waited < Duration::from_secs(5), "waited {waited:?}");
        let t = Instant::now();
        gate.wait();
        assert!(t.elapsed() < Duration::from_millis(50), "open again");
    }
}

#[cfg(test)]
mod start_wait_tests {
    use super::*;

    /// An engine that never boots a VM, with one container that reads as
    /// Running, as during its start.
    fn engine(name: &str) -> (Arc<Engine>, String, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kiri-start-wait-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::boot(Store::open(&dir).unwrap()).unwrap();
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let image = Image { mirror: None,
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["sleep", "60"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "restart": {"name": "no", "max_retries": 0}
        })).unwrap();
        let id = engine.create(Some(name.into()), spec, &image).unwrap();
        engine.lock().containers.get_mut(&id).unwrap().status = Status::Running;
        (engine, id, dir)
    }

    /// `--pid container:` needs a running target other than itself, as
    /// with Docker, and such a container starts after the others.
    #[test]
    fn pid_container_must_be_another_running_one() {
        let (engine, target, dir) = engine("pid-target");
        assert_eq!(engine.pid_container("me", "pid-target").unwrap(), target);
        assert!(matches!(engine.pid_container(&target, &target), Err(Error::BadRequest(m)) if m == "cannot join own PID namespace"));
        assert!(matches!(engine.pid_container("me", "nosuch"), Err(Error::NotFound(_))));
        let mut st = engine.lock();
        let c = st.containers.get_mut(&target).unwrap();
        assert!(!joins_others(c));
        c.spec.pid_mode = "container:x".into();
        assert!(joins_others(c), "starts after the one it joins");
        c.status = Status::Exited;
        drop(st);
        assert!(matches!(engine.pid_container("me", &target), Err(Error::Conflict(m)) if m == format!("cannot join PID of a non running container: {target}")));
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn shut(engine: &Engine, id: &str) -> StartGuard {
        engine.lock().containers.get_mut(id).unwrap().begin_start()
    }

    /// `docker pause` (and the pause `docker commit` makes first) during a
    /// start waits for it: a child frozen while it fills a new volume never
    /// finishes, and the start, the commit and every stop behind them would
    /// wait forever.
    #[test]
    fn a_pause_during_a_start_waits_for_it() {
        let (engine, id, dir) = engine("pause");
        for paused in [true, false] {
            let guard = shut(&engine, &id);
            let pausing = {
                let (engine, id) = (engine.clone(), id.clone());
                std::thread::spawn(move || engine.pause(&id, paused))
            };
            std::thread::sleep(Duration::from_millis(150));
            assert!(!pausing.is_finished(), "pause({paused}) went ahead during a start");
            drop(guard);
            // No VM here: it fails then, but only once the start is done.
            let _ = pausing.join().unwrap();
        }
        for (what, call) in [
            ("update", Box::new(|e: &Engine, id: &str| e.update(id, Limits::default(), None).map(|_| ())) as Box<dyn Fn(&Engine, &str) -> Result<(), Error> + Send>),
            ("connect", Box::new(|e: &Engine, id: &str| e.connect("bridge", id, Vec::new(), String::new(), String::new(), String::new()))),
            ("disconnect", Box::new(|e: &Engine, id: &str| e.disconnect("bridge", id))),
        ] {
            let guard = shut(&engine, &id);
            let calling = {
                let (engine, id) = (engine.clone(), id.clone());
                std::thread::spawn(move || call(&engine, &id))
            };
            std::thread::sleep(Duration::from_millis(150));
            assert!(!calling.is_finished(), "{what} went ahead during a start");
            drop(guard);
            let _ = calling.join().unwrap();
        }
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A copy sent while a start is under way looks the container up once
    /// the start is done: a `--rm` one whose start failed meanwhile is gone,
    /// and the copy must fail rather than write into a folder made again
    /// for a container that no longer exists.
    #[test]
    fn a_copy_during_a_start_looks_again_once_it_is_done() {
        let (engine, id, dir) = engine("copy");
        let guard = shut(&engine, &id);
        let copying = {
            let (engine, id) = (engine.clone(), id.clone());
            std::thread::spawn(move || engine.copy(&id, kiri_proto::copy_op::IN, "/", 0).map(|_| ()))
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(!copying.is_finished(), "a copy went ahead during a start");
        // The start fails and takes the `--rm` container with it.
        engine.lock().containers.remove(&id);
        drop(guard);
        let r = copying.join().unwrap();
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The exit of a container whose start waited for its root can be
    /// reported before the start ends: it is then not made live (the next
    /// run's early input would go straight to a guest with no process for
    /// it), and its `start` still comes before its `die`, once.
    #[test]
    fn an_exit_that_overtakes_its_start() {
        let (engine, id, dir) = engine("overtaken");
        let (_, events) = engine.subscribe();
        let actions = || events.try_iter().filter(|e| e["Actor"]["ID"] == id.as_str()).map(|e| e["Action"].as_str().unwrap_or("").to_owned()).collect::<Vec<_>>();
        // In order: live, and its `start` sent.
        engine.lock().containers.get_mut(&id).unwrap().start_owed = true;
        assert_eq!(engine.take_early(&id, 0), Early::Live { close: false });
        assert!(engine.lock().containers[&id].live);
        assert_eq!(actions(), ["start"]);
        // Its exit first, then the end of the start: no second `start`.
        {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.live = false;
            c.start_owed = true;
            c.early_input = b"early".to_vec();
            c.early_close = true;
        }
        let exits = engine.lock().containers[&id].exits;
        engine.on_event(Event::Exited { id: &id, code: 0, oom: false });
        assert_eq!(engine.take_early(&id, exits), Early::Gone);
        let st = engine.lock();
        let c = &st.containers[&id];
        assert!(!c.live, "an exited run is not made live again");
        assert!(c.early_input.is_empty() && !c.early_close, "its early input went with it");
        drop(st);
        assert_eq!(actions(), ["start", "die"]);
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An exec's output stream ends only once its exit code is recorded: on
    /// its exit, and when the VM stops under it. docker-java inspects the exec
    /// as soon as the stream closes and needs `ExitCode` then (#551).
    #[test]
    fn an_exec_has_its_exit_code_when_its_stream_ends() {
        let (engine, id, dir) = engine("exec-exit");
        let exec = |argv: &str| engine.exec_create(&id, ExecConfig { console_size: None, argv: vec![argv.into()], env: Vec::new(), cwd: "/".into(), user: String::new(), tty: false, stdin: false, privileged: false, internal: true }).unwrap();
        let attach = |exec_id: &str| {
            let (tx, rx) = mpsc::channel();
            let mut st = engine.lock();
            let e = st.execs.get_mut(exec_id).unwrap();
            e.running = true;
            e.attached.push(tx);
            rx
        };
        let state = |exec_id: &str| engine.with_exec(exec_id, |e| (e.running, e.exit_code)).unwrap();
        let (exited, lost, never) = (exec("exited"), exec("lost"), exec("never"));
        assert_eq!(state(&never), (false, None), "an exec not started has no exit code, as in Docker");
        let rx = attach(&exited);
        engine.on_event(Event::Exited { id: &exited, code: 3, oom: false });
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)), "its stream ended");
        assert_eq!(state(&exited), (false, Some(3)));
        let rx = attach(&lost);
        engine.on_event(Event::Lost);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)), "its stream ended with the VM");
        assert_eq!(state(&lost), (false, Some(255)));
        assert_eq!(state(&exited), (false, Some(3)), "a finished exec keeps its own code");
        assert_eq!(state(&never), (false, None));
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A container removed while a call waited for its start (a `--rm` one
    /// whose start failed, or that exited before the start returned) is
    /// not found by that call. It must not abort the daemon.
    #[test]
    fn a_container_removed_while_a_call_waited_for_its_start_is_not_found() {
        for what in ["pause", "update", "connect", "disconnect"] {
            let (engine, id, dir) = engine(&format!("removed-{what}"));
            let guard = shut(&engine, &id);
            let calling = {
                let (engine, id) = (engine.clone(), id.clone());
                std::thread::spawn(move || match what {
                    "pause" => engine.pause(&id, true),
                    "update" => engine.update(&id, Limits::default(), None),
                    "connect" => engine.connect("bridge", &id, Vec::new(), String::new(), String::new(), String::new()),
                    _ => engine.disconnect("bridge", &id),
                })
            };
            std::thread::sleep(Duration::from_millis(150));
            assert!(!calling.is_finished(), "{what} went ahead during a start");
            engine.lock().containers.remove(&id);
            drop(guard);
            let r = calling.join().unwrap_or_else(|_| panic!("{what} panicked"));
            assert!(matches!(r, Err(Error::NotFound(_))), "{what}: {r:?}");
            drop(engine);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// `docker rm -f` of a running `--rm` container: its exit, reported while
    /// the rm waits for the kill, removes it and notes its code. The rm must
    /// not note a second one (0, with no container left to read), or a
    /// `docker run --rm` waiting on it exits 0, not 137. A code the container
    /// exited with by itself as the kill landed (3) is kept the same way; a
    /// container without `--rm` is removed by the rm, with its code.
    #[test]
    fn a_force_remove_keeps_the_exit_code_of_a_container_that_removed_itself() {
        for (auto_remove, code) in [(true, 137), (true, 3), (false, 137)] {
            let (engine, id, dir) = engine(&format!("rm-code-{auto_remove}-{code}"));
            engine.lock().containers.get_mut(&id).unwrap().spec.auto_remove = auto_remove;
            let waiters: Vec<_> = ["removed", "next-exit", "not-running"]
                .into_iter()
                .map(|cond| {
                    let (engine, id) = (engine.clone(), id.clone());
                    (cond, std::thread::spawn(move || engine.wait(&id, cond)))
                })
                .collect();
            let removing = {
                let (engine, id) = (engine.clone(), id.clone());
                std::thread::spawn(move || engine.remove(&id, true, false))
            };
            // The rm now waits for the exit of what it killed.
            std::thread::sleep(Duration::from_millis(150));
            assert!(!removing.is_finished(), "the rm did not wait for the exit");
            engine.on_event(Event::Exited { id: &id, code, oom: false });
            let r = removing.join().unwrap();
            assert!(r.is_ok(), "rm -f: {r:?}");
            let notes: Vec<i32> = engine.lock().removed.iter().filter(|(r, _)| *r == id).map(|(_, c)| *c).collect();
            assert_eq!(notes, [code], "--rm {auto_remove}: one removal, with its real exit code");
            for (cond, w) in waiters {
                assert_eq!(w.join().unwrap().unwrap(), code, "wait {cond}, --rm {auto_remove}");
            }
            drop(engine);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A second container made with a volume (named or anonymous) that a
    /// start under way also uses waits for that start before it reaches the
    /// guest; one sharing no volume, or only a bind or tmpfs mount, does not.
    #[test]
    fn a_start_sharing_a_volume_waits_for_the_earlier_one() {
        let (engine, first, dir) = engine("shared-volume");
        let spec = |v: serde_json::Value| -> Spec {
            let mut s = engine.lock().containers[&first].spec.clone();
            s.volumes = v["named"].as_array().into_iter().flatten().map(|x| x.as_str().unwrap().to_owned()).collect();
            s.anon_volumes = v["anon"].as_array().into_iter().flatten().map(|x| x.as_str().unwrap().to_owned()).collect();
            s.mount_info = v["info"].as_array().cloned().unwrap_or_default();
            s
        };
        {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&first).unwrap();
            c.spec.volumes = vec!["data".into()];
            c.spec.anon_volumes = vec!["anon1".into()];
        }
        let guard = shut(&engine, &first);
        let gates = |spec: &Spec| shared_volume_gates(&engine.lock(), "other", spec);
        // Named, anonymous and `--mount` volumes are all found.
        for s in [spec(serde_json::json!({"anon": ["anon1"]})), spec(serde_json::json!({"info": [{"Type": "volume", "Name": "data"}]}))] {
            assert_eq!(gates(&s).len(), 1);
        }
        let found = gates(&spec(serde_json::json!({"named": ["data"]})));
        assert_eq!(found.len(), 1);
        let waiting = {
            let g = found[0].clone();
            std::thread::spawn(move || g.wait())
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiting.is_finished(), "a start sharing a volume goes ahead during the fill");
        drop(guard);
        waiting.join().unwrap();
        let guard = shut(&engine, &first);
        // No volume shared: nothing to wait for.
        assert!(gates(&spec(serde_json::json!({"named": ["other"]}))).is_empty());
        assert!(gates(&spec(serde_json::json!({}))).is_empty());
        // A bind or tmpfs mount is not a volume, even at the same place.
        assert!(gates(&spec(serde_json::json!({"info": [{"Type": "bind", "Name": "data", "Source": "data"}, {"Type": "tmpfs", "Destination": "/data"}]}))).is_empty());
        // The container itself is not collected, and an open gate is not.
        let own = spec(serde_json::json!({"named": ["data"]}));
        assert!(shared_volume_gates(&engine.lock(), &first, &own).is_empty());
        drop(guard);
        assert!(gates(&spec(serde_json::json!({"named": ["data"]}))).is_empty());
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What `start_on_vm` does under the engine lock for container `id`:
    /// begin its start, then collect the shut gates of others sharing a volume.
    fn begin(engine: &Engine, id: &str) -> (StartGuard, Vec<Arc<StartGate>>) {
        let mut st = engine.lock();
        let guard = st.containers.get_mut(id).unwrap().begin_start();
        let spec = st.containers[id].spec.clone();
        let earlier = shared_volume_gates(&st, id, &spec);
        (guard, earlier)
    }

    /// C fills v1 for a long time; B uses v2; A uses both.
    fn three(name: &str) -> (Arc<Engine>, [String; 3], PathBuf) {
        let (engine, c, dir) = engine(name);
        let image = Image { mirror: None,
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let base = engine.lock().containers[&c].spec.clone();
        let with = |vols: &[&str]| {
            let mut s = base.clone();
            s.volumes = vols.iter().map(|v| (*v).to_owned()).collect();
            s
        };
        engine.lock().containers.get_mut(&c).unwrap().spec = with(&["v1"]);
        let b = engine.create(Some(format!("{name}-b")), with(&["v2"]), &image).unwrap();
        let a = engine.create(Some(format!("{name}-a")), with(&["v1", "v2"]), &image).unwrap();
        (engine, [c, b, a], dir)
    }

    /// A waits for C's fill and for B's first start; B's first start ends
    /// and B starts again (restart policy, `docker restart`) while A still
    /// waits: B's second start waits for A, which began before it. Each
    /// start has a gate of its own, so A is not held by B's second start,
    /// and both end once C's fill is done. Waits are bounded: a hang shows
    /// as a failed assertion, not a stuck test.
    #[test]
    fn a_restarted_container_and_one_waiting_for_it_do_not_wait_for_each_other() {
        let (engine, [c, b, a], dir) = three("restart-cycle");
        let (guard_c, earlier_c) = begin(&engine, &c);
        assert!(earlier_c.is_empty());
        let (guard_b1, earlier_b1) = begin(&engine, &b);
        assert!(earlier_b1.is_empty(), "B shares nothing with C");
        let gate_c = engine.lock().containers[&c].start_gate.clone();
        let (guard_a, mut earlier_a) = begin(&engine, &a);
        assert_eq!(earlier_a.len(), 2, "A waits for C and for B");
        // The map's order is arbitrary: C's first, as in the hang.
        earlier_a.sort_by_key(|g| !Arc::ptr_eq(g, &gate_c));
        let start_a = std::thread::spawn(move || {
            for g in &earlier_a {
                g.wait();
            }
            drop(guard_a);
        });
        std::thread::sleep(Duration::from_millis(100));
        drop(guard_b1);
        let (guard_b2, earlier_b2) = begin(&engine, &b);
        assert_eq!(earlier_b2.len(), 1, "B's second start waits for A, which began before it");
        let start_b2 = std::thread::spawn(move || {
            for g in &earlier_b2 {
                g.wait();
            }
            drop(guard_b2);
        });
        std::thread::sleep(Duration::from_millis(100));
        drop(guard_c);
        let t0 = std::time::Instant::now();
        while !(start_a.is_finished() && start_b2.is_finished()) && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let (a_done, b_done) = (start_a.is_finished(), start_b2.is_finished());
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
        assert!(a_done && b_done, "after C's fill ended: A's start finished: {a_done}, B's second start finished: {b_done}");
    }

    /// A gate opened by an older start stays open when the container starts
    /// again: waiters that took it are not held by the new start.
    #[test]
    fn a_new_start_does_not_shut_the_gate_an_older_start_opened() {
        let (engine, id, dir) = engine("own-gate");
        let guard = shut(&engine, &id);
        let old = engine.lock().containers[&id].start_gate.clone();
        drop(guard);
        let guard = shut(&engine, &id);
        assert!(!old.is_shut() && engine.lock().containers[&id].start_gate.is_shut());
        drop(guard);
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A start that waited ends instead of running when Kiri began to stop
    /// meanwhile or the VM went; not when it was stopping already at the
    /// claim (its signals did not reach it, as before), and not when it had
    /// nothing to wait for (the caller does not look).
    #[test]
    fn a_start_that_waited_does_not_run_after_a_stop() {
        // (halted at claim, halting now, vm gone) -> refuse
        assert!(refuse_after_wait(false, true, false), "Kiri began to stop while it waited");
        assert!(refuse_after_wait(false, false, true), "the VM went");
        assert!(refuse_after_wait(true, true, true));
        assert!(!refuse_after_wait(false, false, false));
        assert!(!refuse_after_wait(true, true, false), "stopping before the claim: as it was");
        assert!(!refuse_after_wait(true, false, false));
        // The flags it reads.
        let (engine, _id, dir) = engine("halting");
        engine.stopped.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(!engine.halting());
        engine.parking.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(engine.halting());
        engine.parking.store(false, std::sync::atomic::Ordering::SeqCst);
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(engine.halting());
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A start by the restart policy that waited and is refused because Kiri
    /// began to stop stays pending, so Stop's hold keeps it and Start
    /// brings it back; a user start is not made pending; nothing is refused
    /// when Kiri was stopping at the claim, or is not stopping.
    #[test]
    fn a_refused_policy_start_stays_pending_for_stop_to_hold() {
        let (engine, id, dir) = engine("refused");
        let spec = {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.spec.restart.name = "always".into();
            c.spec.clone()
        };
        // As after the claim: Running, pending taken, gate shut.
        let claimed = |engine: &Engine, pending: bool| {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.status = Status::Running;
            c.restart_pending = pending;
        };
        let holds = |engine: &Engine| {
            let st = engine.lock();
            let c = &st.containers[&id];
            (c.status == Status::Running || c.restart_pending) && comes_back(c)
        };
        // (1) Kiri began to stop during the wait (the engine's stopped flag is set).
        for reason in [Start::Restart, Start::DaemonRestart, Start::Held] {
            claimed(&engine, false);
            let guard = shut(&engine, &id);
            let e = engine.refuse_waited_start(&id, &spec, reason, false, || false);
            drop(guard);
            assert!(matches!(&e, Some(Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock), "refused");
            let st = engine.lock();
            let c = &st.containers[&id];
            assert!(c.status != Status::Running && c.restart_pending, "still pending, not running");
            drop(st);
            assert!(holds(&engine), "Stop holds it");
        }
        // (2) A user start is not made pending.
        claimed(&engine, false);
        assert!(engine.refuse_waited_start(&id, &spec, Start::User, false, || false).is_some());
        assert!(!engine.lock().containers[&id].restart_pending);
        // The VM gone: not put back (recovery's own path).
        claimed(&engine, false);
        assert!(engine.refuse_waited_start(&id, &spec, Start::Restart, false, || true).is_some());
        assert!(!engine.lock().containers[&id].restart_pending);
        // (3) Stopping at the claim: as it was. Not stopping: not refused.
        claimed(&engine, false);
        assert!(engine.refuse_waited_start(&id, &spec, Start::Restart, true, || false).is_none());
        assert!(engine.lock().containers[&id].status == Status::Running);
        engine.stopped.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(engine.refuse_waited_start(&id, &spec, Start::Restart, false, || false).is_none());
        assert!(engine.lock().containers[&id].status == Status::Running);
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod leave_first_network_tests {
    use super::*;
    use super::super::network::{parse_ipam, Settings};

    /// An engine that never boots a VM, with one created (not running)
    /// container on the default bridge, and user networks `names`
    /// (`internal` ones as such), each on its own /24.
    fn engine(name: &str, nets: &[(&str, bool)]) -> (Arc<Engine>, String, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kiri-leave-first-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::boot(Store::open(&dir).unwrap()).unwrap();
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        for (i, (net, internal)) in nets.iter().enumerate() {
            let ipam = parse_ipam(&serde_json::json!({"Config": [{"Subnet": format!("10.98.{i}.0/24")}]})).unwrap();
            engine.lock_networks().create(net, Settings { ipam, internal: *internal, ..Default::default() }).unwrap();
        }
        let image = Image { mirror: None,
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["sleep", "30"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "bridge", "labels": {},
            "restart": {"name": "no", "max_retries": 0}
        })).unwrap();
        let id = engine.create(Some("probe".into()), spec, &image).unwrap();
        (engine, id, dir)
    }

    fn spec(engine: &Engine, id: &str) -> Spec {
        engine.lock().containers[id].spec.clone()
    }

    fn connect(engine: &Engine, net: &str, id: &str, aliases: &[&str], ip: &str) {
        engine.connect(net, id, aliases.iter().map(|a| (*a).to_owned()).collect(), ip.into(), String::new(), String::new()).unwrap();
    }

    fn done(engine: Arc<Engine>, dir: PathBuf) {
        drop(engine);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The names a rename puts in the guest: the container's own, its
    /// aliases on each network, whether a network has IPv4 addresses, and
    /// nothing for a stopped container or the default bridge.
    #[test]
    fn a_rename_names_each_network_with_its_aliases_and_address_kind() {
        let (engine, id, dir) = engine("rename", &[("front", false), ("back6", false)]);
        connect(&engine, "front", &id, &["web"], "");
        connect(&engine, "back6", &id, &["api"], "");
        // Stopped: nothing to tell the guest.
        engine.rename(&id, "first").unwrap();
        assert!(engine.dns_names_after_rename(&id).is_empty());
        {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.pid = 4242;
            c.status = Status::Running;
            c.ip = None;
            c.extra_ips = vec![("front".to_owned(), [10, 98, 0, 5]), ("back6".to_owned(), [10, 98, 1, 5])];
            c.extra_ip6s = vec![("back6".to_owned(), [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5])];
        }
        engine.lock_networks().find_mut("back6").unwrap().no_ipv4 = true;
        engine.rename(&id, "second").unwrap();
        let mut sent = engine.dns_names_after_rename(&id);
        sent.sort_by(|a, b| a.0.cmp(&b.0));
        let short = engine.lock().containers[&id].short_id().to_owned();
        let bridge_of = |n: &str| engine.lock_networks().find(n).unwrap().bridge.clone();
        assert_eq!(sent.len(), 2);
        let front = sent.iter().find(|u| u.0 == bridge_of("front")).unwrap();
        assert_eq!((front.1, front.3, front.4.clone()), ([10, 98, 0, 5], true, vec!["second".to_owned(), short.clone(), "web".to_owned()]));
        let back = sent.iter().find(|u| u.0 == bridge_of("back6")).unwrap();
        assert_eq!((back.1, back.2.is_some(), back.3, back.4.clone()), ([10, 98, 1, 5], true, false, vec!["second".to_owned(), short, "api".to_owned()]));
        done(engine, dir);
    }

    /// On its first network a rename names the container as its start did:
    /// its name, aliases and hostname (its short ID when it has none). The
    /// default bridge has no embedded DNS and gets nothing.
    #[test]
    fn a_rename_names_the_first_network_with_aliases_and_hostname() {
        let (engine, id, dir) = engine("rename-first", &[("front", false)]);
        let short = engine.lock().containers[&id].short_id().to_owned();
        let bridge = engine.lock_networks().find("front").unwrap().bridge.clone();
        {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.pid = 4242;
            c.status = Status::Running;
            c.ip = Some([10, 98, 0, 7]);
            c.spec.network_mode = "front".into();
            c.spec.aliases = vec!["db".into()];
        }
        engine.rename(&id, "first").unwrap();
        assert_eq!(engine.dns_names_after_rename(&id), [(bridge.clone(), [10, 98, 0, 7], None, true, vec!["first".to_owned(), short, "db".to_owned()])]);
        engine.lock().containers.get_mut(&id).unwrap().spec.hostname = "box".into();
        engine.rename(&id, "second").unwrap();
        assert_eq!(engine.dns_names_after_rename(&id), [(bridge, [10, 98, 0, 7], None, true, vec!["second".to_owned(), "box".to_owned(), "db".to_owned()])]);
        engine.lock().containers.get_mut(&id).unwrap().spec.network_mode = "bridge".into();
        assert!(engine.dns_names_after_rename(&id).is_empty(), "nothing for the default bridge");
        done(engine, dir);
    }

    /// The names sent with a rename are each there once, and never more than
    /// the guest takes in one frame: the container's own name stays first.
    #[test]
    fn a_renames_names_are_deduplicated_and_bounded() {
        let mut names = vec!["web".to_owned(), "abc123".to_owned(), "WEB".to_owned(), String::new()];
        names.extend((0..400).map(|i| format!("alias{}", i % 300)));
        let sent = super::rename_names(names);
        assert_eq!(sent.len(), kiri_proto::MAX_DNS_NAMES);
        assert_eq!(sent[..3], ["web", "abc123", "alias0"]);
        let unique: std::collections::HashSet<_> = sent.iter().map(|n| n.to_ascii_lowercase()).collect();
        assert_eq!(unique.len(), sent.len());
        let (engine, id, dir) = engine("rename-many", &[("front", false)]);
        {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.pid = 4242;
            c.status = Status::Running;
            c.ip = Some([10, 98, 0, 8]);
            c.spec.network_mode = "front".into();
            c.spec.aliases = (0..300).map(|i| format!("a{i}")).collect();
        }
        engine.rename(&id, "many").unwrap();
        let sent = engine.dns_names_after_rename(&id);
        assert_eq!(sent[0].4.len(), kiri_proto::MAX_DNS_NAMES);
        assert_eq!(sent[0].4[0], "many");
        let short = engine.lock().containers[&id].short_id().to_owned();
        assert_eq!(sent[0].4[1], short, "the hostname survives the cap");
        // The frame the guest gets decodes.
        let mut buf = Vec::new();
        kiri_proto::encode_dns_rename(&mut buf, &id, &sent[0].0, sent[0].1, sent[0].2, sent[0].3, &sent[0].4);
        assert!(kiri_proto::decode(&buf).is_ok());
        done(engine, dir);
    }

    /// A rename waits for the container's lifecycle lock, which a start holds
    /// from reading the name until its pid is set (and a stop, kill or
    /// remove holds too): a start never registers the name a rename replaced.
    #[test]
    fn a_rename_waits_for_a_start_or_stop_under_way() {
        let (engine, id, dir) = engine("order-life", &[]);
        let lifecycle = engine.lifecycle(&id).unwrap();
        let held = lifecycle.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (e2, id2) = (engine.clone(), id.clone());
        let t = std::thread::spawn(move || {
            e2.rename(&id2, "later").unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "renamed while a start held the lifecycle");
        assert_eq!(engine.lock().containers[&id].name, "probe");
        drop(held);
        rx.recv_timeout(Duration::from_secs(5)).expect("renamed once the start was done");
        t.join().unwrap();
        assert_eq!(engine.lock().containers[&id].name, "later");
        done(engine, dir);
    }

    /// A rename waits for a change of the same container's guest names under
    /// way (a network joined or left), and for a start: it is never told to
    /// the guest in the middle of one, or over it.
    #[test]
    fn a_rename_waits_for_the_names_change_under_way() {
        let (engine, id, dir) = engine("order", &[]);
        let sync = engine.names_sync(&id).unwrap();
        let held = sync.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (e2, id2) = (engine.clone(), id.clone());
        let t = std::thread::spawn(move || {
            e2.rename(&id2, "later").unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "renamed while a names change held the order");
        assert_eq!(engine.lock().containers[&id].name, "probe");
        drop(held);
        rx.recv_timeout(Duration::from_secs(5)).expect("renamed once the change was done");
        t.join().unwrap();
        assert_eq!(engine.lock().containers[&id].name, "later");
        done(engine, dir);
    }

    /// A container name starts with a letter or digit, as Docker's does.
    #[test]
    fn a_container_name_starts_with_a_letter_or_digit() {
        for ok in ["a", "db", "0db", "my_db-1.x", "DB"] {
            assert!(valid_container_name(ok), "{ok}");
        }
        for bad in ["", ".db", "-db", "_db", "d b", "d/b", "dé"] {
            assert!(!valid_container_name(bad), "{bad}");
        }
        let (engine, id, dir) = engine("badname", &[]);
        assert!(matches!(engine.rename(&id, "_x"), Err(Error::BadRequest(_))));
        done(engine, dir);
    }

    /// The .NET Aspire sequence: created on bridge, connected to a session
    /// network, disconnected from bridge. The session network becomes its
    /// first, with its own aliases, and the record on disk agrees.
    #[test]
    fn leaving_the_first_network_makes_the_other_one_first() {
        let (engine, id, dir) = engine("one", &[("probe-net", false)]);
        connect(&engine, "probe-net", &id, &["web"], "");
        let (_, events) = engine.subscribe();
        engine.disconnect("bridge", &id).unwrap();
        let s = spec(&engine, &id);
        assert_eq!(s.network_mode, "probe-net");
        assert_eq!(s.aliases, ["web"]);
        assert!(s.extra_networks.is_empty(), "{:?}", s.extra_networks);
        assert!(!s.left_all_networks);
        let sent: Vec<_> = events.try_iter().filter(|e| e["Type"] == "network").map(|e| (e["Action"].as_str().unwrap().to_owned(), e["Actor"]["Attributes"]["name"].as_str().unwrap().to_owned())).collect();
        assert_eq!(sent, [("disconnect".to_owned(), "bridge".to_owned())]);
        let saved: Vec<Saved> = serde_json::from_slice(&std::fs::read(&engine.records).unwrap()).unwrap();
        assert_eq!(saved.iter().find(|c| c.id == id).unwrap().spec.network_mode, "probe-net");
        // It is no longer on bridge.
        assert!(matches!(engine.disconnect("bridge", &id), Err(Error::NotFound(_))));
        done(engine, dir);
    }

    /// With two others, the next first is the one Docker gives the default
    /// route to: one that reaches outside before an internal one, then by
    /// name, whatever the order they were connected in. The other stays an
    /// extra network with its settings.
    #[test]
    fn with_two_others_the_first_by_name_that_reaches_outside_takes_its_place() {
        let (engine, id, dir) = engine("two", &[("zeta", false), ("alpha", true), ("beta", false)]);
        connect(&engine, "zeta", &id, &["z"], "10.98.0.7");
        connect(&engine, "alpha", &id, &["a"], "");
        connect(&engine, "beta", &id, &["b"], "10.98.2.9");
        engine.disconnect("bridge", &id).unwrap();
        let s = spec(&engine, &id);
        assert_eq!(s.network_mode, "beta", "alpha is internal; beta comes before zeta");
        assert_eq!((s.aliases.as_slice(), s.ipv4_address.as_str()), (&["b".to_owned()][..], "10.98.2.9"));
        assert_eq!(s.extra_networks.iter().map(|x| (x.name.as_str(), x.ipv4_address.as_str())).collect::<Vec<_>>(), [("zeta", "10.98.0.7"), ("alpha", "")]);
        // Leaving that one too: zeta reaches outside, alpha does not.
        engine.disconnect("beta", &id).unwrap();
        assert_eq!(spec(&engine, &id).network_mode, "zeta");
        engine.disconnect("zeta", &id).unwrap();
        assert_eq!(spec(&engine, &id).network_mode, "alpha", "an internal network when it is the only one left");
        done(engine, dir);
    }

    /// Leaving the only network leaves none, as `--network none`: inspect
    /// shows no networks, a start gives it no network, and it can be
    /// connected again, to what then becomes its first network.
    #[test]
    fn leaving_the_only_network_leaves_none_and_it_can_join_again() {
        let (engine, id, dir) = engine("only", &[("probe-net", false)]);
        engine.disconnect("bridge", &id).unwrap();
        let s = spec(&engine, &id);
        assert_eq!(s.network_mode, "none");
        assert!(s.left_all_networks && s.extra_networks.is_empty());
        assert!(matches!(engine.disconnect("bridge", &id), Err(Error::NotFound(_))));
        assert!(matches!(engine.disconnect("none", &id), Err(Error::NotFound(_))), "it is on no network, not on none");
        connect(&engine, "probe-net", &id, &["web"], "10.98.0.5");
        let s = spec(&engine, &id);
        assert_eq!((s.network_mode.as_str(), s.ipv4_address.as_str(), s.aliases.as_slice()), ("probe-net", "10.98.0.5", &["web".to_owned()][..]));
        assert!(!s.left_all_networks && s.extra_networks.is_empty());
        // And back on the bridge it left, as an extra network.
        connect(&engine, "bridge", &id, &[], "");
        let s = spec(&engine, &id);
        assert_eq!(s.network_mode, "probe-net");
        assert_eq!(s.extra_networks.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["bridge"]);
        done(engine, dir);
    }

    /// `--network none` stays as it was: no disconnect from none, no connect.
    #[test]
    fn a_none_container_is_unchanged() {
        let (engine, id, dir) = engine("none", &[("probe-net", false)]);
        engine.lock().containers.get_mut(&id).unwrap().spec.network_mode = "none".into();
        assert!(matches!(engine.disconnect("none", &id), Err(Error::BadRequest(_))));
        assert!(matches!(engine.connect("probe-net", &id, Vec::new(), String::new(), String::new(), String::new()), Err(Error::BadRequest(_))));
        assert_eq!(spec(&engine, &id).network_mode, "none");
        done(engine, dir);
    }

    /// A fixed `--ip`, `--ip6` and `--mac-address` belong to the network
    /// they were given for: none of them follows the container to the
    /// next one, the address is no longer kept free for it, and an address
    /// a run left behind goes back to the pool, so the network can go.
    #[test]
    fn a_fixed_address_on_the_left_network_is_released_and_not_carried_over() {
        let (engine, id, dir) = engine("fixed", &[("left", false), ("kept", false)]);
        {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.spec.network_mode = "left".into();
            c.spec.ipv4_address = "10.98.0.50".into();
            c.spec.ipv6_address = "fd00::50".into();
            c.spec.mac_address = "02:42:0a:62:00:32".into();
            c.spec.aliases = vec!["old".into()];
            // As if a run had not handed it back.
            c.ip = Some([10, 98, 0, 50]);
        }
        engine.lock_networks().reserve("left", [10, 98, 0, 50]).unwrap();
        assert_eq!(engine.wanted_addresses("left"), [[10, 98, 0, 50]]);
        connect(&engine, "kept", &id, &[], "");
        engine.disconnect("left", &id).unwrap();
        let s = spec(&engine, &id);
        assert_eq!(s.network_mode, "kept");
        assert_eq!((s.ipv4_address.as_str(), s.ipv6_address.as_str(), s.mac_address.as_str()), ("", "", ""));
        assert!(s.aliases.is_empty());
        assert!(engine.wanted_addresses("left").is_empty());
        assert!(engine.wanted_addresses("kept").is_empty());
        assert_eq!(engine.lock().containers[&id].ip, None);
        engine.lock_networks().reserve("left", [10, 98, 0, 50]).expect("the address went back to the pool");
        engine.lock_networks().release("left", [10, 98, 0, 50]);
        engine.lock_networks().remove("left").expect("the left network can be removed");
        done(engine, dir);
    }

    /// Reconnecting to the network it left puts it back as an extra
    /// network: the one that took its place stays first.
    #[test]
    fn reconnecting_to_the_left_network() {
        let (engine, id, dir) = engine("again", &[("probe-net", false)]);
        connect(&engine, "probe-net", &id, &[], "");
        engine.disconnect("bridge", &id).unwrap();
        connect(&engine, "bridge", &id, &[], "");
        let s = spec(&engine, &id);
        assert_eq!(s.network_mode, "probe-net");
        assert_eq!(s.extra_networks.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["bridge"]);
        assert!(matches!(engine.connect("bridge", &id, Vec::new(), String::new(), String::new(), String::new()), Err(Error::Conflict(_))));
        done(engine, dir);
    }

    /// A running container is refused, with what to do instead, and
    /// nothing changes: moving its default route, address and resolver to
    /// another network while it runs is more than an unplug.
    #[test]
    fn a_running_container_must_be_stopped_first() {
        let (engine, id, dir) = engine("running", &[("probe-net", false)]);
        connect(&engine, "probe-net", &id, &[], "");
        engine.lock().containers.get_mut(&id).unwrap().status = Status::Running;
        let r = engine.disconnect("bridge", &id);
        assert!(matches!(&r, Err(Error::Conflict(m)) if m.contains("stop the container first")), "{r:?}");
        let s = spec(&engine, &id);
        assert_eq!(s.network_mode, "bridge");
        assert_eq!(s.extra_networks.len(), 1);
        done(engine, dir);
    }

    /// Records saved before a container could leave all its networks load,
    /// as on a network.
    #[test]
    fn old_records_load() {
        let s: Spec = serde_json::from_value(serde_json::json!({
            "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": ["sh"], "env": [], "cwd": "", "user": "",
            "auto_remove": false, "network_mode": "bridge", "labels": {},
        }))
        .unwrap();
        assert!(!s.left_all_networks);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod cpu_tests {
    /// The CPUs `Cpus_allowed_list` of /proc/self/status lists.
    fn allowed() -> usize {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let list = status.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:")).unwrap().trim().to_owned();
        list.split(',')
            .map(|r| match r.split_once('-') {
                Some((a, b)) => b.parse::<usize>().unwrap() - a.parse::<usize>().unwrap() + 1,
                None => 1,
            })
            .sum()
    }

    /// On Linux there is no VM: `docker info`'s NCPU is the CPUs the engine
    /// may run on, as dockerd counts them (Go's runtime.NumCPU, the affinity
    /// mask), not the Mac VM's default of 2.
    #[test]
    fn on_linux_the_engine_has_the_cpus_it_may_run_on() {
        assert_eq!(super::vm_cpus(), allowed());
    }
}
