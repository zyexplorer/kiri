//! Share in the window: public links the app holds open while it runs, and
//! the Share account. The tunnels are `crate::share`'s, the same library as
//! `kiri share`; this keeps their handles and turns their events into page
//! events (`{"type":"share", "id", ...}`).
//!
//! The Windows window (crates/kiri-win) includes this file as its own
//! `sharing` module: it needs only `super::{emit, notice}`,
//! `crate::client` (the engine's API) and `crate::share`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

use super::emit;
use crate::share;

/// One link: what it forwards to, its traffic, and its tunnel while open.
struct Link {
    /// What was shared, as the page named it: a container's name, or a port.
    source: String,
    target: String,
    name: Option<String>,
    url: Option<String>,
    state: &'static str,
    requests: u64,
    bytes_in: u64,
    bytes_out: u64,
    handle: Option<share::Handle>,
    /// A Kubernetes share's loopback port from the engine, closed with it.
    forward: Option<u16>,
}

static LINKS: Mutex<Option<HashMap<u64, Link>>> = Mutex::new(None);
static NEXT: AtomicU64 = AtomicU64::new(1);
/// A GitHub sign-in is waiting for its code to be entered.
static SIGNING_IN: AtomicBool = AtomicBool::new(false);
/// Everything served since the app opened, links closed since included:
/// requests, bytes in (visitors → here) and bytes out (here → visitors).
static SERVED: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

fn with_links<R>(f: impl FnOnce(&mut HashMap<u64, Link>) -> R) -> R {
    f(LINKS.lock().unwrap().get_or_insert_with(HashMap::new))
}

