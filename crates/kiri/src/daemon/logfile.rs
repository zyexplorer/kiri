//! `docker logs` on disk, the way Docker's `json-file` and `local` drivers
//! keep them: each container's output in `containers/<id>/container.log`
//! beside `containers.json`, one record per line with its stream and the
//! time it arrived, rotated by size, removed with the container.
//!
//! A record is `[len u32][stream u8][time ns u64][line][len u32]` (big
//! endian, `len` = 9 + the line's length), the shape of Docker's `local`
//! driver: binary-safe, and the length at the end lets `--tail` walk back
//! from the end of a file without reading the rest. Lines longer than 16
//! KiB are split, as Docker splits them.
//!
//! Writes go to a buffer and reach the file every 32 KiB or second, never
//! with an fsync: a crash of the Mac loses at most that much. Readers flush
//! it first, so `docker logs` always sees everything.
//!
//! The default is 10 MB × 3 files a container. Docker's `local` driver
//! keeps 20 MB × 5, but these files sit on the Mac's own disk beside
//! everything else, and a dozen chatty dev containers at 100 MB each add up;
//! 30 MB of 17-byte-overhead records is still hundreds of thousands of lines.
//!
//! A `--rm` container started with a client attached (`docker run --rm`
//! without `-d`) keeps its newest 1 MiB in memory instead: its client sees
//! the output as it comes, and the files would be deleted with the container
//! as it exits, unread. Writing them cost 5–7% of output throughput on
//! Windows (a 200 MB `docker run` wrote 200 MB to the distro's disk), for
//! nothing. `docker logs` on it while it runs still works, from memory.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use kiri_proto::Stream;

pub const DEFAULT_MAX_SIZE: u64 = 10 << 20;
pub const DEFAULT_MAX_FILE: u32 = 3;
/// Docker splits lines longer than this into several messages.
const MAX_LINE: usize = 16 << 10;
/// Anything claiming to be longer is a torn or foreign record.
const MAX_RECORD: u32 = (MAX_LINE as u32) + 9;
const FLUSH_BYTES: usize = 32 << 10;
const FLUSH_AFTER: Duration = Duration::from_secs(1);
const FILE: &str = "container.log";
/// How much output a container kept in memory holds.
const RING: usize = 1 << 20;

/// One line of output (or a piece of one, without its newline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub stream: Stream,
    /// Unix nanoseconds.
    pub time: u64,
    pub data: Vec<u8>,
}

/// `HostConfig.LogConfig` as given at create (`--log-driver`, `--log-opt`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            kind: "json-file".into(),
            config: BTreeMap::new(),
        }
    }
}

impl LogConfig {
    /// From a create body's `HostConfig.LogConfig`. Every driver but `none`
    /// is kept as `json-file` would keep it (Docker's "dual logging" does
    /// the same for drivers it cannot read back); the type is still what
    /// inspect shows.
    pub fn from_host(v: &serde_json::Value) -> Result<LogConfig, String> {
        let kind = match v["Type"].as_str() {
            Some("") | None => "json-file".to_owned(),
            Some(t) => t.to_owned(),
        };
        let mut config = BTreeMap::new();
        if let Some(m) = v["Config"].as_object() {
            for (k, v) in m {
                let v = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                config.insert(k.clone(), v);
            }
        }
        let c = LogConfig { kind, config };
        c.limits()?;
        Ok(c)
    }

    /// Whether `docker logs` can read it back: not with `none`.
    pub fn readable(&self) -> bool {
        self.kind != "none"
    }

    /// (max-size in bytes, max-file), from the options or the defaults.
    pub fn limits(&self) -> Result<(u64, u32), String> {
        let size = match self.config.get("max-size").map(String::as_str) {
            None | Some("") | Some("-1") => DEFAULT_MAX_SIZE,
            Some(s) => {
                let n = parse_size(s).ok_or_else(|| format!("invalid size: '{s}'"))?;
                if n == 0 {
                    return Err("max-size must be a positive number".into());
                }
                n
            }
        };
        let files = match self.config.get("max-file").map(String::as_str) {
            None | Some("") => DEFAULT_MAX_FILE,
            Some(s) => {
                let n: i64 = s
                    .parse()
                    .map_err(|_| format!("strconv.Atoi: parsing \"{s}\": invalid syntax"))?;
                if n < 1 {
                    return Err("max-file cannot be less than 1".into());
                }
                n.min(1000) as u32
            }
        };
        Ok((size, files))
    }
}

