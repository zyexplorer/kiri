//! A container's cgroup: limits (`--memory`, `--cpus`, `--pids-limit`,
//! `docker update`), freezing (`docker pause`), and what `docker stats` and
//! `docker top` read.

use std::fs;
use std::io;
use std::path::Path;

use kiri_proto::{ContainerStats, Limits};

use super::sys;

/// Writes the limits into the cgroup. Zero fields mean "no limit".
pub fn apply_limits(dir: &Path, l: &Limits) -> io::Result<()> {
    let write = |file: &str, value: String| fs::write(dir.join(file), value).map_err(|e| io::Error::new(e.kind(), format!("{file}: {e}")));
    write("memory.max", if l.memory_max == 0 { "max".into() } else { l.memory_max.to_string() })?;
    // No swap in the VM: a limit is a limit.
    let _ = fs::write(dir.join("memory.swap.max"), "0");
    let period = if l.cpu_period == 0 { 100_000 } else { l.cpu_period };
    write("cpu.max", if l.cpu_quota == 0 { format!("max {period}") } else { format!("{} {period}", l.cpu_quota) })?;
    write("cpu.weight", if l.cpu_weight == 0 { "100".into() } else { l.cpu_weight.clamp(1, 10_000).to_string() })?;
    write("pids.max", if l.pids_max == 0 { "max".into() } else { l.pids_max.to_string() })?;
    Ok(())
}

/// Freezes or thaws every process in the cgroup, and waits until it is so.
pub fn freeze(dir: &Path, frozen: bool) -> io::Result<()> {
    fs::write(dir.join("cgroup.freeze"), if frozen { "1" } else { "0" })?;
    let want = if frozen { "frozen 1" } else { "frozen 0" };
    for _ in 0..200 {
        if fs::read_to_string(dir.join("cgroup.events"))?.lines().any(|l| l == want) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "the container did not freeze in time"))
}

fn keyed(text: &str, key: &str) -> u64 {
    text.lines().find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(' '))).and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// What the cgroup holds and cannot give back, in bytes: anonymous, shared
/// and kernel memory from memory.stat. The page cache is left out (the
/// kernel drops it when asked), and so are a balloon's pages, which belong
/// to a driver and are charged nowhere: MemTotal minus MemAvailable counts
/// both as used, which is why the window read nonsense. None when the
/// cgroup has no memory.stat (the root's came with Linux 5.8).
pub fn held(dir: &Path) -> Option<u64> {
    let stat = fs::read_to_string(dir.join("memory.stat")).ok()?;
    // "kernel" sums the rest since 5.18; before that, add them up.
    let kernel = match keyed(&stat, "kernel") {
        0 => ["kernel_stack", "pagetables", "percpu", "sock", "vmalloc", "slab"].iter().map(|k| keyed(&stat, k)).sum(),
        k => k,
    };
    Some(keyed(&stat, "anon") + keyed(&stat, "shmem") + kernel)
}

/// Whether the kernel killed a process in the cgroup (or below it) for
/// memory: its own limit, or the whole VM running out.
pub fn oom_killed(dir: &Path) -> bool {
    fs::read_to_string(dir.join("memory.events")).is_ok_and(|e| keyed(&e, "oom_kill") > 0)
}

/// How starved of memory the guest is: PSI's "full avg10" (the share of the
/// last 10 s in which every task waited on memory), in hundredths of a
/// percent, and the processes killed for memory since boot.
pub fn memory_trouble() -> (u64, u64) {
    let stall = fs::read_to_string("/proc/pressure/memory").ok().and_then(|p| full_avg10(&p));
    let kills = fs::read_to_string("/proc/vmstat").map(|v| keyed(&v, "oom_kill")).unwrap_or(0);
    (stall.unwrap_or(0), kills)
}

/// "full avg10" from a PSI file, in hundredths of a percent.
fn full_avg10(psi: &str) -> Option<u64> {
    let full = psi.lines().find(|l| l.starts_with("full "))?;
    let avg = full.split_whitespace().find_map(|f| f.strip_prefix("avg10="))?;
    avg.parse::<f64>().ok().map(|v| (v * 100.0).round() as u64)
}

/// What `docker stats` shows, from the cgroup and the process's network namespace.
pub fn stats(dir: &Path, pid: libc::pid_t) -> io::Result<ContainerStats> {
    let read = |f: &str| fs::read_to_string(dir.join(f)).unwrap_or_default();
    let cpu = read("cpu.stat");
    let mem_stat = read("memory.stat");
    let mut s = ContainerStats {
        read_ns: sys::boottime_ns(),
        mem_current: read("memory.current").trim().parse().unwrap_or(0),
        mem_max: read("memory.max").trim().parse().unwrap_or(0),
        mem_inactive_file: keyed(&mem_stat, "inactive_file"),
        cpu_usage_usec: keyed(&cpu, "usage_usec"),
        cpu_user_usec: keyed(&cpu, "user_usec"),
        cpu_system_usec: keyed(&cpu, "system_usec"),
        pids: read("pids.current").trim().parse().unwrap_or(0),
        // SAFETY: sysconf has no preconditions.
        online_cpus: unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as u32,
        ..Default::default()
    };
    // The first line of /proc/stat: total ticks the VM's CPUs spent.
    if let Some(line) = fs::read_to_string("/proc/stat")?.lines().next() {
        s.system_ticks = line.split_whitespace().skip(1).filter_map(|v| v.parse::<u64>().ok()).sum();
    }
    // Every interface but loopback, as Docker sums them.
    for line in fs::read_to_string(format!("/proc/{pid}/net/dev")).unwrap_or_default().lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        if name.trim() == "lo" {
            continue;
        }
        let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if v.len() >= 10 {
            s.rx_bytes += v[0];
            s.rx_packets += v[1];
            s.tx_bytes += v[8];
            s.tx_packets += v[9];
        }
    }
    for line in read("io.stat").lines() {
        for field in line.split_whitespace().skip(1) {
            if let Some(v) = field.strip_prefix("rbytes=") {
                s.io_read_bytes += v.parse::<u64>().unwrap_or(0);
            } else if let Some(v) = field.strip_prefix("wbytes=") {
                s.io_write_bytes += v.parse::<u64>().unwrap_or(0);
            }
        }
    }
    Ok(s)
}

