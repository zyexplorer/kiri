//! `--security-opt seccomp=...`: Docker's seccomp profiles, compiled here to
//! a classic-BPF program the guest installs as it is.
//!
//! The guest agent has no JSON parser and stays small and single-threaded,
//! so the daemon does the reading: the profile (the docker CLI sends the
//! file's JSON inline) is parsed, its `includes`/`excludes` are decided from
//! the container's capabilities and the kernel the containers run on, and
//! what is left is turned into a filter for the guest's architecture, which
//! is this daemon's own. The guest puts its host boundary (no vsock sockets,
//! no io_uring) under it, so no profile can open what that closes.
//!
//! Semantics follow runc's `matchCall` (libcontainer/seccomp/
//! seccomp_linux.go) and libseccomp's rule database (src/db.c,
//! `db_rule_add`), which is what Docker runs:
//!
//! - a rule whose action equals `defaultAction` is skipped, whatever its
//!   conditions, so it never hides a later rule for the same call;
//! - unknown syscall names are skipped;
//! - for one call an unconditional rule beats every conditional one, in
//!   either order (libseccomp drops a conditional rule when the call already
//!   has an unconditional one and clears the conditional ones when an
//!   unconditional one arrives); the first unconditional rule's action
//!   stays;
//! - a rule's conditions are ANDed, unless two of them name the same
//!   argument index: then runc adds every condition of the rule as its own
//!   rule, so they are ORed;
//! - conditional rules for one call are not tried in profile order.
//!   libseccomp keeps them in one tree per call (`db_rule_add`,
//!   `_db_tree_prune` and `_db_tree_add`, src/db.c, the same in 2.5.5 and
//!   2.6.0): a rule is a chain of nodes, one per 32-bit half of each
//!   condition, and the nodes of a level are kept in the order `_db_chain_lt`
//!   gives (the higher argument index first, the high half of a value before
//!   the low half, then operator priority, EQ, NE and MASKED_EQ over LT and
//!   LE over GT and GE, then the datum), so with `socket` denied for
//!   arg0 == 10 (errno 13) and for arg1 & 15 == 2 (errno 95) a call that
//!   meets both gets 95 in either profile order, as on Docker. `tree.rs`
//!   is a line-for-line port of those functions, so the same rules give the
//!   same tree, and `call.rs` writes the tree out as the filter.
//! - the tree also decides which rules stand. A rule that is the start of
//!   another (its conditions are the first nodes of the other's, as
//!   `_db_rule_gen_64` builds them) keeps what the profile order gave it: the
//!   longer rule after the shorter one is dropped without a word, so the
//!   shorter rule's action stands wherever it holds; the longer rule before
//!   the shorter one is refused (EEXIST: Docker's "error adding seccomp rule
//!   for syscall socket: file exists", which fails the container start),
//!   unless both have the same action, when the shorter takes the longer's
//!   place. Two rules with the same conditions and different actions are
//!   refused the same way. Checked live on Docker 29.8.2 with `socket`
//!   arg0 == 10 (errno 13) and arg0 == 10 and arg1 & 15 == 2 (errno 95).
//! - Kiri's answer is always the action of a rule that holds, or the default
//!   when none does. The tree is read as libseccomp's filter reads it, but
//!   where it ends in an action for a rule that does not hold (its tree can
//!   share nodes between two parents and then answers for the wrong rule)
//!   that action goes to a rule that has it and holds, or else the rules are
//!   read one after the other. A call is never let through by a rule that
//!   does not hold, whatever libseccomp does with the profile.
//! - where libseccomp's own filter is not its tree, Kiri follows the tree.
//!   `_gen_bpf_node` skips the load of an argument half that the node before
//!   it has loaded, and the code of that node's subtree can fall through to
//!   it with another argument in the accumulator. Where that happens
//!   Docker's answer is not the tree's, and it is not always the action of a
//!   rule that holds. Of 4,500 generated profiles that Docker's libseccomp
//!   and Kiri both take, Kiri gives the same answer as libseccomp's tree
//!   at every point where Docker's answer is the action of a rule that holds
//!   and differs from Kiri's. On 8 of the profiles Docker's answer is the
//!   action of a rule that holds at every point and still differs from
//!   Kiri's at some, and on 4 of the 8 Kiri's is the weaker one. Docker's
//!   default profile has only single-condition EQ,
//!   MASKED_EQ and LT rules and compiles to the same program as before.
//! - libseccomp also builds a tree for each other architecture it is given
//!   (x86 and x32 next to amd64); Kiri filters the native one only, so an
//!   EEXIST that only a 32-bit tree gives is not reproduced. A call with
//!   more conditional rules than the filter can hold tests is refused as too
//!   large, never cut.
//! - a call no rule matches gets `defaultAction`.
//!
//! Kiri differs in one more place, on purpose: only the native architecture is
//! filtered for, and a call from any other (32-bit compat on arm64 or
//! x86-64, x32) gets EPERM even where the profile's `archMap` or
//! `architectures` would allow it. That is stricter than Docker and a
//! boundary the guest keeps. Anything Kiri cannot do is an error that names
//! it, never a fallback.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;

use super::seccomp_names as names;

mod call;
mod tree;

/// Kiri's own VM kernel (macOS), for `minKernel` where the daemon does not
/// run on the containers' kernel itself. The guest agent does not report a
/// version.
#[cfg_attr(target_os = "linux", allow(dead_code))]
const VM_KERNEL: (u32, u32) = (6, 18);

/// The kernel containers run on, as (major, minor), for `minKernel`. On
/// Linux (native and WSL 2) the engine runs on the host's kernel, so that is
/// what `/proc/sys/kernel/osrelease` says, or `uname(2)` when that cannot be
/// read or makes no version; there is no guess after those. On macOS it is
/// Kiri's VM kernel.
pub fn guest_kernel() -> Result<(u32, u32), String> {
    #[cfg(target_os = "linux")]
    return host_kernel(super::api::kernel_release(), uname_release).ok_or_else(|| "the kernel's version cannot be read (neither /proc/sys/kernel/osrelease nor uname gives one), which this profile's minKernel needs".to_owned());
    #[cfg(not(target_os = "linux"))]
    Ok(VM_KERNEL)
}

/// The version from the release the daemon read, or else from `uname`'s.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn host_kernel(proc: Option<String>, uname: impl FnOnce() -> Option<String>) -> Option<(u32, u32)> {
    proc.as_deref().and_then(release_version).or_else(|| uname().as_deref().and_then(release_version))
}

#[cfg(target_os = "linux")]
fn uname_release() -> Option<String> {
    // SAFETY: uname fills a plain struct of byte arrays.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return None;
    }
    // SAFETY: the kernel NUL-terminates the release.
    let r = unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) };
    Some(r.to_string_lossy().into_owned())
}

/// "6.8.0-31-generic", "6.6.87.2-microsoft-standard-WSL2" or "6.18-kiri" as
/// (6, 8), (6, 6) and (6, 18).
fn release_version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor: String = parts.next()?.chars().take_while(char::is_ascii_digit).collect();
    Some((major, minor.parse().ok()?))
}

/// The kernel's limit on one filter's length.
const MAX_INSNS: usize = 4096;

// seccomp_data offsets (both architectures are little-endian).
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARGS: u32 = 16;

// Classic BPF opcodes.
const LD_W_ABS: u16 = 0x20;
const AND_K: u16 = 0x54;
const JEQ_K: u16 = 0x15;
const JGT_K: u16 = 0x25;
const JGE_K: u16 = 0x35;
const RET_K: u16 = 0x06;
const JA: u16 = 0x05;

// Return values.
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_KILL_THREAD: u32 = 0;
const RET_TRAP: u32 = 0x0003_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_TRACE: u32 = 0x7ff0_0000;
const RET_LOG: u32 = 0x7ffc_0000;
const RET_ALLOW: u32 = 0x7fff_0000;

/// Longest run of calls one block of compares may cover: every jump in it
/// has to fit an eight-bit offset.
const RUN: usize = 200;

/// Syscalls hot in most programs. Calls with no condition and the same
/// answer can be tested in any order, so these go first and pay the fewest
/// compares.
const HOT: [&str; 24] = [
    "read", "write", "futex", "epoll_pwait", "epoll_wait", "ppoll", "openat", "close", "newfstatat", "fstat", "mmap", "munmap", "mprotect", "brk",
    "clock_gettime", "rt_sigaction", "rt_sigprocmask", "ioctl", "lseek", "getdents64", "fcntl", "recvfrom", "sendto", "pread64",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// The guest's architecture: this daemon's own.
    pub const fn native() -> Arch {
        if cfg!(target_arch = "aarch64") { Arch::Aarch64 } else { Arch::X86_64 }
    }

    fn audit(self) -> u32 {
        match self {
            Arch::X86_64 => 0xC000_003E,
            Arch::Aarch64 => 0xC000_00B7,
        }
    }

    fn table(self) -> &'static [(&'static str, u32)] {
        match self {
            Arch::X86_64 => &names::X86_64,
            Arch::Aarch64 => &names::AARCH64,
        }
    }

    fn number(self, name: &str) -> Option<u32> {
        let t = self.table();
        t.binary_search_by(|(n, _)| (*n).cmp(name)).ok().map(|i| t[i].1)
    }

    /// The names Docker's profiles call this architecture by.
    fn names(self) -> [&'static str; 3] {
        match self {
            Arch::X86_64 => ["amd64", "x86_64", "scmp_arch_x86_64"],
            Arch::Aarch64 => ["arm64", "aarch64", "scmp_arch_aarch64"],
        }
    }
}

/// What `--security-opt` says about seccomp.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice<'a> {
    /// Kiri's built-in default filter.
    Default,
    /// `seccomp=unconfined`.
    Unconfined,
    /// A profile, as JSON.
    Profile(&'a str),
}

/// Reads the seccomp entry of `--security-opt` (the last one wins). A value
/// that is neither a keyword nor a JSON object is refused: the docker CLI
/// reads a profile file itself and sends its contents, so a bare path here
/// means a client that did not.
pub fn choice(security_opt: &[String]) -> Result<Choice<'_>, String> {
    let Some(value) = security_opt.iter().rev().find_map(|o| o.strip_prefix("seccomp=").or_else(|| o.strip_prefix("seccomp:"))) else {
        return Ok(Choice::Default);
    };
    let v = value.trim();
    match v {
        "" | "builtin" => Ok(Choice::Default),
        "unconfined" => Ok(Choice::Unconfined),
        _ if v.starts_with('{') => Ok(Choice::Profile(v)),
        _ => Err(format!("invalid seccomp profile {v:?}: expected \"builtin\", \"unconfined\" or the profile's JSON (the docker CLI reads a profile file and sends its contents)")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    default_action: Option<String>,
    default_errno_ret: Option<u32>,
    #[serde(default)]
    syscalls: Vec<Rule>,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    listener_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rule {
    #[serde(default)]
    names: Vec<String>,
    /// The old, single-name form.
    name: Option<String>,
    action: Option<String>,
    errno_ret: Option<u32>,
    #[serde(default)]
    args: Vec<Arg>,
    #[serde(default)]
    includes: Filter,
    #[serde(default)]
    excludes: Filter,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Filter {
    #[serde(default)]
    caps: Vec<String>,
    #[serde(default)]
    arches: Vec<String>,
    min_kernel: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Arg {
    index: u32,
    #[serde(default)]
    value: u64,
    #[serde(default)]
    value_two: u64,
    op: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ne,
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
    MaskedEq,
}

#[derive(Clone, Copy)]
struct Cond {
    index: u32,
    op: Op,
    value: u64,
    /// MASKED_EQ: what the masked argument must equal.
    datum: u64,
}

struct Entry {
    nr: u32,
    /// The call's tests, in libseccomp's order; empty when one action
    /// decides the call.
    tree: Vec<call::TInst>,
    ret: u32,
    /// Position in [`HOT`], or past it.
    rank: usize,
}

#[derive(Clone, Copy)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

const fn stmt(code: u16, k: u32) -> Insn {
    Insn { code, jt: 0, jf: 0, k }
}

const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code, jt, jf, k }
}

fn action(name: &str, errno: Option<u32>) -> Result<u32, String> {
    // The kernel reports errors up to 4095.
    let e = errno.unwrap_or(libc::EPERM as u32).min(4095);
    Ok(match name {
        "SCMP_ACT_ALLOW" => RET_ALLOW,
        "SCMP_ACT_ERRNO" => RET_ERRNO | e,
        "SCMP_ACT_KILL" | "SCMP_ACT_KILL_THREAD" => RET_KILL_THREAD,
        "SCMP_ACT_KILL_PROCESS" => RET_KILL_PROCESS,
        "SCMP_ACT_TRAP" => RET_TRAP,
        "SCMP_ACT_LOG" => RET_LOG,
        // With no tracer the kernel answers ENOSYS, as runc's does.
        "SCMP_ACT_TRACE" => RET_TRACE | errno.unwrap_or(0).min(0xffff),
        "SCMP_ACT_NOTIFY" => return Err("seccomp action SCMP_ACT_NOTIFY (a listener) is not supported".into()),
        other => return Err(format!("unknown seccomp action {other:?}")),
    })
}

fn op(name: &str) -> Result<Op, String> {
    Ok(match name {
        "SCMP_CMP_NE" => Op::Ne,
        "SCMP_CMP_LT" => Op::Lt,
        "SCMP_CMP_LE" => Op::Le,
        "SCMP_CMP_EQ" => Op::Eq,
        "SCMP_CMP_GE" => Op::Ge,
        "SCMP_CMP_GT" => Op::Gt,
        "SCMP_CMP_MASKED_EQ" => Op::MaskedEq,
        other => return Err(format!("unknown seccomp comparison {other:?}")),
    })
}

/// "4.8" or "5.10.1" as (major, minor).
fn kernel_version(v: &Value) -> Result<(u32, u32), String> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => return Err(format!("invalid minKernel {other}")),
    };
    let mut parts = s.split('.');
    let mut next = || parts.next().map(|p| p.trim().parse::<u32>());
    match (next(), next()) {
        (Some(Ok(major)), Some(Ok(minor))) => Ok((major, minor)),
        (Some(Ok(major)), None) => Ok((major, 0)),
        _ => Err(format!("invalid minKernel {s:?}")),
    }
}