/// `10m`, `1.5g`, `500k`, `2048`, `20MB`: binary units, as Docker's
/// `units.RAMInBytes` reads them.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(end);
    let n: f64 = num.parse().ok()?;
    let unit = unit.trim_start().to_ascii_lowercase();
    let unit = unit.strip_suffix('b').unwrap_or(&unit);
    let unit = unit.strip_suffix('i').unwrap_or(unit);
    let mult: u64 = match unit {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        "p" => 1 << 50,
        _ => return None,
    };
    Some((n * mult as f64) as u64)
}

/// Where a container's logs are kept.
pub fn dir(root: &Path, id: &str) -> PathBuf {
    root.join("containers").join(id)
}

/// Splits a chunk of output into records: one per line (with its newline),
/// a trailing piece without one, and lines over 16 KiB in pieces.
pub fn split(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(rest.len(), |i| i + 1)
            .min(MAX_LINE);
        let (line, more) = rest.split_at(end);
        rest = more;
        Some(line)
    })
}

fn encode(out: &mut Vec<u8>, stream: Stream, time: u64, data: &[u8]) {
    let len = (9 + data.len()) as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.push(stream as u8);
    out.extend_from_slice(&time.to_be_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&len.to_be_bytes());
}

fn stream_of(b: u8) -> Option<Stream> {
    match b {
        1 => Some(Stream::Stdout),
        2 => Some(Stream::Stderr),
        _ => None,
    }
}

/// A container's log: the file it appends to and who follows it live.
#[derive(Debug)]
pub struct Log {
    dir: PathBuf,
    max_size: u64,
    max_file: u32,
    /// `--log-driver none`: nothing is kept.
    off: bool,
    /// Removed with its container: nothing more is written.
    gone: bool,
    file: Option<File>,
    /// Bytes in the current file, flushed or not.
    size: u64,
    buf: Vec<u8>,
    flushed: Instant,
    /// `docker logs -f` readers, sent each record as it is kept.
    followers: Vec<Sender<Record>>,
    /// Kept in memory, not on disk (see `keep_in_memory`).
    ring: Option<Ring>,
}

/// The newest records, encoded as in a file, and where the oldest kept
/// one starts.
#[derive(Debug, Default)]
struct Ring {
    buf: Vec<u8>,
    start: usize,
}

impl Ring {
    fn push(&mut self, stream: Stream, time: u64, line: &[u8]) {
        encode(&mut self.buf, stream, time, line);
        // Drop whole records from the front until it fits again.
        while self.buf.len() - self.start > RING {
            let n = u32::from_be_bytes(
                self.buf[self.start..self.start + 4]
                    .try_into()
                    .expect("4 bytes"),
            ) as usize;
            self.start += 8 + n;
        }
        if self.start >= RING {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }

    fn records(&self) -> &[u8] {
        &self.buf[self.start..]
    }
}

impl Log {
    pub fn new(dir: PathBuf, config: &LogConfig) -> Log {
        let (max_size, max_file) = config
            .limits()
            .unwrap_or((DEFAULT_MAX_SIZE, DEFAULT_MAX_FILE));
        Log {
            dir,
            max_size,
            max_file,
            off: !config.readable(),
            gone: false,
            file: None,
            size: 0,
            buf: Vec::new(),
            flushed: Instant::now(),
            followers: Vec::new(),
            ring: None,
        }
    }

    /// Keeps output in memory from now on rather than on disk: for a `--rm`
    /// container whose client is attached as it starts. Only before
    /// anything has been written (an `--rm` container starts once).
    pub fn keep_in_memory(&mut self) {
        if !self.off && self.file.is_none() && self.size == 0 {
            self.ring = Some(Ring::default());
        }
    }

