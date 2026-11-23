//! A Linux host's own firewall, which must let Kiri's containers through.
//!
//! Every base chain on the forward hook sees each forwarded packet, and an
//! accept in one is not final: any other that drops it wins. Kiri's own
//! table accepting its containers' traffic is not enough when the host has
//! another forward chain that drops what it does not know:
//!
//! - iptables' FORWARD with policy drop: set by Docker when it turns
//!   ip_forward on, by ufw (its default), or by hand. Kiri adds three rules
//!   (`kiri_rules`) with the host's own iptables, into Docker's DOCKER-USER
//!   chain when it is there, else at the top of FORWARD.
//! - firewalld: Kiri's bridges go into its `trusted` zone (runtime only),
//!   as Docker's and libvirt's bridges go into zones with target ACCEPT,
//!   and Kiri's own forward chain then keeps their new connections to the
//!   uplink (`netlink::FORWARD_GUARD`).
//! - any other nftables chain on the forward hook with policy drop (an
//!   /etc/nftables.conf of the administrator's own): Kiri does not change
//!   another program's nftables chain. It says once in its log, and in
//!   `docker info`, which rules let its containers through.
//!
//! Only what Kiri's containers need passes: new connections out through the
//! uplink Kiri masquerades on, replies, and traffic into Kiri's bridges,
//! which Kiri's own forward chain judges (replies, published ports, the same
//! network). No new connection into another program's bridge (Docker's,
//! libvirt's) is accepted by these rules.
//!
//! Checked when the engine starts, when it brings up a bridge, and after
//! every nf_tables change (`changed`), so a firewall started, reloaded or
//! flushed after the engine is covered: the rules are added again, and
//! Kiri's own table too when a flush took it. A check is one netlink dump of
//! the chains; a program runs only when something must change. On a host
//! with none of these firewalls nothing runs. Taken out when the engine
//! stops; rules an engine that crashed left behind are reused, not doubled.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::netlink::{self, Chain};

/// Why the firewall is looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Start,
    Bridge,
    /// An nf_tables change. iptables-legacy's tables make no such event, so
    /// they are looked at only on the others.
    Event,
    Stop,
}

/// Kiri's rules in iptables' filter table, as iptables arguments after the
/// chain: containers out through the uplink (what Kiri masquerades), their
/// replies wherever they go (a published port reached from another
/// interface), and anything into Kiri's bridges, which Kiri's own forward
/// chain judges.
pub fn kiri_rules(uplink: &str) -> [Vec<&str>; 3] {
    [
        vec!["-i", "kiri+", "-o", uplink, "-m", "comment", "--comment", "kiri", "-j", "ACCEPT"],
        vec!["-i", "kiri+", "-m", "conntrack", "--ctstate", "RELATED,ESTABLISHED", "-m", "comment", "--comment", "kiri", "-j", "ACCEPT"],
        vec!["-o", "kiri+", "-m", "comment", "--comment", "kiri", "-j", "ACCEPT"],
    ]
}

/// Docker's chain for other software's rules in iptables' `filter` table.
const DOCKER_USER: &str = "DOCKER-USER";
const FORWARD: &str = "FORWARD";

/// Where Kiri's rules go in iptables' filter table of one family: Docker's
/// DOCKER-USER when it is there (Docker jumps to it first and never empties
/// it, also when it restarts), else the top of FORWARD when its policy is
/// drop (ufw, or set by hand), as Docker, libvirt and podman put theirs.
/// ufw reloads only its own chains, so rules at the top of FORWARD stay,
/// ahead of ufw's. None when nothing there drops Kiri's traffic.
pub fn iptables_target(docker_user: bool, forward_drop: bool) -> Option<&'static str> {
    if docker_user {
        Some(DOCKER_USER)
    } else if forward_drop {
        Some(FORWARD)
    } else {
        None
    }
}

