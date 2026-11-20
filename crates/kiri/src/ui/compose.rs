//! Compose in the window: the projects that have run on Kiri, each one's
//! services and compose file, what Apply will change, and Apply itself
//! (save the file, then `docker compose up -d`, so only the services that
//! changed are recreated). The Mac's and Linux's window and Windows' (crates/kiri-win, by
//! #[path]) answer the page's `compose*` commands with it.
//!
//! A project is found by its containers' labels (com.docker.compose.project,
//! .project.working_dir, .project.config_files): it shows once it has run on
//! Kiri, and its file stays where it is. What the file means comes from
//! `docker compose config --format json`, the CLI's own reading of it, so
//! extends, overrides, profiles and `.env` interpolation are Compose's, not
//! a second parser's. That reading is kept per file and change time.
//!
//! The page names a project, never a path: the file Kiri reads and writes is
//! the one the project's own containers name, or one the person picked in the
//! system's own file dialog (Open a compose file…), which the shell shows and
//! hands to [`open_file`]. Those are kept in Kiri's state folder
//! (compose.json), so a project that has never run is listed too.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use serde_json::{Value, json};

use super::composetech as tech;
use crate::client;

/// A docker CLI command that reaches Kiri: the shell makes it (DOCKER_HOST
/// on the Mac and Linux; the Kiri context and no console window on Windows).
pub type Cli = fn() -> io::Result<Command>;

/// Where the compose files opened in the window are kept (compose.json in
/// Kiri's state folder; the shell says where that is).
pub type Store = fn() -> Option<PathBuf>;

const PROJECT: &str = "com.docker.compose.project";
const SERVICE: &str = "com.docker.compose.service";
const WORKDIR: &str = "com.docker.compose.project.working_dir";
const FILES: &str = "com.docker.compose.project.config_files";
const ENV_FILES: &str = "com.docker.compose.project.environment_file";

/// One container of a project, as the list has it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Member {
    pub service: String,
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub created: i64,
}

/// A project, from its containers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Project {
    pub name: String,
    pub dir: String,
    pub files: Vec<String>,
    pub members: Vec<Member>,
    /// The `--env-file`s it was brought up with, if any (in place of the folder's `.env`).
    pub env_files: Vec<String>,
}

/// The projects among `/containers/json?all=1`, by name; the directory and
/// files are the newest container's (the last `up` that made one).
pub fn projects(containers: &Value) -> Vec<Project> {
    let mut by: BTreeMap<String, Project> = BTreeMap::new();
    let mut newest: HashMap<String, i64> = HashMap::new();
    for c in containers.as_array().into_iter().flatten() {
        let label = |k: &str| c["Labels"][k].as_str().unwrap_or_default().to_owned();
        let name = label(PROJECT);
        if name.is_empty() {
            continue;
        }
        let created = c["Created"].as_i64().unwrap_or_default();
        let p = by.entry(name.clone()).or_insert_with(|| Project { name: name.clone(), ..Project::default() });
        if created >= *newest.get(&name).unwrap_or(&i64::MIN) {
            newest.insert(name.clone(), created);
            if !label(WORKDIR).is_empty() {
                p.dir = label(WORKDIR);
            }
            let files: Vec<String> = label(FILES).split(',').map(str::trim).filter(|f| !f.is_empty()).map(str::to_owned).collect();
            if !files.is_empty() {
                p.files = files;
            }
            p.env_files = label(ENV_FILES).split(',').map(str::trim).filter(|f| !f.is_empty()).map(str::to_owned).collect();
        }
        p.members.push(Member {
            service: label(SERVICE),
            id: c["Id"].as_str().unwrap_or_default().to_owned(),
            name: c["Names"][0].as_str().unwrap_or_default().trim_start_matches('/').to_owned(),
            image: c["Image"].as_str().unwrap_or_default().to_owned(),
            state: c["State"].as_str().unwrap_or_default().to_owned(),
            status: c["Status"].as_str().unwrap_or_default().to_owned(),
            created,
        });
    }
    let mut out: Vec<Project> = by.into_values().collect();
    for p in &mut out {
        p.members.sort_by(|a, b| a.service.cmp(&b.service).then(a.name.cmp(&b.name)));
    }
    out
}

fn find(name: &str, store: Store) -> io::Result<Project> {
    let containers = client::get_background("/containers/json?all=1")?;
    all_projects(&containers, store).into_iter().find(|p| p.name == name).ok_or_else(|| io::Error::other(format!("no Compose project {name} on Kiri")))
}

/// What the window says when a project's file isn't one Kiri will read or write.
pub const NO_FILE: &str = "Kiri can't find this project's compose file";

/// A path as a person and the docker CLI write it: Windows' canonical form
/// `\\?\C:\x` is `C:\x`, and `\\?\UNC\server\share\x` is
/// `\\server\share\x` (as the `dunce` crate does); any other is as it is.
pub fn plain_path(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\")
        && rest.as_bytes().get(1) == Some(&b':')
        && rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
    {
        return PathBuf::from(rest);
    }
    p
}

/// [`std::fs::canonicalize`], as [`plain_path`] writes it.
fn real_path(p: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(p).map(plain_path)
}

/// A compose file Kiri may read into the window and write on Apply: an
/// absolute path to a .yaml or .yml regular file, as it is once links are
/// followed (the labels it comes from are any container's to set).
pub fn valid_file(f: &str) -> Option<PathBuf> {
    valid_file_from(f, false)
}

/// A network path (`\\host\share\x`, `//host/share/x`, `\\?\UNC\…`, a
/// `\\.\` device, the Mac's `/net/<host>/…`): reading one makes the system
/// contact another host.
pub fn remote_path(f: &str) -> bool {
    // macOS mounts another host's exports under /net/<host>/ on first look:
    // checked on the path with `.` and `..` resolved as written (never by
    // looking, which would mount it), in any case (the disk ignores it).
    let n = lexical(f).to_ascii_lowercase();
    if n.starts_with("/net/") || n == "/net" {
        return true;
    }
    let f = f.replace('/', "\\");
    if let Some(rest) = f.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        return !(b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':');
    }
    f.starts_with(r"\\")
}

/// An absolute `/` path with `.`, `..` and repeated `/` resolved by its
/// words alone, as the system would walk it; any other as it is.
fn lexical(f: &str) -> String {
    if !f.starts_with('/') || f.starts_with("//") {
        return f.to_owned();
    }
    let mut parts: Vec<&str> = Vec::new();
    for w in f.split('/') {
        match w {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            w => parts.push(w),
        }
    }
    format!("/{}", parts.join("/"))
}

/// [`valid_file`]; `remote` allows a network path (a file the person picked
/// in the dialog, never one a container's label names).
fn valid_file_from(f: &str, remote: bool) -> Option<PathBuf> {
    let p = Path::new(f);
    if !p.is_absolute() || !compose_like(p) || (!remote && remote_path(f)) {
        return None;
    }
    let real = real_path(p).ok()?;
    if !remote && remote_path(&real.to_string_lossy()) {
        return None;
    }
    (compose_like(&real) && std::fs::metadata(&real).ok()?.file_type().is_file()).then_some(real)
}

/// [`checked_files`], and the env files Compose reads for it: each one the
/// labels name a local regular file, and the folder's `.env` none or one
/// (never a pipe or a device, which would hold Compose and Kiri reading it);
/// else why not, as `docker compose` itself refuses a missing one.
fn checked(p: Project, store: Store) -> Result<Project, String> {
    let mut p = checked_files(p, store)?;
    if let Some(f) = p.env_files.iter().find(|f| valid_env_file(f).is_none()) {
        return Err(bad_env(f));
    }
    p.env_files = p.env_files.iter().filter_map(|f| valid_env_file(f)).collect();
    let dotenv = Path::new(&p.dir).join(".env");
    if p.env_files.is_empty() && !regular_or_none(&dotenv) {
        return Err(bad_env(&dotenv.to_string_lossy()));
    }
    service_env_ok(&p)?;
    Ok(p)
}

/// Each service's `env_file:` in the project's files, read from the YAML
/// before Compose is: none a pipe or a device (Compose would wait on it).
fn service_env_ok(p: &Project) -> Result<(), String> {
    for f in &p.files {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        let base = Path::new(f).parent().unwrap_or(Path::new(""));
        if let Some(bad) = env_file_paths(&text).into_iter().flat_map(|e| [base.join(&e), Path::new(&p.dir).join(&e)]).find(|e| !regular_or_none(e)) {
            return Err(bad_env(&bad.to_string_lossy()));
        }
    }
    Ok(())
}

/// The paths a compose file's `env_file:` keys name, as written: one, a
/// `[list]`, a `- list`, or `- path:` items. Ones with a `$` are left to Compose.
fn env_file_paths(text: &str) -> Vec<String> {
    let unq = |v: &str| {
        let v = v.split(" #").next().unwrap_or(v).trim();
        let v = v.strip_prefix("path:").map_or(v, str::trim);
        v.trim_matches(['"', '\'']).to_owned()
    };
    let (mut out, mut under) = (Vec::new(), None::<usize>);
    for line in text.lines() {
        let ind = line.len() - line.trim_start().len();
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some(i) = under {
            if ind > i || (ind == i && l.starts_with('-')) {
                let item = l.trim_start_matches('-').trim();
                if l.starts_with('-') || item.starts_with("path:") {
                    out.push(unq(item));
                }
                continue;
            }
            under = None;
        }
        if let Some(v) = l.strip_prefix("env_file:") {
            let v = v.trim();
            if v.is_empty() {
                under = Some(ind);
            } else if let Some(list) = v.strip_prefix('[') {
                out.extend(list.trim_end_matches(']').split(',').filter(|i| !i.trim().starts_with("required")).map(|i| unq(i.trim().trim_matches(['{', '}']))));
            } else {
                out.push(unq(v));
            }
        }
    }
    out.retain(|e| !e.is_empty() && !e.contains('$') && !e.starts_with("required"));
    out
}

/// A compose file's own top-level `name:`, as written (none with a `$`).
fn yaml_name(text: &str) -> Option<String> {
    let v = text.lines().find_map(|l| l.strip_prefix("name:"))?;
    let v = v.split(" #").next().unwrap_or(v).trim().trim_matches(['"', '\'']);
    (!v.is_empty() && v.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')).then(|| v.to_owned())
}

/// A compose file's top-level `name:` when Compose can't be asked with
/// the secrets hidden: as written, or with its `${VAR}`s (`${VAR:-x}`,
/// `${VAR-x}`) filled from `vars` when none is secret-named; none when one
/// is, or the name isn't a project's.
fn written_name(text: &str, vars: &[(String, String)]) -> Option<String> {
    if let Some(n) = yaml_name(text) {
        return Some(n);
    }
    let v = text.lines().find_map(|l| l.strip_prefix("name:"))?;
    let v = v.split(" #").next().unwrap_or(v).trim().trim_matches(['"', '\'']);
    let (mut out, mut rest) = (String::new(), v);
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let (inner, after) = rest[i + 2..].split_once('}')?;
        let (k, def) = match inner.split_once(":-").or_else(|| inner.split_once('-')) {
            Some((k, d)) => (k, Some(d)),
            None => (inner, None),
        };
        if secret_name(k) || k.contains('$') {
            return None;
        }
        let val = vars.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty() || !inner.contains(":-"));
        out.push_str(val.or(def)?);
        rest = after;
    }
    out.push_str(rest);
    yaml_name(&format!("name: {out}\n"))
}

/// The secret-named variables (`${NAME}`) of the file's own top-level
/// `name:`, as written: a name Compose makes of them shows the secret.
pub fn name_text_vars(text: &str) -> Vec<String> {
    let Some(v) = name_scalar(text) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for n in text_names(&v).into_iter().filter(|n| secret_name(n)).map(|n| format!("${{{n}}}")) {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// The secret-named variables (`${NAME}`) in the name Compose itself takes
/// from the files (`config` without `-p`, as [`opened_name`] asks it), by
/// the stand-ins it holds: every way YAML writes `name:` is Compose's to
/// read. None when Compose can't read it so ([`name_text_vars`] then).
fn own_name_vars(cli: Cli, p: &Project, draft: Option<&str>) -> Option<Vec<String>> {
    let stands = stands_for(cli, p, draft);
    let m = config_with(cli, &Project { name: String::new(), ..p.clone() }, draft, &[], &stands).ok()?;
    let name = m["name"].as_str()?.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    for st in stands.iter().filter(|st| name.contains(&st.token)) {
        if !out.contains(&st.label) {
            out.push(st.label.clone());
        }
    }
    Some(out)
}

/// The text of the file's top-level `name:` value: `name:`, `"name":`,
/// `name :`, or a JSON file's; without a comment (quotes respected). A
/// block scalar's or an anchored value's own text, lines and all.
fn name_scalar(text: &str) -> Option<String> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim_start().starts_with('{') {
        return serde_json::from_str::<Value>(text).ok()?["name"].as_str().map(str::to_owned);
    }
    let lines: Vec<&str> = text.lines().collect();
    let (i, rest) = lines.iter().enumerate().find_map(|(i, l)| {
        let (k, r) = if let Some(q) = l.strip_prefix(['"', '\'']) {
            let (k, r) = q.split_once(l.as_bytes()[0] as char)?;
            (k, r.trim_start().strip_prefix(':')?)
        } else {
            let (k, r) = l.split_once(':')?;
            (k.trim_end(), r)
        };
        (k == "name" && (r.is_empty() || r.starts_with([' ', '\t']))).then_some((i, r.trim()))
    })?;
    if let Some(q) = rest.strip_prefix('"') {
        let mut out = String::new();
        let mut it = q.chars();
        while let Some(c) = it.next() {
            match c {
                '"' => break,
                '\\' => out.extend(it.next()),
                c => out.push(c),
            }
        }
        return Some(out);
    }
    if let Some(q) = rest.strip_prefix('\'') {
        return Some(q.split("'").next().unwrap_or_default().to_owned());
    }
    let plain = rest.split(" #").next().unwrap_or(rest).split("\t#").next().unwrap_or(rest);
    let plain = if plain.starts_with('#') { "" } else { plain.trim() };
    // A block scalar or an anchor: the value's own text, its lines below too.
    if plain.starts_with(['|', '>', '&', '!']) || plain.is_empty() {
        let below = lines[i + 1..].iter().take_while(|l| l.is_empty() || l.starts_with([' ', '\t'])).map(|l| l.split(" #").next().unwrap_or(l)).collect::<Vec<_>>().join("\n");
        return Some(format!("{plain}\n{below}"));
    }
    Some(plain.to_owned())
}

/// A file whose `name:` uses a secret, listed by its folder's name (as
/// Kiri lists a file it opened): the notice that Kiri never takes that name.
pub fn folder_note(vars: &[String], name: &str, dir: &str) -> Option<Value> {
    let vars = vars.join(", ");
    (!vars.is_empty() && !dir.is_empty() && name == folder_name(Path::new(dir))).then(|| {
        json!({"text": format!("This file's name uses {vars}. Kiri uses the folder's name, {name}, so it never puts a secret in a name."), "errkey": "nameFolder", "errargs": {"vars": vars, "name": name}})
    })
}

/// The networks a project's services are on, by name: what Docker reports
/// for its containers, else the reading's (`name:` when set). In one order
/// both ways: the project's default network first, the others by name.
pub fn project_networks(name: &str, model: Option<&Value>, containers: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in containers.as_array().into_iter().flatten().filter(|c| c["Labels"][PROJECT] == name && !name.is_empty()) {
        for (k, _) in c["NetworkSettings"]["Networks"].as_object().into_iter().flatten() {
            if !out.contains(k) {
                out.push(k.clone());
            }
        }
    }
    if out.is_empty() {
        for (k, v) in model.and_then(|m| m["networks"].as_object()).into_iter().flatten() {
            out.push(v["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(k).to_owned());
        }
    }
    let default = model.and_then(|m| m["networks"]["default"]["name"].as_str()).map_or_else(|| format!("{name}_default"), str::to_owned);
    out.sort_by_key(|n| (*n != default, n.clone()));
    out
}

/// What Kiri says of an env file it won't read.
fn bad_env(f: &str) -> String {
    format!("couldn't read env file {f}: it isn't a local regular file")
}

/// No file there, or a regular one once links are followed.
fn regular_or_none(f: &Path) -> bool {
    match std::fs::metadata(f) {
        Ok(m) => m.is_file(),
        Err(e) => e.kind() == io::ErrorKind::NotFound && std::fs::symlink_metadata(f).is_err(),
    }
}

/// The project with its files checked ([`valid_file`]) and named as they
/// really are, and a directory that is absolute; else [`NO_FILE`].
fn checked_files(mut p: Project, store: Store) -> Result<Project, String> {
    if p.files.is_empty() {
        return Err(NO_FILE.into());
    }
    // A network path only when the person opened that file themselves.
    let picked = opened(store);
    let mine = |f: &str| picked.iter().any(|o| o.files.iter().any(|g| g == f));
    let files: Option<Vec<String>> = p.files.iter().map(|f| valid_file_from(f, mine(f)).map(|r| r.to_string_lossy().into_owned())).collect();
    p.files = files.ok_or_else(|| NO_FILE.to_owned())?;
    let beside = |d: &str| p.files.iter().any(|f| mine(f) && Path::new(f).parent() == Some(Path::new(d)));
    // The folder is a label too: never a network one, before Kiri looks in it.
    // A folder that is gone, or one Kiri won't look in: the file's own, as
    // `docker compose -f` alone would take.
    if remote_path(&p.dir) || Path::new(&p.dir).is_absolute() {
        p.dir = valid_dir(&p.dir, beside(&p.dir)).unwrap_or_default();
    }
    if !Path::new(&p.dir).is_absolute() {
        p.dir = Path::new(&p.files[0]).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    }
    Ok(p)
}

/// A project's folder from its label, as it is written: absolute, and a
/// local folder once links are followed; `remote` allows a network one (the
/// folder of a file the person picked).
fn valid_dir(d: &str, remote: bool) -> Option<String> {
    if !Path::new(d).is_absolute() || (!remote && remote_path(d)) {
        return None;
    }
    let real = real_path(Path::new(d)).ok()?;
    if !remote && remote_path(&real.to_string_lossy()) {
        return None;
    }
    std::fs::metadata(&real).ok()?.is_dir().then(|| d.to_owned())
}

/// An env file a label names: absolute, local, and a regular file once links are followed.
fn valid_env_file(f: &str) -> Option<String> {
    if !Path::new(f).is_absolute() || remote_path(f) {
        return None;
    }
    let real = real_path(Path::new(f)).ok()?;
    (!remote_path(&real.to_string_lossy()) && std::fs::metadata(&real).ok()?.is_file()).then(|| real.to_string_lossy().into_owned())
}

fn find_checked(name: &str, store: Store) -> Result<Project, String> {
    checked(find(name, store).map_err(|e| e.to_string())?, store)
}

/// A content hash of the file (FNV-1a, 64 bits): what the page's draft was
/// based on, so Apply can tell the file changed outside Kiri since.
pub fn file_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The file as it was before Kiri's last save over it, per project, and the
/// hash of what Kiri wrote: Discard after a failed Apply puts it back.
type Before = HashMap<String, (String, Vec<u8>, String)>;
static BEFORE: LazyLock<Mutex<Before>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// After Kiri saved over `path` (`disk` was there, `wrote` is the new
/// hash): what Discard puts back is the file before the first Apply that
/// didn't take, so a second failed Apply over Kiri's own save keeps it. One
/// over a file changed since (by hand, after Reload) starts again from it.
fn remember_before(name: &str, path: &str, disk: Vec<u8>, wrote: &str) {
    let mut before = BEFORE.lock().unwrap_or_else(|e| e.into_inner());
    match before.get_mut(name) {
        Some((f, _, w)) if f == path && *w == file_hash(&disk) => *w = wrote.to_owned(),
        _ => {
            before.insert(name.to_owned(), (path.to_owned(), disk, wrote.to_owned()));
        }
    }
}

/// The values Compose interpolates a project's file with, each name and
/// value, as Compose itself reads them (its dotenv rules: quotes, escapes,
/// `# comments`, `${VAR}`, `export`, `KEY: value`): the environment, then the
/// folder's `.env` or the `--env-file`s the project came up with, from
/// `docker compose config --environment` of an empty file in its place. Kept
/// while those files' change times stay. An included project's env files come
/// first: the project's own value of a name both have is the one it stands for.
pub fn env_vars(cli: Cli, p: &Project) -> Vec<(String, String)> {
    let mut vars = include_vars(p);
    vars.extend(own_env(cli, p));
    vars
}

/// [`env_vars`] without the includes' env files: the project's own
/// environment (the shell, its `.env`, its `--env-file`s), which wins in Compose.
fn own_env(cli: Cli, p: &Project) -> Vec<(String, String)> {
    let t = |f: &Path| std::fs::metadata(f).and_then(|m| m.modified()).ok();
    let key = format!("{}\0{}\0{}", p.dir, p.env_files.join("\0"), p.files.join("\0"));
    let (inc_files, inc_envs) = included(p);
    let mut stamp = vec![if p.dir.is_empty() { None } else { t(&Path::new(&p.dir).join(".env")) }];
    stamp.extend(p.env_files.iter().map(|f| t(Path::new(f))));
    stamp.extend(p.files.iter().map(|f| t(Path::new(f))));
    stamp.extend(inc_files.iter().chain(&inc_envs).map(|f| t(f)));
    if let Some((_, v)) = ENVS.lock().unwrap_or_else(|e| e.into_inner()).get(&key).filter(|(s, _)| *s == stamp) {
        return v.clone();
    }
    // An env file that isn't a regular one is never read (a pipe would hold
    // Compose and Kiri): the environment's values alone.
    let dotenv = (!p.dir.is_empty()).then(|| Path::new(&p.dir).join(".env"));
    if p.env_files.iter().any(|f| !std::fs::metadata(f).is_ok_and(|m| m.is_file())) || dotenv.as_deref().is_some_and(|d| !regular_or_none(d)) {
        return std::env::vars().collect();
    }
    let empty = Project { dir: p.dir.clone(), env_files: p.env_files.clone(), ..Project::default() };
    let read = cli().ok().and_then(|mut cmd| {
        cmd.args(project_args(&empty, None)).args(["-f", "-", "config", "--environment"]);
        let none = if p.dir.is_empty() { Some(EmptyDir::new().ok()?) } else { None };
        if let Some(d) = &none {
            cmd.current_dir(&d.0);
        }
        run_cli(cmd, Some("services: {}\n"), 30).ok()
    });
    // Compose can't read the `.env` at all: nothing in it reaches the file
    // either; its lines are taken as they are, to hide too many, not too few.
    let vars = read.map_or_else(|| env_guess(&p.dir), |out| environment(&String::from_utf8_lossy(&out)));
    ENVS.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (stamp, vars.clone()));
    vars
}

/// One `include:` entry as written: its files, its `project_directory`
/// and its `env_file`s (none with a `$`, which are left to Compose).
#[derive(Debug, Default, PartialEq)]
pub struct Include {
    pub paths: Vec<String>,
    pub dir: Option<String>,
    pub env_files: Vec<String>,
}

/// A compose file's top-level `include:` entries, read from its text: the
/// short form (`- sub/compose.yaml`, `[a.yaml, b.yaml]`) and the long one
/// (`path`, `project_directory`, `env_file`, each one or a list).
pub fn includes(text: &str) -> Vec<Include> {
    let unq = |v: &str| v.split(" #").next().unwrap_or(v).trim().trim_matches(['"', '\'']).to_owned();
    let list = |v: &str| -> Vec<String> {
        match v.trim().strip_prefix('[') {
            Some(l) => l.trim_end_matches(']').split(',').map(unq).filter(|s| !s.is_empty()).collect(),
            None => vec![unq(v)].into_iter().filter(|s| !s.is_empty()).collect(),
        }
    };
    let (mut out, mut inside, mut item_ind, mut key) = (Vec::<Include>::new(), false, None::<usize>, String::new());
    let put = |out: &mut Vec<Include>, key: &str, vals: Vec<String>| {
        let Some(it) = out.last_mut() else { return };
        match key {
            "path" => it.paths.extend(vals),
            "project_directory" => it.dir = vals.into_iter().next(),
            "env_file" => it.env_files.extend(vals),
            _ => {}
        }
    };
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let ind = line.len() - line.trim_start().len();
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l == "---" {
            continue;
        }
        // `include:`'s items may start at the first column too.
        let item0 = inside && l.starts_with('-') && (l.len() == 1 || l[1..].starts_with(' '));
        if ind == 0 && !item0 {
            inside = false;
            let top = l.split_once(':').filter(|(k, _)| k.trim().trim_matches(['"', '\'']) == "include").map(|(_, v)| v);
            if let Some(v) = top {
                inside = true;
                item_ind = None;
                let v = v.trim();
                if v.starts_with('[') {
                    out.extend(list(v).into_iter().map(|p| Include { paths: vec![p], ..Include::default() }));
                    inside = false;
                }
            }
            continue;
        }
        if !inside {
            continue;
        }
        let (dash, rest) = match l.strip_prefix('-') {
            Some(r) if r.is_empty() || r.starts_with(' ') => (true, r.trim()),
            _ => (false, l),
        };
        let kv = rest.split_once(':').filter(|(k, _)| ["path", "project_directory", "env_file"].contains(&k.trim()) || k.trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.contains(['/', '.']));
        if dash && item_ind.is_none_or(|i| ind <= i) {
            item_ind = Some(ind);
            out.push(Include::default());
            match kv {
                Some((k, v)) => {
                    key = k.trim().to_owned();
                    put(&mut out, &key, list(v));
                }
                None => {
                    key.clear();
                    let p = unq(rest);
                    if !p.is_empty() {
                        out.last_mut().unwrap().paths.push(p);
                    }
                }
            }
        } else if dash {
            put(&mut out, &key.clone(), list(rest));
        } else if let Some((k, v)) = kv {
            key = k.trim().to_owned();
            put(&mut out, &key, list(v));
        }
    }
    for i in &mut out {
        i.paths.retain(|p| !p.contains('$'));
        i.env_files.retain(|p| !p.contains('$'));
        i.dir = i.dir.take().filter(|d| !d.contains('$'));
    }
    out.retain(|i| !i.paths.is_empty());
    out
}

/// A local regular file once links are followed: what Kiri reads of a
/// file a project's text names (a pipe would hold Kiri).
fn local_file(f: &Path) -> bool {
    !remote_path(&f.to_string_lossy()) && std::fs::metadata(f).is_ok_and(|m| m.is_file())
}

/// The most Kiri reads of a file a project's text names.
const READ_CAP: u64 = 16 << 20;

/// A local regular file's text ([`local_file`]), as much as [`READ_CAP`]
/// holds, without a byte order mark: bytes that aren't UTF-8 (a Latin-1
/// comment) read as `\u{fffd}`, so the rest of the file is still read.
fn read_text(f: &Path) -> Option<String> {
    use std::io::Read as _;
    if !local_file(f) {
        return None;
    }
    let mut b = Vec::new();
    std::fs::File::open(f).ok()?.take(READ_CAP).read_to_end(&mut b).ok()?;
    Some(String::from_utf8_lossy(b.strip_prefix(BOM).unwrap_or(&b)).into_owned())
}

/// The files the project's `include:` entries bring in, read roughly
/// ([`includes`]) at any depth (up to 16, and 256 files): only to find the
/// values Apply's free text hides; which variables get stand-ins is
/// Compose's own list ([`compose_names`]). Each included compose file, and each env file
/// one is read with (its `env_file`s, else its project folder's `.env`).
/// Local regular files only.
pub fn included(p: &Project) -> (Vec<PathBuf>, Vec<PathBuf>) {
    included_in(p, None)
}

/// [`included`], with `draft` read in place of the first file's text: the
/// page's edit, before it is saved.
pub fn included_in(p: &Project, draft: Option<&str>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut files, mut envs) = (Vec::<PathBuf>::new(), Vec::<PathBuf>::new());
    let first = p.files.first().map(PathBuf::from);
    let mut todo: Vec<(PathBuf, usize)> = p.files.iter().map(|f| (PathBuf::from(f), 0)).collect();
    let mut seen: BTreeSet<PathBuf> = todo.iter().map(|(f, _)| f.clone()).collect();
    while let Some((f, depth)) = todo.pop() {
        let edited = draft.filter(|_| depth == 0 && first.as_ref() == Some(&f));
        if depth >= 16 || (edited.is_none() && !local_file(&f)) {
            continue;
        }
        let base = f.parent().map(Path::to_path_buf).unwrap_or_default();
        let text = edited.map_or_else(|| read_text(&f).unwrap_or_default(), str::to_owned);
        for inc in includes(&text) {
            let paths: Vec<PathBuf> = inc.paths.iter().map(|x| base.join(x)).collect();
            let dir = inc.dir.as_ref().map_or_else(|| paths[0].parent().map(Path::to_path_buf).unwrap_or_default(), |d| base.join(d));
            let mut env: Vec<PathBuf> = inc.env_files.iter().flat_map(|e| [base.join(e), dir.join(e)]).collect();
            if inc.env_files.is_empty() {
                env.push(dir.join(".env"));
            }
            for e in env {
                if local_file(&e) && !envs.contains(&e) && envs.len() < 256 {
                    envs.push(e);
                }
            }
            for x in paths {
                if seen.len() < 256 && seen.insert(x.clone()) {
                    files.push(x.clone());
                    todo.push((x, depth + 1));
                }
            }
        }
    }
    files.retain(|f| local_file(f));
    (files, envs)
}

/// The variables of the env files the project's `include:` entries are
/// read with ([`included`]), read roughly ([`rough_env`]): each gets its
/// stand-in, which the shell's environment gives over an include's own
/// env file as over the project's.
fn include_vars(p: &Project) -> Vec<(String, String)> {
    include_vars_in(p, None)
}

/// [`include_vars`] with `draft` for the first file's text.
fn include_vars_in(p: &Project, draft: Option<&str>) -> Vec<(String, String)> {
    included_in(p, draft).1.iter().filter_map(|f| read_text(f)).flat_map(|t| rough_env(&t)).collect()
}

type Envs = HashMap<String, (Vec<Option<SystemTime>>, Vec<(String, String)>)>;
static ENVS: LazyLock<Mutex<Envs>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `config --environment`'s `NAME=value` lines; a value with a newline goes
/// on over the lines after it, `=` and all (base64's padding, a key's body):
/// a line starts a variable only with a name, after the one before (Compose
/// writes them sorted), and never inside a secret's PEM block (a value that
/// starts with a `-----BEGIN …-----` line, up to its `-----END`): a plain
/// value that only says `-----BEGIN ` never takes the variables after it.
pub fn environment(out: &str) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = Vec::new();
    let name = |k: &str| k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let lines: Vec<&str> = out.lines().map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    // Whether an `-----END ` line comes at or after each line before the
    // next variable that starts a block of its own: a block with no END is
    // only its first line, and takes nothing after it.
    let mut ends = vec![false; lines.len() + 1];
    for i in (0..lines.len()).rev() {
        let l = lines[i];
        let begins = l.split_once('=').is_some_and(|(k, v)| name(k) && v.starts_with("-----BEGIN "));
        ends[i] = l.starts_with("-----END ") || !begins && ends[i + 1];
    }
    for (i, &l) in lines.iter().enumerate() {
        let open = ends[i] && vars.last().is_some_and(|(k, v)| {
            let first = v.lines().next().unwrap_or_default();
            secret_name(k) && first.starts_with("-----BEGIN ") && first.ends_with("-----") && !v.lines().skip(1).any(|l| l.starts_with("-----END "))
        });
        let after = |k: &str| vars.last().is_none_or(|(p, _)| p.as_str() < k);
        match l.split_once('=') {
            Some((k, v)) if name(k) && !v.chars().all(|c| c == '=') && after(k) && !open => vars.push((k.to_owned(), v.to_owned())),
            _ => {
                if let Some((_, v)) = vars.last_mut() {
                    v.push('\n');
                    v.push_str(l);
                }
            }
        }
    }
    vars
}

/// The `.env`'s lines when Compose can't read it: each name and value, the
/// value as written and without quotes or a ` #` comment.
fn env_guess(dir: &str) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = std::env::vars().collect();
    let text = (!dir.is_empty()).then(|| read_text(&Path::new(dir).join(".env"))).flatten().unwrap_or_default();
    vars.extend(rough_env(&text));
    vars
}

/// An env file's lines read roughly: each name with its value as written,
/// without a ` #` comment, and without its quotes (closed or not).
fn rough_env(text: &str) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        let l = l.trim_start_matches("export ");
        let Some((k, v)) = l.split_once('=').or_else(|| l.split_once(':')) else { continue };
        let (k, v) = (k.trim(), v.trim());
        let bare = v.split(" #").next().unwrap_or(v).trim();
        for v in [v, bare, bare.trim_matches(['"', '\''])] {
            vars.push((k.to_owned(), v.to_owned()));
        }
    }
    vars
}

/// The whole parts of a variable's name (split on `_`, `-` and `.`) that
/// say its value is secret when the name ends with one: DB_PASSWORD,
/// GPG_PASSPHRASE, APP_SECRETS, API_TOKENS, DB_CREDS, SENTRY_DSN. TOKEN_ISSUER
/// and AUTH_SERVICE end with something else, and say nothing.
const SECRET_LAST: [&str; 13] = ["PASSWORD", "PASSWD", "PASS", "PASSPHRASE", "SECRET", "SECRETS", "TOKEN", "TOKENS", "CREDENTIAL", "CREDENTIALS", "CREDS", "DSN", "KEY"];
/// Words that make a KEY secret wherever it is in the name (API_KEY_ID,
/// SECRET_KEY_BASE) or joined to it (SECRETKEY, PRIVATEKEY, AUTHKEY).
const KEY_WORDS: [&str; 8] = ["API", "ACCESS", "PRIVATE", "SECRET", "AUTH", "MASTER", "SIGNING", "ENCRYPTION"];
/// A KEY these come before is no secret: a public key, a database's or a
/// keyboard's (PUBLIC_KEY, SORT_KEY, PRIMARY_KEY, HOT_KEY).
const PLAIN_KEYS: [&str; 9] = ["PUBLIC", "PUB", "SORT", "PRIMARY", "FOREIGN", "PARTITION", "HOT", "ROUTING", "CACHE"];
/// Names whose value says where something is, never what it is: a path or
/// an address (a URL's password is hidden on its own, [`url_passwords`]).
const WHERE_LAST: [&str; 6] = ["PATH", "FILE", "DIR", "URL", "URI", "HOST"];

/// Whether a variable's name clearly says its value is secret.
pub fn secret_name(k: &str) -> bool {
    let k = k.to_ascii_uppercase();
    let parts: Vec<&str> = k.split(['_', '-', '.']).filter(|p| !p.is_empty()).collect();
    let Some(last) = parts.last() else { return false };
    // GOOGLE_APPLICATION_CREDENTIALS names the key's file, not the key.
    if WHERE_LAST.contains(last) || k == "GOOGLE_APPLICATION_CREDENTIALS" {
        return false;
    }
    let before = parts.len().checked_sub(2).map(|i| parts[i]);
    if *last == "KEY" {
        return !before.is_some_and(|b| PLAIN_KEYS.contains(&b));
    }
    // A word joined with KEY: SECRETKEY, MASTERKEY (not HOTKEY, MONKEY).
    if let Some(w) = last.strip_suffix("KEY").filter(|w| !w.is_empty()) {
        return KEY_WORDS.contains(&w);
    }
    // AUTHTOKEN and NPM_AUTHTOKEN are tokens: a word ending in PASSWORD,
    // PASSWD, PASSPHRASE, SECRET or TOKEN says secret (PGPASSWORD too).
    if SECRET_LAST.contains(last) || ["PASSWORD", "PASSWD", "PASSPHRASE", "SECRET", "TOKEN"].iter().any(|w| last.ends_with(w)) {
        return true;
    }
    parts.windows(2).any(|w| w[1] == "KEY" && KEY_WORDS.contains(&w[0]))
}

/// A value too plain to hide: three characters or fewer, or a yes or no.
fn plain_value(v: &str) -> bool {
    v.chars().count() <= 3 || ["true", "false", "yes", "no"].iter().any(|w| v.eq_ignore_ascii_case(w))
}

/// A value whose shape says it is no secret, whatever its name: a file's
/// path ([`file_path`]) or a number of up to five digits (a port, a count).
pub fn plain_shape(v: &str) -> bool {
    let b = v.as_bytes();
    file_path(v) || (1..=5).contains(&b.len()) && b.iter().all(u8::is_ascii_digit)
}

/// The folders a path to a file on a machine starts with.
const ROOTS: [&str; 12] = ["etc", "usr", "var", "opt", "home", "Users", "run", "tmp", "mnt", "srv", "data", "app"];

/// Whether `v` reads as a path to a file: relative (`./…`, `../…`, `~/…`)
/// or absolute (`/…`, `C:\…`) with no part over 64 characters, made only of
/// what paths are made of (a key's `+` or `=` says it is none), and with a
/// file's extension or a common first folder. A long part of mixed-case
/// letters and digits with no extension is a key's, not a folder's.
fn file_path(v: &str) -> bool {
    let b = v.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    let relative = ["./", "../", ".\\", "..\\", "~/"].iter().any(|p| v.starts_with(p));
    if !(relative || drive || v.starts_with('/')) || !b.iter().all(|&c| c.is_ascii_alphanumeric() || b"/\\._-~: @".contains(&c)) {
        return false;
    }
    let parts: Vec<&str> = v.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let keyish = |p: &str| p.len() >= 20 && !p.contains('.') && p.bytes().any(|c| c.is_ascii_uppercase()) && p.bytes().any(|c| c.is_ascii_lowercase()) && p.bytes().any(|c| c.is_ascii_digit());
    if parts.iter().any(|p| p.len() > 64 || keyish(p)) {
        return false;
    }
    let first = parts.iter().find(|p| !matches!(**p, "." | ".." | "~"));
    let root = drive || first.is_some_and(|f| ROOTS.contains(f));
    let ext = parts.last().and_then(|l| l.rsplit_once('.')).is_some_and(|(_, e)| (1..=8).contains(&e.len()) && e.bytes().all(|c| c.is_ascii_alphanumeric()));
    root || ext
}

// How secrets are kept out of the window. Before Apply, every `docker
// compose` call that reads the file (the list, the plan, profiles, a file
// opened, Apply's own check) runs with each secret-named variable, and the
// password of each URL among the values, set to a stand-in ([`stand_ins`]),
// which the shell's environment gives over the `.env` and `--env-file`s: the
// value is never in what Compose reads, so never in what it says. Each
// stand-in it says back is shown as its variable, `${DB_PASSWORD}`. Only
// `config --hash` (it prints no values) and Apply's `up` read the real
// values: what `up` says, and the names Kiri shows from what runs (images,
// paths, the engine's own lines in Logs), hide each secret by exact match of
// every form it is printed in ([`forms`]), as its variable too.

/// The secret values Compose puts into a project's file ([`env_vars`]),
/// each with what the window shows in its place: those whose name clearly
/// says secret ([`secret_name`]), each line of one over several lines too,
/// and the password of any URL among the values (`postgres://app:PASS@db`).
#[cfg(test)]
pub fn secrets(cli: Cli, p: &Project) -> Vec<(String, String)> {
    secret_values(env_vars(cli, p))
}

fn secret_values(vars: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in vars {
        out.extend(url_passwords(&v).into_iter().map(|(i, j)| (format!("${{{k} password}}"), v[i..j].to_owned())));
        if secret_name(&k) && !plain_shape(&v) {
            out.extend(v.lines().map(str::trim).filter(|l| *l != v).map(|l| (format!("${{{k}}}"), l.to_owned())));
            out.push((format!("${{{k}}}"), v));
        }
    }
    out.retain(|(_, v)| !plain_value(v));
    out.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.cmp(b)));
    out.dedup_by(|a, b| a.1 == b.1);
    out
}

/// One stand-in: the variable, the value Compose is run with, the token in
/// it, the real value the token stands for, and what the window shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Stand {
    pub name: String,
    pub value: String,
    pub token: String,
    pub real: String,
    pub label: String,
}

/// Each variable's token: lowercase letters and digits (an image tag, a
/// host, a user and a path all take it; Compose lower-cases some errors),
/// with a part no file has (random per run), the same for a name all run.
fn token(key: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    static SALT: LazyLock<u32> = LazyLock::new(|| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.finish() as u32
    });
    let mut t = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    let n = t.len();
    t.entry(key.to_owned()).or_insert_with(|| format!("kirisec{:08x}{n:06}", *SALT)).clone()
}

/// Each variable's token ([`token`]), by its name.
static TOKENS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `text` with any stand-in token left in it as its variable (`${NAME}`),
/// or as `${…}` when no variable has it: the last pass over each event the
/// window gets, so no token is ever shown, whatever said it back.
pub fn untoken(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("kirisec") {
        return text.to_owned();
    }
    let names: HashMap<String, String> = TOKENS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, t)| (t.clone(), k.clone())).collect();
    let (mut out, mut at) = (String::new(), 0);
    for (i, _) in lower.match_indices("kirisec") {
        if i < at {
            continue;
        }
        // A token's shape only (eight hex digits, six digits): a name such
        // as `kirisecure` is the person's own and stays.
        let tail = lower.as_bytes()[i + 7..].iter().take(14).copied().collect::<Vec<u8>>();
        if tail.len() < 14 || !tail[..8].iter().all(u8::is_ascii_hexdigit) || !tail[8..].iter().all(u8::is_ascii_digit) {
            continue;
        }
        let j = i + 21;
        out.push_str(&text[at..i]);
        match names.get(&lower[i..j]) {
            Some(k) => out.push_str(&format!("${{{k}}}")),
            None => out.push_str("${…}"),
        }
        at = j;
    }
    out.push_str(&text[at..]);
    out
}

/// [`untoken`] over each string of `v` (keys too).
pub fn untoken_json(v: Value) -> Value {
    match v {
        Value::String(t) => Value::String(untoken(&t)),
        Value::Array(a) => Value::Array(a.into_iter().map(untoken_json).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (untoken(&k), untoken_json(v))).collect()),
        v => v,
    }
}

/// [`untoken`] for an event the window gets: its JSON's strings, so a
/// variable named with a `"` or `\` still makes JSON the page reads.
/// Text that isn't JSON is untokened as it is.
pub fn untoken_event(ev: &str) -> String {
    if !ev.to_ascii_lowercase().contains("kirisec") {
        return ev.to_owned();
    }
    match serde_json::from_str::<Value>(ev) {
        Ok(v) => untoken_json(v).to_string(),
        Err(_) => untoken(ev),
    }
}

/// The stand-ins for `vars`: a secret-named variable's whole value, and the
/// password of each URL value (the rest of the URL as it is).
pub fn stand_ins(vars: &[(String, String)]) -> Vec<Stand> {
    let mut last: BTreeMap<&str, &str> = BTreeMap::new();
    for (k, v) in vars {
        last.insert(k, v);
    }
    let mut out = Vec::new();
    for (k, v) in last {
        if secret_name(k) && !v.is_empty() && !plain_shape(v) && !["true", "false", "yes", "no"].iter().any(|w| v.eq_ignore_ascii_case(w)) {
            let t = token(k);
            out.push(Stand { name: k.to_owned(), value: t.clone(), token: t, real: v.to_owned(), label: format!("${{{k}}}") });
        } else if let Some(&(i, j)) = url_passwords(v).first() {
            let t = token(&format!("{k} password"));
            let mut value = v.to_owned();
            for (a, b) in url_passwords(v).into_iter().rev() {
                value.replace_range(a..b, &t);
            }
            out.push(Stand { name: k.to_owned(), value, token: t, real: v[i..j].to_owned(), label: format!("${{{k} password}}") });
        }
    }
    out
}

/// The stand-ins a reading of the project runs with ([`stands_for`]).
fn project_stands(cli: Cli, p: &Project) -> Vec<Stand> {
    stands_for(cli, p, None)
}

/// A file stamp: each path with its change time and size.
type Stamp = Vec<(String, Option<(SystemTime, u64)>)>;

/// The names of every variable the whole project's files use, its
/// includes at any depth too, as Compose itself reads them: `config
/// --variables` (names and the defaults the files write, never a value
/// the environment or an env file gives). Only the names are taken and
/// nothing it says is shown: none when Compose can't say them. Kept for a
/// few seconds while the files' change times stay.
fn compose_names(cli: Cli, p: &Project, draft: Option<&str>) -> Option<Vec<Var>> {
    type Names = HashMap<String, (Stamp, std::time::Instant, Option<Vec<Var>>)>;
    static NAMES: LazyLock<Mutex<Names>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    let key = format!("{}\0{}\0{}\0{}\0{}", p.name, p.dir, p.env_files.join("\0"), p.files.join("\0"), draft.map(|d| file_hash(d.as_bytes())).unwrap_or_default());
    let t = |f: &Path| std::fs::metadata(f).and_then(|m| Ok((m.modified()?, m.len()))).ok();
    let mut st = stamp(p);
    st.extend(included_in(p, draft).0.iter().map(|f| (f.to_string_lossy().into_owned(), t(f))));
    if let Some((_, _, v)) = NAMES.lock().unwrap_or_else(|e| e.into_inner()).get(&key).filter(|(s, at, _)| *s == st && at.elapsed() < std::time::Duration::from_secs(3)) {
        return v.clone();
    }
    // An env file that isn't a regular one would hold Compose and Kiri.
    let waits = !p.dir.is_empty() && !regular_or_none(&Path::new(&p.dir).join(".env")) || !p.env_files.iter().all(|f| regular_or_none(Path::new(f))) || service_env_ok(p).is_err();
    let names = (|| {
        if waits {
            return None;
        }
        let args = |json: bool| {
            let mut a = project_args(p, draft.map(|_| "-"));
            a.extend(["config".into(), "--variables".into()]);
            if json {
                a.extend(["--format".into(), "json".into()]);
            }
            a
        };
        let run = |cmd: Command| run_cli(cmd, draft, 30);
        // JSON; an older Compose's table; a Compose without `--variables`: the bundled one's.
        let mut cmd = cli().ok()?;
        cmd.args(args(true));
        if let Some(v) = run(cmd).ok().and_then(|o| variables_json(&o)) {
            return Some(v);
        }
        let mut cmd = cli().ok()?;
        cmd.args(args(false));
        match run(cmd) {
            Ok(o) => variables_table(&String::from_utf8_lossy(&o)),
            Err(e) if e.contains("unknown flag") => {
                let mut cmd = bundled(&cli().ok()?)?;
                cmd.args(args(true));
                run(cmd).ok().and_then(|o| variables_json(&o))
            }
            Err(_) => None,
        }
    })();
    let mut m = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 256 {
        m.clear();
    }
    m.insert(key, (st, std::time::Instant::now(), names.clone()));
    names
}

/// A variable `config --variables` names: whether the file requires it
/// (`${X:?…}`), and whether it gives it a default (`${X:-…}`).
#[derive(Debug, Clone, PartialEq)]
pub struct Var {
    pub name: String,
    pub required: bool,
    pub default: bool,
}

/// `config --variables --format json`'s variables.
pub fn variables_json(out: &[u8]) -> Option<Vec<Var>> {
    let v: Value = serde_json::from_slice(out).ok()?;
    Some(v.as_object()?.iter().map(|(k, x)| Var { name: k.clone(), required: x["Required"] == true, default: x["DefaultValue"].as_str().is_some_and(|d| !d.is_empty()) }).collect())
}

/// The table an older `config --variables` prints (`NAME  REQUIRED  DEFAULT
/// VALUE  ALTERNATE VALUE`): the name column, the required one, and
/// whether the default column has anything, by the header's columns.
pub fn variables_table(out: &str) -> Option<Vec<Var>> {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let head = lines.next()?;
    if !head.starts_with("NAME") {
        return None;
    }
    let col = |w: &str| head.find(w);
    let (req, def, alt) = (col("REQUIRED")?, col("DEFAULT")?, col("ALTERNATE").unwrap_or(usize::MAX));
    let cell = |l: &str, a: usize, b: usize| l.get(a.min(l.len())..b.min(l.len())).unwrap_or_default().trim().to_owned();
    Some(
        lines
            .filter_map(|l| {
                let name = l.split_whitespace().next()?.to_owned();
                Some(Var { name, required: cell(l, req, def) == "true", default: !cell(l, def, alt).is_empty() })
            })
            .collect(),
    )
}

/// The docker CLI Kiri ships (beside Kiri, in Kiri.app's Resources/bin, or
/// docker.exe beside kiri.exe), run as `cmd` is: same arguments and
/// environment. None when Kiri ships none, or `cmd` is that one already.
fn bundled(cmd: &Command) -> Option<Command> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let path = [dir.join("../Resources/bin/docker"), dir.join("docker.exe")].into_iter().find(|d| d.is_file())?;
    let path = std::fs::canonicalize(&path).ok()?;
    if std::fs::canonicalize(cmd.get_program()).ok().as_ref() == Some(&path) {
        return None;
    }
    let mut c = Command::new(path);
    c.args(cmd.get_args());
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    if let Some(d) = cmd.get_current_dir() {
        c.current_dir(d);
    }
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000);
    Some(c)
}

/// The variables `text` names (`${NAME…}`, `$NAME`; `$$` is no variable).
pub fn text_names(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        if b[i] != b'$' {
            i += 1;
            continue;
        }
        if b.get(i + 1) == Some(&b'$') {
            i += 2;
            continue;
        }
        let s = if b.get(i + 1) == Some(&b'{') { i + 2 } else { i + 1 };
        let mut j = s;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
        if j > s && !b[s].is_ascii_digit() {
            out.push(text[s..j].to_owned());
        }
        i = j.max(i + 1);
    }
    out
}

/// [`stand_ins`] of `vars`, and a stand-in for each secret-named variable
/// of `names` whose value Kiri doesn't hold (an included project's env file
/// it couldn't read): its real value is then unknown (empty), and it is
/// hidden all the same. The shell's environment gives a stand-in over every
/// env file, an include's too. A plain value (empty, a path, a short
/// number) is no secret only when it is the project's own (`own`, what wins
/// in Compose): one an include's env file alone gives may stand beside a
/// real secret another include holds, so it gets a stand-in too.
pub fn stand_ins_named(vars: &[(String, String)], names: &BTreeSet<String>, own: &BTreeSet<String>) -> Vec<Stand> {
    let mut out = stand_ins(vars);
    // A value read for a name that isn't the one it ends with (an included
    // project's own, under the project's empty or plain one) may be the one
    // Compose takes: the name gets a stand-in for it.
    let plain = |v: &str| v.is_empty() || plain_shape(v) || ["true", "false", "yes", "no"].iter().any(|w| v.eq_ignore_ascii_case(w));
    for (k, v) in vars.iter().rev() {
        if secret_name(k) && (!plain(v) || !own.contains(k)) && !out.iter().any(|s| &s.name == k) {
            let t = token(k);
            out.push(Stand { name: k.clone(), value: t.clone(), token: t, real: v.clone(), label: format!("${{{k}}}") });
        }
    }
    let held: BTreeSet<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
    for n in names.iter().filter(|n| secret_name(n) && !held.contains(n.as_str())) {
        let t = token(n);
        out.push(Stand { name: n.clone(), value: t.clone(), token: t, real: String::new(), label: format!("${{{n}}}") });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The stand-ins a reading of `draft` (or the saved file) runs with: one
/// for each secret-named variable Compose says the project uses, with
/// those the first file's own text names, and whether Compose said them
/// ([`compose_names`]): a reading made without them is never shown.
fn stands_full(cli: Cli, p: &Project, draft: Option<&str>) -> (Vec<Stand>, bool) {
    let (stands, full, _) = stands_unset(cli, p, draft);
    (stands, full)
}

/// [`stands_full`], and the secret-named variables the file requires
/// (`${X:?…}`, `${X?…}`). Every secret-named variable Compose names gets a
/// stand-in, whatever its default or required marker and whether Kiri
/// holds its value: a value Kiri's own reader misses (an include it can't
/// follow) is never read into the window. Also whether a stand-in has no
/// value Kiri holds, so the real reading may differ ([`config`]).
fn stands_unset(cli: Cli, p: &Project, draft: Option<&str>) -> (Vec<Stand>, bool, Vec<String>) {
    let vars = vars_for(cli, p, draft);
    let text = draft.map_or_else(|| p.files.first().and_then(|f| read_text(Path::new(f))).unwrap_or_default(), str::to_owned);
    let compose = compose_names(cli, p, draft);
    let full = compose.is_some();
    let required: Vec<String> = compose.iter().flatten().filter(|v| v.required && secret_name(&v.name)).map(|v| format!("${{{}}}", v.name)).collect();
    let names: BTreeSet<String> = compose.unwrap_or_default().into_iter().map(|v| v.name).chain(text_names(&text)).collect();
    let own: BTreeSet<String> = own_env(cli, p).into_iter().map(|(k, _)| k).collect();
    (stand_ins_named(&vars, &names, &own), full, required)
}

/// The values a reading of `draft` (the page's edit of the first file)
/// interpolates with: those of the env files its own `include:` entries
/// bring in (an include the saved file lacks yet), then the project's.
fn vars_for(cli: Cli, p: &Project, draft: Option<&str>) -> Vec<(String, String)> {
    let mut v = draft.map(|d| include_vars_in(p, Some(d))).unwrap_or_default();
    v.extend(env_vars(cli, p));
    v
}

/// The stand-ins a reading of `draft` runs with ([`stands_full`]).
fn stands_for(cli: Cli, p: &Project, draft: Option<&str>) -> Vec<Stand> {
    stands_full(cli, p, draft).0
}

/// The secrets of the env files the project's services name (`env_file:`),
/// read roughly ([`rough_env`]) from regular local files only: what `up`
/// or the engine may say of them is hidden too.
fn service_env_secrets(p: &Project, draft: Option<&str>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let inc = included_in(p, draft).0;
    for f in p.files.iter().map(PathBuf::from).chain(inc) {
        let text = read_text(&f).unwrap_or_default();
        let base = f.parent().unwrap_or(Path::new(""));
        for e in env_file_paths(&text) {
            for path in [base.join(&e), Path::new(&p.dir).join(&e)] {
                let Some(t) = read_text(&path) else { continue };
                out.extend(rough_env(&t).into_iter().filter(|(k, v)| secret_name(k) && !plain_shape(v)).map(|(k, v)| (format!("${{{k}}}"), v)));
            }
        }
    }
    out
}

/// `cmd` with each stand-in in its environment, over the `.env`'s value.
fn standing(mut cmd: Command, stands: &[Stand]) -> Command {
    cmd.envs(stands.iter().map(|s| (&s.name, &s.value)));
    cmd
}

/// What Compose read with stand-ins, with the real values back (Apply's
/// own names for what `up` says, never for the window).
fn real(model: &Value, stands: &[Stand]) -> Value {
    let mut text = model.to_string();
    // A value Kiri doesn't hold keeps its token.
    for s in stands.iter().filter(|s| !s.real.is_empty()) {
        text = text.replace(&s.token, &json_escape(&s.real, false));
    }
    serde_json::from_str(&text).unwrap_or_else(|_| model.clone())
}

/// Where each URL's password is in `text` (`scheme://user:PASS@host`), as byte ranges.
fn url_passwords(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (i, _) in text.match_indices("://") {
        let start = i + 3;
        let rest = &text[start..];
        let end = rest.find(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'').unwrap_or(rest.len());
        let Some(at) = rest[..end].rfind('@') else { continue };
        if let Some(colon) = rest[..at].find(':')
            && colon + 1 < at
        {
            out.push((start + colon + 1, start + at));
        }
    }
    out
}

/// A character of a word: a secret is hidden only where it stands whole,
/// never inside a longer word (`root` of `rootfs`, `test` of `latest`).
fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Go's `strconv.Quote` of `v` without its quotes (`ascii`: `QuoteToASCII`, `%+q`).
fn go_quote(v: &str, ascii: bool) -> String {
    let mut o = String::new();
    for c in v.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            '\x07' => o.push_str("\\a"),
            '\x08' => o.push_str("\\b"),
            '\x0c' => o.push_str("\\f"),
            '\x0b' => o.push_str("\\v"),
            c if (c as u32) < 0x20 || c == '\x7f' => o.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{200b}'..='\u{200f}') || (ascii && !c.is_ascii()) => {
                if (c as u32) > 0xffff {
                    o.push_str(&format!("\\U{:08x}", c as u32));
                } else {
                    o.push_str(&format!("\\u{:04x}", c as u32));
                }
            }
            c => o.push(c),
        }
    }
    o
}

/// A JSON string's inside (`html`: as Go's encoding/json writes `<`, `>`, `&`).
fn json_escape(v: &str, html: bool) -> String {
    let mut o = String::new();
    for c in v.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            '\x08' => o.push_str("\\b"),
            '\x0c' => o.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' if html => o.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// Percent-encoded (`form`: a space as `+`), in upper- or lower-case hex.
fn url_encode(v: &str, form: bool, lower: bool) -> String {
    let mut o = String::new();
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => o.push(b as char),
            b' ' if form => o.push('+'),
            b if lower => o.push_str(&format!("%{b:02x}")),
            b => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// Every form in which Go, Compose or a shell writes a value back, whole: as
/// it is, Go-quoted (`%q`, `%+q`), JSON-escaped, URL-encoded, shell-quoted,
/// with Compose's `$$` or an escaped `\$`, a value over several lines with
/// `\r\n` too, and each of those lower-cased. The insides only: the quotes
/// around are the text's own.
pub fn forms(v: &str) -> Vec<String> {
    let mut base = vec![v.to_owned(), v.replace('$', "$$"), v.replace('$', "\\$"), v.replace("\\$", "$"), v.replace("$$", "$")];
    if v.contains('\n') && !v.contains('\r') {
        base.push(v.replace('\n', "\r\n"));
    }
    let mut out = Vec::new();
    for b in base {
        out.extend([go_quote(&b, false), go_quote(&b, true), json_escape(&b, false), json_escape(&b, true)]);
        out.extend([url_encode(&b, false, false), url_encode(&b, true, false), url_encode(&b, false, true)]);
        out.push(b.replace('\'', "'\\''"));
        out.push(b.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace('`', "\\`"));
        // As an older engine wrote its own line: backslashes doubled, line breaks escaped.
        out.push(b.replace('\\', "\\\\"));
        out.push(b.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r"));
        out.push(b);
    }
    let lower: Vec<String> = out.iter().map(|f| f.to_lowercase()).collect();
    out.extend(lower);
    out.sort();
    out.dedup();
    out
}

/// Takes secrets out of the text Kiri shows ([`secrets`]). Only free text
/// that real values can reach (what `up` says, the engine's own lines in
/// Logs, an error) is matched by value: each stand-in Compose says back,
/// and each whole secret in any form it is printed in, become the variable
/// they are; a URL's password no variable gave becomes `…`. A secret is
/// found only as a whole token (`-`, `_` and `.` join words: `redis` is no
/// secret in `redis-cache` or `kiri.redis`), and never where the token is a
/// name the project declares or one Docker reports for it (a container,
/// its image, a volume or network: [`Hider::running`]). Names, paths and
/// the model are never matched by value: their stand-ins alone show as
/// their variables ([`Hider::show_json`]); names from what runs are shown
/// as Docker has them, and a secret used in one is warned of ([`name_vars`]).
#[derive(Debug, Default, Clone)]
pub struct Hider {
    /// What to find, what shows in its place, and whether it is a stand-in
    /// (found in any case, inside a word too); longest first.
    finds: Vec<(String, String, bool)>,
    /// Each secret, and each line of one over several, lower-cased: what a
    /// piece Docker quotes back is looked for in ([`Hider::hide_up`]).
    raw: Vec<String>,
    /// The names the project declares: its services, project, containers,
    /// volumes, networks and image repositories.
    keep: Vec<String>,
    /// Each line of a secret over several lines, and what shows in its place.
    lines: Vec<(String, String)>,
    /// The project's name: `<project>-<service>-1`, `<project>_data` are its own.
    project: String,
    /// The stand-ins the project is read with.
    stands: Vec<Stand>,
    /// The names Docker reports for the project (its containers', their
    /// images, volumes and networks): a token that is one is said as Docker
    /// has it, never hidden or cut ([`Hider::running`]).
    runtime: Vec<String>,
}

/// A character a name, tag or path's part is made of.
fn name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// Whether a word goes on before byte `i` of `text` (`at_end`: after it):
/// `-`, `.` and `_` join two words into one, so `redis` is a word of
/// `redis-cache` and `test` of `kiri-test`; a sentence's `.` joins nothing.
fn joined(text: &str, i: usize, at_end: bool) -> bool {
    let join = |c: char| matches!(c, '-' | '.' | '_');
    let mut it: Box<dyn Iterator<Item = char>> = if at_end { Box::new(text[i..].chars()) } else { Box::new(text[..i].chars().rev()) };
    match it.next() {
        Some(c) if word_char(c) => true,
        Some(c) if join(c) => it.next().is_some_and(|c| c.is_ascii_alphanumeric()),
        _ => false,
    }
}

impl Hider {
    /// `secrets`: what shows in each one's place and the value.
    #[cfg(test)]
    pub fn new(secrets: Vec<(String, String)>, keep: Vec<String>) -> Self {
        Self::with(secrets, &[], keep)
    }

    fn with(secrets: Vec<(String, String)>, stands: &[Stand], keep: Vec<String>) -> Self {
        let mut finds: Vec<(String, String, bool)> = stands.iter().map(|s| (s.token.clone(), s.label.clone(), true)).collect();
        let mut raw = Vec::new();
        for (label, v) in secrets.iter().filter(|(_, v)| !plain_value(v)) {
            finds.extend(forms(v).into_iter().filter(|f| f.chars().count() >= 4).map(|f| (f, label.clone(), false)));
            raw.extend(std::iter::once(v.as_str()).chain(v.lines()).map(str::trim).filter(|l| l.chars().count() >= 6).map(str::to_lowercase));
        }
        finds.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.cmp(b)));
        finds.dedup_by(|a, b| a.0 == b.0);
        raw.sort();
        raw.dedup();
        let mut keep: Vec<String> = keep.into_iter().filter(|k| !k.is_empty()).collect();
        keep.sort();
        keep.dedup();
        let mut lines: Vec<(String, String)> = secrets.iter().filter(|(_, v)| v.contains('\n')).flat_map(|(label, v)| v.lines().map(str::trim).filter(|l| l.chars().count() >= 4).map(|l| (l.to_owned(), label.clone()))).collect();
        lines.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.cmp(b)));
        lines.dedup_by(|a, b| a.0 == b.0);
        Hider { finds, raw, keep, lines, project: String::new(), stands: stands.to_vec(), runtime: Vec::new() }
    }

    /// The project's: its values' and stand-ins' (and the secrets of the
    /// env files its services name), keeping the names the file (and
    /// `model`, or the saved file's reading, and what runs) declares.
    pub fn for_project(cli: Cli, p: &Project, model: Option<&Value>) -> Self {
        Self::for_draft(cli, p, model, None)
    }

    /// [`Hider::for_project`] for a reading of `draft`: the secrets of the
    /// env files its own `include:` entries bring in too.
    pub fn for_draft(cli: Cli, p: &Project, model: Option<&Value>, draft: Option<&str>) -> Self {
        let mut keep: Vec<String> = p.members.iter().map(|m| m.service.clone()).collect();
        keep.push(p.name.clone());
        // Compose reads only files that can't hold it (no pipe for an env file).
        let safe = !p.dir.is_empty() && regular_or_none(&Path::new(&p.dir).join(".env")) && p.env_files.iter().all(|f| regular_or_none(Path::new(f))) && service_env_ok(p).is_ok();
        let saved = if model.is_none() && !p.files.is_empty() && safe { saved_config(cli, p, &[]).ok() } else { None };
        keep.extend(model.or(saved.as_ref()).map(model_names).unwrap_or_default());
        if let Some(f) = p.files.first() {
            keep.extend(declared(&std::fs::read_to_string(f).unwrap_or_default()));
        }
        let vars = vars_for(cli, p, draft);
        let stands = stands_for(cli, p, draft);
        let mut secrets = secret_values(vars);
        secrets.extend(service_env_secrets(p, draft));
        let mut h = Self::with(secrets, &stands, keep);
        h.project = p.name.clone();
        h
    }

    /// The names Docker reports for project `name` in `containers` (a
    /// `/containers/json` list) kept whole in free text: each container's
    /// name and image, and the volumes and networks it uses.
    pub fn running(mut self, name: &str, containers: &Value) -> Self {
        self.runtime.extend(runtime_names(name, containers));
        self.runtime.sort();
        self.runtime.dedup();
        self
    }

    /// These names kept too (a draft's).
    pub fn keeping(mut self, names: Vec<String>) -> Self {
        self.keep.extend(names);
        self
    }

    /// A token the project declares, or one of its own made of one
    /// (`shop-db-1`, `shop_data`, `shop_default`).
    fn kept(&self, t: &str) -> bool {
        if self.keep.iter().any(|k| k == t) || self.runtime.iter().any(|k| k == t) {
            return true;
        }
        let Some(rest) = t.strip_prefix(self.project.as_str()).filter(|_| !self.project.is_empty()).and_then(|r| r.strip_prefix(['-', '_'])) else { return false };
        let svc = rest.rsplit_once(['-', '_']).filter(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).map_or(rest, |(s, _)| s);
        rest == "default" || self.keep.iter().any(|k| k == svc || k == rest)
    }

    fn cut(&self, text: &str, values: bool) -> String {
        let lower = text.to_ascii_lowercase();
        let escaped = |b: &str| ["\\n", "\\r", "\\t"].iter().any(|e| b.ends_with(e)) || b.len() >= 3 && b.is_char_boundary(b.len() - 3) && b[b.len() - 3..].starts_with('%') && b[b.len() - 2..].bytes().all(|c| c.is_ascii_hexdigit());
        let mut cut: Vec<(usize, usize, u8, &str)> = Vec::new();
        // The name a match is part of, whole: one Docker reports stays as it is.
        let whole = |i: usize, j: usize| {
            let part = |c: char| name_char(c) || matches!(c, '/' | ':' | '@');
            let a = text[..i].char_indices().rev().take_while(|(_, c)| part(*c)).last().map_or(i, |(k, _)| k);
            let b = text[j..].char_indices().take_while(|(_, c)| part(*c)).last().map_or(j, |(k, c)| j + k + c.len_utf8());
            let t = text[a..b].trim_start_matches('/').trim_end_matches(['.', ':']);
            self.runtime.iter().any(|r| r == t)
        };
        for (f, label, stand) in self.finds.iter().filter(|(_, _, s)| *s || values) {
            for (i, _) in (if *stand { &lower } else { text }).match_indices(f.as_str()) {
                let j = i + f.len();
                if !*stand {
                    let (before, after) = (f.starts_with(word_char) && !escaped(&text[..i]) && joined(text, i, false), f.ends_with(word_char) && joined(text, j, true));
                    if before || after || self.kept(&text[i..j]) || whole(i, j) {
                        continue;
                    }
                }
                cut.push((i, j, 0, label));
            }
        }
        if values {
            cut.extend(url_passwords(text).into_iter().map(|(i, j)| (i, j, 1, "…")));
        }
        cut.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        let (mut out, mut at) = (String::new(), 0);
        for (i, j, _, label) in cut {
            if i < at {
                continue;
            }
            out.push_str(&text[at..i]);
            out.push_str(label);
            at = j;
        }
        out.push_str(&text[at..]);
        out
    }

    /// Free text, hidden: stand-ins and whole secrets as their variables.
    pub fn hide(&self, text: &str) -> String {
        self.cut(text, true)
    }

    /// A name, a path or a model's text: its stand-ins as their variables, nothing else.
    pub fn show(&self, text: &str) -> String {
        self.cut(text, false)
    }

    /// What `up` says and the engine's own lines, hidden ([`Hider::hide`]);
    /// a line that still has a piece of a secret (six characters or more
    /// between quotes, brackets, `:` or spaces, as Docker cuts and quotes a
    /// value back) says only what kind of error it is. An env file's line
    /// is said without the text it quotes ([`env_read_error`]).
    pub fn hide_up(&self, text: &str) -> String {
        self.hide(text)
            .split('\n')
            .map(|h| {
                let h = env_read_error(h).unwrap_or_else(|| h.to_owned());
                match self.piece(&h) {
                    Some(a) => {
                        let kind = h[..a].trim_end_matches(|c: char| c.is_whitespace() || "\"'([{:`<,=".contains(c));
                        if kind.split(|c: char| !c.is_alphabetic()).any(|w| w.chars().count() >= 3) { kind.to_owned() } else { "docker compose up failed".to_owned() }
                    }
                    None => h,
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Where the first piece of `h` that is part of a secret starts: one
    /// linear pass over its pieces, each looked for in each secret.
    fn piece(&self, h: &str) -> Option<usize> {
        if self.raw.is_empty() {
            return None;
        }
        let delim = |c: char| c.is_whitespace() || "\"'()[]:`,;<>".contains(c);
        let leaks = |p: &str| {
            // A value over several lines, as an escaped line writes it.
            let un = p.replace("\\n", "\n").replace("\\r", "\r");
            let end = p.trim_end_matches(['.', '!', '?']);
            [p, end, un.as_str()].into_iter().chain(p.split(['/', '=', '@', '|', '\\'])).any(|s| {
                // A piece of a secret, never part of a name Docker reports.
                telling(s) && !s.contains("${") && !self.kept(s) && {
                    let l = s.to_lowercase();
                    let t = l.trim_matches('/');
                    self.raw.iter().any(|r| r.contains(&l)) && !self.runtime.iter().any(|r| r.to_lowercase().contains(t))
                }
            })
        };
        let mut start = None;
        for (i, c) in h.char_indices().chain(std::iter::once((h.len(), ' '))) {
            if delim(c) {
                if let Some(s) = start.take()
                    && leaks(&h[s..i])
                {
                    return Some(s);
                }
            } else if start.is_none() {
                start = Some(i);
            }
        }
        None
    }

    /// Each line of a secret over several lines, as its variable, wherever
    /// it is: a line of the program's own that is one of them goes too.
    pub fn hide_lines(&self, text: &str) -> String {
        let mut t = text.to_owned();
        for (l, label) in &self.lines {
            if t.contains(l.as_str()) {
                t = t.replace(l.as_str(), label);
            }
        }
        t
    }

    /// Each string in `v` with its stand-ins as their variables, its keys
    /// as they are: names and paths are never matched by value.
    pub fn show_json(&self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.show(s),
            Value::Array(a) => a.iter_mut().for_each(|x| self.show_json(x)),
            Value::Object(m) => m.values_mut().for_each(|x| self.show_json(x)),
            _ => {}
        }
    }
}

/// Whether `s` could be a piece of a secret, not a plain word (`container`,
/// `postgres`): six characters or more, with a digit, both cases or a
/// symbol, or twelve characters or more.
fn telling(s: &str) -> bool {
    let n = s.chars().count();
    n >= 6 && (s.chars().any(|c| !c.is_alphabetic()) || s.chars().any(char::is_uppercase) && s.chars().any(char::is_lowercase) || n >= 12)
}

/// The names Docker reports for project `name` in a `/containers/json`
/// list: each container's name and image, its volumes' and networks' names.
pub fn runtime_names(name: &str, containers: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for c in containers.as_array().into_iter().flatten().filter(|c| c["Labels"][PROJECT] == name && !name.is_empty()) {
        out.extend(c["Names"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|n| n.trim_start_matches('/').to_owned()));
        out.extend(c["Image"].as_str().filter(|i| !i.starts_with("sha256:")).map(str::to_owned));
        out.extend(c["Mounts"].as_array().into_iter().flatten().filter(|m| m["Type"] == "volume").filter_map(|m| m["Name"].as_str().map(str::to_owned)));
        out.extend(c["NetworkSettings"]["Networks"].as_object().into_iter().flatten().map(|(k, _)| k.clone()));
    }
    out.retain(|n| !n.is_empty());
    out
}

/// The names a reading declares: services, `container_name:`s, volumes and
/// networks (and their `name:`), the project's own `name`, and each image's
/// repository and its parts (`postgres` of `docker.io/library/postgres:16`).
fn model_names(m: &Value) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(m["name"].as_str().map(str::to_owned));
    for (n, s) in m["services"].as_object().into_iter().flatten() {
        out.push(n.clone());
        out.extend(s["container_name"].as_str().map(str::to_owned));
        if let Some(i) = s["image"].as_str() {
            out.extend(image_parts(i).0);
        }
    }
    for k in ["volumes", "networks"] {
        for (n, v) in m[k].as_object().into_iter().flatten() {
            out.push(n.clone());
            out.extend(v["name"].as_str().map(str::to_owned));
        }
    }
    out
}

/// An image's repository and each part of its path (`docker.io/library/postgres`,
/// `postgres`), and its tag (`16`) if it has one.
fn image_parts(i: &str) -> (Vec<String>, Option<String>) {
    let repo = i.split('@').next().unwrap_or(i);
    let (repo, tag) = match repo.rsplit_once(':') {
        Some((r, tag)) if !tag.contains('/') => (r, Some(tag.to_owned())),
        _ => (repo, None),
    };
    let mut out = vec![repo.to_owned()];
    out.extend(repo.split('/').filter(|p| !p.is_empty()).map(str::to_owned));
    (out, tag)
}

/// The names a compose file writes itself: its services ([`declared`]),
/// and each `image:` written without a variable, its repository's parts
/// and its tag.
fn file_names(text: &str) -> Vec<String> {
    let mut out = declared(text);
    for l in text.lines().map(str::trim) {
        let Some(v) = l.trim_start_matches("- ").trim_start().strip_prefix("image:") else { continue };
        let v = v.split(" #").next().unwrap_or(v).trim().trim_matches(['"', '\'']);
        if v.is_empty() || v.contains('$') {
            continue;
        }
        let (parts, tag) = image_parts(v);
        out.extend(parts);
        out.extend(tag);
    }
    out
}

/// An env file's error (`failed to read …/svc.env: line 4: unterminated
/// quoted value "tok…`) as the file, the line and the kind of problem: the
/// text it quotes from the file can be a secret's.
fn env_read_error(t: &str) -> Option<String> {
    let i = t.find("failed to read ")?;
    let (path, after) = t[i + 15..].split_once(": line ")?;
    let (n, what) = after.split_once(": ")?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let kind: String = what.chars().take_while(|c| c.is_ascii_alphabetic() || *c == ' ').collect();
    Some(format!("{}failed to read {path}: line {n}: {}", &t[..i], kind.trim()))
}

/// The services a compose file declares: the keys under its `services:`.
fn declared(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut indent = None::<usize>;
    let mut inside = false;
    for l in text.lines() {
        let t = l.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let d = l.len() - t.len();
        if d == 0 {
            inside = t.trim_end() == "services:";
            indent = None;
            continue;
        }
        if inside && *indent.get_or_insert(d) == d
            && let Some((k, _)) = t.split_once(':')
        {
            out.push(k.trim().trim_matches(['"', '\'']).to_owned());
        }
    }
    out
}

/// The words Kiri itself wrote in an error, as the page translates them:
/// `(key, args)`, or none for Compose's and the system's own.
pub fn error_key(e: &str) -> Option<(&'static str, Value)> {
    let exact = [
        ("pick a .yaml or .yml compose file", "notCompose"),
        (NOT_COMPOSE, "notComposeFile"),
        (NO_FILE, "noFile"),
        ("Kiri has no state folder", "noState"),
        ("The file changed outside Kiri since Apply saved it", "changedSinceApply"),
        ("The file changed outside Kiri", "changed"),
        ("Kiri has nothing to put back", "nothingToRestore"),
        ("Apply cancelled", "cancelled"),
        (READ_ONLY, "readOnly"),
        (BY_NAME, "byName"),
        (REAL_ERR, "rejectReal"),
        ("docker compose up failed", "upFailed"),
        ("docker compose up stopped", "upStopped"),
        ("no docker CLI found", "noCli"),
        ("no such action", "noAction"),
    ];
    if let Some((_, k)) = exact.iter().find(|(t, _)| *t == e) {
        return Some((k, json!({})));
    }
    if let Some(n) = e.strip_prefix("no Compose project ").and_then(|r| r.strip_suffix(" on Kiri")) {
        return Some(("noProject", json!({"name": n})));
    }
    if let Some(n) = e.strip_prefix(REQ_ERR).and_then(|r| r.strip_suffix('.')).filter(|n| n.starts_with("${") && n.split(", ").all(|v| v.starts_with("${") && v.ends_with('}') && !v.contains(' '))) {
        return Some(("reqUnset", json!({"name": n})));
    }
    if let Some(n) = e.strip_prefix(YAML_ERR).filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) {
        return Some(("yamlLine", json!({"n": n})));
    }
    if let Some(n) = e.strip_suffix(" is already being applied") {
        return Some(("busy", json!({"name": n})));
    }
    if let Some((n, d)) = e.strip_prefix("A project named ").and_then(|r| r.split_once(" already runs from ")) {
        return Some(("nameClash", json!({"name": n, "dir": d})));
    }
    if let Some(r) = e.strip_prefix("couldn't run docker compose: ") {
        return Some(("cantRun", json!({"error": r})));
    }
    if let Some(r) = e.strip_prefix("couldn't save the compose file: ") {
        return Some(("cantSave", json!({"error": r})));
    }
    if let Some(f) = e.strip_prefix("couldn't read env file ").and_then(|r| r.strip_suffix(": it isn't a local regular file")) {
        return Some(("badEnv", json!({"file": f})));
    }
    if let Some(n) = e.strip_prefix("the image name from ").and_then(|r| r.strip_suffix(" isn't valid")) {
        return Some(("badImage", json!({"name": n})));
    }
    let secs = e.strip_prefix("docker compose took longer than ").and_then(|r| r.strip_suffix(" seconds"));
    secs.map(|n| ("slow", json!({"secs": n})))
}

/// Runs a short docker CLI command (not `up`) with `input` on its stdin: its
/// stdout, or its stderr when it fails or takes longer than `secs`. Past
/// that, the whole tree goes (the `docker-compose` plugin `docker` started
/// too), and its pipes are never waited on: a child that still holds them
/// keeps nothing of Kiri's waiting.
/// How a short CLI run ended ([`run_ran`]).
#[derive(Debug, PartialEq)]
pub enum Ran {
    Passed(Vec<u8>),
    /// Its exit code wasn't 0: what it said on stderr.
    Failed(String),
    /// Killed after this many seconds.
    TimedOut(u64),
}

impl Ran {
    /// Its output, or why not in Kiri's words for a timeout.
    fn result(self) -> Result<Vec<u8>, String> {
        match self {
            Ran::Passed(o) => Ok(o),
            Ran::Failed(e) => Err(e),
            Ran::TimedOut(secs) => Err(format!("docker compose took longer than {secs} seconds")),
        }
    }
}

fn run_cli(cmd: Command, input: Option<&str>, secs: u64) -> Result<Vec<u8>, String> {
    run_ran(cmd, input, secs).result()
}

/// Runs `cmd` with `input` on its stdin for at most `secs` seconds.
fn run_ran(mut cmd: Command, input: Option<&str>, secs: u64) -> Ran {
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Ran::Failed(format!("couldn't run docker compose: {e}")),
    };
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        let text = text.to_owned();
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let read = |r: Option<Box<dyn io::Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut b = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut b);
            }
            let _ = tx.send(b);
        });
        rx
    };
    let out = read(child.stdout.take().map(|o| Box::new(o) as Box<dyn io::Read + Send>));
    let err = read(child.stderr.take().map(|o| Box::new(o) as Box<dyn io::Read + Send>));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if std::time::Instant::now() < until => std::thread::sleep(std::time::Duration::from_millis(20)),
            _ => {
                kill_tree(&mut child);
                break None;
            }
        }
    };
    // What the pipes had by now: a plugin left holding them never makes this wait.
    let grace = std::time::Duration::from_secs(if status.is_some() { 5 } else { 1 });
    let take = |rx: std::sync::mpsc::Receiver<Vec<u8>>| rx.recv_timeout(grace).unwrap_or_default();
    let (out, err) = (take(out), take(err));
    match status {
        Some(s) if s.success() => Ran::Passed(out),
        Some(_) => Ran::Failed(String::from_utf8_lossy(&err).trim().to_owned()),
        None => Ran::TimedOut(secs),
    }
}

/// Kills `child` and every process it started (its own process group; on
/// Windows, its tree).
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: a plain signal to the group this child leads (`process_group(0)`).
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &child.id().to_string()]).creation_flags(0x0800_0000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The files opened in the window, as projects with no containers yet.
fn opened(store: Store) -> Vec<Project> {
    let v: Value = store().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    v["opened"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let (name, file) = (o["name"].as_str()?, o["file"].as_str()?);
            let dir = Path::new(file).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
            Some(Project { name: name.to_owned(), dir, files: vec![file.to_owned()], members: Vec::new(), env_files: Vec::new() })
        })
        .collect()
}

/// The projects that have run (from their containers), then the opened
/// files whose project hasn't (by name).
pub fn merge(mut ran: Vec<Project>, opened: Vec<Project>) -> Vec<Project> {
    for o in opened {
        if !ran.iter().any(|p| p.name == o.name) {
            ran.push(o);
        }
    }
    ran.sort_by(|a, b| a.name.cmp(&b.name));
    ran
}

fn all_projects(containers: &Value, store: Store) -> Vec<Project> {
    merge(projects(containers), opened(store))
}

/// Saves the opened list: `keep` decides what stays of what was there.
fn save_opened(store: Store, keep: impl Fn(&str, &str) -> bool, add: Option<(&str, &str)>) -> io::Result<()> {
    let path = store().ok_or_else(|| io::Error::other("Kiri has no state folder"))?;
    let mut list: Vec<Value> = opened(store).into_iter().filter(|p| keep(&p.name, &p.files[0])).map(|p| json!({"name": p.name, "file": p.files[0]})).collect();
    if let Some((name, file)) = add {
        list.push(json!({"name": name, "file": file}));
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json!({"opened": list}).to_string())?;
    std::fs::rename(&tmp, &path)
}

/// The names `docker compose` looks for, which the file dialogs offer first.
#[cfg_attr(target_os = "macos", expect(dead_code, reason = "the Mac's open panel offers kinds of file, not names"))]
pub const FILE_NAMES: [&str; 4] = ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];

/// A file the dialog may hand back: a .yaml or .yml file (the dialog
/// offers compose.yaml, compose.yml, docker-compose.yaml and
/// docker-compose.yml first).
pub fn compose_like(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
}

/// The project name Compose gives a folder when the file names none:
/// lower case, letters, digits, `-` and `_` (Compose's own rule).
pub fn folder_name(dir: &Path) -> String {
    let base = dir.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let s: String = base.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_').collect();
    let s = s.trim_start_matches(['-', '_']).to_owned();
    if s.is_empty() { "project".into() } else { s }
}

/// A file picked in the system's dialog: its project (the name Compose
/// gives it) is remembered and listed. `{type: "composeopened", name}`, or
/// `{…, error}`. The path never goes to the page.
pub fn open_file(cli: Cli, store: Store, path: &Path) -> String {
    let r = (|| {
        if !compose_like(path) || !path.is_file() {
            return Err("pick a .yaml or .yml compose file".to_owned());
        }
        if !compose_text(&std::fs::read_to_string(path).map_err(|e| e.to_string())?) {
            return Err(NOT_COMPOSE.to_owned());
        }
        let path = real_path(path).map_err(|e| e.to_string())?;
        let dir = path.parent().unwrap_or(Path::new("/"));
        let file = path.to_string_lossy().into_owned();
        let mut p = Project { name: folder_name(dir), dir: dir.to_string_lossy().into_owned(), files: vec![file.clone()], members: Vec::new(), env_files: Vec::new() };
        p.name = opened_name(cli, &p);
        // The name is another folder's project: never shown as this file's.
        let containers = client::get_background("/containers/json?all=1").unwrap_or_default();
        if let Some(dir) = clash(&p, &all_projects(&containers, store)) {
            return Err(format!("A project named {} already runs from {dir}", p.name));
        }
        save_opened(store, |n, f| n != p.name && f != file, Some((&p.name, &file))).map_err(|e| e.to_string())?;
        Ok(p.name)
    })();
    match r {
        Ok(name) => json!({"type": "composeopened", "name": name}).to_string(),
        Err(e) => own_event(json!({"type": "composeopened"}), e),
    }
}

/// The name Compose gives a file just opened: its own `name:` wins, as with
/// docker compose up. With an env file Compose would wait on, the `name:` as
/// written, and Compose isn't asked (the list then says why).
fn opened_name(cli: Cli, p: &Project) -> String {
    let waits = !regular_or_none(&Path::new(&p.dir).join(".env")) || service_env_ok(p).is_err();
    if waits {
        return yaml_name(&std::fs::read_to_string(&p.files[0]).unwrap_or_default()).unwrap_or_else(|| p.name.clone());
    }
    match config(cli, &Project { name: String::new(), ..p.clone() }, None, &[]) {
        // A name made of a secret is never taken.
        Ok(m) if m.get(UNCHECKED).is_none() => m["name"].as_str().filter(|n| !n.is_empty() && !project_stands(cli, p).iter().any(|s| n.contains(&s.token))).map_or_else(|| p.name.clone(), str::to_owned),
        // Compose read it only with the real values: the file's own name, as written.
        _ => written_name(&std::fs::read_to_string(&p.files[0]).unwrap_or_default(), &env_vars(cli, p)).unwrap_or_else(|| p.name.clone()),
    }
}

/// What Kiri says of a YAML file that isn't a compose file (a Kubernetes manifest).
pub const NOT_COMPOSE: &str = "This file isn't a compose file: it has no services";

/// Whether a YAML file reads as a compose file: a top-level `services` or
/// `include` key (a Kubernetes manifest has `apiVersion:` and `kind:`
/// instead), quoted or not, at the indent of the file's first key however
/// deep that is, or in a top-level flow mapping (`{services: {…}}`). Text
/// only: Compose never runs on a file not opened yet.
pub fn compose_text(text: &str) -> bool {
    let wanted = |k: &str| matches!(k.trim().trim_matches(['"', '\'']), "services" | "include");
    let lines: Vec<&str> = text.trim_start_matches('\u{feff}').lines().filter(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#') && !t.starts_with('%') && t != "---" && t != "..."
    }).collect();
    let Some(first) = lines.first() else { return false };
    let first = first.trim_start_matches("--- ");
    let indent = |l: &str| l.len() - l.trim_start_matches([' ', '\t']).len();
    if first.trim_start().starts_with('{') {
        return flow_keys(&lines.join("\n")).iter().any(|k| wanted(k));
    }
    let top = indent(first);
    lines.iter().filter(|l| indent(l) == top && !l.trim_start().starts_with('-')).any(|l| l.split_once(':').is_some_and(|(k, _)| wanted(k)))
}

/// The keys of the flow mapping `text` starts with (`{a: 1, "b": {c: 2}}`
/// gives `a`, `"b"`): its own, not its values' nor nested ones.
fn flow_keys(text: &str) -> Vec<String> {
    let (mut out, mut depth, mut key, mut at_key, mut quote) = (Vec::new(), 0usize, String::new(), false, None::<char>);
    for c in text.trim_start().chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            if depth == 1 && at_key {
                key.push(c);
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                if depth == 1 && at_key {
                    key.push(c);
                }
            }
            '{' | '[' => {
                depth += 1;
                at_key = depth == 1 && c == '{';
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            ',' if depth == 1 => {
                at_key = true;
                key.clear();
            }
            ':' if depth == 1 && at_key => {
                out.push(std::mem::take(&mut key));
                at_key = false;
            }
            _ if depth == 1 && at_key => key.push(c),
            _ => {}
        }
    }
    out
}

/// The folder another project of the same name runs from (or was opened
/// from), when it isn't this file's: two files are never shown as one.
pub fn clash(p: &Project, known: &[Project]) -> Option<String> {
    let k = known.iter().find(|k| k.name == p.name)?;
    let same = |f: &String| p.files.iter().any(|g| valid_file(f).zip(valid_file(g)).is_some_and(|(a, b)| a == b));
    let file_gone = k.members.is_empty() && valid_file(&k.files[0]).is_none();
    (!k.files.iter().any(same) && !file_gone).then(|| if k.dir.is_empty() { k.files.first().cloned().unwrap_or_default() } else { k.dir.clone() })
}

/// The CLI's arguments that name a project: its name, directory and files.
fn project_args(p: &Project, first: Option<&str>) -> Vec<String> {
    let mut a = vec!["compose".to_owned(), "--ansi".into(), "never".into()];
    if !p.name.is_empty() {
        a.extend(["-p".into(), p.name.clone()]);
    }
    if !p.dir.is_empty() {
        a.extend(["--project-directory".into(), p.dir.clone()]);
    }
    for e in &p.env_files {
        a.extend(["--env-file".into(), e.clone()]);
    }
    for (i, f) in p.files.iter().enumerate() {
        a.push("-f".into());
        a.push(if i == 0 { first.unwrap_or(f).to_owned() } else { f.clone() });
    }
    a
}

/// `--profile` for each profile Apply must keep on (see [`profiles`]).
fn profile_args(profiles: &[String]) -> Vec<String> {
    profiles.iter().flat_map(|p| ["--profile".to_owned(), p.clone()]).collect()
}

/// `docker compose config --format json`, of the file on disk, or of `draft`
/// in place of the project's first file (piped in as `-f -`).
/// Run with stand-ins for the secrets ([`stand_ins`]): neither the reading
/// nor an error holds one, and no real value is ever read into it. A
/// stand-in can't take the place of every value (a path, a profile, a
/// size): when the file reads with the real values and not with the
/// stand-ins, the reading is the last one the stand-ins gave (or none),
/// marked [`UNCHECKED`] with the variables the error names; nothing the
/// real run says is kept but its exit code. When the services the
/// stand-ins give differ from the real ones ([`same_services`]), the real
/// names are shown, those the stand-ins miss with no details ([`HIDDEN`]).
fn config(cli: Cli, p: &Project, draft: Option<&str>, profiles: &[String]) -> Result<Value, CfgErr> {
    config_state(cli, p, draft, profiles).0
}

/// [`config`], and whether Compose couldn't list the file's variables:
/// then Kiri reads nothing of the file but where a YAML error is
/// ([`unlisted`]), and the project is managed by name ([`CfgErr::by_name`]).
fn config_state(cli: Cli, p: &Project, draft: Option<&str>, profiles: &[String]) -> (Result<Value, CfgErr>, bool) {
    // Only the YAML error's shape leaves [`unlisted`]: it needs no stand-in,
    // so nothing else (not the env files) is read by Compose first.
    if compose_names(cli, p, draft).is_none() {
        return (Err(unlisted(cli, p, draft, &[]).into()), true);
    }
    let (stands, _, required) = stands_unset(cli, p, draft);
    let r = config_of(cli, p, draft, profiles, &stands).map_err(|e| match e.key {
        // Kiri's own words, keyed by Kiri: as they are.
        Some(_) => e,
        // Where a YAML error is, decided here from Compose's own text.
        None => match yaml_at(&e, p, true) {
            Some(YamlAt::Top(n)) => CfgErr { at: Some(YamlAt::Top(n)), ..e },
            Some(at) => CfgErr { text: at.text(), at: Some(at), ..CfgErr::default() },
            None => e,
        },
    });
    let r = r.and_then(|m| {
        // A stand-in passes where the real value may not (required, unset):
        // the real run's exit code says so, never a word of it.
        let unheld = stands.iter().any(|s| s.real.is_empty());
        if (unheld || !required.is_empty()) && m.get(UNCHECKED).is_none() && !real_passes(cli, p, draft, profiles).map_err(CfgErr::own)? {
            return Err(CfgErr::own(if required.is_empty() { REAL_ERR.to_owned() } else { format!("{REQ_ERR}{}.", required.join(", ")) }));
        }
        Ok(m)
    });
    (r, false)
}

/// What Kiri may say of a file whose variables Compose can't list: a YAML
/// syntax error's line, and whether it is in the open file; else nothing,
/// and the project is managed by name. Never a word Compose said.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unlisted {
    Yaml { line: u64, top: bool },
    ByName,
}

/// [`Unlisted`] of a file Compose can't list the variables of: one
/// stand-in `config`, read only by the YAML error's shape ([`yaml_place`]).
fn unlisted(cli: Cli, p: &Project, draft: Option<&str>, stands: &[Stand]) -> Unlisted {
    let Err(e) = config_with(cli, p, draft, &[], stands) else { return Unlisted::ByName };
    match yaml_at(&e, p, false) {
        Some(YamlAt::Top(line)) => Unlisted::Yaml { line, top: true },
        Some(YamlAt::Other { line, .. }) => Unlisted::Yaml { line, top: false },
        None => Unlisted::ByName,
    }
}

impl From<Unlisted> for CfgErr {
    fn from(u: Unlisted) -> Self {
        match u {
            Unlisted::Yaml { line, top: true } => CfgErr { at: Some(YamlAt::Top(line)), ..CfgErr::own(format!("{YAML_ERR}{line}")) },
            Unlisted::Yaml { line, top: false } => {
                let at = YamlAt::Other { line, file: None };
                CfgErr { text: at.text(), key: at.key(), at: Some(at), by_name: false }
            }
            Unlisted::ByName => CfgErr::by_name(),
        }
    }
}

/// Where a YAML error is: the file read or edited (`-` for a draft), or
/// another one (an `include:` or `extends:` file), with its path relative
/// to the project folder when the name list is complete.
#[derive(Clone, Debug, PartialEq)]
pub enum YamlAt {
    Top(u64),
    Other { line: u64, file: Option<String> },
}

impl YamlAt {
    /// Kiri's own words for an error in another file.
    fn text(&self) -> String {
        match self {
            YamlAt::Top(n) => format!("{YAML_ERR}{n}"),
            YamlAt::Other { line, file: Some(f) } => format!("{YAML_IN}{f}, line {line}"),
            YamlAt::Other { line, file: None } => format!("{YAML_INC}{line}"),
        }
    }

    /// The page's key for an error in another file (none for the top one's).
    fn key(&self) -> Option<(&'static str, Value)> {
        match self {
            YamlAt::Top(_) => None,
            YamlAt::Other { line, file: Some(f) } => Some(("yamlIn", json!({"n": line.to_string(), "file": f}))),
            YamlAt::Other { line, file: None } => Some(("yamlInc", json!({"n": line.to_string()}))),
        }
    }
}

/// Why `config` can't read a file: the text, and where a YAML error is.
/// `key` is set only by Kiri, for its own words ([`CfgErr::own`]): text a
/// process said is never keyed. `by_name`: Compose can't list the file's
/// variables, so Kiri reads nothing of it and manages it by name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CfgErr {
    pub text: String,
    pub at: Option<YamlAt>,
    pub key: Option<(&'static str, Value)>,
    pub by_name: bool,
}

impl CfgErr {
    /// Kiri's own words, with the page's key for them.
    fn own(text: impl Into<String>) -> Self {
        let text = text.into();
        CfgErr { key: error_key(&text), text, ..CfgErr::default() }
    }

    /// A file Kiri manages by name only ([`BY_NAME`]).
    fn by_name() -> Self {
        CfgErr { by_name: true, ..CfgErr::own(BY_NAME) }
    }
}

/// What a process said: never keyed.
impl From<String> for CfgErr {
    fn from(text: String) -> Self {
        CfgErr { text, ..CfgErr::default() }
    }
}
impl From<&str> for CfgErr {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}
impl From<&String> for CfgErr {
    fn from(text: &String) -> Self {
        text.clone().into()
    }
}
impl From<&CfgErr> for CfgErr {
    fn from(e: &CfgErr) -> Self {
        e.clone()
    }
}
impl From<CfgErr> for String {
    fn from(e: CfgErr) -> Self {
        e.text
    }
}
impl std::ops::Deref for CfgErr {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}
impl std::fmt::Display for CfgErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// [`YamlAt`] of Compose's error `e`, from its shape only.
fn yaml_at(e: &str, p: &Project, full: bool) -> Option<YamlAt> {
    let (path, n) = yaml_place(e)?;
    let top = |f: &str| p.files.first().is_some_and(|t| Path::new(t) == Path::new(f) || std::fs::canonicalize(t).ok().is_some_and(|c| std::fs::canonicalize(f).ok() == Some(c)));
    match path {
        None | Some("-") => Some(YamlAt::Top(n)),
        Some(f) if top(f) => Some(YamlAt::Top(n)),
        Some(f) => Some(YamlAt::Other { line: n, file: full.then(|| relative(Path::new(&p.dir), Path::new(f))) }),
    }
}

/// `path` relative to `dir` (`../x/compose.yaml` outside it).
fn relative(dir: &Path, path: &Path) -> String {
    let (d, f) = (std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned()), std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
    let (a, b): (Vec<_>, Vec<_>) = (d.components().collect(), f.components().collect());
    let same = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in same..a.len() {
        out.push("..");
    }
    for c in &b[same..] {
        out.push(c);
    }
    out.to_string_lossy().replace('\\', "/")
}

/// What Kiri says of a file Compose rejects with its real values only.
pub const REAL_ERR: &str = "Compose rejects this file with its real values. Kiri can't show why with secrets hidden.";
/// … when the file requires a secret-named variable: the start, then the variables.
pub const REQ_ERR: &str = "Compose rejects this file. A required variable may not be set: ";
/// … when Compose can't list the variables and the file isn't YAML: the start, then the line.
pub const YAML_ERR: &str = "Compose rejects this file: YAML error on line ";
/// … of an error in another file, by its path: the start, then `path, line N`.
pub const YAML_IN: &str = "Compose rejects this file: YAML error in ";
/// … of an error in another file, by no path: the start, then the line.
pub const YAML_INC: &str = "Compose rejects this file: YAML error in an included file, line ";

/// The line of a YAML syntax error, from its shape only: `yaml: line N:`
/// or `go-yaml load error in parser (while …) at LN.C…`, at the start or
/// after `failed to parse <path>: `. Nothing else of the text is read.
#[cfg(test)]
pub fn yaml_line(e: &str) -> Option<u64> {
    yaml_place(e).map(|(_, n)| n)
}

/// [`yaml_line`], with the path `failed to parse` names (none when it names none).
fn yaml_place(e: &str) -> Option<(Option<&str>, u64)> {
    let first = e.lines().map(str::trim).find(|l| !l.is_empty() && !(l.starts_with("time=") && l.contains(" level=warning ")))?;
    let num = |s: &str| -> Option<(u64, usize)> {
        let d = s.bytes().take_while(u8::is_ascii_digit).count();
        Some((s[..d].parse().ok()?, d))
    };
    let shape = |t: &str| -> Option<u64> {
        if let Some(r) = t.strip_prefix("yaml: line ") {
            let (n, d) = num(r)?;
            return r[d..].starts_with(':').then_some(n);
        }
        let mut r = t.strip_prefix("go-yaml load error in ")?;
        r = ["parser ", "scanner ", "composer "].iter().find_map(|w| r.strip_prefix(w))?;
        if let Some(x) = r.strip_prefix("(while ") {
            let (ctx, rest) = x.split_once(')')?;
            if !ctx.bytes().all(|b| b.is_ascii_lowercase() || b == b' ') {
                return None;
            }
            r = rest.strip_prefix(' ')?;
        }
        let (mut n, d) = num(r.strip_prefix("at L")?)?;
        let mut r = r[4 + d..].strip_prefix(".C")?;
        r = &r[num(r)?.1..];
        if let Some(x) = r.strip_prefix("-L") {
            let (m, d) = num(x)?;
            x[d..].strip_prefix(".C")?;
            n = m;
        }
        Some(n)
    };
    shape(first).map(|n| (None, n)).or_else(|| {
        let r = first.strip_prefix("failed to parse ")?;
        r.match_indices(": ").find_map(|(i, _)| Some((Some(&r[..i]), shape(&r[i + 2..])?)))
    })
}

fn config_of(cli: Cli, p: &Project, draft: Option<&str>, profiles: &[String], stands: &[Stand]) -> Result<Value, CfgErr> {
    let key = format!("{}\0{}\0{}", p.name, p.dir, profiles.join(","));
    match config_with(cli, p, draft, profiles, stands) {
        Ok(v) => {
            let v = same_services(cli, p, draft, profiles, stands, v);
            GOOD.lock().unwrap_or_else(|e| e.into_inner()).insert(key, v.clone());
            Ok(v)
        }
        Err(e) if stands.is_empty() => Err(e),
        Err(e) => {
            if !real_passes(cli, p, draft, profiles).map_err(CfgErr::own)? {
                return Err(e);
            }
            let lower = e.text.to_ascii_lowercase();
            let named: Vec<&str> = stands.iter().filter(|s| lower.contains(&s.token)).map(|s| s.label.as_str()).collect();
            let named = if named.is_empty() { stands.iter().map(|s| s.label.as_str()).collect() } else { named };
            // The services are the real run's names; the last stand-in
            // reading gives details only to those whose names still match.
            let mut m = GOOD.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned().unwrap_or_else(|| json!({"services": {}}));
            fit_services(&mut m, &real_services(cli, p, draft, profiles).unwrap_or_default());
            fill_hidden(&mut m, &draft.map_or_else(|| p.files.first().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default(), str::to_owned));
            m[UNCHECKED] = named.join(", ").into();
            Ok(m)
        }
    }
}

/// What Kiri says of a project it manages by name: Compose can't list
/// its file's variables, so Kiri reads nothing of the file ([`Unlisted`]).
pub const BY_NAME: &str = "Kiri can't check this file's variables, so it manages this project by name. Apply changes from a terminal.";
/// … beside it while the project has containers: their names are Docker's.
pub const NAMES_SHOWN: &str = "Names Docker shows may contain secret values.";

/// On a reading the stand-ins couldn't make though the real values can:
/// the variables to name ([`config`]); empty when Compose couldn't list them.
const UNCHECKED: &str = "x-kiri-unchecked";
/// On a service only the real values give: its name is all Kiri shows.
const HIDDEN: &str = "x-kiri-hidden";

/// The last reading the stand-ins gave, per project and profiles.
static GOOD: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// What the window says of a project it manages by name ([`BY_NAME`]).
fn by_name_note() -> Value {
    json!({"text": BY_NAME, "errkey": "byName", "errargs": {}})
}

/// What the window says of a reading marked [`UNCHECKED`] (and its key).
fn unchecked_note(m: &Value) -> Option<Value> {
    let name = m[UNCHECKED].as_str()?;
    Some(json!({"text": format!("Kiri can't check this file with {name} hidden. Compose accepts it, so Apply still works."), "errkey": "hiddenCheck", "errargs": {"name": name}}))
}

/// The variables (`${NAME}`) of `stands` used in a field Docker shows as a
/// name: a service's `container_name`, `image` and `hostname`, a bind
/// mount's source, a volume's or network's `name:`, the project's `name`.
/// From the stand-in reading `model`; from the file's `text` too when the
/// reading is the last one ([`UNCHECKED`]) or none.
pub fn name_vars(model: Option<&Value>, text: &str, stands: &[Stand]) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    if let Some(m) = model {
        fields.extend(m["name"].as_str().map(str::to_owned));
        for s in m["services"].as_object().into_iter().flatten().map(|(_, s)| s) {
            fields.extend(["container_name", "image", "hostname"].iter().filter_map(|k| s[*k].as_str().map(str::to_owned)));
            fields.extend(s["volumes"].as_array().into_iter().flatten().filter(|v| v["type"] == "bind").filter_map(|v| v["source"].as_str().map(str::to_owned)));
        }
        for k in ["volumes", "networks"] {
            fields.extend(m[k].as_object().into_iter().flatten().filter_map(|(_, v)| v["name"].as_str().map(str::to_owned)));
        }
    }
    let mut out: Vec<String> = stands.iter().filter(|st| fields.iter().any(|f| f.to_ascii_lowercase().contains(&st.token))).map(|st| st.label.clone()).collect();
    if model.is_none_or(|m| m[UNCHECKED].is_string()) {
        // `name:`, `image:`, `container_name:`, `hostname:`, or a short volume `- ${X}/data:/d`.
        let uses = |v: &str, n: &str| {
            v.match_indices('$').any(|(i, _)| {
                let r = v[i + 1..].strip_prefix('{').unwrap_or(&v[i + 1..]);
                r.strip_prefix(n).is_some_and(|a| !a.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
            })
        };
        for l in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
            let key = l.trim_start_matches("- ").trim();
            let named = ["name:", "image:", "container_name:", "hostname:", "source:"].iter().any(|k| key.starts_with(k));
            let bind = l.starts_with("- ") && key.trim_matches(['"', '\'']).split(':').next().is_some_and(|src| src.contains('$') && (src.contains('/') || src.starts_with('.') || src.starts_with('$')) && key.contains(":/"));
            if named || bind {
                out.extend(stands.iter().filter(|st| !st.label.ends_with(" password}") && uses(l, &st.name)).map(|st| st.label.clone()));
            }
        }
    }
    let mut seen = Vec::new();
    out.retain(|l| if seen.contains(l) { false } else { seen.push(l.clone()); true });
    out
}

/// The variables of `stands` whose value is in a name Docker reports for
/// the project (`names`, [`docker_names`]): anywhere inside one, in any
/// case (`appDpl…` too). Only a telling value counts
/// ([`telling`]): a plain word (`POSTGRES_PASSWORD=postgres` beside a
/// running `postgres:16-alpine`) would warn of names that hold no secret.
/// So is one that is just a name the project itself writes (`declared`: an
/// image's path part or literal tag, a service, project, volume or network).
pub fn value_vars(names: &[String], stands: &[Stand], declared: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for st in stands.iter().filter(|st| telling(&st.real) && !declared.iter().any(|d| d.eq_ignore_ascii_case(&st.real))) {
        let v = st.real.to_ascii_lowercase();
        let found = names.iter().any(|n| n.to_ascii_lowercase().contains(&v));
        if found && !out.contains(&st.label) {
            out.push(st.label.clone());
        }
    }
    out
}

/// Every name Docker reports for project `name`: the project's own, its
/// containers', their images, volumes and networks ([`runtime_names`]), and
/// their bind folders.
pub fn docker_names(name: &str, containers: &Value) -> Vec<String> {
    let mut out = vec![name.to_owned()];
    out.extend(runtime_names(name, containers));
    for c in containers.as_array().into_iter().flatten().filter(|c| c["Labels"][PROJECT] == name && !name.is_empty()) {
        out.extend(c["Mounts"].as_array().into_iter().flatten().filter(|m| m["Type"] == "bind").filter_map(|m| m["Source"].as_str().map(str::to_owned)));
    }
    out
}

/// [`name_vars`] and [`value_vars`] together, each variable once.
fn both_vars(mut a: Vec<String>, b: Vec<String>) -> Vec<String> {
    for v in b {
        if !a.contains(&v) {
            a.push(v);
        }
    }
    a
}

/// The warning for [`name_vars`]: its own key, the variables named.
pub fn name_note(vars: &[String]) -> Option<Value> {
    let list = match vars {
        [] => return None,
        [a] => a.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    let verb = if vars.len() == 1 { "is" } else { "are" };
    Some(json!({"text": format!("{list} {verb} used in a name or folder path. Docker shows those in plain text."), "errkey": "nameSecret", "errargs": {"name": list, "names": vars}}))
}

/// Whether Compose accepts the file with its real values; a run that takes
/// too long says so (Kiri's own words), not that Compose rejects it.
fn real_passes(cli: Cli, p: &Project, draft: Option<&str>, profiles: &[String]) -> Result<bool, String> {
    let Ok(mut cmd) = cli() else { return Ok(false) };
    cmd.args(project_args(p, draft.map(|_| "-"))).args(profile_args(profiles)).args(["config", "--quiet"]);
    // How it ended, never a word it said: a failure that says "took longer" is still a failure.
    match run_ran(cmd, draft, REAL_SECS.with(|s| s.get())) {
        Ran::Passed(_) => Ok(true),
        t @ Ran::TimedOut(_) => t.result().map(|_| false),
        Ran::Failed(_) => Ok(false),
    }
}

thread_local! {
    /// The real check's time limit (a test shortens it).
    static REAL_SECS: std::cell::Cell<u64> = const { std::cell::Cell::new(30) };
}

/// One `config` run, with `stands` in place of their values.
/// Compose's own words come back unkeyed; a timeout is Kiri's ([`CfgErr::own`]).
fn config_with(cli: Cli, p: &Project, draft: Option<&str>, profiles: &[String], stands: &[Stand]) -> Result<Value, CfgErr> {
    let mut cmd = standing(cli().map_err(|e| CfgErr::own(e.to_string()))?, stands);
    cmd.args(project_args(p, draft.map(|_| "-"))).args(profile_args(profiles)).args(["config", "--format", "json"]);
    let out = match run_ran(cmd, draft, 30) {
        Ran::Passed(o) => o,
        Ran::Failed(e) => return Err(e.into()),
        t @ Ran::TimedOut(_) => return Err(CfgErr::own(t.result().err().unwrap_or_default())),
    };
    serde_json::from_slice(&out).map_err(|e| format!("docker compose config: {e}").into())
}

/// `model` with the services the real values give (`config --services`,
/// names only: a service's key is never interpolated), when a variable in
/// the file's `profiles:`, `extends:` or `include:` may change them. One
/// the stand-ins miss is there by name only ([`HIDDEN`]); one only they
/// give is left out. No value of the real run is read.
fn same_services(cli: Cli, p: &Project, draft: Option<&str>, profiles: &[String], stands: &[Stand], mut model: Value) -> Value {
    let text = draft.map_or_else(|| p.files.iter().map(|f| std::fs::read_to_string(f).unwrap_or_default()).collect::<Vec<_>>().join("\n"), str::to_owned);
    if !stands.iter().any(|s| structural(&text, &s.name)) {
        return model;
    }
    if let Some(want) = real_services(cli, p, draft, profiles) {
        fit_services(&mut model, &want);
        fill_hidden(&mut model, &text);
    }
    model
}

/// The services' names with the real values (`config --services`): plain
/// names, as a service's key is never interpolated.
fn real_services(cli: Cli, p: &Project, draft: Option<&str>, profiles: &[String]) -> Option<BTreeSet<String>> {
    let mut cmd = cli().ok()?;
    cmd.args(project_args(p, draft.map(|_| "-"))).args(profile_args(profiles)).args(["config", "--services"]);
    let out = run_cli(cmd, draft, 30).ok()?;
    Some(String::from_utf8_lossy(&out).lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
}

/// Each service's `image:` and short-form `ports:` as the file's `text`
/// writes them, only those that hold no variable (`$`): what a service
/// Kiri can't read with its secrets hidden still shows.
pub fn text_details(text: &str) -> BTreeMap<String, (Option<String>, Vec<String>)> {
    let mut out: BTreeMap<String, (Option<String>, Vec<String>)> = BTreeMap::new();
    let (mut in_services, mut svc_indent, mut svc, mut in_ports) = (false, None::<usize>, None::<String>, None::<usize>);
    let value = |v: &str| {
        let v = v.split(" #").next().unwrap_or(v).trim();
        // An alias, an anchor, a block scalar or a tag: not a value as it is.
        if v.starts_with(['*', '&', '>', '|', '!']) {
            return None;
        }
        let v = v.trim_matches(['"', '\'']);
        (!v.is_empty() && !v.contains('$')).then(|| v.to_owned())
    };
    for l in text.lines() {
        let t = l.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let d = l.len() - t.len();
        if d == 0 {
            in_services = t.trim_end() == "services:";
            svc = None;
            svc_indent = None;
            continue;
        }
        if !in_services {
            continue;
        }
        if svc_indent.is_none_or(|i| d <= i) && t.ends_with(':') && !t.starts_with('-') {
            svc_indent = Some(d);
            let n = t.trim_end_matches(':').trim_matches(['"', '\'']).to_owned();
            out.entry(n.clone()).or_default();
            svc = Some(n);
            in_ports = None;
            continue;
        }
        let Some(n) = svc.clone() else { continue };
        if in_ports.is_some_and(|i| d < i || d == i && !t.starts_with('-')) {
            in_ports = None;
        }
        if in_ports.is_some() {
            if let Some(p) = t.strip_prefix("- ").and_then(value).filter(|p| !p.contains(':') || p.rsplit(':').next().is_some_and(|x| x.split('/').next().is_some_and(|x| x.bytes().all(|b| b.is_ascii_digit() || b == b'-')))) {
                let p = p.split('/').next().unwrap_or(&p).to_owned();
                let parts: Vec<&str> = p.split(':').collect();
                let shown = match parts.as_slice() {
                    [t] => (*t).to_owned(),
                    [h, t] => format!("{h}:{t}"),
                    [_, h, t] => format!("{h}:{t}"),
                    _ => continue,
                };
                out.entry(n).or_default().1.push(shown);
            }
            continue;
        }
        if let Some(v) = t.strip_prefix("image:") {
            out.entry(n).or_default().0 = value(v);
        } else if t.trim_end() == "ports:" {
            in_ports = Some(d);
        }
    }
    out
}

/// The services of `model` only the real values give ([`HIDDEN`]) with
/// what the file's text says of them plainly ([`text_details`]).
fn fill_hidden(model: &mut Value, text: &str) {
    let details = text_details(text);
    let Some(svcs) = model["services"].as_object_mut() else { return };
    for (n, s) in svcs.iter_mut().filter(|(_, s)| s[HIDDEN] == true) {
        let Some((image, ports)) = details.get(n) else { continue };
        if let Some(i) = image {
            s["image"] = i.clone().into();
        }
        if !ports.is_empty() {
            s["ports"] = ports.iter().map(|p| match p.rsplit_once(':') {
                Some((h, t)) => json!({"published": h, "target": t.parse::<u64>().ok()}),
                None => json!({"target": p.parse::<u64>().ok()}),
            }).collect();
        }
    }
}

/// `model`'s services made `want`: one not in it goes, one it lacks is
/// there by name only ([`HIDDEN`]).
fn fit_services(model: &mut Value, want: &BTreeSet<String>) {
    if !model["services"].is_object() {
        model["services"] = json!({});
    }
    let Some(svcs) = model["services"].as_object_mut() else { return };
    svcs.retain(|k, _| want.contains(k));
    for w in want {
        svcs.entry(w.clone()).or_insert_with(|| json!({HIDDEN: true}));
    }
}

/// Whether `text` names the variable `name` in a `profiles:`, `extends:`
/// or `include:` (on its line or under it).
fn structural(text: &str, name: &str) -> bool {
    let names = |l: &str| {
        l.match_indices('$').any(|(i, _)| {
            let r = l[i + 1..].strip_prefix('{').unwrap_or(&l[i + 1..]);
            r.strip_prefix(name).is_some_and(|after| !after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
        })
    };
    let mut under = None::<usize>;
    for l in text.lines() {
        let s = l.trim_start();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        let d = l.len() - s.len();
        if under.is_some_and(|u| d < u || d == u && !s.starts_with('-')) {
            under = None;
        }
        let key = s.trim_start_matches("- ");
        if ["profiles:", "extends:", "include:"].iter().any(|k| key.starts_with(k)) {
            under = Some(d);
            if names(l) {
                return true;
            }
            continue;
        }
        if under.is_some() && names(l) {
            return true;
        }
    }
    false
}

/// The change times a reading depends on: the files, the folder's `.env`,
/// the files an `include:` or `extends:` names (relative to the folder), and
/// each include's env files and `.env` (one not there yet too), as [`env_vars`] reads them.
fn stamp(p: &Project) -> Vec<(String, Option<(SystemTime, u64)>)> {
    let t = |f: &Path| std::fs::metadata(f).and_then(|m| Ok((m.modified()?, m.len()))).ok();
    let mut out: Vec<(String, Option<(SystemTime, u64)>)> = p.files.iter().map(|f| (f.clone(), t(Path::new(f)))).collect();
    let dir = Path::new(&p.dir);
    out.push((".env".into(), t(&dir.join(".env"))));
    out.extend(p.env_files.iter().map(|f| (f.clone(), t(Path::new(f)))));
    let (inc_files, inc_envs) = included(p);
    let inc_dotenvs = inc_files.iter().filter_map(|f| Some(f.parent()?.join(".env")));
    out.extend(inc_files.iter().cloned().chain(inc_envs).chain(inc_dotenvs).map(|f| (f.to_string_lossy().into_owned(), t(&f))));
    for f in &p.files {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        // The env files its services name: one broken since says so in the list.
        let base = Path::new(f).parent().unwrap_or(Path::new(""));
        for e in env_file_paths(&text) {
            for path in [base.join(&e), dir.join(&e)] {
                out.push((path.to_string_lossy().into_owned(), t(&path)));
            }
        }
        let guess = if text.contains("${") { env_guess(&p.dir) } else { Vec::new() };
        for l in text.lines() {
            let l = l.trim().trim_start_matches("- ").trim_start_matches("path:").trim_start_matches("file:").trim().trim_matches(['"', '\'']);
            // A path of plain variables (`${SUBDIR:-sub}/compose.yaml`): the file
            // it names now, so a change to it is seen at once. An extra only: a
            // names-only reading is read again soon whatever its stamp says.
            let filled = if l.contains("${") { fill_plain(l, &guess) } else { Some(l.to_owned()) };
            if let Some(l) = filled.filter(|l| compose_like(Path::new(l)) && !l.contains(' ')) {
                let path = dir.join(l);
                out.push((path.to_string_lossy().into_owned(), t(&path)));
            }
        }
    }
    out
}

/// `v` with each `${VAR}`, `${VAR:-x}` or `${VAR-x}` filled from `vars` (the
/// last one wins), else its default; none when a variable is secret-named,
/// required, or has neither.
pub fn fill_plain(v: &str, vars: &[(String, String)]) -> Option<String> {
    let (mut out, mut rest) = (String::new(), v);
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let (inner, after) = rest[i + 2..].split_once('}')?;
        let (k, def, empty_too) = match inner.split_once(":-") {
            Some((k, d)) => (k, Some(d), true),
            None => match inner.split_once('-') {
                Some((k, d)) => (k, Some(d), false),
                None => (inner, None, false),
            },
        };
        if secret_name(k) || k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        let val = vars.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).filter(|v| !(empty_too && v.is_empty()));
        out.push_str(val.or(def)?);
        rest = after;
    }
    out.push_str(rest);
    (!out.contains('$')).then_some(out)
}

/// The saved file's reading (or why Compose can't read it), kept while the
/// change times it depends on stay; by name ([`Unlisted`]), while the top
/// file's stays, for [`BY_NAME_FOR`] at most.
#[derive(Clone)]
struct Reading {
    stamp: Stamp,
    at: std::time::Instant,
    /// Compose couldn't list the variables ([`config_state`]): files Kiri can't find may change it.
    loose: bool,
    read: Result<Value, CfgErr>,
    /// The variables in the file's own `name:` ([`own_name_vars`]), once asked for.
    name: Option<Vec<String>>,
}
type Readings = HashMap<String, Reading>;
static READ: LazyLock<Mutex<Readings>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// How long a reading by name is kept while the top file stays: its stamp
/// can't cover a file Kiri can't find (`include: - ${SUBDIR:-sub}/compose.yaml`).
const BY_NAME_FOR: std::time::Duration = std::time::Duration::from_secs(30);

/// Every reading of project `name` read again on the next ask (after an Apply).
pub fn forget_readings(name: &str) {
    let pre = format!("{name}\0");
    READ.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| !k.starts_with(&pre));
}

fn reading_key(p: &Project, profiles: &[String]) -> String {
    format!("{}\0{}\0{}", p.name, p.files.join("\0"), profiles.join(","))
}

/// One `config` of a file at a time: a second asker waits for the first's reading.
static READING: LazyLock<Mutex<Locks>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn saved_config(cli: Cli, p: &Project, profiles: &[String]) -> Result<Value, CfgErr> {
    saved_reading(cli, p, profiles, false).read
}

/// The saved file's [`Reading`]: kept while its stamp stays; one by name
/// while the top file's stays, for [`BY_NAME_FOR`] at most (once in that
/// time, however often the list asks). With `name`, the variables of the
/// file's own `name:` too: by name, from its text alone.
fn saved_reading(cli: Cli, p: &Project, profiles: &[String], name: bool) -> Reading {
    let key = reading_key(p, profiles);
    let lock = READING.lock().unwrap_or_else(|e| e.into_inner()).entry(PathBuf::from(&key)).or_default().clone();
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    let stamp = stamp(p);
    let kept = READ.lock().unwrap_or_else(|e| e.into_inner()).get(&key).filter(|r| if r.loose { r.stamp.first() == stamp.first() && r.at.elapsed() < BY_NAME_FOR } else { r.stamp == stamp }).cloned();
    let mut r = kept.unwrap_or_else(|| {
        let (read, loose) = config_state(cli, p, None, profiles);
        Reading { stamp, at: std::time::Instant::now(), loose, read, name: None }
    });
    if name && r.name.is_none() {
        let text = || p.files.first().and_then(|f| read_text(Path::new(f))).map(|t| name_text_vars(&t)).unwrap_or_default();
        r.name = Some(if r.loose { text() } else { own_name_vars(cli, p, None).unwrap_or_else(text) });
    }
    READ.lock().unwrap_or_else(|e| e.into_inner()).insert(key, r.clone());
    r
}

/// The profiles to keep on: those of the services that run now but that the
/// file leaves out without them (`up --profile debug` made them), so plan and
/// Apply both keep them, as `docker compose --profile debug up` would.
fn profiles(cli: Cli, p: &Project, draft: &str, without: &Value, running: &BTreeMap<String, Running>) -> Vec<String> {
    let missing: Vec<&String> = running.keys().filter(|n| without["services"].get(n.as_str()).is_none()).collect();
    if missing.is_empty() {
        return Vec::new();
    }
    let Ok(all) = config(cli, p, Some(draft), &["*".to_owned()]) else { return Vec::new() };
    let mut out: Vec<String> = missing.iter().flat_map(|n| all["services"][n.as_str()]["profiles"].as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_str().map(str::to_owned)).collect();
    out.sort();
    out.dedup();
    out
}

/// What the page shows of one service: no environment values, which can
/// hold secrets; its names as the file gives them.
fn service_view(name: &str, s: &Value) -> Value {
    let strs = |v: &Value| -> Vec<String> {
        match v {
            Value::Object(m) => m.keys().cloned().collect(),
            Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
            _ => Vec::new(),
        }
    };
    let ports: Vec<String> = s["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let target = p["target"].as_u64().map(|t| t.to_string()).unwrap_or_default();
            match p["published"].as_str().filter(|x| !x.is_empty()) {
                Some(host) => format!("{host}:{target}"),
                None => target,
            }
        })
        .collect();
    let volumes: Vec<String> = s["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| {
            let src = v["source"].as_str().unwrap_or_default();
            let tgt = v["target"].as_str().unwrap_or_default();
            if src.is_empty() { tgt.to_owned() } else { format!("{src}:{tgt}") }
        })
        .collect();
    if s[HIDDEN] == true {
        // What the file says of it plainly ([`fill_hidden`]), nothing else.
        return json!({"name": name, "hidden": true, "image": s["image"].as_str().unwrap_or_default(), "build": "", "ports": ports, "volumes": [], "depends": [], "networks": [], "kinds": []});
    }
    json!({
        "name": name,
        "image": s["image"].as_str().unwrap_or_default(),
        "build": s["build"]["context"].as_str().unwrap_or_default(),
        "ports": ports,
        "volumes": volumes,
        "depends": strs(&s["depends_on"]),
        "networks": strs(&s["networks"]),
        "kinds": volumes_named(s),
    })
}

fn volumes_named(s: &Value) -> Vec<String> {
    s["volumes"].as_array().into_iter().flatten().filter(|v| v["type"] == "volume").filter_map(|v| v["source"].as_str().map(str::to_owned)).collect()
}

/// The services in the order Compose starts them: what a service depends
/// on first, then the file's order (name order, as config writes them).
pub fn start_order(model: &Value) -> Vec<String> {
    let Some(svcs) = model["services"].as_object() else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    fn visit(n: &str, svcs: &serde_json::Map<String, Value>, out: &mut Vec<String>, seen: &mut Vec<String>) {
        if out.iter().any(|x| x == n) || seen.iter().any(|x| x == n) {
            return;
        }
        seen.push(n.to_owned());
        if let Some(deps) = svcs.get(n).and_then(|s| s["depends_on"].as_object()) {
            for d in deps.keys() {
                if svcs.contains_key(d) {
                    visit(d, svcs, out, seen);
                }
            }
        }
        out.push(n.to_owned());
    }
    let mut seen = Vec::new();
    for n in svcs.keys() {
        visit(n, svcs, &mut out, &mut seen);
    }
    out
}

/// What Apply does to one service.
#[derive(Debug, PartialEq)]
pub struct Step {
    pub service: String,
    /// "add", "chg", "start", "rm" or "same".
    pub act: &'static str,
    /// What changed: image, ports, environment, volumes, depends_on, build,
    /// settings; or outside (the file changed since it started, not by Kiri),
    /// pulled (its image name points at a newer image).
    pub what: Vec<&'static str>,
    pub image: (String, String),
}

/// What runs of a service now, as Compose decides from it: the
/// container's config-hash label, the image it was made from, its state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Running {
    pub hash: String,
    /// The image ID the container was made from (com.docker.compose.image, else its ImageID).
    pub image_id: String,
    /// The image ID its image name points at now, when Kiri has the image.
    pub image_now: Option<String>,
    pub running: bool,
    pub image: String,
}

/// What Apply will do, against what is running, as Compose decides it:
/// a service whose container's config-hash differs from the draft's (Compose's
/// own `config --hash`), or whose image name now points at another image, is
/// recreated; one with no container is created; a stopped one is started;
/// containers of services the draft no longer has are removed (their volumes
/// stay). What changed is read off the saved file; a change that isn't there
/// was made to the file outside Kiri since the service started.
///
/// The image a container runs (`from`) is Docker's name for it, shown as
/// Docker has it.
pub fn plan_running(old: &Value, new: &Value, hashes: &HashMap<String, String>, now: &BTreeMap<String, Running>) -> Vec<Step> {
    let from = |_: &str, r: &Running| r.image.clone();
    let by_file: HashMap<String, Step> = plan(old, new).into_iter().map(|s| (s.service.clone(), s)).collect();
    let mut out = Vec::new();
    let mut same = Vec::new();
    for n in start_order(new) {
        let to = new["services"][&n]["image"].as_str().unwrap_or_default().to_owned();
        let Some(r) = now.get(&n) else {
            out.push(Step { service: n, act: "add", what: Vec::new(), image: (String::new(), to) });
            continue;
        };
        let image = (from(&n, r), to);
        let changed = hashes.get(&n).is_some_and(|h| *h != r.hash);
        let newer = r.image_now.as_ref().is_some_and(|i| !r.image_id.is_empty() && *i != r.image_id);
        if changed || newer {
            let mut what = by_file.get(&n).filter(|s| s.act == "chg").map(|s| s.what.clone()).unwrap_or_default();
            if changed && what.is_empty() {
                what.push("outside");
            }
            if newer && !changed {
                what.push("pulled");
            }
            out.push(Step { service: n, act: "chg", what, image });
        } else if !r.running {
            out.push(Step { service: n, act: "start", what: Vec::new(), image });
        } else {
            same.push(Step { service: n, act: "same", what: Vec::new(), image });
        }
    }
    let has = |n: &str| new["services"].get(n).is_some();
    for (n, r) in now.iter().filter(|(n, _)| !has(n)) {
        out.push(Step { service: n.clone(), act: "rm", what: Vec::new(), image: (from(n, r), String::new()) });
    }
    out.extend(same);
    out
}

/// A service to recreate though its file is as it was when it started
/// (the draft is the saved file, and no file of the project changed since
/// its container was made): a variable it uses changed (a `.env`'s value).
pub fn vars_changed(steps: &mut [Step], unedited: bool, files: &[PathBuf], members: &[Member]) {
    if !unedited {
        return;
    }
    let changed = |f: &PathBuf| std::fs::metadata(f).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
    let newest = files.iter().filter_map(changed).max();
    for st in steps.iter_mut().filter(|s| s.act == "chg" && s.what == ["outside"]) {
        let made = members.iter().filter(|m| m.service == st.service).map(|m| m.created).max();
        if made.zip(newest).is_some_and(|(m, f)| f <= m) {
            st.what = vec!["vars"];
        }
    }
}

/// `docker compose config --hash '*'`: each service's hash, as Compose
/// labels the containers it makes, for the draft in place of the first file.
/// With the real values, as `up` hashes them: what it prints holds none,
/// and its error is never shown.
fn hashes(cli: Cli, p: &Project, draft: &str, profiles: &[String]) -> Result<HashMap<String, String>, String> {
    let mut cmd = cli().map_err(|e| e.to_string())?;
    cmd.args(project_args(p, Some("-"))).args(profile_args(profiles)).args(["config", "--hash", "*"]);
    let out = run_cli(cmd, Some(draft), 30)?;
    Ok(parse_hashes(&String::from_utf8_lossy(&out)))
}

fn parse_hashes(text: &str) -> HashMap<String, String> {
    text.lines().filter_map(|l| l.trim().split_once(' ')).map(|(a, b)| (a.to_owned(), b.trim().to_owned())).collect()
}

/// What runs of each of the project's services now (the newest container
/// of each), and the image ID each one's image name points at.
fn running(p: &Project, containers: &Value) -> BTreeMap<String, Running> {
    let mut out: BTreeMap<String, (i64, Running)> = BTreeMap::new();
    let mut images: HashMap<String, Option<String>> = HashMap::new();
    for c in containers.as_array().into_iter().flatten() {
        let label = |k: &str| c["Labels"][k].as_str().unwrap_or_default().to_owned();
        if label(PROJECT) != p.name || label(SERVICE).is_empty() {
            continue;
        }
        let created = c["Created"].as_i64().unwrap_or_default();
        if out.get(&label(SERVICE)).is_some_and(|(t, _)| *t > created) {
            continue;
        }
        let image = c["Image"].as_str().unwrap_or_default().to_owned();
        let image_now = images
            .entry(image.clone())
            .or_insert_with(|| {
                if image.is_empty() || image.starts_with("sha256:") {
                    return None;
                }
                client::get_background(&format!("/images/{}/json", super::needs::esc(&image))).ok().and_then(|i| i["Id"].as_str().map(str::to_owned))
            })
            .clone();
        let image_id = Some(label("com.docker.compose.image")).filter(|s| !s.is_empty()).or_else(|| c["ImageID"].as_str().map(str::to_owned)).unwrap_or_default();
        let r = Running { hash: label("com.docker.compose.config-hash"), image_id, image_now, running: c["State"] == "running", image };
        out.insert(label(SERVICE), (created, r));
    }
    out.into_iter().map(|(k, (_, r))| (k, r)).collect()
}

/// What Apply will do, from the saved reading to the draft's: new and
/// changed services (in start order) and removed ones first, then the
/// unchanged ones, which keep running.
pub fn plan(old: &Value, new: &Value) -> Vec<Step> {
    let empty = serde_json::Map::new();
    let a = old["services"].as_object().unwrap_or(&empty);
    let b = new["services"].as_object().unwrap_or(&empty);
    let mut out = Vec::new();
    let mut same = Vec::new();
    for n in start_order(new) {
        let s = &b[&n];
        let image = (a.get(&n).and_then(|x| x["image"].as_str()).unwrap_or_default().to_owned(), s["image"].as_str().unwrap_or_default().to_owned());
        let Some(was) = a.get(&n) else {
            out.push(Step { service: n, act: "add", what: Vec::new(), image });
            continue;
        };
        if was == s {
            same.push(Step { service: n, act: "same", what: Vec::new(), image });
            continue;
        }
        let mut what = Vec::new();
        for (key, word) in [("image", "image"), ("ports", "ports"), ("environment", "environment"), ("volumes", "volumes"), ("depends_on", "depends_on"), ("build", "build")] {
            if was[key] != s[key] {
                what.push(word);
            }
        }
        let rest = |v: &Value| {
            let mut v = v.clone();
            if let Some(m) = v.as_object_mut() {
                for k in ["image", "ports", "environment", "volumes", "depends_on", "build"] {
                    m.remove(k);
                }
            }
            v
        };
        if rest(was) != rest(s) {
            what.push("settings");
        }
        out.push(Step { service: n, act: "chg", what, image });
    }
    for n in a.keys().filter(|n| !b.contains_key(*n)) {
        out.push(Step { service: n.clone(), act: "rm", what: Vec::new(), image: (a[n]["image"].as_str().unwrap_or_default().to_owned(), String::new()) });
    }
    out.extend(same);
    out
}

/// The line a Compose or YAML error points at, if it names one: Compose
/// v5 says `at L2.C3-L4.C4` (the end is where it gave up), YAML errors
/// `line 7:` (the last one: `line 1: line 5: mapping key "image" already
/// defined at line 3` is about line 5).
pub fn error_line(text: &str) -> Option<u64> {
    let num = |s: &str| s.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u64>().ok();
    let mut last = None;
    for (i, _) in text.match_indices('L') {
        let rest = &text[i + 1..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let boundary = i == 0 || !text.as_bytes()[i - 1].is_ascii_alphanumeric();
        if boundary && digits > 0 && rest[digits..].starts_with(".C") {
            last = num(rest);
        }
    }
    let yaml = text.match_indices("line ").filter_map(|(i, _)| {
        let rest = &text[i + 5..];
        let d = rest.chars().take_while(char::is_ascii_digit).count();
        (d > 0 && rest[d..].starts_with(':')).then(|| num(rest)).flatten()
    });
    last.or_else(|| yaml.last())
}

/// The service a schema error is about (`services.api additional
/// properties…`, `service "api" depends on…`), if it names one.
pub fn error_service(text: &str) -> Option<String> {
    let word = |s: &str| s.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect::<String>();
    if let Some(i) = text.find("services.") {
        let w = word(&text[i + 9..]);
        let w = w.split('.').next().unwrap_or_default().to_owned();
        if !w.is_empty() {
            return Some(w);
        }
    }
    let i = text.find("service \"")?;
    let w = word(&text[i + 9..]);
    (!w.is_empty()).then_some(w)
}

/// The line `service` starts on under the top-level `services:` of `text`.
pub fn service_line(text: &str, service: &str) -> Option<u64> {
    let mut inside = false;
    for (i, l) in text.lines().enumerate() {
        if !l.starts_with([' ', '\t']) && !l.trim().is_empty() {
            inside = l.trim_end().trim_end_matches(':').trim() == "services";
            continue;
        }
        let t = l.trim_start();
        let indent = l.len() - t.len();
        if inside && (1..=4).contains(&indent) && t.trim_matches(['"', '\'']).split(':').next().map(|k| k.trim().trim_matches(['"', '\''])) == Some(service) && t.contains(':') {
            return Some(i as u64 + 1);
        }
    }
    None
}

/// The error as a person reads it: the CLI's text, without its own prefixes.
fn error_text(text: &str) -> String {
    // An env file's line is never quoted back ([`env_read_error`]).
    let text = &text.lines().map(|l| env_read_error(l).unwrap_or_else(|| l.to_owned())).collect::<Vec<_>>().join("\n");
    // Without the CLI's warnings (`time="…" level=warning msg="…"`) before it.
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty() && !(l.starts_with("time=") && l.contains(" level=warning "))).collect();
    let mut t = lines.first().copied().unwrap_or(text.trim());
    // `decoding failed due to the following error(s):` says what in the lines after it.
    let more = if t.ends_with(':') && lines.len() > 1 { format!("\n{}", lines[1..].join("\n")) } else { String::new() };
    if let Some(r) = t.strip_prefix("failed to parse ").and_then(|r| r.split_once(": ")) {
        t = r.1;
    }
    if let Some(r) = t.strip_prefix("validating ").and_then(|r| r.split_once(": ")) {
        t = r.1;
    }
    for p in ["go-yaml load error in parser ", "go-yaml load error in scanner ", "go-yaml load error ", "yaml: ", "construct errors: ", "unmarshal errors: "] {
        t = t.strip_prefix(p).unwrap_or(t);
    }
    // `line 1: line 5: mapping key…`: the first is where the document starts.
    while let Some(r) = t.strip_prefix("line ").and_then(|r| r.split_once(": ")).filter(|(n, rest)| n.bytes().all(|b| b.is_ascii_digit()) && rest.starts_with("line ")) {
        t = r.1;
    }
    t.strip_suffix(": invalid compose project").unwrap_or(t).to_owned() + &more
}

/// The line of the port an `invalid hostPort: abc` (or containerPort)
/// error is about: the port entry that says `abc`, or that names a
/// variable whose value (from `vars`, the `.env`, or the environment) is.
pub fn port_line(text: &str, e: &str, vars: &[(String, String)]) -> Option<u64> {
    let i = e.find("Port: ")?;
    let bad = e[i + 6..].split_whitespace().next()?.trim_matches(['"', '\'']);
    if bad.is_empty() || !e[..i].ends_with("invalid host") && !e[..i].ends_with("invalid container") {
        return None;
    }
    let pieces = |l: &str| l.trim_start().trim_start_matches('-').split([':', '"', '\'', '/', ' ', '[', ']', ',']).map(str::to_owned).collect::<Vec<_>>();
    let value = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).or_else(|| std::env::var(name).ok());
    let names_bad = |l: &str| {
        l.match_indices('$').any(|(j, _)| {
            let rest = l[j + 1..].trim_start_matches('{');
            let n: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            !n.is_empty() && value(&n).is_some_and(|v| v.split([':', '/']).any(|p| p == bad))
        })
    };
    // Inside a `ports:` list first; then anywhere.
    let mut in_ports = None::<usize>;
    let mut anywhere = None;
    for (n, l) in text.lines().enumerate() {
        let t = l.trim_start();
        let indent = l.len() - t.len();
        if in_ports.is_some_and(|d| indent <= d && !t.is_empty() && !t.starts_with('-')) {
            in_ports = None;
        }
        if t.starts_with("ports:") {
            in_ports = Some(indent);
        }
        let hit = pieces(t).iter().any(|p| p == bad) || names_bad(t);
        if hit && (in_ports.is_some()) {
            return Some(n as u64 + 1);
        }
        if hit && anywhere.is_none() {
            anywhere = Some(n as u64 + 1);
        }
    }
    anywhere
}

/// What the file tab shows next to "Line N.": the error without the
/// parser's own place in it (`line 5: `, `(while scanning…) at L3.C1: `).
fn shown_text(t: &str) -> String {
    let mut t = t;
    if t.starts_with('(')
        && let Some(i) = t.find(") at L")
        && let Some(j) = t[i..].find(": ")
    {
        t = &t[i + j + 2..];
    }
    if let Some((n, rest)) = t.strip_prefix("line ").and_then(|r| r.split_once(": "))
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
    {
        t = rest;
    }
    t.to_owned()
}

/// `{line, text}` for the page: the line the error names, else the line of
/// the port or service it names, in `draft`; secret values taken out.
fn error_json(e: impl Into<CfgErr>, draft: &str, vars: &[(String, String)], hider: &Hider) -> Value {
    let err: CfgErr = e.into();
    // An error in another file: no line of this one is marked.
    if let Some((k, mut a)) = err.at.as_ref().and_then(YamlAt::key) {
        if let Some(f) = a["file"].as_str().map(|f| hider.hide(f)) {
            a["file"] = f.into();
        }
        return json!({"line": null, "text": hider.hide(&err.text), "errkey": k, "errargs": a});
    }
    let e = err.text.as_str();
    let top = match err.at {
        Some(YamlAt::Top(n)) => Some(n),
        _ => None,
    };
    let line = top.or_else(|| error_line(e)).or_else(|| port_line(draft, e, vars)).or_else(|| error_service(e).and_then(|s| service_line(draft, &s)));
    let text = error_text(e);
    let text = if line.is_some() { shown_text(&text) } else { text };
    let mut v = json!({"line": line, "text": hider.hide(&text)});
    // Only Kiri's own words have a key, given when Kiri made them.
    if let Some((k, a)) = err.key {
        v["errkey"] = k.into();
        v["errargs"] = a;
    }
    v
}

/// The list's key for a reading's error: an error in another file by its
/// place ([`YamlAt`]), its path hidden as the text is; else the key Kiri
/// gave its own words ([`CfgErr::own`]). Compose's text is never keyed.
fn list_key(at: Option<&YamlAt>, own: Option<(&'static str, Value)>, hider: &Hider) -> Option<(&'static str, Value)> {
    let key = at.and_then(YamlAt::key).map(|(k, mut a)| {
        if let Some(f) = a["file"].as_str().map(|f| hider.hide(f)) {
            a["file"] = f.into();
        }
        (k, a)
    });
    key.or(own)
}

/// Logos, once per event: brand slug → [colour, path]. Services name a slug.
fn logos(slugs: impl IntoIterator<Item = String>) -> Value {
    let mut m = serde_json::Map::new();
    for s in slugs {
        if let Ok(i) = super::brands::BRANDS.binary_search_by_key(&s.as_str(), |(k, _, _)| k) {
            let (_, hex, path) = super::brands::BRANDS[i];
            m.insert(s, json!([hex, path]));
        }
    }
    Value::Object(m)
}

fn tech_of(name: &str, s: &Value, image_id: Option<&str>) -> tech::Tech {
    let build = s["build"]["context"].as_str().map(|c| (PathBuf::from(c), s["build"]["dockerfile"].as_str().unwrap_or("Dockerfile").to_owned()));
    tech::detect(&tech::Service {
        name: name.to_owned(),
        image: s["image"].as_str().unwrap_or_default().to_owned(),
        image_id: image_id.map(str::to_owned),
        build,
        command: argv(&s["command"]),
        ports: s["ports"].as_array().is_some_and(|a| !a.is_empty()),
    })
}

fn argv(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
        Value::String(s) => s.split_whitespace().map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// `{type: "composeprojects", projects: [...], logos}`: each project with its
/// services (from the file when Kiri can read it, else from its containers),
/// what each is made of, and its containers.
pub fn projects_event(cli: Cli, store: Store) -> String {
    let containers = match client::get_background("/containers/json?all=1") {
        Ok(c) => c,
        Err(e) => return json!({"type": "composeprojects", "error": e.to_string(), "projects": []}).to_string(),
    };
    let image_ids: HashMap<String, String> = containers
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| Some((format!("{}/{}", c["Labels"][PROJECT].as_str()?, c["Labels"][SERVICE].as_str()?), c["ImageID"].as_str()?.to_owned())))
        .collect();
    let mut slugs = Vec::new();
    let list: Vec<Value> = all_projects(&containers, store).into_iter().map(|p| project_row(cli, store, p, &containers, &image_ids, &mut slugs)).collect();
    slugs.sort();
    slugs.dedup();
    json!({"type": "composeprojects", "projects": list, "logos": logos(slugs)}).to_string()
}

/// One project's row of the list ([`projects_event`]): `containers` as
/// Docker lists them, each service's image ID by `project/service`; the
/// logos its services show go on `slugs`.
fn project_row(cli: Cli, store: Store, p: Project, containers: &Value, image_ids: &HashMap<String, String>, slugs: &mut Vec<String>) -> Value {
    let text = p.files.first().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
    let (read, own_name) = match checked(p.clone(), store) {
        Ok(c) => {
            let r = saved_reading(cli, &c, &[], true);
            (r.read, r.name.unwrap_or_default())
        }
        Err(e) => (Err(CfgErr::own(e)), name_text_vars(&text)),
    };
    if read.as_ref().is_err_and(|e| e.by_name) {
        return by_name_row(p, &text, containers, image_ids, slugs);
    }
    let at = read.as_ref().err().and_then(|e| e.at.clone());
    let own_key = read.as_ref().err().and_then(|e| e.key.clone());
    let model = read.map_err(|e| error_text(&e));
    // Names and paths as they are, each stand-in Compose read as its
    // variable; what runs shows the reading's image and container
    // name for its service ([`member_view`]). Only the error is free text.
    let hider = hider_from(cli, store, Ok(p.clone())).keeping(model.as_ref().ok().map(model_names).unwrap_or_default()).running(&p.name, containers);
    let model = model.map_err(|e| hider.hide(&e));
    let namewarn = name_note(&both_vars(both_vars(name_vars(model.as_ref().ok(), &text, &hider.stands), own_name.clone()), value_vars(&docker_names(&p.name, containers), &hider.stands, &[model.as_ref().ok().map(model_names).unwrap_or_default(), file_names(&text)].concat())));
    let mut model = model;
    if let Ok(m) = &mut model {
        hider.show_json(m);
    }
    let networks = project_networks(&p.name, model.as_ref().ok(), containers);
    let names: Vec<String> = match &model {
        Ok(m) => start_order(m),
        Err(_) => {
            let mut n: Vec<String> = p.members.iter().map(|m| m.service.clone()).collect();
            n.dedup();
            n
        }
    };
    let services: Vec<Value> = names
        .iter()
        .map(|n| {
            let s = model.as_ref().ok().map(|m| m["services"][n].clone()).filter(|s| !s.is_null()).unwrap_or_else(|| {
                let image = p.members.iter().find(|m| &m.service == n).map(|m| m.image.clone()).unwrap_or_default();
                json!({"image": image})
            });
            let t = tech_of(n, &s, image_ids.get(&format!("{}/{n}", p.name)).map(String::as_str));
            slugs.extend(t.slug.clone());
            let mut v = service_view(n, &s);
            v["tech"] = t.to_json();
            v
        })
        .collect();
    let members: Vec<Value> = p.members.iter().map(member_view).collect();
    let key = list_key(at.as_ref(), own_key, &hider);
    let warn = model.as_ref().ok().and_then(unchecked_note);
    json!({
        "name": p.name, "dir": p.dir, "files": p.files, "services": services, "containers": members, "warn": warn, "namewarn": namewarn,
        "namefolder": folder_note(&own_name, &p.name, &p.dir), "networks": networks, "readable": model.is_ok(), "error": model.err(), "errkey": key.as_ref().map(|k| k.0), "errargs": key.map(|k| k.1),
        "updated": p.members.iter().map(|m| m.created).max(), "fresh": p.members.is_empty(),
    })
}

/// The row of a project Kiri manages by name ([`BY_NAME`]): its name, its
/// file, and its services and state from Docker's containers alone, as
/// for a project whose file is gone. Nothing of the file is read but the
/// variables of its own `name:` (from its text).
fn by_name_row(p: Project, text: &str, containers: &Value, image_ids: &HashMap<String, String>, slugs: &mut Vec<String>) -> Value {
    let mut names: Vec<String> = p.members.iter().map(|m| m.service.clone()).collect();
    names.sort();
    names.dedup();
    let services: Vec<Value> = names
        .iter()
        .map(|n| {
            let image = p.members.iter().find(|m| &m.service == n).map(|m| m.image.clone()).unwrap_or_default();
            let s = json!({"image": image});
            let t = tech_of(n, &s, image_ids.get(&format!("{}/{n}", p.name)).map(String::as_str));
            slugs.extend(t.slug.clone());
            let mut v = service_view(n, &s);
            v["tech"] = t.to_json();
            v
        })
        .collect();
    let members: Vec<Value> = p.members.iter().map(member_view).collect();
    let namewarn = (!p.members.is_empty()).then(|| json!({"text": NAMES_SHOWN, "errkey": "namesShown", "errargs": {}}));
    json!({
        "name": p.name, "dir": p.dir, "files": p.files, "services": services, "containers": members, "warn": by_name_note(), "namewarn": namewarn, "byname": true,
        "namefolder": folder_note(&name_text_vars(text), &p.name, &p.dir), "networks": project_networks(&p.name, None, containers), "readable": false, "error": null, "errkey": null, "errargs": null,
        "updated": p.members.iter().map(|m| m.created).max(), "fresh": p.members.is_empty(),
    })
}

/// A container as the list shows it: its name and image as Docker has
/// them, never altered, as Kiri's Containers section shows them too.
fn member_view(m: &Member) -> Value {
    json!({"service": m.service, "id": m.id, "name": m.name, "image": m.image, "state": m.state, "status": m.status, "created": m.created})
}

/// Whether Discard has a file to put back for the project, whose file at
/// `path` now has `hash`: not once the file is as it was before Kiri's save
/// (put back by hand), which is what runs; that one is forgotten. And
/// whether this reading is the one that found it put back so.
fn still_before(name: &str, path: &str, hash: &str) -> (bool, bool) {
    let mut before = BEFORE.lock().unwrap_or_else(|e| e.into_inner());
    let back = before.get(name).is_some_and(|(f, b, _)| f == path && file_hash(b) == hash);
    if back {
        before.remove(name);
    }
    (before.contains_key(name), back)
}

/// `{type: "composefile", name, path, text, hash, mtime}`: the project's
/// first file, as it is on disk (`hash` is what an Apply of a draft made
/// from it sends back), or `error` when Kiri can't find it.
pub fn file_event(name: &str, store: Store) -> String {
    // The file reads whatever its env files are: Apply is what refuses them.
    let r = find(name, store).map_err(|e| e.to_string()).and_then(|p| checked_files(p, store)).and_then(|p| {
        let path = p.files[0].clone();
        let bytes = std::fs::read(&path).map_err(|_| NO_FILE.to_owned())?;
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64);
        Ok((path, String::from_utf8_lossy(bytes.strip_prefix(BOM).unwrap_or(&bytes)).into_owned(), file_hash(&bytes), mtime))
    });
    let (restore, back) = match &r {
        Ok((path, _, hash, _)) => still_before(name, path, hash),
        Err(_) => (BEFORE.lock().unwrap_or_else(|e| e.into_inner()).contains_key(name), false),
    };
    // `back`: put back by hand as it was before Kiri's save, which is what runs.
    match r {
        Ok((path, text, hash, mtime)) => json!({"type": "composefile", "name": name, "path": path, "text": text.replace("\r\n", "\n"), "hash": hash, "mtime": mtime, "restore": restore, "back": back}).to_string(),
        Err(e) => own_event(json!({"type": "composefile", "name": name}), e),
    }
}

fn plan_json(steps: &[Step]) -> Value {
    Value::Array(steps.iter().map(|s| json!({"n": s.service, "act": s.act, "what": s.what, "from": s.image.0, "to": s.image.1})).collect())
}

/// The file on disk at `path` is `draft`, and every service runs as it says
/// (Compose's config hashes): put right by hand after a failed Apply, say.
/// Nothing is left to apply, so the file Discard kept is forgotten.
fn runs_as_saved(name: &str, path: &str, draft: &str, steps: &[Step]) -> bool {
    let disk = std::fs::read(path).ok().map(|b| String::from_utf8_lossy(b.strip_prefix(BOM).unwrap_or(&b)).replace("\r\n", "\n"));
    let runs = !steps.is_empty() && disk.as_deref() == Some(draft) && steps.iter().all(|s| s.act == "same");
    if runs {
        BEFORE.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
    }
    runs
}

/// `{type: "composeplan", name, seq, steps, model}` for a draft of the file,
/// or `{…, error: {line, text}}` when Compose can't read it.
pub fn plan_event(cli: Cli, store: Store, name: &str, draft: &str, seq: &Value) -> String {
    let (mut hider, mut vars) = (Hider::default(), Vec::new());
    let r = (|| {
        let containers = client::get_background("/containers/json?all=1").map_err(|e| CfgErr::own(e.to_string()))?;
        let p = all_projects(&containers, store).into_iter().find(|p| p.name == name).ok_or_else(|| CfgErr::own(format!("no Compose project {name} on Kiri")))?;
        let p = checked(p, store).map_err(CfgErr::own)?;
        // Managed by name: no plan, and nothing of the draft is read.
        if saved_reading(cli, &p, &[], false).read.is_err_and(|e| e.by_name) {
            return Err(CfgErr::by_name());
        }
        hider = Hider::for_draft(cli, &p, None, Some(draft)).keeping(declared(draft));
        // The names the error may say, as the draft declares them.
        // The error's port is found by the stand-in Compose said back.
        vars = stands_for(cli, &p, Some(draft)).into_iter().map(|s| (s.name, s.value)).collect();
        let now = running(&p, &containers);
        let disk = std::fs::read(&p.files[0]).ok().map(|b| String::from_utf8_lossy(b.strip_prefix(BOM).unwrap_or(&b)).replace("\r\n", "\n"));
        let plain = config(cli, &p, Some(draft), &[])?;
        let profs = profiles(cli, &p, draft, &plain, &now);
        let new = if profs.is_empty() { plain } else { config(cli, &p, Some(draft), &profs)? };
        // A project that has never run: every service is new. After an Apply
        // that saved the file but failed, what runs came from the file before
        // it, so what changed is told against that one.
        let before = BEFORE.lock().unwrap_or_else(|e| e.into_inner()).get(name).filter(|(f, _, _)| *f == p.files[0]).map(|(_, b, _)| String::from_utf8_lossy(b).into_owned());
        let old = if p.members.is_empty() {
            json!({})
        } else if let Some(b) = before {
            config(cli, &p, Some(&b), &profs).unwrap_or_else(|_| json!({}))
        } else {
            saved_config(cli, &p, &profs).unwrap_or_else(|_| json!({}))
        };
        // Against what runs, as Compose itself compares it; the saved file
        // alone when Compose can't say (an older one without --hash).
        let (mut steps, hashed) = match hashes(cli, &p, draft, &profs) {
            Ok(h) if !h.is_empty() => (plan_running(&old, &new, &h, &now), true),
            _ => (plan(&old, &new), false),
        };
        let files: Vec<PathBuf> = p.files.iter().map(PathBuf::from).chain(included(&p).0).collect();
        vars_changed(&mut steps, disk.as_deref() == Some(draft), &files, &p.members);
        let runs = hashed && runs_as_saved(name, &p.files[0], draft, &steps);
        // A running container's name or image may still hold a value the draft drops;
        // a name the saved file or the draft declares (an image's) is no secret's.
        let stands = stands_for(cli, &p, Some(draft));
        let namewarn = name_note(&both_vars(both_vars(name_vars(Some(&new), draft, &stands), own_name_vars(cli, &p, Some(draft)).unwrap_or_else(|| name_text_vars(draft))), value_vars(&docker_names(&p.name, &containers), &stands, &[model_names(&new), model_names(&old), file_names(draft), file_names(&std::fs::read_to_string(&p.files[0]).unwrap_or_default())].concat())));
        Ok::<_, CfgErr>((new, steps, runs, namewarn))
    })();
    match r {
        Ok((new, steps, runs, namewarn)) => {
            // A reading the stand-ins couldn't make is no plan to show: only why.
            let warn = unchecked_note(&new);
            let steps = if warn.is_some() { Vec::new() } else { steps };
            let mut shown = json!([plan_json(&steps), start_order(&new).iter().map(|n| service_view(n, &new["services"][n])).collect::<Vec<_>>()]);
            for st in shown[0].as_array_mut().into_iter().flatten() {
                if new["services"][st["n"].as_str().unwrap_or_default()][HIDDEN] == true {
                    st["hidden"] = true.into();
                    st["to"] = "".into();
                    st["what"] = json!([]);
                }
            }
            hider.show_json(&mut shown);
            json!({"type": "composeplan", "name": name, "seq": seq, "steps": shown[0], "services": shown[1], "runs": runs, "warn": warn, "namewarn": namewarn})
        }
        // Managed by name: no plan, no error, only why.
        Err(e) if e.by_name => json!({"type": "composeplan", "name": name, "seq": seq, "byname": true, "warn": by_name_note()}),
        Err(e) => json!({"type": "composeplan", "name": name, "seq": seq, "error": error_json(&e, draft, &vars, &hider)}),
    }
    .to_string()
}

/// Writes the file in one step (a file beside it, renamed over it), so a
/// watcher or a crash never sees half of it; line endings stay the file's.
/// A link is followed and its target written; the file keeps its mode (and
/// owner, where Kiri may set it), and the file beside it has that mode from
/// the start, so a private file is never readable by others on the way.
/// `base` is the hash the file must still have: checked again just before
/// the rename, under the file's lock, so an outside edit made while Compose
/// read the draft is never written over ([`Changed`]).
fn save(path: &Path, text: &str, base: &str) -> io::Result<()> {
    let path = real_path(path)?;
    let before = std::fs::read(&path)?;
    let crlf = before.windows(2).any(|w| w == b"\r\n");
    let text = text.trim_start_matches('\u{feff}');
    let text = if crlf { text.replace("\r\n", "\n").replace('\n', "\r\n") } else { text.to_owned() };
    // A file that starts with a byte order mark keeps it.
    let bytes = if before.starts_with(BOM) { [BOM, text.as_bytes()].concat() } else { text.into_bytes() };
    write_over(&path, &bytes, base)
}

/// UTF-8's byte order mark, which some editors put at a file's start.
const BOM: &[u8] = b"\xef\xbb\xbf";

/// What Kiri says when the compose file is read-only: it is never written.
pub const READ_ONLY: &str = "The compose file is read-only";

/// The file changed outside Kiri: nothing was written.
#[derive(Debug)]
pub struct Changed;
impl std::fmt::Display for Changed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The file changed outside Kiri")
    }
}
impl std::error::Error for Changed {}

fn changed(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<Changed>())
}

/// One lock per file Kiri writes: a save and a Discard never cross.
type Locks = HashMap<PathBuf, std::sync::Arc<Mutex<()>>>;
static WRITING: LazyLock<Mutex<Locks>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// A test's hook, run after the file beside it is written and before the
/// check: what an outside edit at the worst moment does.
#[cfg(test)]
type Between = Option<(PathBuf, fn(&Path))>;
#[cfg(test)]
static BETWEEN: Mutex<Between> = Mutex::new(None);

/// Whether this user may write the file itself: the system's own answer
/// (owner, group, ACLs; on Windows the read-only attribute and the ACL), by
/// opening it for writing without changing a byte.
fn writable(path: &Path, meta: &std::fs::Metadata) -> bool {
    if meta.permissions().readonly() {
        return false;
    }
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => true,
        Err(e) => e.kind() != io::ErrorKind::PermissionDenied,
    }
}

fn write_over(path: &Path, bytes: &[u8], base: &str) -> io::Result<()> {
    let lock = WRITING.lock().unwrap_or_else(|e| e.into_inner()).entry(path.to_path_buf()).or_default().clone();
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    let unchanged = || -> io::Result<()> {
        #[cfg(test)]
        if let Some((p, f)) = BETWEEN.lock().unwrap_or_else(|e| e.into_inner()).clone()
            && p == path
        {
            f(path);
        }
        if file_hash(&std::fs::read(path)?) == base { Ok(()) } else { Err(io::Error::other(Changed)) }
    };
    let meta = std::fs::metadata(path)?;
    // A rename would replace it all the same: a file this user can't write
    // (read-only, another's, the read-only attribute on Windows) stays as it is.
    if !writable(path, &meta) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, READ_ONLY));
    }
    let tmp = path.with_file_name(format!(".{}.kiri-save", path.file_name().and_then(|n| n.to_str()).unwrap_or("compose.yaml")));
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        o.mode(meta.mode() & 0o7777);
        let r = (|| {
            let mut f = o.open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
            std::fs::set_permissions(&tmp, meta.permissions())
        })();
        if let Err(e) = r {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    #[cfg(not(unix))]
    {
        let r = (|| {
            let mut f = o.open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            std::fs::set_permissions(&tmp, meta.permissions())
        })();
        if let Err(e) = r {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    if let Err(e) = unchanged() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A pull's per-layer line (`f5d2b4da9164 Downloading 87.33MB`): many a
/// second, and nothing a person needs; the service's state says it instead.
fn layer_line(line: &str) -> bool {
    let mut w = line.split_whitespace();
    let id = w.next().unwrap_or_default();
    id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The services a progress line is about and the state it puts them in:
/// `Container shop-db-1 Recreate`, `Container my-db Started` (a
/// `container_name:`), `Image postgres:17 Pulling`, `Image shop-api
/// Building`. `names` names each image's and each named container's services.
fn progress(line: &str, project: &str, names: &Names) -> Option<(Vec<String>, &'static str)> {
    let w: Vec<&str> = line.split_whitespace().collect();
    let (kind, what, word) = (*w.first()?, *w.get(1)?, *w.get(2)?);
    let state = match word {
        "Pulling" => "pulling",
        "Pulled" => "pulled",
        "Building" => "building",
        "Built" => "built",
        "Recreate" => "recreating",
        "Creating" => "creating",
        "Recreated" | "Created" => "created",
        "Starting" => "starting",
        "Started" | "Healthy" => "started",
        // Left as it was: not a step of this Apply.
        "Running" => "running",
        "Stopping" | "Stopped" | "Removing" => "removing",
        "Removed" => "removed",
        _ => return None,
    };
    match kind {
        "Container" => {
            if let Some(svc) = names.containers.get(what) {
                return Some((vec![svc.clone()], state));
            }
            let rest = what.strip_prefix(project)?.strip_prefix(['-', '_'])?;
            let svc = rest.rsplit_once(['-', '_']).map_or(rest, |(s, n)| if n.bytes().all(|b| b.is_ascii_digit()) { s } else { rest });
            // `<project>-<service>-<more>` (a container_name the reading couldn't see): the service it starts with, never the rest.
            let known = || names.images.values().flatten().chain(names.containers.values());
            if !known().any(|k| k == svc)
                && let Some(k) = known().filter(|k| rest.strip_prefix(k.as_str()).is_some_and(|r| r.starts_with(['-', '_']))).max_by_key(|k| k.len())
            {
                return Some((vec![k.clone()], state));
            }
            Some((vec![svc.to_owned()], state))
        }
        "Image" => names.images.get(what).filter(|s| !s.is_empty()).map(|s| (s.clone(), state)),
        _ => None,
    }
}

/// What one stderr line of `up` is for the page: a service's new state
/// (read from the line as Compose wrote it, so a project name that is also
/// a `.env` value still reads), a line to show (secrets taken out), or
/// nothing. The services it made or started go into `touched`.
enum UpLine {
    Skip,
    State(Vec<String>, &'static str),
    Line(String),
}

/// `look` gives a container's service by its Compose labels (its
/// `com.docker.compose.service`, when its project is this one): what a
/// `Container <name>` line is about goes by that first, never by its name.
fn up_line(raw: &str, project: &str, images: &mut Names, look: &mut dyn FnMut(&str) -> Option<String>, hider: &Hider, touched: &mut Vec<String>) -> UpLine {
    if raw.trim().is_empty() || layer_line(raw) {
        return UpLine::Skip;
    }
    let mut w = raw.split_whitespace();
    if let (Some("Container"), Some(name), Some(_)) = (w.next(), w.next(), w.next())
        && let Some(svc) = look(name)
    {
        images.containers.insert(name.to_owned(), svc);
    }
    match progress(raw, project, images) {
        Some((_, "running")) => UpLine::Skip,
        Some((svcs, state)) => {
            if matches!(state, "creating" | "recreating" | "created" | "starting" | "started") {
                for s in &svcs {
                    if !touched.contains(s) {
                        touched.push(s.clone());
                    }
                }
            }
            UpLine::State(svcs, state)
        }
        None => UpLine::Line(hider.hide_up(raw)),
    }
}

/// What `up`'s progress lines name, back to the services: each image with
/// the services that use it, and each `container_name:` with its service.
#[derive(Debug, Default)]
struct Names {
    images: HashMap<String, Vec<String>>,
    containers: HashMap<String, String>,
}

/// The [`Names`] of the model (a built service without an image is
/// `<project>-<service>`).
fn images_of(project: &str, model: &Value) -> Names {
    let mut m = Names::default();
    for (n, s) in model["services"].as_object().into_iter().flatten() {
        let image = s["image"].as_str().map_or_else(|| format!("{project}-{n}"), str::to_owned);
        m.images.entry(image).or_default().push(n.clone());
        if let Some(c) = s["container_name"].as_str().filter(|c| !c.is_empty()) {
            m.containers.insert(c.to_owned(), n.clone());
        }
    }
    m
}

/// What Apply reads its progress by: the [`Names`] of `real`, or, on a
/// reading the stand-ins couldn't make ([`UNCHECKED`]), the services' real
/// names alone, as the last reading's images and container names may be stale.
/// What runs adds its containers' names, by their services: a container
/// `up` removes, named with a secret the file no longer has, is said by
/// its service.
fn apply_names(project: &str, model: &Value, real: &Value, members: &[Member]) -> Names {
    let mut names = if model[UNCHECKED].is_string() {
        let mut m = Names::default();
        for n in model["services"].as_object().into_iter().flatten().map(|(n, _)| n) {
            m.images.entry(format!("{project}-{n}")).or_default().push(n.clone());
        }
        m
    } else {
        images_of(project, real)
    };
    for m in members.iter().filter(|m| !m.name.is_empty() && !m.service.is_empty()) {
        names.containers.entry(m.name.clone()).or_insert_with(|| m.service.clone());
    }
    names
}

/// An Apply under way, per project: its `up` once it runs, and whether
/// Cancel was asked (before `up` too, while Compose reads the draft).
#[derive(Default)]
struct Up {
    child: Option<std::process::Child>,
    cancelled: bool,
}
static UPS: LazyLock<Mutex<HashMap<String, Up>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cancel: stops the project's Apply; what `up` already made stays.
pub fn cancel(name: &str) -> bool {
    let mut ups = UPS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(up) = ups.get_mut(name) else { return false };
    up.cancelled = true;
    if let Some(c) = up.child.as_mut() {
        let _ = c.kill();
    }
    true
}

fn cancelled(name: &str) -> bool {
    UPS.lock().unwrap_or_else(|e| e.into_inner()).get(name).is_some_and(|u| u.cancelled)
}

/// An error as an event carries it: Kiri's own words, keyed for the page
/// in its language ([`error_key`]), or what a process said, as it is and
/// never keyed. Only Kiri makes an `Own`.
#[derive(Debug, PartialEq)]
pub enum Fault {
    Own(String),
    Said(String),
}

impl Fault {
    /// `error`, `errkey` and `errargs` of an event, on `ev`.
    fn put(&self, ev: &mut Value) {
        match self {
            Fault::Own(t) => {
                ev["error"] = t.as_str().into();
                if let Some((k, a)) = error_key(t) {
                    ev["errkey"] = k.into();
                    ev["errargs"] = a;
                }
            }
            Fault::Said(t) => ev["error"] = t.as_str().into(),
        }
    }
}

/// An event with Kiri's own error `e` on it, keyed ([`Fault::Own`]).
fn own_event(mut ev: Value, e: impl Into<String>) -> String {
    Fault::Own(e.into()).put(&mut ev);
    ev.to_string()
}

/// Takes the project's Apply off the list however `apply` ends.
struct Applying(String);
impl Drop for Applying {
    fn drop(&mut self) {
        if let Some(mut c) = UPS.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0).and_then(|u| u.child) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Discard after a failed Apply: puts back the file as it was before Kiri
/// saved over it, unless it has changed since (then it says so).
pub fn restore(name: &str, store: Store) -> Result<(), String> {
    let p = find_checked(name, store)?;
    let (file, bytes, wrote) = BEFORE.lock().unwrap_or_else(|e| e.into_inner()).get(name).cloned().ok_or("Kiri has nothing to put back")?;
    if file != p.files[0] {
        return Err(NO_FILE.into());
    }
    write_over(Path::new(&file), &bytes, &wrote).map_err(|e| if changed(&e) { "The file changed outside Kiri since Apply saved it".to_owned() } else { e.to_string() })?;
    BEFORE.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
    Ok(())
}

/// Apply: checks the draft, saves it, and runs `docker compose up -d
/// --remove-orphans`, sending its progress lines as
/// `{type: "composeapply", name, line}` and then `{…, done: true, ok, error,
/// hash}` (`hash`: the file's as Kiri left it, for the next Apply's base).
/// `base` is the hash of the file the draft was made from: when the file on
/// disk is no longer that (or the page sends none), nothing is saved and the
/// answer has `conflict` and the file's hash now. When `up` fails the file
/// stays saved (`saved: true`), so Apply again retries once the cause (a
/// Dockerfile, a port) is fixed. Cancel, before `up` or during it, answers
/// `cancelled` with the services `up` had already changed.
pub fn apply(cli: Cli, store: Store, name: &str, draft: &str, base: Option<&str>, emit: fn(String)) {
    let emit = move |s: String| emit(untoken_event(&s));
    {
        // One Apply per project at a time: a second one is refused, the first goes on.
        let mut ups = UPS.lock().unwrap_or_else(|e| e.into_inner());
        if ups.contains_key(name) {
            drop(ups);
            return emit(own_event(json!({"type": "composeapply", "name": name, "done": true, "ok": false, "busy": true}), format!("{name} is already being applied")));
        }
        ups.insert(name.to_owned(), Up::default());
    }
    let _applying = Applying(name.to_owned());
    let mut hidden = Hider::default();
    // Kiri's own words keyed, a process's hidden and never keyed.
    let done = |ok: bool, error: Option<Fault>, saved: bool, hidden: &Hider, hash: Option<&str>| {
        let mut ev = json!({"type": "composeapply", "name": name, "done": true, "ok": ok, "error": null, "saved": saved, "hash": hash});
        match error {
            Some(Fault::Said(e)) => shown_fault(hidden, &e).put(&mut ev),
            Some(Fault::Own(e)) => Fault::Own(hidden.hide(&e)).put(&mut ev),
            None => {}
        }
        emit(ev.to_string())
    };
    let stopped = |saved: bool, changed: &[String], hash: Option<&str>| emit(own_event(json!({"type": "composeapply", "name": name, "done": true, "ok": false, "cancelled": true, "changed": changed, "saved": saved, "hash": hash}), "Apply cancelled"));
    let conflict = |now: &str| emit(own_event(json!({"type": "composeapply", "name": name, "done": true, "ok": false, "conflict": true, "hash": now}), "The file changed outside Kiri"));
    let own = |e: &str| Some(Fault::Own(e.to_owned()));
    let p = match find_checked(name, store) {
        Ok(p) => p,
        Err(e) => return done(false, own(&e), false, &hidden, None),
    };
    // Managed by name ([`BY_NAME`]): Apply is the terminal's, never Kiri's.
    if saved_reading(cli, &p, &[], false).read.is_err_and(|e| e.by_name) {
        return done(false, own(BY_NAME), false, &hidden, None);
    }
    hidden = Hider::for_project(cli, &p, None);
    let path = p.files[0].clone();
    let disk = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return done(false, own(NO_FILE), false, &hidden, None),
    };
    let disk_hash = file_hash(&disk);
    // No base: the page hasn't read the file it edits; it must, first.
    let Some(base) = base.filter(|b| *b == disk_hash) else { return conflict(&disk_hash) };
    let containers = client::get_background("/containers/json?all=1").unwrap_or_default();
    let now = running(&p, &containers);
    hidden = Hider::for_draft(cli, &p, None, Some(draft)).keeping(declared(draft));
    let (profs, images) = match config(cli, &p, Some(draft), &[]) {
        Ok(m) => {
            let profs = profiles(cli, &p, draft, &m, &now);
            let m = if profs.is_empty() { m } else { config(cli, &p, Some(draft), &profs).unwrap_or(m) };
            hidden = std::mem::take(&mut hidden).keeping(model_names(&m)).running(&p.name, &containers);
            // `up` names images as they really are.
            let stands = stands_for(cli, &p, Some(draft));
            let real = real(&m, &stands);
            if let Some(label) = bad_image(&m, &real, &stands) {
                return done(false, own(&format!("the image name from {label} isn't valid")), false, &hidden, Some(&disk_hash));
            }
            (profs, apply_names(&p.name, &m, &real, &p.members))
        }
        Err(e) => {
            // An error in another file keeps its key: no line of this one is named.
            if e.at.as_ref().and_then(YamlAt::key).is_some() {
                let ej = error_json(&e, draft, &[], &hidden);
                return emit(json!({"type": "composeapply", "name": name, "done": true, "ok": false, "error": ej["text"], "errkey": ej["errkey"], "errargs": ej["errargs"], "saved": false, "hash": disk_hash}).to_string());
            }
            // Kiri's own words keep the key Kiri gave them; Compose's go as said.
            let f = if e.key.is_some() { Fault::Own(e.text.clone()) } else { Fault::Said(error_text(&e)) };
            return done(false, Some(f), false, &hidden, Some(&disk_hash));
        }
    };
    if cancelled(name) {
        return stopped(false, &[], Some(&disk_hash));
    }
    let mut hash = disk_hash.clone();
    if String::from_utf8_lossy(disk.strip_prefix(BOM).unwrap_or(&disk)).replace("\r\n", "\n") != draft.trim_start_matches('\u{feff}').replace("\r\n", "\n") {
        match save(Path::new(&path), draft, base) {
            Err(e) if changed(&e) => return conflict(&std::fs::read(&path).map(|b| file_hash(&b)).unwrap_or_default()),
            Err(e) if e.to_string() == READ_ONLY => return done(false, own(READ_ONLY), false, &hidden, Some(&disk_hash)),
            Err(e) => return done(false, own(&format!("couldn't save the compose file: {e}")), false, &hidden, Some(&disk_hash)),
            Ok(()) => {}
        }
        hash = std::fs::read(&path).map(|b| file_hash(&b)).unwrap_or_default();
        remember_before(name, &path, disk, &hash);
    }
    let mut cmd = match cli() {
        Ok(c) => c,
        Err(e) => return done(false, own(&e.to_string()), true, &hidden, Some(&hash)),
    };
    cmd.args(project_args(&p, None)).args(profile_args(&profs)).args(["--progress", "plain", "up", "-d", "--remove-orphans"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return done(false, own(&format!("couldn't run docker compose: {e}")), true, &hidden, Some(&hash)),
    };
    let out = child.stdout.take();
    let err = child.stderr.take();
    {
        let mut ups = UPS.lock().unwrap_or_else(|e| e.into_inner());
        let up = ups.entry(name.to_owned()).or_default();
        // Cancel came while the file was being saved: `up` stops at once.
        if up.cancelled {
            let _ = child.kill();
        }
        up.child = Some(child);
    }
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut o) = out {
            let _ = io::Read::read_to_string(&mut o, &mut s);
        }
        s
    });
    let mut last = String::new();
    // Compose's plain progress says some lines twice: each goes once.
    let mut said: std::collections::HashSet<String> = std::collections::HashSet::new();
    // The services this Apply made or started: they must stay up after it.
    let mut touched: Vec<String> = Vec::new();
    // Each container's service by its labels, once Docker has it.
    let mut known: HashMap<String, String> = HashMap::new();
    let mut look = |n: &str| -> Option<String> {
        if let Some(s) = known.get(n) {
            return Some(s.clone());
        }
        let c = client::get_background(&format!("/containers/{}/json", super::needs::esc(n))).ok()?;
        let labels = &c["Config"]["Labels"];
        let svc = labels[SERVICE].as_str().filter(|s| labels[PROJECT] == p.name.as_str() && !s.is_empty())?.to_owned();
        known.insert(n.to_owned(), svc.clone());
        Some(svc)
    };
    let mut images = images;
    if let Some(err) = err {
        for raw in io::BufReader::new(err).lines().map_while(Result::ok) {
            match up_line(&raw, &p.name, &mut images, &mut look, &hidden, &mut touched) {
                UpLine::Skip => continue,
                UpLine::State(svcs, state) => emit(json!({"type": "composeapply", "name": name, "svc": svcs, "state": state}).to_string()),
                UpLine::Line(line) => {
                    if said.insert(line.split_whitespace().collect::<Vec<_>>().join(" ")) {
                        emit(json!({"type": "composeapply", "name": name, "line": line}).to_string());
                    }
                }
            }
            last = raw;
        }
    }
    let _ = reader.join();
    // Out of the list before the wait, so Cancel never waits on it.
    let child = UPS.lock().unwrap_or_else(|e| e.into_inner()).get_mut(name).and_then(|u| u.child.take());
    let status = child.map(|mut c| c.wait());
    let was_cancelled = cancelled(name);
    // What `up` read may be newer than the list's reading: read again at once.
    forget_readings(name);
    match status {
        Some(Ok(s)) if s.success() => {
            BEFORE.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
            // Up -d returns once each container started; one that then dies
            // (postgres 17 on 16's data) must not read as plain success.
            let failed = Value::Array(watch(&p.name, &touched, std::time::Duration::from_secs(5)));
            emit(json!({"type": "composeapply", "name": name, "done": true, "ok": true, "failed": failed, "hash": hash}).to_string());
        }
        _ if was_cancelled => stopped(true, &touched, Some(&hash)),
        Some(Ok(_)) => done(false, Some(if last.trim().is_empty() { Fault::Own("docker compose up failed".into()) } else { Fault::Said(last.trim().to_owned()) }), true, &hidden, Some(&hash)),
        Some(Err(e)) => done(false, Some(Fault::Said(e.to_string())), true, &hidden, Some(&hash)),
        None => done(false, own("docker compose up stopped"), true, &hidden, Some(&hash)),
    }
}

/// The variable of a service's image with a stand-in in it whose real
/// image isn't a valid reference: `up` would fail on it and Docker quote
/// a piece of the secret back.
fn bad_image(model: &Value, real: &Value, stands: &[Stand]) -> Option<String> {
    for (n, s) in model["services"].as_object().into_iter().flatten() {
        let Some(image) = s["image"].as_str().map(str::to_ascii_lowercase) else { continue };
        if let Some(st) = stands.iter().find(|st| image.contains(&st.token))
            && !image_reference(real["services"][n]["image"].as_str().unwrap_or_default())
        {
            return Some(st.label.clone());
        }
    }
    None
}

/// Whether `r` is an image reference Docker takes: `[host[:port]/]path[:tag][@digest]`,
/// the path's parts lower-case letters and digits joined by `.`, `_`, `__` or dashes.
pub fn image_reference(r: &str) -> bool {
    let (name, digest) = r.split_once('@').map_or((r, None), |(n, d)| (n, Some(d)));
    if let Some(d) = digest {
        let Some((algo, hex)) = d.split_once(':') else { return false };
        if algo.is_empty() || !algo.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_+.".contains(&b)) || hex.len() < 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    let (name, tag) = match name.rsplit_once(':') {
        Some((n, t)) if !t.contains('/') => (n, Some(t)),
        _ => (name, None),
    };
    if let Some(t) = tag {
        let mut b = t.bytes();
        if t.len() > 128 || !b.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') || !b.all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c)) {
            return false;
        }
    }
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    let mut parts: Vec<&str> = name.split('/').collect();
    if parts.len() > 1 && (parts[0].contains(['.', ':']) || parts[0] == "localhost" || parts[0].bytes().any(|b| b.is_ascii_uppercase())) {
        let host = parts.remove(0);
        let (h, port) = host.split_once(':').map_or((host, None), |(h, p)| (h, Some(p)));
        let label = |l: &str| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-') && !l.ends_with('-');
        if !h.split('.').all(label) || port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
            return false;
        }
    }
    let part = |p: &str| {
        let b = p.as_bytes();
        let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
        if b.is_empty() || !alnum(b[0]) || !alnum(b[b.len() - 1]) || !b.iter().all(|&c| alnum(c) || b"._-".contains(&c)) {
            return false;
        }
        // A separator: one `.`, one or two `_`, or dashes.
        p.split(|c: char| c.is_ascii_alphanumeric()).filter(|s| !s.is_empty()).all(|s| s == "." || s == "_" || s == "__" || s.bytes().all(|c| c == b'-'))
    };
    parts.iter().all(|p| part(p))
}

/// Apply's error as the toast and the note say it: hidden ([`Hider`]), and
/// when nothing but a secret was left, what kind of failure it was.
#[cfg(test)]
fn shown_error(hidden: &Hider, e: &str) -> String {
    match shown_fault(hidden, e) {
        Fault::Own(t) | Fault::Said(t) => t,
    }
}

/// [`shown_error`], as whose words it is: Kiri's when nothing but a secret was left.
fn shown_fault(hidden: &Hider, e: &str) -> Fault {
    let h = hidden.hide_up(e);
    let words = h.split("${").map(|p| p.split_once('}').map_or(p, |(_, r)| r)).collect::<String>().split(|c: char| !c.is_alphabetic()).any(|w| w.chars().count() >= 3);
    if words { Fault::Said(h) } else { Fault::Own("docker compose up failed".into()) }
}

/// The services among `touched` whose newest container stopped with an
/// error or keeps restarting: `[{svc, id, status}]`. Exited (0) is a job
/// that finished, not a failure.
pub fn failed(project: &str, touched: &[String], containers: &Value) -> Vec<Value> {
    let p = projects(containers).into_iter().find(|p| p.name == project);
    let mut out = Vec::new();
    for svc in touched {
        let Some(m) = p.as_ref().and_then(|p| p.members.iter().filter(|m| &m.service == svc).max_by_key(|m| m.created)) else { continue };
        let code = m.status.strip_prefix("Exited (").and_then(|r| r.split(')').next()).and_then(|c| c.parse::<i64>().ok());
        let bad = m.state == "restarting" || m.state == "dead" || (m.state == "exited" && code != Some(0));
        if bad {
            out.push(json!({"svc": svc, "id": m.id, "status": m.status}));
        }
    }
    out
}

/// Looks at what Apply made or started for `within`, once a second: every
/// service that failed in that time, in the order they did (one that dies
/// a moment after another is named too).
fn watch(project: &str, touched: &[String], within: std::time::Duration) -> Vec<Value> {
    watch_with(|| client::get_background("/containers/json?all=1").ok(), project, touched, within, std::time::Duration::from_secs(1))
}

fn watch_with(mut list: impl FnMut() -> Option<Value>, project: &str, touched: &[String], within: std::time::Duration, every: std::time::Duration) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    if touched.is_empty() {
        return out;
    }
    let until = std::time::Instant::now() + within;
    loop {
        std::thread::sleep(every);
        for f in list().map(|c| failed(project, touched, &c)).unwrap_or_default() {
            match out.iter_mut().find(|o| o["svc"] == f["svc"]) {
                Some(o) => *o = f,
                None => out.push(f),
            }
        }
        if std::time::Instant::now() >= until {
            return out;
        }
    }
}

/// `{type: "composelogs", name, lines}`: the project's last lines, each
/// service's interleaved by time, as `docker compose logs` gives them.
pub fn logs_event(cli: Cli, store: Store, name: &str) -> String {
    let r = find(name, store).map_err(|e| Fault::Own(e.to_string())).and_then(|p| {
        // The file gone: the project's name alone, as `docker compose -p` reads it from its containers.
        let p = checked(p.clone(), store).unwrap_or(Project { dir: String::new(), files: Vec::new(), env_files: Vec::new(), ..p });
        let hider = hider_of(cli, store, name);
        // By the project's name, from its containers: the file is never
        // read, so one Kiri can only check with its secrets hidden still
        // gives its logs.
        let out = logs_out(cli, &p.name).map_err(|e| Fault::Said(hider.hide(&e)))?;
        let mut lines = log_lines(&String::from_utf8_lossy(&out));
        engine_lines(&mut lines, &hider);
        label_by_service(&mut lines, &p);
        Ok(lines)
    });
    let ev = match r {
        Ok(lines) => json!({"type": "composelogs", "name": name, "lines": lines}),
        Err(e) => {
            let mut ev = json!({"type": "composelogs", "name": name});
            e.put(&mut ev);
            ev
        }
    }
    .to_string();
    untoken_event(&ev)
}

/// What `docker compose logs` ([`logs_args`]) says, run in a folder of its own ([`EmptyDir`]).
fn logs_out(cli: Cli, name: &str) -> Result<Vec<u8>, String> {
    let mut cmd = cli().map_err(|e| e.to_string())?;
    let empty = EmptyDir::new().map_err(|e| e.to_string())?;
    cmd.args(logs_args(name)).current_dir(&empty.0);
    run_cli(cmd, None, 30)
}

/// `docker compose logs` of the project by its name alone: no file, folder
/// or env file, so Compose reads none and asks the engine for its containers.
pub fn logs_args(name: &str) -> Vec<String> {
    let mut a = project_args(&Project { name: name.to_owned(), ..Project::default() }, None);
    a.extend(["logs", "--no-color", "--timestamps", "--tail", "200"].map(str::to_owned));
    a
}

/// The engine's own lines (`kiri: cannot start container: …`), not the
/// container's, hidden as free text ([`Hider::hide_up`]): with the lines
/// that go on from one in the same second (an older engine wrote a value
/// over several lines as they are), each line of a secret over several too.
pub fn engine_lines(lines: &mut [Value], hider: &Hider) {
    let mut on: HashMap<String, String> = HashMap::new();
    for l in lines {
        let svc = l["svc"].as_str().unwrap_or_default().to_owned();
        let sec: String = l["t"].as_str().unwrap_or_default().chars().take(19).collect();
        let Some(t) = l["text"].as_str().map(str::to_owned) else { continue };
        if t.trim_start().starts_with("kiri: ") {
            on.insert(svc, sec);
        } else if on.get(&svc).is_none_or(|s| *s != sec) {
            // An older engine's key went on over lines it may stamp a second
            // later: each of its lines is hidden on every line.
            on.remove(&svc);
            l["text"] = hider.hide_lines(&t).into();
            continue;
        }
        l["text"] = hider.hide_up(&t).into();
    }
}

/// `docker compose logs` labels a line by its container (`web-1`, or a
/// `container_name:` as it is): each is labelled by its service instead.
pub fn label_by_service(lines: &mut [Value], p: &Project) {
    for l in lines {
        let c = l["svc"].as_str().unwrap_or_default().to_owned();
        let short = |m: &Member| m.name.strip_prefix(&p.name).and_then(|r| r.strip_prefix(['-', '_'])).map(str::to_owned);
        if let Some(m) = p.members.iter().find(|m| m.name == c || short(m).as_deref() == Some(c.as_str())) {
            l["svc"] = m.service.clone().into();
        }
    }
}

/// `docker compose logs` gives each service's lines in turn: these are
/// in time order across them, without empty ones.
pub fn log_lines(text: &str) -> Vec<Value> {
    let mut v: Vec<Value> = text.lines().map(log_line).filter(|l| !l["text"].as_str().unwrap_or_default().trim().is_empty()).collect();
    v.sort_by(|a, b| a["t"].as_str().unwrap_or_default().cmp(b["t"].as_str().unwrap_or_default()));
    v
}

/// The CLI's arguments for Start all, Stop all, Restart all and Remove
/// project: the project by name alone, as Compose reads it from its
/// containers' labels (each service's, profiles included, and in
/// depends_on order), so they work when the file is gone too. Remove is
/// `down`: the containers and the project's network go, volumes stay.
pub fn action_args(name: &str, action: &str) -> Option<Vec<String>> {
    let verb = match action {
        "start" | "stop" | "restart" => action,
        "remove" => "down",
        _ => return None,
    };
    let mut a = vec!["compose".to_owned(), "--ansi".into(), "never".into(), "-p".into(), name.to_owned(), verb.into()];
    if verb == "down" {
        a.push("--remove-orphans".into());
    }
    Some(a)
}

/// The hider for a project's errors when its file may be gone: the
/// folder's (only a local one, [`valid_dir`]) and the environment's values.
fn hider_of(cli: Cli, store: Store, name: &str) -> Hider {
    hider_from(cli, store, find(name, store)).running(name, &client::get_background("/containers/json?all=1").unwrap_or_default())
}

fn hider_from(cli: Cli, store: Store, p: io::Result<Project>) -> Hider {
    match p {
        Ok(p) => match checked(p.clone(), store) {
            Ok(c) => Hider::for_project(cli, &c, None),
            Err(_) => {
                let dir = valid_dir(&p.dir, false).unwrap_or_default();
                let env_files = p.env_files.iter().filter_map(|f| valid_env_file(f)).collect();
                Hider::for_project(cli, &Project { dir, files: Vec::new(), env_files, ..p }, None)
            }
        },
        Err(_) => Hider::for_project(cli, &Project::default(), None),
    }
}

/// A project action's error without the progress lines before it
/// (`Container shop-web-1  Starting`): the error alone.
pub fn action_error(e: &str) -> String {
    let progress = |l: &str| {
        let w: Vec<&str> = l.split_whitespace().collect();
        (2..=3).contains(&w.len()) && ["Container", "Network", "Volume", "Image"].contains(&w[0])
    };
    let kept: Vec<&str> = e.lines().filter(|l| !l.trim().is_empty() && !progress(l)).map(str::trim).collect();
    if kept.is_empty() { e.trim().to_owned() } else { kept.join("\n") }
}

/// Kiri's own words keyed ([`Fault::Own`]); what Compose said, as it is.
fn action(cli: Cli, name: &str, what: &str) -> Result<(), Fault> {
    let own = |e: String| Fault::Own(e);
    let args = action_args(name, what).ok_or_else(|| own("no such action".into()))?;
    let mut cmd = cli().map_err(|e| own(e.to_string()))?;
    // Away from any compose file or `.env` a shared folder may have.
    let empty = EmptyDir::new().map_err(|e| own(e.to_string()))?;
    cmd.args(args).current_dir(&empty.0);
    match run_ran(cmd, None, 180) {
        Ran::Passed(_) => Ok(()),
        Ran::Failed(e) => Err(Fault::Said(e)),
        t @ Ran::TimedOut(_) => Err(own(t.result().err().unwrap_or_default())),
    }
}

/// A fresh, private (mode 700) and empty folder to run a command in that
/// names no folder of its own (`-p` alone, or `-f -`): Compose reads the
/// `.env` of the folder it runs in, and none is ever there. Made per call,
/// never one that was there before; removed when dropped.
struct EmptyDir(PathBuf);

impl EmptyDir {
    fn new() -> io::Result<Self> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        for _ in 0..16 {
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let d = std::env::temp_dir().join(format!("kiri-run-{}-{n}-{nanos}", std::process::id()));
            #[cfg(unix)]
            let made = std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700).create(&d);
            #[cfg(not(unix))]
            let made = std::fs::create_dir(&d);
            match made {
                Ok(()) => return Ok(Self(d)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("couldn't make a private folder to run docker compose in"))
    }
}

impl Drop for EmptyDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// `web-1  | 2026-11-16T13:02:01.5Z listening` → {svc, t, text}.
fn log_line(l: &str) -> Value {
    let (svc, rest) = l.split_once(" | ").or_else(|| l.split_once("| ")).map_or(("", l), |(a, b)| (a.trim(), b));
    let (t, text) = match rest.split_once(' ') {
        Some((t, x)) if t.len() >= 19 && t.as_bytes().get(4) == Some(&b'-') && t.contains('T') => (t, x),
        _ => ("", rest),
    };
    json!({"svc": svc, "t": t, "text": text})
}

/// The page's `compose*` commands. Each runs off the page's thread.
pub fn handle(cmd: &str, name: String, value: &Value, cli: Cli, store: Store, emit: fn(String)) {
    let value = value.clone();
    let cmd = cmd.to_owned();
    // No stand-in token reaches the window, whatever said it back.
    let send = move |s: String| emit(untoken_event(&s));
    // The file's own text goes as it is: what the person wrote is never changed.
    let emit_file = move |s: String| emit(s);
    std::thread::spawn(move || match cmd.as_str() {
        // The page's own ask comes back with it: a list no one asked for (after an action) is told apart.
        "composeprojects" => send(asked(projects_event(cli, store), &value["ask"])),
        "composefile" => emit_file(file_event(&name, store)),
        "composeplan" => send(plan_event(cli, store, &name, value["text"].as_str().unwrap_or_default(), &value["seq"])),
        "composeapply" => apply(cli, store, &name, value["text"].as_str().unwrap_or_default(), value["base"].as_str(), emit),
        "composeaction" => {
            let what = value["action"].as_str().unwrap_or_default();
            match action(cli, &name, what) {
                Err(e) => {
                    let mut ev = json!({"type": "composeaction", "name": name});
                    match e {
                        Fault::Said(e) => Fault::Said(hider_of(cli, store, &name).hide(&action_error(&e))),
                        own => own,
                    }
                    .put(&mut ev);
                    send(ev.to_string());
                }
                // Removed: its containers and network are gone, and so is its
                // place in the list, also when it came from a file opened
                // here. The compose file stays where it is, and opening it
                // again lists the project again.
                Ok(()) if what == "remove" => {
                    if let Err(e) = save_opened(store, |n, _| n != name, None) {
                        send(own_event(json!({"type": "composeopened"}), e.to_string()));
                    }
                    send(json!({"type": "composeaction", "name": name, "removed": true}).to_string())
                }
                // Done: the page's button stops rolling.
                Ok(()) => send(json!({"type": "composeaction", "name": name, "done": true}).to_string()),
            }
            send(projects_event(cli, store));
        }
        "composecancel" => {
            cancel(&name);
        }
        // Discard after a failed Apply: the file as it was before it.
        "composerestore" => {
            if let Err(e) = restore(&name, store) {
                send(own_event(json!({"type": "composerestored", "name": name}), e));
            } else {
                send(json!({"type": "composerestored", "name": name}).to_string());
            }
            emit_file(file_event(&name, store));
        }
        "composelogs" => send(logs_event(cli, store, &name)),
        // Takes an opened file's project off the list (its file stays).
        "composeforget" => {
            if let Err(e) = save_opened(store, |n, _| n != name, None) {
                send(own_event(json!({"type": "composeopened"}), e.to_string()));
            }
            send(projects_event(cli, store));
        }
        _ => {}
    });
}

/// A list with the page's `ask` it answers.
fn asked(ev: String, ask: &Value) -> String {
    match serde_json::from_str::<Value>(&ev) {
        Ok(mut v) if !ask.is_null() => {
            v["ask"] = ask.clone();
            v.to_string()
        }
        _ => ev,
    }
}

/// After the system's file dialog: `path` is what the person picked, if
/// anything. Off the window's thread, since Compose reads the file.
pub fn picked(path: Option<PathBuf>, cli: Cli, store: Store, emit: fn(String)) {
    let Some(path) = path else { return };
    std::thread::spawn(move || {
        emit(untoken_event(&open_file(cli, store, &path)));
        emit(untoken_event(&projects_event(cli, store)));
    });
}

/// The whole flow on a running engine, by hand: `KIRI_SOCKET=<engine's
/// socket> KIRI_LIVE_DIR=<a compose project with web, api, db, cache>
/// cargo test -p kiri live_compose -- --ignored --nocapture`. It brings the
/// project up, reads it as the window does, plans and applies an edit
/// (only the changed services are recreated), and writes each event to
/// `$KIRI_LIVE_OUT` (if set) for a look at the page. On a Unix socket, so
/// not on Windows.
#[cfg(all(test, unix))]
mod live {
    use super::*;

    fn cli() -> io::Result<Command> {
        let mut c = Command::new(client::docker_cli().ok_or_else(|| io::Error::other("no docker CLI"))?);
        c.env("DOCKER_HOST", format!("unix://{}", client::socket()?.display())).env_remove("DOCKER_CONTEXT");
        Ok(c)
    }

    /// The opened files' list: KIRI_LIVE_OUT/compose.json, else none.
    fn store() -> Option<PathBuf> {
        std::env::var_os("KIRI_LIVE_OUT").map(|d| Path::new(&d).join("compose.json"))
    }

    fn keep(name: &str, ev: &str) {
        if let Some(d) = std::env::var_os("KIRI_LIVE_OUT") {
            let _ = std::fs::create_dir_all(&d);
            let _ = std::fs::write(Path::new(&d).join(format!("{name}.json")), ev);
        }
    }

    /// Docker commands run when the test ends, failed or not (and once at its
    /// start, for what a run before it left): a second run on the same
    /// engine finds nothing of the first. A file to put back as it was, too.
    struct Cleanup {
        cmds: Vec<Vec<String>>,
        restore: Option<(PathBuf, String)>,
    }

    impl Cleanup {
        /// Run only when the test ends: for a test whose names are its run's own.
        fn at_end<S: Into<String>>(cmds: Vec<Vec<S>>, restore: Option<(PathBuf, String)>) -> Self {
            Cleanup { cmds: cmds.into_iter().map(|c| c.into_iter().map(Into::into).collect()).collect(), restore }
        }

        fn new<S: Into<String>>(cmds: Vec<Vec<S>>, restore: Option<(PathBuf, String)>) -> Self {
            let cmds = cmds.into_iter().map(|c| c.into_iter().map(Into::into).collect()).collect();
            let c = Cleanup { cmds, restore };
            c.run();
            c
        }

        fn run(&self) {
            for a in &self.cmds {
                let _ = cli().map(|mut c| c.args(a).stdout(Stdio::null()).stderr(Stdio::null()).status());
            }
        }
    }

    /// A test's own folder, removed when it goes (a failed test's too).
    struct RmDir(PathBuf);

    impl Drop for RmDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Whether `t` holds one of `pieces` of a secret: container and image
    /// ids (12 hex digits or more) taken out first, as no secret is one; a
    /// piece under 6 characters (a short secret whole) only as a word.
    fn leak_in<'a>(t: &str, pieces: &[&'a str]) -> Option<&'a str> {
        let (mut s, mut run) = (String::new(), String::new());
        for c in t.chars().chain(['\n']) {
            if c.is_ascii_hexdigit() {
                run.push(c);
                continue;
            }
            if run.len() < 12 {
                s.push_str(&run);
            }
            run.clear();
            s.push(c);
        }
        let word = |s: &str, p: &str| s.match_indices(p).any(|(i, _)| !s[..i].chars().next_back().is_some_and(char::is_alphanumeric) && !s[i + p.len()..].chars().next().is_some_and(char::is_alphanumeric));
        pieces.iter().copied().find(|p| if p.chars().count() >= 6 { s.contains(p) } else { word(&s, p) })
    }

    #[test]
    fn a_leak_check_skips_ids_and_short_pieces_inside_words() {
        assert_eq!(leak_in("container 3f5ee5a91b2c4d5e6f70 started", &["5ee5"]), None);
        assert_eq!(leak_in("pw x-77ff here", &["x-77ff"]), Some("x-77ff"));
        assert_eq!(leak_in("redis r3dis", &["r3dis"]), Some("r3dis"));
        assert_eq!(leak_in("pr3disk", &["r3dis"]), None);
    }

    /// Volumes a test made (unnamed ones too, which `up --remove-orphans`
    /// leaves): removed by name when its [`Cleanup`] goes.
    static MADE: Mutex<Vec<String>> = Mutex::new(Vec::new());

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some((f, text)) = &self.restore {
                let _ = std::fs::write(f, text);
            }
            self.run();
            for v in std::mem::take(&mut *MADE.lock().unwrap_or_else(|e| e.into_inner())) {
                let _ = client::request("DELETE", &format!("/volumes/{v}"), None);
            }
        }
    }

    fn ids(project: &str) -> BTreeMap<String, String> {
        let p = find(project, store).unwrap();
        p.members.iter().map(|m| (m.service.clone(), m.id.clone())).collect()
    }

    #[test]
    #[ignore = "needs a running engine and KIRI_LIVE_DIR"]
    fn live_compose() {
        let dir = PathBuf::from(std::env::var("KIRI_LIVE_DIR").expect("KIRI_LIVE_DIR"));
        let file = dir.join("compose.yaml");
        // This run's own project: another run's containers are never this one's to remove.
        let name = format!("kirilive{}x{}", std::process::id(), SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().subsec_micros());
        let name = name.as_str();
        // The fixture as it was, put back however the test ends; the project's
        // containers, volumes and network removed, and only its own.
        // The images it builds too (`<project>-api`, `-web`): a run leaves no tag behind.
        let built = [format!("{name}-api:latest"), format!("{name}-web:latest")];
        let _clean = Cleanup::new(
            vec![vec!["compose", "-p", name, "-f", &file.to_string_lossy(), "down", "-v", "--remove-orphans"], vec!["rmi", "-f", &built[0], &built[1]]],
            Some((file.clone(), std::fs::read_to_string(&file).unwrap())),
        );
        let up = cli().unwrap().args(["compose", "-p", name, "-f"]).arg(&file).args(["up", "-d", "--build", "--remove-orphans"]).status().unwrap();
        assert!(up.success(), "compose up");
        // Every volume this run's containers use (named and unnamed), at each
        // step: removed at the end by name.
        let mut made: BTreeSet<String> = BTreeSet::new();
        let collect = |made: &mut BTreeSet<String>| {
            for m in find(name, store).map(|p| p.members).unwrap_or_default() {
                let c = client::get(&format!("/containers/{}/json", m.id)).unwrap_or_default();
                made.extend(c["Mounts"].as_array().into_iter().flatten().filter(|m| m["Type"] == "volume").filter_map(|m| m["Name"].as_str().map(str::to_owned)));
            }
        };
        collect(&mut made);

        let ev: Value = serde_json::from_str(&projects_event(cli, store)).unwrap();
        keep("projects", &ev.to_string());
        let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == name).expect("the project is listed").clone();
        let svc = |n: &str| p["services"].as_array().unwrap().iter().find(|s| s["name"] == n).cloned().unwrap_or_default();
        assert_eq!(p["readable"], true, "{p}");
        assert_eq!(start_order(&saved_config(cli, &checked(find(name, store).unwrap(), store).unwrap(), &[]).unwrap()), ["cache", "db", "api", "web"]);
        println!("web tech {}  api tech {}  db tech {}", svc("web")["tech"], svc("api")["tech"], svc("db")["tech"]);
        assert_eq!(svc("web")["tech"]["lang"], "Node.js");
        assert_eq!(svc("api")["tech"]["lang"], "Go");
        assert!(!ev.to_string().contains("secret"), "no environment values");

        let f: Value = serde_json::from_str(&file_event(name, store)).unwrap();
        keep("file", &f.to_string());
        let text = f["text"].as_str().unwrap().to_owned();
        assert!(text.contains("postgres:16-alpine"));

        let broken = text.replace("  cache:\n    image: redis:7-alpine", "  cache:\n    image: redis:7-alpine\n   bad: [");
        let b: Value = serde_json::from_str(&plan_event(cli, store, name, &broken, &json!(1))).unwrap();
        keep("plan-broken", &b.to_string());
        println!("broken: {}", b["error"]);
        assert!(b["error"]["line"].as_u64().is_some(), "a broken file names its line: {b}");

        let draft = text.replace("postgres:16-alpine", "postgres:17-alpine") + "";
        let draft = draft.replacen("volumes:\n  dbdata:", "  worker:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\nvolumes:\n  dbdata:", 1);
        let pl: Value = serde_json::from_str(&plan_event(cli, store, name, &draft, &json!(2))).unwrap();
        keep("plan", &pl.to_string());
        let acts: BTreeMap<String, String> = pl["steps"].as_array().expect("steps").iter().map(|s| (s["n"].as_str().unwrap().to_owned(), s["act"].as_str().unwrap().to_owned())).collect();
        println!("plan {acts:?}");
        assert_eq!(acts["db"], "chg");
        assert_eq!(acts["worker"], "add");
        for n in ["web", "api", "cache"] {
            assert_eq!(acts[n], "same", "{n}");
        }

        let before = ids(name);
        static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn em(s: String) {
            println!("apply: {s}");
            LINES.lock().unwrap().push(s);
        }
        // The draft's base: the hash of the file it was made from, as the page sends it.
        apply(cli, store, name, &draft, f["hash"].as_str(), em);
        let lines = LINES.lock().unwrap().clone();
        keep("apply", &format!("[{}]", lines.join(",")));
        collect(&mut made);
        let last: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["ok"], true, "{last}");
        let after = ids(name);
        for n in ["web", "api", "cache"] {
            assert_eq!(before[n], after[n], "{n} kept running");
        }
        assert_ne!(before["db"], after["db"], "db recreated");
        assert!(after.contains_key("worker"), "worker added");
        assert!(std::fs::read_to_string(&file).unwrap().contains("postgres:17-alpine"), "saved");
        let ev: Value = serde_json::from_str(&projects_event(cli, store)).unwrap();
        keep("projects-after", &ev.to_string());
        let c: Value = client::get("/containers/json?all=1").unwrap();
        keep("containers", &c.to_string());

        // Edited outside Kiri: what Apply does is compared with what runs.
        let outside = std::fs::read_to_string(&file).unwrap().replace("    image: redis:7-alpine", "    image: redis:7-alpine\n    command: [\"redis-server\", \"--save\", \"\"]");
        std::fs::write(&file, &outside).unwrap();
        let pl: Value = serde_json::from_str(&plan_event(cli, store, name, &outside, &json!(3))).unwrap();
        keep("plan-outside", &pl.to_string());
        let step = |n: &str| pl["steps"].as_array().unwrap().iter().find(|s| s["n"] == n).cloned().unwrap_or_default();
        println!("outside plan {}", pl["steps"]);
        assert_eq!((step("cache")["act"].as_str(), step("cache")["what"].clone()), (Some("chg"), json!(["outside"])));
        assert_eq!(step("web")["act"], "same");

        // The window's Stop all, Start all and Remove project: each of the project's containers.
        let ids: Vec<String> = find(name, store).unwrap().members.iter().map(|m| m.id.clone()).collect();
        collect(&mut made);
        for id in &ids {
            client::request("POST", &format!("/containers/{id}/stop"), None).unwrap();
        }
        assert!(find(name, store).unwrap().members.iter().all(|m| m.state != "running"), "stopped all");
        let pl: Value = serde_json::from_str(&plan_event(cli, store, name, &outside, &json!(4))).unwrap();
        assert_eq!(pl["steps"].as_array().unwrap().iter().find(|s| s["n"] == "web").unwrap()["act"], "start", "a stopped service is started");
        for id in &ids {
            let _ = client::request("POST", &format!("/containers/{id}/start"), None);
        }
        assert!(find(name, store).unwrap().members.iter().filter(|m| m.service != "db").all(|m| m.state == "running"), "started all");
        for id in &ids {
            client::request("DELETE", &format!("/containers/{id}?force=1&v=1"), None).unwrap();
        }
        let ev: Value = serde_json::from_str(&projects_event(cli, store)).unwrap();
        assert!(!ev["projects"].as_array().unwrap().iter().any(|p| p["name"] == name), "removed: gone from Compose");
        let vols: Value = client::get("/volumes").unwrap();
        assert!(vols.to_string().contains(&format!("{name}_dbdata")), "its named volume stays");
        // What this run made, by name: its volumes (an unnamed one too) and its network.
        made.insert(format!("{name}_dbdata"));
        for v in &made {
            let _ = client::request("DELETE", &format!("/volumes/{v}"), None);
        }
        let _ = client::request("DELETE", &format!("/networks/{name}_default"), None);
        let vols: Value = client::get("/volumes").unwrap();
        let left: Vec<&String> = made.iter().filter(|v| vols.to_string().contains(v.as_str())).collect();
        assert!(left.is_empty(), "this run's volumes are gone: {left:?}");
    }

    /// A file opened in the window, never run: listed as not started, every
    /// service new, Apply brings it up, and a service that dies right after
    /// is named. `KIRI_SOCKET=… cargo test -p kiri live_open_file -- --ignored`.
    #[test]
    #[ignore = "needs a running engine"]
    fn live_open_file() {
        let _clean = Cleanup::new(vec![vec!["compose", "-p", "kiriopen", "down", "-v", "--remove-orphans"]], None);
        let dir = std::env::temp_dir().join(format!("kiriopen{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("compose.yaml");
        std::fs::write(&file, "name: kiriopen\nservices:\n  ok:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n  bad:\n    image: alpine:3.20\n    command: [\"sh\", \"-c\", \"echo no data; exit 2\"]\n  late:\n    image: alpine:3.20\n    command: [\"sh\", \"-c\", \"sleep 3; exit 4\"]\n").unwrap();
        fn st() -> Option<PathBuf> {
            Some(std::env::temp_dir().join(format!("kiriopen{}", std::process::id())).join("state.json"))
        }
        let o: Value = serde_json::from_str(&open_file(cli, st, &file)).unwrap();
        println!("opened {o}");
        assert_eq!(o["name"], "kiriopen", "{o}");
        assert!(!o.to_string().contains(&*dir.to_string_lossy()), "no path back to the page");
        let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
        let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == "kiriopen").expect("listed before it ran").clone();
        println!("project {p}");
        assert_eq!((p["fresh"].clone(), p["readable"].clone()), (json!(true), json!(true)));
        let fe: Value = serde_json::from_str(&file_event("kiriopen", st)).unwrap();
        let (text, hash) = (fe["text"].as_str().unwrap().to_owned(), fe["hash"].as_str().unwrap().to_owned());
        let pl: Value = serde_json::from_str(&plan_event(cli, st, "kiriopen", &text, &json!(1))).unwrap();
        println!("plan {}", pl["steps"]);
        assert!(pl["steps"].as_array().unwrap().iter().all(|s| s["act"] == "add"), "{pl}");
        static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn em(s: String) {
            println!("apply: {s}");
            LINES.lock().unwrap().push(s);
        }
        apply(cli, st, "kiriopen", &text, Some(&hash), em);
        let last: Value = serde_json::from_str(LINES.lock().unwrap().last().unwrap()).unwrap();
        assert_eq!(last["ok"], true, "{last}");
        let failed: Vec<&str> = last["failed"].as_array().unwrap().iter().filter_map(|f| f["svc"].as_str()).collect();
        // The one that dies at once and the one that dies three seconds later: both named.
        assert_eq!(failed, ["bad", "late"], "{last}");
        let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
        let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == "kiriopen").unwrap().clone();
        assert_eq!(p["fresh"], false);
        let _ = cli().unwrap().args(["compose", "-p", "kiriopen", "-f"]).arg(&file).args(["down", "-v"]).status();

        // A pipe for the folder's .env or a service's env_file: opened under
        // its own name, and the list, the plan and Apply refuse at once.
        #[cfg(unix)]
        for (n, at) in [("kiripipea", ".env"), ("kiripipeb", "web.env")] {
            let d = dir.join(n);
            std::fs::create_dir_all(&d).unwrap();
            let f = d.join("compose.yaml");
            let env = if at == ".env" { String::new() } else { format!("    env_file: [{at}]\n") };
            std::fs::write(&f, format!("name: {n}\nservices:\n  web:\n    image: alpine:3.20\n{env}")).unwrap();
            assert!(Command::new("mkfifo").arg(d.join(at)).status().unwrap().success());
            let t = std::time::Instant::now();
            let o: Value = serde_json::from_str(&open_file(cli, st, &f)).unwrap();
            assert_eq!(o["name"], n, "{o}");
            let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
            let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == n).expect("listed").clone();
            assert_eq!((p["readable"].clone(), p["errkey"].clone()), (json!(false), json!("badEnv")), "{p}");
            let pl: Value = serde_json::from_str(&plan_event(cli, st, n, "services: {}\n", &json!(1))).unwrap();
            assert_eq!(pl["error"]["errkey"], "badEnv", "{pl}");
            apply(cli, st, n, "services: {}\n", None, em);
            let last: Value = serde_json::from_str(LINES.lock().unwrap().last().unwrap()).unwrap();
            assert_eq!((last["ok"].clone(), last["saved"].clone()), (json!(false), json!(false)), "{last}");
            println!("{n}: {:?}", t.elapsed());
            assert!(t.elapsed() < std::time::Duration::from_secs(3), "{n}: {:?}", t.elapsed());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Apply's safety on a running engine: a running profile is kept, an
    /// outside change stops Apply, a failed Apply leaves the file saved and
    /// Discard puts it back, and logs work once the file is gone.
    /// `KIRI_SOCKET=… cargo test -p kiri live_safety -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a running engine"]
    fn live_safety() {
        // A name of this run's own: a second run on the same engine, or one
        // beside it, never meets it, and only what it made is removed.
        static SAFE: LazyLock<String> = LazyLock::new(|| format!("kirisafe{}x{}", std::process::id(), SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().subsec_micros()));
        let n: &str = &SAFE;
        // A project named from a shell-only secret (`name: …-${B_SECRET}`).
        let b2 = format!("{n}b2-bq7-shell-x9");
        let _clean = Cleanup::at_end(
            vec![
                vec!["compose", "-p", b2.as_str(), "down", "-v", "--remove-orphans"],
                vec!["rm", "-f", "x-Dpl-x-4u4u9"],
                vec!["compose", "-p", n, "down", "-v", "--remove-orphans"],
                vec!["rmi", "alpine:Cmt-x-77ff"],
            ],
            None,
        );
        let dir = std::env::temp_dir().join(format!("kirisafe{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bad")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // Removed however the test ends, a failed run too.
        let _gone = RmDir(dir.clone());
        let file = dir.join("compose.yaml");
        let first = &format!("name: {n}\nservices:\n  ok:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n  debug:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n    profiles: [debug]\n");
        std::fs::write(&file, first).unwrap();
        std::fs::write(dir.join("bad/Dockerfile"), "FROM alpine:3.20\nRUN exit 3\n").unwrap();
        fn st() -> Option<PathBuf> {
            Some(std::fs::canonicalize(std::env::temp_dir()).unwrap().join(format!("kirisafe{}", std::process::id())).join("state.json"))
        }
        let up = cli().unwrap().args(["compose", "-p", n, "--profile", "debug", "-f"]).arg(&file).args(["up", "-d"]).status().unwrap();
        assert!(up.success());
        static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn em(s: String) {
            println!("apply: {s}");
            LINES.lock().unwrap().push(s);
        }
        let last = || -> Value { serde_json::from_str(LINES.lock().unwrap().last().unwrap()).unwrap() };
        let id = |svc: &str| find(n, st).unwrap().members.iter().find(|m| m.service == svc).map(|m| m.id.clone());

        // A service up under --profile debug is neither removed by the plan nor by Apply.
        let f: Value = serde_json::from_str(&file_event(n, st)).unwrap();
        let (text, hash) = (f["text"].as_str().unwrap().to_owned(), f["hash"].as_str().unwrap().to_owned());
        let pl: Value = serde_json::from_str(&plan_event(cli, st, n, &text, &json!(1))).unwrap();
        println!("profile plan {}", pl["steps"]);
        assert!(pl["steps"].as_array().unwrap().iter().all(|s| s["act"] == "same"), "{pl}");

        // Changed outside Kiri since the draft was made: nothing is saved.
        let outside = format!("{text}# changed outside\n");
        std::fs::write(&file, &outside).unwrap();
        apply(cli, st, n, &format!("{text}# Kiri's edit\n"), Some(&hash), em);
        assert_eq!((last()["conflict"].clone(), last()["ok"].clone()), (json!(true), json!(false)), "{}", last());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), outside, "the outside change stays");

        // A build that fails: saved, not applied; the plan says so; Discard puts the file back.
        let base = file_hash(outside.as_bytes());
        let bad = format!("{outside}  bad:\n    build: ./bad\n");
        apply(cli, st, n, &bad, Some(&base), em);
        let l = last();
        assert_eq!((l["ok"].clone(), l["saved"].clone()), (json!(false), json!(true)), "{l}");
        assert!(std::fs::read_to_string(&file).unwrap().contains("build: ./bad"), "saved");
        let pl: Value = serde_json::from_str(&plan_event(cli, st, n, &bad, &json!(2))).unwrap();
        assert!(pl["steps"].as_array().unwrap().iter().any(|s| s["n"] == "bad" && s["act"] == "add"), "not what runs: {pl}");
        let f: Value = serde_json::from_str(&file_event(n, st)).unwrap();
        assert_eq!(f["restore"], true);
        restore(n, st).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), outside, "put back");

        // Apply with the profile's service running keeps it.
        let before = id("debug");
        apply(cli, st, n, &format!("{outside}# again\n"), Some(&base), em);
        assert_eq!(last()["ok"], true, "{}", last());
        assert_eq!(id("debug"), before, "debug kept running");

        // Plain .env values (one is the project's name) stay readable, so
        // Apply sees which services it started and names the one that died.
        std::fs::write(dir.join(".env"), format!("COMPOSE_PROJECT_NAME={n}\nWHO=kirisafe\nDEBUG=true\nPORT=8080\nAUTH_SERVICE=auth\nDB_PASSWORD=Hunter2-kirisafe-x\nPOSTGRES_PASSWORD=postgres\n")).unwrap();
        let base = file_hash(&std::fs::read(&file).unwrap());
        let dies = format!("{outside}# again\n  dies:\n    image: alpine:3.20\n    command: [\"sh\", \"-c\", \"sleep 1; exit 1\"]\n    environment: [\"P=${{DB_PASSWORD}}\"]\n");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &dies, Some(&base), em);
        let l = last();
        assert_eq!(l["ok"], true, "{l}");
        assert!(l["failed"].as_array().unwrap().iter().any(|f| f["svc"] == "dies"), "the watch named it: {l}");
        assert!(LINES.lock().unwrap().iter().any(|e| e.contains("\"svc\":[\"dies\"]")), "progress per service");
        assert!(!LINES.lock().unwrap().iter().any(|e| e.contains("Hunter2-kirisafe-x")), "the password never shows");
        let base = l["hash"].as_str().unwrap().to_owned();
        assert_eq!(base, file_hash(&std::fs::read(&file).unwrap()), "the answer carries the saved file's hash");

        // A password that is an image's name (POSTGRES_PASSWORD=postgres): the
        // image stays as it is everywhere, and Apply still reads its progress.
        // Postgres and a container_name job that die a moment apart are both named.
        let two = format!("{dies}  pg:\n    image: postgres:16-alpine\n    environment: [\"POSTGRES_PASSWORD=${{POSTGRES_PASSWORD}}\"]\n    command: [\"postgres\", \"-c\", \"no_such_setting=1\"]\n  job:\n    image: alpine:3.20\n    container_name: {n}-nightly\n    command: [\"sh\", \"-c\", \"sleep 2; exit 5\"]\n");
        let pl: Value = serde_json::from_str(&plan_event(cli, st, n, &two, &json!(5))).unwrap();
        let pg = pl["services"].as_array().unwrap().iter().find(|s| s["name"] == "pg").cloned().unwrap_or_default();
        assert_eq!(pg["image"], "postgres:16-alpine", "{pl}");
        assert!(pl["steps"].as_array().unwrap().iter().any(|s| s["n"] == "pg" && s["to"] == "postgres:16-alpine"), "{pl}");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &two, Some(&base), em);
        let l = last();
        assert_eq!(l["ok"], true, "{l}");
        let named: Vec<&str> = l["failed"].as_array().unwrap().iter().filter_map(|f| f["svc"].as_str()).collect();
        assert!(named.contains(&"pg") && named.contains(&"job"), "both named: {l}");
        assert!(LINES.lock().unwrap().iter().any(|e| e.contains("\"svc\":[\"job\"]")), "the container_name's progress");
        assert!(!LINES.lock().unwrap().iter().any(|e| e.contains("${POSTGRES_PASSWORD}")), "the image's name whole");
        let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
        let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == n).unwrap().clone();
        assert!(p["services"].as_array().unwrap().iter().any(|s| s["image"] == "postgres:16-alpine"), "the card: {p}");
        assert!(p["containers"].as_array().unwrap().iter().any(|c| c["image"] == "postgres:16-alpine"), "the Services table: {p}");
        assert!(p["containers"].as_array().unwrap().iter().any(|c| c["name"] == format!("{n}-nightly")), "the container's name: {p}");
        // postgres's unnamed volume: this run's, removed by name when the test ends.
        for c in p["containers"].as_array().unwrap().iter().filter(|c| c["service"] == "pg") {
            let c = client::get(&format!("/containers/{}/json", c["id"].as_str().unwrap_or_default())).unwrap_or_default();
            MADE.lock().unwrap().extend(c["Mounts"].as_array().into_iter().flatten().filter(|m| m["Type"] == "volume").filter_map(|m| m["Name"].as_str().map(str::to_owned)));
        }
        // A secret where a port belongs: said, without the secret.
        let port = format!("{two}  web:\n    image: nginx\n    ports: [\"${{DB_PASSWORD}}:80\"]\n");
        let pl: Value = serde_json::from_str(&plan_event(cli, st, n, &port, &json!(6))).unwrap();
        let text = pl["error"]["text"].as_str().unwrap_or_default().to_owned();
        assert!(text.contains("${DB_PASSWORD}") && !text.contains("Hunter2-kirisafe-x"), "{pl}");
        let base = l["hash"].as_str().unwrap().to_owned();
        let dies = two;

        // An edit landing while Compose reads the draft: refused, theirs kept.
        fn theirs(p: &Path) {
            let t = std::fs::read_to_string(p).unwrap();
            std::fs::write(p, format!("{t}# theirs mid-apply\n")).unwrap();
        }
        *BETWEEN.lock().unwrap() = Some((file.clone(), theirs));
        apply(cli, st, n, &format!("{dies}# mine\n"), Some(&base), em);
        *BETWEEN.lock().unwrap() = None;
        assert_eq!(last()["conflict"], true, "{}", last());
        let now = std::fs::read_to_string(&file).unwrap();
        assert!(now.ends_with("# theirs mid-apply\n") && !now.contains("# mine"), "their edit stays: {now}");
        // No base at all: refused too.
        apply(cli, st, n, &format!("{now}# mine\n"), None, em);
        assert_eq!(last()["conflict"], true, "{}", last());
        let base = file_hash(now.as_bytes());

        // Cancel while Compose still reads the draft: nothing saved, says cancelled.
        let canceller = std::thread::spawn(|| {
            for _ in 0..500 {
                if cancel(n) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            false
        });
        apply(cli, st, n, &format!("{now}  late:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n"), Some(&base), em);
        assert!(canceller.join().unwrap());
        let l = last();
        assert_eq!((l["cancelled"].clone(), l["ok"].clone()), (json!(true), json!(false)), "{l}");
        if l["saved"] == false {
            assert_eq!(std::fs::read_to_string(&file).unwrap(), now, "not saved");
        }
        assert!(id("late").is_none(), "late never started");
        std::fs::write(&file, &outside).unwrap();

        // Secrets as Compose reads the `.env` (a ` # comment`, quotes, `${VAR}`,
        // `KEY: value`) never show: not in the plan, Apply's lines and answer,
        // nor in what the list says of a file Compose can't read.
        let env = std::fs::read_to_string(dir.join(".env")).unwrap();
        std::fs::write(dir.join(".env"), format!("{env}BASE=b4se\nCMT_PASSWORD=Cmt-x-77ff # prod\nQC_PASSWORD=\"Quo-x-1a\" # note\nVAR_PASSWORD=${{BASE}}-Var-x-2b\nDQ_PASSWORD=\"Dq-x\\$3c\"\nYAML_PASSWORD: Yml-x-8h\n")).unwrap();
        // Most of one counts too (a parser quotes back what it read of it).
        let leak = |t: &str| leak_in(t, &["Cmt-x-77", "Quo-x-1a", "b4se-Var-x-2b", "Dq-x$3c", "Yml-x-8h"]);
        let base = file_hash(outside.as_bytes());
        let nm = format!("{outside}  nm:\n    image: alpine:3.20\n    network_mode: \"container:${{CMT_PASSWORD}}\"\n    environment: [\"A=${{QC_PASSWORD}}\", \"B=${{VAR_PASSWORD}}\", \"C=${{DQ_PASSWORD}}\", \"D=${{YAML_PASSWORD}}\"]\n");
        let pl = plan_event(cli, st, n, &nm, &json!(7));
        println!("network_mode plan {pl}");
        assert_eq!(leak(&pl), None, "{pl}");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &nm, Some(&base), em);
        assert_eq!(last()["ok"], false, "{}", last());
        for e in LINES.lock().unwrap().iter() {
            assert_eq!(leak(e), None, "{e}");
        }
        let bad = format!("{outside}  nm:\n    image: alpine:3.20\n    mem_limit: ${{CMT_PASSWORD}}\n");
        std::fs::write(&file, &bad).unwrap();
        let ev = projects_event(cli, st);
        let warn = serde_json::from_str::<Value>(&ev).unwrap()["projects"].as_array().unwrap().iter().find(|p| p["name"] == n).unwrap()["error"].clone();
        println!("can't read: {warn}");
        assert!(warn.is_string(), "{ev}");
        assert_eq!(leak(&ev), None, "{ev}");
        let pl = plan_event(cli, st, n, &bad, &json!(8));
        assert!(pl.contains("\"error\"") && leak(&pl).is_none(), "{pl}");
        // A key over several lines, a `$` in a value, and a short one: as
        // Compose quotes back what it read of them (`parsing "Dq-x$3"`, `"r3"`),
        // in the plan, the list's warning, Logs and an action's error.
        std::fs::write(dir.join(".env"), format!("{env}DQ_PASSWORD=\"Dq-x\\$3c\"\nREDIS_PASSWORD=r3dis\nML_PRIVATE_KEY=\"-----BEGIN KEY-----\nMlq-x-line1AAA\nMlq-x-line2BBB\n-----END KEY-----\"\n")).unwrap();
        let leak = |t: &str| leak_in(t, &["Dq-x$3", "q-x$3c", "r3dis", "\\\"r3\\\"", "Mlq-x-", "line1AAA", "line2BBB"]);
        for name in ["DQ_PASSWORD", "REDIS_PASSWORD", "ML_PRIVATE_KEY"] {
            for body in [format!("    mem_limit: ${{{name}}}\n"), format!("    depends_on: [\"${{{name}}}\"]\n")] {
                let bad = format!("{outside}  nm:\n    image: alpine:3.20\n{body}");
                let pl = plan_event(cli, st, n, &bad, &json!(9));
                println!("{name} plan {pl}");
                assert!(pl.contains("\"error\"") && leak(&pl).is_none(), "{pl}");
                std::fs::write(&file, &bad).unwrap();
                let ev = projects_event(cli, st);
                assert_eq!(leak(&ev), None, "{ev}");
                let lg = logs_event(cli, st, n);
                assert_eq!(leak(&lg), None, "{lg}");
                if let Err(Fault::Said(e) | Fault::Own(e)) = action(cli, n, "restart") {
                    let e = hider_of(cli, st, n).hide(&action_error(&e));
                    assert_eq!(leak(&e), None, "{e}");
                }
                std::fs::write(&file, &outside).unwrap();
            }
        }
        std::fs::write(&file, &outside).unwrap();

        // The shapes that once reached the window: a `\`, a `"`, a `'`, a `:`,
        // a `/` and `+` (said back lower-cased), in a pull policy, a port, an
        // env file's path; each error names its variable.
        std::fs::write(dir.join(".env"), format!("{env}BS_PASSWORD='Bs-x\\yy\"4dd4'\nBSD_PASSWORD='Bsd-x\\zz\"5ee5'\nSQ_PASSWORD=\"Sq1-x'7vv7\"\nCOLON_TOKEN=Col1-x:9zq7:Pw8k\nSL_ACCESS_KEY=wJalrXUt/K7MDENG+bPx/Zk9Q\nCMT_PASSWORD=Cmt-x-77ff\nEF_PASSWORD=Envf-x-6f\n")).unwrap();
        let leak = |t: &str| leak_in(t, &["Bs-x\\yy", "Bs-x\\\\yy", "yy\"4dd4", "yy\\\"4dd4", "Bsd-x\\z", "Bsd-x\\\\z", "zz\"5ee5", "zz\\\"5ee5", "Sq1-x'", "x'7vv7", "Col1-x", ":9zq7:", "7:Pw8k", "wJalrX", "k7mdeng", "K7MDENG", "bpx/zk9q", "bPx/Zk9Q", "Cmt-x-", "x-77ff", "Envf-x", "kirisec"]);
        let says = |body: &str, var: &str| {
            let bad = format!("{outside}  nm:\n    image: alpine:3.20\n{body}");
            let pl = plan_event(cli, st, n, &bad, &json!(10));
            println!("{var} plan {pl}");
            assert!(leak(&pl).is_none() && pl.contains(&format!("${{{var}}}")), "{pl}");
            std::fs::write(&file, &bad).unwrap();
            let ev = projects_event(cli, st);
            assert_eq!(leak(&ev), None, "{ev}");
            std::fs::write(&file, &outside).unwrap();
        };
        for var in ["BS_PASSWORD", "BSD_PASSWORD", "SQ_PASSWORD"] {
            says(&format!("    pull_policy: ${{{var}}}\n"), var);
        }
        for port in ["${COLON_TOKEN}:80", "80:${COLON_TOKEN}", "${COLON_TOKEN}"] {
            says(&format!("    ports: [\"{port}\"]\n"), "COLON_TOKEN");
        }
        says("    ports: [\"80:${SL_ACCESS_KEY}\"]\n", "SL_ACCESS_KEY");
        says("    env_file: ${EF_PASSWORD}\n", "EF_PASSWORD");
        // An image tag that is a secret: the plan, the cards, the list, Apply's
        // lines, its toast and note all name the variable. Not there first:
        // saved, not applied.
        let tagged = format!("{outside}  tag:\n    image: alpine:${{CMT_PASSWORD}}\n    command: [\"sleep\", \"infinity\"]\n");
        let pl = plan_event(cli, st, n, &tagged, &json!(11));
        assert!(leak(&pl).is_none() && pl.contains("alpine:${CMT_PASSWORD}"), "{pl}");
        // A secret in an image: the warning, before Apply.
        assert_eq!(serde_json::from_str::<Value>(&pl).unwrap()["namewarn"]["errargs"]["names"], json!(["${CMT_PASSWORD}"]), "{pl}");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &tagged, Some(&file_hash(outside.as_bytes())), em);
        let l = last();
        println!("tag not there: {l}");
        assert_eq!((l["ok"].clone(), l["saved"].clone()), (json!(false), json!(true)), "{l}");
        for e in LINES.lock().unwrap().iter() {
            assert_eq!(leak(e), None, "{e}");
        }
        let ev = projects_event(cli, st);
        assert!(leak(&ev).is_none() && ev.contains("alpine:${CMT_PASSWORD}"), "{ev}");
        restore(n, st).unwrap();
        // The tag on a local image: it runs, and what runs says the variable too.
        assert!(cli().unwrap().args(["tag", "alpine:3.20", "alpine:Cmt-x-77ff"]).status().unwrap().success());
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &tagged, Some(&file_hash(outside.as_bytes())), em);
        let l = last();
        assert_eq!(l["ok"], true, "{l}");
        for e in LINES.lock().unwrap().iter() {
            assert_eq!(leak(e), None, "{e}");
        }
        let ev = projects_event(cli, st);
        println!("tag runs: {ev}");
        // What runs is shown as Docker has it, the file's reading by its
        // variable, and the project carries the warning; nothing else of it.
        let ran = |t: &str| t.replace("alpine:Cmt-x-77ff", "alpine:TAG");
        let evv: Value = serde_json::from_str(&ev).unwrap();
        let p = evv["projects"].as_array().unwrap().iter().find(|p| p["name"] == n).cloned().unwrap();
        assert!(p["containers"].as_array().unwrap().iter().any(|c| c["service"] == "tag" && c["image"] == "alpine:Cmt-x-77ff"), "{p}");
        assert!(p["services"].as_array().unwrap().iter().any(|s| s["name"] == "tag" && s["image"] == "alpine:${CMT_PASSWORD}"), "{p}");
        assert_eq!(p["namewarn"]["errargs"]["names"], json!(["${CMT_PASSWORD}"]), "{p}");
        assert_eq!(leak(&ran(&ev)), None, "{ev}");
        // Its image changed while it runs: the plan's `from` is what runs, as Docker has it.
        let changed = tagged.replace("image: alpine:${CMT_PASSWORD}", "image: alpine:3.20");
        let pl = plan_event(cli, st, n, &changed, &json!(14));
        println!("tag changed plan: {pl}");
        assert_eq!(leak(&ran(&pl)), None, "{pl}");
        let plv: Value = serde_json::from_str(&pl).unwrap();
        let step = plv["steps"].as_array().unwrap().iter().find(|s| s["n"] == "tag").cloned().unwrap_or_default();
        assert_eq!((step["act"].as_str(), step["from"].as_str(), step["to"].as_str()), (Some("chg"), Some("alpine:Cmt-x-77ff"), Some("alpine:3.20")), "{pl}");
        // The draft drops the variable, the old image still runs: the plan keeps the warning.
        assert_eq!(plv["namewarn"]["errargs"]["names"], json!(["${CMT_PASSWORD}"]), "{pl}");
        // A user that is a secret: Apply's answer and the engine's own lines in Logs.
        let user = format!("{tagged}  who:\n    image: alpine:3.20\n    user: ${{CMT_PASSWORD}}\n    command: [\"true\"]\n");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &user, l["hash"].as_str(), em);
        println!("user: {}", last());
        for e in LINES.lock().unwrap().iter() {
            assert_eq!(leak(&ran(e)), None, "{e}");
        }
        let lg = logs_event(cli, st, n);
        println!("user logs: {lg}");
        assert_eq!(leak(&ran(&lg)), None, "{lg}");
        let _ = restore(n, st);
        std::fs::write(&file, &outside).unwrap();
        std::fs::write(dir.join(".env"), &env).unwrap();

        // A key file's path in a secret-named variable: the project reads and plans.
        std::fs::create_dir_all(dir.join("certs")).unwrap();
        std::fs::write(dir.join("certs/server.key"), "k").unwrap();
        std::fs::write(dir.join(".env"), format!("{env}SSL_KEY=./certs/server.key\nSL_ACCESS_KEY=wJalrXUt/K7MDENG+bPx/Zk9Q\nML_PRIVATE_KEY=\"-----BEGIN KEY-----\nMlq-x-line1AAA\nMlq-x-line2BBB\n-----END KEY-----\"\n")).unwrap();
        let leak = |t: &str| leak_in(t, &["tokentoken", "wJalrX", "K7MDENG", "k7mdeng", "MDENG+", "bPx/Zk9Q", "bpx/zk9q", "Mlq-x-", "line1AAA", "line2BBB", "END KEY", "kirisec"]);
        let base = file_hash(outside.as_bytes());
        let key = format!("{outside}  tls:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n    volumes: [\"${{SSL_KEY}}:/k:ro\"]\n");
        let pl: Value = serde_json::from_str(&plan_event(cli, st, n, &key, &json!(12))).unwrap();
        println!("key path plan {pl}");
        assert!(pl["error"].is_null() && pl["steps"].as_array().unwrap().iter().any(|s| s["n"] == "tls" && s["act"] == "add"), "{pl}");
        std::fs::write(&file, &key).unwrap();
        let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
        assert_eq!(ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == n).unwrap()["readable"], true, "{ev}");
        std::fs::write(&file, &outside).unwrap();
        // A service's env file with a broken line: the plan and Apply say the line, never its text.
        std::fs::write(dir.join("svc.env"), "A=1\nB=2\nC=3\nAPI_TOKEN=\"tokentokentoken77\n").unwrap();
        let envf = format!("{outside}  envf:\n    image: alpine:3.20\n    env_file: svc.env\n");
        let pl = plan_event(cli, st, n, &envf, &json!(13));
        println!("env file plan {pl}");
        assert!(pl.contains("unterminated quoted value") && leak(&pl).is_none(), "{pl}");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &envf, Some(&base), em);
        assert_eq!(last()["ok"], false, "{}", last());
        for e in LINES.lock().unwrap().iter() {
            assert_eq!(leak(e), None, "{e}");
        }
        // An access key where an image belongs: refused before `up`, by its variable.
        for img in ["alpine:${SL_ACCESS_KEY}", "${SL_ACCESS_KEY}"] {
            LINES.lock().unwrap().clear();
            apply(cli, st, n, &format!("{outside}  ak:\n    image: \"{img}\"\n"), Some(&base), em);
            let l = last();
            println!("access key image: {l}");
            assert_eq!((l["error"].as_str(), l["errkey"].as_str(), l["saved"].as_bool()), (Some("the image name from ${SL_ACCESS_KEY} isn't valid"), Some("badImage"), Some(false)), "{l}");
            for e in LINES.lock().unwrap().iter() {
                assert_eq!(leak(e), None, "{e}");
            }
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), outside, "nothing saved");
        // A key over several lines as a user: Logs show no line of it.
        let ml = format!("{outside}  mlu:\n    image: alpine:3.20\n    user: \"${{ML_PRIVATE_KEY}}\"\n    command: [\"true\"]\n");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &ml, Some(&base), em);
        println!("multi-line user: {}", last());
        for e in LINES.lock().unwrap().iter() {
            assert_eq!(leak(e), None, "{e}");
        }
        let lg = logs_event(cli, st, n);
        println!("multi-line user logs: {lg}");
        assert_eq!(leak(&lg), None, "{lg}");
        let _ = restore(n, st);
        std::fs::write(&file, &outside).unwrap();
        std::fs::write(dir.join(".env"), &env).unwrap();

        // By name (an include whose path holds a variable, so Compose can't
        // list the variables): Kiri reads nothing of the file but a YAML
        // error's line, lists the project from Docker, acts on it by name,
        // and never plans or applies it.
        {
            let loose = format!("{n}loose");
            let _down = Cleanup::at_end(vec![vec!["compose", "-p", loose.as_str(), "down", "-v", "--remove-orphans"]], None);
            let ld = dir.join("loose");
            std::fs::create_dir_all(ld.join("sub")).unwrap();
            let lf = ld.join("compose.yaml");
            let top = format!("name: {loose}\ninclude:\n  - ${{SUBDIR:-sub}}/compose.yaml\nservices:\n  x:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n");
            let sub_ok = "services:\n  db:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n";
            std::fs::write(&lf, &top).unwrap();
            std::fs::write(ld.join("sub/compose.yaml"), format!("{sub_ok}    ports: [\"5432:5432\"\n")).unwrap();
            let opened: Value = serde_json::from_str(&open_file(cli, st, &lf)).unwrap();
            assert_eq!(opened["name"], loose.as_str(), "{opened}");
            let row = || -> Value { serde_json::from_str::<Value>(&projects_event(cli, st)).unwrap()["projects"].as_array().unwrap().iter().find(|p| p["name"] == loose.as_str()).cloned().unwrap() };
            // A change to the top file is seen at once (its stamp).
            let touch = |text: &str| {
                std::thread::sleep(std::time::Duration::from_millis(20));
                std::fs::write(&lf, text).unwrap();
            };
            // A: a YAML error in the included file: a broken file, its line only.
            let r = row();
            println!("by name, broken: {r}");
            assert!(r["errkey"] == "yamlInc" && r["byname"].is_null(), "{r}");
            std::fs::write(ld.join("sub/compose.yaml"), sub_ok).unwrap();
            touch(&format!("{top}# fixed\n"));
            let r = row();
            println!("by name: {r}");
            assert!(r["byname"] == true && r["error"].is_null() && r["warn"]["errkey"] == "byName" && r["fresh"] == true && r["services"] == json!([]), "{r}");
            // No plan, and Apply is refused: nothing saved, nothing run.
            let f: Value = serde_json::from_str(&file_event(&loose, st)).unwrap();
            let text = f["text"].as_str().unwrap().to_owned();
            let pl: Value = serde_json::from_str(&plan_event(cli, st, &loose, &text, &json!(40))).unwrap();
            assert!(pl["byname"] == true && pl["error"].is_null() && pl["steps"].is_null(), "{pl}");
            LINES.lock().unwrap().clear();
            apply(cli, st, &loose, &format!("{text}# x\n"), f["hash"].as_str(), em);
            assert!(last()["ok"] == false && last()["errkey"] == "byName" && last()["saved"] == false, "{}", last());
            assert_eq!(std::fs::read_to_string(&lf).unwrap(), text, "nothing saved");
            // Started from a terminal: its services and state come from Docker.
            let mut up = cli().unwrap();
            up.current_dir(&ld).args(["compose", "-p", loose.as_str(), "-f", "compose.yaml", "up", "-d"]);
            assert!(up.status().unwrap().success());
            let r = row();
            println!("by name, up: {r}");
            let names: Vec<&str> = r["services"].as_array().unwrap().iter().filter_map(|s| s["name"].as_str()).collect();
            assert!(r["byname"] == true && names == ["db", "x"] && r["containers"].as_array().unwrap().iter().filter(|c| c["state"] == "running").count() == 2, "{r}");
            assert_eq!(r["namewarn"]["errkey"], "namesShown", "{r}");
            // Its actions by name: Stop, Start.
            action(cli, &loose, "stop").unwrap();
            assert!(row()["containers"].as_array().unwrap().iter().all(|c| c["state"] != "running"), "stopped");
            action(cli, &loose, "start").unwrap();
            assert_eq!(row()["containers"].as_array().unwrap().iter().filter(|c| c["state"] == "running").count(), 2, "started");
            // B: a required variable whose message holds a secret and Kiri's
            // own words: not a word of it reaches the list, plan or Apply.
            std::fs::write(ld.join(".env"), "DB_PASSWORD=Need-live-x-77q\n").unwrap();
            std::fs::write(ld.join("sub/compose.yaml"), format!("{sub_ok}    environment:\n      - \"A=${{NEED:?${{DB_PASSWORD}} is already being applied}}\"\n")).unwrap();
            touch(&format!("{top}# b\n"));
            LINES.lock().unwrap().clear();
            let r = row();
            let f: Value = serde_json::from_str(&file_event(&loose, st)).unwrap();
            let pl = plan_event(cli, st, &loose, f["text"].as_str().unwrap(), &json!(41));
            apply(cli, st, &loose, f["text"].as_str().unwrap(), f["hash"].as_str(), em);
            let all = format!("{r}{pl}{}", LINES.lock().unwrap().join(""));
            println!("by name, crafted: {all}");
            assert!(r["byname"] == true, "{r}");
            assert!(!all.contains("Need-live-x-77q") && !all.contains("already being applied") && !all.contains("missing") && !all.contains("busy"), "{all}");
            std::fs::remove_file(ld.join(".env")).unwrap();
            std::fs::write(&lf, &top).unwrap();
            save_opened(st, |name, _| name != loose.as_str(), None).unwrap();
        }
        {
            // A whole file indented, its `name:` made of a secret: listed by its folder, said so.
            let id = dir.join(format!("{n}ind"));
            std::fs::create_dir_all(&id).unwrap();
            std::fs::write(id.join(".env"), "IND_PASSWORD=indpw7x9q2\n").unwrap();
            std::fs::write(id.join("compose.yaml"), "  name: app-${IND_PASSWORD}\n  services:\n    x:\n      image: alpine:3.20\n").unwrap();
            let opened: Value = serde_json::from_str(&open_file(cli, st, &id.join("compose.yaml"))).unwrap();
            let folder = folder_name(&id);
            assert_eq!(opened["name"], folder.as_str(), "{opened}");
            let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
            let r = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == folder.as_str()).cloned().unwrap();
            println!("indented name: {r}");
            assert_eq!((r["namefolder"]["errkey"].as_str(), r["namefolder"]["errargs"]["vars"].as_str()), (Some("nameFolder"), Some("${IND_PASSWORD}")), "{r}");
            assert!(!ev.to_string().contains("indpw7x9q2"), "{ev}");
            save_opened(st, |name, _| name != folder.as_str(), None).unwrap();
        }

        // Secrets in the engine's environment and in `.env`, where the file's
        // structure needs them: never in the list, the plan, Apply's lines or
        // Logs; the project still lists and Apply is allowed.
        fn scli() -> io::Result<Command> {
            let mut c = cli()?;
            c.env("GITHUB_TOKEN", "Fake-live-shell-tok-0123456789xyzXYZ987").env("DEPLOY_TOKEN", "Dpl-x-4u4u9");
            Ok(c)
        }
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("conf/app.env"), "A=1\n").unwrap();
        let leak = |t: &str| leak_in(t, &["Fake-live-shell", "Dpl-x-4u4u9", "x-4u4u9", "98765432", "10.11.12.13", "devprofile", "Bs-x\\yy", "Bs-x\\\\yy", "yy\"4dd4", "yy\\\"4dd4", "kirisec"]);
        let svc = "    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n    labels:\n      k: \"${GITHUB_TOKEN}\"\n";
        let cases = [
            ("env_file", "Z_TOKEN=app\n", format!("  ef:\n{svc}    env_file: ./conf/${{Z_TOKEN}}.env\n")),
            ("profiles", "Z_TOKEN=devprofile\nCOMPOSE_PROFILES=devprofile\n", format!("  pr:\n{svc}    profiles: [\"${{Z_TOKEN}}\"]\n")),
            ("mem_limit", "MEM_SECRET=98765432\n", format!("  ml:\n{svc}    mem_limit: ${{MEM_SECRET}}\n")),
            ("extra_hosts", "IP_SECRET=10.11.12.13\n", format!("  eh:\n{svc}    extra_hosts: [\"host:${{IP_SECRET}}\"]\n")),
            ("container_name", "", format!("  cn:\n{svc}    container_name: x-${{DEPLOY_TOKEN}}\n")),
            ("two-token volume", "Z_TOKEN=./src\n", format!("  tv:\n{svc}    volumes: [\"${{Z_TOKEN}}:/data/${{GITHUB_TOKEN}}:ro\"]\n")),
            ("drive volume", "Z_TOKEN=c\n", format!("  dv:\n{svc}    volumes: [\"${{Z_TOKEN}}:/src:/data/${{GITHUB_TOKEN}}:ro\"]\n")),
            ("backslash user", "BS_PASSWORD='Bs-x\\yy\"4dd4'\n", "  bsu:\n    image: alpine:3.20\n    user: ${BS_PASSWORD}\n    command: [\"true\"]\n".to_owned()),
        ];
        for (tag, extra, body) in cases {
            std::fs::write(dir.join(".env"), format!("{env}{extra}")).unwrap();
            let draft = format!("{outside}{body}");
            let pl = plan_event(scli, st, n, &draft, &json!(20));
            println!("{tag} plan {pl}");
            assert_eq!(leak(&pl), None, "{tag}: {pl}");
            assert!(serde_json::from_str::<Value>(&pl).unwrap()["error"].is_null(), "{tag}: {pl}");
            // A volume Compose can't list the variables of: by name, no plan, and Apply refused.
            if tag == "drive volume" {
                assert_eq!(serde_json::from_str::<Value>(&pl).unwrap()["byname"], true, "{tag}: {pl}");
                LINES.lock().unwrap().clear();
                apply(scli, st, n, &draft, Some(&file_hash(outside.as_bytes())), em);
                let l = last();
                assert!(l["errkey"] == "byName" && l["saved"] == false && leak(&l.to_string()).is_none(), "{tag}: {l}");
                assert_eq!(std::fs::read_to_string(&file).unwrap(), outside, "{tag}: nothing saved");
                continue;
            }
            // A secret in a container name: the warning, before Apply; none for the others.
            let named = tag == "container_name";
            let nw = serde_json::from_str::<Value>(&pl).unwrap()["namewarn"].clone();
            assert_eq!(nw["errargs"]["names"], if named { json!(["${DEPLOY_TOKEN}"]) } else { Value::Null }, "{tag}: {pl}");
            // Docker's name for the container is shown as Docker has it; nothing else of the secret.
            let leak = |t: &str| leak(&t.replace("x-Dpl-x-4u4u9", "x-NAME"));
            LINES.lock().unwrap().clear();
            apply(scli, st, n, &draft, Some(&file_hash(outside.as_bytes())), em);
            let l = last();
            println!("{tag} apply {l}");
            assert_ne!(l["saved"], false, "{tag}: Apply refused: {l}");
            for e in LINES.lock().unwrap().iter() {
                assert_eq!(leak(e), None, "{tag}: {e}");
            }
            let ev = projects_event(scli, st);
            assert_eq!(leak(&ev), None, "{tag}: {ev}");
            let p = serde_json::from_str::<Value>(&ev).unwrap()["projects"].as_array().unwrap().iter().find(|p| p["name"] == n).cloned();
            assert!(p.as_ref().is_some_and(|p| p["readable"] == true), "{tag}: {ev}");
            let p = p.unwrap();
            assert_eq!(p["namewarn"]["errkey"], if named { json!("nameSecret") } else { Value::Null }, "{tag}: the list's warning, after Apply");
            if named {
                assert!(p["containers"].as_array().unwrap().iter().any(|c| c["name"] == "x-Dpl-x-4u4u9" && c["service"] == "cn"), "{tag}: the name as Docker has it: {p}");
                assert!(l["ok"] == true || LINES.lock().unwrap().iter().any(|e| e.contains("\"svc\":[\"cn\"]")), "{tag}: progress by the service label");
            }
            if tag == "backslash user" {
                // The engine's own line says the user: give it a moment to be there.
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            let lg = logs_event(scli, st, n);
            println!("{tag} logs {lg}");
            assert_eq!(leak(&lg), None, "{tag}: {lg}");
            // In the warning state too: logs come by the project's name, the file unread.
            let lgv: Value = serde_json::from_str(&lg).unwrap();
            assert!(lgv["error"].is_null() && lgv["lines"].is_array(), "{tag}: {lg}");
            if tag == "backslash user" {
                assert!(lg.contains("${BS_PASSWORD}") || !lg.contains("unknown user"), "{tag}: {lg}");
            }
            let _ = restore(n, st);
            std::fs::write(&file, &outside).unwrap();
            if named {
                // Its container still runs with the secret in its name, and
                // the next cases' warnings would rightly name it: removed.
                assert!(cli().unwrap().args(["rm", "-f", "-v", "x-Dpl-x-4u4u9"]).stdout(Stdio::null()).status().unwrap().success());
            }
        }
        std::fs::write(dir.join(".env"), &env).unwrap();

        // An included project's own `.env`: its secret gets a stand-in too, in
        // the plan, the list, its error, Apply's lines and Logs.
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/.env"), "INC_PASSWORD=Inc-x-5e5e5q\n").unwrap();
        let sub = "services:\n  inc:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n    volumes: [\"./data/${INC_PASSWORD}:/var/lib/x\"]\n";
        std::fs::write(dir.join("sub/compose.yaml"), sub).unwrap();
        let ileak = |t: &str| leak_in(t, &["Inc-x-5e5e5q", "5e5e5q", "kirisec"]);
        let inc = format!("{outside}include:\n  - sub/compose.yaml\n");
        let pl = plan_event(cli, st, n, &inc, &json!(30));
        println!("include plan {pl}");
        assert_eq!(ileak(&pl), None, "{pl}");
        assert!(pl.contains("/sub/data/${INC_PASSWORD}"), "{pl}");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &inc, Some(&file_hash(outside.as_bytes())), em);
        let l = last();
        println!("include apply {l}");
        assert_eq!(l["ok"], true, "{l}");
        for e in LINES.lock().unwrap().iter() {
            assert_eq!(ileak(e), None, "{e}");
        }
        let ev = projects_event(cli, st);
        assert_eq!(ileak(&ev), None, "{ev}");
        let lg = logs_event(cli, st, n);
        assert_eq!(ileak(&lg), None, "{lg}");
        std::fs::write(dir.join("sub/compose.yaml"), "services:\n  inc:\n    image: alpine:3.20\n    ports: [\"${INC_PASSWORD}:80\"]\n").unwrap();
        let pl = plan_event(cli, st, n, &inc, &json!(31));
        println!("include error {pl}");
        assert!(ileak(&pl).is_none() && pl.contains("${INC_PASSWORD}"), "{pl}");
        std::fs::write(&file, &outside).unwrap();
        let _ = std::fs::remove_dir_all(dir.join("sub"));

        // Included at the first column, through a chain five deep: the deepest
        // project's secret gets its stand-in all the same.
        let mut at = dir.join("ch");
        for i in 1..=5 {
            at = at.join(format!("l{i}"));
            std::fs::create_dir_all(&at).unwrap();
            if i < 5 {
                std::fs::write(at.join("compose.yaml"), format!("include:\n- ./l{}/compose.yaml\nservices: {{}}\n", i + 1)).unwrap();
            }
        }
        std::fs::write(at.join(".env"), "DEEP_PASSWORD=Deep-x-7k7k7q\n").unwrap();
        std::fs::write(at.join("compose.yaml"), "services:\n  deep:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n    volumes: [\"./data/${DEEP_PASSWORD}:/var/lib/x\"]\n    environment: [\"P=${DEEP_PASSWORD}\"]\n").unwrap();
        let dleak = |t: &str| leak_in(t, &["Deep-x-7k7k7q", "7k7k7q", "kirisec"]);
        let chain = format!("{outside}include:\n- ch/l1/compose.yaml\n");
        let pl = plan_event(cli, st, n, &chain, &json!(32));
        println!("chain plan {pl}");
        assert_eq!(dleak(&pl), None, "{pl}");
        assert!(pl.contains("${DEEP_PASSWORD}") && pl.contains("\"deep\""), "{pl}");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &chain, Some(&file_hash(outside.as_bytes())), em);
        assert_eq!(last()["ok"], true, "{}", last());
        for e in LINES.lock().unwrap().iter() {
            assert_eq!(dleak(e), None, "{e}");
        }
        let ev = projects_event(cli, st);
        assert_eq!(dleak(&ev), None, "{ev}");
        let lg = logs_event(cli, st, n);
        assert_eq!(dleak(&lg), None, "{lg}");
        std::fs::write(&file, &outside).unwrap();
        let _ = cli().unwrap().args(["rm", "-f", "-v", &format!("{n}-deep-1")]).stdout(Stdio::null()).status();
        let _ = std::fs::remove_dir_all(dir.join("ch"));

        // A secret that is also a word of a name (`admin` of `admin_panel`): the
        // names from Docker are never changed.
        std::fs::write(dir.join(".env"), format!("{env}ADMIN_PASSWORD=admin\nREDIS_PASSWORD=redis\n")).unwrap();
        let words = format!("{outside}  admin_panel:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n    environment:\n      P: ${{ADMIN_PASSWORD}}\n  redis-cache:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &words, Some(&file_hash(outside.as_bytes())), em);
        assert_eq!(last()["ok"], true, "{}", last());
        let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
        let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == n).cloned().unwrap();
        let cn: BTreeSet<String> = p["containers"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str().map(str::to_owned)).collect();
        assert!(cn.contains(&format!("{n}-admin_panel-1")) && cn.contains(&format!("{n}-redis-cache-1")), "{cn:?}");
        assert!(!ev.to_string().contains("${ADMIN_PASSWORD}_") && p["namewarn"].is_null(), "{p}");
        // Put back as it was: only those two of this run's containers go.
        std::fs::write(&file, &outside).unwrap();
        let _ = cli().unwrap().args(["rm", "-f", "-v", &format!("{n}-admin_panel-1"), &format!("{n}-redis-cache-1")]).stdout(Stdio::null()).status();
        std::fs::write(dir.join(".env"), &env).unwrap();

        // A container name a secret joins, taken already: Apply fails; the
        // project carries the warning, and the name is Docker's.
        assert!(cli().unwrap().args(["run", "-d", "--name", "x-Dpl-x-4u4u9", "alpine:3.20", "sleep", "infinity"]).stdout(Stdio::null()).status().unwrap().success());
        let cn = format!("{outside}  cn:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n    container_name: x-${{DEPLOY_TOKEN}}\n");
        LINES.lock().unwrap().clear();
        apply(scli, st, n, &cn, Some(&file_hash(outside.as_bytes())), em);
        let l = last();
        println!("name conflict {l}");
        assert_eq!(l["ok"], false, "{l}");
        // The taken name is Docker's, said as Docker says it; nothing else of the secret.
        for e in LINES.lock().unwrap().iter().chain([l.to_string()].iter()) {
            assert_eq!(leak(&e.replace("x-Dpl-x-4u4u9", "x-NAME")), None, "{e}");
        }
        let pl: Value = serde_json::from_str(&plan_event(scli, st, n, &cn, &json!(40))).unwrap();
        assert_eq!(pl["namewarn"]["errargs"]["names"], json!(["${DEPLOY_TOKEN}"]), "{pl}");
        let _ = restore(n, st);
        std::fs::write(&file, &outside).unwrap();
        let _ = cli().unwrap().args(["rm", "-f", "x-Dpl-x-4u4u9"]).stdout(Stdio::null()).status();

        // An included secret with a default or required marker, by an aliased
        // include Kiri's own reader doesn't follow: it stands in all the same.
        let sub = |d: &Path, body: &str| {
            std::fs::create_dir_all(d.join("sub")).unwrap();
            std::fs::write(d.join("sub/.env"), "DB_PASSWORD=FAKEsubPw-aa11\n").unwrap();
            std::fs::write(d.join("sub/compose.yaml"), format!("services:\n  db:\n    image: alpine:3.20\n{body}")).unwrap();
        };
        let sleak = |t: &str| leak_in(t, &["FAKEsubPw", "subPw-aa11", "kirisec"]);
        let a1 = dir.join("a1");
        sub(&a1, "    container_name: \"db-${DB_PASSWORD:-dflt}\"\n    volumes: [\"./data/${DB_PASSWORD:?need}:/var/lib/x\"]\n");
        let a1top = format!("name: {n}a1\nx-inc: &inc\n  - ./sub/compose.yaml\ninclude: *inc\nservices:\n  x:\n    image: alpine:3.20\n");
        std::fs::write(a1.join("compose.yaml"), &a1top).unwrap();
        let o: Value = serde_json::from_str(&open_file(cli, st, &a1.join("compose.yaml"))).unwrap();
        assert_eq!(o["name"], format!("{n}a1"), "{o}");
        let ev = projects_event(cli, st);
        assert_eq!(sleak(&ev), None, "{ev}");
        let pl = plan_event(cli, st, &format!("{n}a1"), &a1top, &json!(50));
        assert_eq!(sleak(&pl), None, "{pl}");
        assert!(pl.contains("${DB_PASSWORD}") && !pl.contains("dflt"), "{pl}");

        // Compose can't list the variables (an include path with one): by
        // name, none of its words, in the list, the plan, Apply's toast or its note.
        let a2 = dir.join("a2");
        sub(&a2, "    pull_policy: ${DB_PASSWORD}\n");
        let a2top = format!("name: {n}a2\ninclude:\n  - ${{SUBDIR:-sub}}/compose.yaml\nservices:\n  x:\n    image: alpine:3.20\n");
        std::fs::write(a2.join("compose.yaml"), &a2top).unwrap();
        let o: Value = serde_json::from_str(&open_file(cli, st, &a2.join("compose.yaml"))).unwrap();
        assert_eq!(o["name"], format!("{n}a2"), "{o}");
        let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
        assert_eq!(sleak(&ev.to_string()), None, "{ev}");
        let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == format!("{n}a2")).cloned().unwrap();
        assert!(p["byname"] == true && p["error"].is_null() && p["warn"]["errkey"] == "byName", "{p}");
        let pl = plan_event(cli, st, &format!("{n}a2"), &a2top, &json!(51));
        assert!(sleak(&pl).is_none() && pl.contains("\"byname\":true") && !pl.contains("\"steps\""), "{pl}");
        let fe: Value = serde_json::from_str(&file_event(&format!("{n}a2"), st)).unwrap();
        LINES.lock().unwrap().clear();
        apply(cli, st, &format!("{n}a2"), &a2top, fe["hash"].as_str(), em);
        assert!(last()["error"] == BY_NAME && last()["errkey"] == "byName" && last()["saved"] == false, "{}", last());
        for e in LINES.lock().unwrap().iter() {
            assert_eq!(sleak(e), None, "{e}");
        }

        // A project name made of a shell-only secret, started in a terminal:
        // the list warns of it, and shows its network as Docker has it.
        let b2d = dir.join("b2");
        std::fs::create_dir_all(&b2d).unwrap();
        std::fs::write(b2d.join("compose.yaml"), format!("name: {n}b2-${{B_SECRET}}\nservices:\n  x:\n    image: alpine:3.20\n    command: [\"sleep\", \"infinity\"]\n")).unwrap();
        let up = cli().unwrap().env("B_SECRET", "bq7-shell-x9").args(["compose", "-f"]).arg(b2d.join("compose.yaml")).args(["up", "-d"]).status().unwrap();
        assert!(up.success());
        let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
        let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == b2).cloned().unwrap();
        println!("shell-named {p}");
        assert_eq!(p["namewarn"]["errargs"]["names"], json!(["${B_SECRET}"]), "{p}");
        assert_eq!(p["networks"], json!([format!("{b2}_default")]), "{p}");
        let _ = cli().unwrap().args(["compose", "-p", &b2, "down", "-v", "--remove-orphans"]).stdout(Stdio::null()).stderr(Stdio::null()).status();

        // A pipe for the `.env`: the list keeps answering, and says why; Apply is refused.
        std::fs::remove_file(dir.join(".env")).unwrap();
        assert!(Command::new("mkfifo").arg(dir.join(".env")).status().unwrap().success());
        let t = std::time::Instant::now();
        let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
        let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == n).unwrap().clone();
        println!("pipe .env: {}", p["error"]);
        assert!(p["error"].as_str().unwrap_or_default().starts_with("couldn't read env file "), "{p}");
        let f: Value = serde_json::from_str(&file_event(n, st)).unwrap();
        assert!(f["text"].is_string(), "the file still reads: {f}");
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &format!("{outside}# x\n"), f["hash"].as_str(), em);
        assert!(last()["error"].as_str().unwrap_or_default().starts_with("couldn't read env file "), "{}", last());
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "answered: {:?}", t.elapsed());
        std::fs::remove_file(dir.join(".env")).unwrap();
        std::fs::write(dir.join(".env"), env).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), outside, "nothing saved");

        // A YAML error in an included file: never put on this file's line, in the list, the plan or Apply.
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/compose.yaml"), "services:\n  inc:\n    image: alpine:3.20\n    ports: [\"80:80\"\n    environment:\n      A: b\n").unwrap();
        let inc = format!("include:\n  - ./sub/compose.yaml\n{outside}");
        std::fs::write(&file, &inc).unwrap();
        let ev: Value = serde_json::from_str(&projects_event(cli, st)).unwrap();
        let p = ev["projects"].as_array().unwrap().iter().find(|p| p["name"] == n).unwrap().clone();
        println!("included yaml error, list: {} {}", p["error"], p["errkey"]);
        assert_eq!((p["errkey"].as_str(), p["readable"].as_bool()), (Some("yamlInc"), Some(false)), "{p}");
        assert!(p["error"].as_str().unwrap().starts_with(YAML_INC), "{p}");
        let pl: Value = serde_json::from_str(&plan_event(cli, st, n, &inc, &json!(9))).unwrap();
        println!("included yaml error, plan: {}", pl["error"]);
        assert_eq!((pl["error"]["line"].clone(), pl["error"]["errkey"].as_str()), (Value::Null, Some("yamlInc")), "{pl}");
        let f: Value = serde_json::from_str(&file_event(n, st)).unwrap();
        LINES.lock().unwrap().clear();
        apply(cli, st, n, &format!("{inc}# y\n"), f["hash"].as_str(), em);
        println!("included yaml error, apply: {}", last());
        assert_eq!((last()["errkey"].as_str(), last()["ok"].as_bool()), (Some("yamlInc"), Some(false)), "{}", last());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), inc, "nothing saved");

        // Two includes, one secret name: a plain value in the one Kiri follows never shows the other's secret.
        const SUB: &str = "FAKEsubPw-aa11";
        for s in ["sub1", "sub2"] {
            std::fs::create_dir_all(dir.join(s)).unwrap();
        }
        std::fs::write(dir.join("sub1/.env"), "DB_PASSWORD=5432\n").unwrap();
        std::fs::write(dir.join("sub1/compose.yaml"), "services:\n  a:\n    image: alpine:3.20\n    environment: [\"P=${DB_PASSWORD}\"]\n").unwrap();
        std::fs::write(dir.join("sub2/.env"), format!("DB_PASSWORD={SUB}\n")).unwrap();
        std::fs::write(dir.join("sub2/compose.yaml"), "services:\n  b:\n    image: \"alpine:${DB_PASSWORD}\"\n    container_name: \"b-${DB_PASSWORD}\"\n    volumes: [\"./data/${DB_PASSWORD}:/var/lib/x\"]\n").unwrap();
        let two = format!("x-two: &two ./sub2/compose.yaml\ninclude:\n  - ./sub1/compose.yaml\n  - *two\n{outside}");
        std::fs::write(&file, &two).unwrap();
        let ev = projects_event(cli, st);
        let pl = plan_event(cli, st, n, &two, &json!(10));
        println!("two includes: {}", &pl[..pl.len().min(400)]);
        for v in [SUB, "FAKEsub", "aa11"] {
            assert!(!ev.contains(v) && !pl.contains(v), "{v} shown: {ev} {pl}");
        }
        std::fs::write(&file, &outside).unwrap();

        // The file gone: its logs still come, the file tab says it plainly.
        std::fs::rename(&file, dir.join("moved.txt")).unwrap();
        let lg: Value = serde_json::from_str(&logs_event(cli, st, n)).unwrap();
        assert!(lg["error"].is_null(), "{lg}");
        let f: Value = serde_json::from_str(&file_event(n, st)).unwrap();
        assert_eq!(f["error"], NO_FILE);
        action(cli, n, "remove").unwrap();
        assert!(find(n, st).is_err(), "removed");
        let nets: Value = client::get("/networks").unwrap();
        assert!(!nets.to_string().contains(&format!("{n}_default")), "its network went too");
        let _ = cli().unwrap().args(["rmi", "alpine:Cmt-x-77ff"]).status();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The docker CLI with no engine to reach (`config --environment` needs
    /// none) and an environment of its own, so a test sees only its `.env`.
    fn dcli() -> io::Result<Command> {
        let mut c = Command::new(client::docker_cli().expect("a docker CLI"));
        c.env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default()).env("HOME", std::env::var_os("HOME").unwrap_or_default()).env("DOCKER_HOST", "unix:///nonexistent/kiri-test.sock");
        if let Some(d) = std::env::var_os("DOCKER_CONFIG") {
            c.env("DOCKER_CONFIG", d);
        }
        Ok(c)
    }

    fn dproj(d: &Path) -> Project {
        Project { name: "t".into(), dir: d.to_string_lossy().into_owned(), ..Project::default() }
    }

    fn c(project: &str, service: &str, created: i64, files: &str) -> Value {
        json!({"Id": format!("{project}-{service}"), "Names": [format!("/{project}-{service}-1")], "Image": "img", "State": "running", "Status": "Up", "Created": created,
            "Labels": {PROJECT: project, SERVICE: service, WORKDIR: format!("/w/{project}"), FILES: files}})
    }

    #[test]
    fn groups_containers_by_project_with_the_newest_files() {
        let list = json!([c("shop", "web", 1, "/w/shop/old.yaml"), c("shop", "db", 5, "/w/shop/compose.yaml,/w/shop/compose.override.yaml"), {"Id": "x", "Names": ["/solo"], "Labels": {}}]);
        let p = projects(&list);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].files, ["/w/shop/compose.yaml", "/w/shop/compose.override.yaml"]);
        assert_eq!(p[0].members.iter().map(|m| m.service.as_str()).collect::<Vec<_>>(), ["db", "web"]);
    }

    #[test]
    fn a_draft_replaces_only_the_first_file() {
        let p = Project { name: "shop".into(), dir: "/w".into(), files: vec!["/w/a.yaml".into(), "/w/b.yaml".into()], members: Vec::new(), env_files: Vec::new() };
        assert_eq!(project_args(&p, Some("-")), ["compose", "--ansi", "never", "-p", "shop", "--project-directory", "/w", "-f", "-", "-f", "/w/b.yaml"]);
    }

    #[test]
    fn starts_what_a_service_needs_first() {
        let m = json!({"services": {"api": {"depends_on": {"db": {}}}, "db": {}, "web": {"depends_on": {"api": {}, "gone": {}}}}});
        assert_eq!(start_order(&m), ["db", "api", "web"]);
    }

    #[test]
    fn plans_new_changed_removed_and_unchanged() {
        let old = json!({"services": {"db": {"image": "postgres:16"}, "web": {"image": "web", "ports": [1]}, "old": {"image": "x"}}});
        let new = json!({"services": {"db": {"image": "postgres:17"}, "web": {"image": "web", "ports": [1]}, "cache": {"image": "redis"}}});
        let p = plan(&old, &new);
        let acts: Vec<(&str, &str)> = p.iter().map(|s| (s.service.as_str(), s.act)).collect();
        assert_eq!(acts, [("cache", "add"), ("db", "chg"), ("old", "rm"), ("web", "same")]);
        assert_eq!(p[1].what, ["image"]);
        assert_eq!(p[1].image, ("postgres:16".into(), "postgres:17".into()));
    }

    #[test]
    fn plans_against_what_runs() {
        let old = json!({"services": {"db": {"image": "postgres:16"}, "web": {"image": "web"}, "api": {"image": "api"}, "job": {"image": "job"}}});
        let new = json!({"services": {"db": {"image": "postgres:17"}, "web": {"image": "web"}, "api": {"image": "api"}, "job": {"image": "job"}, "cache": {"image": "redis"}}});
        let r = |hash: &str, running: bool, now: Option<&str>| Running { hash: hash.into(), image_id: "i1".into(), image_now: now.map(str::to_owned), running, image: "x".into() };
        let now: BTreeMap<String, Running> = [
            ("db".to_owned(), r("h-db-16", true, Some("i1"))),
            // web's file was edited outside Kiri after it started.
            ("web".to_owned(), r("h-web-old", true, Some("i1"))),
            // api's image name now points at a newer pull.
            ("api".to_owned(), r("h-api", true, Some("i2"))),
            ("job".to_owned(), r("h-job", false, Some("i1"))),
            ("gone".to_owned(), r("h-gone", true, None)),
        ]
        .into_iter()
        .collect();
        let hashes = parse_hashes("db h-db-17\nweb h-web-new\napi h-api\njob h-job\ncache h-cache\n");
        let p = plan_running(&old, &new, &hashes, &now);
        let acts: Vec<(&str, &str, Vec<&str>)> = p.iter().map(|s| (s.service.as_str(), s.act, s.what.clone())).collect();
        assert_eq!(
            acts,
            [("api", "chg", vec!["pulled"]), ("cache", "add", vec![]), ("db", "chg", vec!["image"]), ("job", "start", vec![]), ("web", "chg", vec!["outside"]), ("gone", "rm", vec![])]
        );
        let same = plan_running(&old, &old, &parse_hashes("db h-db-16\n"), &[("db".to_owned(), r("h-db-16", true, Some("i1")))].into_iter().collect());
        assert_eq!(same.iter().find(|s| s.service == "db").unwrap().act, "same");
    }

    #[test]
    fn reads_apply_progress_per_service() {
        let images = images_of("shop", &json!({"services": {"db": {"image": "postgres:17"}, "api": {"build": {"context": "."}}}}));
        let p = |l: &str| progress(l, "shop", &images);
        assert_eq!(p(" Image postgres:17 Pulling "), Some((vec!["db".into()], "pulling")));
        assert_eq!(p(" Image shop-api Building "), Some((vec!["api".into()], "building")));
        assert_eq!(p(" Container shop-db-1 Recreate "), Some((vec!["db".into()], "recreating")));
        assert_eq!(p(" Container shop-my-api-1 Started "), Some((vec!["my-api".into()], "started")));
        assert_eq!(p(" Container other-db-1 Started "), None);
        assert_eq!(p(" Container shop-db-1 Running "), Some((vec!["db".into()], "running")));
        assert_eq!(p("Error response from daemon: No such image"), None);
        assert!(layer_line(" f5d2b4da9164 Downloading 87.33MB"));
        assert!(!layer_line(" Container shop-db-1 Recreate "));
    }

    #[test]
    fn a_container_name_reads_as_its_service() {
        // As `up` writes it for services with `container_name:`, the name alone.
        let model = json!({"services": {"db": {"image": "postgres:16", "container_name": "custom-db"}, "web": {"image": "nginx", "container_name": "a-very-long-name-primary-1"}, "twin": {"image": "alpine"}}});
        let mut names = images_of("shop", &model);
        let mut touched = Vec::new();
        for (line, svc, state) in [(" Container custom-db Creating ", "db", "creating"), (" Container a-very-long-name-primary-1 Started ", "web", "started"), (" Container custom-db Started ", "db", "started"), (" Container shop-twin-1 Started ", "twin", "started")] {
            assert!(matches!(up_line(line, "shop", &mut names, &mut |_| None, &Hider::default(), &mut touched), UpLine::State(ref s, st) if s == &[svc] && st == state), "{line}");
        }
        assert_eq!(touched, ["db", "web", "twin"]);
        // db exits right after it started: the watch names it.
        let mut db = c("shop", "db", 5, "/w/shop/compose.yaml");
        db["Names"] = json!(["/custom-db"]);
        db["State"] = "exited".into();
        db["Status"] = "Exited (2) Less than a second ago".into();
        let f = failed("shop", &touched, &json!([db, c("shop", "web", 5, "/w/shop/compose.yaml")]));
        assert_eq!(f.iter().map(|f| f["svc"].clone()).collect::<Vec<_>>(), [json!("db")]);
    }

    #[test]
    fn other_settings_count_as_a_change() {
        let old = json!({"services": {"w": {"image": "a", "restart": "no"}}});
        let new = json!({"services": {"w": {"image": "a", "restart": "always"}}});
        assert_eq!(plan(&old, &new)[0].what, ["settings"]);
    }

    #[test]
    fn finds_the_line_an_error_names() {
        assert_eq!(error_line("yaml: line 7: mapping values are not allowed in this context"), Some(7));
        assert_eq!(error_line("yaml: unmarshal errors:\n  line 12: cannot unmarshal"), Some(12));
        assert_eq!(error_line("service \"web\" depends on undefined service \"cache\""), None);
        let v5 = "failed to parse -: go-yaml load error in parser (while parsing a block mapping) at L2.C3-L4.C4: did not find expected key";
        assert_eq!(error_line(v5), Some(4));
        assert_eq!(error_text(v5), "(while parsing a block mapping) at L2.C3-L4.C4: did not find expected key");
        assert_eq!(error_text("service \"web\" depends on undefined service \"cache\": invalid compose project"), "service \"web\" depends on undefined service \"cache\"");
        assert_eq!(error_text("validating -: services.web additional properties 'x' not allowed"), "services.web additional properties 'x' not allowed");
    }

    #[test]
    fn hides_environment_values() {
        let v = service_view("db", &json!({"image": "postgres", "environment": {"POSTGRES_PASSWORD": "secret"}, "ports": [{"target": 5432, "published": "5433"}], "depends_on": {"x": {}}}));
        assert!(!v.to_string().contains("secret"));
        assert_eq!(v["ports"], json!(["5433:5432"]));
        assert_eq!(v["depends"], json!(["x"]));
    }

    #[test]
    fn hides_only_names_that_say_secret_as_a_whole_word() {
        for k in ["DB_PASSWORD", "PGPASSWORD", "GPG_PASSPHRASE", "ENCRYPTION_PASSPHRASE", "AWS_SECRETKEY", "MASTERKEY", "SSH_PRIVATEKEY", "APP_SECRETS", "API_TOKENS", "DB_CREDS", "API_KEY", "SECRET_KEY", "SECRETKEY", "AUTHKEY", "PRIVATE_KEY", "SENTRY_DSN", "aws.secret.key", "SECRET_KEY_BASE", "AWS_SECRET_ACCESS_KEY", "SMTP_PASS", "GOOGLE_CREDENTIALS"] {
            assert!(secret_name(k), "{k} says secret");
        }
        for k in ["AUTH_SERVICE", "KEYCLOAK_HOST", "TOKEN_ISSUER", "COMPOSE_PROJECT_NAME", "PORT", "DEBUG", "COMPASS_URL", "PASSWORD_FILE_PATH", "KEYS_DIR", "HOST"] {
            assert!(!secret_name(k), "{k} is plain");
        }
        let d = tdir("words");
        std::fs::write(d.join(".env"), "AUTH_SERVICE=auth\nKEYCLOAK_HOST=keycloak\nTOKEN_ISSUER=web\nGPG_PASSPHRASE=gpg-pp-1\nENCRYPTION_PASSPHRASE=enc9\nAWS_SECRETKEY=awssk\nMASTERKEY=mk77\nSSH_PRIVATEKEY=sshpk\nAPP_SECRETS=appsec\nAPI_TOKENS=tok1\nDB_CREDS=crd\n").unwrap();
        let hider = Hider::new(secrets(dcli, &dproj(&d)), vec![]);
        let e = hider.hide("failed: gpg-pp-1 enc9 awssk mk77 sshpk appsec tok1 crd; auth keycloak web");
        // DB_CREDS=crd: three letters are never hidden.
        assert_eq!(e, "failed: ${GPG_PASSPHRASE} ${ENCRYPTION_PASSPHRASE} ${AWS_SECRETKEY} ${MASTERKEY} ${SSH_PRIVATEKEY} ${APP_SECRETS} ${API_TOKENS} crd; auth keycloak web");
        // Whole words only: never inside a longer word, `-`, `_` and `.` joining words.
        assert_eq!(hider.hide("mk77x tok1x xtok1 tok1_b tok1-b b.tok1 tok1. tok1"), "mk77x tok1x xtok1 tok1_b tok1-b b.tok1 ${API_TOKENS}. ${API_TOKENS}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_password_that_is_a_name_never_changes_the_name() {
        let d = tdir("pgword");
        std::fs::write(d.join(".env"), "POSTGRES_PASSWORD=postgres\nREDIS_PASSWORD=redis\nADMIN_PASSWORD=admin\nAPI_PASSWORD=test\nSHOP_PASSWORD=shop\n").unwrap();
        let p = Project { name: "shop".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], members: vec![], env_files: Vec::new() };
        let model = json!({"services": {"db": {"image": "postgres:16"}, "redis-cache": {"image": "redis:7", "volumes": [{"type": "volume", "source": "redis-data", "target": "/data"}]}, "admin_panel": {"image": "x/admin:1"}},
            "volumes": {"redis-data": {"name": "shop_redis-data"}}, "networks": {"admin-net": {"name": "shop_admin-net"}}});
        let hider = Hider::for_project(dcli, &p, Some(&model));
        // The plan and the cards: names and images as they are.
        let mut steps = plan_json(&[Step { service: "redis-cache".into(), act: "chg", what: vec!["image"], image: ("postgres:15".into(), "postgres:16".into()) }]);
        let mut cards = json!([service_view("redis-cache", &model["services"]["redis-cache"]), service_view("admin_panel", &model["services"]["admin_panel"])]);
        hider.show_json(&mut steps);
        hider.show_json(&mut cards);
        assert_eq!(steps[0]["to"], "postgres:16");
        assert_eq!(steps[0]["n"], "redis-cache");
        assert_eq!(cards[0]["volumes"], json!(["redis-data:/data"]));
        assert_eq!(cards[1]["name"], "admin_panel");
        // A path and a container from what runs.
        let m = Member { service: "db".into(), id: "1".into(), name: "shop-db-1".into(), image: "postgres:16".into(), state: "running".into(), status: "Up".into(), created: 1 };
        let v = member_view(&m);
        assert_eq!((v["name"].as_str(), v["image"].as_str()), (Some("shop-db-1"), Some("postgres:16")));
        let mut path = json!({"dir": "/w/kiri-test", "files": ["/w/kiri-test/compose.yaml"]});
        hider.show_json(&mut path);
        assert_eq!(path["dir"], "/w/kiri-test");
        // Apply's lines: read as Compose wrote them; names and images whole.
        let mut images = images_of("shop", &model);
        let mut touched = Vec::new();
        assert!(matches!(up_line(" postgres:16 Pulling fs layer", "shop", &mut images, &mut |_| None, &hider, &mut touched), UpLine::Line(ref l) if l == " postgres:16 Pulling fs layer"));
        assert!(matches!(up_line(" Image postgres:16 Pulling ", "shop", &mut images, &mut |_| None, &hider, &mut touched), UpLine::State(ref s, "pulling") if s == &["db"]));
        for e in [
            "Error response from daemon: driver failed programming external connectivity on endpoint shop-redis-cache-1: port is already allocated",
            "network shop_admin-net declared as external, but could not be found",
            "open /w/kiri-test/redis.conf: no such file or directory",
            "pull access denied for x/admin, repository does not exist",
            "volume shop_redis-data: in use by kiri.redis-cache",
        ] {
            assert_eq!(hider.hide_up(e), e);
            assert_eq!(shown_error(&hider, e), e);
        }
        // The secret on its own is still its variable.
        assert_eq!(hider.hide("password authentication failed: got test, want redis"), "password authentication failed: got ${API_PASSWORD}, want redis");
        // A secret that only shares letters with a shown name is hidden whole, the name left.
        let h = Hider::new(vec![("${A_TOKEN}".into(), "hunter2db".into())], vec!["db".into(), "web".into()]);
        assert_eq!(h.hide("auth failed: hunter2db for db"), "auth failed: ${A_TOKEN} for db");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn splits_log_lines() {
        let v = log_line("web-1  | 2026-11-16T13:02:01.512Z listening on :3000");
        assert_eq!((v["svc"].as_str(), v["t"].as_str(), v["text"].as_str()), (Some("web-1"), Some("2026-11-16T13:02:01.512Z"), Some("listening on :3000")));
    }

    #[test]
    fn saves_in_place_keeping_line_endings() {
        let dir = std::env::temp_dir().join(format!("kiri-compose-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("compose.yaml");
        std::fs::write(&f, "services:\r\n  a:\r\n").unwrap();
        save(&f, "services:\n  b:\n", &file_hash(b"services:\r\n  a:\r\n")).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "services:\r\n  b:\r\n");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tstore() -> Option<PathBuf> {
        Some(std::env::temp_dir().join(format!("kiri-compose-opened-{}", std::process::id())).join("compose.json"))
    }

    #[test]
    fn remembers_opened_files_and_forgets_them() {
        let _ = std::fs::remove_dir_all(tstore().unwrap().parent().unwrap());
        assert!(opened(tstore).is_empty());
        save_opened(tstore, |_, _| true, Some(("shop", "/w/shop/compose.yaml"))).unwrap();
        save_opened(tstore, |_, _| true, Some(("blog", "/w/blog/docker-compose.yml"))).unwrap();
        let o = opened(tstore);
        assert_eq!(o.iter().map(|p| (p.name.as_str(), p.dir.as_str())).collect::<Vec<_>>(), [("shop", "/w/shop"), ("blog", "/w/blog")]);
        assert!(o.iter().all(|p| p.members.is_empty()));
        save_opened(tstore, |n, _| n != "shop", None).unwrap();
        assert_eq!(opened(tstore).iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["blog"]);
        let _ = std::fs::remove_dir_all(tstore().unwrap().parent().unwrap());
    }

    #[test]
    fn an_opened_project_that_has_run_shows_once() {
        let ran = projects(&json!([c("shop", "web", 1, "/w/shop/compose.yaml")]));
        let opened = vec![
            Project { name: "shop".into(), dir: "/w/shop".into(), files: vec!["/w/shop/compose.yaml".into()], members: Vec::new(), env_files: Vec::new() },
            Project { name: "blog".into(), dir: "/w/blog".into(), files: vec!["/w/blog/compose.yaml".into()], members: Vec::new(), env_files: Vec::new() },
        ];
        let all = merge(ran, opened);
        assert_eq!(all.iter().map(|p| (p.name.as_str(), p.members.len())).collect::<Vec<_>>(), [("blog", 0), ("shop", 1)]);
    }

    #[test]
    fn opens_yaml_files_only_and_names_them_as_compose_does() {
        for f in ["compose.yaml", "docker-compose.yml", "stack.YML", "dev.yaml"] {
            assert!(compose_like(Path::new(f)), "{f}");
        }
        for f in ["compose.json", "Dockerfile", ".env"] {
            assert!(!compose_like(Path::new(f)), "{f}");
        }
        assert_eq!(folder_name(Path::new("/w/My Shop.v2")), "myshopv2");
        assert_eq!(folder_name(Path::new("/w/_api-1")), "api-1");
        assert_eq!(folder_name(Path::new("/")), "project");
        let p = Project { name: String::new(), dir: "/w".into(), files: vec!["/w/compose.yaml".into()], members: Vec::new(), env_files: Vec::new() };
        assert_eq!(project_args(&p, None), ["compose", "--ansi", "never", "--project-directory", "/w", "-f", "/w/compose.yaml"]);
    }

    fn tdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kiri-compose-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn reads_and_writes_only_real_compose_files_the_labels_name() {
        let d = tdir("valid");
        let f = d.join("compose.yaml");
        std::fs::write(&f, "services: {}\n").unwrap();
        std::fs::write(d.join("secret.txt"), "x").unwrap();
        std::fs::create_dir_all(d.join("dir.yaml")).unwrap();
        assert_eq!(valid_file(&f.to_string_lossy()), Some(f.clone()));
        assert_eq!(valid_file("compose.yaml"), None, "relative");
        assert_eq!(valid_file(&d.join("secret.txt").to_string_lossy()), None, "not yaml");
        assert_eq!(valid_file(&d.join("dir.yaml").to_string_lossy()), None, "a folder");
        assert_eq!(valid_file(&d.join("gone.yaml").to_string_lossy()), None, "missing");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join("secret.txt"), d.join("evil.yaml")).unwrap();
            assert_eq!(valid_file(&d.join("evil.yaml").to_string_lossy()), None, "a .yaml link to another kind of file");
            std::os::unix::fs::symlink(&f, d.join("link.yml")).unwrap();
            assert_eq!(valid_file(&d.join("link.yml").to_string_lossy()), Some(f.clone()), "a link to a compose file: its target");
        }
        let p = Project { name: "x".into(), dir: "relative".into(), files: vec![f.to_string_lossy().into_owned()], members: Vec::new(), env_files: Vec::new() };
        assert_eq!(checked(p.clone(), tstore).unwrap().dir, d.to_string_lossy());
        assert_eq!(checked(Project { files: vec!["/etc/passwd".into()], ..p }, tstore).unwrap_err(), NO_FILE);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn saves_through_a_link_keeping_the_files_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = tdir("link");
        let real = d.join("real.yaml");
        std::fs::write(&real, "services: {}\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = d.join("compose.yaml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        save(&link, "services:\n  a: {}\n", &file_hash(b"services: {}\n")).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link stays a link");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "services:\n  a: {}\n", "its target is written");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600, "and keeps its mode");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2, "nothing left beside it");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn errors_name_their_line_or_their_services_line() {
        let dup = "failed to parse -: yaml: construct errors: line 1: line 5: mapping key \"image\" already defined at line 3";
        assert_eq!(error_line(dup), Some(5));
        assert_eq!(error_text(dup), "line 5: mapping key \"image\" already defined at line 3");
        let scan = "go-yaml load error in scanner (while scanning a quoted scalar) at L3.C12-L4.C1: found unexpected end of stream";
        assert_eq!((error_line(scan), error_text(scan).as_str()), (Some(4), "(while scanning a quoted scalar) at L3.C12-L4.C1: found unexpected end of stream"));
        assert_eq!(error_text("failed to parse /w/compose.yaml: yaml: line 2: bad"), "line 2: bad");
        let draft = "name: x\nservices:\n  web:\n    image: nginx\n  api:\n    image: x\n    bogus: 1\n";
        assert_eq!(error_json("validating -: services.api additional properties 'bogus' not allowed", draft, &[], &Hider::default())["line"], 5);
        assert_eq!(error_json("service \"web\" depends on undefined service \"nope\": invalid compose project", draft, &[], &Hider::default())["line"], 3);
        assert_eq!(error_json("invalid containerPort: 3x", draft, &[], &Hider::default())["line"], Value::Null);
    }

    #[test]
    fn the_tab_error_doesnt_say_its_line_twice() {
        let dup = "failed to parse /w/c.yaml: yaml: construct errors: line 1: line 5: mapping key \"image\" already defined at line 4";
        let e = error_json(dup, "", &[], &Hider::default());
        assert_eq!((e["line"].clone(), e["text"].clone()), (json!(5), json!("mapping key \"image\" already defined at line 4")));
        let tab = "go-yaml load error in scanner (while scanning for the next token) at L4.C1: found character that cannot start any token";
        let e = error_json(tab, "", &[], &Hider::default());
        assert_eq!((e["line"].clone(), e["text"].clone()), (json!(4), json!("found character that cannot start any token")));
        // With no line to show, the text keeps its own.
        assert_eq!(error_json("line x: odd", "", &[], &Hider::default())["text"], "line x: odd");
    }

    #[test]
    fn a_bad_port_points_at_its_line() {
        let none = Hider::default();
        let draft = "name: x\nservices:\n  web:\n    image: nginx\n    ports:\n      - \"8080:80\"\n      - \"abc:80\"\n  api:\n    image: abc\n    ports: [\"80:xyz\"]\n";
        assert_eq!(error_json("invalid hostPort: abc", draft, &[], &none)["line"], 7);
        assert_eq!(error_json("invalid containerPort: xyz", draft, &[], &none)["line"], 10);
        // From the .env: the line that names the variable.
        let env = "name: x\nservices:\n  web:\n    image: nginx\n    ports:\n      - \"${WEB_PORT}:80\"\n";
        let vars = vec![("WEB_PORT".to_owned(), "xyz".to_owned())];
        assert_eq!(error_json("invalid hostPort: xyz", env, &vars, &none)["line"], 6);
        // A secret where a port belongs: the line still, the value never.
        let vars = vec![("DB_PASSWORD".to_owned(), "hunter2-xyz".to_owned())];
        let env = env.replace("WEB_PORT", "DB_PASSWORD");
        let e = error_json("invalid hostPort: hunter2-xyz", &env, &vars, &Hider::new(vec![("${DB_PASSWORD}".into(), "hunter2-xyz".into())], vec!["nginx".into(), "web".into()]));
        assert_eq!((e["line"].clone(), e["text"].clone()), (json!(6), json!("invalid hostPort: ${DB_PASSWORD}")));
        // Said back as its stand-in: the line, and the variable.
        let st = stand_ins(&vars);
        let vars = vec![("DB_PASSWORD".to_owned(), st[0].value.clone())];
        let e = error_json(format!("invalid hostPort: {}", st[0].token), &env, &vars, &Hider::with(vec![], &st, vec![]));
        assert_eq!((e["line"].clone(), e["text"].clone()), (json!(6), json!("invalid hostPort: ${DB_PASSWORD}")));
    }

    #[test]
    fn env_values_never_reach_the_page_in_an_error() {
        let d = tdir("env");
        std::fs::write(d.join(".env"), "# x\nREDIS_PASSWORD=SECRET-REDIS-77\nexport TOKEN=\"abc123\"\nEMPTY=\nSESSION=Zx8kQ2pLm4Nv7Rt1Yw\nDATABASE_URL=postgres://app:pw-1234@db/app\n").unwrap();
        let hider = Hider::new(secrets(dcli, &dproj(&d)), vec![]);
        let e = error_json("invalid containerPort: SECRET-REDIS-77 (abc123)", "", &[], &hider);
        assert_eq!(e["text"], "invalid containerPort: ${REDIS_PASSWORD} (${TOKEN})");
        // A URL's password, from the .env or anywhere in the text.
        assert_eq!(hider.hide("auth failed for pw-1234; postgres://app:pw-1234@db/app"), "auth failed for ${DATABASE_URL password}; postgres://app:${DATABASE_URL password}@db/app");
        assert_eq!(Hider::default().hide("dial redis://:s3cr3t@cache:6379 and https://u:p@x.example/y"), "dial redis://:…@cache:6379 and https://u:…@x.example/y");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn plain_env_values_stay_so_apply_reads_its_progress() {
        let d = tdir("plain");
        std::fs::write(d.join(".env"), "COMPOSE_PROJECT_NAME=myapp\nDEBUG=true\nPORT=8080\nHOST=db.internal.example\nIMAGE=postgres:16-alpine3.20\nWEB_NAME=myapp\nAUTH_SERVICE=auth\n").unwrap();
        assert_eq!(secrets(dcli, &dproj(&d)).len(), 0, "a plain value was taken for a secret");
        let p = Project { name: "myapp".into(), dir: d.to_string_lossy().into(), files: vec![], members: vec![], env_files: Vec::new() };
        let model = json!({"services": {"web": {"image": "nginx"}}});
        let hider = Hider::for_project(dcli, &p, Some(&model));
        let mut images = images_of("myapp", &model);
        let mut touched = Vec::new();
        assert!(matches!(up_line(" Container myapp-web-1 Started ", "myapp", &mut images, &mut |_| None, &hider, &mut touched), UpLine::State(ref s, "started") if s == &["web"]));
        assert!(matches!(up_line("Bind for 0.0.0.0:8080 failed: port is already allocated", "myapp", &mut images, &mut |_| None, &hider, &mut touched), UpLine::Line(ref l) if l.contains("0.0.0.0:8080")));
        assert_eq!(touched, ["web"]);
        // So the watch after Apply sees web die, and names it.
        let mut c = c("myapp", "web", 5, "/w/myapp/compose.yaml");
        c["State"] = "exited".into();
        c["Status"] = "Exited (1) 1 second ago".into();
        let f = watch_with(|| Some(json!([c.clone()])), "myapp", &touched, std::time::Duration::ZERO, std::time::Duration::ZERO);
        assert_eq!(f.iter().map(|f| f["svc"].clone()).collect::<Vec<_>>(), [json!("web")]);
        // A secret that is the project's name: the name stays where Kiri shows it.
        let hide = Hider::new(vec![("${APP_TOKEN}".into(), "myapp".into())], vec!["web".to_owned()]);
        assert!(matches!(up_line(" Container myapp-web-1 Recreate ", "myapp", &mut images, &mut |_| None, &hide, &mut touched), UpLine::State(_, "recreating")));
        // A secret that is a service's name: the name is never hidden.
        let web = Hider::new(vec![("${WEB_PASSWORD}".into(), "web1".into())], vec!["web1".to_owned()]);
        assert_eq!(web.hide("service \"web1\" depends on undefined service \"nope\""), "service \"web1\" depends on undefined service \"nope\"");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_save_never_writes_over_an_edit_made_while_compose_read_the_draft() {
        let d = tdir("gap");
        let f = d.join("compose.yaml");
        std::fs::write(&f, "services: {}\n").unwrap();
        let base = file_hash(b"services: {}\n");
        fn outside(p: &Path) {
            std::fs::write(p, "services: {}\n# theirs\n").unwrap();
        }
        *BETWEEN.lock().unwrap() = Some((std::fs::canonicalize(&f).unwrap(), outside));
        let r = save(&f, "services:\n  a: {}\n", &base);
        *BETWEEN.lock().unwrap() = None;
        assert!(r.as_ref().is_err_and(changed), "{r:?}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "services: {}\n# theirs\n", "their edit stays");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "nothing left beside it");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn discard_after_two_failed_applies_puts_back_the_file_before_both() {
        let (a, b, c) = (b"v1".to_vec(), b"v2".to_vec(), b"v3".to_vec());
        remember_before("twice", "/w/c.yaml", a.clone(), &file_hash(&b));
        remember_before("twice", "/w/c.yaml", b, &file_hash(&c));
        assert_eq!(BEFORE.lock().unwrap()["twice"], ("/w/c.yaml".to_owned(), a, file_hash(&c)));
        // Changed by hand since Kiri's save: that is what Discard keeps now.
        remember_before("twice", "/w/c.yaml", b"by hand".to_vec(), &file_hash(b"v4"));
        assert_eq!(BEFORE.lock().unwrap()["twice"].1, b"by hand");
        BEFORE.lock().unwrap().remove("twice");
    }

    #[test]
    fn a_file_put_right_by_hand_that_runs_as_saved_leaves_nothing_to_apply() {
        let dir = std::env::temp_dir().join(format!("kiri-runs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("compose.yaml");
        let text = "services:\n  web:\n    image: nginx # fixed\n";
        std::fs::write(&path, text.replace('\n', "\r\n")).unwrap();
        let path = path.to_string_lossy().into_owned();
        let step = |act| Step { service: "web".into(), act, what: vec![], image: (String::new(), String::new()) };
        remember_before("byhand2", &path, b"services: {}\n".to_vec(), "w1");
        // Still differs from what runs, or isn't what's on disk: kept.
        assert!(!runs_as_saved("byhand2", &path, text, &[step("chg")]));
        assert!(!runs_as_saved("byhand2", &path, "services: {}\n", &[step("same")]));
        assert!(BEFORE.lock().unwrap().contains_key("byhand2"));
        // As on disk and as it runs: nothing to put back.
        assert!(runs_as_saved("byhand2", &path, text, &[step("same")]));
        assert!(!BEFORE.lock().unwrap().contains_key("byhand2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cancel_before_up_is_kept_for_it() {
        assert!(!cancel("never-applied"));
        UPS.lock().unwrap().insert("c1".into(), Up::default());
        let g = Applying("c1".into());
        assert!(cancel("c1"), "an Apply still reading its draft can be cancelled");
        assert!(cancelled("c1"));
        drop(g);
        assert!(!cancelled("c1") && !UPS.lock().unwrap().contains_key("c1"), "gone once Apply ends");
    }

    #[test]
    fn project_actions_go_by_name_in_composes_order_and_remove_its_network() {
        assert_eq!(action_args("shop", "start").unwrap(), ["compose", "--ansi", "never", "-p", "shop", "start"]);
        assert_eq!(action_args("shop", "remove").unwrap(), ["compose", "--ansi", "never", "-p", "shop", "down", "--remove-orphans"]);
        assert!(!action_args("shop", "remove").unwrap().contains(&"-v".to_owned()), "volumes stay");
        assert_eq!(action_args("shop", "rm -rf"), None);
    }

    #[test]
    fn logs_go_in_time_order_without_empty_lines() {
        let text = "web-1  | 2026-11-16T13:02:03.000Z c\nweb-1  | 2026-11-16T13:02:01.000Z a\ndb-1  | 2026-11-16T13:02:02.000Z b\ndb-1  | 2026-11-16T13:02:04.000Z \n";
        let l = log_lines(text);
        assert_eq!(l.iter().map(|x| x["text"].as_str().unwrap()).collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    #[test]
    fn the_file_hash_tells_one_content_from_another() {
        assert_eq!(file_hash(b"services: {}\n"), file_hash(b"services: {}\n"));
        assert_ne!(file_hash(b"services: {}\n"), file_hash(b"services: {}\r\n"));
        assert_eq!(file_hash(b"").len(), 16);
    }

    #[test]
    fn windows_canonical_paths_read_plainly() {
        assert_eq!(plain_path(PathBuf::from(r"\\?\C:\Users\me\shop\compose.yaml")), PathBuf::from(r"C:\Users\me\shop\compose.yaml"));
        assert_eq!(plain_path(PathBuf::from(r"\\?\UNC\server\share\shop\compose.yaml")), PathBuf::from(r"\\server\share\shop\compose.yaml"));
        // Anything else as it is: a long path with no drive, a Unix path.
        assert_eq!(plain_path(PathBuf::from(r"\\?\Volume{x}\a.yaml")), PathBuf::from(r"\\?\Volume{x}\a.yaml"));
        assert_eq!(plain_path(PathBuf::from("/w/shop/compose.yaml")), PathBuf::from("/w/shop/compose.yaml"));
        let a = project_args(&Project { name: "shop".into(), dir: plain_path(PathBuf::from(r"\\?\C:\w")).to_string_lossy().into(), files: vec![plain_path(PathBuf::from(r"\\?\C:\w\compose.yaml")).to_string_lossy().into()], members: vec![], env_files: Vec::new() }, None);
        assert!(!a.iter().any(|x| x.starts_with(r"\\?\")), "{a:?}");
    }

    #[test]
    fn only_compose_files_open() {
        assert!(compose_text("name: x\nservices:\n  web:\n    image: nginx\n"));
        assert!(compose_text("include:\n  - other.yaml\n"));
        assert!(compose_text("\"services\": {}\n"));
        // Indented throughout, quoted, or one flow mapping: Compose takes them all.
        assert!(compose_text("  name: x\n  services:\n    x:\n      image: alpine:3.20\n"));
        assert!(compose_text("{name: \"x\", services: {x: {image: \"alpine:3.20\"}}}\n"));
        assert!(compose_text("{\n  \"include\": [\"a.yaml\"]\n}\n"));
        assert!(compose_text("# a comment\n---\n'services':\n  x: {}\n"));
        assert!(compose_text("\t\tservices:\n\t\t\tx: {}\n"));
        // Neither key at the top: refused.
        assert!(!compose_text("{apiVersion: v1, kind: Service, spec: {services: 1}}\n"));
        assert!(!compose_text("  apiVersion: v1\n  kind: List\n  items:\n    - services: []\n"));
        assert!(!compose_text("fruit:\n  - apple\ncolours: {services: no}\n"));
        assert!(!compose_text("x:\n  services:\n    a: 1\n"));
        assert!(!compose_text("{a: \"services: 1\", b: [include]}\n"));
        assert!(!compose_text(""));
        assert!(!compose_text("apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  services: 1\n"));
        let d = tdir("k8s");
        std::fs::write(d.join("deploy.yaml"), "apiVersion: v1\nkind: Service\n").unwrap();
        let o: Value = serde_json::from_str(&open_file(|| Err(io::Error::other("no CLI")), tstore, &d.join("deploy.yaml"))).unwrap();
        assert_eq!((o["error"].as_str(), o["errkey"].as_str()), (Some(NOT_COMPOSE), Some("notComposeFile")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_name_another_folder_runs_is_never_mixed() {
        let (a, b) = (tdir("clash-a"), tdir("clash-b"));
        for d in [&a, &b] {
            std::fs::write(d.join("compose.yaml"), "name: shop\nservices: {}\n").unwrap();
        }
        let proj = |d: &Path, members: usize| Project {
            name: "shop".into(),
            dir: d.to_string_lossy().into(),
            files: vec![d.join("compose.yaml").to_string_lossy().into()],
            members: (0..members).map(|i| Member { service: "web".into(), id: i.to_string(), name: String::new(), image: String::new(), state: "running".into(), status: String::new(), created: 0 }).collect(),
            env_files: Vec::new(),
        };
        let running = [proj(&a, 1)];
        assert_eq!(clash(&proj(&b, 0), &running), Some(a.to_string_lossy().into_owned()));
        assert_eq!(clash(&proj(&a, 0), &running), None, "its own file opens");
        assert_eq!(clash(&proj(&b, 0), &[]), None);
        // An opened file that is gone since: its name is free again.
        std::fs::remove_file(a.join("compose.yaml")).unwrap();
        assert_eq!(clash(&proj(&b, 0), &[proj(&a, 0)]), None);
        assert_eq!(error_key(&format!("A project named shop already runs from {}", a.display())).unwrap().0, "nameClash");
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[test]
    fn kiris_own_errors_carry_a_key_for_the_page() {
        let k = error_key;
        assert_eq!(k("The file changed outside Kiri since Apply saved it"), Some(("changedSinceApply", json!({}))));
        assert_eq!(k("The file changed outside Kiri"), Some(("changed", json!({}))));
        assert_eq!(k("Kiri has nothing to put back"), Some(("nothingToRestore", json!({}))));
        assert_eq!(k("pick a .yaml or .yml compose file"), Some(("notCompose", json!({}))));
        assert_eq!(k("no Compose project shop on Kiri"), Some(("noProject", json!({"name": "shop"}))));
        assert_eq!(k("shop is already being applied"), Some(("busy", json!({"name": "shop"}))));
        assert_eq!(k("docker compose took longer than 30 seconds"), Some(("slow", json!({"secs": "30"}))));
        assert_eq!(k(READ_ONLY), Some(("readOnly", json!({}))));
        assert_eq!(k(&bad_env("/w/shop/.env")), Some(("badEnv", json!({"file": "/w/shop/.env"}))));
        // Compose's own words go as they are.
        assert_eq!(k("services.web additional properties 'x' not allowed"), None);
        let ev: Value = serde_json::from_str(&own_event(json!({"type": "composerestored", "name": "shop"}), "Kiri has nothing to put back")).unwrap();
        assert_eq!(ev["errkey"], "nothingToRestore");
        // What a process said is never keyed, whatever it says.
        for said in ["shop is already being applied", "Apply cancelled", "no Compose project x on Kiri", "docker compose took longer than 3 seconds", BY_NAME] {
            let mut ev = json!({"type": "composeaction"});
            Fault::Said(said.into()).put(&mut ev);
            assert!(ev["errkey"].is_null() && ev["error"] == said, "{ev}");
            assert!(CfgErr::from(said.to_owned()).key.is_none());
        }
    }

    #[test]
    fn a_second_apply_of_a_project_is_refused_while_one_runs() {
        static OUT: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn em(s: String) {
            OUT.lock().unwrap().push(s);
        }
        UPS.lock().unwrap().insert("busyproj".into(), Up::default());
        apply(|| Err(io::Error::other("must not run")), tstore, "busyproj", "services: {}\n", Some("x"), em);
        let ev: Value = serde_json::from_str(&OUT.lock().unwrap()[0]).unwrap();
        assert_eq!((ev["busy"].clone(), ev["ok"].clone(), ev["errkey"].clone()), (json!(true), json!(false), json!("busy")));
        assert!(UPS.lock().unwrap().contains_key("busyproj"), "the first one's Apply stays on the list");
        UPS.lock().unwrap().remove("busyproj");
    }

    #[test]
    fn one_reading_of_a_file_at_a_time() {
        #[cfg(unix)]
        {
            let d = tdir("flight");
            std::fs::write(d.join("compose.yaml"), "services: {}\n").unwrap();
            fn count() -> PathBuf {
                std::env::temp_dir().join(format!("kiri-flight-count-{}", std::process::id()))
            }
            fn slow() -> io::Result<Command> {
                let mut c = Command::new("sh");
                c.args(["-c", &format!("echo x >> '{}'; sleep 0.4; echo '{{\"services\":{{}}}}'", count().display()), "sh"]);
                Ok(c)
            }
            let _ = std::fs::remove_file(count());
            let p = Project { name: "flight".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], members: vec![], env_files: Vec::new() };
            let ts: Vec<_> = (0..4).map(|_| { let p = p.clone(); std::thread::spawn(move || saved_config(slow, &p, &[]).is_ok()) }).collect();
            assert!(ts.into_iter().all(|t| t.join().unwrap()));
            // The values it reads with (their stand-ins) and the names its
            // files use, once too.
            assert_eq!(std::fs::read_to_string(count()).unwrap().lines().count(), 3, "one config for four askers");
            let _ = (std::fs::remove_file(count()), std::fs::remove_dir_all(&d));
        }
    }

    #[test]
    fn a_read_only_file_is_never_written() {
        let d = tdir("ro");
        let f = d.join("compose.yaml");
        std::fs::write(&f, "services: {}\n").unwrap();
        let mut perm = std::fs::metadata(&f).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&f, perm.clone()).unwrap();
        let e = save(&f, "services:\n  a: {}\n", &file_hash(b"services: {}\n")).unwrap_err();
        assert_eq!(e.to_string(), READ_ONLY);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "services: {}\n");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "nothing left beside it");
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(&f, perm).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn short_cli_commands_time_out() {
        #[cfg(unix)]
        {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 5"]);
            let t = std::time::Instant::now();
            assert!(run_cli(c, None, 1).unwrap_err().contains("longer than 1 seconds"));
            assert!(t.elapsed() < std::time::Duration::from_secs(3));
            let mut c = Command::new("sh");
            c.args(["-c", "cat; echo err >&2; exit 1"]);
            assert_eq!(run_cli(c, Some("in"), 5).unwrap_err(), "err");
        }
    }

    #[test]
    fn names_a_service_that_died_after_apply() {
        let m = |svc: &str, state: &str, status: &str, created: i64| {
            let mut v = c("shop", svc, created, "/w/shop/compose.yaml");
            v["State"] = state.into();
            v["Status"] = status.into();
            v["Id"] = format!("{svc}{created}").into();
            v
        };
        let list = json!([
            m("db", "exited", "Exited (1) 2 seconds ago", 5),
            m("db", "running", "Up 3 hours", 1),
            m("job", "exited", "Exited (0) 1 second ago", 5),
            m("web", "restarting", "Restarting (2) 1 second ago", 5),
            m("api", "running", "Up Less than a second", 5),
            m("cache", "exited", "Exited (137) 1 second ago", 5),
        ]);
        let touched: Vec<String> = ["db", "job", "web", "api"].map(String::from).to_vec();
        let f = failed("shop", &touched, &list);
        assert_eq!(f.iter().map(|x| (x["svc"].as_str().unwrap(), x["id"].as_str().unwrap())).collect::<Vec<_>>(), [("db", "db5"), ("web", "web5")]);
        assert_eq!(f[0]["status"], "Exited (1) 2 seconds ago");
    }

    #[test]
    fn every_service_that_dies_after_apply_is_named() {
        let m = |svc: &str, name: &str, state: &str, status: &str| {
            let mut v = c("shop", svc, 5, "/w/shop/compose.yaml");
            v["Names"] = json!([format!("/{name}")]);
            v["State"] = state.into();
            v["Status"] = status.into();
            v
        };
        // a dies at once, b a moment later; then postgres and a container_name job, a moment apart.
        let polls = [
            json!([m("a", "shop-a-1", "exited", "Exited (2) 1 second ago"), m("b", "shop-b-1", "running", "Up 1 second"), m("db", "shop-db-1", "running", "Up"), m("job", "nightly-job", "running", "Up")]),
            json!([m("a", "shop-a-1", "exited", "Exited (2) 2 seconds ago"), m("b", "shop-b-1", "running", "Up 2 seconds"), m("db", "shop-db-1", "restarting", "Restarting (1) 1 second ago"), m("job", "nightly-job", "running", "Up")]),
            json!([m("a", "shop-a-1", "exited", "Exited (2) 3 seconds ago"), m("b", "shop-b-1", "exited", "Exited (4) Less than a second ago"), m("db", "shop-db-1", "restarting", "Restarting (1) 1 second ago"), m("job", "nightly-job", "exited", "Exited (1) Less than a second ago")]),
            json!([m("a", "shop-a-1", "exited", "Exited (2) 4 seconds ago"), m("b", "shop-b-1", "exited", "Exited (4) 1 second ago"), m("db", "shop-db-1", "restarting", "Restarting (1) 2 seconds ago"), m("job", "nightly-job", "exited", "Exited (1) 1 second ago")]),
        ];
        let mut i = 0;
        let touched: Vec<String> = ["a", "b", "db", "job"].map(String::from).to_vec();
        let f = watch_with(
            || {
                i += 1;
                polls.get(i - 1).cloned()
            },
            "shop",
            &touched,
            std::time::Duration::from_millis(60),
            std::time::Duration::from_millis(20),
        );
        assert_eq!(f.iter().map(|f| f["svc"].as_str().unwrap()).collect::<Vec<_>>(), ["a", "db", "b", "job"], "{f:?}");
        assert!(i >= 3, "watched past the first failure");
    }

    #[test]
    fn a_file_with_a_byte_order_mark_opens_and_keeps_it() {
        assert!(compose_text("\u{feff}services:\n  web:\n    image: nginx\n"));
        let d = tdir("bom");
        let f = d.join("compose.yaml");
        let first = b"\xef\xbb\xbfservices:\r\n  web:\r\n    image: nginx\r\n";
        std::fs::write(&f, first).unwrap();
        save(&f, "services:\n  web:\n    image: nginx:1\n", &file_hash(first)).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"\xef\xbb\xbfservices:\r\n  web:\r\n    image: nginx:1\r\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_this_user_cant_write_is_never_written() {
        // Root writes any file: the check is this user's.
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt as _;
            let d = tdir("ro-owner");
            let f = d.join("compose.yaml");
            std::fs::write(&f, "services: {}\n").unwrap();
            // Writable by its group, not by its owner (this user): not "read-only" by its bits.
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o464)).unwrap();
            assert!(!std::fs::metadata(&f).unwrap().permissions().readonly());
            let e = save(&f, "services:\n  a: {}\n", &file_hash(b"services: {}\n")).unwrap_err();
            assert_eq!(e.to_string(), READ_ONLY);
            assert_eq!(std::fs::read_to_string(&f).unwrap(), "services: {}\n");
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn a_label_never_names_a_network_path() {
        for f in [r"\\host\share\compose.yaml", "//host/share/compose.yaml", r"\\?\UNC\host\share\compose.yaml", r"\\.\pipe\x.yaml"] {
            assert!(remote_path(f), "{f}");
            assert_eq!(valid_file(f), None, "{f}");
        }
        for f in [r"C:\w\compose.yaml", r"\\?\C:\w\compose.yaml", "/w/compose.yaml"] {
            assert!(!remote_path(f), "{f}");
        }
    }

    #[test]
    fn project_logs_are_labelled_by_service() {
        let mut db = c("shop", "db", 5, "/w/shop/compose.yaml");
        db["Names"] = json!(["/custom-db"]);
        let p = projects(&json!([db, c("shop", "web", 5, "/w/shop/compose.yaml")])).remove(0);
        let mut lines = log_lines("custom-db  | 2026-11-16T13:02:01.5Z ready\nweb-1  | 2026-11-16T13:02:02.5Z up\n");
        label_by_service(&mut lines, &p);
        assert_eq!(lines.iter().map(|l| l["svc"].as_str().unwrap()).collect::<Vec<_>>(), ["db", "web"]);
    }

    #[test]
    fn kiris_list_and_plan_errors_carry_a_key_too() {
        assert_eq!(error_key("no docker CLI found"), Some(("noCli", json!({}))));
        assert_eq!(error_key("no such action"), Some(("noAction", json!({}))));
        let e = error_json(CfgErr::own("docker compose took longer than 30 seconds"), "services: {}\n", &[], &Hider::default());
        assert_eq!((e["errkey"].clone(), e["errargs"].clone()), (json!("slow"), json!({"secs": "30"})));
        assert!(error_json("services.web additional properties 'x' not allowed", "", &[], &Hider::default())["errkey"].is_null());
        // Compose's words are never keyed, even when they read as Kiri's.
        for said in ["docker compose took longer than 30 seconds", "pw-x-1 is already being applied", "Compose rejects this file: YAML error on line 3"] {
            assert!(error_json(said, "", &[], &Hider::default())["errkey"].is_null(), "{said}");
        }
    }

    #[test]
    fn secrets_are_read_as_compose_reads_the_env() {
        let d = tdir("dotenv");
        std::fs::write(
            d.join(".env"),
            "BASE=base1\nCMT_PASSWORD=Cmt-x-77ff # prod\nQC_PASSWORD=\"Quo-x-1a\" # note\nVAR_PASSWORD=${BASE}-Var-x-2b\nDQ_PASSWORD=\"Dq-x\\$3c\"\nYAML_PASSWORD: Yml-x-8h\nexport EXP_TOKEN=Exp-x-9k\nBS_PASSWORD=\"Bs\\\\x\\\"q-4d\"\nML_SECRET=\"Ml-x-line1\nMl-x-line2\"\n",
        )
        .unwrap();
        let s: Vec<String> = secrets(dcli, &dproj(&d)).into_iter().map(|(_, v)| v).collect();
        for v in ["Cmt-x-77ff", "Quo-x-1a", "base1-Var-x-2b", "Dq-x$3c", "Yml-x-8h", "Exp-x-9k", "Bs\\x\"q-4d", "Ml-x-line1", "Ml-x-line2"] {
            assert!(s.iter().any(|x| x == v), "{v} is read as Compose reads it");
        }
        let h = Hider::new(secrets(dcli, &dproj(&d)), vec![]);
        assert_eq!(h.hide("service a refers to undefined container Cmt-x-77ff: invalid compose project"), "service a refers to undefined container ${CMT_PASSWORD}: invalid compose project");
        // What `up` quotes back cut off: the line says its kind alone.
        assert_eq!(h.hide_up("strconv.ParseFloat: parsing \"Cmt-x-77\": invalid syntax"), "strconv.ParseFloat: parsing");
        assert_eq!(h.hide("Cmt-x and Cmt"), "Cmt-x and Cmt");
        assert_eq!(h.hide("x Quo-x-1a base1-Var-x-2b Dq-x$3c Yml-x-8h Exp-x-9k"), "x ${QC_PASSWORD} ${VAR_PASSWORD} ${DQ_PASSWORD} ${YAML_PASSWORD} ${EXP_TOKEN}");
        // A value with `\` or `"`, as it is and as an error quotes it back.
        assert_eq!(h.hide("bad Bs\\x\"q-4d and \"Bs\\\\x\\\"q-4d\""), "bad ${BS_PASSWORD} and \"${BS_PASSWORD}\"");
        let e = error_json("invalid hostPort: Cmt-x-77ff", "", &[], &h);
        assert_eq!(e["text"], "invalid hostPort: ${CMT_PASSWORD}");
        // The env file the project came up with, in place of the folder's `.env`.
        let other = d.join("prod.env");
        std::fs::write(&other, "CMT_PASSWORD=Prod-x-5e # x\n").unwrap();
        let p = Project { env_files: vec![other.to_string_lossy().into_owned()], ..dproj(&d) };
        let s: Vec<String> = secrets(dcli, &p).into_iter().map(|(_, v)| v).collect();
        assert!(s.contains(&"Prod-x-5e".to_owned()) && !s.contains(&"Cmt-x-77ff".to_owned()), "the env file's values, not the .env's");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_or_env_file_label_never_names_a_network_path() {
        let d = tdir("netdir");
        let f = d.join("compose.yaml");
        std::fs::write(&f, "services: {}\n").unwrap();
        let p = |dir: &str| Project { name: "x".into(), dir: dir.into(), files: vec![f.to_string_lossy().into_owned()], ..Project::default() };
        for dir in [r"\\host\share", "//host/share", r"\\?\UNC\host\share", r"\\.\pipe\x", "/net/host/share"] {
            assert!(remote_path(dir), "{dir}");
            // Never looked in: the file's own folder in its place.
            assert_eq!(checked(p(dir), tstore).unwrap().dir, d.to_string_lossy(), "{dir}");
            assert_eq!(valid_dir(dir, false), None, "{dir}");
            assert_eq!(valid_env_file(&format!("{dir}/.env")), None, "{dir}");
        }
        assert_eq!(valid_file("/net/host/share/compose.yaml"), None);
        // A local folder and env file stay; a network env file is refused, as
        // Compose refuses one it can't read, never left out to apply with blanks.
        let env = d.join("a.env");
        std::fs::write(&env, "A=1\n").unwrap();
        let mut q = p(&d.to_string_lossy());
        q.env_files = vec![env.to_string_lossy().into_owned()];
        let c = checked(q.clone(), tstore).unwrap();
        assert_eq!(c.dir, d.to_string_lossy());
        assert_eq!(c.env_files.len(), 1);
        assert!(project_args(&c, None).contains(&"--env-file".to_owned()));
        for bad in ["//host/share/b.env", "/net/host/c.env", "/usr/../net/h/c.env"] {
            q.env_files = vec![env.to_string_lossy().into_owned(), bad.into()];
            assert_eq!(checked(q.clone(), tstore).unwrap_err(), bad_env(bad), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_secret_at_the_end_of_a_sentence_is_hidden() {
        let h = Hider::new(vec![("${REDIS_PASSWORD}".into(), "r3dis".into())], vec![]);
        assert_eq!(h.hide("wrong password r3dis."), "wrong password ${REDIS_PASSWORD}.");
        assert_eq!(h.hide("wrong password r3dis. Try again"), "wrong password ${REDIS_PASSWORD}. Try again");
        assert_eq!(h.hide("r3disx xr3dis r3dis_b"), "r3disx xr3dis r3dis_b");
    }

    /// Secret shapes, as Compose reads them from the `.env`.
    const SHAPES: [(&str, &str); 16] = [
        ("CMT_PASSWORD", "Cmt-x-77ff"),
        ("QC_PASSWORD", "Quo-x-1a"),
        ("VAR_PASSWORD", "zzbase-Var-x-2b"),
        ("DQ_PASSWORD", "Dq-x$3c"),
        ("YAML_PASSWORD", "Yml-x-8h"),
        ("EXP_SECRET", "Exp-x-88gg"),
        ("EXQ_TOKEN", "Exq-x-9i"),
        ("ML_PRIVATE_KEY", "-----BEGIN KEY-----\nMlq-x-line1AAA\nMlq-x-line2BBB\n-----END KEY-----"),
        ("BS_PASSWORD", "Bs-x\\yy\"4dd4"),
        ("BSD_PASSWORD", "Bsd-x\\zz\"5ee5"),
        ("SQ_PASSWORD", "Sq1-x'7vv7"),
        ("COLON_TOKEN", "Col1-x:9zq7:Pw8k"),
        ("SL_ACCESS_KEY", "wJalrXUt/K7MDENG+bPx/Zk9Q"),
        ("REDIS_PASSWORD", "r3dis"),
        ("DATABASE_URL", "postgres://app:UrlPw-x-55dd@db:5432/shop"),
        ("EF_PASSWORD", "Envf-x-6f"),
    ];
    /// The `.env` that gives [`SHAPES`], each written as people write them.
    const DOTENV: &str = "BASE=zzbase\nCMT_PASSWORD=Cmt-x-77ff # prod\nQC_PASSWORD=\"Quo-x-1a\" # note\nVAR_PASSWORD=${BASE}-Var-x-2b\nDQ_PASSWORD=\"Dq-x\\$3c\"\nYAML_PASSWORD: Yml-x-8h\nexport EXP_SECRET=Exp-x-88gg\nEXQ_TOKEN=Exq-x-9i\nML_PRIVATE_KEY=\"-----BEGIN KEY-----\nMlq-x-line1AAA\nMlq-x-line2BBB\n-----END KEY-----\"\nBS_PASSWORD='Bs-x\\yy\"4dd4'\nBSD_PASSWORD='Bsd-x\\zz\"5ee5'\nSQ_PASSWORD=\"Sq1-x'7vv7\"\nCOLON_TOKEN=Col1-x:9zq7:Pw8k\nSL_ACCESS_KEY=wJalrXUt/K7MDENG+bPx/Zk9Q\nREDIS_PASSWORD=r3dis\nDATABASE_URL=postgres://app:UrlPw-x-55dd@db:5432/shop\nNOTE=-----BEGIN here\nWEB_PASSWORD=web1\nSECRET_KEY_BASE=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\nMYSQL_ROOT_PASSWORD=root\nT_PASSWORD=test\n";
    const MARKS: [&str; 33] = [
        "Cmt-x", "77ff", "Quo-x", "x-1a", "Var-x", "x-2b", "Dq-x", "$3c", "Yml-x", "x-8h", "Exp-x", "88gg", "Exq-x", "x-9i", "Mlq-x", "line1AAA", "line2BBB", "Bs-x", "4dd4", "Bsd-x", "5ee5", "Sq1-x", "7vv7", "Col1", "9zq7", "Pw8k", "wJalr", "k7mdeng", "K7MDENG", "zk9q", "r3dis", "UrlPw", "Envf-x",
    ];

    fn shapes() -> Hider {
        let vars = SHAPES.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        Hider::new(secret_values(vars), vec!["x".into(), "db".into()])
    }

    fn clean(tag: &str, text: &str) {
        let hit: Vec<&str> = MARKS.iter().filter(|m| text.contains(*m)).copied().collect();
        assert!(hit.is_empty(), "{tag}: {hit:?} shown in {text}");
    }

    #[test]
    fn every_form_up_prints_a_secret_in_is_its_variable() {
        let h = shapes();
        let mut said: Vec<(String, &str)> = Vec::new();
        for (k, v) in SHAPES.iter().filter(|(k, _)| *k != "REDIS_PASSWORD") {
            let v = if *k == "DATABASE_URL" { "UrlPw-x-55dd" } else { v };
            for f in forms(v).into_iter().chain(v.lines().filter(|l| *l != v).map(str::to_owned)) {
                said.push((format!("Error response from daemon: unable to find user {f}: no matching entries"), k));
                said.push((format!("invalid reference format: \"alpine:{f}\""), k));
                said.push((format!("kiri: cannot start container: user: unknown user {f}"), k));
            }
        }
        for (e, k) in &said {
            let shown = shown_error(&h, e);
            clean(&format!("apply {e}"), &shown);
            clean(&format!("line {e}"), &h.hide_up(e));
            assert!(shown.contains(&format!("${{{k}")), "{e} → {shown}");
        }
        assert_eq!(shown_error(&h, "pull access denied for alpine:Cmt-x-77ff"), "pull access denied for alpine:${CMT_PASSWORD}");
        // An older engine's line in Logs: the backslash doubled.
        for e in ["kiri: cannot start container: user: unknown user Bs-x\\\\yy\"4dd4", "kiri: cannot start container: user: unknown user Bs-x\\yy\"4dd4"] {
            let mut lines = vec![json!({"svc": "x", "t": "2026-11-16T13:02:01.900Z", "text": e})];
            engine_lines(&mut lines, &h);
            clean(e, &lines[0].to_string());
            assert!(lines[0]["text"].as_str().unwrap().ends_with("unknown user ${BS_PASSWORD}"), "{}", lines[0]);
        }
        assert_eq!(h.hide("dial postgres://app:UrlPw-x-55dd@db:5432/shop"), "dial postgres://app:${DATABASE_URL password}@db:5432/shop");
        // Cut off as Docker quotes a value back: the line says its kind.
        for e in ["invalid user \"Cmt-x-77\"", "parsing 'x-77ff': invalid", "Error response from daemon: bad tag \"MDENG+bPx/Zk9Q\""] {
            let shown = shown_error(&h, e);
            clean(e, &shown);
            assert!(!shown.contains('"') && !shown.contains('\''), "{e} → {shown}");
        }
        // What is no secret stays as it is.
        for e in ["service \"x\" depends on undefined service \"db2\": 'services[x].mem_limit'", "can't pull alpine:3.20 for x: r3", "Bs-x and \"Cmt-x\""] {
            assert_eq!(shown_error(&h, e), e);
        }
    }

    #[test]
    fn plain_words_are_never_taken_for_a_secret() {
        let hex = "0123456789abcdef".repeat(8);
        let h = Hider::new(secret_values(vec![("SECRET_KEY_BASE".into(), hex.clone()), ("MYSQL_ROOT_PASSWORD".into(), "root".into()), ("T_PASSWORD".into(), "test".into()), ("WEB_PASSWORD".into(), "web1".into())]), vec!["web1".into()]);
        for e in ["service \"db\" depends on \"30\": 'services[db].stop_grace_period'", "mount rootfs: denied", "pull nginx:latest: denied", "service \"web1\" depends on undefined service \"nope\"", "the attest went fine"] {
            assert_eq!(h.hide(e), e);
            assert_eq!(shown_error(&h, e), e);
        }
        assert_eq!(h.hide(&format!("bad key {hex}; user root; test")), "bad key ${SECRET_KEY_BASE}; user ${MYSQL_ROOT_PASSWORD}; ${T_PASSWORD}");
        // A long line with many secrets, in good time.
        let vars: Vec<(String, String)> = (0..50).map(|i| (format!("S{i}_TOKEN"), format!("Tok-{i}-x\"q\\${i}zz{i}-0123456789"))).collect();
        let h = Hider::new(secret_values(vars.clone()), vec![]);
        let line = json!({"errors": vars.iter().map(|(_, v)| format!("bad {v}")).collect::<Vec<_>>()}).to_string();
        assert!(line.len() > 1700, "{}", line.len());
        // The best of five runs: a loaded machine slows one, never all.
        let best = (0..5)
            .map(|_| {
                let t = std::time::Instant::now();
                let shown = h.hide_up(&line);
                (t.elapsed(), shown)
            })
            .min_by_key(|(t, _)| *t)
            .unwrap();
        let shown = best.1;
        eprintln!("a long line, 50 secrets: {:?}", best.0);
        assert!(best.0 < std::time::Duration::from_millis(50), "{:?}", best.0);
        assert!(!shown.contains("zz1") && shown.contains("${S49_TOKEN}"), "{shown}");
    }

    #[test]
    fn compose_reads_the_file_with_stand_ins_and_its_errors_name_the_variable() {
        let d = tdir("stand");
        std::fs::write(d.join(".env"), DOTENV).unwrap();
        let file = d.join("compose.yaml");
        let p = Project { name: "stand".into(), dir: d.to_string_lossy().into_owned(), files: vec![file.to_string_lossy().into_owned()], ..Project::default() };
        // The secret after a plain value that says `-----BEGIN ` is still read.
        let vars = env_vars(dcli, &p);
        assert!(vars.iter().any(|(k, v)| k == "WEB_PASSWORD" && v == "web1"), "{vars:?}");
        let st = stand_ins(&vars);
        for s in &st {
            assert!(s.token.len() >= 12 && s.token.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()), "{s:?}");
        }
        let svc = |body: &str| format!("services:\n  x:\n    image: alpine\n{body}");
        let mut drafts: Vec<(String, &str)> = Vec::new();
        for (k, _) in SHAPES {
            let v = if k == "DATABASE_URL" { "${DATABASE_URL}" } else { "${K}" };
            let v = v.replace('K', k);
            for (body, why) in [
                (format!("    pull_policy: {v}\n"), "pull_policy"),
                (format!("    mem_limit: \"{v}\"\n"), "mem_limit"),
                (format!("    depends_on: [\"{v}\"]\n"), "depends_on"),
                (format!("    ports: [\"{v}:80\"]\n"), "port"),
                (format!("    ports: [\"80:{v}\"]\n"), "port"),
                (format!("    ports: [\"{v}\"]\n"), "port"),
                (format!("    network_mode: \"{v}\"\n"), "network_mode"),
                (format!("    stop_grace_period: \"{v}\"\n"), "stop_grace_period"),
                (format!("    healthcheck:\n      test: [\"CMD\", \"true\"]\n      interval: \"{v}\"\n"), "healthcheck"),
                (format!("    extra_hosts: [\"{v}\"]\n"), "extra_hosts"),
                (format!("    env_file: \"{v}\"\n"), "env_file"),
                (format!("    environment:\n      A: ${{NOPE_UNSET:?needs {v}}}\n"), "required"),
            ] {
                drafts.push((svc(&body), why));
            }
            drafts.push((format!("services:\n  x:\n    image: \"alpine:{v}\"\n    user: \"{v}\"\n"), "image"));
        }
        let mut named = 0;
        for (draft, why) in &drafts {
            std::fs::write(&file, draft).unwrap();
            let hider = Hider::for_project(dcli, &p, None).keeping(declared(draft));
            let vars: Vec<(String, String)> = st.iter().map(|s| (s.name.clone(), s.value.clone())).collect();
            match config(dcli, &p, Some(draft), &[]) {
                Err(e) => {
                    let shown = error_json(&e, draft, &vars, &hider).to_string();
                    clean(&format!("{why} {draft}"), &shown);
                    assert!(!shown.contains("kirisec"), "{shown}");
                    named += usize::from(shown.contains("${"));
                }
                Ok(m) => {
                    let mut v = json!([service_view("x", &m["services"]["x"]), plan_json(&plan(&json!({}), &m)), unchecked_note(&m)]);
                    hider.show_json(&mut v);
                    clean(&format!("{why} {draft}"), &v.to_string());
                    assert!(!v.to_string().contains("kirisec"), "{v}");
                }
            }
        }
        assert!(named > drafts.len() / 3, "{named} of {}", drafts.len());
        // Each says which variable is wrong.
        let says = |draft: &str| {
            let e = config(dcli, &p, Some(draft), &[]).unwrap_err();
            error_json(&e, draft, &[], &Hider::for_project(dcli, &p, None).keeping(declared(draft)))["text"].as_str().unwrap().to_owned()
        };
        assert!(says(&svc("    ports: [\"${COLON_TOKEN}:80\"]\n")).contains("${COLON_TOKEN}"));
        assert!(says(&svc("    ports: [\"80:${SL_ACCESS_KEY}\"]\n")).contains("${SL_ACCESS_KEY}"));
        assert!(says(&svc("    pull_policy: ${BS_PASSWORD}\n")).contains("${BS_PASSWORD}"));
        assert!(says(&svc("    mem_limit: ${DATABASE_URL}\n")).contains("${DATABASE_URL password}"));
        // The image tag, as the plan and the cards show it.
        let m = config(dcli, &p, Some("services:\n  x:\n    image: alpine:${CMT_PASSWORD}\n"), &[]).unwrap();
        let mut v = service_view("x", &m["services"]["x"]);
        Hider::for_project(dcli, &p, None).show_json(&mut v);
        assert_eq!(v["image"], "alpine:${CMT_PASSWORD}");
        // A service named as a password is never hidden.
        assert_eq!(says("services:\n  web1:\n    image: alpine\n    depends_on: [nope]\n"), "service \"web1\" depends on undefined service \"nope\"");
        // Apply's names for what `up` says: the real ones.
        assert_eq!(real(&m, &stand_ins(&vars))["services"]["x"]["image"], "alpine:Cmt-x-77ff");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_value_over_several_lines_keeps_its_lines_with_an_equals_sign() {
        let v = environment("A=1\nML_PRIVATE_KEY=-----BEGIN KEY-----\nMIIBAAA==\nZZZ=x\nQ\n-----END KEY-----\nN_TOKEN=abc\nT=Ab+c=\nAAB==\nU=2\n");
        let names: Vec<&str> = v.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["A", "ML_PRIVATE_KEY", "N_TOKEN", "T", "U"]);
        assert_eq!(v[1].1, "-----BEGIN KEY-----\nMIIBAAA==\nZZZ=x\nQ\n-----END KEY-----");
        assert_eq!(v[3].1, "Ab+c=\nAAB==");
        // A plain value that says `-----BEGIN ` takes nothing after it.
        let v = environment("NOTE=-----BEGIN here\nWEB_PASSWORD=web1\n");
        assert_eq!(v, [("NOTE".to_owned(), "-----BEGIN here".to_owned()), ("WEB_PASSWORD".to_owned(), "web1".to_owned())]);
    }

    #[test]
    fn an_apply_error_that_was_all_secret_says_its_kind() {
        let h = Hider::new(vec![("${S_TOKEN}".into(), "Sx-9-qq7z".into())], vec![]);
        assert_eq!(shown_error(&h, "Sx-9-qq7z"), "docker compose up failed");
        assert_eq!(shown_error(&h, "\"Sx-9-qq7z\": 12"), "docker compose up failed");
        assert_eq!(shown_error(&h, "Error response from daemon: Sx-9-qq7z"), "Error response from daemon: ${S_TOKEN}");
        assert_eq!(shown_fault(&h, "Sx-9-qq7z"), Fault::Own("docker compose up failed".into()));
        assert_eq!(shown_fault(&h, "Error response from daemon: Sx-9-qq7z"), Fault::Said("Error response from daemon: ${S_TOKEN}".into()));
    }

    #[test]
    fn a_network_folder_is_refused_however_its_path_is_written() {
        for f in ["/net/h/x.yaml", "/usr/../net/host/x.yaml", "/./net/h", "/a/b/../../net/h", "//net/h/x", "/NET/h/x.yaml", "/net", "/x/..//net/./h"] {
            assert!(remote_path(f), "{f} is a network path");
            assert!(valid_file(f).is_none() && valid_dir(f, false).is_none() && valid_env_file(f).is_none(), "{f} is never read");
        }
        for f in ["/network/x.yaml", "/srv/net/x.yaml", "/usr/net", "net/x"] {
            assert!(!remote_path(f), "{f} is local");
        }
    }

    #[test]
    fn an_env_file_label_kiri_wont_read_is_said_and_refused() {
        let d = tdir("envlabel");
        let f = d.join("compose.yaml");
        std::fs::write(&f, "services: {}\n").unwrap();
        let p = Project { name: "x".into(), dir: d.to_string_lossy().into(), files: vec![f.to_string_lossy().into_owned()], env_files: vec![d.join("gone.env").to_string_lossy().into_owned()], ..Project::default() };
        let e = checked(p.clone(), tstore).unwrap_err();
        assert!(e.starts_with("couldn't read env file ") && e.contains("gone.env"), "{e}");
        // The file still reads: only Apply, the plan and the list refuse.
        assert_eq!(checked_files(p, tstore).unwrap().files, [f.to_string_lossy()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_that_is_gone_leaves_the_file_with_its_own() {
        let d = tdir("nodir");
        let f = d.join("compose.yaml");
        std::fs::write(&f, "services: {}\n").unwrap();
        let p = Project { name: "x".into(), dir: d.join("gone-folder").to_string_lossy().into(), files: vec![f.to_string_lossy().into_owned()], ..Project::default() };
        let c = checked(p, tstore).unwrap();
        assert_eq!(c.dir, d.to_string_lossy());
        assert!(project_args(&c, None).windows(2).any(|w| w[0] == "--project-directory" && w[1] == d.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_for_an_env_file_never_holds_kiri() {
        let d = tdir("fifo");
        let f = d.join("compose.yaml");
        std::fs::write(&f, "services: {}\n").unwrap();
        let fifo = d.join(".env");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let p = Project { name: "x".into(), dir: d.to_string_lossy().into(), files: vec![f.to_string_lossy().into_owned()], ..Project::default() };
        let t = std::time::Instant::now();
        assert_eq!(checked(p.clone(), tstore).unwrap_err(), bad_env(&fifo.to_string_lossy()));
        let _ = Hider::for_project(dcli, &p, None).hide("x");
        assert!(env_vars(dcli, &p).iter().all(|(k, _)| std::env::var_os(k).is_some()), "the environment's values alone");
        // As a label's --env-file too.
        let q = Project { dir: String::new(), env_files: vec![fifo.to_string_lossy().into_owned()], ..p };
        assert!(checked(q.clone(), tstore).unwrap_err().contains(".env"));
        let _ = hider_from(dcli, tstore, Ok(q));
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "answered at once");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn env_files_a_service_names_are_read_from_the_yaml() {
        let y = "name: \"shop\" # mine\nservices:\n  a:\n    env_file: a.env\n  b:\n    env_file: [\"b1.env\", 'b2.env']\n  c:\n    image: x\n    env_file:\n      - c1.env\n      - path: ./c2.env\n        required: false\n    ports: []\n  d:\n    env_file: ${D}.env\n";
        assert_eq!(env_file_paths(y), ["a.env", "b1.env", "b2.env", "c1.env", "./c2.env"]);
        assert_eq!(yaml_name(y).as_deref(), Some("shop"));
        assert_eq!(yaml_name("name: ${N}\nservices: {}\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_for_a_services_env_file_or_the_folders_never_waits_on_compose() {
        // Compose that would wait: asked at all, the test takes 30 s.
        let slow: Cli = || {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 40"]);
            Ok(c)
        };
        for (tag, at) in [("svcfifo", "web.env"), ("dotfifo", ".env")] {
            let d = tdir(tag);
            let f = d.join("compose.yaml");
            let yaml = if at == ".env" { "name: piped\nservices:\n  web:\n    image: x\n".to_owned() } else { format!("name: piped\nservices:\n  web:\n    image: x\n    env_file:\n      - {at}\n") };
            std::fs::write(&f, yaml).unwrap();
            let fifo = d.join(at);
            assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
            let p = Project { name: folder_name(&d), dir: d.to_string_lossy().into(), files: vec![f.to_string_lossy().into_owned()], ..Project::default() };
            let t = std::time::Instant::now();
            assert_eq!(opened_name(slow, &p), "piped", "{tag}: its own name:");
            assert_eq!(checked(p.clone(), tstore).unwrap_err(), bad_env(&fifo.to_string_lossy()), "{tag}");
            assert!(t.elapsed() < std::time::Duration::from_secs(1), "{tag}: {:?}", t.elapsed());
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_child_left_holding_the_pipes_never_holds_the_timeout() {
        // `docker` killed, its plugin still up: its own group (gone with
        // docker's), and one that left it (still holding the pipes).
        for script in ["(sleep 20) & sleep 20", "perl -e 'setpgrp(0,0); sleep 20' & sleep 20"] {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            let t = std::time::Instant::now();
            assert!(run_cli(c, None, 1).unwrap_err().contains("longer than 1 seconds"));
            assert!(t.elapsed() < std::time::Duration::from_secs(4), "{script}: {:?}", t.elapsed());
        }
    }

    #[test]
    fn no_piece_of_a_random_secret_survives_its_printing() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        // No 4 characters of it with a letter or digit, outside the words around it.
        let survives = |raw: &str, out: &str, around: &[&str]| -> Option<String> {
            let rest = around.iter().fold(out.to_owned(), |o, w| o.replace(w, ""));
            let cs: Vec<char> = raw.chars().collect();
            cs.windows(4).map(|w| w.iter().collect::<String>()).find(|w| w.chars().any(char::is_alphanumeric) && rest.contains(w.as_str()))
        };
        let chars: Vec<char> = "abcXYZ019-_.$\"\\'`!@#%&*+=/:;<>?~^{}| \n\té€".chars().collect();
        for turn in 0..400 {
            let len = 6 + next(14);
            let raw: String = (0..len).map(|_| chars[next(chars.len())]).collect();
            if raw.trim().chars().count() < 4 {
                continue;
            }
            let h = Hider::new(secret_values(vec![("R_PASSWORD".into(), raw.clone())]), vec![]);
            let mut printed = forms(&raw);
            printed.extend(raw.lines().map(str::trim).filter(|l| l.chars().count() >= 4).map(str::to_owned));
            for q in printed {
                for (open, close) in [("parsing \"", "\": invalid syntax"), ("depends on undefined service \"", "\""), ("unknown user ", "\n")] {
                    let out = h.hide_up(&format!("{open}{q}{close}"));
                    if let Some(w) = survives(&raw, &out, &["parsing", "invalid syntax", "depends on undefined service", "unknown user", "${R_PASSWORD}"]) {
                        panic!("try {turn}: {w:?} of {raw:?} survives as {q:?}: {out}");
                    }
                }
            }
            // A piece Docker cuts out and quotes back: never 6 characters of it.
            let cs: Vec<char> = raw.chars().collect();
            for _ in 0..6 {
                let (a, n) = (next(cs.len()), 6 + next(8));
                let piece: String = cs.iter().skip(a).take(n).collect();
                if piece.chars().count() < 6 || piece.contains(|c: char| c.is_whitespace() || "\"'()[]:`,;<>/=@|\\".contains(c)) {
                    continue;
                }
                for (open, close) in [("invalid reference format: repository name (", ") must be lowercase"), ("bad tag \"", "\""), ("unknown user: ", ": no matching entries"), ("[", "]")] {
                    let out = h.hide_up(&format!("Error response from daemon: {open}{piece}{close}"));
                    assert!(!out.contains(&piece), "try {turn}: piece {piece:?} of {raw:?} survives: {out}");
                }
            }
            // A name made of a secret and more is never changed.
            let word: String = cs.iter().filter(|c| c.is_ascii_alphanumeric()).collect();
            if word.chars().count() >= 4 {
                let h = Hider::new(secret_values(vec![("W_PASSWORD".into(), word.clone())]), vec![word.clone()]);
                for name in [format!("{word}-cache"), format!("app_{word}"), format!("{word}.local"), format!("kiri-{word}"), word.clone()] {
                    let line = format!("Container {name} Error: exited (1)");
                    assert_eq!(h.hide_up(&line), line, "try {turn}");
                    let mut v = json!({"n": name, "path": format!("/w/{name}/compose.yaml")});
                    let before = v.clone();
                    h.show_json(&mut v);
                    assert_eq!(v, before);
                }
            }
        }
        // Through Compose itself: a value it reads from the `.env` (in single
        // quotes, as it is) never reaches its errors.
        let d = tdir("random");
        let file = d.join("compose.yaml");
        let p = Project { name: "random".into(), dir: d.to_string_lossy().into_owned(), files: vec![file.to_string_lossy().into_owned()], ..Project::default() };
        let quiet: Vec<char> = chars.iter().copied().filter(|c| !matches!(c, '\'' | '\n' | '\t' | '#' | ' ')).collect();
        for turn in 0..12 {
            let raw: String = (0..8 + next(10)).map(|_| quiet[next(quiet.len())]).collect();
            std::fs::write(d.join(".env"), format!("R_PASSWORD='{raw}'\n")).unwrap();
            for body in ["mem_limit: \"${R_PASSWORD}\"", "depends_on: [\"${R_PASSWORD}\"]", "ports: [\"80:${R_PASSWORD}\"]", "user: \"${R_PASSWORD}\"\n    stop_grace_period: \"${R_PASSWORD}\""] {
                let draft = format!("services:\n  x:\n    image: alpine\n    {body}\n");
                std::fs::write(&file, &draft).unwrap();
                let hider = Hider::for_project(dcli, &p, None);
                let shown = match config(dcli, &p, Some(&draft), &[]) {
                    Err(e) => error_json(&e, &draft, &[], &hider).to_string(),
                    Ok(m) => {
                        let mut v = service_view("x", &m["services"]["x"]);
                        hider.show_json(&mut v);
                        v.to_string()
                    }
                };
                if let Some(w) = survives(&raw, &shown, &["${R_PASSWORD}", "services", "mem_limit", "depends_on", "alpine"]) {
                    panic!("compose try {turn}: {w:?} of {raw:?} survives: {shown}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keys_paths_and_plain_values_are_not_secrets() {
        for k in ["PUBLIC_KEY", "SORT_KEY", "PRIMARY_KEY", "HOTKEY", "MONKEY", "SSH_PUBLIC_KEY", "DB_PASSWORD_FILE", "SECRET_KEY_PATH", "TOKEN_DIR", "AUTH_URL", "GOOGLE_APPLICATION_CREDENTIALS"] {
            assert!(!secret_name(k), "{k} is plain");
        }
        for k in ["NPM_AUTHTOKEN", "AUTHTOKEN", "API_KEY", "STRIPE_SECRET_KEY", "MASTERKEY"] {
            assert!(secret_name(k), "{k} says secret");
        }
        let vals = secret_values(vec![("A_PASSWORD".into(), "abc".into()), ("B_TOKEN".into(), "true".into()), ("C_SECRET".into(), "No".into()), ("D_PASSWORD".into(), "s3cret-x".into())]);
        assert_eq!(vals, [("${D_PASSWORD}".to_owned(), "s3cret-x".to_owned())]);
        // In a path too, as its variable.
        let h = Hider::new(vec![("${R}".into(), "r3dis".into()), ("${Y}".into(), "yes".into())], vec![]);
        assert_eq!(h.hide("open /srv/r3dis/data: denied for r3dis; ./r3dis ~/r3dis C:\\r3dis\\x yes"), "open /srv/${R}/data: denied for ${R}; ./${R} ~/${R} C:\\${R}\\x yes");
        assert_eq!(Hider::new(vec![("${T}".into(), "/tok-x-1".into())], vec![]).hide("bad /tok-x-1"), "bad ${T}");
    }

    #[test]
    fn errors_with_the_file_gone_are_hidden_too() {
        let d = tdir("gone");
        std::fs::write(d.join(".env"), "REDIS_PASSWORD=Gone-x-3f\n").unwrap();
        let p = Project { name: "gone".into(), dir: d.to_string_lossy().into_owned(), files: vec![d.join("compose.yaml").to_string_lossy().into_owned()], ..Project::default() };
        let h = hider_from(dcli, tstore, Ok(p.clone()));
        assert_eq!(h.hide("Error response from daemon: Gone-x-3f"), "Error response from daemon: ${REDIS_PASSWORD}");
        // A network folder is never looked in.
        let far = hider_from(dcli, tstore, Ok(Project { dir: "//host/share".into(), ..p }));
        assert_eq!(far.hide("x Gone-x-3f"), "x Gone-x-3f");
        assert_eq!(action_error("Container shop-web-1  Starting\nError response from daemon: no such image"), "Error response from daemon: no such image");
        assert_eq!(action_error("Network shop_default  Creating\n Container shop-web-1  Starting\nfailed"), "failed");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_compose_error_over_several_lines_says_them_all() {
        let e = "time=\"2026-11-16T19:21:19+01:00\" level=warning msg=\"The \\\"X\\\" variable is not set.\"\ndecoding failed due to the following error(s):\n\n'services[a].mem_limit' invalid size: 'nonsense'\n";
        assert_eq!(error_text(e), "decoding failed due to the following error(s):\n'services[a].mem_limit' invalid size: 'nonsense'");
        assert_eq!(error_text("time=\"x\" level=warning msg=\"y\"\nservice \"web\" has neither an image nor a build context specified: invalid compose project"), "service \"web\" has neither an image nor a build context specified");
        assert_eq!(error_key(NO_FILE), Some(("noFile", json!({}))));
    }

    #[test]
    fn a_file_put_back_by_hand_has_nothing_left_to_put_back() {
        remember_before("byhand", "/w/byhand.yaml", b"services: {}\n".to_vec(), "w1");
        assert_eq!(still_before("byhand", "/w/byhand.yaml", "different"), (true, false));
        assert_eq!(still_before("byhand", "/w/byhand.yaml", &file_hash(b"services: {}\n")), (false, true));
        assert!(!BEFORE.lock().unwrap().contains_key("byhand"));
        // A failed Apply that had nothing to save: nothing was put back by hand.
        assert_eq!(still_before("nosave", "/w/nosave.yaml", "any"), (false, false));
    }

    #[test]
    fn a_path_or_a_port_in_a_secret_named_variable_is_read_as_it_is() {
        for v in ["./certs/server.key", "/etc/ssl/server.key", "./creds.json", "../conf/k.pem", "./data", "~/k.pem", "C:\\certs\\k.pem", "D:/k", "/var/lib/app", "/Users/me/key.p12", "8080", "1"] {
            assert!(plain_shape(v), "{v}");
        }
        // A key that starts with `/` and has no `+` or `=` is no path.
        for v in ["/Ab3dEf9hIjKlMnOpQrStUvWxYz012345/abcDEF", "/K7MDENGbPxRfiCYEXAMPLEKEY", "/etc/Ab3dEf9hIjKlMnOpQrStUv", "wJalrXUt/K7MDENG+bPx/Zk9Q", "/K7MDENG+bPx=", "../k", "~", "~user/k", "~.;x", "123456", "s3cret", "", "~x"] {
            assert!(!plain_shape(v), "{v}");
        }
        let d = tdir("shape");
        std::fs::create_dir_all(d.join("certs")).unwrap();
        std::fs::write(d.join("certs/server.key"), "k").unwrap();
        std::fs::write(d.join("creds.json"), "{}").unwrap();
        std::fs::write(d.join(".env"), "SSL_KEY=./certs/server.key\nGOOGLE_CREDENTIALS=./creds.json\nNUM_TOKEN=8080\nDB_PASSWORD=pw-x-77\nAPI_SECRET=/Ab3dEf9hIjKlMnOpQrStUvWxYz012345/abcDEF\n").unwrap();
        let p = Project { name: "shape".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        let names: Vec<String> = project_stands(dcli, &p).into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["API_SECRET", "DB_PASSWORD"]);
        let draft = "services:\n  x:\n    image: alpine\n    volumes: [\"${SSL_KEY}:/k:ro\", \"${GOOGLE_CREDENTIALS}:/c.json:ro\"]\n    ports: [\"${NUM_TOKEN}:80\"]\n";
        let m = config(dcli, &p, Some(draft), &[]).unwrap();
        let v = service_view("x", &m["services"]["x"]);
        assert_eq!(v["ports"], json!(["8080:80"]));
        assert!(v["volumes"][0].as_str().unwrap().ends_with("certs/server.key:/k"), "{v}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_value_the_files_structure_needs_is_never_read_into_the_window() {
        let d = tdir("unmask");
        std::fs::write(d.join("app.env"), "A=1\n").unwrap();
        std::fs::write(d.join("base.yaml"), "services:\n  base:\n    image: alpine\n").unwrap();
        std::fs::write(d.join(".env"), "APP_SECRETS=app.env\nBASE_SECRET=base.yaml\nPROF_SECRET=debug\nCOMPOSE_PROFILES=debug\nDB_PASSWORD=pw-x-77\n").unwrap();
        let p = Project { name: "unmask".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        // An env file a variable names, and a file to extend: the stand-in
        // reading fails, the real one passes; the reading names the variable.
        let draft = "services:\n  x:\n    image: alpine\n    env_file: ${APP_SECRETS}\n";
        let m = config(dcli, &p, Some(draft), &[]).unwrap();
        assert_eq!(m[UNCHECKED], "${APP_SECRETS}");
        let note = unchecked_note(&m).unwrap();
        assert_eq!(note["errkey"], "hiddenCheck");
        assert_eq!(note["text"], "Kiri can't check this file with ${APP_SECRETS} hidden. Compose accepts it, so Apply still works.");
        // A reading that worked before stands for the file till one works again.
        config(dcli, &p, Some("services:\n  x:\n    image: alpine\n"), &[]).unwrap();
        let m = config(dcli, &p, Some("services:\n  x:\n    image: alpine\n    extends:\n      file: ${BASE_SECRET}\n      service: base\n"), &[]);
        // Compose can't list the variables of a file it extends by a variable: by name.
        let e = m.unwrap_err();
        assert!(e.by_name && e.text == BY_NAME && e.key.as_ref().unwrap().0 == "byName", "{e:?}");
        // A profile: the service is there by name only, as `docker compose` has it.
        let draft = "services:\n  x:\n    image: alpine\n  dbg:\n    image: alpine:${DB_PASSWORD}\n    profiles: [\"${PROF_SECRET}\"]\n";
        let m = config(dcli, &p, Some(draft), &[]).unwrap();
        assert_eq!(m["services"]["dbg"], json!({HIDDEN: true}), "{m}");
        assert_eq!(service_view("dbg", &m["services"]["dbg"])["hidden"], true);
        assert!(!m.to_string().contains("pw-x-77"));
        // Every secret keeps its stand-in.
        let left: Vec<String> = project_stands(dcli, &p).into_iter().map(|s| s.name).collect();
        assert_eq!(left, ["APP_SECRETS", "BASE_SECRET", "DB_PASSWORD", "PROF_SECRET"]);
        // A secret that is wrong with its real value too: its stand-in's error, never the value.
        let draft = "services:\n  x:\n    image: alpine\n    mem_limit: ${DB_PASSWORD}\n";
        let e = config(dcli, &p, Some(draft), &[]).unwrap_err();
        let shown = error_json(&e, draft, &[], &Hider::for_project(dcli, &p, None))["text"].as_str().unwrap().to_owned();
        assert!(!shown.contains("pw-x-77") && shown.contains("${DB_PASSWORD}"), "{shown}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_kiri_cant_check_lists_the_services_compose_has_now() {
        let d = tdir("stale");
        std::fs::write(d.join("app.env"), "A=1\n").unwrap();
        std::fs::write(d.join(".env"), "APP_SECRETS=app.env\n").unwrap();
        let p = Project { name: "stale".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        // A reading that worked, then the file changed: a service gone, one
        // new, and a variable the stand-ins can't read.
        config(dcli, &p, Some("services:\n  x:\n    image: alpine:3\n    container_name: c-old\n  gone:\n    image: busybox\n"), &[]).unwrap();
        let draft = "services:\n  x:\n    image: alpine:3\n    container_name: c-old\n  y:\n    image: nginx\n    env_file: ${APP_SECRETS}\n";
        let m = config(dcli, &p, Some(draft), &[]).unwrap();
        assert_eq!(m[UNCHECKED], "${APP_SECRETS}");
        assert_eq!(start_order(&m).into_iter().collect::<BTreeSet<_>>(), BTreeSet::from(["x".to_owned(), "y".to_owned()]), "{m}");
        assert_eq!(m["services"]["x"]["image"], "alpine:3");
        // What the file says of it plainly still shows.
        assert_eq!(m["services"]["y"], json!({HIDDEN: true, "image": "nginx"}));
        // Apply's progress goes by those names, not the last reading's container names.
        let run = [Member { service: "cn".into(), name: "x-Dpl-x-4u4u9".into(), ..Member::default() }];
        let names = apply_names("stale", &m, &m, &run);
        // A container `up` removes, its name from a secret the file no longer has: said by its service.
        assert_eq!(progress("Container x-Dpl-x-4u4u9 Stopping", "stale", &names), Some((vec!["cn".to_owned()], "removing")));
        assert_eq!(progress(" Container x-Dpl-x-4u4u9 Stopped ", "stale", &names), Some((vec!["cn".to_owned()], "removing")));
        // A new service whose container_name the stand-ins couldn't read, named after the project and a secret: its service.
        assert_eq!(progress(" Container stale-y-Dpl-x-4u4u9 Creating ", "stale", &names), Some((vec!["y".to_owned()], "creating")));
        assert_eq!(progress("Container stale-y-1 Started", "stale", &names), Some((vec!["y".to_owned()], "started")));
        assert_eq!(progress("Image stale-y Building", "stale", &names), Some((vec!["y".to_owned()], "building")));
        assert_eq!(progress("Container c-old Started", "stale", &names), None);
        assert_eq!(progress("Image busybox Pulling", "stale", &names), None);
        // With no reading before, the names alone.
        GOOD.lock().unwrap().retain(|k, _| !k.starts_with("stale\0"));
        let m = config(dcli, &p, Some(draft), &[]).unwrap();
        assert_eq!(m["services"], json!({"x": {HIDDEN: true, "image": "alpine:3"}, "y": {HIDDEN: true, "image": "nginx"}}));
        let _ = std::fs::remove_dir_all(&d);
    }

    const SHELL_GH: &str = "Fake-shell-tok-AbCdEf0123456789xyzXYZ98765";
    const SHELL_AWS: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYSHELLKEY";

    /// The docker CLI with secrets in the shell's environment, as a user's has.
    fn scli() -> io::Result<Command> {
        let mut c = dcli()?;
        c.env("GITHUB_TOKEN", SHELL_GH).env("AWS_SECRET_ACCESS_KEY", SHELL_AWS).env("DEPLOY_TOKEN", "Dpl-x-4u4u9");
        Ok(c)
    }

    /// Everything the list and the plan send the window of a saved file.
    fn window_json(cli: Cli, p: &Project, yaml: &str) -> (String, bool) {
        let model = saved_config(cli, p, &[]).map_err(|e| error_text(&e));
        let hider = Hider::for_project(cli, p, None).keeping(model.as_ref().ok().map(model_names).unwrap_or_default());
        let listed = model.is_ok();
        let mut out = vec![json!(opened_name(cli, p))];
        match model {
            Ok(mut m) => {
                hider.show_json(&mut m);
                out.push(json!(start_order(&m).iter().map(|n| service_view(n, &m["services"][n])).collect::<Vec<_>>()));
                out.push(json!(unchecked_note(&m)));
            }
            Err(e) => out.push(json!(hider.hide(&e))),
        }
        let hider = Hider::for_project(cli, p, None).keeping(declared(yaml));
        match config(cli, p, Some(yaml), &[]) {
            Ok(new) => {
                let mut v = json!([plan_json(&plan(&json!({}), &new)), start_order(&new).iter().map(|n| service_view(n, &new["services"][n])).collect::<Vec<_>>(), unchecked_note(&new)]);
                hider.show_json(&mut v);
                out.push(v);
            }
            Err(e) => out.push(error_json(&e, yaml, &[], &hider)),
        }
        (Value::Array(out).to_string(), listed)
    }

    #[test]
    fn a_secret_in_the_shells_environment_never_reaches_the_window() {
        for (tag, env, yaml) in [
            ("sh-profiles", "Z_TOKEN=devprofile\nCOMPOSE_PROFILES=devprofile\n", "services:\n  x:\n    image: \"evil/${GITHUB_TOKEN}\"\n    profiles: [\"${Z_TOKEN}\"]\n  y:\n    image: alpine\n    profiles: [\"${GITHUB_TOKEN}\"]\n"),
            ("sh-windrive", "Z_TOKEN=c\n", "services:\n  x:\n    image: \"evil/${GITHUB_TOKEN}\"\n    labels:\n      k: \"${AWS_SECRET_ACCESS_KEY}\"\n    volumes: [\"${Z_TOKEN}:/src:/data/${GITHUB_TOKEN}:ro\"]\n"),
            ("sh-envfile", "Z_TOKEN=app\n", "services:\n  x:\n    image: \"evil/${GITHUB_TOKEN}\"\n    env_file: ./conf/${Z_TOKEN}.env\n"),
            ("sh-extends", "Z_TOKEN=base.yaml\n", "services:\n  x:\n    image: \"evil/${GITHUB_TOKEN}\"\n    extends:\n      file: \"${Z_TOKEN}\"\n      service: \"base\"\n"),
            ("sh-memlimit", "MEM_SECRET=9876543210\n", "services:\n  x:\n    image: \"evil/${GITHUB_TOKEN}\"\n    mem_limit: ${MEM_SECRET}\n"),
            ("sh-hosts", "IP_SECRET=10.11.12.13\n", "services:\n  x:\n    image: \"evil/${GITHUB_TOKEN}\"\n    extra_hosts: [\"host:${IP_SECRET}\", \"other=${IP_SECRET}\"]\n"),
            ("sh-name", "", "services:\n  x:\n    image: \"evil/${GITHUB_TOKEN}\"\n    container_name: x-${DEPLOY_TOKEN}\n"),
        ] {
            let d = tdir(tag);
            std::fs::create_dir_all(d.join("conf")).unwrap();
            std::fs::create_dir_all(d.join("src")).unwrap();
            std::fs::write(d.join("conf/app.env"), "A=1\n").unwrap();
            std::fs::write(d.join("base.yaml"), "services:\n  base:\n    image: alpine\n").unwrap();
            if !env.is_empty() {
                std::fs::write(d.join(".env"), env).unwrap();
            }
            std::fs::write(d.join("compose.yaml"), yaml).unwrap();
            let p = Project { name: tag.into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
            let (shown, listed) = window_json(scli, &p, yaml);
            for v in [SHELL_GH, SHELL_AWS, "Dpl-x-4u4u9", "9876543210", "10.11.12.13", "kirisec"] {
                assert!(!shown.contains(v), "{tag}: a value in {shown}");
            }
            // An include path Compose can't list the variables of: by name.
            assert!(listed || shown.contains("byName"), "{tag}: {shown}");
            assert!(real_passes(scli, &p, None, &[]) == Ok(true), "{tag}: Compose refuses the fixture");
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn no_place_a_secret_is_put_in_shows_it() {
        // A small generator: `${GITHUB_TOKEN}` in places of the file's own
        // structure, one or two at a time, picked by a fixed seed.
        let places = [
            "    image: \"evil/$S\"\n", "    container_name: \"c-$S\"\n", "    profiles: [\"$S\"]\n", "    volumes: [\"./src:/data/$S:ro\"]\n", "    volumes: [\"$S:/data\"]\n",
            "    env_file: ./conf/$S.env\n", "    mem_limit: \"$S\"\n", "    extra_hosts: [\"h:$S\"]\n", "    ports: [\"$S:80\"]\n", "    user: \"$S\"\n", "    working_dir: /w/$S\n",
            "    labels:\n      k: \"$S\"\n", "    networks: [\"$S\"]\n", "    extends:\n      file: \"$S\"\n      service: base\n", "    build:\n      context: \"$S\"\n", "    platform: \"$S\"\n",
        ];
        let d = tdir("gen");
        std::fs::create_dir_all(d.join("conf")).unwrap();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("base.yaml"), "services:\n  base:\n    image: alpine\n").unwrap();
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for i in 0..24 {
            let (a, b) = (next(places.len()), next(places.len()));
            let var = if next(2) == 0 { "${GITHUB_TOKEN}" } else { "${AWS_SECRET_ACCESS_KEY}" };
            let mut body = places[a].replace("$S", var);
            if b != a && !places[b].starts_with(&places[a][..10]) {
                body.push_str(&places[b].replace("$S", var));
            }
            let yaml = format!("services:\n  x:\n{body}  y:\n    image: alpine\n");
            let tag = format!("gen{i}");
            let p = Project { name: tag, dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
            std::fs::write(d.join("compose.yaml"), &yaml).unwrap();
            let (shown, _) = window_json(scli, &p, &yaml);
            for v in [SHELL_GH, SHELL_AWS, "kirisec"] {
                assert!(!shown.contains(v), "{yaml}: a value in {shown}");
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_broken_env_file_shows_in_the_list_at_once() {
        let d = tdir("envstamp");
        std::fs::write(d.join("app.env"), "A=1\n").unwrap();
        std::fs::write(d.join("compose.yaml"), "services:\n  x:\n    image: alpine\n    env_file: app.env\n").unwrap();
        let p = Project { name: "envstamp".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        assert!(saved_config(dcli, &p, &[]).is_ok());
        std::fs::write(d.join("app.env"), "A=\"never ends\n").unwrap();
        assert!(saved_config(dcli, &p, &[]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_wordy_secret_never_trims_a_common_error() {
        let h = Hider::new(secret_values(vec![("APP_SECRET".into(), "my-container-secret".into()), ("DB_PASSWORD".into(), "docker123".into()), ("RAND_TOKEN".into(), "q7Zr-Kp2x-Wm9v".into())]), vec![]);
        for e in ["kiri: cannot start container: exec: no such file", "dependency failed to start: container x-1 exited (1)", "Error response from daemon: docker daemon not reachable"] {
            assert_eq!(h.hide_up(e), e);
        }
        // A piece of a random one still goes.
        assert_eq!(h.hide_up("bad value \"Kp2x-Wm9v\" here"), "bad value");
    }

    #[test]
    fn a_services_env_file_never_quotes_its_line_back() {
        assert_eq!(
            env_read_error("failed to read /w/svc.env: line 4: unterminated quoted value \"tokentokentoken77").as_deref(),
            Some("failed to read /w/svc.env: line 4: unterminated quoted value")
        );
        assert_eq!(env_read_error("x: failed to read /w/a.env: line 2: unexpected character \"!\" in variable name \"A!B\"").as_deref(), Some("x: failed to read /w/a.env: line 2: unexpected character"));
        assert_eq!(env_read_error("failed to read /w/a.env: permission denied"), None);
        let d = tdir("svcenv");
        std::fs::write(d.join("svc.env"), "A=1\nB=2\nC=3\nAPI_TOKEN=\"tokentokentoken77\n").unwrap();
        std::fs::write(d.join("compose.yaml"), "services:\n  x:\n    image: alpine\n    env_file: svc.env\n").unwrap();
        let p = Project { name: "svcenv".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        let draft = std::fs::read_to_string(d.join("compose.yaml")).unwrap();
        let hider = Hider::for_project(dcli, &p, None);
        let e = config(dcli, &p, Some(&draft), &[]).unwrap_err();
        let shown = error_json(&e, &draft, &[], &hider).to_string();
        assert!(!shown.contains("tokentoken") && shown.contains("svc.env: line ") && shown.ends_with(": unterminated quoted value\"}"), "{shown}");
        assert!(!shown_error(&hider, &error_text(&e)).contains("tokentoken"));
        // What `up` and the engine say of its value is hidden too.
        assert_eq!(hider.hide_up("kiri: cannot start container: bad env tokentokentoken77"), "kiri: cannot start container: bad env ${API_TOKEN}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_piece_of_an_access_key_docker_quotes_back_says_only_its_kind() {
        let h = shapes();
        for e in ["invalid reference format: repository name (K7MDENG+bPx/Zk9Q) must be lowercase", "invalid reference format: repository name (k7mdeng+bpx/zk9q) must be lowercase", "pull alpine:wJalrXUt/K7MDENG: denied", "[K7MDENG+bPx] bad"] {
            let shown = shown_error(&h, e);
            clean(e, &shown);
            assert!(!shown.to_lowercase().contains("k7mdeng"), "{e} → {shown}");
        }
        assert_eq!(shown_error(&h, "invalid reference format: repository name (K7MDENG+bPx/Zk9Q) must be lowercase"), "invalid reference format: repository name");
        // Caught before `up`: the real image isn't a reference.
        for (r, ok) in [("alpine", true), ("alpine:3.20", true), ("ghcr.io/a/b-c__d:1.0", true), ("localhost:5000/x", true), ("x@sha256:0123456789abcdef0123456789abcdef", true), ("alpine:wJalrXUt/K7MDENG+bPx/Zk9Q", false), ("wJalrXUt/K7MDENG+bPx/Zk9Q", false), ("Alpine", false), ("a:", false), ("", false)] {
            assert_eq!(image_reference(r), ok, "{r}");
        }
        let stands = stand_ins(&[("SL_ACCESS_KEY".to_owned(), "wJalrXUt/K7MDENG+bPx/Zk9Q".to_owned())]);
        let tok = &stands[0].token;
        for img in [format!("alpine:{tok}"), tok.clone()] {
            let m = json!({"services": {"x": {"image": img}}});
            assert_eq!(bad_image(&m, &real(&m, &stands), &stands).as_deref(), Some("${SL_ACCESS_KEY}"));
        }
        let m = json!({"services": {"x": {"image": "alpine:3"}}});
        assert_eq!(bad_image(&m, &real(&m, &stands), &stands), None);
        assert_eq!(error_key("the image name from ${SL_ACCESS_KEY} isn't valid").map(|k| k.1), Some(json!({"name": "${SL_ACCESS_KEY}"})));
    }

    #[test]
    fn every_line_of_a_key_the_engine_says_is_hidden_in_logs() {
        let h = shapes();
        let text = "x-1  | 2026-11-16T13:02:01.500Z kiri: cannot start container: user: unknown user -----BEGIN KEY-----\nx-1  | 2026-11-16T13:02:01.500Z Mlq-x-line1AAA\nx-1  | 2026-11-16T13:02:01.501Z Mlq-x-line2BBB\nx-1  | 2026-11-16T13:02:01.501Z -----END KEY-----\nx-1  | 2026-11-16T13:02:09.000Z Mlq-x-line9 is the app's own\n";
        let mut lines = log_lines(text);
        engine_lines(&mut lines, &h);
        let all = Value::Array(lines.clone()).to_string();
        for m in ["line1AAA", "line2BBB", "-----END KEY-----", "-----BEGIN KEY-----"] {
            assert!(!all.contains(m), "{m}: {all}");
        }
        assert_eq!(lines[4]["text"], "Mlq-x-line9 is the app's own");
        // A line of the key stamped a second after the engine's line.
        let text = "x-1  | 2026-11-16T13:02:01.900Z kiri: cannot start container: user: unknown user -----BEGIN KEY-----\nx-1  | 2026-11-16T13:02:02.100Z Mlq-x-line1AAA\nx-1  | 2026-11-16T13:02:03.000Z Mlq-x-line2BBB\n";
        let mut lines = log_lines(text);
        engine_lines(&mut lines, &h);
        let all = Value::Array(lines.clone()).to_string();
        assert!(!all.contains("line1AAA") && !all.contains("line2BBB"), "{all}");
        // As the engine writes it now: the key's line breaks as spaces.
        let spaced = "kiri: cannot start container: user: unknown user -----BEGIN KEY----- Mlq-x-line1AAA Mlq-x-line2BBB -----END KEY-----";
        let shown = h.hide_up(spaced);
        clean(spaced, &shown);
        // On one line, as the engine now writes it.
        let one = "kiri: cannot start container: user: unknown user -----BEGIN KEY-----\\nMlq-x-line1AAA\\nMlq-x-line2BBB\\n-----END KEY-----";
        let shown = h.hide_up(one);
        clean(one, &shown);
        assert!(!shown.contains("END KEY"), "{shown}");
    }

    #[test]
    fn a_key_with_no_end_takes_only_its_own_line() {
        let v = environment("A=1\nML_PRIVATE_KEY=-----BEGIN KEY-----\nN_TOKEN=abc\nU=2\n");
        let names: Vec<&str> = v.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["A", "ML_PRIVATE_KEY", "N_TOKEN", "U"]);
        assert_eq!(v[1].1, "-----BEGIN KEY-----");
        // Nor up to the END of the next variable's own block.
        let v = environment("A_KEY=-----BEGIN A-----\nB=1\nC_KEY=-----BEGIN C-----\nMIIB==\n-----END C-----\nD=2\n");
        let names: Vec<&str> = v.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["A_KEY", "B", "C_KEY", "D"]);
        assert_eq!(v[2].1, "-----BEGIN C-----\nMIIB==\n-----END C-----");
    }

    #[test]
    fn the_list_of_a_big_project_with_many_secrets_is_quick() {
        let vars: Vec<(String, String)> = (0..50).map(|i| (format!("S{i}_TOKEN"), format!("Tok-{i}-x-0123456789"))).collect();
        let h = Hider::new(secret_values(vars), vec![]);
        let mut model = json!({"services": {}});
        for i in 0..20 {
            model["services"][format!("svc{i}")] = json!({"image": format!("img{i}:1"), "ports": [{"target": 80, "published": format!("{}", 8000 + i)}], "volumes": [{"type": "volume", "source": format!("v{i}"), "target": "/d"}], "depends_on": {"svc0": {}}});
        }
        // The best of five runs: a loaded machine slows one, never all.
        let (took, v) = (0..5)
            .map(|_| {
                let t = std::time::Instant::now();
                let mut v = json!({"services": start_order(&model).iter().map(|n| service_view(n, &model["services"][n])).collect::<Vec<_>>()});
                h.show_json(&mut v);
                (t.elapsed(), v)
            })
            .min_by_key(|(t, _)| *t)
            .unwrap();
        eprintln!("list of 20 services, 50 secrets: {took:?}");
        assert!(took < std::time::Duration::from_millis(20), "{took:?}");
        assert_eq!(v["services"].as_array().map(Vec::len), Some(20), "{v}");
    }

    #[test]
    fn an_include_entrys_env_files_are_read_as_entries_are_written() {
        let t = "name: top\ninclude:\n  - sub/compose.yaml\n  - path: ./b.yaml # a note\n    project_directory: e\n    env_file: e/x.env\n  - path:\n      - c.yaml\n      - d.yaml\n    env_file:\n      - c.env\n      - \"${NOT_READ}.env\"\n  - path: [f.yaml]\nservices:\n  x:\n    image: alpine\n";
        let got = includes(t);
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[0], Include { paths: vec!["sub/compose.yaml".into()], ..Include::default() });
        assert_eq!(got[1], Include { paths: vec!["./b.yaml".into()], dir: Some("e".into()), env_files: vec!["e/x.env".into()] });
        assert_eq!(got[2], Include { paths: vec!["c.yaml".into(), "d.yaml".into()], dir: None, env_files: vec!["c.env".into()] });
        assert_eq!(got[3].paths, ["f.yaml"]);
        assert_eq!(includes("include: [a.yaml, 'b.yaml']\n").iter().map(|i| i.paths[0].clone()).collect::<Vec<_>>(), ["a.yaml", "b.yaml"]);
        assert!(includes("services:\n  x:\n    include: [a.yaml]\n").is_empty());
    }

    #[test]
    fn an_included_projects_secrets_get_stand_ins_too() {
        const PW: &str = "Sup3rS3cretPw99";
        let d = tdir("incenv");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::create_dir_all(d.join("e")).unwrap();
        std::fs::create_dir_all(d.join("deep")).unwrap();
        std::fs::write(d.join("sub/.env"), format!("DB_PASSWORD={PW}\n")).unwrap();
        std::fs::write(d.join("sub/compose.yaml"), "include:\n  - ../deep/compose.yaml\nservices:\n  inc:\n    image: reg/app:${DB_PASSWORD}\n    volumes: [\"./data/${DB_PASSWORD}:/var/lib/x\"]\n").unwrap();
        // Two levels down, with its own folder's `.env`.
        std::fs::write(d.join("deep/.env"), "DEEP_TOKEN=Deep-x-Tok3n55\n").unwrap();
        std::fs::write(d.join("deep/compose.yaml"), "services:\n  deep:\n    image: reg/deep:${DEEP_TOKEN}\n").unwrap();
        // The long form: an env file of its own.
        std::fs::write(d.join("e/x.env"), "API_SECRET=Long-x-Form88\n").unwrap();
        std::fs::write(d.join("e/long.yaml"), "services:\n  long:\n    image: reg/long:${API_SECRET}\n").unwrap();
        let top = "include:\n  - sub/compose.yaml\n  - path: e/long.yaml\n    env_file: e/x.env\nservices:\n  x:\n    image: alpine\n";
        std::fs::write(d.join("compose.yaml"), top).unwrap();
        let p = Project { name: "incenv".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        let names: Vec<String> = project_stands(dcli, &p).into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["API_SECRET", "DB_PASSWORD", "DEEP_TOKEN"]);
        let hider = Hider::for_project(dcli, &p, None);
        let mut m = config(dcli, &p, None, &[]).unwrap();
        hider.show_json(&mut m);
        let text = m.to_string();
        for v in [PW, "Deep-x-Tok3n55", "Long-x-Form88", "kirisec"] {
            assert!(!text.contains(v), "{v} in {text}");
        }
        assert_eq!(m["services"]["inc"]["image"], "reg/app:${DB_PASSWORD}");
        assert_eq!(m["services"]["deep"]["image"], "reg/deep:${DEEP_TOKEN}");
        assert_eq!(m["services"]["long"]["image"], "reg/long:${API_SECRET}");
        assert!(text.contains("/sub/data/${DB_PASSWORD}"), "{text}");
        // An error that quotes the value names the variable.
        std::fs::write(d.join("sub/compose.yaml"), "services:\n  inc:\n    image: alpine\n    ports: [\"${DB_PASSWORD}:80\"]\n").unwrap();
        let e = config(dcli, &p, None, &[]).unwrap_err();
        let shown = error_json(&e, top, &[], &Hider::for_project(dcli, &p, None))["text"].as_str().unwrap().to_owned();
        assert!(!shown.contains(PW) && shown.contains("${DB_PASSWORD}"), "{shown}");
        // What `up` says of it is hidden by value.
        assert_eq!(hider.hide_up(&format!("pull reg/app:{PW}")), "pull reg/app:${DB_PASSWORD}");
        // An include the page's edit adds, not saved yet: its secrets get stand-ins too.
        std::fs::write(d.join("compose.yaml"), "services:\n  x:\n    image: alpine\n").unwrap();
        std::fs::write(d.join("sub/compose.yaml"), "services:\n  inc:\n    image: reg/app:${DB_PASSWORD}\n").unwrap();
        let q = Project { name: "incdraft".into(), ..p.clone() };
        let mut m = config(dcli, &q, Some(top), &[]).unwrap();
        let hider = Hider::for_draft(dcli, &q, None, Some(top));
        hider.show_json(&mut m);
        assert_eq!(m["services"]["inc"]["image"], "reg/app:${DB_PASSWORD}", "{m}");
        assert_eq!(hider.hide_up(&format!("pull reg/app:{PW}")), "pull reg/app:${DB_PASSWORD}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Docker's containers JSON for project `proj`: one container per
    /// (name, image, service), with a volume and a network.
    fn docker_list(proj: &str, ctrs: &[(&str, &str, &str)], volume: &str, network: &str) -> Value {
        Value::Array(ctrs.iter().map(|(n, i, svc)| json!({"Names": [format!("/{n}")], "Image": i, "Labels": {PROJECT: proj, SERVICE: svc}, "Mounts": [{"Type": "volume", "Name": volume}], "NetworkSettings": {"Networks": {network: {}}}})).collect())
    }

    fn secret_hider(proj: &str, pairs: &[(&str, &str)], containers: &Value) -> Hider {
        let vars: Vec<(String, String)> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let stands = stand_ins(&vars);
        let mut h = Hider::with(secret_values(vars), &stands, vec![proj.to_owned()]);
        h.project = proj.to_owned();
        h.running(proj, containers)
    }

    #[test]
    fn names_from_docker_are_shown_as_docker_has_them() {
        const V: &str = "Dpl0y-Tok-77aa99";
        // The list: a container's name and image as Docker has them.
        let m = Member { service: "app".into(), id: "1".into(), name: format!("x-{V}"), image: format!("reg/app:{V}"), state: "running".into(), status: "Up".into(), created: 1 };
        let v = member_view(&m);
        assert_eq!((v["name"].as_str().unwrap(), v["image"].as_str().unwrap()), (format!("x-{V}").as_str(), format!("reg/app:{V}").as_str()));
        // The plan's `from`: the running image, even one made from a value `.env` no longer has.
        let r = Running { hash: "old".into(), image_id: "i1".into(), image_now: Some("i1".into()), running: true, image: format!("reg/app:{V}") };
        let model = json!({"services": {"app": {"image": "reg/app:${DEPLOY_TOKEN}"}}});
        let now: BTreeMap<String, Running> = [("app".to_owned(), r)].into_iter().collect();
        let steps = plan_running(&model, &model, &parse_hashes("app new\n"), &now);
        assert_eq!(steps[0].image, (format!("reg/app:{V}"), "reg/app:${DEPLOY_TOKEN}".to_owned()));
        // Showing it changes nothing: no map from a real name to a variable.
        let h = secret_hider("j2ju", &[("DEPLOY_TOKEN", V)], &json!([]));
        let mut shown = plan_json(&steps);
        h.show_json(&mut shown);
        assert_eq!(shown[0]["from"], format!("reg/app:{V}"));
    }

    #[test]
    fn a_password_that_is_part_of_a_name_never_changes_a_name_from_docker() {
        // ADMIN_PASSWORD=admin with a service admin_panel: never `j2jnames-${ADMIN_PASSWORD}_panel-1`.
        let pairs = [("ADMIN_PASSWORD", "admin"), ("REDIS_PASSWORD", "redis"), ("POSTGRES_PASSWORD", "postgres"), ("SHOP_PASSWORD", "shop")];
        let ctrs = docker_list("j2jnames", &[("j2jnames-admin_panel-1", "x/admin:1", "admin_panel"), ("j2jnames-redis-cache-1", "redis:7", "redis-cache"), ("j2jnames-db-1", "postgres:16", "db")], "j2jnames_redis-data", "j2jnames_default");
        let h = secret_hider("j2jnames", &pairs, &ctrs);
        for (n, i) in [("j2jnames-admin_panel-1", "x/admin:1"), ("j2jnames-redis-cache-1", "redis:7"), ("j2jnames-db-1", "postgres:16")] {
            let m = Member { service: "s".into(), id: "1".into(), name: n.into(), image: i.into(), ..Member::default() };
            assert_eq!((member_view(&m)["name"].clone(), member_view(&m)["image"].clone()), (json!(n), json!(i)));
            for line in [format!(" Container {n} Started "), format!("Error response from daemon: Conflict. The container name \"/{n}\" is already in use"), format!("pull {i}")] {
                assert_eq!(h.hide_up(&line), line);
            }
        }
        // The project named like its secret.
        let ctrs = docker_list("shop", &[("shop-db-1", "postgres:16", "db")], "shop_data", "shop_default");
        let h = secret_hider("shop", &pairs, &ctrs);
        for line in [" Container shop-db-1 Started ", "Volume shop_data Created", "Network shop_default Created"] {
            assert_eq!(h.hide_up(line), line);
        }
    }

    #[test]
    fn free_text_keeps_dockers_names_whole_and_still_hides_a_bare_secret() {
        const V: &str = "Dpl0y-Tok-77aa99";
        let name = format!("x-{V}");
        let image = format!("reg/app:{V}");
        let ctrs = docker_list("j2ju", &[(&name, &image, "app")], &format!("data-{V}"), "j2ju_default");
        let h = secret_hider("j2ju", &[("DEPLOY_TOKEN", V)], &ctrs);
        for line in [format!(" Container {name} Creating "), format!("Error response from daemon: Conflict. The container name \"/{name}\" is already in use by container \"0123abcd\"."), format!("pulling {image}"), format!("Volume data-{V} Created")] {
            assert_eq!(h.hide_up(&line), line);
            assert_eq!(shown_error(&h, &line), line);
        }
        // The secret on its own is still hidden, next to a name too.
        assert_eq!(h.hide_up(&format!("token {V} refused for {name}")), format!("token ${{DEPLOY_TOKEN}} refused for {name}"));
        assert!(!h.hide_up(&format!("auth failed: {V}")).contains(V));
        // Without Docker's names, a piece of it never shows.
        let bare = secret_hider("j2ju", &[("DEPLOY_TOKEN", V)], &json!([]));
        assert!(!bare.hide_up(&format!("Error: value \"{}\" is bad", &V[..10])).contains("Dpl0y-Tok"));
    }

    #[test]
    fn apply_progress_goes_by_the_compose_service_label() {
        const V: &str = "Dpl0y-Tok-77aa99";
        // The warning state: the reading is the last one, its names stale.
        let m = json!({"services": {"a": {HIDDEN: true}}, UNCHECKED: "${DEPLOY_TOKEN}"});
        let mut names = apply_names("j2judotenvenvfile", &m, &m, &[]);
        let line = format!(" Container j2ju-{V} Started ");
        let mut touched = Vec::new();
        // Without the label the line can't be told; with it, it is a's.
        assert!(matches!(up_line(&line, "j2judotenvenvfile", &mut names, &mut |_| None, &Hider::default(), &mut touched), UpLine::Line(_)));
        let mut look = |n: &str| (n == format!("j2ju-{V}")).then(|| "a".to_owned());
        assert!(matches!(up_line(&line, "j2judotenvenvfile", &mut names, &mut look, &Hider::default(), &mut touched), UpLine::State(ref s, "started") if s == &["a"]));
        assert_eq!(touched, ["a"]);
        // The label wins over a name that reads as another service.
        let mut names = images_of("shop", &json!({"services": {"db": {"image": "postgres"}, "web": {"image": "nginx"}}}));
        let mut look = |_: &str| Some("web".to_owned());
        assert!(matches!(up_line(" Container shop-db-1 Started ", "shop", &mut names, &mut look, &Hider::default(), &mut touched), UpLine::State(ref s, _) if s == &["web"]));
    }

    #[test]
    fn a_secret_in_a_name_field_warns_and_one_in_the_environment_does_not() {
        let vars = vec![("DEPLOY_TOKEN".to_owned(), "Dpl0y-Tok-77aa99".to_owned()), ("TAG_SECRET".to_owned(), "Tg-s3cr3t-9".to_owned()), ("DB_PASSWORD".to_owned(), "pw-Zz-123456".to_owned())];
        let st = stand_ins(&vars);
        let t = |n: &str| st.iter().find(|s| s.name == n).unwrap().token.clone();
        let (d, g, p) = (t("DEPLOY_TOKEN"), t("TAG_SECRET"), t("DB_PASSWORD"));
        let svc = |k: &str, v: Value| json!({"services": {"a": {k: v, "environment": {"P": p.clone()}}}});
        for m in [
            svc("container_name", json!(format!("x-{d}"))),
            svc("image", json!(format!("alpine:{d}"))),
            svc("hostname", json!(format!("{d}.local"))),
            svc("volumes", json!([{"type": "bind", "source": format!("/w/{d}/data"), "target": "/d"}])),
            json!({"services": {"a": {"image": "alpine"}}, "volumes": {"v": {"name": format!("v-{d}")}}}),
            json!({"services": {"a": {"image": "alpine"}}, "networks": {"n": {"name": format!("n-{d}")}}}),
        ] {
            assert_eq!(name_vars(Some(&m), "", &st), ["${DEPLOY_TOKEN}"], "{m}");
        }
        // Only in environment:, command or a named volume's mount: no warning.
        let m = json!({"services": {"a": {"image": "alpine", "environment": {"P": p.clone()}, "command": ["echo", p.clone()], "volumes": [{"type": "volume", "source": "data", "target": "/d"}]}}});
        assert!(name_vars(Some(&m), "", &st).is_empty());
        assert_eq!(name_note(&[]), None);
        // Two: both named.
        let m = json!({"services": {"a": {"container_name": format!("x-{d}"), "image": format!("alpine:{g}")}}});
        let n = name_note(&name_vars(Some(&m), "", &st)).unwrap();
        assert_eq!(n["text"], "${DEPLOY_TOKEN} and ${TAG_SECRET} are used in a name or folder path. Docker shows those in plain text.");
        assert_eq!((n["errkey"].as_str(), n["errargs"]["names"].clone()), (Some("nameSecret"), json!(["${DEPLOY_TOKEN}", "${TAG_SECRET}"])));
        let one = name_note(&["${DEPLOY_TOKEN}".to_owned()]).unwrap();
        assert_eq!(one["text"], "${DEPLOY_TOKEN} is used in a name or folder path. Docker shows those in plain text.");
        // A reading the stand-ins couldn't make: from the file's text.
        let stale = json!({"services": {"a": {HIDDEN: true}}, UNCHECKED: "${DEPLOY_TOKEN}"});
        let text = "services:\n  a:\n    image: alpine\n    container_name: j2ju-${DEPLOY_TOKEN}\n    environment:\n      P: ${DB_PASSWORD}\n  b:\n    image: alpine:$TAG_SECRET\n    volumes:\n      - ./${DB_PASSWORD}/x:/d\n";
        assert_eq!(name_vars(Some(&stale), text, &st), ["${DEPLOY_TOKEN}", "${TAG_SECRET}", "${DB_PASSWORD}"]);
        assert!(name_vars(None, "services:\n  a:\n    image: alpine\n    environment:\n      P: ${DB_PASSWORD}\n", &st).is_empty());
    }

    #[test]
    fn a_service_kiri_cant_read_keeps_what_its_file_says_plainly() {
        let text = "name: p\nservices:\n  web:\n    image: nginx:1.27 # the site\n    ports:\n      - \"8080:80\"\n      - 127.0.0.1:9000:9000/tcp\n      - 443\n    env_file: ${APP_SECRETS}\n  api:\n    image: reg/api:${TAG_SECRET}\n    ports:\n      - ${PORT}:80\n  db:\n    image: postgres:16\nvolumes:\n  data:\n";
        let d = text_details(text);
        assert_eq!(d["web"], (Some("nginx:1.27".to_owned()), vec!["8080:80".to_owned(), "9000:9000".to_owned(), "443".to_owned()]));
        assert_eq!(d["api"], (None, vec![]));
        assert_eq!(d["db"].0.as_deref(), Some("postgres:16"));
        assert!(!d.contains_key("data"));
        let mut m = json!({"services": {"web": {HIDDEN: true}, "api": {HIDDEN: true}}});
        fill_hidden(&mut m, text);
        let v = service_view("web", &m["services"]["web"]);
        assert_eq!((v["image"].clone(), v["ports"].clone(), v["hidden"].clone()), (json!("nginx:1.27"), json!(["8080:80", "9000:9000", "443"]), json!(true)));
        assert_eq!(service_view("api", &m["services"]["api"])["image"], "");
    }

    #[test]
    fn random_secrets_outside_dockers_names_never_survive_and_names_are_never_altered() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        const CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        for round in 0..200 {
            let len = 8 + (next() % 10) as usize;
            let secret: String = (0..len).map(|_| CHARS[(next() % CHARS.len() as u64) as usize] as char).collect();
            let other: String = (0..len).map(|_| CHARS[(next() % CHARS.len() as u64) as usize] as char).collect();
            let sep = ["-", "_", "."][(next() % 3) as usize];
            let word = ["x", "app", "data", "v1", "cache"][(next() % 5) as usize];
            let name = format!("{word}{sep}{secret}");
            let image = format!("reg/{word}:v2{sep}{secret}");
            let ctrs = docker_list("proj", &[(&name, &image, "svc")], &format!("proj_{word}"), "proj_default");
            let h = secret_hider("proj", &[("DEPLOY_TOKEN", &secret), ("API_TOKEN", &other)], &ctrs);
            // Docker's names, never altered.
            for line in [format!(" Container {name} Creating "), format!("Error response from daemon: Conflict. The container name \"/{name}\" is already in use"), format!("pulling {image}")] {
                assert_eq!(h.hide_up(&line), line, "round {round}");
            }
            // A secret no name of Docker's has: never shown, however it is joined.
            for s in [&other, &secret] {
                // Whole, or a piece Docker cuts out and quotes back.
                let cut: String = s.chars().skip(1).take(7).collect();
                for line in [format!("auth {s} refused"), format!("bad value \"{s}\""), format!("token={s}"), format!("manifest for reg/{word}:{s} not found"), format!("bad tag \"{cut}\""), format!("unknown user: {cut}: no matching entries")] {
                    let shown = h.hide_up(&line);
                    let low = shown.to_ascii_lowercase();
                    let mut rest = low.clone();
                    for n in [&name, &image] {
                        rest = rest.replace(&n.to_ascii_lowercase(), "");
                    }
                    assert!(!rest.contains(&s.to_ascii_lowercase()), "round {round}: {shown} (joined with {sep})");
                }
            }
        }
    }
    #[test]
    fn logs_are_asked_for_by_the_projects_name_alone() {
        let a = logs_args("shop");
        assert_eq!(a, ["compose", "--ansi", "never", "-p", "shop", "logs", "--no-color", "--timestamps", "--tail", "200"]);
        assert!(!a.iter().any(|x| x == "-f" || x == "--env-file" || x == "--project-directory"));
    }

    #[test]
    fn no_stand_in_token_is_ever_shown() {
        let t = token("UNTOKEN_SECRET");
        let p = token("UNTOKEN_DB_URL password");
        let text = format!("parsing \"{t}\": invalid syntax; {} and postgres://u:{p}@db and KIRISEC0000dead999999x", t.to_ascii_uppercase());
        let shown = untoken(&text);
        assert!(!shown.to_ascii_lowercase().contains("kirisec"), "{shown}");
        assert_eq!(shown, "parsing \"${UNTOKEN_SECRET}\": invalid syntax; ${UNTOKEN_SECRET} and postgres://u:${UNTOKEN_DB_URL password}@db and ${…}x");
        assert_eq!(untoken("plain text"), "plain text");
        // A name of the person's own that starts the same way stays.
        for n in ["kirisecure", "/w/kirisecrets/compose.yaml", "kirisec0000dead99", "kirisec"] {
            assert_eq!(untoken(n), n);
        }
        // In an event's JSON too: it stays JSON.
        let ev = json!({"type": "composelogs", "error": format!("bad {t}")}).to_string();
        assert_eq!(serde_json::from_str::<Value>(&untoken(&ev)).unwrap()["error"], "bad ${UNTOKEN_SECRET}");
    }

    #[test]
    fn a_file_kiri_can_only_check_with_secrets_hidden_keeps_its_own_name() {
        let vars = vec![("STAGE".to_owned(), "dev".to_owned()), ("DB_PASSWORD".to_owned(), "pw-x-77".to_owned())];
        assert_eq!(written_name("name: shop\n", &vars).as_deref(), Some("shop"));
        assert_eq!(written_name("name: shop-${STAGE}\n", &vars).as_deref(), Some("shop-dev"));
        assert_eq!(written_name("name: shop-${NONE:-x}\n", &vars).as_deref(), Some("shop-x"));
        assert_eq!(written_name("name: shop-${DB_PASSWORD}\n", &vars), None);
        assert_eq!(written_name("name: shop-${NONE}\n", &vars), None);
        let d = tdir("ownname");
        std::fs::write(d.join(".env"), "APP_SECRETS=app.env\n").unwrap();
        std::fs::write(d.join("app.env"), "A=1\n").unwrap();
        std::fs::write(d.join("compose.yaml"), "name: myshop\nservices:\n  x:\n    image: alpine\n    env_file: ${APP_SECRETS}\n").unwrap();
        let p = Project { name: "ownname".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        let m = config(dcli, &Project { name: String::new(), ..p.clone() }, None, &[]).unwrap();
        assert_eq!(m[UNCHECKED], "${APP_SECRETS}");
        assert_eq!(opened_name(dcli, &p), "myshop");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The included project the include tests read: its secret in a bind
    /// mount's source and a container's name, from its folder's `.env`.
    const INC: &str = "services:\n  db:\n    image: \"postgres:16\"\n    container_name: \"db-${DB_PASSWORD}\"\n    volumes: [\"./data/${DB_PASSWORD}:/var/lib/postgresql/data\"]\n";
    const INC_PW: &str = "FAKEsubPw-aa11";

    fn inc_sub(d: &Path) {
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/compose.yaml"), INC).unwrap();
        std::fs::write(d.join("sub/.env"), format!("DB_PASSWORD={INC_PW}\n")).unwrap();
    }

    /// The list and the plan of a project whose top file is `yaml`, made by
    /// `extra` in its folder: what they show, with whether Compose accepts it.
    fn inc_window(tag: &str, yaml: &str, extra: &dyn Fn(&Path)) -> (String, bool) {
        let d = tdir(tag);
        std::fs::write(d.join("compose.yaml"), yaml).unwrap();
        extra(&d);
        let p = Project { name: tag.into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        let (shown, _) = window_json(dcli, &p, yaml);
        let real = real_passes(dcli, &p, None, &[]);
        let _ = std::fs::remove_dir_all(&d);
        (untoken(&shown), real == Ok(true))
    }

    fn no_inc_secret(tag: &str, shown: &str, secrets: &[&str]) {
        let lower = shown.to_lowercase();
        for v in secrets.iter().copied().chain(["kirisec", "fakesub", "subpw", "zz9secret"]) {
            assert!(!lower.contains(&v.to_lowercase()), "{tag}: {v} shown in {shown}");
        }
    }

    #[test]
    fn an_included_secret_never_shows_however_the_include_is_written() {
        let tail = "services:\n  x:\n    image: alpine\n";
        let mut forms: Vec<(&str, String)> = [
            ("plain", "include:\n  - ./sub/compose.yaml\n"),
            ("dash0", "include:\n- ./sub/compose.yaml\n"),
            ("dash0long", "include:\n- path: ./sub/compose.yaml\n"),
            ("bom", "\u{feff}include:\n  - ./sub/compose.yaml\n"),
            ("flowmap", "include: [ {path: ./sub/compose.yaml} ]\n"),
            ("flowitem", "include:\n  - {path: ./sub/compose.yaml}\n"),
            ("quotedkey", "include:\n  - \"path\": ./sub/compose.yaml\n"),
            ("quotedinclude", "\"include\":\n  - ./sub/compose.yaml\n"),
            ("var", "include:\n  - ${SUBDIR:-sub}/compose.yaml\n"),
            ("dashnl", "include:\n  -\n    path: ./sub/compose.yaml\n"),
            ("mlflow", "include:\n  [\n    ./sub/compose.yaml\n  ]\n"),
            ("anchor", "x-inc: &inc\n  - ./sub/compose.yaml\ninclude: *inc\n"),
            ("folded", "include:\n  - path: >-\n      ./sub/compose.yaml\n"),
            ("tag", "include:\n  - !!str ./sub/compose.yaml\n"),
            ("comment", "include: # the db\n  - ./sub/compose.yaml # here\n"),
            ("spacecolon", "include :\n  - ./sub/compose.yaml\n"),
            ("longenv", "include:\n  - path: ./sub/compose.yaml\n    env_file: ./sub/.env\n"),
            ("longenvlist", "include:\n  - path:\n      - ./sub/compose.yaml\n    env_file:\n      - ./sub/.env\n"),
            ("projdir", "include:\n  - path: ./sub/compose.yaml\n    project_directory: ./sub\n"),
            ("dotdot", "include:\n  - ./sub/../sub/compose.yaml\n"),
        ].into_iter().map(|(t, h)| (t, format!("{h}{tail}"))).collect();
        forms.push(("indroot", "  include:\n    - ./sub/compose.yaml\n  services:\n    x:\n      image: alpine\n".into()));
        forms.push(("crlf", "include:\r\n  - ./sub/compose.yaml\r\nservices:\r\n  x:\r\n    image: alpine\r\n".into()));
        assert_eq!(forms.len(), 22);
        for (tag, yaml) in &forms {
            let (shown, real) = inc_window(&format!("if-{tag}"), yaml, &inc_sub);
            assert!(real, "{tag}: Compose itself refuses the fixture");
            no_inc_secret(tag, &shown, &[INC_PW]);
            // The included service is listed; by a variable path, the project by name.
            assert!(shown.contains("\"db\"") || (*tag == "var" && shown.contains("byName")), "{tag}: {shown}");
        }
    }

    #[test]
    fn an_included_secret_never_shows_past_what_kiri_reads_of_includes() {
        // A chain 5 and 6 deep, the secret at its end.
        for depth in [5usize, 6] {
            let (shown, real) = inc_window(&format!("il-depth{depth}"), "include:\n  - ./l1/compose.yaml\nservices:\n  x:\n    image: alpine\n", &|d: &Path| {
                let mut dir = d.to_path_buf();
                for i in 1..=depth {
                    dir = dir.join(format!("l{i}"));
                    std::fs::create_dir_all(&dir).unwrap();
                    if i < depth {
                        std::fs::write(dir.join("compose.yaml"), format!("include:\n  - ./l{}/compose.yaml\nservices:\n  s{i}:\n    image: alpine\n", i + 1)).unwrap();
                    } else {
                        std::fs::write(dir.join("compose.yaml"), INC).unwrap();
                        std::fs::write(dir.join(".env"), format!("DB_PASSWORD={INC_PW}\n")).unwrap();
                    }
                }
            });
            assert!(real);
            no_inc_secret(&format!("depth {depth}"), &shown, &[INC_PW]);
        }
        let top = "include:\n  - ./sub/compose.yaml\nservices:\n  x:\n    image: alpine\n";
        // A Latin-1 byte in the include's `.env`.
        let (shown, _) = inc_window("il-latin1", top, &|d: &Path| {
            inc_sub(d);
            let mut b = b"# caf\xe9\n".to_vec();
            b.extend(format!("DB_PASSWORD={INC_PW}\n").as_bytes());
            std::fs::write(d.join("sub/.env"), b).unwrap();
        });
        no_inc_secret("latin-1", &shown, &[INC_PW]);
        // A 2 MiB `.env`, the secret at its end.
        let (shown, _) = inc_window("il-big", top, &|d: &Path| {
            inc_sub(d);
            let mut t = String::new();
            for i in 0..60000 {
                t.push_str(&format!("PLAIN_VAR_{i}=some-plain-value-{i}\n"));
            }
            assert!(t.len() > 2 << 20);
            t.push_str(&format!("DB_PASSWORD={INC_PW}\n"));
            std::fs::write(d.join("sub/.env"), t).unwrap();
        });
        no_inc_secret("2 MiB", &shown, &[INC_PW]);
        // 40 included files, each secret in a container's name.
        let mut y = String::from("include:\n");
        for i in 0..40 {
            y.push_str(&format!("  - ./m{i}/compose.yaml\n"));
        }
        y.push_str("services:\n  x:\n    image: alpine\n");
        let (shown, real) = inc_window("il-many", &y, &|d: &Path| {
            for i in 0..40 {
                let m = d.join(format!("m{i}"));
                std::fs::create_dir_all(&m).unwrap();
                std::fs::write(m.join("compose.yaml"), format!("services:\n  s{i}:\n    image: alpine\n    container_name: \"c-${{K{i}_PASSWORD}}\"\n")).unwrap();
                std::fs::write(m.join(".env"), format!("K{i}_PASSWORD=FAKEmany{i}-Zz9Secret\n")).unwrap();
            }
        });
        assert!(real);
        no_inc_secret("40 includes", &shown, &["FAKEmany"]);
        assert!(shown.contains("\"s39\""), "{shown}");
    }

    #[test]
    fn a_file_compose_cant_list_the_variables_of_is_managed_by_name() {
        // `config --variables` can't follow an include path with a variable
        // in it: Kiri reads nothing more of the file, not even its services.
        let (shown, _) = inc_window("il-varpath", "include:\n  - ${SUBDIR:-sub}/compose.yaml\nservices:\n  x:\n    image: alpine\n", &inc_sub);
        no_inc_secret("var path", &shown, &[INC_PW]);
        assert!(shown.contains(BY_NAME) && shown.contains("byName") && !shown.contains("\"db\""), "{shown}");
        assert!(!shown.contains("/data/"), "no details of a reading without every name: {shown}");
        // The general warning names no variable, so none the file doesn't use.
        assert!(!shown.contains("hiddenCheck") && !shown.contains("${…}") && !shown.contains("${DB_PASSWORD}"), "{shown}");
    }

    #[test]
    fn compose_s_error_is_never_passed_on_without_every_variables_name() {
        // An include path with a variable: Compose can't list the variables,
        // and its error quotes the included secret back.
        for (tag, field) in [("port", "    ports: [\"${DB_PASSWORD}:5432\"]\n"), ("cpus", "    cpus: \"${DB_PASSWORD}\"\n"), ("deps", "    depends_on: [\"${DB_PASSWORD}\"]\n")] {
            let (shown, _) = inc_window(&format!("il-rej-{tag}"), "include:\n  - ${SUBDIR:-sub}/compose.yaml\nservices:\n  x:\n    image: alpine\n", &|d: &Path| {
                std::fs::create_dir_all(d.join("sub")).unwrap();
                std::fs::write(d.join("sub/compose.yaml"), format!("services:\n  db:\n    image: postgres:16\n{field}")).unwrap();
                std::fs::write(d.join("sub/.env"), "DB_PASSWORD=FAKErej77zz\n").unwrap();
            });
            no_inc_secret(tag, &shown, &["FAKErej77zz", "rej77"]);
            assert!(shown.contains(BY_NAME) && shown.contains("byName"), "{tag}: {shown}");
        }
        assert_eq!(error_key(BY_NAME).unwrap().0, "byName");
        // A file that isn't YAML: Compose's words, read before any variable, with the line.
        let top = "include:\n  - ${SUBDIR:-sub}/compose.yaml\nservices:\n  x:\n    image: alpine\n   bad: [\n";
        let (shown, _) = inc_window("il-rej-yaml", top, &inc_sub);
        assert!(!shown.contains(BY_NAME) && !shown.contains("did not find expected key") && shown.contains("yamlLine") && shown.contains("\"line\":6"), "{shown}");
        no_inc_secret("yaml", &shown, &[INC_PW]);
    }

    #[test]
    fn a_name_docker_reports_with_a_known_secret_value_is_warned_of() {
        let st = |n: &str, v: &str| Stand { name: n.into(), value: String::new(), token: String::new(), real: v.into(), label: format!("${{{n}}}") };
        let stands = [st("A_SECRET", "Hunter22x"), st("SHORT_TOKEN", "abc12"), st("B_TOKEN", "zz9q8w7e")];
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(value_vars(&names(&["p-hunter22x", "p-hunter22x-x-1"]), &stands, &[]), ["${A_SECRET}"]);
        for n in ["alpine:Hunter22x", "reg/hunter22x/app", "Hunter22x", "x_Hunter22x.y", "/data-x/Hunter22x"] {
            assert_eq!(value_vars(&names(&[n]), &stands, &[]), ["${A_SECRET}"], "{n}");
        }
        // Inside a longer word too; a value under 6 characters: no warning.
        assert_eq!(value_vars(&names(&["xhunter22xy", "zz9q8w7e9"]), &stands, &[]), ["${A_SECRET}", "${B_TOKEN}"]);
        assert!(value_vars(&names(&["abc12-web-1"]), &stands, &[]).is_empty());
        // A value that is a name the project declares (an image's): no warning.
        assert!(value_vars(&names(&["postgres:16-alpine"]), &[st("POSTGRES_PASSWORD", "postgres")], &names(&["postgres"])).is_empty());
        let containers = json!([{"Labels": {PROJECT: "p-hunter22x"}, "Names": ["/web"], "Image": "alpine", "Mounts": [{"Type": "bind", "Source": "/srv/data-zz9q8w7e"}]}]);
        let n = docker_names("p-hunter22x", &containers);
        assert_eq!(value_vars(&n, &stands, &[]), ["${A_SECRET}", "${B_TOKEN}"]);
        let note = name_note(&value_vars(&n, &stands, &[])).unwrap();
        assert_eq!(note["errkey"], "nameSecret");
        assert_eq!(note["text"], "${A_SECRET} and ${B_TOKEN} are used in a name or folder path. Docker shows those in plain text.");
        assert_eq!(name_note(&["${X}".to_owned()]).unwrap()["text"], "${X} is used in a name or folder path. Docker shows those in plain text.");
    }

    #[test]
    fn a_common_word_or_a_name_raises_no_value_warning() {
        let st = |n: &str, v: &str| Stand { name: n.into(), value: String::new(), token: String::new(), real: v.into(), label: format!("${{{n}}}") };
        let ran = |p: &str, names: &[&str], image: &str| docker_names(p, &json!([{"Labels": {PROJECT: p}, "Names": names, "Image": image}]));
        // A running image the file no longer has: the plain word is still no secret.
        assert!(value_vars(&ran("p", &["/p-pg-1"], "postgres:16-alpine"), &[st("POSTGRES_PASSWORD", "postgres")], &[]).is_empty());
        assert!(value_vars(&ran("p", &["/p-cache-1"], "redis:7"), &[st("REDIS_PASSWORD", "redis")], &[]).is_empty());
        assert!(value_vars(&ran("shop", &["/shop-web-1"], "nginx"), &[st("X_PASSWORD", "shop")], &[]).is_empty());
        // A telling value joined into a name: warned of.
        assert_eq!(value_vars(&ran("p", &["/p-Dpl0y-Tok-77aa99-x-1"], "alpine"), &[st("A_SECRET", "Dpl0y-Tok-77aa99")], &[]), ["${A_SECRET}"]);
        // Twelve plain letters or more: telling too.
        assert_eq!(value_vars(&ran("p", &["/p-quietmeadowfox-1"], "alpine"), &[st("B_TOKEN", "quietmeadowfox")], &[]), ["${B_TOKEN}"]);
        assert!(!telling("postgres") && !telling("Redis") && telling("Redis1") && telling("Ab-cd!") && !telling("Ab1"));
        // A telling value that is just a name the file writes: an image's
        // literal tag, a part of its path, a service.
        let text = "name: shop\nservices:\n  app-core2:\n    image: \"ghcr.io/acme/svc2-api:Rel-2024x9\" # pinned\n  web:\n    image: nginx:${TAG}\n";
        let fnames = file_names(text);
        let run = ran("shop", &["/shop-app-core2-1"], "ghcr.io/acme/svc2-api:Rel-2024x9");
        for (var, v) in [("X_TOKEN", "Rel-2024x9"), ("Y_TOKEN", "svc2-api"), ("Z_TOKEN", "app-core2")] {
            assert_eq!(value_vars(&run, &[st(var, v)], &[]), [format!("${{{var}}}")], "{v} counts without the file");
            assert!(value_vars(&run, &[st(var, v)], &fnames).is_empty(), "{v}: {fnames:?}");
        }
        // A tag through a variable is not written literally.
        assert!(!fnames.iter().any(|n| n.contains('$') || n == "TAG"), "{fnames:?}");
        assert_eq!(value_vars(&ran("shop", &["/shop-web-1"], "nginx:Rel-2024y8"), &[st("TAG", "Rel-2024y8")], &fnames), ["${TAG}"]);
    }

    #[test]
    fn a_service_recreated_for_a_variable_alone_says_so() {
        let d = tdir("varsch");
        let f = d.join("compose.yaml");
        std::fs::write(&f, "services: {}\n").unwrap();
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64;
        let step = || vec![Step { service: "db".into(), act: "chg", what: vec!["outside"], image: (String::new(), String::new()) }];
        let m = |created| vec![Member { service: "db".into(), created, ..Member::default() }];
        let mut s = step();
        vars_changed(&mut s, true, std::slice::from_ref(&f), &m(now + 5));
        assert_eq!(s[0].what, ["vars"]);
        // The file changed after the container was made, or the draft is an edit: outside.
        let mut s = step();
        vars_changed(&mut s, true, std::slice::from_ref(&f), &m(now - 100));
        assert_eq!(s[0].what, ["outside"]);
        let mut s = step();
        vars_changed(&mut s, false, std::slice::from_ref(&f), &m(now + 5));
        assert_eq!(s[0].what, ["outside"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_required_secret_set_nowhere_is_a_file_error_and_a_default_shows() {
        let d = tdir("requnset");
        let p = Project { name: "requnset".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        let e = config(dcli, &p, Some("services:\n  x:\n    image: \"alpine:${Q9_TOKEN:?need a token}\"\n"), &[]).unwrap_err();
        assert_eq!(e.text, "Compose rejects this file. A required variable may not be set: ${Q9_TOKEN}.");
        assert_eq!(error_key(&e).unwrap(), ("reqUnset", json!({"name": "${Q9_TOKEN}"})));
        // Two, and one with `?` alone: each named; never "not set" as a fact.
        let e = config(dcli, &p, Some("services:\n  x:\n    image: \"alpine:${Q9_TOKEN?x}\"\n    environment: [\"A=${R8_SECRET:?y}\"]\n"), &[]).unwrap_err();
        assert_eq!(error_key(&e).unwrap(), ("reqUnset", json!({"name": "${Q9_TOKEN}, ${R8_SECRET}"})), "{e}");
        // A default is never read in: the variable stands, as `${NAME}`.
        let m = config(dcli, &p, Some("services:\n  x:\n    image: \"alpine:${Q9_TOKEN:-3.21}\"\n"), &[]).unwrap();
        let shown = untoken(&Hider::for_draft(dcli, &p, None, Some("")).show(&m.to_string()));
        assert!(shown.contains("alpine:${Q9_TOKEN}") && !shown.contains("3.21"), "{shown}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_alias_or_block_scalar_is_not_read_as_a_written_value() {
        let t = text_details("x-img: &img alpine\nservices:\n  a:\n    image: *img\n  b:\n    image: >-\n      alpine\n  c:\n    image: !!str alpine\n  d:\n    image: \"redis:7\"\n");
        assert_eq!(t["a"].0, None);
        assert_eq!(t["b"].0, None);
        assert_eq!(t["c"].0, None);
        assert_eq!(t["d"].0.as_deref(), Some("redis:7"));
    }

    /// A Compose too old for `config --variables --format json`: its table.
    fn old_cli() -> io::Result<Command> {
        let d = std::env::temp_dir().join(format!("kiri-oldcli-{}", std::process::id()));
        std::fs::create_dir_all(&d)?;
        let f = d.join("docker");
        if !f.exists() {
            let tmp = d.join("docker.tmp");
            std::fs::write(&tmp, "#!/bin/sh\ncase \"$*\" in\n*--variables*--format*) echo 'unknown flag: --format' >&2; exit 1;;\n*--variables*) printf 'NAME                REQUIRED            DEFAULT VALUE       ALTERNATE VALUE\\nDB_PASSWORD         false                                   \\nQ_TOKEN             true                                    \\nD_TOKEN             false               dflt                \\n';;\n*) exit 1;;\nesac\n")?;
            #[cfg(unix)]
            std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
            std::fs::rename(&tmp, &f)?;
        }
        Ok(Command::new(f))
    }

    #[test]
    fn an_older_compose_s_variable_table_is_read() {
        let t = "NAME                REQUIRED            DEFAULT VALUE       ALTERNATE VALUE\nQ_TOKEN             true                                    \nD_TOKEN             false               dflt                \n";
        let v = variables_table(t).unwrap();
        assert_eq!(v, [Var { name: "Q_TOKEN".into(), required: true, default: false }, Var { name: "D_TOKEN".into(), required: false, default: true }]);
        assert_eq!(variables_table("something else\n"), None);
        if cfg!(unix) {
            let d = tdir("oldcli");
            std::fs::write(d.join("compose.yaml"), "services: {}\n").unwrap();
            let p = Project { name: "oldcli".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
            let names: Vec<String> = compose_names(old_cli, &p, None).expect("the table's names").into_iter().map(|v| v.name).collect();
            assert_eq!(names, ["DB_PASSWORD", "Q_TOKEN", "D_TOKEN"]);
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn no_include_layout_shows_an_included_secret() {
        // A small generator: random depth, form, env file encoding and size.
        let mut seed: u64 = std::process::id() as u64 ^ 0x9e37_79b9_7f4a_7c15;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let heads = ["include:\n  - ./{}/compose.yaml\n", "include:\n- ./{}/compose.yaml\n", "include: [ {path: ./{}/compose.yaml} ]\n", "\"include\":\n  - path: ./{}/compose.yaml\n", "include :\n  -\n    path: >-\n      ./{}/compose.yaml\n", "x-i: &i\n  - ./{}/compose.yaml\ninclude: *i\n"];
        for round in 0..10 {
            let depth = 1 + next(7);
            let picks: Vec<usize> = (0..=depth).map(|_| next(heads.len())).collect();
            let (latin, bom, big) = (next(2) == 0, next(2) == 0, next(4) == 0);
            let secret = format!("FAKErand{round}x{}-Zz9Secret", next(100000));
            let top = format!("{}services:\n  x:\n    image: alpine\n", heads[picks[0]].replace("{}", "r1"));
            let s2 = secret.clone();
            let (shown, real) = inc_window(&format!("ip-{round}"), &top, &|d: &Path| {
                let mut dir = d.to_path_buf();
                for i in 1..=depth {
                    dir = dir.join(format!("r{i}"));
                    std::fs::create_dir_all(&dir).unwrap();
                    if i < depth {
                        std::fs::write(dir.join("compose.yaml"), format!("{}services:\n  s{i}:\n    image: alpine\n", heads[picks[i]].replace("{}", &format!("r{}", i + 1)))).unwrap();
                    } else {
                        std::fs::write(dir.join("compose.yaml"), "services:\n  deep:\n    image: \"reg.example.com/app:${DEEP_TOKEN}\"\n    container_name: \"d-${DEEP_TOKEN}\"\n").unwrap();
                        let mut b = Vec::new();
                        if bom {
                            b.extend(BOM);
                        }
                        if latin {
                            b.extend(b"# na\xefve\n");
                        }
                        if big {
                            b.extend("PLAIN=x\n".repeat(160_000).as_bytes());
                        }
                        b.extend(format!("DEEP_TOKEN={s2}\n").as_bytes());
                        std::fs::write(dir.join(".env"), b).unwrap();
                    }
                }
            });
            if real {
                no_inc_secret(&format!("round {round}: depth {depth} {picks:?} latin {latin} bom {bom} big {big}"), &shown, &[&secret]);
            }
        }
    }

    #[test]
    fn an_included_secret_under_the_projects_plain_value_gets_a_stand_in() {
        let vars = [("DB_PASSWORD".to_owned(), "Inc-x-9q9q9".to_owned()), ("DB_PASSWORD".to_owned(), String::new()), ("PORT_TOKEN".to_owned(), "Inc-x-8p8p8".to_owned()), ("PORT_TOKEN".to_owned(), "8080".to_owned())];
        let own: BTreeSet<String> = ["DB_PASSWORD".to_owned(), "PORT_TOKEN".to_owned()].into();
        let got: Vec<(String, String)> = stand_ins_named(&vars, &BTreeSet::new(), &own).into_iter().map(|s| (s.name, s.real)).collect();
        assert_eq!(got, [("DB_PASSWORD".to_owned(), "Inc-x-9q9q9".to_owned()), ("PORT_TOKEN".to_owned(), "Inc-x-8p8p8".to_owned())]);
        // A plain value an include's env file alone gives earns nothing: another include may hold the secret.
        for v in ["5432", "/run/secrets/pw", ""] {
            let vars = [("DB_PASSWORD".to_owned(), v.to_owned())];
            let got: Vec<String> = stand_ins_named(&vars, &BTreeSet::new(), &BTreeSet::new()).into_iter().map(|s| s.name).collect();
            assert_eq!(got, ["DB_PASSWORD"], "{v:?}");
            assert!(stand_ins_named(&vars, &BTreeSet::new(), &["DB_PASSWORD".to_owned()].into()).is_empty(), "{v:?}: the project's own");
        }
    }

    #[test]
    fn two_includes_with_one_secret_name_never_show_the_hidden_one() {
        const SUB: &str = "FAKEsubPw-aa11";
        for (tag, top, env1) in [
            ("ph-alias", "x-two: &two ./sub2/compose.yaml\ninclude:\n  - ./sub1/compose.yaml\n  - *two\nservices:\n  x:\n    image: alpine\n", "DB_PASSWORD=5432\n"),
            ("ph-alias-path", "x-two: &two ./sub2/compose.yaml\ninclude:\n  - ./sub1/compose.yaml\n  - *two\nservices:\n  x:\n    image: alpine\n", "DB_PASSWORD=/run/secrets/pw\n"),
            ("ph-alias-empty", "x-two: &two ./sub2/compose.yaml\ninclude:\n  - ./sub1/compose.yaml\n  - *two\nservices:\n  x:\n    image: alpine\n", "DB_PASSWORD=\n"),
        ] {
            let (shown, _) = inc_window(tag, top, &|d: &Path| {
                for s in ["sub1", "sub2"] {
                    std::fs::create_dir_all(d.join(s)).unwrap();
                }
                std::fs::write(d.join("sub1/.env"), env1).unwrap();
                std::fs::write(d.join("sub1/compose.yaml"), "services:\n  a:\n    image: postgres:16\n    environment: [\"P=${DB_PASSWORD}\"]\n").unwrap();
                std::fs::write(d.join("sub2/.env"), format!("DB_PASSWORD={SUB}\n")).unwrap();
                std::fs::write(d.join("sub2/compose.yaml"), "services:\n  b:\n    image: \"postgres:${DB_PASSWORD}\"\n    container_name: \"b-${DB_PASSWORD}\"\n    volumes: [\"./data/${DB_PASSWORD}:/var/lib/x\"]\n").unwrap();
            });
            no_inc_secret(tag, &shown, &[SUB, "FAKEsub", "aa11"]);
        }
    }

    #[test]
    fn untoken_keeps_the_event_json_whatever_the_variable_is_called() {
        let t = token("we\"ird\\_TOKEN");
        let v = json!({"text": format!("left {t} here")});
        let back = untoken_json(v);
        assert_eq!(back["text"], "left ${we\"ird\\_TOKEN} here");
        let ev = untoken_event(&json!({"type": "x", "text": format!("a {t}"), "n": [t.clone()]}).to_string());
        let parsed: Value = serde_json::from_str(&ev).expect("the event still parses");
        assert_eq!(parsed["n"][0], "${we\"ird\\_TOKEN}");
    }

    #[cfg(unix)]
    #[test]
    fn project_actions_and_logs_run_in_a_fresh_private_empty_folder() {
        fn out() -> PathBuf {
            std::env::temp_dir().join(format!("kiri-emptydir-{}", std::process::id()))
        }
        fn rec() -> io::Result<Command> {
            let mut c = Command::new("sh");
            c.args(["-c", &format!("{{ pwd; ls -A; ls -ld .; }} >> '{}'", out().display()), "sh"]);
            Ok(c)
        }
        let _ = std::fs::remove_file(out());
        action(rec, "someproj", "stop").unwrap();
        let _ = logs_out(rec, "someproj");
        // Not a reading another test kept.
        ENVS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let _ = env_vars(rec, &Project { name: "someproj".into(), ..Project::default() });
        let got = std::fs::read_to_string(out()).unwrap();
        let _ = std::fs::remove_file(out());
        let lines: Vec<&str> = got.lines().collect();
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let mut seen = BTreeSet::new();
        for run in lines.chunks(2) {
            let dir = PathBuf::from(run[0]);
            assert_ne!(std::fs::canonicalize(&dir).unwrap_or(dir.clone()), tmp, "not the shared temp folder: {got}");
            assert!(run[1].starts_with("drwx------"), "private and empty: {got}");
            assert!(!dir.exists(), "removed after: {got}");
            seen.insert(dir);
        }
        assert_eq!(seen.len(), 3, "a fresh one each call: {got}");
    }
    /// Remove project takes a project opened from its file off the list too:
    /// its containers go, and so does its row. The file itself isn't touched.
    #[cfg(unix)]
    #[test]
    fn removing_a_project_opened_from_its_file_takes_it_off_the_list() {
        static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        fn dir() -> PathBuf {
            std::env::temp_dir().join(format!("kiri-rm-opened-{}", std::process::id()))
        }
        fn store() -> Option<PathBuf> {
            Some(dir().join("compose.json"))
        }
        fn passes() -> io::Result<Command> {
            let mut c = Command::new("sh");
            c.args(["-c", "exit 0", "sh"]);
            Ok(c)
        }
        fn emit(s: String) {
            SAID.lock().unwrap().push(s)
        }
        std::fs::create_dir_all(dir()).unwrap();
        let file = dir().join("compose.yaml");
        std::fs::write(&file, "services:\n  web:\n    image: alpine\n").unwrap();
        save_opened(store, |_, _| true, Some(("shop", file.to_str().unwrap()))).unwrap();
        assert_eq!(opened(store).len(), 1, "listed once opened");
        handle("composeaction", "shop".into(), &json!({"action": "remove"}), passes, store, emit);
        // handle() answers from a thread of its own: the list comes last.
        for _ in 0..400 {
            if SAID.lock().unwrap().iter().any(|s| s.contains("composeprojects")) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let said = SAID.lock().unwrap().clone();
        assert!(said.iter().any(|s| s.contains("\"removed\":true")), "said removed: {said:?}");
        assert!(said.iter().any(|s| s.contains("composeprojects")), "and the list again: {said:?}");
        assert!(opened(store).is_empty(), "off the list");
        assert!(file.exists(), "the compose file stays");
        let _ = std::fs::remove_dir_all(dir());
    }
    /// The include forms Kiri's own reader may not follow, each top file
    /// with the included project's secret in `sub/.env` (or its env file).
    fn include_forms() -> Vec<(&'static str, String)> {
        let tail = "services:\n  x:\n    image: alpine\n";
        vec![
            ("plain", format!("include:\n  - ./sub/compose.yaml\n{tail}")),
            ("anchor", format!("x-inc: &inc\n  - ./sub/compose.yaml\ninclude: *inc\n{tail}")),
            ("envfile-long", format!("include:\n  - path: ./sub/compose.yaml\n    env_file: ./sub/secrets.env\n{tail}")),
            ("envfile-merge", format!("x-e: &e\n  env_file: ./sub/secrets.env\ninclude:\n  - path: ./sub/compose.yaml\n    <<: *e\n{tail}")),
            ("envfile-folded", format!("include:\n  - path: ./sub/compose.yaml\n    env_file: >-\n      ./sub/secrets.env\n{tail}")),
            ("envfile-escape", format!("include:\n  - path: ./sub/compose.yaml\n    env_file: \"./sub/secr\\x65ts.env\"\n{tail}")),
            ("envfile-alias", format!("x-f: &f ./sub/secrets.env\ninclude:\n  - path: ./sub/compose.yaml\n    env_file: *f\n{tail}")),
            ("envfile-var", format!("include:\n  - path: ./sub/compose.yaml\n    env_file: ./sub/${{ENVNAME:-secrets}}.env\n{tail}")),
            ("json", "{\"include\":[{\"path\":\"./sub/compose.yaml\",\"env_file\":\"./sub/secrets.env\"}],\"services\":{\"x\":{\"image\":\"alpine\"}}}\n".to_owned()),
            ("path-alias", format!("x-p: &p ./sub/compose.yaml\ninclude:\n  - *p\n{tail}")),
            ("path-tagged", format!("include:\n  - !!str ./sub/compose.yaml\n{tail}")),
        ]
    }

    #[test]
    fn a_secret_with_a_default_or_required_marker_always_stands_in() {
        let forms = include_forms();
        assert_eq!(forms.len(), 11);
        for (form, top) in &forms {
            for (kind, r) in [("default", "${DB_PASSWORD:-dflt}"), ("dash", "${DB_PASSWORD-dflt}"), ("required", "${DB_PASSWORD:?need}"), ("req2", "${DB_PASSWORD?need}"), ("bare", "${DB_PASSWORD}")] {
                let tag = format!("mk-{form}-{kind}");
                let (shown, real) = inc_window(&tag, top, &|d: &Path| {
                    std::fs::create_dir_all(d.join("sub")).unwrap();
                    let env = if form.starts_with("envfile") || *form == "json" { "sub/secrets.env" } else { "sub/.env" };
                    std::fs::write(d.join(env), format!("DB_PASSWORD={INC_PW}\n")).unwrap();
                    std::fs::write(d.join("sub/compose.yaml"), format!("services:\n  db:\n    image: \"postgres:{r}\"\n    volumes: [\"./data/{r}:/var/lib/x\"]\n")).unwrap();
                });
                assert!(real, "{tag}: Compose itself refuses the fixture");
                no_inc_secret(&tag, &shown, &[INC_PW]);
                // Never its default either: the variable as `${NAME}`.
                assert!(!shown.contains("dflt"), "{tag}: {shown}");
            }
        }
    }

    #[test]
    fn a_file_the_real_values_reject_says_so_without_composes_words() {
        let d = tdir("realrej");
        let p = Project { name: "realrej".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        // An image of a secret Kiri holds nowhere: empty with the real values.
        let e = config(dcli, &p, Some("services:\n  x:\n    image: \"${Q7_TOKEN}\"\n"), &[]).unwrap_err();
        assert_eq!(error_key(&e).unwrap().0, "rejectReal", "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The names-only fixture: an include path with a variable, and `body`
    /// in the included service, whose folder's `.env` holds the secret.
    fn names_only_window(tag: &str, body: &str, extra: &dyn Fn(&Path)) -> (String, bool, String) {
        let top = "include:\n  - ${SUBDIR:-sub}/compose.yaml\nservices:\n  x:\n    image: alpine\n";
        let pre = std::cell::RefCell::new(String::new());
        let (shown, real) = inc_window(tag, top, &|d: &Path| {
            std::fs::create_dir_all(d.join("sub")).unwrap();
            std::fs::write(d.join("sub/.env"), format!("DB_PASSWORD={INC_PW}\n")).unwrap();
            std::fs::write(d.join("sub/compose.yaml"), format!("services:\n  db:\n    image: postgres:16\n{body}")).unwrap();
            extra(d);
            // Apply's own check, with the toast's and note's text.
            let p = Project { name: tag.into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
            *pre.borrow_mut() = match config(dcli, &p, Some(top), &[]) {
                Ok(m) => format!("ok {m}"),
                // Kiri's own words only: Compose's would be unkeyed.
                Err(e) if e.key.is_some() => e.text,
                Err(e) => format!("unkeyed {}", e.text),
            };
        });
        (shown, real, pre.into_inner())
    }

    #[test]
    fn no_word_of_compose_passes_on_when_kiri_cant_list_the_variables() {
        let base = |d: &Path| std::fs::write(d.join("sub/base.yaml"), "services:\n  base:\n    image: alpine\n").unwrap();
        type Case<'a> = (&'a str, &'a str, &'a dyn Fn(&Path));
        let cases: [Case; 5] = [
            ("pull", "    pull_policy: ${DB_PASSWORD}\n", &|_: &Path| {}),
            ("extfile", "    extends: {file: \"./${DB_PASSWORD}.yaml\", service: base}\n", &base),
            ("extsvc", "    extends: {file: ./base.yaml, service: \"${DB_PASSWORD}\"}\n", &base),
            ("envfile", "    env_file: ./${DB_PASSWORD}.yaml\n", &|_: &Path| {}),
            ("nested", "", &|d: &Path| {
                let t = std::fs::read_to_string(d.join("sub/compose.yaml")).unwrap();
                std::fs::write(d.join("sub/compose.yaml"), format!("include:\n  - ./${{DB_PASSWORD}}/compose.yaml\n{t}")).unwrap();
            }),
        ];
        for (tag, body, extra) in cases {
            let (shown, real, pre) = names_only_window(&format!("no-{tag}"), body, extra);
            assert!(!real, "{tag}: Compose accepts the fixture");
            no_inc_secret(tag, &format!("{shown}{pre}"), &[INC_PW]);
            assert!(shown.contains("byName") && pre == BY_NAME, "{tag}: {shown} / {pre}");
        }
        // A YAML error in the included file: its line only, and never this file's.
        let (shown, _, pre) = names_only_window("no-yaml", "    environment: [\"P=${DB_PASSWORD}\"\n", &|_: &Path| {});
        no_inc_secret("yaml", &format!("{shown}{pre}"), &[INC_PW]);
        assert!(pre.starts_with(YAML_INC) && shown.contains("yamlInc") && !shown.contains("yamlLine"), "{pre} / {shown}");
    }

    #[test]
    fn the_list_sees_a_variable_set_in_an_includes_own_env() {
        let d = tdir("incstamp");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("compose.yaml"), "include:\n  - sub/compose.yaml\nservices:\n  x:\n    image: alpine\n").unwrap();
        std::fs::write(d.join("sub/compose.yaml"), "services:\n  db:\n    image: \"postgres:${DB_PASSWORD:?set it}\"\n").unwrap();
        let p = Project { name: "incstamp".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        let e = saved_config(dcli, &p, &[]).unwrap_err();
        assert_eq!(error_key(&e).map(|k| k.0), Some("reqUnset"), "{e}");
        // Set in the include's own `.env`, no compose file touched.
        std::fs::write(d.join("sub/.env"), "DB_PASSWORD=Stamp-x-Pw44\n").unwrap();
        assert!(saved_config(dcli, &p, &[]).is_ok(), "the list still says it's unset");
        // And in an env file the include names.
        std::fs::write(d.join("compose.yaml"), "include:\n  - path: sub/compose.yaml\n    env_file: sub/x.env\nservices:\n  x:\n    image: alpine\n").unwrap();
        std::fs::write(d.join("sub/x.env"), "OTHER=1\n").unwrap();
        assert!(saved_config(dcli, &p, &[]).is_err());
        std::fs::write(d.join("sub/x.env"), "DB_PASSWORD=Stamp-x-Pw45\n").unwrap();
        assert!(saved_config(dcli, &p, &[]).is_ok(), "the include's env file");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_real_check_that_takes_too_long_says_so() {
        // `config --quiet` (the real values' check) hangs; every other run is Compose's own.
        fn hangs() -> io::Result<Command> {
            let docker = client::docker_cli().expect("a docker CLI");
            let script = format!("case \"$*\" in *'config --quiet'*) sleep 5; exit 1;; esac; exec '{}' \"$@\"", std::path::Path::new(&docker).display());
            let mut c = Command::new("sh");
            c.env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default()).env("HOME", std::env::var_os("HOME").unwrap_or_default()).env("DOCKER_HOST", "unix:///nonexistent/kiri-test.sock").args(["-c", &script, "sh"]);
            Ok(c)
        }
        let d = tdir("realslow");
        std::fs::write(d.join("compose.yaml"), "services: {}\n").unwrap();
        let p = Project { name: "realslow".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        REAL_SECS.with(|s| s.set(1));
        let e = config(hangs, &p, Some("services:\n  x:\n    image: \"alpine:${Q8_TOKEN}\"\n"), &[]).unwrap_err();
        REAL_SECS.with(|s| s.set(30));
        assert_eq!(error_key(&e).map(|k| k.0), Some("slow"), "{e}");
        assert_ne!(e.text, REAL_ERR);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_yaml_error_in_another_file_is_never_put_on_the_open_one() {
        let broken = "services:\n  db:\n    image: postgres:16\n    ports: [\"5432:5432\"\n    environment:\n      A: b\n";
        let base = "services:\n  base:\n    image: alpine\n";
        type Case<'a> = (&'a str, &'a str, Option<&'a str>);
        let cases: [Case; 5] = [
            // The file itself: its line, as before.
            ("top", "services:\n  x:\n    image: alpine\n    ports: [\"80:80\"\n    environment:\n      A: b\n", None),
            // Compose can't list the variables of a broken file: names only, so no path.
            ("inc", "include:\n  - ./sub/compose.yaml\nservices:\n  x:\n    image: alpine\n", Some("")),
            ("incvar", "include:\n  - ${SUBDIR:-sub}/compose.yaml\nservices:\n  x:\n    image: alpine\n", Some("")),
            ("ext", "services:\n  x:\n    extends:\n      file: ./sub/compose.yaml\n      service: db\n", Some("")),
            ("extup", "services:\n  x:\n    extends:\n      file: ../ext-base.yaml\n      service: base\n", Some("")),
        ];
        for (tag, top, other) in cases {
            let d = tdir(&format!("yamlat-{tag}"));
            std::fs::create_dir_all(d.join("sub")).unwrap();
            std::fs::write(d.join("sub/compose.yaml"), broken).unwrap();
            std::fs::write(d.join("sub/.env"), "DB_PASSWORD=Inc-x-Pw7788\n").unwrap();
            let up = d.join("proj");
            let (d, top_file) = if tag == "extup" {
                std::fs::create_dir_all(&up).unwrap();
                std::fs::write(d.join("ext-base.yaml"), format!("{base}  bad: [\"x\"\n  more: 1\n")).unwrap();
                (up.clone(), up.join("compose.yaml"))
            } else {
                (d.clone(), d.join("compose.yaml"))
            };
            std::fs::write(&top_file, top).unwrap();
            let p = Project { name: format!("yamlat-{tag}"), dir: d.to_string_lossy().into(), files: vec![top_file.to_string_lossy().into()], ..Project::default() };
            let hider = Hider::for_project(dcli, &p, None);
            // The list (the saved file), the plan (a draft piped in) and Apply's pre-check (the same reading).
            let listed = saved_config(dcli, &p, &[]).unwrap_err();
            let drafted = config(dcli, &p, Some(top), &[]).unwrap_err();
            let key = list_key(listed.at.as_ref(), listed.key.clone(), &hider);
            let planned = error_json(&drafted, top, &[], &hider);
            match other {
                None => {
                    assert_eq!((listed.at.clone(), drafted.at.clone()), (Some(YamlAt::Top(5)), Some(YamlAt::Top(5))), "{tag}: {listed} / {drafted}");
                    assert_eq!(planned["line"], 5, "{tag}: {planned}");
                }
                Some(f) => {
                    let file = (!f.is_empty()).then(|| f.to_owned());
                    // Where the parser gave up, in that file.
                    let n = match &listed.at {
                        Some(YamlAt::Other { line, .. }) => *line,
                        _ => 0,
                    };
                    assert!((4..=6).contains(&n), "{tag}: {listed}");
                    for e in [&listed, &drafted] {
                        assert_eq!(e.at, Some(YamlAt::Other { line: n, file: file.clone() }), "{tag}: {e}");
                    }
                    let (k, args) = if f.is_empty() { ("yamlInc", json!({"n": n.to_string()})) } else { ("yamlIn", json!({"n": n.to_string(), "file": f})) };
                    assert_eq!(key, Some((k, args.clone())), "{tag}: the list");
                    assert_eq!((planned["line"].clone(), planned["errkey"].clone(), planned["errargs"].clone()), (Value::Null, json!(k), args), "{tag}: {planned}");
                    let want = if f.is_empty() { format!("{YAML_INC}{n}") } else { format!("{YAML_IN}{f}, line {n}") };
                    assert_eq!(planned["text"], want.as_str(), "{tag}");
                    assert!(!planned.to_string().contains("Inc-x-Pw7788"), "{tag}");
                }
            }
            let _ = std::fs::remove_dir_all(d.parent().filter(|_| tag == "extup").unwrap_or(&d));
            let _ = std::fs::remove_dir_all(&d);
        }
        // With every name known, the path relative to the folder; never with names only.
        let d = tdir("yamlat-path");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/compose.yaml"), "x").unwrap();
        let p = Project { name: "p".into(), dir: d.join("sub").parent().unwrap().to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        let e = |f: &Path| format!("failed to parse {}: yaml: line 9: did not find expected ',' or ']'", f.display());
        assert_eq!(yaml_at(&e(&d.join("sub/compose.yaml")), &p, true), Some(YamlAt::Other { line: 9, file: Some("sub/compose.yaml".into()) }));
        assert_eq!(yaml_at(&e(&d.join("../up.yaml")), &p, true), Some(YamlAt::Other { line: 9, file: Some("../up.yaml".into()) }));
        assert_eq!(yaml_at(&e(&d.join("sub/compose.yaml")), &p, false), Some(YamlAt::Other { line: 9, file: None }));
        assert_eq!(yaml_at(&e(&d.join("compose.yaml")), &p, true), Some(YamlAt::Top(9)));
        assert_eq!(yaml_at(&e(Path::new("-")), &p, false), Some(YamlAt::Top(9)));
        assert_eq!(yaml_at("yaml: line 3: x", &p, true), Some(YamlAt::Top(3)));
        let ej = error_json(CfgErr { text: YamlAt::Other { line: 9, file: Some("sub/compose.yaml".into()) }.text(), at: Some(YamlAt::Other { line: 9, file: Some("sub/compose.yaml".into()) }), ..CfgErr::default() }, "", &[], &Hider::default());
        assert_eq!((ej["line"].clone(), ej["errkey"].clone(), ej["text"].clone()), (Value::Null, json!("yamlIn"), json!("Compose rejects this file: YAML error in sub/compose.yaml, line 9")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_yaml_error_is_known_by_its_shape_alone() {
        assert_eq!(yaml_line("yaml: line 7: did not find expected key"), Some(7));
        assert_eq!(yaml_line("failed to parse /a/b: c/compose.yaml: yaml: line 3: mapping values are not allowed"), Some(3));
        assert_eq!(yaml_line("failed to parse -: go-yaml load error in parser (while parsing a block mapping) at L2.C3-L4.C4: did not find expected key"), Some(4));
        assert_eq!(yaml_line("go-yaml load error in scanner (while scanning a quoted scalar) at L3.C12: found unexpected end of stream"), Some(3));
        assert_eq!(yaml_line("go-yaml load error in composer at L4.C15: unknown anchor 'nope' referenced"), Some(4));
        // Anything else, whatever it holds.
        for e in ["services.db.pull_policy 'yaml: line 2:' does not match", "open /x/yaml: line 1: no such file", "failed to parse /x: invalid", "line 5: mapping key \"a\" already defined", "yaml: line x:"] {
            assert_eq!(yaml_line(e), None, "{e}");
        }
        assert_eq!(error_key(&format!("{YAML_ERR}12")).unwrap(), ("yamlLine", json!({"n": "12"})));
    }

    #[test]
    fn a_value_glued_inside_a_name_is_warned_of() {
        let st = |n: &str, v: &str| Stand { name: n.into(), value: String::new(), token: String::new(), real: v.into(), label: format!("${{{n}}}") };
        let ran = |p: &str, names: &[&str], image: &str| docker_names(p, &json!([{"Labels": {PROJECT: p}, "Names": names, "Image": image}]));
        let s = [st("DEPLOY_TOKEN", "Dpl-j-7q7q3")];
        assert_eq!(value_vars(&ran("p", &["/appDpl-j-7q7q3"], "alpine"), &s, &[]), ["${DEPLOY_TOKEN}"]);
        assert_eq!(value_vars(&ran("p", &["/p-web-1"], "repo/app:vDpl-j-7q7q3"), &s, &[]), ["${DEPLOY_TOKEN}"]);
        // A plain word or a name the file declares still raises none.
        assert!(value_vars(&ran("p", &["/p-postgresql-1"], "alpine"), &[st("PG_PASSWORD", "postgres")], &[]).is_empty());
        assert!(value_vars(&ran("p", &["/xapp-core2x"], "alpine"), &[st("Z_TOKEN", "app-core2")], &["app-core2".into()]).is_empty());
    }

    #[test]
    fn a_secret_in_the_files_own_name_is_warned_of() {
        assert_eq!(name_text_vars("name: p-${B_SECRET}\nservices:\n  x:\n    image: alpine\n"), ["${B_SECRET}"]);
        assert_eq!(name_text_vars("name: \"${A_TOKEN:-x}-${STAGE}-${A_TOKEN}\"\n"), ["${A_TOKEN}"]);
        assert!(name_text_vars("name: shop-${STAGE}\nservices:\n  x:\n    container_name: c-${B_SECRET}\n").is_empty());
        // A comment, quotes respected; other ways of writing the key; JSON.
        for t in ["name: shop # was ${DB_PASSWORD}\n", "name: \"shop\" # ${DB_PASSWORD}\n", "name: 'shop'  # ${DB_PASSWORD}\n", "name: # ${DB_PASSWORD}\n  shop\n", "{\"name\": \"shop\", \"x\": \"${DB_PASSWORD}\"}"] {
            assert!(name_text_vars(t).is_empty(), "{t}");
        }
        for t in ["name: \"p # ${DB_PASSWORD}\"\n", "\"name\": p-${DB_PASSWORD}\n", "'name': p-${DB_PASSWORD}\n", "name : p-${DB_PASSWORD}\n", "{\"name\": \"p-${DB_PASSWORD}\", \"services\": {}}", "{\n  \"services\": {},\n  \"name\": \"p-${DB_PASSWORD}\"\n}\n",
            "name: |\n  p-${DB_PASSWORD}\nservices: {}\n", "name: &n p-${DB_PASSWORD}\nservices: {}\n", "name: >-\n  p-${DB_PASSWORD} # x\n"] {
            assert_eq!(name_text_vars(t), ["${DB_PASSWORD}"], "{t}");
        }
        // A block or anchor without `${` of its own: nothing.
        assert!(name_text_vars("x: &n ${DB_PASSWORD}\nname: *n\n").is_empty());
        assert!(name_text_vars("name: |\n  shop\nservices:\n  x:\n    image: a:${DB_PASSWORD}\n").is_empty());
        assert!(name_text_vars("names: p-${DB_PASSWORD}\n").is_empty());
        let n = name_note(&name_text_vars("name: p-${B_SECRET}\n")).unwrap();
        assert_eq!(n["errkey"], "nameSecret");
        assert_eq!(n["errargs"]["names"], json!(["${B_SECRET}"]));
    }

    #[test]
    fn a_name_kiri_wont_take_says_it_uses_the_folders() {
        let d = tdir("m-pname-env");
        let dir = d.to_string_lossy().into_owned();
        let name = folder_name(&d);
        let n = folder_note(&name_text_vars("name: pnenv-${A_SECRET}\n"), &name, &dir).unwrap();
        assert_eq!(n["errkey"], "nameFolder");
        assert_eq!(n["errargs"], json!({"vars": "${A_SECRET}", "name": name}));
        assert!(!n.to_string().contains("pnenv-"), "{n}");
        // Named by Docker (`-p`, or the name the terminal's run made): no notice.
        assert!(folder_note(&name_text_vars("name: pnenv-${A_SECRET}\n"), "pnenv-x1", &dir).is_none());
        assert!(folder_note(&name_text_vars("name: shop\n"), &name, &dir).is_none());
        // A file opened in Kiri whose name holds a secret is listed by its folder.
        std::fs::write(d.join("compose.yaml"), "name: pnenv-${A_SECRET}\nservices:\n  x:\n    image: alpine\n").unwrap();
        std::fs::write(d.join(".env"), "A_SECRET=Zq9-pname-77x\n").unwrap();
        let p = Project { name, dir: dir.clone(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        assert_eq!(opened_name(dcli, &p), p.name);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_projects_networks_are_its_own() {
        let ctr = |nets: &[&str]| json!({"Labels": {PROJECT: "p"}, "NetworkSettings": {"Networks": nets.iter().map(|n| (n.to_string(), json!({}))).collect::<serde_json::Map<_, _>>()}});
        // What Docker reports for a running project.
        assert_eq!(project_networks("p", None, &json!([ctr(&["p_default"])])), ["p_default"]);
        assert_eq!(project_networks("p", None, &json!([ctr(&["p_front", "p_back"]), ctr(&["p_back"])])), ["p_back", "p_front"]);
        // Else the reading's, with a custom name; a stand-in as its variable.
        let m = json!({"networks": {"default": {"name": "p_default"}, "edge": {"name": "net-${NET_SECRET}"}, "bare": {}}});
        assert_eq!(project_networks("p", Some(&m), &json!([])), ["p_default", "bare", "net-${NET_SECRET}"]);
        // The same order before Apply (the reading) and after (Docker's).
        let m = json!({"networks": {"default": {"name": "p_default"}, "front": {"name": "p_front"}, "back": {"name": "p_back"}}});
        let before = project_networks("p", Some(&m), &json!([]));
        assert_eq!(before, ["p_default", "p_back", "p_front"]);
        assert_eq!(project_networks("p", Some(&m), &json!([ctr(&["p_front", "p_default"]), ctr(&["p_back"])])), before);
        // None known.
        assert!(project_networks("p", Some(&json!({"services": {}})), &json!([])).is_empty());
        assert!(project_networks("p", None, &json!([{"Labels": {PROJECT: "other"}, "NetworkSettings": {"Networks": {"o_default": {}}}}])).is_empty());
    }

    /// Compose's own name for the files (no `-p`), with the real values: the
    /// secret-named variables whose value it holds, or none when Compose can't read them.
    fn compose_takes(p: &Project) -> Option<Vec<String>> {
        let mut c = dcli().unwrap();
        c.args(project_args(&Project { name: String::new(), ..p.clone() }, None)).args(["config", "--format", "json"]);
        let m: Value = serde_json::from_slice(&run_cli(c, None, 30).ok()?).ok()?;
        let name = m["name"].as_str()?.to_owned();
        let mut out: Vec<String> = env_vars(dcli, p).into_iter().filter(|(k, v)| secret_name(k) && v.len() > 3 && name.contains(&v.to_ascii_lowercase())).map(|(k, _)| format!("${{{k}}}")).collect();
        out.dedup();
        Some(out)
    }

    #[test]
    fn the_files_own_name_is_the_one_compose_takes() {
        let svc = "services:\n  x:\n    image: alpine\n";
        let mut forms: Vec<String> = [
            // Each YAML way of writing `name:` Compose reads: the whole file indented,
            // a flow mapping, a double-quoted value over lines, a doubled single quote.
            "  name: app-${DB_PASSWORD}\n  services:\n    x:\n      image: alpine\n",
            "{name: \"app-${DB_PASSWORD}\", services: {x: {image: alpine}}}\n",
            "name: \"app-\n  ${DB_PASSWORD}\"\nservices:\n  x:\n    image: alpine\n",
            "name: 'a''b-${DB_PASSWORD}'\nservices:\n  x:\n    image: alpine\n",
        ]
        .map(str::to_owned)
        .to_vec();
        // The forms the line scan reads, each with a service.
        for t in ["name: p-${B_SECRET}\n", "name: \"${A_TOKEN:-x}-${STAGE}-${A_TOKEN}\"\n", "name: shop-${STAGE}\n", "name: shop # was ${DB_PASSWORD}\n", "name: \"shop\" # ${DB_PASSWORD}\n", "name: 'shop'  # ${DB_PASSWORD}\n", "name: # ${DB_PASSWORD}\n  shop\n",
            "name: \"p # ${DB_PASSWORD}\"\n", "\"name\": p-${DB_PASSWORD}\n", "'name': p-${DB_PASSWORD}\n", "name : p-${DB_PASSWORD}\n", "name: |\n  p-${DB_PASSWORD}\n", "name: &n p-${DB_PASSWORD}\n", "name: >-\n  p-${DB_PASSWORD} # x\n",
            "x-n: &n p-${DB_PASSWORD}\nname: *n\n", "name: |\n  shop\n", "names: p-${DB_PASSWORD}\n", "name: pnenv-${A_SECRET}\n", "name: shop\n", "name: p-${B_SECRET}-${DB_PASSWORD}\n"] {
            forms.push(format!("{t}{svc}"));
        }
        forms.push(format!("{svc}  y:\n    image: \"alpine:${{DB_PASSWORD}}\"\n"));
        forms.push("{\"name\": \"p-${DB_PASSWORD}\", \"services\": {\"x\": {\"image\": \"alpine\"}}}".into());
        forms.push("{\n  \"services\": {\"x\": {\"image\": \"alpine\"}},\n  \"name\": \"p-${DB_PASSWORD}\"\n}\n".into());
        forms.push("{\"name\": \"shop\", \"x-a\": \"${DB_PASSWORD}\", \"services\": {\"x\": {\"image\": \"alpine\"}}}".into());
        assert!(forms.len() >= 28, "{}", forms.len());
        let d = tdir("ownname");
        std::fs::write(d.join(".env"), "DB_PASSWORD=zq9pw77xk\nB_SECRET=bsec4q8zz\nA_TOKEN=atok5m2qq\nA_SECRET=asec6n3ww\nSTAGE=dev\n").unwrap();
        let p = Project { name: "ownname".into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        let mut asked = 0;
        for t in &forms {
            std::fs::write(d.join("compose.yaml"), t).unwrap();
            let Some(took) = compose_takes(&p) else {
                // Compose rejects it (`names:`): the line scan, as before.
                assert!(own_name_vars(dcli, &p, None).is_none(), "{t:?}");
                assert_eq!(saved_reading(dcli, &p, &[], true).name.unwrap(), name_text_vars(t), "{t:?}");
                continue;
            };
            asked += 1;
            assert_eq!(own_name_vars(dcli, &p, None).as_ref(), Some(&took), "{t:?}");
            assert_eq!(own_name_vars(dcli, &p, Some(t)).as_ref(), Some(&took), "draft {t:?}");
            // The list's reading says the same, the folder notice too.
            let r = saved_reading(dcli, &p, &[], true);
            assert_eq!(r.name.as_ref(), Some(&took), "{t:?}");
            let note = folder_note(r.name.as_deref().unwrap_or_default(), &folder_name(&d), &p.dir);
            assert_eq!(note.is_some(), !took.is_empty(), "{t:?}");
        }
        assert!(asked + 1 >= forms.len(), "{asked}");
        // The four forms the line scan missed: Compose names the secret.
        for t in &forms[..4] {
            std::fs::write(d.join("compose.yaml"), t).unwrap();
            assert_eq!(own_name_vars(dcli, &p, None).unwrap(), ["${DB_PASSWORD}"], "{t:?}");
        }
        // Compose can't read it: the line scan, as before.
        std::fs::write(d.join("compose.yaml"), "name: p-${DB_PASSWORD}\nservices:\n  x: [\n").unwrap();
        assert!(own_name_vars(dcli, &p, None).is_none());
        assert_eq!(saved_reading(dcli, &p, &[], true).name.unwrap(), ["${DB_PASSWORD}"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A docker CLI that counts its runs in `KIRI_T_COUNT`'s file.
    fn counted() -> io::Result<Command> {
        let docker = client::docker_cli().expect("a docker CLI");
        let log = std::env::temp_dir().join(format!("kiri-compose-count-{}", std::process::id()));
        let script = format!("echo \"$*\" >> '{}'; exec '{}' \"$@\"", log.display(), std::path::Path::new(&docker).display());
        let mut c = Command::new("sh");
        c.env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default()).env("HOME", std::env::var_os("HOME").unwrap_or_default()).env("DOCKER_HOST", "unix:///nonexistent/kiri-test.sock").args(["-c", &script, "sh"]);
        Ok(c)
    }

    /// A project whose include path holds a variable: Compose can't list
    /// its variables, so Kiri reads it by names only.
    fn loose_project(tag: &str, sub: &str) -> (PathBuf, Project) {
        let d = tdir(tag);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("compose.yaml"), "include:\n  - ${SUBDIR:-sub}/compose.yaml\nservices:\n  x:\n    image: alpine\n").unwrap();
        std::fs::write(d.join("sub/compose.yaml"), sub).unwrap();
        let p = Project { name: tag.into(), dir: d.to_string_lossy().into(), files: vec![d.join("compose.yaml").to_string_lossy().into()], ..Project::default() };
        (d, p)
    }

    fn row(cli: Cli, p: &Project) -> Value {
        project_row(cli, tstore, p.clone(), &json!([]), &HashMap::new(), &mut Vec::new())
    }

    /// Every docker CLI run `counted` made.
    fn count_all() -> usize {
        let log = std::env::temp_dir().join(format!("kiri-compose-count-{}", std::process::id()));
        std::fs::read_to_string(log).unwrap_or_default().lines().count()
    }

    #[test]
    fn a_by_name_project_is_read_again_when_its_top_file_changes() {
        let _one = LOOSE_TEST.lock().unwrap_or_else(|e| e.into_inner());
        // A YAML error in the included file: a broken file, its line only.
        let (d, p) = loose_project("loose-yaml", "services:\n  db:\n    image: postgres:16\n    ports: [\"5432:5432\"\n");
        let r = row(dcli, &p);
        assert!(r["errkey"] == "yamlInc" && r["byname"].is_null() && r["warn"].is_null(), "{r}");
        assert_eq!(r["error"], format!("{YAML_INC}5"), "{r}");
        // Fixed outside Kiri: seen once the top file changes (or in 30 s).
        std::fs::write(d.join("sub/compose.yaml"), "services:\n  db:\n    image: postgres:16\n").unwrap();
        assert_eq!(row(dcli, &p)["errkey"], "yamlInc", "kept while the top file stays");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(d.join("compose.yaml"), "include:\n  - ${SUBDIR:-sub}/compose.yaml\nservices:\n  x:\n    image: alpine\n# fixed\n").unwrap();
        let r = row(dcli, &p);
        assert!(r["byname"] == true && r["error"].is_null() && r["readable"] == false && r["warn"]["errkey"] == "byName" && r["warn"]["text"] == BY_NAME, "{r}");
        // No containers: no services, not started, no names warning.
        assert!(r["services"] == json!([]) && r["fresh"] == true && r["namewarn"].is_null(), "{r}");
        // After an Apply from Kiri (or a forget): at once.
        std::fs::write(d.join("sub/compose.yaml"), "services:\n  db: [\n").unwrap();
        forget_readings(&p.name);
        assert_eq!(row(dcli, &p)["errkey"], "yamlInc");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// One by-name test at a time: they count Compose's runs.
    static LOOSE_TEST: Mutex<()> = Mutex::new(());

    #[test]
    fn twenty_by_name_projects_cost_no_compose_run_on_an_unchanged_poll() {
        let _one = LOOSE_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let ps: Vec<(PathBuf, Project)> = (0..20).map(|i| loose_project(&format!("loose-poll{i}"), "services:\n  db:\n    image: postgres:16\n")).collect();
        let log = std::env::temp_dir().join(format!("kiri-compose-count-{}", std::process::id()));
        let _ = std::fs::remove_file(&log);
        for (_, p) in &ps {
            assert_eq!(row(counted, p)["byname"], true);
        }
        let first = count_all();
        // Only `config --variables` and one stand-in `config` per project.
        let runs = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(runs.lines().all(|l| l.contains(" config --variables") || l.contains(" config --format json")), "{runs}");
        assert!(!runs.contains("--quiet") && !runs.contains("--services") && !runs.contains("--hash"), "{runs}");
        assert!(first <= 20 * 4, "{first}");
        for _ in 0..3 {
            for (_, p) in &ps {
                assert_eq!(row(counted, p)["byname"], true);
            }
        }
        assert_eq!(count_all(), first, "an unchanged poll runs nothing");
        // The top file changed: that one project is read again.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&ps[3].1.files[0], "include:\n  - ${SUBDIR:-sub}/compose.yaml\nservices:\n  y:\n    image: alpine\n").unwrap();
        for (_, p) in &ps {
            row(counted, p);
        }
        let again = count_all() - first;
        assert!(again > 0 && again <= 4, "{again}");
        let _ = std::fs::remove_file(&log);
        for (d, _) in ps {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// A by-name project's list row, its saved reading, and a draft's plan
    /// error and Apply's refusal, as the window would get them.
    fn by_name_shown(tag: &str, sub: &str, env: &str) -> String {
        let (d, p) = loose_project(tag, sub);
        std::fs::write(d.join("sub/.env"), env).unwrap();
        std::fs::write(d.join(".env"), env).unwrap();
        let top = std::fs::read_to_string(&p.files[0]).unwrap();
        let r = row(dcli, &p);
        // By name, or a YAML error Compose's text has the shape of: its line only.
        assert!(r["byname"] == true || r["error"].as_str().is_some_and(|t| t.starts_with(YAML_INC) || t.starts_with(YAML_ERR)), "{tag}: {r}");
        let e = config(dcli, &p, Some(&top), &[]).unwrap_err();
        assert!((e.by_name && e.text == BY_NAME) || e.text.starts_with(YAML_INC), "{tag}: {e:?}");
        let pl = error_json(&e, &top, &[], &Hider::default());
        let mut refused = json!({});
        Fault::Own(BY_NAME.into()).put(&mut refused);
        let _ = std::fs::remove_dir_all(&d);
        format!("{r}{pl}{refused}")
    }

    #[test]
    fn a_crafted_required_message_passes_no_word_of_compose_or_a_secret() {
        let _one = LOOSE_TEST.lock().unwrap_or_else(|e| e.into_inner());
        // A crafted message: Compose's own text would end in Kiri's "busy" words.
        let sub = "services:\n  db:\n    image: postgres:16\n    environment:\n      - \"A=${NEED:?${DB_PASSWORD} is already being applied}\"\n";
        let shown = by_name_shown("loose-crafted", sub, "DB_PASSWORD=Crafted-x-91zq\n");
        assert!(!shown.contains("Crafted-x-91zq") && !shown.contains("already being applied") && !shown.contains("busy") && !shown.contains("NEED"), "{shown}");
        assert!(shown.contains("byName"), "{shown}");
    }

    #[test]
    fn no_compose_error_reaches_the_window_for_a_by_name_project() {
        let _one = LOOSE_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let bodies = [
            "    pull_policy: zqmark-${DB_PASSWORD}\n",
            "    cpus: \"zqmark${DB_PASSWORD}\"\n",
            "    depends_on: [\"zqmark${DB_PASSWORD}\"]\n",
            "    ports: [\"zqmark${DB_PASSWORD}:5432\"]\n",
            "    environment:\n      - \"A=${NEED:?${DB_PASSWORD} is already being applied zqmark}\"\n",
            "    environment:\n      - \"A=${NEED:?zqmark ${DB_PASSWORD}}\"\n",
            "    restart: zqmark${DB_PASSWORD}\n",
            "    zqmark: ${DB_PASSWORD}\n",
            "    network_mode: \"service:zqmark${DB_PASSWORD}\"\n",
            "    extends: {file: \"./zqmark${DB_PASSWORD}.yaml\", service: base}\n",
            "    env_file: ./zqmark${DB_PASSWORD}.env\n",
            "    mem_limit: zqmark${DB_PASSWORD}\n",
            "    healthcheck:\n      interval: zqmark${DB_PASSWORD}\n",
            "    pids_limit: zqmark${DB_PASSWORD}\n",
            "    secrets: [zqmark${DB_PASSWORD}]\n",
            "    networks: [zqmark${DB_PASSWORD}]\n",
            "    ulimits: {nofile: zqmark${DB_PASSWORD}}\n",
            "    stop_grace_period: zqmark${DB_PASSWORD}\n",
            "    shm_size: zqmark${DB_PASSWORD}\n",
            "    labels: \"zqmark${DB_PASSWORD}\"\n",
        ];
        let mut said = 0;
        for (i, body) in bodies.iter().enumerate() {
            let sub = format!("services:\n  db:\n    image: postgres:16\n{body}");
            // What Compose itself says of it, with the real value.
            let d = tdir(&format!("loose-said{i}"));
            std::fs::write(d.join("compose.yaml"), &sub).unwrap();
            let out = dcli().unwrap().current_dir(&d).env("DB_PASSWORD", "Sweep-x-55qv").args(["compose", "config"]).output().unwrap();
            let words = String::from_utf8_lossy(&out.stderr).to_string();
            if !out.status.success() && (words.contains("zqmark") || words.contains("Sweep-x-55qv")) {
                said += 1;
            }
            let _ = std::fs::remove_dir_all(&d);
            let shown = by_name_shown(&format!("loose-sweep{i}"), &sub, "DB_PASSWORD=Sweep-x-55qv\n");
            for w in ["zqmark", "Sweep-x-55qv", "already being applied", "busy"] {
                assert!(!shown.contains(w), "{i}: {w} in {shown}");
            }
            if !out.status.success() {
                let first = words.lines().next().unwrap_or_default();
                assert!(first.len() < 12 || !shown.contains(first), "{i}: Compose's words in {shown}");
            }
        }
        assert!(said >= 10, "the fixtures make Compose quote them: {said}");
    }

    #[test]
    fn a_failure_that_says_it_took_too_long_is_still_a_failure() {
        #[cfg(unix)]
        {
            fn says() -> io::Result<Command> {
                let mut c = Command::new("sh");
                c.args(["-c", "echo 'docker compose took longer than 30 seconds' >&2; exit 1", "sh"]);
                Ok(c)
            }
            let p = Project { name: "says".into(), files: vec!["/nonexistent/compose.yaml".into()], ..Project::default() };
            assert_eq!(real_passes(says, &p, None, &[]), Ok(false));
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 5"]);
            assert_eq!(run_ran(c, None, 1), Ran::TimedOut(1));
            let mut c = Command::new("sh");
            c.args(["-c", "echo took longer than 1 seconds >&2; exit 1"]);
            assert_eq!(run_ran(c, None, 5), Ran::Failed("took longer than 1 seconds".into()));
        }
    }

    #[test]
    fn a_plain_variable_include_path_is_stamped() {
        let vars = vec![("SUBDIR".to_owned(), "inc".to_owned()), ("EMPTY".to_owned(), String::new())];
        assert_eq!(fill_plain("${SUBDIR:-sub}/compose.yaml", &vars).as_deref(), Some("inc/compose.yaml"));
        assert_eq!(fill_plain("${NONE:-sub}/compose.yaml", &vars).as_deref(), Some("sub/compose.yaml"));
        assert_eq!(fill_plain("${EMPTY:-sub}/c.yaml", &vars).as_deref(), Some("sub/c.yaml"));
        assert_eq!(fill_plain("${EMPTY-sub}/c.yaml", &vars).as_deref(), Some("/c.yaml"));
        for v in ["${DB_PASSWORD:-sub}/c.yaml", "${NONE}/c.yaml", "${SUBDIR:?}/c.yaml", "${A${B}}/c.yaml"] {
            assert_eq!(fill_plain(v, &vars), None, "{v}");
        }
        // The stamp covers the file the include names now: a change to it is a new stamp.
        let (d, p) = loose_project("loose-stamp", "services:\n  db:\n    image: postgres:16\n");
        let before = stamp(&p);
        assert!(before.iter().any(|(f, t)| f.ends_with("sub/compose.yaml") && t.is_some()), "{before:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(d.join("sub/compose.yaml"), "services:\n  db:\n    image: postgres:17\n").unwrap();
        assert_ne!(stamp(&p), before);
        let _ = std::fs::remove_dir_all(&d);
    }
}
