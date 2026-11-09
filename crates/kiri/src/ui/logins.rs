//! Signing in to container registries, as `docker login` does, so private
//! images pull: the Mac's window and Windows' (crates/kiri-win, by #[path])
//! answer `logins`, `login` and `logout` with it, and a pull sends the
//! sign-in for its registry in `X-Registry-Auth`. A sign-in is found the way
//! the docker CLI finds one (config.json's `credHelpers`, then `credsStore`,
//! then `auths`) and saved where the CLI will look: with the configured
//! credential helper, or, with none, in the Keychain or Windows' Credential
//! Manager. A password never goes into config.json, a log or an event.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::client;

// Docker's base64, the engine's own file (kiri-win doesn't link kiri-image).
#[path = "../../../kiri-image/src/b64.rs"]
mod b64;

/// Docker Hub's key in config.json and with credential helpers.
pub const HUB: &str = "https://index.docker.io/v1/";
/// What credential helpers label their items, in the Keychain and in
/// Credential Manager; Kiri's own items say the same, so either finds both.
#[cfg_attr(target_os = "linux", allow(dead_code))]
const LABEL: &str = "Docker Credentials";
/// The username a helper gives for an identity token instead of a password.
const TOKEN_USER: &str = "<token>";
/// The longest a helper may take: one waiting on a Keychain prompt included.
const HELPER_WAIT: Duration = Duration::from_secs(60);

/// A username and its password or token.
pub struct Login {
    pub username: String,
    pub secret: String,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// A registry as config.json keys it, whatever the spelling: Docker Hub's
/// URL for any of its names, else the host (and port) in lower case.
pub fn server_key(server: &str) -> String {
    let s = server.trim().to_ascii_lowercase();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(&s);
    let host = s.split('/').next().unwrap_or_default();
    match host {
        ""
        | "docker.io"
        | "index.docker.io"
        | "registry-1.docker.io"
        | "registry.hub.docker.com" => HUB.to_owned(),
        _ => host.to_owned(),
    }
}

/// The registry an image reference is pulled from, as a key.
pub fn registry(reference: &str) -> String {
    match reference.trim().split_once('/') {
        Some((first, _)) if first.contains(['.', ':']) || first == "localhost" => server_key(first),
        _ => HUB.to_owned(),
    }
}

/// A key's name for people: Docker Hub, or the host.
fn display(key: &str) -> &str {
    if key == HUB { "Docker Hub" } else { key }
}

/// A registry's logo for the page, as an image's: [colour, path].
fn logo(key: &str) -> Option<[&'static str; 2]> {
    let slug = match key {
        HUB => "docker",
        k if k.contains("gitlab") => "gitlab",
        k if k.contains("harbor") => "harbor",
        _ => return None,
    };
    let i = super::brands::BRANDS
        .binary_search_by_key(&slug, |(s, _, _)| s)
        .ok()?;
    let (_, hex, path) = super::brands::BRANDS[i];
    Some([hex, path])
}

/// A key that can go to a helper's stdin and into a request: a host (and
/// port), or Docker Hub's URL.
fn plain_key(key: &str) -> bool {
    key == HUB
        || (!key.is_empty()
            && key.len() <= 253
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]_".contains(&b)))
}

/// The `X-Registry-Auth` header for a sign-in, as the docker CLI sends it:
/// base64url JSON.
pub fn header(key: &str, login: &Login) -> String {
    let v = if login.username == TOKEN_USER {
        json!({"identitytoken": login.secret, "serveraddress": key})
    } else {
        json!({"username": login.username, "password": login.secret, "serveraddress": key})
    };
    b64::encode(v.to_string().as_bytes())
        .replace('+', "-")
        .replace('/', "_")
}

/// The header for pulling `reference`, when signed in to its registry.
pub fn header_for(reference: &str) -> Option<String> {
    let key = registry(reference);
    find(&key).map(|l| header(&key, &l))
}

/// The docker CLI's directory: `DOCKER_CONFIG`, else `~/.docker`.
fn config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DOCKER_CONFIG") {
        return Some(PathBuf::from(d));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".docker"))
}

/// config.json's JSON; a file saved by Notepad may start with a BOM.
fn parse_config(b: &[u8]) -> serde_json::Result<Value> {
    serde_json::from_slice(b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(b))
}

/// config.json, or `{}` when there is none (or it can't be read).
fn read_config() -> Value {
    config_dir()
        .and_then(|d| std::fs::read(d.join("config.json")).ok())
        .and_then(|b| parse_config(&b).ok())
        .unwrap_or_else(|| json!({}))
}

