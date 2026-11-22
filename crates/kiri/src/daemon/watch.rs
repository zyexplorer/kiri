//! Changes made on the Mac to bind-mounted files, sent into the VM as
//! inotify events.
//!
//! The VM sees the Mac's files over virtiofs, and Apple's virtiofs server
//! tells the guest nothing when a file changes on the Mac: a file watcher in
//! a container (Vite, webpack, nodemon, `cargo watch`) never heard of a save
//! made in an editor on the Mac. Reads were always fresh; only the event was
//! missing.
//!
//! This watches the bind sources of running containers with FSEvents and
//! sends each change to the guest agent as `OP\tPATH` (`Msg::FsEvents`),
//! which raises the inotify event Linux would for a change made in the VM
//! (crates/kiri-init/src/linux/fsnotify.rs, and the guest kernel's
//! /proc/kiri-fsnotify). Nothing is written to the files.
//!
//! A change a container makes itself reaches the Mac too, and comes back as
//! a second event: watchers see it twice, which they take as one change.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Weak};
use std::time::Duration;

use super::engine::Engine;

/// Where the Mac's filesystem is mounted in the VM.
const MAC_ROOT: &str = "/mnt/mac";
/// How often the set of bind sources is looked at again.
const RESCAN: Duration = Duration::from_secs(1);
/// FSEvents' latency: changes within it arrive together.
const LATENCY: f64 = 0.05;
/// At most this many lines per message to the guest.
const BATCH: usize = 512;

pub fn start(engine: Weak<Engine>) {
    let _ = std::thread::Builder::new().name("kiri-watch".into()).spawn(move || run(engine));
}

