//! Moving to Kiri from OrbStack or Docker Desktop: images, volumes and
//! containers, copied through the docker CLI so each engine speaks its own
//! API. The source is only read: `docker save` for images, `inspect` for
//! containers, and for a volume `docker cp` out of a container that mounts
//! it read-only, is never started and is removed after (that keeps file
//! owners, which a copy through the Finder would lose). Containers arrive
//! stopped.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::client;

/// The engines Kiri can import from: docker context name, label. Docker
/// Desktop's context has the same name on Windows; OrbStack is Mac-only.
pub const SOURCES: &[(&str, &str)] = if cfg!(windows) { &[("desktop-linux", "Docker Desktop")] } else { &[("orbstack", "OrbStack"), ("desktop-linux", "Docker Desktop")] };

// kiri-win builds this file too, where docker_cli() gives a String.
#[allow(clippy::useless_conversion)]
fn docker() -> io::Result<PathBuf> {
    client::docker_cli().map(PathBuf::from).ok_or_else(|| io::Error::other("no docker CLI found (Kiri bundles one)"))
}

/// The docker CLI, run without a console window of its own (Windows opens
/// one for each console program a windowed one starts).
fn cli() -> io::Result<Command> {
    #[allow(unused_mut)]
    let mut cmd = Command::new(docker()?);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    Ok(cmd)
}

/// The name a person knows the source by: "OrbStack".
fn label(id: &str) -> &str {
    SOURCES.iter().find(|(c, _)| *c == id).map_or(id, |(_, l)| *l)
}

fn known(source: &str) -> io::Result<&'static str> {
    SOURCES.iter().find(|(c, _)| *c == source).map(|(c, _)| *c).ok_or_else(|| io::Error::other(format!("unknown source {source}")))
}

/// `docker --context ctx <args>`, its output as JSON lines.
fn json_lines(ctx: &str, args: &[&str]) -> io::Result<Vec<Value>> {
    let out = cli()?.arg("--context").arg(ctx).args(args).stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

/// [`json_lines`] for `inspect` of several: what was found, though one of
/// them is gone (docker then fails, having printed the others).
fn json_lines_found(ctx: &str, args: &[&str]) -> io::Result<Vec<Value>> {
    let out = cli()?.arg("--context").arg(ctx).args(args).stderr(Stdio::piped()).output()?;
    let found: Vec<Value> = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    // Only "No such ..." is a container that went; any other failure is one.
    let said = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && said.lines().any(|l| !l.trim().is_empty() && !l.contains("No such")) {
        return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned()));
    }
    Ok(found)
}

/// Which sources are installed and answering.
pub fn sources() -> Vec<Value> {
    SOURCES
        .iter()
        .map(|(ctx, label)| {
            json!({"id": ctx, "label": label, "running": answers(ctx)})
        })
        .collect()
}

