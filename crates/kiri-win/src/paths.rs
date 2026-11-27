//! Where this install lives: next to `kiri-win.exe` when the MSI/portable
//! bundle is there, otherwise `%LOCALAPPDATA%\Kiri` (dev builds, first copy).

use std::path::{Path, PathBuf};

/// Install / state root: exe folder when it looks like a Kiri bundle, else
/// the classic per-user LocalAppData path.
pub fn kiri_dir() -> PathBuf {
    if let Some(parent) = std::env::current_exe().ok().as_ref().and_then(|e| e.parent()) {
        if looks_like_install(parent) {
            return parent.to_path_buf();
        }
    }
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Kiri")).unwrap_or_else(|| PathBuf::from("."))
}

fn looks_like_install(dir: &Path) -> bool {
    dir.join("kiri").is_file() || dir.join("install.sh").is_file() || dir.join("bin").join("docker.exe").is_file() || dir.join("kiri-linux.tar.gz").is_file()
}
