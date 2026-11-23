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
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How often the people still connected are checked again.
const RECHECK: Duration = Duration::from_secs(2);
/// How long a single `getent` call may run before it is killed.
const GETENT_TIMEOUT: Duration = Duration::from_secs(3);
/// Maximum stdout accepted from a single `getent` call.
const GETENT_MAX_STDOUT: usize = 4 * 1024 * 1024;
/// Maximum distinct group members resolved for the socket's POSIX ACL.
/// ext4 fits ~507 entries in a 4 KiB xattr block; larger groups fall back to
/// the socket's 0660 group permissions.
const MAX_ACL_MEMBERS: usize = 512;
/// Number of directory account names queried per `getent passwd` call.
const GETENT_PASSWD_BATCH: usize = 64;
/// Short cache window for group lookups on the accept path, invalidated
/// immediately whenever `/etc/group` changes.
const GROUP_CACHE_TTL: Duration = Duration::from_secs(1);
/// Idle refresh cadence when directory or unresolved members are present.
const IDLE_DIRECTORY_REFRESH: Duration = Duration::from_secs(6);
/// Idle refresh cadence when all members are local and `/etc/group` is unchanged.
const IDLE_LOCAL_NSS_REFRESH: Duration = Duration::from_secs(30);
/// Consecutive ambiguous exit-2 misses before shrinking an existing socket ACL
/// or closing an active directory connection.
const DIRECTORY_MISS_GRACE: u32 = 3;
/// Maximum cached per-uid decisions on the accept path.
const MAX_RECENT_ENTRIES: usize = 1024;
/// Maximum concurrent access checks on the accept path.
const MAX_CONCURRENT_LOOKUPS: usize = 4;
/// Maximum threads waiting for a lookup slot before failing fast.
const MAX_WAITING_LOOKUPS: usize = 16;

const ETC_GROUP: &str = "/etc/group";

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
unsafe fn nss<T>(mut call: impl FnMut(*mut T, *mut c_char, usize, *mut *mut T) -> c_int) -> io::Result<Option<Rec<T>>> {
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

/// Trusted locations for `getent`: never searched through `PATH` because this
/// code runs as root in the daemon and in `kiri host-access`.
const GETENT_PATHS: &[&str] = &["/usr/bin/getent", "/bin/getent", "/run/current-system/sw/bin/getent"];

fn find_getent() -> Option<&'static Path> {
    GETENT_PATHS.iter().map(Path::new).find(|p| p.is_file())
}

/// An account: its name, uid and primary group.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Account {
    name: CString,
    uid: u32,
    gid: u32,
}

impl Account {
    fn from_rec(a: &Rec<libc::passwd>) -> Option<Account> {
        if a.rec.pw_name.is_null() {
            return None;
        }
        // SAFETY: pw_name is a NUL-terminated string in the record's buffer.
        let name = unsafe { CStr::from_ptr(a.rec.pw_name) }.to_owned();
        Some(Account { name, uid: a.rec.pw_uid, gid: a.rec.pw_gid })
    }
}

/// A group: its name, gid and explicit member names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    name: CString,
    gid: u32,
    members: Vec<CString>,
}

impl Group {
    fn from_rec(g: &Rec<libc::group>) -> Option<Group> {
        if g.rec.gr_name.is_null() {
            return None;
        }
        // SAFETY: gr_name and gr_mem strings are NUL-terminated in the record's buffer.
        let name = unsafe { CStr::from_ptr(g.rec.gr_name) }.to_owned();
        let mut members = Vec::new();
        let mut m = g.rec.gr_mem;
        // SAFETY: gr_mem is a NULL-terminated array of account names for this group.
        unsafe {
            while !m.is_null() && !(*m).is_null() {
                members.push(CStr::from_ptr(*m).to_owned());
                m = m.add(1);
            }
        }
        Some(Group { name, gid: g.rec.gr_gid, members })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LookupResult<T> {
    /// Entry found.
    Found(T),
    /// Definitely absent (no `getent` binary and local miss, or invalid/mismatched key).
    NotFound,
    /// `getent` returned exit 2: either absent or NSS directory temporarily unavailable.
    UnavailableOrNotFound,
}

fn local_account_by_name(name: &CStr) -> io::Result<Option<Account>> {
    // SAFETY: getpwnam_r with a valid C string, a passwd record and a buffer of the stated size.
    Ok(unsafe { nss(|rec, buf, len, out| libc::getpwnam_r(name.as_ptr(), rec, buf, len, out)) }?.and_then(|a| Account::from_rec(&a)))
}

fn local_group_by_name(name: &CStr) -> io::Result<Option<Group>> {
    // SAFETY: getgrnam_r with a valid C string, a group record and a buffer of the stated size.
    Ok(unsafe { nss(|rec, buf, len, out| libc::getgrnam_r(name.as_ptr(), rec, buf, len, out)) }?.and_then(|g| Group::from_rec(&g)))
}

fn local_account_by_uid(uid: u32) -> io::Result<Option<Account>> {
    // SAFETY: getpwuid_r with a passwd record and a buffer of the stated size.
    Ok(unsafe { nss(|rec, buf, len, out| libc::getpwuid_r(uid, rec, buf, len, out)) }?.and_then(|a| Account::from_rec(&a)))
}

fn account_by_uid_detailed(uid: u32) -> io::Result<LookupResult<Account>> {
    if let Some(a) = local_account_by_uid(uid)? {
        return Ok(LookupResult::Found(a));
    }
    let Some(program) = find_getent() else { return Ok(LookupResult::NotFound) };
    let key = uid.to_string();
    match getent_account_lookup_with(program, key.as_bytes(), GETENT_TIMEOUT, GETENT_MAX_STDOUT)? {
        LookupResult::Found(a) if a.uid == uid => Ok(LookupResult::Found(a)),
        LookupResult::Found(_) | LookupResult::NotFound => Ok(LookupResult::NotFound),
        LookupResult::UnavailableOrNotFound => Ok(LookupResult::UnavailableOrNotFound),
    }
}

#[cfg(test)]
fn account_by_uid(uid: u32) -> io::Result<Option<Account>> {
    match account_by_uid_detailed(uid)? {
        LookupResult::Found(a) => Ok(Some(a)),
        LookupResult::NotFound | LookupResult::UnavailableOrNotFound => Ok(None),
    }
}

fn account_by_name(name: &CStr) -> io::Result<Option<Account>> {
    match local_account_by_name(name)? {
        Some(a) => Ok(Some(a)),
        None => Ok(getent_account(name.to_bytes())?.and_then(|a| match_account_name(name.to_bytes(), a))),
    }
}

fn valid_key(key: &[u8]) -> bool {
    !key.is_empty() && key[0] != b'-' && !key.iter().any(|&b| matches!(b, b':' | b'\n' | b'\r' | 0))
}

/// Whether `strtoul(key, &end, 10)` in the C locale would consume all of `key`
/// as a numeric id rather than a name (`"0"`, `"+0"`, `" 0"`, `" -0"`, etc.).
fn is_numeric_key(key: &[u8]) -> bool {
    let Some(start) = key.iter().position(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)) else {
        return false;
    };
    let rest = &key[start..];
    let digits = rest.strip_prefix(b"+").or_else(|| rest.strip_prefix(b"-")).unwrap_or(rest);
    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
}

fn match_account_name(query: &[u8], account: Account) -> Option<Account> {
    if is_numeric_key(query) && account.name.as_bytes() != query {
        return None;
    }
    Some(account)
}

fn match_group_name(query: &[u8], group: Group) -> Option<Group> {
    if is_numeric_key(query) && group.name.as_bytes() != query {
        return None;
    }
    Some(group)
}

fn single_line(text: &[u8]) -> Option<&[u8]> {
    let line = text.strip_suffix(b"\n").unwrap_or(text);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() || line.contains(&b'\n') || line.contains(&b'\r') {
        return None;
    }
    Some(line)
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Parses a GID token from `getent initgroups`: glibc formats `gid_t` with
/// signed `PRId32`, so GIDs >= 2^31 appear as negative decimal integers.
fn parse_initgroups_gid(bytes: &[u8]) -> Option<u32> {
    if let Some(rest) = bytes.strip_prefix(b"-") {
        if rest.is_empty() || !rest.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let v: i32 = std::str::from_utf8(bytes).ok()?.parse().ok()?;
        if v >= 0 {
            return None;
        }
        return Some(v as u32);
    }
    parse_u32(bytes)
}

enum GetentRaw {
    /// The `getent` binary does not exist.
    NoBinary,
    /// `getent` ran and exited with `(status, stdout)`.
    Ran(std::process::ExitStatus, Vec<u8>),
}

/// Runs `program -- <args...>` with a sanitized environment, bounded duration,
/// and bounded stdout capture. Kills the child process group and reaps it on
/// timeout or oversized output.
fn exec_getent(program: &Path, args: &[&OsStr], timeout: Duration, max_bytes: usize) -> io::Result<GetentRaw> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(program);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .arg("--")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(GetentRaw::NoBinary),
        Err(e) => return Err(e),
    };

    let kill_and_reap = |child: &mut std::process::Child| {
        let pid = child.id();
        if pid > 0 && pid <= i32::MAX as u32 {
            // SAFETY: child has not been waited yet and leads its own process group.
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    };

    let mut stdout = child.stdout.take().expect("stdout piped");
    let fd = stdout.as_raw_fd();
    // SAFETY: fd is an open pipe descriptor owned by `stdout`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        let err = io::Error::last_os_error();
        drop(stdout);
        kill_and_reap(&mut child);
        return Err(err);
    }

    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut eof = false;

    while !eof {
        let now = Instant::now();
        if now >= deadline {
            drop(stdout);
            kill_and_reap(&mut child);
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("getent timed out after {}ms", timeout.as_millis())));
        }
        let rem_ms = deadline.saturating_duration_since(now).as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: `pfd` points to one valid `pollfd` struct.
        let ready = unsafe { libc::poll(&mut pfd, 1, rem_ms) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            drop(stdout);
            kill_and_reap(&mut child);
            return Err(err);
        }
        if ready == 0 {
            drop(stdout);
            kill_and_reap(&mut child);
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("getent timed out after {}ms", timeout.as_millis())));
        }
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    if out.len().saturating_add(n) > max_bytes {
                        drop(stdout);
                        kill_and_reap(&mut child);
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "getent output exceeded size limit"));
                    }
                    out.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    drop(stdout);
                    kill_and_reap(&mut child);
                    return Err(e);
                }
            }
        }
    }
    drop(stdout);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(GetentRaw::Ran(status, out)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_and_reap(&mut child);
                    return Err(io::Error::new(io::ErrorKind::TimedOut, format!("getent timed out after {}ms", timeout.as_millis())));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                kill_and_reap(&mut child);
                return Err(e);
            }
        }
    }
}

/// Looks up one `passwd` record via `getent`, distinguishing exit 2 (absent or
/// directory unavailable) from missing `getent` binary or invalid key.
fn getent_account_lookup_with(program: &Path, key: &[u8], timeout: Duration, max_bytes: usize) -> io::Result<LookupResult<Account>> {
    if !valid_key(key) {
        return Ok(LookupResult::NotFound);
    }
    let args = [OsStr::new("passwd"), OsStr::from_bytes(key)];
    match exec_getent(program, &args, timeout, max_bytes)? {
        GetentRaw::NoBinary => Ok(LookupResult::NotFound),
        GetentRaw::Ran(status, stdout) => match status.code() {
            Some(0) => passwd_line(&stdout)
                .map(LookupResult::Found)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unexpected getent passwd output for {}", String::from_utf8_lossy(key)))),
            Some(2) => Ok(LookupResult::UnavailableOrNotFound),
            _ => Err(io::Error::other(format!("getent passwd {}: {status}", String::from_utf8_lossy(key)))),
        },
    }
}

/// An account the system knows from another source than /etc/passwd (SSSD,
/// LDAP, winbind, Entra ID, systemd-homed): this binary is static, so its own
/// lookups read /etc/passwd alone and can't load the system's NSS modules
/// (#349). `getent` uses them. Ok(None): no such account, or no getent.
fn getent_account(key: &[u8]) -> io::Result<Option<Account>> {
    let Some(program) = find_getent() else { return Ok(None) };
    getent_account_with(program, key)
}

fn getent_account_with(program: &Path, key: &[u8]) -> io::Result<Option<Account>> {
    getent_account_with_limits(program, key, GETENT_TIMEOUT, GETENT_MAX_STDOUT)
}