/// Every process in the cgroup and the ones below it.
fn pids(dir: &Path, out: &mut Vec<libc::pid_t>) {
    for line in fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default().lines() {
        if let Ok(p) = line.trim().parse() {
            out.push(p);
        }
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                pids(&e.path(), out);
            }
        }
    }
}

/// `docker top`: one tab-separated line per process. The first seven fields
/// are `ps -ef`'s (user pid ppid c stime tty time); then what other `ps_args`
/// columns need (comm stat rss-KiB vsz-KiB %cpu %mem elapsed-secs cpu-secs
/// uid); the command line last. The host picks and formats the columns.
pub fn top(dir: &Path) -> Vec<String> {
    let mut list = Vec::new();
    pids(dir, &mut list);
    list.sort_unstable();
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    // SAFETY: as above.
    let page_kib = (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1024) as u64) / 1024;
    let mem_kib = super::meminfo().0.max(1);
    let uptime_secs: f64 = fs::read_to_string("/proc/uptime").ok().and_then(|u| u.split_whitespace().next()?.parse().ok()).unwrap_or(0.0);
    let boot_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) - uptime_secs;
    let mut out = Vec::with_capacity(list.len());
    for pid in list {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // The command name is in parentheses and may hold spaces.
        let Some(close) = stat.rfind(')') else { continue };
        let f: Vec<&str> = stat[close + 2..].split_whitespace().collect();
        if f.len() < 22 {
            continue;
        }
        let num = |i: usize| f[i].parse::<i64>().unwrap_or(0);
        let ppid = f[1];
        let tty_nr = num(4) as u32;
        let ticks = (num(11) + num(12)).max(0) as u64;
        let start_ticks = num(19).max(0) as u64;
        let started = boot_unix + start_ticks as f64 / hz as f64;
        let elapsed = (uptime_secs - start_ticks as f64 / hz as f64).max(0.01);
        let cpu = (ticks as f64 / hz as f64) / elapsed * 100.0;
        let c = cpu as u64;
        let uid = fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Uid:").and_then(|v| v.split_whitespace().next().map(str::to_owned))))
            .unwrap_or_default();
        let user = if uid == "0" { "root".to_owned() } else { uid.clone() };
        let comm = stat[stat.find('(').map_or(0, |i| i + 1)..close].replace('\t', " ");
        let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let mut cmd = String::from_utf8_lossy(&cmd).replace(['\0', '\t'], " ").trim().to_owned();
        if cmd.is_empty() {
            cmd = format!("[{comm}]");
        }
        let secs = ticks / hz;
        let time = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
        let stime = {
            let t = started as i64;
            let day = t.rem_euclid(86_400);
            format!("{:02}:{:02}", day / 3600, day / 60 % 60)
        };
        let tty = if tty_nr == 0 { "?".to_owned() } else { format!("pts/{}", tty_nr & 0xff) };
        // ps's STAT: the state, then < or N (nice), s (session leader), l
        // (threads), + (foreground).
        let mut stat_col = f[0].to_owned();
        let nice = num(16);
        if nice < 0 {
            stat_col.push('<');
        } else if nice > 0 {
            stat_col.push('N');
        }
        if num(3) == pid as i64 {
            stat_col.push('s');
        }
        if num(17) > 1 {
            stat_col.push('l');
        }
        if num(5) > 0 && num(5) == num(2) {
            stat_col.push('+');
        }
        let rss = num(21).max(0) as u64 * page_kib;
        let vsz = num(20).max(0) as u64 / 1024;
        let mem = rss as f64 * 100.0 / mem_kib as f64;
        out.push(format!("{user}\t{pid}\t{ppid}\t{c}\t{stime}\t{tty}\t{time}\t{comm}\t{stat_col}\t{rss}\t{vsz}\t{cpu:.1}\t{mem:.1}\t{}\t{secs}\t{uid}\t{cmd}", elapsed as u64));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_pressure_reads_full_avg10() {
        let psi = "some avg10=100.00 avg60=100.00 avg300=98.32 total=1712105326\nfull avg10=91.73 avg60=94.09 avg300=81.92 total=1200142082\n";
        assert_eq!(full_avg10(psi), Some(9173));
        assert_eq!(full_avg10("some avg10=1.00 avg60=0 avg300=0 total=1\n"), None, "no full line");
    }

    #[test]
    fn oom_kills_come_from_their_line() {
        assert_eq!(keyed("low 0\nhigh 0\nmax 3\noom 2\noom_kill 1\noom_group_kill 0\n", "oom_kill"), 1);
        assert_eq!(keyed("pgfault 9\noom_kill 0\n", "oom_kill"), 0);
    }
}
