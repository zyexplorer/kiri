//! Opens TCP connections to registries the way browsers and curl do, Happy
//! Eyeballs (RFC 8305): the resolved addresses are tried with the families
//! taking turns, a new attempt starts every 250 ms or as soon as one fails,
//! and the first to connect wins.
//!
//! ureq on its own tries the addresses one after another and splits the
//! connect timeout between them, half to the first. A host or VM with an
//! IPv6 address but no working IPv6 route (WSL2 and many home networks)
//! then spends most of the 15 s on `registry-1.docker.io`'s AAAA records,
//! which drop the SYN, before an A record gets the little time left, and
//! a pull stalls or fails although IPv4 works.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Weak, mpsc};
use std::time::{Duration, Instant};
use std::{fmt, thread};

use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, RustlsConnector, ShutdownHandle,
    Transport,
};
use ureq::{Error, Timeout};

/// How long to wait for an attempt before starting the next, RFC 8305's
/// recommended Connection Attempt Delay.
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// How long one address gets to answer. Attempts overlap, so this caps a
/// single silent address, not the whole connect.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest one read or write may wait with nothing moving, whatever
/// ureq allows for the call as a whole (by default no limit at all). A
/// connection that goes silent mid-body (a stalled CDN, a NAT that dropped
/// it) then fails, and the pull resumes the blob on a new one, rather than
/// waiting forever with the builder's lock held.
pub(crate) const STALL: Duration = Duration::from_secs(60);

/// ureq's default chain with the TCP step swapped for Happy Eyeballs: an
/// HTTP CONNECT proxy if one is set (its own connection to the proxy goes
/// through this chain too), then TCP, then TLS for https.
pub(crate) fn connector() -> impl Connector<(), Out = impl Transport> {
    connector_with(STALL)
}

/// [`connector`], with reads and writes giving up after `stall` of silence.
pub(crate) fn connector_with(stall: Duration) -> impl Connector<(), Out = impl Transport> {
    ().chain(ConnectProxyConnector::default()).chain(HappyConnector { stall }).chain(RustlsConnector::default())
}

/// TLS without checking the server's certificate: only for the hosts
/// daemon.json lists in `insecure-registries` (a self-signed registry), as
/// Docker reaches them. Nothing else is built on it.
pub(crate) fn unverified_tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder().disable_verification(true).build()
}

/// The addresses in the order to try them: the family the resolver put
/// first leads (it sorts by RFC 6724, so that is IPv6 where IPv6 looks
/// usable), then the families take turns, each keeping its own order.
pub(crate) fn interleave(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addrs.first() else {
        return Vec::new();
    };
    let (mut lead, mut other): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs.iter().partition(|a| a.is_ipv6() == first.is_ipv6());
    lead.reverse();
    other.reverse();
    let mut out = Vec::with_capacity(addrs.len());
    while !lead.is_empty() || !other.is_empty() {
        out.extend(lead.pop());
        out.extend(other.pop());
    }
    out
}

/// One connect attempt: the address, and how long it may take.
type Attempt<T> = dyn Fn(SocketAddr, Duration) -> io::Result<T> + Send + Sync;

/// Races `attempt` over `addrs` in [`interleave`] order: a new attempt
/// starts `delay` after the last one started, or straight away when one
/// fails, and the first to succeed is returned. Attempts still running
/// carry on in their threads and close what they open; each is bounded by
/// its own timeout. `budget` bounds the whole race, if there is one.
pub(crate) fn race<T: Send + 'static>(addrs: &[SocketAddr], delay: Duration, budget: Option<Duration>, attempt: Arc<Attempt<T>>) -> io::Result<T> {
    let order = interleave(addrs);
    if order.is_empty() {
        return Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "no addresses to connect to"));
    }
    let start = Instant::now();
    let end = budget.map(|b| start + b);
    let (tx, rx) = mpsc::channel::<io::Result<T>>();
    let mut next = 0;
    let mut running = 0;
    let mut next_start = start;
    let mut last_err = None;
    loop {
        let now = Instant::now();
        if end.is_some_and(|e| now >= e) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
        }
        if next < order.len() && (running == 0 || now >= next_start) {
            let addr = order[next];
            let limit = end.map_or(ATTEMPT_TIMEOUT, |e| (e - now).min(ATTEMPT_TIMEOUT));
            let (tx, attempt) = (tx.clone(), attempt.clone());
            let spawned = thread::Builder::new().name("kiri-connect".into()).spawn(move || {
                // Nobody listens once another attempt has won; what this
                // one opened is then dropped, and so closed.
                let _ = tx.send(attempt(addr, limit));
            });
            next += 1;
            next_start = now + delay;
            match spawned {
                Ok(_) => running += 1,
                Err(e) => last_err = Some(e),
            }
            continue;
        }
        if running == 0 {
            return Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused")));
        }
        // Wake for the next attempt's turn, or the end of the budget.
        let wake = if next < order.len() { Some(next_start) } else { end };
        let got = match wake {
            Some(w) => rx.recv_timeout(w.saturating_duration_since(now)),
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match got {
            Ok(Ok(won)) => return Ok(won),
            Ok(Err(e)) => {
                running -= 1;
                last_err = Some(e);
                // A failure hands its turn to the next address now.
                next_start = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // We hold a sender, so the channel cannot close.
            Err(mpsc::RecvTimeoutError::Disconnected) => unreachable!(),
        }
    }
}

