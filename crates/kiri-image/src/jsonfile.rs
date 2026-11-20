//! JSON list files that must survive a crash: `images.json` here, and the
//! daemon's `containers.json`.
//!
//! A file is only ever replaced whole, by renaming a finished temporary
//! file over it, so a reader sees the old list or the new one, never a mix
//! or a cut-short file; syncing it to disk, before or after, is
//! [`Durable`]'s choice. A list that does not parse anyway (an
//! older Kiri wrote it, or the disk lost its tail) is kept as a
//! `<name>.corrupt-*` copy ([`keep_copy`]; the image store moves its own
//! aside, [`move_aside`]), and its complete leading records are what can
//! still be read ([`salvage`]).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// When [`replace`] makes the new file durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durable {
    /// Before it returns: the file is synced before the rename and the
    /// folder after it. For rare writes (`images.json`).
    Now,
    /// Not here: the caller runs [`sync`] soon after, on a thread of its own
    /// (the container list, written on every container start and stop). The
    /// rename still replaces the file whole, so a reader always sees a
    /// complete list; only a crash of the whole system before the sync can
    /// lose the write, and then the old list or a damaged one is found.
    Later,
}

/// Replaces `path` with `bytes`: written to a temporary file of its own in
/// the same folder, then renamed over `path`. The old file is replaced, not
/// overwritten in place, so no bytes of it can trail the new one. With
/// [`Durable::Now`], synced as [`sync`] does, the file before the rename.
pub fn replace(path: &Path, bytes: &[u8], durable: Durable) -> io::Result<()> {
    let dir = parent(path);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!("{name}.{}.{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let result = File::create(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        if durable == Durable::Now {
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    } else if durable == Durable::Now {
        sync_dir(dir);
    }
    result
}

