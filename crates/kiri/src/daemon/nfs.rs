//! Container files in Finder: a read-only NFSv3 server on a Unix socket, one
//! folder per running container, mounted at `~/Kiri` with macOS's own
//! `mount_nfs` (no admin rights, no kernel extension). A socket rather than a
//! loopback port, because every account on the Mac can reach loopback and an
//! NFS client names its own uid: here the kernel says who connected, and only
//! the Mac's NFS client (root) and this user get in. NFS gives
//! close-to-open consistency: every open asks for fresh attributes, so a file
//! rewritten inside a container never shows stale bytes in Finder. Files are
//! read from the guest on demand; nothing is copied up front.
//!
//! The MOUNT and NFS programs share the socket, so no portmapper is needed.
//! File handles name `(container, path)` pairs in a bounded table; when it
//! fills it starts a new generation and old handles answer `STALE`, which the
//! client recovers from by looking the path up again.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::engine::{Engine, Status};
use super::vmlink::FileInfo;

/// Handles kept before the table starts over.
const MAX_HANDLES: usize = 50_000;
/// Largest READ answered, matching what the guest reads per request.
const MAX_READ: u32 = 512 * 1024;
/// Largest RPC record accepted; requests are small.
const MAX_RECORD: usize = 64 * 1024;

const PROG_NFS: u32 = 100_003;
const PROG_MOUNT: u32 = 100_005;

// nfsstat3
const OK: u32 = 0;
const NOENT: u32 = 2;
const IO: u32 = 5;
const ACCES: u32 = 13;
const NOTDIR: u32 = 20;
const ROFS: u32 = 30;
const STALE: u32 = 70;
const BADHANDLE: u32 = 10001;
const NOTSUPP: u32 = 10004;

/// The export's filesystem id, `KIRI`.
const FSID: u64 = 0x4b49_5249;

/// `~/.kiri/files/nfs.sock`. Under HOME, as the daemon's state is, in a
/// folder only this user can open.
pub fn socket_path() -> io::Result<PathBuf> {
    Ok(super::state_dir()?.join("files/nfs.sock"))
}

pub fn start(engine: Arc<Engine>) -> io::Result<PathBuf> {
    let path = socket_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no folder for the files socket"))?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    // SAFETY: getuid cannot fail.
    if std::fs::symlink_metadata(dir)?.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user", dir.display()),
        ));
    }
    let _ = std::fs::remove_file(&path); // stale, from a daemon that died
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let handles = Arc::new(Mutex::new(Handles::default()));
    std::thread::Builder::new()
        .name("kiri-nfs".into())
        .spawn(move || {
            for conn in listener.incoming().flatten() {
                if !allowed(&conn) {
                    continue;
                }
                let (engine, handles) = (engine.clone(), handles.clone());
                let _ = std::thread::Builder::new()
                    .name("kiri-nfs-conn".into())
                    .spawn(move || {
                        let _ = Server {
                            engine: &engine,
                            handles: &handles,
                            listing: None,
                        }
                        .serve(conn);
                    });
            }
        })?;
    Ok(path)
}

/// Whether the peer is the Mac's NFS client, which connects as root, or this
/// user. The folder already keeps other users out; this holds if it doesn't.
fn allowed(conn: &UnixStream) -> bool {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid on a connected socket we own, writing two ids.
    if unsafe { libc::getpeereid(conn.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return false;
    }
    // SAFETY: getuid cannot fail.
    uid == 0 || uid == unsafe { libc::getuid() }
}

/// Where Finder shows the files.
pub fn mount_point() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(home.join(crate::flavor::APP.get()))
}