fn connect_one(addr: SocketAddr, limit: Duration, no_delay: bool) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, limit)?;
    if no_delay {
        stream.set_nodelay(true)?;
    }
    Ok(stream)
}

/// The TCP step of [`connector`].
#[derive(Debug)]
struct HappyConnector {
    stall: Duration,
}

impl<In: Transport> Connector<In> for HappyConnector {
    type Out = Either<In, TcpTransport>;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, Error> {
        // A proxy connector already made the connection.
        if chained.is_some() {
            return Ok(chained.map(Either::A));
        }
        let config = details.config;
        let no_delay = config.no_delay();
        let budget = details.timeout.not_zero().map(|t| *t);
        let stream = race(&details.addrs, ATTEMPT_DELAY, budget, Arc::new(move |addr, limit| connect_one(addr, limit, no_delay)))
            .map_err(|e| if e.kind() == io::ErrorKind::TimedOut { Error::Timeout(Timeout::Connect) } else { Error::Io(e) })?;
        let transport = TcpTransport {
            stream: Arc::new(stream),
            buffers: LazyBuffers::new(config.input_buffer_size(), config.output_buffer_size()),
            timeout_read: None,
            timeout_write: None,
            stall: self.stall,
        };
        (details.register_shutdown)(Arc::downgrade(&transport.stream) as Weak<dyn ShutdownHandle>)?;
        Ok(Some(Either::B(transport)))
    }
}

/// A plain TCP connection, as ureq's own `TcpTransport` (which it does not
/// export) handles one.
struct TcpTransport {
    /// Shared with the agent's shutdown handle, which holds it weakly.
    stream: Arc<TcpStream>,
    buffers: LazyBuffers,
    timeout_read: Option<Duration>,
    timeout_write: Option<Duration>,
    /// See [`STALL`].
    stall: Duration,
}

impl fmt::Debug for TcpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpTransport").field("addr", &self.stream.peer_addr().ok()).finish()
    }
}

/// The timeout left, or the error to give when it has run out.
fn remaining(timeout: NextTimeout) -> Result<Option<Duration>, Error> {
    if !timeout.after.is_not_happening() && timeout.after.is_zero() {
        return Err(Error::Timeout(timeout.reason));
    }
    Ok(timeout.not_zero().map(|t| *t))
}

/// `limit`, never longer than `stall`.
fn capped(limit: Option<Duration>, stall: Duration) -> Option<Duration> {
    Some(limit.map_or(stall, |l| l.min(stall)))
}

/// Socket I/O errors as ureq reports them: a timeout (which a socket
/// timeout shows as `WouldBlock` on some systems) names its reason.
fn io_error(e: io::Error, reason: Timeout) -> Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::Timeout(reason),
        _ => Error::Io(e),
    }
}

