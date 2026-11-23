//! The guest's layer store: compressed layer blobs arrive from the host over
//! vsock and are unpacked once into `layers/<hex>/`, ready to be stacked by
//! overlayfs. A layer directory only appears, by rename, once it is complete,
//! and is only used once `<hex>.done` beside it says its files are on disk:
//! a VM that stops before the guest flushes can keep a layer's names but
//! lose its contents, leaving every file empty.

use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, BufReader, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kiri_proto::{LayerBegin, LayerStored};

use super::{inroot, sys};

pub fn hex(digest: &str) -> io::Result<&str> {
    let h = digest.strip_prefix("sha256:").unwrap_or(digest);
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad layer digest {digest}")));
    }
    Ok(h)
}

pub fn dir(root: &Path, digest: &str) -> io::Result<PathBuf> {
    Ok(root.join(hex(digest)?))
}

fn marker(root: &Path, digest: &str) -> io::Result<PathBuf> {
    Ok(root.join(format!("{}.done", hex(digest)?)))
}

pub fn present(root: &Path, digest: &str) -> bool {
    dir(root, digest).is_ok_and(|d| d.is_dir()) && marker(root, digest).is_ok_and(|m| m.is_file())
}

/// Reads exactly `begin.size` bytes of blob from `stream` and unpacks them.
/// The stream stays in sync even if unpacking fails part-way.
pub fn receive(stream: &mut impl Read, begin: &LayerBegin<'_>, root: &Path) -> io::Result<LayerStored> {
    let started = Instant::now();
    let final_dir = dir(root, begin.digest)?;
    let tmp = root.join(format!(".tmp-{}", hex(begin.digest)?));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;

    let mut limited = stream.take(begin.size);
    let result = unpack(&mut limited, begin.media_type, &tmp, begin.size);
    // Whatever happened, consume the rest of the blob so the next frame lines up.
    io::copy(&mut limited, &mut io::sink())?;

    match result.and_then(|unpacked_bytes| install(root, begin.digest, &tmp, &final_dir).map(|()| unpacked_bytes)) {
        Ok(unpacked_bytes) => Ok(LayerStored { unpacked_bytes, elapsed_ns: started.elapsed().as_nanos() as u64 }),
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

/// Puts an unpacked layer in place: its files reach the disk first, then
/// the directory is renamed in, then the marker says it is whole.
fn install(root: &Path, digest: &str, tmp: &Path, final_dir: &Path) -> io::Result<()> {
    if present(root, digest) {
        let _ = fs::remove_dir_all(tmp);
        return Ok(());
    }
    sync_fs(tmp)?;
    if final_dir.exists() {
        // A copy without a marker was cut short and may hold empty files.
        // Moved aside rather than deleted here, which can take a while;
        // the next RetainLayers clears it.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        fs::rename(final_dir, root.join(format!(".stale-{}-{nanos}", hex(digest)?)))?;
    }
    fs::rename(tmp, final_dir)?;
    fs::File::create(marker(root, digest)?)?.sync_all()?;
    fs::File::open(root)?.sync_all()
}

/// Flushes the filesystem holding `path` to disk.
fn sync_fs(path: &Path) -> io::Result<()> {
    let f = fs::File::open(path)?;
    // SAFETY: a valid descriptor that outlives the call.
    sys::check(unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&f)) } as _, "syncfs")?;
    Ok(())
}

/// Deletes every layer not in `keep` (blob digests), every layer without
/// its marker, and whatever an unpack left half-done. Anything that isn't
/// one of those names is left alone.
pub fn retain<'a>(root: &Path, keep: impl Iterator<Item = &'a str>) {
    let keep: std::collections::HashSet<&str> = keep.filter_map(|d| hex(d).ok()).collect();
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let leftover = name.starts_with(".tmp-") || name.starts_with(".stale-");
        let (h, is_marker) = match name.strip_suffix(".done") {
            Some(h) => (h, true),
            None => (name, false),
        };
        let layer = h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit());
        if !leftover && !layer {
            continue;
        }
        let whole = layer && root.join(h).is_dir() && root.join(format!("{h}.done")).is_file();
        if leftover || !keep.contains(h) || !whole {
            let path = entry.path();
            let _ = if is_marker || !path.is_dir() { fs::remove_file(&path) } else { fs::remove_dir_all(&path) };
        }
    }
}

/// The most a layer may expand to. The blob's digest covers only its
/// compressed bytes, so a tiny, digest-valid layer could otherwise expand
/// without limit onto the disk (on a Linux host or in WSL, the host's own).
/// 256x the compressed size, never below 1 GiB nor above 64 GiB.
fn expansion_cap(compressed: u64) -> u64 {
    compressed.saturating_mul(256).clamp(1 << 30, 64 << 30)
}

/// A reader that fails once more than `left` bytes have come through.
struct Capped<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.left {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "layer expands beyond its allowed size"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

fn unpack(blob: &mut impl Read, media_type: &str, dest: &Path, compressed: u64) -> io::Result<u64> {
    let buffered = BufReader::with_capacity(256 * 1024, blob);
    let left = expansion_cap(compressed);
    if media_type.ends_with("gzip") {
        untar(Capped { inner: flate2::read::MultiGzDecoder::new(buffered), left }, dest)
    } else if media_type.ends_with("zstd") {
        let decoder = ruzstd::decoding::StreamingDecoder::new(buffered)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("zstd: {e}")))?;
        untar(Capped { inner: decoder, left }, dest)
    } else {
        untar(Capped { inner: buffered, left }, dest)
    }
}

/// Unpacks a layer tarball, translating OCI whiteouts into the form overlayfs
/// reads: `.wh.name` becomes a 0:0 character device `name`, and
/// `.wh..wh..opq` marks its directory opaque.
fn untar(reader: impl Read, dest: &Path) -> io::Result<u64> {
    untar_in(reader, dest, in_user_namespace)
}

/// [`untar`], with how a user namespace is detected passed in, so tests can
/// say "in one" without making one.
fn untar_in(reader: impl Read, dest: &Path, detect_userns: fn() -> bool) -> io::Result<u64> {
    // Only looked at once a device or fifo entry turns up.
    let mut userns = None;
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_mtime(true);
    archive.set_unpack_xattrs(true);
    archive.set_overwrite(true);
    let mut total = 0u64;
    // Each directory's time, set once the layer is unpacked: tar-rs gives a
    // directory none, and what is unpacked into it later would move it.
    let mut dir_times = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.file_name().map(|n| n.as_bytes()).unwrap_or_default();
        if let Some(hidden) = name.strip_prefix(b".wh.") {
            // `.wh.`, `.wh..` and `.wh...` would hide the directory itself or its parent.
            if matches!(hidden, b"" | b"." | b"..") {
                return Err(escapes());
            }
            let parent = whiteout_dir(dest, path.parent().unwrap_or(Path::new("")))?;
            if hidden == b".wh..opq" {
                let p = sys::cstr(&parent)?;
                // SAFETY: valid NUL-terminated path, name and a 1-byte value.
                let ret = unsafe {
                    libc::setxattr(p.as_ptr(), c"trusted.overlay.opaque".as_ptr(), b"y".as_ptr().cast(), 1, 0)
                };
                sys::check(ret as _, "mark opaque")?;
            } else {
                let target = parent.join(std::ffi::OsStr::from_bytes(hidden));
                let _ = fs::remove_file(&target);
                let _ = fs::remove_dir_all(&target);
                sys::mknod(&sys::cstr(&target)?, libc::S_IFCHR, 0, 0)?;
            }
            continue;
        }
        total += entry.size();
        if entry.header().entry_type().is_hard_link() {
            let source = entry.link_name()?.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hard link has no target"))?;
            let root = inroot::open_root(dest)?;
            let (from, source) = hardlink_parent(root.as_fd(), &source)?;
            let (to, name) = hardlink_parent(root.as_fd(), &path)?;
            // A link to what is already there (itself, or the same file under
            // another path) has nothing to do; clearing first would remove
            // the very file to link to.
            let same = same_entry(from.as_fd(), &source, to.as_fd(), &name).map_err(|e| io::Error::new(e.kind(), format!("hard link: {e}")))?;
            if !same {
                // What an earlier entry left at this path goes, as in Docker.
                clear(to.as_fd(), &name)?;
                // SAFETY: parents are confined directory fds and names are NUL-terminated.
                // Flags 0 links the symlink itself, without following its absolute target.
                let ret = unsafe { libc::linkat(from.as_raw_fd(), source.as_ptr(), to.as_raw_fd(), name.as_ptr(), 0) };
                sys::check(ret as _, "hard link")?;
            }
        } else if let Some(kind) = special_kind(entry.header().entry_type()) {
            // In a user namespace (nested Docker, Sysbox, rootless, CI) the
            // kernel refuses mknod of a device, so the entry is left out, as
            // Docker leaves it out; a fifo is still made, and left out only
            // if refused.
            let userns = *userns.get_or_insert_with(detect_userns);
            if userns && kind != libc::S_IFIFO {
                // Docker removes what is at the path before leaving it out,
                // and carries on when there is no such path: a parent that
                // is missing or a file, a dangling symlink, or a loop.
                let root = inroot::open_root(dest)?;
                let cleared = hardlink_parent(root.as_fd(), &path).and_then(|(parent, name)| clear(parent.as_fd(), &name));
                match cleared {
                    Err(e) if !no_such_path(&e) => return Err(e),
                    _ => continue,
                }
            }
            make_special(dest, &path, entry.header(), kind, userns)?;
        } else if unpack_replacing(&mut entry, dest, &path)? {
            keep_zero_time(dest, &path, entry.header())?;
            if entry.header().entry_type().is_dir()
                && let Ok(mtime) = entry.header().mtime()
            {
                // By its plain names: `./usr` and `usr` are one directory.
                let plain = path.components().filter(|c| matches!(c, std::path::Component::Normal(_))).collect();
                dir_times.push((plain, mtime));
            }
        }
    }
    let root = inroot::open_root(dest)?;
    set_dir_times(root.as_fd(), dir_times);
    Ok(total)
}

