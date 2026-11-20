//! The default syscall filter, after Docker's: syscalls that reach outside
//! the container's namespaces (kernel keyrings, modules, the clock, swap,
//! new mount and user namespaces, io_uring, bpf) fail with EPERM unless the
//! container holds the capability that would make them legitimate. A deny
//! list rather than Docker's allow list: the same protection for a fraction
//! of the program length, so every syscall pays fewer instructions.
//!
//! A container started with its own profile (`--security-opt seccomp=file`)
//! gets a program the daemon compiled for it instead; that is installed as
//! given, and the host boundary stays on top of it.

use std::io;

/// The architecture syscalls must come from. On Apple silicon Rosetta
/// translates x86-64 binaries to arm64 syscalls, so arm64 is the only one.
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64

const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;

const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JSET_K: u16 = 0x45;
#[cfg(target_arch = "x86_64")]
const JGE_K: u16 = 0x35;
const RET_K: u16 = 0x06;

/// Without it the kernel turns on the Speculative Store Bypass mitigation
/// for every filtered task, which in a VM is a firmware call on each
/// context switch: half a millisecond on a short-lived container. That
/// mitigation guards a process against itself (JIT sandboxes), not
/// containers against each other, which separate processes already are.
const SECCOMP_FILTER_FLAG_SPEC_ALLOW: libc::c_ulong = 1 << 2;

/// Not in every libc list.
#[cfg(target_arch = "aarch64")]
const SYS_KEXEC_FILE_LOAD: libc::c_long = 294;
#[cfg(target_arch = "x86_64")]
const SYS_KEXEC_FILE_LOAD: libc::c_long = 320;

/// x86-64's x32 ABI numbers its syscalls from here; they would otherwise
/// slip past every rule.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// seccomp_data offsets.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16;

// Capability numbers that relax the filter, as in Docker's profile.
const CAP_SYS_MODULE: u32 = 16;
#[cfg(target_arch = "x86_64")]
const CAP_SYS_RAWIO: u32 = 17;
const CAP_SYS_PTRACE: u32 = 19;
const CAP_SYS_PACCT: u32 = 20;
const CAP_SYS_ADMIN: u32 = 21;
const CAP_SYS_BOOT: u32 = 22;
const CAP_SYS_NICE: u32 = 23;
const CAP_SYS_TIME: u32 = 25;
const CAP_PERFMON: u32 = 38;
const CAP_BPF: u32 = 39;

const NS_FLAGS: u32 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWCGROUP) as u32;

#[repr(C)]
#[derive(Clone, Copy)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct Prog {
    len: u16,
    filter: *const Insn,
}

const fn stmt(code: u16, k: u32) -> Insn {
    Insn { code, jt: 0, jf: 0, k }
}

const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code, jt, jf, k }
}

// Syscalls newer than the libc crate knows. The numbers are the same on
// aarch64 and x86-64 (the shared table, from 424 on).
const SYS_PROCESS_MADVISE: libc::c_long = 440;
const SYS_QUOTACTL_FD: libc::c_long = 443;
const SYS_SET_MEMPOLICY_HOME_NODE: libc::c_long = 450;
const SYS_LSM_GET_SELF_ATTR: libc::c_long = 459;
const SYS_LSM_SET_SELF_ATTR: libc::c_long = 460;
const SYS_LSM_LIST_MODULES: libc::c_long = 461;
const SYS_OPEN_TREE_ATTR: libc::c_long = 467;

/// `personality` values Docker's profile allows: the default, UNAME26, and
/// the 32-bit variants, plus 0xffffffff, the query. Anything else can turn
/// off address randomisation or make readable memory executable.
const PERSONALITIES: [u32; 5] = [0, 0x8, 0x2_0000, 0x2_0008, 0xffff_ffff];

