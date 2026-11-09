//! `kiri bench`: the same container lifecycle on Kiri and on OrbStack.
//!
//! For each run: create a container from the image, run `true`, wait for it
//! to exit and be removed. Measured three ways:
//!
//! - Kiri, through its engine (host → vsock → guest agent), VM already up.
//! - OrbStack, through the Docker Engine API on its socket: create, wait for
//!   removal, start. No CLI in the path, so engine against engine.
//! - OrbStack, through `docker run --rm`, which is what people actually type.
//! - Docker Desktop, the same two ways, when it is running.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kiri_image::Store;
use kiri_proto::run_flags;

use crate::guest::{BootOptions, Guest, RunRequest};
use crate::run::{ensure_layers, image, layers_top_down, new_id};

const USAGE: &str = "usage: kiri bench [--runs N] [--image IMAGE] [--network none|bridge] [--only kiri|orbstack|docker]";

struct Opts {
    runs: usize,
    image: String,
    bridge: bool,
    only: Option<String>,
}

/// A Docker-compatible engine to compare against: its socket under $HOME
/// and the docker CLI context that points at it.
const ENGINES: [(&str, &str, &str); 3] = [
    ("Kiri daemon", ".kiri/run/docker.sock", "kiri"),
    ("OrbStack", ".orbstack/run/docker.sock", "orbstack"),
    ("Docker Desktop", ".docker/run/docker.sock", "desktop-linux"),
];

pub fn run(args: &[String]) -> io::Result<()> {
    let mut o = Opts {
        runs: 50,
        image: "alpine:latest".into(),
        bridge: false,
        only: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, USAGE);
        match a.as_str() {
            "--runs" => o.runs = it.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?,
            "--image" => o.image = it.next().cloned().ok_or_else(bad)?,
            "--network" => o.bridge = it.next().ok_or_else(bad)? == "bridge",
            "--only" => o.only = Some(it.next().ok_or_else(bad)?.to_ascii_lowercase()),
            _ => return Err(bad()),
        }
    }
    let warmup = 5;
    println!(
        "Container lifecycle: {} → `true` → exit → removed, {} runs (+{warmup} warm-up)\n",
        o.image, o.runs
    );

    let wanted = |name: &str| {
        o.only
            .as_deref()
            .is_none_or(|w| name.to_ascii_lowercase().starts_with(w))
    };
    let mut rows: Vec<(String, Vec<Duration>)> = Vec::new();
    if o.bridge {
        // The engine-only path has no networking; the daemon rows below cover Kiri.
    } else if wanted("kiri") {
        // The daemon holds the data disk; a second VM may not open it.
        let daemon_up = crate::daemon::socket_path().is_ok_and(|p| UnixStream::connect(p).is_ok());
        if daemon_up {
            println!(
                "  Kiri engine-only row skipped: the daemon is running (measured through it below)"
            );
        } else {
            rows.push((
                "Kiri — engine (vsock to guest agent)".into(),
                kiri(&o, warmup)?,
            ));
        }
    }
    let net = if o.bridge { "bridge" } else { "none" };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    for (name, socket, context) in ENGINES {
        if !wanted(name) {
            continue;
        }
        let socket = home.join(socket);
        if UnixStream::connect(&socket).is_err() {
            println!("  {name}: not running, skipped");
            continue;
        }
        eprintln!("  measuring {name}…");
        rows.push((
            format!("{name} — Docker API, --network {net}"),
            docker_api(&socket, &o, net, warmup)?,
        ));
        rows.push((
            format!("{name} — docker CLI, --network {net}"),
            docker_cli(context, &o, net, warmup)?,
        ));
    }

    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    println!("{:44} {:>9} {:>9} {:>9}", "", "p50", "p99", "mean");
    // Each engine is compared with Kiri on the same path (API with API, CLI
    // with CLI); the engine-only row stands in when the daemon was not up.
    let path_of = |n: &str| if n.contains("CLI") { "CLI" } else { "API" };
    let kiri_on = |path: &str| {
        rows.iter()
            .filter(|(n, _)| n.starts_with("Kiri") && (path_of(n) == path || n.contains("engine")))
            .map(|(_, v)| pct(v, 50))
            .next()
    };
    for (name, v) in &rows {
        let mean = v.iter().sum::<Duration>() / v.len().max(1) as u32;
        let ratio = match kiri_on(path_of(name)) {
            Some(b) if !b.is_zero() && !name.starts_with("Kiri") => {
                format!("   {:.0}× Kiri", pct(v, 50).as_secs_f64() / b.as_secs_f64())
            }
            _ => String::new(),
        };
        println!(
            "{name:44} {:>6.2} ms {:>6.2} ms {:>6.2} ms{ratio}",
            ms(pct(v, 50)),
            ms(pct(v, 99)),
            ms(mean)
        );
    }
    Ok(())
}

fn pct(v: &[Duration], p: usize) -> Duration {
    let mut s = v.to_vec();
    s.sort_unstable();
    s.get((s.len() * p / 100).min(s.len().saturating_sub(1)))
        .copied()
        .unwrap_or_default()
}