/// A 64-hex name is an anonymous volume: not worth moving by default.
fn anonymous(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What the source has, and which of it Kiri already has. Containers come
/// oldest first, the order they were made in, so one that joins another's
/// network (`container:`) comes after that one.
pub fn inventory(source: &str) -> io::Result<Value> {
    let ctx = known(source)?;
    let kiri = crate::flavor::CONTEXT.get();
    let mine = |args: &[&str], key: &str| -> Vec<String> {
        json_lines(kiri, args).unwrap_or_default().iter().filter_map(|v| v[key].as_str().map(str::to_owned)).collect()
    };
    let kiri_images = image_names(kiri).unwrap_or_default();
    let kiri_volumes = mine(&["volume", "ls", "--format", "{{json .}}"], "Name");
    let kiri_containers = mine(&["ps", "-a", "--format", "{{json .}}"], "Names");
    // An import's own reader, left by one that was stopped, is no one's.
    let rows: Vec<Value> = json_lines(ctx, &["ps", "-a", "--format", "{{json .}}"])?.into_iter().filter(|v| !v["Labels"].as_str().unwrap_or("").split(',').any(|l| l == HELPER)).collect();
    // The image each names, as created: `ps` shows an untagged one by id.
    // One removed since `ps` is left out, not the whole list.
    let ids: Vec<&str> = rows.iter().filter_map(|v| v["ID"].as_str()).collect();
    let inspected: Vec<Value> = match ids.is_empty() {
        true => Vec::new(),
        false => json_lines_found(ctx, &[&["container", "inspect", "--format", r#"{"id":{{json .Id}},"image":{{json .Config.Image}}}"#][..], &ids].concat())?,
    };
    let rows: Vec<Value> = rows.into_iter().filter(|v| inspected.iter().any(|i| i["id"].as_str().is_some_and(|id| v["ID"].as_str().is_some_and(|short| !short.is_empty() && id.starts_with(short))))).collect();
    let used: Vec<String> = inspected.iter().filter_map(|v| v["image"].as_str().map(str::to_owned)).collect();
    let mut images: Vec<Value> = Vec::new();
    for v in json_lines(ctx, &["image", "ls", "--digests", "--format", "{{json .}}"])? {
        let Some(name) = listed_name(&v) else { continue };
        // Only by digest: an image a container names so (#377), not every
        // one that lost its tag to a newer pull.
        if images.iter().any(|i| i["name"] == name.as_str()) || (name.contains('@') && !used.contains(&name)) {
            continue;
        }
        // Kiri lists an image pinned by digest by that reference, which
        // `image ls` can't show: it is asked for by name.
        let have = kiri_images.contains(&name) || (name.contains('@') && has(kiri, "image", &name));
        images.push(json!({"name": name, "size": v["Size"], "have": have}));
    }
    let names: Vec<String> = json_lines(ctx, &["volume", "ls", "--format", "{{json .}}"])?.into_iter().filter_map(|v| v["Name"].as_str().map(str::to_owned)).collect();
    // "Already in Kiri" only when Kiri's copy has files in and was finished:
    // an empty one, left by a container's create, or one a copy stopped
    // half-way in, is offered again. Without an image to look with, none is
    // taken as there.
    let both: Vec<String> = names.iter().filter(|n| kiri_volumes.contains(n)).cloned().collect();
    let states = match both.is_empty() {
        true => Vec::new(),
        false => match helper_in_kiri() {
            Ok(Some(img)) => kiri_states(&both, &img).unwrap_or_default(),
            _ => Vec::new(),
        },
    };
    let volumes: Vec<Value> = names
        .into_iter()
        .map(|name| {
            let have = states.iter().any(|(n, s)| *n == name && *s == Filled::Yes);
            json!({"name": name, "anonymous": anonymous(&name), "have": have})
        })
        .collect();
    let mut containers: Vec<Value> = rows
        .into_iter()
        .map(|v| {
            let name = v["Names"].as_str().unwrap_or("").to_owned();
            json!({"name": name, "image": v["Image"], "state": v["State"], "have": kiri_containers.contains(&name)})
        })
        .collect();
    containers.reverse();
    Ok(json!({"source": ctx, "images": images, "volumes": volumes, "containers": containers}))
}

/// An `image ls --digests` row's name: `repo:tag`, or `repo@digest` for an
/// image that is only pinned by digest (#377). Dangling images have neither.
fn listed_name(v: &Value) -> Option<String> {
    let s = |k: &str| v[k].as_str().unwrap_or("<none>").to_owned();
    match (s("Repository"), s("Tag"), s("Digest")) {
        (r, _, _) if r == "<none>" => None,
        (r, t, d) if t == "<none>" => d.starts_with("sha256:").then(|| format!("{r}@{d}")),
        (r, t, _) => Some(format!("{r}:{t}")),
    }
}

/// The tagged images `ctx` has, as `repo:tag`.
fn image_names(ctx: &str) -> io::Result<Vec<String>> {
    Ok(json_lines(ctx, &["image", "ls", "--format", "{{json .}}"])?
        .iter()
        .filter(|v| v["Repository"] != "<none>" && v["Tag"] != "<none>")
        .map(|v| format!("{}:{}", v["Repository"].as_str().unwrap_or(""), v["Tag"].as_str().unwrap_or("")))
        .collect())
}

/// Whether `ctx` has the image, container or volume `name`.
fn has(ctx: &str, kind: &str, name: &str) -> bool {
    cli().is_ok_and(|mut c| c.args(["--context", ctx, kind, "inspect", name]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()))
}

/// `from` piped through `retar` (in this process) into `to`, both docker
/// commands already set up; the first failure that explains the others.
fn pipe(mut from: Command, mut to: Command, from_ctx: &str, retar: impl FnOnce(&mut dyn Read, &mut dyn Write) -> io::Result<()>) -> io::Result<()> {
    let mut from = from.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut out = from.stdout.take().ok_or_else(|| io::Error::other("no output from docker"))?;
    let mut to = match to.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = from.kill();
            let _ = from.wait();
            return Err(e);
        }
    };
    let mut into = to.stdin.take().ok_or_else(|| io::Error::other("no input to docker"))?;
    // Both sides' errors are read while the copy runs: a full stderr pipe
    // would stop either one, and the copy with it.
    let drain = |e: Option<std::process::ChildStderr>| {
        std::thread::spawn(move || {
            let mut said = Vec::new();
            if let Some(mut e) = e {
                let _ = e.read_to_end(&mut said);
            }
            String::from_utf8_lossy(&said).trim().to_owned()
        })
    };
    let (from_said, to_said) = (drain(from.stderr.take()), drain(to.stderr.take()));
    let copied = retar(&mut out, &mut into);
    // A copy cut short must not reach the writer's end as a clean one.
    if copied.is_err() {
        let _ = from.kill();
    }
    drop((out, into));
    let written = to.wait()?.success();
    let read = from.wait()?.success();
    let to_said = to_said.join().unwrap_or_default();
    let from_said = from_said.join().unwrap_or_default();
    // A writer that stopped reading breaks the reader's pipe: its reason first.
    let broke = copied.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::BrokenPipe);
    if !written && (broke || read) {
        return Err(io::Error::other(format!("kiri: {to_said}")));
    }
    // The filter's own reason first; what the reader said, if anything,
    // after it.
    if let Err(e) = copied {
        let also = if from_said.is_empty() { String::new() } else { format!(" ({from_ctx}: {from_said})") };
        return Err(io::Error::other(format!("copying from {}: {e}{also}", label(from_ctx))));
    }
    if !read && !from_said.is_empty() {
        return Err(io::Error::other(format!("{from_ctx}: {from_said}")));
    }
    if !read {
        return Err(io::Error::other(format!("{from_ctx}: the copy stopped")));
    }
    if !written {
        return Err(io::Error::other(format!("kiri: {to_said}")));
    }
    Ok(())
}

/// `docker --context src save IMAGE | docker --context kiri load`. What
/// `docker save` writes for an image pinned by digest (`repo@sha256:…`)
/// has no name, so Kiri would know it only by its id and the container
/// that names it would not find it (#377): its manifest gets the name.
pub fn image(source: &str, name: &str) -> io::Result<()> {
    let ctx = known(source)?;
    let mut save = cli()?;
    save.args(["--context", ctx, "save", name]);
    let mut load = cli()?;
    load.args(["--context", crate::flavor::CONTEXT.get(), "load", "-q"]);
    let named = name.contains('@').then_some(name);
    pipe(save, load, ctx, |from, to| retar(from, to, named))
}

/// A tar number field: octal while it fits, else GNU base-256 (a first
/// byte of 0x80, then the value big-endian), which busybox tar reads too.
fn put_number(field: &mut [u8], v: u64) {
    let digits = field.len() - 1;
    if digits * 3 >= 64 || v >> (digits * 3) == 0 {
        field.copy_from_slice(format!("{v:0digits$o}\0").as_bytes());
    } else {
        field.fill(0);
        field[0] = 0x80;
        for (i, b) in field.iter_mut().skip(1).rev().enumerate() {
            *b = (v.checked_shr(8 * i as u32).unwrap_or(0) & 0xff) as u8;
        }
    }
}

/// A tar number field as written: octal, or base-256.
fn get_number(field: &[u8]) -> Option<u64> {
    if field[0] & 0x80 != 0 {
        return Some(field[1..].iter().fold(0u64, |n, b| n << 8 | u64::from(*b)));
    }
    let text = String::from_utf8_lossy(field.split(|c| *c == 0).next().unwrap_or_default()).trim().to_owned();
    if text.is_empty() { Some(0) } else { u64::from_str_radix(&text, 8).ok() }
}

/// A pax header's records (`LEN key=value\n`), each read by its length:
/// a value may hold anything, `" size="` too.
fn pax_records(data: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = data;
    while let Some(sp) = rest.iter().position(|b| *b == b' ') {
        let Some(len) = std::str::from_utf8(&rest[..sp]).ok().and_then(|l| l.parse::<usize>().ok()).filter(|l| *l > sp + 1 && *l <= rest.len()) else { break };
        let record = &rest[sp + 1..len];
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        if let Some(eq) = record.iter().position(|b| *b == b'=') {
            out.push((String::from_utf8_lossy(&record[..eq]).into_owned(), String::from_utf8_lossy(&record[eq + 1..]).into_owned()));
        }
        rest = &rest[len..];
    }
    out
}

/// Passes a tar stream from `from` to `to`, entry by entry. A pax header's
/// size, uid and gid are written into the next entry's own header as well:
/// `docker cp` puts a size of 8 GiB or more and an owner above 2097151 only
/// there, and busybox tar reads pax for names alone, so it would take a
/// big file's zeros for the archive's end and report success. With `name`,
/// each image in a `docker save` manifest.json gets that name too. The
/// stream must reach the archive's end: one cut short is an error.
fn retar(mut from: impl Read, mut to: impl Write, name: Option<&str>) -> io::Result<()> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("tar stream: {what}"));
    let text = |b: &[u8]| String::from_utf8_lossy(b.split(|c| *c == 0).next().unwrap_or_default()).trim().to_owned();
    let cut = |what: &'static str| move |e: io::Error| if e.kind() == io::ErrorKind::UnexpectedEof { bad(&format!("cut off inside {what}")) } else { e };
    let mut h = [0u8; 512];
    // What a pax header said, for the entry after it.
    let mut pax: Vec<(String, String)> = Vec::new();
    loop {
        from.read_exact(&mut h).map_err(cut("a header, before the archive's end"))?;
        if h.iter().all(|b| *b == 0) {
            to.write_all(&h)?;
            io::copy(&mut from, &mut to)?;
            return to.flush();
        }
        let mut changed = false;
        for (key, at) in [("size", 124..136), ("uid", 108..116), ("gid", 116..124)] {
            if let Some(v) = pax.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.parse::<u64>().ok()) {
                put_number(&mut h[at], v);
                changed = true;
            }
        }
        pax.clear();
        let size = get_number(&h[124..136]).ok_or_else(|| bad("a damaged header"))?;
        let padded = size.div_ceil(512) * 512;
        let manifest = name.is_some() && matches!(h[156], b'0' | 0) && text(&h[345..500]).is_empty() && text(&h[..100]).trim_start_matches("./") == "manifest.json";
        let small = manifest || h[156] == b'x';
        if small && size > 16 << 20 {
            return Err(bad("a header or manifest.json too large"));
        }
        let mut data = Vec::new();
        if small {
            data = vec![0u8; padded as usize];
            from.read_exact(&mut data).map_err(cut(if manifest { "manifest.json" } else { "a pax header" }))?;
            data.truncate(size as usize);
        }
        if h[156] == b'x' {
            pax = pax_records(&data);
        }
        if let (true, Some(name)) = (manifest, name) {
            let mut m: Value = serde_json::from_slice(&data).map_err(|e| bad(&format!("manifest.json: {e}")))?;
            for e in m.as_array_mut().into_iter().flatten().filter_map(Value::as_object_mut) {
                let mut tags = e.get("RepoTags").and_then(Value::as_array).cloned().unwrap_or_default();
                if !tags.iter().any(|t| t == name) {
                    tags.push(name.into());
                    e.insert("RepoTags".into(), tags.into());
                }
            }
            data = serde_json::to_vec(&m).map_err(io::Error::other)?;
            put_number(&mut h[124..136], data.len() as u64);
            changed = true;
        }
        if changed {
            h[148..156].fill(b' ');
            let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
            h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        }
        to.write_all(&h)?;
        if small {
            to.write_all(&data)?;
            to.write_all(&vec![0u8; data.len().div_ceil(512) * 512 - data.len()])?;
        } else if io::copy(&mut (&mut from).take(padded), &mut to)? != padded {
            return Err(bad("cut off inside a file's body"));
        }
    }
}

