//! Where an API request comes from: this machine's own programs (the CLI,
//! the window, tools run by the person), or a container that was given the
//! engine's socket (`-v /var/run/docker.sock:/var/run/docker.sock`).
//!
//! What this stops: a container changing Kiri's settings through the
//! engine's API. Docker's API has nothing that changes the engine's own
//! settings, and Kiri's own endpoints that change something (its settings,
//! Stop, Sleep, ports, USB, the builder's limits) refuse a container's
//! connection (api.rs, `CONTAINER_MAY`).
//!
//! What it doesn't, as with Docker:
//! - A container you give writable host folders (`-v $HOME:/m`) can change
//!   your files there, Kiri's settings among them.
//! - A container that may create containers with `--privileged`,
//!   `--pid=host`, `SYS_ADMIN`, `--device` or a mount of the host's root
//!   is as good as root on this machine.
//!
//! Which one a connection is comes from where it arrived, never from
//! anything the other end says or is:
//! - On a Mac, a container reaches the daemon only through the VM's relay
//!   (ports::serve_engine_api).
//! - On Linux and in WSL, a container that mounts the engine's socket is
//!   given a socket of its own beside it ([`container_socket`], binds.rs),
//!   served by the same API.

use std::path::{Path, PathBuf};

/// Who is at the other end of an API connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caller {
    /// This machine's programs, on the daemon's own socket.
    Host,
    /// A container, through the VM's relay or its own socket.
    Container,
}

/// The socket containers that mount the engine's are given, beside the
/// daemon's own: `docker.sock` → `docker.container.sock`.
pub fn container_socket(own: &Path) -> PathBuf {
    let stem = own.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "docker".into());
    own.with_file_name(format!("{stem}.container.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_container_socket_sits_beside_the_daemons() {
        assert_eq!(container_socket(Path::new("/home/me/.kiri/run/docker.sock")), Path::new("/home/me/.kiri/run/docker.container.sock"));
        assert_eq!(container_socket(Path::new("/run/kiri/engine")), Path::new("/run/kiri/engine.container.sock"));
    }
}
