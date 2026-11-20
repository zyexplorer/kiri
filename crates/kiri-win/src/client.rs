//! The window talks to the daemon the way the docker CLI does: HTTP over
//! `\\.\pipe\kiri`, which kiri-win itself serves and carries into WSL. As
//! crates/kiri/src/client.rs over the unix socket: one short connection
//! per request, and long-lived ones for `/events`, logs and stats.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;

use crate::pipe::Pipe;

static PIPE: OnceLock<String> = OnceLock::new();

/// The pipe the daemon is served on (once, at start).
pub fn set_pipe(name: &str) {
    let _ = PIPE.set(name.to_owned());
}

/// The pipe the daemon is served on.
pub fn pipe() -> String {
    PIPE.get().cloned().unwrap_or_else(|| r"\\.\pipe\kiri".to_owned())
}

fn connect() -> io::Result<Arc<Pipe>> {
    Pipe::open(PIPE.get().map_or(r"\\.\pipe\kiri", String::as_str)).map(Arc::new)
}

/// A connection as a byte stream. With `timeout`, a read that gets nothing
/// for that long fails (TimedOut) instead of waiting on: for the ping, which
/// gates the engine thread at start and must never hang it.
struct Conn(Arc<Pipe>, Option<Duration>);

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_client(buf, self.1)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Write::write(&mut &*self.0, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An answer: status and body.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The daemon's `{"message": …}` on failure, or the body as text.
    pub fn error(&self) -> String {
        self.json()["message"].as_str().map(str::to_owned).unwrap_or_else(|| String::from_utf8_lossy(&self.body).into_owned())
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

fn head(r: &mut impl BufRead) -> io::Result<(u16, Option<usize>, bool)> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Kiri is not answering"))?;
    let (mut len, mut chunked) = (None, false);
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            match k.to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().ok(),
                "transfer-encoding" => chunked = v.trim().eq_ignore_ascii_case("chunked"),
                _ => {}
            }
        }
    }
    Ok((status, len, chunked))
}

/// Reads one chunk of a chunked body; `None` at the end.
fn chunk(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk"))?;
    if n == 0 {
        return Ok(None);
    }
    let mut buf = vec![0; n + 2];
    r.read_exact(&mut buf)?;
    buf.truncate(n);
    Ok(Some(buf))
}

/// One request; the whole body is read, chunked or not.
/// A value for the request line must not carry a line break, or it would
/// inject headers or a second request.
fn line_safe(v: &str) -> io::Result<()> {
    if v.contains(['\r', '\n']) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "request contains a line break"));
    }
    Ok(())
}

pub fn request(method: &str, path: &str, body: Option<&Value>) -> io::Result<Response> {
    request_with(method, path, body, false)
}

/// The header that marks a request as the window's own (`background`):
/// refreshing a list never counts as use of the engine.
fn background_header(background: bool) -> &'static str {
    if background { "X-Kiri-Background: 1\r\n" } else { "" }
}

/// One request, marked as the window's own when `background`.
pub fn request_with(method: &str, path: &str, body: Option<&Value>, background: bool) -> io::Result<Response> {
    request_within(method, path, body, background, None)
}

/// How long Start's ping waits for its answer: as long as the bridge may
/// take to start the engine (crates/kiri/src/bridge.rs, 25 s), then 5 s
/// for the answer, and no more, so it never hangs the engine thread.
const PING_WAIT: Duration = Duration::from_secs(30);

/// How long the window's own background calls wait (its lists, the engine
/// panel, the event watcher's ping): while the bridge starts the engine,
/// they fail soon rather than hold the engine thread for its 25 s, so a
/// Stop or Start clicked meanwhile runs next.
const QUICK: Duration = Duration::from_secs(3);

/// `request_with`, failing (TimedOut) when a read waits longer than `timeout`.
pub fn request_within(method: &str, path: &str, body: Option<&Value>, background: bool, timeout: Option<Duration>) -> io::Result<Response> {
    line_safe(path)?;
    let mut s = Conn(connect()?, timeout);
    let payload = body.map(Value::to_string).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: kiri\r\nConnection: close\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
        background_header(background),
        payload.len()
    )?;
    let mut r = BufReader::new(s);
    let (status, len, chunked) = head(&mut r)?;
    let mut body = Vec::new();
    if status == 204 || status == 304 {
        return Ok(Response { status, body });
    }
    if chunked {
        while let Some(c) = chunk(&mut r)? {
            body.extend_from_slice(&c);
        }
    } else if let Some(n) = len {
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
    }
    Ok(Response { status, body })
}