fn getent_account_with_limits(program: &Path, key: &[u8], timeout: Duration, max_bytes: usize) -> io::Result<Option<Account>> {
    match getent_account_lookup_with(program, key, timeout, max_bytes)? {
        LookupResult::Found(a) => Ok(Some(a)),
        LookupResult::NotFound | LookupResult::UnavailableOrNotFound => Ok(None),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BatchAccounts {
    accounts: Vec<Account>,
    unresolved: usize,
}

fn parse_passwd_lines(text: &[u8]) -> io::Result<Vec<Account>> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let trimmed = text.strip_suffix(b"\n").unwrap_or(text);
    let mut out = Vec::new();
    for raw in trimmed.split(|&b| b == b'\n') {
        let account = passwd_line(raw).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unexpected getent passwd output"))?;
        out.push(account);
    }
    Ok(out)
}

/// Queries a batch of non-numeric account names in one `getent passwd` call,
/// falling back to single-key queries if a non-glibc `getent` rejects or
/// ignores extra keys.
fn getent_accounts_batch_with(program: &Path, keys: &[&CStr], timeout: Duration, max_bytes: usize) -> io::Result<BatchAccounts> {
    if keys.is_empty() {
        return Ok(BatchAccounts::default());
    }
    let mut args: Vec<&OsStr> = Vec::with_capacity(1 + keys.len());
    args.push(OsStr::new("passwd"));
    for k in keys {
        args.push(OsStr::from_bytes(k.to_bytes()));
    }
    match exec_getent(program, &args, timeout, max_bytes)? {
        GetentRaw::NoBinary => Ok(BatchAccounts::default()),
        GetentRaw::Ran(status, stdout) => match status.code() {
            Some(0) => {
                let accounts = parse_passwd_lines(&stdout)?;
                if accounts.is_empty() || accounts.len() > keys.len() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected getent passwd batch output"));
                }
                if keys.len() > 1 && accounts.len() < keys.len() {
                    return getent_accounts_individually(program, keys, timeout, max_bytes);
                }
                Ok(BatchAccounts { accounts, unresolved: 0 })
            }
            Some(2) => {
                let accounts = parse_passwd_lines(&stdout)?;
                if accounts.len() > keys.len() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected getent passwd batch output"));
                }
                let unresolved = keys.len().saturating_sub(accounts.len()).max(1);
                Ok(BatchAccounts { accounts, unresolved })
            }
            Some(1) if keys.len() > 1 => getent_accounts_individually(program, keys, timeout, max_bytes),
            _ => Err(io::Error::other(format!("getent passwd batch: {status}"))),
        },
    }
}

fn getent_accounts_individually(program: &Path, keys: &[&CStr], timeout: Duration, max_bytes: usize) -> io::Result<BatchAccounts> {
    let mut out = BatchAccounts::default();
    for k in keys {
        match getent_account_lookup_with(program, k.to_bytes(), timeout, max_bytes)? {
            LookupResult::Found(a) => out.accounts.push(a),
            LookupResult::UnavailableOrNotFound => out.unresolved += 1,
            LookupResult::NotFound => {}
        }
    }
    Ok(out)
}

/// A group resolved through NSS (`getent group`), returning `Ok(None)` only
/// when no `getent` binary exists so the caller can fall back to `/etc/group`.
fn getent_group_lookup(key: &[u8]) -> io::Result<Option<LookupResult<Group>>> {
    let Some(program) = find_getent() else { return Ok(None) };
    getent_group_lookup_with(program, key)
}

fn getent_group_lookup_with(program: &Path, key: &[u8]) -> io::Result<Option<LookupResult<Group>>> {
    if !valid_key(key) {
        return Ok(Some(LookupResult::NotFound));
    }
    let args = [OsStr::new("group"), OsStr::from_bytes(key)];
    match exec_getent(program, &args, GETENT_TIMEOUT, GETENT_MAX_STDOUT)? {
        GetentRaw::NoBinary => Ok(None),
        GetentRaw::Ran(status, stdout) => match status.code() {
            Some(0) => group_line(&stdout)
                .map(|g| Some(LookupResult::Found(g)))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unexpected getent group output for {}", String::from_utf8_lossy(key)))),
            Some(2) => Ok(Some(LookupResult::UnavailableOrNotFound)),
            _ => Err(io::Error::other(format!("getent group {}: {status}", String::from_utf8_lossy(key)))),
        },
    }
}

#[cfg(test)]
fn getent_group(key: &[u8]) -> io::Result<Option<Group>> {
    match getent_group_lookup(key)? {
        Some(LookupResult::Found(g)) => Ok(Some(g)),
        _ => Ok(None),
    }
}