/// Mounts the server at `~/Kiri`, as the user (no admin rights needed).
///
/// Only over what is safe to cover: no folder (it is made), an empty one
/// (Finder's `.DS_Store` and `.localized` don't count), or a mount this
/// engine left behind, say after `kill -9`. A folder with the person's own
/// files in it, a file, a link, or anything else mounted there is left
/// alone and the mount fails: `~/kiri` is the same folder on a
/// case-insensitive disk, and a clone of the repo there is a likely thing
/// to find.
pub fn mount(socket: &Path) -> io::Result<PathBuf> {
    let dir = mount_point()?;
    let socket = socket
        .to_str()
        .filter(|s| !s.contains(','))
        .ok_or_else(|| io::Error::other("the files socket's path cannot be passed to mount_nfs"))?;
    match mounted_at(&dir) {
        Some(m) if m.is_ours(socket) => {
            unmount();
            if mounted_at(&dir).is_some() {
                return Err(io::Error::other(format!(
                    "the last mount at {} could not be removed",
                    dir.display()
                )));
            }
        }
        Some(m) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} already has {} mounted on it ({}), so it was left alone",
                    dir.display(),
                    m.from,
                    m.fstype
                ),
            ));
        }
        None => {}
    }
    check_folder(&dir)?;
    std::fs::create_dir_all(&dir)?;
    // ticotsord with socket paths: mount_nfs's local transport ("<>" is the
    // host). noac: attributes are never reused, so every read sees the
    // current file. soft: if the daemon is gone, Finder gets an error
    // instead of hanging.
    let opts = format!(
        "vers=3,proto=ticotsord,port={socket},mountport={socket},rdonly,nolocks,soft,intr,timeo=10,retrans=2,noac"
    );
    let status = std::process::Command::new("/sbin/mount_nfs")
        .args(["-o", &opts, "<>:/"])
        .arg(&dir)
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!("mount_nfs failed: {status}")));
    }
    Ok(dir)
}

/// Removes this engine's mount at `~/Kiri`, and nothing else: another
/// mount there is someone else's.
pub fn unmount() {
    let (Ok(dir), Ok(socket)) = (mount_point(), socket_path()) else {
        return;
    };
    let Some(socket) = socket.to_str() else {
        return;
    };
    if mounted_at(&dir).is_some_and(|m| m.is_ours(socket)) {
        let _ = std::process::Command::new("/sbin/umount")
            .arg("-f")
            .arg(&dir)
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Whether `~/Kiri` shows this engine's container files. The window asks
/// before it reads a container's folder there, so it never lists the Mac's
/// own files as the container's.
pub fn is_mounted() -> bool {
    let (Ok(dir), Ok(socket)) = (mount_point(), socket_path()) else { return false };
    socket.to_str().is_some_and(|s| mounted_at(&dir).is_some_and(|m| m.is_ours(s)))
}

/// Files Finder leaves in a folder it has shown; they may be covered.
const FINDER_FILES: [&str; 2] = [".DS_Store", ".localized"];

/// Whether `dir` may be mounted over: missing, or a real folder with
/// nothing in it but Finder's own files.
fn check_folder(dir: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} is not a folder, so it was left alone", dir.display())));
    }
    let own = std::fs::read_dir(dir)?.flatten().any(|e| !FINDER_FILES.iter().any(|f| e.file_name() == *f));
    if own {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already has files in it, so it was left alone; move them and restart the engine to see container files there", dir.display())));
    }
    Ok(())
}

/// A mount, as the kernel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mounted {
    fstype: String,
    from: String,
}

impl Mounted {
    /// This engine's: NFS from its own socket. `mount_nfs` names the
    /// socket as the source, `</path/nfs.sock>:/`.
    fn is_ours(&self, socket: &str) -> bool {
        self.fstype == "nfs" && self.from == format!("<{socket}>:/")
    }
}

/// What is mounted on `dir`, if anything. Read from the kernel's table
/// with `MNT_NOWAIT`, so a mount whose server died never blocks this.
/// Names are compared without case, as the Mac's disks do by default, and
/// against the folder's real path, which is what the table holds.
fn mounted_at(dir: &Path) -> Option<Mounted> {
    let real = match (dir.parent().and_then(|p| std::fs::canonicalize(p).ok()), dir.file_name()) {
        (Some(p), Some(n)) => p.join(n),
        _ => dir.to_path_buf(),
    };
    let trim = |p: &str| if p.len() > 1 { p.trim_end_matches('/').to_owned() } else { p.to_owned() };
    let same = |on: &str| [dir, real.as_path()].iter().any(|d| d.to_str().is_some_and(|d| trim(d).eq_ignore_ascii_case(&trim(on))));
    mounts().into_iter().find(|(on, _)| same(on)).map(|(_, m)| m)
}

