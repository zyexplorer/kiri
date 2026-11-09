//! `kiri k8s`: a single-node Kubernetes cluster (k3s) in one privileged
//! Kiri container, as OrbStack and Docker Desktop offer. Its API server is
//! on `127.0.0.1:6443` (or the next free port), its state in the `kiri-k8s` volume, and `kubectl`
//! gets a `kiri` context in its own file (src/kubeconfig.rs), at each start, at the engine's
//! start while the node runs, and by `kiri k8s status`; `delete` takes it out.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::client;
use crate::flavor::CONTEXT;

const IMAGE: (&str, &str) = ("rancher/k3s", "v1.37.1-k3s1");
/// The node container; the window reads the cluster through it.
pub(crate) const NAME: &str = "kiri-k8s";
const VOLUME: &str = "kiri-k8s";

fn home() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))
}

/// The cluster's settings, in `~/.kiri/k8s.json`: whether its API server
/// (and, through the daemon, its LoadBalancer services) answers on every
/// interface rather than 127.0.0.1, and extra names for its certificate.
/// They are applied when the node container is created, so `kiri k8s set`
/// recreates it; the cluster's state is in the volume and stays.
#[derive(Default, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Config {
    #[serde(default)]
    pub lan: bool,
    #[serde(default)]
    pub tls_san: Vec<String>,
    /// The Mac port asked for the API server (6443 when unset). When it is
    /// taken the node takes the next free one; `port` in its container says which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// The API server's port in the node, and the Mac port it is published on by default.
pub(crate) const API_PORT: u16 = 6443;
/// How many ports after the asked-for one a start tries before giving up.
const PORT_TRIES: u16 = 20;

impl Config {
    pub(crate) fn api_port(&self) -> u16 {
        self.port.unwrap_or(API_PORT)
    }
}

/// The label that keeps the Mac port a node was made with, for while it's stopped.
const PORT_LABEL: &str = "dev.kiri.k8s.port";

/// The Mac port a node container publishes its API server on: its live
/// binding while it runs, else the one it was made with.
pub(crate) fn bound_port(info: &serde_json::Value) -> Option<u16> {
    let live = &info["NetworkSettings"]["Ports"][format!("{API_PORT}/tcp")][0]["HostPort"];
    let made = &info["Config"]["Labels"][PORT_LABEL];
    live.as_str().or(made.as_str()).and_then(|p| p.parse().ok())
}

/// A port no program on the Mac holds (another Kiri's cluster, Docker
/// Desktop's, a tunnel): nothing answers on it and it can be bound.
fn port_free(port: u16, lan: bool) -> bool {
    let local = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    if std::net::TcpStream::connect_timeout(&local, Duration::from_millis(300)).is_ok() {
        return false;
    }
    let ip = if lan { [0, 0, 0, 0] } else { [127, 0, 0, 1] };
    std::net::TcpListener::bind(std::net::SocketAddr::from((ip, port))).is_ok()
}

/// The asked-for port, or the first free one of the next few.
fn pick_port(want: u16, lan: bool) -> io::Result<u16> {
    let last = want.saturating_add(PORT_TRIES);
    (want..=last).find(|&p| port_free(p, lan)).ok_or_else(|| {
        io::Error::other(format!(
            "ports {want}–{last} are all in use on this Mac, so the cluster's API server has none to take. Choose another in Settings → Kubernetes (or `kiri k8s set --port N`), or stop what holds them, such as another Kiri's Kubernetes"
        ))
    })
}

fn valid_port(p: &str) -> Option<u16> {
    p.parse::<u16>().ok().filter(|&p| p >= 1024)
}

fn config_path() -> io::Result<PathBuf> {
    Ok(home()?.join(crate::flavor::STATE.get()).join("k8s.json"))
}

pub(crate) fn config() -> Config {
    config_path()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// An extra certificate name: an IP address or a DNS name (`*.` allowed).
fn valid_san(name: &str) -> bool {
    name.parse::<std::net::IpAddr>().is_ok()
        || (!name.is_empty()
            && name.len() <= 253
            && name.trim_start_matches("*.").split('.').all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && !l.starts_with('-')
                    && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }))
}

