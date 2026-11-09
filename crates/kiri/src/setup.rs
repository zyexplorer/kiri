//! `kiri setup`: makes this Mac use Kiri the way OrbStack's installer does,
//! without touching anything another engine owns.
//!
//! - a `kiri` docker context (`--use` also makes it the current one)
//! - a LaunchAgent that starts `kiri daemon` at login (`--no-agent` skips it)
//! - the app's bundled `docker`, Compose and buildx, when run from Kiri.app
//! - `docker` and `kiri` in `~/.kiri/bin`, which a marked line in the shell
//!   profiles (zsh's, bash's, and fish's if used) adds to the end of PATH,
//!   so a docker CLI already installed keeps priority; and
//!   `/usr/local/bin/kiri` when that directory is the user's
//! - on a Mac with no other Docker engine, `kiri` as docker's default context
//! - a buildx builder, `kiri-build`, that loads what it builds into Kiri,
//!   and the choice of builder for plain `docker build` (src/buildwith.rs)
//! - `*.kiri.local` names for containers: `/etc/resolver/kiri.local`, which
//!   needs an administrator once (`--no-domains` skips it; `--domains` does
//!   only this)
//! - `/var/run/docker.sock` linked to Kiri's socket, for tools that ignore
//!   docker contexts (docker-py, Go's `client.FromEnv`, docker-java,
//!   Testcontainers, IDE plugins), when no other engine has it: asked for
//!   with `*.kiri.local`'s password (`--docker-socket` does only this), and
//!   put back at boot by a LaunchDaemon. Until then, or if the password is
//!   declined, `~/.testcontainers.properties` points Testcontainers at Kiri,
//!   unless the user has a file of their own there
//!
//! `kiri setup --uninstall` takes back exactly those, and the `kiri` kubectl
//! context that `kiri k8s` adds.
//!
//! Kiri Dev (src/flavor.rs) sets up only its own: the `kiri-dev` context,
//! its LaunchAgent, its builder and `~/.kiri-dev/bin`. PATH, docker's
//! default context and `*.kiri.local` stay the installed Kiri's.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

// The bundle identifier (`APP_ID`) named in the LaunchAgent makes macOS
// say "Kiri" in Login Items, rather than the name on the signing
// certificate.
use crate::flavor::{AGENT, APP_ID, CONTEXT, dev};

pub use crate::builds::ensure_builder;
use crate::builds::{BUILDER, builder};

/// `~/.kiri/bin`, or `~/.kiri-dev/bin`.
fn bin_dir() -> io::Result<PathBuf> {
    Ok(home()?.join(crate::flavor::STATE.get()).join("bin"))
}

fn home() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))
}

fn docker_dir() -> io::Result<PathBuf> {
    match std::env::var_os("DOCKER_CONFIG") {
        Some(d) => Ok(PathBuf::from(d)),
        None => Ok(home()?.join(".docker")),
    }
}

/// The docker CLI's directory for a context: named by the SHA-256 of its name.
fn context_dir(name: &str) -> io::Result<PathBuf> {
    let digest = kiri_image::store::sha256_digest(name.as_bytes());
    Ok(docker_dir()?
        .join("contexts/meta")
        .join(digest.trim_start_matches("sha256:")))
}

fn read_json(path: &Path) -> io::Result<Value> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e),
    }
}

/// Writes JSON through a temporary file, so a crash never leaves half a config.
fn write_json(path: &Path, v: &Value) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("kiri-tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(v).map_err(io::Error::other)?,
    )?;
    std::fs::rename(tmp, path)
}

/// Kiri.app's `Contents`, when this binary runs from the app.
fn bundle_contents() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let contents = exe.parent()?.parent()?.to_path_buf();
    contents.join("Info.plist").is_file().then_some(contents)
}

/// Whether this process is Kiri.app being opened from Finder or the Dock:
/// the app's binary, started by launchd (LaunchServices), not from a shell,
/// where running it bare must not change anything.
pub fn launched_from_app() -> bool {
    // SAFETY: getppid cannot fail.
    bundle_contents().is_some() && unsafe { libc::getppid() } == 1
}

/// The first time the app opens: the same as `kiri setup`, quietly, except
/// `*.kiri.local`, which the window offers and the user chooses.
pub fn first_launch() -> io::Result<()> {
    if agent_plist()?.is_file() && agent_runs_this()? {
        // Set up by an older Kiri: make sure the commands are there too.
        if let Some(contents) = bundle_contents() {
            let _ = bundled_cli(&contents);
        }
        let _ = name_agent();
        if !dev() {
            let _ = crate::daemon::socket_path().map(|s| docker_socket_fallback(&s));
        }
        return Ok(());
    }
    run(&["--no-domains".to_owned()])
}

