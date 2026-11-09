//! How busy the VM is: the CPU time it has used, read from the host. On a
//! Mac the VM runs in Virtualization's own process, which macOS starts on
//! this daemon's behalf; elsewhere it runs in this process.

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::atomic::{AtomicI32, Ordering};

    /// The VM's process, once found.
    static VM_PID: AtomicI32 = AtomicI32::new(0);

    /// The process macOS holds responsible for `pid`: for an XPC service such
    /// as Virtualization's, the app it runs for, which is the app's first
    /// process rather than the engine when the window started the engine. A private function, so it is
    /// looked up rather than linked; without it there is no reading.
    fn responsible(pid: i32) -> Option<i32> {
        type F = unsafe extern "C" fn(i32) -> i32;
        static F: std::sync::OnceLock<Option<F>> = std::sync::OnceLock::new();
        let f = F.get_or_init(|| {
            // SAFETY: dlsym with a NUL-terminated name; the symbol, when there, has this signature.
            let p = unsafe {
                libc::dlsym(
                    libc::RTLD_DEFAULT,
                    c"responsibility_get_pid_responsible_for_pid".as_ptr(),
                )
            };
            (!p.is_null()).then(|| unsafe { std::mem::transmute::<*mut libc::c_void, F>(p) })
        });
        // SAFETY: takes and returns a pid.
        f.map(|f| unsafe { f(pid) })
    }

    fn is_ours(pid: i32) -> bool {
        let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is as large as we say.
        let n = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        n > 0
            && String::from_utf8_lossy(&path[..n as usize])
                .contains("com.apple.Virtualization.VirtualMachine")
            && {
                // The engine, or whatever macOS holds responsible for it: the
                // window, when the window started the engine.
                let me = std::process::id() as i32;
                let owner = responsible(pid);
                owner == Some(me) || owner == responsible(me)
            }
    }

    fn find() -> Option<i32> {
        let mut pids = vec![0i32; 8192];
        // SAFETY: the buffer is as large as we say, in bytes.
        let n =
            unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
        pids.truncate(n.max(0) as usize);
        pids.into_iter().filter(|&p| p > 0).find(|&p| is_ours(p))
    }

    fn cpu_of(pid: i32) -> Option<u64> {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: info is a rusage_info_v2, the flavor asked for.
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                (&mut info as *mut libc::rusage_info_v2).cast(),
            )
        };
        (rc == 0).then_some(info.ri_user_time + info.ri_system_time)
    }

    /// The VM's CPU time so far, in nanoseconds.
    #[allow(deprecated)] // libc's mach_timebase_info, which points to the mach2 crate
    pub fn cpu_ns() -> Option<u64> {
        let mut pid = VM_PID.load(Ordering::Relaxed);
        if pid == 0 || !is_ours(pid) {
            pid = find()?;
            VM_PID.store(pid, Ordering::Relaxed);
        }
        let ticks = cpu_of(pid)?;
        // rusage times are in Mach time units: nanoseconds on Intel, not on Apple silicon.
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: tb is the struct the call fills.
        unsafe { libc::mach_timebase_info(&mut tb) };
        Some(if tb.denom == 0 {
            ticks
        } else {
            (ticks as u128 * tb.numer as u128 / tb.denom as u128) as u64
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    /// This process's CPU time so far, in nanoseconds: the VM runs in it.
    pub fn cpu_ns() -> Option<u64> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // Fields after the command, which is in parentheses and may hold spaces.
        let rest = &stat[stat.rfind(')')? + 2..];
        let f: Vec<u64> = rest
            .split_whitespace()
            .skip(11)
            .take(2)
            .filter_map(|v| v.parse().ok())
            .collect();
        // SAFETY: sysconf takes no pointers.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        (f.len() == 2).then(|| (f[0] + f[1]) * 1_000_000_000 / hz)
    }
}

pub use imp::cpu_ns;
