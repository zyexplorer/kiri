//! A running Kiri guest: the VM plus the agent connection, with boot timings.

// Parts of this serve only the Mac (the window, boot timings).
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kiri_proto::{HEADER, Hello, LayerBegin, Msg, Stream};
use kiri_vmm::{Config, Event, Vm};

/// How long each boot stage took, measured from the host.
#[derive(Debug, Clone, Copy)]
pub struct BootTimings {
    /// Virtualization.framework configure + start.
    pub vm_started: Duration,
    /// Host-side time until the guest agent's Hello arrived.
    pub agent_ready: Duration,
    /// Guest-side: kernel start to PID 1.
    pub kernel: Duration,
    /// Guest-side: PID 1 start to agent ready.
    pub init: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct Latency {
    pub p50: Duration,
    pub p99: Duration,
}

#[derive(Debug, Default)]
pub struct BootOptions {
    /// Send the guest console to stdout.
    pub console: bool,
    /// Attach the persistent data disk (layers and containers). Without it
    /// the guest keeps them in memory, which is fine for a quick boot test.
    pub data_disk: bool,
    /// The VM's memory ceiling; 1 GiB if not set. The daemon sets a larger
    /// one and keeps the guest inside it with the balloon.
    pub memory_mib: Option<u64>,
    /// The VM's CPUs; 2 if not set.
    pub cpus: Option<usize>,
}

/// One container run, as the host asks for it.
#[derive(Debug)]
pub struct RunRequest<'a> {
    pub id: &'a str,
    /// Layer digests from the top layer down to the base.
    pub layers: &'a [String],
    pub argv: &'a [String],
    pub env: &'a [String],
    pub cwd: &'a str,
    pub user: &'a str,
    pub hostname: &'a str,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct RunResult {
    pub code: i32,
    /// Host-side: request sent until the exit arrived.
    pub total: Duration,
    /// Guest-side: request received until the process was cloned.
    pub guest_setup: Duration,
    /// Guest-side: clone until the exit was reaped.
    pub guest_run: Duration,
}

#[derive(Debug)]
pub struct Guest {
    vm: Vm,
    conn: File,
    /// Reused for every frame, so steady-state traffic does not allocate.
    frame: Vec<u8>,
    out: Vec<u8>,
    rtts: Vec<Duration>,
    seq: u64,
    pub hello: Hello,
    pub timings: BootTimings,
}

impl Guest {
    pub fn boot(opts: &BootOptions) -> io::Result<Guest> {
        // The Intel build opened on Apple Silicon runs under Rosetta, where
        // Virtualization.framework cannot boot its x86-64 guest.
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        if translated() {
            return Err(io::Error::other(
                "this is Kiri for Intel Macs, and this Mac has Apple silicon: install Kiri for Apple silicon from runkiri.com",
            ));
        }
        if !Vm::is_supported() {
            return Err(io::Error::other(if cfg!(target_os = "linux") {
                "on Linux, Kiri runs containers on this machine's kernel and needs root: sudo kiri daemon"
            } else {
                "this Mac does not support virtualization"
            }));
        }
        #[cfg(target_os = "linux")]
        let cfg = Config {
            cpus: 0,
            memory_mib: 0,
            kernel: PathBuf::new(),
            initrd: agent_binary(),
            cmdline: String::new(),
            console: if opts.console {
                Some(io::stdout().as_fd().try_clone_to_owned()?)
            } else {
                None
            },
            agent_port: kiri_proto::HOST_PORT,
            disk: None,
            share: None,
            rosetta_tag: None,
            nat_mac: None,
        };
        #[cfg(target_os = "macos")]
        let cfg = Self::vm_config(opts)?;
        Self::start(cfg)
    }

