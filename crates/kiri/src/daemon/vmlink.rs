//! One vsock connection to the guest agent, shared by every daemon thread.
//!
//! The agent answers requests in the order it receives them, and tags
//! container output and exits with the container id. So: senders take the
//! write lock, queue a reply slot and write; a single reader thread hands each
//! reply to the oldest waiting slot and each container event to the engine.

// The file-view replies serve the Finder view, which only macOS has.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};

use kiri_proto::{HEADER, LayerBegin, Msg, Stats, Stream};
use kiri_vmm::Vm;

/// A reply to one request.
#[derive(Debug)]
pub enum Reply {
    LayerStatus(bool),
    LayerStored,
    /// The process's PID, as the agent's kernel numbers it, and whether
    /// its root is still being made (see `VmLink::run`).
    Started(u32, bool),
    Stats(Stats),
    Pong,
    FsStat(FileInfo),
    FsList(Vec<FileInfo>),
    FsData(Vec<u8>),
    Ack,
    ContainerStats(kiri_proto::ContainerStats),
    Top(Vec<String>),
    Listening(Vec<String>),
    UsbNodes(Vec<String>),
    Uplink([u8; 4], [u8; 4]),
    Error(String),
}

/// One file inside a container, as the file browser sees it.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub mode: u32,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: u32,
}

impl From<kiri_proto::FsEntry<'_>> for FileInfo {
    fn from(e: kiri_proto::FsEntry<'_>) -> FileInfo {
        FileInfo { name: e.name.to_owned(), mode: e.mode, size: e.size, mtime: e.mtime, mtime_nsec: e.mtime_nsec }
    }
}


/// Something a container did, routed to the engine.
#[derive(Debug)]
pub enum Event<'a> {
    Output { id: &'a str, stream: Stream, data: &'a [u8] },
    /// `oom` when the kernel killed a process in it for memory.
    Exited { id: &'a str, code: i32, oom: bool },
    /// The guest is short of memory right now.
    Pressure,
    /// The connection is gone: the VM stopped.
    Lost,
}

pub struct VmLink {
    writer: Mutex<Writer>,
    /// Stdin credit per running process; see `input`.
    windows: Arc<Windows>,
    /// The guest's latest `Usage` lines, for `usage`.
    usage: Arc<UsageSlot>,
    pending: Arc<Mutex<VecDeque<SyncSender<Reply>>>>,
    /// Kept so the VM lives as long as the link. Behind a mutex only so the
    /// link can be shared between threads; nothing touches it after boot.
    _vm: Mutex<Vm>,
    /// Opens vsock streams without that mutex, so connects run side by side.
    connector: kiri_vmm::Connector,
}

impl std::fmt::Debug for VmLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VmLink")
    }
}

struct Writer {
    conn: File,
    out: Vec<u8>,
    blob: Vec<u8>,
}

impl Writer {
    fn write(&mut self, windows: &Windows, body: Option<(File, u64)>) -> io::Result<()> {
        let result = self.write_request(body);
        if result.is_err() {
            // A partial frame cannot be retried on this stream. Publish
            // loss now, then wake the reader to do the exit accounting.
            windows.lose();
            use std::os::fd::AsRawFd;
            // SAFETY: conn owns a live stream socket for this entire call.
            unsafe { libc::shutdown(self.conn.as_raw_fd(), libc::SHUT_RDWR); }
        }
        result
    }

    fn write_request(&mut self, body: Option<(File, u64)>) -> io::Result<()> {
        self.conn.write_all(&self.out)?;
        if let Some((mut file, size)) = body {
            let mut left = size;
            while left > 0 {
                let want = left.min(self.blob.len() as u64) as usize;
                let n = file.read(&mut self.blob[..want])?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "layer blob shrank while sending"));
                }
                self.conn.write_all(&self.blob[..n])?;
                left -= n as u64;
            }
        }
        Ok(())
    }
}

/// How much stdin each running process may still be sent before the guest
/// reports some of it consumed. An entry exists from `run`/`exec` until
/// `Exited`; `input` for a process without one is discarded.
#[derive(Default)]
struct Windows {
    credit: Mutex<Credits>,
    changed: Condvar,
}