/// Denied syscalls, each with the capability that would allow it (or none).
fn denied() -> Vec<(libc::c_long, Option<u32>)> {
    use libc::*;
    vec![
        // Kernel keyrings are not namespaced.
        (SYS_add_key, None),
        (SYS_keyctl, None),
        // Kernel log and KASLR hints; Docker allows it only with CAP_SYSLOG.
        (libc::SYS_syslog, None),
        (SYS_request_key, None),
        // Modules and kexec.
        (SYS_init_module, Some(CAP_SYS_MODULE)),
        (SYS_finit_module, Some(CAP_SYS_MODULE)),
        (SYS_delete_module, Some(CAP_SYS_MODULE)),
        (SYS_kexec_load, Some(CAP_SYS_BOOT)),
        (SYS_KEXEC_FILE_LOAD, Some(CAP_SYS_BOOT)),
        (SYS_reboot, Some(CAP_SYS_BOOT)),
        // The VM's clock and accounting.
        // clock_adjtime stays open, as in Docker's default: glibc reads the clock state through it
        // (adjtimex and ntp_adjtime with modes 0), and the kernel itself refuses any change without
        // CAP_SYS_TIME.
        (SYS_clock_settime, Some(CAP_SYS_TIME)),
        (SYS_settimeofday, Some(CAP_SYS_TIME)),
        (SYS_acct, Some(CAP_SYS_PACCT)),
        // Swap, quotas, NUMA policy.
        (SYS_swapon, Some(CAP_SYS_ADMIN)),
        (SYS_swapoff, Some(CAP_SYS_ADMIN)),
        (SYS_quotactl, Some(CAP_SYS_ADMIN)),
        (SYS_mbind, Some(CAP_SYS_NICE)),
        (SYS_set_mempolicy, Some(CAP_SYS_NICE)),
        (SYS_migrate_pages, Some(CAP_SYS_NICE)),
        (SYS_move_pages, Some(CAP_SYS_NICE)),
        // Mounts and namespaces.
        (SYS_mount, Some(CAP_SYS_ADMIN)),
        (SYS_umount2, Some(CAP_SYS_ADMIN)),
        (SYS_pivot_root, Some(CAP_SYS_ADMIN)),
        (SYS_open_tree, Some(CAP_SYS_ADMIN)),
        (SYS_move_mount, Some(CAP_SYS_ADMIN)),
        (SYS_fsopen, Some(CAP_SYS_ADMIN)),
        (SYS_fsconfig, Some(CAP_SYS_ADMIN)),
        (SYS_fsmount, Some(CAP_SYS_ADMIN)),
        (SYS_fspick, Some(CAP_SYS_ADMIN)),
        (SYS_mount_setattr, Some(CAP_SYS_ADMIN)),
        (SYS_unshare, Some(CAP_SYS_ADMIN)),
        (SYS_setns, Some(CAP_SYS_ADMIN)),
        (SYS_name_to_handle_at, Some(CAP_SYS_ADMIN)),
        (SYS_open_by_handle_at, Some(CAP_SYS_ADMIN)),
        (SYS_lookup_dcookie, Some(CAP_SYS_ADMIN)),
        (SYS_vhangup, Some(CAP_SYS_ADMIN)),
        // Kernel attack surface Docker also closes.
        (SYS_bpf, Some(CAP_BPF)),
        (SYS_perf_event_open, Some(CAP_PERFMON)),
        (SYS_userfaultfd, None),
        (SYS_io_uring_setup, None),
        (SYS_io_uring_enter, None),
        (SYS_io_uring_register, None),
        // Also closed by Docker's profile, each behind the capability that
        // makes it legitimate there.
        (SYS_sethostname, Some(CAP_SYS_ADMIN)),
        (SYS_setdomainname, Some(CAP_SYS_ADMIN)),
        (SYS_fanotify_init, Some(CAP_SYS_ADMIN)),
        (SYS_QUOTACTL_FD, Some(CAP_SYS_ADMIN)),
        (SYS_OPEN_TREE_ATTR, Some(CAP_SYS_ADMIN)),
        (SYS_LSM_GET_SELF_ATTR, Some(CAP_SYS_ADMIN)),
        (SYS_LSM_SET_SELF_ATTR, Some(CAP_SYS_ADMIN)),
        (SYS_LSM_LIST_MODULES, Some(CAP_SYS_ADMIN)),
        (SYS_kcmp, Some(CAP_SYS_PTRACE)),
        (SYS_pidfd_getfd, Some(CAP_SYS_PTRACE)),
        (SYS_PROCESS_MADVISE, Some(CAP_SYS_PTRACE)),
        (SYS_get_mempolicy, Some(CAP_SYS_NICE)),
        (SYS_SET_MEMPOLICY_HOME_NODE, Some(CAP_SYS_NICE)),
        #[cfg(target_arch = "x86_64")]
        (SYS_ioperm, Some(CAP_SYS_RAWIO)),
        #[cfg(target_arch = "x86_64")]
        (SYS_iopl, Some(CAP_SYS_RAWIO)),
    ]
}