fn run(engine: Weak<Engine>) {
    let (tx, rx) = channel::<Vec<(String, u32)>>();
    let mut stream: Option<Stream> = None;
    let mut roots: Vec<Root> = Vec::new();
    let mut known = Known::new();
    loop {
        let Some(e) = engine.upgrade() else { return };
        let now = e.mac_bind_sources();
        if !roots.iter().map(|r| &r.path).eq(now.iter()) {
            // The new stream picks up where the old one stopped, so changes
            // made in between are not lost; Known is kept with it.
            let last = stream.as_ref().map_or(0, Stream::last_id);
            drop(stream.take()); // stops the old one
            // A source new to this stream has nothing from before now: the
            // resumed stream would replay its folder's whole history.
            let cur = current_event_id();
            roots = now.into_iter().map(|path| roots.iter().find(|r| r.path == path).cloned().unwrap_or_else(|| Root::new(path, cur))).collect();
            let since = since_for(last);
            if since == SINCE_NOW {
                known = Known::new();
            }
            known.sync(&roots);
            if !roots.is_empty() {
                let dirs = watch_dirs(&roots.iter().map(|r| r.path.clone()).collect::<Vec<_>>());
                stream = Stream::start(&dirs, roots.clone(), tx.clone(), since, cur).inspect_err(|err| crate::diag::warn!("could not watch bind mounts for changes", error = err)).ok();
            }
        }
        drop(e);
        let first = match rx.recv_timeout(RESCAN) {
            Ok(events) => events,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let lines = lines(first, &rx, &roots, &mut known);
        if lines.is_empty() || stream.is_none() {
            continue;
        }
        let Some(e) = engine.upgrade() else { return };
        if let Some(vm) = e.awake_link() {
            for chunk in lines.chunks(BATCH) {
                let _ = vm.fs_events(chunk);
            }
        }
    }
}

/// The folders to give FSEvents: a bind-mounted folder itself, a bind-mounted
/// file's folder (FSEvents watches folders).
fn watch_dirs(roots: &[String]) -> Vec<String> {
    let mut dirs: Vec<String> = roots
        .iter()
        .map(|r| if std::path::Path::new(r).is_dir() { r.clone() } else { std::path::Path::new(r).parent().map_or_else(|| "/".to_owned(), |p| p.display().to_string()) })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// This batch and whatever else is already waiting, as guest lines: each
/// path looked at once, however many times it changed.
fn lines(first: Vec<(String, u32)>, rx: &Receiver<Vec<(String, u32)>>, roots: &[Root], known: &mut Known) -> Vec<String> {
    let mut order = Vec::new();
    let mut flags: HashMap<String, u32> = HashMap::new();
    let mut rescan = Vec::new();
    for (path, f) in first.into_iter().chain(rx.try_iter().flatten()) {
        // FSEvents lost changes under `path`: look at the files themselves.
        if f & flag::LOST != 0 {
            rescan.push(path.clone());
        }
        let Some(path) = under(&path, roots) else { continue };
        match flags.get_mut(&path) {
            Some(old) => *old |= f,
            None => {
                flags.insert(path.clone(), f);
                order.push(path);
            }
        }
    }
    for dir in rescan {
        for path in known.rescan(&dir, roots) {
            // Named by a rescan, not by FSEvents: nothing is known to have
            // happened to it.
            if !flags.contains_key(&path) {
                flags.insert(path.clone(), flag::LOST);
                order.push(path);
            }
        }
    }
    let mut out = Vec::new();
    for path in order {
        for op in known.check(&path, flags[&path]) {
            out.push(format!("{op}\t{MAC_ROOT}{path}"));
        }
    }
    out
}

/// Where a new stream starts: after the last event the old one delivered,
/// or now if it delivered none.
fn since_for(last: u64) -> u64 {
    if last == 0 { SINCE_NOW } else { last }
}

/// A bind source being watched.
#[derive(Clone, Debug)]
struct Root {
    path: String,
    /// Its volume tells apart names that differ only by case. Looked at once
    /// here, not per event.
    sensitive: bool,
    /// Events at or below this id happened before the source was added.
    start_id: u64,
}

impl Root {
    fn new(path: String, start_id: u64) -> Root {
        let sensitive = case_sensitive(&path);
        Root { path, sensitive, start_id }
    }
}

/// Whether the volume of `path` is case-sensitive; the macOS default
/// (insensitive) when that can't be told.
fn case_sensitive(path: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(path) else { return false };
    // SAFETY: a NUL-terminated path.
    unsafe { libc::pathconf(c.as_ptr(), libc::_PC_CASE_SENSITIVE) == 1 }
}

/// The newest event id FSEvents has issued so far.
fn current_event_id() -> u64 {
    // SAFETY: takes no arguments.
    unsafe { FSEventsGetCurrentEventId() }
}

/// `path` spelled as `root` if it is or is inside it, else None.
fn spell(path: &str, root: &str, sensitive: bool) -> Option<String> {
    let head = path.get(..root.len())?;
    let rest = &path[root.len()..];
    let same = if sensitive { head == root } else { head.eq_ignore_ascii_case(root) };
    (same && (rest.is_empty() || rest.starts_with('/'))).then(|| format!("{root}{rest}"))
}

/// The bind source `path` is or is inside, and `path` spelled as that source
/// is written. Mac folders ignore case unless their volume says otherwise,
/// and FSEvents may spell a path as it is on disk rather than as the bind
/// source was written.
fn root_of<'a>(path: &str, roots: &'a [Root]) -> Option<(&'a Root, String)> {
    roots.iter().find_map(|r| spell(path, &r.path, r.sensitive).map(|s| (r, s)))
}

fn under(path: &str, roots: &[Root]) -> Option<String> {
    root_of(path, roots).map(|(_, s)| s)
}

/// Whether an event is to be ignored: the end of a history replay, or a
/// change under a source from before the source was added. Root-changed and
/// mount events carry no id and always count; a path under no source (a
/// lost-changes event for a folder above one) is left to the caller. Under
/// nested sources the oldest decides: a parent added later must not hide a
/// change the already-watched child is owed.
fn stale(path: &str, id: u64, flags: u32, roots: &[Root]) -> bool {
    if flags & flag::HISTORY_DONE != 0 {
        return true;
    }
    if flags & (flag::ROOT_CHANGED | flag::MOUNT | flag::UNMOUNT) != 0 {
        return false;
    }
    roots
        .iter()
        .filter(|r| spell(path, &r.path, r.sensitive).is_some())
        .map(|r| r.start_id)
        .min()
        .is_some_and(|start| id <= start)
}

/// `dir` without trailing slashes ("/" stays).
fn trimmed(dir: &str) -> &str {
    let t = dir.trim_end_matches('/');
    if t.is_empty() { "/" } else { t }
}

/// What a path was when last looked at: enough to tell what changed.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Meta {
    dir: bool,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    mode: u32,
    owner: (u32, u32),
}