#[cfg(test)]
fn getent_group_with(program: &Path, key: &[u8]) -> io::Result<Option<Group>> {
    match getent_group_lookup_with(program, key)? {
        Some(LookupResult::Found(g)) => Ok(Some(g)),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitgroupsResult {
    Member,
    NotMember,
    /// `getent initgroups` exited 2 (unknown user or NSS temporarily unavailable).
    UnavailableOrNotFound,
}

/// Checks supplementary group memberships through NSS (`getent initgroups`),
/// covering directory memberships that are not written into /etc/group.
fn initgroups_has_detailed(user: &CStr, gid: u32) -> io::Result<InitgroupsResult> {
    let Some(program) = find_getent() else { return Ok(InitgroupsResult::NotMember) };
    initgroups_has_detailed_with_limits(program, user, gid, GETENT_TIMEOUT, GETENT_MAX_STDOUT)
}

fn initgroups_has_detailed_with_limits(program: &Path, user: &CStr, gid: u32, timeout: Duration, max_bytes: usize) -> io::Result<InitgroupsResult> {
    let key = user.to_bytes();
    if !valid_key(key) {
        return Ok(InitgroupsResult::NotMember);
    }
    let args = [OsStr::new("initgroups"), OsStr::from_bytes(key)];
    match exec_getent(program, &args, timeout, max_bytes)? {
        GetentRaw::NoBinary => Ok(InitgroupsResult::NotMember),
        GetentRaw::Ran(status, stdout) => match status.code() {
            Some(0) => {
                let gids = initgroups_line(&stdout, key)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unexpected getent initgroups output for {}", user.to_string_lossy())))?;
                Ok(if gids.contains(&gid) { InitgroupsResult::Member } else { InitgroupsResult::NotMember })
            }
            // 1: database unknown (non-glibc getent).
            Some(1) => Ok(InitgroupsResult::NotMember),
            // 2: no such key or NSS temporarily unavailable.
            Some(2) => Ok(InitgroupsResult::UnavailableOrNotFound),
            _ => Err(io::Error::other(format!("getent initgroups {}: {status}", user.to_string_lossy()))),
        },
    }
}

#[cfg(test)]
fn initgroups_has_with(program: &Path, user: &CStr, gid: u32) -> io::Result<bool> {
    initgroups_has_with_limits(program, user, gid, GETENT_TIMEOUT, GETENT_MAX_STDOUT)
}

#[cfg(test)]
fn initgroups_has_with_limits(program: &Path, user: &CStr, gid: u32, timeout: Duration, max_bytes: usize) -> io::Result<bool> {
    Ok(initgroups_has_detailed_with_limits(program, user, gid, timeout, max_bytes)? == InitgroupsResult::Member)
}

/// `name:password:uid:gid:gecos:dir:shell`, the single line of `getent passwd`.
fn passwd_line(text: &[u8]) -> Option<Account> {
    let line = single_line(text)?;
    let mut f = line.split(|&b| b == b':');
    let name = CString::new(f.next().filter(|n| !n.is_empty())?).ok()?;
    let _password = f.next()?;
    let uid = parse_u32(f.next()?)?;
    let gid = parse_u32(f.next()?)?;
    let _gecos = f.next()?;
    let _dir = f.next()?;
    let _shell = f.next()?;
    Some(Account { name, uid, gid })
}

/// `name:password:gid:members`, the single line of `getent group`.
fn group_line(text: &[u8]) -> Option<Group> {
    let line = single_line(text)?;
    let mut f = line.split(|&b| b == b':');
    let name = CString::new(f.next().filter(|n| !n.is_empty())?).ok()?;
    let _password = f.next()?;
    let gid = parse_u32(f.next()?)?;
    let raw_members = f.next()?;
    if f.next().is_some() {
        return None;
    }
    let mut members = Vec::new();
    for m in raw_members.split(|&b| b == b',').filter(|m| !m.is_empty()) {
        members.push(CString::new(m).ok()?);
    }
    Some(Group { name, gid, members })
}

/// `<user> [<gid> ...]`, the single line of `getent initgroups <user>`.
fn initgroups_line(text: &[u8], expected_user: &[u8]) -> Option<Vec<u32>> {
    let line = single_line(text)?;
    let rest = line.strip_prefix(expected_user)?;
    if !rest.is_empty() && !matches!(rest[0], b' ' | b'\t') {
        return None;
    }
    let mut gids = Vec::new();
    for tok in rest.split(|b| matches!(b, b' ' | b'\t')).filter(|s| !s.is_empty()) {
        gids.push(parse_initgroups_gid(tok)?);
    }
    Some(gids)
}

fn group_by_name_with(
    name: &CStr,
    getent_lookup: impl Fn(&[u8]) -> io::Result<Option<LookupResult<Group>>>,
    local_lookup: impl Fn(&CStr) -> io::Result<Option<Group>>,
) -> io::Result<LookupResult<Group>> {
    // Prefer `getent group` when available so /etc/nsswitch.conf ordering and
    // `[SUCCESS=merge]` are honored. Fall back to static `getgrnam_r` only
    // when no `getent` binary exists.
    match getent_lookup(name.to_bytes())? {
        Some(LookupResult::Found(g)) => Ok(match_group_name(name.to_bytes(), g).map_or(LookupResult::NotFound, LookupResult::Found)),
        Some(other) => Ok(other),
        None => Ok(local_lookup(name)?.map_or(LookupResult::NotFound, LookupResult::Found)),
    }
}

fn group_by_name_detailed(name: &CStr) -> io::Result<LookupResult<Group>> {
    group_by_name_with(name, getent_group_lookup, local_group_by_name)
}

fn group_by_name(name: &CStr) -> io::Result<Group> {
    match group_by_name_detailed(name)? {
        LookupResult::Found(g) => Ok(g),
        LookupResult::NotFound | LookupResult::UnavailableOrNotFound => Err(io::Error::new(io::ErrorKind::NotFound, format!("no group {}", name.to_string_lossy()))),
    }
}

fn c_name(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn group_members(group: &CStr) -> io::Result<Vec<String>> {
    let g = group_by_name(group)?;
    Ok(g.members.iter().map(|n| n.to_string_lossy().into_owned()).collect())
}

/// The users in `group` by name (its member list, not primary-group users).
pub fn members(group: &str) -> io::Result<Vec<String>> {
    group_members(&c_name(group)?)
}

/// A group's numeric id.
pub fn group_gid(group: &OsStr) -> io::Result<u32> {
    let name = CString::new(group.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    Ok(group_by_name(&name)?.gid)
}

/// A user's numeric id, if the account exists.
#[cfg(test)]
pub fn uid(user: &str) -> Option<u32> {
    Some(account_by_name(&c_name(user).ok()?).ok()??.uid)
}

/// Resolves an account by name and returns its canonical name from the user
/// database (which may differ in case or domain suffix for directory accounts).
pub fn account_name(user: &str) -> io::Result<Option<String>> {
    let Ok(name) = c_name(user) else { return Ok(None) };
    match account_by_name(&name)? {
        Some(a) => a
            .name
            .into_string()
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("non-UTF-8 account name for {user}"))),
        None => Ok(None),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ResolvedMembers {
    /// Sorted, deduplicated UIDs of all resolved members.
    uids: Vec<u32>,
    /// Members resolved through NSS (`getent`) rather than `/etc/passwd`.
    directory_members: usize,
    /// Members whose `getent passwd` lookup returned exit 2 (absent or NSS unavailable).
    unresolved: usize,
    /// True when distinct members exceed `MAX_ACL_MEMBERS`.
    oversized: bool,
}

/// Resolves group member names to UIDs, using `/etc/passwd` first and batching
/// directory accounts through `getent passwd`. Groups larger than
/// `MAX_ACL_MEMBERS` skip lookup and fall back to 0660 group permissions.
fn resolve_members(group: &Group) -> io::Result<ResolvedMembers> {
    let program = find_getent();
    resolve_members_with(
        group,
        local_account_by_name,
        |chunk| match program {
            Some(p) => getent_accounts_batch_with(p, chunk, GETENT_TIMEOUT, GETENT_MAX_STDOUT),
            None => Ok(BatchAccounts::default()),
        },
        |name| match program {
            Some(p) => getent_account_lookup_with(p, name.to_bytes(), GETENT_TIMEOUT, GETENT_MAX_STDOUT),
            None => Ok(LookupResult::NotFound),
        },
    )
}

fn resolve_members_with(
    group: &Group,
    local_lookup: impl Fn(&CStr) -> io::Result<Option<Account>>,
    batch_lookup: impl Fn(&[&CStr]) -> io::Result<BatchAccounts>,
    single_lookup: impl Fn(&CStr) -> io::Result<LookupResult<Account>>,
) -> io::Result<ResolvedMembers> {
    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut unique: Vec<&CStr> = Vec::new();
    for name in &group.members {
        if seen.insert(name.to_bytes()) {
            unique.push(name.as_c_str());
        }
    }
    if unique.len() > MAX_ACL_MEMBERS {
        return Ok(ResolvedMembers { uids: Vec::new(), directory_members: 0, unresolved: 0, oversized: true });
    }

    let mut uids = Vec::new();
    let mut dir_batch: Vec<&CStr> = Vec::new();
    let mut numeric: Vec<&CStr> = Vec::new();
    for name in unique {
        if let Some(a) = local_lookup(name)? {
            uids.push(a.uid);
        } else if !valid_key(name.to_bytes()) {
            continue;
        } else if is_numeric_key(name.to_bytes()) {
            numeric.push(name);
        } else {
            dir_batch.push(name);
        }
    }

    let mut directory_members = 0usize;
    let mut unresolved = 0usize;

    for name in numeric {
        match single_lookup(name)? {
            LookupResult::Found(a) => {
                if let Some(a) = match_account_name(name.to_bytes(), a) {
                    uids.push(a.uid);
                    directory_members += 1;
                }
            }
            LookupResult::UnavailableOrNotFound => unresolved += 1,
            LookupResult::NotFound => {}
        }
    }

    for chunk in dir_batch.chunks(GETENT_PASSWD_BATCH) {
        let batch = batch_lookup(chunk)?;
        directory_members += batch.accounts.len();
        unresolved += batch.unresolved;
        for a in batch.accounts {
            uids.push(a.uid);
        }
    }

    uids.sort_unstable();
    uids.dedup();
    Ok(ResolvedMembers { uids, directory_members, unresolved, oversized: false })
}

#[cfg(test)]
fn uids_of_with(group: &Group, lookup: impl Fn(&CStr) -> io::Result<Option<Account>>) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    for name in &group.members {
        if let Some(a) = lookup(name)? {
            out.push(a.uid);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Gives every member of `group` read-write access to `socket` by name, so a
/// member just added can use it without signing in again. Returns how many.
pub fn grant(socket: &Path, group: &str) -> io::Result<usize> {
    let resolved = resolve_members(&group_by_name(&c_name(group)?)?)?;
    if resolved.oversized {
        let _ = set(socket, &[]);
        return Err(io::Error::from_raw_os_error(libc::E2BIG));
    }
    if let Err(e) = set(socket, &resolved.uids) {
        if acl_too_large(&e) {
            let _ = set(socket, &[]);
        }
        return Err(e);
    }
    Ok(resolved.uids.len())
}

/// The uid at the other end of an accepted connection.
fn peer_uid(conn: &UnixStream) -> io::Result<u32> {
    // SAFETY: a connected socket and zeroed storage for its credentials.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the output pointers are valid and the buffer is the stated size.
    let r = unsafe { libc::getsockopt(conn.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) };
    if r != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(if r != 0 { io::Error::last_os_error() } else { io::Error::new(io::ErrorKind::InvalidData, "short peer credentials") });
    }
    Ok(cred.uid)
}

/// Whether the account is in the group by name or as its primary group.
fn group_member(user: &CStr, primary_gid: libc::gid_t, group: &Group) -> bool {
    primary_gid == group.gid || group.members.iter().any(|m| m.as_c_str() == user)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Membership {
    /// Confirmed member.
    Allowed,
    /// Confirmed non-member (or deleted local group/account).
    Revoked,
    /// Ambiguous `getent` exit-2 miss (absent or directory service temporarily down).
    MissingFromDirectory,
}

fn member_of_detailed(uid: u32, group: &LookupResult<Group>) -> io::Result<Membership> {
    member_of_detailed_with(uid, group, account_by_uid_detailed, initgroups_has_detailed)
}

fn member_of_detailed_with(
    uid: u32,
    group: &LookupResult<Group>,
    lookup: impl Fn(u32) -> io::Result<LookupResult<Account>>,
    initgroups: impl Fn(&CStr, u32) -> io::Result<InitgroupsResult>,
) -> io::Result<Membership> {
    let group = match group {
        LookupResult::Found(g) => g,
        LookupResult::NotFound => return Ok(Membership::Revoked),
        LookupResult::UnavailableOrNotFound => return Ok(Membership::MissingFromDirectory),
    };
    let a = match lookup(uid)? {
        LookupResult::Found(a) => a,
        LookupResult::NotFound => return Ok(Membership::Revoked),
        LookupResult::UnavailableOrNotFound => return Ok(Membership::MissingFromDirectory),
    };
    if group_member(&a.name, a.gid, group) {
        return Ok(Membership::Allowed);
    }
    match initgroups(&a.name, group.gid)? {
        InitgroupsResult::Member => Ok(Membership::Allowed),
        InitgroupsResult::NotMember => Ok(Membership::Revoked),
        InitgroupsResult::UnavailableOrNotFound => Ok(Membership::MissingFromDirectory),
    }
}

#[cfg(test)]
fn member_of_with(
    uid: u32,
    group: Option<&Group>,
    lookup: impl Fn(u32) -> io::Result<Option<Account>>,
    initgroups: impl Fn(&CStr, u32) -> io::Result<bool>,
) -> io::Result<bool> {
    let g = match group {
        Some(g) => LookupResult::Found(g.clone()),
        None => LookupResult::NotFound,
    };
    let m = member_of_detailed_with(
        uid,
        &g,
        |u| Ok(lookup(u)?.map_or(LookupResult::NotFound, LookupResult::Found)),
        |n, gid| Ok(if initgroups(n, gid)? { InitgroupsResult::Member } else { InitgroupsResult::NotMember }),
    )?;
    Ok(m == Membership::Allowed)
}

#[cfg(test)]
fn member(uid: u32, group: &CStr) -> io::Result<bool> {
    Ok(member_of_detailed(uid, &group_by_name_detailed(group)?)? == Membership::Allowed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some(FileStamp { dev: m.dev(), ino: m.ino(), len: m.len(), mtime_sec: m.mtime(), mtime_nsec: m.mtime_nsec() })
}

#[derive(Debug, Clone)]
enum CachedDecision {
    Allowed,
    Denied,
    Failed(io::ErrorKind, String),
}

impl CachedDecision {
    fn from_result(res: &io::Result<bool>) -> Self {
        match res {
            Ok(true) => Self::Allowed,
            Ok(false) => Self::Denied,
            Err(e) => Self::Failed(e.kind(), e.to_string()),
        }
    }

    fn to_result(&self) -> io::Result<bool> {
        match self {
            Self::Allowed => Ok(true),
            Self::Denied => Ok(false),
            Self::Failed(kind, msg) => Err(io::Error::new(*kind, msg.clone())),
        }
    }
}

#[derive(Default)]
struct LookupState {
    recent: HashMap<u32, (CachedDecision, Instant)>,
    in_flight: HashMap<u32, Arc<Condvar>>,
    active: usize,
    waiting: usize,
}

struct LookupGuard<'a> {
    gate: &'a Gate,
    uid: u32,
    waiter: Arc<Condvar>,
    finished: bool,
}

impl<'a> LookupGuard<'a> {
    fn finish(mut self, res: &io::Result<bool>) {
        self.finished = true;
        let mut state = self.gate.lookup.lock().unwrap_or_else(|e| e.into_inner());
        Gate::insert_recent(&mut state.recent, self.uid, CachedDecision::from_result(res));
        state.in_flight.remove(&self.uid);
        state.active = state.active.saturating_sub(1);
        self.waiter.notify_all();
        self.gate.lookup_slot.notify_all();
    }
}

impl Drop for LookupGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self.gate.lookup.lock().unwrap_or_else(|e| e.into_inner());
            state.in_flight.remove(&self.uid);
            state.active = state.active.saturating_sub(1);
            self.waiter.notify_all();
            self.gate.lookup_slot.notify_all();
        }
    }
}

struct CachedGroup {
    stamp: Option<FileStamp>,
    at: Instant,
    result: Result<LookupResult<Group>, (io::ErrorKind, String)>,
}

#[derive(Debug, Default)]
struct AclState {
    /// Member names as of the last evaluated group record.
    names: Option<Vec<CString>>,
    /// Exact sorted UID set last written to the socket ACL.
    uids: Option<Vec<u32>>,
    /// Whether the last resolution included NSS directory accounts.
    has_directory: bool,
    /// Whether the last resolution had unresolved names (exit 2).
    has_unresolved: bool,
    /// Consecutive ticks with unresolved members (or exit-2 group).
    unresolved_streak: u32,
    /// Consecutive ticks where unresolved lookups would drop an existing non-closed ACL UID.
    shrink_streak: u32,
    /// Member list for which ACL writes permanently failed or exceeded limits.
    skipped_for_names: Option<Vec<CString>>,
    /// Timestamp of the last successful `fetch_group()`.
    last_group_fetch: Option<Instant>,
    /// Timestamp of the last `resolve(&group)` (or skip for unchanged names).
    last_resolve: Option<Instant>,
    /// Last observed `/etc/group` stamp so local edits trigger immediate refresh.
    etc_group_stamp: Option<FileStamp>,
    /// Avoid repeating the oversized/permanent-error warning on every tick.
    warned_skip: bool,
}

impl AclState {
    fn initialized(&self) -> bool {
        self.uids.is_some() || self.skipped_for_names.is_some()
    }

    fn fast_retry(&self) -> bool {
        (self.has_unresolved && self.unresolved_streak < DIRECTORY_MISS_GRACE)
            || (self.shrink_streak > 0 && self.shrink_streak < DIRECTORY_MISS_GRACE)
    }
}

/// Who may use the engine's socket: root, the account the daemon runs as, and
/// the members of one group, checked when they connect and again while they
/// stay connected.
pub struct Gate {
    group: CString,
    /// The daemon's socket, and the one containers are given (caller.rs).
    sockets: Vec<PathBuf>,
    owner: u32,
    open: Mutex<HashMap<u64, (u32, UnixStream)>>,
    next: AtomicU64,
    failing: AtomicBool,
    /// Recent per-uid decisions, in-flight coalescing, and concurrency cap so
    /// bursts of connections or local lookup storms stay bounded.
    lookup: Mutex<LookupState>,
    lookup_slot: Condvar,
    /// Short-TTL group lookup cache for the accept path, invalidated when
    /// `/etc/group` changes.
    group_cache: Mutex<Option<CachedGroup>>,
    /// Consecutive ambiguous exit-2 misses per connected uid.
    misses: Mutex<HashMap<u32, u32>>,
    /// UIDs written by `grant()` at startup, used to seed `watch()` so an
    /// outage right after boot does not shrink the initial ACL on tick 1.
    startup_uids: Mutex<Option<Vec<u32>>>,
}

/// A connection the gate let in; dropping it stops watching that connection.
pub struct Seat {
    gate: Arc<Gate>,
    id: u64,
}

impl Drop for Seat {
    fn drop(&mut self) {
        self.gate.open.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
    }
}

impl Gate {
    pub fn new(sockets: &[&Path], group: &OsStr) -> io::Result<Arc<Gate>> {
        let group = CString::new(group.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: geteuid cannot fail.
        let owner = unsafe { libc::geteuid() };
        Ok(Arc::new(Gate {
            group,
            sockets: sockets.iter().map(|s| s.to_path_buf()).collect(),
            owner,
            open: Mutex::default(),
            next: AtomicU64::new(0),
            failing: AtomicBool::new(false),
            lookup: Mutex::default(),
            lookup_slot: Condvar::new(),
            group_cache: Mutex::default(),
            misses: Mutex::default(),
            startup_uids: Mutex::default(),
        }))
    }

    fn name(&self) -> std::borrow::Cow<'_, str> {
        self.group.to_string_lossy()
    }

    fn insert_recent(recent: &mut HashMap<u32, (CachedDecision, Instant)>, uid: u32, decision: CachedDecision) {
        recent.retain(|_, (_, at)| at.elapsed() < RECHECK);
        if recent.len() >= MAX_RECENT_ENTRIES
            && !recent.contains_key(&uid)
            && let Some(&oldest) = recent.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k)
        {
            recent.remove(&oldest);
        }
        recent.insert(uid, (decision, Instant::now()));
    }

    fn remember_decision(&self, uid: u32, decision: CachedDecision) {
        let mut state = self.lookup.lock().unwrap_or_else(|e| e.into_inner());
        Self::insert_recent(&mut state.recent, uid, decision);
    }

    fn store_group_cache(&self, stamp: Option<FileStamp>, at: Instant, result: Result<LookupResult<Group>, (io::ErrorKind, String)>) {
        *self.group_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedGroup { stamp, at, result });
    }

    fn cached_group(&self) -> io::Result<LookupResult<Group>> {
        let stamp = file_stamp(Path::new(ETC_GROUP));
        let now = Instant::now();
        {
            let cache = self.group_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = cache.as_ref()
                && c.stamp == stamp
                && now.saturating_duration_since(c.at) < GROUP_CACHE_TTL
            {
                return match &c.result {
                    Ok(g) => Ok(g.clone()),
                    Err((kind, msg)) => Err(io::Error::new(*kind, msg.clone())),
                };
            }
        }
        let fresh = group_by_name_detailed(&self.group);
        let stored = match &fresh {
            Ok(g) => Ok(g.clone()),
            Err(e) => Err((e.kind(), e.to_string())),
        };
        self.store_group_cache(stamp, Instant::now(), stored);
        fresh
    }

    fn allowed(&self, uid: u32) -> io::Result<bool> {
        self.allowed_with(uid, |u| {
            let group = self.cached_group()?;
            Ok(member_of_detailed(u, &group)? == Membership::Allowed)
        })
    }

    fn allowed_with(&self, uid: u32, check: impl Fn(u32) -> io::Result<bool>) -> io::Result<bool> {
        if uid == 0 || uid == self.owner {
            return Ok(true);
        }
        let mut state = self.lookup.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some((decision, at)) = state.recent.get(&uid)
                && at.elapsed() < RECHECK
            {
                return decision.to_result();
            }
            if let Some(waiter) = state.in_flight.get(&uid).cloned() {
                state = waiter.wait(state).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            if state.active >= MAX_CONCURRENT_LOOKUPS {
                if state.waiting >= MAX_WAITING_LOOKUPS {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "too many concurrent access checks"));
                }
                state.waiting += 1;
                state = self.lookup_slot.wait(state).unwrap_or_else(|e| e.into_inner());
                state.waiting -= 1;
                continue;
            }
            let waiter = Arc::new(Condvar::new());
            state.in_flight.insert(uid, waiter.clone());
            state.active += 1;
            drop(state);

            let guard = LookupGuard { gate: self, uid, waiter, finished: false };
            let res = check(uid);
            guard.finish(&res);
            return res;
        }
    }

    /// One recheck: everyone connected against one read of the group, then
    /// the access list, if the group's members or directory UIDs changed.
    fn tick(&self, state: &mut AclState) {
        let now = Instant::now();
        let stamp = file_stamp(Path::new(ETC_GROUP));
        self.tick_with(state, now, stamp, || group_by_name_detailed(&self.group), member_of_detailed, resolve_members, |uids| self.set_all(uids));
    }

    #[allow(clippy::too_many_arguments)]
    fn tick_with(
        &self,
        state: &mut AclState,
        now: Instant,
        stamp: Option<FileStamp>,
        fetch_group: impl Fn() -> io::Result<LookupResult<Group>>,
        check_member: impl Fn(u32, &LookupResult<Group>) -> io::Result<Membership>,
        resolve: impl Fn(&Group) -> io::Result<ResolvedMembers>,
        write_acl: impl Fn(&[u32]) -> io::Result<()>,
    ) {
        let connected = !self.open.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
        let stamp_changed = state.etc_group_stamp != stamp;

        // Avoid idle `getent` forks when nobody is connected, `/etc/group` is
        // unchanged, and no unresolved directory members need fast retry.
        if !connected && !stamp_changed && state.initialized() && !state.fast_retry() {
            let interval = if state.has_directory || state.has_unresolved { IDLE_DIRECTORY_REFRESH } else { IDLE_LOCAL_NSS_REFRESH };
            if let Some(last) = state.last_group_fetch
                && now.saturating_duration_since(last) < interval
            {
                return;
            }
        }

        let group = fetch_group();
        if let Ok(ref g) = group {
            self.store_group_cache(stamp, now, Ok(g.clone()));
        }

        let closed_uids = if connected {
            self.sweep_membership_with(|uid| {
                if uid == 0 || uid == self.owner {
                    return Ok(Membership::Allowed);
                }
                let g = group.as_ref().map_err(|e| io::Error::new(e.kind(), e.to_string()))?;
                check_member(uid, g)
            })
        } else {
            HashSet::new()
        };

        let Ok(group) = group else { return };
        state.etc_group_stamp = stamp;
        state.last_group_fetch = Some(now);

        match group {
            LookupResult::NotFound => {
                state.unresolved_streak = 0;
                state.shrink_streak = 0;
                state.has_directory = false;
                state.has_unresolved = false;
                state.skipped_for_names = None;
                state.warned_skip = false;
                if state.uids.as_deref() == Some(&[]) {
                    state.names = Some(Vec::new());
                    return;
                }
                match write_acl(&[]) {
                    Ok(()) => {
                        crate::diag::info!("socket access list written", members = 0);
                        state.names = Some(Vec::new());
                        state.uids = Some(Vec::new());
                    }
                    Err(e) if acl_permanent_err(&e) => {
                        state.names = Some(Vec::new());
                        state.skipped_for_names = Some(Vec::new());
                    }
                    Err(e) => crate::diag::warn!("socket access list not written", error = e),
                }
            }
            LookupResult::UnavailableOrNotFound => {
                // Ambiguous exit 2 on `getent group`: do not wipe an existing
                // ACL on a single transient outage; clear only after grace.
                state.has_unresolved = true;
                state.unresolved_streak = state.unresolved_streak.saturating_add(1);
                let shrinking = state.uids.as_ref().is_some_and(|prev| prev.iter().any(|u| !closed_uids.contains(u)));
                if shrinking {
                    state.shrink_streak = state.shrink_streak.saturating_add(1);
                } else {
                    state.shrink_streak = 0;
                }
                if shrinking && state.shrink_streak < DIRECTORY_MISS_GRACE {
                    return;
                }
                if state.uids.as_deref() == Some(&[]) {
                    state.names = Some(Vec::new());
                    return;
                }
                match write_acl(&[]) {
                    Ok(()) => {
                        crate::diag::info!("socket access list written", members = 0);
                        state.names = Some(Vec::new());
                        state.uids = Some(Vec::new());
                        state.shrink_streak = 0;
                    }
                    Err(e) if acl_permanent_err(&e) => {
                        state.names = Some(Vec::new());
                        state.skipped_for_names = Some(Vec::new());
                        state.shrink_streak = 0;
                    }
                    Err(e) => crate::diag::warn!("socket access list not written", error = e),
                }
            }
            LookupResult::Found(group) => {
                let now_names = group.members.clone();
                let names_changed = state.names.as_ref() != Some(&now_names);

                if !names_changed && state.skipped_for_names.as_ref() == Some(&now_names) {
                    state.last_resolve = Some(now);
                    return;
                }

                let refresh_due = !state.initialized()
                    || names_changed
                    || !closed_uids.is_empty()
                    || state.fast_retry()
                    || ((state.has_directory || state.has_unresolved) && state.last_resolve.is_none_or(|t| now.saturating_duration_since(t) >= IDLE_DIRECTORY_REFRESH));

                if !refresh_due {
                    return;
                }

                let resolved = match resolve(&group) {
                    Ok(r) => r,
                    Err(e) => {
                        crate::diag::warn!("socket access list not written", error = e);
                        return;
                    }
                };

                state.last_resolve = Some(now);
                state.has_directory = resolved.directory_members > 0;
                state.has_unresolved = resolved.unresolved > 0;

                if resolved.oversized {
                    if state.uids.as_ref().is_none_or(|u| !u.is_empty()) {
                        let _ = write_acl(&[]);
                    }
                    if !state.warned_skip {
                        crate::diag::warn!("socket access list skipped: group exceeds access list limit; using group permissions", group = self.name(), members = now_names.len());
                        state.warned_skip = true;
                    }
                    state.names = Some(now_names.clone());
                    state.uids = Some(Vec::new());
                    state.skipped_for_names = Some(now_names);
                    state.unresolved_streak = 0;
                    state.shrink_streak = 0;
                    return;
                }

                if resolved.unresolved > 0 {
                    state.unresolved_streak = state.unresolved_streak.saturating_add(1);
                } else {
                    state.unresolved_streak = 0;
                }

                let shrinking = resolved.unresolved > 0
                    && state.uids.as_ref().is_some_and(|prev| prev.iter().any(|u| !closed_uids.contains(u) && !resolved.uids.contains(u)));
                if shrinking {
                    state.shrink_streak = state.shrink_streak.saturating_add(1);
                } else {
                    state.shrink_streak = 0;
                }

                let effective_uids = if shrinking && state.shrink_streak < DIRECTORY_MISS_GRACE {
                    if let Some(prev) = &state.uids {
                        let mut merged: Vec<u32> = prev.iter().copied().filter(|u| !closed_uids.contains(u)).collect();
                        merged.extend(resolved.uids.iter().copied());
                        merged.sort_unstable();
                        merged.dedup();
                        merged
                    } else {
                        resolved.uids
                    }
                } else {
                    resolved.uids
                };

                if state.uids.as_ref() == Some(&effective_uids) {
                    state.names = Some(now_names);
                    state.skipped_for_names = None;
                    state.warned_skip = false;
                    return;
                }

                match write_acl(&effective_uids) {
                    Ok(()) => {
                        crate::diag::info!("socket access list written", members = effective_uids.len());
                        state.names = Some(now_names);
                        state.uids = Some(effective_uids);
                        state.skipped_for_names = None;
                        state.warned_skip = false;
                        if state.shrink_streak >= DIRECTORY_MISS_GRACE {
                            state.shrink_streak = 0;
                        }
                    }
                    Err(e) if acl_too_large(&e) => {
                        let _ = write_acl(&[]);
                        if !state.warned_skip {
                            crate::diag::warn!("socket access list skipped: filesystem access list limit reached; using group permissions", group = self.name(), error = e);
                            state.warned_skip = true;
                        }
                        state.names = Some(now_names.clone());
                        state.uids = Some(Vec::new());
                        state.skipped_for_names = Some(now_names);
                        state.shrink_streak = 0;
                    }
                    Err(e) if unsupported(&e) => {
                        state.names = Some(now_names.clone());
                        state.skipped_for_names = Some(now_names);
                        state.shrink_streak = 0;
                    }
                    Err(e) => crate::diag::warn!("socket access list not written", error = e),
                }
            }
        }
    }

    /// Writes the socket's access list for the group's current members.
    pub fn grant(&self) -> io::Result<usize> {
        let resolved = resolve_members(&group_by_name(&self.group)?)?;
        if resolved.oversized {
            let _ = self.set_all(&[]);
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        if let Err(e) = self.set_all(&resolved.uids) {
            if acl_too_large(&e) {
                let _ = self.set_all(&[]);
            }
            return Err(e);
        }
        let len = resolved.uids.len();
        *self.startup_uids.lock().unwrap_or_else(|e| e.into_inner()) = Some(resolved.uids);
        Ok(len)
    }

    /// Writes each socket's access list.
    fn set_all(&self, uids: &[u32]) -> io::Result<()> {
        set_each(&self.sockets, uids, set)
    }

    /// Checks a new connection. `None`: refused, and the reason logged.
    pub fn admit(self: &Arc<Self>, conn: &UnixStream) -> Option<Seat> {
        self.admit_with(conn, |uid| self.allowed(uid))
    }

    fn admit_with(self: &Arc<Self>, conn: &UnixStream, check: impl Fn(u32) -> io::Result<bool>) -> Option<Seat> {
        let checked = peer_uid(conn).and_then(|uid| Ok((uid, check(uid)?)));
        let uid = match checked {
            Ok((uid, true)) => uid,
            Ok((uid, false)) => {
                crate::diag::warn!("socket connection refused: user is not in the access group", group = self.name(), uid = uid);
                return None;
            }
            Err(e) => {
                crate::diag::warn!("socket connection refused: access check failed", group = self.name(), error = e);
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
        self.open.lock().unwrap_or_else(|e| e.into_inner()).insert(id, (uid, keep));
        Some(Seat { gate: self.clone(), id })
    }

    /// Checks everyone still connected, once per account, and closes the
    /// connections of anyone no longer allowed. A failed lookup leaves the
    /// connections open (new ones are refused until lookups work again).
    /// Ambiguous directory exit-2 misses use a grace window before closing.
    fn sweep_membership_with(&self, check: impl Fn(u32) -> io::Result<Membership>) -> HashSet<u32> {
        let uids: HashSet<u32> = self.open.lock().unwrap_or_else(|e| e.into_inner()).values().map(|(u, _)| *u).collect();
        let mut removed = HashSet::new();
        let mut failed = None;
        let mut misses = self.misses.lock().unwrap_or_else(|e| e.into_inner());
        misses.retain(|u, _| uids.contains(u));
        drop(misses);

        for uid in uids {
            match check(uid) {
                Ok(Membership::Allowed) => {
                    self.misses.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
                    self.remember_decision(uid, CachedDecision::Allowed);
                }
                Ok(Membership::Revoked) => {
                    self.misses.lock().unwrap_or_else(|e| e.into_inner()).remove(&uid);
                    self.remember_decision(uid, CachedDecision::Denied);
                    removed.insert(uid);
                }
                Ok(Membership::MissingFromDirectory) => {
                    self.remember_decision(uid, CachedDecision::Denied);
                    let mut misses = self.misses.lock().unwrap_or_else(|e| e.into_inner());
                    let count = misses.entry(uid).or_insert(0);
                    *count = count.saturating_add(1);
                    if *count >= DIRECTORY_MISS_GRACE {
                        misses.remove(&uid);
                        removed.insert(uid);
                    }
                }
                Err(e) => failed = Some(e),
            }
        }
        match failed {
            Some(e) if !self.failing.swap(true, Ordering::Relaxed) => crate::diag::warn!("access check failed; open connections kept until it works again", group = self.name(), error = e),
            None if self.failing.swap(false, Ordering::Relaxed) => crate::diag::info!("access check works again", group = self.name()),
            _ => {}
        }
        if removed.is_empty() {
            return removed;
        }
        // Taken out as they're closed, so a thread that takes a while to
        // notice isn't closed and logged again on every recheck.
        self.open.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, (uid, conn)| {
            if !removed.contains(uid) {
                return true;
            }
            // Ends the connection for every thread using it: reads see the
            // end, writes fail, as if the client had hung up.
            let _ = conn.shutdown(Shutdown::Both);
            false
        });
        for uid in &removed {
            crate::diag::warn!("connections closed: user is no longer in the access group", group = self.name(), uid = uid);
        }
        removed
    }

    #[cfg(test)]
    fn sweep_with(&self, check: impl Fn(u32) -> io::Result<bool>) -> usize {
        self.sweep_membership_with(|uid| check(uid).map(|yes| if yes { Membership::Allowed } else { Membership::Revoked })).len()
    }

    /// Checks everyone connected every few seconds, and writes the access
    /// list again whenever the group's members change.
    pub fn watch(self: Arc<Self>) -> io::Result<()> {
        std::thread::Builder::new().name("kiri-access".into()).spawn(move || {
            // Names unknown at first so tick 1 catches any change after startup;
            // seed uids from startup grant() so a brief outage right after boot
            // does not shrink the initial ACL on tick 1.
            let initial_uids = self.startup_uids.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let mut state = AclState { uids: initial_uids, ..AclState::default() };
            loop {
                std::thread::sleep(RECHECK);
                self.tick(&mut state);
            }
        })?;
        Ok(())
    }
}

/// Writes the socket's access list: its owner and group (rw, as the mode
/// says), each of `uids` (rw), nobody else.
pub fn set(socket: &Path, uids: &[u32]) -> io::Result<()> {
    let blob = acl(uids);
    let path = CString::new(socket.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: both strings are valid and NUL-terminated; blob outlives the call.
    let r = unsafe { libc::setxattr(path.as_ptr(), c"system.posix_acl_access".as_ptr(), blob.as_ptr().cast(), blob.len(), 0) };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Writes the access list of every one of `sockets` with `set`, each tried
/// whatever became of the one before; the first error, if any.
fn set_each(sockets: &[PathBuf], uids: &[u32], set: impl Fn(&Path, &[u32]) -> io::Result<()>) -> io::Result<()> {
    let mut first = Ok(());
    for s in sockets {
        if let Err(e) = set(s, uids) {
            first = first.and(Err(e));
        }
    }
    first
}

/// Whether a failure to write the list means this system can't keep one
/// (a filesystem without ACLs): then only signing in again lets a new member in.
pub fn unsupported(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EOPNOTSUPP)
}

/// Whether a failure to write the list means the member list exceeds the
/// filesystem's POSIX ACL size limit: fall back to the socket's 0660 group.
pub fn acl_too_large(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::E2BIG))
}