/// `kiri k8s set [--lan on|off] [--tls-san a,b] [--port N]`: saves the settings and, if
/// the cluster exists and they changed, recreates its node with them.
fn set(args: &[String]) -> io::Result<()> {
    let mut cfg = config();
    let before = (cfg.lan, cfg.tls_san.clone(), cfg.api_port());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--lan" => cfg.lan = matches!(it.next().map(String::as_str), Some("on" | "true" | "1")),
            "--tls-san" => {
                let list: Vec<String> = it
                    .next()
                    .map(String::as_str)
                    .unwrap_or("")
                    .split(',')
                    .map(|n| n.trim().to_owned())
                    .filter(|n| !n.is_empty())
                    .collect();
                if let Some(bad) = list.iter().find(|n| !valid_san(n)) {
                    return Err(io::Error::other(format!(
                        "not an IP address or domain name: {bad}"
                    )));
                }
                cfg.tls_san = list;
            }
            "--port" => {
                let raw = it.next().map(String::as_str).unwrap_or("");
                let Some(port) = valid_port(raw) else {
                    return Err(io::Error::other(format!(
                        "the API server port must be a number from 1024 to 65535, not `{raw}`"
                    )));
                };
                cfg.port = (port != API_PORT).then_some(port);
            }
            _ => {
                return Err(io::Error::other(
                    "usage: kiri k8s set [--lan on|off] [--tls-san NAME,…] [--port N]",
                ));
            }
        }
    }
    ensure_daemon()?;
    let node = if exists()? {
        Some(client::get(&format!("/containers/{NAME}/json"))?)
    } else {
        None
    };
    // A port asked for that the node doesn't already hold must be free now.
    let port = cfg.api_port();
    if port != before.2
        && node.as_ref().and_then(bound_port) != Some(port)
        && !port_free(port, cfg.lan)
    {
        return Err(io::Error::other(format!(
            "port {port} is in use on this Mac; choose another"
        )));
    }
    let path = config_path()?;
    std::fs::create_dir_all(path.parent().expect("has a parent"))?;
    std::fs::write(&path, serde_json::to_vec(&cfg).map_err(io::Error::other)?)?;
    println!(
        "API server on {}:{port}; extra certificate names: {}",
        if cfg.lan {
            "every interface"
        } else {
            "127.0.0.1"
        },
        if cfg.tls_san.is_empty() {
            "none".to_owned()
        } else {
            cfg.tls_san.join(", ")
        }
    );
    let Some(node) = node else { return Ok(()) };
    if (cfg.lan, cfg.tls_san.clone(), port) == before {
        return Ok(());
    }
    if node["State"]["Running"] != true {
        // A stopped node is made again with them at its next start.
        println!("they apply when the cluster next starts");
        return Ok(());
    }
    println!("recreating the node with them (the cluster's state is kept)…");
    ok(client::request(
        "DELETE",
        &format!("/containers/{NAME}?force=1"),
        None,
    ))?;
    start(false)
}

/// The label that keeps the settings a node was made with.
const CONFIG_LABEL: &str = "dev.kiri.k8s.config";

/// What of the settings a node is made with, as its label keeps them. The
/// host folders are in it too, so a node made before it had them (or before
/// one existed) is made again at its next start. `"regyaml"` marks nodes that
/// bind Settings → Network mirrors into k3s' registries.yaml.
fn made_with(cfg: &Config) -> String {
    serde_json::to_string(&(
        cfg.lan,
        &cfg.tls_san,
        cfg.api_port(),
        host_folders(),
        "regyaml",
    ))
    .unwrap_or_default()
}

/// The folders of this computer the node sees at their own paths, so a pod's
/// `hostPath` names a file here as it would with k3s on the host itself
/// (Rancher Desktop, k3s): the Mac's /Users and /Volumes, a Linux host's
/// /home, /mnt and /media. Binds are recursive, so what is mounted under
/// them when the node starts comes too.
const HOST_FOLDERS: &[&str] = if cfg!(target_os = "macos") {
    &["/Users", "/Volumes"]
} else {
    &["/home", "/mnt", "/media"]
};