    /// The file inspect calls `LogPath`; empty with `none`.
    pub fn path(&self) -> String {
        if self.off {
            String::new()
        } else {
            self.dir.join(FILE).display().to_string()
        }
    }

    /// Keeps a chunk of output, a record per line, and passes them to followers.
    pub fn append(&mut self, stream: Stream, time: u64, data: &[u8]) {
        if self.gone {
            return;
        }
        for line in split(data) {
            if !self.followers.is_empty() {
                let rec = Record {
                    stream,
                    time,
                    data: line.to_vec(),
                };
                self.followers.retain(|tx| tx.send(rec.clone()).is_ok());
            }
            if self.off {
                continue;
            }
            if let Some(ring) = &mut self.ring {
                ring.push(stream, time, line);
                continue;
            }
            if self.file.is_none() && self.open().is_err() {
                continue;
            }
            let len = 17 + line.len() as u64;
            if self.size > 0 && self.size + len > self.max_size {
                self.rotate();
                if self.file.is_none() && self.open().is_err() {
                    continue;
                }
            }
            encode(&mut self.buf, stream, time, line);
            self.size += len;
        }
        if self.buf.len() >= FLUSH_BYTES || self.flushed.elapsed() >= FLUSH_AFTER {
            self.flush();
        }
    }

    /// Writes what is buffered (no fsync).
    pub fn flush(&mut self) {
        self.flushed = Instant::now();
        if self.buf.is_empty() {
            return;
        }
        let Some(f) = self.file.as_mut() else {
            self.buf.clear();
            return;
        };
        if let Err(e) = f.write_all(&self.buf) {
            crate::diag::warn!(
                "writing container log failed",
                dir = self.dir.display(),
                error = e
            );
            // Start again from what is on disk.
            self.file = None;
        }
        self.buf.clear();
    }

    /// Ends every `docker logs -f`: the container stopped.
    pub fn end_follow(&mut self) {
        self.followers.clear();
        self.flush();
    }

    /// Deletes the files: the container is gone.
    pub fn remove(&mut self) {
        self.gone = true;
        self.followers.clear();
        self.buf.clear();
        self.ring = None;
        self.file = None;
        if let Err(e) = fs::remove_dir_all(&self.dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            crate::diag::warn!(
                "removing container log failed",
                dir = self.dir.display(),
                error = e
            );
        }
    }

    /// Everything kept so far, and (if `follow`) a subscription to what
    /// comes after it: taken under this log's lock, so nothing is missed or
    /// sent twice. The files are opened now, so rotation later does not
    /// move them under the reader.
    pub fn snapshot(&mut self, follow: Option<Sender<Record>>) -> Snapshot {
        self.flush();
        if let Some(tx) = follow {
            self.followers.push(tx);
        }
        let mut files = Vec::new();
        if self.off || self.gone {
            return Snapshot { files };
        }
        if let Some(ring) = &self.ring {
            // Read as a file like any other: one that is already unlinked.
            match self.memory_file(ring.records()) {
                Ok(f) => files.push((f, ring.records().len() as u64)),
                Err(e) => crate::diag::warn!(
                    "reading container log failed",
                    dir = self.dir.display(),
                    error = e
                ),
            }
            return Snapshot { files };
        }
        for i in (0..self.max_file).rev() {
            let p = self.file_path(i);
            if let Ok(f) = File::open(&p)
                && let Ok(m) = f.metadata()
            {
                let len = if i == 0 && self.file.is_some() {
                    self.size.min(m.len())
                } else {
                    m.len()
                };
                files.push((f, len));
            }
        }
        Snapshot { files }
    }

    /// `records` in a file of their own, deleted as soon as it is open.
    fn memory_file(&self, records: &[u8]) -> io::Result<File> {
        fs::create_dir_all(&self.dir)?;
        let p = self.dir.join(format!("{FILE}.memory"));
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(&p)?;
        let _ = fs::remove_file(&p);
        f.write_all(records)?;
        io::Seek::seek(&mut f, io::SeekFrom::Start(0))?;
        Ok(f)
    }

    fn file_path(&self, i: u32) -> PathBuf {
        if i == 0 {
            self.dir.join(FILE)
        } else {
            self.dir.join(format!("{FILE}.{i}"))
        }
    }

