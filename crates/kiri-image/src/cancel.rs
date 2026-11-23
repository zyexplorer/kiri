//! Stopping a pull from another thread: the window's Cancel, or every
//! `docker pull` waiting on it having hung up. Cancelling shuts down the
//! registry connections the pull is reading (mid-read too, so a stalled
//! CDN doesn't hold it for a minute), and the pull checks between chunks
//! and layers, ending with [`crate::registry::Error::Cancelled`] before it
//! records anything. Blobs it finished stay in the store for the next pull;
//! a half-downloaded one is deleted with its writer.

use std::cell::RefCell;
use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// A pull's cancel switch. Clones share it.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<Inner>);

#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    /// The connections the pull's threads have read from, to shut down.
    streams: Mutex<Vec<Weak<TcpStream>>>,
}

thread_local! {
    /// The pull this thread is working for, if any (see [`Cancel::enter`]).
    static CURRENT: RefCell<Option<Cancel>> = const { RefCell::new(None) };
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    /// Stops the pull: its connections are shut down now, and it ends at
    /// its next check. Cancelling twice is harmless.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        let streams = std::mem::take(&mut *self.0.streams.lock().unwrap_or_else(|e| e.into_inner()));
        for s in streams.iter().filter_map(Weak::upgrade) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Makes registry connections used on this thread answer to this switch
    /// until the guard is dropped.
    pub(crate) fn enter(&self) -> Entered {
        let before = CURRENT.with(|c| c.replace(Some(self.clone())));
        Entered(before)
    }

    /// Sleeps `d` (a retry's back-off), waking early if cancelled.
    pub(crate) fn sleep(&self, d: Duration) {
        let end = Instant::now() + d;
        while !self.is_cancelled() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }

    /// Notes `stream` as one to shut down on cancel, or refuses it if the
    /// pull is already cancelled. Checked under the lock [`Cancel::cancel`]
    /// takes, so a connection is either refused or shut down, never missed.
    fn watch(&self, stream: &Arc<TcpStream>) -> io::Result<()> {
        let mut streams = self.0.streams.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_cancelled() {
            return Err(io::Error::other("pull cancelled"));
        }
        streams.retain(|s| s.strong_count() > 0);
        if !streams.iter().any(|s| std::ptr::eq(s.as_ptr(), Arc::as_ptr(stream))) {
            streams.push(Arc::downgrade(stream));
        }
        Ok(())
    }
}

/// See [`Cancel::enter`]; puts back what the thread had before.
pub(crate) struct Entered(Option<Cancel>);

impl Drop for Entered {
    fn drop(&mut self) {
        let before = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = before);
    }
}

/// Called by a connection before each read and write: ties it to the pull
/// this thread works for, if one, and fails once that is cancelled.
pub(crate) fn watch(stream: &Arc<TcpStream>) -> io::Result<()> {
    CURRENT.with(|c| match &*c.borrow() {
        Some(cancel) => cancel.watch(stream),
        None => Ok(()),
    })
}

/// Whether the pull this thread works for, if one, has been cancelled.
pub(crate) fn current_cancelled() -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(Cancel::is_cancelled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn cancel_ends_a_read_that_is_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = Arc::new(TcpStream::connect(listener.local_addr().unwrap()).unwrap());
        // The server says nothing: the read would wait for ever.
        let (_server, _) = listener.accept().unwrap();
        let cancel = Cancel::new();
        let reader = {
            let (cancel, client) = (cancel.clone(), client.clone());
            std::thread::spawn(move || {
                let _on = cancel.enter();
                watch(&client).unwrap();
                let started = Instant::now();
                let n = (&*client).read(&mut [0u8; 16]).unwrap_or(0);
                (n, started.elapsed())
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        cancel.cancel();
        let (n, waited) = reader.join().unwrap();
        assert_eq!(n, 0);
        assert!(waited < Duration::from_secs(2), "{waited:?}");
        // Once cancelled, a connection is refused outright.
        let _on = cancel.enter();
        assert!(watch(&client).is_err());
    }

    #[test]
    fn a_thread_answers_only_to_the_pull_it_entered() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = Arc::new(TcpStream::connect(listener.local_addr().unwrap()).unwrap());
        let (a, b) = (Cancel::new(), Cancel::new());
        a.cancel();
        assert!(watch(&client).is_ok(), "no pull entered");
        {
            let _on = b.enter();
            assert!(watch(&client).is_ok());
            {
                let _inner = a.enter();
                assert!(watch(&client).is_err());
            }
            assert!(watch(&client).is_ok(), "back to b");
        }
        assert!(watch(&client).is_ok());
    }

    #[test]
    fn sleep_wakes_on_cancel() {
        let cancel = Cancel::new();
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            c.cancel();
        });
        let started = Instant::now();
        cancel.sleep(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
