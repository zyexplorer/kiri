//! `kiri ps`, `kiri run`, `kiri compose up`…: Docker's own commands, run
//! by the docker CLI against Kiri's daemon, whatever docker context is
//! selected. `kiri run -d --name web nginx` is `docker --context kiri run
//! -d --name web nginx`.

use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use crate::client;

/// The docker commands `kiri` answers. `import` is left out: `kiri import`
/// brings things over from OrbStack and Docker Desktop.
pub const COMMANDS: &[&str] = &[
    "attach",
    "build",
    "builder",
    "buildx",
    "commit",
    "compose",
    "container",
    "cp",
    "create",
    "diff",
    "events",
    "exec",
    "export",
    "history",
    "image",
    "images",
    "info",
    "inspect",
    "kill",
    "load",
    "login",
    "logout",
    "logs",
    "manifest",
    "network",
    "pause",
    "port",
    "ps",
    "pull",
    "push",
    "rename",
    "restart",
    "rm",
    "rmi",
    "run",
    "save",
    "search",
    "start",
    "stats",
    "stop",
    "system",
    "tag",
    "top",
    "unpause",
    "update",
    "version",
    "volume",
    "wait",
];

pub fn handles(command: &str) -> bool {
    COMMANDS.contains(&command)
}

/// Replaces this process with `docker <args>`, so the terminal, signals and
/// the exit code are docker's own. Returns only if that fails.
#[cfg(unix)]
pub fn run(args: &[String]) -> io::Result<()> {
    let docker = client::docker_cli()
        .ok_or_else(|| io::Error::other("no docker CLI found; open Kiri once to set it up"))?;
    let socket = client::socket()?;
    if !client::alive() {
        return Err(io::Error::other(
            "Kiri isn't running. Open Kiri, or start it with `kiri daemon`",
        ));
    }
    let mut cmd = std::process::Command::new(docker);
    cmd.args(args)
        .env("DOCKER_HOST", format!("unix://{}", socket.display()))
        .env_remove("DOCKER_CONTEXT");
    // With DOCKER_HOST and no context, buildx would pick the endpoint's own
    // docker-driver builder, which needs BuildKit inside the engine: name
    // Kiri's, if buildx knows it (the window sets it up) and the person
    // hasn't picked one.
    if std::env::var_os("BUILDX_BUILDER").is_none() && buildx_knows(crate::builds::BUILDER.get()) {
        cmd.env("BUILDX_BUILDER", crate::builds::BUILDER.get());
    }
    Err(cmd.exec())
}

/// On Windows: Kiri's docker.exe, beside this kiri.exe in
/// %LOCALAPPDATA%\Kiri\bin (else one on PATH), with the `kiri` context
/// setup made, which reaches the engine in WSL through kiri-win's pipe.
/// Exits with docker's exit code.
#[cfg(windows)]
pub fn run(args: &[String]) -> io::Result<()> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.join("docker.exe")))
        .filter(|d| d.is_file());
    let status = std::process::Command::new(beside.unwrap_or_else(|| "docker.exe".into()))
        .args(["--context", crate::flavor::CONTEXT.get()])
        .args(args)
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .status()
        .map_err(|e| {
            io::Error::other(format!(
                "could not run docker.exe ({e}); open Kiri once to set it up"
            ))
        })?;
    std::process::exit(status.code().unwrap_or(1))
}

/// buildx has a builder of this name: its instance file, in BUILDX_CONFIG,
/// else DOCKER_CONFIG/buildx, else ~/.docker/buildx.
#[cfg(unix)]
fn buildx_knows(name: &str) -> bool {
    let dir = std::env::var_os("BUILDX_CONFIG")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("DOCKER_CONFIG").map(|d| std::path::PathBuf::from(d).join("buildx"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".docker/buildx"))
        });
    dir.is_some_and(|d| d.join("instances").join(name).is_file())
}