const NFPROTO_INET: u8 = 1;
const NFPROTO_IPV4: u8 = libc::NFPROTO_IPV4 as u8;
const NFPROTO_IPV6: u8 = libc::NFPROTO_IPV6 as u8;
const NF_INET_FORWARD: u32 = 2;
const NF_DROP: u32 = 0;

/// What the host's nf_tables chains say about its forward hook.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Hook {
    /// Per family (IPv4, IPv6): where Kiri's rules go in iptables-nft's
    /// filter table.
    pub iptables: [Option<&'static str>; 2],
    /// firewalld runs (its `inet firewalld` table), by the handle of a chain
    /// of it: a new one after firewalld reloads or restarts, which drops the
    /// runtime zone Kiri's bridges were put in.
    pub firewalld: Option<u64>,
    /// Kiri's own forward chain, IPv4 and IPv6: gone after an `nft flush
    /// ruleset` (nftables.service starting or reloading).
    pub kiri: [bool; 2],
    /// Other base chains on the forward hook with policy drop, as (family,
    /// table, chain): an nftables.conf of the administrator's.
    pub foreign: Vec<(u8, String, String)>,
}

pub fn read_hook(chains: &[Chain]) -> Hook {
    let mut h = Hook::default();
    for (i, family) in [NFPROTO_IPV4, NFPROTO_IPV6].into_iter().enumerate() {
        let has = |table: &str, name: &str| chains.iter().find(|c| c.family == family && c.table == table && c.name == name);
        let forward_drop = has("filter", FORWARD).is_some_and(|c| c.policy == Some(NF_DROP));
        h.iptables[i] = iptables_target(has("filter", DOCKER_USER).is_some(), forward_drop);
        h.kiri[i] = has("kiri", "forward").is_some();
    }
    h.firewalld = chains.iter().filter(|c| c.family == NFPROTO_INET && c.table == "firewalld").map(|c| c.handle).min();
    for c in chains {
        let iptables = matches!(c.family, NFPROTO_IPV4 | NFPROTO_IPV6) && c.table == "filter" && c.name == FORWARD;
        let ours = c.table == "kiri" && c.family != NFPROTO_INET;
        let firewalld = c.family == NFPROTO_INET && c.table == "firewalld";
        if c.hook == Some(NF_INET_FORWARD) && c.policy == Some(NF_DROP) && matches!(c.family, NFPROTO_INET | NFPROTO_IPV4 | NFPROTO_IPV6) && !iptables && !ours && !firewalld {
            h.foreign.push((c.family, c.table.clone(), c.name.clone()));
        }
    }
    h
}

/// Kiri's rules in an `iptables -S` listing, as (chain, arguments after
/// it): the lines with the comment `kiri`, quotes taken off.
pub fn kiri_lines(listing: &str) -> Vec<(String, Vec<String>)> {
    listing
        .lines()
        .filter_map(|l| {
            let words: Vec<String> = l.split_whitespace().map(|w| w.trim_matches('"').to_owned()).collect();
            let ours = words.windows(2).any(|w| w[0] == "--comment" && w[1] == "kiri");
            (ours && words.len() > 2 && words[0] == "-A").then(|| (words[1].clone(), words[2..].to_vec()))
        })
        .collect()
}

/// Where Kiri's rules go, from an `iptables -S` listing of the filter
/// table (iptables-legacy, which only the program reads).
pub fn listed_target(listing: &str) -> Option<&'static str> {
    let docker_user = listing.lines().any(|l| l.trim_end() == format!("-N {DOCKER_USER}"));
    let forward_drop = listing.lines().any(|l| l.trim_end() == format!("-P {FORWARD} DROP"));
    iptables_target(docker_user, forward_drop)
}