/// Every program starts here: the native architecture only, then the
/// syscall number loaded for the rules that follow. A call from another
/// architecture (32-bit compat, x32) gets EPERM here even where a custom
/// profile's `archMap` would allow it: stricter than Docker, on purpose,
/// since the rules after this are written for native numbers.
fn head() -> Vec<Insn> {
    #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
    let mut p = vec![
        stmt(LD_W_ABS, OFF_ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, RET_ERRNO | libc::EPERM as u32),
        stmt(LD_W_ABS, OFF_NR),
    ];
    #[cfg(target_arch = "x86_64")]
    {
        p.push(jump(JGE_K, X32_SYSCALL_BIT, 0, 1));
        p.push(stmt(RET_K, RET_ERRNO | libc::EPERM as u32));
    }
    p
}

fn deny(p: &mut Vec<Insn>, nr: libc::c_long) {
    p.push(jump(JEQ_K, nr as u32, 0, 1));
    p.push(stmt(RET_K, RET_ERRNO | libc::EPERM as u32));
}

/// `personality` outside [`PERSONALITIES`] fails with EPERM. The kernel
/// reads an `unsigned int`, so only the low half of the argument counts.
/// Leaves the syscall number loaded.
fn deny_personality(p: &mut Vec<Insn>) {
    let n = PERSONALITIES.len();
    // JEQ number, LD arg0, one JEQ per value (a hit jumps to the reload at
    // the end), RET EPERM, LD number.
    p.push(jump(JEQ_K, libc::SYS_personality as u32, 0, n as u8 + 3));
    p.push(stmt(LD_W_ABS, OFF_ARG0));
    for (i, v) in PERSONALITIES.iter().enumerate() {
        p.push(jump(JEQ_K, *v, (n - i) as u8, 0));
    }
    p.push(stmt(RET_K, RET_ERRNO | libc::EPERM as u32));
    p.push(stmt(LD_W_ABS, OFF_NR));
}

/// vsock is not namespaced: with a socket a container could dial the
/// host's ports (the host.docker.internal relay, the SSH agent, the
/// control channel) past `--network none` and the SSH-agent opt-in.
/// Docker's profile refuses it for everyone; so do we, whatever the
/// capabilities. Leaves the syscall number loaded.
fn deny_vsock(p: &mut Vec<Insn>) {
    p.push(jump(JEQ_K, libc::SYS_socket as u32, 0, 4));
    p.push(stmt(LD_W_ABS, OFF_ARG0));
    p.push(jump(JEQ_K, libc::AF_VSOCK as u32, 0, 1));
    p.push(stmt(RET_K, RET_ERRNO | libc::EAFNOSUPPORT as u32));
    p.push(stmt(LD_W_ABS, OFF_NR));
}

/// The filter for `--privileged` and `seccomp=unconfined`: nothing of the
/// default profile, only the host boundary. io_uring goes too, since its
/// socket operation makes a socket without the syscall a filter sees.
fn boundary() -> Vec<Insn> {
    let mut p = head();
    for nr in [libc::SYS_io_uring_setup, libc::SYS_io_uring_enter, libc::SYS_io_uring_register] {
        deny(&mut p, nr);
    }
    deny_vsock(&mut p);
    p.push(stmt(RET_K, RET_ALLOW));
    p
}

