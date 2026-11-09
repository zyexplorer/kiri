//! Moving to Kiri from OrbStack or Docker Desktop: images, volumes and
//! containers, copied through the docker CLI so each engine speaks its own
//! API. The source is only read: `docker save` for images, `inspect` for
//! containers, and for a volume a short-lived helper container that mounts
//! it read-only and is removed after (that keeps file owners, which a copy
//! through the Finder would lose). Containers arrive stopped.

use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::client;

/// The engines Kiri can import from: docker context name, label.
pub const SOURCES: [(&str, &str); 2] = [
    ("orbstack", "OrbStack"),
    ("desktop-linux", "Docker Desktop"),
];

fn docker() -> io::Result<PathBuf> {
    client::docker_cli()
        .ok_or_else(|| io::Error::other("no docker CLI found (Kiri.app bundles one)"))
}

fn known(source: &str) -> io::Result<&'static str> {
    SOURCES
        .iter()
        .find(|(c, _)| *c == source)
        .map(|(c, _)| *c)
        .ok_or_else(|| io::Error::other(format!("unknown source {source}")))
}

/// `docker --context ctx <args>`, its output as JSON lines.
fn json_lines(ctx: &str, args: &[&str]) -> io::Result<Vec<Value>> {
    let out = Command::new(docker()?)
        .arg("--context")
        .arg(ctx)
        .args(args)
        .stderr(Stdio::piped())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

/// Which sources are installed and answering.
pub fn sources() -> Vec<Value> {
    SOURCES
        .iter()
        .map(|(ctx, label)| {
            let running = docker().is_ok_and(|d| {
                Command::new(d)
                    .args([
                        "--context",
                        ctx,
                        "version",
                        "--format",
                        "{{.Server.Version}}",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            });
            json!({"id": ctx, "label": label, "running": running})
        })
        .collect()
}

/// A 64-hex name is an anonymous volume: not worth moving by default.
fn anonymous(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What the source has, and which of it Kiri already has.
pub fn inventory(source: &str) -> io::Result<Value> {
    let ctx = known(source)?;
    let mine = |args: &[&str], key: &str| -> Vec<String> {
        json_lines(crate::flavor::CONTEXT.get(), args)
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v[key].as_str().map(str::to_owned))
            .collect()
    };
    let kiri_images: Vec<String> = json_lines(
        crate::flavor::CONTEXT.get(),
        &["image", "ls", "--format", "{{json .}}"],
    )
    .unwrap_or_default()
    .iter()
    .map(|v| {
        format!(
            "{}:{}",
            v["Repository"].as_str().unwrap_or(""),
            v["Tag"].as_str().unwrap_or("")
        )
    })
    .collect();
    let kiri_volumes = mine(&["volume", "ls", "--format", "{{json .}}"], "Name");
    let kiri_containers = mine(&["ps", "-a", "--format", "{{json .}}"], "Names");
    let images: Vec<Value> = json_lines(ctx, &["image", "ls", "--format", "{{json .}}"])?
        .into_iter()
        .filter(|v| v["Repository"] != "<none>" && v["Tag"] != "<none>")
        .map(|v| {
            let name = format!(
                "{}:{}",
                v["Repository"].as_str().unwrap_or(""),
                v["Tag"].as_str().unwrap_or("")
            );
            json!({"name": name, "size": v["Size"], "have": kiri_images.contains(&name)})
        })
        .collect();
    let volumes: Vec<Value> = json_lines(ctx, &["volume", "ls", "--format", "{{json .}}"])?
        .into_iter()
        .filter_map(|v| v["Name"].as_str().map(str::to_owned))
        .map(|name| json!({"name": name, "anonymous": anonymous(&name), "have": kiri_volumes.contains(&name)}))
        .collect();
    let containers: Vec<Value> = json_lines(ctx, &["ps", "-a", "--format", "{{json .}}"])?
        .into_iter()
        .map(|v| {
            let name = v["Names"].as_str().unwrap_or("").to_owned();
            json!({"name": name, "image": v["Image"], "state": v["State"], "have": kiri_containers.contains(&name)})
        })
        .collect();
    Ok(json!({"source": ctx, "images": images, "volumes": volumes, "containers": containers}))
}

/// `docker --context src save IMAGE | docker --context kiri load`.
pub fn image(source: &str, name: &str) -> io::Result<()> {
    let ctx = known(source)?;
    let d = docker()?;
    let mut save = Command::new(&d)
        .args(["--context", ctx, "save", name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = save
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no output from docker save"))?;
    let load = Command::new(&d)
        .args(["--context", crate::flavor::CONTEXT.get(), "load", "-q"])
        .stdin(out)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    let saved = save.wait_with_output()?;
    if !saved.status.success() {
        return Err(io::Error::other(format!(
            "{ctx}: {}",
            String::from_utf8_lossy(&saved.stderr).trim()
        )));
    }
    if !load.status.success() {
        return Err(io::Error::other(format!(
            "kiri: {}",
            String::from_utf8_lossy(&load.stderr).trim()
        )));
    }
    Ok(())
}

/// An image with `tar` that both engines have, for the volume helpers.
fn helper_image(ctx: &str) -> io::Result<&'static str> {
    let has = |c: &str, img: &str| {
        Command::new(docker().ok()?)
            .args(["--context", c, "image", "inspect", img])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .map(|s| s.success())
    };
    for img in [
        "alpine:latest",
        "busybox:latest",
        "alpine:3.22",
        "debian:trixie",
        "ubuntu:24.04",
    ] {
        if has(ctx, img) == Some(true) {
            if has(crate::flavor::CONTEXT.get(), img) != Some(true) {
                image(ctx, img)?;
            }
            return Ok(img);
        }
    }
    Err(io::Error::other(format!(
        "{ctx} has no alpine or busybox image to read volumes with; pull alpine there, or import the volume by hand"
    )))
}

/// A volume's files, owners kept: tar in a read-only helper on the source,
/// untar in a helper on Kiri. Both helpers remove themselves.
pub fn volume(source: &str, name: &str) -> io::Result<()> {
    let ctx = known(source)?;
    let d = docker()?;
    let img = helper_image(ctx)?;
    let _ = Command::new(&d)
        .args([
            "--context",
            crate::flavor::CONTEXT.get(),
            "volume",
            "create",
            name,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let src_mount = format!("{name}:/from:ro");
    let dst_mount = format!("{name}:/to");
    let mut read = Command::new(&d)
        .args([
            "--context",
            ctx,
            "run",
            "--rm",
            "--network",
            "none",
            "--label",
            "dev.kiri.import-helper=1",
            "-v",
            &src_mount,
            img,
            "tar",
            "-C",
            "/from",
            "-cf",
            "-",
            ".",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = read
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no output from the source helper"))?;
    let write = Command::new(&d)
        .args([
            "--context",
            crate::flavor::CONTEXT.get(),
            "run",
            "--rm",
            "-i",
            "--network",
            "none",
            "-v",
            &dst_mount,
            img,
            "tar",
            "-C",
            "/to",
            "-xpf",
            "-",
        ])
        .stdin(out)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    let r = read.wait_with_output()?;
    if !r.status.success() {
        return Err(io::Error::other(format!(
            "{ctx}: {}",
            String::from_utf8_lossy(&r.stderr).trim()
        )));
    }
    if !write.status.success() {
        return Err(io::Error::other(format!(
            "kiri: {}",
            String::from_utf8_lossy(&write.stderr).trim()
        )));
    }
    Ok(())
}

/// The container, made again in Kiri from the source's `inspect`, stopped:
/// command, environment, labels, ports, mounts, restart policy and its
/// networks (made in Kiri first if missing). Its image must be in Kiri.
pub fn container(source: &str, name: &str) -> io::Result<()> {
    let ctx = known(source)?;
    let info = json_lines(
        ctx,
        &[
            "inspect",
            "--type",
            "container",
            "--format",
            "{{json .}}",
            name,
        ],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| io::Error::other(format!("{name}: not found in {ctx}")))?;
    let cfg = &info["Config"];
    let image = cfg["Image"].as_str().unwrap_or("").to_owned();
    if json_lines(
        crate::flavor::CONTEXT.get(),
        &["image", "inspect", "--format", "{{json .Id}}", &image],
    )
    .map_or(true, |v| v.is_empty())
    {
        self::image(source, &image)?;
    }
    let mut networks = serde_json::Map::new();
    let mode = info["HostConfig"]["NetworkMode"]
        .as_str()
        .unwrap_or("default")
        .to_owned();
    for (net, ep) in info["NetworkSettings"]["Networks"]
        .as_object()
        .into_iter()
        .flatten()
    {
        if !["bridge", "host", "none", "default"].contains(&net.as_str()) {
            let _ = Command::new(docker()?)
                .args([
                    "--context",
                    crate::flavor::CONTEXT.get(),
                    "network",
                    "create",
                    net,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // The source's own short ID is no name in Kiri.
        let short = info["Id"]
            .as_str()
            .unwrap_or("")
            .get(..12)
            .unwrap_or("")
            .to_owned();
        let aliases: Vec<&Value> = ep["Aliases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a.as_str() != Some(short.as_str()))
            .collect();
        networks.insert(net.clone(), json!({"Aliases": aliases}));
    }
    let hc = &info["HostConfig"];
    let mut host = json!({
        "Binds": hc["Binds"], "PortBindings": hc["PortBindings"], "RestartPolicy": hc["RestartPolicy"], "Mounts": hc["Mounts"],
        "NetworkMode": mode, "Privileged": hc["Privileged"], "CapAdd": hc["CapAdd"], "CapDrop": hc["CapDrop"], "ExtraHosts": hc["ExtraHosts"],
        "Tmpfs": hc["Tmpfs"], "ReadonlyRootfs": hc["ReadonlyRootfs"], "SecurityOpt": hc["SecurityOpt"], "Memory": hc["Memory"],
        "NanoCpus": hc["NanoCpus"], "ShmSize": hc["ShmSize"], "PublishAllPorts": hc["PublishAllPorts"],
    });
    if let Some(o) = host.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    let body = json!({
        "Image": image, "Cmd": cfg["Cmd"], "Entrypoint": cfg["Entrypoint"], "Env": cfg["Env"], "Labels": cfg["Labels"],
        "WorkingDir": cfg["WorkingDir"], "User": cfg["User"], "ExposedPorts": cfg["ExposedPorts"], "Tty": cfg["Tty"],
        "OpenStdin": cfg["OpenStdin"], "StopSignal": cfg["StopSignal"], "Hostname": cfg["Hostname"],
        "HostConfig": host, "NetworkingConfig": {"EndpointsConfig": networks},
    });
    let r = client::request(
        "POST",
        &format!("/containers/create?name={name}"),
        Some(&body),
    )?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    Ok(())
}

/// `kiri import [orbstack|desktop-linux] [--list | image|volume|container NAME]`:
/// one item, or every image, named volume and container Kiri lacks.
pub fn run(args: &[String]) -> io::Result<()> {
    let words: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect();
    let source = words.first().copied().unwrap_or("orbstack");
    if let [_, kind, name] = words[..] {
        let r = match kind {
            "image" => image(source, name),
            "volume" => volume(source, name),
            "container" => container(source, name),
            _ => return Err(io::Error::other("import image, volume or container NAME")),
        };
        return r.map(|()| println!("{kind} {name}: done"));
    }
    let inv = inventory(source)?;
    let list = |k: &str| inv[k].as_array().cloned().unwrap_or_default();
    if args.iter().any(|a| a == "--list") {
        for k in ["images", "volumes", "containers"] {
            println!("{k}:");
            for v in list(k) {
                println!(
                    "  {}{}",
                    v["name"].as_str().unwrap_or(""),
                    if v["have"] == true {
                        "  (already in Kiri)"
                    } else {
                        ""
                    }
                );
            }
        }
        return Ok(());
    }
    let mut failed = 0;
    let mut step = |what: &str, name: &str, r: io::Result<()>| match r {
        Ok(()) => println!("{what} {name}: done"),
        Err(e) => {
            failed += 1;
            println!("{what} {name}: {e}");
        }
    };
    for v in list("images").iter().filter(|v| v["have"] != true) {
        let n = v["name"].as_str().unwrap_or("");
        step("image", n, image(source, n));
    }
    for v in list("volumes")
        .iter()
        .filter(|v| v["have"] != true && v["anonymous"] != true)
    {
        let n = v["name"].as_str().unwrap_or("");
        step("volume", n, volume(source, n));
    }
    for v in list("containers").iter().filter(|v| v["have"] != true) {
        let n = v["name"].as_str().unwrap_or("");
        step("container", n, container(source, n));
    }
    if failed > 0 {
        Err(io::Error::other(format!(
            "{failed} items could not be imported"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn anonymous_volumes() {
        assert!(super::anonymous(&"a1".repeat(32)));
        assert!(!super::anonymous("pgdata"));
    }
}