/// The changes that bring Kiri's rules in `listing` in line: all three in
/// `target` (None: nowhere, the engine stopping) and none elsewhere, as
/// iptables arguments (`-D chain …`, then `-I chain …`). Rules an engine
/// that crashed left behind are kept where they belong, and taken out of a
/// chain they no longer belong in (or for an uplink that is not this one).
/// Inserted last rule first, so the three keep their order at the top.
pub fn iptables_changes(listing: &str, target: Option<&str>, uplink: &str) -> Vec<Vec<String>> {
    let have = kiri_lines(listing);
    let want: Vec<(String, Vec<String>)> = target.map_or_else(Vec::new, |t| kiri_rules(uplink).iter().map(|r| (t.to_owned(), r.iter().map(|w| w.to_string()).collect())).collect());
    let mut out = Vec::new();
    for (chain, args) in &have {
        if !want.contains(&(chain.clone(), args.clone())) {
            out.push([vec!["-D".to_owned(), chain.clone()], args.clone()].concat());
        }
    }
    for (chain, args) in want.iter().rev() {
        if !have.contains(&(chain.clone(), args.clone())) {
            out.push([vec!["-I".to_owned(), chain.clone()], args.clone()].concat());
        }
    }
    out
}

/// What the administrator's own forward chain needs to let Kiri's
/// containers through, as `nft` commands.
pub fn foreign_advice(family: u8, table: &str, chain: &str, up4: &str, up6: &str) -> String {
    let fam = match family {
        NFPROTO_INET => "inet",
        NFPROTO_IPV6 => "ip6",
        _ => "ip",
    };
    let mut ups = vec![if family == NFPROTO_IPV6 { up6 } else { up4 }];
    if family == NFPROTO_INET && up6 != up4 {
        ups.push(up6);
    }
    let mut rules: Vec<String> = ups.iter().map(|u| format!("iifname \"kiri*\" oifname \"{u}\" accept")).collect();
    rules.push("iifname \"kiri*\" ct state established,related accept".into());
    rules.push("oifname \"kiri*\" accept".into());
    let cmds: Vec<String> = rules.iter().map(|r| format!("nft insert rule {fam} {table} {chain} {r}")).collect();
    format!(
        "the firewall's nftables chain '{fam} {table} {chain}' (policy drop) drops the traffic of Kiri's containers, so they have no internet; Kiri does not change another program's chain. To let them through, run: {} (and add the same rules to that chain in /etc/nftables.conf to keep them)",
        cmds.join("; ")
    )
}

