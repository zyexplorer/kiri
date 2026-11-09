//! Who may use the engine's socket on a Linux host, without signing in again.
//!
//! The socket is 0660 root:kiri, so members of the `kiri` group may use it, as
//! the docker group may use dockerd's. But a group someone was just added to
//! only applies to sessions that start after: until they log out and back in,
//! the window and every terminal they have open are refused. So each member
//! also gets their own entry in the socket's access list (a POSIX ACL), which
//! applies at once, to every session they have.
//!
//! The daemon writes the list when it makes the socket, and `kiri host-access`
//! (run as root, from the window's onboarding through pkexec) writes it again
//! after adding someone.
//!
//! An entry in the list outlives the membership it was written for, so the
//! daemon doesn't rely on it alone: a `Gate` checks every new connection
//! against the group, and every few seconds checks the people still connected,
//! closing the connections of anyone removed (streams and `exec` sessions
//! too) and writing the list again without them. Removal applies as soon as
//! the system's user database says so; where that is cached (nscd, sssd), it
//! is when the cache catches up.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::raw::{c_char, c_int};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the people still connected are checked again.
const RECHECK: Duration = Duration::from_secs(2);

/// An account or group record and the buffer its strings live in.
struct Rec<T> {
    rec: T,
    _buf: Vec<c_char>,
}

/// Runs a reentrant NSS lookup (getpwuid_r and the like), growing the buffer
/// until the record fits. `Ok(None)`: there is no such account or group.
///
/// # Safety
/// `T` must be `libc::passwd` or `libc::group`, and `call` the matching `_r` lookup.
unsafe fn nss<T>(
    mut call: impl FnMut(*mut T, *mut c_char, usize, *mut *mut T) -> c_int,
) -> io::Result<Option<Rec<T>>> {
    let mut size = 1024;
    loop {
        let mut buf: Vec<c_char> = vec![0; size];
        // SAFETY: passwd and group are plain data; all-zero is a valid value.
        let mut rec: T = unsafe { std::mem::zeroed() };
        let mut out: *mut T = std::ptr::null_mut();
        match call(&mut rec, buf.as_mut_ptr(), buf.len(), &mut out) {
            0 if out.is_null() => return Ok(None),
            // The record's strings point into `buf`, whose heap storage moves with it.
            0 => return Ok(Some(Rec { rec, _buf: buf })),
            // Groups of many thousands of names run to megabytes.
            libc::ERANGE if size < 1 << 28 => size *= 2,
            libc::ENOENT | libc::ESRCH => return Ok(None),
            e => return Err(io::Error::from_raw_os_error(e)),
        }
    }
}

fn account_by_uid(uid: u32) -> io::Result<Option<Rec<libc::passwd>>> {
    // SAFETY: getpwuid_r with a passwd record and a buffer of the stated size.
    unsafe { nss(|rec, buf, len, out| libc::getpwuid_r(uid, rec, buf, len, out)) }
}

fn account_by_name(name: &CStr) -> io::Result<Option<Rec<libc::passwd>>> {
    // SAFETY: getpwnam_r with a valid C string, a passwd record and a buffer of the stated size.
    unsafe { nss(|rec, buf, len, out| libc::getpwnam_r(name.as_ptr(), rec, buf, len, out)) }
}

fn group_by_name(name: &CStr) -> io::Result<Rec<libc::group>> {
    // SAFETY: getgrnam_r with a valid C string, a group record and a buffer of the stated size.
    unsafe { nss(|rec, buf, len, out| libc::getgrnam_r(name.as_ptr(), rec, buf, len, out)) }?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no group {}", name.to_string_lossy()),
            )
        })
}

