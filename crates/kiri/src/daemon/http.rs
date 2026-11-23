//! Just enough HTTP/1.1 for Docker clients: keep-alive, Content-Length and
//! chunked bodies, streamed (chunked) responses and connection upgrades for
//! `attach`. Docker clients keep a few connections open, so the server runs
//! one thread per connection and never parses more than it needs.

use std::cell::Cell;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;

/// API version this daemon speaks. The Docker CLI negotiates down to it.
pub const API_VERSION: &str = "1.47";
/// The oldest API version served; a request path naming an older one is
/// refused, as Docker does. A newer one than [`API_VERSION`] is still
/// served: clients built against a newer Docker that don't negotiate work
/// today, and refusing them would gain nothing.
pub const MIN_API_VERSION: (u32, u32) = (1, 24);

/// Largest request body read into memory (container create, etc.). Archives
/// and image loads are streamed with [`Body`] instead, at any size.
const MAX_BODY: usize = 8 << 20;
/// The request line and headers together may not exceed this: an endless
/// header line must not grow memory without bound.
const MAX_HEAD: u64 = 64 << 10;
/// Most of a streamed body its handler left unread that is read out so the
/// connection can serve the next request; past this it closes. Go's
/// net/http server, so Docker's, reads out as much.
const MAX_UNREAD: u64 = 256 << 10;
/// How long, and how much, a connection that ends while its client may still
/// be sending reads out and drops before it closes ([`close`]). Enough for a
/// client to take in the answer and stop; one that sends on past it gets the
/// reset it would have got without the wait.
const LINGER_TIME: std::time::Duration = std::time::Duration::from_secs(1);
const LINGER_BYTES: u64 = 16 << 20;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// Path without the `/v1.xx` prefix or the query string, percent-decoded
    /// (`%2F` in an image name is the `/` the router must resolve).
    pub path: String,
    /// The `/v1.xx` prefix, when the path had one.
    pub api_version: Option<(u32, u32)>,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Sent as HTTP/1.0: its connection closes after the response unless it
    /// asks to keep it.
    http10: bool,
    /// How much of a streamed body has been read, through [`Body`].
    streamed: Cell<Streamed>,
}

/// A streamed request body's progress: whether the connection can serve
/// another request after it ([`finish_body`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Streamed {
    /// The body was read with the head (or there is none to stream).
    No,
    Unread,
    Started,
    Done,
    /// Larger than [`MAX_BODY`] for a request whose body is read whole: not
    /// read (or read only up to the limit), and never routed ([`too_large`]).
    TooLarge,
}

impl Request {
    pub fn query(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Every value of a repeated key (`names=a&names=b`).
    pub fn queries<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.query.iter().filter(move |(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Docker sends booleans as `1`/`true`/`0`/`false`.
    pub fn flag(&self, key: &str) -> bool {
        matches!(self.query(key), Some("1" | "true" | "True"))
    }

    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }

    /// The connection closes after this request's response: the client
    /// sent `Connection: close` (Docker.DotNet does on every request), or
    /// spoke HTTP/1.0 without `Connection: keep-alive` (RFC 9112 §9.3), as
    /// Go's net/http in dockerd decides it. A client that reads a 204 to the
    /// end of the stream waits for that close and times out if it never comes.
    pub fn wants_close(&self) -> bool {
        let has = |token: &str| self.header("Connection").is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)));
        has("close") || (self.http10 && !has("keep-alive"))
    }

    /// A request the daemon makes of itself (the managed builder's
    /// container create), so it goes through the same code as a client's.
    pub fn internal(method: &str, path: &str, query: &[(&str, &str)], body: Vec<u8>) -> Request {
        let query = query.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        Request { method: method.to_owned(), path: path.to_owned(), api_version: None, query, headers: Vec::new(), body, http10: false, streamed: Cell::new(Streamed::No) }
    }

    #[cfg(test)]
    pub fn with_header(mut self, key: &str, value: &str) -> Request {
        self.headers.push((key.to_owned(), value.to_owned()));
        self
    }
}