/// Sets each directory's time once the layer is unpacked (#676): deepest
/// first, as Docker sets them, and the last entry's time for a directory
/// named twice. One a later entry removed, or made something else, is
/// skipped, and a time that can't be set doesn't fail the layer.
fn set_dir_times(root: BorrowedFd<'_>, mut dirs: Vec<(PathBuf, u64)>) {
    // By path, component by component, so a directory's subtree sits right
    // after it, and stable, so a path named twice keeps its entries in
    // archive order. Layers list their paths in this order already, which
    // makes the sort a single pass.
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut walk = DirWalk::default();
    let mut last: Option<&Path> = None;
    // Backwards, every directory comes before its parent, and consecutive
    // directories share most of their path, which `walk` opens only once.
    for (dir, mtime) in dirs.iter().rev() {
        if last == Some(dir.as_path()) {
            continue;
        }
        last = Some(dir);
        let _ = walk.set_time(root, dir, *mtime);
    }
}

/// The most directories a [`DirWalk`] keeps open, so a layer nested deeper
/// than this can't run the agent out of descriptors; deeper ones are opened
/// for each path on their own.
const DIR_WALK_KEEP: usize = 64;

/// Finds directories inside a layer one name at a time, as the kernel walks
/// a path but never through a symlink (one a later entry may have put
/// there) and making nothing. The parents of the last directory stay open
/// for the next one, which mostly shares them, so setting a time costs two
/// lookups of one name each.
#[derive(Default)]
struct DirWalk {
    /// The open parents of the last directory, from the layer root down.
    open: Vec<(std::ffi::OsString, OwnedFd)>,
}

impl DirWalk {
    /// Sets directory `rel`'s access and modification times to `mtime`.
    fn set_time(&mut self, root: BorrowedFd<'_>, rel: &Path, mtime: u64) -> io::Result<()> {
        let secs = i64::try_from(mtime).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "directory time out of range"))?;
        let mut names = Vec::new();
        for part in rel.components() {
            match part {
                std::path::Component::Normal(name) => names.push(name),
                std::path::Component::CurDir | std::path::Component::RootDir => {}
                _ => return Err(escapes()),
            }
        }
        // The layer root itself keeps its time, as before.
        let Some((name, parents)) = names.split_last() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the layer root"));
        };
        let shared = self.open.iter().zip(parents).take_while(|((open, _), name)| open.as_os_str() == **name).count();
        self.open.truncate(shared);
        // Past the cap, the rest of the parents are walked with one
        // descriptor at a time, kept only until the next step.
        let mut deep: Option<OwnedFd> = None;
        for part in &parents[shared..] {
            let parent = match &deep {
                Some(fd) => fd.as_fd(),
                None => self.open.last().map_or(root, |(_, fd)| fd.as_fd()),
            };
            let fd = open_dir_nofollow(parent, part)?;
            if deep.is_none() && self.open.len() < DIR_WALK_KEEP {
                self.open.push((part.to_os_string(), fd));
            } else {
                deep = Some(fd);
            }
        }
        let parent = match &deep {
            Some(fd) => fd.as_fd(),
            None => self.open.last().map_or(root, |(_, fd)| fd.as_fd()),
        };
        let name = sys::cstr(Path::new(name))?;
        // Only a directory: not a symlink, which AT_SYMLINK_NOFOLLOW would
        // give the time instead of its target, nor anything else a later
        // entry put in the directory's place. Nothing else changes the layer
        // while it unpacks.
        // SAFETY: stat is plain data (all-zero is valid); a valid directory
        // descriptor and a NUL-terminated name.
        let st = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            sys::check(libc::fstatat(parent.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) as _, "stat directory")?;
            st
        };
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let time = libc::timespec { tv_sec: secs, tv_nsec: 0 };
        // SAFETY: a valid directory descriptor, a NUL-terminated name and two timespecs.
        let ret = unsafe { libc::utimensat(parent.as_raw_fd(), name.as_ptr(), [time, time].as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        sys::check(ret as _, "set directory time")?;
        Ok(())
    }
}

/// Opens directory `name` in `parent` with O_NOFOLLOW; a symlink, anything
/// else that isn't a directory, and a name that isn't there are errors.
fn open_dir_nofollow(parent: BorrowedFd<'_>, name: &std::ffi::OsStr) -> io::Result<OwnedFd> {
    let name = sys::cstr(Path::new(name))?;
    // SAFETY: a valid directory descriptor and a NUL-terminated name.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    sys::check(fd as _, "open directory")?;
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The file type of a device or fifo entry, which the tar crate would
/// unpack as an empty regular file.
fn special_kind(t: tar::EntryType) -> Option<libc::mode_t> {
    if t.is_character_special() {
        Some(libc::S_IFCHR)
    } else if t.is_block_special() {
        Some(libc::S_IFBLK)
    } else if t.is_fifo() {
        Some(libc::S_IFIFO)
    } else {
        None
    }
}

/// Makes a device node or fifo at `path` inside `dest` with the entry's
/// numbers, owner, mode and time, replacing what is there. The parent is
/// looked up inside the layer, as a hard link's is. With `may_be_refused`
/// (a user namespace), a mknod the kernel refuses leaves the entry out.
fn make_special(dest: &Path, path: &Path, h: &tar::Header, kind: libc::mode_t, may_be_refused: bool) -> io::Result<()> {
    let root = inroot::open_root(dest)?;
    let (parent, name) = hardlink_parent(root.as_fd(), path)?;
    let mode = (h.mode()? & 0o7777) as libc::mode_t;
    // A fifo has no device number, and archive writers leave its device
    // fields blank (Python's tarfile does), which is not a number: they are
    // not read for one. Docker unpacks such a fifo.
    let dev = if kind == libc::S_IFIFO {
        0
    } else {
        libc::makedev(h.device_major()?.unwrap_or(0), h.device_minor()?.unwrap_or(0))
    };
    let (uid, gid) = (h.uid()? as libc::uid_t, h.gid()? as libc::gid_t);
    let mtime = h.mtime()? as i64;
    let p = parent.as_raw_fd();
    let t = [libc::timespec { tv_sec: mtime, tv_nsec: 0 }, libc::timespec { tv_sec: mtime, tv_nsec: 0 }];
    // SAFETY: a confined parent descriptor and a single NUL-terminated name.
    unsafe {
        clear(parent.as_fd(), &name)?;
        if libc::mknodat(p, name.as_ptr(), kind | mode, dev) != 0 {
            let e = io::Error::last_os_error();
            if may_be_refused && e.raw_os_error() == Some(libc::EPERM) {
                return Ok(());
            }
            return Err(io::Error::new(e.kind(), format!("mknod: {e}")));
        }
        sys::check(libc::fchownat(p, name.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW) as _, "chown")?;
        // mknod applies the umask.
        sys::check(libc::fchmodat(p, name.as_ptr(), mode, 0) as _, "chmod")?;
        sys::check(libc::utimensat(p, name.as_ptr(), t.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) as _, "set time")?;
    }
    Ok(())
}

/// Whether this process runs in a user namespace other than the first, read
/// as Docker reads it (moby/sys/userns): the first user namespace's
/// /proc/self/uid_map is `0 0 4294967295`. No such file means a kernel
/// without user namespaces.
fn in_user_namespace() -> bool {
    match fs::read_to_string("/proc/self/uid_map") {
        Ok(map) => uid_map_in_user_namespace(map.lines().next().unwrap_or("")),
        Err(_) => false,
    }
}

fn uid_map_in_user_namespace(first_line: &str) -> bool {
    // Empty: a namespace just made, its map not written yet.
    if first_line.is_empty() {
        return true;
    }
    let mut fields = first_line.split_whitespace().map(|f| f.parse::<u64>());
    match (fields.next(), fields.next(), fields.next()) {
        (Some(Ok(inside)), Some(Ok(outside)), Some(Ok(count))) => (inside, outside, count) != (0, 0, 4294967295),
        // Not a map: taken as the first namespace, as Docker takes it.
        _ => false,
    }
}

/// Puts back a time of 0 the tar crate made 1 (it does so for every 0, so
/// that tools which take 0 for "no time" see one). Docker keeps the 0, and
/// what a container reads must be what the archive says. Folders are left
/// alone: what is unpacked into one changes its time afterwards anyway.
pub(crate) fn keep_zero_time(dest: &Path, path: &Path, h: &tar::Header) -> io::Result<()> {
    if h.mtime()? != 0 || h.entry_type().is_dir() || h.entry_type().is_hard_link() {
        return Ok(());
    }
    let root = inroot::open_root(dest)?;
    let (parent, name) = hardlink_parent(root.as_fd(), path)?;
    let t = [libc::timespec { tv_sec: 0, tv_nsec: 0 }, libc::timespec { tv_sec: 0, tv_nsec: 0 }];
    // SAFETY: a confined parent descriptor, a NUL-terminated name and two timespecs.
    sys::check(unsafe { libc::utimensat(parent.as_raw_fd(), name.as_ptr(), t.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } as _, "set time")?;
    Ok(())
}

fn escapes() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "archive path escapes the layer")
}

