//! `docker pull`s under way, shared as dockerd shares them: a client asking
//! for an image that is already being pulled (same platform, same sign-in)
//! follows that pull instead of starting another, from the lines so far.
//! A pull every client has left (Ctrl-C on `docker pull`, Cancel in the
//! window) is cancelled: its downloads stop and no image is recorded.
//! While one client is still following, it carries on for that one.

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::Value;

use kiri_image::Cancel;

use super::http;

/// What a follower is sent: a progress line, or the end.
#[derive(Debug)]
pub enum Msg {
    Line(Value),
    End,
}

/// One pull and who follows it.
pub struct Job {
    pub cancel: Cancel,
    key: String,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// What a client joining late is sent first: each line so far, a layer's
    /// latest only (keyed by `id`), in the order they came.
    seen: Vec<(String, Value)>,
    followers: Vec<(u64, Sender<Msg>)>,
    next: u64,
    done: bool,
}

/// One client's end of a [`Job`].
pub struct Follower {
    pub job: Arc<Job>,
    id: u64,
    rx: Receiver<Msg>,
}

static JOBS: LazyLock<Mutex<HashMap<String, Arc<Job>>>> = LazyLock::new(Default::default);

/// Pulls that may be shared: the image, the platform asked for (as given,
/// none meaning the host's) and the sign-in, so nobody follows a pull made
/// with someone else's credentials.
pub fn key(image: &str, platform: &str, auth: &str) -> String {
    format!("{image}\n{platform}\n{auth}")
}

/// Follows the pull of `key` under way, or starts one. True when this
/// client starts it, and so must run it and [`Job::finish`] it.
pub fn join(key: &str) -> (Follower, bool) {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(job) = jobs.get(key)
        && let Some(f) = job.follow()
    {
        return (f, false);
    }
    let job = Arc::new(Job {
        cancel: Cancel::new(),
        key: key.to_owned(),
        state: Mutex::default(),
    });
    jobs.insert(key.to_owned(), job.clone());
    drop(jobs);
    let f = job.follow().expect("a new job takes followers");
    (f, true)
}

impl Job {
    /// A new follower, sent the lines so far; None once the pull is over or
    /// cancelled, when a new one must start.
    fn follow(self: &Arc<Job>) -> Option<Follower> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.done || self.cancel.is_cancelled() {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        for (_, v) in &st.seen {
            let _ = tx.send(Msg::Line(v.clone()));
        }
        st.next += 1;
        let id = st.next;
        st.followers.push((id, tx));
        Some(Follower {
            job: self.clone(),
            id,
            rx,
        })
    }

    /// A progress line, to every follower.
    pub fn send(&self, v: Value) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = v["id"].as_str().unwrap_or_default().to_owned();
        match st.seen.iter_mut().find(|(k, _)| !id.is_empty() && *k == id) {
            Some(slot) => slot.1 = v.clone(),
            None => st.seen.push((id, v.clone())),
        }
        st.followers
            .retain(|(_, tx)| tx.send(Msg::Line(v.clone())).is_ok());
    }

    /// The pull is over: followers get the end, and the next client asking
    /// starts afresh.
    pub fn finish(self: &Arc<Job>) {
        {
            let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
            if jobs.get(&self.key).is_some_and(|j| Arc::ptr_eq(j, self)) {
                jobs.remove(&self.key);
            }
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.done = true;
        for (_, tx) in st.followers.drain(..) {
            let _ = tx.send(Msg::End);
        }
    }

    /// A follower hung up; the last one to go cancels the pull.
    fn leave(&self, id: u64) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.followers.retain(|(f, _)| *f != id);
        if st.followers.is_empty() && !st.done {
            crate::diag::info!(
                "image pull cancelled: no client is waiting for it",
                image = self.key.lines().next().unwrap_or_default()
            );
            self.cancel.cancel();
        }
    }
}