/// Official images with `tar` and `sh`: one of these does Kiri's half of a
/// volume copy.
const TAR_IMAGES: [&str; 4] = ["alpine", "busybox", "debian", "ubuntu"];

/// Whether a `RepoDigests` entry shows the engine pulled the image from an
/// official repository of [`TAR_IMAGES`]: Docker Hub's, or one of its two
/// public mirrors, by digest. A name or a tag says nothing (anyone can give
/// an image one), and neither does any other registry or path.
fn official(repo_digest: &str) -> bool {
    let Some((repo, digest)) = repo_digest.split_once('@') else { return false };
    let hex = digest.strip_prefix("sha256:").unwrap_or_default();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return false;
    }
    let homes = ["", "library/", "docker.io/library/", "mirror.gcr.io/library/", "public.ecr.aws/docker/library/"];
    homes.iter().any(|home| repo.strip_prefix(home).is_some_and(|base| TAR_IMAGES.contains(&base)))
}

/// The name to run Kiri's half of a volume copy by: a `repo:tag` of the
/// first of `images` (Kiri's image list: `RepoTags`, `RepoDigests`) whose
/// own record shows it was pulled under that name from an official
/// repository ([`official`]). Run by that name, it is that record.
fn pick_helper(images: &[Value]) -> Option<String> {
    let strings = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
    images.iter().find_map(|i| {
        let digests = strings(&i["RepoDigests"]);
        strings(&i["RepoTags"]).into_iter().find(|name| {
            let Some((repo, _)) = name.rsplit_once(':').filter(|(_, tag)| !tag.is_empty() && !tag.contains('/')) else { return false };
            digests.iter().any(|d| official(d) && d.split_once('@').is_some_and(|(r, _)| r == repo))
        })
    })
}

/// [`pick_helper`] of the images Kiri has, read once from its API.
fn helper_in_kiri() -> io::Result<Option<String>> {
    let r = client::request("GET", "/images/json", None)?;
    if !r.ok() {
        return Err(io::Error::other(format!("kiri: {}", r.error())));
    }
    Ok(r.json().as_array().and_then(|images| pick_helper(images)))
}

/// When Kiri last failed to fetch a helper: tried once per import, not
/// once per volume.
static PULL_FAILED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// [`helper_in_kiri`], else busybox fetched by Kiri: from the public mirror,
/// else from Docker Hub when no image of the person's is called that.
/// Nothing is taken from the source.
fn helper() -> io::Result<String> {
    if let Some(name) = helper_in_kiri()? {
        return Ok(name);
    }
    let none = || io::Error::other("Kiri needs one small official image to unpack volumes with: pull alpine or busybox in Kiri (docker pull alpine) and import again. A re-tagged or loaded copy does not count");
    let mut failed = PULL_FAILED.lock().unwrap_or_else(|e| e.into_inner());
    if failed.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60)) {
        return Err(none());
    }
    let kiri = crate::flavor::CONTEXT.get();
    let pull = |name: &str| cli().is_ok_and(|mut c| c.args(["--context", kiri, "pull", "-q", name]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()));
    if (pull("mirror.gcr.io/library/busybox") || (!has(kiri, "image", "busybox:latest") && pull("busybox")))
        && let Some(name) = helper_in_kiri()?
    {
        return Ok(name);
    }
    *failed = Some(std::time::Instant::now());
    Err(none())
}

/// The label on the containers an import makes for itself.
const HELPER: &str = "dev.kiri.import-helper=1";

/// In a Kiri volume while a copy fills it; gone once the copy is whole.
const UNFINISHED: &str = ".kiri-import-unfinished";

/// What a Kiri volume holds, for an import.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Filled {
    /// Nothing: a copy may go in.
    No,
    /// Files from a copy that did not finish: emptied and copied again.
    Unfinished,
    /// Files, there to stay.
    Yes,
}

/// How full each of `names`, volumes Kiri has, is: one helper mounts them all
/// read-only and says which have files in and which a copy left unfinished.
fn kiri_states(names: &[String], img: &str) -> io::Result<Vec<(String, Filled)>> {
    let mut cmd = cli()?;
    cmd.args(["--context", crate::flavor::CONTEXT.get(), "run", "--rm", "--network", "none", "--label", HELPER]);
    for (i, n) in names.iter().enumerate() {
        cmd.arg("-v").arg(format!("{n}:/v/{i}:ro"));
    }
    let script = format!(r#"for d in /v/*; do if [ -e "$d/{UNFINISHED}" ]; then echo "${{d#/v/}} u"; elif [ -n "$(ls -A "$d")" ]; then echo "${{d#/v/}} y"; fi; done; true"#);
    let out = cmd.args(["--pull", "never", "--entrypoint", "sh", img, "-c", &script]).stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!("kiri: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let state = said.lines().find_map(|l| l.strip_prefix(&format!("{i} "))).map_or(Filled::No, |s| if s == "u" { Filled::Unfinished } else { Filled::Yes });
            (n.clone(), state)
        })
        .collect())
}