impl Meta {
    /// The path's Meta, and whether it was made since `since`.
    fn of(path: &str, since: std::time::SystemTime) -> Option<(Meta, bool)> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path).ok()?;
        let new = m.created().is_ok_and(|t| t >= since);
        Some((Meta { dir: m.is_dir(), ino: m.ino(), size: m.size(), mtime: (m.mtime(), m.mtime_nsec()), ctime: (m.ctime(), m.ctime_nsec()), mode: m.mode(), owner: (m.uid(), m.gid()) }, new))
    }
}

/// What changed, from what the path was and is. FSEvents' flags only say
/// which paths to look at: they keep a path's earlier changes (a chmod
/// after a write still says "modified"), and can be lost in a storm.
/// `new`: the path was made since watching began, so unknown means created.
/// `flags`: FSEvents' flags for the path, used only for a path not known
/// before, where there is nothing to compare with: a created or renamed
/// path that is there is created, a removed or renamed one that is gone is
/// deleted. A rescan (`flags` says changes were lost, with no item flag)
/// only records a path it hasn't seen.
fn ops(was: Option<Meta>, now: Option<Meta>, new: bool, flags: u32) -> Vec<char> {
    let quiet = flags & flag::LOST != 0 && flags & flag::ITEM == 0;
    match (was, now) {
        (None, None) => if flags & (flag::REMOVED | flag::RENAMED) != 0 { vec!['D'] } else { Vec::new() },
        (Some(_), None) => vec!['D'],
        // A file made with content: created, then written, as in the VM.
        (None, Some(n)) if new => if n.dir { vec!['C'] } else { vec!['C', 'M'] },
        // Here before watching began and not seen since: the flags say what
        // happened (a folder gets nothing but its creation: children say more).
        (None, Some(n)) if flags & (flag::CREATED | flag::RENAMED) != 0 => if n.dir { vec!['C'] } else { vec!['C', 'M'] },
        (None, Some(_)) if quiet => Vec::new(),
        (None, Some(n)) => if n.dir { Vec::new() } else { vec!['M'] },
        // Folders: a child's change moves a folder's times, which Linux
        // doesn't report on the folder; only its own mode or owner count.
        (Some(w), Some(n)) if n.dir => {
            if !w.dir {
                vec!['D', 'C']
            } else if (w.mode, w.owner) != (n.mode, n.owner) {
                vec!['A']
            } else {
                Vec::new()
            }
        }
        (Some(w), Some(n)) => {
            if w.dir {
                vec!['D', 'C', 'M']
            } else if w.ino != n.ino || w.size != n.size || w.mtime != n.mtime {
                // Written, or replaced by a rename onto it (how editors save).
                vec!['M']
            } else if w.ctime != n.ctime || (w.mode, w.owner) != (n.mode, n.owner) {
                vec!['A']
            } else {
                Vec::new()
            }
        }
    }
}

/// Every path looked at since watching began, as it was then.
struct Known {
    paths: HashMap<String, Meta>,
    /// Used for a path under no source in `starts`.
    since: std::time::SystemTime,
    /// When each source began to be watched: a file made before that isn't new.
    starts: Vec<(String, std::time::SystemTime)>,
}

/// Paths remembered at most; past it, the oldest knowledge is dropped (a
/// path then counts as "here before watching began").
const MAX_KNOWN: usize = 200_000;
/// Paths a rescan looks at at most.
const MAX_RESCAN: usize = 100_000;

impl Known {
    fn new() -> Known {
        // A second back: a file made just before the stream started is new.
        Known { paths: HashMap::new(), since: std::time::SystemTime::now() - Duration::from_secs(1), starts: Vec::new() }
    }