/// Makes what [`replace`] last put at `path` durable: the file, then its
/// folder, so the rename itself is on disk. Nothing to do if there is no
/// file (yet).
pub fn sync(path: &Path) -> io::Result<()> {
    match File::open(path) {
        Ok(f) => f.sync_all()?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    sync_dir(parent(path));
    Ok(())
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}

/// Best effort: not every system can open a folder as a file, and the
/// file's own contents are synced either way.
fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// A name beside `path` for a copy of it, `<name>.corrupt-<seconds>-<n>`,
/// that no file has yet.
fn copy_name(path: &Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    loop {
        let aside = path.with_file_name(format!("{name}.corrupt-{secs}-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
        if fs::symlink_metadata(&aside).is_err() {
            return aside;
        }
    }
}

/// Moves a damaged file aside as `<name>.corrupt-<seconds>-<n>` in the same
/// folder, never over an earlier copy, and says where it went.
pub fn move_aside(path: &Path) -> io::Result<PathBuf> {
    let aside = copy_name(path);
    fs::rename(path, &aside)?;
    Ok(aside)
}

/// Keeps a copy of the damaged `path`, whose contents are `bytes`, beside
/// it as `<name>.corrupt-<seconds>-<n>`, and leaves `path` where it is, so
/// the list file is never missing: the caller then replaces it in one
/// rename. Always a file of its own, written and synced, never a hard link:
/// a link would be the list itself until that rename, and a later change
/// in place would change every copy at once.
pub fn keep_copy(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let aside = copy_name(path);
    let written = fs::OpenOptions::new().write(true).create_new(true).open(&aside).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    match written {
        Ok(()) => {
            sync_dir(parent(path));
            Ok(aside)
        }
        Err(e) => {
            let _ = fs::remove_file(&aside);
            Err(e)
        }
    }
}

/// The copies [`move_aside`] and [`keep_copy`] kept of `path`, oldest
/// first. An error when the folder cannot be listed: then no one can say
/// there are none.
pub fn kept_copies(path: &Path) -> io::Result<Vec<PathBuf>> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{name}.corrupt-");
    let mut copies: Vec<((u64, u64), PathBuf)> = Vec::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        let Some(file) = e.file_name().to_str().map(str::to_owned) else { continue };
        let Some(rest) = file.strip_prefix(&prefix) else { continue };
        // Oldest first by the seconds and counter in the name, not by text
        // (`-10` sorts before `-9` as text).
        let mut n = rest.splitn(2, '-').map(|x| x.parse::<u64>().unwrap_or(u64::MAX));
        let key = (n.next().unwrap_or(u64::MAX), n.next().unwrap_or(u64::MAX));
        copies.push((key, e.path()));
    }
    copies.sort();
    Ok(copies.into_iter().map(|(_, p)| p).collect())
}

/// Removes temporary files [`replace`] left beside `path` when the program
/// stopped midway (`<name>.<pid>.<n>.tmp`). Only for a file one process
/// writes, at its start.
pub fn remove_stale_temps(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else { return };
    let prefix = format!("{name}.");
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".tmp") && !n.contains(".corrupt-")) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// The records that can still be read from a damaged JSON array: its
/// complete leading records. That is everything when a shorter rewrite left
/// old bytes after a complete array (the document ends early, the records
/// are whole), and the records before the cut when the file was truncated.
pub fn salvage<T: DeserializeOwned>(bytes: &[u8]) -> Vec<T> {
    let Some(mut rest) = bytes.trim_ascii_start().strip_prefix(b"[") else {
        return Vec::new();
    };
    let mut records = Vec::new();
    loop {
        rest = rest.trim_ascii_start();
        rest = rest.strip_prefix(b",").unwrap_or(rest);
        let mut stream = serde_json::Deserializer::from_slice(rest).into_iter::<T>();
        match stream.next() {
            Some(Ok(record)) => {
                records.push(record);
                rest = &rest[stream.byte_offset()..];
            }
            _ => return records,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kiri-jsonfile-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn replace_leaves_only_the_new_file() {
        let d = dir("replace");
        let p = d.join("list.json");
        fs::write(&p, b"[1,2,3,4,5,6,7,8,9]").unwrap();
        replace(&p, b"[1]", Durable::Now).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"[1]");
        replace(&p, b"[2]", Durable::Later).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"[2]", "the new list is there before it is synced");
        sync(&p).unwrap();
        sync(&d.join("missing.json")).unwrap();
        let names: Vec<_> = fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["list.json"], "no temporary file is left behind");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn move_aside_keeps_the_bytes_and_is_listed() {
        let d = dir("aside");
        let p = d.join("list.json");
        fs::write(&p, b"[1,2").unwrap();
        let aside = move_aside(&p).unwrap();
        assert!(!p.exists());
        assert_eq!(fs::read(&aside).unwrap(), b"[1,2");
        fs::write(&p, b"[]").unwrap();
        let second = move_aside(&p).unwrap();
        assert_eq!(kept_copies(&p).unwrap(), [aside.clone(), second.clone()]);
        assert!(kept_copies(&d.join("other.json")).unwrap().is_empty());
        // Never over a copy that is already there, whatever its name.
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let next = SEQ.load(Ordering::Relaxed);
        let taken = d.join(format!("list.json.corrupt-{secs}-{next}"));
        fs::write(&taken, b"earlier").unwrap();
        fs::write(&p, b"[3").unwrap();
        let third = move_aside(&p).unwrap();
        assert_ne!(third, taken);
        assert_eq!(fs::read(&taken).unwrap(), b"earlier");
        assert_eq!(fs::read(&third).unwrap(), b"[3");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn copies_list_oldest_first_by_number() {
        let d = dir("order");
        let p = d.join("list.json");
        for n in ["100-10", "100-9", "99-200"] {
            fs::write(d.join(format!("list.json.corrupt-{n}")), b"").unwrap();
        }
        let names: Vec<String> = kept_copies(&p).unwrap().iter().map(|c| c.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["list.json.corrupt-99-200", "list.json.corrupt-100-9", "list.json.corrupt-100-10"]);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn stale_temps_go_and_nothing_else() {
        let d = dir("temps");
        let p = d.join("list.json");
        for f in ["list.json", "list.json.12.3.tmp", "list.json.tmp", "list.json.corrupt-1-0", "other.json.1.2.tmp"] {
            fs::write(d.join(f), b"").unwrap();
        }
        remove_stale_temps(&p);
        let mut left: Vec<String> = fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["list.json", "list.json.corrupt-1-0", "other.json.1.2.tmp"]);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn keep_copy_leaves_the_file_in_place() {
        let d = dir("keep");
        let p = d.join("list.json");
        fs::write(&p, b"[1,2").unwrap();
        let copy = keep_copy(&p, b"[1,2").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"[1,2", "the list file is never missing");
        assert_eq!(fs::read(&copy).unwrap(), b"[1,2");
        // A file of its own: a change to the list in place leaves it as it was.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(fs::metadata(&p).unwrap().ino(), fs::metadata(&copy).unwrap().ino(), "not a hard link");
        }
        fs::write(&p, b"[9,9,9").unwrap();
        assert_eq!(fs::read(&copy).unwrap(), b"[1,2");
        replace(&p, b"[1]", Durable::Now).unwrap();
        assert_eq!(fs::read(&copy).unwrap(), b"[1,2");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn salvage_keeps_complete_leading_records() {
        // Records are objects or strings: a number cut short still parses.
        assert_eq!(salvage::<String>(b"[\"a\", \"b\", \"c"), ["a", "b"]);
        assert_eq!(salvage::<String>(b"[\"a\",\"b\",\"c\"]"), ["a", "b", "c"]);
        assert_eq!(salvage::<String>(b"[\"a\",\"b\"]\"c\",\"d\"]"), ["a", "b"]);
        assert!(salvage::<u32>(b"").is_empty());
        assert!(salvage::<u32>(b"garbage").is_empty());
        assert!(salvage::<String>(b"[\"ab").is_empty());
    }
}