/// What Kiri did in one place: nothing yet, its rules are in (the chain
/// or zone), or adding them there failed (said once, and not tried again on
/// every change while that is still where they go).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Rules {
    #[default]
    None,
    In(&'static str),
    Failed(&'static str),
}

impl Rules {
    fn failed(self) -> bool {
        matches!(self, Rules::Failed(_))
    }
}

#[derive(Default)]
struct State {
    /// iptables-nft and iptables-legacy, each for IPv4 and IPv6.
    iptables: [[Rules; 2]; 2],
    firewalld: Rules,
    /// The firewalld (by `Hook::firewalld`) that `firewalld` is about.
    firewalld_seen: Option<u64>,
    /// Foreign chains already told about in the log.
    told: Vec<String>,
    /// What `docker info` shows, as last written.
    warnings: String,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// The Linux host's uplinks by name, IPv4 and IPv6 (`net::host_uplink`).
static UPLINKS: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

pub fn set_uplinks(up4: String, up6: String) {
    let _ = UPLINKS.set((up4, up6));
}

/// Brings the host's firewall in line with Kiri's needs (see the module
/// documentation), or takes Kiri's part out of it (`Why::Stop`). `rebuild`
/// builds Kiri's own tables (IPv4, IPv6) again as last built, when a flush
/// took them or their forward chain must change; it does nothing for a
/// table Kiri has not built.
pub fn sync(why: Why, rebuild: impl Fn([bool; 2])) {
    let Some((up4, up6)) = UPLINKS.get() else { return };
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let st = guard.get_or_insert_with(State::default);
    if why == Why::Stop {
        stop(st, up4, up6);
        return;
    }
    let chains = match netlink::chains() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kiri-init: cannot list the firewall's nftables chains: {e}");
            Vec::new()
        }
    };
    let hook = read_hook(&chains);
    let mut warnings = Vec::new();
    let families = [(NFPROTO_IPV4, "iptables", "/proc/net/ip_tables_names", up4.as_str()), (NFPROTO_IPV6, "ip6tables", "/proc/net/ip6_tables_names", up6.as_str())];
    for (i, (family, tool, legacy_names, uplink)) in families.into_iter().enumerate() {
        // iptables-nft: its filter table is in nf_tables, read above.
        let rules = &mut st.iptables[0][i];
        if let Rules::In(chain) = *rules {
            // Gone with a flush, `iptables -F`, a restore of saved rules.
            if netlink::rules_with(family, "filter", chain, b"kiri\0").unwrap_or(0) < 3 {
                *rules = Rules::None;
            }
        }
        // Tried again when the engine starts or makes a bridge, or when
        // the place changed (the chain it failed in went, Docker came).
        if let Rules::Failed(t) = *rules
            && (why != Why::Event || hook.iptables[i] != Some(t))
        {
            *rules = Rules::None;
        }
        if let (Rules::None, Some(target)) = (*rules, hook.iptables[i]) {
            *rules = match sbin(&format!("{tool}-nft")).or_else(|| sbin(tool)) {
                Some(path) => iptables_sync(&path, None, Some(target), uplink),
                None => {
                    eprintln!("kiri-init: iptables' {target} chain drops forwarded traffic on this host, but there is no {tool} to let Kiri's containers through it");
                    Rules::Failed(target)
                }
            };
        }
        if rules.failed() {
            warnings.push(format!("{tool}: Kiri could not add its rules to the filter table, whose FORWARD chain drops the traffic of Kiri's containers; see the engine log"));
        }
        // iptables-legacy: in x_tables, which only the program reads, and
        // which makes no nf_tables event.
        let rules = &mut st.iptables[1][i];
        if why != Why::Event
            && std::fs::read_to_string(legacy_names).is_ok_and(|t| t.lines().any(|l| l == "filter"))
            && let Some(path) = sbin(&format!("{tool}-legacy")).or_else(|| sbin(tool))
            && let Some(listing) = run(&path, &["-S"])
        {
            // Where they go, or where they are when nothing drops any more.
            let keep = match (*rules, listed_target(&listing)) {
                (Rules::In(c), None) => Some(c),
                (_, target) => target,
            };
            if keep.is_some() {
                *rules = iptables_sync(&path, Some(&listing), keep, uplink);
            }
        }
        if rules.failed() {
            warnings.push(format!("{tool}-legacy: Kiri could not add its rules to the filter table, whose FORWARD chain drops the traffic of Kiri's containers; see the engine log"));
        }
    }
    firewalld(st, hook.firewalld, why, &mut warnings);
    // Kiri's forward chain keeps new connections to the uplink while its
    // bridges are in firewalld's trusted zone (`netlink::FORWARD_GUARD`).
    let guard = (st.firewalld == Rules::In(ZONE)).then(|| (up4.clone(), up6.clone()));
    let guard_changed = {
        let mut g = netlink::FORWARD_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let changed = *g != guard;
        *g = guard;
        changed
    };
    // A flush of the whole ruleset (nftables.service starting or
    // reloading) took Kiri's own tables. The IPv6 one is there only with an
    // IPv6 network, which `rebuild` knows.
    let gone = [why == Why::Event && !hook.kiri[0], why == Why::Event && !hook.kiri[1]];
    if gone[0] {
        eprintln!("kiri-init: Kiri's nftables table was removed (a flush of the whole ruleset?); building it again");
    }
    if guard_changed || gone[0] || gone[1] {
        rebuild([guard_changed || gone[0], guard_changed || gone[1]]);
    }
    for (family, table, chain) in &hook.foreign {
        // Already let through (a rule naming Kiri's bridges).
        if netlink::rules_with(*family, table, chain, b"kiri").unwrap_or(0) > 0 {
            continue;
        }
        let advice = foreign_advice(*family, table, chain, up4, up6);
        let key = format!("{family} {table} {chain}");
        if !st.told.contains(&key) {
            eprintln!("kiri-init: {advice}");
            st.told.push(key);
        }
        warnings.push(format!("Linux host: {advice}"));
    }
    write_warnings(st, warnings.join("\n"));
}

