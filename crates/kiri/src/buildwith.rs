//! Which builder plain `docker build` uses: Docker's own, or Kiri's.
//!
//! On a machine with no other engine, Kiri's, without asking. On one with
//! OrbStack, Docker Desktop or another engine, the window asks once
//! ("Keep building with Docker" is the default), and Settings → Builds
//! switches it any time, at once, in terminals already open too.
//!
//! How the docker CLI picks a builder: `docker build` (and `docker
//! builder`) is handed to buildx with `BUILDX_BUILDER=<current context>`,
//! unless the config's `builder` alias is set (`docker buildx install`),
//! in which case buildx takes its current builder (`~/.docker/buildx/current`).
//! Compose and `docker buildx build` take buildx's current builder.
//!
//! - Always: a buildx builder named after Kiri's context (`kiri`), a copy
//!   of Kiri's builder (`kiri-build`: the same BuildKit and cache). A
//!   builder in buildx's store wins over the context of the same name, so
//!   `docker --context kiri build` builds on Kiri and loads into Kiri,
//!   where buildx would otherwise ask the engine for a BuildKit of its own,
//!   which Kiri doesn't have. No other context is touched.
//! - Build with Docker: nothing more. Builds follow docker's context, as
//!   they did before Kiri: OrbStack's builds stay OrbStack's, and once
//!   docker uses Kiri, builds run on Kiri.
//! - Build with Kiri: the `builder` alias, and Kiri's builder as buildx's
//!   current builder for every context (what `docker buildx use --global`
//!   writes). `docker build`, `docker compose build` and `docker buildx
//!   build` run on Kiri and load into Kiri whatever context docker uses.
//!   Everything else (`docker run`, `ps`, `compose up`…) still follows the
//!   context, so an image built this way is in Kiri, not in Docker
//!   Desktop's store.
//!
//! What Kiri changes for "Build with Kiri" (the alias and buildx's current
//! builder) is kept as it was in `build.json` first, and choosing Docker
//! puts it back exactly, unless something else has changed it since.
//! `kiri build` always builds on Kiri, whichever is chosen.
//!
//! The same file serves the Mac and Linux (`kiri build-with`, the window)
//! and Windows (kiri-win includes it).

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Docker's builder, or Kiri's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum With {
    Docker,
    Kiri,
}

impl With {
    pub fn parse(s: &str) -> Option<With> {
        match s {
            "docker" => Some(With::Docker),
            "kiri" => Some(With::Kiri),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            With::Docker => "docker",
            With::Kiri => "kiri",
        }
    }
}

/// Where everything is: docker's config directory, buildx's, Kiri's
/// `build.json`, and Kiri's builder and context names.
#[derive(Debug, Clone)]
pub struct Places {
    pub docker: PathBuf,
    pub buildx: PathBuf,
    pub state: PathBuf,
    pub builder: String,
    pub context: String,
}

fn read_json(path: &Path) -> io::Result<Value> {
    match std::fs::read(path) {
        // A BOM, as Windows PowerShell writes: docker itself tolerates none,
        // but reading through one costs nothing.
        Ok(b) => {
            serde_json::from_slice(b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&b)).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e),
    }
}