/// Changes config.json's `auths`, through a temporary file so a crash never
/// leaves half a config. A config that can't be read is left alone.
fn edit_auths(change: impl FnOnce(&mut serde_json::Map<String, Value>)) -> io::Result<()> {
    let path = config_dir()
        .ok_or_else(|| io::Error::other("no home folder"))?
        .join("config.json");
    let mut cfg: Value = match std::fs::read(&path) {
        Ok(b) => parse_config(&b).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e),
    };
    let Some(obj) = cfg.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "config.json isn't an object",
        ));
    };
    let auths = obj.entry("auths").or_insert_with(|| json!({}));
    if !auths.is_object() {
        *auths = json!({});
    }
    if let Some(a) = auths.as_object_mut() {
        change(a);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("kiri-tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&cfg).map_err(io::Error::other)?,
    )?;
    std::fs::rename(tmp, path)
}

/// The credential helper config.json names for `key`: its own in
/// `credHelpers`, else `credsStore`.
fn helper_name(cfg: &Value, key: &str) -> Option<String> {
    let own = cfg["credHelpers"]
        .as_object()
        .and_then(|m| m.iter().find(|(k, _)| server_key(k) == key))
        .and_then(|(_, v)| v.as_str());
    own.or(cfg["credsStore"].as_str())
        .filter(|n| valid_helper(n))
        .map(str::to_owned)
}

/// A helper name is part of a file name: nothing that could leave the folder.
fn valid_helper(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !name.starts_with('.')
}

/// `docker-credential-<name>`: on PATH, else where Docker Desktop, Homebrew
/// or Kiri put it (an app started from Finder has a short PATH).
fn helper_path(name: &str) -> Option<PathBuf> {
    let file = format!("docker-credential-{name}{}", std::env::consts::EXE_SUFFIX);
    let path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    path.into_iter()
        .chain(helper_dirs())
        .map(|d| d.join(&file))
        .find(|f| f.is_file())
}

#[cfg(not(windows))]
fn helper_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [
        "/usr/local/bin",
        "/opt/homebrew/bin",
        "/Applications/Docker.app/Contents/Resources/bin",
    ]
    .map(PathBuf::from)
    .into();
    if let Some(h) = std::env::var_os("HOME").map(PathBuf::from) {
        v.push(h.join(".docker/bin"));
        v.push(h.join(".kiri/bin"));
        v.push(h.join("Applications/Docker.app/Contents/Resources/bin"));
    }
    // Kiri.app's own.
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.canonicalize().ok())
        .and_then(|e| e.parent().map(|d| d.join("../Resources/bin")))
    {
        v.push(dir);
    }
    v
}

#[cfg(windows)]
fn helper_dirs() -> Vec<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut v = Vec::new();
    for pf in [env("ProgramFiles"), env("ProgramW6432")]
        .into_iter()
        .flatten()
    {
        v.push(pf.join(r"Docker\Docker\resources\bin"));
    }
    if let Some(d) = env("LOCALAPPDATA") {
        v.push(d.join(r"Kiri\bin"));
    }
    if let Some(d) = env("USERPROFILE") {
        v.push(d.join(r".docker\bin"));
    }
    // Beside kiri-win.exe, as unzipped.
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        v.push(d.join("bin"));
        v.push(d);
    }
    v
}

/// Runs `docker-credential-<name> <verb>` with `input` on its stdin: its
/// output when it succeeds, its message when not. One that hangs is given up on.
fn run(path: &Path, verb: &str, input: &[u8]) -> io::Result<Vec<u8>> {
    let mut cmd = Command::new(path);
    cmd.arg(verb)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // No console window flashing up on Windows.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A helper that exits without reading says why on stdout.
        let _ = stdin.write_all(input);
    }
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no output"))?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let deadline = Instant::now() + HELPER_WAIT;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "it didn't answer"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = reader.join().unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&out).trim().to_owned(),
        ))
    }
}

/// A helper's answer to `get`: `{ServerURL, Username, Secret}`.
fn parse_get(out: &[u8]) -> Option<Login> {
    let v: Value = serde_json::from_slice(out).ok()?;
    let secret = v["Secret"].as_str().filter(|s| !s.is_empty())?.to_owned();
    Some(Login {
        username: v["Username"].as_str().unwrap_or_default().to_owned(),
        secret,
    })
}

/// A helper's answer to `list`: `{server: username}`, as keys.
fn parse_list(out: &[u8]) -> Vec<(String, String)> {
    let v: Value = serde_json::from_slice(out).unwrap_or_default();
    v.as_object()
        .into_iter()
        .flatten()
        .map(|(s, u)| (server_key(s), u.as_str().unwrap_or_default().to_owned()))
        .collect()
}