/// Reads the next request on a connection. `Ok(None)` is a clean close.
/// When `stream(method, path)` is true the body stays unread: the handler
/// reads it through [`Body`].
pub fn read_request(r: &mut BufReader<UnixStream>, stream: impl Fn(&str, &str) -> bool) -> io::Result<Option<Request>> {
    // The request line and headers are read through a cap, so a client
    // cannot exhaust memory with an endless line. The body is read after.
    let (method, path, api_version, query, headers, http10) = {
        let mut head = (&mut *r).take(MAX_HEAD);
        let mut line = String::new();
        if head.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request line"));
        };
        let http10 = parts.next().is_some_and(|v| v.eq_ignore_ascii_case("HTTP/1.0"));
        let method = method.to_owned();
        let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
        // Decoded once, here, so every route that reads a name from the
        // path — an image's `%2F` repository slash above all — resolves the
        // name, not its encoding, as Docker's router does.
        let unversioned = strip_version(raw_path);
        let api_version = version_of(&raw_path[..raw_path.len() - unversioned.len()]);
        let path = percent_decode_path(unversioned);
        let query: Vec<(String, String)> = raw_query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (percent_decode(k), percent_decode(v))
            })
            .collect();
        let mut headers = Vec::new();
        loop {
            line.clear();
            if head.read_line(&mut line)? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "headers too large or cut short"));
            }
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_owned(), v.trim().to_owned()));
            }
        }
        (method, path, api_version, query, headers, http10)
    };
    let streamed = stream(&method, &path);
    let mut req = Request { method, path, api_version, query, headers, body: Vec::new(), http10, streamed: Cell::new(Streamed::No) };
    if streamed {
        req.streamed.set(Streamed::Unread);
        return Ok(Some(req));
    }
    if req.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        let mut line = String::new();
        loop {
            line.clear();
            // A chunk-size line is short hex (plus an optional ;extension).
            (&mut *r).take(64).read_line(&mut line)?;
            let size = line.trim().split(';').next().unwrap_or("");
            let n = usize::from_str_radix(size, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if n > MAX_BODY || req.body.len().checked_add(n).is_none_or(|t| t > MAX_BODY) {
                return Ok(Some(too_large(req)));
            }
            let start = req.body.len();
            req.body.resize(start + n, 0);
            r.read_exact(&mut req.body[start..])?;
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf)?;
            if n == 0 {
                break;
            }
        }
    } else if let Some(len) = req.header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if len > MAX_BODY {
            return Ok(Some(too_large(req)));
        }
        req.body.resize(len, 0);
        r.read_exact(&mut req.body)?;
    }
    Ok(Some(req))
}

/// A request whose body is past [`MAX_BODY`]: what was read of it is dropped
/// and the rest stays unread, for the connection's [`close`] to read out.
fn too_large(mut req: Request) -> Request {
    req.body = Vec::new();
    req.streamed.set(Streamed::TooLarge);
    req
}

/// Answers a request whose body is too large to read ([`Request::too_large`])
/// and ends the connection. Docker sets no limit (its handlers decode the
/// body whatever its size, and a route that does not read it answers as
/// usual); holding all of it is not safe here, and routing the request
/// without it would run a handler on a body it never saw (a create with an
/// empty config). So it is refused, in Docker's error shape, and the client,
/// which may still be sending, gets the whole answer and a clean end.
pub fn refuse_too_large(req: &Request, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<()> {
    serving(req);
    let sent = error_closing(w, 413, &format!("request body too large: more than the {} MiB a request may send", MAX_BODY >> 20));
    close(w, r);
    sent
}

/// After a streamed request's response: whether the connection can serve
/// the client's next request. A body its handler left unread (a pull sent
/// with one, as docker-modem sends its options) is read out first, up to
/// [`MAX_UNREAD`], or its bytes would be taken for the next request's head.
/// One read partly, too large, or held back for a `100 Continue` the daemon
/// never sends cannot be; the connection closes.
pub fn finish_body(req: &Request, r: &mut BufReader<UnixStream>) -> bool {
    match req.streamed.get() {
        Streamed::No | Streamed::Done => true,
        Streamed::Started | Streamed::TooLarge => false,
        Streamed::Unread if req.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) => false,
        Streamed::Unread => {
            let mut body = Body::new(req, r);
            io::copy(&mut (&mut body).take(MAX_UNREAD), &mut io::sink()).is_ok() && body.read(&mut [0; 1]).is_ok_and(|n| n == 0)
        }
    }
}