/// Through a temporary file, so a crash never leaves half a file.
fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("kiri-tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

fn write_json(path: &Path, v: &Value) -> io::Result<()> {
    write_bytes(
        path,
        &serde_json::to_vec_pretty(v).map_err(io::Error::other)?,
    )
}

impl Places {
    fn config(&self) -> PathBuf {
        self.docker.join("config.json")
    }

    fn current(&self) -> PathBuf {
        self.buildx.join("current")
    }

    fn instance(&self, name: &str) -> PathBuf {
        self.buildx.join("instances").join(name)
    }

    fn state(&self) -> Value {
        read_json(&self.state).unwrap_or_else(|_| json!({}))
    }

    /// What was chosen (or set for a machine with no other engine).
    pub fn saved(&self) -> Option<With> {
        self.state()["with"].as_str().and_then(With::parse)
    }

    /// Whether to ask: another engine is here, and nobody has chosen. A
    /// choice Kiri made by itself on a machine that had none at the time is
    /// asked about once another engine appears.
    pub fn ask(&self, other: Option<&str>) -> bool {
        let s = self.state();
        other.is_some() && (s["with"].is_null() || s["auto"] == true)
    }

    /// Whether Kiri's builder is in buildx's store yet.
    pub fn ready(&self) -> bool {
        self.instance(&self.builder).is_file()
    }

    /// Whether `docker build` goes to Kiri's builder in every context now.
    pub fn taken(&self) -> bool {
        let cfg = read_json(&self.config()).unwrap_or_default();
        let cur = read_json(&self.current()).unwrap_or_default();
        cfg["aliases"]["builder"] == "buildx"
            && cur["Name"] == self.builder.as_str()
            && cur["Global"] == true
    }

    /// The builder named after Kiri's context: Kiri's builder under that
    /// name. Left alone if one of that name is there that isn't a copy of
    /// Kiri's.
    fn shadow(&self) -> io::Result<()> {
        let ours = read_json(&self.instance(&self.builder))?;
        if ours.get("Name").is_none() {
            return Ok(()); // Not made yet.
        }
        let path = self.instance(&self.context);
        let theirs = read_json(&path)?;
        if theirs.get("Name").is_some() && !self.copied(&theirs) {
            return Ok(());
        }
        let mut copy = ours;
        copy["Name"] = self.context.as_str().into();
        if theirs != copy {
            write_bytes(&path, &serde_json::to_vec(&copy).map_err(io::Error::other)?)?;
        }
        Ok(())
    }

    /// Whether a builder is Kiri's copy: its nodes are Kiri's builder's.
    fn copied(&self, instance: &Value) -> bool {
        let nodes = instance["Nodes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        !nodes.is_empty()
            && nodes.iter().all(|n| {
                n["Name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with(&self.builder))
            })
    }

    fn save(&self, with: With, auto: bool) -> io::Result<()> {
        let mut s = self.state();
        s["with"] = with.name().into();
        s["auto"] = auto.into();
        write_json(&self.state, &s)
    }

    /// Build with Kiri: the alias and the global current builder, after
    /// keeping what was there. False if Kiri's builder isn't made yet (it
    /// is taken once it is: `apply`).
    fn take(&self) -> io::Result<bool> {
        if !self.ready() {
            return Ok(false);
        }
        let mut s = self.state();
        if s["before"].is_null() {
            let cfg = read_json(&self.config())?;
            let current = match std::fs::read_to_string(self.current()) {
                Ok(t) => Value::String(t),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Value::Null,
                Err(e) => return Err(e),
            };
            s["before"] = json!({"alias": cfg["aliases"]["builder"], "aliases": cfg.get("aliases").is_some(), "current": current});
            write_json(&self.state, &s)?;
        }
        let mut cfg = read_json(&self.config())?;
        if !cfg["aliases"].is_object() {
            cfg["aliases"] = json!({});
        }
        if cfg["aliases"]["builder"] != "buildx" {
            cfg["aliases"]["builder"] = "buildx".into();
            write_json(&self.config(), &cfg)?;
        }
        // buildx's own format, as `docker buildx use --global` writes it.
        let current = json!({"Key": self.context, "Name": self.builder, "Global": true});
        write_bytes(&self.current(), current.to_string().as_bytes())?;
        Ok(true)
    }

    /// Build with Docker: puts back what `take` kept, where it is still
    /// as Kiri left it. Also when Kiri's program is gone (appwatch.rs):
    /// the choice itself is kept, so `apply` takes it again later.
    pub fn give_back(&self) -> io::Result<()> {
        let mut s = self.state();
        let before = s["before"].take();
        if before.is_null() {
            return Ok(());
        }
        let mut cfg = read_json(&self.config())?;
        if cfg["aliases"]["builder"] == "buildx" {
            match &before["alias"] {
                Value::Null => {
                    if let Some(a) = cfg["aliases"].as_object_mut() {
                        a.remove("builder");
                    }
                    if before["aliases"] != true
                        && cfg["aliases"]
                            .as_object()
                            .is_some_and(serde_json::Map::is_empty)
                        && let Some(o) = cfg.as_object_mut()
                    {
                        o.remove("aliases");
                    }
                }
                was => cfg["aliases"]["builder"] = was.clone(),
            }
            write_json(&self.config(), &cfg)?;
        }
        let cur = read_json(&self.current()).unwrap_or_default();
        if cur["Name"] == self.builder.as_str() && cur["Global"] == true {
            match before["current"].as_str() {
                Some(t) => write_bytes(&self.current(), t.as_bytes())?,
                None => match std::fs::remove_file(self.current()) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                },
            }
        }
        if let Some(o) = s.as_object_mut() {
            o.remove("before");
        }
        write_json(&self.state, &s)
    }

    /// What was chosen, made so: called whenever Kiri's builder has been
    /// (re)made. Never fights a change made since, such as a `docker buildx
    /// use` of the user's own: only choosing again does that.
    pub fn apply(&self, other: Option<&str>) -> io::Result<()> {
        self.shadow()?;
        let s = self.state();
        match self.saved() {
            Some(With::Kiri) if s["before"].is_null() => self.take().map(drop),
            Some(With::Docker) => self.give_back(),
            None if other.is_none() => {
                self.save(With::Kiri, true)?;
                self.take().map(drop)
            }
            _ => Ok(()),
        }
    }

    /// The user's choice, kept and made so now.
    pub fn choose(&self, with: With) -> io::Result<()> {
        self.save(with, false)?;
        self.shadow()?;
        match with {
            With::Kiri => self.take().map(drop),
            With::Docker => self.give_back(),
        }
    }

    /// For the window: {with, ask, engine, taken, ready, builder, context}.
    pub fn status(&self, other: Option<&str>) -> Value {
        // Not chosen yet: Docker's, which is what happens until someone does.
        let with = self.saved().unwrap_or(if other.is_some() {
            With::Docker
        } else {
            With::Kiri
        });
        json!({"type": "buildwith", "with": with.name(), "ask": self.ask(other), "engine": other, "taken": self.taken(), "ready": self.ready(), "builder": self.builder, "context": self.context})
    }

    /// Uninstalling: Docker's builder back, the copy and `build.json` gone.
    pub fn uninstall(&self) -> io::Result<()> {
        self.give_back()?;
        let path = self.instance(&self.context);
        if self.copied(&read_json(&path)?) {
            std::fs::remove_file(path)?;
        }
        match std::fs::remove_file(&self.state) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn docker_dir(home: &Path) -> PathBuf {
    std::env::var_os("DOCKER_CONFIG")
        .filter(|d| !d.is_empty())
        .map_or_else(|| home.join(".docker"), PathBuf::from)
}

fn buildx_dir(docker: &Path) -> PathBuf {
    std::env::var_os("BUILDX_CONFIG")
        .filter(|d| !d.is_empty())
        .map_or_else(|| docker.join("buildx"), PathBuf::from)
}

#[cfg(unix)]
pub fn places() -> io::Result<Places> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    let docker = docker_dir(&home);
    Ok(Places {
        buildx: buildx_dir(&docker),
        docker,
        state: home.join(crate::flavor::STATE.get()).join("build.json"),
        builder: crate::builds::BUILDER.get().to_owned(),
        context: crate::flavor::CONTEXT.get().to_owned(),
    })
}

#[cfg(windows)]
pub fn places() -> io::Result<Places> {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("USERPROFILE is not set"))?;
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?;
    let docker = docker_dir(&home);
    Ok(Places {
        buildx: buildx_dir(&docker),
        docker,
        state: local.join("Kiri").join("build.json"),
        builder: crate::builds::BUILDER.get().to_owned(),
        context: crate::flavor::CONTEXT.get().to_owned(),
    })
}

/// The engine besides Kiri on this machine, by name, if there is one.
#[cfg(unix)]
pub fn other_engine() -> Option<&'static str> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let ours = crate::client::socket()
        .ok()
        .and_then(|s| s.canonicalize().ok());
    // A socket that is Kiri's own (a /var/run/docker.sock pointing at Kiri) isn't another engine.
    let live = |p: &Path| p.exists() && p.canonicalize().ok() != ours;
    let name = |p: &Path| {
        let t = p
            .canonicalize()
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned();
        if t.contains(".orbstack") {
            "OrbStack"
        } else if t.contains("/.docker/") {
            "Docker Desktop"
        } else if t.contains("/.rd/") {
            "Rancher Desktop"
        } else if t.contains("/.colima/") {
            "Colima"
        } else if t.contains("podman") {
            "Podman"
        } else {
            "Docker"
        }
    };
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let sockets = [
        home.join(".orbstack/run/docker.sock"),
        home.join(".docker/run/docker.sock"),
        home.join(".docker/desktop/docker.sock"),
        home.join(".rd/docker.sock"),
        home.join(".colima/default/docker.sock"),
        PathBuf::from("/var/run/docker.sock"),
        PathBuf::from("/run/docker.sock"),
        PathBuf::from("/run/podman/podman.sock"),
    ];
    if let Some(s) = sockets
        .iter()
        .chain(xdg.map(|x| x.join("podman/podman.sock")).as_ref())
        .find(|s| live(s))
    {
        return Some(name(s));
    }
    // Installed, though not running now.
    let apps: &[(&str, &str)] = if cfg!(target_os = "macos") {
        &[
            ("/Applications/OrbStack.app", "OrbStack"),
            ("/Applications/Docker.app", "Docker Desktop"),
            ("/Applications/Rancher Desktop.app", "Rancher Desktop"),
        ]
    } else {
        &[
            ("/usr/bin/dockerd", "Docker"),
            ("/opt/docker-desktop", "Docker Desktop"),
            ("/opt/rancher-desktop", "Rancher Desktop"),
        ]
    };
    apps.iter()
        .find(|(p, _)| Path::new(p).exists())
        .map(|(_, n)| *n)
}

/// The engine besides Kiri on this PC, by name, if there is one.
#[cfg(windows)]
pub fn other_engine() -> Option<&'static str> {
    let under = |var: &str, rel: &str| {
        std::env::var_os(var).is_some_and(|d| Path::new(&d).join(rel).is_file())
    };
    if under("ProgramFiles", r"Docker\Docker\Docker Desktop.exe") {
        Some("Docker Desktop")
    } else if under("ProgramFiles", r"Rancher Desktop\Rancher Desktop.exe")
        || under(
            "LOCALAPPDATA",
            r"Programs\Rancher Desktop\Rancher Desktop.exe",
        )
    {
        Some("Rancher Desktop")
    } else if under("ProgramFiles", r"RedHat\Podman\podman.exe") {
        Some("Podman")
    } else if Path::new(r"\\.\pipe\docker_engine").exists() {
        Some("Docker")
    } else {
        None
    }
}

