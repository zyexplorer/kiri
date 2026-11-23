//! `~/.kiri/daemon.json`: Kiri's own daemon.json, with Docker's key names,
//! so a setup copied from Docker's file works unchanged. Kiri reads the
//! registry keys (`insecure-registries`, `registry-mirrors`; see
//! kiri_image::config) and keeps every other key as it finds it. Docker
//! Desktop's daemon.json is never read or written.
//!
//! The file is the daemon's, because pulls, pushes and logins happen here;
//! Settings → Registries and `kiri registry` change it through the API
//! (`GET`/`POST /kiri/registries`). An edit by hand applies at the next
//! pull, push or login, without a restart: the file is read again when it
//! changes (its size or time), and otherwise not at all.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use kiri_image::{Insecure, Mirror, RegistryConfig};
use serde_json::{Map, Value, json};

pub const INSECURE: &str = "insecure-registries";
pub const MIRRORS: &str = "registry-mirrors";

/// Enough mirrors to fail over a few times, few enough that a pull does not
/// spend its time asking one dead address after another.
const MAX_MIRRORS: usize = 16;
/// More insecure hosts than anyone lists.
const MAX_INSECURE: usize = 256;
/// A daemon.json is a few lines; this is far more than any real one.
const MAX_BYTES: u64 = 256 << 10;
/// Problems kept and shown; the rest are counted.
const MAX_PROBLEMS: usize = 32;
/// The longest entry either list is read from: a mirror's address with a
/// long host and path is about half this. A longer one is said to be wrong
/// without being repeated back.
const MAX_ENTRY: usize = 600;

pub fn path() -> Option<PathBuf> {
    super::state_dir().ok().map(|d| d.join("daemon.json"))
}

/// What identifies one version of the file: its time, size and, where there
/// is one, inode (a file renamed into place is a new one).
type Stamp = (SystemTime, u64, u64);

fn stamp_of(m: &std::fs::Metadata) -> Stamp {
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(m);
    #[cfg(not(unix))]
    let ino = 0;
    (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len(), ino)
}

/// The file's bytes, if it is a regular file of a sane size. Opened without
/// waiting (a FIFO would wait for a writer) and then looked at, the file
/// that was opened rather than whatever the path names by then: a FIFO or
/// `/dev/zero` (which never ends) is refused before anything is read.
fn read_limited(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    let file = options.open(path)?;
    let m = file.metadata()?;
    if !m.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    if m.len() > MAX_BYTES {
        return Err(std::io::Error::other(format!("larger than {} KiB", MAX_BYTES >> 10)));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(std::io::Error::other(format!("larger than {} KiB", MAX_BYTES >> 10)));
    }
    Ok(bytes)
}

/// The file's registry settings and what was wrong with it, as last read.
#[derive(Default)]
struct Read {
    stamp: Option<Stamp>,
    config: Arc<RegistryConfig>,
    /// Every mirror the file lists, in order, those not used included (a
    /// plain-HTTP one whose host is not listed), so Settings and `kiri
    /// registry mirror remove` can take one out.
    listed: Vec<Mirror>,
    problems: Vec<String>,
}

static READ: Mutex<Option<Read>> = Mutex::new(None);

/// What the file says now. A stat per call; read and parsed again only when
/// it changed. No file, or one Kiri can't read, is no settings.
pub fn current() -> Arc<RegistryConfig> {
    with_current(|r| r.config.clone())
}

fn with_current<T>(f: impl FnOnce(&Read) -> T) -> T {
    let path = path();
    let stamp = path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| stamp_of(&m));
    let mut slot = READ.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_none_or(|r| r.stamp != stamp) {
        let (config, problems, listed) = match (&path, stamp) {
            (Some(p), Some(_)) => match read_limited(p) {
                Ok(bytes) => parse_all(&bytes),
                Err(e) => (RegistryConfig::default(), vec![format!("{} could not be read: {e}", p.display())], Vec::new()),
            },
            _ => (RegistryConfig::default(), Vec::new(), Vec::new()),
        };
        for p in &problems {
            crate::diag::warn!("daemon.json", problem = p.as_str());
        }
        *slot = Some(Read { stamp, config: Arc::new(config), listed, problems });
    }
    f(slot.as_ref().expect("just set"))
}