fn kiri(o: &Opts, warmup: usize) -> io::Result<Vec<Duration>> {
    let store = Store::open(Store::default_root()?)?;
    let image = image(&store, &o.image)?;
    let mut guest = Guest::boot(&BootOptions {
        data_disk: true,
        ..Default::default()
    })?;
    ensure_layers(&mut guest, &store, &image)?;
    let layers = layers_top_down(&image);
    let argv = vec!["true".to_owned()];
    let env = image.config.env.clone();
    let mut times = Vec::with_capacity(o.runs);
    let (mut setup, mut process) = (Duration::ZERO, Duration::ZERO);
    for i in 0..warmup + o.runs {
        let id = new_id();
        let req = RunRequest {
            id: &id,
            layers: &layers,
            argv: &argv,
            env: &env,
            cwd: "/",
            user: "",
            hostname: &id,
            flags: run_flags::REMOVE | run_flags::NET_NONE,
        };
        let r = guest.run(&req, |_, _| {})?;
        if r.code != 0 {
            return Err(io::Error::other(format!("container exited {}", r.code)));
        }
        if i >= warmup {
            times.push(r.total);
            setup += r.guest_setup;
            process += r.guest_run;
        }
    }
    let n = o.runs.max(1) as u32;
    println!(
        "  Kiri guest-side mean: namespaces + mounts + cgroup {:.2} ms, process lifetime {:.2} ms",
        (setup / n).as_secs_f64() * 1e3,
        (process / n).as_secs_f64() * 1e3
    );
    let _ = guest.shutdown();
    Ok(times)
}

fn docker_api(socket: &PathBuf, o: &Opts, net: &str, warmup: usize) -> io::Result<Vec<Duration>> {
    let image = o.image.clone();
    let body = format!(
        r#"{{"Image":"{image}","Cmd":["true"],"HostConfig":{{"AutoRemove":true,"NetworkMode":"{net}"}}}}"#
    );
    let mut times = Vec::with_capacity(o.runs);
    for i in 0..warmup + o.runs {
        let t = Instant::now();
        let (status, resp) = http(socket, "POST", "/v1.41/containers/create", Some(&body))?;
        if status != 201 {
            return Err(io::Error::other(format!(
                "create: HTTP {status}: {}",
                String::from_utf8_lossy(&resp)
            )));
        }
        let id = json_field(&resp, "Id").ok_or_else(|| io::Error::other("create: no Id"))?;
        // Ask to be told about removal before starting, and wait until the
        // engine has registered that (it answers with headers right away);
        // otherwise a fast container can be gone before the wait exists.
        let wait = http_send(
            socket,
            "POST",
            &format!("/v1.41/containers/{id}/wait?condition=removed"),
            None,
        )?;
        let mut wait = io::BufReader::new(wait);
        let head = read_head(&mut wait)?;
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let (started, resp) = http(
            socket,
            "POST",
            &format!("/v1.41/containers/{id}/start"),
            None,
        )?;
        if started != 204 {
            return Err(io::Error::other(format!(
                "start: HTTP {started}: {}",
                String::from_utf8_lossy(&resp)
            )));
        }
        if status == 200 {
            // The body (one chunk with the exit status) arrives once removed.
            loop {
                let mut line = String::new();
                io::BufRead::read_line(&mut wait, &mut line)?;
                let n = usize::from_str_radix(line.trim(), 16)
                    .map_err(|_| io::Error::other("wait: bad chunk"))?;
                let mut chunk = vec![0u8; n + 2];
                wait.read_exact(&mut chunk)?;
                if n == 0 {
                    break;
                }
            }
        }
        // 404 means it was already gone, which is also done.
        if status != 200 && status != 404 {
            return Err(io::Error::other(format!("wait: HTTP {status}")));
        }
        if i >= warmup {
            times.push(t.elapsed());
        }
    }
    Ok(times)
}

fn docker_cli(context: &str, o: &Opts, net: &str, warmup: usize) -> io::Result<Vec<Duration>> {
    let mut times = Vec::with_capacity(o.runs);
    for i in 0..warmup + o.runs {
        let t = Instant::now();
        let status = Command::new("docker")
            .args([
                "--context",
                context,
                "run",
                "--rm",
                "--network",
                net,
                &o.image,
                "true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(io::Error::other(format!("docker run failed: {status}")));
        }
        if i >= warmup {
            times.push(t.elapsed());
        }
    }
    Ok(times)
}

/// One HTTP/1.1 request over a Unix socket, `Connection: close`.
fn http(
    socket: &PathBuf,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> io::Result<(u16, Vec<u8>)> {
    let mut s = http_send(socket, method, path, body)?;
    http_read(&mut s)
}

fn http_send(
    socket: &PathBuf,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> io::Result<UnixStream> {
    let mut s = UnixStream::connect(socket)?;
    // A stalled engine becomes an error, not a hung benchmark.
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let body = body.unwrap_or("");
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    Ok(s)
}

/// Reads one response: headers, then the body by Content-Length or chunked
/// framing. Never waits for the server to close the connection, which
/// Docker does not always do after a streamed response.
fn http_read(s: &mut UnixStream) -> io::Result<(u16, Vec<u8>)> {
    let mut r = io::BufReader::new(s);
    let head = read_head(&mut r)?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let lower = head.to_ascii_lowercase();
    let mut body = Vec::new();
    if lower.contains("transfer-encoding: chunked") {
        loop {
            let mut line = String::new();
            io::BufRead::read_line(&mut r, &mut line)?;
            let n = usize::from_str_radix(line.trim(), 16)
                .map_err(|_| io::Error::other("bad chunk size"))?;
            let mut chunk = vec![0u8; n + 2];
            r.read_exact(&mut chunk)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    } else if let Some(len) = lower
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        body.resize(len, 0);
        r.read_exact(&mut body)?;
    }
    Ok((status, body))
}

/// Reads up to and including the blank line that ends the headers.
fn read_head(r: &mut impl io::BufRead) -> io::Result<String> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Err(io::Error::other("connection closed in headers"));
        }
        if line == "\r\n" {
            return Ok(head);
        }
        head.push_str(&line);
    }
}

/// Pulls a top-level string field out of a small JSON object.
fn json_field(json: &[u8], field: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    v.get(field)?.as_str().map(str::to_owned)
}