    /// Follows the set of sources: a source new here starts being watched
    /// now, one gone has its paths forgotten.
    fn sync(&mut self, roots: &[Root]) {
        self.starts.retain(|(p, _)| roots.iter().any(|r| r.path == *p));
        for r in roots {
            if !self.starts.iter().any(|(p, _)| *p == r.path) {
                // A second back: a file made just before it was added is new.
                self.starts.push((r.path.clone(), std::time::SystemTime::now() - Duration::from_secs(1)));
            }
        }
        self.paths.retain(|p, _| under(p, roots).is_some());
    }

    /// The time before which a file under the source of `path` isn't new.
    fn since_of(&self, path: &str) -> std::time::SystemTime {
        // `path` is spelled as its source, so an exact match is right.
        self.starts.iter().find(|(r, _)| spell(path, r, true).is_some()).map_or(self.since, |(_, t)| *t)
    }

    /// What changed at `path` since it was last looked at, remembering now.
    fn check(&mut self, path: &str, flags: u32) -> Vec<char> {
        let now = Meta::of(path, self.since_of(path));
        let was = match now {
            Some((m, _)) => {
                if self.paths.len() >= MAX_KNOWN && !self.paths.contains_key(path) {
                    self.paths.clear();
                }
                self.paths.insert(path.to_owned(), m)
            }
            None => self.paths.remove(path),
        };
        ops(was, now.map(|n| n.0), now.is_some_and(|n| n.1), flags)
    }

    /// The paths to look at after FSEvents lost changes under `dir`: what's
    /// there now and what was known there, inside the bind sources only.
    fn rescan(&self, dir: &str, roots: &[Root]) -> Vec<String> {
        let dir = trimmed(dir);
        // Every source `dir` is, or is above ("/" is above all of them) ...
        let mut tops: Vec<String> = roots.iter().filter(|r| dir == "/" || spell(&r.path, dir, r.sensitive).is_some()).map(|r| r.path.clone()).collect();
        // ... and `dir` itself when it is inside one.
        if let Some(d) = under(dir, roots) {
            tops.push(d);
        }
        let mut out = Vec::new();
        for top in tops {
            out.extend(self.paths.keys().filter(|p| spell(p, &top, true).is_some()).cloned());
            let mut stack = vec![top];
            while let Some(p) = stack.pop() {
                if out.len() >= MAX_RESCAN {
                    break;
                }
                if let Ok(entries) = std::fs::read_dir(&p) {
                    for e in entries.flatten() {
                        let child = format!("{p}/{}", e.file_name().to_string_lossy());
                        if e.file_type().is_ok_and(|t| t.is_dir()) {
                            stack.push(child.clone());
                        }
                        out.push(child);
                    }
                }
                out.push(p);
            }
        }
        out
    }
}

mod flag {
    pub const MUST_SCAN_SUB_DIRS: u32 = 0x1;
    pub const USER_DROPPED: u32 = 0x2;
    pub const KERNEL_DROPPED: u32 = 0x4;
    /// Changes under the path were lost.
    pub const LOST: u32 = MUST_SCAN_SUB_DIRS | USER_DROPPED | KERNEL_DROPPED;
    pub const CREATED: u32 = 0x100;
    pub const REMOVED: u32 = 0x200;
    pub const RENAMED: u32 = 0x800;
    pub const HISTORY_DONE: u32 = 0x10;
    pub const ROOT_CHANGED: u32 = 0x20;
    pub const MOUNT: u32 = 0x40;
    pub const UNMOUNT: u32 = 0x80;
    /// Every flag that says what happened to an item.
    pub const ITEM: u32 = 0xff00;
}

// FSEvents (CoreServices), CoreFoundation and libdispatch, as much as this uses.
type FSEventStreamRef = *mut c_void;
type CFRef = *const c_void;