/// Builds the filter for a container that keeps `caps` (one bit each).
fn program(caps: u64) -> Vec<Insn> {
    let has = |cap: u32| caps & (1u64 << cap) != 0;
    let mut p = head();
    for (nr, cap) in denied() {
        if cap.is_some_and(has) {
            continue;
        }
        deny(&mut p, nr);
    }
    deny_personality(&mut p);
    deny_vsock(&mut p);
    if !has(CAP_SYS_ADMIN) {
        // clone3 hides its flags in memory a filter cannot read: ENOSYS
        // makes libc fall back to clone, whose flags it can.
        p.push(jump(JEQ_K, libc::SYS_clone3 as u32, 0, 1));
        p.push(stmt(RET_K, RET_ERRNO | libc::ENOSYS as u32));
        p.push(jump(JEQ_K, libc::SYS_clone as u32, 0, 3));
        p.push(stmt(LD_W_ABS, OFF_ARG0));
        p.push(jump(JSET_K, NS_FLAGS, 0, 1));
        p.push(stmt(RET_K, RET_ERRNO | libc::EPERM as u32));
    }
    p.push(stmt(RET_K, RET_ALLOW));
    p
}

/// The kernel's limit on one filter's length.
const MAX_INSNS: usize = 4096;

/// Reads a program the daemon compiled: instructions of eight bytes each
/// (code, jt, jf, k; little-endian), as the kernel lays them out. A program
/// that is empty, ragged or longer than the kernel takes is an error.
fn decode(bytes: &[u8]) -> io::Result<Vec<Insn>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(8) || bytes.len() / 8 > MAX_INSNS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("a seccomp program of {} bytes", bytes.len())));
    }
    Ok(bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| Insn { code: u16::from_le_bytes([c[0], c[1]]), jt: c[2], jf: c[3], k: u32::from_le_bytes([c[4], c[5], c[6], c[7]]) })
        .collect())
}

/// Whether the kernel would take this program's shape.
pub fn check(program: &[u8]) -> io::Result<()> {
    decode(program).map(drop)
}

/// Installs the default filter on this process, or with `None` only the
/// host boundary; what it execs inherits it. Needs CAP_SYS_ADMIN (or
/// no_new_privs), so it runs before the user switch.
pub fn install(caps: Option<u64>) -> io::Result<()> {
    set_filter(&caps.map_or_else(boundary, program))
}

/// Installs a profile the daemon compiled together with the host boundary:
/// filters stack and the most restrictive answer wins, so no profile can
/// open what the boundary closes. The boundary goes first and the profile
/// last, so the profile's `seccomp()` call is the only one it is not
/// installed under: a profile that denies `seccomp` (or kills on it) still
/// starts, and the container sees exactly one call, as under runc.
pub fn install_profile(program: &[u8]) -> io::Result<()> {
    let program = decode(program)?;
    set_filter(&boundary())?;
    set_filter(&program)
}

/// Only the host boundary, for a container whose profile follows later
/// (`install_late`): under no-new-privileges runc installs the profile as
/// late as it can, just before the program starts.
pub fn install_boundary() -> io::Result<()> {
    set_filter(&boundary())
}

/// A profile the daemon compiled, on top of what is installed already. With
/// no-new-privileges set it needs no capability, so it can wait until the
/// user is switched and the capabilities are dropped, right before execve.
pub fn install_late(program: &[u8]) -> io::Result<()> {
    set_filter(&decode(program)?)
}