/// A short name for this sign-in, to show and quote: a hash of the token, so
/// it says which session without giving the token away.
fn session_id(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(token.as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("ks_{hex}")
}

fn account() -> Value {
    match share::load() {
        Ok(c) => {
            json!({"type": "shareaccount", "signedIn": c.token.is_some(), "login": c.login, "server": c.server, "session": c.token.as_deref().map(session_id)})
        }
        Err(e) => json!({"type": "shareaccount", "signedIn": false, "error": e.to_string()}),
    }
}

/// Everything the Share view shows: the account and the open links.
pub fn list() {
    emit(account().to_string());
    let links: Vec<Value> = with_links(|m| {
        let mut v: Vec<_> = m
            .iter()
            .map(|(id, l)| json!({"id": id, "source": l.source, "target": l.target, "name": l.name, "url": l.url, "state": l.state, "requests": l.requests, "in": l.bytes_in, "out": l.bytes_out}))
            .collect();
        v.sort_by_key(|x| x["id"].as_u64());
        v
    });
    let [r, i, o] = &SERVED;
    let served = json!({"requests": r.load(Ordering::Relaxed), "in": i.load(Ordering::Relaxed), "out": o.load(Ordering::Relaxed)});
    emit(json!({"type": "shares", "shares": links, "served": served}).to_string());
}

/// The account's plan and what it has used this month, from the edge.
pub fn usage() {
    std::thread::spawn(|| {
        let v = match share::whoami() {
            Ok(w) => json!({
                "type": "shareusage", "login": w["login"], "used": w["usage"]["used"], "cap": w["usage"]["cap"],
                "tunnels": w["limits"]["tunnels"], "rate": w["limits"]["rate"], "warning": w["limits"]["warning"],
            }),
            Err(e) => json!({"type": "shareusage", "error": e.to_string()}),
        };
        emit(v.to_string());
    });
}

fn event(id: u64, ev: share::Event) -> Value {
    use share::Event::*;
    let mut v = match ev {
        Connecting { server } => json!({"state": "connecting", "server": server}),
        Live {
            url,
            rate,
            remaining,
            warning,
            domains,
        } => {
            json!({"state": "live", "url": url, "rate": rate, "remaining": remaining, "warning": warning, "domains": domains})
        }
        Request(r) => json!({"request": {
            "method": r.method, "path": r.path, "status": r.status, "ms": r.ms, "in": r.bytes_in, "out": r.bytes_out,
            "at": r.at.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
        }}),
        Notice(text) => json!({"notice": text}),
        Reconnecting { reason, delay } => {
            json!({"state": "reconnecting", "reason": reason, "delay_ms": delay.as_millis() as u64})
        }
        Ended { reason, error } => json!({"state": "ended", "reason": reason, "error": error}),
    };
    v["type"] = "share".into();
    v["id"] = id.into();
    v
}

/// Opens a link to `target` (a port, `host:port` or a container name),
/// optionally asking for `name` as the first label of its hostname.
pub fn open(target: String, name: String) {
    let source = target.trim().to_owned();
    start(source.clone(), name, move || {
        resolve(&source).map(|t| (t, None))
    });
}

/// A port, `host:port` or a container name, the container looked up
/// through this window's client of the engine.
fn resolve(arg: &str) -> std::io::Result<share::Target> {
    share::resolve_via(arg, |name| {
        if !crate::client::alive() {
            return Err(std::io::Error::other(format!(
                "`{name}` isn't a port, and Kiri isn't running to look up a container by that name"
            )));
        }
        let r = crate::client::request("GET", &format!("/containers/{name}/json"), None)?;
        if r.status == 404 {
            return Ok(None);
        }
        if !r.ok() {
            return Err(std::io::Error::other(r.error()));
        }
        Ok(Some(r.json()))
    })
}

/// Opens a URL in the default browser.
fn browser(url: &str) {
    #[cfg(unix)]
    super::open(url);
    #[cfg(windows)]
    crate::app::open_url(url);
}

/// Opens a link to a Kubernetes service or pod at `ip:port` in the cluster,
/// through a loopback port the engine forwards there. `source` is how the
/// page names it ("service default/web :80"); `label` the hostname's
/// suggested first label.
pub fn open_k8s(source: String, ip: String, port: u16, label: String, name: String) {
    let shown = source.clone();
    start(source, name, move || {
        let r = crate::client::request(
            "POST",
            "/kiri/k8s/forward",
            Some(&json!({"ip": ip, "port": port})),
        )?;
        if r.status == 404 {
            return Err(std::io::Error::other(
                "this Kiri engine can't reach Kubernetes services yet; restart Kiri",
            ));
        }
        if !r.ok() {
            return Err(std::io::Error::other(r.error()));
        }
        let mac = r.json()["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .ok_or_else(|| std::io::Error::other("the engine gave no port"))?;
        Ok((
            share::Target {
                addr: format!("127.0.0.1:{mac}"),
                label,
                display: shown,
            },
            Some(mac),
        ))
    });
}

/// Closes the engine's forward for a Kubernetes share.
fn unforward(port: Option<u16>) {
    if let Some(p) = port {
        std::thread::spawn(move || {
            let _ = crate::client::request("DELETE", &format!("/kiri/k8s/forward/{p}"), None);
        });
    }
}

fn start(
    source: String,
    name: String,
    resolve: impl FnOnce() -> std::io::Result<(share::Target, Option<u16>)> + Send + 'static,
) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = Some(name.trim().to_owned()).filter(|n| !n.is_empty());
    std::thread::spawn(move || {
        let (resolved, forward) = match resolve() {
            Ok(t) => t,
            Err(e) => return emit(json!({"type": "share", "id": id, "source": source, "target": source, "state": "ended", "reason": e.to_string(), "error": true}).to_string()),
        };
        let shown = resolved.display.clone();
        with_links(|m| {
            m.insert(
                id,
                Link {
                    source: source.clone(),
                    target: shown.clone(),
                    name: name.clone(),
                    url: None,
                    state: "connecting",
                    requests: 0,
                    bytes_in: 0,
                    bytes_out: 0,
                    handle: None,
                    forward,
                },
            )
        });
        emit(json!({"type": "share", "id": id, "source": source, "target": shown, "name": name, "state": "connecting"}).to_string());
        let started = share::start(
            share::Options {
                target: resolved,
                name,
                app: true,
            },
            move |ev| {
                if let share::Event::Request(r) = &ev {
                    for (n, add) in SERVED.iter().zip([1, r.bytes_in, r.bytes_out]) {
                        n.fetch_add(add, Ordering::Relaxed);
                    }
                    with_links(|m| {
                        if let Some(l) = m.get_mut(&id) {
                            l.requests += 1;
                            l.bytes_in += r.bytes_in;
                            l.bytes_out += r.bytes_out;
                        }
                    });
                }
                let v = event(id, ev);
                with_links(|m| match v["state"].as_str() {
                    Some("ended") => {
                        unforward(m.remove(&id).and_then(|l| l.forward));
                    }
                    Some(s) => {
                        if let Some(l) = m.get_mut(&id) {
                            l.state = match s {
                                "live" => "live",
                                "reconnecting" => "reconnecting",
                                _ => "connecting",
                            };
                            if let Some(u) = v["url"].as_str() {
                                l.url = Some(u.to_owned());
                            }
                        }
                    }
                    None => {}
                });
                emit(v.to_string());
            },
        );
        match started {
            // Unless it already ended, keep the tunnel so Stop can close it.
            Ok(h) => {
                let leftover = with_links(|m| match m.get_mut(&id) {
                    Some(l) => {
                        l.handle = Some(h);
                        None
                    }
                    None => Some(h),
                });
                if let Some(h) = leftover {
                    h.stop();
                }
            }
            Err(e) => {
                unforward(with_links(|m| m.remove(&id)).and_then(|l| l.forward));
                emit(json!({"type": "share", "id": id, "state": "ended", "reason": e.to_string(), "error": true}).to_string());
            }
        }
    });
}

/// Closes a link; its last event is `ended`.
pub fn close(id: u64) {
    let handle = with_links(|m| m.get_mut(&id).and_then(|l| l.handle.take()));
    if let Some(h) = handle {
        std::thread::spawn(move || h.stop());
    }
}

/// Kiri's Stop: every share this window opened closes, and so does every
/// `kiri share` on this computer (the account's tunnels with this machine's
/// id), so nothing stays public while Kiri is off.
pub fn close_all() {
    let handles: Vec<share::Handle> =
        with_links(|m| m.values_mut().filter_map(|l| l.handle.take()).collect());
    std::thread::spawn(move || {
        for h in handles {
            h.stop();
        }
        let Ok(live) = share::tunnels() else { return };
        let device = share::load().map(|c| c.device).unwrap_or_default();
        let machine = share::machine_id(&device);
        for t in live.items.iter().filter(|t| {
            (!t.machine.is_empty() && t.machine == machine)
                || (!t.device.is_empty() && t.device == device)
        }) {
            if let Err(e) = share::close_tunnel(&t.name) {
                emit(super::notice(
                    "error",
                    &format!("Couldn't stop {}: {e}", t.name),
                ));
            }
        }
        tunnels();
        list();
    });
}

/// Opens a link's public URL in the browser: only a URL the edge gave one of
/// this app's tunnels, never one the page names.
pub fn browse(id: u64) {
    if let Some(url) =
        with_links(|m| m.get(&id).and_then(|l| l.url.clone())).filter(|u| u.starts_with("https://"))
    {
        browser(&url);
    }
}

/// Polls of the account's tunnels: one at a time, and once more if asked
/// for while one was on its way, so a change just made shows.
static POLLING: AtomicBool = AtomicBool::new(false);
static AGAIN: AtomicBool = AtomicBool::new(false);
/// The last list's links by tunnel name, the only ones `browse_tunnel` opens.
static LISTED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Every live tunnel on the account, whichever computer runs it, as
/// `{"type":"sharetunnels", "items", "limit"}`. Each item says where it
/// runs (see [`tagged`]); the page colours it by that.
pub fn tunnels() {
    AGAIN.store(true, Ordering::SeqCst);
    if POLLING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        loop {
            AGAIN.store(false, Ordering::SeqCst);
            emit(tunnels_event(share::tunnels()).to_string());
            POLLING.store(false, Ordering::SeqCst);
            if !AGAIN.load(Ordering::SeqCst) || POLLING.swap(true, Ordering::SeqCst) {
                break;
            }
        }
    });
}