#[repr(C)]
struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type Callback = extern "C" fn(FSEventStreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(alloc: CFRef, cb: Callback, ctx: *const FSEventStreamContext, paths: CFRef, since: u64, latency: f64, flags: u32) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: *mut c_void);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: u8;
    fn CFStringCreateWithBytes(alloc: CFRef, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CFRef;
    fn CFArrayCreate(alloc: CFRef, values: *const CFRef, count: isize, callbacks: *const u8) -> CFRef;
    fn CFRelease(cf: CFRef);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_sync_f(queue: *mut c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
}

const UTF8: u32 = 0x0800_0100;
const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x2;
const CREATE_FILE_EVENTS: u32 = 0x10;

/// What the callback has: where to send changes, and the bind sources, so
/// only their changes are copied out of FSEvents (a bind-mounted file is
/// watched through its whole folder, often the home folder).
struct Info {
    tx: Sender<Vec<(String, u32)>>,
    roots: Vec<Root>,
    warned: AtomicBool,
    /// The highest event id delivered so far.
    last: Arc<AtomicU64>,
}

/// A running FSEvents stream; dropping it stops it.
struct Stream(FSEventStreamRef, Arc<AtomicU64>);

impl Stream {
    /// The last event id the stream delivered, 0 if none.
    fn last_id(&self) -> u64 {
        self.1.load(Ordering::Relaxed)
    }
}

// SAFETY: the stream is only started and torn down from the thread that
// owns this, and FSEvents' calls are thread-safe.
unsafe impl Send for Stream {}

extern "C" fn on_events(_: FSEventStreamRef, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, ids: *const u64) {
    // SAFETY: FSEvents passes `n` C strings and `n` flags, and `info` is the
    // Info this stream was made with, freed only by `release_info` once the
    // stream is gone.
    let (info, paths, flags) = unsafe { (&*(info as *const Info), std::slice::from_raw_parts(paths as *const *const c_char, n), std::slice::from_raw_parts(flags, n)) };
    // SAFETY: as above, `n` ids.
    let ids = unsafe { std::slice::from_raw_parts(ids, n) };
    if let Some(&max) = ids.iter().max() {
        info.last.fetch_max(max, Ordering::Relaxed);
    }
    let mut events = Vec::new();
    for ((p, &f), &id) in paths.iter().zip(flags).zip(ids) {
        let lost = f & flag::LOST != 0;
        if lost && !info.warned.swap(true, Ordering::Relaxed) {
            crate::diag::info!("FSEvents dropped changes to bind mounts; looking at the files again");
        }
        // SAFETY: a NUL-terminated path from FSEvents.
        let Ok(path) = unsafe { CStr::from_ptr(*p) }.to_str() else { continue };
        if stale(path, id, f, &info.roots) {
            continue;
        }
        // A lost-changes event may name a folder above a bind source.
        if lost || under(path, &info.roots).is_some() {
            events.push((path.to_owned(), f));
        }
    }
    if !events.is_empty() {
        let _ = info.tx.send(events);
    }
}

/// The context's release: FSEvents calls it once the stream is released and
/// no callback can run again.
extern "C" fn release_info(info: *const c_void) {
    // SAFETY: the Box made in Stream::start, released once.
    drop(unsafe { Box::from_raw(info as *mut Info) });
}

fn queue() -> *mut c_void {
    static QUEUE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // SAFETY: a serial queue made once and kept for the process's life.
    *QUEUE.get_or_init(|| unsafe { dispatch_queue_create(c"dev.kiri.watch".as_ptr(), std::ptr::null()) } as usize) as *mut c_void
}

impl Stream {
    fn start(dirs: &[String], roots: Vec<Root>, tx: Sender<Vec<(String, u32)>>, since: u64, cur: u64) -> std::io::Result<Stream> {
        // Until an event comes, a restart resumes from where this one began.
        let last = Arc::new(AtomicU64::new(if since == SINCE_NOW { cur } else { since }));
        let info = Box::into_raw(Box::new(Info { tx, roots, warned: AtomicBool::new(false), last: last.clone() }));
        // SAFETY: CoreFoundation objects made and released here. FSEvents
        // owns `info` from FSEventStreamCreate on and frees it through
        // `release_info`; before that it is still ours.
        unsafe {
            let strings: Vec<CFRef> = dirs.iter().map(|d| CFStringCreateWithBytes(std::ptr::null(), d.as_ptr(), d.len() as isize, UTF8, 0)).filter(|s| !s.is_null()).collect();
            let array = CFArrayCreate(std::ptr::null(), strings.as_ptr(), strings.len() as isize, &raw const kCFTypeArrayCallBacks);
            for s in &strings {
                CFRelease(*s);
            }
            let ctx = FSEventStreamContext { version: 0, info: info.cast(), retain: std::ptr::null(), release: release_info as *const c_void, copy_description: std::ptr::null() };
            let stream = FSEventStreamCreate(std::ptr::null(), on_events, &ctx, array, since, LATENCY, CREATE_FILE_EVENTS | CREATE_NO_DEFER);
            CFRelease(array);
            if stream.is_null() {
                drop(Box::from_raw(info));
                return Err(std::io::Error::other("FSEventStreamCreate failed"));
            }
            FSEventStreamSetDispatchQueue(stream, queue());
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                return Err(std::io::Error::other("FSEventStreamStart failed"));
            }
            Ok(Stream(stream, last))
        }
    }
}