    /// The VM: kernel, initramfs, data disk, NAT, the Mac's files, Rosetta.
    #[cfg(target_os = "macos")]
    fn vm_config(opts: &BootOptions) -> io::Result<Config> {
        let dir = guest_dir();
        // swiotlb=noforce: the VM's virtio devices address all of memory, so
        // the kernel's 64 MB bounce buffer would sit there unused.
        let mut cmdline = String::from(
            "console=hvc0 rdinit=/init panic=-1 rcupdate.rcu_expedited=1 swiotlb=noforce",
        );
        if !opts.console {
            cmdline.push_str(" quiet loglevel=0");
        }
        let cfg = Config {
            cpus: opts.cpus.unwrap_or(2),
            memory_mib: opts.memory_mib.unwrap_or(1024),
            kernel: dir.join(KERNEL),
            initrd: dir.join("initramfs.cpio"),
            cmdline,
            console: if opts.console {
                Some(io::stdout().as_fd().try_clone_to_owned()?)
            } else {
                None
            },
            agent_port: kiri_proto::HOST_PORT,
            disk: if opts.data_disk {
                Some(data_disk(&dir)?)
            } else {
                None
            },
            nat_mac: if opts.data_disk {
                Some(mac_address()?)
            } else {
                None
            },
            // The Mac's filesystem, for bind mounts: /mnt/mac in the guest.
            share: opts
                .data_disk
                .then(|| (PathBuf::from("/"), "kiri-mac".to_owned())),
            rosetta_tag: opts.data_disk.then(|| "rosetta".to_owned()),
        };
        if !cfg.kernel.is_file() {
            return Err(missing_kernel(
                &cfg.kernel,
                std::env::current_exe().ok().as_deref(),
            ));
        }
        Ok(cfg)
    }

    fn start(cfg: Config) -> io::Result<Guest> {
        let t0 = Instant::now();
        let vm = Vm::start(cfg)?;
        let vm_started = t0.elapsed();
        let conn = match vm.next_event(Duration::from_secs(10)) {
            Some(Event::GuestConnected(fd)) => File::from(fd),
            Some(Event::Stopped(why)) => {
                return Err(io::Error::other(format!(
                    "guest stopped before connecting: {why:?}"
                )));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "guest did not connect within 10 s",
                ));
            }
        };
        let mut guest = Guest {
            vm,
            conn,
            frame: Vec::with_capacity(256),
            out: Vec::with_capacity(64),
            rtts: Vec::new(),
            seq: 0,
            hello: Hello {
                protocol: 0,
                init_start_ns: 0,
                ready_ns: 0,
                mem_total_kib: 0,
                mem_available_kib: 0,
            },
            timings: BootTimings {
                vm_started,
                agent_ready: Duration::ZERO,
                kernel: Duration::ZERO,
                init: Duration::ZERO,
            },
        };
        guest.hello = match guest.read()? {
            Msg::Hello(h) => h,
            other => return Err(io::Error::other(format!("expected Hello, got {other:?}"))),
        };
        // The agent comes from the same install, so another version means the
        // files changed under a running Kiri: stop rather than misread it.
        if guest.hello.protocol != kiri_proto::PROTOCOL {
            return Err(io::Error::other(format!(
                "the VM agent speaks protocol {}, this Kiri speaks {}: quit Kiri and open it again",
                guest.hello.protocol,
                kiri_proto::PROTOCOL
            )));
        }
        guest.timings.agent_ready = t0.elapsed();
        // The guest has no real-time clock source of its own.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        guest.send(Msg::SetTime(now.as_nanos() as u64))?;
        guest.timings.kernel = Duration::from_nanos(guest.hello.init_start_ns);
        guest.timings.init = Duration::from_nanos(guest.hello.ready_ns - guest.hello.init_start_ns);
        Ok(guest)
    }

    /// Measures `n` sequential round trips over the agent connection.
    pub fn ping(&mut self, n: usize) -> io::Result<Latency> {
        self.rtts.clear();
        self.rtts.reserve(n);
        for _ in 0..n {
            self.seq += 1;
            let seq = self.seq;
            let s = Instant::now();
            self.send(Msg::Ping(seq))?;
            match self.read()? {
                Msg::Pong(v) if v == seq => {}
                other => {
                    return Err(io::Error::other(format!(
                        "expected Pong({seq}), got {other:?}"
                    )));
                }
            }
            self.rtts.push(s.elapsed());
        }
        self.rtts.sort_unstable();
        let pct = |p: usize| {
            self.rtts
                .get((self.rtts.len() * p / 100).min(self.rtts.len().saturating_sub(1)))
        };
        Ok(Latency {
            p50: pct(50).copied().unwrap_or_default(),
            p99: pct(99).copied().unwrap_or_default(),
        })
    }

    pub fn has_layer(&mut self, digest: &str) -> io::Result<bool> {
        self.send(Msg::LayerQuery(digest))?;
        match self.read()? {
            Msg::LayerStatus(have) => Ok(have),
            other => Err(unexpected("LayerStatus", &other)),
        }
    }

    /// Streams a compressed layer blob to the guest, which unpacks it.
    /// Returns the unpacked size.
    pub fn send_layer(
        &mut self,
        digest: &str,
        media_type: &str,
        blob: &std::path::Path,
    ) -> io::Result<u64> {
        let mut file = File::open(blob)?;
        let size = file.metadata()?.len();
        self.send(Msg::LayerBegin(LayerBegin {
            digest,
            media_type,
            size,
        }))?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut left = size;
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let n = file.read(&mut buf[..want])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "layer blob shrank while sending",
                ));
            }
            self.conn.write_all(&buf[..n])?;
            left -= n as u64;
        }
        match self.read()? {
            Msg::LayerStored(s) => Ok(s.unpacked_bytes),
            Msg::Error(e) => Err(io::Error::other(e.to_owned())),
            other => Err(unexpected("LayerStored", &other)),
        }
    }

    /// Runs one container to completion, passing its output to `on_output`.
    pub fn run(
        &mut self,
        req: &RunRequest<'_>,
        mut on_output: impl FnMut(Stream, &[u8]),
    ) -> io::Result<RunResult> {
        self.out.clear();
        let none: &[String] = &[];
        let net = kiri_proto::Net {
            bridge: "",
            ip: [0; 4],
            prefix: 0,
            gateway: [0; 4],
            extra_hosts: none,
            dns_names: none,
            opts: none,
        };
        kiri_proto::encode_run(
            &mut self.out,
            req.id,
            req.layers,
            req.argv,
            req.env,
            req.cwd,
            req.user,
            req.hostname,
            req.flags,
            net,
            none,
            &kiri_proto::Limits::default(),
            0,
            none,
        );
        let t = Instant::now();
        self.conn.write_all(&self.out)?;
        loop {
            match self.read()? {
                Msg::Started { .. } => {}
                Msg::Output { stream, data, .. } => on_output(stream, data),
                Msg::Exited(e) => {
                    return Ok(RunResult {
                        code: e.code,
                        total: t.elapsed(),
                        guest_setup: Duration::from_nanos(e.setup_ns),
                        guest_run: Duration::from_nanos(e.run_ns),
                    });
                }
                Msg::Error(e) => return Err(io::Error::other(e.to_owned())),
                other => return Err(unexpected("container output", &other)),
            }
        }
    }

    /// Asks the guest to power off and waits for the VM to stop. Returns how
    /// long that took.
    pub fn shutdown(mut self) -> io::Result<Duration> {
        let t = Instant::now();
        self.send(Msg::Shutdown)?;
        loop {
            match self.vm.next_event(Duration::from_secs(10)) {
                Some(Event::Stopped(None)) => return Ok(t.elapsed()),
                Some(Event::Stopped(Some(why))) => return Err(io::Error::other(why)),
                Some(Event::GuestConnected(_)) => continue,
                None => {
                    self.vm.force_stop();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "guest did not power off; forced",
                    ));
                }
            }
        }
    }

    /// Hands over the VM and the agent connection, for callers that drive the
    /// connection from several threads (the daemon).
    pub fn into_parts(self) -> (Vm, File) {
        (self.vm, self.conn)
    }

    fn send(&mut self, msg: Msg<'_>) -> io::Result<()> {
        self.out.clear();
        msg.encode(&mut self.out);
        self.conn.write_all(&self.out)
    }

    fn read(&mut self) -> io::Result<Msg<'_>> {
        self.frame.resize(4, 0);
        self.conn.read_exact(&mut self.frame[..4])?;
        let len = kiri_proto::frame_len(self.frame[..4].try_into().unwrap())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
        self.frame.resize(len.max(HEADER), 0);
        self.conn.read_exact(&mut self.frame[4..])?;
        kiri_proto::decode(&self.frame)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))
    }
}

