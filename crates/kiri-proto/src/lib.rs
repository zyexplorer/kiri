//! Wire protocol between the Kiri host and the guest agent.
//!
//! Every frame is `len: u32 LE | kind: u16 LE | payload`, where `len` counts
//! `kind` plus `payload`. Encoding writes into a caller-owned buffer and
//! decoding borrows from one, so steady-state traffic never allocates.
//!
//! Strings are `u32 len | bytes`; string lists are `u32 count | strings`.
//! A [`Msg::LayerBegin`] frame is followed by exactly `size` raw bytes of the
//! layer blob, outside any frame, so bulk data is never copied into frames.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

/// vsock port the host listens on; the guest dials it once it is ready.
pub const HOST_PORT: u32 = 1024;

/// Largest frame either side will accept. Anything bigger is a protocol error,
/// which keeps a misbehaving peer from making us allocate without bound.
pub const MAX_FRAME: usize = 1 << 20;

/// Bytes before the payload: length plus kind.
pub const HEADER: usize = 6;

/// Largest `Output` chunk; keeps frames well under `MAX_FRAME`.
pub const MAX_OUTPUT_CHUNK: usize = 64 * 1024;
/// Stdin bytes the host may have sent a process beyond what the guest has
/// reported consumed (`InputConsumed`). The guest buffers at most this much
/// for a process that is not reading; the host blocks the client instead.
pub const INPUT_WINDOW: usize = 4 << 20;

/// The wire version. The host refuses an agent with another one: 3 added
/// `InputConsumed`, which the host waits on, so an older agent would leave
/// a client's stdin blocked after `INPUT_WINDOW` bytes. 4 added
/// `UsageQuery`, which has no reply: an older agent's "unexpected" error
/// would be taken as the reply to the next request. 5 added `RunSpec::seccomp`,
/// the container's own seccomp program: an older agent would ignore it and
/// run the container under the default filter instead of its profile. 6
/// added `Started::starting` and `AwaitStart`: an older host would read a
/// start still filling a new volume as started, and let an exec in before
/// the container's root is in place. 7 added `CancelStart`, which has no
/// reply: an older agent's "unexpected" error would be taken as the reply
/// to the next request. 8 added `DnsRename`, another no-reply message, for
/// the same reason.
pub const PROTOCOL: u16 = 8;

/// Most names a `DnsRename` may carry: a name, its aliases and a hostname.
pub const MAX_DNS_NAMES: usize = 256;