    fn open(&mut self) -> io::Result<()> {
        let open = || -> io::Result<File> {
            fs::create_dir_all(&self.dir)?;
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(self.dir.join(FILE))?;
            // A crash can leave half a record at the end: cut it off, so
            // what is appended next can be read.
            let len = f.metadata()?.len();
            let good = valid_end(&f, len);
            if good != len {
                f.set_len(good)?;
            }
            Ok(f)
        };
        match open() {
            Ok(f) => {
                self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
                self.file = Some(f);
                Ok(())
            }
            Err(e) => {
                crate::diag::warn!(
                    "opening container log failed",
                    dir = self.dir.display(),
                    error = e.to_string()
                );
                Err(e)
            }
        }
    }

    /// `container.log` → `.1` → `.2` …, dropping the oldest; with one file, it starts over.
    fn rotate(&mut self) {
        self.flush();
        self.file = None;
        self.size = 0;
        if self.max_file <= 1 {
            let _ = fs::remove_file(self.file_path(0));
            return;
        }
        let _ = fs::remove_file(self.file_path(self.max_file - 1));
        for i in (0..self.max_file - 1).rev() {
            let _ = fs::rename(self.file_path(i), self.file_path(i + 1));
        }
    }
}

/// Where a file's records end: its length if the last one is whole, else
/// the end of the last whole record found reading from the start.
fn valid_end(f: &File, len: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    if len >= 8 {
        let mut t = [0u8; 4];
        let mut h = [0u8; 4];
        if f.read_exact_at(&mut t, len - 4).is_ok() {
            let n = u32::from_be_bytes(t);
            if (9..=MAX_RECORD).contains(&n) && u64::from(n) + 8 <= len {
                let start = len - 8 - u64::from(n);
                if f.read_exact_at(&mut h, start).is_ok() && h == t {
                    return len;
                }
            }
        }
    }
    let Ok(f) = f.try_clone() else { return len };
    let mut r = Reader {
        files: vec![(f, len, 0)],
        cur: None,
        pos: 0,
    };
    while r.next().is_some() {}
    r.pos
}

/// A container's files as they were when `docker logs` asked, oldest first,
/// each with how much of it to read.
pub struct Snapshot {
    files: Vec<(File, u64)>,
}

impl Snapshot {
    /// The records from the `n`th last on, or all of them if `n` is `None`.
    /// The start is found walking back from the end, reading only the
    /// records' length fields, so a short tail of a big log is cheap.
    pub fn tail(self, n: Option<usize>) -> Reader {
        let mut start = (0, 0);
        if let Some(n) = n {
            start = (self.files.len(), 0);
            let mut left = n;
            'files: for (i, (f, len)) in self.files.iter().enumerate().rev() {
                if left == 0 {
                    break;
                }
                let starts = match back_starts(f, *len, left) {
                    Some(s) => s,
                    // Not walkable backwards: read it forwards.
                    None => forward_starts(f, *len),
                };
                if starts.len() >= left {
                    start = (i, starts[starts.len() - left]);
                    break 'files;
                }
                left -= starts.len();
                start = (i, 0);
            }
        }
        let (i, off) = start;
        Reader {
            files: self
                .files
                .into_iter()
                .skip(i)
                .enumerate()
                .map(|(j, (f, l))| (f, l, if j == 0 { off } else { 0 }))
                .collect(),
            cur: None,
            pos: 0,
        }
    }
}

const BLOCK: u64 = 64 << 10;