impl Transport for TcpTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        let timeout = timeout.for_write();
        let limit = capped(remaining(timeout)?, self.stall);
        if limit != self.timeout_write {
            self.stream.set_write_timeout(limit)?;
            self.timeout_write = limit;
        }
        crate::cancel::watch(&self.stream)?;
        let output = &self.buffers.output()[..amount];
        (&*self.stream).write_all(output).map_err(|e| io_error(e, timeout.reason))
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let timeout = timeout.for_read();
        let limit = capped(remaining(timeout)?, self.stall);
        if limit != self.timeout_read {
            self.stream.set_read_timeout(limit)?;
            self.timeout_read = limit;
        }
        // A cancelled pull's reads end here, or at once if already waiting.
        crate::cancel::watch(&self.stream)?;
        let started = Instant::now();
        let input = self.buffers.input_append_buf();
        let amount = loop {
            match (&*self.stream).read(input) {
                // A signal: Linux does not restart a socket read that has a
                // timeout, so go again with what is left of it.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    let Some(limit) = limit else {
                        return Err(Error::Io(e));
                    };
                    let left = limit.saturating_sub(started.elapsed());
                    if left.is_zero() {
                        return Err(Error::Timeout(timeout.reason));
                    }
                    self.stream.set_read_timeout(Some(left))?;
                    self.timeout_read = Some(left);
                }
                result => break result.map_err(|e| io_error(e, timeout.reason))?,
            }
        };
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        // Nothing should be waiting on an idle connection: a byte means the
        // server sent something unasked, an error or EOF that it closed.
        let stream = &*self.stream;
        if stream.set_nonblocking(true).is_err() {
            return false;
        }
        let open = matches!((&*self.stream).read(&mut [0]), Err(e) if e.kind() == io::ErrorKind::WouldBlock);
        open && stream.set_nonblocking(false).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn families_take_turns_led_by_the_first() {
        let got = interleave(&[addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443"), addr("[2001:db8::3]:443"), addr("192.0.2.1:443"), addr("192.0.2.2:443")]);
        let want = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443"), addr("[2001:db8::2]:443"), addr("192.0.2.2:443"), addr("[2001:db8::3]:443")];
        assert_eq!(got, want);
        // The resolver put IPv4 first: it leads.
        let got = interleave(&[addr("192.0.2.1:443"), addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443")]);
        assert_eq!(got, [addr("192.0.2.1:443"), addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443")]);
        assert_eq!(interleave(&[addr("192.0.2.1:443")]), [addr("192.0.2.1:443")]);
        assert!(interleave(&[]).is_empty());
    }

    /// A fake connect: IPv6 per `v6`, IPv4 connects after 20 ms.
    fn fake(v6: fn(Duration) -> io::Result<SocketAddr>) -> Arc<Attempt<SocketAddr>> {
        Arc::new(move |a: SocketAddr, limit| {
            if a.is_ipv6() {
                v6(limit).map(|_| a)
            } else {
                thread::sleep(Duration::from_millis(20));
                Ok(a)
            }
        })
    }

    #[test]
    fn an_unreachable_ipv6_hands_over_to_ipv4_at_once() {
        let addrs = [addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443"), addr("192.0.2.1:443")];
        let start = Instant::now();
        let won = race(&addrs, ATTEMPT_DELAY, Some(Duration::from_secs(15)), fake(|_| Err(io::ErrorKind::NetworkUnreachable.into()))).unwrap();
        assert_eq!(won, addr("192.0.2.1:443"));
        // No 250 ms wait: the failure started the next attempt.
        assert!(start.elapsed() < Duration::from_millis(200), "{:?}", start.elapsed());
    }

    #[test]
    fn a_silent_ipv6_gives_ipv4_its_turn_after_the_delay() {
        let addrs = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443")];
        let start = Instant::now();
        let won = race(
            &addrs,
            ATTEMPT_DELAY,
            Some(Duration::from_secs(15)),
            fake(|limit| {
                // A dropped SYN: nothing until the attempt's timeout.
                thread::sleep(limit.min(Duration::from_secs(3)));
                Err(io::ErrorKind::TimedOut.into())
            }),
        )
        .unwrap();
        assert_eq!(won, addr("192.0.2.1:443"));
        let took = start.elapsed();
        assert!(took >= ATTEMPT_DELAY && took < Duration::from_secs(1), "{took:?}");
    }

    #[test]
    fn a_working_ipv6_wins_before_ipv4_starts() {
        let addrs = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443")];
        let tried = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = tried.clone();
        let won = race(
            &addrs,
            ATTEMPT_DELAY,
            None,
            Arc::new(move |a: SocketAddr, _| {
                seen.lock().unwrap().push(a);
                Ok(a)
            }),
        )
        .unwrap();
        assert_eq!(won, addr("[2001:db8::1]:443"));
        assert_eq!(*tried.lock().unwrap(), [won]);
    }

    #[test]
    fn every_address_failing_reports_the_last_error() {
        let addrs = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443")];
        let err = race(&addrs, ATTEMPT_DELAY, None, Arc::new(|_, _| Err::<(), _>(io::ErrorKind::ConnectionRefused.into()))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn the_budget_bounds_the_whole_race() {
        let addrs = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443")];
        let start = Instant::now();
        let err = race(
            &addrs,
            Duration::from_millis(50),
            Some(Duration::from_millis(300)),
            Arc::new(|_, limit| {
                thread::sleep(limit);
                Err::<(), _>(io::ErrorKind::TimedOut.into())
            }),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
    }

    #[test]
    fn connects_past_a_refused_address_for_real() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap();
        // A port just freed: nothing listens there, so it is refused (or,
        // without IPv6 on loopback, unavailable); either way, move on.
        let closed = TcpListener::bind("[::1]:0").ok().map(|l| l.local_addr().unwrap()).unwrap_or(addr("[::1]:9"));
        let stream = race(&[closed, open], ATTEMPT_DELAY, Some(Duration::from_secs(5)), Arc::new(|a, limit| connect_one(a, limit, true))).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open);
    }
}