/// A sign-in written into config.json itself (`docker login` with no
/// credential store): `auth` is base64 `user:password`.
fn inline(entry: &Value) -> Option<Login> {
    if let Some(token) = entry["identitytoken"].as_str().filter(|t| !t.is_empty()) {
        return Some(Login {
            username: TOKEN_USER.to_owned(),
            secret: token.to_owned(),
        });
    }
    if let Some(auth) = entry["auth"].as_str().filter(|a| !a.is_empty()) {
        let pair = String::from_utf8(b64::decode(auth)?).ok()?;
        let (u, p) = pair.split_once(':')?;
        return Some(Login {
            username: u.to_owned(),
            secret: p.to_owned(),
        });
    }
    match (entry["username"].as_str(), entry["password"].as_str()) {
        (Some(u), Some(p)) if !p.is_empty() => Some(Login {
            username: u.to_owned(),
            secret: p.to_owned(),
        }),
        _ => None,
    }
}

/// `auths`' entry for `key`, under any spelling of it.
fn auths_entry<'a>(cfg: &'a Value, key: &str) -> Option<&'a Value> {
    cfg["auths"]
        .as_object()?
        .iter()
        .find(|(k, _)| server_key(k) == key)
        .map(|(_, v)| v)
}

/// The sign-in for `key`, found as the docker CLI finds it: the configured
/// helper when it's there; else the system's store, where Kiri saves one
/// when there's no helper; then config.json itself.
pub fn find(key: &str) -> Option<Login> {
    let cfg = read_config();
    let helper = helper_name(&cfg, key).and_then(|n| helper_path(&n));
    let found = match &helper {
        Some(path) => run(path, "get", key.as_bytes())
            .ok()
            .and_then(|o| parse_get(&o)),
        None => os::get(key),
    };
    found.or_else(|| auths_entry(&cfg, key).and_then(inline))
}

/// Where a new sign-in for `key` goes, said for people.
fn store_label(cfg: &Value, key: &str) -> String {
    match helper_name(cfg, key).filter(|n| helper_path(n).is_some()) {
        Some(n) => format!("Saved with docker-credential-{n}"),
        None => format!("Kept in {}", os::NAME),
    }
}

/// Saves a sign-in where the docker CLI looks: with the configured helper,
/// noting the registry in `auths` as `docker login` does (and dropping a
/// password written there before); else in the system's store. Says where.
fn save(key: &str, login: &Login) -> Result<String, String> {
    let cfg = read_config();
    if let Some((name, path)) = helper_name(&cfg, key).and_then(|n| helper_path(&n).map(|p| (n, p)))
    {
        let input = json!({"ServerURL": key, "Username": login.username, "Secret": login.secret})
            .to_string();
        run(&path, "store", input.as_bytes())
            .map_err(|e| format!("docker-credential-{name} couldn't save it: {e}"))?;
        // The helper is what counts: the CLI asks it whether or not `auths` lists the registry.
        let _ = edit_auths(|a| {
            let k = a
                .keys()
                .find(|k| server_key(k) == key)
                .cloned()
                .unwrap_or_else(|| key.to_owned());
            a.insert(k, json!({}));
        });
        return Ok(format!("Saved with docker-credential-{name}"));
    }
    os::set(key, login).map_err(|e| format!("Couldn't save it in {}: {e}", os::NAME))?;
    Ok(format!("Kept in {}", os::NAME))
}

/// `{type: "login", server, error}`.
pub fn failed(key: &str, error: &str) -> String {
    json!({"type": "login", "server": key, "error": error}).to_string()
}

