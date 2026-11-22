//! Containers' root filesystems: one overlay mount per container, shared by
//! everything that needs it at once (the container while it runs, `docker
//! cp`, export) and unmounted when the last of them lets go.
//!
//! Two overlay mounts of one writable layer at the same time are undefined
//! behaviour to the kernel, and a copy into a stopped container used to make
//! its own while the container started (#227). Only the agent mounts roots;
//! a copy child asks it for one (see `copy`). The agent is single-threaded,
//! so the table needs no lock.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::container::{self, Paths};
use super::sys;

#[derive(Debug, Default, PartialEq)]
struct Root {
    users: u32,
    /// Removed (`docker rm`, `--rm`) while in use: its files go with the
    /// last user.
    remove: bool,
}

/// What letting go of a root leaves to do.
#[derive(Debug, PartialEq)]
enum Left {
    InUse,
    Unmount { remove: bool },
}

#[derive(Default)]
struct Table(HashMap<PathBuf, Root>);

impl Table {
    /// True when this is the first user, who must mount it.
    fn take(&mut self, base: &Path) -> bool {
        let r = self.0.entry(base.to_owned()).or_default();
        r.users += 1;
        r.users == 1
    }

    /// One more user of a root already mounted; false if it isn't.
    fn hold(&mut self, base: &Path) -> bool {
        match self.0.get_mut(base) {
            Some(r) if r.users > 0 => {
                r.users += 1;
                true
            }
            _ => false,
        }
    }

    /// Undoes a `take` whose mount failed.
    fn forget(&mut self, base: &Path) {
        if let Some(r) = self.0.get_mut(base) {
            r.users = r.users.saturating_sub(1);
            if r.users == 0 && !r.remove {
                self.0.remove(base);
            }
        }
    }

    fn give(&mut self, base: &Path) -> Option<Left> {
        let r = self.0.get_mut(base)?;
        r.users = r.users.saturating_sub(1);
        if r.users > 0 {
            return Some(Left::InUse);
        }
        let remove = r.remove;
        self.0.remove(base);
        Some(Left::Unmount { remove })
    }

    /// True when nobody uses it and its files can go now.
    fn doom(&mut self, base: &Path) -> bool {
        match self.0.get_mut(base) {
            Some(r) if r.users > 0 => {
                r.remove = true;
                false
            }
            _ => {
                self.0.remove(base);
                true
            }
        }
    }
}

thread_local! {
    static TABLE: RefCell<Table> = RefCell::new(Table::default());
}

/// The container's mounted root, mounting it if nobody has yet.
pub fn acquire(paths: &Paths, id: &str, layers: &[&str]) -> io::Result<PathBuf> {
    container::valid_id(id)?;
    let base = paths.containers.join(id);
    if !TABLE.with_borrow_mut(|t| t.take(&base)) {
        return Ok(base.join("rootfs"));
    }
    container::mount_rootfs(paths, id, layers).inspect_err(|_| TABLE.with_borrow_mut(|t| t.forget(&base)))
}

/// Holds a root that is mounted now (a running container's), for a copy
/// into it; an error when it isn't mounted.
pub fn hold(paths: &Paths, id: &str) -> io::Result<PathBuf> {
    container::valid_id(id)?;
    let base = paths.containers.join(id);
    if TABLE.with_borrow_mut(|t| t.hold(&base)) {
        Ok(base.join("rootfs"))
    } else {
        Err(io::Error::new(io::ErrorKind::NotFound, "not mounted"))
    }
}

/// One user is done with the root at `<containers>/<id>`; the last one
/// unmounts it, and deletes the container's files if it was removed.
pub fn release(base: &Path) {
    match TABLE.with_borrow_mut(|t| t.give(base)) {
        None | Some(Left::InUse) => {}
        Some(Left::Unmount { remove }) => {
            unmount(&base.join("rootfs"));
            if remove {
                let _ = fs::remove_dir_all(base);
            }
        }
    }
}

/// Deletes a container's files now, or when the last user of its root lets
/// go.
pub fn remove(base: &Path) {
    if TABLE.with_borrow_mut(|t| t.doom(base)) {
        let _ = sys::umount_lazy(&base.join("rootfs"));
        let _ = fs::remove_dir_all(base);
    }
}

/// Unmounts at once when nothing holds the mount, so the next mount of the
/// same layer never overlaps one still being let go; lazily otherwise.
fn unmount(rootfs: &Path) {
    let Ok(t) = sys::cstr(rootfs) else { return };
    // SAFETY: `t` is a valid NUL-terminated string.
    if unsafe { libc::umount2(t.as_ptr(), 0) } != 0 {
        let _ = sys::umount_lazy(rootfs);
    }
}

#[cfg(test)]
mod tests {
    use super::{Left, Table};
    use std::path::Path;

    #[test]
    fn the_first_user_mounts_and_the_last_unmounts() {
        let (mut t, a) = (Table::default(), Path::new("/c/a"));
        assert!(t.take(a), "the container starts: it mounts");
        assert!(!t.take(a), "a copy meanwhile uses the same mount");
        assert_eq!(t.give(a), Some(Left::InUse), "the copy ends: still running");
        assert_eq!(t.give(a), Some(Left::Unmount { remove: false }), "the container exits");
        assert_eq!(t.give(a), None, "nothing left to let go");
        assert!(t.take(a), "a later start mounts again");
    }

    #[test]
    fn a_root_removed_in_use_goes_with_its_last_user() {
        let (mut t, a) = (Table::default(), Path::new("/c/a"));
        assert!(t.doom(a), "not in use: removed now");
        t.take(a);
        t.take(a);
        assert!(!t.doom(a));
        assert_eq!(t.give(a), Some(Left::InUse));
        assert_eq!(t.give(a), Some(Left::Unmount { remove: true }));
    }

    #[test]
    fn a_copy_into_a_running_container_keeps_its_root_past_a_restart() {
        let (mut t, a) = (Table::default(), Path::new("/c/a"));
        assert!(!t.hold(a), "nothing mounted: nothing to hold");
        t.take(a);
        assert!(t.hold(a));
        assert_eq!(t.give(a), Some(Left::InUse), "the container exits: the copy still writes");
        assert!(!t.take(a), "the restart shares it instead of mounting again");
        assert_eq!(t.give(a), Some(Left::InUse));
        assert_eq!(t.give(a), Some(Left::Unmount { remove: false }));
    }

    #[test]
    fn a_failed_mount_is_not_counted() {
        let (mut t, a) = (Table::default(), Path::new("/c/a"));
        assert!(t.take(a));
        t.forget(a);
        assert!(t.take(a), "the next user mounts");
    }
}