pub fn run(args: &[String]) -> io::Result<()> {
    let flag = |f: &str| args.iter().any(|a| a == f);
    if flag("--uninstall") {
        return uninstall();
    }
    if flag("--domains") {
        return if dev() {
            Err(io::Error::other(format!(
                "*.kiri.local belongs to the installed Kiri, not {}",
                crate::flavor::APP
            )))
        } else {
            domains()
        };
    }
    if flag("--docker-socket") {
        if dev() {
            return Err(io::Error::other(format!(
                "/var/run/docker.sock belongs to the installed Kiri, not {}",
                crate::flavor::APP
            )));
        }
        let result = docker_socket();
        println!("{}", docker_socket_fallback(&crate::daemon::socket_path()?));
        return result;
    }
    let socket = crate::daemon::socket_path()?;
    context(&socket)?;
    println!("docker context `{CONTEXT}` → unix://{}", socket.display());
    if flag("--use") && !dev() {
        let path = docker_dir()?.join("config.json");
        let mut cfg = read_json(&path)?;
        cfg["currentContext"] = CONTEXT.get().into();
        write_json(&path, &cfg)?;
        println!("docker now uses Kiri by default (`docker context use default` to switch back)");
    }
    if let Some(contents) = bundle_contents() {
        bundled_cli(&contents)?;
    }
    if !flag("--no-agent") {
        let plist = agent()?;
        println!("kiri daemon starts at login ({})", plist.display());
    }
    match builder() {
        Ok(true) => match crate::buildwith::places().map(|p| p.saved()) {
            Ok(Some(crate::buildwith::With::Kiri)) => {
                println!("docker build uses BuildKit in Kiri (builder `{BUILDER}`)")
            }
            _ => println!(
                "docker build follows docker's context, and builds on Kiri when docker uses Kiri (`kiri build-with kiri` sends every build to Kiri)"
            ),
        },
        Ok(false) => {}
        Err(e) => println!(
            "docker build: the `{BUILDER}` builder comes later ({e}); the window makes it once Kiri is running"
        ),
    }
    if !flag("--no-domains") && !dev() {
        // domains() asks for /var/run/docker.sock with the same password;
        // with the names already there, it is asked for on its own.
        let named = crate::daemon::dns::installed();
        if let Err(e) = domains() {
            println!(
                "container names stay on *.kiri.localhost ({e}); `kiri setup --domains` tries again"
            );
        } else if named {
            if let Err(e) = docker_socket() {
                println!("{DOCKER_SOCK} stays as it is ({e})");
            }
        }
    }
    if !dev() {
        println!("{}", docker_socket_fallback(&socket));
    }
    Ok(())
}

/// Points macOS at Kiri for `*.kiri.local`. Writing /etc/resolver needs an
/// administrator, so macOS asks for the password, once.
pub fn domains() -> io::Result<()> {
    use crate::daemon::dns;
    if dns::installed() {
        println!("containers answer at http://<name>.kiri.local");
        return Ok(());
    }
    let tmp = std::env::temp_dir().join(format!("kiri-resolver-{}", std::process::id()));
    std::fs::write(
        &tmp,
        format!(
            "# Kiri: container names (kiri setup --uninstall removes this)\nnameserver 127.0.0.1\nport {}\n",
            dns::PORT
        ),
    )?;
    // /var/run/docker.sock with the same password, when it is free. Its
    // failing must not undo the names: how it went is read back after.
    let link = docker_socket_script()?;
    let (command, why) = match &link {
        Some(l) => (
            format!(
                "mkdir -p /etc/resolver && cp '{}' {} && chmod 644 {} && ({l} || true)",
                tmp.display(),
                dns::RESOLVER,
                dns::RESOLVER
            ),
            "Kiri wants to give your containers names like myapp.kiri.local, and to answer at /var/run/docker.sock for tools that look for Docker there.",
        ),
        None => (
            format!(
                "mkdir -p /etc/resolver && cp '{}' {} && chmod 644 {}",
                tmp.display(),
                dns::RESOLVER,
                dns::RESOLVER
            ),
            "Kiri wants to give your containers names like myapp.kiri.local.",
        ),
    };
    let result = as_admin(&command, why);
    let _ = std::fs::remove_file(&tmp);
    if let Some(l) = &link {
        let _ = std::fs::remove_file(&l.tmp);
        let _ = docker_socket_fallback(&crate::daemon::socket_path()?);
    }
    result?;
    println!("containers answer at http://<name>.kiri.local");
    Ok(())
}

/// Where tools that ignore docker contexts look for Docker: docker-py, Go's
/// `client.FromEnv`, docker-java, Testcontainers, many IDE plugins.
const DOCKER_SOCK: &str = "/var/run/docker.sock";
/// macOS empties /var/run at every boot, so a LaunchDaemon puts Kiri's link
/// back then, only if nothing else has taken the path by that time. Docker
/// Desktop's and OrbStack's helpers replace it when they start, as they
/// replace each other's.
const SOCKET_LABEL: &str = "dev.kiri.docker-socket";
const SOCKET_DAEMON: &str = "/Library/LaunchDaemons/dev.kiri.docker-socket.plist";

/// Who has /var/run/docker.sock.
#[derive(Debug, PartialEq)]
enum DockerSock {
    Kiri,
    Free,
    /// Another engine: what it points at.
    Taken(String),
}

/// The engines whose links stay theirs while the app is installed, running
/// or not: their socket is missing only until they start.
fn other_engine_dirs() -> Vec<PathBuf> {
    let Ok(h) = home() else { return Vec::new() };
    [
        ("/Applications/Docker.app", ".docker"),
        ("/Applications/OrbStack.app", ".orbstack"),
        ("/Applications/Rancher Desktop.app", ".rd"),
    ]
    .iter()
    .filter(|(app, _)| Path::new(app).exists())
    .map(|(_, dir)| h.join(dir))
    .collect()
}