/// Brings Kiri's rules in iptables' filter table (`listing`, or listed
/// now) in line with `target`; see `iptables_changes`.
fn iptables_sync(tool: &Path, listing: Option<&str>, target: Option<&'static str>, uplink: &str) -> Rules {
    let listed;
    let listing = match listing {
        Some(l) => l,
        None => match run(tool, &["-S"]) {
            Some(l) => {
                listed = l;
                &listed
            }
            None => {
                eprintln!("kiri-init: {} cannot list the filter table, so Kiri's containers may not pass the host's firewall", tool.display());
                return target.map_or(Rules::None, Rules::Failed);
            }
        },
    };
    let mut ok = true;
    for change in iptables_changes(listing, target, uplink) {
        let args: Vec<&str> = change.iter().map(String::as_str).collect();
        ok &= run(tool, &args).is_some();
    }
    match (target, ok) {
        (Some(t), true) => {
            eprintln!("kiri-init: the host's firewall drops forwarded traffic: Kiri's containers pass it by rules in iptables' {t} chain ({})", tool.display());
            Rules::In(t)
        }
        (Some(t), false) => {
            eprintln!("kiri-init: {} rules for Kiri's containers could not be added to {t}", tool.display());
            Rules::Failed(t)
        }
        (None, ok) => {
            if !ok {
                eprintln!("kiri-init: {} rules for Kiri's containers could not all be removed", tool.display());
            }
            Rules::None
        }
    }
}

/// firewalld's zone for Kiri's bridges (all of them: `kiri+`).
const ZONE: &str = "trusted";

/// Puts Kiri's bridges into firewalld's trusted zone, runtime only, once
/// per firewalld run (a reload or restart drops runtime settings, and makes
/// its table anew). An interface in no zone is in the default one, whose
/// rules can reject the containers' traffic: a zone without forwarding (as
/// firewalld before 1.0 had them, or set so), or the drop zone; and public
/// takes only ssh from them, not the host's other services. A zone with
/// target ACCEPT is what Docker (its `docker` zone) and libvirt give their
/// bridges; Kiri's own forward chain then keeps the containers' new
/// connections to the uplink, and the other programs' bridges keep their own
/// filters. When the administrator put `kiri+` in a zone already, that zone
/// stays.
fn firewalld(st: &mut State, seen: Option<u64>, why: Why, warnings: &mut Vec<String>) {
    if seen.is_none() {
        st.firewalld = Rules::None;
        st.firewalld_seen = None;
        return;
    }
    if st.firewalld_seen != seen || (st.firewalld.failed() && why != Why::Event) {
        st.firewalld_seen = seen;
        st.firewalld = match sbin("firewall-cmd") {
            None => {
                eprintln!("kiri-init: firewalld runs, but there is no firewall-cmd to put Kiri's bridges in its {ZONE} zone");
                Rules::Failed(ZONE)
            }
            Some(cmd) => match std::process::Command::new(&cmd).args([&format!("--zone={ZONE}"), "--add-interface=kiri+"]).stdin(std::process::Stdio::null()).output() {
                Ok(o) if o.status.success() => {
                    eprintln!("kiri-init: firewalld runs: Kiri's bridges (kiri+) are in its {ZONE} zone until the engine stops");
                    Rules::In(ZONE)
                }
                Ok(o) => {
                    let why = String::from_utf8_lossy(&o.stderr).trim().to_owned();
                    if why.contains("ZONE_CONFLICT") {
                        eprintln!("kiri-init: firewalld runs, and Kiri's bridges (kiri+) are in a zone of the administrator's already, which decides what Kiri's containers may reach");
                        Rules::None
                    } else {
                        eprintln!("kiri-init: firewalld runs, but Kiri's bridges could not be put in its {ZONE} zone: {why}");
                        Rules::Failed(ZONE)
                    }
                }
                Err(e) => {
                    eprintln!("kiri-init: firewalld runs, but firewall-cmd failed: {e}");
                    Rules::Failed(ZONE)
                }
            },
        };
    }
    if st.firewalld.failed() {
        warnings.push(format!("firewalld: Kiri could not put its bridges (kiri+) in the {ZONE} zone, so firewalld may reject its containers' traffic; see the engine log"));
    }
}