/// Kiri's own version from a `/version` answer: `KiriVersion`, or `Version`
/// from an engine older than that field (it reported Kiri's version there;
/// it is now the Docker Engine version clients gate on).
pub fn kiri_version(version: &Value) -> Option<&str> {
    version["KiriVersion"].as_str().or_else(|| version["Version"].as_str())
}

pub fn get(path: &str) -> io::Result<Value> {
    let r = request("GET", path, None)?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    Ok(r.json())
}

/// A GET of the window's own refresh, failing after QUICK without an answer.
pub fn get_quick(path: &str) -> io::Result<Value> {
    let r = request_within("GET", path, None, false, Some(QUICK))?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    Ok(r.json())
}

/// A GET marked as the window's own, which doesn't count as use (as the
/// Mac's `get_background`, for crates/kiri/src/builds.rs).
pub fn get_background(path: &str) -> io::Result<Value> {
    let r = request_with("GET", path, None, true)?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    Ok(r.json())
}

/// The docker CLI Kiri ships, as the Mac's `docker_cli` (for builds.rs).
pub fn docker_cli() -> Option<String> {
    Some(crate::detail::docker_cli())
}

/// A POST whose answer is a stream of JSON lines (`/images/create`), each
/// given to `on_line` as it arrives; returns the HTTP status. As the Mac's
/// client::post_lines.
pub fn post_lines(path: &str, on_line: impl FnMut(Value)) -> io::Result<u16> {
    post_lines_with(path, None, on_line)
}

/// `post_lines` with a registry sign-in (`X-Registry-Auth`, base64url) for a pull.
pub fn post_lines_with(path: &str, auth: Option<&str>, on_line: impl FnMut(Value)) -> io::Result<u16> {
    post_lines_closable(path, auth, |_| {}, on_line)
}

/// `post_lines_with`, handing `opened` a Closer for the connection once it
/// is made, so another thread can stop the request (Cancel on a pull). A
/// stopped read ends as if the body had.
pub fn post_lines_closable(path: &str, auth: Option<&str>, opened: impl FnOnce(Closer), mut on_line: impl FnMut(Value)) -> io::Result<u16> {
    line_safe(path)?;
    if let Some(a) = auth {
        line_safe(a)?;
    }
    let auth = auth.map(|a| format!("X-Registry-Auth: {a}\r\n")).unwrap_or_default();
    let pipe = connect()?;
    opened(Closer(pipe.clone()));
    let mut s = Conn(pipe, None);
    write!(s, "POST {path} HTTP/1.1\r\nHost: kiri\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: 0\r\n\r\n")?;
    let mut r = BufReader::new(s);
    let (status, len, chunked) = head(&mut r)?;
    let mut pending = Vec::new();
    let mut lines = |pending: &mut Vec<u8>| {
        while let Some(i) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=i).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                on_line(v);
            }
        }
    };
    if chunked {
        while let Some(c) = chunk(&mut r)? {
            pending.extend_from_slice(&c);
            lines(&mut pending);
        }
    } else if let Some(n) = len {
        pending.resize(n, 0);
        r.read_exact(&mut pending)?;
    } else if status != 204 {
        r.read_to_end(&mut pending)?;
    }
    // A last line with no newline, or a plain error body.
    pending.push(b'\n');
    lines(&mut pending);
    Ok(status)
}

/// Whether the daemon answers at all, waiting for a start the bridge has
/// under way (Start).
pub fn alive() -> bool {
    request_within("GET", "/_ping", None, false, Some(PING_WAIT)).is_ok_and(|r| r.ok())
}

/// `alive`, for the event watcher: false after QUICK without an answer.
pub fn alive_quick() -> bool {
    request_within("GET", "/_ping", None, false, Some(QUICK)).is_ok_and(|r| r.ok())
}