fn docker_sock(ours: &Path) -> DockerSock {
    docker_sock_at(Path::new(DOCKER_SOCK), ours, &other_engine_dirs())
}

fn docker_sock_at(path: &Path, ours: &Path, engines: &[PathBuf]) -> DockerSock {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return DockerSock::Free;
    };
    if meta.file_type().is_symlink() {
        let Ok(target) = std::fs::read_link(path) else {
            return DockerSock::Taken(path.display().to_string());
        };
        if target == ours {
            return DockerSock::Kiri;
        }
        if target.exists() || engines.iter().any(|e| target.starts_with(e)) {
            return DockerSock::Taken(target.display().to_string());
        }
        // Left behind by an engine that has been removed.
        return DockerSock::Free;
    }
    // A socket (or anything else) of its own: an engine's, serving or not.
    DockerSock::Taken(path.display().to_string())
}

/// `'…'` for sh.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The root commands that link /var/run/docker.sock to Kiri's socket and
/// install the LaunchDaemon that puts it back at boot.
struct LinkScript {
    command: String,
    /// The LaunchDaemon's plist, written here for the command to copy.
    tmp: PathBuf,
}

impl std::fmt::Display for LinkScript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.command)
    }
}

/// The commands for when /var/run/docker.sock is free; None when it is
/// Kiri's already or another engine's, or for Kiri Dev.
fn docker_socket_script() -> io::Result<Option<LinkScript>> {
    let ours = crate::daemon::socket_path()?;
    if dev() || docker_sock(&ours) != DockerSock::Free {
        return Ok(None);
    }
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;");
    let ours_s = ours.to_string_lossy();
    let boot =
        format!("[ -e {DOCKER_SOCK} ] || [ -L {DOCKER_SOCK} ] || ln -s \"$1\" {DOCKER_SOCK}");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{SOCKET_LABEL}</string>
  <key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>{}</string><string>kiri-docker-socket</string><string>{}</string></array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
        esc(&boot),
        esc(&ours_s)
    );
    let tmp = std::env::temp_dir().join(format!("kiri-docker-socket-{}.plist", std::process::id()));
    std::fs::write(&tmp, plist)?;
    // `[ ! -e ]` again as root: another engine may have started since. A
    // link left dangling by a removed engine is replaced.
    let command = format!(
        "cp {} {SOCKET_DAEMON} && chown root:wheel {SOCKET_DAEMON} && chmod 644 {SOCKET_DAEMON} && [ ! -e {DOCKER_SOCK} ] && rm -f {DOCKER_SOCK} && ln -s {} {DOCKER_SOCK}",
        sh_quote(&tmp.to_string_lossy()),
        sh_quote(&ours_s)
    );
    Ok(Some(LinkScript { command, tmp }))
}

/// `kiri setup --docker-socket`: /var/run/docker.sock to Kiri, if free
/// (macOS asks for the password).
pub fn docker_socket() -> io::Result<()> {
    let Some(l) = docker_socket_script()? else {
        return Ok(());
    };
    let result = as_admin(
        &l.command,
        "Kiri wants to answer at /var/run/docker.sock, for tools that look for Docker there.",
    );
    let _ = std::fs::remove_file(&l.tmp);
    result
}

/// Says where tools that ignore docker contexts find Kiri, and keeps
/// `~/.testcontainers.properties` for when /var/run/docker.sock is not
/// Kiri's but could be (a declined password, not yet asked).
fn docker_socket_fallback(ours: &Path) -> String {
    match docker_sock(ours) {
        DockerSock::Kiri => {
            let _ = testcontainers(ours, false);
            format!(
                "{DOCKER_SOCK} → Kiri (docker-py, Testcontainers and other tools that ignore docker contexts)"
            )
        }
        DockerSock::Taken(t) => {
            let _ = testcontainers(ours, false);
            format!(
                "{DOCKER_SOCK} is another engine's ({t}) and stays so; DOCKER_HOST=unix://{} points a tool at Kiri",
                ours.display()
            )
        }
        DockerSock::Free => match testcontainers(ours, true) {
            Ok(true) => format!(
                "{DOCKER_SOCK} is not Kiri's yet (`kiri setup --docker-socket` asks for the password); Testcontainers finds Kiri through ~/.testcontainers.properties"
            ),
            _ => format!(
                "{DOCKER_SOCK} is not Kiri's yet (`kiri setup --docker-socket` asks for the password); until then DOCKER_HOST=unix://{} points a tool at Kiri",
                ours.display()
            ),
        },
    }
}

/// The first line of the `~/.testcontainers.properties` Kiri writes: only a
/// file that starts with it is Kiri's to change or remove.
const TESTCONTAINERS_MARK: &str = "# Written by Kiri while /var/run/docker.sock is not Kiri's (kiri setup --uninstall removes it)";

fn testcontainers_text(ours: &Path) -> String {
    format!(
        "{TESTCONTAINERS_MARK}\ndocker.host=unix://{}\n",
        ours.display()
    )
}

/// Writes (`on`) or removes Kiri's `~/.testcontainers.properties`. A file
/// the user (or Testcontainers itself) wrote is never touched. True when
/// Kiri's file is there afterwards.
fn testcontainers(ours: &Path, on: bool) -> io::Result<bool> {
    testcontainers_at(&home()?.join(".testcontainers.properties"), ours, on)
}