fn host_folders() -> Vec<&'static str> {
    HOST_FOLDERS
        .iter()
        .copied()
        .filter(|d| std::path::Path::new(d).is_dir())
        .collect()
}

/// Host path for the k3s `registries.yaml` bind (Settings → Network mirrors).
fn registries_host_path() -> io::Result<PathBuf> {
    Ok(home()?
        .join(crate::flavor::STATE.get())
        .join("k8s-registries.yaml"))
}

/// k3s private-registry file for Docker Hub mirrors (and insecure TLS).
/// See https://docs.k3s.io/installation/private-registry
fn registries_yaml(mirrors: &[String], insecure: &[String]) -> String {
    let mut out = String::from("mirrors:\n");
    if mirrors.is_empty() {
        // A bind mount always needs a file; empty means pull Hub directly.
        out.push_str("  docker.io:\n    endpoint: []\n");
    } else {
        out.push_str("  docker.io:\n    endpoint:\n");
        for m in mirrors {
            out.push_str(&format!("      - \"{m}\"\n"));
        }
    }
    // HTTP mirrors and daemon.json insecure-registries need skip-verify.
    let mut hosts: Vec<String> = mirrors
        .iter()
        .filter(|m| m.starts_with("http://"))
        .filter_map(|m| mirror_host(m))
        .collect();
    for h in insecure {
        // configs is host-keyed; CIDRs are not usable there.
        if !h.contains('/') {
            hosts.push(h.clone());
        }
    }
    hosts.sort();
    hosts.dedup();
    if !hosts.is_empty() {
        out.push_str("configs:\n");
        for h in hosts {
            out.push_str(&format!(
                "  \"{h}\":\n    tls:\n      insecure_skip_verify: true\n"
            ));
        }
    }
    out
}

fn mirror_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = rest.split('/').next()?.trim();
    (!host.is_empty()).then(|| host.to_owned())
}