fn hardlink_parent(root: BorrowedFd<'_>, path: &Path) -> io::Result<(OwnedFd, CString)> {
    let mut relative = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::Normal(name) => relative.push(name),
            std::path::Component::CurDir | std::path::Component::RootDir => {}
            _ => return Err(escapes()),
        }
    }
    let name = relative.file_name().ok_or_else(escapes)?;
    let parent = inroot::open(root, relative.parent().unwrap_or(Path::new("")), inroot::Last::Dir)?;
    Ok((parent.fd, sys::cstr(Path::new(name))?))
}

/// The directory under `dest` a whiteout lives in, made if missing. Every
/// step must be a real directory of this layer: an earlier entry could have
/// made one a symlink, which a check of the path's text cannot see and which
/// would carry the whiteout outside the layer.
fn whiteout_dir(dest: &Path, parent: &Path) -> io::Result<PathBuf> {
    let mut dir = dest.to_path_buf();
    for part in parent.components() {
        match part {
            std::path::Component::Normal(name) => dir.push(name),
            std::path::Component::CurDir => continue,
            _ => return Err(escapes()),
        }
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(escapes()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&dir)?,
            Err(e) => return Err(e),
        }
    }
    Ok(dir)
}

/// Unpacks `entry` as the tar crate does, which already replaces one file
/// or symlink with another, and also when an earlier entry of the layer
/// left a directory where this one isn't, or something else where this one
/// is a directory: as Docker does, the directory goes for the file, and the
/// file for the directory, while a directory over a directory is merged.
/// Only looked into once the tar crate fails, and only when it failed on
/// what is at the path (not on a path that leads outside the layer, or
/// can't be looked up), so the usual entry costs nothing more.
fn unpack_replacing<R: Read>(entry: &mut tar::Entry<'_, R>, dest: &Path, path: &Path) -> io::Result<bool> {
    let err = match entry.unpack_in(dest) {
        Ok(unpacked) => return Ok(unpacked),
        Err(e) => e,
    };
    if !matches!(err.kind(), io::ErrorKind::AlreadyExists | io::ErrorKind::IsADirectory | io::ErrorKind::NotADirectory) {
        return Err(err);
    }
    let is_dir = entry.header().entry_type().is_dir();
    match replace_other_kind(dest, path, is_dir) {
        Ok(true) => entry.unpack_in(dest),
        Ok(false) => Err(err),
        Err(e) => Err(io::Error::new(e.kind(), format!("replacing {}: {e}", path.display()))),
    }
}

/// Removes what is at `path` inside the layer when it is a directory and
/// the entry for it isn't, or the other way round; says whether it did.
fn replace_other_kind(dest: &Path, path: &Path, is_dir: bool) -> io::Result<bool> {
    let root = inroot::open_root(dest)?;
    let (parent, name) = hardlink_parent(root.as_fd(), path)?;
    match kind_at(parent.as_fd(), &name)? {
        Some(was_dir) if was_dir != is_dir => {
            clear(parent.as_fd(), &name)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Whether `name` in `parent` is a directory (not following a symlink), or
/// None when nothing is there.
fn kind_at(parent: BorrowedFd<'_>, name: &CStr) -> io::Result<Option<bool>> {
    // SAFETY: stat is plain data (all-zero is valid); a valid directory
    // descriptor and a NUL-terminated name.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstatat(parent.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) != 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::ENOENT) { Ok(None) } else { Err(e) };
        }
        Ok(Some(st.st_mode & libc::S_IFMT == libc::S_IFDIR))
    }
}

/// Whether `a` in `pa` and `b` in `pb` are both there and the same file
/// (device and inode, not following a symlink).
fn same_entry(pa: BorrowedFd<'_>, a: &CStr, pb: BorrowedFd<'_>, b: &CStr) -> io::Result<bool> {
    let id = |parent: BorrowedFd<'_>, name: &CStr| -> io::Result<Option<(libc::dev_t, libc::ino_t)>> {
        // SAFETY: stat is plain data (all-zero is valid); a valid directory
        // descriptor and a NUL-terminated name.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstatat(parent.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) != 0 {
                let e = io::Error::last_os_error();
                return if e.raw_os_error() == Some(libc::ENOENT) { Ok(None) } else { Err(e) };
            }
            Ok(Some((st.st_dev, st.st_ino)))
        }
    };
    Ok(match (id(pa, a)?, id(pb, b)?) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    })
}

/// Removes whatever is at `name` in `parent`: a directory with all in it,
/// anything else unlinked. Nothing there is fine.
fn clear(parent: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    match kind_at(parent, name)? {
        None => Ok(()),
        Some(true) => remove_tree(parent, name, 0),
        Some(false) => {
            // SAFETY: a valid directory descriptor and a NUL-terminated name.
            sys::check(unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } as _, "remove").map(drop)
        }
    }
}

/// Whether `e` says the path can't be reached (ENOENT, ENOTDIR or ELOOP),
/// also after a lookup put its path in it.
fn no_such_path(e: &io::Error) -> bool {
    [libc::ENOENT, libc::ENOTDIR, libc::ELOOP].into_iter().any(|code| e.raw_os_error() == Some(code) || e.kind() == io::Error::from_raw_os_error(code).kind())
}

/// How deep a directory being replaced may go: each level holds one
/// descriptor while what is in it goes. A deeper one fails the layer.
const REMOVE_DEPTH: usize = 128;

/// How many times a directory is listed and emptied, as long as each listing
/// still finds names, before its removal fails.
const REMOVE_PASSES: usize = 8;

/// Removes directory `name` in `parent` and all in it, through descriptors
/// alone: every directory is opened with O_NOFOLLOW from the one above, so
/// a symlink inside is removed, never followed, and /proc isn't needed. A
/// directory on another mount than the one above it is an error, before
/// anything in it goes: another filesystem by its device, a bind mount of
/// the same one by its mount ID (statx; kernels before 5.8 lack it and see
/// only the device). Layers unpack into a fresh folder with no mounts, so
/// this is a guard, not the isolation.
///
/// Names are read in full, and the listing closed, before any goes, so
/// removing doesn't disturb the listing and only the directory's own
/// descriptor stays open while the ones below go. A directory still not
/// empty (ENOTEMPTY, or EEXIST, which POSIX also allows) is listed again
/// while the last listing found something.
fn remove_tree(parent: BorrowedFd<'_>, name: &CStr, depth: usize) -> io::Result<()> {
    if depth >= REMOVE_DEPTH {
        return Err(io::Error::other("directory too deep to replace"));
    }
    let dir = open_below(parent, name)?;
    for _ in 0..REMOVE_PASSES {
        let found = empty_dir(dir.as_fd(), depth)?;
        // SAFETY: a valid directory descriptor and a NUL-terminated name.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if found == 0 || !matches!(e.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) {
            return Err(io::Error::new(e.kind(), format!("remove directory: {e}")));
        }
    }
    Err(io::Error::other("remove directory: still not empty"))
}

/// Opens directory `name` in `parent` without following a symlink, and only
/// when it is on the same mount as `parent` (see [`remove_tree`]).
fn open_below(parent: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    // SAFETY: a valid directory descriptor and a NUL-terminated name.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    sys::check(fd as _, "open directory")?;
    // SAFETY: a fresh descriptor nothing else owns.
    let dir = unsafe { OwnedFd::from_raw_fd(fd) };
    if !same_mount(mount_of(dir.as_fd())?, mount_of(parent)?) {
        return Err(io::Error::other("a mount inside the directory to replace"));
    }
    Ok(dir)
}

/// The filesystem `fd` is on, and its mount's ID where the kernel tells it.
fn mount_of(fd: BorrowedFd<'_>) -> io::Result<(libc::dev_t, Option<u64>)> {
    // SAFETY: stat is plain data (all-zero is valid); fstat fills it from a
    // valid descriptor.
    let dev = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        sys::check(libc::fstat(fd.as_raw_fd(), &mut st) as _, "stat directory")?;
        st.st_dev
    };
    Ok((dev, mount_id(fd)))
}