/// What each `kiri share` on this computer has served (share.rs's traffic
/// files): `{"type":"sharetraffic", "items": [{url, requests, in, out}]}`.
/// Local and cheap, so the page asks every couple of seconds on Share.
pub fn traffic() {
    let items: Vec<Value> = share::traffic()
        .into_iter()
        .map(|(url, [r, i, o])| json!({"url": url, "requests": r, "in": i, "out": o}))
        .collect();
    emit(json!({"type": "sharetraffic", "items": items}).to_string());
}

fn tunnels_event(r: std::io::Result<share::LiveTunnels>) -> Value {
    match r {
        Ok(list) => {
            *LISTED.lock().unwrap() = list
                .items
                .iter()
                .map(|t| (t.name.clone(), t.url.clone()))
                .collect();
            let device = share::load().map(|c| c.device).unwrap_or_default();
            let machine = share::machine_id(&device);
            let own: Vec<String> =
                with_links(|m| m.values().filter_map(|l| l.url.clone()).collect());
            let items: Vec<Value> = list
                .items
                .iter()
                .map(|t| tagged(t, &machine, &device, &own))
                .collect();
            json!({"type": "sharetunnels", "items": items, "limit": list.limit})
        }
        // An edge from before the list: the page shows its own shares only.
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            json!({"type": "sharetunnels", "unsupported": true})
        }
        Err(e) => json!({"type": "sharetunnels", "error": e.to_string()}),
    }
}