/// The registry settings in a daemon.json, and what in it was left out and
/// why. As Docker, a key of the wrong kind is an error; unlike Docker, the
/// daemon keeps running with the entries it could read and says what it
/// skipped (Settings and `kiri registry` show it).
fn parse(bytes: &[u8]) -> (RegistryConfig, Vec<String>) {
    let (config, problems, _) = parse_all(bytes);
    (config, problems)
}

/// [`parse`], and every mirror the file lists, the unused ones included.
fn parse_all(bytes: &[u8]) -> (RegistryConfig, Vec<String>, Vec<Mirror>) {
    let mut problems = Problems::default();
    let v: Value = match serde_json::from_slice(bytes) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => return (RegistryConfig::default(), vec!["daemon.json is not a JSON object".into()], Vec::new()),
        Err(e) => return (RegistryConfig::default(), vec![format!("daemon.json is not valid JSON: {e}")], Vec::new()),
    };
    let mut read = |key: &str| strings(&v, key, &mut problems);
    let (insecure, mirrors) = (read(INSECURE), read(MIRRORS));
    let mut parsed = Vec::new();
    for s in insecure {
        match entry(&s).and_then(Insecure::parse) {
            Ok(e) => parsed.push(e),
            Err(e) => problems.push(|| format!("{INSECURE}: {e}")),
        }
    }
    let insecure = capped(parsed.into_iter(), INSECURE, MAX_INSECURE, &mut problems);
    let mut parsed = Vec::new();
    for s in mirrors {
        match entry(&s).and_then(Mirror::parse) {
            Ok(m) => parsed.push(m),
            Err(e) => problems.push(|| format!("{MIRRORS}: {e}")),
        }
    }
    let mirrors = capped(parsed.into_iter(), MIRRORS, MAX_MIRRORS, &mut problems);
    let mut config = RegistryConfig { insecure, mirrors };
    let listed = config.mirrors.clone();
    let (usable, plain): (Vec<Mirror>, Vec<Mirror>) = config.mirrors.iter().cloned().partition(|m| config.mirror_allowed(m));
    for m in plain {
        problems.push(|| format!("{MIRRORS}: {}", plain_mirror(&m)));
    }
    config.mirrors = usable;
    (config, problems.into_list(), listed)
}

/// Why a plain-HTTP mirror is not used.
fn plain_mirror(m: &Mirror) -> String {
    format!("{} is plain HTTP: use https://, or list {} in {INSECURE} too", m.as_url(), m.host)
}

/// An entry as it is parsed, or why it is not: one far longer than any host
/// or address is wrong, and is not repeated back.
fn entry(s: &str) -> Result<&str, String> {
    if s.len() > MAX_ENTRY { Err(format!("an entry of {} characters is not a host or an address", s.len())) } else { Ok(s) }
}

/// What was wrong with the file: the first [`MAX_PROBLEMS`], each made only
/// when it is kept, and how many more.
#[derive(Default)]
struct Problems {
    list: Vec<String>,
    more: usize,
}

impl Problems {
    fn push(&mut self, problem: impl FnOnce() -> String) {
        if self.list.len() < MAX_PROBLEMS {
            self.list.push(problem());
        } else {
            self.more += 1;
        }
    }

    fn into_list(mut self) -> Vec<String> {
        if self.more > 0 {
            self.list.push(format!("and {} more", self.more));
        }
        self.list
    }
}