/// Whether a capability named like `CAP_SYS_ADMIN` is in `caps`. A name this
/// kernel has no such capability for is never held.
fn holds(caps: u64, name: &str) -> bool {
    let n = name.trim().to_ascii_uppercase();
    let n = n.strip_prefix("CAP_").unwrap_or(&n);
    super::api::CAPS.iter().position(|c| *c == n).is_some_and(|i| caps & (1u64 << i) != 0)
}

impl Rule {
    /// Whether the rule applies to this container: `excludes` that match
    /// drop it, `includes` that do not match drop it (Docker's reading).
    fn applies(&self, arch: Arch, caps: u64, kernel: &dyn Fn() -> Result<(u32, u32), String>) -> Result<bool, String> {
        let on_arch = |list: &[String]| list.iter().any(|a| arch.names().contains(&a.to_ascii_lowercase().as_str()));
        if on_arch(&self.excludes.arches) || self.excludes.caps.iter().any(|c| holds(caps, c)) {
            return Ok(false);
        }
        if let Some(v) = &self.excludes.min_kernel
            && kernel()? >= kernel_version(v)?
        {
            return Ok(false);
        }
        if !self.includes.arches.is_empty() && !on_arch(&self.includes.arches) {
            return Ok(false);
        }
        if !self.includes.caps.iter().all(|c| holds(caps, c)) {
            return Ok(false);
        }
        if let Some(v) = &self.includes.min_kernel
            && kernel()? < kernel_version(v)?
        {
            return Ok(false);
        }
        Ok(true)
    }
}

/// Compiles a profile for the guest's architecture and a container that
/// keeps `caps` (one bit per capability, as the kernel numbers them).
pub fn compile(json: &str, caps: u64) -> Result<Vec<u8>, String> {
    compile_with(Arch::native(), json, caps, &guest_kernel)
}

/// For the tests: the macOS VM's kernel.
#[cfg(test)]
pub fn compile_for(arch: Arch, json: &str, caps: u64) -> Result<Vec<u8>, String> {
    compile_for_kernel(arch, json, caps, VM_KERNEL)
}

#[cfg(test)]
pub fn compile_for_kernel(arch: Arch, json: &str, caps: u64, kernel: (u32, u32)) -> Result<Vec<u8>, String> {
    compile_with(arch, json, caps, &|| Ok(kernel))
}

/// `kernel` is asked only when a rule has a `minKernel`.
fn compile_with(arch: Arch, json: &str, caps: u64, kernel: &dyn Fn() -> Result<(u32, u32), String>) -> Result<Vec<u8>, String> {
    let profile: Profile = serde_json::from_str(json).map_err(|e| format!("invalid seccomp profile: {e}"))?;
    if profile.listener_path.as_deref().is_some_and(|p| !p.is_empty()) {
        return Err("seccomp listenerPath is not supported".into());
    }
    for f in &profile.flags {
        // Kiri always sets this one, for every filter.
        if f != "SECCOMP_FILTER_FLAG_SPEC_ALLOW" {
            return Err(format!("seccomp flag {f:?} is not supported"));
        }
    }
    let default = action(profile.default_action.as_deref().ok_or("invalid seccomp profile: defaultAction is required")?, profile.default_errno_ret)?;

    // What a profile rule becomes, as runc's `matchCall` reads it.
    struct Raw {
        nr: u32,
        /// One entry per libseccomp rule this profile rule becomes.
        groups: Vec<Vec<Cond>>,
        ret: u32,
        rank: usize,
    }
    let mut raws: Vec<Raw> = Vec::new();
    for rule in &profile.syscalls {
        let ret = action(rule.action.as_deref().ok_or("invalid seccomp profile: a syscall rule has no action")?, rule.errno_ret)?;
        // runc: a rule with the default's own action is redundant and is
        // skipped before anything else about it is looked at, its
        // `includes` and `excludes` and conditions included.
        if ret == default {
            continue;
        }
        let applies = rule.applies(arch, caps, kernel)?;
        let names: Vec<(u32, &String)> = rule.names.iter().chain(rule.name.iter()).filter_map(|n| arch.number(n).map(|nr| (nr, n))).collect();
        // libseccomp skips names the architecture has no call for, and runc
        // never reads the conditions of a rule that has none left.
        if names.is_empty() {
            continue;
        }
        // Checked whatever the container's capabilities are, so a profile
        // that is wrong is refused wherever it runs.
        let mut conds = Vec::new();
        for a in &rule.args {
            if a.index > 5 {
                return Err(format!("seccomp argument index {} is out of range (0 to 5)", a.index));
            }
            conds.push(Cond { index: a.index, op: op(&a.op)?, value: a.value, datum: a.value_two });
        }
        if !applies {
            continue;
        }
        // runc: two conditions on one argument index make every condition of
        // the rule a rule of its own (ORed); otherwise they are one (ANDed).
        // More than six conditions cannot have six different indexes, so such
        // a rule is always split.
        let mut per_index = [0u32; 6];
        for c in &conds {
            per_index[c.index as usize] += 1;
        }
        let groups: Vec<Vec<Cond>> = if per_index.iter().any(|&n| n > 1) { conds.iter().map(|c| vec![*c]).collect() } else { vec![conds] };
        let mut seen = HashSet::new();
        for (nr, name) in names {
            if !seen.insert(nr) {
                continue;
            }
            let rank = HOT.iter().position(|h| h == name).unwrap_or(HOT.len());
            raws.push(Raw { nr, groups: groups.clone(), ret, rank });
        }
    }
    // One call's rules: the first unconditional rule decides the call and
    // drops the conditional ones, as in libseccomp's rule database; the
    // conditional ones are kept in profile order, and `call::compile` puts
    // them through libseccomp's own tree.
    struct Call {
        rank: usize,
        uncond: Option<u32>,
        rules: Vec<(Vec<Cond>, u32)>,
    }
    let mut calls: std::collections::HashMap<u32, Call> = std::collections::HashMap::new();
    let mut order: Vec<u32> = Vec::new();
    for r in &raws {
        let c = calls.entry(r.nr).or_insert_with(|| {
            order.push(r.nr);
            Call { rank: r.rank, uncond: None, rules: Vec::new() }
        });
        for g in &r.groups {
            if c.uncond.is_some() {
                break;
            }
            // A mask of nothing tests nothing.
            let mut conds: Vec<Cond> = g.iter().filter(|x| !(x.op == Op::MaskedEq && x.value == 0)).copied().collect();
            if conds.is_empty() {
                c.uncond = Some(r.ret);
                c.rules.clear();
                continue;
            }
            conds.sort_by_key(|x| x.index);
            c.rules.push((conds, r.ret));
        }
    }
    // Every test is at least two instructions.
    let mut budget = MAX_INSNS / 2;
    // Steps all the calls together may spend walking libseccomp's trees (a
    // tree that loops costs what is left); then calls are read rule by rule.
    let mut pool: u64 = TREE_STEPS;
    let mut entries: Vec<Entry> = Vec::new();
    for nr in order {
        let c = calls.remove(&nr).expect("a call");
        entries.push(match c.uncond {
            Some(ret) => Entry { nr, tree: Vec::new(), ret, rank: c.rank },
            None => {
                let tree = call::compile(&c.rules, &mut budget, &mut pool).map_err(|e| match e {
                    call::Fail::TooLarge => too_large(None),
                    call::Fail::Conflict => format!("the seccomp profile has rules for one call that libseccomp cannot hold together (the rules conflict, which Docker refuses too, or its rule tree never finishes): call number {nr}"),
                })?;
                Entry { nr, tree, ret: 0, rank: c.rank }
            }
        });
    }

    let insns = emit(arch, &entries, default)?;
    if insns.len() > MAX_INSNS {
        return Err(too_large(Some(insns.len())));
    }
    let mut out = Vec::with_capacity(insns.len() * 8);
    for i in insns {
        out.extend_from_slice(&i.code.to_le_bytes());
        out.push(i.jt);
        out.push(i.jf);
        out.extend_from_slice(&i.k.to_le_bytes());
    }
    Ok(out)
}

const TREE_STEPS: u64 = 2_000_000;

fn too_large(n: Option<usize>) -> String {
    match n {
        Some(n) => format!("the seccomp profile is too large: it compiles to {n} BPF instructions, the kernel takes {MAX_INSNS}"),
        None => format!("the seccomp profile is too large: it compiles to more than the {MAX_INSNS} BPF instructions the kernel takes"),
    }
}

fn emit(arch: Arch, entries: &[Entry], default: u32) -> Result<Vec<Insn>, String> {
    // The native architecture only, and on x86-64 no x32 numbers, which
    // would slip past every rule.
    let mut p = vec![stmt(LD_W_ABS, OFF_ARCH), jump(JEQ_K, arch.audit(), 1, 0), stmt(RET_K, RET_ERRNO | libc::EPERM as u32), stmt(LD_W_ABS, OFF_NR)];
    if arch == Arch::X86_64 {
        p.push(jump(JGE_K, 0x4000_0000, 0, 1));
        p.push(stmt(RET_K, RET_ERRNO | libc::EPERM as u32));
    }
    let mut i = 0;
    while i < entries.len() {
        if !entries[i].tree.is_empty() {
            conditional(&mut p, &entries[i])?;
            i += 1;
            continue;
        }
        // A run of calls with no condition and one answer: tested in any
        // order, so the hot ones first, each one compare.
        let mut j = i;
        while j < entries.len() && j - i < RUN && entries[j].tree.is_empty() && entries[j].ret == entries[i].ret {
            j += 1;
        }
        let mut run: Vec<&Entry> = entries[i..j].iter().collect();
        run.sort_by_key(|e| (e.rank, e.nr));
        let n = run.len();
        for (k, e) in run.iter().enumerate() {
            p.push(if k + 1 < n { jump(JEQ_K, e.nr, (n - 1 - k) as u8, 0) } else { jump(JEQ_K, e.nr, 0, 1) });
        }
        p.push(stmt(RET_K, entries[i].ret));
        i = j;
    }
    p.push(stmt(RET_K, default));
    Ok(p)
}