/// The engine stopping: Kiri's rules out of iptables (where it put them)
/// and its bridges out of firewalld's zone, unless the administrator put
/// them there for good (the permanent configuration).
fn stop(st: &mut State, up4: &str, up6: &str) {
    for (backend, suffix) in ["-nft", "-legacy"].into_iter().enumerate() {
        for (i, (tool, uplink)) in [("iptables", up4), ("ip6tables", up6)].into_iter().enumerate() {
            if let Rules::In(_) = st.iptables[backend][i]
                && let Some(path) = sbin(&format!("{tool}{suffix}")).or_else(|| sbin(tool))
            {
                iptables_sync(&path, None, None, uplink);
            }
            st.iptables[backend][i] = Rules::None;
        }
    }
    if st.firewalld == Rules::In(ZONE)
        && let Some(cmd) = sbin("firewall-cmd")
    {
        let kept = run(&cmd, &["--permanent", &format!("--zone={ZONE}"), "--query-interface=kiri+"]).is_some_and(|o| o.trim() == "yes");
        if !kept && run(&cmd, &[&format!("--zone={ZONE}"), "--remove-interface=kiri+"]).is_none() {
            eprintln!("kiri-init: Kiri's bridges could not be taken out of firewalld's {ZONE} zone");
        }
    }
    st.firewalld = Rules::None;
    st.firewalld_seen = None;
    *netlink::FORWARD_GUARD.lock().unwrap_or_else(|e| e.into_inner()) = None;
    write_warnings(st, String::new());
}