fn c_name(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// The account names listed in a group record.
fn names(group: &libc::group) -> Vec<&CStr> {
    let mut out = Vec::new();
    let mut m = group.gr_mem;
    // SAFETY: gr_mem is a NULL-terminated array of account names for this group.
    unsafe {
        while !m.is_null() && !(*m).is_null() {
            out.push(CStr::from_ptr(*m));
            m = m.add(1);
        }
    }
    out
}

fn group_members(group: &CStr) -> io::Result<Vec<String>> {
    let g = group_by_name(group)?;
    Ok(names(&g.rec)
        .iter()
        .map(|n| n.to_string_lossy().into_owned())
        .collect())
}

/// The users in `group` by name (its member list, not primary-group users).
pub fn members(group: &str) -> io::Result<Vec<String>> {
    group_members(&c_name(group)?)
}

/// A user's numeric id, if the account exists.
pub fn uid(user: &str) -> Option<u32> {
    let a = account_by_name(&c_name(user).ok()?).ok()??;
    Some(a.rec.pw_uid)
}

/// The uids of the names in a group record, sorted. A name with no account
/// is left out; a lookup that fails fails the whole list, so a slow directory
/// never shrinks it.
fn uids_of(group: &libc::group) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    for name in names(group) {
        if let Some(a) = account_by_name(name)? {
            out.push(a.rec.pw_uid);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn member_uids(group: &CStr) -> io::Result<Vec<u32>> {
    uids_of(&group_by_name(group)?.rec)
}

/// Gives every member of `group` read-write access to `socket` by name, so a
/// member just added can use it without signing in again. Returns how many.
pub fn grant(socket: &Path, group: &str) -> io::Result<usize> {
    let uids = member_uids(&c_name(group)?)?;
    set(socket, &uids)?;
    Ok(uids.len())
}

/// The uid at the other end of an accepted connection.
fn peer_uid(conn: &UnixStream) -> io::Result<u32> {
    // SAFETY: a connected socket and zeroed storage for its credentials.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the output pointers are valid and the buffer is the stated size.
    let r = unsafe {
        libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if r != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(if r != 0 {
            io::Error::last_os_error()
        } else {
            io::Error::new(io::ErrorKind::InvalidData, "short peer credentials")
        });
    }
    Ok(cred.uid)
}

/// Whether the account is in the group by name or as its primary group.
fn group_member(user: &CStr, primary_gid: libc::gid_t, group: &libc::group) -> bool {
    primary_gid == group.gr_gid || names(group).contains(&user)
}

/// The group's record, or `None` if the group was deleted: that leaves
/// nobody in it, which is an answer, not a failure.
fn current(group: &CStr) -> io::Result<Option<Rec<libc::group>>> {
    match group_by_name(group) {
        Ok(g) => Ok(Some(g)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether the account `uid` is in the group now. No account, or no group: no.
fn member_of(uid: u32, group: Option<&libc::group>) -> io::Result<bool> {
    let Some(group) = group else { return Ok(false) };
    let Some(a) = account_by_uid(uid)? else {
        return Ok(false);
    };
    if a.rec.pw_name.is_null() {
        return Ok(false);
    }
    // SAFETY: pw_name is a NUL-terminated string in the record's buffer.
    let user = unsafe { CStr::from_ptr(a.rec.pw_name) };
    Ok(group_member(user, a.rec.pw_gid, group))
}

fn member(uid: u32, group: &CStr) -> io::Result<bool> {
    member_of(uid, current(group)?.as_ref().map(|g| &g.rec))
}

/// Who may use the engine's socket: root, the account the daemon runs as, and
/// the members of one group, checked when they connect and again while they
/// stay connected.
pub struct Gate {
    group: CString,
    socket: PathBuf,
    owner: u32,
    open: Mutex<HashMap<u64, (u32, UnixStream)>>,
    next: AtomicU64,
    failing: AtomicBool,
    /// Recent answers by uid, so the docker CLI's burst of connections per
    /// command costs one lookup. They last no longer than the recheck does.
    recent: Mutex<HashMap<u32, (bool, Instant)>>,
}

/// A connection the gate let in; dropping it stops watching that connection.
pub struct Seat {
    gate: Arc<Gate>,
    id: u64,
}

impl Drop for Seat {
    fn drop(&mut self) {
        self.gate
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

impl Gate {
    pub fn new(socket: &Path, group: &OsStr) -> io::Result<Arc<Gate>> {
        let group = CString::new(group.as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: geteuid cannot fail.
        let owner = unsafe { libc::geteuid() };
        Ok(Arc::new(Gate {
            group,
            socket: socket.to_owned(),
            owner,
            open: Mutex::default(),
            next: AtomicU64::new(0),
            failing: AtomicBool::new(false),
            recent: Mutex::default(),
        }))
    }

    fn name(&self) -> std::borrow::Cow<'_, str> {
        self.group.to_string_lossy()
    }

    fn allowed(&self, uid: u32) -> io::Result<bool> {
        if uid == 0 || uid == self.owner {
            return Ok(true);
        }
        if let Some((yes, at)) = self
            .recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&uid)
            && at.elapsed() < RECHECK
        {
            return Ok(*yes);
        }
        let yes = member(uid, &self.group)?;
        self.remember(uid, yes);
        Ok(yes)
    }

    fn remember(&self, uid: u32, yes: bool) {
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        recent.retain(|_, (_, at)| at.elapsed() < RECHECK);
        recent.insert(uid, (yes, Instant::now()));
    }

    /// One recheck: everyone connected against one read of the group, then
    /// the access list, if the group's names changed since it was written.
    /// `listed`: the names the list was last written for.
    fn tick(&self, listed: &mut Option<Vec<CString>>) {
        let group = current(&self.group);
        let connected = !self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        if connected {
            self.sweep_with(|uid| {
                if uid == 0 || uid == self.owner {
                    return Ok(true);
                }
                let g = group
                    .as_ref()
                    .map_err(|e| io::Error::new(e.kind(), e.to_string()))?;
                let yes = member_of(uid, g.as_ref().map(|g| &g.rec))?;
                self.remember(uid, yes);
                Ok(yes)
            });
        }
        let Ok(group) = group else { return };
        let now: Vec<CString> = group
            .as_ref()
            .map(|g| names(&g.rec).into_iter().map(CStr::to_owned).collect())
            .unwrap_or_default();
        if listed.as_ref() == Some(&now) {
            return;
        }
        // Only a list actually written counts: anything else is tried again.
        let written = group
            .as_ref()
            .map_or(Ok(Vec::new()), |g| uids_of(&g.rec))
            .and_then(|uids| set(&self.socket, &uids).map(|()| uids.len()));
        match written {
            Ok(n) => {
                crate::diag::info!("socket access list written", members = n);
                *listed = Some(now);
            }
            Err(e) if unsupported(&e) => *listed = Some(now),
            Err(e) => crate::diag::warn!("socket access list not written", error = e),
        }
    }

    /// Writes the socket's access list for the group's current members.
    pub fn grant(&self) -> io::Result<usize> {
        let uids = member_uids(&self.group)?;
        set(&self.socket, &uids)?;
        Ok(uids.len())
    }

    /// Checks a new connection. `None`: refused, and the reason logged.
    pub fn admit(self: &Arc<Self>, conn: &UnixStream) -> Option<Seat> {
        self.admit_with(conn, |uid| self.allowed(uid))
    }

    fn admit_with(
        self: &Arc<Self>,
        conn: &UnixStream,
        check: impl Fn(u32) -> io::Result<bool>,
    ) -> Option<Seat> {
        let checked = peer_uid(conn).and_then(|uid| Ok((uid, check(uid)?)));
        let uid = match checked {
            Ok((uid, true)) => uid,
            Ok((uid, false)) => {
                crate::diag::warn!(
                    "socket connection refused: user is not in the access group",
                    group = self.name(),
                    uid = uid
                );
                return None;
            }
            Err(e) => {
                crate::diag::warn!(
                    "socket connection refused: access check failed",
                    group = self.name(),
                    error = e
                );
                return None;
            }
        };
        let keep = match conn.try_clone() {
            Ok(c) => c,
            Err(e) => {
                crate::diag::warn!("socket connection refused: cannot watch it", error = e);
                return None;
            }
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (uid, keep));
        Some(Seat {
            gate: self.clone(),
            id,
        })
    }

    /// Checks everyone still connected, once per account, and closes the
    /// connections of anyone no longer allowed. A failed lookup leaves the
    /// connections open (new ones are refused until lookups work again).
    fn sweep_with(&self, check: impl Fn(u32) -> io::Result<bool>) -> usize {
        let uids: HashSet<u32> = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|(u, _)| *u)
            .collect();
        let mut removed = HashSet::new();
        let mut failed = None;
        for uid in uids {
            match check(uid) {
                Ok(true) => {}
                Ok(false) => {
                    removed.insert(uid);
                }
                Err(e) => failed = Some(e),
            }
        }
        match failed {
            Some(e) if !self.failing.swap(true, Ordering::Relaxed) => crate::diag::warn!(
                "access check failed; open connections kept until it works again",
                group = self.name(),
                error = e
            ),
            None if self.failing.swap(false, Ordering::Relaxed) => {
                crate::diag::info!("access check works again", group = self.name())
            }
            _ => {}
        }
        if removed.is_empty() {
            return 0;
        }
        let mut closed = 0;
        // Taken out as they're closed, so a thread that takes a while to
        // notice isn't closed and logged again on every recheck.
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (uid, conn)| {
                if !removed.contains(uid) {
                    return true;
                }
                // Ends the connection for every thread using it: reads see the
                // end, writes fail, as if the client had hung up.
                let _ = conn.shutdown(Shutdown::Both);
                closed += 1;
                false
            });
        for uid in &removed {
            crate::diag::warn!(
                "connections closed: user is no longer in the access group",
                group = self.name(),
                uid = uid
            );
        }
        closed
    }

    /// Checks everyone connected every few seconds, and writes the access
    /// list again whenever the group's members change.
    pub fn watch(self: Arc<Self>) -> io::Result<()> {
        std::thread::Builder::new()
            .name("kiri-access".into())
            .spawn(move || {
                // Unknown at first: the first recheck writes the list once more,
                // so a change between startup's write and now isn't missed.
                let mut listed = None;
                loop {
                    std::thread::sleep(RECHECK);
                    self.tick(&mut listed);
                }
            })?;
        Ok(())
    }
}

/// Writes the socket's access list: its owner and group (rw, as the mode
/// says), each of `uids` (rw), nobody else.
pub fn set(socket: &Path, uids: &[u32]) -> io::Result<()> {
    let blob = acl(uids);
    let path = CString::new(socket.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: both strings are valid and NUL-terminated; blob outlives the call.
    let r = unsafe {
        libc::setxattr(
            path.as_ptr(),
            c"system.posix_acl_access".as_ptr(),
            blob.as_ptr().cast(),
            blob.len(),
            0,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether a failure to write the list means this system can't keep one
/// (a filesystem without ACLs): then only signing in again lets a new member in.
pub fn unsupported(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EOPNOTSUPP)
}

/// The kernel's form of an access list (`system.posix_acl_access`): a version,
/// then entries of tag, permissions and id, in tag order and, within a tag,
/// in id order.
fn acl(uids: &[u32]) -> Vec<u8> {
    const USER_OBJ: u16 = 0x01;
    const USER: u16 = 0x02;
    const GROUP_OBJ: u16 = 0x04;
    const MASK: u16 = 0x10;
    const OTHER: u16 = 0x20;
    const NO_ID: u32 = u32::MAX;
    const RW: u16 = 6;
    fn entry(out: &mut Vec<u8>, tag: u16, perm: u16, id: u32) {
        out.extend(tag.to_le_bytes());
        out.extend(perm.to_le_bytes());
        out.extend(id.to_le_bytes());
    }
    let mut ids: Vec<u32> = uids.iter().copied().filter(|&u| u != 0).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut out = 2u32.to_le_bytes().to_vec();
    entry(&mut out, USER_OBJ, RW, NO_ID);
    for id in ids {
        entry(&mut out, USER, RW, id);
    }
    entry(&mut out, GROUP_OBJ, RW, NO_ID);
    entry(&mut out, MASK, RW, NO_ID);
    entry(&mut out, OTHER, 0, NO_ID);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acl_lists_each_user_once_in_order_and_not_root() {
        let b = acl(&[1001, 0, 1000, 1001]);
        assert_eq!(&b[..4], &[2, 0, 0, 0]);
        let entries: Vec<(u16, u16, u32)> = b[4..]
            .chunks(8)
            .map(|c| {
                (
                    u16::from_le_bytes([c[0], c[1]]),
                    u16::from_le_bytes([c[2], c[3]]),
                    u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                (1, 6, u32::MAX),
                (2, 6, 1000),
                (2, 6, 1001),
                (4, 6, u32::MAX),
                (0x10, 6, u32::MAX),
                (0x20, 0, u32::MAX)
            ]
        );
    }

    #[test]
    fn membership_includes_names_and_primary_group() {
        let alice = c"alice";
        let mut names = [alice.as_ptr().cast_mut(), std::ptr::null_mut()];
        let group = libc::group {
            gr_name: c"kiri".as_ptr().cast_mut(),
            gr_passwd: c"x".as_ptr().cast_mut(),
            gr_gid: 100,
            gr_mem: names.as_mut_ptr(),
        };
        assert!(group_member(alice, 1, &group));
        assert!(group_member(c"bob", 100, &group));
        assert!(!group_member(c"bob", 1, &group));
    }

    #[test]
    fn accepted_connection_reports_its_peer_uid() {
        let (a, _b) = UnixStream::pair().unwrap();
        // SAFETY: getuid cannot fail.
        assert_eq!(peer_uid(&a).unwrap(), unsafe { libc::getuid() });
    }

    /// The real lookup, without the root and owner shortcuts. Skipped where
    /// the test's uid has no account (some CI containers).
    #[test]
    fn current_users_primary_group_counts() {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let Some(a) = account_by_uid(uid).unwrap() else {
            return;
        };
        let g = {
            // SAFETY: getgrgid_r with a group record and a buffer of the stated size.
            let gid = a.rec.pw_gid;
            unsafe { nss(|rec, buf, len, out| libc::getgrgid_r(gid, rec, buf, len, out)) }.unwrap()
        };
        let Some(g) = g else { return };
        // SAFETY: gr_name is a NUL-terminated string in the record's buffer.
        let name = unsafe { CStr::from_ptr(g.rec.gr_name) }.to_owned();
        assert!(member(uid, &name).unwrap());
        assert!(!member(u32::MAX - 7, &name).unwrap(), "no such account");
        assert!(
            !member(uid, c"kiri-no-such-group-x").unwrap(),
            "a deleted group has nobody in it"
        );
    }

    #[test]
    fn lookups_from_many_threads_agree() {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let Some(a) = account_by_uid(uid).unwrap() else {
            return;
        };
        // SAFETY: pw_name is a NUL-terminated string in the record's buffer.
        let me = unsafe { CStr::from_ptr(a.rec.pw_name) }.to_owned();
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..200 {
                        let b = account_by_uid(uid).unwrap().unwrap();
                        // SAFETY: as above.
                        assert_eq!(unsafe { CStr::from_ptr(b.rec.pw_name) }, me.as_c_str());
                        assert!(account_by_uid(u32::MAX - 7).unwrap().is_none());
                    }
                });
            }
        });
    }

    fn gate() -> Arc<Gate> {
        let mut g = Gate::new(Path::new("/nonexistent"), OsStr::new("kiri")).unwrap();
        Arc::get_mut(&mut g).unwrap().owner = u32::MAX;
        g
    }

    #[test]
    fn root_and_the_daemons_own_account_need_no_group() {
        let g = gate();
        assert!(g.allowed(0).unwrap());
        let mut g2 = Gate::new(
            Path::new("/nonexistent"),
            OsStr::new("kiri-no-such-group-x"),
        )
        .unwrap();
        Arc::get_mut(&mut g2).unwrap().owner = 4242;
        assert!(g2.allowed(4242).unwrap());
        assert!(!g2.allowed(4243).unwrap(), "anyone else is looked up");
    }

    #[test]
    fn refused_or_failed_checks_keep_the_connection_out() {
        let g = gate();
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(g.admit_with(&a, |_| Ok(false)).is_none());
        assert!(
            g.admit_with(&a, |_| Err(io::Error::other("nss down")))
                .is_none()
        );
        assert!(g.open.lock().unwrap().is_empty());
        let seat = g.admit_with(&a, |_| Ok(true)).unwrap();
        assert_eq!(g.open.lock().unwrap().len(), 1);
        drop(seat);
        assert!(
            g.open.lock().unwrap().is_empty(),
            "a closed connection isn't watched"
        );
    }

    #[test]
    fn removal_closes_an_open_connection_mid_stream() {
        use std::io::{Read, Write};
        let g = gate();
        let (a, mut b) = UnixStream::pair().unwrap();
        let _seat = g.admit_with(&a, |_| Ok(true)).unwrap();
        // A thread blocked reading the client, as an exec session's stdin is.
        let mut reader = a.try_clone().unwrap();
        let blocked = std::thread::spawn(move || reader.read(&mut [0; 16]).unwrap());
        b.write_all(b"x").unwrap();
        // Still a member: nothing happens.
        assert_eq!(g.sweep_with(|_| Ok(true)), 0);
        // A lookup failure keeps it open too.
        assert_eq!(g.sweep_with(|_| Err(io::Error::other("nss down"))), 0);
        assert_eq!(blocked.join().unwrap(), 1);
        let mut reader = a.try_clone().unwrap();
        let blocked = std::thread::spawn(move || reader.read(&mut [0; 16]).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        // Removed: the blocked read sees the end, and the client does too.
        assert_eq!(g.sweep_with(|_| Ok(false)), 1);
        assert_eq!(
            g.sweep_with(|_| Ok(false)),
            0,
            "closed once, not again on every recheck"
        );
        assert_eq!(blocked.join().unwrap(), 0);
        assert_eq!(b.read(&mut [0; 16]).unwrap(), 0);
        assert!((&a).write_all(b"y").is_err());
    }

    /// The kernel takes the list and lets a user who isn't in the socket's
    /// group in by it, at once. Needs root (cargo test -- --ignored, as root).
    #[test]
    #[ignore]
    fn a_listed_user_gets_in_and_others_dont() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Path::new("/dev/shm").join(format!("kiri-acl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sock = dir.join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660)).unwrap();
        // As uid `as_uid` (no groups): can it connect?
        let try_as = |as_uid: u32| {
            // SAFETY: fork in a test; the child only drops ids, connects and exits.
            unsafe {
                let pid = libc::fork();
                if pid == 0 {
                    libc::setgroups(0, std::ptr::null());
                    libc::setgid(as_uid);
                    libc::setuid(as_uid);
                    libc::_exit(if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                        0
                    } else {
                        1
                    });
                }
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0);
                libc::WEXITSTATUS(status) == 0
            }
        };
        assert!(!try_as(65534), "nobody isn't let in before the list");
        set(&sock, &[65534]).unwrap();
        assert!(try_as(65534), "a listed user gets in");
        assert!(!try_as(65533), "an unlisted one doesn't");
        set(&sock, &[]).unwrap();
        assert!(!try_as(65534), "removing the entry takes access away");

        // An old ACL entry is not enough: the daemon's group check still says no.
        set(&sock, &[65534]).unwrap();
        assert!(try_as(65534));
        let (conn, _) = listener.accept().unwrap();
        let mut gate = Gate::new(&sock, OsStr::new("root")).unwrap();
        Arc::get_mut(&mut gate).unwrap().owner = u32::MAX;
        assert!(gate.admit(&conn).is_none(), "nobody is not in root's group");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A real account, added to a real group and taken out again while
    /// connected: its open connection is closed and the list rewritten.
    /// Needs root and useradd (cargo test -- --ignored, as root, in a throwaway system).
    #[test]
    #[ignore]
    fn removing_a_real_member_closes_their_connection() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let run = |args: &[&str]| {
            assert!(
                Command::new(args[0])
                    .args(&args[1..])
                    .status()
                    .unwrap()
                    .success(),
                "{args:?}"
            )
        };
        let _ = Command::new("userdel").arg("kiritest").status();
        let _ = Command::new("groupdel").arg("kiritestg").status();
        run(&["groupadd", "kiritestg"]);
        run(&["useradd", "-M", "-G", "kiritestg", "kiritest"]);
        let who = uid("kiritest").unwrap();
        assert!(
            members("kiritestg")
                .unwrap()
                .contains(&"kiritest".to_owned())
        );

        let dir = Path::new("/dev/shm").join(format!("kiri-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sock = dir.join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660)).unwrap();
        let mut gate = Gate::new(&sock, OsStr::new("kiritestg")).unwrap();
        Arc::get_mut(&mut gate).unwrap().owner = u32::MAX;
        assert_eq!(gate.grant().unwrap(), 1);

        // The member connects (by the list: they have no group in this
        // process) and waits for an answer that never comes.
        // SAFETY: fork in a test; the child only drops ids, connects, reads and exits.
        let pid = unsafe {
            let pid = libc::fork();
            if pid == 0 {
                // A gid that isn't the test group, so only the list can let it in.
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setgid(65534) != 0
                    || libc::setuid(who) != 0
                {
                    libc::_exit(3);
                }
                let code = match std::os::unix::net::UnixStream::connect(&sock) {
                    Ok(mut c) => match c.read(&mut [0; 1]) {
                        Ok(0) => 0,
                        _ => 2,
                    },
                    Err(_) => 1,
                };
                libc::_exit(code);
            }
            pid
        };
        let (conn, _) = listener.accept().unwrap();
        let _seat = gate.admit(&conn).expect("a member is let in");
        let mut listed = None;
        gate.tick(&mut listed);
        assert_eq!(gate.open.lock().unwrap().len(), 1, "still a member");

        run(&["gpasswd", "-d", "kiritest", "kiritestg"]);
        gate.tick(&mut listed);
        assert!(
            gate.open.lock().unwrap().is_empty(),
            "removed: their connection is closed"
        );
        assert_eq!(listed, Some(vec![]), "and the list is written without them");
        let mut status = 0;
        // SAFETY: waits for the child forked above.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "the client saw the connection end"
        );
        assert_eq!(
            gate.grant().unwrap(),
            0,
            "and the list no longer names them"
        );

        let _ = Command::new("userdel").arg("kiritest").status();
        let _ = Command::new("groupdel").arg("kiritestg").status();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn acl_without_users_is_the_plain_mode() {
        assert_eq!(acl(&[]).len(), 4 + 4 * 8);
    }
}