fn unexpected(wanted: &str, got: &Msg<'_>) -> io::Error {
    io::Error::other(format!("expected {wanted}, got {got:?}"))
}

/// The agent on a Linux host: `KIRI_AGENT`, else `kiri-init` beside `kiri`.
#[cfg(target_os = "linux")]
fn agent_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("KIRI_AGENT") {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("kiri-init")))
        .unwrap_or_else(|| PathBuf::from("kiri-init"))
}

#[cfg(target_os = "macos")]
/// The guest's data disk, copied from the empty template on first use. On
/// APFS the copy is a clone: instant, and it takes no space until written.
fn data_disk(guest_dir: &std::path::Path) -> io::Result<PathBuf> {
    let vm = kiri_image::Store::default_root()?.join("vm");
    std::fs::create_dir_all(&vm)?;
    let disk = vm.join("data.img");
    if !disk.exists() {
        // In a checkout the Intel guest (guest/out/x86_64) shares the one
        // template in guest/out.
        let template = Some(guest_dir.join("data.img"))
            .filter(|t| t.is_file())
            .unwrap_or_else(|| guest_dir.join("../data.img"));
        let tmp = vm.join("data.img.tmp");
        if template.is_file() {
            std::fs::copy(&template, &tmp).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("copy data disk template {}: {e}", template.display()),
                )
            })?;
        } else {
            // The app ships the 64 GiB template as a sparse tarball (a few
            // hundred KB); tar recreates its holes, so it stays small on disk.
            let packed = guest_dir.join("data.img.tgz");
            let unpack = vm.join("unpack");
            let _ = std::fs::remove_dir_all(&unpack);
            std::fs::create_dir_all(&unpack)?;
            let ok = std::process::Command::new("/usr/bin/tar")
                .args(["-xzf"])
                .arg(&packed)
                .arg("-C")
                .arg(&unpack)
                .status()?
                .success();
            if !ok {
                return Err(io::Error::other(format!(
                    "could not unpack the data disk template {}",
                    packed.display()
                )));
            }
            std::fs::rename(unpack.join("data.img"), &tmp)?;
            let _ = std::fs::remove_dir_all(&unpack);
        }
        std::fs::rename(tmp, &disk)?;
    }
    Ok(disk)
}