impl Follower {
    /// The next message, or None after `wait` with nothing.
    pub fn next(&self, wait: Duration) -> Option<Msg> {
        match self.rx.recv_timeout(wait) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Msg::End),
        }
    }

    /// Writes the pull's lines to the client until it ends, checking every
    /// half second that the client is still there. A client that hangs up
    /// leaves; Ok(false) then, and the connection is done.
    pub fn stream(self, w: &mut UnixStream) -> io::Result<bool> {
        loop {
            match self.next(Duration::from_millis(500)) {
                Some(Msg::Line(v)) => {
                    let mut s = v.to_string();
                    s.push('\n');
                    if http::chunk(w, s.as_bytes()).is_err() {
                        return Ok(false);
                    }
                }
                Some(Msg::End) => {
                    http::end(w)?;
                    return Ok(true);
                }
                None if super::ops::gone(w) => return Ok(false),
                None => {}
            }
        }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.job.leave(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(f: &Follower) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some(Msg::Line(v)) = f.next(Duration::from_millis(10)) {
            out.push(v);
        }
        out
    }

    #[test]
    fn a_second_client_follows_the_pull_under_way() {
        let k = key("test/share:1", "", "");
        let (a, lead) = join(&k);
        assert!(lead);
        a.job
            .send(json!({"status": "Pulling from test/share", "id": "1"}));
        a.job
            .send(json!({"status": "Pulling fs layer", "id": "aaa", "size": 10}));
        a.job.send(json!({"status": "Downloading", "id": "aaa", "progressDetail": {"current": 5, "total": 10}}));
        let (b, lead) = join(&k);
        assert!(!lead);
        assert!(Arc::ptr_eq(&a.job, &b.job));
        // The lines so far, a layer's latest only.
        let got = lines(&b);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1]["status"], "Downloading");
        assert_eq!(lines(&a).len(), 3);
        // One leaving doesn't stop it for the other.
        let job = a.job.clone();
        drop(a);
        assert!(!job.cancel.is_cancelled());
        job.send(json!({"status": "Pull complete", "id": "aaa"}));
        assert_eq!(lines(&b).len(), 1);
        job.finish();
        assert!(matches!(b.next(Duration::from_millis(10)), Some(Msg::End)));
        drop(b);
        assert!(
            !job.cancel.is_cancelled(),
            "a finished pull isn't cancelled"
        );
        // Over: the next client starts a new one.
        let (c, lead) = join(&k);
        assert!(lead && !Arc::ptr_eq(&c.job, &job));
        c.job.finish();
    }

    #[test]
    fn the_last_client_leaving_cancels_it() {
        let k = key("test/cancel:1", "", "");
        let (a, _) = join(&k);
        let (b, lead) = join(&k);
        assert!(!lead);
        let job = a.job.clone();
        drop(b);
        assert!(!job.cancel.is_cancelled());
        drop(a);
        assert!(job.cancel.is_cancelled());
        // A client asking now doesn't follow the cancelled pull.
        let (c, lead) = join(&k);
        assert!(lead && !Arc::ptr_eq(&c.job, &job));
        // The cancelled one finishing doesn't take the new one's place.
        job.finish();
        let (d, lead) = join(&k);
        assert!(!lead && Arc::ptr_eq(&d.job, &c.job));
        c.job.finish();
    }

    #[test]
    fn a_client_hanging_up_mid_pull_stops_it() {
        let (follower, _) = join(&key("test/hangup:1", "", ""));
        let job = follower.job.clone();
        let (mut daemon_end, client_end) = UnixStream::pair().unwrap();
        // Quiet, then Ctrl-C: the check finds it gone.
        let started = std::time::Instant::now();
        drop(client_end);
        assert!(!follower.stream(&mut daemon_end).unwrap());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(job.cancel.is_cancelled());
        job.finish();
        // Mid-stream: the write fails.
        let (follower, _) = join(&key("test/hangup:2", "", ""));
        let job = follower.job.clone();
        let (mut daemon_end, client_end) = UnixStream::pair().unwrap();
        drop(client_end);
        job.send(json!({"status": "Downloading", "id": "aaa"}));
        assert!(!follower.stream(&mut daemon_end).unwrap());
        assert!(job.cancel.is_cancelled());
        job.finish();
    }

    #[test]
    fn pulls_differ_by_platform_and_sign_in() {
        let (a, _) = join(&key("test/apart:1", "", ""));
        let (b, lead_b) = join(&key("test/apart:1", "linux/amd64", ""));
        let (c, lead_c) = join(&key("test/apart:1", "", "someone"));
        assert!(lead_b && lead_c);
        for f in [&a, &b, &c] {
            f.job.finish();
        }
    }
}