impl Request {
    /// A streamed body its handler did not read to its end: the client may
    /// still be sending it.
    pub fn body_left(&self) -> bool {
        matches!(self.streamed.get(), Streamed::Unread | Streamed::Started | Streamed::TooLarge)
    }

    /// A body past [`MAX_BODY`] on a route that reads it whole: the request
    /// is answered by [`refuse_too_large`], never routed.
    pub fn too_large(&self) -> bool {
        self.streamed.get() == Streamed::TooLarge
    }
}

/// Ends a connection after a whole response, as Go's net/http server (so
/// Docker's) does: the write side is shut first, so the client sees the end
/// of the stream at once, even while anything else still holds a copy of the
/// socket; then whatever the client is still sending (a body the handler left
/// unread, a pipelined request) is read and dropped, up to [`LINGER_BYTES`]
/// and [`LINGER_TIME`], until the client closes its side. Closing a socket
/// with unread data in it resets the connection on Linux: the client's read
/// fails with "connection reset" after the answer instead of ending cleanly.
pub fn close(w: &UnixStream, r: &mut BufReader<UnixStream>) {
    let _ = w.shutdown(std::net::Shutdown::Write);
    let deadline = std::time::Instant::now() + LINGER_TIME;
    let mut left = LINGER_BYTES;
    r.consume(r.buffer().len());
    let s = r.get_mut();
    let mut buf = vec![0u8; 64 << 10];
    while left > 0 {
        let Some(wait) = deadline.checked_duration_since(std::time::Instant::now()).filter(|d| !d.is_zero()) else { break };
        if s.set_read_timeout(Some(wait)).is_err() {
            break;
        }
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => left = left.saturating_sub(n as u64),
        }
    }
}

/// A streamed request body: Content-Length or chunked, read as it arrives.
pub struct Body<'a> {
    r: &'a mut BufReader<UnixStream>,
    chunked: bool,
    /// Bytes left in the body (or in the current chunk).
    left: u64,
    done: bool,
    /// The request's record of how far its body was read.
    progress: &'a Cell<Streamed>,
}

impl<'a> Body<'a> {
    pub fn new(req: &'a Request, r: &'a mut BufReader<UnixStream>) -> Body<'a> {
        let chunked = req.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
        let left = if chunked { 0 } else { req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0) };
        let done = !chunked && left == 0;
        req.streamed.set(if done { Streamed::Done } else { Streamed::Started });
        Body { r, chunked, left, done, progress: &req.streamed }
    }
}

impl Read for Body<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.chunked && self.left == 0 {
            let mut line = String::new();
            self.r.read_line(&mut line)?;
            // A chunk size may carry extensions after a semicolon.
            let size = line.trim().split(';').next().unwrap_or("");
            self.left = u64::from_str_radix(size, 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if self.left == 0 {
                // Trailers, then the blank line.
                loop {
                    line.clear();
                    if self.r.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                self.done = true;
                self.progress.set(Streamed::Done);
                return Ok(0);
            }
        }
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.r.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request body cut short"));
        }
        self.left -= n as u64;
        if self.left == 0 {
            if self.chunked {
                let mut crlf = [0u8; 2];
                self.r.read_exact(&mut crlf)?;
            } else {
                self.done = true;
                self.progress.set(Streamed::Done);
            }
        }
        Ok(n)
    }
}

/// `/v1.47/containers/json` → `/containers/json`.
fn strip_version(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("/v")
        && let Some(i) = rest.find('/')
            && rest[..i].bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                return &rest[i..];
            }
    path
}