/// Writes `~/.kiri/k8s-registries.yaml` from the current daemon.json mirrors.
fn write_registries_host() -> io::Result<PathBuf> {
    let path = registries_host_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mirrors = crate::daemon::hub_mirrors();
    let insecure = crate::daemon::insecure_registries();
    let yaml = registries_yaml(&mirrors, &insecure);
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// The node's binds: its state, Hub mirrors for k3s, then the host folders.
fn binds() -> Vec<String> {
    let mut out = vec![format!("{VOLUME}:/var/lib/rancher/k3s")];
    if let Ok(p) = write_registries_host() {
        out.push(format!(
            "{}:/etc/rancher/k3s/registries.yaml:ro",
            p.display()
        ));
    }
    out.extend(host_folders().into_iter().map(|d| format!("{d}:{d}")));
    out
}

/// After Settings → Network saves mirrors: refresh the bind file and, when
/// the cluster is running, restart the node so containerd reloads them.
pub(crate) fn sync_registry_mirrors() -> io::Result<()> {
    write_registries_host()?;
    let Ok(info) = client::get(&format!("/containers/{NAME}/json")) else {
        return Ok(());
    };
    if info["State"]["Running"] != true {
        return Ok(());
    }
    // A node made before the registries bind: write the file in place, then
    // restart. With the bind, updating the host file is enough before restart.
    let bound = info["Mounts"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|m| m["Destination"].as_str() == Some("/etc/rancher/k3s/registries.yaml"));
    if !bound {
        let yaml = registries_yaml(
            &crate::daemon::hub_mirrors(),
            &crate::daemon::insecure_registries(),
        );
        let script = "set -e; d=/etc/rancher/k3s; f=$d/registries.yaml; mkdir -p $d; \
             printf '%s' \"$1\" > $d/.kiri-registries.tmp; mv $d/.kiri-registries.tmp $f";
        let (code, out) = client::exec(NAME, &["sh", "-c", script, "sh", &yaml])?;
        if code != 0 {
            return Err(io::Error::other(format!(
                "could not write registries.yaml: {}",
                out.trim()
            )));
        }
    }
    ok(client::request(
        "POST",
        &format!("/containers/{NAME}/restart?t=20"),
        None,
    ))?;
    Ok(())
}

fn ensure_daemon() -> io::Result<()> {
    if client::alive() {
        return Ok(());
    }
    Err(io::Error::other(
        "kiri daemon is not running: open Kiri.app or run `kiri daemon`",
    ))
}

fn ok(r: io::Result<client::Response>) -> io::Result<client::Response> {
    let r = r?;
    if r.ok() {
        Ok(r)
    } else {
        Err(io::Error::other(r.error()))
    }
}

pub fn run(args: &[String]) -> io::Result<()> {
    let what = args.first().map(String::as_str).unwrap_or("");
    let logged = matches!(what, "start" | "stop" | "delete" | "set");
    if logged {
        crate::diag::info!(format!("Kubernetes {what}"));
    }
    let result = run_inner(args);
    // A failure is logged with the command's error, in main.
    if logged && result.is_ok() {
        crate::diag::info!(format!("Kubernetes {what} done"));
    }
    result
}

fn run_inner(args: &[String]) -> io::Result<()> {
    let flag = |f: &str| args.iter().any(|a| a == f);
    match args.first().map(String::as_str) {
        Some("start") => start(flag("--use")),
        Some("stop") => {
            ensure_daemon()?;
            ok(client::request(
                "POST",
                &format!("/containers/{NAME}/stop"),
                None,
            ))?;
            println!("Kubernetes stopped (its state is kept; `kiri k8s start` resumes it)");
            Ok(())
        }
        Some("status") => status(),
        Some("delete") => delete(),
        Some("set") => set(&args[1..]),
        _ => Err(io::Error::other(
            "usage: kiri k8s start [--use] | stop | status | delete | set [--lan on|off] [--tls-san NAME,…] [--port N]",
        )),
    }
}

fn exists() -> io::Result<bool> {
    Ok(client::request("GET", &format!("/containers/{NAME}/json"), None)?.ok())
}

fn start(use_context: bool) -> io::Result<()> {
    ensure_daemon()?;
    let t0 = Instant::now();
    // Refresh before create/start so a new node binds the current mirrors.
    let _ = write_registries_host();
    let cfg = config();
    let want = cfg.api_port();
    // A stopped node keeps its port unless something else has taken it since;
    // then it is made again (its state is in the volume) on a free one.
    let mut moved_to = None;
    if exists()? {
        let info = client::get(&format!("/containers/{NAME}/json"))?;
        if info["State"]["Running"] != true {
            // Settings changed while it was stopped (a node from before the label: its port).
            let stale = match info["Config"]["Labels"][CONFIG_LABEL].as_str() {
                Some(made) => made != made_with(&cfg),
                None => bound_port(&info) != Some(want),
            };
            if stale {
                println!("applying the changed settings (the cluster's state is kept)…");
                ok(client::request(
                    "DELETE",
                    &format!("/containers/{NAME}?force=1"),
                    None,
                ))?;
            } else if let Some(held) = bound_port(&info).filter(|&p| !port_free(p, cfg.lan)) {
                let port = pick_port(want, cfg.lan)?;
                println!("port {held} is in use now; moving the API server to {port}…");
                ok(client::request(
                    "DELETE",
                    &format!("/containers/{NAME}?force=1"),
                    None,
                ))?;
                moved_to = Some(port);
            }
        }
    }
    if !exists()? {
        let port = match moved_to {
            Some(p) => p,
            None => pick_port(want, cfg.lan)?,
        };
        if port != want {
            println!("port {want} is in use; the API server takes {port}");
        }
        let (repo, tag) = IMAGE;
        println!("pulling {repo}:{tag}…");
        let pulled = ok(client::request(
            "POST",
            &format!("/images/create?fromImage={repo}&tag={tag}"),
            None,
        ))?;
        if let Some(e) = String::from_utf8_lossy(&pulled.body)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find_map(|v| v["error"].as_str().map(str::to_owned))
        {
            return Err(io::Error::other(e));
        }
        let mut cmd: Vec<String> = [
            "server",
            "--disable",
            "traefik",
            "--tls-san",
            "127.0.0.1",
            "--tls-san",
            "localhost",
            "--write-kubeconfig-mode",
            "644",
        ]
        .map(String::from)
        .to_vec();
        for name in cfg.tls_san.iter().filter(|n| valid_san(n)) {
            cmd.extend(["--tls-san".to_owned(), name.clone()]);
        }
        let body = json!({
            "Image": format!("{repo}:{tag}"),
            "Cmd": cmd,
            "Hostname": "kiri",
            "Labels": {"dev.kiri.k8s": "1", PORT_LABEL: port.to_string(), CONFIG_LABEL: made_with(&cfg)},
            "ExposedPorts": {"6443/tcp": {}},
            "HostConfig": {
                "Privileged": true,
                "Tmpfs": {"/run": "", "/var/run": ""},
                "Binds": binds(),
                "PortBindings": {"6443/tcp": [{"HostIp": if cfg.lan { "0.0.0.0" } else { "127.0.0.1" }, "HostPort": port.to_string()}]},
                "RestartPolicy": {"Name": "unless-stopped"},
            },
        });
        ok(client::request(
            "POST",
            &format!("/containers/create?name={NAME}"),
            Some(&body),
        ))?;
    }
    let started = client::request("POST", &format!("/containers/{NAME}/start"), None)?;
    if !started.ok() && started.status != 304 {
        return Err(io::Error::other(started.error()));
    }
    // Early, so a new cluster's CoreDNS can start with it.
    let host_names_written = host_names().unwrap_or_else(|e| {
        println!("pods may not resolve host.docker.internal: {e}");
        false
    });
    println!("waiting for the node…");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok((0, out)) = client::exec(
            NAME,
            &[
                "timeout",
                "-s",
                "KILL",
                "20",
                "kubectl",
                "get",
                "nodes",
                "--no-headers",
            ],
        ) {
            if out.split_whitespace().nth(1) == Some("Ready") {
                break;
            }
        }
        if Instant::now() > deadline {
            return Err(io::Error::other(format!(
                "the node was not ready after 3 minutes; see `docker --context kiri logs {NAME}`"
            )));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    if host_names_written {
        if let Err(e) = restart_coredns() {
            println!("pods may not resolve host.docker.internal until CoreDNS restarts: {e}");
        }
    }
    let port = bound_port(&client::get(&format!("/containers/{NAME}/json"))?).unwrap_or(API_PORT);
    kubeconfig(use_context, port)?;
    let moved = if port != want {
        format!(" ({want} was in use)")
    } else {
        String::new()
    };
    println!(
        "Kubernetes is ready in {:.0} s, API on 127.0.0.1:{port}{moved}: kubectl --context {CONTEXT} get nodes",
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

/// k3s' own kubeconfig, from the node.
fn k3s_yaml() -> io::Result<String> {
    let (code, raw) = client::exec(NAME, &["cat", "/etc/rancher/k3s/k3s.yaml"])?;
    if code != 0 || !raw.contains("server:") {
        return Err(io::Error::other("could not read the cluster's kubeconfig"));
    }
    Ok(raw)
}

/// The names every container has in /etc/hosts for the host. A pod's
/// lookups go to CoreDNS, which forwards to the node's resolver, and the
/// node is on the default bridge, which has no embedded DNS: upstream says
/// NXDOMAIN. Docker Desktop's and OrbStack's clusters answer them.
const HOST_NAMES: [&str; 3] = [
    "host.docker.internal",
    "host.kiri.internal",
    "gateway.docker.internal",
];
/// Where k3s applies manifests from, at its start and whenever one changes.
/// It is in the cluster's volume, so the file outlives the node container
/// and k3s upgrades (they replace only k3s' own manifests).
const MANIFESTS: &str = "/var/lib/rancher/k3s/server/manifests";
const HOST_MANIFEST: &str = "kiri-host-names.yaml";

/// k3s' CoreDNS imports `*.server` from the `coredns-custom` ConfigMap as
/// server blocks of their own: one for the host's names answers them with
/// the host's address (A; AAAA has no records), and only them, so
/// everything else resolves as before. A `hosts` block in a `.override`
/// would clash with the NodeHosts one k3s' Corefile already has.
fn host_manifest() -> String {
    let addr = crate::daemon::ports::HOST_ADDR;
    let zones: Vec<String> = HOST_NAMES.iter().map(|n| format!("{n}:53")).collect();
    format!(
        "# Written by Kiri at each `kiri k8s start`: pods resolve the host's names, as containers do.\n\
apiVersion: v1\n\
kind: ConfigMap\n\
metadata:\n\
\x20 name: coredns-custom\n\
\x20 namespace: kube-system\n\
data:\n\
\x20 kiri-host.server: |\n\
\x20   {zones} {{\n\
\x20       hosts {{\n\
\x20           {addr} {names}\n\
\x20       }}\n\
\x20   }}\n",
        zones = zones.join(" "),
        names = HOST_NAMES.join(" "),
    )
}

/// A CoreDNS that was running before the manifest was there doesn't see it
/// (its `reload` watches the Corefile, not what it imports), so once k3s
/// has applied the ConfigMap, CoreDNS is restarted: only when the manifest
/// was just written, so a cluster's first start with it, or a Kiri that
/// changes it.
fn restart_coredns() -> io::Result<()> {
    let script = "k='kubectl -n kube-system'; for i in $(seq 90); do \
         if $k get configmap coredns-custom -o name >/dev/null 2>&1 && $k get deployment coredns -o name >/dev/null 2>&1; then exec $k rollout restart deployment coredns; fi; sleep 1; done; \
         echo 'the coredns-custom ConfigMap was not applied in 90 s'; exit 1";
    let (code, out) = client::exec(NAME, &["timeout", "-s", "KILL", "120", "sh", "-c", script])?;
    if code != 0 {
        return Err(io::Error::other(out.trim().to_owned()));
    }
    Ok(())
}

/// Puts the host-names manifest in the node, unless it is there already as
/// it should be. Whether it was written.
fn host_names() -> io::Result<bool> {
    let script = format!(
        "set -e; d={MANIFESTS}; f=$d/{HOST_MANIFEST}; mkdir -p $d; printf '%s' \"$1\" > $d/.kiri-host-names.tmp; \
         if cmp -s $d/.kiri-host-names.tmp $f; then rm -f $d/.kiri-host-names.tmp; else mv $d/.kiri-host-names.tmp $f; echo written; fi"
    );
    let manifest = host_manifest();
    let (code, out) = client::exec(NAME, &["sh", "-c", &script, "sh", &manifest])?;
    if code != 0 {
        return Err(io::Error::other(format!(
            "could not write {MANIFESTS}/{HOST_MANIFEST}: {}",
            out.trim()
        )));
    }
    Ok(out.contains("written"))
}

/// The cluster's kubeconfig, renamed to `kiri`, at ~/.kiri/kubeconfig, and
/// its cluster, user and context put into kubectl's file (src/kubeconfig.rs),
/// with no kubectl needed: the app's `kiri k8s start` runs with launchd's
/// PATH, where none is found. Whether kubectl's file changed.
fn kubeconfig(use_context: bool, port: u16) -> io::Result<bool> {
    let ours = crate::kubeconfig::from_k3s(&k3s_yaml()?, CONTEXT.get(), port);
    let own = home()?.join(crate::flavor::STATE.get()).join("kubeconfig");
    std::fs::create_dir_all(own.parent().expect("has a parent"))?;
    std::fs::write(&own, &ours)?;
    std::fs::set_permissions(&own, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let Some(kube) = crate::kubeconfig::path() else {
        println!(
            "kubeconfig: {} (HOME is not set, so not in ~/.kube/config)",
            own.display()
        );
        return Ok(false);
    };
    let changed = crate::kubeconfig::install(&kube, &ours, CONTEXT.get(), use_context)?;
    if changed {
        println!("kubectl context `{CONTEXT}` written to {}", kube.display());
    }
    Ok(changed)
}

/// The context of a cluster that is already running, as at the engine's
/// start, which restarts the node without `kiri k8s start`: written once
/// k3s has its kubeconfig (within `wait`), and rewritten if its port or
/// certificate changed. Nothing when there is no running node.
pub(crate) fn sync(wait: Duration) -> io::Result<bool> {
    let r = client::request("GET", &format!("/containers/{NAME}/json"), None)?;
    if !r.ok() || r.json()["State"]["Running"] != true {
        return Ok(false);
    }
    // Keep the host registries file current (the bind reads it; an older
    // node without the bind gets the file written in place at next mirror save).
    let _ = write_registries_host();
    // A cluster made by an older Kiri gets it here too, at the engine's start.
    let host_names_written = host_names().unwrap_or_else(|e| {
        crate::diag::warn!("host names manifest not written", error = e);
        false
    });
    let deadline = Instant::now() + wait;
    while k3s_yaml().is_err() {
        if Instant::now() > deadline {
            return Err(io::Error::other(
                "the cluster's kubeconfig was not there in time",
            ));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if host_names_written {
        if let Err(e) = restart_coredns() {
            crate::diag::warn!("CoreDNS not restarted for the host names", error = e);
        }
    }
    let port = bound_port(&client::get(&format!("/containers/{NAME}/json"))?).unwrap_or(API_PORT);
    kubeconfig(false, port)
}

/// The engine's start: a node it restarts gets its kubectl context, in
/// the background. A daemon running as root (a Linux host's systemd
/// service) has root's home, not the user's, and leaves it to `kiri k8s`.
pub(crate) fn sync_at_start() {
    #[cfg(target_os = "linux")]
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("kiri-k8s-context".into())
        .spawn(|| {
            for _ in 0..50 {
                if client::alive() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            match sync(Duration::from_secs(180)) {
                Ok(true) => crate::diag::info!(format!("kubectl context `{CONTEXT}` written")),
                Ok(false) => {}
                Err(e) => crate::diag::warn!("kubectl context not written", error = e),
            }
        });
}

/// Takes the cluster's cluster, context and user out of kubectl's file.
pub(crate) fn forget() {
    let Some(kube) = crate::kubeconfig::path() else {
        return;
    };
    match crate::kubeconfig::uninstall(&kube, CONTEXT.get()) {
        Ok(true) => println!(
            "removed kubectl context `{CONTEXT}` from {}",
            kube.display()
        ),
        Ok(false) => {}
        Err(e) => println!("could not remove kubectl context `{CONTEXT}` ({e})"),
    }
}

fn status() -> io::Result<()> {
    ensure_daemon()?;
    if !exists()? {
        println!("Kubernetes: not created (kiri k8s start)");
        return Ok(());
    }
    let info = client::get(&format!("/containers/{NAME}/json"))?;
    let state = info["State"]["Status"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let port = bound_port(&info).unwrap_or(API_PORT);
    let want = config().api_port();
    let moved = if port != want {
        format!(" ({want} was in use)")
    } else {
        String::new()
    };
    println!("Kubernetes: {state}, API on 127.0.0.1:{port}{moved}");
    if state == "running" {
        // A context missing or out of date (written by an older Kiri, or not at all) is put right.
        if let Err(e) = sync(Duration::ZERO) {
            println!("kubectl context `{CONTEXT}` not written: {e}");
        }
        if let Ok((_, out)) = client::exec(
            NAME,
            &[
                "timeout", "-s", "KILL", "20", "kubectl", "get", "nodes", "-o", "wide",
            ],
        ) {
            print!("{out}");
        }
    }
    Ok(())
}

fn delete() -> io::Result<()> {
    ensure_daemon()?;
    let _ = client::request("DELETE", &format!("/containers/{NAME}?force=1"), None)?;
    let _ = client::request("DELETE", &format!("/volumes/{VOLUME}"), None)?;
    let _ = std::fs::remove_file(home()?.join(crate::flavor::STATE.get()).join("kubeconfig"));
    forget();
    println!("Kubernetes deleted, with its state");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_port_comes_from_the_binding_then_the_label() {
        let running = serde_json::json!({"NetworkSettings": {"Ports": {"6443/tcp": [{"HostIp": "127.0.0.1", "HostPort": "6444"}]}}, "Config": {"Labels": {"dev.kiri.k8s.port": "6450"}}});
        assert_eq!(super::bound_port(&running), Some(6444));
        let stopped = serde_json::json!({"NetworkSettings": {"Ports": {}}, "Config": {"Labels": {"dev.kiri.k8s.port": "6450"}}});
        assert_eq!(super::bound_port(&stopped), Some(6450));
        assert_eq!(super::bound_port(&serde_json::json!({})), None);
    }

    #[test]
    fn the_node_sees_the_host_folders_at_their_own_paths() {
        let b = super::binds();
        assert_eq!(b[0], "kiri-k8s:/var/lib/rancher/k3s");
        assert!(
            b.iter()
                .any(|x| x.ends_with(":/etc/rancher/k3s/registries.yaml:ro")),
            "Hub mirrors bind missing: {b:?}"
        );
        let home = if cfg!(target_os = "macos") {
            "/Users"
        } else {
            "/home"
        };
        assert!(b.contains(&format!("{home}:{home}")), "{b:?}");
        // A node made before the folders were bound is made again.
        let old = serde_json::to_string(&(false, Vec::<String>::new(), 6443u16)).unwrap();
        assert_ne!(super::made_with(&super::Config::default()), old);
    }

    #[test]
    fn registries_yaml_lists_hub_mirrors() {
        let y = super::registries_yaml(
            &[
                "https://docker.m.daocloud.io".into(),
                "http://mirror.local:5000".into(),
            ],
            &["harbor.example:9090".into(), "10.0.0.0/8".into()],
        );
        assert!(y.contains("mirrors:\n  docker.io:\n    endpoint:\n"), "{y}");
        assert!(y.contains("      - \"https://docker.m.daocloud.io\"\n"), "{y}");
        assert!(y.contains("      - \"http://mirror.local:5000\"\n"), "{y}");
        assert!(
            y.contains("configs:\n  \"harbor.example:9090\":\n    tls:\n      insecure_skip_verify: true\n"),
            "{y}"
        );
        assert!(
            y.contains("  \"mirror.local:5000\":\n    tls:\n      insecure_skip_verify: true\n"),
            "{y}"
        );
        assert!(!y.contains("10.0.0.0/8"), "CIDRs are not configs hosts: {y}");
        let empty = super::registries_yaml(&[], &[]);
        assert!(empty.contains("endpoint: []\n"), "{empty}");
    }

    #[test]
    fn api_ports_are_unprivileged() {
        assert_eq!(super::valid_port("6444"), Some(6444));
        for bad in ["80", "1023", "65536", "", "x", "-1"] {
            assert_eq!(super::valid_port(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_busy_port_moves_the_api_to_the_next_free_one() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        assert!(!super::port_free(port, false));
        let got = super::pick_port(port, false).unwrap();
        assert!(got > port && got <= port + super::PORT_TRIES, "{got}");
    }

    #[test]
    fn pods_resolve_the_host_names_in_a_server_block_of_their_own() {
        let m = super::host_manifest();
        assert!(
            m.contains("\n  name: coredns-custom\n  namespace: kube-system\n"),
            "{m}"
        );
        assert!(m.contains("\n  kiri-host.server: |\n    host.docker.internal:53 host.kiri.internal:53 gateway.docker.internal:53 {\n"), "{m}");
        assert!(m.contains(&format!("\n            {} host.docker.internal host.kiri.internal gateway.docker.internal\n", crate::daemon::ports::HOST_ADDR)), "{m}");
        // No `fallthrough`: the block is for these names only, and nothing else reaches it.
        assert!(!m.contains("fallthrough") && !m.contains('\t'), "{m}");
    }

    #[test]
    fn certificate_names_are_addresses_or_domains() {
        for ok in [
            "192.168.1.20",
            "::1",
            "my-mac.local",
            "*.example.com",
            "k8s",
        ] {
            assert!(super::valid_san(ok), "{ok}");
        }
        for bad in ["", "a b", "-x.com", "a..b", "x/y", "--flag"] {
            assert!(!super::valid_san(bad), "{bad}");
        }
    }
}