/// A long-lived streamed response (`/events`, `logs?follow=1`, `stats`),
/// read chunk by chunk. `closer` ends it from another thread.
#[derive(Debug)]
pub struct Stream {
    pipe: Arc<Pipe>,
    reader: BufReader<ConnReader>,
}

/// Debug for the reader half.
struct ConnReader(Conn);

impl std::fmt::Debug for ConnReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnReader")
    }
}

impl Read for ConnReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

/// Ends a Stream's wait from another thread.
#[derive(Debug, Clone)]
pub struct Closer(Arc<Pipe>);

impl Closer {
    pub fn close(&self) {
        self.0.cancel();
    }
}

impl Stream {
    pub fn open(path: &str) -> io::Result<Stream> {
        line_safe(path)?;
        let pipe = connect()?;
        let mut s = Conn(pipe.clone(), None);
        write!(s, "GET {path} HTTP/1.1\r\nHost: kiri\r\n\r\n")?;
        let mut reader = BufReader::new(ConnReader(s));
        let (status, _, chunked) = head(&mut reader)?;
        if status != 200 || !chunked {
            return Err(io::Error::other(format!("{path}: HTTP {status}")));
        }
        Ok(Stream { pipe, reader })
    }

    pub fn closer(&self) -> Closer {
        Closer(self.pipe.clone())
    }

    pub fn next(&mut self) -> Option<Vec<u8>> {
        chunk(&mut self.reader).ok().flatten()
    }
}

/// Calls `on_event` for each event until the connection ends or it returns false.
pub fn events(mut on_event: impl FnMut(Value) -> bool) -> io::Result<()> {
    let mut s = Stream::open("/events")?;
    let mut pending = Vec::new();
    while let Some(c) = s.next() {
        pending.extend_from_slice(&c);
        while let Some(i) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=i).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                if !on_event(v) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// A command started in a running container with a terminal: its output
/// arrives unframed, read from this, until it ends. `closer()` ends a read
/// that waits (a log that follows) from another thread; the exec itself
/// goes on in the container.
#[derive(Debug)]
pub struct Exec {
    pub id: String,
    pipe: Arc<Pipe>,
    reader: BufReader<ConnReader>,
}

impl Exec {
    pub fn closer(&self) -> Closer {
        Closer(self.pipe.clone())
    }
}

impl Read for Exec {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl BufRead for Exec {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.reader.fill_buf()
    }

    fn consume(&mut self, n: usize) {
        self.reader.consume(n)
    }
}

/// Starts `cmd` in a running container, through the exec API with a
/// terminal; `background` marks it as the window's own.
pub fn exec_start(container: &str, cmd: &[&str], background: bool) -> io::Result<Exec> {
    line_safe(container)?;
    let created = request_with("POST", &format!("/containers/{container}/exec"), Some(&serde_json::json!({"Cmd": cmd, "AttachStdout": true, "AttachStderr": true, "Tty": true})), background)?;
    if !created.ok() {
        return Err(io::Error::other(created.error()));
    }
    let id = created.json()["Id"].as_str().unwrap_or_default().to_owned();
    let pipe = connect()?;
    let mut s = Conn(pipe.clone(), None);
    let body = r#"{"Detach":false,"Tty":true}"#;
    write!(
        s,
        "POST /exec/{id}/start HTTP/1.1\r\nHost: kiri\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        background_header(background),
        body.len()
    )?;
    let mut reader = BufReader::new(ConnReader(s));
    let (status, _, _) = head(&mut reader)?;
    if status != 101 && status != 200 {
        return Err(io::Error::other(format!("exec: HTTP {status}")));
    }
    Ok(Exec { id, pipe, reader })
}

/// Runs `cmd` in a running container and returns (exit code, output),
/// through the exec API with a terminal, so output arrives unframed.
pub fn exec(container: &str, cmd: &[&str]) -> io::Result<(i64, String)> {
    let mut e = exec_start(container, cmd, false)?;
    let mut out = Vec::new();
    e.read_to_end(&mut out)?;
    let code = get(&format!("/exec/{}/json", e.id))?["ExitCode"].as_i64().unwrap_or(-1);
    Ok((code, String::from_utf8_lossy(&out).replace("\r\n", "\n")))
}
