//! What a docker client is told when the bridge cannot reach the engine in
//! WSL (crates/kiri/src/bridge.rs closes the connection with why), rather
//! than a pipe that simply breaks. Plain functions, tested on any host.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// How long a client's request may take to come before it is answered anyway.
pub const REQUEST_WAIT: Duration = Duration::from_secs(2);

/// What a client the engine cannot serve is told: why the bridge could not
/// reach it, and what to do. A CLOSE with no reason says only what
/// happened: this bridge sends one when the daemon itself closed the
/// connection without a word (a request it would not take), and one from
/// before reasons sends it for an engine it could not reach too.
pub fn message(why: Option<&str>) -> String {
    match why {
        Some(why) => format!("Kiri's engine did not start: {why}. Open Kiri and press Start"),
        None => "Kiri's engine closed the connection without answering".to_owned(),
    }
}

/// Whether the request that begins `request` is a HEAD (docker's first
/// `HEAD /_ping`): its reply has no body.
pub fn is_head(request: &[u8]) -> bool {
    request.starts_with(b"HEAD ")
}

/// The 503 a client the engine cannot serve gets: docker prints "Error
/// response from daemon: <message>". A reply to HEAD has the headers only.
pub fn refusal(head: bool, message: &str) -> Vec<u8> {
    let body = serde_json::json!({ "message": message }).to_string();
    let mut reply = format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    if !head {
        reply.extend_from_slice(body.as_bytes());
    }
    reply
}

/// Whether a client's request has come, and is a HEAD: set by the thread
/// reading the client, waited for by the one answering it.
#[derive(Debug, Default)]
pub struct Arrival {
    head: Mutex<Option<bool>>,
    cv: Condvar,
}

impl Arrival {
    /// The request came (`head`: it is a HEAD), or the client went (false).
    /// Only the first call counts.
    pub fn set(&self, head: bool) {
        let mut h = self.head.lock().unwrap_or_else(PoisonError::into_inner);
        if h.is_none() {
            *h = Some(head);
            self.cv.notify_all();
        }
    }

    /// Whether the request is a HEAD, once it has come or `wait` has passed.
    pub fn head(&self, wait: Duration) -> bool {
        let h = self.head.lock().unwrap_or_else(PoisonError::into_inner);
        let (h, _) = self.cv.wait_timeout_while(h, wait, |h| h.is_none()).unwrap_or_else(PoisonError::into_inner);
        h.unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    fn split(reply: &[u8]) -> (String, String) {
        let text = String::from_utf8(reply.to_vec()).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        (head.to_owned(), body.to_owned())
    }

    /// What docker prints: the reason, and what to do about it.
    #[test]
    fn a_refusal_says_why_and_what_to_do() {
        let m = message(Some("kiri daemon exited as it started; see /var/log/kiri.log"));
        assert_eq!(m, "Kiri's engine did not start: kiri daemon exited as it started; see /var/log/kiri.log. Open Kiri and press Start");
        let (head, body) = split(&refusal(false, &m));
        assert!(head.starts_with("HTTP/1.1 503 Service Unavailable\r\n"), "{head}");
        assert!(head.contains("Content-Type: application/json") && head.contains("Connection: close"), "{head}");
        let len: usize = head.lines().find_map(|l| l.strip_prefix("Content-Length: ")).unwrap().parse().unwrap();
        assert_eq!(len, body.len());
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["message"], m);
    }

    /// A reason with quotes and newlines from the bridge stays one JSON string.
    #[test]
    fn a_reason_is_escaped() {
        let (_, body) = split(&refusal(false, &message(Some("said \"no\"\n\\ then"))));
        assert!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["message"].as_str().unwrap().contains("said \"no\"\n\\ then"));
    }

    /// HEAD /_ping: the same headers, Content-Length included, no body.
    #[test]
    fn a_refusal_to_head_has_no_body() {
        assert!(is_head(b"HEAD /_ping HTTP/1.1\r\nHost: api.moby.localhost\r\n\r\n"));
        assert!(!is_head(b"GET /_ping HTTP/1.1\r\n\r\n"));
        assert!(!is_head(b""));
        let m = message(None);
        let (head, body) = split(&refusal(true, &m));
        assert!(body.is_empty());
        assert_eq!(head, split(&refusal(false, &m)).0);
    }

    /// A CLOSE with no reason (the daemon closed a request it would not
    /// take, or a bridge from before reasons): what happened, true either
    /// way, and no "press Start" for an engine that may well be running.
    #[test]
    fn a_close_with_no_reason_says_only_what_happened() {
        assert_eq!(message(None), "Kiri's engine closed the connection without answering");
    }

    /// The refusal waits for the request, and no longer than it is given.
    #[test]
    fn the_answer_waits_for_the_request() {
        let a = Arc::new(Arrival::default());
        let b = a.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            b.set(true);
            b.set(false); // only the first counts
        });
        let began = Instant::now();
        assert!(a.head(REQUEST_WAIT));
        assert!(began.elapsed() >= Duration::from_millis(100) && began.elapsed() < REQUEST_WAIT);
        t.join().unwrap();
        let none = Arrival::default();
        let began = Instant::now();
        assert!(!none.head(Duration::from_millis(100)));
        assert!(began.elapsed() >= Duration::from_millis(100));
    }
}