extern "C" fn tear_down(stream: *mut c_void) {
    // SAFETY: a started stream, torn down once.
    unsafe {
        FSEventStreamStop(stream);
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // On the stream's own serial queue, so no callback is running while
        // it stops. Never called from that queue (only from kiri-watch).
        // SAFETY: the queue lives for the process; the stream is ours.
        unsafe { dispatch_sync_f(queue(), self.0, tear_down) };
    }
}

#[cfg(test)]
mod tests {
    use super::{Known, Meta, Root, flag, ops, since_for, stale, trimmed, under, SINCE_NOW};

    fn root(path: &str, sensitive: bool, start_id: u64) -> Root {
        Root { path: path.to_owned(), sensitive, start_id }
    }

    fn file(size: u64, mtime: i64, ctime: i64, mode: u32) -> Option<Meta> {
        Some(Meta { dir: false, ino: 1, size, mtime: (mtime, 0), ctime: (ctime, 0), mode, owner: (501, 20) })
    }

    #[test]
    fn what_each_change_means() {
        // Made since watching began: created and written; a folder created.
        assert_eq!(ops(None, file(1, 1, 1, 0o644), true, 0), ['C', 'M']);
        assert_eq!(ops(None, Some(Meta { dir: true, ..file(0, 1, 1, 0o755).unwrap() }), true, 0), ['C']);
        // Written, or replaced by a rename (a new inode).
        assert_eq!(ops(file(1, 1, 1, 0o644), file(2, 2, 2, 0o644), false, 0), ['M']);
        assert_eq!(ops(file(1, 1, 1, 0o644), Some(Meta { ino: 2, ..file(1, 1, 1, 0o644).unwrap() }), false, 0), ['M']);
        // chmod after a write: only the attributes, whatever FSEvents' flags say.
        assert_eq!(ops(file(2, 2, 2, 0o644), file(2, 2, 3, 0o600), false, 0), ['A']);
        // Nothing changed (a stale event): nothing.
        assert!(ops(file(1, 1, 1, 0o644), file(1, 1, 1, 0o644), false, 0).is_empty());
        // Gone.
        assert_eq!(ops(file(1, 1, 1, 0o644), None, false, 0), ['D']);
        assert!(ops(None, None, false, 0).is_empty());
        // A folder's times move with its children: not reported.
        let d = Meta { dir: true, ..file(0, 1, 1, 0o755).unwrap() };
        assert!(ops(Some(d), Some(Meta { mtime: (9, 0), ctime: (9, 0), ..d }), false, 0).is_empty());
        // Here before watching began, first seen now.
        assert_eq!(ops(None, file(1, 1, 1, 0o644), false, 0), ['M']);
    }