fn testcontainers_at(path: &Path, ours: &Path, on: bool) -> io::Result<bool> {
    let ours_text = testcontainers_text(ours);
    match std::fs::read_to_string(path) {
        // Only exactly what Kiri wrote: Testcontainers adds keys of its own.
        Ok(text)
            if text.starts_with(TESTCONTAINERS_MARK)
                && text
                    .lines()
                    .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
                    .all(|l| l.starts_with("docker.host=")) =>
        {
            if on {
                if text != ours_text {
                    std::fs::write(path, ours_text)?;
                }
                Ok(true)
            } else {
                std::fs::remove_file(path)?;
                Ok(false)
            }
        }
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if on {
                std::fs::write(path, ours_text)?;
            }
            Ok(on)
        }
        Err(e) => Err(e),
    }
}

/// For the window, after the password prompt: how /var/run/docker.sock
/// went, if it is not Kiri's.
pub fn docker_socket_note() -> Option<String> {
    let ours = crate::daemon::socket_path().ok()?;
    match docker_sock(&ours) {
        DockerSock::Free if !dev() => Some(docker_socket_fallback(&ours)),
        _ => None,
    }
}

/// Takes /var/run/docker.sock back, if it is still Kiri's, and the
/// LaunchDaemon that put it there.
fn remove_docker_socket() -> io::Result<()> {
    let ours = crate::daemon::socket_path()?;
    let _ = testcontainers(&ours, false);
    let linked = docker_sock(&ours) == DockerSock::Kiri;
    if !linked && !Path::new(SOCKET_DAEMON).exists() {
        return Ok(());
    }
    let mut command =
        format!("launchctl bootout system/{SOCKET_LABEL} 2>/dev/null; rm -f {SOCKET_DAEMON}");
    if linked {
        command.push_str(&format!(
            "; [ \"$(readlink {DOCKER_SOCK})\" = {} ] && rm -f {DOCKER_SOCK}; true",
            sh_quote(&ours.to_string_lossy())
        ));
    }
    as_admin(&command, "Kiri wants to remove its /var/run/docker.sock.")?;
    println!("removed {DOCKER_SOCK}");
    Ok(())
}

/// Takes `*.kiri.local` back; containers stay on `*.kiri.localhost`.
pub fn remove_domains() -> io::Result<()> {
    let resolver = crate::daemon::dns::RESOLVER;
    if !Path::new(resolver).exists() {
        return Ok(());
    }
    as_admin(
        &format!("rm -f {resolver}"),
        "Kiri wants to remove its *.kiri.local names.",
    )?;
    println!("removed *.kiri.local");
    Ok(())
}

/// Runs a shell command as root through macOS's own password prompt. The
/// script runs in this process, not through `osascript`, so the prompt says
/// "Kiri wants to make changes". AppleScript belongs on the main thread: the
/// window's workers hand it there, and the CLI is already on it.
fn as_admin(command: &str, why: &str) -> io::Result<()> {
    let script = format!(
        "do shell script \"{}\" with prompt \"{}\" with administrator privileges",
        command.replace('"', "\\\""),
        why.replace('"', "\\\"")
    );
    // SAFETY: pthread_main_np only reads the calling thread's state.
    if unsafe { libc::pthread_main_np() } == 1 {
        return apple_script(&script);
    }
    let mut result = None;
    dispatch2::DispatchQueue::main().exec_sync(|| result = Some(apple_script(&script)));
    result.unwrap_or_else(|| Err(io::Error::other("the password prompt did not run")))
}

fn apple_script(source: &str) -> io::Result<()> {
    use objc2::AllocAnyThread;
    use objc2_foundation::{NSAppleScript, NSString};
    let script = NSAppleScript::initWithSource(NSAppleScript::alloc(), &NSString::from_str(source))
        .ok_or_else(|| io::Error::other("bad AppleScript"))?;
    let mut error: Option<
        objc2::rc::Retained<objc2_foundation::NSDictionary<NSString, objc2::runtime::AnyObject>>,
    > = None;
    // Called directly: the generated binding panics on the nil it returns
    // when the script fails.
    // SAFETY: the selector takes an NSDictionary** out-parameter, and the
    // result is an optional NSAppleEventDescriptor.
    let _: Option<objc2::rc::Retained<objc2::runtime::AnyObject>> =
        unsafe { objc2::msg_send![&*script, executeAndReturnError: Some(&mut error)] };
    let Some(info) = error else { return Ok(()) };
    // "message (number)"; the number is -128 when the prompt is cancelled.
    let field = |key: &str| {
        info.objectForKey(&NSString::from_str(key)).map(|v| {
            // SAFETY: every object answers -description with an NSString.
            let d: objc2::rc::Retained<NSString> = unsafe { objc2::msg_send![&*v, description] };
            d.to_string()
        })
    };
    Err(io::Error::other(format!(
        "{} ({})",
        field("NSAppleScriptErrorMessage").unwrap_or_default(),
        field("NSAppleScriptErrorNumber").unwrap_or_default()
    )))
}

/// Kiri Lite, once its tools are downloaded: the CLI links and the build
/// builder that `kiri setup` could not make without them.
pub fn after_tools() -> io::Result<()> {
    if let Some(contents) = bundle_contents() {
        bundled_cli(&contents)?;
    }
    builder().map(drop)
}