// `Run` is the one big message and is built and read once per container.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg<'a> {
    /// Guest → host, sent once on connect.
    Hello(Hello),
    /// Either direction; the peer answers with `Pong` carrying the same value.
    Ping(u64),
    Pong(u64),
    /// Host → guest: flush and power off.
    Shutdown,
    /// Guest → host: a human-readable failure.
    Error(&'a str),
    /// Host → guest: report current resource use.
    StatsRequest,
    /// Guest → host: answer to `StatsRequest`.
    Stats(Stats),
    /// Host → guest: is this layer already unpacked? Answered by `LayerStatus`.
    LayerQuery(&'a str),
    LayerStatus(bool),
    /// Host → guest: the next `size` raw bytes on the stream are this layer's
    /// blob. Answered by `LayerStored` or `Error`.
    LayerBegin(LayerBegin<'a>),
    LayerStored(LayerStored),
    /// Host → guest: start a container. Answered by `Started` (or `Error`),
    /// then any number of `Output`, then `Exited`.
    Run(RunSpec<'a>),
    /// `starting`: the container's process is running but its root is not
    /// in place yet (a new volume is still being filled); ask `AwaitStart`
    /// until it is. Always false for an exec, and from older agents.
    Started { id: &'a str, pid: u32, starting: bool },
    /// Host → guest: is the root of a container that started `starting` in
    /// place now? Answered by `Started` for that container (`starting`
    /// false once it is), or `Error` when it is not running.
    AwaitStart { id: &'a str },
    /// Host → guest: the container is being stopped or removed while its
    /// start has not finished. A child still making its root (one that has
    /// not begun filling a volume) is killed at once, and its start fails;
    /// one filling a volume is killed as soon as the fill is done. No reply;
    /// nothing happens to a container whose root is in place.
    CancelStart { id: &'a str },
    Output { id: &'a str, stream: Stream, data: &'a [u8] },
    Exited(Exited<'a>),
    /// Host → guest: send `signal` to the container's process. No reply;
    /// the effect shows up as `Exited`.
    Kill { id: &'a str, signal: u32 },
    /// Host → guest: delete a stopped container's writable layer. No reply.
    Remove { id: &'a str },
    /// Host → guest: set the wall clock (Unix nanoseconds). No reply. Sent
    /// on connect; the guest has no other source of real time.
    SetTime(u64),
    /// Host → guest: bytes for a process's stdin (container or exec). No reply.
    Input { id: &'a str, data: &'a [u8] },
    /// Host → guest: end of stdin. No reply.
    CloseStdin { id: &'a str },
    /// Host → guest: terminal size for a TTY process. No reply.
    Resize { id: &'a str, rows: u16, cols: u16 },
    /// Guest → host: the process took `bytes` more of its stdin, so the
    /// host may send that much more (see `INPUT_WINDOW`). Unsolicited.
    InputConsumed { id: &'a str, bytes: u32 },
    /// Host → guest: forward a published port in the kernel (nftables
    /// DNAT) instead of relaying it. Answered by `Ack` or `Error`. In the VM
    /// TCP only, and only from the uplink's gateway (see `UplinkQuery`).
    Publish(Publish<'a>),
    /// Host → guest: drop every `Publish` for this id. No reply.
    Unpublish { id: &'a str },
    /// Host → guest: run another process inside a running container.
    /// Answered like `Run`: `Started`, `Output`…, `Exited`, all with `exec_id`.
    Exec(ExecSpec<'a>),
    /// Host → guest: delete every stopped container's files except these.
    /// Sent once after connecting. No reply.
    Retain(StrList<'a>),
    /// Host → guest: delete every unpacked layer except these (by blob
    /// digest), and any left half-written. Sent once after connecting,
    /// before anything runs. No reply.
    RetainLayers(StrList<'a>),
    /// Asks how much disk the guest's layers, containers and volumes use.
    UsageQuery,
    /// The reply: one `KIND NAME BYTES` line per entry, KIND `l` (an
    /// unpacked layer, by blob hex), `c` (a container's writable layer, by
    /// id) or `v` (a volume, by name). BYTES is what it takes on disk.
    Usage(StrList<'a>),
    /// Host → guest: metadata of a path inside a running container's root
    /// (symlinks are not followed). Answered by `FsStat` or `Error`.
    FsStatRequest { id: &'a str, path: &'a str },
    FsStat(FsEntry<'a>),
    /// Host → guest: a directory's entries. Answered by `FsList` or `Error`.
    FsListRequest { id: &'a str, path: &'a str },
    /// Entries, each encoded as an `FsEntry`; iterate with [`fs_entries`].
    FsList { count: u32, raw: &'a [u8] },
    /// Host → guest: up to `len` bytes from `offset`. Answered by `FsData` or `Error`.
    FsRead { id: &'a str, path: &'a str, offset: u64, len: u32 },
    FsData(&'a [u8]),
    /// Guest → host: memory is tight right now (a PSI trigger fired); grow
    /// the VM's memory. No reply.
    MemoryPressure,
    /// Host → guest: delete a named volume's data. No reply.
    RemoveVolume { name: &'a str },
    /// Host → guest, first frame on a connection to [`COPY_PORT`]: copy
    /// files into or out of a container (`docker cp`). `layers` lets the
    /// guest mount a container that is not running. For `copy_op::IN` raw
    /// tar bytes follow until the host shuts its side, then the guest answers
    /// `CopyDone` or `Error`. For `OUT` and `STAT` the guest answers
    /// `CopyStat` or `Error`; for `OUT` raw tar bytes follow until it closes.
    Copy(CopySpec<'a>),
    /// Guest → host: the copied path's metadata, and its target if a link.
    CopyStat { entry: FsEntry<'a>, link: &'a str },
    CopyDone,
    /// Host → guest: change a running container's limits. Answered by `Ack` or `Error`.
    Update { id: &'a str, limits: Limits },
    /// Host → guest: freeze (pause) or thaw a container. Answered by `Ack` or `Error`.
    Freeze { id: &'a str, frozen: bool },
    /// Host → guest: a container's resource use. Answered by `ContainerStats` or `Error`.
    StatsQuery { id: &'a str },
    ContainerStats(ContainerStats),
    /// Host → guest: the container's processes. Answered by `Top` or `Error`.
    TopQuery { id: &'a str },
    /// Tab-separated `user pid ppid c stime tty time comm stat rss vsz %cpu
    /// %mem etime-secs time-secs uid f priority nice sz cmd`, one process
    /// each (cmd last; an older agent sent fewer fields before cmd).
    Top(StrList<'a>),
    /// Host → guest: plug a running container into another bridge network
    /// as `ifname`. Answered by `Ack` or `Error`.
    NetAttach { id: &'a str, ifname: &'a str, net: NetSpec<'a> },
    /// Host → guest: unplug it again. Answered by `Ack` or `Error`.
    NetDetach { id: &'a str, ifname: &'a str, bridge: &'a str, ip: [u8; 4] },
    /// Host → guest: the TCP sockets a running container listens on.
    /// Answered by `Listening` or `Error`; an older agent answers `Error`.
    ListenQuery { id: &'a str },
    /// The container's `/proc/net/tcp` and `tcp6` lines in state LISTEN, as
    /// the kernel writes them.
    Listening(StrList<'a>),
    /// Host → guest: where the USB devices passed through to the VM show
    /// up. Answered by `UsbNodes` or `Error`; an older agent answers `Error`.
    UsbNodesQuery,
    /// One `PORT\tPATH\tWHAT` line per device node: PORT is the vhci port
    /// the device was attached on (the agent's answer to it), PATH the node
    /// (`/dev/ttyUSB0`, or a network interface's name), WHAT what it is
    /// (`serial port`, `raw USB`, …).
    UsbNodes(StrList<'a>),
    /// Host → guest: route `dst/prefix` through `via` in the VM (or remove
    /// that route), so the host's relay can reach addresses that live behind
    /// a container, such as a Kubernetes node's pods and services. Answered
    /// by `Ack` or `Error`; an older agent answers `Error`.
    Route { dst: [u8; 4], prefix: u8, via: [u8; 4], add: bool },
    /// Host → guest, in the VM: the uplink's address and the gateway it
    /// leased from (the Mac's side of the NAT network). Answered by `Uplink`,
    /// or `Error` with no lease yet; an older agent answers `Error`.
    UplinkQuery,
    Uplink { ip: [u8; 4], gateway: [u8; 4] },
    /// Host → guest: container `id` on `bridge` was renamed. Its old DNS
    /// names are dropped and `names` registered to the same address (A
    /// records only when `v4`: a network without IPv4 addresses keeps the
    /// address only as the container's key). `ip6` is either empty or 16
    /// bytes, and `names` has at most `MAX_DNS_NAMES`. No reply, hence the
    /// protocol bump to 8: an older agent answers an unknown frame with an
    /// error, which would be read as the next request's reply.
    DnsRename { id: &'a str, bridge: &'a str, ip: [u8; 4], ip6: &'a [u8], v4: bool, names: StrList<'a> },
    /// Host → guest: a network was removed; delete its bridge, and with it
    /// the bridge's addresses and routes, its DNS server and, with the last
    /// IPv6 network, the IPv6 NAT table. No reply; an older agent ignores it.
    RemoveBridge { bridge: &'a str },
    /// Host → guest, in the VM: files the Mac changed under bind mounts, one
    /// `OP\tPATH` line each: OP is `C` (created), `M` (modified), `A`
    /// (attributes changed) or `D` (deleted), PATH the file's path in the VM
    /// (under /mnt/mac). The agent raises the inotify events Linux would for
    /// a change made in the VM. No reply. An older agent answers an unknown
    /// kind with an `Error`, which would be taken as the reply to the next
    /// request; the VM runs inside the daemon process today, so the two
    /// versions cannot differ.
    FsEvents(StrList<'a>),
    /// Success, for requests with nothing else to say.
    Ack,
}

/// vsock port of the guest's copy service: one connection per copy.
pub const COPY_PORT: u32 = 1028;

/// vsock port of the host's DNS relay (VM only). The agent sends a lookup
/// the VM's gateway resolver didn't answer as `u16 len | query`, as DNS over
/// TCP frames it, and gets `u16 len | reply` back: one lookup per connection.
pub const DNS_RELAY_PORT: u32 = 1037;

/// vsock port of the guest's USB passthrough: one connection per device,
/// `speed u32 | devid u32` then USB/IP (crates/kiri-init/src/linux/usb.rs).
pub const USB_PORT: u32 = 1040;

pub mod copy_op {
    pub const IN: u8 = 1;
    pub const OUT: u8 = 2;
    pub const STAT: u8 = 3;
    /// The next three answer `CopyDone` (or `Error`), then send raw data
    /// until the connection closes.
    /// `docker diff`: lines of `A path`, `C path`, `D path`.
    pub const DIFF: u8 = 4;
    /// `docker export`: the whole filesystem, without mounts, as a tar.
    pub const EXPORT: u8 = 5;
    /// `docker commit`: the writable layer as an OCI layer tar (whiteouts as
    /// `.wh.` files).
    pub const COMMIT: u8 = 6;
}

pub mod copy_flags {
    /// Keep the archive's owners (`docker cp -a`); otherwise root owns everything.
    pub const KEEP_OWNERS: u32 = 1;
    /// Refuse to replace a directory with a file or the reverse.
    pub const NO_OVERWRITE_DIR_NON_DIR: u32 = 2;
    /// `IN`: the archive comes as chunks, each `len: u32 LE | bytes`, and an
    /// empty one ends it. The guest then knows where it ends without the
    /// host half-closing the connection: Virtualization.framework on an
    /// Intel Mac with macOS 14 passes a half-close on as a full one, and the
    /// guest's answer fails with EPIPE.
    pub const CHUNKED: u32 = 4;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopySpec<'a> {
    pub op: u8,
    pub id: &'a str,
    pub path: &'a str,
    pub flags: u32,
    pub layers: StrList<'a>,
}

/// Encodes a `Copy` frame from the sender's side.
pub fn encode_copy<S: AsRef<str>>(out: &mut Vec<u8>, op: u8, id: &str, path: &str, flags: u32, layers: &[S]) {
    frame(out, kind::COPY, |o| {
        o.push(op);
        put_str(o, id);
        put_str(o, path);
        put_u32(o, flags);
        StrList::put(o, layers);
    });
}

/// One file's metadata, as the file browser needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsEntry<'a> {
    pub name: &'a str,
    /// `st_mode`: type and permission bits.
    pub mode: u32,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub mtime: i64,
    /// Nanoseconds past `mtime`: same-second rewrites still look changed.
    pub mtime_nsec: u32,
    /// Inode number, unique within the container's filesystems.
    pub ino: u64,
}

fn put_entry(out: &mut Vec<u8>, e: &FsEntry<'_>) {
    put_str(out, e.name);
    put_u32(out, e.mode);
    put_u64(out, e.size);
    put_u64(out, e.mtime as u64);
    put_u32(out, e.mtime_nsec);
    put_u64(out, e.ino);
}

/// Encodes an `FsList` reply from entries.
pub fn encode_fs_list(out: &mut Vec<u8>, entries: &[FsEntry<'_>]) {
    frame(out, kind::FS_LIST, |o| {
        put_u32(o, entries.len() as u32);
        for e in entries {
            put_entry(o, e);
        }
    });
}

/// The entries of an `FsList`, without allocating.
pub fn fs_entries<'a>(count: u32, raw: &'a [u8]) -> impl Iterator<Item = FsEntry<'a>> + 'a {
    let mut r = Reader { buf: raw, pos: 0 };
    (0..count).map_while(move |_| read_entry(&mut r).ok())
}

fn read_entry<'a>(r: &mut Reader<'a>) -> Result<FsEntry<'a>, DecodeError> {
    Ok(FsEntry { name: r.str()?, mode: r.u32()?, size: r.u64()?, mtime: r.u64()? as i64, mtime_nsec: r.u32()?, ino: r.u64()? })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecSpec<'a> {
    pub container_id: &'a str,
    pub exec_id: &'a str,
    pub argv: StrList<'a>,
    pub env: StrList<'a>,
    pub cwd: &'a str,
    pub user: &'a str,
    /// `run_flags::TTY` and `run_flags::STDIN` apply.
    pub flags: u32,
    /// As for the container: 0 means Docker's default set.
    pub caps: u64,
    /// The container's `HostOpts` lines; an exec uses `rlimit` and `group`.
    pub opts: StrList<'a>,
}

/// Live guest resource use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// CLOCK_BOOTTIME when sampled, in nanoseconds.
    pub uptime_ns: u64,
    pub mem_total_kib: u64,
    pub mem_available_kib: u64,
    /// Resident memory of the guest agent (PID 1) itself.
    pub agent_rss_kib: u64,
    /// What the guest holds and cannot give back: anonymous, shared and
    /// kernel memory, from cgroup v2 accounting, which leaves out the page
    /// cache and the balloon's own pages. 0 from an agent that cannot tell.
    pub mem_used_kib: u64,
    /// Share of the last 10 s in which every task waited on memory (PSI
    /// "full avg10"), in hundredths of a percent: high means the guest is
    /// thrashing, out of memory in all but name. 0 from an older agent.
    pub mem_stall: u64,
    /// Processes the kernel has killed for memory since boot (/proc/vmstat oom_kill).
    pub oom_kills: u64,
    /// Pages held by the memory balloon, in KiB. None from an agent or
    /// kernel that does not report it.
    pub mem_balloon_kib: Option<u64>,
}

/// What the guest knows about its own boot, in nanoseconds of CLOCK_BOOTTIME.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub protocol: u16,
    /// When PID 1 started running.
    pub init_start_ns: u64,
    /// When the agent was ready to serve requests.
    pub ready_ns: u64,
    /// MemTotal and MemAvailable from /proc/meminfo, in KiB.
    pub mem_total_kib: u64,
    pub mem_available_kib: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerBegin<'a> {
    pub digest: &'a str,
    pub media_type: &'a str,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerStored {
    pub unpacked_bytes: u64,
    pub elapsed_ns: u64,
}

pub mod run_flags {
    /// Remove the container's writable layer and cgroup once it exits.
    pub const REMOVE: u32 = 1;
    /// Give the container its own network namespace with only loopback.
    pub const NET_NONE: u32 = 2;
    /// Run on a pseudo-terminal created inside the container; output arrives
    /// as `Stdout` only, and `Resize` applies.
    pub const TTY: u32 = 4;
    /// Keep stdin open for `Input` (otherwise it is /dev/null).
    pub const STDIN: u32 = 8;
    /// Point /etc/resolv.conf at the bridge gateway (the agent's DNS for
    /// user-defined networks) instead of the VM's upstream resolver.
    pub const DNS_GATEWAY: u32 = 16;
    /// Share the VM's network namespace (`--network host`).
    pub const NET_HOST: u32 = 32;
    /// `--privileged`: every capability, the VM's devices, writable /sys and cgroups.
    pub const PRIVILEGED: u32 = 64;
    /// `--security-opt seccomp=unconfined`: no default syscall filter, only
    /// the host boundary (no vsock sockets, no io_uring), as with `PRIVILEGED`.
    pub const NO_SECCOMP: u32 = 128;
    /// `--security-opt no-new-privileges`.
    pub const NO_NEW_PRIVS: u32 = 256;
    /// `--read-only`: the root filesystem is mounted read-only.
    pub const READONLY_ROOTFS: u32 = 512;
    /// `--init`: PID 1 is a small init (the agent's own code) that runs the
    /// program as its child, forwards signals to it, reaps orphans and exits
    /// with the program's status, as tini does for Docker.
    pub const INIT: u32 = 1 << 16;
    /// `--pid host`: no PID namespace of its own; the container sees the
    /// VM's processes (on a Linux host, the host's).
    pub const PID_HOST: u32 = 1 << 17;
}

/// A container's network, as `Run` carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetSpec<'a> {
    /// Bridge to join; empty for `none` or `host` (see the flags).
    pub bridge: &'a str,
    pub ip: [u8; 4],
    pub prefix: u8,
    pub gateway: [u8; 4],
    /// Extra /etc/hosts lines (`host.docker.internal`, `--add-host`, links).
    pub extra_hosts: StrList<'a>,
    /// Names other containers on this network can resolve it by (its name,
    /// aliases, short id). Empty on the default bridge, as with Docker.
    pub dns_names: StrList<'a>,
    /// More settings, each `key=value` (see [`net_opts`]). Sent last in the
    /// frame, so an agent that predates them reads an empty list.
    pub opts: StrList<'a>,
}

/// Keys of [`NetSpec::opts`].
pub mod net_opts {
    /// `netns=<container id>`: join that running container's network
    /// namespace instead of getting one (`--network container:<id>`).
    pub const NETNS: &str = "netns";
    /// `pidns=<container id>`: join that running container's PID namespace
    /// instead of getting one (`--pid container:<id>`).
    pub const PIDNS: &str = "pidns";
    /// `mac=02:42:ac:11:00:02`: the container side's MAC address.
    pub const MAC: &str = "mac";
    /// `nameserver=1.1.1.1` (`--dns`): with `DNS_GATEWAY` the gateway's DNS
    /// forwards this container's queries there; otherwise it goes into
    /// resolv.conf.
    pub const NAMESERVER: &str = "nameserver";
    /// `search=example.com` (`--dns-search`), one per domain.
    pub const SEARCH: &str = "search";
    /// `option=ndots:2` (`--dns-option`), one per option.
    pub const OPTION: &str = "option";
    /// `domainname=example.com` (`--domainname`).
    pub const DOMAINNAME: &str = "domainname";
    /// `ip6=fd00:1::2/64`: the container's IPv6 address and prefix on an
    /// IPv6 network (`--ipv6`), given with [`GW6`].
    pub const IP6: &str = "ip6";
    /// `gw6=fd00:1::1`: that network's IPv6 gateway, the bridge's address.
    pub const GW6: &str = "gw6";
    /// `internal=1`: the network is `--internal`: no default route, so
    /// nothing beyond it is reached (and fails at once).
    pub const INTERNAL: &str = "internal";
    /// `noipv4=1`: an IPv6-only network. The container's IPv4 address is
    /// only the name the guest keeps it by: neither it nor the bridge gets
    /// one, and DNS has no A record for it.
    pub const NO_IPV4: &str = "noipv4";
    /// `dnsbridge=kiri3`, one per network: a user network the container
    /// joins once it runs (`NetAttach`). Its embedded DNS answers that
    /// network's names from the start, as Docker's does with every network
    /// joined before the program runs.
    pub const DNS_BRIDGE: &str = "dnsbridge";
    /// `network=frontend`: the Docker network's name. The guest only knows
    /// bridges (`kiri3`); a container's embedded DNS searches its networks
    /// in this name's order, as Docker does, not in the order it joined.
    pub const NETWORK: &str = "network";

    /// The values given for `key`, in order.
    pub fn values<'a>(opts: super::StrList<'a>, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let mut r = super::Reader { buf: opts.raw, pos: 0 };
        (0..opts.count).map_while(move |_| r.str().ok()).filter_map(move |o| o.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSpec<'a> {
    pub id: &'a str,
    /// Layer digests from the top layer down to the base.
    pub layers: StrList<'a>,
    pub argv: StrList<'a>,
    pub env: StrList<'a>,
    pub cwd: &'a str,
    /// `user`, `uid`, `user:group` or `uid:gid`; empty means root.
    pub user: &'a str,
    pub hostname: &'a str,
    pub flags: u32,
    pub net: NetSpec<'a>,
    /// Bind mounts, each `source\ttarget\tflags`: `source` is a path in the
    /// guest; `flags` is a comma list (`ro`).
    pub mounts: StrList<'a>,
    pub limits: Limits,
    /// Capabilities to keep, one bit each; 0 means Docker's default set.
    pub caps: u64,
    /// HostConfig settings applied at start, one per line, tab-separated:
    /// `rlimit res soft hard`, `sysctl ns path value`, `device host
    /// container perms`, `devrule type major minor perms`, `group name`.
    /// Empty from an older daemon.
    pub opts: StrList<'a>,
    /// The container's own seccomp profile, compiled by the daemon to a
    /// classic-BPF program for the guest's architecture: instructions of
    /// eight bytes each (code u16, jt u8, jf u8, k u32; little-endian). Empty
    /// for the guest's built-in default. Execs run under the same program;
    /// the guest keeps it with the container.
    pub seccomp: &'a [u8],
}

/// cgroup limits (`--memory`, `--cpus`, `--cpu-shares`, `--pids-limit`).
/// Zero means no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    pub memory_max: u64,
    /// CPU time allowed per `cpu_period` microseconds (`--cpus 1.5` is 150000/100000).
    pub cpu_quota: u64,
    pub cpu_period: u32,
    /// cgroup v2 `cpu.weight`, 1–10000 (Docker's shares, converted).
    pub cpu_weight: u32,
    pub pids_max: u32,
    /// `--shm-size` in bytes; 0 means 64 MiB.
    pub shm_size: u64,
}

fn put_limits(out: &mut Vec<u8>, l: &Limits) {
    put_u64(out, l.memory_max);
    put_u64(out, l.cpu_quota);
    put_u32(out, l.cpu_period);
    put_u32(out, l.cpu_weight);
    put_u32(out, l.pids_max);
    put_u64(out, l.shm_size);
}

fn read_limits(r: &mut Reader<'_>) -> Result<Limits, DecodeError> {
    Ok(Limits { memory_max: r.u64()?, cpu_quota: r.u64()?, cpu_period: r.u32()?, cpu_weight: r.u32()?, pids_max: r.u32()?, shm_size: r.u64()? })
}

/// One container's resource use, read from its cgroup and network namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContainerStats {
    /// CLOCK_BOOTTIME when sampled, in nanoseconds.
    pub read_ns: u64,
    pub mem_current: u64,
    /// 0 when unlimited.
    pub mem_max: u64,
    /// `inactive_file` from memory.stat: page cache Docker leaves out of usage.
    pub mem_inactive_file: u64,
    pub cpu_usage_usec: u64,
    pub cpu_user_usec: u64,
    pub cpu_system_usec: u64,
    /// Whole-VM CPU time from /proc/stat, in clock ticks (USER_HZ).
    pub system_ticks: u64,
    pub online_cpus: u32,
    pub pids: u32,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
}

fn put_container_stats(o: &mut Vec<u8>, s: &ContainerStats) {
    for v in [s.read_ns, s.mem_current, s.mem_max, s.mem_inactive_file, s.cpu_usage_usec, s.cpu_user_usec, s.cpu_system_usec, s.system_ticks] {
        put_u64(o, v);
    }
    put_u32(o, s.online_cpus);
    put_u32(o, s.pids);
    for v in [s.rx_bytes, s.rx_packets, s.tx_bytes, s.tx_packets, s.io_read_bytes, s.io_write_bytes] {
        put_u64(o, v);
    }
}

fn read_container_stats(r: &mut Reader<'_>) -> Result<ContainerStats, DecodeError> {
    Ok(ContainerStats {
        read_ns: r.u64()?,
        mem_current: r.u64()?,
        mem_max: r.u64()?,
        mem_inactive_file: r.u64()?,
        cpu_usage_usec: r.u64()?,
        cpu_user_usec: r.u64()?,
        cpu_system_usec: r.u64()?,
        system_ticks: r.u64()?,
        online_cpus: r.u32()?,
        pids: r.u32()?,
        rx_bytes: r.u64()?,
        rx_packets: r.u64()?,
        tx_bytes: r.u64()?,
        tx_packets: r.u64()?,
        io_read_bytes: r.u64()?,
        io_write_bytes: r.u64()?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout = 1,
    Stderr = 2,
}

/// One published port to forward in the kernel: `host_ip:host_port` (host
/// ip 0.0.0.0 = every local address) to `to_ip:to_port` in a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publish<'a> {
    pub id: &'a str,
    pub udp: bool,
    pub host_ip: [u8; 4],
    pub host_port: u16,
    pub to_ip: [u8; 4],
    pub to_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exited<'a> {
    pub id: &'a str,
    /// Exit status, or 128 + signal number if it was killed.
    pub code: i32,
    /// Guest-side time from receiving `Run` until the process was cloned.
    pub setup_ns: u64,
    /// Guest-side time from clone until the exit was reaped.
    pub run_ns: u64,
    /// The kernel killed a process in the container's cgroup for memory.
    pub oom_killed: bool,
}

/// A list of strings borrowed from a frame. Iterate it; it never allocates.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct StrList<'a> {
    count: u32,
    /// The encoded strings, each `u32 len | bytes`.
    raw: &'a [u8],
}

impl core::fmt::Debug for StrList<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> StrList<'a> {
    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a str> + 'a {
        let mut r = Reader { buf: self.raw, pos: 0 };
        (0..self.count).map_while(move |_| r.str().ok())
    }

    /// Encodes `items` as a list. Used by senders, which hold plain slices.
    pub fn put<S: AsRef<str>>(out: &mut Vec<u8>, items: &[S]) {
        out.extend_from_slice(&(items.len() as u32).to_le_bytes());
        for s in items {
            put_str(out, s.as_ref());
        }
    }
}

mod kind {
    pub const HELLO: u16 = 1;
    pub const PING: u16 = 2;
    pub const PONG: u16 = 3;
    pub const SHUTDOWN: u16 = 4;
    pub const ERROR: u16 = 5;
    pub const STATS_REQUEST: u16 = 6;
    pub const STATS: u16 = 7;
    pub const LAYER_QUERY: u16 = 8;
    pub const LAYER_STATUS: u16 = 9;
    pub const LAYER_BEGIN: u16 = 10;
    pub const LAYER_STORED: u16 = 11;
    pub const RUN: u16 = 12;
    pub const STARTED: u16 = 13;
    pub const OUTPUT: u16 = 14;
    pub const EXITED: u16 = 15;
    pub const KILL: u16 = 16;
    pub const REMOVE: u16 = 17;
    pub const SET_TIME: u16 = 18;
    pub const INPUT: u16 = 19;
    pub const CLOSE_STDIN: u16 = 20;
    pub const RESIZE: u16 = 21;
    pub const EXEC: u16 = 22;
    pub const RETAIN: u16 = 23;
    pub const FS_STAT_REQUEST: u16 = 24;
    pub const FS_STAT: u16 = 25;
    pub const FS_LIST_REQUEST: u16 = 26;
    pub const FS_LIST: u16 = 27;
    pub const FS_READ: u16 = 28;
    pub const FS_DATA: u16 = 29;
    pub const MEMORY_PRESSURE: u16 = 30;
    pub const REMOVE_VOLUME: u16 = 31;
    pub const COPY: u16 = 32;
    pub const COPY_STAT: u16 = 33;
    pub const COPY_DONE: u16 = 34;
    pub const UPDATE: u16 = 35;
    pub const FREEZE: u16 = 36;
    pub const STATS_QUERY: u16 = 37;
    pub const CONTAINER_STATS: u16 = 38;
    pub const TOP_QUERY: u16 = 39;
    pub const TOP: u16 = 40;
    pub const NET_ATTACH: u16 = 41;
    pub const NET_DETACH: u16 = 42;
    pub const ACK: u16 = 43;
    pub const INPUT_CONSUMED: u16 = 44;
    pub const PUBLISH: u16 = 45;
    pub const UNPUBLISH: u16 = 46;
    pub const LISTEN_QUERY: u16 = 47;
    pub const LISTENING: u16 = 48;
    pub const ROUTE: u16 = 49;
    pub const UPLINK_QUERY: u16 = 70;
    pub const UPLINK: u16 = 71;
    pub const REMOVE_BRIDGE: u16 = 72;
    pub const RETAIN_LAYERS: u16 = 73;
    pub const USAGE_QUERY: u16 = 74;
    pub const USAGE: u16 = 75;
    pub const USB_NODES_QUERY: u16 = 76;
    pub const USB_NODES: u16 = 77;
    pub const FS_EVENTS: u16 = 78;
    pub const AWAIT_START: u16 = 79;
    pub const CANCEL_START: u16 = 80;
    pub const DNS_RENAME: u16 = 81;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    TooLarge(usize),
    UnknownKind(u16),
    Truncated,
    BadUtf8,
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

/// Encodes an `Exec` frame straight from the sender's slices.
#[allow(clippy::too_many_arguments)]
pub fn encode_exec<S: AsRef<str>>(
    out: &mut Vec<u8>,
    container_id: &str,
    exec_id: &str,
    argv: &[S],
    env: &[S],
    cwd: &str,
    user: &str,
    flags: u32,
    caps: u64,
    opts: &[S],
) {
    frame(out, kind::EXEC, |out| {
        put_str(out, container_id);
        put_str(out, exec_id);
        StrList::put(out, argv);
        StrList::put(out, env);
        put_str(out, cwd);
        put_str(out, user);
        put_u32(out, flags);
        put_u64(out, caps);
        StrList::put(out, opts);
    });
}

/// Encodes a `NetAttach` frame from the sender's side.
#[allow(clippy::too_many_arguments)]
pub fn encode_net_attach<S: AsRef<str>>(out: &mut Vec<u8>, id: &str, ifname: &str, bridge: &str, ip: [u8; 4], prefix: u8, gateway: [u8; 4], dns_names: &[S], opts: &[S]) {
    frame(out, kind::NET_ATTACH, |o| {
        put_str(o, id);
        put_str(o, ifname);
        put_str(o, bridge);
        put_net_tail(o, ip, prefix, gateway);
        StrList::put::<&str>(o, &[]);
        StrList::put(o, dns_names);
        StrList::put(o, opts);
    });
}

/// Whether `frame` is of a kind the guest answers with no reply
/// (`DnsRename`): one that doesn't decode is then dropped, not answered
/// with an `Error` the host would read as the next request's reply.
pub fn takes_no_reply(frame: &[u8]) -> bool {
    frame.len() >= HEADER && u16::from_le_bytes([frame[4], frame[5]]) == kind::DNS_RENAME
}

/// Encodes a `DnsRename` frame from the sender's slices.
pub fn encode_dns_rename<S: AsRef<str>>(out: &mut Vec<u8>, id: &str, bridge: &str, ip: [u8; 4], ip6: Option<[u8; 16]>, v4: bool, names: &[S]) {
    frame(out, kind::DNS_RENAME, |o| {
        put_str(o, id);
        put_str(o, bridge);
        o.extend_from_slice(&ip);
        put_bytes(o, ip6.as_ref().map_or(&[][..], |a| &a[..]));
        o.push(v4 as u8);
        StrList::put(o, names);
    });
}

/// Encodes a `Usage` reply from the sender's lines.
pub fn encode_usage<S: AsRef<str>>(out: &mut Vec<u8>, lines: &[S]) {
    frame(out, kind::USAGE, |out| StrList::put(out, lines));
}

/// Encodes a `Top` reply from the sender's lines.
pub fn encode_top<S: AsRef<str>>(out: &mut Vec<u8>, lines: &[S]) {
    frame(out, kind::TOP, |out| StrList::put(out, lines));
}

/// Encodes a `UsbNodes` reply from the sender's lines.
pub fn encode_usb_nodes<S: AsRef<str>>(out: &mut Vec<u8>, lines: &[S]) {
    frame(out, kind::USB_NODES, |out| StrList::put(out, lines));
}

/// Encodes an `FsEvents` frame from the sender's `OP\tPATH` lines.
pub fn encode_fs_events<S: AsRef<str>>(out: &mut Vec<u8>, lines: &[S]) {
    frame(out, kind::FS_EVENTS, |out| StrList::put(out, lines));
}

/// Encodes a `Listening` reply from the sender's lines.
pub fn encode_listening<S: AsRef<str>>(out: &mut Vec<u8>, lines: &[S]) {
    frame(out, kind::LISTENING, |out| StrList::put(out, lines));
}

/// Encodes a `Retain` frame from the sender's ids.
pub fn encode_retain<S: AsRef<str>>(out: &mut Vec<u8>, ids: &[S]) {
    frame(out, kind::RETAIN, |out| StrList::put(out, ids));
}

/// Encodes a `RetainLayers` frame from the sender's layer digests.
pub fn encode_retain_layers<S: AsRef<str>>(out: &mut Vec<u8>, digests: &[S]) {
    frame(out, kind::RETAIN_LAYERS, |out| StrList::put(out, digests));
}

/// A container's network, from the sender's side.
#[derive(Debug, Clone, Copy)]
pub struct Net<'a, S> {
    pub bridge: &'a str,
    pub ip: [u8; 4],
    pub prefix: u8,
    pub gateway: [u8; 4],
    pub extra_hosts: &'a [S],
    pub dns_names: &'a [S],
    /// `key=value` settings; see [`net_opts`].
    pub opts: &'a [S],
}

fn put_net_tail(out: &mut Vec<u8>, ip: [u8; 4], prefix: u8, gateway: [u8; 4]) {
    out.extend_from_slice(&ip);
    out.push(prefix);
    out.extend_from_slice(&gateway);
}

/// Encodes a `Run` frame straight from the sender's slices, without first
/// building a `RunSpec` (whose lists borrow from an encoded frame).
#[allow(clippy::too_many_arguments)]
pub fn encode_run<S: AsRef<str>>(
    out: &mut Vec<u8>,
    id: &str,
    layers: &[S],
    argv: &[S],
    env: &[S],
    cwd: &str,
    user: &str,
    hostname: &str,
    flags: u32,
    net: Net<'_, S>,
    mounts: &[S],
    limits: &Limits,
    caps: u64,
    opts: &[S],
    seccomp: &[u8],
) {
    frame(out, kind::RUN, |out| {
        put_str(out, id);
        StrList::put(out, layers);
        StrList::put(out, argv);
        StrList::put(out, env);
        put_str(out, cwd);
        put_str(out, user);
        put_str(out, hostname);
        put_u32(out, flags);
        put_str(out, net.bridge);
        put_net_tail(out, net.ip, net.prefix, net.gateway);
        StrList::put(out, net.extra_hosts);
        StrList::put(out, net.dns_names);
        StrList::put(out, mounts);
        put_limits(out, limits);
        put_u64(out, caps);
        StrList::put(out, net.opts);
        StrList::put(out, opts);
        put_bytes(out, seccomp);
    });
}

fn frame(out: &mut Vec<u8>, kind: u16, body: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend_from_slice(&[0; HEADER]);
    body(out);
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    out[start + 4..start + HEADER].copy_from_slice(&kind.to_le_bytes());
}

impl Msg<'_> {
    /// Appends one encoded frame to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Msg::Hello(h) => frame(out, kind::HELLO, |o| {
                o.extend_from_slice(&h.protocol.to_le_bytes());
                for v in [h.init_start_ns, h.ready_ns, h.mem_total_kib, h.mem_available_kib] {
                    put_u64(o, v);
                }
            }),
            Msg::Ping(v) => frame(out, kind::PING, |o| put_u64(o, v)),
            Msg::Pong(v) => frame(out, kind::PONG, |o| put_u64(o, v)),
            Msg::Shutdown => frame(out, kind::SHUTDOWN, |_| {}),
            Msg::Error(s) => frame(out, kind::ERROR, |o| o.extend_from_slice(s.as_bytes())),
            Msg::StatsRequest => frame(out, kind::STATS_REQUEST, |_| {}),
            Msg::Stats(st) => frame(out, kind::STATS, |o| {
                for v in [st.uptime_ns, st.mem_total_kib, st.mem_available_kib, st.agent_rss_kib, st.mem_used_kib, st.mem_stall, st.oom_kills] {
                    put_u64(o, v);
                }
                // Always emit the 8th word (u64::MAX for None) so any future
                // trailing fields stay at fixed offsets.
                put_u64(o, st.mem_balloon_kib.unwrap_or(u64::MAX));
            }),
            Msg::LayerQuery(d) => frame(out, kind::LAYER_QUERY, |o| put_str(o, d)),
            Msg::LayerStatus(have) => frame(out, kind::LAYER_STATUS, |o| o.push(have as u8)),
            Msg::LayerBegin(l) => frame(out, kind::LAYER_BEGIN, |o| {
                put_str(o, l.digest);
                put_str(o, l.media_type);
                put_u64(o, l.size);
            }),
            Msg::LayerStored(l) => frame(out, kind::LAYER_STORED, |o| {
                put_u64(o, l.unpacked_bytes);
                put_u64(o, l.elapsed_ns);
            }),
            Msg::Run(r) => frame(out, kind::RUN, |o| {
                put_str(o, r.id);
                for list in [r.layers, r.argv, r.env] {
                    put_u32(o, list.count);
                    o.extend_from_slice(list.raw);
                }
                put_str(o, r.cwd);
                put_str(o, r.user);
                put_str(o, r.hostname);
                put_u32(o, r.flags);
                put_str(o, r.net.bridge);
                put_net_tail(o, r.net.ip, r.net.prefix, r.net.gateway);
                for list in [r.net.extra_hosts, r.net.dns_names, r.mounts] {
                    put_u32(o, list.count);
                    o.extend_from_slice(list.raw);
                }
                put_limits(o, &r.limits);
                put_u64(o, r.caps);
                put_u32(o, r.net.opts.count);
                o.extend_from_slice(r.net.opts.raw);
                put_u32(o, r.opts.count);
                o.extend_from_slice(r.opts.raw);
                put_bytes(o, r.seccomp);
            }),
            Msg::Started { id, pid, starting } => frame(out, kind::STARTED, |o| {
                put_str(o, id);
                put_u32(o, pid);
                // Only when set: a normal start's frame stays as it was.
                if starting {
                    o.push(1);
                }
            }),
            Msg::AwaitStart { id } => frame(out, kind::AWAIT_START, |o| put_str(o, id)),
            Msg::CancelStart { id } => frame(out, kind::CANCEL_START, |o| put_str(o, id)),
            Msg::Output { id, stream, data } => frame(out, kind::OUTPUT, |o| {
                put_str(o, id);
                o.push(stream as u8);
                put_bytes(o, data);
            }),
            Msg::Kill { id, signal } => frame(out, kind::KILL, |o| {
                put_str(o, id);
                put_u32(o, signal);
            }),
            Msg::Remove { id } => frame(out, kind::REMOVE, |o| put_str(o, id)),
            Msg::SetTime(ns) => frame(out, kind::SET_TIME, |o| put_u64(o, ns)),
            Msg::Input { id, data } => frame(out, kind::INPUT, |o| {
                put_str(o, id);
                put_bytes(o, data);
            }),
            Msg::CloseStdin { id } => frame(out, kind::CLOSE_STDIN, |o| put_str(o, id)),
            Msg::Resize { id, rows, cols } => frame(out, kind::RESIZE, |o| {
                put_str(o, id);
                o.extend_from_slice(&rows.to_le_bytes());
                o.extend_from_slice(&cols.to_le_bytes());
            }),
            Msg::Exec(e) => frame(out, kind::EXEC, |o| {
                put_str(o, e.container_id);
                put_str(o, e.exec_id);
                for list in [e.argv, e.env] {
                    put_u32(o, list.count);
                    o.extend_from_slice(list.raw);
                }
                put_str(o, e.cwd);
                put_str(o, e.user);
                put_u32(o, e.flags);
                put_u64(o, e.caps);
                put_u32(o, e.opts.count);
                o.extend_from_slice(e.opts.raw);
            }),
            Msg::Retain(ids) => frame(out, kind::RETAIN, |o| {
                put_u32(o, ids.count);
                o.extend_from_slice(ids.raw);
            }),
            Msg::UsageQuery => frame(out, kind::USAGE_QUERY, |_| {}),
            Msg::Usage(lines) => frame(out, kind::USAGE, |o| {
                put_u32(o, lines.count);
                o.extend_from_slice(lines.raw);
            }),
            Msg::RetainLayers(ids) => frame(out, kind::RETAIN_LAYERS, |o| {
                put_u32(o, ids.count);
                o.extend_from_slice(ids.raw);
            }),
            Msg::FsStatRequest { id, path } => frame(out, kind::FS_STAT_REQUEST, |o| {
                put_str(o, id);
                put_str(o, path);
            }),
            Msg::FsStat(e) => frame(out, kind::FS_STAT, |o| put_entry(o, &e)),
            Msg::FsListRequest { id, path } => frame(out, kind::FS_LIST_REQUEST, |o| {
                put_str(o, id);
                put_str(o, path);
            }),
            Msg::FsList { count, raw } => frame(out, kind::FS_LIST, |o| {
                put_u32(o, count);
                o.extend_from_slice(raw);
            }),
            Msg::FsRead { id, path, offset, len } => frame(out, kind::FS_READ, |o| {
                put_str(o, id);
                put_str(o, path);
                put_u64(o, offset);
                put_u32(o, len);
            }),
            Msg::FsData(d) => frame(out, kind::FS_DATA, |o| put_bytes(o, d)),
            Msg::MemoryPressure => frame(out, kind::MEMORY_PRESSURE, |_| {}),
            Msg::RemoveVolume { name } => frame(out, kind::REMOVE_VOLUME, |o| put_str(o, name)),
            Msg::RemoveBridge { bridge } => frame(out, kind::REMOVE_BRIDGE, |o| put_str(o, bridge)),
            Msg::DnsRename { id, bridge, ip, ip6, v4, names } => frame(out, kind::DNS_RENAME, |o| {
                put_str(o, id);
                put_str(o, bridge);
                o.extend_from_slice(&ip);
                put_bytes(o, ip6);
                o.push(v4 as u8);
                put_u32(o, names.count);
                o.extend_from_slice(names.raw);
            }),
            Msg::Copy(c) => frame(out, kind::COPY, |o| {
                o.push(c.op);
                put_str(o, c.id);
                put_str(o, c.path);
                put_u32(o, c.flags);
                put_u32(o, c.layers.count);
                o.extend_from_slice(c.layers.raw);
            }),
            Msg::CopyStat { entry, link } => frame(out, kind::COPY_STAT, |o| {
                put_entry(o, &entry);
                put_str(o, link);
            }),
            Msg::CopyDone => frame(out, kind::COPY_DONE, |_| {}),
            Msg::Update { id, limits } => frame(out, kind::UPDATE, |o| {
                put_str(o, id);
                put_limits(o, &limits);
            }),
            Msg::Freeze { id, frozen } => frame(out, kind::FREEZE, |o| {
                put_str(o, id);
                o.push(u8::from(frozen));
            }),
            Msg::StatsQuery { id } => frame(out, kind::STATS_QUERY, |o| put_str(o, id)),
            Msg::ContainerStats(st) => frame(out, kind::CONTAINER_STATS, |o| put_container_stats(o, &st)),
            Msg::TopQuery { id } => frame(out, kind::TOP_QUERY, |o| put_str(o, id)),
            Msg::Top(lines) => frame(out, kind::TOP, |o| {
                put_u32(o, lines.count);
                o.extend_from_slice(lines.raw);
            }),
            Msg::ListenQuery { id } => frame(out, kind::LISTEN_QUERY, |o| put_str(o, id)),
            Msg::UsbNodesQuery => frame(out, kind::USB_NODES_QUERY, |_| {}),
            Msg::UsbNodes(lines) => frame(out, kind::USB_NODES, |o| {
                put_u32(o, lines.count);
                o.extend_from_slice(lines.raw);
            }),
            Msg::FsEvents(lines) => frame(out, kind::FS_EVENTS, |o| {
                put_u32(o, lines.count);
                o.extend_from_slice(lines.raw);
            }),
            Msg::Listening(lines) => frame(out, kind::LISTENING, |o| {
                put_u32(o, lines.count);
                o.extend_from_slice(lines.raw);
            }),
            Msg::NetAttach { id, ifname, net } => frame(out, kind::NET_ATTACH, |o| {
                put_str(o, id);
                put_str(o, ifname);
                put_str(o, net.bridge);
                put_net_tail(o, net.ip, net.prefix, net.gateway);
                for list in [net.extra_hosts, net.dns_names, net.opts] {
                    put_u32(o, list.count);
                    o.extend_from_slice(list.raw);
                }
            }),
            Msg::NetDetach { id, ifname, bridge, ip } => frame(out, kind::NET_DETACH, |o| {
                put_str(o, id);
                put_str(o, ifname);
                put_str(o, bridge);
                o.extend_from_slice(&ip);
            }),
            Msg::Route { dst, prefix, via, add } => frame(out, kind::ROUTE, |o| {
                o.extend_from_slice(&dst);
                o.push(prefix);
                o.extend_from_slice(&via);
                o.push(add as u8);
            }),
            Msg::UplinkQuery => frame(out, kind::UPLINK_QUERY, |_| {}),
            Msg::Uplink { ip, gateway } => frame(out, kind::UPLINK, |o| {
                o.extend_from_slice(&ip);
                o.extend_from_slice(&gateway);
            }),
            Msg::Ack => frame(out, kind::ACK, |_| {}),
            Msg::InputConsumed { id, bytes } => frame(out, kind::INPUT_CONSUMED, |o| {
                put_str(o, id);
                put_u32(o, bytes);
            }),
            Msg::Publish(p) => frame(out, kind::PUBLISH, |o| {
                put_str(o, p.id);
                put_u32(o, u32::from(p.udp));
                put_u32(o, u32::from_be_bytes(p.host_ip));
                put_u32(o, u32::from(p.host_port));
                put_u32(o, u32::from_be_bytes(p.to_ip));
                put_u32(o, u32::from(p.to_port));
            }),
            Msg::Unpublish { id } => frame(out, kind::UNPUBLISH, |o| put_str(o, id)),
            Msg::Exited(e) => frame(out, kind::EXITED, |o| {
                put_str(o, e.id);
                put_u32(o, e.code as u32);
                put_u64(o, e.setup_ns);
                put_u64(o, e.run_ns);
                o.push(e.oom_killed as u8);
            }),
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(DecodeError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_| DecodeError::Truncated)?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| DecodeError::Truncated)?))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| DecodeError::Truncated)?))
    }

    fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    fn str(&mut self) -> Result<&'a str, DecodeError> {
        core::str::from_utf8(self.bytes()?).map_err(|_| DecodeError::BadUtf8)
    }

    /// Validates a list and returns it as a borrowed view.
    fn list(&mut self) -> Result<StrList<'a>, DecodeError> {
        let count = self.u32()?;
        let start = self.pos;
        for _ in 0..count {
            self.str()?;
        }
        Ok(StrList { count, raw: &self.buf[start..self.pos] })
    }

    fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
}

/// Returns the total size of the frame at the start of `buf` once its header
/// is available, so readers can size exactly one read for the body.
pub fn frame_len(header: &[u8; 4]) -> Result<usize, DecodeError> {
    let len = u32::from_le_bytes(*header) as usize;
    if len < 2 || len + 4 > MAX_FRAME {
        return Err(DecodeError::TooLarge(len));
    }
    Ok(len + 4)
}

/// Decodes one complete frame (header included).
pub fn decode(frame: &[u8]) -> Result<Msg<'_>, DecodeError> {
    if frame.len() < HEADER {
        return Err(DecodeError::Truncated);
    }
    let kind = u16::from_le_bytes([frame[4], frame[5]]);
    let mut r = Reader { buf: &frame[HEADER..], pos: 0 };
    Ok(match kind {
        kind::HELLO => Msg::Hello(Hello {
            protocol: r.u16()?,
            init_start_ns: r.u64()?,
            ready_ns: r.u64()?,
            mem_total_kib: r.u64()?,
            mem_available_kib: r.u64()?,
        }),
        kind::PING => Msg::Ping(r.u64()?),
        kind::PONG => Msg::Pong(r.u64()?),
        kind::SHUTDOWN => Msg::Shutdown,
        kind::ERROR => Msg::Error(core::str::from_utf8(r.rest()).map_err(|_| DecodeError::BadUtf8)?),
        kind::STATS_REQUEST => Msg::StatsRequest,
        kind::STATS => Msg::Stats(Stats {
            uptime_ns: r.u64()?,
            mem_total_kib: r.u64()?,
            mem_available_kib: r.u64()?,
            agent_rss_kib: r.u64()?,
            // An agent from before the field sends four words.
            mem_used_kib: if r.remaining() >= 8 { r.u64()? } else { 0 },
            mem_stall: if r.remaining() >= 8 { r.u64()? } else { 0 },
            oom_kills: if r.remaining() >= 8 { r.u64()? } else { 0 },
            mem_balloon_kib: if r.remaining() >= 8 {
                let v = r.u64()?;
                (v != u64::MAX).then_some(v)
            } else {
                None
            },
        }),
        kind::LAYER_QUERY => Msg::LayerQuery(r.str()?),
        kind::LAYER_STATUS => Msg::LayerStatus(r.u8()? != 0),
        kind::LAYER_BEGIN => Msg::LayerBegin(LayerBegin { digest: r.str()?, media_type: r.str()?, size: r.u64()? }),
        kind::LAYER_STORED => Msg::LayerStored(LayerStored { unpacked_bytes: r.u64()?, elapsed_ns: r.u64()? }),
        kind::RUN => Msg::Run({
            let mut run = RunSpec {
                id: r.str()?,
                layers: r.list()?,
                argv: r.list()?,
                env: r.list()?,
                cwd: r.str()?,
                user: r.str()?,
                hostname: r.str()?,
                flags: r.u32()?,
                net: NetSpec {
                    bridge: r.str()?,
                    ip: r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)?,
                    prefix: r.u8()?,
                    gateway: r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)?,
                    extra_hosts: r.list()?,
                    dns_names: r.list()?,
                    opts: StrList::default(),
                },
                mounts: r.list()?,
                limits: read_limits(&mut r)?,
                caps: r.u64()?,
                opts: StrList::default(),
                seccomp: &[],
            };
            // Last, and absent from an older daemon.
            if r.remaining() >= 4 {
                run.net.opts = r.list()?;
            }
            if r.remaining() >= 4 {
                run.opts = r.list()?;
            }
            if r.remaining() >= 4 {
                run.seccomp = r.bytes()?;
            }
            run
        }),
        kind::STARTED => Msg::Started {
            id: r.str()?,
            pid: r.u32()?,
            // An older agent doesn't say: its root was in place.
            starting: r.remaining() >= 1 && r.u8()? != 0,
        },
        kind::AWAIT_START => Msg::AwaitStart { id: r.str()? },
        kind::CANCEL_START => Msg::CancelStart { id: r.str()? },
        kind::OUTPUT => {
            let id = r.str()?;
            let stream = match r.u8()? {
                1 => Stream::Stdout,
                _ => Stream::Stderr,
            };
            Msg::Output { id, stream, data: r.bytes()? }
        }
        kind::EXITED => Msg::Exited(Exited {
            id: r.str()?,
            code: r.u32()? as i32,
            setup_ns: r.u64()?,
            run_ns: r.u64()?,
            // An older agent doesn't say.
            oom_killed: r.remaining() >= 1 && r.u8()? != 0,
        }),
        kind::KILL => Msg::Kill { id: r.str()?, signal: r.u32()? },
        kind::REMOVE => Msg::Remove { id: r.str()? },
        kind::SET_TIME => Msg::SetTime(r.u64()?),
        kind::INPUT => Msg::Input { id: r.str()?, data: r.bytes()? },
        kind::CLOSE_STDIN => Msg::CloseStdin { id: r.str()? },
        kind::RESIZE => Msg::Resize { id: r.str()?, rows: r.u16()?, cols: r.u16()? },
        kind::EXEC => Msg::Exec(ExecSpec {
            container_id: r.str()?,
            exec_id: r.str()?,
            argv: r.list()?,
            env: r.list()?,
            cwd: r.str()?,
            user: r.str()?,
            flags: r.u32()?,
            caps: r.u64()?,
            opts: if r.remaining() >= 4 { r.list()? } else { StrList { count: 0, raw: &[] } },
        }),
        kind::RETAIN => Msg::Retain(r.list()?),
        kind::RETAIN_LAYERS => Msg::RetainLayers(r.list()?),
        kind::USAGE_QUERY => Msg::UsageQuery,
        kind::USAGE => Msg::Usage(r.list()?),
        kind::FS_STAT_REQUEST => Msg::FsStatRequest { id: r.str()?, path: r.str()? },
        kind::FS_STAT => Msg::FsStat(read_entry(&mut r)?),
        kind::FS_LIST_REQUEST => Msg::FsListRequest { id: r.str()?, path: r.str()? },
        kind::FS_LIST => {
            let count = r.u32()?;
            let start = r.pos;
            for _ in 0..count {
                read_entry(&mut r)?;
            }
            Msg::FsList { count, raw: &r.buf[start..r.pos] }
        }
        kind::FS_READ => Msg::FsRead { id: r.str()?, path: r.str()?, offset: r.u64()?, len: r.u32()? },
        kind::FS_DATA => Msg::FsData(r.bytes()?),
        kind::MEMORY_PRESSURE => Msg::MemoryPressure,
        kind::REMOVE_VOLUME => Msg::RemoveVolume { name: r.str()? },
        kind::REMOVE_BRIDGE => Msg::RemoveBridge { bridge: r.str()? },
        kind::DNS_RENAME => {
            let id = r.str()?;
            let bridge = r.str()?;
            let ip = r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)?;
            let ip6 = r.bytes()?;
            let v4 = r.u8()? != 0;
            let names = r.list()?;
            // The address is empty or a whole IPv6 address, and the list is
            // bounded, so the guest never reads a half address or a frame of
            // endless names.
            if !(ip6.is_empty() || ip6.len() == 16) || names.len() > MAX_DNS_NAMES {
                return Err(DecodeError::Truncated);
            }
            Msg::DnsRename { id, bridge, ip, ip6, v4, names }
        }
        kind::COPY => Msg::Copy(CopySpec { op: r.u8()?, id: r.str()?, path: r.str()?, flags: r.u32()?, layers: r.list()? }),
        kind::COPY_STAT => Msg::CopyStat { entry: read_entry(&mut r)?, link: r.str()? },
        kind::COPY_DONE => Msg::CopyDone,
        kind::UPDATE => Msg::Update { id: r.str()?, limits: read_limits(&mut r)? },
        kind::FREEZE => Msg::Freeze { id: r.str()?, frozen: r.u8()? != 0 },
        kind::STATS_QUERY => Msg::StatsQuery { id: r.str()? },
        kind::CONTAINER_STATS => Msg::ContainerStats(read_container_stats(&mut r)?),
        kind::TOP_QUERY => Msg::TopQuery { id: r.str()? },
        kind::TOP => Msg::Top(r.list()?),
        kind::LISTEN_QUERY => Msg::ListenQuery { id: r.str()? },
        kind::LISTENING => Msg::Listening(r.list()?),
        kind::USB_NODES_QUERY => Msg::UsbNodesQuery,
        kind::USB_NODES => Msg::UsbNodes(r.list()?),
        kind::FS_EVENTS => Msg::FsEvents(r.list()?),
        kind::NET_ATTACH => Msg::NetAttach {
            id: r.str()?,
            ifname: r.str()?,
            net: NetSpec {
                bridge: r.str()?,
                ip: r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)?,
                prefix: r.u8()?,
                gateway: r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)?,
                extra_hosts: r.list()?,
                dns_names: r.list()?,
                opts: if r.remaining() >= 4 { r.list()? } else { StrList::default() },
            },
        },
        kind::ROUTE => Msg::Route {
            dst: r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)?,
            prefix: r.u8()?,
            via: r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)?,
            add: r.u8()? != 0,
        },
        kind::NET_DETACH => Msg::NetDetach { id: r.str()?, ifname: r.str()?, bridge: r.str()?, ip: r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)? },
        kind::UPLINK_QUERY => Msg::UplinkQuery,
        kind::UPLINK => Msg::Uplink { ip: r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)?, gateway: r.take(4)?.try_into().map_err(|_| DecodeError::Truncated)? },
        kind::ACK => Msg::Ack,
        kind::INPUT_CONSUMED => Msg::InputConsumed { id: r.str()?, bytes: r.u32()? },
        kind::PUBLISH => Msg::Publish(Publish {
            id: r.str()?,
            udp: r.u32()? != 0,
            host_ip: r.u32()?.to_be_bytes(),
            host_port: r.u32()? as u16,
            to_ip: r.u32()?.to_be_bytes(),
            to_port: r.u32()? as u16,
        }),
        kind::UNPUBLISH => Msg::Unpublish { id: r.str()? },
        other => return Err(DecodeError::UnknownKind(other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(m: Msg<'_>) {
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(frame_len(buf[..4].try_into().unwrap()).unwrap(), buf.len());
        assert_eq!(decode(&buf).unwrap(), m);
    }

    #[test]
    fn round_trip() {
        round(Msg::Hello(Hello { protocol: PROTOCOL, init_start_ns: 1, ready_ns: 2, mem_total_kib: 3, mem_available_kib: 4 }));
        round(Msg::Ping(7));
        round(Msg::Pong(7));
        round(Msg::Shutdown);
        round(Msg::Error("boom"));
        round(Msg::StatsRequest);
        let full = Stats { uptime_ns: 5, mem_total_kib: 6, mem_available_kib: 7, agent_rss_kib: 8, mem_used_kib: 9, mem_stall: 9173, oom_kills: 2, mem_balloon_kib: Some(4096) };
        round(Msg::Stats(full));
        round(Msg::Stats(Stats { mem_balloon_kib: Some(0), ..full }));
        round(Msg::Stats(Stats { mem_balloon_kib: None, ..full }));
        // Older agents' Stats, without the later words, still decode.
        let mut buf = Vec::new();
        Msg::Stats(full).encode(&mut buf);
        let no_balloon = Stats { mem_balloon_kib: None, ..full };
        assert_eq!(decode(&buf[..buf.len() - 8]).unwrap(), Msg::Stats(no_balloon));
        let no_memory_words = Stats { mem_stall: 0, oom_kills: 0, ..no_balloon };
        assert_eq!(decode(&buf[..buf.len() - 24]).unwrap(), Msg::Stats(no_memory_words));
        assert_eq!(decode(&buf[..buf.len() - 32]).unwrap(), Msg::Stats(Stats { mem_used_kib: 0, ..no_memory_words }));
        round(Msg::LayerQuery("sha256:ab"));
        round(Msg::LayerStatus(true));
        round(Msg::LayerBegin(LayerBegin { digest: "sha256:ab", media_type: "x+gzip", size: 9 }));
        round(Msg::LayerStored(LayerStored { unpacked_bytes: 1, elapsed_ns: 2 }));
        round(Msg::Started { id: "c1", pid: 42, starting: false });
        round(Msg::Started { id: "c1", pid: 42, starting: true });
        // A normal start's frame is the one older agents send; and an older
        // agent's Started, without the flag, reads as root in place.
        let mut buf = Vec::new();
        Msg::Started { id: "c1", pid: 42, starting: true }.encode(&mut buf);
        let mut old = buf[..buf.len() - 1].to_vec();
        let len = (old.len() - 4) as u32;
        old[..4].copy_from_slice(&len.to_le_bytes());
        assert_eq!(decode(&old).unwrap(), Msg::Started { id: "c1", pid: 42, starting: false });
        let mut normal = Vec::new();
        Msg::Started { id: "c1", pid: 42, starting: false }.encode(&mut normal);
        assert_eq!(normal, old, "a normal start's frame is unchanged");
        round(Msg::AwaitStart { id: "c1" });
        round(Msg::CancelStart { id: "c1" });
        round(Msg::Output { id: "c1", stream: Stream::Stderr, data: b"hi\n" });
        round(Msg::Exited(Exited { id: "c1", code: -1, setup_ns: 3, run_ns: 4, oom_killed: false }));
        round(Msg::Exited(Exited { id: "c1", code: 137, setup_ns: 3, run_ns: 4, oom_killed: true }));
        // An older agent's Exited, without the flag, reads as not killed for memory.
        let mut buf = Vec::new();
        Msg::Exited(Exited { id: "c1", code: 137, setup_ns: 3, run_ns: 4, oom_killed: true }).encode(&mut buf);
        assert_eq!(decode(&buf[..buf.len() - 1]).unwrap(), Msg::Exited(Exited { id: "c1", code: 137, setup_ns: 3, run_ns: 4, oom_killed: false }));
        round(Msg::Kill { id: "c1", signal: 15 });
        round(Msg::Remove { id: "c1" });
        round(Msg::SetTime(1_792_800_000_000_000_000));
        round(Msg::Input { id: "c1", data: b"ls\n" });
        round(Msg::CloseStdin { id: "c1" });
        round(Msg::Resize { id: "c1", rows: 40, cols: 120 });
        round(Msg::InputConsumed { id: "c1", bytes: 65_536 });
        round(Msg::Publish(Publish { id: "c1", udp: true, host_ip: [0, 0, 0, 0], host_port: 8080, to_ip: [172, 17, 0, 2], to_port: 80 }));
        round(Msg::Unpublish { id: "c1" });
    }

    #[test]
    fn stats_none_balloon_keeps_wire_offset() {
        let base = Stats { uptime_ns: 5, mem_total_kib: 6, mem_available_kib: 7, agent_rss_kib: 8, mem_used_kib: 9, mem_stall: 10, oom_kills: 1, mem_balloon_kib: Some(4096) };
        let absent = Stats { mem_balloon_kib: None, ..base };
        let mut with_balloon = Vec::new();
        let mut without_balloon = Vec::new();
        Msg::Stats(base).encode(&mut with_balloon);
        Msg::Stats(absent).encode(&mut without_balloon);
        assert_eq!(without_balloon.len(), with_balloon.len());
        // Appending a future 9th word must not be decoded into mem_balloon_kib.
        let mut extended = without_balloon.clone();
        extended.extend_from_slice(&42u64.to_le_bytes());
        let len = (extended.len() - 4) as u32;
        extended[..4].copy_from_slice(&len.to_le_bytes());
        assert_eq!(decode(&extended).unwrap(), Msg::Stats(absent));
        // An older 7-word agent omitting the 8th word still decodes as None.
        let mut old = without_balloon[..without_balloon.len() - 8].to_vec();
        let len = (old.len() - 4) as u32;
        old[..4].copy_from_slice(&len.to_le_bytes());
        assert_eq!(decode(&old).unwrap(), Msg::Stats(absent));
    }

    #[test]
    fn run_spec_lists_survive_encoding() {
        let mut buf = Vec::new();
        let net = Net { bridge: "kiri0", ip: [172, 17, 0, 2], prefix: 16, gateway: [172, 17, 0, 1], extra_hosts: &["1.2.3.4 x"], dns_names: &["web"], opts: &["netns=c0", "nameserver=8.8.8.8"] };
        let limits = Limits { memory_max: 256 << 20, cpu_quota: 150_000, cpu_period: 100_000, cpu_weight: 100, pids_max: 64, shm_size: 1 << 20 };
        encode_run(&mut buf, "c1", &["sha256:top", "sha256:base"], &["echo", "hi"], &["A=1"], "/", "", "c1", 3, net, &["/a\t/b\tro"], &limits, 0b101, &["rlimit\t7\t1024\t2048"], &[]);
        let Msg::Run(r) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(r.id, "c1");
        assert_eq!(r.layers.iter().collect::<Vec<_>>(), ["sha256:top", "sha256:base"]);
        assert_eq!(r.argv.iter().collect::<Vec<_>>(), ["echo", "hi"]);
        assert_eq!(r.env.len(), 1);
        assert_eq!((r.cwd, r.user, r.hostname, r.flags), ("/", "", "c1", 3));
        assert_eq!((r.net.bridge, r.net.ip, r.net.prefix, r.net.gateway), ("kiri0", [172, 17, 0, 2], 16, [172, 17, 0, 1]));
        assert_eq!(r.net.extra_hosts.iter().collect::<Vec<_>>(), ["1.2.3.4 x"]);
        assert_eq!(r.mounts.iter().collect::<Vec<_>>(), ["/a\t/b\tro"]);
        assert_eq!(r.net.opts.iter().collect::<Vec<_>>(), ["netns=c0", "nameserver=8.8.8.8"]);
        assert_eq!(net_opts::values(r.net.opts, net_opts::NAMESERVER).collect::<Vec<_>>(), ["8.8.8.8"]);
        assert_eq!(r.opts.iter().collect::<Vec<_>>(), ["rlimit\t7\t1024\t2048"]);
        assert!(r.seccomp.is_empty());
        // Re-encoding the borrowed view gives identical bytes.
        let mut again = Vec::new();
        Msg::Run(r).encode(&mut again);
        assert_eq!(buf, again);
    }

    #[test]
    fn the_protocol_version_covers_cancel_start() {
        // An agent from before `CancelStart` answers it with an error that
        // the host would take as the reply to its next request.
        const { assert!(PROTOCOL >= 7) };
    }

    #[test]
    fn the_protocol_version_covers_starting() {
        // A host from before `Started::starting` would take a start still
        // filling a new volume for started and never ask `AwaitStart`; it
        // speaks 5, and refuses this agent (and this host refuses an agent
        // that speaks 5, which never answers `starting`).
        const { assert!(PROTOCOL >= 6) };
        let mut buf = Vec::new();
        Msg::Hello(Hello { protocol: PROTOCOL, init_start_ns: 1, ready_ns: 2, mem_total_kib: 3, mem_available_kib: 4 }).encode(&mut buf);
        let Msg::Hello(h) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(h.protocol, PROTOCOL);
    }

    #[test]
    fn the_protocol_version_covers_the_seccomp_field() {
        // An agent from before `RunSpec::seccomp` says 4, and the host refuses
        // it (guest.rs compares the Hello's version with this one).
        assert!(PROTOCOL >= 5);
        let mut buf = Vec::new();
        Msg::Hello(Hello { protocol: PROTOCOL, init_start_ns: 1, ready_ns: 2, mem_total_kib: 3, mem_available_kib: 4 }).encode(&mut buf);
        let Msg::Hello(h) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(h.protocol, PROTOCOL);
    }

    #[test]
    fn run_spec_carries_a_seccomp_program() {
        let net = Net { bridge: "", ip: [0; 4], prefix: 0, gateway: [0; 4], extra_hosts: &[] as &[&str], dns_names: &[], opts: &[] };
        // Two instructions of eight bytes: a binary blob, not text.
        let program: Vec<u8> = vec![0x20, 0, 0, 0, 4, 0, 0, 0, 0x06, 0, 0, 0, 0, 0, 0xff, 0x7f];
        let mut buf = Vec::new();
        encode_run(&mut buf, "c1", &[], &["sh"], &[], "/", "", "c1", 0, net, &[], &Limits::default(), 0, &["group\tvideo"], &program);
        let Msg::Run(r) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(r.seccomp, &program[..]);
        assert_eq!(r.opts.iter().collect::<Vec<_>>(), ["group\tvideo"]);
        let mut again = Vec::new();
        Msg::Run(r).encode(&mut again);
        assert_eq!(buf, again);
        // A daemon from before the field sends none: the default applies.
        let Msg::Run(old) = decode(&buf[..buf.len() - 4 - program.len()]).unwrap() else { panic!() };
        assert!(old.seccomp.is_empty());
        assert_eq!(old.opts.len(), 1);
    }

    #[test]
    fn exec_and_retain_survive_encoding() {
        let mut buf = Vec::new();
        encode_exec(&mut buf, "c1", "e1", &["sh", "-c", "id"], &["A=1"], "/", "root", run_flags::TTY, 0, &["group\tvideo"]);
        let Msg::Exec(e) = decode(&buf).unwrap() else { panic!() };
        assert_eq!((e.container_id, e.exec_id, e.cwd, e.user, e.flags), ("c1", "e1", "/", "root", run_flags::TTY));
        assert_eq!(e.argv.iter().collect::<Vec<_>>(), ["sh", "-c", "id"]);
        assert_eq!(e.opts.iter().collect::<Vec<_>>(), ["group\tvideo"]);
        // An older daemon's Exec, without the list, decodes with none.
        let Msg::Exec(old) = decode(&buf[..buf.len() - 4 - 4 - "group\tvideo".len()]).unwrap() else { panic!() };
        assert!(old.opts.is_empty());
        let mut again = Vec::new();
        Msg::Exec(e).encode(&mut again);
        assert_eq!(buf, again);

        buf.clear();
        encode_retain(&mut buf, &["a", "b"]);
        let Msg::Retain(ids) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(ids.iter().collect::<Vec<_>>(), ["a", "b"]);

        buf.clear();
        encode_retain_layers(&mut buf, &["sha256:aa"]);
        let Msg::RetainLayers(ids) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(ids.iter().collect::<Vec<_>>(), ["sha256:aa"]);
        let mut again = Vec::new();
        Msg::RetainLayers(ids).encode(&mut again);
        assert_eq!(buf, again);
    }

    #[test]
    fn dns_rename_survives_encoding() {
        let mut buf = Vec::new();
        encode_dns_rename(&mut buf, "c1", "br0", [10, 5, 0, 2], Some([0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]), false, &["web", "alias"]);
        let Msg::DnsRename { id, bridge, ip, ip6, v4, names } = decode(&buf).unwrap() else { panic!("not a dns rename") };
        assert_eq!((id, bridge, ip, v4), ("c1", "br0", [10, 5, 0, 2], false));
        assert_eq!(ip6.len(), 16);
        assert_eq!(names.iter().collect::<Vec<_>>(), ["web", "alias"]);
        let mut again = Vec::new();
        Msg::DnsRename { id, bridge, ip, ip6, v4, names }.encode(&mut again);
        assert_eq!(buf, again);
    }

    #[test]
    fn dns_rename_with_a_bad_address_or_too_many_names_is_refused() {
        let mut buf = Vec::new();
        frame(&mut buf, kind::DNS_RENAME, |o| {
            put_str(o, "c1");
            put_str(o, "kiri1");
            o.extend_from_slice(&[10, 5, 0, 2]);
            put_bytes(o, &[1, 2, 3]); // neither none nor a whole address
            o.push(1);
            StrList::put(o, &["web"]);
        });
        assert!(decode(&buf).is_err());
        let names: Vec<String> = (0..=MAX_DNS_NAMES).map(|i| format!("n{i}")).collect();
        let mut buf = Vec::new();
        encode_dns_rename(&mut buf, "c1", "kiri1", [10, 5, 0, 2], None, true, &names);
        assert!(decode(&buf).is_err());
        let mut buf = Vec::new();
        encode_dns_rename(&mut buf, "c1", "kiri1", [10, 5, 0, 2], None, true, &names[..MAX_DNS_NAMES]);
        assert!(decode(&buf).is_ok());
    }

    /// A `DnsRename` the guest can't decode is still one it answers with
    /// nothing; frames that get replies are answered as before.
    #[test]
    fn a_bad_dns_rename_is_known_to_take_no_reply() {
        let names: Vec<String> = (0..=MAX_DNS_NAMES).map(|i| format!("n{i}")).collect();
        let mut buf = Vec::new();
        encode_dns_rename(&mut buf, "c1", "kiri1", [10, 5, 0, 2], None, true, &names);
        assert!(decode(&buf).is_err());
        assert!(takes_no_reply(&buf));
        let mut ping = Vec::new();
        Msg::Ping(1).encode(&mut ping);
        assert!(!takes_no_reply(&ping));
        assert!(!takes_no_reply(&buf[..3]));
    }

    #[test]
    fn the_protocol_version_covers_dns_rename() {
        // An agent from before `DnsRename` answers it with an error that the
        // host would take as the reply to its next request.
        const { assert!(PROTOCOL >= 8) };
    }

    #[test]
    fn fs_messages_survive_encoding() {
        round(Msg::FsStatRequest { id: "c1", path: "/etc" });
        round(Msg::FsStat(FsEntry { name: "etc", mode: 0o40755, size: 4096, mtime: 1_700_000_000, mtime_nsec: 5, ino: 77 }));
        round(Msg::FsRead { id: "c1", path: "/etc/os-release", offset: 10, len: 4096 });
        round(Msg::FsData(b"NAME=Alpine"));
        round(Msg::MemoryPressure);
        round(Msg::RemoveVolume { name: "pgdata" });
        round(Msg::RemoveBridge { bridge: "kiri3" });
        round(Msg::CopyStat { entry: FsEntry { name: "hosts", mode: 0o100644, size: 9, mtime: 3, mtime_nsec: 4, ino: 5 }, link: "" });
        round(Msg::CopyDone);
        round(Msg::Update { id: "c1", limits: Limits { memory_max: 1, cpu_quota: 2, cpu_period: 3, cpu_weight: 4, pids_max: 5, shm_size: 6 } });
        round(Msg::Freeze { id: "c1", frozen: true });
        round(Msg::StatsQuery { id: "c1" });
        round(Msg::ContainerStats(ContainerStats { read_ns: 1, mem_current: 2, online_cpus: 4, rx_bytes: 9, io_write_bytes: 11, ..Default::default() }));
        round(Msg::TopQuery { id: "c1" });
        round(Msg::UsageQuery);
        let mut buf = Vec::new();
        encode_usage(&mut buf, &["l 0a1b 4096", "c c1 0", "v data 12288"]);
        let Msg::Usage(lines) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(lines.iter().collect::<Vec<_>>(), ["l 0a1b 4096", "c c1 0", "v data 12288"]);
        round(Msg::ListenQuery { id: "c1" });
        round(Msg::UsbNodesQuery);
        let mut buf = Vec::new();
        encode_usb_nodes(&mut buf, &["0\t/dev/ttyUSB0\tserial port", "0\t/dev/bus/usb/003/002\traw USB"]);
        let Msg::UsbNodes(lines) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(lines.iter().collect::<Vec<_>>(), ["0\t/dev/ttyUSB0\tserial port", "0\t/dev/bus/usb/003/002\traw USB"]);
        let mut buf = Vec::new();
        encode_fs_events(&mut buf, &["M\t/mnt/mac/Users/a/app/main.js", "D\t/mnt/mac/Users/a/app/old.js"]);
        let Msg::FsEvents(lines) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(lines.iter().collect::<Vec<_>>(), ["M\t/mnt/mac/Users/a/app/main.js", "D\t/mnt/mac/Users/a/app/old.js"]);
        let mut buf = Vec::new();
        encode_listening(&mut buf, &["   0: 00000000:11D7 00000000:0000 0A"]);
        let Msg::Listening(lines) = decode(&buf).unwrap() else { panic!() };
        assert_eq!(lines.iter().collect::<Vec<_>>(), ["   0: 00000000:11D7 00000000:0000 0A"]);
        round(Msg::NetDetach { id: "c1", ifname: "eth1", bridge: "kiri2", ip: [172, 18, 0, 2] });
        let mut buf = Vec::new();
        encode_net_attach(&mut buf, "c1", "eth1", "kiri2", [10, 5, 0, 9], 24, [10, 5, 0, 1], &["web"], &["mac=02:42:0a:05:00:09"]);
        let Msg::NetAttach { net, .. } = decode(&buf).unwrap() else { panic!() };
        assert_eq!(net.opts.iter().collect::<Vec<_>>(), ["mac=02:42:0a:05:00:09"]);
        let mut again = Vec::new();
        decode(&buf).unwrap().encode(&mut again);
        assert_eq!(buf, again);
        // A frame without the list, as an older sender writes it.
        let mut old = Vec::new();
        encode_net_attach::<&str>(&mut old, "c1", "eth1", "kiri2", [10, 5, 0, 9], 24, [10, 5, 0, 1], &[], &[]);
        old.truncate(old.len() - 4);
        let len = (old.len() - 4) as u32;
        old[..4].copy_from_slice(&len.to_le_bytes());
        let Msg::NetAttach { net, .. } = decode(&old).unwrap() else { panic!() };
        assert!(net.opts.is_empty());
        round(Msg::Route { dst: [10, 43, 0, 0], prefix: 16, via: [172, 17, 0, 4], add: true });
        round(Msg::UplinkQuery);
        round(Msg::Uplink { ip: [192, 168, 64, 18], gateway: [192, 168, 64, 1] });
        round(Msg::Ack);
        let mut buf = Vec::new();
        encode_copy(&mut buf, copy_op::IN, "c1", "/etc", copy_flags::KEEP_OWNERS, &["sha256:a", "sha256:b"]);
        let Msg::Copy(c) = decode(&buf).unwrap() else { panic!() };
        assert_eq!((c.op, c.id, c.path, c.flags), (copy_op::IN, "c1", "/etc", copy_flags::KEEP_OWNERS));
        assert_eq!(c.layers.iter().collect::<Vec<_>>(), ["sha256:a", "sha256:b"]);
        let mut again = Vec::new();
        Msg::Copy(c).encode(&mut again);
        assert_eq!(buf, again);
        let mut buf = Vec::new();
        let entries = [
            FsEntry { name: "a", mode: 0o100644, size: 1, mtime: 2, mtime_nsec: 9, ino: 1 },
            FsEntry { name: "b", mode: 0o40755, size: 3, mtime: 4, mtime_nsec: 0, ino: 2 },
        ];
        encode_fs_list(&mut buf, &entries);
        let Msg::FsList { count, raw } = decode(&buf).unwrap() else { panic!() };
        assert_eq!(fs_entries(count, raw).collect::<Vec<_>>(), entries);
    }

    #[test]
    fn rejects_oversized_unknown_and_truncated() {
        assert!(frame_len(&(MAX_FRAME as u32).to_le_bytes()).is_err());
        let mut buf = Vec::new();
        Msg::Shutdown.encode(&mut buf);
        buf[4] = 0xff;
        assert_eq!(decode(&buf), Err(DecodeError::UnknownKind(0x00ff)));
        buf.clear();
        Msg::LayerQuery("sha256:ab").encode(&mut buf);
        buf.truncate(buf.len() - 1);
        assert_eq!(decode(&buf), Err(DecodeError::Truncated));
    }
}
