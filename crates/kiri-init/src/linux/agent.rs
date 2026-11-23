//! The agent's event loop: one poll over the host connection, a signalfd for
//! child exits, and every process's output (and pending input) descriptors.
//! Single-threaded, which is also what lets clone3 copy the process safely.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use kiri_proto::{Exited, MAX_OUTPUT_CHUNK, Msg, Stats, Stream};

use super::container::{self, NetCtx, Paths, Process, Root};
use super::net::{self, Bridges, Dhcp};
use super::{fsnotify, layers, meminfo_full, sys};

/// Input held for a process that is not reading yet. The host sends at most
/// `INPUT_WINDOW` bytes beyond what `InputConsumed` acknowledged, so a
/// correct host never gets here; a broken one has the process's stdin closed
/// rather than the agent holding unbounded memory.
const MAX_PENDING_INPUT: usize = 2 * kiri_proto::INPUT_WINDOW;

struct Proc {
    run: Process,
    received_ns: u64,
    cloned_ns: u64,
    /// Set once reaped: (exit code, when).
    exited: Option<(i32, u64)>,
    input: VecDeque<u8>,
    close_stdin_when_drained: bool,
    /// When a running TTY process last had its terminal hung up: no one
    /// held the pty open, as while systemd reopens /dev/console. Kept, and
    /// looked at again shortly, rather than taken for the end of output.
    tty_hup: Option<std::time::Instant>,
}

impl Proc {
    /// The one rule for what may be entered (exec, copy): a container's
    /// own process, not an exec; its root in place; its setup not failed;
    /// not exited. Nothing may enter a container before its root is in
    /// place.
    fn enterable(&self) -> bool {
        self.run.container.is_none() && self.run.entered() && self.exited.is_none()
    }
}

/// How long a hung-up terminal waits before it's polled again.
const TTY_HUP_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

pub fn serve(conn: File, paths: &Paths) -> io::Result<()> {
    let mut procs: HashMap<libc::pid_t, Proc> = HashMap::new();
    let served = serve_loop(conn, paths, &mut procs);
    leave(&mut procs, super::transport::is_host_process());
    served
}

/// However the agent leaves its loop (Shutdown, SIGTERM, the daemon gone,
/// or an error, such as a write to a daemon that died before its end of
/// stream was read), no child still filling a new volume is killed half
/// way (see `Process::wait_root`): on a Linux host, everything is then
/// stopped and torn down (`run` sweeps what is left after this); in the
/// VM, which powers off next, it is waited for.
fn leave(procs: &mut HashMap<libc::pid_t, Proc>, host_process: bool) {
    if host_process {
        stop_containers(procs);
    } else {
        for p in procs.values_mut() {
            p.run.wait_root();
        }
    }
}