/// Every mount: where it is, and what it is.
fn mounts() -> Vec<(String, Mounted)> {
    let text = |c: &[libc::c_char]| {
        // SAFETY: c_char and u8 have the same size; the kernel NUL-terminates.
        let b: &[u8] = unsafe { std::slice::from_raw_parts(c.as_ptr().cast(), c.len()) };
        String::from_utf8_lossy(&b[..b.iter().position(|&x| x == 0).unwrap_or(b.len())]).into_owned()
    };
    for _ in 0..4 {
        // SAFETY: a null buffer asks for the count only.
        let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if n < 0 {
            return Vec::new();
        }
        // Room for a few more, in case one is added in between.
        let cap = n as usize + 8;
        // SAFETY: statfs is plain data; all zeroes is a valid value.
        let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; cap];
        let size = (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int;
        // SAFETY: the buffer holds `cap` entries, `size` bytes.
        let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), size, libc::MNT_NOWAIT) };
        if got < 0 {
            return Vec::new();
        }
        if (got as usize) < cap {
            buf.truncate(got as usize);
            return buf.iter().map(|s| (text(&s.f_mntonname), Mounted { fstype: text(&s.f_fstypename), from: text(&s.f_mntfromname) })).collect();
        }
    }
    Vec::new()
}

/// What a handle names: the root, or a path inside a container.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Node {
    Root,
    File { id: String, path: String },
}

#[derive(Default)]
struct Handles {
    generation: u64,
    by_node: HashMap<Node, u64>,
    nodes: Vec<Node>,
}

impl Handles {
    /// The handle for `node`, allocating one. Index 0 is always the root.
    fn get(&mut self, node: Node) -> [u8; 16] {
        if node == Node::Root {
            return encode_fh(u64::MAX, 0);
        }
        if let Some(&i) = self.by_node.get(&node) {
            return encode_fh(self.generation, i);
        }
        if self.nodes.len() >= MAX_HANDLES {
            self.generation += 1;
            self.by_node.clear();
            self.nodes.clear();
        }
        let i = self.nodes.len() as u64 + 1;
        self.nodes.push(node.clone());
        self.by_node.insert(node, i);
        encode_fh(self.generation, i)
    }

    fn node(&self, fh: &[u8]) -> Result<Node, u32> {
        let (generation, i) = decode_fh(fh).ok_or(BADHANDLE)?;
        if generation == u64::MAX && i == 0 {
            return Ok(Node::Root);
        }
        if generation != self.generation {
            return Err(STALE);
        }
        self.nodes
            .get((i as usize).wrapping_sub(1))
            .cloned()
            .ok_or(STALE)
    }

    /// A stable file id: the handle's index, unique across the export.
    fn fileid(fh: &[u8; 16]) -> u64 {
        decode_fh(fh).map_or(1, |(_, i)| i + 1)
    }
}

fn encode_fh(generation: u64, i: u64) -> [u8; 16] {
    let mut fh = [0u8; 16];
    fh[..8].copy_from_slice(&generation.to_be_bytes());
    fh[8..].copy_from_slice(&i.to_be_bytes());
    fh
}

fn decode_fh(fh: &[u8]) -> Option<(u64, u64)> {
    let fh: &[u8; 16] = fh.try_into().ok()?;
    Some((
        u64::from_be_bytes(fh[..8].try_into().ok()?),
        u64::from_be_bytes(fh[8..].try_into().ok()?),
    ))
}

/// XDR decoding over one call's bytes.
struct Args<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Args<'a> {
    fn u32(&mut self) -> Option<u32> {
        let v = self.b.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_be_bytes(v.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some((u64::from(self.u32()?) << 32) | u64::from(self.u32()?))
    }

    fn opaque(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        let v = self.b.get(self.pos..self.pos.checked_add(len)?)?;
        self.pos += len.div_ceil(4) * 4;
        Some(v)
    }

    fn fixed(&mut self, len: usize) -> Option<&'a [u8]> {
        let v = self.b.get(self.pos..self.pos + len)?;
        self.pos += len;
        Some(v)
    }

    fn str(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.opaque()?).ok()
    }
}

/// XDR encoding into a reply.
#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn bool(&mut self, v: bool) -> &mut Self {
        self.u32(u32::from(v))
    }

    fn opaque(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self.0.resize(self.0.len().div_ceil(4) * 4, 0);
        self
    }
}