/// The start offsets of up to the last `want` records of a file, in file
/// order; `None` if a length does not add up.
fn back_starts(f: &File, len: u64, want: usize) -> Option<Vec<u64>> {
    let mut block: Vec<u8> = Vec::new();
    let mut block_at = 0u64;
    let mut read_u32 = |pos: u64| -> Option<u32> {
        if pos < block_at || pos + 4 > block_at + block.len() as u64 {
            let end = (pos + 4).min(len);
            let from = end.saturating_sub(BLOCK);
            block.resize((end - from) as usize, 0);
            f.read_exact_at(&mut block, from).ok()?;
            block_at = from;
        }
        let i = (pos - block_at) as usize;
        Some(u32::from_be_bytes(block[i..i + 4].try_into().ok()?))
    };
    let mut starts = Vec::new();
    let mut pos = len;
    while pos > 0 && starts.len() < want {
        if pos < 8 {
            return None;
        }
        let n = read_u32(pos - 4)?;
        if !(9..=MAX_RECORD).contains(&n) || u64::from(n) + 8 > pos {
            return None;
        }
        let start = pos - 8 - u64::from(n);
        if read_u32(start)? != n {
            return None;
        }
        starts.push(start);
        pos = start;
    }
    starts.reverse();
    Some(starts)
}

fn forward_starts(f: &File, len: u64) -> Vec<u64> {
    let Ok(f) = f.try_clone() else {
        return Vec::new();
    };
    let mut r = Reader {
        files: vec![(f, len, 0)],
        cur: None,
        pos: 0,
    };
    let mut starts = Vec::new();
    loop {
        let at = r.pos;
        if r.next().is_none() {
            break;
        }
        starts.push(at);
    }
    starts
}

/// Reads records forwards through a snapshot's files.
pub struct Reader {
    /// (file, length to read, offset to start at), the next file first.
    files: Vec<(File, u64, u64)>,
    cur: Option<(io::Take<BufReader<File>>, u64)>,
    /// Offset in the current file after the last whole record.
    pos: u64,
}

impl Iterator for Reader {
    type Item = Record;

    fn next(&mut self) -> Option<Record> {
        loop {
            if self.cur.is_none() {
                if self.files.is_empty() {
                    return None;
                }
                let (f, len, off) = self.files.remove(0);
                let mut r = BufReader::with_capacity(64 << 10, f);
                if off > 0 && std::io::Seek::seek(&mut r, io::SeekFrom::Start(off)).is_err() {
                    continue;
                }
                self.pos = off;
                self.cur = Some((r.take(len.saturating_sub(off)), len));
            }
            let (r, _) = self.cur.as_mut().expect("set");
            match read_record(r) {
                Some(rec) => {
                    self.pos += 17 + rec.data.len() as u64;
                    return Some(rec);
                }
                // The end of this file, or a torn record: on to the next.
                None => self.cur = None,
            }
        }
    }
}

fn read_record(r: &mut impl Read) -> Option<Record> {
    let mut head = [0u8; 13];
    r.read_exact(&mut head).ok()?;
    let n = u32::from_be_bytes(head[..4].try_into().ok()?);
    if !(9..=MAX_RECORD).contains(&n) {
        return None;
    }
    let stream = stream_of(head[4])?;
    let time = u64::from_be_bytes(head[5..13].try_into().ok()?);
    let mut data = vec![0u8; n as usize - 9];
    r.read_exact(&mut data).ok()?;
    let mut tail = [0u8; 4];
    r.read_exact(&mut tail).ok()?;
    (u32::from_be_bytes(tail) == n).then_some(Record { stream, time, data })
}