#[cfg(target_os = "macos")]
/// The VM's MAC address: made once (locally administered, unicast) and kept
/// beside the data disk, so the DHCP lease stays the same across boots.
fn mac_address() -> io::Result<String> {
    let path = kiri_image::Store::default_root()?.join("vm/mac");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_owned();
        if s.len() == 17 {
            return Ok(s);
        }
    }
    let mut b = [0u8; 6];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[0] = (b[0] | 0x02) & !0x01;
    let mac = b
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, &mac)?;
    Ok(mac)
}

/// The guest kernel in the app: an arm64 `Image` for Apple Silicon, an
/// x86-64 `bzImage` for Intel (each Mac's app carries only its own;
/// `cargo xtask app [--arch x86_64]`).
#[cfg(target_arch = "x86_64")]
pub const KERNEL: &str = "bzImage";
#[cfg(not(target_arch = "x86_64"))]
pub const KERNEL: &str = "Image";

/// Whether this process runs under Rosetta (`sysctl.proc_translated`).
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn translated() -> bool {
    let mut on: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: sysctlbyname writes an int into `on`; on a Mac without
    // Rosetta the name does not exist and `on` stays 0.
    unsafe {
        libc::sysctlbyname(
            c"sysctl.proc_translated".as_ptr(),
            (&mut on as *mut libc::c_int).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    on == 1
}

/// The error for a guest kernel that isn't there. Mostly the app was
/// deleted or moved while its engine ran: say that, not a bare path.
#[cfg(target_os = "macos")]
fn missing_kernel(kernel: &std::path::Path, exe: Option<&std::path::Path>) -> io::Error {
    let msg = match exe {
        Some(exe) if !exe.exists() => format!(
            "{} was deleted or moved while it ran: open Kiri again from where it is now",
            crate::flavor::APP
        ),
        _ => format!(
            "the guest kernel {} is missing: reinstall Kiri",
            kernel.display()
        ),
    };
    io::Error::new(io::ErrorKind::NotFound, msg)
}

#[cfg(target_os = "macos")]
/// Where the kernel and initramfs live: `KIRI_GUEST_DIR`, else
/// `Kiri.app/Contents/Resources/guest` when running from the app, else the
/// in-repo build output (development).
pub fn guest_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("KIRI_GUEST_DIR") {
        return PathBuf::from(d);
    }
    let exe = std::env::current_exe().ok();
    let bundled = exe
        .as_ref()
        .and_then(|e| e.parent().map(|d| d.join("../Resources/guest")))
        .filter(|d| d.join(KERNEL).is_file());
    // From a checkout: what `cargo xtask guest`/`kernel` built, the Intel
    // guest under guest/out/x86_64. Found from the binary's place in
    // target/<profile>/, not a path compiled in, so no build machine's
    // folders end up in the binary.
    let built = if cfg!(target_arch = "x86_64") {
        "guest/out/x86_64"
    } else {
        "guest/out"
    };
    bundled.unwrap_or_else(|| {
        let repo = exe.as_ref().and_then(|e| {
            e.ancestors()
                .find(|d| d.join("Cargo.lock").is_file() && d.join("guest").is_dir())
        });
        repo.map(|r| r.join(built))
            .unwrap_or_else(|| PathBuf::from(built))
    })
}