/// The context, as `docker context create kiri --docker host=…` writes it.
fn context(socket: &Path) -> io::Result<()> {
    let meta = json!({
        "Name": CONTEXT.get(),
        "Metadata": {"Description": crate::flavor::APP.get()},
        "Endpoints": {"docker": {"Host": format!("unix://{}", socket.display()), "SkipTLSVerify": false}},
    });
    write_json(&context_dir(CONTEXT.get())?.join("meta.json"), &meta)
}

/// Makes `name` the context plain `docker` uses: Kiri's own (made first if
/// missing), "default", or another context that exists, to switch back to.
pub fn use_context(name: &str) -> io::Result<()> {
    if name == CONTEXT.get() {
        context(&crate::daemon::socket_path()?)?;
    } else if name != "default" && !context_dir(name)?.join("meta.json").is_file() {
        return Err(io::Error::other(format!(
            "there is no docker context `{name}`"
        )));
    }
    let path = docker_dir()?.join("config.json");
    let mut cfg = read_json(&path)?;
    match cfg.as_object_mut() {
        Some(o) if name == "default" => {
            o.remove("currentContext");
        }
        _ => cfg["currentContext"] = name.into(),
    }
    write_json(&path, &cfg)
}

/// Links `kiri` and the app's `docker` into `~/.kiri/bin`, puts that
/// directory on PATH, and adds docker's plugins (Compose, buildx) to the
/// CLI's plugin search path. An existing `docker` on PATH is left alone.
/// The docker CLI, Compose and buildx are bundled in Kiri.app, or in Kiri
/// Lite downloaded to tools::dir() (nothing yet if not downloaded); `kiri`
/// is linked either way.
fn bundled_cli(contents: &Path) -> io::Result<()> {
    let ours = bin_dir()?;
    std::fs::create_dir_all(&ours)?;
    // `kiri` itself, for `kiri k8s`, `kiri machine`, `kiri import`…
    let kiri = ours.join("kiri");
    let _ = std::fs::remove_file(&kiri);
    std::os::unix::fs::symlink(contents.join("MacOS/kiri"), &kiri)?;
    // Kiri Dev's commands are run by path: ~/.kiri-dev/bin/kiri.
    if !dev() {
        let h = home()?;
        shell_path(&h, &zdotdir(&h))?;
        system_link(&kiri);
    }
    let mut bin = contents.join("Resources/bin");
    if !bin.join("docker").is_file() {
        match crate::tools::dir() {
            Ok(d) if crate::tools::installed() => bin = d,
            _ => return Ok(()),
        }
    }
    let link = ours.join("docker");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(bin.join("docker"), &link)?;
    // Kiri Dev's plugins stay its own: `docker --context kiri-dev`, or
    // ~/.kiri-dev/bin/docker.
    if dev() {
        println!("docker and kiri commands: {}", ours.display());
        return Ok(());
    }
    default_context()?;
    let plugins = bin.join("cli-plugins").to_string_lossy().into_owned();
    let path = docker_dir()?.join("config.json");
    let mut cfg = read_json(&path)?;
    let mut dirs: Vec<Value> = cfg["cliPluginsExtraDirs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !dirs.iter().any(|d| d.as_str() == Some(plugins.as_str())) {
        dirs.push(plugins.into());
    }
    cfg["cliPluginsExtraDirs"] = dirs.into();
    write_json(&path, &cfg)?;
    println!(
        "docker and kiri commands: {} (on PATH in new terminal windows)",
        ours.display()
    );
    Ok(())
}

/// `/usr/local/bin/kiri`, when the user can write there (Homebrew on Intel
/// Macs makes it theirs). That directory is on every shell's PATH already,
/// so `kiri` works even in a terminal opened before Kiri was. A `kiri`
/// there that isn't Kiri's is left alone.
const SYSTEM_LINK: &str = "/usr/local/bin/kiri";

fn system_link(kiri: &Path) {
    let link = Path::new(SYSTEM_LINK);
    // Missing, or a link an older Kiri left (to a Kiri.app or ~/.kiri/bin
    // that moved, or to nothing): made again to this one. Anything else there
    // isn't Kiri's to replace; stale::stale_kiri() names it.
    let replace = match std::fs::read_link(link) {
        Ok(target) => target != kiri && (kiri_link(&target) || !link.exists()),
        Err(_) => link.symlink_metadata().is_err(),
    };
    if replace {
        let _ = std::fs::remove_file(link);
        let _ = std::os::unix::fs::symlink(kiri, link);
    }
}

/// A link target that is some Kiri's `kiri`: inside a Kiri app bundle, or a
/// Kiri bin directory (~/.kiri/bin, ~/.kiri-dev/bin).
fn kiri_link(target: &Path) -> bool {
    let t = target.to_string_lossy();
    t.ends_with("/kiri")
        && (t.contains(".app/Contents/MacOS/")
            || t.contains("/.kiri/bin/")
            || t.contains("/.kiri-dev/bin/"))
}

/// Whether `/usr/local/bin/kiri` is the link system_link() makes.
fn system_link_is_ours() -> bool {
    bin_dir().is_ok_and(|b| std::fs::read_link(SYSTEM_LINK).is_ok_and(|t| t == b.join("kiri")))
}