/// Signs in: the engine checks the password with the registry first
/// (`POST /auth`, as `docker login` does), so a wrong one is never saved.
/// `{type: "login", server, username, store}`, or with `error`.
pub fn login(server: &str, username: &str, secret: &str) -> String {
    let key = server_key(server);
    let username = username.trim();
    if !plain_key(&key) {
        return failed(&key, "That isn't a registry's name, such as ghcr.io.");
    }
    if username.is_empty() || secret.is_empty() {
        return failed(&key, "Enter a username and a token or password.");
    }
    let body = json!({"username": username, "password": secret, "serveraddress": key});
    let resp = match client::request("POST", "/auth", Some(&body)) {
        Ok(r) => r,
        Err(e) => return failed(&key, &format!("Kiri's engine didn't answer: {e}")),
    };
    if resp.status == 401 || resp.status == 403 {
        return failed(
            &key,
            &format!(
                "{} didn't accept that username and password or token.",
                display(&key)
            ),
        );
    }
    if !resp.ok() {
        return failed(&key, &resp.error());
    }
    // A registry may answer with a token to keep instead of the password.
    let token = resp.json()["IdentityToken"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let keep = if token.is_empty() {
        Login {
            username: username.to_owned(),
            secret: secret.to_owned(),
        }
    } else {
        Login {
            username: TOKEN_USER.to_owned(),
            secret: token,
        }
    };
    match save(&key, &keep) {
        Ok(store) => json!({"type": "login", "server": key, "username": username, "store": store})
            .to_string(),
        Err(e) => failed(&key, &e),
    }
}

/// Signs out, as `docker logout`: the helper (or the system's store) forgets
/// it and `auths` loses its entry. Then the list again.
pub fn logout(server: &str) -> String {
    let key = server_key(server);
    if plain_key(&key) {
        let cfg = read_config();
        match helper_name(&cfg, &key).and_then(|n| helper_path(&n)) {
            Some(path) => {
                let _ = run(&path, "erase", key.as_bytes());
            }
            None => os::delete(&key),
        }
        let _ = edit_auths(|a| a.retain(|k, _| server_key(k) != key));
    }
    list()
}

/// The registries signed in to: `{type: "logins", items: [{server, username,
/// via, icon}], store, stores}`. `store` says where a new sign-in goes, `stores`
/// the same for registries with a helper of their own.
pub fn list() -> String {
    let cfg = read_config();
    let mut items: Vec<Value> = Vec::new();
    let mut add = |key: String, username: String, via: String| {
        if plain_key(&key) && !items.iter().any(|i| i["server"] == key.as_str()) {
            items
                .push(json!({"server": key, "username": username, "via": via, "icon": logo(&key)}));
        }
    };
    if let Some(name) = cfg["credsStore"].as_str().filter(|n| valid_helper(n))
        && let Some(out) = helper_path(name).and_then(|p| run(&p, "list", b"").ok())
    {
        for (key, username) in parse_list(&out) {
            add(key, username, format!("docker-credential-{name}"));
        }
    }
    let mut stores = serde_json::Map::new();
    for (host, name) in cfg["credHelpers"].as_object().into_iter().flatten() {
        let Some(name) = name.as_str().filter(|n| valid_helper(n)) else {
            continue;
        };
        let key = server_key(host);
        stores.insert(key.clone(), store_label(&cfg, &key).into());
        add(key, String::new(), format!("docker-credential-{name}"));
    }
    for (host, entry) in cfg["auths"].as_object().into_iter().flatten() {
        if let Some(l) = inline(entry) {
            add(
                server_key(host),
                if l.username == TOKEN_USER {
                    String::new()
                } else {
                    l.username
                },
                "config.json".to_owned(),
            );
        }
    }
    for (key, username) in os::list() {
        add(key, username, os::NAME.to_owned());
    }
    // Docker Hub first, then by name.
    items.sort_by(|a, b| {
        (a["server"] != HUB, a["server"].as_str()).cmp(&(b["server"] != HUB, b["server"].as_str()))
    });
    json!({"type": "logins", "items": items, "store": store_label(&cfg, HUB), "stores": stores})
        .to_string()
}

/// The page's `logins`, `login` {name: server, value: {username, secret}}
/// and `logout` {name: server}, each on a thread of its own. `ready` starts
/// the engine first where the window does that itself.
pub fn handle(
    cmd: &str,
    server: String,
    value: &Value,
    emit: fn(String),
    ready: fn() -> io::Result<()>,
) {
    let cmd = cmd.to_owned();
    let username = value["username"].as_str().unwrap_or_default().to_owned();
    let secret = value["secret"].as_str().unwrap_or_default().to_owned();
    let _ = std::thread::Builder::new()
        .name("kiri-login".into())
        .spawn(move || match cmd.as_str() {
            "login" => {
                match ready() {
                    Ok(()) => emit(login(&server, &username, &secret)),
                    Err(e) => emit(failed(
                        &server_key(&server),
                        &format!("Kiri's engine isn't running: {e}"),
                    )),
                }
                emit(list());
            }
            "logout" => emit(logout(&server)),
            _ => emit(list()),
        });
}

/// The login keychain, through the Security framework's item calls: the same
/// kind of item docker-credential-osxkeychain keeps (an internet password
/// labelled "Docker Credentials"), so each finds the other's.
#[cfg(target_os = "macos")]
mod os {
    use std::ffi::c_void;
    use std::io;
    use std::ptr::null;

    use super::{HUB, LABEL, Login, server_key};

    pub const NAME: &str = "the Keychain";

    type Ref = *const c_void;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        // Declared as daemon/tls.rs does: only their addresses are used.
        static kCFTypeDictionaryKeyCallBacks: u8;
        static kCFTypeDictionaryValueCallBacks: u8;
        static kCFBooleanTrue: Ref;
        fn CFDictionaryCreate(
            alloc: Ref,
            keys: *const Ref,
            values: *const Ref,
            n: isize,
            kcb: *const u8,
            vcb: *const u8,
        ) -> Ref;
        fn CFDictionaryGetValue(d: Ref, key: Ref) -> Ref;
        fn CFStringCreateWithBytes(
            alloc: Ref,
            bytes: *const u8,
            n: isize,
            encoding: u32,
            external: u8,
        ) -> Ref;
        fn CFStringGetLength(s: Ref) -> isize;
        fn CFStringGetCString(s: Ref, buf: *mut u8, size: isize, encoding: u32) -> u8;
        fn CFDataCreate(alloc: Ref, bytes: *const u8, n: isize) -> Ref;
        fn CFDataGetLength(d: Ref) -> isize;
        fn CFDataGetBytePtr(d: Ref) -> *const u8;
        fn CFArrayGetCount(a: Ref) -> isize;
        fn CFArrayGetValueAtIndex(a: Ref, i: isize) -> Ref;
        fn CFGetTypeID(r: Ref) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFDataGetTypeID() -> usize;
        fn CFArrayGetTypeID() -> usize;
        fn CFDictionaryGetTypeID() -> usize;
        fn CFRelease(r: Ref);
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecClass: Ref;
        static kSecClassInternetPassword: Ref;
        static kSecAttrServer: Ref;
        static kSecAttrPath: Ref;
        static kSecAttrProtocol: Ref;
        static kSecAttrProtocolHTTPS: Ref;
        static kSecAttrAccount: Ref;
        static kSecAttrLabel: Ref;
        static kSecValueData: Ref;
        static kSecReturnData: Ref;
        static kSecReturnAttributes: Ref;
        static kSecMatchLimit: Ref;
        static kSecMatchLimitOne: Ref;
        static kSecMatchLimitAll: Ref;
        fn SecItemAdd(attributes: Ref, result: *mut Ref) -> i32;
        fn SecItemCopyMatching(query: Ref, result: *mut Ref) -> i32;
        fn SecItemDelete(query: Ref) -> i32;
    }

    /// A Core Foundation object of ours, released when dropped.
    struct Cf(Ref);

    impl Drop for Cf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: an object this code created or was given to own.
                unsafe { CFRelease(self.0) };
            }
        }
    }

    fn string(s: &str) -> Cf {
        // SAFETY: the bytes and their length, UTF-8.
        Cf(unsafe { CFStringCreateWithBytes(null(), s.as_ptr(), s.len() as isize, UTF8, 0) })
    }

    fn data(b: &[u8]) -> Cf {
        // SAFETY: the bytes and their length; CFData copies them.
        Cf(unsafe { CFDataCreate(null(), b.as_ptr(), b.len() as isize) })
    }

    /// A dictionary of CF values; it retains them, so they may go after.
    fn dict(pairs: &[(Ref, Ref)]) -> Cf {
        let keys: Vec<Ref> = pairs.iter().map(|p| p.0).collect();
        let values: Vec<Ref> = pairs.iter().map(|p| p.1).collect();
        // SAFETY: two arrays of n CF objects, with the CFType callbacks.
        Cf(unsafe {
            CFDictionaryCreate(
                null(),
                keys.as_ptr(),
                values.as_ptr(),
                pairs.len() as isize,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            )
        })
    }

    fn text(r: Ref) -> Option<String> {
        // SAFETY: type-checked before use as a CFString; the buffer is as long as said.
        unsafe {
            if r.is_null() || CFGetTypeID(r) != CFStringGetTypeID() {
                return None;
            }
            let mut buf = vec![0u8; CFStringGetLength(r) as usize * 4 + 1];
            if CFStringGetCString(r, buf.as_mut_ptr(), buf.len() as isize, UTF8) == 0 {
                return None;
            }
            buf.truncate(buf.iter().position(|&b| b == 0).unwrap_or(buf.len()));
            String::from_utf8(buf).ok()
        }
    }

    fn bytes(r: Ref) -> Option<Vec<u8>> {
        // SAFETY: type-checked before use as a CFData.
        unsafe {
            if r.is_null() || CFGetTypeID(r) != CFDataGetTypeID() {
                return None;
            }
            let n = CFDataGetLength(r) as usize;
            Some(if n == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(CFDataGetBytePtr(r), n).to_vec()
            })
        }
    }

    /// The item's server: Docker Hub's is index.docker.io, path /v1/.
    fn server(key: &str) -> &str {
        if key == HUB { "index.docker.io" } else { key }
    }

    /// Runs a query of Docker's items (for `key`, or all) with `extra` in it.
    fn matching(key: Option<&str>, extra: &[(Ref, Ref)]) -> Result<Cf, i32> {
        let label = string(LABEL);
        let host = key.map(|k| string(server(k)));
        // SAFETY: reading the framework's constant keys.
        let mut pairs = unsafe {
            vec![
                (kSecClass, kSecClassInternetPassword),
                (kSecAttrLabel, label.0),
            ]
        };
        if let Some(h) = &host {
            // SAFETY: as above.
            pairs.push((unsafe { kSecAttrServer }, h.0));
        }
        pairs.extend_from_slice(extra);
        let query = dict(&pairs);
        let mut out: Ref = null();
        // SAFETY: a query dictionary and a place for the result, which we then own.
        let status = unsafe { SecItemCopyMatching(query.0, &mut out) };
        if status == 0 {
            Ok(Cf(out))
        } else {
            Err(status)
        }
    }

    pub fn get(key: &str) -> Option<Login> {
        // SAFETY: reading the framework's constant keys.
        let extra = unsafe {
            [
                (kSecReturnAttributes, kCFBooleanTrue),
                (kSecReturnData, kCFBooleanTrue),
                (kSecMatchLimit, kSecMatchLimitOne),
            ]
        };
        let item = matching(Some(key), &extra).ok()?;
        // SAFETY: a dictionary, checked, read by the framework's keys.
        unsafe {
            if CFGetTypeID(item.0) != CFDictionaryGetTypeID() {
                return None;
            }
            let secret =
                String::from_utf8(bytes(CFDictionaryGetValue(item.0, kSecValueData))?).ok()?;
            Some(Login {
                username: text(CFDictionaryGetValue(item.0, kSecAttrAccount)).unwrap_or_default(),
                secret,
            })
        }
    }

    pub fn set(key: &str, login: &Login) -> io::Result<()> {
        delete(key);
        let (label, host, path, account, secret) = (
            string(LABEL),
            string(server(key)),
            string(if key == HUB { "/v1/" } else { "" }),
            string(&login.username),
            data(login.secret.as_bytes()),
        );
        // SAFETY: reading the framework's constant keys.
        let attrs = dict(&unsafe {
            [
                (kSecClass, kSecClassInternetPassword),
                (kSecAttrLabel, label.0),
                (kSecAttrServer, host.0),
                (kSecAttrPath, path.0),
                (kSecAttrProtocol, kSecAttrProtocolHTTPS),
                (kSecAttrAccount, account.0),
                (kSecValueData, secret.0),
            ]
        });
        // SAFETY: an attributes dictionary; no result asked for.
        match unsafe { SecItemAdd(attrs.0, std::ptr::null_mut()) } {
            0 => Ok(()),
            s => Err(io::Error::other(format!("Keychain error {s}"))),
        }
    }

    pub fn delete(key: &str) {
        let (label, host) = (string(LABEL), string(server(key)));
        // SAFETY: reading the framework's constant keys.
        let query = dict(&unsafe {
            [
                (kSecClass, kSecClassInternetPassword),
                (kSecAttrLabel, label.0),
                (kSecAttrServer, host.0),
            ]
        });
        // SAFETY: a query dictionary.
        let _ = unsafe { SecItemDelete(query.0) };
    }

    /// Every item's registry and username; no passwords, so no prompt.
    pub fn list() -> Vec<(String, String)> {
        // SAFETY: reading the framework's constant keys.
        let extra = unsafe {
            [
                (kSecReturnAttributes, kCFBooleanTrue),
                (kSecMatchLimit, kSecMatchLimitAll),
            ]
        };
        // None is errSecItemNotFound.
        let Ok(items) = matching(None, &extra) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // SAFETY: an array of dictionaries, each checked, read by the framework's keys.
        unsafe {
            if CFGetTypeID(items.0) != CFArrayGetTypeID() {
                return out;
            }
            for i in 0..CFArrayGetCount(items.0) {
                let d = CFArrayGetValueAtIndex(items.0, i);
                if d.is_null() || CFGetTypeID(d) != CFDictionaryGetTypeID() {
                    continue;
                }
                if let Some(host) = text(CFDictionaryGetValue(d, kSecAttrServer)) {
                    out.push((
                        server_key(&host),
                        text(CFDictionaryGetValue(d, kSecAttrAccount)).unwrap_or_default(),
                    ));
                }
            }
        }
        out
    }
}

