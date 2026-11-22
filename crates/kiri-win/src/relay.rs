//! Windows' end of the proxy relay (crates/kiri/src/hostrelay.rs): a proxy
//! on Windows' own 127.0.0.1, as most Windows proxy apps set, can't be
//! reached from WSL, so `kiri hostrelay` in the distro listens for it and
//! sends each connection here over wsl.exe's stdio, and this dials the proxy.

use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::hostrelay::{self, Out};

struct Relay {
    out: Out,
    distro: Option<String>,
    /// Windows ports it was asked to listen for: the only ones dialled.
    allowed: Mutex<HashSet<u16>>,
    /// Windows port → the distro's port; 0 when it couldn't listen.
    ports: Mutex<HashMap<u16, u16>>,
    changed: Condvar,
    dead: AtomicBool,
}

static RELAY: Mutex<Option<Arc<Relay>>> = Mutex::new(None);
/// The last failure logged, so a minute's retry doesn't repeat it.
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn log_once(msg: String) {
    let mut last = lock(&LAST_ERROR);
    if *last != msg {
        crate::serve::log(&msg);
        *last = msg;
    }
}

fn start(distro: Option<String>) -> std::io::Result<Arc<Relay>> {
    let mut cmd = crate::win::tool("wsl.exe");
    if let Some(d) = &distro {
        cmd.args(["-d", d]);
    }
    let mut child = cmd
        .args(["-u", "root", "--", "/usr/local/bin/kiri", "hostrelay"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn()?;
    let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else { return Err(std::io::Error::other("wsl.exe has no stdio")) };
    let relay = Arc::new(Relay {
        out: Arc::new(Mutex::new(input)),
        distro,
        allowed: Mutex::new(HashSet::new()),
        ports: Mutex::new(HashMap::new()),
        changed: Condvar::new(),
        dead: AtomicBool::new(false),
    });
    let r = relay.clone();
    std::thread::spawn(move || {
        let (a, p) = (r.clone(), r.clone());
        let end = hostrelay::dial_side(
            output,
            &r.out,
            move |port| lock(&a.allowed).contains(&port),
            move |target, local| {
                lock(&p.ports).insert(target, local);
                p.changed.notify_all();
            },
        );
        r.dead.store(true, Ordering::SeqCst);
        r.changed.notify_all();
        let status = child.wait();
        log_once(format!("proxy relay ended: {end:?}, {status:?}"));
    });
    Ok(relay)
}

/// For each Windows loopback port, the distro's port that leads to it,
/// starting the relay when it isn't running. Empty when it can't run (an
/// engine from before the relay), and the proxy is used as set.
pub fn ports(targets: &[u16]) -> HashMap<u16, u16> {
    if targets.is_empty() {
        return HashMap::new();
    }
    // The engine's distro, or during setup the one setup just made, as the bridge finds it.
    // None before setup has made one: WSL's default distro may be anything.
    let Some(distro) = crate::engine::distro().or_else(crate::setup::distro) else { return HashMap::new() };
    let distro = Some(distro);
    let relay = {
        let mut slot = lock(&RELAY);
        match slot.as_ref() {
            Some(r) if !r.dead.load(Ordering::SeqCst) && r.distro == distro => r.clone(),
            _ => match start(distro) {
                Ok(r) => {
                    *slot = Some(r.clone());
                    r
                }
                Err(e) => {
                    log_once(format!("proxy relay: {e}"));
                    return HashMap::new();
                }
            },
        }
    };
    for &t in targets {
        if lock(&relay.allowed).insert(t) {
            let _ = hostrelay::send(&relay.out, hostrelay::LISTEN, 0, &t.to_be_bytes());
        }
    }
    // A relay just started answers once WSL has run it: well within this.
    let until = Instant::now() + Duration::from_secs(10);
    let mut ports = lock(&relay.ports);
    while !targets.iter().all(|t| ports.contains_key(t)) && !relay.dead.load(Ordering::SeqCst) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        ports = relay.changed.wait_timeout(ports, left.min(Duration::from_millis(200))).unwrap_or_else(PoisonError::into_inner).0;
    }
    let map: HashMap<u16, u16> = targets.iter().filter_map(|t| ports.get(t).filter(|&&l| l != 0).map(|&l| (*t, l))).collect();
    drop(ports);
    if map.len() == targets.len() {
        log_once(format!("proxy relay: {}", map.iter().map(|(t, l)| format!("Windows 127.0.0.1:{t} is 127.0.0.1:{l} in WSL")).collect::<Vec<_>>().join(", ")));
    } else {
        log_once(format!("proxy relay: no answer for {targets:?}"));
    }
    map
}