/// Attributes of something the client can see.
struct Attr {
    dir: bool,
    link: bool,
    mode: u32,
    size: u64,
    mtime: i64,
    mtime_nsec: u32,
    fileid: u64,
}

impl Attr {
    fn of(f: &FileInfo, fileid: u64) -> Attr {
        let kind = f.mode & 0o170000;
        Attr {
            dir: kind == 0o040000,
            link: kind == 0o120000,
            mode: f.mode,
            size: f.size,
            mtime: f.mtime,
            mtime_nsec: f.mtime_nsec,
            fileid,
        }
    }

    fn dir(fileid: u64) -> Attr {
        Attr {
            dir: true,
            link: false,
            mode: 0o040755,
            size: 4096,
            mtime: 0,
            mtime_nsec: 0,
            fileid,
        }
    }

    /// fattr3. Files belong to the Mac user and read as read-only, so Finder
    /// opens them without asking and shows the lock.
    fn put(&self, o: &mut Out) {
        let ftype = if self.dir {
            2
        } else if self.link {
            5
        } else {
            match self.mode & 0o170000 {
                0o100000 => 1,
                0o060000 => 3,
                0o020000 => 4,
                0o140000 => 6,
                0o010000 => 7,
                _ => 1,
            }
        };
        let mut mode = (self.mode & 0o7777 & !0o222) | 0o444;
        if self.dir {
            mode |= 0o111;
        }
        // SAFETY: getuid/getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        o.u32(ftype)
            .u32(mode)
            .u32(if self.dir { 2 } else { 1 })
            .u32(uid)
            .u32(gid);
        o.u64(self.size)
            .u64(self.size.div_ceil(4096) * 4096)
            .u32(0)
            .u32(0)
            .u64(FSID)
            .u64(self.fileid);
        for _ in 0..3 {
            o.u32(self.mtime.max(0) as u32).u32(self.mtime_nsec);
        }
    }

    fn post_op(&self, o: &mut Out) {
        o.bool(true);
        self.put(o);
    }
}

struct Server<'a> {
    engine: &'a Engine,
    handles: &'a Mutex<Handles>,
    /// The last directory listed: READDIR pages through one listing.
    listing: Option<([u8; 16], Vec<FileInfo>)>,
}