fn acl_permanent_err(e: &io::Error) -> bool {
    unsupported(e) || acl_too_large(e)
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

    /// One socket's list that cannot be written does not keep the next
    /// from being written; the error is still reported.
    #[test]
    fn every_sockets_list_is_written_when_one_fails() {
        let sockets = [PathBuf::from("/run/a.sock"), PathBuf::from("/run/b.sock")];
        let written = Mutex::new(Vec::new());
        let r = set_each(&sockets, &[1000], |s, _| {
            written.lock().unwrap().push(s.to_path_buf());
            if s.ends_with("a.sock") { Err(io::Error::other("no")) } else { Ok(()) }
        });
        assert!(r.is_err());
        assert_eq!(*written.lock().unwrap(), sockets);
    }

    #[test]
    fn acl_lists_each_user_once_in_order_and_not_root() {
        let b = acl(&[1001, 0, 1000, 1001]);
        assert_eq!(&b[..4], &[2, 0, 0, 0]);
        let entries: Vec<(u16, u16, u32)> = b[4..]
            .chunks(8)
            .map(|c| (u16::from_le_bytes([c[0], c[1]]), u16::from_le_bytes([c[2], c[3]]), u32::from_le_bytes([c[4], c[5], c[6], c[7]])))
            .collect();
        assert_eq!(entries, vec![(1, 6, u32::MAX), (2, 6, 1000), (2, 6, 1001), (4, 6, u32::MAX), (0x10, 6, u32::MAX), (0x20, 0, u32::MAX)]);
    }

    #[test]
    fn membership_includes_names_and_primary_group() {
        let group = Group { name: c"kiri".to_owned(), gid: 100, members: vec![c"alice".to_owned()] };
        assert!(group_member(c"alice", 1, &group));
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
        let Some(a) = account_by_uid(uid).unwrap() else { return };
        let g = {
            // SAFETY: getgrgid_r with a group record and a buffer of the stated size.
            let gid = a.gid;
            unsafe { nss(|rec, buf, len, out| libc::getgrgid_r(gid, rec, buf, len, out)) }.unwrap()
        };
        let Some(g) = g else { return };
        // SAFETY: gr_name is a NUL-terminated string in the record's buffer.
        let name = unsafe { CStr::from_ptr(g.rec.gr_name) }.to_owned();
        assert!(member(uid, &name).unwrap());
        assert!(!member(u32::MAX - 7, &name).unwrap(), "no such account");
        assert!(!member(uid, c"kiri-no-such-group-x").unwrap(), "a deleted group has nobody in it");
    }

    #[test]
    fn lookups_from_many_threads_agree() {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let Some(me) = account_by_uid(uid).unwrap() else { return };
        // Asked of getent too, so once rather than in the loop.
        assert!(account_by_uid(u32::MAX - 7).unwrap().is_none());
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..200 {
                        assert_eq!(account_by_uid(uid).unwrap().unwrap(), me);
                    }
                });
            }
        });
    }

    fn gate() -> Arc<Gate> {
        let mut g = Gate::new(&[Path::new("/nonexistent")], OsStr::new("kiri")).unwrap();
        Arc::get_mut(&mut g).unwrap().owner = u32::MAX;
        g
    }

    #[test]
    fn root_and_the_daemons_own_account_need_no_group() {
        let g = gate();
        assert!(g.allowed(0).unwrap());
        let mut g2 = Gate::new(&[Path::new("/nonexistent")], OsStr::new("kiri-no-such-group-x")).unwrap();
        Arc::get_mut(&mut g2).unwrap().owner = 4242;
        assert!(g2.allowed(4242).unwrap());
        assert!(!g2.allowed(4243).unwrap(), "anyone else is looked up");
    }

    #[test]
    fn refused_or_failed_checks_keep_the_connection_out() {
        let g = gate();
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(g.admit_with(&a, |_| Ok(false)).is_none());
        assert!(g.admit_with(&a, |_| Err(io::Error::other("nss down"))).is_none());
        assert!(g.open.lock().unwrap().is_empty());
        let seat = g.admit_with(&a, |_| Ok(true)).unwrap();
        assert_eq!(g.open.lock().unwrap().len(), 1);
        drop(seat);
        assert!(g.open.lock().unwrap().is_empty(), "a closed connection isn't watched");
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
        assert_eq!(g.sweep_with(|_| Ok(false)), 0, "closed once, not again on every recheck");
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
                    libc::_exit(if std::os::unix::net::UnixStream::connect(&sock).is_ok() { 0 } else { 1 });
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
        let mut gate = Gate::new(&[sock.as_path()], OsStr::new("root")).unwrap();
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
        let run = |args: &[&str]| assert!(Command::new(args[0]).args(&args[1..]).status().unwrap().success(), "{args:?}");
        let _ = Command::new("userdel").arg("kiritest").status();
        let _ = Command::new("groupdel").arg("kiritestg").status();
        run(&["groupadd", "kiritestg"]);
        run(&["useradd", "-M", "-G", "kiritestg", "kiritest"]);
        let who = uid("kiritest").unwrap();
        assert!(members("kiritestg").unwrap().contains(&"kiritest".to_owned()));

        let dir = Path::new("/dev/shm").join(format!("kiri-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sock = dir.join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660)).unwrap();
        let mut gate = Gate::new(&[sock.as_path()], OsStr::new("kiritestg")).unwrap();
        Arc::get_mut(&mut gate).unwrap().owner = u32::MAX;
        assert_eq!(gate.grant().unwrap(), 1);

        // The member connects (by the list: they have no group in this
        // process) and waits for an answer that never comes.
        // SAFETY: fork in a test; the child only drops ids, connects, reads and exits.
        let pid = unsafe {
            let pid = libc::fork();
            if pid == 0 {
                // A gid that isn't the test group, so only the list can let it in.
                if libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(65534) != 0 || libc::setuid(who) != 0 {
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
        let mut state = AclState::default();
        gate.tick(&mut state);
        assert_eq!(gate.open.lock().unwrap().len(), 1, "still a member");

        run(&["gpasswd", "-d", "kiritest", "kiritestg"]);
        gate.tick(&mut state);
        assert!(gate.open.lock().unwrap().is_empty(), "removed: their connection is closed");
        assert_eq!(state.names, Some(vec![]), "and the list is written without them");
        assert_eq!(state.uids, Some(vec![]), "and the list uids are cleared");
        let mut status = 0;
        // SAFETY: waits for the child forked above.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(libc::WEXITSTATUS(status), 0, "the client saw the connection end");
        assert_eq!(gate.grant().unwrap(), 0, "and the list no longer names them");

        let _ = Command::new("userdel").arg("kiritest").status();
        let _ = Command::new("groupdel").arg("kiritestg").status();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn acl_without_users_is_the_plain_mode() {
        assert_eq!(acl(&[]).len(), 4 + 4 * 8);
    }

    /// Accounts only NSS knows (#349) come from getent's passwd line.
    #[test]
    fn getent_lines_read_as_accounts() {
        let alice = Account { name: c"alice".to_owned(), uid: 5001, gid: 5001 };
        assert_eq!(passwd_line(b"alice:x:5001:5001:Directory user:/home/alice:/bin/bash\n"), Some(alice));
        assert_eq!(passwd_line(b"alice@corp.example:*:1916400001:1916400001::/home/alice:/bin/sh").map(|a| a.uid), Some(1_916_400_001));
        assert_eq!(passwd_line(b"CORP\\alice:x:5002:5002:Room: 101:/home/alice:/bin/sh").map(|a| a.uid), Some(5002));
        for bad in [
            &b""[..],
            b"\n",
            b":x:1:1::/:/bin/sh",
            b"alice:x:one:1::/:/bin/sh",
            b"alice:x:+5001:5001::/:/bin/sh",
            b"alice:x:1",
            b"alice:x:1:1",
            b"alice:x:5001:5001::/:/bin/sh\nbob:x:5002:5002::/:/bin/sh\n",
        ] {
            assert_eq!(passwd_line(bad), None, "{bad:?}");
        }
        // Never an option or delimiter to getent.
        for bad_key in [&b""[..], b"--help", b"-s", b"alice:x", b"alice\nbob"] {
            assert_eq!(getent_account(bad_key).unwrap(), None, "{bad_key:?}");
            assert_eq!(getent_group(bad_key).unwrap(), None, "{bad_key:?}");
        }
    }

    #[test]
    fn getent_lines_read_as_groups() {
        let g = Group { name: c"kiri".to_owned(), gid: 998, members: vec![c"alice".to_owned(), c"bob@corp.example".to_owned()] };
        assert_eq!(group_line(b"kiri:x:998:alice,bob@corp.example\n"), Some(g));
        assert_eq!(group_line(b"kiri:x:998:\n"), Some(Group { name: c"kiri".to_owned(), gid: 998, members: vec![] }));
        for bad in [
            &b""[..],
            b"\n",
            b":x:998:alice",
            b"kiri:x:abc:alice",
            b"kiri:x:+998:alice",
            b"kiri:x:998",
            b"kiri:x:998:alice:extra",
            b"kiri:x:998:alice\nother:x:999:bob\n",
        ] {
            assert_eq!(group_line(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn getent_initgroups_lines_match_exact_user_and_gids() {
        assert_eq!(initgroups_line(b"alice                 65200 1002\n", b"alice"), Some(vec![65200, 1002]));
        assert_eq!(initgroups_line(b"alice\t998\n", b"alice"), Some(vec![998]));
        assert_eq!(initgroups_line(b"alice                 \n", b"alice"), Some(vec![]));
        assert_eq!(initgroups_line(b"alice\n", b"alice"), Some(vec![]));
        assert_eq!(initgroups_line(b"alice@corp.example 998 44\n", b"alice@corp.example"), Some(vec![998, 44]));
        for bad in [
            &b""[..],
            b"\n",
            b"alice2 998\n",
            b"bob 998\n",
            b"alice 998 bad\n",
            b"alice +998\n",
            b"alice 998\nbob 999\n",
        ] {
            assert_eq!(initgroups_line(bad, b"alice"), None, "{bad:?}");
        }
    }

    #[test]
    fn getent_process_handles_status_codes_and_invalid_keys() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kiri-getent-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prog = dir.join("fake-getent");
        let touched = dir.join("touched");
        let script = format!(
            "#!/bin/sh\n\
             [ \"$1\" = \"--\" ] || exit 9\n\
             shift\n\
             db=\"$1\"; key=\"$2\"\n\
             case \"$key\" in -*) echo bad > \"{}\"; exit 9 ;; esac\n\
             case \"$db:$key\" in\n\
               passwd:alice) echo 'alice:x:5001:5001:Alice:/home/alice:/bin/sh'; exit 0 ;;\n\
               passwd:5001) echo 'alice:x:5001:5001:Alice:/home/alice:/bin/sh'; exit 0 ;;\n\
               passwd:0) echo 'root:x:0:0:root:/root:/bin/sh'; exit 0 ;;\n\
               passwd:badline) echo 'not-a-passwd-line'; exit 0 ;;\n\
               passwd:err) exit 3 ;;\n\
               group:dirkiri) echo 'dirkiri:x:65200:alice,localbob'; exit 0 ;;\n\
               group:badline) echo 'bad:group'; exit 0 ;;\n\
               group:err) exit 1 ;;\n\
               initgroups:alice) echo 'alice                 65200 1002'; exit 0 ;;\n\
               initgroups:outsider) echo 'outsider              2000'; exit 0 ;;\n\
               initgroups:unsupported) exit 1 ;;\n\
               initgroups:badline) echo 'alice2 1002'; exit 0 ;;\n\
               initgroups:err) exit 3 ;;\n\
               *) exit 2 ;;\n\
             esac\n",
            touched.display()
        );
        std::fs::write(&prog, script).unwrap();
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();

        let alice = Account { name: c"alice".to_owned(), uid: 5001, gid: 5001 };
        assert_eq!(getent_account_with(&prog, b"alice").unwrap(), Some(alice));
        assert_eq!(getent_account_with(&prog, b"missing").unwrap(), None);
        assert_eq!(getent_account_with(&prog, b"badline").unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(getent_account_with(&prog, b"err").is_err());
        // Numeric username returning a different account name is rejected by the caller's exact check.
        assert_eq!(getent_account_with(&prog, b"0").unwrap().filter(|a| a.name.as_c_str() == c"0"), None);

        let dirkiri = Group { name: c"dirkiri".to_owned(), gid: 65200, members: vec![c"alice".to_owned(), c"localbob".to_owned()] };
        assert_eq!(getent_group_with(&prog, b"dirkiri").unwrap(), Some(dirkiri));
        assert_eq!(getent_group_with(&prog, b"missing").unwrap(), None);
        assert_eq!(getent_group_with(&prog, b"badline").unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(getent_group_with(&prog, b"err").is_err());

        assert!(initgroups_has_with(&prog, c"alice", 1002).unwrap());
        assert!(!initgroups_has_with(&prog, c"alice", 9999).unwrap());
        assert!(!initgroups_has_with(&prog, c"outsider", 1002).unwrap());
        assert!(!initgroups_has_with(&prog, c"unsupported", 1002).unwrap());
        assert!(!initgroups_has_with(&prog, c"missing", 1002).unwrap());
        assert_eq!(initgroups_has_with(&prog, c"badline", 1002).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(initgroups_has_with(&prog, c"err", 1002).is_err());

        // Option-like keys never invoke the helper.
        assert_eq!(getent_account_with(&prog, b"-s").unwrap(), None);
        assert_eq!(getent_group_with(&prog, b"--help").unwrap(), None);
        assert!(!initgroups_has_with(&prog, c"-s", 1002).unwrap());
        assert!(!touched.exists());

        // Missing binary counts as no entry rather than an error.
        let missing = dir.join("no-such-getent");
        assert_eq!(getent_account_with(&missing, b"alice").unwrap(), None);
        assert_eq!(getent_group_with(&missing, b"dirkiri").unwrap(), None);
        assert!(!initgroups_has_with(&missing, c"alice", 1002).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn member_and_uids_enforce_configured_group_only() {
        let group = Group { name: c"kiri".to_owned(), gid: 998, members: vec![c"local".to_owned(), c"alice".to_owned(), c"ghost".to_owned()] };
        let accounts = |uid: u32| -> io::Result<Option<Account>> {
            Ok(match uid {
                1001 => Some(Account { name: c"local".to_owned(), uid: 1001, gid: 1001 }),
                5001 => Some(Account { name: c"alice".to_owned(), uid: 5001, gid: 5001 }),
                5002 => Some(Account { name: c"dirgroup".to_owned(), uid: 5002, gid: 5002 }),
                5003 => Some(Account { name: c"outsider".to_owned(), uid: 5003, gid: 5003 }),
                5004 => Some(Account { name: c"primary".to_owned(), uid: 5004, gid: 998 }),
                _ => None,
            })
        };
        let initgroups = |user: &CStr, gid: u32| -> io::Result<bool> { Ok(user == c"dirgroup" && gid == 998) };

        assert!(member_of_with(1001, Some(&group), accounts, initgroups).unwrap());
        assert!(member_of_with(5001, Some(&group), accounts, initgroups).unwrap());
        assert!(member_of_with(5002, Some(&group), accounts, initgroups).unwrap());
        assert!(member_of_with(5004, Some(&group), accounts, initgroups).unwrap());
        assert!(!member_of_with(5003, Some(&group), accounts, initgroups).unwrap(), "non-member refused");
        assert!(!member_of_with(7777, Some(&group), accounts, initgroups).unwrap(), "unknown uid refused");
        assert!(!member_of_with(5001, None, accounts, initgroups).unwrap(), "deleted group admits nobody");

        // uids_of resolves directory members, omits missing accounts, and fails closed on errors.
        let by_name = |name: &CStr| -> io::Result<Option<Account>> {
            Ok(match name.to_bytes() {
                b"local" => Some(Account { name: c"local".to_owned(), uid: 1001, gid: 1001 }),
                b"alice" => Some(Account { name: c"alice".to_owned(), uid: 5001, gid: 5001 }),
                _ => None,
            })
        };
        assert_eq!(uids_of_with(&group, by_name).unwrap(), vec![1001, 5001]);
        assert!(uids_of_with(&group, |_| Err(io::Error::other("directory unavailable"))).is_err());
    }

    fn write_test_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn make_temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("kiri-access-{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn getent_runs_with_clean_environment() {
        let dir = make_temp_dir("env");
        let env_passwd = dir.join("env-passwd");
        let env_init = dir.join("env-init");
        let prog = write_test_script(
            &dir,
            "fake-getent",
            &format!(
                "#!/bin/sh\n\
                 [ \"$1\" = \"--\" ] || exit 9\n\
                 shift\n\
                 case \"$1\" in\n\
                   passwd) export -p > \"{}\"; echo 'alice:x:5001:5001::/:/bin/sh'; exit 0 ;;\n\
                   initgroups) export -p > \"{}\"; echo 'alice 998'; exit 0 ;;\n\
                   *) exit 2 ;;\n\
                 esac\n",
                env_passwd.display(),
                env_init.display(),
            ),
        );

        assert!(getent_account_with(&prog, b"alice").unwrap().is_some());
        assert!(initgroups_has_with(&prog, c"alice", 998).unwrap());

        for dump in [&env_passwd, &env_init] {
            let text = std::fs::read_to_string(dump).unwrap();
            let mut saw_lc_all = false;
            for line in text.lines() {
                let rest = line.strip_prefix("export ").unwrap_or(line);
                let (k, v) = rest.split_once('=').unwrap_or((rest, ""));
                if k == "LC_ALL" {
                    assert!(v == "'C'" || v == "C" || v == "\"C\"", "unexpected LC_ALL: {line}");
                    saw_lc_all = true;
                } else {
                    assert!(matches!(k, "PWD" | "SHLVL" | "_"), "unexpected inherited env var in getent: {line}");
                }
            }
            assert!(saw_lc_all, "LC_ALL=C not set in {text}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn getent_times_out_and_caps_stdout() {
        let dir = make_temp_dir("limits");
        let prog = write_test_script(
            &dir,
            "fake-getent",
            "#!/bin/sh\n\
             [ \"$1\" = \"--\" ] || exit 9\n\
             shift\n\
             case \"$2\" in\n\
               hang) [ -x /bin/sleep ] && /bin/sleep 30 || /usr/bin/sleep 30; exit 0 ;;\n\
               hang_closed) exec >&-; [ -x /bin/sleep ] && /bin/sleep 30 || /usr/bin/sleep 30; exit 0 ;;\n\
               flood)\n\
                 line='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n\
                 i=0; while [ \"$i\" -lt 64 ]; do printf '%s\\n' \"$line\"; i=$((i+1)); done\n\
                 exit 0 ;;\n\
               *) exit 2 ;;\n\
             esac\n",
        );

        let limit = Duration::from_millis(150);
        let t0 = Instant::now();
        let err = getent_account_with_limits(&prog, b"hang", limit, GETENT_MAX_STDOUT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(t0.elapsed() < Duration::from_secs(3), "hung passwd lookup took {:?}", t0.elapsed());

        let t1 = Instant::now();
        let err = initgroups_has_with_limits(&prog, c"hang", 998, limit, GETENT_MAX_STDOUT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(t1.elapsed() < Duration::from_secs(3), "hung initgroups lookup took {:?}", t1.elapsed());

        let t2 = Instant::now();
        let err = getent_account_with_limits(&prog, b"hang_closed", limit, GETENT_MAX_STDOUT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(t2.elapsed() < Duration::from_secs(3), "closed-stdout hang took {:?}", t2.elapsed());

        let err = getent_account_with_limits(&prog, b"flood", Duration::from_secs(3), 256).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_by_name_prefers_getent_for_nss_merge_and_override() {
        let local = Group { name: c"kiri".to_owned(), gid: 100, members: vec![c"local".to_owned()] };
        let merged = Group { name: c"kiri".to_owned(), gid: 100, members: vec![c"local".to_owned(), c"diralice".to_owned()] };
        let overridden = Group { name: c"kiri".to_owned(), gid: 200, members: vec![c"sssonly".to_owned()] };

        // Merged local + NSS members are preserved when getent is available.
        let g = group_by_name_with(c"kiri", |_| Ok(Some(LookupResult::Found(merged.clone()))), |_| Ok(Some(local.clone()))).unwrap();
        assert_eq!(g, LookupResult::Found(merged));

        // NSS-first GID and membership override /etc/group when getent is available.
        let g = group_by_name_with(c"kiri", |_| Ok(Some(LookupResult::Found(overridden.clone()))), |_| Ok(Some(local.clone()))).unwrap();
        assert_eq!(g, LookupResult::Found(overridden));

        // Exit 2 from getent does not silently override NSS policy with /etc/group.
        let g = group_by_name_with(c"kiri", |_| Ok(Some(LookupResult::UnavailableOrNotFound)), |_| Ok(Some(local.clone()))).unwrap();
        assert_eq!(g, LookupResult::UnavailableOrNotFound);

        // Only when no getent binary exists do we fall back to static getgrnam_r.
        let g = group_by_name_with(c"kiri", |_| Ok(None), |_| Ok(Some(local.clone()))).unwrap();
        assert_eq!(g, LookupResult::Found(local));
    }

    #[test]
    fn canonical_directory_names_accepted_while_numeric_keys_require_exact_match() {
        let dir = make_temp_dir("canon");
        let prog = write_test_script(
            &dir,
            "fake-getent",
            "#!/bin/sh\n\
             [ \"$1\" = \"--\" ] || exit 9\n\
             shift\n\
             case \"$1:$2\" in\n\
               passwd:alice|passwd:Alice@corp.example) echo 'alice@corp.example:x:5001:5001::/home/alice:/bin/sh'; exit 0 ;;\n\
               passwd:0|'passwd:+0'|'passwd: 0'|'passwd: -0') echo 'root:x:0:0:root:/root:/bin/sh'; exit 0 ;;\n\
               group:DevOps) echo 'devops@corp.example:x:65300:alice@corp.example'; exit 0 ;;\n\
               group:0|'group:+0'|'group: 0') echo 'root:x:0:'; exit 0 ;;\n\
               *) exit 2 ;;\n\
             esac\n",
        );

        let canonical = Account { name: c"alice@corp.example".to_owned(), uid: 5001, gid: 5001 };
        for query in [&b"alice"[..], b"Alice@corp.example"] {
            let got = getent_account_with(&prog, query).unwrap().and_then(|a| match_account_name(query, a));
            assert_eq!(got, Some(canonical.clone()), "{query:?}");
        }
        for numeric in [&b"0"[..], b"+0", b" 0", b" -0"] {
            assert!(is_numeric_key(numeric), "{numeric:?}");
            let got = getent_account_with(&prog, numeric).unwrap().and_then(|a| match_account_name(numeric, a));
            assert_eq!(got, None, "{numeric:?} must not resolve to root");
        }
        assert!(is_numeric_key(b"\t+42"));
        assert!(!is_numeric_key(b"123alice"));
        assert!(!is_numeric_key(b"   "));

        let devops = Group { name: c"devops@corp.example".to_owned(), gid: 65300, members: vec![c"alice@corp.example".to_owned()] };
        let g = group_by_name_with(c"DevOps", |k| getent_group_lookup_with(&prog, k), |_| Ok(None)).unwrap();
        assert_eq!(g, LookupResult::Found(devops));
        for numeric in [c"0", c"+0", c" 0"] {
            let g = group_by_name_with(numeric, |k| getent_group_lookup_with(&prog, k), |_| Ok(None)).unwrap();
            assert_eq!(g, LookupResult::NotFound, "{numeric:?} must not resolve to group root");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn initgroups_parses_signed_high_gids_and_rejects_malformed() {
        assert_eq!(
            initgroups_line(b"alice -2147483648 -1 -1000 998\n", b"alice"),
            Some(vec![2_147_483_648, u32::MAX, 4_294_966_296, 998])
        );
        assert_eq!(initgroups_line(b"alice 2147483648 998\n", b"alice"), Some(vec![2_147_483_648, 998]));
        for bad in [&b"alice -0\n"[..], b"alice -\n", b"alice --1\n", b"alice -2147483649\n", b"alice +998\n"] {
            assert_eq!(initgroups_line(bad, b"alice"), None, "{bad:?}");
        }

        let dir = make_temp_dir("highgid");
        let prog = write_test_script(
            &dir,
            "fake-getent",
            "#!/bin/sh\n\
             [ \"$1\" = \"--\" ] || exit 9\n\
             shift\n\
             case \"$1:$2\" in\n\
               initgroups:alice) echo 'alice -2147483648 -1000 998'; exit 0 ;;\n\
               *) exit 2 ;;\n\
             esac\n",
        );
        assert!(initgroups_has_with(&prog, c"alice", 998).unwrap());
        assert!(initgroups_has_with(&prog, c"alice", 2_147_483_648).unwrap());
        assert!(!initgroups_has_with(&prog, c"alice", 1234).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uids_of_batches_directory_lookups_and_caps_oversized_groups() {
        let dir = make_temp_dir("batch");
        let calls = dir.join("calls");
        let prog = write_test_script(
            &dir,
            "fake-getent",
            &format!(
                "#!/bin/sh\n\
                 [ \"$1\" = \"--\" ] || exit 9\n\
                 shift\n\
                 db=\"$1\"; shift\n\
                 echo x >> \"{}\"\n\
                 [ \"$db\" = \"passwd\" ] || exit 2\n\
                 rc=0\n\
                 for key in \"$@\"; do\n\
                   case \"$key\" in\n\
                     dir1) echo 'dir1:x:5001:5001::/:/bin/sh' ;;\n\
                     dir2) echo 'dir2:x:5002:5002::/:/bin/sh' ;;\n\
                     dir3) echo 'dir3:x:5003:5003::/:/bin/sh' ;;\n\
                     dir4) echo 'dir4:x:5004:5004::/:/bin/sh' ;;\n\
                     dir5) echo 'dir5:x:5005:5005::/:/bin/sh' ;;\n\
                     *) rc=2 ;;\n\
                   esac\n\
                 done\n\
                 exit \"$rc\"\n",
                calls.display(),
            ),
        );

        let group = Group {
            name: c"kiri".to_owned(),
            gid: 998,
            members: vec![c"local".to_owned(), c"dir1".to_owned(), c"dir2".to_owned(), c"dir3".to_owned(), c"dir4".to_owned(), c"dir5".to_owned(), c"ghost".to_owned()],
        };
        let resolved = resolve_members_with(
            &group,
            |n| Ok((n == c"local").then(|| Account { name: c"local".to_owned(), uid: 1001, gid: 1001 })),
            |chunk| getent_accounts_batch_with(&prog, chunk, GETENT_TIMEOUT, GETENT_MAX_STDOUT),
            |n| getent_account_lookup_with(&prog, n.to_bytes(), GETENT_TIMEOUT, GETENT_MAX_STDOUT),
        )
        .unwrap();
        assert_eq!(resolved.uids, vec![1001, 5001, 5002, 5003, 5004, 5005]);
        assert_eq!(resolved.directory_members, 5);
        assert_eq!(resolved.unresolved, 1);
        assert!(!resolved.oversized);
        // All 6 directory names were queried in a single batched `getent passwd` call.
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);

        // Groups exceeding MAX_ACL_MEMBERS skip lookups completely and flag oversized.
        let huge = Group { name: c"corp".to_owned(), gid: 998, members: (0..=MAX_ACL_MEMBERS).map(|i| CString::new(format!("u{i}")).unwrap()).collect() };
        let batch_calls = AtomicU64::new(0);
        let resolved = resolve_members_with(
            &huge,
            |_| {
                batch_calls.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            },
            |_| {
                batch_calls.fetch_add(1, Ordering::Relaxed);
                Ok(BatchAccounts::default())
            },
            |_| {
                batch_calls.fetch_add(1, Ordering::Relaxed);
                Ok(LookupResult::NotFound)
            },
        )
        .unwrap();
        assert!(resolved.oversized);
        assert!(resolved.uids.is_empty());
        assert_eq!(batch_calls.load(Ordering::Relaxed), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tick_stops_retrying_on_acl_size_errors_and_clears_stale_acl() {
        let g = gate();
        let mut state = AclState::default();
        let t0 = Instant::now();
        let stamp = Some(FileStamp { dev: 1, ino: 1, len: 10, mtime_sec: 1, mtime_nsec: 0 });
        let grp_big = Group { name: c"kiri".to_owned(), gid: 998, members: vec![c"u1".to_owned(), c"u2".to_owned(), c"u3".to_owned()] };
        let grp_small = Group { name: c"kiri".to_owned(), gid: 998, members: vec![c"u1".to_owned()] };

        let resolve_calls = AtomicU64::new(0);
        let writes: Mutex<Vec<Vec<u32>>> = Mutex::default();

        // Tick 1: set_all fails with ENOSPC -> clears stale ACL (&[]) and stops retrying.
        g.tick_with(
            &mut state,
            t0,
            stamp,
            || Ok(LookupResult::Found(grp_big.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| {
                resolve_calls.fetch_add(1, Ordering::Relaxed);
                Ok(ResolvedMembers { uids: vec![5001, 5002, 5003], directory_members: 3, unresolved: 0, oversized: false })
            },
            |uids| {
                writes.lock().unwrap().push(uids.to_vec());
                if uids.len() > 2 { Err(io::Error::from_raw_os_error(libc::ENOSPC)) } else { Ok(()) }
            },
        );
        assert_eq!(resolve_calls.load(Ordering::Relaxed), 1);
        assert_eq!(*writes.lock().unwrap(), vec![vec![5001, 5002, 5003], vec![]]);

        // Tick 2 (even after directory refresh interval): unchanged member list does NOT re-resolve or retry.
        g.tick_with(
            &mut state,
            t0 + IDLE_DIRECTORY_REFRESH + Duration::from_secs(1),
            stamp,
            || Ok(LookupResult::Found(grp_big.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| {
                resolve_calls.fetch_add(1, Ordering::Relaxed);
                Ok(ResolvedMembers { uids: vec![5001, 5002, 5003], directory_members: 3, unresolved: 0, oversized: false })
            },
            |uids| {
                writes.lock().unwrap().push(uids.to_vec());
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            },
        );
        assert_eq!(resolve_calls.load(Ordering::Relaxed), 1, "must not repeat lookups on ENOSPC");
        assert_eq!(writes.lock().unwrap().len(), 2);

        // Tick 3: group shrinks -> retries and succeeds.
        let stamp2 = Some(FileStamp { dev: 1, ino: 1, len: 5, mtime_sec: 2, mtime_nsec: 0 });
        g.tick_with(
            &mut state,
            t0 + IDLE_DIRECTORY_REFRESH + Duration::from_secs(3),
            stamp2,
            || Ok(LookupResult::Found(grp_small.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| {
                resolve_calls.fetch_add(1, Ordering::Relaxed);
                Ok(ResolvedMembers { uids: vec![5001], directory_members: 1, unresolved: 0, oversized: false })
            },
            |uids| {
                writes.lock().unwrap().push(uids.to_vec());
                Ok(())
            },
        );
        assert_eq!(resolve_calls.load(Ordering::Relaxed), 2);
        assert_eq!(writes.lock().unwrap().last().unwrap(), &vec![5001]);
    }

    #[test]
    fn allowed_coalesces_concurrent_lookups_and_caches_errors() {
        let g = gate();
        let calls = Arc::new(AtomicU64::new(0));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let g = g.clone();
                let calls = calls.clone();
                s.spawn(move || {
                    let yes = g
                        .allowed_with(5001, |_| {
                            calls.fetch_add(1, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(60));
                            Ok(false)
                        })
                        .unwrap();
                    assert!(!yes);
                });
            }
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1, "concurrent checks for the same uid must coalesce");

        // Lookup errors (e.g. getent timeout) are short-TTL cached too so repeated
        // connection attempts do not spawn a fork storm.
        let err_calls = AtomicU64::new(0);
        let e1 = g
            .allowed_with(5002, |_| {
                err_calls.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::TimedOut, "getent timed out"))
            })
            .unwrap_err();
        assert_eq!(e1.kind(), io::ErrorKind::TimedOut);
        let e2 = g
            .allowed_with(5002, |_| {
                err_calls.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::TimedOut, "getent timed out"))
            })
            .unwrap_err();
        assert_eq!(e2.kind(), io::ErrorKind::TimedOut);
        assert_eq!(err_calls.load(Ordering::Relaxed), 1, "cached error must avoid immediate re-lookup");
    }

    #[test]
    fn tick_skips_idle_local_group_refreshes_until_file_changes_or_interval_elapses() {
        let g = gate();
        let mut state = AclState::default();
        let t0 = Instant::now();
        let stamp1 = Some(FileStamp { dev: 1, ino: 1, len: 100, mtime_sec: 10, mtime_nsec: 0 });
        let stamp2 = Some(FileStamp { dev: 1, ino: 1, len: 110, mtime_sec: 11, mtime_nsec: 0 });
        let grp = Group { name: c"kiri".to_owned(), gid: 998, members: vec![c"local".to_owned()] };
        let group_fetches = AtomicU64::new(0);

        // Initial tick initializes local-only group state.
        g.tick_with(
            &mut state,
            t0,
            stamp1,
            || {
                group_fetches.fetch_add(1, Ordering::Relaxed);
                Ok(LookupResult::Found(grp.clone()))
            },
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 0, oversized: false }),
            |_| Ok(()),
        );
        assert_eq!(group_fetches.load(Ordering::Relaxed), 1);

        // Idle tick 2s later with unchanged /etc/group skips getent completely.
        g.tick_with(
            &mut state,
            t0 + RECHECK,
            stamp1,
            || {
                group_fetches.fetch_add(1, Ordering::Relaxed);
                Ok(LookupResult::Found(grp.clone()))
            },
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 0, oversized: false }),
            |_| Ok(()),
        );
        assert_eq!(group_fetches.load(Ordering::Relaxed), 1, "idle local tick must not fork getent");

        // Changing /etc/group triggers an immediate refresh on the next tick.
        g.tick_with(
            &mut state,
            t0 + RECHECK * 2,
            stamp2,
            || {
                group_fetches.fetch_add(1, Ordering::Relaxed);
                Ok(LookupResult::Found(grp.clone()))
            },
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 0, oversized: false }),
            |_| Ok(()),
        );
        assert_eq!(group_fetches.load(Ordering::Relaxed), 2);

        // After IDLE_LOCAL_NSS_REFRESH, an idle refresh runs even without /etc/group edits.
        let t_refresh = t0 + RECHECK * 2 + IDLE_LOCAL_NSS_REFRESH;
        g.tick_with(
            &mut state,
            t_refresh,
            stamp2,
            || {
                group_fetches.fetch_add(1, Ordering::Relaxed);
                Ok(LookupResult::Found(grp.clone()))
            },
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 0, oversized: false }),
            |_| Ok(()),
        );
        assert_eq!(group_fetches.load(Ordering::Relaxed), 3);

        // The next idle tick after the 30s refresh must NOT fetch again for another 30s.
        g.tick_with(
            &mut state,
            t_refresh + RECHECK,
            stamp2,
            || {
                group_fetches.fetch_add(1, Ordering::Relaxed);
                Ok(LookupResult::Found(grp.clone()))
            },
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 0, oversized: false }),
            |_| Ok(()),
        );
        assert_eq!(group_fetches.load(Ordering::Relaxed), 3, "tick after idle refresh must not refork getent");

        // Another full IDLE_LOCAL_NSS_REFRESH later, one refresh runs again.
        g.tick_with(
            &mut state,
            t_refresh + IDLE_LOCAL_NSS_REFRESH,
            stamp2,
            || {
                group_fetches.fetch_add(1, Ordering::Relaxed);
                Ok(LookupResult::Found(grp.clone()))
            },
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 0, oversized: false }),
            |_| Ok(()),
        );
        assert_eq!(group_fetches.load(Ordering::Relaxed), 4);

        // While a client stays connected, frequent group fetches must not starve
        // periodic directory UID refreshes (IDLE_DIRECTORY_REFRESH).
        let g_dir = gate();
        let (a, _b) = UnixStream::pair().unwrap();
        let _seat = g_dir.admit_with(&a, |_| Ok(true)).unwrap();
        let mut dir_state = AclState::default();
        let dir_grp = Group { name: c"kiri".to_owned(), gid: 998, members: vec![c"alice".to_owned()] };
        let dir_resolves = AtomicU64::new(0);
        for step in 0..=3u32 {
            g_dir.tick_with(
                &mut dir_state,
                t0 + RECHECK * step,
                stamp1,
                || Ok(LookupResult::Found(dir_grp.clone())),
                |_, _| Ok(Membership::Allowed),
                |_| {
                    dir_resolves.fetch_add(1, Ordering::Relaxed);
                    Ok(ResolvedMembers { uids: vec![5001], directory_members: 1, unresolved: 0, oversized: false })
                },
                |_| Ok(()),
            );
        }
        assert_eq!(dir_resolves.load(Ordering::Relaxed), 2, "connected 2s ticks must still refresh directory UIDs after 6s");
    }

    #[test]
    fn tick_recovers_acl_after_boot_outage_and_detects_offline_uid_changes() {
        let g = gate();
        let mut state = AclState::default();
        let t0 = Instant::now();
        let stamp = Some(FileStamp { dev: 1, ino: 1, len: 100, mtime_sec: 1, mtime_nsec: 0 });
        let grp = Group { name: c"kiri".to_owned(), gid: 998, members: vec![c"local".to_owned(), c"alice".to_owned()] };
        let writes: Mutex<Vec<Vec<u32>>> = Mutex::default();

        // 1. Boot before sssd: alice is unresolved (exit 2); local uid is written, state does not freeze.
        g.tick_with(
            &mut state,
            t0,
            stamp,
            || Ok(LookupResult::Found(grp.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 1, oversized: false }),
            |u| {
                writes.lock().unwrap().push(u.to_vec());
                Ok(())
            },
        );
        assert_eq!(writes.lock().unwrap().last().unwrap(), &vec![1001]);

        // 2. sssd comes up on the next tick: alice resolves to 5001 and ACL updates automatically.
        let t1 = t0 + RECHECK;
        g.tick_with(
            &mut state,
            t1,
            stamp,
            || Ok(LookupResult::Found(grp.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001, 5001], directory_members: 1, unresolved: 0, oversized: false }),
            |u| {
                writes.lock().unwrap().push(u.to_vec());
                Ok(())
            },
        );
        assert_eq!(writes.lock().unwrap().last().unwrap(), &vec![1001, 5001]);

        // 3. Brief transient exit-2 outage does NOT shrink the existing ACL.
        let t2 = t1 + IDLE_DIRECTORY_REFRESH;
        g.tick_with(
            &mut state,
            t2,
            stamp,
            || Ok(LookupResult::Found(grp.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 1, oversized: false }),
            |u| {
                writes.lock().unwrap().push(u.to_vec());
                Ok(())
            },
        );
        assert_eq!(state.uids, Some(vec![1001, 5001]), "single exit-2 miss must not shrink ACL");
        assert_eq!(writes.lock().unwrap().len(), 2);

        // 4. Offline UID reassignment (closed == 0, same group.members): alice now has uid 5099.
        let t3 = t2 + RECHECK;
        g.tick_with(
            &mut state,
            t3,
            stamp,
            || Ok(LookupResult::Found(grp.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001, 5099], directory_members: 1, unresolved: 0, oversized: false }),
            |u| {
                writes.lock().unwrap().push(u.to_vec());
                Ok(())
            },
        );
        assert_eq!(writes.lock().unwrap().last().unwrap(), &vec![1001, 5099]);

        // 5. Offline deletion (closed == 0): after DIRECTORY_MISS_GRACE consecutive misses, stale uid 5099 is removed.
        let mut t = t3 + IDLE_DIRECTORY_REFRESH;
        for step in 1..=DIRECTORY_MISS_GRACE {
            g.tick_with(
                &mut state,
                t,
                stamp,
                || Ok(LookupResult::Found(grp.clone())),
                |_, _| Ok(Membership::Allowed),
                |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 1, oversized: false }),
                |u| {
                    writes.lock().unwrap().push(u.to_vec());
                    Ok(())
                },
            );
            if step < DIRECTORY_MISS_GRACE {
                assert_eq!(state.uids, Some(vec![1001, 5099]), "kept during grace tick {step}");
            }
            t += RECHECK;
        }
        assert_eq!(state.uids, Some(vec![1001]), "removed after grace expires");
        assert_eq!(writes.lock().unwrap().last().unwrap(), &vec![1001]);
    }

    #[test]
    fn sweep_survives_transient_directory_exit2_and_closes_permanent_removal() {
        let g = gate();
        let (a, _b) = UnixStream::pair().unwrap();
        let _seat = g.admit_with(&a, |_| Ok(true)).unwrap();

        // Transient exit-2 misses below DIRECTORY_MISS_GRACE keep the connection open.
        for _ in 1..DIRECTORY_MISS_GRACE {
            assert!(g.sweep_membership_with(|_| Ok(Membership::MissingFromDirectory)).is_empty());
            assert_eq!(g.open.lock().unwrap().len(), 1);
        }
        // Recovery resets the miss streak.
        assert!(g.sweep_membership_with(|_| Ok(Membership::Allowed)).is_empty());
        assert_eq!(g.open.lock().unwrap().len(), 1);

        // Consecutive misses reaching DIRECTORY_MISS_GRACE close the connection.
        for _ in 1..DIRECTORY_MISS_GRACE {
            assert!(g.sweep_membership_with(|_| Ok(Membership::MissingFromDirectory)).is_empty());
        }
        let closed = g.sweep_membership_with(|_| Ok(Membership::MissingFromDirectory));
        assert_eq!(closed.len(), 1);
        assert!(g.open.lock().unwrap().is_empty());

        // Confirmed revocation closes immediately on the first tick.
        let (c, _d) = UnixStream::pair().unwrap();
        let _seat2 = g.admit_with(&c, |_| Ok(true)).unwrap();
        assert_eq!(g.sweep_membership_with(|_| Ok(Membership::Revoked)).len(), 1);
        assert!(g.open.lock().unwrap().is_empty());
    }

    #[test]
    fn allowed_wakes_distinct_waiter_when_coalesced_waiters_drain() {
        use std::sync::mpsc;

        let wait_until_waiting = |g: &Gate, target: usize| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if g.lookup.lock().unwrap().waiting >= target {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {target} lookup_slot waiters");
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        // Part 1: finish() path — with 4 active slots (5001..=5004), 5 waiters for
        // 5005 and 1 waiter for 5006 sleep on lookup_slot. Draining the coalesced
        // 5005 waiters must not leave 5006 stranded.
        let g = gate();
        let (entered_tx, entered_rx) = mpsc::channel();
        let release_active = Arc::new((Mutex::new(0u32), Condvar::new()));
        let release_5005 = Arc::new((Mutex::new(false), Condvar::new()));
        let (u5005_started_tx, u5005_started_rx) = mpsc::channel();
        let (u5006_done_tx, u5006_done_rx) = mpsc::channel();

        let mut handles = Vec::new();
        for uid in 5001..=5004u32 {
            let g = g.clone();
            let entered_tx = entered_tx.clone();
            let release_active = release_active.clone();
            handles.push(std::thread::spawn(move || {
                g.allowed_with(uid, |u| {
                    let _ = entered_tx.send(u);
                    let (lock, cv) = &*release_active;
                    let mut stage = lock.lock().unwrap();
                    while *stage < if u == 5001 { 1 } else { 2 } {
                        stage = cv.wait(stage).unwrap();
                    }
                    Ok(true)
                })
            }));
        }
        for _ in 0..4 {
            entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        }

        for _ in 0..5 {
            let g = g.clone();
            let release_5005 = release_5005.clone();
            let u5005_started_tx = u5005_started_tx.clone();
            handles.push(std::thread::spawn(move || {
                g.allowed_with(5005, |_| {
                    let _ = u5005_started_tx.send(());
                    let (lock, cv) = &*release_5005;
                    let mut go = lock.lock().unwrap();
                    while !*go {
                        go = cv.wait(go).unwrap();
                    }
                    Ok(true)
                })
            }));
        }
        wait_until_waiting(&g, 5);

        {
            let g = g.clone();
            let u5006_done_tx = u5006_done_tx.clone();
            handles.push(std::thread::spawn(move || {
                let res = g.allowed_with(5006, |_| Ok(true));
                let _ = u5006_done_tx.send(res.is_ok());
                res
            }));
        }
        wait_until_waiting(&g, 6);

        {
            let (lock, cv) = &*release_active;
            *lock.lock().unwrap() = 1;
            cv.notify_all();
        }
        u5005_started_rx.recv_timeout(Duration::from_secs(3)).unwrap();

        {
            let (lock, cv) = &*release_active;
            *lock.lock().unwrap() = 2;
            cv.notify_all();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while (g.lookup.lock().unwrap().active > 1 || g.lookup.lock().unwrap().waiting > 2) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }

        {
            let (lock, cv) = &*release_5005;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }

        let completed = u5006_done_rx.recv_timeout(Duration::from_secs(2));
        if completed.is_err() {
            g.lookup_slot.notify_all();
        }
        assert_eq!(completed.ok(), Some(true), "distinct uid 5006 waiter must not stay stranded after coalesced waiters drain");
        for h in handles {
            assert!(h.join().unwrap().unwrap());
        }
        assert_eq!(g.lookup.lock().unwrap().waiting, 0);

        // Part 2: Drop path (panic in check) also wakes distinct waiters promptly.
        let g2 = gate();
        let (e2_tx, e2_rx) = mpsc::channel();
        let rel2 = Arc::new((Mutex::new(0u32), Condvar::new()));
        let rel2_6005 = Arc::new((Mutex::new(false), Condvar::new()));
        let (s2_tx, s2_rx) = mpsc::channel();
        let (d2_tx, d2_rx) = mpsc::channel();
        let mut handles2 = Vec::new();
        for uid in 6001..=6004u32 {
            let g2 = g2.clone();
            let e2_tx = e2_tx.clone();
            let rel2 = rel2.clone();
            handles2.push(std::thread::spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = g2.allowed_with(uid, |u| {
                        let _ = e2_tx.send(u);
                        {
                            let (lock, cv) = &*rel2;
                            let mut stage = lock.lock().unwrap();
                            while *stage < if u == 6001 { 1 } else { 2 } {
                                stage = cv.wait(stage).unwrap();
                            }
                        }
                        panic!("simulated lookup panic");
                    });
                }));
            }));
        }
        for _ in 0..4 {
            e2_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        for _ in 0..5 {
            let g2 = g2.clone();
            let rel2_6005 = rel2_6005.clone();
            let s2_tx = s2_tx.clone();
            handles2.push(std::thread::spawn(move || {
                let _ = g2.allowed_with(6005, |_| {
                    let _ = s2_tx.send(());
                    let (lock, cv) = &*rel2_6005;
                    let mut go = lock.lock().unwrap();
                    while !*go {
                        go = cv.wait(go).unwrap();
                    }
                    Ok(true)
                });
            }));
        }
        wait_until_waiting(&g2, 5);
        {
            let g2 = g2.clone();
            let d2_tx = d2_tx.clone();
            handles2.push(std::thread::spawn(move || {
                let ok = g2.allowed_with(6006, |_| Ok(true)).is_ok();
                let _ = d2_tx.send(ok);
            }));
        }
        wait_until_waiting(&g2, 6);
        {
            let (lock, cv) = &*rel2;
            *lock.lock().unwrap() = 1;
            cv.notify_all();
        }
        s2_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        {
            let (lock, cv) = &*rel2;
            *lock.lock().unwrap() = 2;
            cv.notify_all();
        }
        let completed2 = d2_rx.recv_timeout(Duration::from_secs(2));
        {
            let (lock, cv) = &*rel2_6005;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        if completed2.is_err() {
            g2.lookup_slot.notify_all();
        }
        assert_eq!(completed2.ok(), Some(true), "distinct uid 6006 waiter must wake when LookupGuard drops on panic");
        for h in handles2 {
            h.join().unwrap();
        }
        assert_eq!(g2.lookup.lock().unwrap().waiting, 0);
    }

    #[test]
    fn tick_preserves_acl_grace_window_when_another_member_is_permanently_unresolved() {
        let g = gate();
        let mut state = AclState::default();
        let mut t = Instant::now();
        let stamp = Some(FileStamp { dev: 1, ino: 1, len: 100, mtime_sec: 1, mtime_nsec: 0 });
        let grp = Group {
            name: c"kiri".to_owned(),
            gid: 998,
            members: vec![c"local".to_owned(), c"alice".to_owned(), c"bob".to_owned(), c"stale".to_owned()],
        };
        let writes: Mutex<Vec<Vec<u32>>> = Mutex::default();

        // Saturate unresolved_streak with a permanently unresolved username ("stale").
        for step in 0..=DIRECTORY_MISS_GRACE {
            g.tick_with(
                &mut state,
                t,
                stamp,
                || Ok(LookupResult::Found(grp.clone())),
                |_, _| Ok(Membership::Allowed),
                |_| Ok(ResolvedMembers { uids: vec![1001, 5001, 5002], directory_members: 2, unresolved: 1, oversized: false }),
                |u| {
                    writes.lock().unwrap().push(u.to_vec());
                    Ok(())
                },
            );
            t += if step < DIRECTORY_MISS_GRACE { RECHECK } else { IDLE_DIRECTORY_REFRESH };
        }
        assert_eq!(state.uids, Some(vec![1001, 5001, 5002]));
        assert!(state.unresolved_streak >= DIRECTORY_MISS_GRACE);
        assert_eq!(state.shrink_streak, 0);
        assert_eq!(writes.lock().unwrap().len(), 1);

        // A transient 1-tick exit-2 outage for alice must still get the full grace window
        // even though stale already saturated unresolved_streak.
        g.tick_with(
            &mut state,
            t,
            stamp,
            || Ok(LookupResult::Found(grp.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001, 5002], directory_members: 1, unresolved: 2, oversized: false }),
            |u| {
                writes.lock().unwrap().push(u.to_vec());
                Ok(())
            },
        );
        assert_eq!(state.uids, Some(vec![1001, 5001, 5002]), "transient outage must not drop alice on tick 1");
        assert_eq!(state.shrink_streak, 1);
        assert_eq!(writes.lock().unwrap().len(), 1);

        // Fast 2s retry must run while inside the shrink grace window, and recovery resets it.
        t += RECHECK;
        let recovered_resolves = AtomicU64::new(0);
        g.tick_with(
            &mut state,
            t,
            stamp,
            || Ok(LookupResult::Found(grp.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| {
                recovered_resolves.fetch_add(1, Ordering::Relaxed);
                Ok(ResolvedMembers { uids: vec![1001, 5001, 5002], directory_members: 2, unresolved: 1, oversized: false })
            },
            |u| {
                writes.lock().unwrap().push(u.to_vec());
                Ok(())
            },
        );
        assert_eq!(recovered_resolves.load(Ordering::Relaxed), 1, "shrink grace must trigger fast 2s retry");
        assert_eq!(state.shrink_streak, 0);
        assert_eq!(state.uids, Some(vec![1001, 5001, 5002]));

        // A transient exit-2 on `getent group` must also preserve the ACL despite stale.
        t += IDLE_DIRECTORY_REFRESH;
        g.tick_with(
            &mut state,
            t,
            stamp,
            || Ok(LookupResult::UnavailableOrNotFound),
            |_, _| Ok(Membership::Allowed),
            |_| unreachable!(),
            |u| {
                writes.lock().unwrap().push(u.to_vec());
                Ok(())
            },
        );
        assert_eq!(state.uids, Some(vec![1001, 5001, 5002]), "transient group exit-2 must not wipe ACL");
        assert_eq!(state.shrink_streak, 1);
        t += RECHECK;
        g.tick_with(
            &mut state,
            t,
            stamp,
            || Ok(LookupResult::Found(grp.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001, 5001, 5002], directory_members: 2, unresolved: 1, oversized: false }),
            |u| {
                writes.lock().unwrap().push(u.to_vec());
                Ok(())
            },
        );
        assert_eq!(state.shrink_streak, 0);

        // Permanent removal of alice removes 5001 after DIRECTORY_MISS_GRACE consecutive ticks,
        // and resets shrink grace so a subsequent transient miss for bob still gets grace.
        t += IDLE_DIRECTORY_REFRESH;
        for step in 1..=DIRECTORY_MISS_GRACE {
            g.tick_with(
                &mut state,
                t,
                stamp,
                || Ok(LookupResult::Found(grp.clone())),
                |_, _| Ok(Membership::Allowed),
                |_| Ok(ResolvedMembers { uids: vec![1001, 5002], directory_members: 1, unresolved: 2, oversized: false }),
                |u| {
                    writes.lock().unwrap().push(u.to_vec());
                    Ok(())
                },
            );
            if step < DIRECTORY_MISS_GRACE {
                assert_eq!(state.uids, Some(vec![1001, 5001, 5002]), "alice kept during grace tick {step}");
            }
            t += RECHECK;
        }
        assert_eq!(state.uids, Some(vec![1001, 5002]), "alice dropped after grace expires");
        assert_eq!(state.shrink_streak, 0);

        // Next refresh: transient 1-tick miss for bob must still get its own grace window.
        t += IDLE_DIRECTORY_REFRESH;
        g.tick_with(
            &mut state,
            t,
            stamp,
            || Ok(LookupResult::Found(grp.clone())),
            |_, _| Ok(Membership::Allowed),
            |_| Ok(ResolvedMembers { uids: vec![1001], directory_members: 0, unresolved: 3, oversized: false }),
            |u| {
                writes.lock().unwrap().push(u.to_vec());
                Ok(())
            },
        );
        assert_eq!(state.uids, Some(vec![1001, 5002]), "bob must get a fresh grace window after alice was dropped");
        assert_eq!(state.shrink_streak, 1);
    }
}