/// One call with argument rules: its tests in libseccomp's order (see
/// `tree`), each a load of one half of an argument, a mask if there is one
/// and a compare, with every jump going forward. A call no test decides
/// reloads the number and goes on, to the default in the end.
fn conditional(p: &mut Vec<Insn>, e: &Entry) -> Result<(), String> {
    use call::Tgt;
    #[derive(Clone, Copy)]
    enum Pre {
        Plain(Insn),
        Jump(u16, u32, Tgt, Tgt),
    }
    // Labels: a test by position, an action by its own return, the call's
    // end (reload the number) and what is after it.
    const AFTER: Tgt = Tgt::Act(u32::MAX);
    let mut pre: Vec<Pre> = vec![Pre::Jump(JEQ_K, e.nr, Tgt::Inst(0), AFTER)];
    let mut at_inst = vec![0usize; e.tree.len()];
    for (i, t) in e.tree.iter().enumerate() {
        at_inst[i] = pre.len();
        pre.push(Pre::Plain(stmt(LD_W_ABS, t.off)));
        if t.mask != u32::MAX {
            pre.push(Pre::Plain(stmt(AND_K, t.mask)));
        }
        pre.push(Pre::Jump(t.code, t.datum, t.t, t.f));
    }
    // The first Jump above targets Inst(0): the first test's load.
    let mut at_act: Vec<(u32, usize)> = Vec::new();
    for t in &e.tree {
        for g in [t.t, t.f] {
            if let Tgt::Act(a) = g
                && !at_act.iter().any(|(x, _)| *x == a)
            {
                at_act.push((a, 0));
            }
        }
    }
    for (a, pos) in &mut at_act {
        *pos = pre.len();
        pre.push(Pre::Plain(stmt(RET_K, *a)));
    }
    let at_end = pre.len();
    pre.push(Pre::Plain(stmt(LD_W_ABS, OFF_NR)));
    let at_after = pre.len();
    let target = |t: Tgt| match t {
        Tgt::Inst(i) => at_inst[i],
        Tgt::End => at_end,
        AFTER => at_after,
        Tgt::Act(a) => at_act.iter().find(|(x, _)| *x == a).expect("an action").1,
    };
    // Jumps that cannot reach (an eight-bit offset) become a compare that
    // skips one unconditional jump and then the two unconditional jumps.
    let mut far = vec![false; pre.len()];
    let back = || "the seccomp profile compiles to a jump that goes backward".to_string();
    let pos = loop {
        let mut pos: Vec<usize> = Vec::with_capacity(pre.len() + 1);
        let mut at = 0;
        for f in &far {
            pos.push(at);
            at += if *f { 3 } else { 1 };
        }
        pos.push(at);
        let mut changed = false;
        for (i, x) in pre.iter().enumerate() {
            if let Pre::Jump(_, _, t, f) = x
                && !far[i]
                && [t, f].iter().any(|g| pos[target(**g)].checked_sub(pos[i] + 1).is_none_or(|d| d > 255))
            {
                far[i] = true;
                changed = true;
            }
        }
        if !changed {
            break pos;
        }
    };
    // The first test must follow the number check directly.
    debug_assert!(e.tree.is_empty() || at_inst[0] == 1);
    for (i, x) in pre.iter().enumerate() {
        match *x {
            Pre::Plain(ins) => p.push(ins),
            Pre::Jump(code, k, t, f) => {
                let (pt, pf) = (pos[target(t)], pos[target(f)]);
                if far[i] {
                    p.push(jump(code, k, 0, 1));
                    p.push(stmt(JA, pt.checked_sub(pos[i] + 2).ok_or_else(back)? as u32));
                    p.push(stmt(JA, pf.checked_sub(pos[i] + 3).ok_or_else(back)? as u32));
                } else {
                    let (dt, df) = (pt.checked_sub(pos[i] + 1).ok_or_else(back)?, pf.checked_sub(pos[i] + 1).ok_or_else(back)?);
                    p.push(jump(code, k, u8::try_from(dt).map_err(|_| back())?, u8::try_from(df).map_err(|_| back())?));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYS_ADMIN: u64 = 1 << 21;
    const NOARGS: [u64; 6] = [0; 6];

    fn insns(bytes: &[u8]) -> Vec<Insn> {
        bytes.as_chunks::<8>().0.iter().map(|c| Insn { code: u16::from_le_bytes([c[0], c[1]]), jt: c[2], jf: c[3], k: u32::from_le_bytes([c[4], c[5], c[6], c[7]]) }).collect()
    }

    /// A classic-BPF interpreter for what this compiler emits: what the
    /// kernel would answer for a call.
    pub(super) fn run(prog: &[u8], arch: Arch, nr: u32, args: [u64; 6]) -> u32 {
        let p = insns(prog);
        let word = |off: u32| -> u32 {
            match off {
                OFF_NR => nr,
                OFF_ARCH => arch.audit(),
                o => {
                    let a = ((o - OFF_ARGS) / 8) as usize;
                    if (o - OFF_ARGS).is_multiple_of(8) { args[a] as u32 } else { (args[a] >> 32) as u32 }
                }
            }
        };
        let (mut pc, mut acc) = (0usize, 0u32);
        for _ in 0..100_000 {
            let i = p[pc];
            pc += 1;
            match i.code {
                LD_W_ABS => acc = word(i.k),
                AND_K => acc &= i.k,
                RET_K => return i.k,
                JA => pc += i.k as usize,
                JEQ_K | JGT_K | JGE_K => {
                    let t = match i.code {
                        JEQ_K => acc == i.k,
                        JGT_K => acc > i.k,
                        _ => acc >= i.k,
                    };
                    pc += if t { i.jt } else { i.jf } as usize;
                }
                other => panic!("opcode {other:#x}"),
            }
        }
        panic!("no end");
    }

    fn eperm() -> u32 {
        RET_ERRNO | libc::EPERM as u32
    }

    fn compiled(json: &str, caps: u64) -> Vec<u8> {
        compile_for(Arch::X86_64, json, caps).unwrap_or_else(|e| panic!("{e}"))
    }

    fn call(prog: &[u8], name: &str, args: [u64; 6]) -> u32 {
        run(prog, Arch::X86_64, Arch::X86_64.number(name).unwrap(), args)
    }

    fn arm_call(prog: &[u8], name: &str, args: [u64; 6]) -> u32 {
        run(prog, Arch::Aarch64, Arch::Aarch64.number(name).unwrap(), args)
    }

    #[test]
    fn name_tables_are_sorted_and_know_the_basics() {
        for t in [&names::X86_64[..], &names::AARCH64[..]] {
            assert!(t.windows(2).all(|w| w[0].0 < w[1].0), "sorted for binary search");
        }
        assert_eq!(Arch::X86_64.number("mkdir"), Some(83));
        assert_eq!(Arch::X86_64.number("write"), Some(1));
        assert_eq!(Arch::Aarch64.number("mkdirat"), Some(34));
        assert_eq!(Arch::Aarch64.number("write"), Some(64));
        assert_eq!(Arch::Aarch64.number("mkdir"), None, "arm64 has only mkdirat");
        assert_eq!(Arch::Aarch64.number("memfd_secret"), Some(447));
        assert_eq!(Arch::Aarch64.number("getrlimit"), Some(163));
        assert_eq!(Arch::X86_64.number("pidfd_getfd"), Some(438));
        assert_eq!(Arch::X86_64.number("no_such_syscall"), None);
    }

    #[test]
    fn deny_list_over_allow_default() {
        let j = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir","mkdirat"],"action":"SCMP_ACT_ERRNO"}]}"#;
        let p = compiled(j, 0);
        assert_eq!(call(&p, "mkdir", NOARGS), eperm());
        assert_eq!(call(&p, "mkdirat", NOARGS), eperm());
        assert_eq!(call(&p, "write", NOARGS), RET_ALLOW);
        // The same profile on arm64, where only mkdirat exists.
        let a = compile_for(Arch::Aarch64, j, 0).unwrap();
        assert_eq!(arm_call(&a, "mkdirat", NOARGS), eperm());
        assert_eq!(arm_call(&a, "write", NOARGS), RET_ALLOW);
    }

    #[test]
    fn allow_list_under_error_default() {
        let p = compiled(r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":1,"syscalls":[{"names":["read","write"],"action":"SCMP_ACT_ALLOW"}]}"#, 0);
        assert_eq!(call(&p, "read", NOARGS), RET_ALLOW);
        assert_eq!(call(&p, "write", NOARGS), RET_ALLOW);
        assert_eq!(call(&p, "mkdir", NOARGS), eperm());
        let p = compiled(r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":13,"syscalls":[]}"#, 0);
        assert_eq!(call(&p, "read", NOARGS), RET_ERRNO | 13);
    }

    #[test]
    fn every_action_and_errno() {
        let one = |act: &str, extra: &str| {
            let j = format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["mkdir"],"action":"{act}"{extra}}}]}}"#);
            call(&compiled(&j, 0), "mkdir", NOARGS)
        };
        assert_eq!(one("SCMP_ACT_ALLOW", ""), RET_ALLOW);
        assert_eq!(one("SCMP_ACT_ERRNO", ""), RET_ERRNO | 1);
        assert_eq!(one("SCMP_ACT_ERRNO", r#","errnoRet":38"#), RET_ERRNO | 38);
        assert_eq!(one("SCMP_ACT_ERRNO", r#","errnoRet":99999"#), RET_ERRNO | 4095, "clamped to what the kernel reports");
        assert_eq!(one("SCMP_ACT_KILL", ""), 0);
        assert_eq!(one("SCMP_ACT_KILL_THREAD", ""), 0);
        assert_eq!(one("SCMP_ACT_KILL_PROCESS", ""), 0x8000_0000);
        assert_eq!(one("SCMP_ACT_TRAP", ""), 0x0003_0000);
        assert_eq!(one("SCMP_ACT_LOG", ""), 0x7ffc_0000);
        assert_eq!(one("SCMP_ACT_TRACE", ""), 0x7ff0_0000, "with no tracer the kernel answers ENOSYS");
        assert_eq!(one("SCMP_ACT_TRACE", r#","errnoRet":7"#), 0x7ff0_0007);
        // The default action too.
        let p = compiled(r#"{"defaultAction":"SCMP_ACT_KILL_PROCESS"}"#, 0);
        assert_eq!(call(&p, "read", NOARGS), 0x8000_0000);
    }

    #[test]
    fn the_old_single_name_form_works() {
        let p = compiled(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"name":"mkdir","action":"SCMP_ACT_ERRNO"}]}"#, 0);
        assert_eq!(call(&p, "mkdir", NOARGS), eperm());
    }

    #[test]
    fn first_matching_rule_decides() {
        let p = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
                {"names":["mkdir"],"action":"SCMP_ACT_ERRNO","errnoRet":13},
                {"names":["mkdir","rmdir"],"action":"SCMP_ACT_ERRNO","errnoRet":2}]}"#,
            0,
        );
        assert_eq!(call(&p, "mkdir", NOARGS), RET_ERRNO | 13);
        assert_eq!(call(&p, "rmdir", NOARGS), RET_ERRNO | 2);
    }

    fn arg_profile(op: &str, value: u64, two: u64) -> Vec<u8> {
        compiled(
            &format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[{{"index":0,"value":{value},"valueTwo":{two},"op":"{op}"}}]}}]}}"#),
            0,
        )
    }

    fn denied(p: &[u8], a0: u64) -> bool {
        call(p, "socket", [a0, 0, 0, 0, 0, 0]) == eperm()
    }

    #[test]
    fn argument_comparisons_with_small_values() {
        let p = arg_profile("SCMP_CMP_EQ", 10, 0);
        assert!(denied(&p, 10) && !denied(&p, 9) && !denied(&p, 11));
        let p = arg_profile("SCMP_CMP_NE", 10, 0);
        assert!(!denied(&p, 10) && denied(&p, 9) && denied(&p, 11));
        let p = arg_profile("SCMP_CMP_LT", 10, 0);
        assert!(denied(&p, 9) && !denied(&p, 10) && !denied(&p, 11));
        let p = arg_profile("SCMP_CMP_LE", 10, 0);
        assert!(denied(&p, 9) && denied(&p, 10) && !denied(&p, 11));
        let p = arg_profile("SCMP_CMP_GT", 10, 0);
        assert!(!denied(&p, 9) && !denied(&p, 10) && denied(&p, 11));
        let p = arg_profile("SCMP_CMP_GE", 10, 0);
        assert!(!denied(&p, 9) && denied(&p, 10) && denied(&p, 11));
    }

    #[test]
    fn argument_comparisons_use_all_64_bits() {
        // High half 2, low half with its top bit set.
        let big = 0x0000_0002_8000_0000u64;
        let p = arg_profile("SCMP_CMP_EQ", big, 0);
        assert!(denied(&p, big));
        assert!(!denied(&p, big & 0xffff_ffff), "the low half alone is not it");
        assert!(!denied(&p, 0x0000_0003_8000_0000), "nor a different high half");
        assert!(!denied(&p, 0x0000_0002_8000_0001));

        let p = arg_profile("SCMP_CMP_NE", big, 0);
        assert!(!denied(&p, big));
        assert!(denied(&p, big + 1) && denied(&p, big | (1 << 40)) && denied(&p, 0));

        // GT/GE: the high half dominates even when the low half is smaller.
        let p = arg_profile("SCMP_CMP_GT", big, 0);
        assert!(denied(&p, 0x0000_0003_0000_0000), "greater high half, tiny low half");
        assert!(denied(&p, big + 1));
        assert!(!denied(&p, big));
        assert!(!denied(&p, 0x0000_0001_ffff_ffff), "smaller high half, huge low half");
        assert!(!denied(&p, 0xffff_ffff), "a 32-bit view would call this greater");
        let p = arg_profile("SCMP_CMP_GE", big, 0);
        assert!(denied(&p, big) && denied(&p, 0x0000_0003_0000_0000) && !denied(&p, big - 1));

        // LT/LE the same the other way.
        let p = arg_profile("SCMP_CMP_LT", big, 0);
        assert!(denied(&p, 0x0000_0001_ffff_ffff) && denied(&p, big - 1));
        assert!(!denied(&p, big) && !denied(&p, 0x0000_0003_0000_0000));
        assert!(!denied(&p, 1 << 63), "a set top bit is large, not negative");
        let p = arg_profile("SCMP_CMP_LE", big, 0);
        assert!(denied(&p, big) && denied(&p, 0) && denied(&p, 0x0000_0001_ffff_ffff));
        assert!(!denied(&p, big + 1) && !denied(&p, 0x0000_0003_0000_0000));

        // A value that fits 32 bits against an argument with high bits set.
        let p = arg_profile("SCMP_CMP_GT", 5, 0);
        assert!(denied(&p, 1 << 32), "any high bit exceeds 5");
        let p = arg_profile("SCMP_CMP_EQ", u64::MAX, 0);
        assert!(denied(&p, u64::MAX) && !denied(&p, u64::MAX - 1) && !denied(&p, u32::MAX as u64));
    }

    #[test]
    fn masked_equality_uses_the_mask_and_the_second_value() {
        // (arg & 0xF000_0000_0000_00F0) == 0x1000_0000_0000_0010
        let p = arg_profile("SCMP_CMP_MASKED_EQ", 0xF000_0000_0000_00F0, 0x1000_0000_0000_0010);
        assert!(denied(&p, 0x1000_0000_0000_0010));
        assert!(denied(&p, 0x1FFF_FFFF_FFFF_FF1F), "bits outside the mask are free");
        assert!(!denied(&p, 0x2000_0000_0000_0010), "high half differs inside the mask");
        assert!(!denied(&p, 0x1000_0000_0000_0020), "low half differs inside the mask");
        // Docker's use: the namespace flags clear, no second value.
        let p = arg_profile("SCMP_CMP_MASKED_EQ", 2114060288, 0);
        assert!(denied(&p, 0x0100_0011), "no namespace flags");
        assert!(!denied(&p, 0x1000_0000), "CLONE_NEWUSER");
    }

    #[test]
    fn several_conditions_are_anded_and_other_calls_pass() {
        let j = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO",
            "args":[{"index":0,"value":10,"op":"SCMP_CMP_EQ"},{"index":1,"value":2,"op":"SCMP_CMP_EQ"},{"index":2,"value":4294967296,"op":"SCMP_CMP_GE"}]}]}"#;
        let p = compiled(j, 0);
        let s = |a: [u64; 3]| call(&p, "socket", [a[0], a[1], a[2], 0, 0, 0]);
        assert_eq!(s([10, 2, 1 << 32]), eperm());
        assert_eq!(s([10, 2, 5 << 32]), eperm());
        assert_eq!(s([11, 2, 1 << 32]), RET_ALLOW);
        assert_eq!(s([10, 3, 1 << 32]), RET_ALLOW);
        assert_eq!(s([10, 2, 7]), RET_ALLOW);
        assert_eq!(call(&p, "write", NOARGS), RET_ALLOW);
        // After a failed condition the next rule still sees the number.
        let two = compiled(
            r#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[
                {"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":2,"op":"SCMP_CMP_EQ"}]},
                {"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":10,"op":"SCMP_CMP_EQ"}]},
                {"names":["write"],"action":"SCMP_ACT_ALLOW"}]}"#,
            0,
        );
        assert_eq!(call(&two, "socket", [2, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(call(&two, "socket", [10, 0, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(call(&two, "socket", [3, 0, 0, 0, 0, 0]), eperm());
        assert_eq!(call(&two, "write", NOARGS), RET_ALLOW);
    }

    #[test]
    fn higher_argument_indexes_are_read() {
        let j = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mmap"],"action":"SCMP_ACT_ERRNO","args":[{"index":5,"value":4096,"op":"SCMP_CMP_EQ"}]}]}"#;
        let p = compiled(j, 0);
        assert_eq!(call(&p, "mmap", [0, 0, 0, 0, 0, 4096]), eperm());
        assert_eq!(call(&p, "mmap", [4096, 0, 0, 0, 0, 0]), RET_ALLOW);
    }

    #[test]
    fn capabilities_decide_includes_and_excludes() {
        let excl = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir"],"action":"SCMP_ACT_ERRNO","excludes":{"caps":["CAP_SYS_ADMIN"]}}]}"#;
        assert_eq!(call(&compiled(excl, 0), "mkdir", NOARGS), eperm());
        assert_eq!(call(&compiled(excl, SYS_ADMIN), "mkdir", NOARGS), RET_ALLOW);
        let incl = r#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{"names":["mkdir"],"action":"SCMP_ACT_ALLOW","includes":{"caps":["CAP_SYS_ADMIN"]}}]}"#;
        assert_eq!(call(&compiled(incl, 0), "mkdir", NOARGS), eperm());
        assert_eq!(call(&compiled(incl, SYS_ADMIN), "mkdir", NOARGS), RET_ALLOW);
        // Every included capability is needed; any excluded one is enough.
        let both = r#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{"names":["mkdir"],"action":"SCMP_ACT_ALLOW","includes":{"caps":["CAP_SYS_ADMIN","CAP_SYS_PTRACE"]}}]}"#;
        assert_eq!(call(&compiled(both, SYS_ADMIN), "mkdir", NOARGS), eperm());
        assert_eq!(call(&compiled(both, SYS_ADMIN | 1 << 19), "mkdir", NOARGS), RET_ALLOW);
        let any = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir"],"action":"SCMP_ACT_ERRNO","excludes":{"caps":["CAP_SYS_ADMIN","CAP_SYS_PTRACE"]}}]}"#;
        assert_eq!(call(&compiled(any, 1 << 19), "mkdir", NOARGS), RET_ALLOW);
        // A capability name this kernel lacks is never held.
        let unknown = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir"],"action":"SCMP_ACT_ERRNO","excludes":{"caps":["CAP_NO_SUCH"]}}]}"#;
        assert_eq!(call(&compiled(unknown, u64::MAX), "mkdir", NOARGS), eperm());
    }

    #[test]
    fn architectures_and_kernel_versions_decide_includes_and_excludes() {
        let rule = |extra: &str| format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["mkdir"],"action":"SCMP_ACT_ERRNO",{extra}}}]}}"#);
        let x = |extra: &str| call(&compiled(&rule(extra), 0), "mkdir", NOARGS);
        assert_eq!(x(r#""includes":{"arches":["amd64","x32"]}"#), eperm());
        assert_eq!(x(r#""includes":{"arches":["arm64"]}"#), RET_ALLOW);
        assert_eq!(x(r#""excludes":{"arches":["amd64"]}"#), RET_ALLOW);
        assert_eq!(x(r#""excludes":{"arches":["s390x"]}"#), eperm());
        assert_eq!(x(r#""includes":{"minKernel":"4.8"}"#), eperm());
        assert_eq!(x(r#""includes":{"minKernel":"6.18"}"#), eperm());
        assert_eq!(x(r#""includes":{"minKernel":"6.19"}"#), RET_ALLOW, "newer than the guest");
        assert_eq!(x(r#""includes":{"minKernel":"7"}"#), RET_ALLOW);
        assert_eq!(x(r#""excludes":{"minKernel":"6.0"}"#), RET_ALLOW, "excluded from kernels that new");
        assert_eq!(x(r#""excludes":{"minKernel":"6.19"}"#), eperm());
        // The other architecture's names.
        let arm = compile_for(Arch::Aarch64, &rule(r#""includes":{"arches":["arm64"]}"#), 0).unwrap();
        assert_eq!(arm_call(&arm, "write", NOARGS), RET_ALLOW);
        let arm = compile_for(Arch::Aarch64, &rule(r#""includes":{"arches":["amd64"]}"#), 0).unwrap();
        assert_eq!(arm_call(&arm, "mkdirat", NOARGS), RET_ALLOW, "an amd64-only rule is skipped");
    }

    #[test]
    fn the_native_architecture_only_and_no_x32() {
        let p = compiled(r#"{"defaultAction":"SCMP_ACT_ALLOW"}"#, 0);
        assert_eq!(run(&p, Arch::Aarch64, 1, NOARGS), eperm(), "another architecture");
        assert_eq!(run(&p, Arch::X86_64, 0x4000_0001, NOARGS), eperm(), "an x32 number");
        assert_eq!(run(&p, Arch::X86_64, 1, NOARGS), RET_ALLOW);
        let a = compile_for(Arch::Aarch64, r#"{"defaultAction":"SCMP_ACT_ALLOW"}"#, 0).unwrap();
        assert_eq!(run(&a, Arch::X86_64, 1, NOARGS), eperm());
        assert_eq!(run(&a, Arch::Aarch64, 64, NOARGS), RET_ALLOW);
        // archMap and architectures are read and the native architecture used.
        let p = compiled(r#"{"defaultAction":"SCMP_ACT_ALLOW","architectures":["SCMP_ARCH_X86_64"],"archMap":[{"architecture":"SCMP_ARCH_X86_64","subArchitectures":["SCMP_ARCH_X86","SCMP_ARCH_X32"]}]}"#, 0);
        assert_eq!(call(&p, "write", NOARGS), RET_ALLOW);
    }

    #[test]
    fn unknown_names_are_skipped() {
        let p = compiled(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["no_such_syscall_xyz","mkdir"],"action":"SCMP_ACT_ERRNO"}]}"#, 0);
        assert_eq!(call(&p, "mkdir", NOARGS), eperm());
        assert_eq!(call(&p, "write", NOARGS), RET_ALLOW);
        // Only unknown names: nothing but the default.
        let p = compiled(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["no_such_syscall_xyz"],"action":"SCMP_ACT_ERRNO"}]}"#, 0);
        assert_eq!(call(&p, "mkdir", NOARGS), RET_ALLOW);
    }

    #[test]
    fn bad_profiles_are_refused_with_a_reason() {
        let err = |j: &str| compile_for(Arch::X86_64, j, 0).unwrap_err();
        assert!(err("not json").contains("invalid seccomp profile"));
        assert!(err("{}").contains("defaultAction"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_NOPE"}"#).contains("SCMP_ACT_NOPE"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"],"action":"SCMP_ACT_NOPE"}]}"#).contains("SCMP_ACT_NOPE"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"]}]}"#).contains("no action"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_NOTIFY"}"#).contains("SCMP_ACT_NOTIFY"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"],"action":"SCMP_ACT_NOTIFY"}]}"#).contains("SCMP_ACT_NOTIFY"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_ALLOW","listenerPath":"/run/l.sock"}"#).contains("listenerPath"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_ALLOW","flags":["SECCOMP_FILTER_FLAG_LOG"]}"#).contains("SECCOMP_FILTER_FLAG_LOG"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"],"action":"SCMP_ACT_ERRNO","args":[{"index":0,"value":1,"op":"SCMP_CMP_WAT"}]}]}"#).contains("SCMP_CMP_WAT"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"],"action":"SCMP_ACT_ERRNO","args":[{"index":6,"value":1,"op":"SCMP_CMP_EQ"}]}]}"#).contains("index 6"));
        assert!(err(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"],"action":"SCMP_ACT_ERRNO","includes":{"minKernel":"new"}}]}"#).contains("minKernel"));
        // A rule that would be skipped is still checked.
        let skipped = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"],"action":"SCMP_ACT_NOPE","includes":{"arches":["s390x"]}}]}"#;
        assert!(err(skipped).contains("SCMP_ACT_NOPE"));
        // The empty listenerPath and the always-set flag are fine.
        assert!(compile_for(Arch::X86_64, r#"{"defaultAction":"SCMP_ACT_ALLOW","listenerPath":"","flags":["SECCOMP_FILTER_FLAG_SPEC_ALLOW"]}"#, 0).is_ok());
    }

    #[test]
    fn a_program_over_the_kernel_limit_is_refused() {
        // Conditional rules cost several instructions each: far more than 4096.
        let all: Vec<String> = names::X86_64.iter().map(|(n, _)| format!("\"{n}\"")).collect();
        let rules: Vec<String> = (0..6)
            .map(|k| format!(r#"{{"names":[{}],"action":"SCMP_ACT_ERRNO","args":[{{"index":0,"value":{},"op":"SCMP_CMP_GT"}}]}}"#, all.join(","), 1000 + k))
            .collect();
        let j = format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{}]}}"#, rules.join(","));
        let e = compile_for(Arch::X86_64, &j, 0).unwrap_err();
        assert!(e.contains("too large") && e.contains("4096"), "{e}");
        // The same names without conditions fit.
        let j = format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":[{}],"action":"SCMP_ACT_ERRNO"}}]}}"#, all.join(","));
        let p = compile_for(Arch::X86_64, &j, 0).unwrap();
        assert!(p.len() / 8 < 4096);
        assert_eq!(call(&p, "write", NOARGS), eperm());
    }

    #[test]
    fn long_runs_keep_every_jump_in_range() {
        // More than one run's worth of calls with the same answer.
        let all: Vec<String> = names::X86_64.iter().map(|(n, _)| format!("\"{n}\"")).collect();
        let j = format!(r#"{{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{{"names":[{}],"action":"SCMP_ACT_ALLOW"}}]}}"#, all.join(","));
        let p = compiled(&j, 0);
        for (n, nr) in names::X86_64.iter() {
            assert_eq!(run(&p, Arch::X86_64, *nr, NOARGS), RET_ALLOW, "{n}");
        }
        assert_eq!(run(&p, Arch::X86_64, 9999, NOARGS), eperm());
    }

    #[test]
    fn dockers_default_profile_compiles_and_behaves() {
        // Docker's own default profile (moby, Apache-2.0), as the docker CLI would send it.
        let json = include_str!("seccomp_docker_default.json");
        // Docker's default capabilities.
        let caps: u64 = [0u32, 1, 3, 4, 5, 6, 7, 8, 10, 13, 18, 27, 29, 31].iter().fold(0, |m, c| m | 1 << c);
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let p = compile_for(arch, json, caps).unwrap();
            assert!(p.len() / 8 < 700, "{} instructions", p.len() / 8);
            let c = |name: &str, args: [u64; 6]| run(&p, arch, arch.number(name).unwrap(), args);
            assert_eq!(c("read", NOARGS), RET_ALLOW);
            assert_eq!(c("futex", NOARGS), RET_ALLOW);
            assert_eq!(c("openat", NOARGS), RET_ALLOW);
            assert_eq!(c("chroot", NOARGS), RET_ALLOW, "CAP_SYS_CHROOT is held");
            assert_eq!(c("mount", NOARGS), eperm());
            assert_eq!(c("unshare", NOARGS), eperm());
            assert_eq!(c("bpf", NOARGS), eperm());
            assert_eq!(c("clone3", NOARGS), RET_ERRNO | 38, "ENOSYS without CAP_SYS_ADMIN");
            // clone: no namespace flags yes, a user namespace no.
            assert_eq!(c("clone", [0x0100_0011, 0, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(c("clone", [0x1000_0000, 0, 0, 0, 0, 0]), eperm());
            assert_eq!(c("personality", NOARGS), RET_ALLOW);
            assert_eq!(c("personality", [0x4_0000, 0, 0, 0, 0, 0]), eperm());
            assert_eq!(c("socket", [2, 0, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(c("socket", [40, 0, 0, 0, 0, 0]), eperm(), "AF_VSOCK is not on Docker's list");
            // With CAP_SYS_ADMIN the namespace calls open up.
            let p = compile_for(arch, json, caps | SYS_ADMIN).unwrap();
            assert_eq!(run(&p, arch, arch.number("mount").unwrap(), NOARGS), RET_ALLOW);
            assert_eq!(run(&p, arch, arch.number("clone3").unwrap(), NOARGS), RET_ALLOW);
        }
    }

    #[test]
    fn rules_after_an_unconditional_one_for_the_same_call_are_dropped() {
        let p = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
                {"names":["mkdir"],"action":"SCMP_ACT_ERRNO"},
                {"names":["mkdir"],"action":"SCMP_ACT_KILL"}]}"#,
            0,
        );
        assert_eq!(call(&p, "mkdir", NOARGS), eperm());
        // An allow that only restates the default adds nothing.
        let p = compiled(r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read","write"],"action":"SCMP_ACT_ALLOW"}]}"#, 0);
        assert_eq!(p.len() / 8, 4 + 2 + 1, "head, x32 check and the default");
    }

    fn mkdir_nr() -> u32 {
        Arch::X86_64.number("mkdir").unwrap()
    }

    fn sock(p: &[u8], a0: u64) -> u32 {
        call(p, "socket", [a0, 0, 0, 0, 0, 0])
    }

    #[test]
    fn a_rule_equal_to_the_default_does_not_hide_later_rules() {
        // runc skips it; a later rule for the same call still applies.
        let p = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
                {"names":["mkdir"],"action":"SCMP_ACT_ALLOW"},
                {"names":["mkdir"],"action":"SCMP_ACT_ERRNO","errnoRet":13}]}"#,
            0,
        );
        assert_eq!(call(&p, "mkdir", NOARGS), RET_ERRNO | 13);
        // A conditional rule that restates the default is skipped as well.
        let p = compiled(
            r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":1,"syscalls":[
                {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":1,"args":[{"index":0,"value":10,"op":"SCMP_CMP_EQ"}]},
                {"names":["socket"],"action":"SCMP_ACT_ALLOW"}]}"#,
            0,
        );
        assert_eq!(sock(&p, 10), RET_ALLOW);
        assert_eq!(sock(&p, 2), RET_ALLOW);
    }

    #[test]
    fn an_unconditional_rule_beats_conditional_ones_in_either_order() {
        // Docker: default ALLOW, socket ERRNO 13 when arg0 == 10, then socket
        // ERRNO 1 unconditionally: AF_INET6 and AF_INET both get 1.
        let after = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
                {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":13,"args":[{"index":0,"value":10,"op":"SCMP_CMP_EQ"}]},
                {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":1}]}"#,
            0,
        );
        assert_eq!(sock(&after, 10), RET_ERRNO | 1);
        assert_eq!(sock(&after, 2), RET_ERRNO | 1);
        let before = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
                {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":1},
                {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":13,"args":[{"index":0,"value":10,"op":"SCMP_CMP_EQ"}]}]}"#,
            0,
        );
        assert_eq!(sock(&before, 10), RET_ERRNO | 1);
        assert_eq!(sock(&before, 2), RET_ERRNO | 1);
        // The first unconditional rule's action stays.
        let two = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
                {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":13,"args":[{"index":0,"value":10,"op":"SCMP_CMP_EQ"}]},
                {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":1},
                {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":7}]}"#,
            0,
        );
        assert_eq!(sock(&two, 2), RET_ERRNO | 1);
        // An unconditional rule for one call leaves other calls' conditions alone.
        let other = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
                {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":1},
                {"names":["mmap"],"action":"SCMP_ACT_ERRNO","errnoRet":13,"args":[{"index":0,"value":10,"op":"SCMP_CMP_EQ"}]}]}"#,
            0,
        );
        assert_eq!(call(&other, "mmap", [10, 0, 0, 0, 0, 0]), RET_ERRNO | 13);
        assert_eq!(call(&other, "mmap", [11, 0, 0, 0, 0, 0]), RET_ALLOW);
    }

    #[test]
    fn conditions_on_one_argument_are_ored_and_on_different_ones_anded() {
        // runc: two conditions on index 0 become two rules.
        let p = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[
                {"index":0,"value":10,"op":"SCMP_CMP_EQ"},{"index":0,"value":2,"op":"SCMP_CMP_EQ"}]}]}"#,
            0,
        );
        assert_eq!(sock(&p, 10), eperm());
        assert_eq!(sock(&p, 2), eperm());
        assert_eq!(sock(&p, 1), RET_ALLOW);
        // When any index repeats, every condition of the rule is its own
        // rule, the one on another index included.
        let p = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[
                {"index":0,"value":10,"op":"SCMP_CMP_EQ"},{"index":0,"value":2,"op":"SCMP_CMP_EQ"},{"index":1,"value":1,"op":"SCMP_CMP_EQ"}]}]}"#,
            0,
        );
        assert_eq!(call(&p, "socket", [7, 1, 0, 0, 0, 0]), eperm());
        assert_eq!(call(&p, "socket", [10, 5, 0, 0, 0, 0]), eperm());
        assert_eq!(call(&p, "socket", [7, 5, 0, 0, 0, 0]), RET_ALLOW);
        // Different indexes only: ANDed.
        let p = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[
                {"index":0,"value":10,"op":"SCMP_CMP_EQ"},{"index":1,"value":1,"op":"SCMP_CMP_EQ"}]}]}"#,
            0,
        );
        assert_eq!(call(&p, "socket", [10, 1, 0, 0, 0, 0]), eperm());
        assert_eq!(call(&p, "socket", [10, 2, 0, 0, 0, 0]), RET_ALLOW);
        assert_eq!(call(&p, "socket", [9, 1, 0, 0, 0, 0]), RET_ALLOW);
    }

    #[test]
    fn conditional_rules_for_one_call_are_tried_in_order_and_identical_ones_conflict() {
        let rule = |errno: u32, v: u64| format!(r#"{{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":{errno},"args":[{{"index":0,"value":{v},"op":"SCMP_CMP_EQ"}}]}}"#);
        let p = compiled(&format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{},{}]}}"#, rule(13, 10), rule(1, 2)), 0);
        assert_eq!(sock(&p, 10), RET_ERRNO | 13);
        assert_eq!(sock(&p, 2), RET_ERRNO | 1);
        assert_eq!(sock(&p, 3), RET_ALLOW);
        // The same condition with another action: libseccomp's EEXIST.
        let e = compile_for(Arch::X86_64, &format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{},{}]}}"#, rule(13, 10), rule(1, 10)), 0).unwrap_err();
        assert!(e.contains("libseccomp"), "{e}");
        // With the same action it is fine.
        assert!(compile_for(Arch::X86_64, &format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{},{}]}}"#, rule(13, 10), rule(13, 10)), 0).is_ok());
    }

    /// A `socket` rule on one argument; `errno` is read by ERRNO only.
    fn arg_rule(act: &str, errno: u32, index: u32, op: &str, value: u64, two: u64) -> String {
        format!(r#"{{"names":["socket"],"action":"{act}","errnoRet":{errno},"args":[{{"index":{index},"value":{value},"valueTwo":{two},"op":"{op}"}}]}}"#)
    }

    fn profile_of(default: &str, rules: &[String]) -> Vec<u8> {
        compiled(&format!(r#"{{"defaultAction":"{default}","defaultErrnoRet":1,"syscalls":[{}]}}"#, rules.join(",")), 0)
    }

    #[test]
    fn overlapping_conditional_rules_are_decided_in_libseccomps_order() {
        // Checked live on Docker 29.8.2: default ALLOW, socket arg0 == 10
        // gives 13 and socket arg1 & 15 == 2 gives 95, in both profile
        // orders. The higher argument index is tested first.
        let by_domain = arg_rule("SCMP_ACT_ERRNO", 13, 0, "SCMP_CMP_EQ", 10, 0);
        let by_type = arg_rule("SCMP_ACT_ERRNO", 95, 1, "SCMP_CMP_MASKED_EQ", 15, 2);
        for rules in [[by_domain.clone(), by_type.clone()], [by_type.clone(), by_domain.clone()]] {
            let p = profile_of("SCMP_ACT_ALLOW", &rules);
            let (inet6, inet, dgram_cloexec, stream, dgram) = (10, 2, 2 | 0o2000000, 1, 2);
            assert_eq!(call(&p, "socket", [inet6, dgram_cloexec, 0, 0, 0, 0]), RET_ERRNO | 95);
            assert_eq!(call(&p, "socket", [inet6, stream, 0, 0, 0, 0]), RET_ERRNO | 13);
            assert_eq!(call(&p, "socket", [inet, dgram, 0, 0, 0, 0]), RET_ERRNO | 95);
            assert_eq!(call(&p, "socket", [inet, stream, 0, 0, 0, 0]), RET_ALLOW);
        }
    }

    #[test]
    fn a_kill_on_the_higher_index_beats_an_allow_on_the_lower() {
        let allow = arg_rule("SCMP_ACT_ALLOW", 0, 0, "SCMP_CMP_EQ", 10, 0);
        let kill = arg_rule("SCMP_ACT_KILL_PROCESS", 0, 1, "SCMP_CMP_EQ", 2, 0);
        for rules in [[allow.clone(), kill.clone()], [kill.clone(), allow.clone()]] {
            let p = profile_of("SCMP_ACT_ERRNO", &rules);
            assert_eq!(call(&p, "socket", [10, 2, 0, 0, 0, 0]), RET_KILL_PROCESS);
            assert_eq!(call(&p, "socket", [10, 3, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(call(&p, "socket", [11, 2, 0, 0, 0, 0]), RET_KILL_PROCESS);
            assert_eq!(call(&p, "socket", [11, 3, 0, 0, 0, 0]), RET_ERRNO | 1);
        }
    }

    #[test]
    fn one_index_with_different_operators_and_values() {
        // EQ before GE before nothing, whatever the profile order.
        let eq = arg_rule("SCMP_ACT_ERRNO", 13, 0, "SCMP_CMP_EQ", 10, 0);
        let ge = arg_rule("SCMP_ACT_ERRNO", 95, 0, "SCMP_CMP_GE", 5, 0);
        for rules in [[eq.clone(), ge.clone()], [ge.clone(), eq.clone()]] {
            let p = profile_of("SCMP_ACT_ALLOW", &rules);
            assert_eq!(call(&p, "socket", [10, 0, 0, 0, 0, 0]), RET_ERRNO | 13);
            assert_eq!(call(&p, "socket", [7, 0, 0, 0, 0, 0]), RET_ERRNO | 95);
            assert_eq!(call(&p, "socket", [2, 0, 0, 0, 0, 0]), RET_ALLOW);
        }
        // Equality with another value, and a less-than: equality is tried
        // before less-than before greater-than.
        let eq3 = arg_rule("SCMP_ACT_ERRNO", 2, 0, "SCMP_CMP_EQ", 3, 0);
        let lt8 = arg_rule("SCMP_ACT_ERRNO", 1, 0, "SCMP_CMP_LT", 8, 0);
        let gt5 = arg_rule("SCMP_ACT_ERRNO", 7, 0, "SCMP_CMP_GT", 5, 0);
        for rules in [[eq3.clone(), lt8.clone(), gt5.clone()], [gt5.clone(), lt8.clone(), eq3.clone()], [lt8.clone(), gt5.clone(), eq3.clone()]] {
            let p = profile_of("SCMP_ACT_ALLOW", &rules);
            assert_eq!(call(&p, "socket", [3, 0, 0, 0, 0, 0]), RET_ERRNO | 2);
            assert_eq!(call(&p, "socket", [2, 0, 0, 0, 0, 0]), RET_ERRNO | 1);
            assert_eq!(call(&p, "socket", [6, 0, 0, 0, 0, 0]), RET_ERRNO | 1, "LT 8 is tried before GT 5");
            assert_eq!(call(&p, "socket", [9, 0, 0, 0, 0, 0]), RET_ERRNO | 7);
        }
    }

    #[test]
    fn three_overlapping_rules_are_tried_from_the_highest_index() {
        let a0 = arg_rule("SCMP_ACT_ERRNO", 13, 0, "SCMP_CMP_EQ", 10, 0);
        let a1 = arg_rule("SCMP_ACT_ERRNO", 95, 1, "SCMP_CMP_EQ", 2, 0);
        let a2 = arg_rule("SCMP_ACT_ERRNO", 1, 2, "SCMP_CMP_EQ", 7, 0);
        let orders = [[&a0, &a1, &a2], [&a2, &a1, &a0], [&a1, &a0, &a2], [&a1, &a2, &a0]];
        for o in orders {
            let rules: Vec<String> = o.iter().map(|r| (*r).clone()).collect();
            let p = profile_of("SCMP_ACT_ALLOW", &rules);
            assert_eq!(call(&p, "socket", [10, 2, 7, 0, 0, 0]), RET_ERRNO | 1);
            assert_eq!(call(&p, "socket", [10, 2, 0, 0, 0, 0]), RET_ERRNO | 95);
            assert_eq!(call(&p, "socket", [10, 0, 0, 0, 0, 0]), RET_ERRNO | 13);
            assert_eq!(call(&p, "socket", [0, 0, 7, 0, 0, 0]), RET_ERRNO | 1);
            assert_eq!(call(&p, "socket", [0, 0, 0, 0, 0, 0]), RET_ALLOW);
        }
    }

    #[test]
    fn a_rule_with_more_than_six_conditions_is_split_and_a_skipped_one_is_not_read() {
        // Seven conditions on index 0 and 1: runc makes each its own rule.
        let args: Vec<String> = (0..7).map(|i| format!(r#"{{"index":{},"value":{},"op":"SCMP_CMP_EQ"}}"#, i % 2, 10 + i)).collect();
        let j = format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[{}]}}]}}"#, args.join(","));
        let p = compiled(&j, 0);
        assert_eq!(call(&p, "socket", [10, 0, 0, 0, 0, 0]), eperm());
        assert_eq!(call(&p, "socket", [0, 11, 0, 0, 0, 0]), eperm());
        assert_eq!(call(&p, "socket", [0, 0, 0, 0, 0, 0]), RET_ALLOW);
        // A rule that restates the default is never read beyond its shape.
        let j = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":9,"value":1,"op":"SCMP_CMP_NOPE"}]}]}"#;
        assert!(compile_for(Arch::X86_64, j, 0).is_ok());
        let j = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[{"index":9,"value":1,"op":"SCMP_CMP_EQ"}]}]}"#;
        assert!(compile_for(Arch::X86_64, j, 0).is_err());
    }

    #[test]
    fn minimum_kernel_is_the_one_given() {
        let j = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir"],"action":"SCMP_ACT_ERRNO","includes":{"minKernel":"6.8"}}]}"#;
        let on = |k: (u32, u32)| run(&compile_for_kernel(Arch::X86_64, j, 0, k).unwrap(), Arch::X86_64, mkdir_nr(), NOARGS);
        assert_eq!(on((6, 6)), RET_ALLOW, "WSL's 6.6 is older than 6.8");
        assert_eq!(on((6, 8)), eperm());
        assert_eq!(on((6, 18)), eperm());
        assert_eq!(on((5, 15)), RET_ALLOW);
        let ex = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir"],"action":"SCMP_ACT_ERRNO","excludes":{"minKernel":"6.8"}}]}"#;
        assert_eq!(run(&compile_for_kernel(Arch::X86_64, ex, 0, (6, 6)).unwrap(), Arch::X86_64, mkdir_nr(), NOARGS), eperm());
        assert_eq!(run(&compile_for_kernel(Arch::X86_64, ex, 0, (6, 8)).unwrap(), Arch::X86_64, mkdir_nr(), NOARGS), RET_ALLOW);
    }

    #[test]
    fn kernel_releases_are_read() {
        assert_eq!(release_version("6.8.0-31-generic"), Some((6, 8)));
        assert_eq!(release_version("6.6.87.2-microsoft-standard-WSL2\n"), Some((6, 6)));
        assert_eq!(release_version("6.18-kiri"), Some((6, 18)));
        assert_eq!(release_version("5.15.0"), Some((5, 15)));
        assert_eq!(release_version("6"), None);
        assert_eq!(release_version("kiri"), None);
        assert_eq!(release_version(""), None);
        let (major, _) = guest_kernel().unwrap();
        assert!(major >= 5, "this machine's own kernel (or the VM's) reads as a version");
    }

    #[test]
    fn the_kernel_falls_back_to_uname_and_never_to_a_guess() {
        let no = || -> Option<String> { panic!("uname is not asked when /proc gave a version") };
        assert_eq!(host_kernel(Some("6.6.87.2-microsoft-standard-WSL2".into()), no), Some((6, 6)));
        assert_eq!(host_kernel(Some("6.18".into()), no), Some((6, 18)));
        assert_eq!(host_kernel(Some("7.0-rc1".into()), no), Some((7, 0)));
        // Unreadable or no version in /proc: uname's.
        assert_eq!(host_kernel(None, || Some("6.8.0-31-generic".into())), Some((6, 8)));
        assert_eq!(host_kernel(Some("garbage".into()), || Some("5.15.0".into())), Some((5, 15)));
        assert_eq!(host_kernel(Some(String::new()), || Some("6.6.87.2-microsoft-standard-WSL2\n".into())), Some((6, 6)));
        // Neither: nothing, not the VM's 6.18.
        assert_eq!(host_kernel(Some("garbage".into()), || Some("garbage".into())), None);
        assert_eq!(host_kernel(None, || None), None);
        assert_eq!(release_version("7.0-rc1"), Some((7, 0)));
        assert_eq!(release_version("garbage"), None);
        assert_eq!(release_version("6.18"), Some((6, 18)));
    }

    #[test]
    fn a_kernel_that_cannot_be_read_fails_only_a_profile_that_needs_it() {
        let broken = || -> Result<(u32, u32), String> { Err("no kernel".into()) };
        let plain = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir"],"action":"SCMP_ACT_ERRNO"}]}"#;
        assert!(compile_with(Arch::X86_64, plain, 0, &broken).is_ok());
        let needs = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir"],"action":"SCMP_ACT_ERRNO","includes":{"minKernel":"6.8"}}]}"#;
        assert_eq!(compile_with(Arch::X86_64, needs, 0, &broken).unwrap_err(), "no kernel");
    }

    /// Differential tooling: compiles every `$DIFF_DIR/profs/*.json` and
    /// writes what the filter answers for `socket` over a grid of
    /// arguments to `$DIFF_DIR/mine/`, or the refusal.
    #[test]
    #[ignore]
    fn zz_dump() {
        let dir = std::env::var("DIFF_DIR").unwrap();
        let outdir = std::env::var("DIFF_OUT").unwrap_or_else(|_| "mine".into());
        call::RAW.with(|r| r.set(std::env::var("DIFF_RAW").is_ok()));
        let g: [u64; 17] = [0, 1, 2, 3, 4, 5, 8, 9, 10, 11, 15, 16, 1 << 32, (1 << 32) + 3, 1 << 33, (1 << 32) + 1, 0x1f];
        let mut names: Vec<_> = std::fs::read_dir(format!("{dir}/profs")).unwrap().map(|e| e.unwrap().path()).collect();
        names.sort();
        std::fs::create_dir_all(format!("{dir}/{outdir}")).unwrap();
        for f in names {
            let j = std::fs::read_to_string(&f).unwrap();
            let body = match compile_for(Arch::X86_64, &j, 0) {
                Err(e) => format!("REFUSED: {e}"),
                Ok(p) => {
                    let mut v = Vec::new();
                    for a in g {
                        for b in g {
                            for c in g {
                                let r = run(&p, Arch::X86_64, 41, [a, b, c, 0, 0, 0]);
                                v.push(if r == RET_ALLOW { serde_json::json!("A") } else if r & 0xffff_0000 == RET_ERRNO { serde_json::json!(r & 0xffff) } else { serde_json::json!(format!("{r:#x}")) });
                            }
                        }
                    }
                    serde_json::to_string(&v).unwrap()
                }
            };
            let stem = f.file_stem().unwrap().to_str().unwrap().to_owned();
            std::fs::write(format!("{dir}/{outdir}/{stem}.out"), body).unwrap();
            std::fs::write(format!("{dir}/{outdir}/{stem}.fb"), "0").unwrap();
        }
    }

    const P0244: &str = r#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{"names":["socket"],"action":"SCMP_ACT_KILL_PROCESS","args":[{"index":0,"value":10,"op":"SCMP_CMP_LT"},{"index":1,"value":15,"op":"SCMP_CMP_LT"},{"index":2,"value":15,"op":"SCMP_CMP_GE"}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":8,"op":"SCMP_CMP_EQ"},{"index":2,"value":1,"op":"SCMP_CMP_LT"},{"index":1,"value":3,"op":"SCMP_CMP_NE"}]},{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":103,"args":[{"index":2,"value":1,"op":"SCMP_CMP_GT"},{"index":1,"value":1,"op":"SCMP_CMP_EQ"}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":1,"value":5,"op":"SCMP_CMP_GT"},{"index":0,"value":1,"op":"SCMP_CMP_LT"}]}],"defaultErrnoRet":120}"#;
    const P1165: &str = r#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":1,"value":3,"op":"SCMP_CMP_LE"},{"index":0,"value":5,"op":"SCMP_CMP_NE"},{"index":2,"value":0,"op":"SCMP_CMP_GE"}]},{"names":["socket"],"action":"SCMP_ACT_KILL_PROCESS","args":[{"index":0,"value":255,"valueTwo":0,"op":"SCMP_CMP_MASKED_EQ"},{"index":2,"value":5,"op":"SCMP_CMP_NE"}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":15,"op":"SCMP_CMP_EQ"},{"index":1,"value":0,"op":"SCMP_CMP_GE"}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":1,"value":4294967296,"op":"SCMP_CMP_LT"},{"index":2,"value":4294967297,"valueTwo":1,"op":"SCMP_CMP_MASKED_EQ"},{"index":0,"value":3,"op":"SCMP_CMP_NE"}]}],"defaultErrnoRet":120}"#;
    const P0002: &str = r#"{"defaultAction":"SCMP_ACT_KILL_PROCESS","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":101,"args":[{"index":1,"value":5,"op":"SCMP_CMP_LT"},{"index":2,"value":2,"op":"SCMP_CMP_NE"},{"index":0,"value":3,"op":"SCMP_CMP_GE"}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":1,"value":4294967299,"op":"SCMP_CMP_LT"},{"index":2,"value":1,"op":"SCMP_CMP_EQ"},{"index":0,"value":10,"op":"SCMP_CMP_GT"}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":1,"value":8589934592,"op":"SCMP_CMP_EQ"},{"index":2,"value":4294967296,"op":"SCMP_CMP_EQ"}]}]}"#;
    const P111: &str = r#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":101,"args":[{"index":1,"value":1,"valueTwo":1,"op":"SCMP_CMP_MASKED_EQ"}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":3,"op":"SCMP_CMP_LT"},{"index":1,"value":4294967299,"op":"SCMP_CMP_LE"}]},{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":103,"args":[{"index":0,"value":15,"op":"SCMP_CMP_LT"},{"index":1,"value":5,"op":"SCMP_CMP_LT"}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":2,"value":15,"op":"SCMP_CMP_LT"},{"index":1,"value":15,"op":"SCMP_CMP_GT"}]}],"defaultErrnoRet":120}"#;
    const B: &str = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[{"index":1,"value":5,"op":"SCMP_CMP_LT"},{"index":2,"value":2,"op":"SCMP_CMP_NE"},{"index":0,"value":3,"op":"SCMP_CMP_GE"}],"errnoRet":101},{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[{"index":1,"value":4294967299,"op":"SCMP_CMP_LT"},{"index":2,"value":1,"op":"SCMP_CMP_EQ"},{"index":0,"value":10,"op":"SCMP_CMP_GT"}],"errnoRet":7},{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[{"index":1,"value":8589934592,"op":"SCMP_CMP_EQ"},{"index":2,"value":4294967296,"op":"SCMP_CMP_EQ"}],"errnoRet":7}]}"#;

    // ---- libseccomp's tree against the rules read literally ----

    const GRID: [u64; 9] = [0, 1, 2, 3, 5, 8, 10, 1 << 32, (1 << 32) + 3];

    /// The profile's own reading of a `socket` call: the actions of every
    /// rule that holds, or the default when none does.
    fn literal_answers(json: &str, args: [u64; 3]) -> Vec<u32> {
        let v: Value = serde_json::from_str(json).unwrap();
        let act = |r: &Value, d: bool| {
            let (a, e) = if d { ("defaultAction", "defaultErrnoRet") } else { ("action", "errnoRet") };
            action(r[a].as_str().unwrap(), r[e].as_u64().map(|x| x as u32)).unwrap()
        };
        let default = act(&v, true);
        let holds = |c: &Value, a: [u64; 3]| {
            let x = c["value"].as_u64().unwrap_or(0);
            let v = a[c["index"].as_u64().unwrap() as usize];
            match c["op"].as_str().unwrap() {
                "SCMP_CMP_EQ" => v == x,
                "SCMP_CMP_NE" => v != x,
                "SCMP_CMP_LT" => v < x,
                "SCMP_CMP_LE" => v <= x,
                "SCMP_CMP_GT" => v > x,
                "SCMP_CMP_GE" => v >= x,
                _ => v & x == c["valueTwo"].as_u64().unwrap_or(0) & x,
            }
        };
        let mut out = Vec::new();
        for r in v["syscalls"].as_array().unwrap() {
            let ret = act(r, false);
            if ret == default {
                continue;
            }
            let cs = r["args"].as_array().cloned().unwrap_or_default();
            let mut idx: Vec<u64> = cs.iter().map(|c| c["index"].as_u64().unwrap()).collect();
            idx.sort();
            idx.dedup();
            let groups: Vec<Vec<Value>> = if idx.len() < cs.len() { cs.iter().map(|c| vec![c.clone()]).collect() } else { vec![cs] };
            for g in groups {
                if g.iter().all(|c| holds(c, args)) {
                    out.push(ret);
                }
            }
        }
        if out.is_empty() {
            out.push(default);
        }
        out
    }

    fn severity(a: u32) -> u8 {
        match a & 0xffff_0000 {
            0x7fff_0000 => 0,
            0x0005_0000 => 1,
            _ => 2,
        }
    }

    /// Every answer on the grid is the action of a rule that holds there,
    /// or the default when none does. Err: the profile, which libseccomp
    /// refuses too, is refused.
    fn assert_literal(json: &str) {
        let Ok(p) = compile_for(Arch::X86_64, json, 0) else { return };
        for a in GRID {
            for b in GRID {
                for c in GRID {
                    let got = run(&p, Arch::X86_64, 41, [a, b, c, 0, 0, 0]);
                    let ok = literal_answers(json, [a, b, c]);
                    assert!(ok.contains(&got), "{json}\nsocket({a:#x}, {b:#x}, {c:#x}) is {got:#x}, the rules say {ok:x?}");
                }
            }
        }
    }

    fn answer(json: &str, a: [u64; 3]) -> u32 {
        run(&compile_for(Arch::X86_64, json, 0).unwrap(), Arch::X86_64, 41, [a[0], a[1], a[2], 0, 0, 0])
    }


    const P00_S1279: &str = r#"{"defaultAction":"SCMP_ACT_KILL_PROCESS","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":1,"value":17,"op":"SCMP_CMP_GE"},{"index":2,"value":8589934597,"op":"SCMP_CMP_NE"}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":17,"op":"SCMP_CMP_GE"},{"index":2,"value":8,"op":"SCMP_CMP_MASKED_EQ","valueTwo":8}]},{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":102,"args":[{"index":2,"value":8,"op":"SCMP_CMP_MASKED_EQ","valueTwo":8},{"index":1,"value":2,"op":"SCMP_CMP_MASKED_EQ","valueTwo":0}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":1,"value":8,"op":"SCMP_CMP_MASKED_EQ","valueTwo":0},{"index":0,"value":14,"op":"SCMP_CMP_GE"},{"index":2,"value":3,"op":"SCMP_CMP_MASKED_EQ","valueTwo":1}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":2,"op":"SCMP_CMP_MASKED_EQ","valueTwo":2},{"index":1,"value":3,"op":"SCMP_CMP_MASKED_EQ","valueTwo":1}]},{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":102,"args":[{"index":1,"value":17,"op":"SCMP_CMP_LT"},{"index":0,"value":14,"op":"SCMP_CMP_LE"},{"index":2,"value":8589934597,"op":"SCMP_CMP_LE"}]}]}"#;
    const P03_S4450: &str = r#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":1,"value":17,"op":"SCMP_CMP_LT"},{"index":2,"value":6,"op":"SCMP_CMP_LT"},{"index":0,"value":12,"op":"SCMP_CMP_GE"}]},{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":101,"args":[{"index":1,"value":48,"op":"SCMP_CMP_MASKED_EQ","valueTwo":16},{"index":2,"value":7,"op":"SCMP_CMP_LE"},{"index":0,"value":13,"op":"SCMP_CMP_LE"}]},{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":101,"args":[{"index":1,"value":17,"op":"SCMP_CMP_LE"},{"index":2,"value":2,"op":"SCMP_CMP_MASKED_EQ","valueTwo":0},{"index":0,"value":7,"op":"SCMP_CMP_GT"}]}],"defaultErrnoRet":102}"#;
    /// A profile whose libseccomp tree loops back on itself: Docker accepts
    /// it at create and runc hangs at start.
    const P_CYCLE: &str = r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":101,"syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":102,"args":[{"index":2,"op":"SCMP_CMP_MASKED_EQ","value":2,"valueTwo":2},{"index":0,"op":"SCMP_CMP_NE","value":7}]},{"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":1,"op":"SCMP_CMP_MASKED_EQ","value":1,"valueTwo":1},{"index":0,"op":"SCMP_CMP_NE","value":17},{"index":2,"op":"SCMP_CMP_LT","value":17}]},{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":102,"args":[{"index":1,"op":"SCMP_CMP_LE","value":4294967303},{"index":2,"op":"SCMP_CMP_GT","value":4294967303},{"index":0,"op":"SCMP_CMP_MASKED_EQ","value":15,"valueTwo":9}]},{"names":["socket"],"action":"SCMP_ACT_KILL_PROCESS","args":[{"index":1,"op":"SCMP_CMP_LT","value":4294967303},{"index":0,"op":"SCMP_CMP_NE","value":4294967280}]},{"names":["socket"],"action":"SCMP_ACT_KILL_PROCESS","args":[{"index":1,"op":"SCMP_CMP_NE","value":4294967303},{"index":0,"op":"SCMP_CMP_NE","value":4294967280},{"index":2,"op":"SCMP_CMP_GT","value":4294967313}]}]}"#;

    #[test]
    fn the_judges_counterexamples_answer_with_a_rule_that_holds() {
        for j in [P00_S1279, P03_S4450, P_CYCLE, P0244, P1165, P0002, P111, B] {
            assert!(compile_for(Arch::X86_64, j, 0).is_ok(), "{j}");
            assert_literal(j);
        }
        // Default KILL_PROCESS and no rule holds at socket(15, 2, 8): not allowed.
        assert_eq!(answer(P00_S1279, [15, 2, 8]), RET_KILL_PROCESS);
    }

    #[test]
    fn a_profile_whose_libseccomp_tree_loops_compiles_quickly() {
        let t = std::time::Instant::now();
        let p = compiled(P_CYCLE, 0);
        assert!(t.elapsed() < std::time::Duration::from_millis(50), "{:?}", t.elapsed());
        assert!(p.len() < MAX_INSNS * 8);
    }

    /// The looping profile with its rules copied across `n` calls.
    fn cycle_across(n: usize) -> String {
        let mut v: serde_json::Value = serde_json::from_str(P_CYCLE).unwrap();
        let names: Vec<&str> = names::X86_64.iter().map(|(n, _)| *n).filter(|n| *n != "socket").take(n).collect();
        assert_eq!(names.len(), n, "the x86-64 table has fewer names");
        for s in v["syscalls"].as_array_mut().unwrap() {
            s["names"] = serde_json::json!(names);
        }
        v.to_string()
    }

    #[test]
    fn a_looping_profile_spread_over_many_calls_is_quick() {
        // Debug builds are several times slower; the release bound is the aim.
        let limit = if cfg!(debug_assertions) { 1500 } else { 50 };
        for n in [40, 380] {
            let json = cycle_across(n);
            let t = std::time::Instant::now();
            let r = compile_for(Arch::X86_64, &json, 0);
            let took = t.elapsed();
            assert!(took < std::time::Duration::from_millis(limit), "{n} calls took {took:?}");
            if let Err(e) = r {
                assert!(e.contains("too large"), "{e}");
            }
        }
    }

    #[test]
    fn a_backward_jump_in_a_calls_code_sends_it_down_the_rule_by_rule_path() {
        let json = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":1,"args":[{"index":0,"op":"SCMP_CMP_EQ","value":1},{"index":1,"op":"SCMP_CMP_EQ","value":2}]},{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":2,"args":[{"index":0,"op":"SCMP_CMP_EQ","value":3}]}]}"#;
        call::BY_RULE.with(|c| c.set(0));
        let plain = compiled(json, 0);
        assert_eq!(call::BY_RULE.with(|c| c.get()), 0, "the tree path is the usual one");
        call::BACK_EDGE.with(|b| b.set(true));
        let forced = compiled(json, 0);
        call::BACK_EDGE.with(|b| b.set(false));
        assert_eq!(call::BY_RULE.with(|c| c.get()), 1, "the rule-by-rule path was taken");
        for a in [[1, 2, 0], [1, 3, 0], [3, 0, 0], [4, 4, 4]] {
            let args = [a[0], a[1], a[2], 0, 0, 0];
            assert_eq!(call(&plain, "socket", args), call(&forced, "socket", args), "{a:?}");
        }
    }

    /// A `socket` rule on several conditions, (index, op, value, valueTwo).
    fn conds_rule(act: &str, errno: u32, conds: &[(u32, &str, u64, u64)]) -> String {
        let args: Vec<String> = conds.iter().map(|(i, op, v, two)| format!(r#"{{"index":{i},"value":{v},"valueTwo":{two},"op":"{op}"}}"#)).collect();
        format!(r#"{{"names":["socket"],"action":"{act}","errnoRet":{errno},"args":[{}]}}"#, args.join(","))
    }

    type Pair = (&'static str, String, String, [u64; 2], [u64; 2]);

    /// The pairs of rules where one's conditions are the start of the other's,
    /// as `_db_rule_gen_64` builds them: (shorter, longer), a call both rules
    /// hold for, and one only the shorter holds for.
    fn prefix_pairs() -> Vec<Pair> {
        let e = |errno, conds: &[(u32, &str, u64, u64)]| conds_rule("SCMP_ACT_ERRNO", errno, conds);
        vec![
            ("EQ", e(13, &[(0, "SCMP_CMP_EQ", 10, 0)]), e(95, &[(0, "SCMP_CMP_EQ", 10, 0), (1, "SCMP_CMP_EQ", 2, 0)]), [10, 2], [10, 3]),
            ("MASKED_EQ", e(13, &[(0, "SCMP_CMP_EQ", 10, 0)]), e(95, &[(0, "SCMP_CMP_EQ", 10, 0), (1, "SCMP_CMP_MASKED_EQ", 15, 2)]), [10, 2 | 0o2000000], [10, 3]),
            ("GT", e(13, &[(0, "SCMP_CMP_GT", 5, 0)]), e(95, &[(0, "SCMP_CMP_GT", 5, 0), (1, "SCMP_CMP_EQ", 2, 0)]), [7, 2], [7, 3]),
            ("LT", e(13, &[(0, "SCMP_CMP_LT", 12, 0)]), e(95, &[(0, "SCMP_CMP_LT", 12, 0), (1, "SCMP_CMP_EQ", 2, 0)]), [4, 2], [4, 3]),
            ("NE", e(13, &[(0, "SCMP_CMP_NE", 3, 0)]), e(95, &[(0, "SCMP_CMP_NE", 3, 0), (1, "SCMP_CMP_EQ", 2, 0)]), [9, 2], [9, 3]),
            ("64-bit LE", e(13, &[(0, "SCMP_CMP_LE", (1 << 32) + 5, 0)]), e(95, &[(0, "SCMP_CMP_LE", (1 << 32) + 5, 0), (1, "SCMP_CMP_EQ", 2, 0)]), [(1 << 32) + 4, 2], [(1 << 32) + 4, 3]),
        ]
    }

    #[test]
    fn a_rule_that_is_the_start_of_a_later_one_wins_where_both_hold() {
        // libseccomp (src/db.c, `_db_tree_add`, 2.5 and 2.6): the longer rule
        // after the shorter is dropped without a word; checked live on Docker
        // 29.8.2, where socket arg0 == 10 gives 13 and the later rule
        // arg0 == 10 and arg1 & 15 == 2 is never reached.
        for (what, short, long, both, only_short) in prefix_pairs() {
            let p = profile_of("SCMP_ACT_ALLOW", &[short, long]);
            assert_eq!(call(&p, "socket", [both[0], both[1], 0, 0, 0, 0]), RET_ERRNO | 13, "{what}: both hold");
            assert_eq!(call(&p, "socket", [only_short[0], only_short[1], 0, 0, 0, 0]), RET_ERRNO | 13, "{what}: only the shorter holds");
        }
    }

    #[test]
    fn a_rule_that_is_the_start_of_an_earlier_one_is_refused() {
        // The same pair the other way round: libseccomp's EEXIST, which fails
        // the container start in Docker ("error adding seccomp rule for
        // syscall socket: file exists").
        for (what, short, long, ..) in prefix_pairs() {
            let e = compile_for(Arch::X86_64, &format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{long},{short}]}}"#), 0).unwrap_err();
            assert!(e.contains("seccomp profile"), "{what}: {e}");
        }
    }

    #[test]
    fn a_rule_that_is_the_start_of_another_with_the_same_action_is_fine_in_either_order() {
        for (what, short, long, both, only_short) in prefix_pairs() {
            let same = long.replace("\"errnoRet\":95", "\"errnoRet\":13");
            for rules in [[short.clone(), same.clone()], [same.clone(), short.clone()]] {
                let p = profile_of("SCMP_ACT_ALLOW", &rules);
                assert_eq!(call(&p, "socket", [both[0], both[1], 0, 0, 0, 0]), RET_ERRNO | 13, "{what}: both hold");
                assert_eq!(call(&p, "socket", [only_short[0], only_short[1], 0, 0, 0, 0]), RET_ERRNO | 13, "{what}: only the shorter holds");
                assert_eq!(call(&p, "socket", [0, 0, 0, 0, 0, 0]), if what == "LT" || what == "64-bit LE" || what == "NE" { RET_ERRNO | 13 } else { RET_ALLOW }, "{what}: neither");
            }
        }
    }

    #[test]
    fn rules_that_only_share_a_first_node_are_not_a_prefix_pair() {
        // The same argument and datum but another operator, or a longer rule
        // on other arguments, is not the start of anything: both stand.
        let wide = arg_rule("SCMP_ACT_ERRNO", 13, 0, "SCMP_CMP_EQ", 10, 0);
        let other_arg = conds_rule("SCMP_ACT_ERRNO", 95, &[(1, "SCMP_CMP_EQ", 2, 0), (2, "SCMP_CMP_EQ", 3, 0)]);
        for rules in [[wide.clone(), other_arg.clone()], [other_arg.clone(), wide.clone()]] {
            let p = profile_of("SCMP_ACT_ALLOW", &rules);
            assert_eq!(call(&p, "socket", [10, 2, 3, 0, 0, 0]), RET_ERRNO | 95);
            assert_eq!(call(&p, "socket", [10, 1, 3, 0, 0, 0]), RET_ERRNO | 13);
        }
    }

    #[test]
    fn a_call_with_many_rules_compiles_in_linear_time_or_is_refused() {
        let rules: Vec<String> = (0..3000).map(|i| arg_rule("SCMP_ACT_ERRNO", 1 + i % 50, i % 3, ["SCMP_CMP_GT", "SCMP_CMP_NE", "SCMP_CMP_LT"][i as usize % 3], i as u64 * 7, 0)).collect();
        let t = std::time::Instant::now();
        let r = compile_for(Arch::X86_64, &format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{}]}}"#, rules.join(",")), 0);
        assert!(t.elapsed() < std::time::Duration::from_millis(50), "{:?}", t.elapsed());
        assert!(r.unwrap_err().contains("too large"));
    }

    #[test]
    fn libseccomps_corrupted_trees_never_weaken_a_profile() {
        // Cases where libseccomp's tree, and so Docker, answers with the
        // action of a rule that does not hold, or the call goes through
        // where the rules deny it; each is the rules' answer here.
        for (json, at) in [(P0244, [1, 8, 0]), (P1165, [3, 4, 1]), (P0002, [3, 5, 1]), (P111, [3, 8, 0]), (B, [3, 5, 1]), (B, [3, 4, 1])] {
            let got = answer(json, at);
            let ok = literal_answers(json, at);
            assert!(ok.contains(&got), "{at:?}: {got:#x} is not one of {ok:x?}");
            assert!(severity(got) >= ok.iter().map(|a| severity(*a)).min().unwrap());
        }
        // No rule holds at (3,5,1), so the default allows, as Docker does.
        assert_eq!(answer(B, [3, 5, 1]), RET_ALLOW);
        assert_eq!(answer(B, [3, 4, 1]), RET_ERRNO | 101, "the first rule holds there");
        // Whole grids, not only the named points.
        for j in [P0244, P1165, P0002, P111, B] {
            assert_literal(j);
        }
    }

    /// Rules for `socket` from a seed: several-argument rules whose first
    /// conditions come from a few choices, so that different rules share a
    /// lower-index prefix of range, not-equal and masked conditions.
    fn shared_prefix_profile(seed: u64, ops: &[&str], allow_list: bool) -> String {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut rnd = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        let values = [0u64, 1, 2, 3, 5, 8, 10, 1 << 32, (1 << 32) + 3];
        let acts = if allow_list { vec!["ALLOW", "ALLOW", "ERRNO", "KILL_PROCESS"] } else { vec!["ALLOW", "ERRNO", "KILL_PROCESS"] };
        let default = if allow_list { "ERRNO" } else { acts[rnd(acts.len())] };
        let mut rules = Vec::new();
        for r in 0..2 + rnd(4) {
            let act = acts[rnd(acts.len())];
            let n = 2 + rnd(2);
            let args: Vec<String> = (0..n)
                .map(|i| {
                    let op = ops[rnd(ops.len())];
                    // The first condition comes from two choices: rules share it.
                    let v = if i == 0 { values[[3, 5][rnd(2)]] } else { values[rnd(values.len())] };
                    if op == "SCMP_CMP_MASKED_EQ" {
                        format!(r#"{{"index":{i},"value":{},"valueTwo":{},"op":"{op}"}}"#, [1u64, 3, 15][rnd(3)], rnd(2))
                    } else {
                        format!(r#"{{"index":{i},"value":{v},"op":"{op}"}}"#)
                    }
                })
                .collect();
            let errno = if act == "ERRNO" { format!(r#","errnoRet":{}"#, 101 + r) } else { String::new() };
            rules.push(format!(r#"{{"names":["socket"],"action":"SCMP_ACT_{act}"{errno},"args":[{}]}}"#, args.join(",")));
        }
        let derr = if default == "ERRNO" { r#","defaultErrnoRet":120"# } else { "" };
        format!(r#"{{"defaultAction":"SCMP_ACT_{default}"{derr},"syscalls":[{}]}}"#, rules.join(","))
    }

    #[test]
    fn range_rules_from_different_rules_sharing_a_prefix_follow_the_rules() {
        let all = ["SCMP_CMP_EQ", "SCMP_CMP_NE", "SCMP_CMP_LT", "SCMP_CMP_LE", "SCMP_CMP_GT", "SCMP_CMP_GE", "SCMP_CMP_MASKED_EQ"];
        let ranges = ["SCMP_CMP_LT", "SCMP_CMP_LE", "SCMP_CMP_GT", "SCMP_CMP_GE", "SCMP_CMP_NE"];
        let mut compiled = 0;
        for seed in 0..120 {
            for (ops, allow) in [(&all[..], false), (&all[..], true), (&ranges[..], true)] {
                let j = shared_prefix_profile(seed, ops, allow);
                compiled += usize::from(compile_for(Arch::X86_64, &j, 0).is_ok());
                assert_literal(&j);
            }
        }
        assert!(compiled > 240, "most of the profiles are held by libseccomp: {compiled}");
    }

    /// FNV-1a over the program, with its length.
    fn fingerprint(p: &[u8]) -> (usize, u64) {
        (p.len(), p.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3)))
    }

    fn default_profile_fingerprints() -> Vec<(usize, u64)> {
        let json = include_str!("seccomp_docker_default.json");
        let mut out = Vec::new();
        for arch in [Arch::X86_64, Arch::Aarch64] {
            for caps in [0u64, super::super::api::effective_caps(0), SYS_ADMIN | (1 << 5), (1u64 << 41) - 1] {
                out.push(fingerprint(&compile_for(arch, json, caps).unwrap()));
            }
        }
        out
    }

    #[test]
    #[ignore]
    fn zz_print_default_fingerprints() {
        println!("FINGERPRINTS {:?}", default_profile_fingerprints());
    }

    #[test]
    fn dockers_default_profile_compiles_to_the_same_program_as_before_the_check() {
        // Taken from the programs the compiler gave at 02c4a75, before the
        // check against the rules was added; x86-64 then arm64, with no
        // capabilities, Docker's, CAP_SYS_ADMIN and all of them.
        const BEFORE: [(usize, u64); 8] = [(3344, 7119315196984305919), (3360, 10520078845130455648), (3456, 4672869488848160370), (3608, 18302366267810599159), (2984, 13122617528245846209), (3000, 4195708849367642816), (3104, 8218185919013394092), (3240, 5574472550552570181)];
        assert_eq!(default_profile_fingerprints(), BEFORE);
    }

    #[test]
    fn the_security_option_is_read() {
        let o = |s: &[&str]| choice(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>()).map(|c| match c {
            Choice::Default => "default".to_owned(),
            Choice::Unconfined => "unconfined".to_owned(),
            Choice::Profile(j) => format!("profile {j}"),
        });
        assert_eq!(o(&[]), Ok("default".into()));
        assert_eq!(o(&["no-new-privileges"]), Ok("default".into()));
        assert_eq!(o(&["seccomp=builtin"]), Ok("default".into()));
        assert_eq!(o(&["seccomp="]), Ok("default".into()));
        assert_eq!(o(&["seccomp=unconfined"]), Ok("unconfined".into()));
        assert_eq!(o(&["seccomp:unconfined"]), Ok("unconfined".into()));
        assert_eq!(o(&[r#"seccomp={"defaultAction":"SCMP_ACT_ALLOW"}"#]), Ok(r#"profile {"defaultAction":"SCMP_ACT_ALLOW"}"#.into()));
        assert_eq!(o(&["seccomp=unconfined", "seccomp=builtin"]), Ok("default".into()), "the last one wins");
        assert!(o(&["seccomp=/etc/profile.json"]).unwrap_err().contains("/etc/profile.json"));
    }
}

#[cfg(test)]
mod spec_tests {
    use super::super::engine::Spec;
    use super::{Arch, RET_ERRNO, tests::run};

    /// A container spec as the API would keep it, with only what the test sets.
    fn spec(security_opt: &[&str], caps: u64, privileged: bool) -> Spec {
        let mut s: Spec = serde_json::from_value(serde_json::json!({
            "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": [], "env": [], "cwd": "", "user": "", "auto_remove": false,
            "labels": null, "mounts": [], "mount_info": [], "volumes": [], "ports": [], "network_mode": "bridge", "aliases": [], "extra_hosts": [], "links": [],
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        s.security_opt = security_opt.iter().map(|x| x.to_string()).collect();
        s.caps = caps;
        s.privileged = privileged;
        s
    }

    const DENY_MKDIR: &str = r#"seccomp={"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir","mkdirat"],"action":"SCMP_ACT_ERRNO","excludes":{"caps":["CAP_SYS_ADMIN"]}}]}"#;

    #[test]
    fn only_a_profile_makes_a_program() {
        assert!(spec(&[], 0, false).seccomp_program().unwrap().is_empty());
        assert!(spec(&["seccomp=builtin"], 0, false).seccomp_program().unwrap().is_empty());
        assert!(spec(&["seccomp=unconfined"], 0, false).seccomp_program().unwrap().is_empty());
        assert!(!spec(&[DENY_MKDIR], 0, false).seccomp_program().unwrap().is_empty());
    }

    #[test]
    fn the_containers_capabilities_decide_the_program() {
        // Default capabilities (0) lack SYS_ADMIN; adding it drops the rule, which leaves only the head.
        let plain = spec(&[DENY_MKDIR], 0, false).seccomp_program().unwrap();
        let admin = spec(&[DENY_MKDIR], 1 << 21, false).seccomp_program().unwrap();
        assert!(plain.len() > admin.len());
        assert_eq!(super::super::api::effective_caps(0) & (1 << 21), 0);
        assert_ne!(super::super::api::effective_caps(0) & 1, 0, "CHOWN is in the default set");
        assert_eq!(super::super::api::effective_caps(0b1010), 0b1010);
    }

    #[test]
    fn a_bad_profile_fails_even_under_privileged() {
        assert!(spec(&[r#"seccomp={"defaultAction":"SCMP_ACT_NOPE"}"#], 0, true).seccomp_program().is_err());
        assert!(spec(&[r#"seccomp={"defaultAction":"SCMP_ACT_NOPE"}"#], 0, false).seccomp_program().is_err());
        assert!(spec(&["seccomp=/some/path.json"], 0, false).seccomp_program().is_err());
    }

    #[test]
    fn privileged_keeps_an_explicit_profile_and_drops_only_the_default() {
        // moby's WithSeccomp: no profile or unconfined and privileged runs
        // without one; an explicit profile is still loaded.
        assert!(spec(&[], 0, true).seccomp_program().unwrap().is_empty());
        assert!(spec(&["seccomp=unconfined"], 0, true).seccomp_program().unwrap().is_empty());
        assert!(spec(&["seccomp=builtin"], 0, true).seccomp_program().unwrap().is_empty());
        let p = spec(&[DENY_MKDIR], 0, true).seccomp_program().unwrap();
        assert!(!p.is_empty());
        // A privileged container holds every capability, so the rule that
        // excludes CAP_SYS_ADMIN does not apply and only the head is left.
        assert_eq!(p.len(), super::compile(r#"{"defaultAction":"SCMP_ACT_ALLOW"}"#, 0).unwrap().len());
        let denies = spec(&[r#"seccomp={"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir","mkdirat"],"action":"SCMP_ACT_ERRNO"}]}"#], 0, true).seccomp_program().unwrap();
        assert_eq!(run(&denies, Arch::native(), Arch::native().number("mkdir").or(Arch::native().number("mkdirat")).unwrap(), [0; 6]), RET_ERRNO | 1);
    }

    #[test]
    fn unconfined_sets_the_flag_and_a_profile_does_not() {
        use kiri_proto::run_flags::NO_SECCOMP;
        assert_ne!(spec(&["seccomp=unconfined"], 0, false).security_flags() & NO_SECCOMP, 0);
        assert_eq!(spec(&[DENY_MKDIR], 0, false).security_flags() & NO_SECCOMP, 0);
        assert_eq!(spec(&[], 0, false).security_flags() & NO_SECCOMP, 0);
    }
}
