//! What the engine's storage takes on disk, for `docker system df`: each
//! unpacked layer, each container's writable layer and each volume, as
//! `du -x` counts it (allocated blocks, hard links once, never across into
//! another filesystem such as a host folder or NFS export mounted on a
//! volume). Walking tens of gigabytes takes a while, so the agent runs this
//! on a thread of its own.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// One `KIND NAME BYTES` line per entry (see `kiri_proto::Msg::Usage`).
pub fn measure(layers: &Path, containers: &Path, volumes: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    // Every entry in the layer store, half-done unpacks included, so the
    // total is what the store really takes.
    for (name, path) in entries(layers) {
        lines.push(format!("l {name} {}", du(&path)));
    }
    for (id, path) in entries(containers) {
        let upper = path.join("upper");
        if upper.is_dir() {
            lines.push(format!("c {id} {}", du(&upper)));
        }
    }
    for (name, path) in entries(volumes) {
        lines.push(format!("v {name} {}", du(&path)));
    }
    lines
}

/// The directories in `dir`, by a name that is safe on one line.
fn entries(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| (name, e.path()))
        })
        .collect()
}

/// Bytes allocated under `root`, on `root`'s filesystem only.
pub fn du(root: &Path) -> u64 {
    let Ok(top) = fs::symlink_metadata(root) else { return 0 };
    let dev = top.dev();
    let mut seen = HashSet::new();
    let mut total = top.blocks() * 512;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            let Ok(m) = entry.metadata() else { continue };
            if m.dev() != dev {
                continue;
            }
            if m.nlink() > 1 && !m.is_dir() && !seen.insert(m.ino()) {
                continue;
            }
            total += m.blocks() * 512;
            if m.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_files_once_and_lists_each_kind() {
        let root = std::env::temp_dir().join(format!("kiri-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (layers, containers, volumes) = (root.join("layers"), root.join("containers"), root.join("volumes"));
        fs::create_dir_all(layers.join("ab12")).unwrap();
        fs::create_dir_all(containers.join("c1/upper")).unwrap();
        fs::create_dir_all(volumes.join("data/_data/sub")).unwrap();
        fs::write(layers.join("ab12/f"), vec![1u8; 64 << 10]).unwrap();
        fs::write(volumes.join("data/_data/sub/big"), vec![1u8; 256 << 10]).unwrap();
        // A hard link costs nothing more; a symlink is not followed.
        fs::hard_link(volumes.join("data/_data/sub/big"), volumes.join("data/_data/again")).unwrap();
        std::os::unix::fs::symlink(&layers, volumes.join("data/_data/link")).unwrap();
        // A plain file in the store is not an entry.
        fs::write(layers.join("ab12.done"), b"").unwrap();

        let lines = measure(&layers, &containers, &volumes);
        let size = |kind: &str, name: &str| -> u64 {
            let l = lines.iter().find(|l| l.starts_with(&format!("{kind} {name} "))).unwrap_or_else(|| panic!("no {kind} {name} in {lines:?}"));
            l.rsplit(' ').next().unwrap().parse().unwrap()
        };
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(size("l", "ab12") >= 64 << 10);
        assert!(size("c", "c1") < 64 << 10);
        let v = size("v", "data");
        assert!((256 << 10..512 << 10).contains(&v), "the hard link was counted twice, or the symlink followed: {v}");
        let _ = fs::remove_dir_all(&root);
    }
}