/// After Kiri's builder is (re)made: what was chosen, made so.
pub fn apply() -> io::Result<()> {
    places()?.apply(other_engine())
}

/// The window's or the CLI's choice.
pub fn choose(with: With) -> io::Result<()> {
    places()?.choose(with)
}

/// The window's `buildwith` event.
pub fn event() -> String {
    let other = other_engine();
    match places() {
        Ok(p) => p.status(other),
        Err(e) => json!({"type": "buildwith", "error": e.to_string()}),
    }
    .to_string()
}

/// Whether the window should ask, and about which engine.
pub fn ask() -> Option<&'static str> {
    let other = other_engine();
    places().ok().filter(|p| p.ask(other)).and(other)
}

/// `kiri build-with [docker|kiri]`: says which, or switches.
#[cfg(unix)]
pub fn cli(args: &[String]) -> io::Result<()> {
    let p = places()?;
    let other = other_engine();
    if let Some(a) = args.first() {
        let with = With::parse(a)
            .ok_or_else(|| io::Error::other("usage: kiri build-with [docker|kiri]"))?;
        p.choose(with)?;
        if with == With::Kiri && !p.ready() {
            crate::builds::ensure_builder();
        }
    }
    let s = p.status(other);
    let builder = &p.builder;
    match (s["with"].as_str(), other) {
        (Some("kiri"), _) if !p.ready() => println!(
            "docker build builds on Kiri once its builder `{builder}` is made (the window or `kiri setup` makes it while Kiri runs)"
        ),
        (Some("kiri"), Some(o)) => println!(
            "docker build and docker compose build run on Kiri, whatever docker context is in use; the images are in Kiri, not {o}. `kiri build-with docker` switches back"
        ),
        (Some("kiri"), None) => {
            println!("docker build and docker compose build run on Kiri (builder `{builder}`)")
        }
        (_, o) => println!(
            "docker build follows docker's context{}: on Kiri when docker uses Kiri. `kiri build` always builds on Kiri; `kiri build-with kiri` sends every build to Kiri",
            o.map(|o| format!(" ({o}'s builds stay {o}'s)"))
                .unwrap_or_default()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(dir: &Path) -> Places {
        let docker = dir.join(".docker");
        Places {
            buildx: docker.join("buildx"),
            docker,
            state: dir.join(".kiri/build.json"),
            builder: "kiri-build".into(),
            context: "kiri".into(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kiri-buildwith-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn builder(p: &Places) {
        let i = json!({"Name": "kiri-build", "Driver": "docker-container", "Nodes": [{"Name": "kiri-build0", "Endpoint": "kiri", "DriverOpts": {"default-load": "true"}}], "Dynamic": false});
        write_bytes(&p.instance("kiri-build"), i.to_string().as_bytes()).unwrap();
    }

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    #[test]
    fn a_fresh_machine_builds_with_kiri_and_back_leaves_nothing() {
        let d = scratch("fresh");
        let p = places(&d);
        // Before the builder exists: chosen, taken later.
        p.apply(None).unwrap();
        assert_eq!(p.saved(), Some(With::Kiri));
        assert!(!p.taken());
        builder(&p);
        p.apply(None).unwrap();
        assert!(p.taken());
        let copy = read_json(&p.instance("kiri")).unwrap();
        assert_eq!(
            (copy["Name"].as_str(), copy["Nodes"][0]["Name"].as_str()),
            (Some("kiri"), Some("kiri-build0"))
        );
        assert_eq!(
            read_json(&p.current()).unwrap(),
            json!({"Key": "kiri", "Name": "kiri-build", "Global": true})
        );
        assert!(!p.ask(None));
        // Docker Desktop installed afterwards: asked once.
        assert!(p.ask(Some("Docker Desktop")));
        p.choose(With::Docker).unwrap();
        assert!(!p.ask(Some("Docker Desktop")));
        assert!(!p.taken());
        assert_eq!(read_json(&p.config()).unwrap(), json!({}));
        assert!(!p.current().exists());
        // The context's builder stays: it serves `docker --context kiri build`.
        assert!(p.instance("kiri").is_file());
    }

    #[test]
    fn a_second_kiri_whose_app_is_gone_hands_builds_back_to_the_first() {
        let d = scratch("second");
        let main = places(&d);
        builder(&main);
        main.choose(With::Kiri).unwrap();
        // A development build of Kiri, its own state and builder, takes them.
        let dev = Places {
            state: d.join(".kiri-layers/build.json"),
            builder: "kiri-layers-build".into(),
            context: "kiri-layers".into(),
            ..places(&d)
        };
        let i = json!({"Name": "kiri-layers-build", "Driver": "docker-container", "Nodes": [{"Name": "kiri-layers-build0", "Endpoint": "kiri-layers"}], "Dynamic": false});
        write_bytes(&dev.instance("kiri-layers-build"), i.to_string().as_bytes()).unwrap();
        dev.choose(With::Kiri).unwrap();
        assert!(dev.taken() && !main.taken());
        // Its app deleted: the daemon hands back (daemon/mod.rs).
        dev.give_back().unwrap();
        assert!(main.taken(), "builds go to the first Kiri again");
        assert_eq!(dev.saved(), Some(With::Kiri), "the choice stays");
        // Taken up again, it takes them again.
        dev.apply(None).unwrap();
        assert!(dev.taken());
    }

    #[test]
    fn switching_back_restores_exactly() {
        let d = scratch("restore");
        let p = places(&d);
        builder(&p);
        let cfg = json!({"auths": {"ghcr.io": {}}, "currentContext": "orbstack", "aliases": {"foo": "bar"}});
        write_json(&p.config(), &cfg).unwrap();
        let current = r#"{"Key":"orbstack","Name":"mine","Global":false}"#;
        write_bytes(&p.current(), current.as_bytes()).unwrap();
        // Another engine: nothing happens until asked.
        p.apply(Some("OrbStack")).unwrap();
        assert!(p.ask(Some("OrbStack")));
        assert_eq!(read(&p.current()).as_deref(), Some(current));
        assert_eq!(read_json(&p.config()).unwrap(), cfg);
        p.choose(With::Kiri).unwrap();
        assert!(p.taken());
        assert_eq!(
            read_json(&p.config()).unwrap()["aliases"],
            json!({"foo": "bar", "builder": "buildx"})
        );
        // Choosing again keeps the first snapshot.
        p.choose(With::Kiri).unwrap();
        p.choose(With::Docker).unwrap();
        assert_eq!(read_json(&p.config()).unwrap(), cfg);
        assert_eq!(read(&p.current()).as_deref(), Some(current));
        assert_eq!(p.status(Some("OrbStack"))["with"], "docker");
    }

    #[test]
    fn an_alias_of_the_users_own_comes_back() {
        let d = scratch("alias");
        let p = places(&d);
        builder(&p);
        write_json(&p.config(), &json!({"aliases": {"builder": "mybuilder"}})).unwrap();
        p.choose(With::Kiri).unwrap();
        assert_eq!(
            read_json(&p.config()).unwrap()["aliases"]["builder"],
            "buildx"
        );
        p.choose(With::Docker).unwrap();
        assert_eq!(
            read_json(&p.config()).unwrap(),
            json!({"aliases": {"builder": "mybuilder"}})
        );
        // An empty `aliases` that was there stays.
        write_json(&p.config(), &json!({"aliases": {}})).unwrap();
        p.choose(With::Kiri).unwrap();
        p.choose(With::Docker).unwrap();
        assert_eq!(read_json(&p.config()).unwrap(), json!({"aliases": {}}));
    }

    #[test]
    fn a_change_made_since_is_left_alone() {
        let d = scratch("since");
        let p = places(&d);
        builder(&p);
        p.choose(With::Kiri).unwrap();
        // `docker buildx use other` afterwards: apply doesn't take it back.
        let theirs = r#"{"Key":"kiri","Name":"other","Global":false}"#;
        write_bytes(&p.current(), theirs.as_bytes()).unwrap();
        p.apply(None).unwrap();
        assert_eq!(read(&p.current()).as_deref(), Some(theirs));
        // Back to Docker: their builder stays, the alias goes.
        p.choose(With::Docker).unwrap();
        assert_eq!(read(&p.current()).as_deref(), Some(theirs));
        assert_eq!(read_json(&p.config()).unwrap(), json!({}));
    }

    #[test]
    fn a_builder_of_another_name_kiri_is_not_replaced() {
        let d = scratch("foreign");
        let p = places(&d);
        builder(&p);
        let foreign = json!({"Name": "kiri", "Driver": "remote", "Nodes": [{"Name": "kiri0", "Endpoint": "tcp://x:1234"}]});
        write_json(&p.instance("kiri"), &foreign).unwrap();
        p.choose(With::Docker).unwrap();
        assert_eq!(read_json(&p.instance("kiri")).unwrap(), foreign);
        p.uninstall().unwrap();
        assert!(p.instance("kiri").is_file());
    }

    #[test]
    fn uninstall_gives_everything_back() {
        let d = scratch("uninstall");
        let p = places(&d);
        builder(&p);
        p.apply(None).unwrap();
        assert!(p.taken());
        p.uninstall().unwrap();
        assert_eq!(read_json(&p.config()).unwrap(), json!({}));
        assert!(!p.current().exists() && !p.instance("kiri").exists() && !p.state.exists());
    }
}