/// The ID of the mount `fd` is on, from statx (Linux 5.8+); None when the
/// kernel can't say. The raw syscall, as musl before 1.2.5 has no wrapper.
fn mount_id(fd: BorrowedFd<'_>) -> Option<u64> {
    // SAFETY: statx is plain data (all-zero is valid); the kernel fills it
    // for a valid descriptor named by an empty path with AT_EMPTY_PATH.
    unsafe {
        let mut stx: libc::statx = std::mem::zeroed();
        let ret = libc::syscall(
            libc::SYS_statx,
            // Full-width arguments: a variadic call leaves the top half of
            // a narrower one unspecified.
            libc::c_long::from(fd.as_raw_fd()),
            c"".as_ptr(),
            libc::c_long::from(libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW),
            libc::c_long::from(libc::STATX_MNT_ID),
            &mut stx as *mut libc::statx,
        );
        (ret == 0 && stx.stx_mask & libc::STATX_MNT_ID != 0).then_some(stx.stx_mnt_id)
    }
}

/// Whether two directories are on the same mount: the same device, and the
/// same mount ID when both have one.
fn same_mount(a: (libc::dev_t, Option<u64>), b: (libc::dev_t, Option<u64>)) -> bool {
    a.0 == b.0 && !matches!((a.1, b.1), (Some(x), Some(y)) if x != y)
}

/// Removes everything in directory `dir` (see [`remove_tree`]); says how
/// many names its listing found.
fn empty_dir(dir: BorrowedFd<'_>, depth: usize) -> io::Result<usize> {
    let names = list_dir(dir)?;
    for (child, kind) in &names {
        let is_dir = match *kind {
            libc::DT_DIR => true,
            libc::DT_UNKNOWN => kind_at(dir, child)? == Some(true),
            _ => false,
        };
        if is_dir {
            remove_tree(dir, child, depth + 1)?;
        } else {
            // SAFETY: a valid directory descriptor and a NUL-terminated name.
            sys::check(unsafe { libc::unlinkat(dir.as_raw_fd(), child.as_ptr(), 0) } as _, "remove")?;
        }
    }
    Ok(names.len())
}

/// The names in directory `dir` (but . and ..) with their d_type, read
/// through a listing of its own that is closed before this returns.
fn list_dir(dir: BorrowedFd<'_>) -> io::Result<Vec<(CString, u8)>> {
    // A fresh open of "." starts at the first name, whatever an earlier
    // listing read.
    // SAFETY: a valid directory descriptor and a NUL-terminated name.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c".".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    sys::check(fd as _, "open directory")?;
    // SAFETY: a fresh descriptor nothing else owns.
    let stream = DirStream::new(unsafe { OwnedFd::from_raw_fd(fd) })?;
    let mut names = Vec::new();
    loop {
        // SAFETY: errno is this thread's; readdir reads a valid open stream
        // and sets errno only on failure.
        let ent = unsafe {
            *libc::__errno_location() = 0;
            libc::readdir(stream.0.as_ptr())
        };
        if ent.is_null() {
            let e = io::Error::last_os_error();
            return if e.raw_os_error().unwrap_or(0) == 0 { Ok(names) } else { Err(e) };
        }
        // SAFETY: readdir's entry stays valid until the next call on this
        // stream; the name is copied out first.
        let (child, kind) = unsafe { (CStr::from_ptr((*ent).d_name.as_ptr()), (*ent).d_type) };
        if !matches!(child.to_bytes(), b"." | b"..") {
            names.push((child.to_owned(), kind));
        }
    }
}

/// A directory stream, closed with its descriptor when dropped.
struct DirStream(std::ptr::NonNull<libc::DIR>);

impl DirStream {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        // SAFETY: an owned directory descriptor; on success the stream owns
        // it, on failure it is still ours and `fd` closes it.
        let stream = unsafe { libc::fdopendir(fd.as_raw_fd()) };
        let stream = std::ptr::NonNull::new(stream).ok_or_else(io::Error::last_os_error)?;
        std::mem::forget(fd);
        Ok(DirStream(stream))
    }
}

impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: the stream is open and closed only here, once.
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete header: tar-rs refuses to unpack one with empty uid/gid.
    fn header(size: u64, mode: u32) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_size(size);
        h.set_mode(mode);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_cksum();
        h
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("kiri-layers-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_path_named_again_as_another_kind_takes_the_later_entry() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};

        // As Docker's archive.Unpack: what is at the path goes, unless both
        // are directories, which merge.
        let mut b = tar::Builder::new(Vec::new());
        let mut add = |path: &str, kind: tar::EntryType, link: Option<&str>, data: &[u8]| {
            let mut h = header(data.len() as u64, if kind.is_symlink() { 0o777 } else { 0o755 });
            h.set_entry_type(kind);
            h.set_mtime(1_000_000);
            match link {
                Some(to) => b.append_link(&mut h, path, to).unwrap(),
                None => {
                    h.set_cksum();
                    b.append_data(&mut h, path, data).unwrap();
                }
            }
        };
        use tar::EntryType::{Directory as D, Fifo, Link, Regular as F, Symlink as S};
        // A directory with a file in it, then a file.
        add("swap1", D, None, b"");
        add("swap1/inner", F, None, b"old");
        add("swap1", F, None, b"new");
        // A symlink (to a directory outside), then a directory.
        add("swap2", S, Some("/srv"), b"");
        add("swap2", D, None, b"");
        add("swap2/in", F, None, b"in");
        // A file, then a directory.
        add("swap3", F, None, b"x");
        add("swap3", D, None, b"");
        // A directory, then a symlink: the symlink, as in Docker (which also
        // sets the gone directory's time on the link's target; here the
        // target is left alone).
        add("swap4", D, None, b"");
        add("swap4/inner", F, None, b"");
        add("swap4", S, Some("swap1"), b"");
        // A directory, then a hard link; a file, then a hard link.
        add("swap5", D, None, b"");
        add("swap5/inner", F, None, b"");
        add("swap5", Link, Some("swap1"), b"");
        add("swap6", F, None, b"other");
        add("swap6", Link, Some("swap1"), b"");
        // A directory, then a fifo.
        add("swap7", D, None, b"");
        add("swap7/inner", F, None, b"");
        add("swap7", Fifo, None, b"");
        // A directory twice: merged, the first one's file kept.
        add("keep", D, None, b"");
        add("keep/a", F, None, b"a");
        add("keep", D, None, b"");
        let bytes = b.into_inner().unwrap();

        let dest = scratch("retype");
        untar_in(Capped { inner: &bytes[..], left: u64::MAX }, &dest, || false).unwrap();
        let meta = |p: &str| fs::symlink_metadata(dest.join(p)).unwrap();
        assert_eq!(fs::read(dest.join("swap1")).unwrap(), b"new");
        assert!(meta("swap2").is_dir() && meta("swap2/in").is_file());
        assert!(meta("swap3").is_dir());
        assert_eq!(fs::read_link(dest.join("swap4")).unwrap(), Path::new("swap1"));
        assert_eq!(meta("swap5").ino(), meta("swap1").ino());
        assert_eq!(meta("swap6").ino(), meta("swap1").ino());
        assert!(meta("swap7").file_type().is_fifo());
        assert_eq!(fs::read(dest.join("keep/a")).unwrap(), b"a");
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn a_hard_link_to_itself_keeps_the_file() {
        let mut b = tar::Builder::new(Vec::new());
        b.append_data(&mut header(4, 0o644), "a", &b"data"[..]).unwrap();
        let mut h = header(0, 0o777);
        h.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut h, "here", ".").unwrap();
        for (path, to) in [("a", "a"), ("here/a", "a"), ("b", "a"), ("b", "a")] {
            let mut h = header(0, 0o644);
            h.set_entry_type(tar::EntryType::Link);
            b.append_link(&mut h, path, to).unwrap();
        }
        let bytes = b.into_inner().unwrap();
        let dest = scratch("self-link");
        untar_in(Capped { inner: &bytes[..], left: u64::MAX }, &dest, || false).unwrap();
        assert_eq!(fs::read(dest.join("a")).unwrap(), b"data");
        assert_eq!(fs::read(dest.join("b")).unwrap(), b"data");
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn a_retype_stays_inside_the_layer() {
        let mut b = tar::Builder::new(Vec::new());
        let mut add = |path: &str, kind: tar::EntryType, link: Option<&str>| {
            let mut h = header(0, if kind.is_symlink() { 0o777 } else { 0o755 });
            h.set_entry_type(kind);
            match link {
                Some(to) => b.append_link(&mut h, path, to).unwrap(),
                None => {
                    h.set_cksum();
                    b.append_data(&mut h, path, io::empty()).unwrap();
                }
            }
        };
        use tar::EntryType::{Directory as D, Regular as F, Symlink as S};
        // A file under a parent that is an absolute symlink: the tar crate
        // refuses it (the link leads off the layer), and what the link would
        // name inside the layer stays.
        add("real", D, None);
        add("real/x", D, None);
        add("real/x/keep", F, None);
        add("lnk", S, Some("/real"));
        add("lnk/x", F, None);
        let bytes = b.into_inner().unwrap();
        let dest = scratch("retype-abs-parent");
        assert!(untar_in(Capped { inner: &bytes[..], left: u64::MAX }, &dest, || false).is_err());
        assert!(dest.join("real/x/keep").is_file());
        fs::remove_dir_all(dest).unwrap();

        // Under a relative symlink inside the layer, as in Docker: the
        // directory it reaches goes for the file.
        let mut b = tar::Builder::new(Vec::new());
        let mut add = |path: &str, kind: tar::EntryType, link: Option<&str>| {
            let mut h = header(0, if kind.is_symlink() { 0o777 } else { 0o755 });
            h.set_entry_type(kind);
            match link {
                Some(to) => b.append_link(&mut h, path, to).unwrap(),
                None => {
                    h.set_cksum();
                    b.append_data(&mut h, path, io::empty()).unwrap();
                }
            }
        };
        add("real", D, None);
        add("real/x", D, None);
        add("real/x/keep", F, None);
        add("lnk", S, Some("real"));
        add("lnk/x", F, None);
        let bytes = b.into_inner().unwrap();
        let dest = scratch("retype-rel-parent");
        untar_in(Capped { inner: &bytes[..], left: u64::MAX }, &dest, || false).unwrap();
        assert!(fs::symlink_metadata(dest.join("real/x")).unwrap().is_file());
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn a_replaced_directory_goes_without_following_links() {
        // Outside the layer: a directory its symlinks point to.
        let outside = scratch("retype-outside");
        fs::write(outside.join("keep"), b"keep").unwrap();
        let dest = scratch("retype-tree");
        let mut deep = dest.join("d");
        fs::create_dir(&deep).unwrap();
        std::os::unix::fs::symlink(&outside, deep.join("abs")).unwrap();
        for i in 0..40 {
            deep.push(format!("{i}"));
            fs::create_dir(&deep).unwrap();
            fs::write(deep.join("f"), b"f").unwrap();
            std::os::unix::fs::symlink(&outside, deep.join("out")).unwrap();
        }
        let root = inroot::open_root(&dest).unwrap();
        clear(root.as_fd(), c"d").unwrap();
        assert!(!dest.join("d").exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");

        // Wide: more names than one listing buffer holds, files and folders.
        let wide = dest.join("w");
        fs::create_dir(&wide).unwrap();
        for i in 0..3000 {
            if i % 10 == 0 {
                fs::create_dir(wide.join(format!("d{i}"))).unwrap();
                fs::write(wide.join(format!("d{i}/f")), b"").unwrap();
            } else {
                fs::write(wide.join(format!("f{i}-{}", "x".repeat(40))), b"").unwrap();
            }
        }
        clear(root.as_fd(), c"w").unwrap();
        assert!(!wide.exists());

        // Deeper than a removal goes: an error, not a run out of descriptors.
        let mut deep = dest.join("e");
        for _ in 0..REMOVE_DEPTH + 2 {
            fs::create_dir(&deep).unwrap();
            deep.push("n");
        }
        assert!(clear(root.as_fd(), c"e").is_err());
        drop(root);
        fs::remove_dir_all(dest).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn a_device_left_out_in_a_user_namespace_still_replaces() {
        // Docker removes the directory, then leaves the device out.
        let mut b = tar::Builder::new(Vec::new());
        let mut h = header(0, 0o755);
        h.set_entry_type(tar::EntryType::Directory);
        h.set_cksum();
        b.append_data(&mut h, "dev", io::empty()).unwrap();
        b.append_data(&mut header(1, 0o644), "dev/x", &b"x"[..]).unwrap();
        let mut h = header(0, 0o666);
        h.set_entry_type(tar::EntryType::Char);
        h.set_device_major(1).unwrap();
        h.set_device_minor(3).unwrap();
        h.set_cksum();
        b.append_data(&mut h, "dev", io::empty()).unwrap();
        let bytes = b.into_inner().unwrap();
        let dest = scratch("retype-userns");
        untar_in(Capped { inner: &bytes[..], left: u64::MAX }, &dest, || true).unwrap();
        assert!(fs::symlink_metadata(dest.join("dev")).is_err());
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn a_device_left_out_where_there_is_no_such_path_is_skipped() {
        // Under a file, a loop, a missing parent or a dangling symlink:
        // Docker finds no such path and carries on.
        let mut b = tar::Builder::new(Vec::new());
        b.append_data(&mut header(1, 0o644), "pf", &b"x"[..]).unwrap();
        let mut h = header(0, 0o777);
        h.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut h, "lp", "lp").unwrap();
        let mut h = header(0, 0o777);
        h.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut h, "dl", "missing").unwrap();
        for path in ["pf/dev", "lp/dev", "nodir/dev", "dl/dev"] {
            let mut h = header(0, 0o666);
            h.set_entry_type(tar::EntryType::Char);
            h.set_device_major(1).unwrap();
            h.set_device_minor(3).unwrap();
            h.set_cksum();
            b.append_data(&mut h, path, io::empty()).unwrap();
        }
        b.append_data(&mut header(1, 0o644), "after", &b"a"[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        let dest = scratch("retype-userns-skip");
        untar_in(Capped { inner: &bytes[..], left: u64::MAX }, &dest, || true).unwrap();
        assert_eq!(fs::read(dest.join("pf")).unwrap(), b"x");
        assert!(dest.join("after").is_file());
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn a_replaced_directory_does_not_reach_into_a_mount() {
        let dest = scratch("retype-mount");
        fs::create_dir_all(dest.join("d/m")).unwrap();
        fs::write(dest.join("d/f"), b"f").unwrap();
        let m = sys::cstr(dest.join("d/m")).unwrap();
        // SAFETY: NUL-terminated strings.
        if unsafe { libc::mount(c"tmpfs".as_ptr(), m.as_ptr(), c"tmpfs".as_ptr(), 0, std::ptr::null()) } != 0 {
            eprintln!("skipped: no tmpfs mount here ({})", io::Error::last_os_error());
            fs::remove_dir_all(dest).unwrap();
            return;
        }
        fs::write(dest.join("d/m/keep"), b"keep").unwrap();
        let root = inroot::open_root(&dest).unwrap();
        let err = clear(root.as_fd(), c"d").unwrap_err();
        assert!(err.to_string().contains("mount"), "{err}");
        assert_eq!(fs::read(dest.join("d/m/keep")).unwrap(), b"keep");
        // SAFETY: a NUL-terminated path.
        unsafe { libc::umount(m.as_ptr()) };

        // A bind mount of the same filesystem: the same device, another
        // mount ID (a kernel before 5.8 can't tell it apart: skipped).
        if mount_id(root.as_fd()).is_none() {
            eprintln!("skipped: no mount IDs on this kernel");
        } else {
            let src = scratch("retype-bind-src");
            fs::write(src.join("keep"), b"keep").unwrap();
            let s = sys::cstr(&src).unwrap();
            // SAFETY: NUL-terminated strings.
            assert_eq!(unsafe { libc::mount(s.as_ptr(), m.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) }, 0, "{}", io::Error::last_os_error());
            let err = clear(root.as_fd(), c"d").unwrap_err();
            assert!(err.to_string().contains("mount"), "{err}");
            assert_eq!(fs::read(src.join("keep")).unwrap(), b"keep");
            // SAFETY: a NUL-terminated path.
            unsafe { libc::umount(m.as_ptr()) };
            fs::remove_dir_all(src).unwrap();
        }
        drop(root);
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn directories_keep_the_layers_time() {
        use std::os::unix::fs::MetadataExt;

        // #676: a directory, an empty one, and one whose files come after it.
        let dir = |path: &str, mtime: u64| {
            let mut h = header(0, 0o755);
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mtime(mtime);
            h.set_cksum();
            (h, path.to_owned())
        };
        let mut b = tar::Builder::new(Vec::new());
        for (mut h, p) in [dir("usr", 1_000_000), dir("usr/bin", 2_000_000), dir("mnt", 3_000_000)] {
            b.append_data(&mut h, p, io::empty()).unwrap();
        }
        let mut h = header(2, 0o755);
        h.set_mtime(4_000_000);
        h.set_cksum();
        b.append_data(&mut h, "usr/bin/tool", &b"#!"[..]).unwrap();
        let bytes = b.into_inner().unwrap();

        let dest = scratch("dir-times");
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        let mtime = |p: &str| fs::symlink_metadata(dest.join(p)).unwrap().mtime();
        assert_eq!(mtime("usr"), 1_000_000);
        assert_eq!(mtime("usr/bin"), 2_000_000, "after its file was unpacked into it");
        assert_eq!(mtime("mnt"), 3_000_000, "an empty one");
        assert_eq!(mtime("usr/bin/tool"), 4_000_000);

        // A directory a later entry turned into a symlink (after a whiteout
        // removed it): its time is not set through the link, nor through a
        // link on the way.
        std::os::unix::fs::symlink("/mnt", dest.join("swapped")).unwrap();
        let root = inroot::open_root(&dest).unwrap();
        // A walk that has the directories of the last path open, and one
        // that starts fresh.
        let mut walk = DirWalk::default();
        walk.set_time(root.as_fd(), Path::new("usr/bin"), 2_000_000).unwrap();
        for walk in [&mut walk, &mut DirWalk::default()] {
            assert!(walk.set_time(root.as_fd(), Path::new("swapped"), 9).is_err());
            assert!(walk.set_time(root.as_fd(), Path::new("swapped/x"), 9).is_err());
            assert!(walk.set_time(root.as_fd(), Path::new("usr/bin/tool"), 9).is_err(), "a file");
            assert!(walk.set_time(root.as_fd(), Path::new("gone"), 9).is_err(), "nothing is made");
            assert!(walk.set_time(root.as_fd(), Path::new("usr/gone/x"), 9).is_err(), "nothing is made");
            assert!(walk.set_time(root.as_fd(), Path::new("usr/../mnt"), 9).is_err());
        }
        assert!(!dest.join("gone").exists() && !dest.join("usr/gone").exists());
        assert_eq!(mtime("mnt"), 3_000_000, "the link's target keeps its own time");
        assert_eq!(mtime("usr/bin/tool"), 4_000_000);
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn directory_times_in_any_order_depth_and_kind() {
        use std::os::unix::fs::MetadataExt;

        let mut b = tar::Builder::new(Vec::new());
        // `./dot`, then `dot/` further on (the tar crate's builder drops a
        // leading `./`, so it is written by hand).
        let mut h = header(0, 0o755);
        h.as_gnu_mut().unwrap().name[..5].copy_from_slice(b"./dot");
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mtime(7_000_000);
        h.set_cksum();
        b.append(&h, io::empty()).unwrap();
        let mut add = |path: &str, kind: tar::EntryType, mtime: u64, link: Option<&str>| {
            let mut h = header(0, if kind.is_symlink() { 0o777 } else { 0o755 });
            h.set_entry_type(kind);
            h.set_mtime(mtime);
            match link {
                Some(to) => b.append_link(&mut h, path, to).unwrap(),
                None => {
                    h.set_cksum();
                    b.append_data(&mut h, path, io::empty()).unwrap();
                }
            }
        };
        let dir = tar::EntryType::Directory;
        // Out of order, with names that sort between a directory and its
        // children byte by byte ("a-c", "a.d" before "a/b").
        add("a/b", dir, 2_000_000, None);
        add("a-c", dir, 3_000_000, None);
        add("a", dir, 1_000_000, None);
        add("a.d", dir, 4_000_000, None);
        add("a/b/c", dir, 5_000_000, None);
        // Named twice: the later entry's time, also when spelt another way
        // (`./dot` came first).
        add("dup", dir, 6_000_000, None);
        add("dup", dir, 7_000_000, None);
        add("dot/", dir, 6_000_000, None);
        // Deeper than a walk keeps open, then a sibling at each end.
        let mut deep = String::from("deep");
        let mut deeps = vec![deep.clone()];
        for i in 0..(DIR_WALK_KEEP + 20) {
            deep.push_str(&format!("/{i}"));
            deeps.push(deep.clone());
        }
        for (i, p) in deeps.iter().enumerate() {
            add(p, dir, 10_000_000 + i as u64, None);
        }
        add(&format!("{deep}-sib"), dir, 8_000_000, None);
        add("deep/0-sib", dir, 9_000_000, None);
        // A symlink to a directory keeps its own time, and so does the directory.
        add("real", dir, 11_000_000, None);
        add("sw", tar::EntryType::Symlink, 14_000_000, Some("real"));
        let bytes = b.into_inner().unwrap();

        let dest = scratch("dir-times-order");
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        let mtime = |p: &str| fs::symlink_metadata(dest.join(p)).unwrap().mtime();
        for (p, t) in [("a", 1_000_000), ("a/b", 2_000_000), ("a-c", 3_000_000), ("a.d", 4_000_000), ("a/b/c", 5_000_000), ("dup", 7_000_000), ("dot", 6_000_000)] {
            assert_eq!(mtime(p), t, "{p}");
        }
        for (i, p) in deeps.iter().enumerate() {
            assert_eq!(mtime(p), 10_000_000 + i as i64, "{p}");
        }
        assert_eq!(mtime(&format!("{deep}-sib")), 8_000_000);
        assert_eq!(mtime("deep/0-sib"), 9_000_000);
        assert_eq!(mtime("real"), 11_000_000);
        assert_eq!(mtime("sw"), 14_000_000, "the symlink's own time");
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn hardlinks_preserve_source_inode() {
        use std::os::unix::fs::MetadataExt;

        for (i, link) in [None, Some("target"), Some("/target"), Some("/missing-target")].into_iter().enumerate() {
            let mut b = tar::Builder::new(Vec::new());
            b.append_data(&mut header(2, 0o644), "target", &b"[]"[..]).unwrap();
            let source = if let Some(to) = link {
                let mut h = header(0, 0o777);
                h.set_entry_type(tar::EntryType::Symlink);
                b.append_link(&mut h, "source", to).unwrap();
                "source"
            } else {
                "target"
            };
            let mut h = header(0, 0o777);
            h.set_entry_type(tar::EntryType::Link);
            b.append_link(&mut h, "nested/manifest.nix", source).unwrap();
            let bytes = b.into_inner().unwrap();
            let dest = scratch(&format!("hardlink-{i}"));
            untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
            let from = fs::symlink_metadata(dest.join(source)).unwrap();
            let to = fs::symlink_metadata(dest.join("nested/manifest.nix")).unwrap();
            assert_eq!((from.dev(), from.ino()), (to.dev(), to.ino()), "{link:?}");
            if let Some(link) = link {
                assert_eq!(fs::read_link(dest.join("nested/manifest.nix")).unwrap(), Path::new(link));
            } else {
                assert_eq!(fs::read(dest.join("nested/manifest.nix")).unwrap(), b"[]");
            }
            fs::remove_dir_all(dest).unwrap();
        }
    }

    #[test]
    fn hardlinks_resolve_parents_inside_layer() {
        use std::os::unix::fs::MetadataExt;

        let mut b = tar::Builder::new(Vec::new());
        b.append_data(&mut header(1, 0o644), "dir/file", &b"x"[..]).unwrap();
        let mut h = header(0, 0o777);
        h.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut h, "alias", "dir").unwrap();
        let mut h = header(0, 0o777);
        h.set_entry_type(tar::EntryType::Link);
        b.append_link(&mut h, "alias/copy", "alias/file").unwrap();
        let bytes = b.into_inner().unwrap();
        let dest = scratch("hardlink-parents");
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        assert_eq!(fs::read(dest.join("dir/copy")).unwrap(), b"x");
        assert_eq!(fs::metadata(dest.join("dir/file")).unwrap().ino(), fs::metadata(dest.join("dir/copy")).unwrap().ino());
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn hardlink_paths_cannot_escape_layer() {
        use std::os::unix::fs::MetadataExt;

        for (i, (source, target)) in [
            ("../outside/secret", "copied"),
            ("escape/secret", "copied"),
            ("file", "escape/copied"),
            ("file", "../outside/copied"),
        ].into_iter().enumerate() {
            let base = scratch(&format!("hardlink-escape-{i}"));
            let dest = base.join("layer");
            let outside = base.join("outside");
            fs::create_dir(&dest).unwrap();
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("secret"), b"keep").unwrap();
            let mut b = tar::Builder::new(Vec::new());
            b.append_data(&mut header(1, 0o644), "file", &b"x"[..]).unwrap();
            let mut h = header(0, 0o777);
            h.set_entry_type(tar::EntryType::Symlink);
            b.append_link(&mut h, "escape", &outside).unwrap();
            let mut h = header(0, 0o777);
            h.set_entry_type(tar::EntryType::Link);
            h.set_link_name(source).unwrap();
            // The builder refuses a traversal path; an untrusted archive need not.
            h.as_mut_bytes()[..target.len()].copy_from_slice(target.as_bytes());
            h.set_cksum();
            b.append(&h, io::empty()).unwrap();
            let bytes = b.into_inner().unwrap();
            let _ = untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest);
            assert_eq!(fs::read_link(dest.join("escape")).unwrap(), outside);
            assert!(!dest.join("copied").exists());
            assert!(!outside.join("copied").exists());
            assert_eq!(fs::read(outside.join("secret")).unwrap(), b"keep");
            assert_eq!(fs::symlink_metadata(outside.join("secret")).unwrap().nlink(), 1);
            fs::remove_dir_all(base).unwrap();
        }
    }

    /// A layer whose first entry makes `foo` a symlink to `/` and whose
    /// second writes `foo/x` must not write outside the layer directory.
    #[test]
    fn symlink_parent_cannot_escape_layer() {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = header(0, 0o777);
        h.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut h, "foo", "/").unwrap();
        let mut h = header(1, 0o644);
        b.append_data(&mut h, "foo/x", &b"x"[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        let dest = scratch("escape");
        let err = untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).expect_err("writing through a symlinked parent must fail");
        // The refusal must be the path check, not some other unpack error.
        assert!(err.to_string().contains("outside"), "unexpected error: {err}");
        assert!(!Path::new("/x").is_file() || fs::read_to_string("/x").map_or(true, |c| c != "x"), "escaped to /x");
        let _ = fs::remove_dir_all(&dest);
    }

    fn whiteout_layer(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, link) in entries {
            if let Some(to) = link {
                let mut h = header(0, 0o777);
                h.set_entry_type(tar::EntryType::Symlink);
                b.append_link(&mut h, name, to).unwrap();
            } else {
                let mut h = header(0, 0o644);
                b.append_data(&mut h, name, &[][..]).unwrap();
            }
        }
        b.into_inner().unwrap()
    }

    /// A whiteout under a symlink to a directory outside the layer must not
    /// delete, replace or mark anything there.
    #[test]
    fn whiteout_through_symlink_stays_in_layer() {
        let outside = scratch("wh-outside");
        fs::write(outside.join("victim"), "keep").unwrap();
        let to = outside.to_str().unwrap();
        for hidden in ["foo/.wh.victim", "foo/.wh..wh..opq"] {
            let dest = scratch("wh-escape");
            let bytes = whiteout_layer(&[("foo", Some(to)), (hidden, None)]);
            let err = untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).expect_err(hidden);
            assert!(err.to_string().contains("escapes"), "{hidden}: unexpected error: {err}");
            assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "keep", "{hidden}");
            assert!(!outside.join(&hidden[4..]).exists(), "{hidden}: placeholder written outside");
            let _ = fs::remove_dir_all(&dest);
        }
        let _ = fs::remove_dir_all(&outside);
    }

    /// Whiteouts naming the directory itself or its parent are refused
    /// before anything is removed.
    #[test]
    fn whiteout_of_dot_or_dotdot_is_refused() {
        for hidden in ["d/.wh.", "d/.wh..", "d/.wh..."] {
            let dest = scratch("wh-dots");
            fs::create_dir_all(dest.join("d")).unwrap();
            fs::write(dest.join("d/keep"), "x").unwrap();
            let bytes = whiteout_layer(&[(hidden, None)]);
            let err = untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).expect_err(hidden);
            assert!(err.to_string().contains("escapes"), "{hidden}: unexpected error: {err}");
            assert!(dest.join("d/keep").is_file(), "{hidden}");
            let _ = fs::remove_dir_all(&dest);
        }
    }

    /// A whiteout whose own path is absolute (the tar builder won't write
    /// one, so the header is patched by hand) must not reach outside.
    #[test]
    fn whiteout_path_cannot_escape_layer() {
        let base = scratch("wh-abs");
        let dest = base.join("layer");
        fs::create_dir(&dest).unwrap();
        fs::write(base.join("keep"), "x").unwrap();
        let path = format!("{}/.wh.keep", base.display());
        let mut b = tar::Builder::new(Vec::new());
        b.append_data(&mut header(0, 0o644), "a".repeat(path.len()), &[][..]).unwrap();
        let mut bytes = b.into_inner().unwrap();
        bytes[..100].fill(0);
        bytes[..path.len()].copy_from_slice(path.as_bytes());
        bytes[148..156].fill(b' ');
        let checksum: u32 = bytes[..512].iter().map(|b| u32::from(*b)).sum();
        bytes[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        assert!(untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).is_err());
        assert!(base.join("keep").is_file());
        let _ = fs::remove_dir_all(&base);
    }

    /// A hard link to `/bin/tool` links the layer's `bin/tool`, as with Docker
    /// (issue 302). `..` is refused; `root` → `/` is followed inside the layer,
    /// where `etc/hostname` doesn't exist; nothing is linked in either case.
    #[test]
    fn absolute_hard_link_is_read_from_the_layer_root() {
        let layer = |to: &str| {
            let mut b = tar::Builder::new(Vec::new());
            b.append_data(&mut header(3, 0o755), "bin/tool", &b"hi\n"[..]).unwrap();
            let mut h = header(0, 0o777);
            h.set_entry_type(tar::EntryType::Symlink);
            b.append_link(&mut h, "root", "/").unwrap();
            let mut h = header(0, 0o755);
            h.set_entry_type(tar::EntryType::Link);
            b.append_link(&mut h, "bin/tool-1.0", to).unwrap();
            b.into_inner().unwrap()
        };
        let dest = scratch("abs-link");
        untar(Capped { inner: &layer("/bin/tool")[..], left: u64::MAX }, &dest).unwrap();
        let (tool, link) = (fs::metadata(dest.join("bin/tool")).unwrap(), fs::metadata(dest.join("bin/tool-1.0")).unwrap());
        assert_eq!(std::os::unix::fs::MetadataExt::ino(&link), std::os::unix::fs::MetadataExt::ino(&tool), "not a hard link");
        let _ = fs::remove_dir_all(&dest);

        for (to, why) in [("/../../etc/hostname", "escapes"), ("/root/etc/hostname", "No such file"), ("/bin/nope", "No such file")] {
            let dest = scratch("abs-link-bad");
            let err = untar(Capped { inner: &layer(to)[..], left: u64::MAX }, &dest).expect_err(to);
            assert!(err.to_string().contains(why), "{to}: unexpected error: {err}");
            assert!(!dest.join("bin/tool-1.0").exists(), "{to}: linked");
            let _ = fs::remove_dir_all(&dest);
        }
    }

    /// An ordinary whiteout becomes overlayfs's 0:0 character device and
    /// leaves no `.wh.` file behind. mknod needs root, as in the guest.
    #[test]
    fn whiteout_becomes_char_device() {
        // SAFETY: a plain getter.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: needs root for mknod");
            return;
        }
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let dest = scratch("wh-ok");
        fs::create_dir_all(dest.join("etc")).unwrap();
        fs::write(dest.join("etc/gone"), "x").unwrap();
        let bytes = whiteout_layer(&[("etc/.wh.gone", None)]);
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        let m = fs::symlink_metadata(dest.join("etc/gone")).unwrap();
        assert!(m.file_type().is_char_device() && m.rdev() == 0, "not a 0:0 char device");
        assert!(!dest.join("etc/.wh.gone").exists(), "placeholder left behind");
        let _ = fs::remove_dir_all(&dest);
        // As `docker commit` writes them: no owner or time, which tar-rs
        // would refuse to unpack. A whiteout is never unpacked, so it works,
        // and missing parents are made.
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(0);
        h.set_mode(0o644);
        b.append_data(&mut h, "usr/share/.wh.doc", std::io::empty()).unwrap();
        let bytes = b.into_inner().unwrap();
        let dest = scratch("wh-bare");
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        assert!(fs::symlink_metadata(dest.join("usr/share/doc")).unwrap().file_type().is_char_device());
        let _ = fs::remove_dir_all(&dest);
    }

    fn layer_blob(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in files {
            b.append_data(&mut header(data.len() as u64, 0o644), name, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn send(root: &Path, digest: &str, blob: &[u8]) -> LayerStored {
        let begin = LayerBegin { digest, media_type: "application/vnd.oci.image.layer.v1.tar", size: blob.len() as u64 };
        receive(&mut &blob[..], &begin, root).unwrap()
    }

    /// A stored layer is present only with its marker; one cut short
    /// (directory, no marker) is not, and is replaced when sent again.
    #[test]
    fn layer_without_marker_is_replaced() {
        let root = scratch("marker");
        let blob = layer_blob(&[("bin/sh", b"real")]);
        send(&root, D1, &blob);
        assert!(present(&root, D1));
        assert_eq!(fs::read(dir(&root, D1).unwrap().join("bin/sh")).unwrap(), b"real");

        // What a VM stopped before the flush leaves: names, empty contents.
        fs::remove_file(marker(&root, D1).unwrap()).unwrap();
        fs::write(dir(&root, D1).unwrap().join("bin/sh"), b"").unwrap();
        assert!(!present(&root, D1));
        send(&root, D1, &blob);
        assert!(present(&root, D1));
        assert_eq!(fs::read(dir(&root, D1).unwrap().join("bin/sh")).unwrap(), b"real");
        let _ = fs::remove_dir_all(&root);
    }

    /// Retain keeps whole layers it is told to and deletes the rest:
    /// unlisted layers, unmarked ones, leftovers. Other names stay.
    #[test]
    fn retain_deletes_unused_and_broken_layers() {
        let root = scratch("retain");
        let blob = layer_blob(&[("f", b"x")]);
        send(&root, D1, &blob);
        send(&root, D2, &blob);
        let broken = "3".repeat(64);
        fs::create_dir(root.join(&broken)).unwrap();
        fs::create_dir(root.join(format!(".stale-{broken}-1"))).unwrap();
        fs::create_dir(root.join(format!(".tmp-{broken}"))).unwrap();
        fs::write(root.join("other"), "keep").unwrap();

        retain(&root, [D1, &format!("sha256:{broken}")].into_iter());
        assert!(present(&root, D1));
        assert!(!dir(&root, D2).unwrap().exists() && !marker(&root, D2).unwrap().exists());
        assert!(!root.join(&broken).exists(), "unmarked layer kept");
        assert!(!root.join(format!(".stale-{broken}-1")).exists() && !root.join(format!(".tmp-{broken}")).exists());
        assert!(root.join("other").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    /// Device nodes and fifos come out as such, numbers, owner and mode kept,
    /// not as the empty files the tar crate would make of them.
    #[test]
    fn devices_and_fifos_are_unpacked_as_themselves() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        // SAFETY: a plain getter.
        if unsafe { libc::geteuid() } != 0 || in_user_namespace() {
            eprintln!("skipped: needs root outside a user namespace for mknod");
            return;
        }
        let mut b = tar::Builder::new(Vec::new());
        for (name, kind, major, minor, mode) in [("dev/null", tar::EntryType::Char, 1, 3, 0o666), ("dev/loop9", tar::EntryType::Block, 7, 9, 0o660), ("pipe", tar::EntryType::Fifo, 0, 0, 0o640)] {
            let mut h = header(0, mode);
            h.set_entry_type(kind);
            h.set_device_major(major).unwrap();
            h.set_device_minor(minor).unwrap();
            h.set_uid(12);
            h.set_mtime(1_600_000_000);
            h.set_cksum();
            b.append_data(&mut h, name, &[][..]).unwrap();
        }
        // A fifo whose device fields are blank, as other archive writers
        // leave them.
        let mut h = header(0, 0o600);
        h.set_entry_type(tar::EntryType::Fifo);
        h.as_gnu_mut().unwrap().dev_major = [b' '; 8];
        h.as_gnu_mut().unwrap().dev_minor = [b' '; 8];
        h.set_cksum();
        b.append_data(&mut h, "blankpipe", &[][..]).unwrap();
        let bytes = b.into_inner().unwrap();
        let dest = scratch("special");
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        let m = |p: &str| fs::symlink_metadata(dest.join(p)).unwrap();
        let null = m("dev/null");
        assert!(null.file_type().is_char_device());
        assert_eq!((libc::major(null.rdev()), libc::minor(null.rdev()), null.mode() & 0o7777, null.uid(), null.mtime()), (1, 3, 0o666, 12, 1_600_000_000));
        let lp = m("dev/loop9");
        assert!(lp.file_type().is_block_device());
        assert_eq!((libc::major(lp.rdev()), libc::minor(lp.rdev()), lp.mode() & 0o7777), (7, 9, 0o660));
        let pipe = m("pipe");
        assert!(pipe.file_type().is_fifo());
        assert_eq!(pipe.mode() & 0o7777, 0o640);
        assert!(m("blankpipe").file_type().is_fifo());
        let _ = fs::remove_dir_all(&dest);
    }

    /// A layer with a character device, a block device, a fifo and a file
    /// after them, all owned by root.
    fn layer_with_devices() -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = header(0, 0o755);
        h.set_entry_type(tar::EntryType::Directory);
        h.set_cksum();
        b.append_data(&mut h, "dev/", &[][..]).unwrap();
        for (name, kind, major, minor) in [("dev/null", tar::EntryType::Char, 1, 3), ("dev/loop9", tar::EntryType::Block, 7, 9), ("pipe", tar::EntryType::Fifo, 0, 0)] {
            let mut h = header(0, 0o640);
            h.set_entry_type(kind);
            h.set_device_major(major).unwrap();
            h.set_device_minor(minor).unwrap();
            h.set_cksum();
            b.append_data(&mut h, name, &[][..]).unwrap();
        }
        b.append_data(&mut header(2, 0o644), "after", &b"ok"[..]).unwrap();
        b.into_inner().unwrap()
    }

    /// What a user namespace gets of [`layer_with_devices`], as Docker
    /// unpacks it there: no file for either device, the fifo, and the rest
    /// of the layer.
    fn assert_unpacked_as_in_a_user_namespace(dest: &Path) {
        use std::os::unix::fs::FileTypeExt;
        for dev in ["dev/null", "dev/loop9"] {
            assert!(fs::symlink_metadata(dest.join(dev)).is_err(), "{dev}: a device is left out in a user namespace");
        }
        assert!(fs::symlink_metadata(dest.join("pipe")).unwrap().file_type().is_fifo(), "a fifo is still made");
        assert_eq!(fs::read(dest.join("after")).unwrap(), b"ok", "the rest of the layer unpacks");
    }

    /// Docker's test of /proc/self/uid_map: only the first namespace's own
    /// map is outside a user namespace.
    #[test]
    fn a_user_namespace_is_told_from_uid_map_as_docker_tells_it() {
        for (line, userns) in [
            ("         0          0 4294967295", false),
            ("0 0 4294967295", false),
            ("0 165536 65536", true),
            ("0 1000 1", true),
            ("0 0 1", true),
            ("0 0 65536", true),
            ("", true),
            ("not a map", false),
        ] {
            assert_eq!(uid_map_in_user_namespace(line), userns, "{line:?}");
        }
    }

    /// The detection seam: told it is in a user namespace, the unpack leaves
    /// out the devices (mknod would fail there and, before, failed the whole
    /// layer) and keeps the rest. Needs no privilege: a fifo is made by
    /// anyone.
    #[test]
    fn devices_are_left_out_when_told_of_a_user_namespace() {
        let bytes = layer_with_devices();
        let dest = scratch("userns-seam");
        untar_in(Capped { inner: &bytes[..], left: u64::MAX }, &dest, || true).expect("a layer with devices unpacks in a user namespace");
        assert_unpacked_as_in_a_user_namespace(&dest);
        let _ = fs::remove_dir_all(&dest);
    }

    /// The same in a real user namespace: the test binary runs this test
    /// again in a new one, root inside mapped to the caller outside, where
    /// the kernel refuses mknod of a device with EPERM. Skipped where no
    /// user namespace can be made (a container's seccomp profile, a
    /// sysctl).
    #[test]
    fn a_layer_with_devices_unpacks_in_a_real_user_namespace() {
        use std::os::unix::process::CommandExt;
        const INSIDE: &str = "KIRI_TEST_IN_USER_NAMESPACE";
        if std::env::var_os(INSIDE).is_some() {
            assert!(in_user_namespace(), "uid_map: {:?}", fs::read_to_string("/proc/self/uid_map"));
            let bytes = layer_with_devices();
            let dest = scratch("userns-real");
            untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).expect("a layer with devices unpacks in a user namespace");
            assert_unpacked_as_in_a_user_namespace(&dest);
            let _ = fs::remove_dir_all(&dest);
            return;
        }
        // Built before the fork: the child only makes system calls.
        // SAFETY: plain getters.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let uid_map = format!("0 {uid} 1");
        let gid_map = format!("0 {gid} 1");
        let module = module_path!();
        let name = format!("{}::a_layer_with_devices_unpacks_in_a_real_user_namespace", &module[module.find("::").unwrap() + 2..]);
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([name.as_str(), "--exact", "--nocapture", "--test-threads=1"]).env(INSIDE, "1");
        // SAFETY: between fork and exec only unshare, open, write and close.
        unsafe {
            cmd.pre_exec(move || {
                let write = |path: &std::ffi::CStr, data: &[u8]| -> io::Result<()> {
                    let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                    if fd < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let n = libc::write(fd, data.as_ptr().cast(), data.len());
                    let e = io::Error::last_os_error();
                    libc::close(fd);
                    if n != data.len() as isize {
                        return Err(e);
                    }
                    Ok(())
                };
                if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                    return Err(io::Error::last_os_error());
                }
                write(c"/proc/self/setgroups", b"deny")?;
                write(c"/proc/self/uid_map", uid_map.as_bytes())?;
                write(c"/proc/self/gid_map", gid_map.as_bytes())
            });
        }
        let status = match cmd.status() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipped: no user namespace can be made here: {e}");
                return;
            }
        };
        assert!(status.success(), "unpacking in a user namespace failed: {status}");
    }

    /// A time of 0 in the archive is 0 on disk, as Docker unpacks it. The
    /// tar crate makes it 1 ("0-mtime files ... some tools don't ingest them
    /// well"), which a container then sees.
    #[test]
    fn a_zero_time_stays_zero() {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: a plain getter.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: needs root to keep owners");
            return;
        }
        let mut b = tar::Builder::new(Vec::new());
        let mut f = header(1, 0o644);
        b.append_data(&mut f, "f", &b"x"[..]).unwrap();
        let mut l = header(0, 0o777);
        l.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut l, "l", "f").unwrap();
        let mut g = header(1, 0o644);
        g.set_mtime(5);
        g.set_cksum();
        b.append_data(&mut g, "g", &b"y"[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        let dest = scratch("zero-time");
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        let t = |p: &str| fs::symlink_metadata(dest.join(p)).unwrap().mtime();
        assert_eq!((t("f"), t("l"), t("g")), (0, 0, 5));
        let _ = fs::remove_dir_all(&dest);
    }

    /// A layer that expands past its cap is refused.
    #[test]
    fn expansion_is_capped() {
        let mut b = tar::Builder::new(Vec::new());
        let data = vec![0u8; 4096];
        let mut h = header(data.len() as u64, 0o644);
        b.append_data(&mut h, "big", &data[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        // As in receive(): a layer that fails is discarded, never reused.
        let capped = scratch("cap");
        assert!(untar(Capped { inner: &bytes[..], left: 1024 }, &capped).is_err());
        let _ = fs::remove_dir_all(&capped);
        let fresh = scratch("cap-ok");
        assert_eq!(untar(Capped { inner: &bytes[..], left: u64::MAX }, &fresh).unwrap(), 4096);
        let _ = fs::remove_dir_all(&fresh);
        assert_eq!(expansion_cap(1), 1 << 30);
        assert_eq!(expansion_cap(1 << 40), 64 << 30);
    }
}