fn serve_loop(mut conn: File, paths: &Paths, procs: &mut HashMap<libc::pid_t, Proc>) -> io::Result<()> {
    let sigfd = child_signals()?;
    // In the VM a DHCP lease gives the uplink and DNS; on a Linux host the
    // host has them already.
    let host_process = super::transport::is_host_process();
    // The published ports the kernel forwards, by container (on a Linux
    // host; in the VM, TCP ones reached over the NAT network).
    let mut published: Vec<(String, super::netlink::Dnat)> = Vec::new();
    let host_dns = if host_process { super::transport::host_dns() } else { Vec::new() };
    let mut dhcp: Option<Dhcp> = if host_process {
        if let Err(e) = net::host_uplink() {
            eprintln!("kiri-init: no uplink: {e}");
        }
        None
    } else {
        match net::uplink() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("kiri-init: no uplink: {e}");
                None
            }
        }
    };
    // On a Linux host, SIGTERM or SIGINT stops the agent as Shutdown does.
    let term = if host_process { Some(term_signals()?) } else { None };
    // On a Linux host, its firewall may start, reload or flush after the engine.
    let firewall = if host_process { net::firewall_events() } else { None };
    let mut bridges = Bridges::default();
    // In the VM, lookups the gateway's resolver fails go through the host.
    let mut dns = super::dns::Dns::default();
    dns.relay = !host_process;
    let forward = super::relay::listen()?;
    // host.docker.internal's relay to the Mac (VM only). A Linux host maps
    // that name to the bridge gateway and redirects nothing here, so the
    // listener would only be a port open to the LAN and a clash with any
    // host service on it.
    let host_bound = if host_process { None } else { Some(super::relay::listen_host_bound(net::HOST_RELAY_PORT)?) };
    let framed = super::relay::listen_framed()?;
    let ssh_agent = super::relay::listen_ssh_agent()?;
    // The engine's API for containers that mount the docker socket (VM only).
    let engine_api = if host_process {
        None
    } else {
        super::relay::listen_engine().inspect_err(|e| eprintln!("kiri-init: no engine socket for containers: {e}")).ok()
    };
    let copies = super::copy::listen()?;
    // USB passthrough (VM only: on a Linux host containers see the host's devices).
    let usb = if host_process { None } else { super::usb::listen().inspect_err(|e| eprintln!("kiri-init: no USB passthrough: {e}")).ok() };
    let udp = super::relay::listen_udp()?;
    let pressure = memory_pressure_trigger();
    let mut last_pressure = std::time::Instant::now() - std::time::Duration::from_secs(1);
    // `docker system df`: measured in a child process (never a thread: see
    // `usage`), whose pipe the loop polls; the loop sends the `Usage` lines
    // (it is the only writer).
    let mut measuring: Option<super::usage::Job> = None;
    let mut relays: Vec<super::relay::Relay> = Vec::new();
    // Starts that failed after they were answered `starting`: the error the
    // daemon's next `AwaitStart` for that id gets.
    let mut failed_starts: HashMap<String, String> = HashMap::new();
    let mut fds: Vec<libc::pollfd> = Vec::with_capacity(16);
    let mut frame = Vec::with_capacity(4096);
    let mut out = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; MAX_OUTPUT_CHUNK];
    let mut fs_buf = Vec::new();
    let mut fs_events = fsnotify::Pending::default();

    loop {
        // Woken by a starting container's word (its pipe is polled below):
        // settled here, so its output is read from then on, or its failed
        // start torn down.
        settle_roots(procs, &mut dns, &mut failed_starts);
        fds.clear();
        fds.push(libc::pollfd { fd: conn.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: sigfd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        // Always present (fd -1 is ignored by poll), so process fds start at 3.
        fds.push(libc::pollfd { fd: dhcp.as_ref().map_or(-1, Dhcp::fd), events: libc::POLLIN, revents: 0 });
        for fd in dns.fds() {
            fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
        }
        fds.push(libc::pollfd { fd: forward.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: framed.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: host_bound.as_ref().map_or(-1, |l| l.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: ssh_agent.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: engine_api.as_ref().map_or(-1, |l| l.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: copies.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: usb.as_ref().map_or(-1, |l| l.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: udp.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: pressure.as_ref().map_or(-1, |f| f.as_raw_fd()), events: libc::POLLPRI, revents: 0 });
        fds.push(libc::pollfd { fd: term.as_ref().map_or(-1, |f| f.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: firewall.as_ref().map_or(-1, |f| f.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: measuring.as_ref().map_or(-1, super::usage::Job::fd), events: libc::POLLIN, revents: 0 });
        super::copy::interest(&mut fds);
        for r in &relays {
            r.interest(&mut fds);
        }
        let mut hung = false;
        for p in procs.values() {
            // A hung-up pty polls as ready all the time; skip it a moment.
            if p.tty_hup.is_some_and(|t| t.elapsed() < TTY_HUP_WAIT) {
                hung = true;
                continue;
            }
            if let Some(fd) = p.run.starting_fd() {
                fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
            }
            // Before its root is in place a child writes nothing there but
            // the reason it failed, which the agent keeps for `AwaitStart`.
            if p.run.entered() {
                for fd in [&p.run.stdout, &p.run.stderr].into_iter().flatten() {
                    fds.push(libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
                }
            }
            if let (Some(fd), false) = (&p.run.stdin, p.input.is_empty()) {
                fds.push(libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLOUT, revents: 0 });
            }
        }
        // SAFETY: `fds` is a valid array of pollfd of the given length.
        let mut timeout = dhcp.as_ref().map_or(-1, Dhcp::timeout_ms);
        if hung {
            let wait = TTY_HUP_WAIT.as_millis() as i32;
            timeout = if timeout < 0 { wait } else { timeout.min(wait) };
        }
        let wait = dns.timeout_ms();
        if wait >= 0 {
            timeout = if timeout < 0 { wait } else { timeout.min(wait) };
        }
        // Changes from the Mac still to raise: a few now, the rest after
        // whatever else is ready.
        if !fs_events.is_empty() {
            fs_events.raise_some();
            if !fs_events.is_empty() {
                timeout = 0;
            }
        }
        // SAFETY: `fds` is a valid array of pollfd of the given length.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }

        if let Some(d) = dhcp.as_mut() {
            if fds[2].revents != 0 {
                match d.on_readable() {
                    Ok(true) => {
                        if let Some(l) = &d.lease {
                            eprintln!("kiri-init: {} is {}/{} via {}", net::UPLINK, net::ip_str(l.ip), l.prefix, net::ip_str(l.gateway));
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("kiri-init: dhcp: {e}"),
                }
            }
            if let Err(e) = d.on_timer() {
                eprintln!("kiri-init: dhcp: {e}");
            }
        }

        // Output and input first, so a process's last words go out before its exit.
        for pfd in &fds[3..] {
            if pfd.revents == 0 {
                continue;
            }
            if term.as_ref().is_some_and(|t| t.as_raw_fd() == pfd.fd) {
                stop_containers(procs);
                return Ok(());
            }
            if let Some(f) = firewall.as_ref().filter(|f| f.as_raw_fd() == pfd.fd) {
                net::firewall_changed(f);
                continue;
            }
            if measuring.as_ref().is_some_and(|j| j.fd() == pfd.fd) {
                // The child is reaped like any other (`reap`).
                if let Some(lines) = measuring.as_mut().and_then(super::usage::Job::read) {
                    measuring = None;
                    out.clear();
                    kiri_proto::encode_usage(&mut out, &lines);
                    conn.write_all(&out)?;
                }
                continue;
            }
            if pfd.fd == forward.as_raw_fd() {
                while let Some(r) = super::relay::accept(&forward) {
                    relays.push(r);
                }
                continue;
            }
            if pfd.fd == framed.as_raw_fd() {
                while let Some(r) = super::relay::accept_framed(&framed) {
                    relays.push(r);
                }
                continue;
            }
            if pressure.as_ref().is_some_and(|f| f.as_raw_fd() == pfd.fd) {
                // At most ten a second: the host reacts to the first one.
                if last_pressure.elapsed() >= std::time::Duration::from_millis(100) {
                    last_pressure = std::time::Instant::now();
                    out.clear();
                    Msg::MemoryPressure.encode(&mut out);
                    conn.write_all(&out)?;
                }
                continue;
            }
            if pfd.fd == udp.as_raw_fd() {
                while let Some(r) = super::relay::accept_udp(&udp) {
                    relays.push(r);
                }
                continue;
            }
            if let Some(l) = usb.as_ref().filter(|l| pfd.fd == l.as_raw_fd()) {
                super::usb::accept(l);
                continue;
            }
            if pfd.fd == copies.as_raw_fd() {
                // Nothing may enter a container before its root is in
                // place: one still making it is copied through its mounted
                // root instead, as a stopped one is.
                settle_roots(procs, &mut dns, &mut failed_starts);
                let running: Vec<(&str, libc::pid_t)> = procs
                    .iter()
                    .filter(|(_, p)| p.enterable())
                    .map(|(pid, p)| (p.run.id.as_str(), *pid))
                    .collect();
                super::copy::accept(&copies, &running, paths);
                continue;
            }
            if pfd.fd == ssh_agent.as_raw_fd() {
                while let Some(r) = super::relay::accept_ssh_agent(&ssh_agent) {
                    relays.push(r);
                }
                continue;
            }
            if let Some(l) = engine_api.as_ref().filter(|l| l.as_raw_fd() == pfd.fd) {
                while let Some(r) = super::relay::accept_engine(l) {
                    relays.push(r);
                }
                continue;
            }
            if let Some(l) = host_bound.as_ref().filter(|l| l.as_raw_fd() == pfd.fd) {
                while let Some(r) = super::relay::accept_host_bound(l, super::net::HOST_ADDR) {
                    relays.push(r);
                }
                continue;
            }
            if super::copy::on_readable(pfd.fd, paths) {
                continue;
            }
            if let Some(r) = relays.iter_mut().find(|r| r.owns(pfd.fd)) {
                r.step();
                continue;
            }
            if dns.owns(pfd.fd) {
                let upstream = dhcp.as_ref().and_then(|d| d.lease.as_ref()).map_or(&host_dns[..], |l| &l.dns);
                dns.on_readable(pfd.fd, upstream);
                continue;
            }
            // Only process stdin is polled for writing beyond this point.
            if pfd.events & libc::POLLOUT != 0 {
                flush_input(procs, pfd.fd, &mut conn, &mut out)?;
                continue;
            }
            let Some((pid, stream)) = owner(procs, pfd.fd) else { continue };
            let p = procs.get_mut(&pid).expect("owner found it");
            let slot = if stream == Stream::Stdout { &mut p.run.stdout } else { &mut p.run.stderr };
            let fd = slot.as_ref().expect("polled fd is open").as_raw_fd();
            // SAFETY: reading into a buffer we own from a descriptor we own.
            let got = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if got > 0 {
                p.tty_hup = None;
                out.clear();
                Msg::Output { id: &p.run.id, stream, data: &chunk[..got as usize] }.encode(&mut out);
                conn.write_all(&out)?;
            } else if got < 0 && p.run.tty && p.exited.is_none() && io::Error::last_os_error().raw_os_error() == Some(libc::EIO) {
                // Nobody holds the terminal right now, but the process lives
                // and may open it again (systemd does): keep it.
                p.tty_hup = Some(std::time::Instant::now());
            } else if got == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
                // EOF, or EIO from a pty whose other side has closed.
                *slot = None;
                if p.run.tty {
                    p.run.stdin = None;
                }
            }
        }

        relays.retain(|r| !r.dead);

        // After the replies that arrived with this wake-up were read, so one
        // that came in just as its wait ran out is not thrown away.
        dns.on_timer();

        if fds[1].revents != 0 {
            drain_signals(&sigfd);
            reap(procs);
        }

        // Report processes that have exited and whose output is fully read,
        // then tear containers down after the host already has its answer.
        let done: Vec<libc::pid_t> = procs
            .iter()
            .filter(|(_, p)| p.exited.is_some() && p.run.stdout.is_none() && p.run.stderr.is_none())
            .map(|(pid, _)| *pid)
            .collect();
        for pid in done {
            let p = procs.remove(&pid).expect("listed above");
            finish(&mut conn, &mut dns, procs, p, true)?;
        }

        if fds[0].revents != 0 {
            if !read_frame(&mut conn, &mut frame)? {
                // The host went away: on a Linux host, the daemon died. Its
                // containers do not outlive it (there is no live restore).
                if host_process {
                    stop_containers(procs);
                }
                return Ok(());
            }
            let received_ns = sys::boottime_ns();
            // Whatever this request is, it sees where starting containers'
            // roots stand now.
            settle_roots(procs, &mut dns, &mut failed_starts);
            out.clear();
            match kiri_proto::decode(&frame) {
                Ok(Msg::Ping(v)) => Msg::Pong(v).encode(&mut out),
                Ok(Msg::Shutdown) => {
                    if host_process {
                        // The daemon has stopped them Docker's way already;
                        // what is left is killed and torn down.
                        stop_containers(procs);
                    } else {
                        for p in procs.values_mut().filter(|p| p.exited.is_none()) {
                            // Never half-way through filling a volume.
                            p.run.wait_root();
                            // SAFETY: signalling a child we started and have
                            // not reaped (one that ended before its root was
                            // ready is a zombie until reaped).
                            unsafe { libc::kill(p.run.pid, libc::SIGKILL) };
                        }
                    }
                    return Ok(());
                }
                Ok(Msg::StatsRequest) => {
                    let (mem_total_kib, mem_available_kib, mem_balloon_kib) = meminfo_full();
                    let agent_rss_kib = self_rss_kib();
                    let (mem_stall, oom_kills) = super::cgroup::memory_trouble();
                    Msg::Stats(Stats {
                        uptime_ns: sys::boottime_ns(),
                        mem_total_kib,
                        mem_available_kib,
                        agent_rss_kib,
                        mem_used_kib: held_kib(paths, agent_rss_kib),
                        mem_stall,
                        oom_kills,
                        mem_balloon_kib,
                    })
                    .encode(&mut out)
                }
                Ok(Msg::LayerQuery(digest)) => Msg::LayerStatus(layers::present(&paths.layers, digest)).encode(&mut out),
                // The blob follows the frame on the same stream.
                Ok(Msg::LayerBegin(begin)) => match layers::receive(&mut conn, &begin, &paths.layers) {
                    Ok(stored) => Msg::LayerStored(stored).encode(&mut out),
                    Err(e) => Msg::Error(&format!("unpack {}: {e}", begin.digest)).encode(&mut out),
                },
                Ok(Msg::Run(spec)) => match {
                    // A container that wants the network must not start before the
                    // VM has it: right after boot the DHCP lease can be a few
                    // hundred milliseconds away.
                    if spec.flags & (kiri_proto::run_flags::NET_NONE | kiri_proto::run_flags::NET_HOST) == 0 {
                        wait_for_lease(&mut dhcp, std::time::Duration::from_secs(3));
                    }
                    // Only its network namespace is joined, which exists from
                    // clone: a container still filling a volume will do.
                    let join_pid = kiri_proto::net_opts::values(spec.net.opts, kiri_proto::net_opts::NETNS).next().and_then(|t| netns_pid(procs, t));
                    // A PID namespace exists from clone too.
                    let pid_join_pid = kiri_proto::net_opts::values(spec.net.opts, kiri_proto::net_opts::PIDNS).next().and_then(|t| live_pid(procs, t));
                    failed_starts.remove(spec.id);
                    container::spawn(
                    &spec,
                    paths,
                    NetCtx { bridges: &mut bridges, dns: &mut dns, upstream_dns: dhcp.as_ref().and_then(|d| d.lease.as_ref()).map_or(&host_dns[..], |l| &l.dns), join_pid, pid_join_pid },
                )
                } {
                    Ok(run) => {
                        let cloned_ns = sys::boottime_ns();
                        Msg::Started { id: &run.id, pid: run.pid as u32, starting: !run.entered() }.encode(&mut out);
                        admit(&mut conn, &mut dns, procs, new_proc(run, received_ns, cloned_ns))?;
                    }
                    Err(e) => Msg::Error(&format!("run {}: {e}", spec.id)).encode(&mut out),
                },
                Ok(Msg::AwaitStart { id }) => match await_start(procs, &mut failed_starts, id) {
                    Ok((pid, starting)) => Msg::Started { id, pid, starting }.encode(&mut out),
                    Err(e) => Msg::Error(&e).encode(&mut out),
                },
                Ok(Msg::Exec(spec)) => {
                    // Its namespaces are entered through /proc/<pid>: only a
                    // container still running, never one exited under a freed pid.
                    // The container's own seccomp profile goes with it, if it has one.
                    match running_pid(procs, spec.container_id).and_then(|pid| procs.get(&pid).map(|p| (pid, p.run.seccomp.clone()))) {
                        None => Msg::Error(&format!("container {} is not running", spec.container_id)).encode(&mut out),
                        Some((pid, profile)) => match container::exec(&spec, pid, paths, &profile) {
                            Ok(run) => {
                                let cloned_ns = sys::boottime_ns();
                                Msg::Started { id: &run.id, pid: run.pid as u32, starting: false }.encode(&mut out);
                                admit(&mut conn, &mut dns, procs, new_proc(run, received_ns, cloned_ns))?;
                            }
                            Err(e) => Msg::Error(&format!("exec in {}: {e}", spec.container_id)).encode(&mut out),
                        },
                    }
                }
                Ok(Msg::Input { id, data }) => {
                    let mut target = None;
                    if let Some(p) = procs.values_mut().find(|p| p.run.id == id) {
                        if let Some(fd) = p.run.stdin.as_ref().map(|f| f.as_raw_fd()) {
                            if p.input.len() + data.len() > MAX_PENDING_INPUT {
                                p.input.clear();
                                p.run.stdin = None;
                            } else {
                                p.input.extend(data);
                                target = Some(fd);
                            }
                        }
                    }
                    if let Some(fd) = target {
                        flush_input(procs, fd, &mut conn, &mut out)?;
                    }
                    continue;
                }
                Ok(Msg::CloseStdin { id }) => {
                    if let Some(p) = procs.values_mut().find(|p| p.run.id == id) {
                        // A terminal has no separate end of input; keep it open.
                        if !p.run.tty {
                            if p.input.is_empty() {
                                p.run.stdin = None;
                            } else {
                                p.close_stdin_when_drained = true;
                            }
                        }
                    }
                    continue;
                }
                Ok(Msg::Resize { id, rows, cols }) => {
                    if let Some(fd) = running(procs, id).filter(|p| p.run.tty).and_then(|p| p.run.stdout.as_ref()) {
                        sys::resize_pty(fd.as_raw_fd(), rows, cols);
                    }
                    continue;
                }
                Ok(Msg::Kill { id, signal }) => {
                    if let Some(p) = procs.values_mut().find(|p| p.run.id == id && p.exited.is_none()) {
                        // A child still filling a new volume finishes first
                        // (see `Process::wait_root`); one that ended before
                        // its root was ready has failed its start, and is
                        // torn down by `settle_roots`, not signalled.
                        if p.run.wait_root() != Root::Ended {
                            // SAFETY: signalling a child we started and have not
                            // reaped, so its pid can't belong to another process.
                            unsafe { libc::kill(p.run.pid, signal as libc::c_int) };
                        }
                    }
                    continue; // no reply: the exit, if any, is reported as Exited
                }
                Ok(Msg::CancelStart { id }) => {
                    // Never waits: a child still making its root is killed
                    // now, one filling a volume once the fill is done.
                    if let Some(p) = procs.values_mut().find(|p| p.run.id == id && p.run.container.is_none() && p.exited.is_none()) {
                        p.run.cancel();
                    }
                    continue; // no reply: the start's next `AwaitStart` says how it ended
                }
                Ok(Msg::Remove { id }) => {
                    if !procs.values().any(|p| p.run.id == id) {
                        container::remove(paths, id);
                    }
                    continue;
                }
                Ok(Msg::Publish(p)) => {
                    // In the VM: from the Mac over the NAT network, TCP only.
                    let gateway = dhcp.as_ref().and_then(|d| d.lease.as_ref()).map(|l| l.gateway);
                    if !host_process && (p.udp || gateway.is_none()) {
                        Msg::Error("published ports are relayed on this platform").encode(&mut out);
                    } else {
                        let d = super::netlink::Dnat { udp: p.udp, host_ip: p.host_ip, host_port: p.host_port, to_ip: p.to_ip, to_port: p.to_port };
                        published.push((p.id.to_owned(), d));
                        let all = published.iter().map(|(_, d)| *d).collect::<Vec<_>>();
                        match if host_process { net::publish(&all) } else { net::publish_vm(&all, gateway.expect("checked")) } {
                            Ok(()) => Msg::Ack.encode(&mut out),
                            Err(e) => {
                                published.pop();
                                Msg::Error(&format!("publish {}:{}: {e}", p.host_port, p.to_port)).encode(&mut out)
                            }
                        }
                    }
                }
                Ok(Msg::Unpublish { id }) => {
                    let before = published.len();
                    published.retain(|(i, _)| i != id);
                    if published.len() != before {
                        let all = published.iter().map(|(_, d)| *d).collect::<Vec<_>>();
                        let gateway = dhcp.as_ref().and_then(|d| d.lease.as_ref()).map_or([0; 4], |l| l.gateway);
                        if let Err(e) = if host_process { net::publish(&all) } else { net::publish_vm(&all, gateway) } {
                            eprintln!("kiri-init: unpublish {id}: {e}");
                        }
                    }
                    continue;
                }
                Ok(Msg::UsageQuery) => {
                    // No reply here: replies go in order, and this can take
                    // seconds. The lines follow as a `Usage` message.
                    // One at a time: a query while one runs gets its answer.
                    if measuring.is_none() {
                        match super::usage::Job::start(&paths.layers, &paths.containers, Path::new(container::VOLUMES)) {
                            Ok(job) => measuring = Some(job),
                            Err(e) => {
                                // Answered at once, unmeasured, rather than
                                // left to the daemon's wait.
                                eprintln!("kiri-init: system df: {e}");
                                kiri_proto::encode_usage(&mut out, &[] as &[String]);
                                conn.write_all(&out)?;
                            }
                        }
                    }
                    continue;
                }
                Ok(Msg::RemoveVolume { name }) => {
                    container::remove_volume(name);
                    continue;
                }
                Ok(Msg::RemoveBridge { bridge }) => {
                    // Only Kiri's own bridges, never another interface.
                    if bridge.starts_with("kiri") && bridge != "kiri0" {
                        dns.forget_bridge(bridge);
                        bridges.remove(bridge);
                    }
                    continue;
                }
                Ok(Msg::FsEvents(lines)) => {
                    fs_events.add(lines.iter());
                    continue;
                }
                Ok(Msg::Retain(keep)) => {
                    let running: Vec<&str> = procs.values().map(|p| p.run.id.as_str()).collect();
                    container::retain(paths, keep, &running);
                    continue;
                }
                Ok(Msg::RetainLayers(keep)) => {
                    layers::retain(&paths.layers, keep.iter());
                    continue;
                }
                Ok(Msg::SetTime(ns)) => {
                    // A host process shares the host's clock; only the VM needs
                    // telling. The host sends it again every minute and after
                    // the Mac wakes: stepped only when off by more than a
                    // quarter second, so the clock is not nudged back and forth.
                    if !host_process && clock_off(ns) {
                        let ts = libc::timespec { tv_sec: (ns / 1_000_000_000) as _, tv_nsec: (ns % 1_000_000_000) as _ };
                        // SAFETY: `ts` is a valid timespec.
                        unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
                    }
                    continue;
                }
                Ok(Msg::Update { id, limits }) => match live_pid(procs, id) {
                    Some(_) => match super::cgroup::apply_limits(&paths.cgroup.join(id), &limits) {
                        Ok(()) => Msg::Ack.encode(&mut out),
                        Err(e) => Msg::Error(&format!("update: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::Freeze { id, frozen }) => match freeze_target(procs, id, frozen) {
                    Ok(_) => match super::cgroup::freeze(&paths.cgroup.join(id), frozen) {
                        Ok(()) => Msg::Ack.encode(&mut out),
                        Err(e) => Msg::Error(&format!("freeze: {e}")).encode(&mut out),
                    },
                    Err(e) => Msg::Error(&e).encode(&mut out),
                },
                Ok(Msg::StatsQuery { id }) => match netns_pid(procs, id) {
                    Some(pid) => match super::cgroup::stats(&paths.cgroup.join(id), pid) {
                        Ok(s) => Msg::ContainerStats(s).encode(&mut out),
                        Err(e) => Msg::Error(&format!("stats: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::TopQuery { id }) => match live_pid(procs, id) {
                    Some(_) => {
                        let lines = super::cgroup::top(&paths.cgroup.join(id));
                        kiri_proto::encode_top(&mut out, &lines);
                    }
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::UsbNodesQuery) => kiri_proto::encode_usb_nodes(&mut out, &super::usb::nodes()),
                Ok(Msg::ListenQuery { id }) => match netns_pid(procs, id) {
                    Some(pid) => kiri_proto::encode_listening(&mut out, &net::listening(pid)),
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::NetAttach { id, ifname, net: n }) => match netns_pid(procs, id) {
                    Some(pid) => {
                        let num: u32 = ifname.strip_prefix("eth").and_then(|v| v.parse().ok()).unwrap_or(1);
                        let host_if = extra_veth(id, num);
                        // A previous run's veth can outlive its namespace for a moment;
                        // the name is this container's own, so it can go.
                        let _ = net::detach(&host_if);
                        let mac = kiri_proto::net_opts::values(n.opts, kiri_proto::net_opts::MAC).next().and_then(net::parse_mac);
                        let v6 = net::Ip6::from_opts(n.opts);
                        let plug = net::Plug::from_opts(n.opts);
                        let attached = net::attach(&mut bridges, n.bridge, &host_if, pid, num, n.ip, n.prefix, n.gateway, mac, v6, plug)
                            // A gateway that won't take the listener doesn't
                            // stop the connect: names answer at 127.0.0.11 in
                            // the container (dns.rs `serve_or_warn`, #477).
                            .map(|()| {
                                if !plug.no_ipv4 {
                                    dns.serve_or_warn(n.bridge, n.gateway);
                                }
                            })
                            .and_then(|()| std::fs::File::open(format!("/proc/{pid}/ns/net")).and_then(|ns| dns.serve_in(&ns, pid, n.ip, n.bridge)));
                        match attached {
                            Ok(()) => {
                                if let Some(name) = kiri_proto::net_opts::values(n.opts, kiri_proto::net_opts::NETWORK).next() {
                                    dns.name_network(n.bridge, name);
                                }
                                if !plug.no_ipv4 {
                                    dns.register(n.bridge, n.dns_names.iter(), n.ip);
                                }
                                if let Some(v) = v6 {
                                    dns.register6(n.bridge, n.dns_names.iter(), n.ip, v.ip);
                                }
                                if let Some(p) = procs.get_mut(&pid) {
                                    p.run.dns_entries.push((n.bridge.to_owned(), n.ip));
                                    p.run.veths.push(host_if.clone());
                                }
                                // A user network's names come from the embedded
                                // DNS, which now answers in the container: its
                                // resolv.conf names it from here on (#449).
                                if n.bridge != "kiri0" && !plug.no_ipv4 {
                                    let upstream = dhcp.as_ref().and_then(|d| d.lease.as_ref()).map_or(&host_dns[..], |l| &l.dns);
                                    let is_kiri = |a: [u8; 4]| upstream.contains(&a) || dns.serves_at(a) || a == [1, 1, 1, 1];
                                    match container::use_embedded_dns(id, is_kiri) {
                                        Ok(Some(servers)) if !servers.is_empty() => {
                                            if let Some(owner) = dns.owner_of(pid) {
                                                dns.set_upstream(owner, servers);
                                            }
                                        }
                                        Ok(_) => {}
                                        Err(e) => eprintln!("kiri-init: {id}: resolv.conf: {e}"),
                                    }
                                }
                                Msg::Ack.encode(&mut out)
                            }
                            Err(e) => Msg::Error(&format!("connect: {e}")).encode(&mut out),
                        }
                    }
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::DnsRename { id, bridge, ip, ip6, v4, names }) => {
                    // The names registered at start are keyed by address, so
                    // they are replaced as a set; its `--dns` servers stay. A
                    // stopped container sends no such frame. No reply.
                    // Only a live container's own address on one of Kiri's
                    // bridges: a frame that raced the container's exit or its
                    // leaving the network, or that names another's address,
                    // changes nothing.
                    if rename_allowed(procs, id, bridge, ip) {
                        let ip6 = <[u8; 16]>::try_from(ip6).ok();
                        dns.rename(bridge, ip, ip6, v4, &names.iter().collect::<Vec<_>>());
                    }
                    continue;
                }
                Ok(Msg::UplinkQuery) => match dhcp.as_ref().and_then(|d| d.lease.as_ref()) {
                    Some(l) => Msg::Uplink { ip: l.ip, gateway: l.gateway }.encode(&mut out),
                    None => Msg::Error("no uplink lease").encode(&mut out),
                },
                Ok(Msg::Route { dst, prefix, via, add }) => match net::route(dst, prefix, via, add) {
                    Ok(()) => Msg::Ack.encode(&mut out),
                    Err(e) => Msg::Error(&format!("route: {e}")).encode(&mut out),
                },
                Ok(Msg::NetDetach { id, ifname, bridge, ip }) => match netns_pid(procs, id) {
                    Some(pid) => {
                        let num: u32 = ifname.strip_prefix("eth").and_then(|v| v.parse().ok()).unwrap_or(1);
                        dns.unregister(bridge, ip);
                        dns.leave(pid, bridge);
                        let host_if = extra_veth(id, num);
                        if let Some(p) = procs.get_mut(&pid) {
                            p.run.dns_entries.retain(|(b, a)| !(b == bridge && *a == ip));
                            p.run.veths.retain(|v| *v != host_if);
                        }
                        match net::detach(&host_if) {
                            Ok(()) => Msg::Ack.encode(&mut out),
                            Err(e) => Msg::Error(&format!("disconnect: {e}")).encode(&mut out),
                        }
                    }
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::FsStatRequest { id, path }) => match running_rootfs(procs, paths, id) {
                    Some(root) => match super::fsview::stat(&root, path) {
                        Ok(meta) => {
                            let name = path.rsplit('/').next().unwrap_or("");
                            Msg::FsStat(super::fsview::entry(name, meta)).encode(&mut out)
                        }
                        Err(e) => Msg::Error(&format!("stat {path}: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::FsListRequest { id, path }) => match running_rootfs(procs, paths, id) {
                    Some(root) => match super::fsview::list(&root, path) {
                        Ok(list) => {
                            let entries: Vec<_> = list.iter().map(|(n, m)| super::fsview::entry(n, *m)).collect();
                            kiri_proto::encode_fs_list(&mut out, &entries)
                        }
                        Err(e) => Msg::Error(&format!("list {path}: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::FsRead { id, path, offset, len }) => match running_rootfs(procs, paths, id) {
                    Some(root) => match super::fsview::read(&root, path, offset, len as usize, &mut fs_buf) {
                        Ok(()) => Msg::FsData(&fs_buf).encode(&mut out),
                        Err(e) => Msg::Error(&format!("read {path}: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(other) => Msg::Error(&format!("unexpected {other:?}")).encode(&mut out),
                // A frame the host expects no reply to gets none, decoded
                // or not: an answer would be read as the next one's reply.
                Err(e) if kiri_proto::takes_no_reply(&frame) => {
                    eprintln!("kiri-init: dropped a bad no-reply frame: {e:?}");
                    continue;
                }
                Err(e) => Msg::Error(&format!("bad frame: {e:?}")).encode(&mut out),
            }
            conn.write_all(&out)?;
        }
    }
}

/// A PSI trigger: readable (POLLPRI) when tasks stall on memory for 50 ms
/// within any 500 ms window. None if the kernel has no PSI.
/// Runs the DHCP client alone until it has a lease or `max` passes.
fn wait_for_lease(dhcp: &mut Option<Dhcp>, max: std::time::Duration) {
    let Some(d) = dhcp.as_mut() else { return };
    let deadline = std::time::Instant::now() + max;
    while d.lease.is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            eprintln!("kiri-init: no DHCP lease yet; starting the container without waiting longer");
            return;
        }
        let mut pfd = libc::pollfd { fd: d.fd(), events: libc::POLLIN, revents: 0 };
        let wait = (d.timeout_ms().max(0) as u128).min(left.as_millis()) as libc::c_int;
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut pfd, 1, wait.max(1)) };
        if pfd.revents != 0 {
            let _ = d.on_readable();
        }
        let _ = d.on_timer();
    }
}

fn memory_pressure_trigger() -> Option<std::fs::File> {
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open("/proc/pressure/memory").ok()?;
    f.write_all(b"some 50000 500000\0").ok()?;
    Some(f)
}

/// A running container's init that may be entered (see `Proc::enterable`):
/// one still filling a new volume reads as not running.
fn running_pid(procs: &HashMap<libc::pid_t, Proc>, id: &str) -> Option<libc::pid_t> {
    procs.iter().find(|(_, p)| p.run.id == id && p.enterable()).map(|(pid, _)| *pid)
}

/// A running container's init (not an exec), its root in place or not: for
/// what only touches its cgroup or network namespace.
fn live_pid(procs: &HashMap<libc::pid_t, Proc>, id: &str) -> Option<libc::pid_t> {
    procs.iter().find(|(_, p)| p.run.container.is_none() && p.run.id == id && p.exited.is_none()).map(|(pid, _)| *pid)
}

/// The init of a container whose network namespace may be joined: one
/// still filling a volume will do, since its own namespace exists from the
/// clone. Not one that itself joined another's (`--network container:`)
/// and is not yet entered: its child has perhaps not joined it, and
/// `/proc/<pid>/ns/net` could still be the host's. Answered as for a
/// container that is not running.
/// Whether a `DnsRename` may change names: `ip` on `bridge` is a live
/// container's own entry, and the bridge is one of Kiri's user networks.
fn rename_allowed(procs: &HashMap<libc::pid_t, Proc>, id: &str, bridge: &str, ip: [u8; 4]) -> bool {
    bridge.starts_with("kiri")
        && bridge != "kiri0"
        && ip != [0; 4]
        && procs.values().any(|p| p.run.container.is_none() && p.exited.is_none() && p.run.id == id && p.run.dns_entries.iter().any(|(b, a)| b == bridge && *a == ip))
}

fn netns_pid(procs: &HashMap<libc::pid_t, Proc>, id: &str) -> Option<libc::pid_t> {
    procs.iter().find(|(_, p)| p.run.container.is_none() && p.run.id == id && p.exited.is_none() && (!p.run.joins_netns || p.enterable())).map(|(pid, _)| *pid)
}

/// The container a `Freeze` may act on. A child still filling a new volume
/// is never frozen: the daemon's start waits (`AwaitStart`) for it to say
/// its root is in place, which a frozen child never does, and a `docker
/// commit` that paused it would then wait for that start forever. The
/// daemon waits for the start before it pauses; this is the second line.
/// Thawing touches nothing the child does, so it needs only a live one.
fn freeze_target(procs: &HashMap<libc::pid_t, Proc>, id: &str, frozen: bool) -> Result<libc::pid_t, String> {
    match (live_pid(procs, id), running_pid(procs, id)) {
        (None, _) => Err(format!("container {id} is not running")),
        (Some(pid), _) if !frozen => Ok(pid),
        (Some(_), Some(pid)) => Ok(pid),
        (Some(_), None) => Err(format!("container {id} is still starting")),
    }
}

/// Looks again, without waiting, at the roots of containers that started
/// before theirs was in place (see `Process::root`).
fn settle_roots(procs: &mut HashMap<libc::pid_t, Proc>, dns: &mut super::dns::Dns, failed: &mut HashMap<String, String>) {
    let mut ended = Vec::new();
    for (pid, p) in procs.iter_mut().filter(|(_, p)| !p.run.entered()) {
        match p.run.root() {
            Root::Ended => ended.push(*pid),
            // Its start was cancelled while it filled a volume: the fill is
            // done, so it goes now (see `Process::cancel`).
            Root::Ready if p.run.take_cancel() && p.exited.is_none() => {
                // SAFETY: SIGKILL to our own child, not reaped yet.
                unsafe { libc::kill(p.run.pid, libc::SIGKILL) };
            }
            _ => {}
        }
    }
    for pid in ended {
        let p = procs.remove(&pid).expect("listed above");
        let why = format!("run {}: {}", p.run.id, p.run.start_failure().unwrap_or_default());
        failed.insert(p.run.id.clone(), why);
        drop_failed_start(dns, procs, p);
    }
}

/// Tears down a container whose child ended before its root was in place,
/// as `spawn` does for a start that fails within its wait: no `Exited`,
/// since the start itself failed (the daemon hears it from `AwaitStart`),
/// and its output, only the failure message, is not forwarded.
fn drop_failed_start(dns: &mut super::dns::Dns, procs: &HashMap<libc::pid_t, Proc>, p: Proc) {
    if p.exited.is_none() {
        // Its pipe closed because it exited (it never execs before its
        // root is ready), and it isn't reaped yet: the pid is still ours.
        // SAFETY: reaping our own unreaped child.
        unsafe { libc::waitpid(p.run.pid, std::ptr::null_mut(), 0) };
    }
    for (bridge, ip) in &p.run.dns_entries {
        dns.unregister(bridge, *ip);
    }
    dns.drop_container(p.run.pid);
    let shared = p.run.netns != 0 && procs.values().any(|o| o.exited.is_none() && o.run.netns == p.run.netns);
    container::teardown(&p.run, shared);
}

/// `AwaitStart`: the container's pid, and whether its root is still being
/// made; an error once it is not running, or ended before its root was in
/// place (its exit is reported as usual).
fn await_start(procs: &mut HashMap<libc::pid_t, Proc>, failed: &mut HashMap<String, String>, id: &str) -> Result<(u32, bool), String> {
    if let Some(why) = failed.remove(id) {
        return Err(why);
    }
    // The run still starting, before an earlier run of the same id whose
    // exit is still being reported.
    let mine = |p: &Proc| p.run.container.is_none() && p.run.id == id;
    let pid = procs.iter().find(|(_, p)| mine(p) && !p.run.entered()).or_else(|| procs.iter().find(|(_, p)| mine(p))).map(|(pid, _)| *pid);
    let Some((pid, p)) = pid.and_then(|pid| procs.get_mut(&pid).map(|p| (pid, p))) else {
        // Its root was in place and it has exited since, its exit reported
        // as usual: the start itself succeeded.
        return Ok((0, false));
    };
    match p.run.root() {
        Root::Ready => Ok((pid as u32, false)),
        // Ended: the next `settle_roots` tears it down, and the next ask
        // gets its reason.
        Root::Starting | Root::Ended => Ok((pid as u32, true)),
    }
}

/// The host side of a container's `eth{n}` for n > 0: 15 characters at most.
fn extra_veth(id: &str, n: u32) -> String {
    format!("kx{}{n}", &id[..id.len().min(9)])
}

/// The root of a running container (not an exec), for the Mac's file
/// browser. Through /proc it is the container's own view, with its volumes
/// and bind mounts, as `docker cp` reads a running container: those are
/// mounted only in its mount namespace, so the overlay the agent mounted
/// shows their folders empty (#317). Its init just exited: that overlay.
fn running_rootfs(procs: &HashMap<libc::pid_t, Proc>, paths: &Paths, id: &str) -> Option<std::path::PathBuf> {
    if let Some(pid) = running_pid(procs, id) {
        return Some(std::path::PathBuf::from(format!("/proc/{pid}/root")));
    }
    procs.values().any(|p| p.run.container.is_none() && p.run.id == id).then(|| paths.containers.join(id).join("rootfs"))
}

fn new_proc(run: Process, received_ns: u64, cloned_ns: u64) -> Proc {
    Proc { run, received_ns, cloned_ns, exited: None, input: VecDeque::new(), close_stdin_when_drained: false, tty_hup: None }
}

/// Writes as much pending input as the process will take without blocking,
/// and tells the host how much, so it can send that much more.
fn flush_input(procs: &mut HashMap<libc::pid_t, Proc>, fd: RawFd, conn: &mut File, out: &mut Vec<u8>) -> io::Result<()> {
    let Some(p) = procs.values_mut().find(|p| p.run.stdin.as_ref().is_some_and(|f| f.as_raw_fd() == fd)) else { return Ok(()) };
    let mut consumed = 0usize;
    while !p.input.is_empty() {
        let (front, _) = p.input.as_slices();
        // SAFETY: writing from a buffer we own to a non-blocking descriptor we own.
        let n = unsafe { libc::write(fd, front.as_ptr().cast(), front.len()) };
        if n > 0 {
            p.input.drain(..n as usize);
            consumed += n as usize;
        } else {
            if io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
                // The reader is gone.
                p.input.clear();
                p.run.stdin = None;
            }
            break;
        }
    }
    if p.input.is_empty() && p.close_stdin_when_drained {
        p.run.stdin = None;
    }
    if consumed > 0 {
        out.clear();
        Msg::InputConsumed { id: &p.run.id, bytes: consumed as u32 }.encode(out);
        conn.write_all(out)?;
    }
    Ok(())
}

/// This process's resident memory, from /proc/self/statm (in pages).
/// What Kiri's Linux holds, in KiB. In the VM, where this process is init,
/// it is the root cgroup: the whole guest. On a Linux host it is the
/// containers' cgroup and this process; the daemon adds itself.
fn held_kib(paths: &Paths, agent_rss_kib: u64) -> u64 {
    if std::process::id() == 1 {
        return super::cgroup::held(std::path::Path::new("/sys/fs/cgroup")).unwrap_or(0) >> 10;
    }
    match super::cgroup::held(&paths.cgroup) {
        Some(b) => (b >> 10) + agent_rss_kib,
        None => 0,
    }
}

fn self_rss_kib() -> u64 {
    let mut buf = [0u8; 128];
    let n = std::fs::File::open("/proc/self/statm").and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    let pages: u64 = std::str::from_utf8(&buf[..n]).ok().and_then(|s| s.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0);
    // SAFETY: sysconf takes no pointers.
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64 / 1024
}

/// Whether the guest's clock is more than a quarter second from `ns`
/// (Unix nanoseconds, the host's time).
fn clock_off(ns: u64) -> bool {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    off_by_more(now, ns, 250_000_000)
}

fn off_by_more(a: u64, b: u64, limit: u64) -> bool {
    a.abs_diff(b) > limit
}

/// Reads one frame into `buf`. Returns false on a clean end of stream.
fn read_frame(conn: &mut File, buf: &mut Vec<u8>) -> io::Result<bool> {
    buf.resize(4, 0);
    if let Err(e) = conn.read_exact(&mut buf[..4]) {
        return if e.kind() == io::ErrorKind::UnexpectedEof { Ok(false) } else { Err(e) };
    }
    let len = kiri_proto::frame_len(buf[..4].try_into().expect("4 bytes"))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
    buf.resize(len, 0);
    conn.read_exact(&mut buf[4..])?;
    Ok(true)
}

fn owner(procs: &HashMap<libc::pid_t, Proc>, fd: RawFd) -> Option<(libc::pid_t, Stream)> {
    procs.iter().find_map(|(pid, p)| {
        if p.run.stdout.as_ref().is_some_and(|f| f.as_raw_fd() == fd) {
            Some((*pid, Stream::Stdout))
        } else if p.run.stderr.as_ref().is_some_and(|f| f.as_raw_fd() == fd) {
            Some((*pid, Stream::Stderr))
        } else {
            None
        }
    })
}

/// Blocks SIGCHLD and returns a signalfd that becomes readable when a child exits.
fn child_signals() -> io::Result<OwnedFd> {
    // SAFETY: sigset manipulation on a local set, then plain syscalls.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        sys::check(libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) as _, "sigprocmask")?;
        let fd = sys::check(libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) as _, "signalfd")?;
        Ok(OwnedFd::from_raw_fd(fd as RawFd))
    }
}

/// Linux host: every process still running is killed and reaped (a few
/// seconds at most), and each container torn down: mounts, cgroup, and the
/// writable layer of a `--rm` one. Veths go with their network namespaces.
fn stop_containers(procs: &mut HashMap<libc::pid_t, Proc>) {
    super::copy::stop_all();
    for p in procs.values_mut().filter(|p| p.exited.is_none()) {
        // Never half-way through filling a volume (see `Process::wait_root`).
        p.run.wait_root();
        // SAFETY: signalling a child we started and have not reaped.
        unsafe { libc::kill(p.run.pid, libc::SIGKILL) };
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        reap(procs);
        if procs.values().all(|p| p.exited.is_some()) || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for p in procs.values() {
        container::teardown(&p.run, false);
    }
    procs.clear();
}

/// The read end of a pipe written to on SIGTERM and SIGINT. A handler, not
/// a blocked signal read from a signalfd: a blocked mask would be inherited
/// by every container process, and a caught signal is reset by exec.
fn term_signals() -> io::Result<OwnedFd> {
    let mut fds = [0; 2];
    // SAFETY: plain pipe2 and sigaction; the handler only calls write(2),
    // which is async-signal-safe, on a descriptor that is never closed.
    unsafe {
        sys::check(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) as _, "pipe")?;
        TERM_PIPE.store(fds[1], std::sync::atomic::Ordering::SeqCst);
        AGENT_PID.store(libc::getpid(), std::sync::atomic::Ordering::SeqCst);
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_term as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT] {
            sys::check(libc::sigaction(sig, &sa, std::ptr::null_mut()) as _, "sigaction")?;
        }
        Ok(OwnedFd::from_raw_fd(fds[0]))
    }
}

static TERM_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
static AGENT_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn on_term(_: libc::c_int) {
    // A container's process, between its clone and its exec, still has
    // this handler (and the pipe): a signal meant for it must not stop the
    // agent.
    // SAFETY: getpid and write(2) are async-signal-safe.
    unsafe {
        if libc::getpid() == AGENT_PID.load(std::sync::atomic::Ordering::SeqCst) {
            libc::write(TERM_PIPE.load(std::sync::atomic::Ordering::SeqCst), b"t".as_ptr().cast(), 1);
        }
    }
}

fn drain_signals(fd: &OwnedFd) {
    let mut info = [0u8; 128 * 8];
    // SAFETY: reading into a local buffer from a non-blocking signalfd.
    while unsafe { libc::read(fd.as_raw_fd(), info.as_mut_ptr().cast(), info.len()) } > 0 {}
}

/// Reports an exited process to the host, then tears it down. `listener`:
/// whether its 127.0.0.11 server goes too (not when `admit` evicts it: the
/// server under that pid may be the new process's by then).
fn finish(conn: &mut File, dns: &mut super::dns::Dns, procs: &HashMap<libc::pid_t, Proc>, p: Proc, listener: bool) -> io::Result<()> {
    let (code, reaped_ns) = p.exited.unwrap_or((128 + libc::SIGKILL, sys::boottime_ns()));
    let mut out = Vec::new();
    let oom_killed = container::oom_killed(&p.run);
    Msg::Exited(Exited { id: &p.run.id, code, setup_ns: p.cloned_ns - p.received_ns, run_ns: reaped_ns - p.cloned_ns, oom_killed })
        .encode(&mut out);
    conn.write_all(&out)?;
    for (bridge, ip) in &p.run.dns_entries {
        dns.unregister(bridge, *ip);
    }
    if listener {
        dns.drop_container(p.run.pid);
    }
    // A container still running in its network namespace keeps that
    // alive (its execs went with its PID namespace and count as 0).
    let shared = p.run.netns != 0 && procs.values().any(|o| o.exited.is_none() && o.run.netns == p.run.netns);
    container::teardown(&p.run, shared);
    Ok(())
}

/// The most `admit` forwards of an exited process's leftover output, per
/// stream. More than a pipe holds (64 KiB by default, 1 MiB at most for a
/// process without privileges), so nothing written before is lost; and an
/// end to it, since a process the old one left behind can write for ever.
const LAST_OUTPUT_MAX: usize = 1 << 20;

/// Adds a started process. An exited one whose output is still being read
/// can hold the same pid, which `reap` freed and a new clone just took:
/// replaced silently, its exit was never reported (a `docker wait` or exec
/// hung) and its mounts, cgroup and DNS names stayed (#324). It is
/// finished first, with the output it still has buffered.
fn admit(conn: &mut File, dns: &mut super::dns::Dns, procs: &mut HashMap<libc::pid_t, Proc>, new: Proc) -> io::Result<()> {
    let pid = new.run.pid;
    if let Some(mut old) = procs.remove(&pid) {
        let mut chunk = vec![0u8; MAX_OUTPUT_CHUNK];
        let mut out = Vec::new();
        for (stream, fd) in [(Stream::Stdout, old.run.stdout.take()), (Stream::Stderr, old.run.stderr.take())] {
            let Some(fd) = fd else { continue };
            // Only what is in the pipe already. A process the old one left
            // behind in a session of its own can still hold the write end:
            // a read that waited for it would hold up every container, and
            // so would following one that never stops writing.
            if sys::set_nonblocking(fd.as_raw_fd()).is_err() {
                continue;
            }
            let mut left = LAST_OUTPUT_MAX;
            while left > 0 {
                // SAFETY: reading into a buffer we own from a descriptor we
                // own, made non-blocking just above.
                let got = unsafe { libc::read(fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len().min(left)) };
                if got <= 0 {
                    break;
                }
                left -= got as usize;
                out.clear();
                Msg::Output { id: &old.run.id, stream, data: &chunk[..got as usize] }.encode(&mut out);
                conn.write_all(&out)?;
            }
        }
        finish(conn, dns, procs, old, false)?;
    }
    procs.insert(pid, new);
    Ok(())
}

/// The process still running as `id`. An exited one stays in `procs` until
/// its output is read, under a pid already freed by `reap` (and, after a
/// restart, under the same id as the new one): signalling it could reach
/// another process, and would miss the running container (#324).
fn running<'a>(procs: &'a HashMap<libc::pid_t, Proc>, id: &str) -> Option<&'a Proc> {
    procs.values().find(|p| p.run.id == id && p.exited.is_none())
}

fn reap(procs: &mut HashMap<libc::pid_t, Proc>) {
    loop {
        let mut status = 0;
        // SAFETY: plain waitpid on any child.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            return;
        }
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            continue;
        };
        super::copy::exited(pid);
        if let Some(p) = procs.get_mut(&pid) {
            p.exited = Some((code, sys::boottime_ns()));
            // Nobody will read input any more.
            p.input.clear();
            p.run.stdin = None;
        }
    }
}

#[cfg(test)]
mod running_tests {
    use super::{Process, await_start, live_pid, netns_pid, new_proc, rename_allowed, running, running_pid, settle_roots};
    use std::collections::HashMap;

    #[test]
    fn a_dns_rename_changes_only_a_live_containers_own_address() {
        let ip = [10, 5, 0, 2];
        let mut procs = HashMap::new();
        let mut run = Process::for_test("c1", 100);
        run.dns_entries.push(("kiri1".to_owned(), ip));
        procs.insert(100, new_proc(run, 0, 0));
        assert!(rename_allowed(&procs, "c1", "kiri1", ip));
        assert!(!rename_allowed(&procs, "c2", "kiri1", ip), "another container's id");
        assert!(!rename_allowed(&procs, "c1", "kiri1", [10, 5, 0, 3]), "another address");
        assert!(!rename_allowed(&procs, "c1", "kiri2", ip), "a network it is not on");
        assert!(!rename_allowed(&procs, "c1", "kiri0", ip), "the default bridge");
        assert!(!rename_allowed(&procs, "c1", "eth0", ip), "not one of Kiri's bridges");
        let mut run = Process::for_test("c3", 300);
        run.dns_entries.push(("kiri1".to_owned(), [0; 4]));
        procs.insert(300, new_proc(run, 0, 0));
        assert!(!rename_allowed(&procs, "c3", "kiri1", [0; 4]), "no address");
        // Once it has exited its names are gone with it: a late frame
        // must not put them back.
        procs.get_mut(&100).unwrap().exited = Some((0, 0));
        assert!(!rename_allowed(&procs, "c1", "kiri1", ip));
    }

    #[test]
    fn a_restarted_container_is_found_never_its_exited_run() {
        // Each map hashes with its own seed, so 20 of them walk the two
        // entries in both orders.
        for _ in 0..20 {
            let mut procs = HashMap::new();
            // The first run has exited (reaped, its pid free) but its output
            // is still being read; the restart runs under the same id.
            let mut old = new_proc(Process::for_test("c1", 100), 0, 0);
            old.exited = Some((0, 0));
            procs.insert(100, old);
            assert!(running(&procs, "c1").is_none(), "only an exited run: nothing to signal");
            assert_eq!(running_pid(&procs, "c1"), None, "nor to exec into");
            procs.insert(200, new_proc(Process::for_test("c1", 200), 0, 0));
            assert_eq!(running(&procs, "c1").map(|p| p.run.pid), Some(200));
            assert_eq!(running_pid(&procs, "c1"), Some(200));
        }
    }

    #[test]
    fn a_container_still_making_its_root_cannot_be_entered() {
        use std::io::Write;
        let (r, mut w) = pipe();
        let mut procs = HashMap::new();
        let mut dns = super::super::dns::Dns::default();
        let mut failed = HashMap::new();
        procs.insert(300, new_proc(Process::starting_for_test("c1", 300, r.into(), None), 0, 0));
        // Exec (and copy) see it as not running; signals, stats and its
        // network still find it.
        assert_eq!(running_pid(&procs, "c1"), None, "no exec while its root is not in place");
        assert_eq!(live_pid(&procs, "c1"), Some(300), "its network can be joined");
        assert_eq!(netns_pid(&procs, "c1"), Some(300), "it owns its namespace, which exists from the clone");
        assert_eq!(running(&procs, "c1").map(|p| p.run.pid), Some(300), "kill still reaches it");
        assert_eq!(await_start(&mut procs, &mut failed, "c1"), Ok((300, true)));
        settle_roots(&mut procs, &mut dns, &mut failed);
        assert_eq!(running_pid(&procs, "c1"), None);
        // The child's byte: in place now.
        w.write_all(b"r").unwrap();
        settle_roots(&mut procs, &mut dns, &mut failed);
        assert_eq!(running_pid(&procs, "c1"), Some(300));
        assert_eq!(await_start(&mut procs, &mut failed, "c1"), Ok((300, false)));
        // Gone since, its exit reported as usual: the start succeeded.
        assert_eq!(await_start(&mut procs, &mut failed, "nope"), Ok((0, false)));
        // Restarted while the first run's exit is still being reported: the
        // ask is about the run still starting.
        let mut old = new_proc(Process::for_test("c3", 400), 0, 0);
        old.exited = Some((0, 0));
        procs.insert(400, old);
        let (r, _w) = pipe();
        procs.insert(401, new_proc(Process::starting_for_test("c3", 401, r.into(), None), 0, 0));
        assert_eq!(await_start(&mut procs, &mut failed, "c3"), Ok((401, true)));
    }

    #[test]
    fn a_container_still_joining_a_network_cannot_be_joined_itself() {
        use std::io::Write;
        let (r, mut w) = pipe();
        let mut procs = HashMap::new();
        let mut dns = super::super::dns::Dns::default();
        let mut failed = HashMap::new();
        let mut p = Process::starting_for_test("c1", 300, r.into(), None);
        p.joins_netns = true;
        procs.insert(300, new_proc(p, 0, 0));
        assert_eq!(live_pid(&procs, "c1"), Some(300));
        assert_eq!(netns_pid(&procs, "c1"), None, "its child may not have joined yet: as if not running");
        w.write_all(b"r").unwrap();
        settle_roots(&mut procs, &mut dns, &mut failed);
        assert_eq!(netns_pid(&procs, "c1"), Some(300), "entered: its namespace is the one joined");
    }

    #[test]
    fn a_start_that_fails_while_starting_is_an_error_not_an_exit() {
        use std::io::Write;
        let (r, w) = pipe();
        let r_fd = std::os::fd::AsRawFd::as_raw_fd(&r);
        let (err_r, mut err_w) = pipe();
        let mut procs = HashMap::new();
        let mut dns = super::super::dns::Dns::default();
        let mut failed = HashMap::new();
        let mut p = new_proc(Process::starting_for_test("c2", 301, r.into(), Some(err_r.into())), 0, 0);
        // Already reaped, so the teardown does not wait for pid 301.
        p.exited = Some((127, 0));
        procs.insert(301, p);
        err_w.write_all(b"kiri: cannot start container: mount /payload: No space left on device (os error 28)\n").unwrap();
        settle_roots(&mut procs, &mut dns, &mut failed);
        assert_eq!(await_start(&mut procs, &mut failed, "c2"), Ok((301, true)), "still starting until the pipe closes");
        drop(w);
        wait_all_writers_gone(r_fd);
        settle_roots(&mut procs, &mut dns, &mut failed);
        assert!(procs.is_empty(), "torn down, never left for the exit path (no Exited)");
        assert_eq!(running_pid(&procs, "c2"), None);
        assert_eq!(
            await_start(&mut procs, &mut failed, "c2"),
            Err("run c2: mount /payload: No space left on device (os error 28)".into()),
            "the start fails with the child's own reason"
        );
        assert!(failed.is_empty(), "answered once");
    }

    /// Waits, for at most 5 seconds, until no writer of the pipe is left:
    /// `poll` reports POLLHUP on the read end then, whatever the events
    /// asked for. Tests run as threads of one process, and a child another
    /// test forks holds a copy of every open descriptor until it execs or
    /// exits, so a write end dropped here is not yet closed for the reader.
    /// The real agent is single-threaded, so this cannot happen there.
    fn wait_all_writers_gone(read_end: std::os::fd::RawFd) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let mut pfd = libc::pollfd { fd: read_end, events: 0, revents: 0 };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
            if n > 0 && pfd.revents & libc::POLLHUP != 0 {
                return;
            }
            if n < 0 {
                assert_eq!(std::io::Error::last_os_error().kind(), std::io::ErrorKind::Interrupted, "poll failed");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the write end is still held somewhere 5 s after it was dropped"
            );
        }
    }

    fn pipe() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        let mut fds = [0; 2];
        // SAFETY: pipe2 fills two descriptors we then own.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both were just opened and are owned by nobody else.
        unsafe { (std::fs::File::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1])) }
    }

    #[test]
    fn a_new_process_on_a_freed_pid_finishes_the_old_one_first() {
        use kiri_proto::Msg;
        use std::io::Write;
        let (mut host, mut conn) = pipe();
        let mut dns = super::super::dns::Dns::default();
        let mut procs = HashMap::new();
        // Exited and reaped, with output still unread when its pid is reused.
        let (out_r, mut out_w) = pipe();
        out_w.write_all(b"bye").unwrap();
        drop(out_w);
        let mut old = new_proc(Process::for_test("old", 100), 0, 0);
        old.run.stdout = Some(out_r.into());
        old.exited = Some((3, 0));
        procs.insert(100, old);

        super::admit(&mut conn, &mut dns, &mut procs, new_proc(Process::for_test("new", 100), 0, 0)).unwrap();
        drop(conn);

        assert_eq!(procs.len(), 1);
        assert_eq!(procs[&100].run.id, "new");
        let mut frame = Vec::new();
        assert!(super::read_frame(&mut host, &mut frame).unwrap());
        assert!(matches!(kiri_proto::decode(&frame), Ok(Msg::Output { id: "old", data: b"bye", .. })), "its last output first");
        assert!(super::read_frame(&mut host, &mut frame).unwrap());
        assert!(matches!(kiri_proto::decode(&frame), Ok(Msg::Exited(e)) if e.id == "old" && e.code == 3), "then its exit");
        assert!(!super::read_frame(&mut host, &mut frame).unwrap(), "nothing about the new one");
    }

    /// A process the old one left behind, in a session of its own, can keep
    /// the old one's output pipe open for as long as it lives.
    #[test]
    fn a_new_process_on_a_freed_pid_does_not_wait_for_a_pipe_still_held() {
        use kiri_proto::Msg;
        use std::io::Write;
        // Made as `container::stdio` makes a process's stdout.
        let (out_r, out_w) = super::super::sys::pipe().unwrap();
        let mut held = std::fs::File::from(out_w);
        held.write_all(b"bye").unwrap();
        let (done, waited) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let (mut host, mut conn) = pipe();
            let mut dns = super::super::dns::Dns::default();
            let mut procs = HashMap::new();
            let mut old = new_proc(Process::for_test("old", 100), 0, 0);
            old.run.stdout = Some(out_r);
            old.exited = Some((3, 0));
            procs.insert(100, old);
            super::admit(&mut conn, &mut dns, &mut procs, new_proc(Process::for_test("new", 100), 0, 0)).unwrap();
            drop(conn);
            let mut frame = Vec::new();
            let last_words = super::read_frame(&mut host, &mut frame).unwrap() && matches!(kiri_proto::decode(&frame), Ok(Msg::Output { id: "old", data: b"bye", .. }));
            let exit = super::read_frame(&mut host, &mut frame).unwrap() && matches!(kiri_proto::decode(&frame), Ok(Msg::Exited(e)) if e.id == "old" && e.code == 3);
            let _ = done.send((last_words, exit, procs[&100].run.id.clone()));
        });
        let got = waited.recv_timeout(std::time::Duration::from_secs(5));
        // Only now does the pipe close: a read still waiting for it ends,
        // so the thread can be joined whatever happened.
        drop(held);
        thread.join().unwrap();
        assert_eq!(got, Ok((true, true, "new".to_owned())), "the old one's last output and exit go out, and nothing waits for the pipe to close");
    }

    /// The process left behind may also never stop writing. The agent has
    /// one thread: following that output held up every container.
    #[test]
    fn a_new_process_on_a_freed_pid_does_not_follow_a_writer_that_never_stops() {
        use kiri_proto::Msg;
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        let (out_r, out_w) = super::super::sys::pipe().unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let stop = stop.clone();
            let mut w = std::fs::File::from(out_w);
            let block = [b'x'; 4096];
            // Some of it is in the pipe before the new process arrives.
            w.write_all(&block).unwrap();
            std::thread::spawn(move || while !stop.load(Ordering::SeqCst) && w.write_all(&block).is_ok() {})
        };
        let (mut host, mut conn) = pipe();
        // The daemon's end: it reads what the agent sends, as it always does.
        let reader = std::thread::spawn(move || {
            let (mut frame, mut output, mut exited) = (Vec::new(), 0usize, false);
            while super::read_frame(&mut host, &mut frame).unwrap_or(false) {
                match kiri_proto::decode(&frame) {
                    Ok(Msg::Output { id: "old", data, .. }) => output += data.len(),
                    Ok(Msg::Exited(e)) if e.id == "old" => exited = true,
                    _ => {}
                }
            }
            (output, exited)
        });
        let (done, waited) = std::sync::mpsc::channel();
        let agent = std::thread::spawn(move || {
            let mut dns = super::super::dns::Dns::default();
            let mut procs = HashMap::new();
            let mut old = new_proc(Process::for_test("old", 100), 0, 0);
            old.run.stdout = Some(out_r);
            old.exited = Some((0, 0));
            procs.insert(100, old);
            super::admit(&mut conn, &mut dns, &mut procs, new_proc(Process::for_test("new", 100), 0, 0)).unwrap();
            let _ = done.send(procs[&100].run.id.clone());
        });
        let got = waited.recv_timeout(std::time::Duration::from_secs(10));
        // Whatever happened, the writer stops now, so every thread ends.
        stop.store(true, Ordering::SeqCst);
        agent.join().unwrap();
        writer.join().unwrap();
        let (output, exited) = reader.join().unwrap();
        assert_eq!(got, Ok("new".to_owned()), "the new process is taken in while the writer is still writing");
        assert!(exited, "the old one's exit goes out");
        assert!(output > 0 && output <= super::LAST_OUTPUT_MAX, "its buffered output goes out, up to the limit: {output}");
    }

    #[test]
    fn a_container_still_making_its_root_is_never_frozen() {
        use std::io::Write;
        let (r, mut w) = pipe();
        let mut procs = HashMap::new();
        let mut dns = super::super::dns::Dns::default();
        let mut failed = HashMap::new();
        procs.insert(300, new_proc(Process::starting_for_test("c1", 300, r.into(), None), 0, 0));
        // Frozen while filling its volume, it never says its root is in
        // place, and the daemon's start (and a commit that paused it) would
        // wait for that forever.
        assert_eq!(super::freeze_target(&procs, "c1", true), Err("container c1 is still starting".into()));
        assert_eq!(super::freeze_target(&procs, "c1", false), Ok(300), "thawing is harmless");
        assert_eq!(super::freeze_target(&procs, "nope", true), Err("container nope is not running".into()));
        w.write_all(b"r").unwrap();
        settle_roots(&mut procs, &mut dns, &mut failed);
        assert_eq!(super::freeze_target(&procs, "c1", true), Ok(300), "its root in place: frozen as usual");
    }

    /// The container files view (`FsStat`, `FsList`, `FsRead`) reads through
    /// the container's own root. A container still making its root has not
    /// pivoted: /proc/<pid>/root is the agent's own root then, so it is
    /// read through the overlay the agent mounted for it, as a stopped one
    /// is, until its root is in place.
    #[test]
    fn the_files_view_never_reads_through_a_container_still_making_its_root() {
        use std::io::Write;
        let (r, mut w) = pipe();
        let mut procs = HashMap::new();
        let mut dns = super::super::dns::Dns::default();
        let mut failed = HashMap::new();
        let paths = super::Paths { layers: "/l".into(), containers: "/c".into(), cgroup: "/g".into() };
        procs.insert(300, new_proc(Process::starting_for_test("c1", 300, r.into(), None), 0, 0));
        assert_eq!(super::running_rootfs(&procs, &paths, "c1"), Some("/c/c1/rootfs".into()), "not /proc/300/root, which is the agent's own root");
        assert_eq!(super::running_rootfs(&procs, &paths, "nope"), None);
        w.write_all(b"r").unwrap();
        settle_roots(&mut procs, &mut dns, &mut failed);
        assert_eq!(super::running_rootfs(&procs, &paths, "c1"), Some("/proc/300/root".into()), "its root in place: its own view, with its volumes");
    }

    /// The agent leaving on a Linux host, by error as much as by Shutdown,
    /// waits for a child still filling a new volume before it kills it.
    #[test]
    fn leaving_waits_for_a_child_still_making_its_root() {
        use std::os::fd::AsRawFd;
        // A real child, so the kill and the reap are real.
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        let (r, w) = pipe();
        let mut procs = HashMap::new();
        procs.insert(pid, new_proc(Process::starting_for_test("c1", pid, r.into(), None), 0, 0));
        // It says it began a fill (`container::FILLING`).
        let mut w = w;
        std::io::Write::write_all(&mut w, b"f").unwrap();
        let speaker = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            // SAFETY: signal 0 only asks whether the process exists.
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            // SAFETY: one byte to a pipe we own.
            unsafe { libc::write(w.as_raw_fd(), b"r".as_ptr().cast(), 1) };
            alive
        });
        let t = std::time::Instant::now();
        super::leave(&mut procs, true);
        let waited = t.elapsed();
        assert!(speaker.join().unwrap(), "not killed while it was still making its root");
        assert!(waited >= std::time::Duration::from_millis(140), "left after {waited:?}, before the child spoke");
        assert!(procs.is_empty(), "stopped and torn down");
        // SAFETY: signal 0 only asks whether the process exists.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "killed and reaped once its root was in place");
        let _ = child.try_wait();
    }
}

#[cfg(test)]
mod clock_tests {
    use super::off_by_more;

    #[test]
    fn steps_only_when_the_clock_is_off() {
        let now = 1_792_800_000_000_000_000;
        assert!(!off_by_more(now, now, 250_000_000));
        assert!(!off_by_more(now, now + 200_000_000, 250_000_000));
        assert!(!off_by_more(now + 200_000_000, now, 250_000_000));
        // An hour behind, after the Mac slept.
        assert!(off_by_more(now - 3_600_000_000_000, now, 250_000_000));
        assert!(off_by_more(now + 300_000_000, now, 250_000_000));
    }
}