/// A tunnel as the page shows it. `mine`: one of this window's links.
/// `where`: `here` when it runs on this computer (the same machine id, or
/// the same device for clients older than machine ids), `external` when
/// another computer on the account runs it.
fn tagged(t: &share::LiveTunnel, machine: &str, device: &str, own: &[String]) -> Value {
    let mine = !t.url.is_empty() && own.contains(&t.url);
    let here = mine
        || (!t.machine.is_empty() && t.machine == machine)
        || (!t.device.is_empty() && t.device == device);
    json!({
        "name": t.name, "label": t.label, "url": t.url, "host": t.host, "kind": if t.kind == "app" { "app" } else { "cli" }, "program": program(t),
        "target": t.target, "started": t.started, "mine": mine, "where": if here { "here" } else { "external" },
    })
}

/// What opened a tunnel, to name it by: `kiri share`, or the app's name
/// without its version (`Kiri Dev`).
fn program(t: &share::LiveTunnel) -> String {
    if t.kind != "app" {
        return "kiri share".into();
    }
    match t.client.trim().rsplit_once(' ') {
        Some((name, v)) if v.starts_with(|c: char| c.is_ascii_digit()) => name.to_owned(),
        _ if t.client.trim().is_empty() => "Kiri".into(),
        _ => t.client.trim().to_owned(),
    }
}

/// Opens the link of a tunnel the edge listed: never one the page names.
pub fn browse_tunnel(name: &str) {
    let url = LISTED
        .lock()
        .unwrap()
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, u)| u.clone());
    if let Some(url) = url.filter(|u| u.starts_with("https://")) {
        browser(&url);
    }
}

/// Closes one of the account's tunnels that this window didn't open: a
/// `kiri share` on this computer, or one on another computer.
pub fn stop_tunnel(name: String) {
    std::thread::spawn(move || {
        if let Err(e) = share::close_tunnel(&name) {
            emit(super::notice(
                "error",
                &format!("Couldn't stop {name}: {e}"),
            ));
        }
        tunnels();
    });
}