/// Runs an administrator's program; its output when it succeeded.
fn run(tool: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new(tool);
    if tool.file_name().is_some_and(|n| n.to_string_lossy().contains("tables")) {
        // Waits for another program holding the xtables lock (ufw, Docker).
        cmd.args(["-w", "5"]);
    }
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Where the warnings go for `docker info`: a file in the agent's folder,
/// which the engine reads.
pub const WARNINGS_FILE: &str = "firewall-warnings";

fn write_warnings(st: &mut State, text: String) {
    if st.warnings == text {
        return;
    }
    if let Some(dir) = super::transport::host_dir() {
        let path = dir.join(WARNINGS_FILE);
        let _ = if text.is_empty() { std::fs::remove_file(path) } else { std::fs::write(path, &text) };
    }
    st.warnings = text;
}

/// An administrator's program (iptables) by name, wherever the host keeps
/// it: the agent's PATH may lack the sbin folders.
fn sbin(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':')
        .chain(["/usr/sbin", "/sbin", "/usr/local/sbin", "/usr/bin", "/bin"])
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(family: u8, table: &str, name: &str, hook: Option<u32>, policy: Option<u32>) -> Chain {
        Chain { family, table: table.into(), name: name.into(), handle: 1, hook, policy }
    }

    #[test]
    fn kiri_rules_accept_only_kiris_own_traffic() {
        let [out, replies, into] = kiri_rules("eth0");
        // New connections out of Kiri's bridges only through the uplink,
        // never into another program's bridges; replies anywhere.
        assert_eq!(out.join(" "), "-i kiri+ -o eth0 -m comment --comment kiri -j ACCEPT");
        assert_eq!(replies.join(" "), "-i kiri+ -m conntrack --ctstate RELATED,ESTABLISHED -m comment --comment kiri -j ACCEPT");
        // Into Kiri's bridges, where Kiri's forward chain judges.
        assert_eq!(into.join(" "), "-o kiri+ -m comment --comment kiri -j ACCEPT");
    }

    #[test]
    fn a_host_without_a_firewall_needs_nothing() {
        let chains = [chain(2, "kiri", "forward", Some(2), Some(1)), chain(2, "kiri", "postrouting", Some(4), Some(1)), chain(10, "kiri", "forward", Some(2), Some(1))];
        assert_eq!(read_hook(&chains), Hook { kiri: [true, true], ..Hook::default() });
    }

    #[test]
    fn a_forward_policy_of_drop_gets_rules_at_the_top_of_forward() {
        // ufw (iptables-nft), or `iptables -P FORWARD DROP` by hand: no DOCKER-USER.
        let mut chains = vec![chain(2, "filter", "FORWARD", Some(2), Some(0)), chain(2, "filter", "ufw-before-forward", None, None), chain(10, "filter", "FORWARD", Some(2), Some(0))];
        assert_eq!(read_hook(&chains).iptables, [Some("FORWARD"), Some("FORWARD")]);
        assert!(read_hook(&chains).foreign.is_empty(), "iptables' own chain is not a foreign one");
        // IPv4 only, by hand.
        chains[2].policy = Some(1);
        assert_eq!(read_hook(&chains).iptables, [Some("FORWARD"), None]);
        // Policy accept: nothing.
        chains[0].policy = Some(1);
        assert_eq!(read_hook(&chains).iptables, [None, None]);
    }

    #[test]
    fn with_docker_the_rules_stay_in_docker_user() {
        let chains = [chain(2, "filter", "FORWARD", Some(2), Some(0)), chain(2, "filter", "DOCKER-USER", None, None)];
        assert_eq!(read_hook(&chains).iptables, [Some("DOCKER-USER"), None]);
        // Docker's chain is used even while FORWARD accepts (Docker started after Kiri turned forwarding on).
        let chains = [chain(2, "filter", "FORWARD", Some(2), Some(1)), chain(2, "filter", "DOCKER-USER", None, None)];
        assert_eq!(read_hook(&chains).iptables, [Some("DOCKER-USER"), None]);
    }

    #[test]
    fn firewalld_and_an_nftables_conf_of_ones_own_are_told_apart() {
        let mut fwd = chain(1, "firewalld", "filter_FORWARD", Some(2), Some(1));
        fwd.handle = 7;
        let mut zones = chain(1, "firewalld", "filter_FORWARD_POLICIES", None, None);
        zones.handle = 9;
        let own = chain(1, "filter", "forward", Some(2), Some(0));
        let own_input = chain(1, "filter", "input", Some(1), Some(0));
        let own_accepting = chain(2, "mine", "fwd", Some(2), Some(1));
        let h = read_hook(&[fwd, zones, own, own_input, own_accepting]);
        assert_eq!(h.firewalld, Some(7));
        assert_eq!(h.foreign, vec![(1, "filter".to_owned(), "forward".to_owned())]);
        assert_eq!(h.iptables, [None, None]);
    }

    #[test]
    fn kiris_lines_are_found_in_a_listing() {
        let listing = "-P INPUT DROP\n-P FORWARD DROP\n-N ufw-before-forward\n-A FORWARD -o kiri+ -m comment --comment kiri -j ACCEPT\n-A FORWARD -j ufw-before-forward\n-A ufw-before-forward -i kiri+ -j ACCEPT\n-A DOCKER-USER -i kiri+ -o eth0 -m comment --comment \"kiri\" -j ACCEPT\n";
        assert_eq!(
            kiri_lines(listing),
            vec![
                ("FORWARD".to_owned(), "-o kiri+ -m comment --comment kiri -j ACCEPT".split(' ').map(String::from).collect()),
                ("DOCKER-USER".to_owned(), "-i kiri+ -o eth0 -m comment --comment kiri -j ACCEPT".split(' ').map(String::from).collect()),
            ]
        );
        assert_eq!(listed_target(listing), Some("FORWARD"));
        assert_eq!(listed_target("-P FORWARD DROP\n-N DOCKER-USER\n"), Some("DOCKER-USER"));
        assert_eq!(listed_target("-P FORWARD ACCEPT\n"), None);
    }

    #[test]
    fn rules_left_by_a_crashed_engine_are_reused_not_doubled() {
        let all = |chain: &str, up: &str| kiri_rules(up).iter().map(|r| format!("-A {chain} {}\n", r.join(" "))).collect::<String>();
        let listing = format!("-P FORWARD DROP\n{}", all("FORWARD", "eth0"));
        assert!(iptables_changes(&listing, Some("FORWARD"), "eth0").is_empty());
        // One missing: only it is added.
        let partial: String = listing.lines().filter(|l| !l.contains("RELATED")).map(|l| format!("{l}\n")).collect();
        let changes = iptables_changes(&partial, Some("FORWARD"), "eth0");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0][..3], ["-I", "FORWARD", "-i"]);
        assert!(changes[0].contains(&"RELATED,ESTABLISHED".to_owned()));
        // Stopping: all three out.
        let out = iptables_changes(&listing, None, "eth0");
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|c| c[0] == "-D" && c[1] == "FORWARD"));
    }

    #[test]
    fn rules_in_the_wrong_chain_or_for_another_uplink_move() {
        let all = |chain: &str, up: &str| kiri_rules(up).iter().map(|r| format!("-A {chain} {}\n", r.join(" "))).collect::<String>();
        // Left in FORWARD; Docker came since: kept where they are only if
        // asked for there, so here they move to DOCKER-USER.
        let changes = iptables_changes(&all("FORWARD", "eth0"), Some("DOCKER-USER"), "eth0");
        assert_eq!(changes.iter().filter(|c| c[0] == "-D" && c[1] == "FORWARD").count(), 3);
        assert_eq!(changes.iter().filter(|c| c[0] == "-I" && c[1] == "DOCKER-USER").count(), 3);
        // The uplink changed: only the rule naming it is replaced.
        let changes = iptables_changes(&all("FORWARD", "wlan0"), Some("FORWARD"), "eth0");
        assert_eq!(changes.len(), 2);
        assert!(changes[0].join(" ").starts_with("-D FORWARD -i kiri+ -o wlan0"));
        assert!(changes[1].join(" ").starts_with("-I FORWARD -i kiri+ -o eth0"));
        // Inserted last first, so they end up in order at the top.
        let changes = iptables_changes("-P FORWARD DROP\n", Some("FORWARD"), "eth0");
        assert_eq!(changes.iter().map(|c| c[2].as_str()).collect::<Vec<_>>(), ["-o", "-i", "-i"]);
    }

    #[test]
    fn advice_for_an_nftables_conf_names_the_chain_and_the_uplink() {
        let a = foreign_advice(1, "filter", "forward", "eth0", "eth0");
        assert!(a.contains("nft insert rule inet filter forward iifname \"kiri*\" oifname \"eth0\" accept"));
        assert!(a.contains("nft insert rule inet filter forward iifname \"kiri*\" ct state established,related accept"));
        assert!(a.contains("nft insert rule inet filter forward oifname \"kiri*\" accept"));
        // Another IPv6 uplink in an inet chain: a rule for each.
        let a = foreign_advice(1, "filter", "forward", "eth0", "wg0");
        assert!(a.contains("oifname \"wg0\" accept"));
        assert!(foreign_advice(10, "f", "fwd", "eth0", "wg0").contains("nft insert rule ip6 f fwd iifname \"kiri*\" oifname \"wg0\" accept"));
    }
}