/// `since`/`until` as the CLI sends them: Unix seconds with an optional
/// fraction (`1700000000.123456789`), as nanoseconds; 0 or empty is none.
pub fn parse_time(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (secs, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs: i64 = secs.parse().ok()?;
    let mut digits: String = frac
        .chars()
        .take_while(char::is_ascii_digit)
        .take(9)
        .collect();
    while digits.len() < 9 {
        digits.push('0');
    }
    let nanos: u64 = digits.parse().ok()?;
    let t = u64::try_from(secs)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(nanos)?;
    (t > 0).then_some(t)
}

/// `tail`: a number of lines, or everything (`all`, empty, or anything
/// that is not a number, as Docker reads it).
pub fn parse_tail(s: Option<&str>) -> Option<usize> {
    s.and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&n| n >= 0)
        .map(|n| n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kiri-logtest-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn config(size: &str, files: &str) -> LogConfig {
        let mut c = LogConfig::default();
        c.config.insert("max-size".into(), size.into());
        c.config.insert("max-file".into(), files.into());
        c
    }

    fn text(r: impl Iterator<Item = Record>) -> String {
        r.map(|r| String::from_utf8(r.data).unwrap()).collect()
    }

    #[test]
    fn sizes_and_options() {
        assert_eq!(parse_size("10m"), Some(10 << 20));
        assert_eq!(parse_size("20MB"), Some(20 << 20));
        assert_eq!(parse_size("1.5k"), Some(1536));
        assert_eq!(parse_size("2048"), Some(2048));
        assert_eq!(parse_size("1g"), Some(1 << 30));
        assert_eq!(parse_size("10x"), None);
        assert_eq!(
            LogConfig::default().limits(),
            Ok((DEFAULT_MAX_SIZE, DEFAULT_MAX_FILE))
        );
        assert_eq!(config("1m", "5").limits(), Ok((1 << 20, 5)));
        assert!(config("1m", "0").limits().is_err());
        assert!(config("big", "2").limits().is_err());
        let c = LogConfig::from_host(
            &serde_json::json!({"Type": "syslog", "Config": {"syslog-address": "udp://x:514"}}),
        )
        .unwrap();
        assert_eq!((c.kind.as_str(), c.readable()), ("syslog", true));
        assert_eq!(
            LogConfig::from_host(&serde_json::Value::Null).unwrap(),
            LogConfig::default()
        );
        assert!(
            !LogConfig::from_host(&serde_json::json!({"Type": "none"}))
                .unwrap()
                .readable()
        );
    }

    #[test]
    fn times_and_tails() {
        assert_eq!(parse_time("1700000000.5"), Some(1_700_000_000_500_000_000));
        assert_eq!(
            parse_time("1700000000.000000001"),
            Some(1_700_000_000_000_000_001)
        );
        assert_eq!(parse_time("1700000000"), Some(1_700_000_000_000_000_000));
        assert_eq!(parse_time("0"), None);
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_tail(Some("all")), None);
        assert_eq!(parse_tail(Some("0")), Some(0));
        assert_eq!(parse_tail(Some("50")), Some(50));
        assert_eq!(parse_tail(None), None);
    }

    #[test]
    fn splits_lines() {
        let parts: Vec<&[u8]> = split(b"a\nbb\nccc").collect();
        assert_eq!(parts, [&b"a\n"[..], b"bb\n", b"ccc"]);
        let long = vec![b'x'; MAX_LINE + 10];
        assert_eq!(
            split(&long).map(<[u8]>::len).collect::<Vec<_>>(),
            [MAX_LINE, 10]
        );
    }

    #[test]
    fn keeps_tails_rotates_and_survives_reopen() {
        let d = tmp("rotate");
        let mut log = Log::new(d.clone(), &config("1k", "3"));
        for i in 0..200 {
            log.append(
                if i % 2 == 0 {
                    Stream::Stdout
                } else {
                    Stream::Stderr
                },
                i,
                format!("line {i}\n").as_bytes(),
            );
        }
        log.flush();
        assert!(
            d.join("container.log.1").exists()
                && d.join("container.log.2").exists()
                && !d.join("container.log.3").exists()
        );
        let all: Vec<Record> = log.snapshot(None).tail(None).collect();
        // The oldest were rotated away; what is left runs to the end in order.
        assert!(all.len() > 50 && all.len() < 200, "{}", all.len());
        assert_eq!(all.last().unwrap().data, b"line 199\n");
        assert!(all.windows(2).all(|w| w[0].time + 1 == w[1].time));
        assert_eq!(
            text(log.snapshot(None).tail(Some(3))),
            "line 197\nline 198\nline 199\n"
        );
        assert_eq!(log.snapshot(None).tail(Some(0)).count(), 0);
        // A tail reaching into the rotated files.
        let n = all.len() - 2;
        assert_eq!(log.snapshot(None).tail(Some(n)).next().unwrap(), all[2]);
        assert_eq!(log.snapshot(None).tail(Some(100_000)).count(), all.len());
        // A new daemon carries on from the same files.
        drop(log);
        let mut log = Log::new(d.clone(), &config("1k", "3"));
        log.append(Stream::Stdout, 1000, b"after\n");
        assert_eq!(text(log.snapshot(None).tail(Some(2))), "line 199\nafter\n");
        log.remove();
        assert!(!d.exists());
    }

    #[test]
    fn keeps_the_newest_in_memory() {
        let d = tmp("memory");
        let mut log = Log::new(d.clone(), &LogConfig::default());
        log.keep_in_memory();
        let (tx, rx) = mpsc::channel();
        let _ = log.snapshot(Some(tx));
        let n = 3 * RING / 20;
        for i in 0..n {
            log.append(
                Stream::Stdout,
                i as u64,
                format!("line {i:09}\n").as_bytes(),
            );
        }
        log.flush();
        // Nothing on disk, followers still fed.
        assert!(!d.join(FILE).exists());
        assert_eq!(rx.try_iter().count(), n);
        let all: Vec<Record> = log.snapshot(None).tail(None).collect();
        let kept: usize = all.iter().map(|r| 17 + r.data.len()).sum();
        assert!(kept <= RING && kept > RING - 64, "{kept}");
        assert_eq!(
            all.last().unwrap().data,
            format!("line {:09}\n", n - 1).as_bytes()
        );
        assert!(all.windows(2).all(|w| w[0].time + 1 == w[1].time));
        assert_eq!(
            text(log.snapshot(None).tail(Some(2))),
            format!("line {:09}\nline {:09}\n", n - 2, n - 1)
        );
        // Long lines are split and dropped whole, as on disk.
        log.append(Stream::Stderr, 1 << 40, &vec![b'x'; 3 * MAX_LINE]);
        let last: Vec<Record> = log.snapshot(None).tail(Some(3)).collect();
        assert!(
            last.iter()
                .all(|r| r.stream == Stream::Stderr && r.data.len() == MAX_LINE)
        );
        assert!(!d.join(format!("{FILE}.memory")).exists());
        log.remove();
        assert!(log.snapshot(None).tail(None).next().is_none());
    }

    #[test]
    fn memory_only_before_anything_is_on_disk() {
        let d = tmp("memory-late");
        let mut log = Log::new(d.clone(), &LogConfig::default());
        log.append(Stream::Stdout, 1, b"on disk\n");
        log.keep_in_memory();
        log.append(Stream::Stdout, 2, b"still on disk\n");
        log.flush();
        assert_eq!(
            text(log.snapshot(None).tail(None)),
            "on disk\nstill on disk\n"
        );
        assert!(d.join(FILE).exists());
        log.remove();
    }

    #[test]
    fn follow_has_no_gap_or_repeat() {
        let d = tmp("follow");
        let mut log = Log::new(d, &LogConfig::default());
        log.append(Stream::Stdout, 1, b"one\ntw");
        let (tx, rx) = mpsc::channel();
        let snap = log.snapshot(Some(tx));
        log.append(Stream::Stdout, 2, b"o\nthree\n");
        log.end_follow();
        assert_eq!(text(snap.tail(None)), "one\ntw");
        assert_eq!(text(rx.iter()), "o\nthree\n");
        log.remove();
    }

    #[test]
    fn torn_end_is_cut() {
        let d = tmp("torn");
        let mut log = Log::new(d.clone(), &LogConfig::default());
        log.append(Stream::Stdout, 1, b"whole\n");
        log.flush();
        drop(log);
        OpenOptions::new()
            .append(true)
            .open(d.join(FILE))
            .unwrap()
            .write_all(&[0, 0, 0, 20, 1, 2])
            .unwrap();
        let mut log = Log::new(d.clone(), &LogConfig::default());
        log.append(Stream::Stderr, 2, b"next\n");
        assert_eq!(text(log.snapshot(None).tail(None)), "whole\nnext\n");
        log.remove();
    }

    #[test]
    fn none_keeps_nothing() {
        let d = tmp("none");
        let mut log = Log::new(
            d.clone(),
            &LogConfig {
                kind: "none".into(),
                config: BTreeMap::new(),
            },
        );
        log.append(Stream::Stdout, 1, b"x\n");
        log.flush();
        assert!(!d.exists());
        assert_eq!(log.path(), "");
        assert_eq!(log.snapshot(None).tail(None).count(), 0);
    }
}