/// GitHub's device flow: the page shows the code, the browser opens GitHub's
/// page for it, and this polls until the person approves.
pub fn sign_in() {
    if SIGNING_IN.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let result = (|| -> std::io::Result<String> {
            let code = share::login_start()?;
            emit(
                json!({"type": "sharelogin", "code": code.user_code, "uri": code.verification_uri})
                    .to_string(),
            );
            if code.verification_uri.starts_with("https://github.com/") {
                browser(&code.verification_uri);
            }
            loop {
                if !SIGNING_IN.load(Ordering::SeqCst) {
                    return Err(std::io::Error::other("cancelled"));
                }
                match share::login_poll(&code)? {
                    share::LoginPoll::Done(login) => return Ok(login),
                    share::LoginPoll::Pending(wait) => std::thread::sleep(wait),
                }
            }
        })();
        SIGNING_IN.store(false, Ordering::SeqCst);
        match result {
            Ok(_) => emit(account().to_string()),
            Err(e) if e.to_string() == "cancelled" => emit(account().to_string()),
            Err(e) => emit(json!({"type": "sharelogin", "error": e.to_string()}).to_string()),
        }
    });
}

pub fn cancel_sign_in() {
    SIGNING_IN.store(false, Ordering::SeqCst);
}

/// Signs out and closes every link, since they ran on this account.
pub fn sign_out() {
    let handles: Vec<_> = with_links(|m| m.values_mut().filter_map(|l| l.handle.take()).collect());
    std::thread::spawn(move || {
        for h in handles {
            h.stop();
        }
        if let Err(e) = share::logout() {
            emit(json!({"type": "sharelogin", "error": e.to_string()}).to_string());
        }
        emit(account().to_string());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(url: &str, machine: &str, device: &str, kind: &str, client: &str) -> share::LiveTunnel {
        share::LiveTunnel {
            name: "x".into(),
            url: url.into(),
            machine: machine.into(),
            device: device.into(),
            kind: kind.into(),
            client: client.into(),
            ..Default::default()
        }
    }

    #[test]
    fn tunnels_are_coloured_by_computer_not_program() {
        let own = vec!["https://web-k3x9ab.runkiri.cloud".to_owned()];
        let at = |x: &share::LiveTunnel| {
            let v = tagged(x, "mac1", "dev1", &own);
            (
                v["where"].as_str().unwrap().to_owned(),
                v["mine"].as_bool().unwrap(),
            )
        };
        // This window's own link.
        assert_eq!(
            at(&t(
                "https://web-k3x9ab.runkiri.cloud",
                "mac1",
                "dev1",
                "app",
                "Kiri Dev 0.9.4"
            )),
            ("here".into(), true)
        );
        // `kiri share` on this Mac, with its own config dir: another device, the same machine.
        assert_eq!(
            at(&t(
                "https://flower-x1ab23.runkiri.cloud",
                "mac1",
                "dev9",
                "cli",
                "kiri 0.9.4"
            )),
            ("here".into(), false)
        );
        // An older CLI that sends no machine id, sharing this app's config.
        assert_eq!(
            at(&t(
                "https://old-ab12cd.runkiri.cloud",
                "",
                "dev1",
                "cli",
                "kiri 0.8.0"
            )),
            ("here".into(), false)
        );
        // The app on another computer.
        assert_eq!(
            at(&t(
                "https://api-7fq2ab.runkiri.cloud",
                "mac2",
                "dev2",
                "app",
                "Kiri Dev 0.9.4"
            )),
            ("external".into(), false)
        );
        // No machine and another device: another computer, as far as anyone can tell.
        assert_eq!(
            at(&t(
                "https://old-zz12cd.runkiri.cloud",
                "",
                "dev7",
                "cli",
                "kiri 0.8.0"
            )),
            ("external".into(), false)
        );
    }

    #[test]
    fn programs_are_named_without_their_version() {
        assert_eq!(program(&t("", "", "", "app", "Kiri Dev 0.9.4")), "Kiri Dev");
        assert_eq!(program(&t("", "", "", "app", "Kiri 1.0.0")), "Kiri");
        assert_eq!(program(&t("", "", "", "app", "")), "Kiri");
        assert_eq!(program(&t("", "", "", "cli", "kiri 0.9.4")), "kiri share");
        assert_eq!(program(&t("", "", "", "", "bench")), "kiri share");
    }

    #[test]
    fn an_edge_without_the_list_says_so() {
        let v = tunnels_event(Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no",
        )));
        assert_eq!(v["unsupported"], true);
        let v = tunnels_event(Err(std::io::Error::other("offline")));
        assert_eq!(v["error"], "offline");
    }
}