#[derive(Default)]
struct Credits {
    by_id: HashMap<String, usize>,
    /// The link is gone; nothing will ever be consumed.
    lost: bool,
    loss_handled: bool,
}

impl Windows {
    fn lock(&self) -> std::sync::MutexGuard<'_, Credits> {
        self.credit.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn open(&self, id: &str) {
        self.lock().by_id.insert(id.to_owned(), kiri_proto::INPUT_WINDOW);
    }
    fn close(&self, id: &str) {
        self.lock().by_id.remove(id);
        self.changed.notify_all();
    }
    fn add(&self, id: &str, n: usize) {
        if let Some(c) = self.lock().by_id.get_mut(id) {
            *c = (*c + n).min(kiri_proto::INPUT_WINDOW);
        }
        self.changed.notify_all();
    }
    fn lose(&self) {
        let mut g = self.lock();
        g.lost = true;
        g.by_id.clear();
        drop(g);
        self.changed.notify_all();
    }
    fn finish_loss(&self) {
        self.lock().loss_handled = true;
        self.changed.notify_all();
    }

    /// Wait for exit accounting before replacing the VM. The caller must
    /// hold no state lock needed by Lost, and Lost must not wait for link.
    fn is_lost(&self) -> bool {
        let mut g = self.lock();
        while g.lost && !g.loss_handled {
            g = self.changed.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        g.lost
    }
    /// Waits until `n` bytes may be sent to `id` and takes them. `Ok(false)`:
    /// the process is not running, the bytes should be dropped.
    fn take(&self, id: &str, n: usize) -> io::Result<bool> {
        let mut g = self.lock();
        loop {
            if g.lost {
                return Err(io::Error::other("the VM stopped"));
            }
            let Some(c) = g.by_id.get_mut(id) else { return Ok(false) };
            if *c >= n {
                *c -= n;
                return Ok(true);
            }
            g = self.changed.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Where the reader thread leaves `Usage` for whoever asked: a count of
/// answers so far, and the latest one.
#[derive(Default)]
struct UsageSlot {
    lines: Mutex<(u64, Option<Vec<String>>)>,
    arrived: Condvar,
}

impl VmLink {
    pub fn is_lost(&self) -> bool {
        self.windows.is_lost()
    }

    /// Observes disconnection without waiting for the reader's cleanup.
    pub fn is_disconnected(&self) -> bool {
        self.windows.lock().lost
    }

    /// Starts the reader thread. `on_event` runs on that thread, so it must
    /// not block for long: it holds up every reply behind it.
    pub fn new(vm: Vm, conn: File, on_event: impl Fn(Event<'_>) + Send + 'static) -> io::Result<VmLink> {
        let pending: Arc<Mutex<VecDeque<SyncSender<Reply>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let mut reader = conn.try_clone()?;
        let queue = pending.clone();
        let windows = Arc::new(Windows::default());
        let wins = windows.clone();
        let usage = Arc::new(UsageSlot::default());
        let slot = usage.clone();
        std::thread::Builder::new().name("kiri-vm-reader".into()).spawn(move || {
            let mut frame = Vec::with_capacity(64 * 1024);
            while read_frame(&mut reader, &mut frame).is_ok() {
                let msg = kiri_proto::decode(&frame);
                let reply = match msg {
                    Ok(Msg::Output { id, stream, data }) => {
                        on_event(Event::Output { id, stream, data });
                        continue;
                    }
                    // Not a reply: the guest measures on a thread and sends
                    // this when done, between other replies.
                    Ok(Msg::Usage(lines)) => {
                        let mut g = slot.lines.lock().unwrap_or_else(|e| e.into_inner());
                        g.0 += 1;
                        g.1 = Some(lines.iter().map(str::to_owned).collect());
                        drop(g);
                        slot.arrived.notify_all();
                        continue;
                    }
                    Ok(Msg::Exited(e)) => {
                        wins.close(e.id);
                        on_event(Event::Exited { id: e.id, code: e.code, oom: e.oom_killed });
                        continue;
                    }
                    Ok(Msg::InputConsumed { id, bytes }) => {
                        wins.add(id, bytes as usize);
                        continue;
                    }
                    Ok(Msg::MemoryPressure) => {
                        on_event(Event::Pressure);
                        continue;
                    }
                    Ok(Msg::LayerStatus(have)) => Reply::LayerStatus(have),
                    Ok(Msg::LayerStored(_)) => Reply::LayerStored,
                    Ok(Msg::Started { pid, starting, .. }) => Reply::Started(pid, starting),
                    Ok(Msg::Stats(s)) => Reply::Stats(s),
                    Ok(Msg::Pong(_)) => Reply::Pong,
                    Ok(Msg::FsStat(e)) => Reply::FsStat(FileInfo::from(e)),
                    Ok(Msg::FsList { count, raw }) => Reply::FsList(
                        kiri_proto::fs_entries(count, raw)
                            .map(FileInfo::from)
                            .collect(),
                    ),
                    Ok(Msg::FsData(d)) => Reply::FsData(d.to_vec()),
                    Ok(Msg::Ack) => Reply::Ack,
                    Ok(Msg::ContainerStats(s)) => Reply::ContainerStats(s),
                    Ok(Msg::Top(lines)) => Reply::Top(lines.iter().map(str::to_owned).collect()),
                    Ok(Msg::Listening(lines)) => Reply::Listening(lines.iter().map(str::to_owned).collect()),
                    Ok(Msg::UsbNodes(lines)) => Reply::UsbNodes(lines.iter().map(str::to_owned).collect()),
                    Ok(Msg::Uplink { ip, gateway }) => Reply::Uplink(ip, gateway),
                    Ok(Msg::Error(e)) => Reply::Error(e.to_owned()),
                    Ok(other) => Reply::Error(format!("unexpected message from guest: {other:?}")),
                    Err(e) => Reply::Error(format!("bad frame from guest: {e:?}")),
                };
                if let Some(slot) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() {
                    let _ = slot.send(reply);
                }
            }
            // Publish loss before waking callers: a retry must not borrow
            // this dead link while the engine is still accounting exits.
            wins.lose();
            for slot in queue.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
                let _ = slot.send(Reply::Error("the VM stopped".into()));
            }
            on_event(Event::Lost);
            wins.finish_loss();
        })?;
        Ok(VmLink {
            writer: Mutex::new(Writer { conn, out: Vec::with_capacity(4096), blob: vec![0; 256 * 1024] }),
            pending,
            windows,
            usage,
            connector: vm.connector(),
            _vm: Mutex::new(vm),
        })
    }

    /// Sends one request and waits for its reply.
    /// A body is opened by the caller, before anything is sent: a blob that
    /// can't be read fails this call alone, not the connection (a request
    /// cut off after its header can't be taken back, and loses the VM).
    fn call(&self, encode: impl FnOnce(&mut Vec<u8>), body: Option<(File, u64)>) -> io::Result<Reply> {
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            w.out.clear();
            encode(&mut w.out);
            // Queue the slot before writing, so the reply can never arrive first.
            {
                let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                if self.windows.lock().lost {
                    return Err(io::Error::other("the VM stopped"));
                }
                pending.push_back(tx);
            }
            w.write(&self.windows, body)?;
        }
        match rx.recv() {
            Ok(Reply::Error(e)) => Err(io::Error::other(e)),
            Ok(r) => Ok(r),
            Err(_) => Err(io::Error::other("the VM stopped")),
        }
    }

    /// Sends a message that has no reply.
    fn send(&self, encode: impl FnOnce(&mut Vec<u8>)) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.out.clear();
        encode(&mut w.out);
        w.write(&self.windows, None)
    }

    /// What the guest's layers, containers and volumes take on disk: the
    /// `KIND NAME BYTES` lines of `Msg::Usage`, or None if they don't come
    /// within `wait` or the VM stops.
    pub fn usage(&self, wait: std::time::Duration) -> Option<Vec<String>> {
        let asked = self.usage.lines.lock().unwrap_or_else(|e| e.into_inner()).0;
        self.send(|o| Msg::UsageQuery.encode(o)).ok()?;
        let g = self.usage.lines.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = self.usage.arrived.wait_timeout_while(g, wait, |g| g.0 == asked).unwrap_or_else(|e| e.into_inner());
        if g.0 == asked { None } else { g.1.clone() }
    }

    pub fn has_layer(&self, digest: &str) -> io::Result<bool> {
        match self.call(|o| Msg::LayerQuery(digest).encode(o), None)? {
            Reply::LayerStatus(have) => Ok(have),
            other => Err(io::Error::other(format!("expected LayerStatus, got {other:?}"))),
        }
    }

    pub fn send_layer(&self, digest: &str, media_type: &str, blob: &Path) -> io::Result<()> {
        // Opened first, its size from the open file, so a missing blob
        // fails here rather than half-way through the request.
        let file = File::open(blob)?;
        let size = file.metadata()?.len();
        self.call(|o| Msg::LayerBegin(LayerBegin { digest, media_type, size }).encode(o), Some((file, size)))
            .map(drop)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &self,
        id: &str,
        layers: &[String],
        argv: &[String],
        env: &[String],
        cwd: &str,
        user: &str,
        hostname: &str,
        flags: u32,
        net: kiri_proto::Net<'_, String>,
        mounts: &[String],
        limits: &kiri_proto::Limits,
        caps: u64,
        opts: &[String],
        seccomp: &[u8],
    ) -> io::Result<u32> {
        self.windows.open(id);
        // Its process runs, but its root may not be in place yet (a new
        // volume is being filled): the agent answered at once rather than
        // stall every other container meanwhile. The start is reported only
        // once an exec can enter it (#311), or fails as a start.
        let r = self
            .call(|o| kiri_proto::encode_run(o, id, layers, argv, env, cwd, user, hostname, flags, net, mounts, limits, caps, opts, seccomp), None)
            .and_then(started)
            .and_then(|(pid, starting)| {
                if starting {
                    wait_started(|| self.call(|o| Msg::AwaitStart { id }.encode(o), None), std::thread::sleep)?;
                }
                Ok(pid)
            });
        if r.is_err() {
            self.windows.close(id);
        }
        r
    }

    fn ack(&self, encode: impl FnOnce(&mut Vec<u8>)) -> io::Result<()> {
        match self.call(encode, None)? {
            Reply::Ack => Ok(()),
            other => Err(io::Error::other(format!("expected Ack, got {other:?}"))),
        }
    }

    pub fn update(&self, id: &str, limits: kiri_proto::Limits) -> io::Result<()> {
        self.ack(|o| Msg::Update { id, limits }.encode(o))
    }

    pub fn freeze(&self, id: &str, frozen: bool) -> io::Result<()> {
        self.ack(|o| Msg::Freeze { id, frozen }.encode(o))
    }

    pub fn container_stats(&self, id: &str) -> io::Result<kiri_proto::ContainerStats> {
        match self.call(|o| Msg::StatsQuery { id }.encode(o), None)? {
            Reply::ContainerStats(s) => Ok(s),
            other => Err(io::Error::other(format!("expected ContainerStats, got {other:?}"))),
        }
    }

    pub fn top(&self, id: &str) -> io::Result<Vec<String>> {
        match self.call(|o| Msg::TopQuery { id }.encode(o), None)? {
            Reply::Top(lines) => Ok(lines),
            other => Err(io::Error::other(format!("expected Top, got {other:?}"))),
        }
    }

    /// The container's LISTEN lines from /proc/net/tcp and tcp6.
    pub fn listening(&self, id: &str) -> io::Result<Vec<String>> {
        match self.call(|o| Msg::ListenQuery { id }.encode(o), None)? {
            Reply::Listening(lines) => Ok(lines),
            other => Err(io::Error::other(format!("expected Listening, got {other:?}"))),
        }
    }

    /// `PORT\tPATH\tWHAT` for the nodes of the USB devices attached to the
    /// VM (`Msg::UsbNodes`).
    pub fn usb_nodes(&self) -> io::Result<Vec<String>> {
        match self.call(|o| Msg::UsbNodesQuery.encode(o), None)? {
            Reply::UsbNodes(lines) => Ok(lines),
            other => Err(io::Error::other(format!("expected UsbNodes, got {other:?}"))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Routes `dst/prefix` through `via` in the VM, or removes that route.
    pub fn route(&self, dst: [u8; 4], prefix: u8, via: [u8; 4], add: bool) -> io::Result<()> {
        self.ack(|o| Msg::Route { dst, prefix, via, add }.encode(o))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn net_attach(&self, id: &str, n: u32, bridge: &str, ip: [u8; 4], prefix: u8, gateway: [u8; 4], dns_names: &[String], opts: &[String]) -> io::Result<()> {
        let ifname = format!("eth{n}");
        self.ack(|o| kiri_proto::encode_net_attach(o, id, &ifname, bridge, ip, prefix, gateway, dns_names, opts))
    }

    pub fn net_detach(&self, id: &str, n: u32, bridge: &str, ip: [u8; 4]) -> io::Result<()> {
        let ifname = format!("eth{n}");
        self.ack(|o| Msg::NetDetach { id, ifname: &ifname, bridge, ip }.encode(o))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn exec(&self, container_id: &str, exec_id: &str, argv: &[String], env: &[String], cwd: &str, user: &str, flags: u32, caps: u64, opts: &[String]) -> io::Result<u32> {
        self.windows.open(exec_id);
        let r = self.call(|o| kiri_proto::encode_exec(o, container_id, exec_id, argv, env, cwd, user, flags, caps, opts), None).and_then(started).map(|(pid, _)| pid);
        if r.is_err() {
            self.windows.close(exec_id);
        }
        r
    }

    /// Sends stdin for a process. Blocks while the guest already holds
    /// `INPUT_WINDOW` bytes it has not consumed, so the pressure reaches the
    /// client (its write blocks) instead of the agent dropping input.
    pub fn input(&self, id: &str, data: &[u8]) -> io::Result<()> {
        // Frames are bounded, so large writes go in pieces.
        for piece in data.chunks(kiri_proto::MAX_OUTPUT_CHUNK) {
            if !self.windows.take(id, piece.len())? {
                return Ok(());
            }
            self.send(|o| Msg::Input { id, data: piece }.encode(o))?;
        }
        Ok(())
    }

    pub fn close_stdin(&self, id: &str) -> io::Result<()> {
        self.send(|o| Msg::CloseStdin { id }.encode(o))
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> io::Result<()> {
        self.send(|o| Msg::Resize { id, rows, cols }.encode(o))
    }

    /// The VM's address on the NAT network and its gateway (the Mac's own
    /// address there). `Err` with no lease yet, or from an older agent.
    pub fn uplink(&self) -> io::Result<([u8; 4], [u8; 4])> {
        match self.call(|o| Msg::UplinkQuery.encode(o), None)? {
            Reply::Uplink(ip, gateway) => Ok((ip, gateway)),
            Reply::Error(e) => Err(io::Error::other(e)),
            other => Err(io::Error::other(format!("expected Uplink, got {other:?}"))),
        }
    }

    /// Forwards a published port in the kernel (on a Linux host; in the VM,
    /// from the NAT network). `Err` means the agent could not; the caller
    /// falls back to relaying.
    pub fn publish(&self, p: kiri_proto::Publish<'_>) -> io::Result<()> {
        match self.call(|o| Msg::Publish(p).encode(o), None)? {
            Reply::Ack => Ok(()),
            Reply::Error(e) => Err(io::Error::other(e)),
            other => Err(io::Error::other(format!("expected Ack, got {other:?}"))),
        }
    }
    pub fn unpublish(&self, id: &str) -> io::Result<()> {
        // No reply: this runs when a container exits, on the link's own reader
        // thread, which could not wait for one.
        self.send(|o| Msg::Unpublish { id }.encode(o))
    }
    pub fn remove_volume(&self, name: &str) -> io::Result<()> {
        self.send(|o| Msg::RemoveVolume { name }.encode(o))
    }

    /// A removed network's bridge goes, with its addresses, routes and DNS
    /// server (and the IPv6 NAT table with the last IPv6 network).
    pub fn remove_bridge(&self, bridge: &str) -> io::Result<()> {
        self.send(|o| Msg::RemoveBridge { bridge }.encode(o))
    }

    /// Files the Mac changed under bind mounts (`OP\tPATH` lines, PATH in
    /// the VM), for the guest to raise as inotify events. No reply.
    pub fn fs_events<S: AsRef<str>>(&self, lines: &[S]) -> io::Result<()> {
        self.send(|o| kiri_proto::encode_fs_events(o, lines))
    }

    /// The Mac's wall clock, for the guest. Sent at boot, and again while it
    /// runs: the VM's clock stops while the Mac sleeps.
    pub fn set_time(&self) -> io::Result<()> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        self.send(|o| Msg::SetTime(now.as_nanos() as u64).encode(o))
    }

    /// Tells the guest which stopped containers still exist; it deletes the rest.
    pub fn retain(&self, ids: &[String]) -> io::Result<()> {
        self.send(|o| kiri_proto::encode_retain(o, ids))
    }

    /// Tells the guest which unpacked layers are still used; it deletes the rest.
    pub fn retain_layers(&self, digests: &[String]) -> io::Result<()> {
        self.send(|o| kiri_proto::encode_retain_layers(o, digests))
    }

    pub fn kill(&self, id: &str, signal: u32) -> io::Result<()> {
        self.send(|o| Msg::Kill { id, signal }.encode(o))
    }

    pub fn remove(&self, id: &str) -> io::Result<()> {
        self.send(|o| Msg::Remove { id }.encode(o))
    }

    /// Sets the balloon so the guest runs in `bytes`; returns the old target.
    pub fn set_memory_target(&self, bytes: u64) -> Option<u64> {
        self._vm.lock().unwrap_or_else(|e| e.into_inner()).set_memory_target(bytes)
    }

    /// Accepts guest-initiated streams on `port`.
    pub fn listen(&self, port: u32, handler: kiri_vmm::Handler) -> io::Result<()> {
        self._vm.lock().unwrap_or_else(|e| e.into_inner()).listen(port, handler)
    }

    /// A new stream to a port the guest agent listens on.
    pub fn connect(&self, port: u32) -> io::Result<kiri_vmm::VsockStream> {
        self.connector.connect(port)
    }

    pub fn fs_stat(&self, id: &str, path: &str) -> io::Result<FileInfo> {
        match self.call(|o| Msg::FsStatRequest { id, path }.encode(o), None)? {
            Reply::FsStat(f) => Ok(f),
            other => Err(io::Error::other(format!("expected FsStat, got {other:?}"))),
        }
    }

    pub fn fs_list(&self, id: &str, path: &str) -> io::Result<Vec<FileInfo>> {
        match self.call(|o| Msg::FsListRequest { id, path }.encode(o), None)? {
            Reply::FsList(v) => Ok(v),
            other => Err(io::Error::other(format!("expected FsList, got {other:?}"))),
        }
    }

    pub fn fs_read(&self, id: &str, path: &str, offset: u64, len: u32) -> io::Result<Vec<u8>> {
        match self.call(|o| Msg::FsRead { id, path, offset, len }.encode(o), None)? {
            Reply::FsData(d) => Ok(d),
            other => Err(io::Error::other(format!("expected FsData, got {other:?}"))),
        }
    }

    pub fn stats(&self) -> io::Result<Stats> {
        match self.call(|o| Msg::StatsRequest.encode(o), None)? {
            Reply::Stats(s) => Ok(s),
            other => Err(io::Error::other(format!("expected Stats, got {other:?}"))),
        }
    }

    /// Asks the guest to power off. The VM itself is released when the link drops.
    pub fn shutdown(&self) {
        let _ = self.send(|o| Msg::Shutdown.encode(o));
    }

    /// Success confirms both shutdown and release of the old generation's
    /// resources, even while relay threads retain this link.
    pub fn retire(&self) -> io::Result<()> {
        self._vm.lock().unwrap_or_else(|e| e.into_inner())
            .retire(std::time::Duration::from_secs(10), std::time::Duration::from_secs(10))
    }

    #[cfg(test)]
    pub(super) fn hold_retirement(&self) -> std::sync::MutexGuard<'_, Vm> {
        self._vm.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    pub(super) fn interrupted_layer(&self, blob: &Path) -> io::Result<()> {
        self.call(|out| Msg::LayerBegin(LayerBegin {
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip", size: 1,
        }).encode(out), Some((File::open(blob)?, 1))).map(|_| ())
    }

    /// Waits for the guest to stop after `shutdown`, and forces it if it has
    /// not within `timeout`. False if it had to be forced.
    ///
    /// In a VM the guest flushes its disk before powering off; stopping the
    /// VM before then (as dropping it does) loses what was still cached, and
    /// files written shortly before come back empty. On a Linux host the
    /// agent kills what is left and removes its bridges and nftables table
    /// first; an agent left behind kept its containers, ports and sockets,
    /// and the next daemon could not start.
    pub fn wait_stopped(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let vm = self._vm.lock().unwrap_or_else(|e| e.into_inner());
        while vm.is_running() {
            if std::time::Instant::now() >= deadline {
                vm.force_stop();
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }
}

/// The PID a `Run` or `Exec` was answered with, and whether its root is
/// still being made.
fn started(r: Reply) -> io::Result<(u32, bool)> {
    match r {
        Reply::Started(pid, starting) => Ok((pid, starting)),
        other => Err(io::Error::other(format!("expected Started, got {other:?}"))),
    }
}

/// The pauses between `AwaitStart` asks: the first at once, then 1, 2, 4,
/// 8, 16 ms, then every 25 ms. Most volumes fill within the first few; a
/// big one is noticed within 25 ms of being done.
fn await_backoff() -> impl Iterator<Item = std::time::Duration> {
    [0, 1, 2, 4, 8, 16].into_iter().chain(std::iter::repeat(25)).map(std::time::Duration::from_millis)
}

/// Asks (`ask` sends `AwaitStart`) until the agent says the root is in
/// place. An error fails the start, as it would have failed the `Run`
/// itself: the child ended before its root was ready (the agent answers
/// with the child's own reason, and sends no `Exited`), or the VM stopped.
/// No lock is held while it sleeps: `call` takes and lets go of its own for
/// each ask.
fn wait_started(mut ask: impl FnMut() -> io::Result<Reply>, mut sleep: impl FnMut(std::time::Duration)) -> io::Result<()> {
    for pause in await_backoff() {
        if !pause.is_zero() {
            sleep(pause);
        }
        match ask()? {
            Reply::Started(_, true) => {}
            Reply::Started(_, false) => return Ok(()),
            other => return Err(io::Error::other(format!("expected Started, got {other:?}"))),
        }
    }
    unreachable!("the backoff never ends")
}

fn read_frame(conn: &mut File, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.resize(4, 0);
    conn.read_exact(&mut buf[..4])?;
    let len = kiri_proto::frame_len(buf[..4].try_into().expect("4 bytes"))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
    buf.resize(len.max(HEADER), 0);
    conn.read_exact(&mut buf[4..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_starting_container_is_asked_about_until_its_root_is_in_place() {
        use std::time::Duration;
        let ms = |v: &[u64]| v.iter().map(|&m| Duration::from_millis(m)).collect::<Vec<_>>();
        assert_eq!(await_backoff().take(9).collect::<Vec<_>>(), ms(&[0, 1, 2, 4, 8, 16, 25, 25, 25]));
        // Starting twice, then in place: three asks, the first at once.
        let mut answers = vec![Reply::Started(7, false), Reply::Started(7, true), Reply::Started(7, true)];
        let (mut asks, mut slept) = (0, Vec::new());
        assert!(wait_started(|| { asks += 1; Ok(answers.pop().unwrap()) }, |d| slept.push(d)).is_ok());
        assert_eq!((asks, slept), (3, ms(&[1, 2])));
        // An error (the child failed before its root was ready, the VM
        // stopped) ends the wait and fails the start with it.
        let mut answers = vec![Err(io::Error::other("run c1: mount /payload: No space left on device")), Ok(Reply::Started(7, true))];
        let mut asks = 0;
        let e = wait_started(|| { asks += 1; answers.pop().unwrap() }, |_| {}).unwrap_err();
        assert_eq!((asks, e.to_string().as_str()), (2, "run c1: mount /payload: No space left on device"));
    }

    #[test]
    fn a_failed_write_disconnects_before_the_reader_accounts_loss() {
        let (socket, peer) = UnixStream::pair().unwrap();
        socket.shutdown(std::net::Shutdown::Write).unwrap();
        let mut writer = Writer { conn: std::os::fd::OwnedFd::from(socket).into(), out: vec![1], blob: vec![0; 16] };
        let windows = Windows::default();
        assert!(writer.write(&windows, None).is_err());
        assert!(windows.lock().lost, "a writer failure must publish loss before a start can record an ordinary failure");
        assert!(!windows.lock().loss_handled, "only the reader owns exit accounting");
        let mut peer = peer;
        assert_eq!(peer.read(&mut [0]).unwrap(), 0, "the failed transport is closed in both directions");
    }
}