/// What kind of JSON value `v` is, for a problem: the value itself may be
/// any size.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "true or false",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// `entries` once each, in order, at most `max` of them (the rest are
/// said, not used).
fn capped<T: PartialEq>(entries: impl Iterator<Item = T>, key: &str, max: usize, problems: &mut Problems) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    let mut over = 0;
    for e in entries {
        if out.contains(&e) {
            continue;
        }
        if out.len() >= max {
            over += 1;
        } else {
            out.push(e);
        }
    }
    if over > 0 {
        problems.push(|| format!("{key}: only the first {max} are used, {over} more are left out"));
    }
    out
}

fn strings(v: &Value, key: &str, problems: &mut Problems) -> Vec<String> {
    match &v[key] {
        Value::Null => Vec::new(),
        Value::Array(a) => a
            .iter()
            .filter_map(|e| match e.as_str() {
                Some(s) => Some(s.to_owned()),
                None => {
                    problems.push(|| format!("{key}: {} is not a string", kind(e)));
                    None
                }
            })
            .collect(),
        other => {
            problems.push(|| format!("{key} must be a list, not {}", kind(other)));
            Vec::new()
        }
    }
}

/// What Settings and `kiri registry` show: both lists as the file keeps
/// them (a mirror not used among them, so it can be taken out), the file's
/// path, and what in it was skipped.
pub fn report() -> Value {
    with_current(|r| {
        json!({
            INSECURE: r.config.insecure.iter().map(Insecure::as_string).collect::<Vec<_>>(),
            MIRRORS: r.listed.iter().map(Mirror::as_url).collect::<Vec<_>>(),
            "path": path().map(|p| p.display().to_string()).unwrap_or_default(),
            "problems": r.problems,
        })
    })
}

/// Docker's `RegistryConfig` in `GET /info`, which `docker info` prints as
/// "Insecure Registries" and "Registry Mirrors": loopback is insecure as on
/// Docker, and so is every host and network daemon.json lists.
pub fn info() -> Value {
    info_of(&current())
}

fn info_of(c: &RegistryConfig) -> Value {
    let mut cidrs = vec!["::1/128".to_owned(), "127.0.0.0/8".to_owned()];
    let mut indexes = Map::new();
    let mirrors: Vec<String> = c.mirrors.iter().map(|m| format!("{}/", m.as_url())).collect();
    indexes.insert("docker.io".into(), json!({"Name": "docker.io", "Mirrors": mirrors, "Secure": true, "Official": true}));
    for e in &c.insecure {
        match e {
            Insecure::Net(..) => {
                let cidr = e.as_string();
                if !cidrs.contains(&cidr) {
                    cidrs.push(cidr);
                }
            }
            Insecure::Host(h) => {
                indexes.insert(h.clone(), json!({"Name": h, "Mirrors": [], "Secure": false, "Official": false}));
            }
        }
    }
    json!({"InsecureRegistryCIDRs": cidrs, "IndexConfigs": indexes, "Mirrors": mirrors})
}

/// Saves the lists in `body` (`insecure-registries`, `registry-mirrors`;
/// a key left out keeps what the file has), each entry checked and written
/// as it will be read, duplicates dropped. Every other key in the file is
/// kept. A file that isn't a JSON object is not overwritten: the person
/// has to fix it, so nothing of theirs is lost. Returns the new report,
/// and whether the settings in use changed, as this save found them.
pub fn set(body: &Value) -> Result<(Value, bool), String> {
    let path = path().ok_or_else(|| "HOME is not set".to_owned())?;
    set_with(body, &path, current, || *READ.lock().unwrap_or_else(|e| e.into_inner()) = None, report)
}

/// [`set`], with where the file is, what is in use, how to forget what was
/// read and the report given.
fn set_with(body: &Value, path: &std::path::Path, current: impl Fn() -> Arc<RegistryConfig>, forget: impl Fn(), report: impl Fn() -> Value) -> Result<(Value, bool), String> {
    // One save at a time: each reads the file, changes a list and replaces
    // the file, and two at once would lose one change (and share a temp
    // file), or each see the other's change as already there.
    static SAVING: Mutex<()> = Mutex::new(());
    let _saving = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    let before = current();
    save_at(path, body)?;
    // What was read before the rename is not what is there now, even when
    // the time and size agree.
    forget();
    let changed = *current() != *before;
    Ok((report(), changed))
}