impl Server<'_> {
    fn serve(mut self, mut conn: UnixStream) -> io::Result<()> {
        let mut record = Vec::new();
        loop {
            record.clear();
            // Record marking: fragments until one has the last bit set.
            loop {
                let mut mark = [0u8; 4];
                conn.read_exact(&mut mark)?;
                let mark = u32::from_be_bytes(mark);
                let len = (mark & 0x7fff_ffff) as usize;
                if record.len() + len > MAX_RECORD {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "RPC record too large",
                    ));
                }
                let at = record.len();
                record.resize(at + len, 0);
                conn.read_exact(&mut record[at..])?;
                if mark & 0x8000_0000 != 0 {
                    break;
                }
            }
            let Some(reply) = self.call(&record) else {
                continue;
            };
            let mut framed = Vec::with_capacity(reply.len() + 4);
            framed.extend_from_slice(&(0x8000_0000 | reply.len() as u32).to_be_bytes());
            framed.extend_from_slice(&reply);
            conn.write_all(&framed)?;
        }
    }

    fn call(&mut self, record: &[u8]) -> Option<Vec<u8>> {
        let mut a = Args { b: record, pos: 0 };
        let xid = a.u32()?;
        if a.u32()? != 0 {
            return None; // not a CALL
        }
        let mut o = Out::default();
        o.u32(xid).u32(1).u32(0); // REPLY, MSG_ACCEPTED
        if a.u32()? != 2 {
            // RPC_MISMATCH is a denied reply.
            let mut d = Out::default();
            d.u32(xid).u32(1).u32(1).u32(0).u32(2).u32(2);
            return Some(d.0);
        }
        let (prog, vers, proc_) = (a.u32()?, a.u32()?, a.u32()?);
        a.u32()?; // credential flavor
        a.opaque()?;
        a.u32()?; // verifier flavor
        a.opaque()?;
        o.u32(0).u32(0); // AUTH_NONE verifier
        match (prog, vers) {
            (PROG_NFS, 3) => {
                o.u32(0); // SUCCESS
                if self.nfs(proc_, &mut a, &mut o).is_none() {
                    return Some(garbage(xid));
                }
            }
            (PROG_MOUNT, 3) => {
                o.u32(0);
                if self.mount_proc(proc_, &mut a, &mut o).is_none() {
                    return Some(garbage(xid));
                }
            }
            (PROG_NFS | PROG_MOUNT, _) => {
                o.u32(2).u32(3).u32(3); // PROG_MISMATCH: only version 3
            }
            _ => {
                o.u32(1); // PROG_UNAVAIL
            }
        }
        Some(o.0)
    }

    fn mount_proc(&mut self, proc_: u32, a: &mut Args<'_>, o: &mut Out) -> Option<()> {
        match proc_ {
            0 | 3 | 4 => {} // NULL, UMNT, UMNTALL
            1 => {
                a.str()?;
                let fh = self
                    .handles
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(Node::Root);
                o.u32(OK).opaque(&fh).u32(1).u32(1); // AUTH_SYS
            }
            5 => {
                // EXPORT: one export, "/"; the socket decides who reaches it.
                o.bool(true).opaque(b"/").bool(false).bool(false);
            }
            _ => {
                o.0.truncate(o.0.len() - 4);
                o.u32(3); // PROC_UNAVAIL
            }
        }
        Some(())
    }

    fn nfs(&mut self, proc_: u32, a: &mut Args<'_>, o: &mut Out) -> Option<()> {
        match proc_ {
            0 => {}
            1 => {
                let fh = a.opaque()?;
                match self.attr(fh) {
                    Ok(at) => {
                        o.u32(OK);
                        at.put(o);
                    }
                    Err(e) => {
                        o.u32(e);
                    }
                }
            }
            3 => {
                let dir = a.opaque()?;
                let name = a.str()?;
                self.lookup(dir, name, o);
            }
            4 => {
                let fh = a.opaque()?;
                let want = a.u32()?;
                match self.attr(fh) {
                    Ok(at) => {
                        o.u32(OK);
                        at.post_op(o);
                        o.u32(want & (0x01 | 0x02 | 0x20)); // READ, LOOKUP, EXECUTE
                    }
                    Err(e) => {
                        o.u32(e).bool(false);
                    }
                }
            }
            5 => {
                // Links are followed inside the container, so none are shown.
                a.opaque()?;
                o.u32(NOTSUPP).bool(false);
            }
            6 => {
                let fh = a.opaque()?;
                let (offset, count) = (a.u64()?, a.u32()?);
                self.read(fh, offset, count.min(MAX_READ), o);
            }
            16 | 17 => {
                let fh: [u8; 16] = a.opaque()?.try_into().unwrap_or([0; 16]);
                let cookie = a.u64()?;
                a.fixed(8)?;
                let count = if proc_ == 17 {
                    a.u32()?;
                    a.u32()?
                } else {
                    a.u32()?
                };
                self.readdir(&fh, cookie, count, proc_ == 17, o);
            }
            18 => {
                let fh = a.opaque()?;
                match self.attr(fh) {
                    Ok(at) => {
                        o.u32(OK);
                        at.post_op(o);
                        o.u64(1 << 40)
                            .u64(0)
                            .u64(0)
                            .u64(1 << 20)
                            .u64(0)
                            .u64(0)
                            .u32(0);
                    }
                    Err(e) => {
                        o.u32(e).bool(false);
                    }
                }
            }
            19 => {
                let fh = a.opaque()?;
                match self.attr(fh) {
                    Ok(at) => {
                        o.u32(OK);
                        at.post_op(o);
                        o.u32(MAX_READ)
                            .u32(MAX_READ)
                            .u32(4096)
                            .u32(0)
                            .u32(0)
                            .u32(4096)
                            .u32(64 * 1024);
                        o.u64(u64::MAX >> 1).u32(0).u32(1).u32(0x08); // FSF_HOMOGENEOUS
                    }
                    Err(e) => {
                        o.u32(e).bool(false);
                    }
                }
            }
            20 => {
                let fh = a.opaque()?;
                match self.attr(fh) {
                    Ok(at) => {
                        o.u32(OK);
                        at.post_op(o);
                        o.u32(1)
                            .u32(255)
                            .bool(true)
                            .bool(true)
                            .bool(false)
                            .bool(true);
                    }
                    Err(e) => {
                        o.u32(e).bool(false);
                    }
                }
            }
            // Everything that would change a file: the export is read-only.
            2 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 21 => {
                o.u32(ROFS).u32(0).u32(0);
            }
            14 => {
                o.u32(ROFS).u32(0).u32(0).u32(0).u32(0);
            }
            15 => {
                o.u32(ROFS).u32(0).u32(0).u32(0);
            }
            _ => {
                o.0.truncate(o.0.len() - 4);
                o.u32(3); // PROC_UNAVAIL
            }
        }
        Some(())
    }

    fn node(&self, fh: &[u8]) -> Result<Node, u32> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .node(fh)
    }

    fn handle(&self, node: Node) -> [u8; 16] {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(node)
    }

    /// The running container `id`, or STALE once it stopped.
    fn running(&self, id: &str) -> Result<(), u32> {
        match self.engine.with(id, |c| c.status == Status::Running) {
            Ok(true) => Ok(()),
            _ => Err(STALE),
        }
    }

    fn attr(&self, fh: &[u8]) -> Result<Attr, u32> {
        let fh16: [u8; 16] = fh.try_into().map_err(|_| BADHANDLE)?;
        match self.node(fh)? {
            Node::Root => Ok(self.root_attr()),
            Node::File { id, path } => {
                self.running(&id)?;
                let f = self.engine.fs_stat(&id, &path).map_err(errno)?;
                Ok(Attr::of(&f, Handles::fileid(&fh16)))
            }
        }
    }

    /// The root changes whenever the set of running containers does; its
    /// mtime says so, which is what makes the client list it again.
    fn root_attr(&self) -> Attr {
        let mut names = self
            .engine
            .each(|c| (c.status == Status::Running).then(|| c.id.clone()));
        names.sort();
        let h = names.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, n| {
            n.bytes()
                .fold(h, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
        });
        Attr {
            mtime: 1_700_000_000 + (h % 1_000_000) as i64,
            mtime_nsec: ((h >> 20) % 1_000_000_000) as u32,
            ..Attr::dir(1)
        }
    }

    fn lookup(&mut self, dir: &[u8], name: &str, o: &mut Out) {
        let dir_attr = match self.attr(dir) {
            Ok(a) => a,
            Err(e) => {
                o.u32(e).bool(false);
                return;
            }
        };
        if !dir_attr.dir {
            o.u32(NOTDIR);
            dir_attr.post_op(o);
            return;
        }
        let found = match self.node(dir) {
            _ if name == "." => Ok((dir.try_into().unwrap_or([0; 16]), None)),
            Ok(Node::Root) => {
                // Names are not unique over time; the running one wins.
                let id = self
                    .engine
                    .each(|c| (c.status == Status::Running && c.name == name).then(|| c.id.clone()))
                    .into_iter()
                    .next();
                match id {
                    Some(id) => Ok((
                        self.handle(Node::File {
                            id,
                            path: "/".into(),
                        }),
                        None,
                    )),
                    None => Err(NOENT),
                }
            }
            Ok(Node::File { id, path }) => {
                let child = if name == ".." {
                    match path.rsplit_once('/') {
                        Some(("", _)) | None => "/".to_owned(),
                        Some((parent, _)) => parent.to_owned(),
                    }
                } else if name.contains('/') {
                    String::new()
                } else {
                    format!("{}/{name}", path.trim_end_matches('/'))
                };
                if child.is_empty() {
                    Err(NOENT)
                } else {
                    match self.engine.fs_stat(&id, &child) {
                        Ok(f) => Ok((self.handle(Node::File { id, path: child }), Some(f))),
                        Err(e) => Err(errno(e)),
                    }
                }
            }
            Err(e) => Err(e),
        };
        match found {
            Ok((fh, info)) => {
                o.u32(OK).opaque(&fh);
                let at = match info {
                    Some(f) => Ok(Attr::of(&f, Handles::fileid(&fh))),
                    None => self.attr(&fh),
                };
                match at {
                    Ok(at) => at.post_op(o),
                    Err(_) => {
                        o.bool(false);
                    }
                }
                dir_attr.post_op(o);
            }
            Err(e) => {
                o.u32(e);
                dir_attr.post_op(o);
            }
        }
    }

    fn read(&self, fh: &[u8], offset: u64, count: u32, o: &mut Out) {
        let Node::File { id, path } = (match self.node(fh) {
            Ok(n) => n,
            Err(e) => {
                o.u32(e).bool(false);
                return;
            }
        }) else {
            o.u32(21).bool(false); // ISDIR
            return;
        };
        if let Err(e) = self.running(&id) {
            o.u32(e).bool(false);
            return;
        }
        match self.engine.fs_read(&id, &path, offset, count) {
            Ok(data) => {
                // No attributes: the client already holds fresh ones from its
                // open, and fetching them would cost another guest round trip.
                o.u32(OK)
                    .bool(false)
                    .u32(data.len() as u32)
                    .bool((data.len() as u32) < count)
                    .opaque(&data);
            }
            Err(e) => {
                o.u32(errno(e)).bool(false);
            }
        }
    }

    fn readdir(&mut self, fh: &[u8; 16], cookie: u64, count: u32, plus: bool, o: &mut Out) {
        let dir_attr = match self.attr(fh) {
            Ok(a) => a,
            Err(e) => {
                o.u32(e).bool(false);
                return;
            }
        };
        if !dir_attr.dir {
            o.u32(NOTDIR);
            dir_attr.post_op(o);
            return;
        }
        let node = self.node(fh).unwrap_or(Node::Root);
        // A listing is fetched once per pass: cookie 0 starts a new one.
        let fresh = cookie == 0 || self.listing.as_ref().is_none_or(|(h, _)| h != fh);
        if fresh {
            let list = match &node {
                Node::Root => {
                    let mut names = self.engine.each(|c| {
                        (c.status == Status::Running).then(|| FileInfo {
                            name: c.name.clone(),
                            mode: 0o040755,
                            size: 4096,
                            mtime: 0,
                            mtime_nsec: 0,
                        })
                    });
                    names.sort_by(|a, b| a.name.cmp(&b.name));
                    names
                }
                Node::File { id, path } => match self.engine.fs_list(id, path) {
                    Ok(l) => l,
                    Err(e) => {
                        o.u32(errno(e));
                        dir_attr.post_op(o);
                        return;
                    }
                },
            };
            self.listing = Some((*fh, list));
        }
        let entries = self
            .listing
            .as_ref()
            .map(|(_, l)| l.clone())
            .unwrap_or_default();
        o.u32(OK);
        dir_attr.post_op(o);
        o.u64(0); // cookie verifier
        // Leave room for the reply header and the closing words.
        let budget = (count as usize).saturating_sub(128);
        let start = o.0.len();
        let mut eof = true;
        for (i, f) in entries.iter().enumerate().skip(cookie as usize) {
            let child = match &node {
                Node::Root => self
                    .engine
                    .each(|c| {
                        (c.status == Status::Running && c.name == f.name).then(|| c.id.clone())
                    })
                    .into_iter()
                    .next()
                    .map(|id| Node::File {
                        id,
                        path: "/".into(),
                    }),
                Node::File { id, path } => Some(Node::File {
                    id: id.clone(),
                    path: format!("{}/{}", path.trim_end_matches('/'), f.name),
                }),
            };
            let Some(child) = child else { continue };
            let cfh = self.handle(child);
            let mut e = Out::default();
            e.bool(true)
                .u64(Handles::fileid(&cfh))
                .opaque(f.name.as_bytes())
                .u64(i as u64 + 1);
            if plus {
                if matches!(node, Node::Root) {
                    // Container folders' own attributes come with LOOKUP/GETATTR.
                    e.bool(false);
                } else {
                    Attr::of(f, Handles::fileid(&cfh)).post_op(&mut e);
                }
                e.bool(true).opaque(&cfh);
            }
            if o.0.len() - start + e.0.len() > budget {
                eof = false;
                break;
            }
            o.0.extend_from_slice(&e.0);
        }
        o.bool(false).bool(eof);
    }
}