/// Windows' Credential Manager: generic credentials named by the registry
/// and labelled as docker-credential-wincred labels its own, so each finds
/// the other's.
#[cfg(windows)]
mod os {
    use std::ffi::c_void;
    use std::io;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIAL_ATTRIBUTEW, CREDENTIALW,
        CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW,
    };

    use super::{LABEL, Login, server_key};

    pub const NAME: &str = "Credential Manager";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// A NUL-terminated UTF-16 string, or "" for none.
    ///
    /// # Safety
    /// `p` is null or points at a NUL-terminated UTF-16 string.
    unsafe fn from_wide(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut n = 0;
        // SAFETY: NUL-terminated, as the caller promises.
        while unsafe { *p.add(n) } != 0 {
            n += 1;
        }
        // SAFETY: the n units before the NUL.
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
    }

    /// Whether a credential is Docker's: its "label" attribute says so.
    ///
    /// # Safety
    /// `c` is a credential Windows returned, with its attributes.
    unsafe fn docker(c: &CREDENTIALW) -> bool {
        if c.Attributes.is_null() {
            return false;
        }
        // SAFETY: AttributeCount attributes, each with its keyword and value.
        let attrs = unsafe { std::slice::from_raw_parts(c.Attributes, c.AttributeCount as usize) };
        attrs.iter().any(|a| {
            // SAFETY: as above.
            let keyword = unsafe { from_wide(a.Keyword) };
            let value = if a.Value.is_null() {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(a.Value, a.ValueSize as usize) }
            };
            keyword == "label" && value == LABEL.as_bytes()
        })
    }

    pub fn get(key: &str) -> Option<Login> {
        let target = wide(key);
        let mut cred: *mut CREDENTIALW = null_mut();
        // SAFETY: a NUL-terminated name and a place for Windows' copy, freed below.
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) } == 0
            || cred.is_null()
        {
            return None;
        }
        // SAFETY: the credential Windows just returned, read and then freed.
        unsafe {
            let c = &*cred;
            let secret = if c.CredentialBlob.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
            };
            let username = from_wide(c.UserName);
            CredFree(cred as *const c_void);
            String::from_utf8(secret)
                .ok()
                .filter(|s| !s.is_empty())
                .map(|secret| Login { username, secret })
        }
    }

    pub fn set(key: &str, login: &Login) -> io::Result<()> {
        let mut target = wide(key);
        let mut user = wide(&login.username);
        let mut keyword = wide("label");
        let mut label = LABEL.as_bytes().to_vec();
        let mut secret = login.secret.as_bytes().to_vec();
        // SAFETY: plain structs, all-zero is their empty value.
        let mut attr: CREDENTIAL_ATTRIBUTEW = unsafe { std::mem::zeroed() };
        attr.Keyword = keyword.as_mut_ptr();
        attr.ValueSize = label.len() as u32;
        attr.Value = label.as_mut_ptr();
        // SAFETY: as above.
        let mut c: CREDENTIALW = unsafe { std::mem::zeroed() };
        c.Type = CRED_TYPE_GENERIC;
        c.TargetName = target.as_mut_ptr();
        c.CredentialBlobSize = secret.len() as u32;
        c.CredentialBlob = secret.as_mut_ptr();
        c.Persist = CRED_PERSIST_LOCAL_MACHINE;
        c.AttributeCount = 1;
        c.Attributes = &mut attr;
        c.UserName = user.as_mut_ptr();
        // SAFETY: every pointer in `c` is to a buffer above that outlives the call.
        let ok = unsafe { CredWriteW(&c, 0) } != 0;
        secret.fill(0);
        if ok {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn delete(key: &str) {
        let target = wide(key);
        // SAFETY: a NUL-terminated name.
        let _ = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
    }

    /// Every Docker credential's registry and username, without its secret.
    pub fn list() -> Vec<(String, String)> {
        let mut count = 0u32;
        let mut creds: *mut *mut CREDENTIALW = null_mut();
        // SAFETY: no filter (every credential), Windows' array freed below.
        if unsafe { CredEnumerateW(null(), 0, &mut count, &mut creds) } == 0 || creds.is_null() {
            return Vec::new();
        }
        let mut out = Vec::new();
        // SAFETY: `count` credentials Windows returned, read and then freed.
        unsafe {
            for &c in std::slice::from_raw_parts(creds, count as usize) {
                let c = &*c;
                if c.Type == CRED_TYPE_GENERIC && docker(c) {
                    out.push((server_key(&from_wide(c.TargetName)), from_wide(c.UserName)));
                }
            }
            CredFree(creds as *const c_void);
        }
        out
    }
}

/// Elsewhere there's no store of the system's to keep a sign-in in.
#[cfg(not(any(target_os = "macos", windows)))]
mod os {
    use std::io;

    use super::Login;

    pub const NAME: &str = "a credential helper";

    pub fn get(_key: &str) -> Option<Login> {
        None
    }

    pub fn set(_key: &str, _login: &Login) -> io::Result<()> {
        Err(io::Error::other(
            "set credsStore in ~/.docker/config.json to a credential helper",
        ))
    }

    pub fn delete(_key: &str) {}

    pub fn list() -> Vec<(String, String)> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_registries_as_config_json_does() {
        for hub in [
            "",
            "docker.io",
            "index.docker.io",
            "registry-1.docker.io",
            "https://index.docker.io/v1/",
            "HTTPS://Docker.IO",
        ] {
            assert_eq!(server_key(hub), HUB, "{hub}");
        }
        assert_eq!(server_key("https://ghcr.io"), "ghcr.io");
        assert_eq!(server_key("GHCR.io/"), "ghcr.io");
        assert_eq!(server_key("http://localhost:5000/v2/"), "localhost:5000");
        assert_eq!(registry("nginx:alpine"), HUB);
        assert_eq!(registry("bitnami/redis:7"), HUB);
        assert_eq!(registry("docker.io/library/redis"), HUB);
        assert_eq!(registry("ghcr.io/kiri-dev/api:0.4"), "ghcr.io");
        assert_eq!(registry("localhost:5000/app"), "localhost:5000");
        assert_eq!(registry("localhost/app"), "localhost");
        assert_eq!(
            registry("123.dkr.ecr.eu-west-1.amazonaws.com/app@sha256:abc"),
            "123.dkr.ecr.eu-west-1.amazonaws.com"
        );
        assert!(
            plain_key(HUB)
                && plain_key("localhost:5000")
                && !plain_key("ghcr.io\nget")
                && !plain_key("a b")
        );
    }

    #[test]
    fn picks_the_helper_as_the_cli_does() {
        let cfg = json!({"credsStore": "osxkeychain", "credHelpers": {"https://123.dkr.ecr.us-east-1.amazonaws.com": "ecr-login", "bad.io": "../evil"}});
        assert_eq!(
            helper_name(&cfg, "123.dkr.ecr.us-east-1.amazonaws.com").as_deref(),
            Some("ecr-login")
        );
        assert_eq!(helper_name(&cfg, "ghcr.io").as_deref(), Some("osxkeychain"));
        assert_eq!(helper_name(&cfg, "bad.io").as_deref(), None);
        assert_eq!(helper_name(&json!({"auths": {}}), HUB), None);
        assert_eq!(helper_name(&json!({"credsStore": ""}), HUB), None);
    }

    #[test]
    fn reads_what_config_json_and_helpers_hold() {
        let cfg = json!({"auths": {"https://index.docker.io/v1/": {"auth": b64::encode(b"me:pa:ss")}, "ghcr.io": {}, "quay.io": {"identitytoken": "tok"}}});
        let hub = auths_entry(&cfg, &server_key("docker.io"))
            .and_then(inline)
            .unwrap();
        assert_eq!(
            (hub.username.as_str(), hub.secret.as_str()),
            ("me", "pa:ss")
        );
        assert!(auths_entry(&cfg, "ghcr.io").and_then(inline).is_none());
        assert_eq!(
            parse_config(b"\xEF\xBB\xBF{\"credsStore\":\"wincred\"}").unwrap()["credsStore"],
            "wincred"
        );
        let quay = auths_entry(&cfg, "quay.io").and_then(inline).unwrap();
        assert_eq!(
            (quay.username.as_str(), quay.secret.as_str()),
            (TOKEN_USER, "tok")
        );
        let got = parse_get(br#"{"ServerURL":"ghcr.io","Username":"octo","Secret":"s3"}"#).unwrap();
        assert_eq!((got.username.as_str(), got.secret.as_str()), ("octo", "s3"));
        assert!(parse_get(b"credentials not found in native keychain").is_none());
        assert_eq!(
            parse_list(br#"{"https://ghcr.io":"octo","https://index.docker.io/v1/":"me"}"#),
            vec![
                ("ghcr.io".to_owned(), "octo".to_owned()),
                (HUB.to_owned(), "me".to_owned())
            ]
        );
        // Never the secret.
        assert!(!format!("{got:?}").contains("s3"));
    }

    #[test]
    fn builds_the_header_the_engine_reads() {
        let h = header(
            "ghcr.io",
            &Login {
                username: "me".into(),
                secret: "p?>>?".into(),
            },
        );
        assert!(!h.contains(['+', '/']), "{h}");
        let v: Value = serde_json::from_slice(&b64::decode(&h).unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"username": "me", "password": "p?>>?", "serveraddress": "ghcr.io"})
        );
        let t = header(
            HUB,
            &Login {
                username: TOKEN_USER.into(),
                secret: "tok".into(),
            },
        );
        let v: Value = serde_json::from_slice(&b64::decode(&t).unwrap()).unwrap();
        assert_eq!(v, json!({"identitytoken": "tok", "serveraddress": HUB}));
    }
}