/// [`set`]'s work on the file at `path`.
fn save_at(path: &std::path::Path, body: &Value) -> Result<(), String> {
    let existing_mode = std::fs::metadata(path).ok().map(|m| mode_of(&m));
    let mut file = match read_limited(path) {
        Ok(b) if b.iter().all(u8::is_ascii_whitespace) => Map::new(),
        Ok(b) => match serde_json::from_slice::<Value>(&b) {
            Ok(Value::Object(m)) => m,
            _ => return Err(format!("{} is not a JSON object; fix it or remove it, then save again", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    // The insecure list as it was: a plain-HTTP mirror it did not allow
    // before (written by hand) does not stop a save that leaves it alone.
    let was = parse(&serde_json::to_vec(&Value::Object(file.clone())).map_err(|e| e.to_string())?).0;
    let mut given = false;
    if let Some(v) = body.get(INSECURE) {
        file.insert(INSECURE.into(), json!(normalize(v, INSECURE, MAX_INSECURE, |s| Insecure::parse(s).map(|e| e.as_string()))?));
        given = true;
    }
    if let Some(v) = body.get(MIRRORS) {
        file.insert(MIRRORS.into(), json!(normalize(v, MIRRORS, MAX_MIRRORS, |s| Mirror::parse(s).map(|m| m.as_url()))?));
        given = true;
    }
    if !given {
        return Err(format!("nothing to save: send {INSECURE} or {MIRRORS}"));
    }
    // A plain-HTTP mirror goes with its host in the insecure list: a mirror
    // list saved must have it, and an insecure list saved may not take a
    // used mirror's host out. One the file already had unused is said in
    // the problems, not in the way of an unrelated save.
    let saved = parse(&serde_json::to_vec(&Value::Object(file.clone())).map_err(|e| e.to_string())?).0;
    let listed: Vec<Mirror> = file.get(MIRRORS).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter_map(|s| Mirror::parse(s).ok()).collect();
    let mirrors_given = body.get(MIRRORS).is_some();
    if let Some(m) = listed.iter().find(|m| !saved.mirror_allowed(m) && (mirrors_given || was.mirror_allowed(m))) {
        return Err(format!("{MIRRORS}: {}", plain_mirror(m)));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(file)).map_err(|e| e.to_string())?;
    text.push('\n');
    // What would not be read back is not written.
    if text.len() as u64 > MAX_BYTES {
        return Err(format!("{} would be larger than {} KiB", path.display(), MAX_BYTES >> 10));
    }
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, text.as_bytes(), existing_mode).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

fn mode_of(m: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        0o600
    }
}

/// Writes a file only this user can read or write (what a daemon.json that
/// was kept private must stay, and Docker's keys may hold a proxy
/// credential), or the file's own mode when that is stricter.
fn write_private(path: &std::path::Path, bytes: &[u8], existing_mode: Option<u32>) -> std::io::Result<()> {
    // A stale temp file, or a link put there, is not written through.
    let _ = std::fs::remove_file(path);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mode = existing_mode.map(|m| m & 0o600).filter(|m| *m != 0).unwrap_or(0o600);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = existing_mode;
        std::fs::write(path, bytes)
    }
}

/// A list from Settings or the CLI: strings, each one checked and written
/// the way it reads back, in order, once each.
fn normalize(v: &Value, key: &str, max: usize, check: impl Fn(&str) -> Result<String, String>) -> Result<Vec<String>, String> {
    let given = v.as_array().ok_or_else(|| format!("{key} must be a list"))?;
    if given.len() > max {
        return Err(format!("{key}: at most {max}"));
    }
    let mut out: Vec<String> = Vec::new();
    for e in given {
        let s = e.as_str().ok_or_else(|| format!("{key}: {} is not a string", kind(e)))?;
        let n = entry(s).and_then(&check).map_err(|e| format!("{key}: {e}"))?;
        if !out.contains(&n) {
            out.push(n);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dockers_keys_are_read() {
        let (c, problems) = parse(br#"{"insecure-registries": ["registry.lan:5000", "http://10.0.0.0/8"], "registry-mirrors": ["https://mirror.example.com/"], "debug": true}"#);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(c.is_insecure("registry.lan:5000"));
        assert!(c.is_insecure("10.1.2.3:5000"));
        assert_eq!(c.mirrors.iter().map(Mirror::as_url).collect::<Vec<_>>(), ["https://mirror.example.com"]);
    }

    #[test]
    fn what_cant_be_read_is_skipped_and_said() {
        let (c, problems) = parse(br#"{"insecure-registries": ["ok.lan:5000", "0.0.0.0/0", 7], "registry-mirrors": "https://m.example.com"}"#);
        assert!(c.is_insecure("ok.lan:5000"));
        assert!(c.mirrors.is_empty());
        assert_eq!(problems.len(), 3, "{problems:?}");
        let (c, problems) = parse(b"{not json");
        assert_eq!(c, RegistryConfig::default());
        assert!(problems[0].contains("not valid JSON"));
        assert!(parse(b"[]").1[0].contains("not a JSON object"));
    }

    #[test]
    fn a_hand_edited_file_is_bounded_in_what_it_adds() {
        let many: Vec<String> = (0..1000).map(|i| format!("\"r{i}.lan:5000\"")).collect();
        let dup: Vec<String> = (0..1000).map(|_| "\"same.lan\"".to_owned()).collect();
        let bad: Vec<String> = (0..1000).map(|i| format!("\"bad host {i}\"")).collect();
        let text = format!(r#"{{"insecure-registries": [{}], "registry-mirrors": [{}, {}]}}"#, many.join(","), bad.join(","), dup.join(","));
        let (c, problems) = parse(text.as_bytes());
        assert_eq!(c.insecure.len(), MAX_INSECURE);
        assert_eq!(c.mirrors.len(), 1, "a thousand copies are one mirror");
        assert!(problems.len() <= MAX_PROBLEMS + 1, "{}", problems.len());
        assert!(problems.last().unwrap().starts_with("and "));
        let (c, problems) = parse(format!(r#"{{"registry-mirrors": [{}]}}"#, (0..40).map(|i| format!("\"m{i}.example.com\"")).collect::<Vec<_>>().join(",")).as_bytes());
        assert_eq!(c.mirrors.len(), MAX_MIRRORS);
        assert!(problems.iter().any(|p| p.contains("left out")));
        let (c, _) = parse(br#"{"insecure-registries": ["a.lan", "A.lan", "http://a.lan"]}"#);
        assert_eq!(c.insecure.len(), 1);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kiri-daemon-json-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_a_regular_file_of_a_sane_size_is_read() {
        let d = scratch("read");
        let f = d.join("daemon.json");
        std::fs::write(&f, br#"{"debug": true}"#).unwrap();
        assert!(read_limited(&f).is_ok());
        std::fs::write(&f, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(read_limited(&f).unwrap_err().to_string().contains("larger"));
        assert!(read_limited(&d).unwrap_err().to_string().contains("regular"));
        #[cfg(unix)]
        {
            assert!(read_limited(std::path::Path::new("/dev/zero")).unwrap_err().to_string().contains("regular"));
            let fifo = d.join("fifo");
            assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
            assert!(read_limited(&fifo).unwrap_err().to_string().contains("regular"), "a FIFO is not opened");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_save_keeps_the_files_privacy_and_leaves_no_temp_file() {
        let d = scratch("write");
        let f = d.join("daemon.json");
        let tmp = d.join("daemon.json.tmp");
        write_private(&tmp, b"{}", None).unwrap();
        std::fs::rename(&tmp, &f).unwrap();
        // A stale temp file does not stop the next write, or change its mode.
        std::fs::write(&tmp, b"stale").unwrap();
        write_private(&tmp, b"{\"a\":1}", Some(0o644)).unwrap();
        assert_eq!(std::fs::read(&tmp).unwrap(), b"{\"a\":1}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&tmp), 0o600);
            write_private(&tmp, b"{}", Some(0o400)).unwrap();
            assert_eq!(mode(&tmp), 0o400);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_plain_http_mirror_is_used_only_with_its_host_listed() {
        let (c, problems) = parse(br#"{"registry-mirrors": ["http://mirror.lan:5000", "https://m.example.com"]}"#);
        assert_eq!(c.mirrors.iter().map(Mirror::as_url).collect::<Vec<_>>(), ["https://m.example.com"]);
        assert!(problems.iter().any(|p| p.contains("plain HTTP")), "{problems:?}");
        let (c, problems) = parse(br#"{"insecure-registries": ["mirror.lan:5000"], "registry-mirrors": ["http://mirror.lan:5000", "http://127.0.0.1:5000"]}"#);
        assert_eq!(c.mirrors.len(), 2, "{problems:?}");
        assert!(problems.is_empty(), "{problems:?}");

        let d = scratch("plain-mirror");
        let f = d.join("daemon.json");
        assert!(save_at(&f, &json!({MIRRORS: ["http://mirror.lan:5000"]})).unwrap_err().contains("plain HTTP"));
        assert!(!f.exists(), "nothing written");
        save_at(&f, &json!({INSECURE: ["mirror.lan:5000"], MIRRORS: ["http://mirror.lan:5000"]})).unwrap();
        // And the host cannot be taken out of the list while the mirror needs it.
        assert!(save_at(&f, &json!({INSECURE: []})).unwrap_err().contains("plain HTTP"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A plain-HTTP mirror written by hand without its host listed: it is
    /// not used, it is shown so it can be taken out, and it does not stop
    /// a save of the insecure list that leaves it alone. Saving the mirror
    /// list with it still in is refused; without it, it is gone.
    #[test]
    fn a_hand_written_plain_http_mirror_does_not_lock_the_lists() {
        let d = scratch("hand-plain");
        let f = d.join("daemon.json");
        std::fs::write(&f, br#"{"registry-mirrors": ["http://mirror.lan:5000", "https://m.example.com"]}"#).unwrap();
        let (c, problems, listed) = parse_all(&std::fs::read(&f).unwrap());
        assert_eq!(c.mirrors.iter().map(Mirror::as_url).collect::<Vec<_>>(), ["https://m.example.com"]);
        assert_eq!(listed.iter().map(Mirror::as_url).collect::<Vec<_>>(), ["http://mirror.lan:5000", "https://m.example.com"], "shown, in order");
        assert!(problems.iter().any(|p| p.contains("plain HTTP")), "{problems:?}");
        save_at(&f, &json!({INSECURE: ["other.lan:5000"]})).unwrap();
        assert!(save_at(&f, &json!({MIRRORS: ["http://mirror.lan:5000", "https://m.example.com"]})).unwrap_err().contains("plain HTTP"));
        save_at(&f, &json!({MIRRORS: ["https://m.example.com"]})).unwrap();
        let (c, problems, listed) = parse_all(&std::fs::read(&f).unwrap());
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(listed, c.mirrors);
        assert!(c.is_insecure("other.lan:5000"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Two saves of the same change at once: the one that made it says so,
    /// and the other, which waited for it, finds nothing changed (it would
    /// otherwise restart the builder a second time).
    #[test]
    fn a_save_says_whether_it_changed_anything_as_it_found_the_file() {
        let d = scratch("changed");
        let f = Arc::new(d.join("daemon.json"));
        let current = {
            let f = f.clone();
            move || Arc::new(parse(&std::fs::read(&*f).unwrap_or_default()).0)
        };
        let body = json!({INSECURE: ["changed.lan:5000"]});
        let second: Arc<Mutex<Option<std::thread::JoinHandle<bool>>>> = Arc::default();
        let (_, first) = set_with(
            &body,
            &f,
            &current,
            || {
                // The first save has written; the second starts now.
                let (body, f, current) = (body.clone(), f.clone(), current.clone());
                *second.lock().unwrap() = Some(std::thread::spawn(move || set_with(&body, &f, current, || {}, || Value::Null).unwrap().1));
                std::thread::sleep(std::time::Duration::from_millis(200));
            },
            || Value::Null,
        )
        .unwrap();
        let second = second.lock().unwrap().take().unwrap().join().unwrap();
        assert!(first, "the first save changed the list");
        assert!(!second, "the second found it already changed");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn what_is_wrong_is_said_briefly() {
        // A huge entry or value is said to be wrong, not repeated back.
        let huge = "a".repeat(200_000);
        let text = format!(r#"{{"insecure-registries": ["{huge}", {{"k": "{huge}"}}], "registry-mirrors": {{"k": "{huge}"}}}}"#);
        let (_, problems) = parse(text.as_bytes());
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().all(|p| p.len() < 200), "{problems:?}");
        assert!(normalize(&json!([huge]), INSECURE, 8, |s| Insecure::parse(s).map(|e| e.as_string())).unwrap_err().len() < 200);
        assert!(normalize(&json!([{"k": huge}]), INSECURE, 8, |s| Insecure::parse(s).map(|e| e.as_string())).unwrap_err().len() < 200);
    }

    #[test]
    fn a_save_that_would_not_be_read_back_is_not_written() {
        let d = scratch("too-big");
        let f = d.join("daemon.json");
        // Another key of the person's, just under the size the daemon reads.
        let other = "x".repeat(MAX_BYTES as usize - 200);
        std::fs::write(&f, format!(r#"{{"note": "{other}"}}"#)).unwrap();
        let before = std::fs::read(&f).unwrap();
        let entries: Vec<String> = (0..40).map(|i| format!("registry-{i}.lan:5000")).collect();
        assert!(save_at(&f, &json!({INSECURE: entries})).unwrap_err().contains("larger"));
        assert_eq!(std::fs::read(&f).unwrap(), before, "the file is as it was");
        assert!(read_limited(&f).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn info_lists_each_network_once() {
        // Loopback is always there; listing it again does not repeat it.
        let (c, _) = parse(br#"{"insecure-registries": ["127.0.0.0/8", "::1/128", "10.0.0.0/8", "10.0.0.0/8"]}"#);
        assert_eq!(info_of(&c)["InsecureRegistryCIDRs"], json!(["::1/128", "127.0.0.0/8", "10.0.0.0/8"]));
    }

    #[test]
    fn lists_are_checked_and_written_as_they_read() {
        let insecure = normalize(&json!(["Registry.LAN:5000", "http://registry.lan:5000", "10.1.0.0/16"]), INSECURE, 8, |s| Insecure::parse(s).map(|e| e.as_string())).unwrap();
        assert_eq!(insecure, ["registry.lan:5000", "10.1.0.0/16"]);
        assert!(normalize(&json!(["docker.io"]), INSECURE, 8, |s| Insecure::parse(s).map(|e| e.as_string())).is_err());
        assert!(normalize(&json!("registry.lan"), INSECURE, 8, |s| Insecure::parse(s).map(|e| e.as_string())).is_err());
        assert!(normalize(&json!(["a.lan", "b.lan"]), INSECURE, 1, |s| Insecure::parse(s).map(|e| e.as_string())).is_err());
        let mirrors = normalize(&json!(["mirror.example.com", "https://mirror.example.com/", "http://mirror.lan:5000"]), MIRRORS, 8, |s| Mirror::parse(s).map(|m| m.as_url())).unwrap();
        assert_eq!(mirrors, ["https://mirror.example.com", "http://mirror.lan:5000"]);
    }
}