/// `/v1.47` → (1, 47). None for no prefix, or one that isn't two numbers.
fn version_of(prefix: &str) -> Option<(u32, u32)> {
    let (major, minor) = prefix.strip_prefix("/v")?.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Percent-decodes a query value, where `+` means a space.
fn percent_decode(s: &str) -> String {
    unescape(s, true)
}

/// Percent-decodes the request path. A client may send a name's slash or
/// colon encoded (`/images/mongodb%2Fmongodb-atlas-local:8.0/json`), and the
/// router has to see the name, not its encoding. A `+` is a plus here, not
/// a space; an invalid escape stays written, as in a query value.
fn percent_decode_path(s: &str) -> String {
    unescape(s, false)
}

fn unescape(s: &str, plus_space: bool) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' if plus_space => out.push(b' '),
            b'%' if i + 2 < b.len() && hex(b[i + 1]).is_some() && hex(b[i + 2]).is_some() => {
                out.push(hex(b[i + 1]).unwrap_or(0) << 4 | hex(b[i + 2]).unwrap_or(0));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reason(status: u16) -> &'static str {
    match status {
        101 => "UPGRADED",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Request Entity Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "",
    }
}

thread_local! {
    /// The request this connection thread is answering, for the log.
    static SERVING: std::cell::RefCell<(String, String)> = const { std::cell::RefCell::new((String::new(), String::new())) };
    /// Whether the connection closes after the request being answered: its
    /// response then says so (`Connection: close`), as Docker's does.
    static CLOSING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Notes the request being answered: a failure status written for it is
/// logged with its method and path (never its query or body), and its
/// response carries `Connection: close` when the connection ends after it.
pub fn serving(req: &Request) {
    SERVING.with(|s| *s.borrow_mut() = (req.method.clone(), req.path.clone()));
    CLOSING.with(|c| c.set(req.wants_close()));
}

/// Engine API failures in the log: 5xx as errors, 4xx as warnings, except
/// 404s, which clients use to ask (does this image exist yet?).
fn note_failure(status: u16) {
    SERVING.with(|s| {
        let (method, path) = &*s.borrow();
        match status {
            404 => crate::diag::debug!("request failed", method = method, path = path, status = status),
            400..=499 => crate::diag::warn!("request failed", method = method, path = path, status = status),
            _ => crate::diag::error!("request failed", method = method, path = path, status = status),
        }
    });
}

fn head(w: &mut impl Write, status: u16, extra: &str) -> io::Result<()> {
    if status >= 400 {
        note_failure(status);
    }
    // A response that names its own Connection (an upgrade, `CLOSE`) keeps it.
    let closing = if CLOSING.with(std::cell::Cell::get) && !extra.contains("Connection:") { CLOSE } else { "" };
    write!(
        w,
        "HTTP/1.1 {status} {}\r\nApi-Version: {API_VERSION}\r\nDocker-Experimental: false\r\nOstype: linux\r\nServer: Kiri/{} (linux)\r\n{extra}{closing}\r\n",
        reason(status),
        env!("CARGO_PKG_VERSION")
    )
}

pub fn respond(w: &mut impl Write, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let extra = if status == 204 || status == 304 {
        String::new()
    } else {
        format!("Content-Type: {content_type}\r\nContent-Length: {}\r\n", body.len())
    };
    head(w, status, &extra)?;
    w.write_all(body)?;
    w.flush()
}

/// `respond` plus extra header lines, each ending in CRLF.
pub fn respond_with(w: &mut impl Write, status: u16, content_type: &str, extra: &str, body: &[u8]) -> io::Result<()> {
    head(w, status, &format!("Content-Type: {content_type}\r\nContent-Length: {}\r\n{extra}", body.len()))?;
    w.write_all(body)?;
    w.flush()
}

pub fn json(w: &mut impl Write, status: u16, body: &str) -> io::Result<()> {
    respond(w, status, "application/json", body.as_bytes())
}

/// A success with no body, under the status the route names.
pub fn empty(w: &mut impl Write, status: u16) -> io::Result<()> {
    respond(w, status, "", b"")
}

/// Docker's error shape: `{"message": "..."}`.
pub fn error(w: &mut impl Write, status: u16, message: &str) -> io::Result<()> {
    json(w, status, &serde_json::json!({ "message": message }).to_string())
}

/// The header of a response after which the server closes the connection
/// (a request body may be left unread). Without it an HTTP/1.1 client keeps
/// the connection for its next request and writes into a closed socket:
/// docker-java's pool (Testcontainers) failed the `start` after a copy into
/// the container with "Broken pipe".
pub const CLOSE: &str = "Connection: close\r\n";

/// `error` on a connection that closes after it (`CLOSE`).
pub fn error_closing(w: &mut impl Write, status: u16, message: &str) -> io::Result<()> {
    respond_with(w, status, "application/json", CLOSE, serde_json::json!({ "message": message }).to_string().as_bytes())
}

/// Starts a streamed response; follow with [`chunk`] calls and [`end`].
pub fn begin_stream(w: &mut impl Write, status: u16, content_type: &str) -> io::Result<()> {
    begin_stream_with(w, status, content_type, "")
}

/// `begin_stream` plus extra header lines, each ending in CRLF.
pub fn begin_stream_with(w: &mut impl Write, status: u16, content_type: &str, extra: &str) -> io::Result<()> {
    head(w, status, &format!("Content-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n{extra}"))?;
    w.flush()
}

/// Headers only, for HEAD: `extra` lines end in CRLF.
pub fn head_only(w: &mut impl Write, status: u16, extra: &str) -> io::Result<()> {
    head(w, status, &format!("Content-Length: 0\r\n{extra}"))?;
    w.flush()
}

pub fn chunk(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(w, "{:x}\r\n", data.len())?;
    w.write_all(data)?;
    w.write_all(b"\r\n")?;
    w.flush()
}

pub fn end(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"0\r\n\r\n")?;
    w.flush()
}

/// Switches the connection to Docker's raw stream for `attach`. After this
/// the connection carries multiplexed output frames until it closes.
pub fn upgrade(w: &mut impl Write) -> io::Result<()> {
    head(w, 101, "Content-Type: application/vnd.docker.raw-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n")?;
    w.flush()
}

/// Starts the raw stream for Docker's `attach` and `exec` start. As Docker
/// does (it checks that the `Upgrade` header is present, not its value), a
/// client that asked for the upgrade (the docker CLI) gets `101`; one that did
/// not (docker-java, so Testcontainers) gets `200 OK` with the stream as the
/// body. A `101` to a client that never asked for it can end its read before
/// the process's exit code is recorded, and it then sees an exec with no
/// `ExitCode`.
pub fn upgrade_if_asked(req: &Request, w: &mut impl Write) -> io::Result<()> {
    if req.header("Upgrade").is_some() {
        return upgrade(w);
    }
    head(w, 200, "Content-Type: application/vnd.docker.raw-stream\r\n")?;
    w.flush()
}

/// Docker's multiplexed stream framing: `[stream, 0, 0, 0, len u32 BE] data`.
pub fn mux_frame(w: &mut impl Write, stream: u8, data: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 8];
    header[0] = stream;
    header[4..].copy_from_slice(&(data.len() as u32).to_be_bytes());
    w.write_all(&header)?;
    w.write_all(data)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_connection(value: Option<&str>) -> Request {
        let headers = value.map(|v| vec![("connection".to_owned(), v.to_owned())]).unwrap_or_default();
        Request { method: "POST".to_owned(), path: "/containers/x/start".to_owned(), api_version: None, query: Vec::new(), headers, body: Vec::new(), http10: false, streamed: Cell::new(Streamed::No) }
    }

    /// A request as `read_request` reads it off the wire.
    fn parsed(raw: &str) -> Request {
        use std::io::Write;
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(raw.as_bytes()).unwrap();
        drop(a);
        read_request(&mut BufReader::new(b), |_, _| false).unwrap().unwrap()
    }

    /// Docker.DotNet sends `Connection: close` on every request and reads a
    /// 204 to the end of the stream: the connection must close after it.
    #[test]
    fn connection_close_is_asked_for_in_any_case_and_list() {
        assert!(with_connection(Some("close")).wants_close());
        assert!(with_connection(Some("Close")).wants_close());
        assert!(with_connection(Some("keep-alive, close")).wants_close());
    }

    #[test]
    fn keep_alive_or_no_header_keeps_the_connection() {
        assert!(!with_connection(Some("keep-alive")).wants_close());
        assert!(!with_connection(Some("Upgrade")).wants_close());
        assert!(!with_connection(None).wants_close());
    }

    /// HTTP/1.0 closes after each response unless it asks to keep the
    /// connection; HTTP/1.1 keeps it unless it asks to close.
    #[test]
    fn the_close_follows_the_http_version_and_header() {
        let closes = |raw: &str| parsed(raw).wants_close();
        assert!(closes("POST /v1.44/containers/x/start HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n"));
        assert!(closes("GET /_ping HTTP/1.1\r\nHost: d\r\nconnection: Upgrade, Close\r\n\r\n"));
        assert!(closes("POST /containers/x/start HTTP/1.0\r\nHost: d\r\n\r\n"));
        assert!(closes("GET /_ping HTTP/1.0\r\n\r\n"));
        assert!(closes("GET /_ping HTTP/1.0\r\nConnection: keep-alive, close\r\n\r\n"));
        assert!(!closes("GET /_ping HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n"));
        assert!(!closes("GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n"));
        assert!(!closes("GET /_ping HTTP/1.1\r\nHost: d\r\nConnection: keep-alive\r\n\r\n"));
        assert!(!closes("POST /containers/x/attach HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n"));
        // The daemon's own requests keep nothing open, but never ask to close.
        assert!(!Request::internal("POST", "/containers/create", &[], Vec::new()).wants_close());
    }

    /// The response to a request whose connection closes says so, as
    /// Docker's does; a kept-alive one says nothing, and an upgrade or a
    /// response that already names its Connection is not given a second.
    #[test]
    fn a_closing_response_says_connection_close() {
        let answer = |raw: &str, write: &dyn Fn(&mut Vec<u8>) -> io::Result<()>| {
            serving(&parsed(raw));
            let mut out = Vec::new();
            write(&mut out).unwrap();
            String::from_utf8(out).unwrap()
        };
        let close11 = "POST /containers/x/start HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n";
        let start10 = "POST /containers/x/start HTTP/1.0\r\n\r\n";
        let kept = "POST /containers/x/start HTTP/1.1\r\nHost: d\r\n\r\n";

        let r = answer(close11, &|w| empty(w, 204));
        assert!(r.starts_with("HTTP/1.1 204 No Content\r\n") && r.ends_with("Connection: close\r\n\r\n"), "{r}");
        let r = answer(start10, &|w| empty(w, 204));
        assert_eq!(r.matches("Connection: close\r\n").count(), 1, "{r}");
        let r = answer(close11, &|w| json(w, 200, "{}"));
        assert!(r.contains("Content-Length: 2\r\nConnection: close\r\n\r\n{}"), "{r}");
        let r = answer(close11, &|w| begin_stream(w, 200, "application/json"));
        assert!(r.contains("Transfer-Encoding: chunked\r\nConnection: close\r\n"), "{r}");
        let r = answer(close11, &|w| error_closing(w, 409, "busy"));
        assert_eq!(r.matches("Connection:").count(), 1, "{r}");
        let r = answer(close11, &|w| upgrade(w));
        assert!(r.contains("Connection: Upgrade\r\n") && !r.contains("Connection: close"), "{r}");

        let r = answer(kept, &|w| empty(w, 204));
        assert!(!r.contains("Connection:"), "{r}");
        let r = answer(kept, &|w| json(w, 200, "{}"));
        assert!(!r.contains("Connection:"), "{r}");
    }

    #[test]
    fn strips_api_version() {
        assert_eq!(strip_version("/v1.47/containers/json"), "/containers/json");
        assert_eq!(strip_version("/_ping"), "/_ping");
        assert_eq!(version_of("/v1.47"), Some((1, 47)));
        assert_eq!(version_of("/v1.12"), Some((1, 12)));
        assert_eq!(version_of(""), None);
        assert_eq!(version_of("/v2"), None);
        assert!((1, 12) < MIN_API_VERSION && (1, 24) >= MIN_API_VERSION && (1, 99) > MIN_API_VERSION);
        assert_eq!(strip_version("/volumes"), "/volumes");
    }

    #[test]
    fn decodes_query_values() {
        assert_eq!(percent_decode("docker.io%2Flibrary%2Falpine"), "docker.io/library/alpine");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%7B%22a%22%3A1%7D"), "{\"a\":1}");
    }

    /// The path a request is routed by, read from a real request line so
    /// the decoding `read_request` does is what's checked.
    #[test]
    fn decodes_request_paths() {
        use std::io::Write;
        let path_of = |target: &str| {
            let (mut a, b) = UnixStream::pair().unwrap();
            a.write_all(format!("GET {target} HTTP/1.1\r\nHost: d\r\n\r\n").as_bytes())
                .unwrap();
            drop(a);
            let mut r = BufReader::new(b);
            read_request(&mut r, |_, _| false).unwrap().unwrap().path
        };
        // A repository slash a client percent-encoded names the same image
        // as its plain form, with or without the /v1.xx prefix.
        assert_eq!(
            path_of("/v1.44/images/mongodb%2Fmongodb-atlas-local:8.0/json"),
            "/images/mongodb/mongodb-atlas-local:8.0/json"
        );
        assert_eq!(
            path_of("/images/mongodb/mongodb-atlas-local:8.0/json"),
            "/images/mongodb/mongodb-atlas-local:8.0/json"
        );
        // The same hole, everywhere else a name is read from the path.
        assert_eq!(
            path_of("/v1.44/containers/db%3A5/json"),
            "/containers/db:5/json"
        );
        assert_eq!(path_of("/volumes/data%2Fset"), "/volumes/data/set");
        assert_eq!(path_of("/networks/seg%3Anet"), "/networks/seg:net");
        // A `+` is a plus in a path, not the space it is in a query value.
        assert_eq!(path_of("/images/a+b/json"), "/images/a+b/json");
        // An invalid escape stays written, as in query values.
        assert_eq!(path_of("/images/100%/json"), "/images/100%/json");
    }

    /// `attach` and `exec` start answer `101` only to a client that asked to
    /// upgrade (the docker CLI). docker-java, so Testcontainers, does not, and
    /// gets `200 OK` with the raw stream as the body, as from Docker.
    #[test]
    fn upgrades_only_when_asked() {
        let reply = |headers: Vec<(String, String)>| {
            let req = Request { method: "POST".into(), path: "/exec/x/start".into(), api_version: None, query: Vec::new(), headers, body: Vec::new(), http10: false, streamed: Cell::new(Streamed::No) };
            let mut out = Vec::new();
            upgrade_if_asked(&req, &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };
        let asked = reply(vec![("Upgrade".into(), "tcp".into()), ("Connection".into(), "Upgrade".into())]);
        assert!(asked.starts_with("HTTP/1.1 101 "), "{asked}");
        assert!(asked.contains("Upgrade: tcp\r\n"), "{asked}");
        let other = reply(vec![("upgrade".into(), "h2c".into())]);
        assert!(other.starts_with("HTTP/1.1 101 "), "{other}");
        let plain = reply(Vec::new());
        assert!(plain.starts_with("HTTP/1.1 200 OK\r\n"), "{plain}");
        assert!(plain.contains("Content-Type: application/vnd.docker.raw-stream\r\n"), "{plain}");
        assert!(!plain.contains("Upgrade"), "{plain}");
    }
}