fn set_filter(insns: &[Insn]) -> io::Result<()> {
    let prog = Prog { len: insns.len() as u16, filter: insns.as_ptr() };
    // SAFETY: a valid sock_fprog whose instructions outlive the call.
    let ret = unsafe { libc::syscall(libc::SYS_seccomp, 1 /* SECCOMP_SET_MODE_FILTER */, SECCOMP_FILTER_FLAG_SPEC_ALLOW, &prog as *const Prog) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_relax_the_filter() {
        let plain = program(0).len();
        let admin = program(1 << CAP_SYS_ADMIN).len();
        assert!(admin < plain, "SYS_ADMIN allows mounts and namespaces");
        // Every jump lands inside the program.
        for p in [program(0), program(u64::MAX)] {
            for (i, insn) in p.iter().enumerate() {
                if insn.code == JEQ_K || insn.code == JSET_K {
                    assert!(i + 1 + (insn.jt.max(insn.jf) as usize) < p.len());
                }
            }
            assert_eq!(p.last().map(|i| (i.code, i.k)), Some((RET_K, RET_ALLOW)));
        }
    }

    #[test]
    fn vsock_sockets_are_refused_for_everyone() {
        for p in [program(0), program(u64::MAX), boundary()] {
            // socket(AF_VSOCK, …): the number, then arg0, then EAFNOSUPPORT.
            let at = p.iter().position(|i| i.code == JEQ_K && i.k == libc::SYS_socket as u32).expect("a socket rule");
            assert_eq!((p[at + 1].code, p[at + 1].k), (LD_W_ABS, OFF_ARG0));
            assert_eq!((p[at + 2].code, p[at + 2].k, p[at + 2].jf), (JEQ_K, libc::AF_VSOCK as u32, 1));
            assert_eq!((p[at + 3].code, p[at + 3].k), (RET_K, RET_ERRNO | libc::EAFNOSUPPORT as u32));
            // Other sockets fall through with the syscall number reloaded.
            assert_eq!((p[at + 4].code, p[at + 4].k), (LD_W_ABS, OFF_NR));
            assert_eq!(p[at].jf as usize, 4);
        }
    }

    #[test]
    fn boundary_keeps_only_the_host_boundary() {
        let p = boundary();
        let denies = |nr: libc::c_long| p.windows(2).any(|w| w[0].code == JEQ_K && w[0].k == nr as u32 && w[1].k == RET_ERRNO | libc::EPERM as u32);
        for nr in [libc::SYS_io_uring_setup, libc::SYS_io_uring_enter, libc::SYS_io_uring_register] {
            assert!(denies(nr), "io_uring makes sockets the filter cannot see");
        }
        // Nothing of the default profile: mounts, namespaces, keyrings pass.
        for nr in [libc::SYS_mount, libc::SYS_unshare, libc::SYS_keyctl, libc::SYS_clone3] {
            assert!(!denies(nr), "{nr} filtered");
        }
        assert!(!p.windows(2).any(|w| w[1].k == RET_ERRNO | libc::ENOSYS as u32), "clone3 left alone");
        for (i, insn) in p.iter().enumerate() {
            if insn.code == JEQ_K || insn.code == JSET_K {
                assert!(i + 1 + (insn.jt.max(insn.jf) as usize) < p.len());
            }
        }
        assert_eq!(p.last().map(|i| (i.code, i.k)), Some((RET_K, RET_ALLOW)));
    }

    /// What the kernel would answer for a call, for the instructions this
    /// file emits.
    fn answer(p: &[Insn], nr: u32, arg0: u32) -> u32 {
        let (mut pc, mut acc) = (0usize, 0u32);
        loop {
            let i = p[pc];
            pc += 1;
            match i.code {
                LD_W_ABS => {
                    acc = match i.k {
                        OFF_NR => nr,
                        OFF_ARCH => AUDIT_ARCH,
                        OFF_ARG0 => arg0,
                        other => panic!("offset {other}"),
                    }
                }
                RET_K => return i.k,
                JEQ_K => pc += if acc == i.k { i.jt } else { i.jf } as usize,
                #[cfg(target_arch = "x86_64")]
                JGE_K => pc += if acc >= i.k { i.jt } else { i.jf } as usize,
                JSET_K => pc += if acc & i.k != 0 { i.jt } else { i.jf } as usize,
                other => panic!("opcode {other:#x}"),
            }
        }
    }

    #[test]
    fn only_docker_s_personalities_pass() {
        let p = program(0);
        let nr = libc::SYS_personality as u32;
        for v in PERSONALITIES {
            assert_eq!(answer(&p, nr, v), RET_ALLOW, "{v:#x}");
        }
        // ADDR_NO_RANDOMIZE, READ_IMPLIES_EXEC and a made-up one.
        for v in [0x0004_0000, 0x0040_0000, 0x1234_5678] {
            assert_eq!(answer(&p, nr, v), RET_ERRNO | libc::EPERM as u32, "{v:#x}");
        }
        // The calls after it are still seen with their number.
        assert_eq!(answer(&p, libc::SYS_write as u32, 0), RET_ALLOW);
        assert_eq!(answer(&p, libc::SYS_mount as u32, 0), RET_ERRNO | libc::EPERM as u32);
        // Capabilities do not matter for it.
        assert_eq!(answer(&program(u64::MAX), nr, 0x0004_0000), RET_ERRNO | libc::EPERM as u32);
    }

    #[test]
    fn the_calls_docker_also_closes_follow_their_capabilities() {
        let eperm = RET_ERRNO | libc::EPERM as u32;
        // (call, capability that opens it, or none)
        let calls = [
            (libc::SYS_sethostname, Some(CAP_SYS_ADMIN)),
            (libc::SYS_setdomainname, Some(CAP_SYS_ADMIN)),
            (libc::SYS_fanotify_init, Some(CAP_SYS_ADMIN)),
            (SYS_QUOTACTL_FD, Some(CAP_SYS_ADMIN)),
            (SYS_OPEN_TREE_ATTR, Some(CAP_SYS_ADMIN)),
            (SYS_LSM_GET_SELF_ATTR, Some(CAP_SYS_ADMIN)),
            (SYS_LSM_SET_SELF_ATTR, Some(CAP_SYS_ADMIN)),
            (SYS_LSM_LIST_MODULES, Some(CAP_SYS_ADMIN)),
            (libc::SYS_kcmp, Some(CAP_SYS_PTRACE)),
            (libc::SYS_pidfd_getfd, Some(CAP_SYS_PTRACE)),
            (SYS_PROCESS_MADVISE, Some(CAP_SYS_PTRACE)),
            (libc::SYS_get_mempolicy, Some(CAP_SYS_NICE)),
            (SYS_SET_MEMPOLICY_HOME_NODE, Some(CAP_SYS_NICE)),
        ];
        for (nr, cap) in calls {
            assert_eq!(answer(&program(0), nr as u32, 0), eperm, "{nr} denied by default");
            match cap {
                Some(c) => assert_eq!(answer(&program(1 << c), nr as u32, 0), RET_ALLOW, "{nr} opens with cap {c}"),
                None => assert_eq!(answer(&program(u64::MAX), nr as u32, 0), eperm, "{nr} has no capability"),
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn port_io_needs_rawio() {
        let eperm = RET_ERRNO | libc::EPERM as u32;
        for nr in [libc::SYS_ioperm, libc::SYS_iopl] {
            assert_eq!(answer(&program(0), nr as u32, 0), eperm);
            assert_eq!(answer(&program(1 << CAP_SYS_RAWIO), nr as u32, 0), RET_ALLOW);
        }
    }

    #[test]
    fn a_compiled_program_is_read_back_as_sent() {
        // LD arch; JEQ native; RET EPERM ...: any whole number of eight-byte instructions.
        let mut bytes = Vec::new();
        for i in boundary() {
            bytes.extend_from_slice(&i.code.to_le_bytes());
            bytes.push(i.jt);
            bytes.push(i.jf);
            bytes.extend_from_slice(&i.k.to_le_bytes());
        }
        let read = decode(&bytes).unwrap();
        assert_eq!(read.len(), boundary().len());
        assert!(read.iter().zip(boundary()).all(|(a, b)| (a.code, a.jt, a.jf, a.k) == (b.code, b.jt, b.jf, b.k)));
        assert!(check(&bytes).is_ok());
    }

    #[test]
    fn a_program_the_kernel_would_refuse_is_refused_here() {
        assert!(decode(&[]).is_err(), "empty");
        assert!(decode(&[0; 12]).is_err(), "not whole instructions");
        assert!(decode(&vec![0; 8 * (MAX_INSNS + 1)]).is_err(), "too long");
        assert!(decode(&vec![0; 8 * MAX_INSNS]).is_ok());
    }
}