fn garbage(xid: u32) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(xid).u32(1).u32(0).u32(0).u32(0).u32(4); // GARBAGE_ARGS
    o.0
}

/// The guest reports errors as text; map the common ones.
fn errno(e: io::Error) -> u32 {
    let m = e.to_string();
    if m.contains("No such file") || m.contains("not found") {
        NOENT
    } else if m.contains("Permission denied") {
        ACCES
    } else if m.contains("Not a directory") {
        NOTDIR
    } else if m.contains("not running") {
        STALE
    } else {
        IO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_round_trip_and_roll_over() {
        let mut h = Handles::default();
        let root = h.get(Node::Root);
        assert_eq!(h.node(&root), Ok(Node::Root));
        let a = Node::File {
            id: "c1".into(),
            path: "/etc".into(),
        };
        let fa = h.get(a.clone());
        assert_eq!(h.get(a.clone()), fa);
        assert_eq!(h.node(&fa), Ok(a));
        assert_eq!(h.node(b"short"), Err(BADHANDLE));
        for i in 0..MAX_HANDLES {
            h.get(Node::File {
                id: "c1".into(),
                path: format!("/{i}"),
            });
        }
        // The table started over: the old handle is stale, the root is not.
        assert_eq!(h.node(&fa), Err(STALE));
        assert_eq!(h.node(&root), Ok(Node::Root));
        assert!(h.nodes.len() <= MAX_HANDLES);
    }

    /// This user's own connections get in (as does the Mac's NFS client, root).
    #[test]
    fn own_user_is_allowed() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(allowed(&a));
    }

    #[test]
    fn xdr_opaque_is_padded() {
        let mut o = Out::default();
        o.opaque(b"abcde");
        assert_eq!(o.0.len(), 4 + 8);
        let mut a = Args { b: &o.0, pos: 0 };
        assert_eq!(a.opaque(), Some(&b"abcde"[..]));
        assert_eq!(a.pos, 12);
        assert_eq!(a.u32(), None);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kiri-nfs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The folder is covered only when nothing of the person's would be
    /// hidden: missing, empty, or holding only Finder's own files.
    #[test]
    fn mounts_only_over_an_empty_folder() {
        let d = scratch("folder");
        assert!(check_folder(&d.join("missing")).is_ok());
        let empty = d.join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(check_folder(&empty).is_ok());
        let finder = d.join("finder");
        std::fs::create_dir(&finder).unwrap();
        std::fs::write(finder.join(".DS_Store"), b"x").unwrap();
        std::fs::write(finder.join(".localized"), b"").unwrap();
        assert!(check_folder(&finder).is_ok());
        let files = d.join("files");
        std::fs::create_dir(&files).unwrap();
        std::fs::write(files.join("keep.txt"), b"keep").unwrap();
        assert_eq!(check_folder(&files).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let hidden = d.join("hidden");
        std::fs::create_dir_all(hidden.join(".git")).unwrap();
        assert!(check_folder(&hidden).is_err());
        let file = d.join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(check_folder(&file).is_err());
        let link = d.join("link");
        std::os::unix::fs::symlink(&empty, &link).unwrap();
        assert!(check_folder(&link).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Only an NFS mount from this engine's own socket is this engine's.
    #[test]
    fn knows_its_own_mount() {
        let sock = "/Users/a/.kiri/files/nfs.sock";
        let m = |fstype: &str, from: &str| Mounted { fstype: fstype.into(), from: from.into() };
        assert!(m("nfs", "</Users/a/.kiri/files/nfs.sock>:/").is_ours(sock));
        assert!(!m("nfs", "</Users/a/.kiri-other/files/nfs.sock>:/").is_ours(sock));
        assert!(!m("nfs", "server:/export").is_ours(sock));
        assert!(!m("apfs", "/dev/disk5s1").is_ours(sock));
        assert!(!m("hfs", "</Users/a/.kiri/files/nfs.sock>:/").is_ours(sock));
    }

    /// The kernel's table is read, and a folder with nothing on it has no mount.
    #[test]
    fn reads_the_mount_table() {
        assert!(mounts().iter().any(|(on, _)| on == "/"));
        assert_eq!(mounted_at(Path::new("/")).map(|m| m.from.is_empty()), Some(false));
        let d = scratch("table");
        assert_eq!(mounted_at(&d), None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