/// The containers running (or restarting) on `ctx` with the volume `name`.
fn users(ctx: &str, name: &str) -> io::Result<Vec<String>> {
    let users = json_lines(ctx, &["ps", "--filter", &format!("volume={name}"), "--filter", "status=running", "--filter", "status=restarting", "--format", "{{json .Names}}"])?;
    Ok(users.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
}

/// Every container Kiri has with the volume `name`, in any state (a paused
/// one, one waiting on its restart policy, a stopped one all write to it
/// again): (name, whether it is an import's own helper).
fn mounting(ctx: &str, name: &str) -> io::Result<Vec<(String, bool)>> {
    let rows = json_lines(ctx, &["ps", "-a", "--filter", &format!("volume={name}"), "--format", r#"{"name":{{json .Names}},"helper":{{json (.Label "dev.kiri.import-helper")}}}"#])?;
    Ok(rows.iter().filter_map(|v| Some((v["name"].as_str()?.to_owned(), v["helper"] == "1"))).collect())
}

/// Why a volume a copy left unfinished in Kiri is not emptied while
/// containers there have it: what they wrote in it since would go.
fn in_use_in_kiri(name: &str, users: &[(String, bool)]) -> Option<String> {
    if users.is_empty() {
        return None;
    }
    let theirs: Vec<&str> = users.iter().filter(|(_, helper)| !helper).map(|(n, _)| n.as_str()).collect();
    if theirs.is_empty() {
        return Some(format!("another import of {name} is running in Kiri: let it finish, then import again if it did not"));
    }
    let them = if theirs.len() == 1 { "it" } else { "them" };
    Some(format!(
        "{name} holds an unfinished copy in Kiri, and Kiri has {} using it: importing it again would replace what is in it. Remove {them} in Kiri (or remove the volume in Kiri) and import again; importing a container again brings its volumes first",
        theirs.join(", ")
    ))
}

/// Why a volume is not copied while `users` run with it on the source
/// (#345): the copy would be a snapshot taken mid-write. The window has no
/// --force, so stopping them comes first.
fn in_use(name: &str, label: &str, users: &[String]) -> Option<String> {
    let them = if users.len() == 1 { "it" } else { "them" };
    (!users.is_empty()).then(|| {
        format!(
            "{name} is in use by {}, running on {label}: a copy taken now can be cut off mid-write. Stop {them} there and import again for an exact copy (in a terminal, kiri import --force copies it anyway)",
            users.join(", ")
        )
    })
}

/// A volume's files, owners kept.
pub fn volume(source: &str, name: &str) -> io::Result<()> {
    volume_with(source, name, false)
}

/// [`volume`]; `force` copies it even while a running container on the
/// source uses it. A volume Kiri already has files in is left as it is;
/// one a copy left unfinished is emptied and copied again.
pub fn volume_with(source: &str, name: &str, force: bool) -> io::Result<()> {
    let ctx = known(source)?;
    let kiri = crate::flavor::CONTEXT.get();
    // Reading it below would make it on the source.
    if !has(ctx, "volume", name) {
        return Err(io::Error::other(format!("{ctx}: no such volume: {name}")));
    }
    let there = has(kiri, "volume", name);
    let already = || io::Error::new(io::ErrorKind::AlreadyExists, format!("Kiri already has {name} with files in it, and it is left as it is"));
    // Asked first with an image Kiri has, before anything is fetched.
    let kiri_img = helper_in_kiri()?;
    let mut state = match (&kiri_img, there) {
        (_, false) => Some(Filled::No),
        (Some(img), true) => Some(kiri_states(&[name.to_owned()], img)?[0].1),
        (None, true) => None,
    };
    if state == Some(Filled::Yes) {
        return Err(already());
    }
    if !force && let Some(why) = in_use(name, label(ctx), &users(ctx, name)?) {
        return Err(io::Error::other(why));
    }
    let img = match kiri_img {
        Some(img) => img,
        None => helper()?,
    };
    if state.is_none() {
        state = Some(kiri_states(&[name.to_owned()], &img)?[0].1);
    }
    if state == Some(Filled::Yes) {
        return Err(already());
    }
    // Emptying it is safe only while no container in Kiri has it, in any
    // state; --force is about the source and does not change that. A helper
    // of this import that looked into it may take a moment to go.
    if state == Some(Filled::Unfinished) {
        let mut users = mounting(kiri, name)?;
        for _ in 0..10 {
            if users.is_empty() || users.iter().any(|(_, helper)| !helper) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            users = mounting(kiri, name)?;
        }
        if let Some(why) = in_use_in_kiri(name, &users) {
            return Err(io::Error::other(why));
        }
    }
    // Nothing runs on the source: a container that is made and never
    // started mounts the volume read-only (`volume-nocopy`: its image's
    // files don't go in), from any image there and never pulled, and
    // `docker cp` reads it as an archive with owners, modes and links as
    // they are. It goes after, with any anonymous volume its image made;
    // one an import that was stopped left behind goes first.
    let names = image_names(ctx)?;
    let reader_image = match names.first() {
        Some(n) => n.clone(),
        None => json_lines(ctx, &["image", "ls", "--format", "{{json .ID}}"])?
            .first()
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| io::Error::other(format!("{ctx} has no image to read volumes through; pull any image there and import again")))?,
    };
    let reader = format!("kiri-import-{name}");
    let leftover = json_lines(ctx, &["inspect", "--type", "container", "--format", "{{json .Config.Labels}}", &reader]).unwrap_or_default();
    if leftover.first().is_some_and(|l| l["dev.kiri.import-helper"] == "1") {
        let _ = cli()?.args(["--context", ctx, "rm", "-fv", &reader]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    // At a path no image can have a VOLUME of its own at or under.
    use std::hash::BuildHasher;
    let at = format!("/kiri-import-{:016x}", std::collections::hash_map::RandomState::new().hash_one(name));
    let mount = format!("type=volume,src={name},dst={at},readonly,volume-nocopy");
    let made = cli()?
        .args(["--context", ctx, "create", "--name", &reader, "--pull", "never", "--network", "none", "--label", HELPER, "--entrypoint", &at, "--mount", &mount, &reader_image])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !made.status.success() {
        return Err(io::Error::other(format!("{ctx}: {}", String::from_utf8_lossy(&made.stderr).trim())));
    }
    let copied = copy_volume(ctx, &format!("{reader}:{at}/."), name, &img);
    let _ = cli()?.args(["--context", ctx, "rm", "-v", &reader]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    copied.map_err(|e| io::Error::other(format!("{e}; {name} is unfinished in Kiri: the next import empties it and copies it again, replacing what is in it then")))
}

/// `docker cp` out of the reader on the source, through [`retar`], untarred
/// into the volume by a helper on Kiri that removes itself. The helper
/// marks the volume unfinished first (emptying what an unfinished copy
/// left), and the mark goes only once every part of the copy succeeded.
fn copy_volume(ctx: &str, from: &str, name: &str, img: &str) -> io::Result<()> {
    let kiri = crate::flavor::CONTEXT.get();
    let dst_mount = format!("{name}:/to");
    let mut read = cli()?;
    read.args(["--context", ctx, "cp", from, "-"]);
    let mut write = cli()?;
    // Emptied in one pass, all but the mark (a folder by that name counts as
    // it); anything still there after, hidden names too, stops the copy with
    // the mark left on.
    let script = format!(
        r#"set -e; cd /to; if [ -e {m} ]; then find . -mindepth 1 -maxdepth 1 ! -name {m} -exec rm -rf -- {{}} + || true; for f in .[!.]* ..?* *; do if [ "$f" != {m} ] && {{ [ -e "$f" ] || [ -L "$f" ]; }}; then echo "could not empty the volume: $f is left" >&2; exit 1; fi; done; fi; touch {m}; tar -xpf -"#,
        m = UNFINISHED
    );
    write.args(["--context", kiri, "run", "--rm", "-i", "--network", "none", "--label", HELPER, "-v", &dst_mount, "--pull", "never", "--entrypoint", "sh", img, "-c", &script]);
    pipe(read, write, ctx, |from, to| retar(from, to, None))?;
    let done = cli()?.args(["--context", kiri, "run", "--rm", "--network", "none", "--label", HELPER, "-v", &dst_mount, "--pull", "never", "--entrypoint", "rm", img, "-rf", "--", &format!("/to/{UNFINISHED}")]).stdout(Stdio::null()).stderr(Stdio::piped()).output()?;
    if !done.status.success() {
        return Err(io::Error::other(format!("kiri: {}", String::from_utf8_lossy(&done.stderr).trim())));
    }
    Ok(())
}

/// The container, made again in Kiri from the source's `inspect`, stopped.
/// One brought already as the network another joins, earlier in the same
/// run of the window, is done.
pub fn container(source: &str, name: &str) -> io::Result<()> {
    let mut brought = BROUGHT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = brought.iter().position(|(s, n)| s == source && n == name) {
        brought.remove(i);
        if has(crate::flavor::CONTEXT.get(), "container", name) {
            return Ok(());
        }
    }
    drop(brought);
    container_with(source, name, false, &[])
}

/// Containers imported as the network another joins: (source, name).
static BROUGHT: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// [`container`]: its image, its volumes with their files (named and
/// anonymous, unless Kiri has them with files already; `force` as for
/// [`volume_with`]), the container whose network it joins and its own
/// networks are brought first. `joining`: the containers waiting on this
/// one for its network, so a loop stops.
pub fn container_with(source: &str, name: &str, force: bool, joining: &[&str]) -> io::Result<()> {
    let ctx = known(source)?;
    let kiri = crate::flavor::CONTEXT.get();
    if joining.contains(&name) {
        return Err(io::Error::other(format!("{name}: the containers {} join each other's network in a loop", joining.join(", "))));
    }
    let info = json_lines(ctx, &["inspect", "--type", "container", "--format", "{{json .}}", name])?.into_iter().next().ok_or_else(|| io::Error::other(format!("{name}: not found in {ctx}")))?;
    let image = info["Config"]["Image"].as_str().unwrap_or("").to_owned();
    if !has(kiri, "image", &image) {
        self::image(source, &image)?;
    }
    // Else create would make them in Kiri, empty.
    for v in volumes_of(&info) {
        match volume_with(source, &v, force) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            r => r.map_err(|e| io::Error::other(format!("volume {v}: {e}")))?,
        }
    }
    let mut mode = info["HostConfig"]["NetworkMode"].as_str().unwrap_or("default").to_owned();
    let mut networks = serde_json::Map::new();
    if let Some(target) = mode.strip_prefix("container:") {
        // The source's id for it is no name in Kiri; its own name is, once
        // it is there.
        let target = json_lines(ctx, &["inspect", "--type", "container", "--format", "{{json .Name}}", target])?
            .first()
            .and_then(|v| v.as_str().map(|n| n.trim_start_matches('/').to_owned()))
            .ok_or_else(|| io::Error::other(format!("{name}: the container whose network it joins is not in {ctx}")))?;
        if !has(kiri, "container", &target) {
            let chain: Vec<&str> = joining.iter().copied().chain([name]).collect();
            container_with(source, &target, force, &chain)?;
            BROUGHT.lock().unwrap_or_else(|e| e.into_inner()).push((source.to_owned(), target.clone()));
        }
        mode = format!("container:{target}");
    }
    for (net, ep) in info["NetworkSettings"]["Networks"].as_object().into_iter().flatten().filter(|_| !mode.starts_with("container:")) {
        if !["bridge", "host", "none", "default"].contains(&net.as_str()) {
            let _ = cli()?.args(["--context", kiri, "network", "create", net]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        // The source's own short ID is no name in Kiri.
        let short = info["Id"].as_str().unwrap_or("").get(..12).unwrap_or("").to_owned();
        let aliases: Vec<&Value> = ep["Aliases"].as_array().into_iter().flatten().filter(|a| a.as_str() != Some(short.as_str())).collect();
        networks.insert(net.clone(), json!({"Aliases": aliases}));
    }
    let r = client::request("POST", &format!("/containers/create?name={name}"), Some(&create_body(&info, &mode, networks.into())))?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    Ok(())
}

/// The volumes a container has on the source, named and anonymous.
fn volumes_of(info: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for m in info["Mounts"].as_array().into_iter().flatten().filter(|m| m["Type"] == "volume") {
        if let Some(n) = m["Name"].as_str().filter(|n| !n.is_empty() && !names.iter().any(|x| x == n)) {
            names.push(n.to_owned());
        }
    }
    names
}

/// What Kiri's create takes of the source's `inspect`: command, environment,
/// labels, ports, mounts, restart policy, devices, DNS, healthcheck, limits
/// and the rest Kiri keeps, with `mode` its network mode in Kiri and
/// `networks` its endpoints. Anonymous volumes (`-v /path`, an image's
/// VOLUME) are named, as the copies already in Kiri are; else Kiri would make
/// new empty ones. Beside `container:` mode, what Docker and Kiri refuse
/// there is left out: the source fills some of it in itself (#375).
fn create_body(info: &Value, mode: &str, networks: Value) -> Value {
    let (cfg, hc) = (&info["Config"], &info["HostConfig"]);
    let anon: Vec<(&str, &str, bool)> = info["Mounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["Type"] == "volume")
        .filter_map(|m| Some((m["Name"].as_str().filter(|n| anonymous(n))?, m["Destination"].as_str()?, m["RW"] != false)))
        .collect();
    let mut mounts = hc["Mounts"].clone();
    for m in mounts.as_array_mut().into_iter().flatten().filter(|m| m["Type"] == "volume" && m["Source"].as_str().unwrap_or("").is_empty()) {
        if let Some((n, _, _)) = anon.iter().find(|(_, d, _)| m["Target"].as_str() == Some(d)) {
            m["Source"] = (*n).into();
        }
    }
    let mut binds = hc["Binds"].clone();
    for (n, d, rw) in &anon {
        if !mounts.as_array().into_iter().flatten().any(|m| m["Target"].as_str() == Some(d)) {
            let bind = format!("{n}:{d}{}", if *rw { "" } else { ":ro" });
            match binds.as_array_mut() {
                Some(b) => b.push(bind.into()),
                None => binds = json!([bind]),
            }
        }
    }
    let mut host = json!({
        "Binds": binds, "PortBindings": hc["PortBindings"], "RestartPolicy": hc["RestartPolicy"], "Mounts": mounts,
        "NetworkMode": mode, "Privileged": hc["Privileged"], "CapAdd": hc["CapAdd"], "CapDrop": hc["CapDrop"], "ExtraHosts": hc["ExtraHosts"],
        "Tmpfs": hc["Tmpfs"], "ReadonlyRootfs": hc["ReadonlyRootfs"], "SecurityOpt": hc["SecurityOpt"], "Memory": hc["Memory"],
        "NanoCpus": hc["NanoCpus"], "ShmSize": hc["ShmSize"], "PublishAllPorts": hc["PublishAllPorts"],
        "Devices": hc["Devices"], "DeviceCgroupRules": hc["DeviceCgroupRules"], "GroupAdd": hc["GroupAdd"], "Ulimits": hc["Ulimits"],
        "Sysctls": hc["Sysctls"], "Dns": hc["Dns"], "DnsSearch": hc["DnsSearch"], "DnsOptions": hc["DnsOptions"], "PidMode": hc["PidMode"],
        "Init": hc["Init"], "LogConfig": hc["LogConfig"], "AutoRemove": hc["AutoRemove"], "CpuShares": hc["CpuShares"],
        "CpuQuota": hc["CpuQuota"], "CpuPeriod": hc["CpuPeriod"], "PidsLimit": hc["PidsLimit"],
    });
    let mut body = json!({
        "Image": cfg["Image"], "Cmd": cfg["Cmd"], "Entrypoint": cfg["Entrypoint"], "Env": cfg["Env"], "Labels": cfg["Labels"],
        "WorkingDir": cfg["WorkingDir"], "User": cfg["User"], "ExposedPorts": cfg["ExposedPorts"], "Tty": cfg["Tty"],
        "OpenStdin": cfg["OpenStdin"], "StopSignal": cfg["StopSignal"], "StopTimeout": cfg["StopTimeout"], "Hostname": cfg["Hostname"],
        "Domainname": cfg["Domainname"], "MacAddress": cfg["MacAddress"], "Healthcheck": cfg["Healthcheck"], "Shell": cfg["Shell"],
        "NetworkingConfig": {"EndpointsConfig": networks},
    });
    if mode.starts_with("container:") {
        for k in ["Hostname", "MacAddress", "ExposedPorts"] {
            body[k] = Value::Null;
        }
        for k in ["PortBindings", "PublishAllPorts", "Dns", "ExtraHosts"] {
            host[k] = Value::Null;
        }
    }
    for v in [&mut host, &mut body] {
        if let Some(o) = v.as_object_mut() {
            o.retain(|_, v| !v.is_null());
        }
    }
    body["HostConfig"] = host;
    body
}

/// Whether `ctx`'s engine answers.
fn answers(ctx: &str) -> bool {
    cli().is_ok_and(|mut c| c.args(["--context", ctx, "version", "--format", "{{.Server.Version}}"]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()))
}

/// The engine to import from: the one named, or else the first that
/// answers. Only the engines that need it are asked (`answers` waits on
/// each), and one that isn't there gets a plain message instead of docker's
/// "context not found" (#266).
fn pick(given: Option<&str>, answers: impl Fn(&str) -> bool) -> io::Result<&'static str> {
    match given {
        Some(id) => {
            let id = known(id)?;
            if answers(id) {
                Ok(id)
            } else {
                Err(io::Error::other(format!("{} isn't installed or running here (no docker context \"{id}\" answering)", label(id))))
            }
        }
        None => SOURCES.iter().map(|(c, _)| *c).find(|c| answers(c)).ok_or_else(|| {
            let names: Vec<&str> = SOURCES.iter().map(|(_, l)| *l).collect();
            io::Error::other(format!("nothing to import from: {} isn't installed or running here", names.join(" or ")))
        }),
    }
}

const KINDS: [&str; 3] = ["image", "volume", "container"];
const USAGE: &str = "usage: kiri import [orbstack|desktop-linux] [--list | [--force] [image|volume|container NAME]]";

/// What `kiri import` was asked to do.
#[derive(Debug, PartialEq)]
enum Ask<'a> {
    Everything,
    List,
    One(&'a str, &'a str),
}

/// The source named (if any), what to do, and whether `--force` copies
/// volumes that running containers on the source use. A kind without
/// exactly one name, `--list` with an item or with `--force`, or a flag Kiri
/// doesn't know is a usage error, never "import everything".
fn parse<'a>(args: &[&'a str]) -> io::Result<(Option<&'a str>, Ask<'a>, bool)> {
    let usage = || io::Error::other(USAGE);
    let (mut list, mut force) = (false, false);
    let mut words = Vec::new();
    for a in args {
        match *a {
            "--list" => list = true,
            "--force" => force = true,
            f if f.starts_with('-') => return Err(usage()),
            w => words.push(w),
        }
    }
    let (given, rest) = match words.split_first() {
        Some((first, rest)) if !KINDS.contains(first) => (Some(*first), rest),
        _ => (None, &words[..]),
    };
    let ask = match (rest, list) {
        ([], false) => Ask::Everything,
        ([], true) if !force => Ask::List,
        ([kind, name], false) if KINDS.contains(kind) => Ask::One(kind, name),
        _ => return Err(usage()),
    };
    Ok((given, ask, force))
}

/// `kiri import [orbstack|desktop-linux] [--list | [--force] [image|volume|container NAME]]`:
/// one item, or every image, named volume and container Kiri lacks.
pub fn run(args: &[String]) -> io::Result<()> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (given, ask, force) = parse(&args)?;
    let source = pick(given, answers)?;
    if given.is_none() {
        println!("from {} ({source})", label(source));
    }
    if let Ask::One(kind, name) = ask {
        let r = match kind {
            "image" => image(source, name),
            "volume" => volume_with(source, name, force),
            _ => container_with(source, name, force, &[]),
        };
        return r.map(|()| println!("{kind} {name}: done"));
    }
    let inv = inventory(source)?;
    let list = |k: &str| inv[k].as_array().cloned().unwrap_or_default();
    if ask == Ask::List {
        for k in ["images", "volumes", "containers"] {
            println!("{k}:");
            for v in list(k) {
                println!("  {}{}", v["name"].as_str().unwrap_or(""), if v["have"] == true { "  (already in Kiri)" } else { "" });
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
    for v in list("volumes").iter().filter(|v| v["have"] != true && v["anonymous"] != true) {
        let n = v["name"].as_str().unwrap_or("");
        step("volume", n, volume_with(source, n, force));
    }
    for v in list("containers").iter().filter(|v| v["have"] != true) {
        let n = v["name"].as_str().unwrap_or("");
        // Already brought as the one another joins the network of.
        if !has(crate::flavor::CONTEXT.get(), "container", n) {
            step("container", n, container_with(source, n, force, &[]));
        }
    }
    if failed > 0 { Err(io::Error::other(format!("{failed} items could not be imported"))) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn anonymous_volumes() {
        assert!(super::anonymous(&"a1".repeat(32)));
        assert!(!super::anonymous("pgdata"));
    }

    // #266: a fresh machine with neither engine gets a plain answer, not
    // docker's "context not found"; with no source named, the first that
    // answers is used, and only as many engines are asked as needed.
    #[test]
    fn picks_a_running_source() {
        use super::pick;
        let none = |_: &str| false;
        let only_desktop = |c: &str| c == "desktop-linux";
        if cfg!(windows) {
            assert_eq!(pick(None, none).unwrap_err().to_string(), "nothing to import from: Docker Desktop isn't installed or running here");
        } else {
            assert_eq!(pick(None, none).unwrap_err().to_string(), "nothing to import from: OrbStack or Docker Desktop isn't installed or running here");
            assert!(pick(Some("orbstack"), only_desktop).is_err());
            // Named: only that engine is asked.
            let asked = std::cell::RefCell::new(Vec::new());
            assert_eq!(pick(Some("desktop-linux"), |c: &str| { asked.borrow_mut().push(c.to_owned()); true }).unwrap(), "desktop-linux");
            assert_eq!(*asked.borrow(), ["desktop-linux"]);
            // Not named: stops at the first that answers.
            asked.borrow_mut().clear();
            assert_eq!(pick(None, |c: &str| { asked.borrow_mut().push(c.to_owned()); true }).unwrap(), "orbstack");
            assert_eq!(*asked.borrow(), ["orbstack"]);
        }
        assert_eq!(pick(None, only_desktop).unwrap(), "desktop-linux");
        assert!(pick(Some("desktop-linux"), none).unwrap_err().to_string().starts_with("Docker Desktop isn't installed or running here"));
        // An unknown source is refused before any engine is asked.
        assert_eq!(pick(Some("podman"), |_: &str| panic!("asked")).unwrap_err().to_string(), "unknown source podman");
    }

    // A kind without exactly one name is a usage error, never "import
    // everything"; the source may be left out before a kind.
    #[test]
    fn parses_what_to_import() {
        use super::{Ask, parse};
        assert_eq!(parse(&[]).unwrap(), (None, Ask::Everything, false));
        assert_eq!(parse(&["orbstack"]).unwrap(), (Some("orbstack"), Ask::Everything, false));
        assert_eq!(parse(&["--list"]).unwrap(), (None, Ask::List, false));
        assert_eq!(parse(&["orbstack", "--list"]).unwrap(), (Some("orbstack"), Ask::List, false));
        assert_eq!(parse(&["image", "nginx"]).unwrap(), (None, Ask::One("image", "nginx"), false));
        assert_eq!(parse(&["desktop-linux", "volume", "pg"]).unwrap(), (Some("desktop-linux"), Ask::One("volume", "pg"), false));
        // #345: --force copies volumes running containers use, one or all.
        assert_eq!(parse(&["orbstack", "volume", "pg", "--force"]).unwrap(), (Some("orbstack"), Ask::One("volume", "pg"), true));
        assert_eq!(parse(&["--force", "container", "db"]).unwrap(), (None, Ask::One("container", "db"), true));
        assert_eq!(parse(&["orbstack", "--force"]).unwrap(), (Some("orbstack"), Ask::Everything, true));
        for bad in [
            &["orbstack", "image"][..],
            &["image"],
            &["orbstack", "image", "a", "b"],
            &["orbstack", "photo", "a"],
            // --list with an item would import it.
            &["--list", "image", "nginx"],
            &["image", "nginx", "--list"],
            &["--list", "--force"],
            // An unknown flag must not start a full import.
            &["--dry-run"],
            &["orbstack", "-l"],
        ] {
            assert!(parse(bad).unwrap_err().to_string().starts_with("usage:"), "{bad:?}");
        }
        assert_eq!(parse(&["podman"]).unwrap(), (Some("podman"), Ask::Everything, false));
    }

    // Kiri's half of a volume copy runs only an image the engine pulled
    // from an official repository, as its repo digests show: never one
    // that only has a name like one, wherever it came from.
    #[test]
    fn only_an_official_image_helps() {
        use super::{official, pick_helper};
        use serde_json::json;
        let d = format!("@sha256:{}", "ab12".repeat(16));
        for good in ["busybox", "library/alpine", "docker.io/library/debian", "mirror.gcr.io/library/ubuntu", "public.ecr.aws/docker/library/busybox"] {
            assert!(official(&format!("{good}{d}")), "{good}");
        }
        let look_alikes = [
            "ghcr.io/x/library/busybox",
            "localhost:5000/library/alpine",
            "localhost/x/library/busybox",
            "evil/alpine",
            "docker.io/evil/alpine",
            "mirror.gcr.io/evil/library/alpine",
            "public.ecr.aws/x/library/alpine",
            "alpinelinux",
            "nginx",
            "busybox:1",
        ];
        for bad in look_alikes {
            assert!(!official(&format!("{bad}{d}")), "{bad}");
        }
        for bad in ["busybox", "busybox:1", "busybox@sha256:abc", &format!("busybox@sha512:{}", "a".repeat(64)), &format!("busybox@sha256:{}", "A".repeat(64))] {
            assert!(!official(bad), "{bad}");
        }
        // Kiri's image list: one record per name, each with its own digests.
        let rec = |name: &str, digests: &[String]| json!({"Id": format!("sha256:{}", "c".repeat(64)), "RepoTags": [name], "RepoDigests": digests});
        let records = [
            // Untagged: a load or build without a name.
            rec(&format!("sha256:{}", "c".repeat(64)), &[]),
            rec("localhost/imp-x/library/busybox:9", &[]),
            // Re-tagged from the official one: no digest of its own.
            rec("alpine:latest", &[]),
            // Called busybox, its digest another repository's.
            rec("busybox:1", &[format!("evil/tools{d}")]),
            // An official digest under a name of another repository.
            rec("evil/alpine:1", &[format!("alpine{d}")]),
            rec("ghcr.io/x/library/busybox:1", &[format!("ghcr.io/x/library/busybox{d}")]),
            rec("mirror.gcr.io/library/alpine:latest", &[format!("mirror.gcr.io/library/alpine{d}")]),
            rec("busybox:latest", &[format!("busybox{d}")]),
        ];
        assert_eq!(pick_helper(&records), Some("mirror.gcr.io/library/alpine:latest".into()));
        assert_eq!(pick_helper(&records[..6]), None);
        assert_eq!(pick_helper(&records[7..]), Some("busybox:latest".into()));
        assert_eq!(pick_helper(&[json!({"RepoTags": null, "RepoDigests": [format!("busybox{d}")]})]), None);
    }

    // #377: an image only pinned by digest is listed by that reference.
    #[test]
    fn lists_images_pinned_by_digest() {
        use serde_json::json;
        let d = "sha256:49dbca2bb9fc8e48bfc2cc5e039c0e867ca32012f6c739d07b8b0ad603b0d9c8";
        let name = |r: &str, t: &str, dg: &str| super::listed_name(&json!({"Repository": r, "Tag": t, "Digest": dg}));
        assert_eq!(name("nginx", "alpine", d), Some("nginx:alpine".into()));
        assert_eq!(name("ghcr.io/yetone/magpie", "<none>", d), Some(format!("ghcr.io/yetone/magpie@{d}")));
        assert_eq!(name("<none>", "<none>", "<none>"), None);
        assert_eq!(name("built", "<none>", "<none>"), None);
    }

    // #345: a volume a running container uses is not copied, and the
    // reason says what to do.
    #[test]
    fn refuses_volumes_in_use() {
        assert_eq!(super::in_use("pg", "OrbStack", &[]), None);
        assert_eq!(
            super::in_use("pg", "OrbStack", &["db".into()]).unwrap(),
            "pg is in use by db, running on OrbStack: a copy taken now can be cut off mid-write. Stop it there and import again for an exact copy (in a terminal, kiri import --force copies it anyway)"
        );
        assert!(super::in_use("pg", "Docker Desktop", &["db".into(), "backup".into()]).unwrap().starts_with("pg is in use by db, backup, running on Docker Desktop: a copy taken now can be cut off mid-write. Stop them there"));
    }

    // An unfinished volume in Kiri is not emptied while any container there
    // has it; only our own helpers there means another import is at it.
    #[test]
    fn refuses_to_empty_a_volume_kiri_has_in_use() {
        use super::in_use_in_kiri;
        assert_eq!(in_use_in_kiri("pg", &[]), None);
        assert_eq!(
            in_use_in_kiri("pg", &[("db".into(), false)]).unwrap(),
            "pg holds an unfinished copy in Kiri, and Kiri has db using it: importing it again would replace what is in it. Remove it in Kiri (or remove the volume in Kiri) and import again; importing a container again brings its volumes first"
        );
        let mixed = in_use_in_kiri("pg", &[("db".into(), false), ("kiri-import-pg".into(), true), ("backup".into(), false)]).unwrap();
        assert!(mixed.contains("Kiri has db, backup using it") && mixed.contains("Remove them in Kiri"), "{mixed}");
        assert_eq!(in_use_in_kiri("pg", &[("x".into(), true), ("y".into(), true)]).unwrap(), "another import of pg is running in Kiri: let it finish, then import again if it did not");
    }

    // #375: beside `container:` mode, what Kiri refuses is left out (the
    // source fills in a hostname and the image's ports itself); devices,
    // DNS, healthcheck and the rest Kiri keeps come along.
    #[test]
    fn builds_the_create_body() {
        use serde_json::{Value, json};
        let info = |mode: &str| {
            json!({
                "Config": {"Image": "alpine", "Hostname": "4c1e1a2b3c4d", "ExposedPorts": {"80/tcp": {}}, "Cmd": ["sh"], "Labels": null,
                           "Healthcheck": {"Test": ["CMD", "true"], "Interval": 1_000_000_000_u64}, "Domainname": "lan", "StopTimeout": 5},
                "HostConfig": {"NetworkMode": mode, "PortBindings": {"80/tcp": [{"HostPort": "8080"}]}, "Dns": ["1.1.1.1"], "ExtraHosts": ["h:1.2.3.4"],
                               "Devices": [{"PathOnHost": "/dev/net/tun", "PathInContainer": "/dev/net/tun", "CgroupPermissions": "rwm"}],
                               "GroupAdd": ["audio"], "PidMode": "", "CapAdd": ["NET_ADMIN"], "Binds": null, "Mounts": null, "Ulimits": [{"Name": "nofile", "Soft": 1024, "Hard": 2048}]},
                "Mounts": []
            })
        };
        let joined = super::create_body(&info("container:4c1e1a2b3c4d5e6f"), "container:vpn", json!({}));
        for gone in ["Hostname", "ExposedPorts", "MacAddress"] {
            assert!(joined[gone].is_null(), "{gone}");
        }
        for gone in ["PortBindings", "PublishAllPorts", "Dns", "ExtraHosts"] {
            assert!(joined["HostConfig"][gone].is_null(), "{gone}");
        }
        assert_eq!(joined["HostConfig"]["NetworkMode"], "container:vpn");
        assert_eq!(joined["HostConfig"]["Devices"][0]["PathOnHost"], "/dev/net/tun");
        assert_eq!(joined["HostConfig"]["GroupAdd"], json!(["audio"]));
        assert_eq!(joined["HostConfig"]["CapAdd"], json!(["NET_ADMIN"]));
        assert_eq!(joined["Healthcheck"]["Test"], json!(["CMD", "true"]));
        assert_eq!(joined["Domainname"], "lan");
        // Its own network: as before, plus what was dropped.
        let own = super::create_body(&info("bridge"), "bridge", json!({"bridge": {"Aliases": []}}));
        assert_eq!(own["Hostname"], "4c1e1a2b3c4d");
        assert_eq!(own["HostConfig"]["PortBindings"]["80/tcp"][0]["HostPort"], "8080");
        assert_eq!(own["HostConfig"]["Dns"], json!(["1.1.1.1"]));
        assert_eq!(own["HostConfig"]["Ulimits"][0]["Name"], "nofile");
        assert_eq!(own["StopTimeout"], 5);
        assert!(own["HostConfig"].as_object().unwrap().values().all(|v| !v.is_null()));
        assert!(own.as_object().unwrap().values().all(|v: &Value| !v.is_null()));
    }

    // #376: a container's anonymous volumes keep their names (their files
    // were copied under them), as a bind or in its --mount.
    #[test]
    fn names_anonymous_volumes() {
        use serde_json::json;
        let anon = "a1".repeat(32);
        let anon2 = "b2".repeat(32);
        let info = json!({
            "Config": {"Image": "postgres"},
            "HostConfig": {"Binds": ["pgconf:/etc/pg:ro"], "Mounts": [{"Type": "volume", "Source": "", "Target": "/cache"}]},
            "Mounts": [
                {"Type": "volume", "Name": "pgconf", "Destination": "/etc/pg", "RW": false},
                {"Type": "volume", "Name": anon, "Destination": "/var/lib/postgresql/data", "RW": true},
                {"Type": "volume", "Name": anon2, "Destination": "/cache", "RW": true},
                {"Type": "bind", "Source": "/Users/me/src", "Destination": "/src", "RW": true}
            ]
        });
        assert_eq!(super::volumes_of(&info), ["pgconf".to_owned(), anon.clone(), anon2.clone()]);
        let body = super::create_body(&info, "bridge", json!({}));
        assert_eq!(body["HostConfig"]["Binds"], json!(["pgconf:/etc/pg:ro", format!("{anon}:/var/lib/postgresql/data")]));
        assert_eq!(body["HostConfig"]["Mounts"][0]["Source"], anon2.as_str());
        // None at all: no Binds made up.
        let plain = super::create_body(&json!({"Config": {"Image": "alpine"}, "HostConfig": {}, "Mounts": []}), "bridge", json!({}));
        assert!(plain["HostConfig"]["Binds"].is_null());
    }

    // #377: an image pinned by digest gets its reference in the archive's
    // manifest, so Kiri knows it by that name; the rest passes unchanged.
    // (The tar crate is a test dependency of crates/kiri only.)
    #[cfg(not(windows))]
    #[test]
    fn names_an_image_pinned_by_digest() {
        let name = "ghcr.io/o/r@sha256:49dbca2bb9fc8e48bfc2cc5e039c0e867ca32012f6c739d07b8b0ad603b0d9c8";
        let layer: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let mut b = tar::Builder::new(Vec::new());
        let mut add = |path: &str, data: &[u8], pax: bool| {
            if pax {
                b.append_pax_extensions([("size", data.len().to_string().as_bytes())]).unwrap();
            }
            let mut h = tar::Header::new_ustar();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, data).unwrap();
        };
        add("blobs/sha256/aa", &layer, true);
        add("manifest.json", br#"[{"Config":"blobs/sha256/cc","RepoTags":null,"Layers":["blobs/sha256/aa"]}]"#, false);
        add("index.json", b"{}", false);
        let input = b.into_inner().unwrap();
        let mut out = Vec::new();
        super::retar(input.as_slice(), &mut out, Some(name)).unwrap();
        let mut seen = Vec::new();
        for e in tar::Archive::new(out.as_slice()).entries().unwrap() {
            let mut e = e.unwrap();
            let path = e.path().unwrap().to_string_lossy().into_owned();
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut e, &mut data).unwrap();
            match path.as_str() {
                "blobs/sha256/aa" => assert_eq!(data, layer),
                "manifest.json" => {
                    let m: serde_json::Value = serde_json::from_slice(&data).unwrap();
                    assert_eq!(m[0]["RepoTags"], serde_json::json!([name]));
                    assert_eq!(m[0]["Layers"][0], "blobs/sha256/aa");
                }
                _ => assert_eq!(data, b"{}"),
            }
            seen.push(path);
        }
        assert_eq!(seen, ["blobs/sha256/aa", "manifest.json", "index.json"]);
        // Cut short: an error, never a quiet half.
        assert!(super::retar(&input[..1024], &mut Vec::new(), Some(name)).is_err());
        // A manifest that is not a list of objects is left as it is, not a panic.
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_ustar();
        h.set_size(10);
        h.set_cksum();
        b.append_data(&mut h, "manifest.json", &b"[1,\"x\",{}]"[..]).unwrap();
        let odd = b.into_inner().unwrap();
        let mut out = Vec::new();
        super::retar(odd.as_slice(), &mut out, Some(name)).unwrap();
    }
    // Big numbers in tar headers: octal while they fit, GNU base-256 after.
    #[test]
    fn writes_tar_numbers() {
        use super::{get_number, put_number};
        let mut size = [0u8; 12];
        put_number(&mut size, 0o777);
        assert_eq!(&size, b"00000000777\0");
        let big = 9 << 30;
        put_number(&mut size, big);
        assert_eq!(size, [0x80, 0, 0, 0, 0, 0, 0, 0x02, 0x40, 0, 0, 0]);
        assert_eq!(get_number(&size), Some(big));
        let mut uid = [0u8; 8];
        put_number(&mut uid, 2_097_151);
        assert_eq!(&uid, b"7777777\0");
        put_number(&mut uid, 3_000_000);
        assert_eq!(uid, [0x80, 0, 0, 0, 0, 0x2d, 0xc6, 0xc0]);
        assert_eq!(get_number(&uid), Some(3_000_000));
        assert_eq!(get_number(b"0000644 \0"), Some(0o644));
    }

    // A stream cut off anywhere before the archive's end is an error that
    // says where: inside a pax header, inside a file's body, at a header.
    #[cfg(not(windows))]
    #[test]
    fn a_cut_stream_is_an_error() {
        let body: Vec<u8> = (0..1500u32).map(|i| (i % 251) as u8).collect();
        let mut b = tar::Builder::new(Vec::new());
        b.append_pax_extensions([("uid", &b"3000000"[..])]).unwrap();
        let mut h = tar::Header::new_ustar();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "file", body.as_slice()).unwrap();
        let whole = b.into_inner().unwrap();
        // pax header, its records, the file's header, its body: then the end.
        let end = 512 + 512 + 512 + 1536;
        assert!(whole[end..].iter().all(|b| *b == 0));
        let err = |at: usize| super::retar(&whole[..at], &mut Vec::new(), None).unwrap_err().to_string();
        assert!(err(512 + 10).contains("inside a pax header"), "{}", err(512 + 10));
        assert!(err(1536 + 700).contains("inside a file's body"), "{}", err(1536 + 700));
        assert!(err(end).contains("before the archive's end"), "{}", err(end));
        for at in 0..=end {
            assert!(super::retar(&whole[..at], &mut Vec::new(), None).is_err(), "a cut at {at} passed");
        }
        super::retar(whole.as_slice(), &mut Vec::new(), None).unwrap();
    }

    // Records are read by their length: a value holding " size=" is no size.
    #[test]
    fn reads_pax_records() {
        let path = "a size=99 b";
        let rec = |k: &str, v: &str| {
            let body = format!(" {k}={v}\n");
            let mut n = body.len() + 1;
            while format!("{n}{body}").len() != n {
                n += 1;
            }
            format!("{n}{body}")
        };
        let data = format!("{}{}", rec("path", path), rec("uid", "3000000"));
        assert_eq!(super::pax_records(data.as_bytes()), [("path".to_owned(), path.to_owned()), ("uid".to_owned(), "3000000".to_owned())]);
        assert!(super::pax_records(b"garbage").is_empty());
    }

    // What `docker cp` writes for a file of 8 GiB or more, or an owner above
    // 2097151: the value only in pax, the header's field
    // zero. busybox tar reads only names from pax, so the values go into the
    // entry's own header; the bodies pass byte for byte.
    #[cfg(not(windows))]
    #[test]
    fn puts_pax_sizes_and_owners_in_the_header() {
        let body: Vec<u8> = (0..1500u32).map(|i| (i % 253) as u8).collect();
        let mut b = tar::Builder::new(Vec::new());
        // As Go's archive/tar writes it: the pax records, then a header whose
        // fields could not hold them, left zero.
        b.append_pax_extensions([("path", &b"odd size=5 name"[..]), ("size", b"1500"), ("uid", b"3000000"), ("gid", b"3000001")]).unwrap();
        let mut h = tar::Header::new_ustar();
        h.set_size(0);
        h.set_mode(0o640);
        h.set_cksum();
        let mut raw = h.as_bytes().to_vec();
        raw[..4].copy_from_slice(b"file");
        let mut fixed = [0u8; 512];
        fixed.copy_from_slice(&raw);
        let mut h = tar::Header::from_byte_slice(&fixed).clone();
        h.set_cksum();
        b.append(&h, std::io::empty()).unwrap();
        let mut input = b.into_inner().unwrap();
        // The body after the header, as the size in pax says.
        let end = input.len() - 1024;
        input.truncate(end);
        input.extend_from_slice(&body);
        input.extend(std::iter::repeat_n(0u8, 1536usize.div_ceil(512) * 512 - body.len() + 1024));
        let mut h2 = tar::Header::new_ustar();
        h2.set_size(3);
        h2.set_uid(0);
        h2.set_gid(0);
        h2.set_mtime(0);
        h2.set_mode(0o644);
        h2.set_cksum();
        let mut tail = tar::Builder::new(Vec::new());
        tail.append_data(&mut h2, "next", &b"abc"[..]).unwrap();
        let tail = tail.into_inner().unwrap();
        // Before the end blocks of the first archive: the next entry.
        let cut = input.len() - 1024;
        input.truncate(cut);
        input.extend_from_slice(&tail);
        let mut out = Vec::new();
        super::retar(input.as_slice(), &mut out, None).unwrap();
        assert_eq!(out.len(), input.len());
        // Each header as a reader that ignores pax sees it.
        let first = &out[1024..1536];
        assert_eq!(super::get_number(&first[124..136]), Some(1500));
        assert_eq!(super::get_number(&first[108..116]), Some(3_000_000));
        assert_eq!(super::get_number(&first[116..124]), Some(3_000_001));
        let sum: u32 = first.iter().enumerate().map(|(i, b)| if (148..156).contains(&i) { 32 } else { u32::from(*b) }).sum();
        assert_eq!(super::get_number(&first[148..156]), Some(u64::from(sum)));
        // Everything but that header is byte for byte as it came.
        assert_eq!(out[..1024], input[..1024]);
        assert_eq!(out[1536..], input[1536..]);
        let mut seen = Vec::new();
        for e in tar::Archive::new(out.as_slice()).entries().unwrap() {
            let mut e = e.unwrap();
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut e, &mut data).unwrap();
            seen.push((e.path().unwrap().to_string_lossy().into_owned(), e.header().uid().unwrap(), data));
        }
        assert_eq!(seen, [("odd size=5 name".to_owned(), 3_000_000, body), ("next".to_owned(), 0, b"abc".to_vec())]);
    }

}
