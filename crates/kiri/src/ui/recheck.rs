//! Asking for a newer release again while Kiri keeps running. The window
//! checks when it opens; a laptop that stays up for days, sleeping and
//! waking with Kiri open, would otherwise never hear of a release until
//! Kiri was quit. Every 6 hours by the wall clock, so time asleep counts:
//! a Mac that wakes after a night checks within minutes. The Mac and Linux
//! (ui/mod.rs) and Windows (kiri-win's update.rs, which includes this file)
//! use it the same way.

use std::sync::{Mutex, Once, PoisonError};
use std::time::{Duration, SystemTime};

/// How long after one check the next is due.
pub const EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How often the timer looks at the clock: a monotonic sleep stops while
/// the computer sleeps, so it wakes often and compares wall-clock times.
const TICK: Duration = Duration::from_secs(10 * 60);

static LAST: Mutex<Option<SystemTime>> = Mutex::new(None);

/// A check asked the feed just now.
pub fn checked() {
    *LAST.lock().unwrap_or_else(PoisonError::into_inner) = Some(SystemTime::now());
}

fn due(last: Option<SystemTime>, now: SystemTime) -> bool {
    match last {
        None => true,
        // A clock set back: check, and count from now.
        Some(l) => now.duration_since(l).map_or(true, |d| d >= EVERY),
    }
}

/// Starts the timer once for the life of the process (a page reload asks
/// again): `check` runs whenever a check is due.
pub fn start(check: fn()) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("kiri-recheck".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    let last = *LAST.lock().unwrap_or_else(PoisonError::into_inner);
                    if due(last, SystemTime::now()) {
                        checked();
                        check();
                    }
                }
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_six_hours_after_the_last_check_by_the_wall_clock() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(due(None, t));
        assert!(!due(Some(t), t + Duration::from_secs(5 * 3600)));
        assert!(due(Some(t), t + EVERY));
        // Asleep all night: due the moment it wakes.
        assert!(due(Some(t), t + Duration::from_secs(14 * 3600)));
        // The clock went back.
        assert!(due(Some(t), t - Duration::from_secs(60)));
    }
}