/// Every other process running this app's `kiri` binary, asked to quit,
/// then made to after a few seconds.
fn stop_others() {
    let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|e| std::fs::canonicalize(e).ok())
    else {
        return;
    };
    let me = std::process::id().to_string();
    let pattern = format!("^{}( |$)", regex_escape(&exe.to_string_lossy()));
    for (sig, wait) in [("-TERM", 3), ("-KILL", 0)] {
        let Ok(out) = Command::new("/usr/bin/pgrep")
            .args(["-f", &pattern])
            .output()
        else {
            return;
        };
        let pids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter(|p| *p != me)
            .map(str::to_owned)
            .collect();
        if pids.is_empty() {
            return;
        }
        let _ = Command::new("/bin/kill").arg(sig).args(&pids).status();
        std::thread::sleep(std::time::Duration::from_secs(wait));
    }
}

fn regex_escape(s: &str) -> String {
    s.chars().fold(String::new(), |mut out, c| {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

/// The marked lines Kiri adds to shell profiles, and removes on uninstall.
const PROFILE_MARK: &str =
    "# Added by Kiri: its docker and kiri commands (kiri setup --uninstall removes this)";

/// Adds `~/.kiri/bin` to the end of PATH once, however many of the
/// profiles a shell reads. Plain sh, for zsh and bash alike.
const SH_LINE: &str =
    r#"case ":$PATH:" in *":$HOME/.kiri/bin:"*) ;; *) export PATH="$PATH:$HOME/.kiri/bin" ;; esac"#;

/// The line Kiri 0.1.0 wrote, only to `~/.zprofile`; taken back on uninstall.
const OLD_SH_LINE: &str = "export PATH=\"$PATH:$HOME/.kiri/bin\"";

const FISH_LINE: &str = "fish_add_path --append $HOME/.kiri/bin";

/// The shell profiles Kiri puts `~/.kiri/bin` in, so `kiri` is found in
/// every terminal:
/// - zsh (the macOS default) reads `.zprofile` only in login shells, which
///   Terminal opens but VS Code, tmux and IDE terminals do not, and a
///   `.zshrc` that sets PATH runs after it: both files, in `$ZDOTDIR` if set
/// - bash reads the first of `.bash_profile`, `.bash_login` and `.profile`
///   in login shells (`.bash_profile` is made if there is none) and
///   `.bashrc` in others, when the user has one
/// - fish's `conf.d`, when the user has fish
fn profiles(home: &Path, zdotdir: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut v = vec![
        (zdotdir.join(".zprofile"), SH_LINE),
        (zdotdir.join(".zshrc"), SH_LINE),
    ];
    let login = [".bash_profile", ".bash_login", ".profile"]
        .iter()
        .map(|f| home.join(f))
        .find(|p| p.is_file());
    v.push((login.unwrap_or_else(|| home.join(".bash_profile")), SH_LINE));
    if home.join(".bashrc").is_file() {
        v.push((home.join(".bashrc"), SH_LINE));
    }
    if home.join(".config/fish").is_dir() {
        v.push((home.join(".config/fish/conf.d/kiri.fish"), FISH_LINE));
    }
    v
}

fn zdotdir(home: &Path) -> PathBuf {
    std::env::var_os("ZDOTDIR")
        .filter(|d| !d.is_empty())
        .map_or_else(|| home.to_path_buf(), PathBuf::from)
}

fn shell_path(home: &Path, zdotdir: &Path) -> io::Result<()> {
    for (file, line) in profiles(home, zdotdir) {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        if text.contains(PROFILE_MARK) {
            continue;
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let sep = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        std::fs::write(&file, format!("{text}{sep}\n{PROFILE_MARK}\n{line}\n"))?;
    }
    Ok(())
}

fn remove_shell_path(home: &Path, zdotdir: &Path) -> io::Result<()> {
    for (file, line) in profiles(home, zdotdir) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if !text.contains(PROFILE_MARK) {
            continue;
        }
        let mut kept = text;
        for line in [line, OLD_SH_LINE] {
            kept = kept
                .replace(&format!("\n{PROFILE_MARK}\n{line}\n"), "\n")
                .replace(&format!("{PROFILE_MARK}\n{line}\n"), "");
        }
        if kept.trim().is_empty() && file.extension().is_some_and(|e| e == "fish") {
            std::fs::remove_file(&file)?;
        } else {
            std::fs::write(&file, kept)?;
        }
    }
    Ok(())
}

/// On a Mac with no other Docker engine, plain `docker` uses Kiri. With
/// OrbStack or Docker Desktop around, the user's choice stays as it is.
fn default_context() -> io::Result<()> {
    let h = home()?;
    // Kiri's own /var/run/docker.sock is not another engine.
    let ours = crate::daemon::socket_path()?;
    let system = !matches!(docker_sock(&ours), DockerSock::Kiri) && Path::new(DOCKER_SOCK).exists();
    let other = system
        || [
            h.join(".orbstack/run/docker.sock"),
            h.join(".docker/run/docker.sock"),
        ]
        .iter()
        .any(|p| p.exists())
        || Path::new("/Applications/OrbStack.app").exists()
        || Path::new("/Applications/Docker.app").exists();
    let path = docker_dir()?.join("config.json");
    let mut cfg = read_json(&path)?;
    if other
        || cfg["currentContext"]
            .as_str()
            .is_some_and(|c| !c.is_empty() && c != "default")
    {
        return Ok(());
    }
    cfg["currentContext"] = CONTEXT.get().into();
    write_json(&path, &cfg)?;
    println!("docker uses Kiri by default (no other Docker engine on this Mac)");
    Ok(())
}

fn agent_plist() -> io::Result<PathBuf> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("{AGENT}.plist")))
}

fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

/// A LaunchAgent for `kiri daemon`: started at login, restarted if it
/// crashes, never restarted after a clean exit.
fn agent() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let log = home()?.join(crate::flavor::STATE.get()).join("daemon.log");
    let esc = |p: &Path| {
        p.to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
    };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{AGENT}</string>
  <key>AssociatedBundleIdentifiers</key><array><string>{APP_ID}</string></array>
  <key>ProgramArguments</key><array><string>{}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        esc(&exe),
        esc(&log),
        esc(&log)
    );
    let path = agent_plist()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, plist)?;
    // Reload, so a new path to the binary takes effect.
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{}/{AGENT}", uid())])
        .stderr(std::process::Stdio::null())
        .status();
    // bootout returns before launchd has let the old job go, and a
    // bootstrap in that moment fails (error 5): try again for a few seconds.
    let mut ok = false;
    for _ in 0..20 {
        ok = Command::new("/bin/launchctl")
            .args(["bootstrap", &format!("gui/{}", uid())])
            .arg(&path)
            .stderr(std::process::Stdio::null())
            .status()?
            .success();
        if ok {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if !ok {
        return Err(io::Error::other(format!(
            "launchctl could not load {}",
            path.display()
        )));
    }
    Ok(path)
}

/// Whether the LaunchAgent starts this copy of Kiri. One left by another
/// copy (an older download, a build elsewhere) is replaced when this one opens.
fn agent_runs_this() -> io::Result<bool> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let esc = exe
        .to_string_lossy()
        .replace('&', "&amp;")
        .replace('<', "&lt;");
    Ok(std::fs::read_to_string(agent_plist()?)?
        .contains(&format!("<string>{esc}</string><string>daemon</string>")))
}

/// Adds the app's name to a LaunchAgent written by an older Kiri. launchd
/// reads it at the next login, so the running daemon is left alone.
fn name_agent() -> io::Result<()> {
    let path = agent_plist()?;
    let text = std::fs::read_to_string(&path)?;
    if text.contains("AssociatedBundleIdentifiers") {
        return Ok(());
    }
    let named = text.replacen("  <key>ProgramArguments</key>", &format!("  <key>AssociatedBundleIdentifiers</key><array><string>{APP_ID}</string></array>\n  <key>ProgramArguments</key>"), 1);
    std::fs::write(&path, named)
}

/// The docker CLI to drive buildx with: on PATH, else the app's.
pub fn docker_cli() -> Option<PathBuf> {
    crate::client::docker_cli()
}