    #[test]
    fn known_paths() {
        let dir = std::env::temp_dir().join(format!("kiri-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f").display().to_string();
        let mut k = Known::new();
        std::fs::write(&f, "a").unwrap();
        assert_eq!(k.check(&f, 0), ['C', 'M']);
        assert!(k.check(&f, 0).is_empty());
        std::fs::write(&f, "bb").unwrap();
        assert_eq!(k.check(&f, 0), ['M']);
        std::fs::remove_file(&f).unwrap();
        assert_eq!(k.check(&f, 0), ['D']);
        // A lost-changes rescan finds a file FSEvents never named.
        std::fs::write(&f, "c").unwrap();
        let roots = [root(&dir.display().to_string(), false, 0)];
        assert!(k.rescan(&roots[0].path, &roots).contains(&f));
        // From "/", or a path with a trailing slash, too.
        assert!(k.rescan("/", &roots).contains(&f));
        assert!(k.rescan(&format!("{}/", roots[0].path), &roots).contains(&f));
        assert!(k.rescan(&format!("{}//", roots[0].path), &roots).contains(&f));
        assert_eq!(trimmed("///"), "/");
        assert_eq!(trimmed("/a/b//"), "/a/b");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_paths_by_flags() {
        let was_here = file(1, 1, 1, 0o644);
        let d = Some(Meta { dir: true, ..file(0, 1, 1, 0o755).unwrap() });
        // Gone and never looked at: removed or renamed says deleted.
        assert_eq!(ops(None, None, false, flag::REMOVED), ['D']);
        assert_eq!(ops(None, None, false, flag::RENAMED), ['D']);
        assert!(ops(None, None, false, flag::CREATED).is_empty());
        assert!(ops(None, None, false, 0).is_empty());
        // There and never looked at: created or renamed (the new name of a
        // rename, whose birth is old) is created; a plain write is modified.
        assert_eq!(ops(None, was_here, false, flag::RENAMED), ['C', 'M']);
        assert_eq!(ops(None, was_here, false, flag::CREATED), ['C', 'M']);
        assert_eq!(ops(None, d, false, flag::RENAMED), ['C']);
        assert_eq!(ops(None, was_here, false, 0x1000), ['M']);
    }

    #[test]
    fn deleting_and_renaming_what_was_never_looked_at() {
        let dir = std::env::temp_dir().join(format!("kiri-watch-del-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let (a, b, c) = (dir.join("a").display().to_string(), dir.join("b").display().to_string(), dir.join("sub/c").display().to_string());
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&c, "x").unwrap();
        let mut k = Known::new();
        k.since += std::time::Duration::from_secs(3600); // everything here predates watching
        std::fs::rename(&a, &b).unwrap();
        assert_eq!(k.check(&a, flag::RENAMED), ['D']);
        assert_eq!(k.check(&b, flag::RENAMED), ['C', 'M']);
        // rm -rf: the file and then the folder, both reported removed.
        std::fs::remove_dir_all(dir.join("sub")).unwrap();
        assert_eq!(k.check(&c, flag::REMOVED), ['D']);
        assert_eq!(k.check(&dir.join("sub").display().to_string(), flag::REMOVED), ['D']);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rescan_stays_silent_about_files_it_never_saw() {
        let dir = std::env::temp_dir().join(format!("kiri-watch-rescan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, young) = (dir.join("old").display().to_string(), dir.join("young").display().to_string());
        std::fs::write(&old, "x").unwrap();
        let mut k = Known::new();
        k.since += std::time::Duration::from_secs(3600);
        assert!(k.check(&old, flag::LOST).is_empty());
        // Remembered now, so a real change is seen.
        std::fs::write(&old, "yy").unwrap();
        assert_eq!(k.check(&old, 0), ['M']);
        // Born since watching began: created even in a rescan.
        k.since -= std::time::Duration::from_secs(7200);
        std::fs::write(&young, "x").unwrap();
        assert_eq!(k.check(&young, flag::LOST), ['C', 'M']);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_stream_starts_where_the_old_one_stopped() {
        assert_eq!(since_for(0), SINCE_NOW);
        assert_eq!(since_for(12345), 12345);
    }

    #[test]
    fn only_inside_bind_sources() {
        let roots = [root("/Users/a/app", false, 0), root("/Users/a/.env", false, 0)];
        assert_eq!(under("/Users/a/app/src/main.js", &roots).as_deref(), Some("/Users/a/app/src/main.js"));
        assert_eq!(under("/Users/a/.env", &roots).as_deref(), Some("/Users/a/.env"));
        assert_eq!(under("/users/A/App/src/main.js", &roots).as_deref(), Some("/Users/a/app/src/main.js"));
        assert_eq!(under("/Users/a/application/x", &roots), None);
        assert_eq!(under("/Users/a/.envrc", &roots), None);
    }

    #[test]
    fn case_is_kept_on_case_sensitive_volumes() {
        let roots = [root("/Volumes/cs/app", true, 0)];
        assert_eq!(under("/Volumes/cs/app/x", &roots).as_deref(), Some("/Volumes/cs/app/x"));
        assert_eq!(under("/Volumes/cs/App/x", &roots), None);
        assert_eq!(under("/volumes/cs/app", &roots), None);
        let roots = [root("/Volumes/cs/app", false, 0)];
        assert_eq!(under("/Volumes/cs/App/x", &roots).as_deref(), Some("/Volumes/cs/app/x"));
    }

    #[test]
    fn events_from_before_a_source_was_added_are_dropped() {
        let roots = [root("/a", false, 0), root("/b", false, 100)];
        // The old source loses nothing; the new one nothing at or before 100.
        assert!(!stale("/a/x", 5, flag::CREATED, &roots));
        assert!(stale("/b/x", 100, flag::CREATED, &roots));
        assert!(stale("/b", 50, flag::CREATED, &roots));
        assert!(!stale("/b/x", 101, flag::CREATED, &roots));
        // History done, whatever its path or id.
        assert!(stale("/a/x", 500, flag::HISTORY_DONE, &roots));
        // Root changed and mounts have no id and always count.
        assert!(!stale("/b", 0, flag::ROOT_CHANGED, &roots));
        assert!(!stale("/b", 0, flag::MOUNT, &roots));
        assert!(!stale("/b", 0, flag::UNMOUNT, &roots));
        // A folder above a source (changes lost) is not under one.
        assert!(!stale("/", 1, flag::MUST_SCAN_SUB_DIRS, &roots));
    }

    #[test]
    fn a_new_parent_source_does_not_hide_its_watched_childs_changes() {
        // /u/app was watched from id 5; a container then binds /u (from 100).
        let roots = [root("/u", false, 100), root("/u/app", false, 5)];
        assert!(!stale("/u/app/x", 50, flag::CREATED, &roots));
        assert!(stale("/u/other", 50, flag::CREATED, &roots));
        assert!(stale("/u/app/x", 5, flag::CREATED, &roots));
    }

    #[test]
    fn a_new_source_counts_only_files_made_after_it_was_added() {
        let dir = std::env::temp_dir().join(format!("kiri-watch-new-{}", std::process::id()));
        let (old_root, new_root) = (dir.join("old"), dir.join("new"));
        std::fs::create_dir_all(&old_root).unwrap();
        std::fs::create_dir_all(&new_root).unwrap();
        let (fo, fnw) = (old_root.join("f").display().to_string(), new_root.join("f").display().to_string());
        std::fs::write(&fo, "x").unwrap();
        std::fs::write(&fnw, "x").unwrap();
        let mut k = Known::new();
        k.sync(&[root(&old_root.display().to_string(), false, 0)]);
        // The second source is added an hour from now, by the clock of its start.
        k.sync(&[root(&old_root.display().to_string(), false, 0), root(&new_root.display().to_string(), false, 9)]);
        for (_, t) in k.starts.iter_mut().filter(|(p, _)| p.ends_with("new")) {
            *t += std::time::Duration::from_secs(3600);
        }
        // Both files were made a moment ago: new in the first source, not in the second.
        assert!(k.since_of(&fo) < k.since_of(&fnw));
        assert_eq!(k.check(&fo, 0), ['C', 'M']);
        assert_eq!(k.check(&fnw, 0), ['M']);
        // Dropping a source forgets its paths and start.
        k.sync(&[root(&old_root.display().to_string(), false, 0)]);
        assert!(!k.paths.contains_key(&fnw) && k.paths.contains_key(&fo));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