fn uninstall() -> io::Result<()> {
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{}/{AGENT}", uid())])
        .stderr(std::process::Stdio::null())
        .status();
    // Uninstalling stops everything this Kiri runs: the engine (the agent,
    // just booted out, which stops the VM and its containers) and any window
    // or kiri command still open from this app (`kiri share`, a second
    // daemon). Not this process: it's the one uninstalling.
    stop_others();
    match std::fs::remove_file(agent_plist()?) {
        Ok(()) => println!("removed the LaunchAgent"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Only a context that still points at Kiri's socket is ours to remove.
    let dir = context_dir(CONTEXT.get())?;
    let meta = read_json(&dir.join("meta.json"))?;
    if meta["Endpoints"]["docker"]["Host"]
        .as_str()
        .is_some_and(|h| h.ends_with(&format!("{}/run/docker.sock", crate::flavor::STATE)))
    {
        std::fs::remove_dir_all(&dir)?;
        println!("removed docker context `{CONTEXT}`");
    }
    let path = docker_dir()?.join("config.json");
    let mut cfg = read_json(&path)?;
    let mut changed = false;
    if cfg["currentContext"].as_str() == Some(CONTEXT.get()) {
        if let Some(o) = cfg.as_object_mut() {
            o.remove("currentContext");
        }
        changed = true;
    }
    // `get_mut`: indexing would add the key to a config that lacks it.
    if let Some(dirs) = cfg
        .get_mut("cliPluginsExtraDirs")
        .and_then(Value::as_array_mut)
    {
        let before = dirs.len();
        dirs.retain(|d| {
            !d.as_str()
                .is_some_and(|s| s.contains("Kiri.app/Contents/Resources/bin"))
        });
        changed |= dirs.len() != before;
    }
    if changed {
        write_json(&path, &cfg)?;
    }
    // Docker's builder back as it was, before Kiri's goes.
    if let Err(e) = crate::buildwith::places().and_then(|p| p.uninstall()) {
        println!("could not give docker build back its own builder ({e})");
    }
    if let Some(docker) = docker_cli() {
        let _ = Command::new(docker)
            .args(["buildx", "rm", BUILDER.get()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    if !dev() && system_link_is_ours() {
        let _ = std::fs::remove_file(SYSTEM_LINK);
    }
    let _ = std::fs::remove_file(bin_dir()?.join("docker"));
    let _ = std::fs::remove_file(bin_dir()?.join("kiri"));
    // The cluster's kubectl context (`kiri k8s`); the cluster stays in Kiri's data.
    crate::k8s::forget();
    if dev() {
        println!(
            "{0}'s data stays in ~/Library/Application Support/{0}; delete it to remove images and volumes too",
            crate::flavor::APP
        );
        return Ok(());
    }
    match home().and_then(|h| remove_shell_path(&h, &zdotdir(&h))) {
        Ok(()) => println!("removed ~/.kiri/bin from shell profiles"),
        Err(e) => println!("could not tidy the shell profiles ({e})"),
    }
    if let Err(e) = remove_domains() {
        println!("could not remove {} ({e})", crate::daemon::dns::RESOLVER);
    }
    if let Err(e) = remove_docker_socket() {
        println!("could not remove {DOCKER_SOCK} ({e})");
    }
    println!(
        "Kiri's data stays in ~/Library/Application Support/Kiri; delete it to remove images and volumes too"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn runs_apple_script() {
        super::apple_script("do shell script \"true\"").unwrap();
        let e = super::apple_script("error number -128").unwrap_err();
        assert!(e.to_string().contains("-128"), "{e}");
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("kiri-setup-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn docker_sock_owner() {
        use super::DockerSock::*;
        let d = scratch("sock");
        let ours = d.join("kiri/run/docker.sock");
        let link = d.join("docker.sock");
        let at = |engines: &[std::path::PathBuf]| super::docker_sock_at(&link, &ours, engines);
        assert_eq!(at(&[]), Free);
        std::os::unix::fs::symlink(&ours, &link).unwrap();
        assert_eq!(at(&[]), Kiri, "Kiri's link, even with the daemon stopped");
        // OrbStack's link with OrbStack not running: still OrbStack's.
        std::fs::remove_file(&link).unwrap();
        let orb = d.join(".orbstack/run/docker.sock");
        std::os::unix::fs::symlink(&orb, &link).unwrap();
        assert_eq!(at(&[d.join(".orbstack")]), Taken(orb.display().to_string()));
        // ... and free once OrbStack is gone.
        assert_eq!(at(&[]), Free);
        // Another engine's own socket.
        std::fs::remove_file(&link).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&link).unwrap();
        assert!(matches!(at(&[]), Taken(_)));
        drop(listener);
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "").unwrap();
        assert!(matches!(at(&[]), Taken(_)), "a plain file is left alone");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn testcontainers_file_is_kiris_only_when_marked() {
        let d = scratch("tc");
        let path = d.join(".testcontainers.properties");
        let ours = std::path::Path::new("/Users/me/.kiri/run/docker.sock");
        assert!(super::testcontainers_at(&path, ours, true).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            super::testcontainers_text(ours)
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("docker.host=unix:///Users/me/.kiri/run/docker.sock\n")
        );
        assert!(!super::testcontainers_at(&path, ours, false).unwrap());
        assert!(!path.exists());
        // The user's own file, or one Testcontainers has added to, stays.
        for text in [
            "docker.host=tcp://remote:2375\n",
            &format!(
                "{}testcontainers.reuse.enable=true\n",
                super::testcontainers_text(ours)
            ),
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(!super::testcontainers_at(&path, ours, true).unwrap());
            assert!(!super::testcontainers_at(&path, ours, false).unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(
            super::sh_quote("/Users/o'neil/.kiri"),
            r"'/Users/o'\''neil/.kiri'"
        );
    }

    #[test]
    fn shell_profiles_put_kiri_on_path_once_and_take_it_back() {
        let h = scratch("profiles");
        std::fs::write(h.join(".zprofile"), "export A=1").unwrap();
        // An install by Kiri 0.1.0: the old line, in .zprofile only.
        std::fs::write(
            h.join(".profile"),
            format!(
                "export B=2\n\n{}\n{}\n",
                super::PROFILE_MARK,
                super::OLD_SH_LINE
            ),
        )
        .unwrap();
        std::fs::write(h.join(".bashrc"), "").unwrap();
        super::shell_path(&h, &h).unwrap();
        super::shell_path(&h, &h).unwrap();
        let read = |f: &str| std::fs::read_to_string(h.join(f)).unwrap();
        for f in [".zprofile", ".zshrc", ".bashrc"] {
            assert_eq!(
                read(f).matches(super::SH_LINE).count(),
                1,
                "{f}: {}",
                read(f)
            );
        }
        assert!(read(".zprofile").starts_with("export A=1\n\n"));
        // bash's login file is .profile here: no .bash_profile to shadow it.
        assert!(!h.join(".bash_profile").exists());
        assert!(!read(".profile").contains(super::SH_LINE));
        super::remove_shell_path(&h, &h).unwrap();
        assert_eq!(read(".zprofile").trim_end(), "export A=1");
        assert_eq!(read(".profile").trim_end(), "export B=2");
        assert_eq!(read(".zshrc").trim(), "");
        assert!(!read(".bashrc").contains("kiri"));
    }

    #[test]
    fn path_line_works_in_sh_and_adds_once() {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{0}\n{0}\nprintf %s \"$PATH\"", super::SH_LINE))
            .env("HOME", "/h")
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "/usr/bin:/bin:/h/.kiri/bin"
        );
    }
}
