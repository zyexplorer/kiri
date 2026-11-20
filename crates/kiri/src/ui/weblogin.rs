//! Signing in to a registry through the registry's own page, so a password
//! is never typed into Kiri: the Mac's window and Windows' (crates/kiri-win,
//! by #[path]) answer the Pull from menu's Sign in with it.
//!
//! - Docker Hub: the docker CLI's own browser sign-in (`docker login`), on a
//!   pseudo-terminal, which it needs. It shows a one-time code; pressing Open
//!   has it open Docker's page; it saves the sign-in where it always does.
//! - GitHub (ghcr.io): the GitHub CLI's sign-in, asking only to read packages
//!   (`gh auth refresh -s read:packages`, or `gh auth login` when it isn't
//!   signed in), then its token is checked and saved as logins.rs does.
//! - Amazon ECR: the AWS profile already set up: `aws ecr get-login-password`
//!   for the profile's account and region; an expired SSO session signs in
//!   again with `aws sso login`, which opens AWS's page itself.
//!
//! The page sends `weblogin` {name: docker|ghcr|ecr, value: {profile}},
//! `webloginopen`, `webloginsso` {value: {profile}}, `weblogincancel` and
//! `awsprofiles`; it hears `{type: "weblogin", reg, state, ...}` with state
//! code (code, url), waiting, done (user, server), failed (error), notool
//! (tool) or sso (profile), and `{type: "awsprofiles", items, tool}`.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use serde_json::{Value, json};

use super::logins;

/// A program on a pseudo-terminal: what it prints, what to type to it, a way
/// to stop it, and its exit code once it ends.
pub type Run = (Receiver<Vec<u8>>, Sender<Vec<u8>>, Box<dyn Fn() + Send + Sync>, Receiver<Option<i32>>);
/// Starts a program on a pseudo-terminal (term.rs on each system).
pub type Spawn = fn(&Path, &[&str]) -> io::Result<Run>;

const GITHUB_DEVICE: &str = "https://github.com/login/device";
const DOCKER_DEVICE: &str = "https://login.docker.com/activate";
/// The longest a plain CLI call (a status, a token) may take.
const CALL_WAIT: Duration = Duration::from_secs(30);

/// The sign-in going on: what to type to it and how to stop it.
struct Active {
    turn: u64,
    input: Option<Sender<Vec<u8>>>,
    stop: Option<Box<dyn Fn() + Send + Sync>>,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);
static GEN: AtomicU64 = AtomicU64::new(0);

fn active<R>(f: impl FnOnce(&mut Option<Active>) -> R) -> R {
    f(&mut ACTIVE.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Still the sign-in the page is waiting on (not cancelled or replaced).
fn current(turn: u64) -> bool {
    active(|a| a.as_ref().is_some_and(|a| a.turn == turn))
}

fn event(reg: &str, state: &str, more: Value) -> String {
    let mut e = json!({"type": "weblogin", "reg": reg, "state": state});
    if let (Some(e), Value::Object(m)) = (e.as_object_mut(), more) {
        e.extend(m);
    }
    e.to_string()
}

/// The page's commands, each on a thread of its own. `ready` starts the
/// engine first where the window does that itself.
pub fn handle(cmd: &str, reg: String, value: &Value, emit: fn(String), ready: fn() -> io::Result<()>, spawn: Spawn) {
    match cmd {
        "webloginopen" => {
            // Enter: the CLI opens its page in the browser.
            if let Some(input) = active(|a| a.as_ref().and_then(|a| a.input.clone())) {
                let _ = input.send(b"\r".to_vec());
            }
            return;
        }
        // An MFA code the AWS CLI asked for: six digits, typed to it.
        "webloginmfa" => {
            let code = value["code"].as_str().unwrap_or_default();
            if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())
                && let Some(input) = active(|a| a.as_ref().and_then(|a| a.input.clone()))
            {
                let _ = input.send(format!("{code}\r").into_bytes());
            }
            return;
        }
        "weblogincancel" => {
            if let Some(a) = active(Option::take)
                && let Some(stop) = a.stop
            {
                stop();
            }
            return;
        }
        _ => {}
    }
    let cmd = cmd.to_owned();
    let profile = value["profile"].as_str().unwrap_or_default().to_owned();
    let _ = std::thread::Builder::new().name("kiri-weblogin".into()).spawn(move || {
        if cmd == "awsprofiles" {
            emit(aws_profiles());
            return;
        }
        // A new sign-in replaces one going on.
        let turn = GEN.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(old) = active(|a| a.replace(Active { turn, input: None, stop: None }))
            && let Some(stop) = old.stop
        {
            stop();
        }
        if let Err(e) = ready() {
            emit(event(&reg, "failed", json!({"error": format!("Kiri's engine isn't running: {e}")})));
            return;
        }
        let out = match (cmd.as_str(), reg.as_str()) {
            ("weblogin", "docker") => docker(turn, emit, spawn),
            ("weblogin", "ghcr") => github(turn, emit, spawn),
            ("weblogin", "ecr") => ecr(turn, &profile, emit, spawn),
            ("webloginsso", "ecr") => aws_sso(turn, &profile, emit, spawn),
            _ => Some(event(&reg, "failed", json!({"error": "Kiri can't sign in to that registry yet."}))),
        };
        // Cancelled meanwhile: the page has moved on.
        if current(turn) {
            active(|a| *a = None);
            if let Some(e) = out {
                emit(e);
            }
            emit(logins::list());
        }
    });
}

/// Keeps what to type to the sign-in and how to stop it, unless it was
/// cancelled before it started (then it's stopped now).
fn attach(turn: u64, input: &Sender<Vec<u8>>, stop: Box<dyn Fn() + Send + Sync>) {
    let stop = active(|a| match a.as_mut().filter(|a| a.turn == turn) {
        Some(a) => {
            a.input = Some(input.clone());
            a.stop = Some(stop);
            None
        }
        None => Some(stop),
    });
    if let Some(stop) = stop {
        stop();
    }
}

/// `docker login` on a pseudo-terminal: its code and page go to the window;
/// it saves the sign-in itself, where `docker login` always does.
fn docker(turn: u64, emit: fn(String), spawn: Spawn) -> Option<String> {
    let Some(exe) = tool("docker") else {
        return Some(event("docker", "notool", json!({"tool": "docker"})));
    };
    let (output, input, stop, exit) = match spawn(&exe, &["login"]) {
        Ok(r) => r,
        Err(e) => {
            return Some(event("docker", "failed", json!({"error": format!("Couldn't start docker login: {e}")})));
        }
    };
    attach(turn, &input, stop);
    let mut flow = Device::new(DOCKER_DEVICE);
    for line in lines(output) {
        if let Some((code, url)) = flow.feed(&line) {
            emit(event("docker", "code", json!({"code": code, "url": url})));
        }
    }
    let ok = exit.recv().ok().flatten() == Some(0);
    if !ok {
        return Some(event("docker", "failed", json!({"error": flow.why("docker login didn't finish")})));
    }
    let user = logins::find(logins::HUB).map(|l| l.username).unwrap_or_default();
    Some(event("docker", "done", json!({"user": user, "server": logins::HUB})))
}

/// GitHub through the GitHub CLI, asking only to read packages; then its
/// token is checked with ghcr.io and saved as any sign-in is.
fn github(turn: u64, emit: fn(String), spawn: Spawn) -> Option<String> {
    let Some(exe) = tool("gh") else {
        return Some(event("ghcr", "notool", json!({"tool": "gh"})));
    };
    let status = call(&exe, &["auth", "status", "-h", "github.com"]);
    let (signed_in, scopes) = gh_status(&status.clone().unwrap_or_else(|e| e));
    if !signed_in || !can_read_packages(&scopes) {
        let args: &[&str] =
            if signed_in { &["auth", "refresh", "-h", "github.com", "-s", "read:packages"] } else { &["auth", "login", "-h", "github.com", "-w", "-p", "https", "-s", "read:packages"] };
        let (output, input, stop, exit) = match spawn(&exe, args) {
            Ok(r) => r,
            Err(e) => {
                return Some(event("ghcr", "failed", json!({"error": format!("Couldn't start the GitHub CLI: {e}")})));
            }
        };
        attach(turn, &input, stop);
        let mut flow = Device::new(GITHUB_DEVICE);
        for line in lines(output) {
            // gh login asks whether to set up git's sign-in too: no, only packages.
            if line.contains("Authenticate Git") {
                let _ = input.send(b"n\r".to_vec());
            }
            if let Some((code, url)) = flow.feed(&line) {
                emit(event("ghcr", "code", json!({"code": code, "url": url})));
            }
        }
        if exit.recv().ok().flatten() != Some(0) {
            return Some(event("ghcr", "failed", json!({"error": flow.why("GitHub's sign-in didn't finish")})));
        }
    }
    if !current(turn) {
        return None;
    }
    let token = match call(&exe, &["auth", "token", "-h", "github.com"]) {
        Ok(t) => t.trim().to_owned(),
        Err(e) => return Some(event("ghcr", "failed", json!({"error": e}))),
    };
    let user = match call(&exe, &["api", "user", "--jq", ".login"]) {
        Ok(u) => u.trim().to_owned(),
        Err(e) => return Some(event("ghcr", "failed", json!({"error": e}))),
    };
    saved("ghcr", &logins::login("ghcr.io", &user, &token), &user)
}

/// Turns logins.rs's answer into this one's.
fn saved(reg: &str, answer: &str, user: &str) -> Option<String> {
    let a: Value = serde_json::from_str(answer).unwrap_or_default();
    Some(match a["error"].as_str() {
        Some(e) => event(reg, "failed", json!({"error": e})),
        None => event(reg, "done", json!({"user": user, "server": a["server"]})),
    })
}

/// The AWS CLI's profiles: `{type: "awsprofiles", items, tool}`.
fn aws_profiles() -> String {
    let Some(exe) = tool("aws") else {
        return json!({"type": "awsprofiles", "items": [], "tool": false}).to_string();
    };
    let items: Vec<String> = call(&exe, &["configure", "list-profiles"]).unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
    json!({"type": "awsprofiles", "items": items, "tool": true}).to_string()
}

/// A profile's own registry: `<account>.dkr.ecr.<region>.amazonaws.com`,
/// signed in to with `aws ecr get-login-password` (good for 12 hours).
fn ecr(turn: u64, profile: &str, emit: fn(String), spawn: Spawn) -> Option<String> {
    let Some(exe) = tool("aws") else {
        return Some(event("ecr", "notool", json!({"tool": "aws"})));
    };
    if !valid_profile(profile) {
        return Some(event("ecr", "failed", json!({"error": "Choose an AWS profile."})));
    }
    let region = call(&exe, &["configure", "get", "region", "--profile", profile]).unwrap_or_default().trim().to_owned();
    if region.is_empty() {
        return Some(event("ecr", "failed", json!({"error": format!("The profile {profile} has no region. Set one with aws configure --profile {profile}.")})));
    }
    // On a terminal: a profile that assumes a role with an MFA device asks
    // for its code here, and AWS keeps the role's sign-in for the next call.
    let account = match aws_asking(turn, &exe, &["sts", "get-caller-identity", "--profile", profile, "--query", "Account", "--output", "text"], profile, emit, spawn) {
        Ok(a) => a.split_whitespace().rev().find(|w| w.len() == 12 && w.bytes().all(|b| b.is_ascii_digit())).unwrap_or_default().to_owned(),
        Err(e) if e.is_empty() => return None,
        Err(e) if sso_expired(&e) => return Some(event("ecr", "sso", json!({"profile": profile}))),
        Err(e) => return Some(event("ecr", "failed", json!({"error": aws_error(&e)}))),
    };
    if account.is_empty() {
        return Some(event("ecr", "failed", json!({"error": "AWS didn't say which account the profile is in."})));
    }
    let password = match call(&exe, &["ecr", "get-login-password", "--region", &region, "--profile", profile]) {
        Ok(p) => p.trim().to_owned(),
        Err(e) if sso_expired(&e) => return Some(event("ecr", "sso", json!({"profile": profile}))),
        Err(e) => return Some(event("ecr", "failed", json!({"error": aws_error(&e)}))),
    };
    let host = format!("{account}.dkr.ecr.{region}.amazonaws.com");
    saved("ecr", &logins::login(&host, "AWS", &password), profile)
}

/// `aws sso login`: it opens AWS's page itself; its code goes to the window
/// to check against. Then the profile's registry, as above.
fn aws_sso(turn: u64, profile: &str, emit: fn(String), spawn: Spawn) -> Option<String> {
    let Some(exe) = tool("aws") else {
        return Some(event("ecr", "notool", json!({"tool": "aws"})));
    };
    if !valid_profile(profile) {
        return Some(event("ecr", "failed", json!({"error": "Choose an AWS profile."})));
    }
    let (output, input, stop, exit) = match spawn(&exe, &["sso", "login", "--profile", profile]) {
        Ok(r) => r,
        Err(e) => {
            return Some(event("ecr", "failed", json!({"error": format!("Couldn't start aws sso login: {e}")})));
        }
    };
    attach(turn, &input, stop);
    emit(event("ecr", "waiting", json!({})));
    let mut flow = Device::new("");
    for line in lines(output) {
        if let Some((code, url)) = flow.feed(&line) {
            emit(event("ecr", "waiting", json!({"code": code, "url": url})));
        }
    }
    if exit.recv().ok().flatten() != Some(0) {
        return Some(event("ecr", "failed", json!({"error": flow.why("AWS's sign-in didn't finish")})));
    }
    if !current(turn) {
        return None;
    }
    ecr(turn, profile, emit, spawn)
}

/// Runs the AWS CLI on a terminal so it can ask for an MFA code: the
/// question goes to the window (`mfa`, with the device's name), the code
/// comes back through `webloginmfa`. A code AWS turns down is asked for
/// again, up to three times. What it printed, or why it failed ("" when the
/// sign-in was cancelled).
fn aws_asking(turn: u64, exe: &Path, args: &[&str], profile: &str, emit: fn(String), spawn: Spawn) -> Result<String, String> {
    for attempt in 0..3 {
        let (output, input, stop, exit) = spawn(exe, args).map_err(|e| format!("Couldn't run aws: {e}"))?;
        attach(turn, &input, stop);
        let (mut asked, mut said) = (false, Vec::new());
        for line in lines(output) {
            if let Some(device) = mfa_device(&line) {
                asked = true;
                emit(event("ecr", "mfa", json!({"profile": profile, "device": device, "bad": attempt > 0})));
            } else if !line.trim().is_empty() {
                said.push(line.trim().to_owned());
            }
        }
        let code = exit.recv().ok().flatten();
        if !current(turn) {
            return Err(String::new());
        }
        let said = said.join("\n");
        if code == Some(0) {
            return Ok(said);
        }
        if !(asked && mfa_refused(&said)) {
            return Err(said);
        }
    }
    Err("AWS didn't accept the MFA code.".to_owned())
}

/// "Enter MFA code for arn:aws:iam::123456789012:mfa/my-phone: ": the device's name.
fn mfa_device(line: &str) -> Option<String> {
    let rest = line.split("Enter MFA code for").nth(1)?;
    let arn = rest.trim().trim_end_matches(':').trim();
    Some(arn.rsplit('/').next().filter(|d| *d != arn).unwrap_or_default().to_owned())
}

fn mfa_refused(said: &str) -> bool {
    let s = said.to_lowercase();
    s.contains("multifactorauthentication failed") || s.contains("invalid mfa") || s.contains("mfa one time pass code")
}

/// The AWS CLI's message, said plainly: the part after "operation:" of its
/// "An error occurred (…) when calling the … operation: …", else its last line.
fn aws_error(said: &str) -> String {
    let line = said.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("The AWS CLI failed.");
    let line = line.trim_start_matches("aws: [ERROR]:").trim();
    match said.lines().find_map(|l| l.split(" operation: ").nth(1).filter(|_| l.contains("An error occurred"))) {
        Some(m) => m.trim().to_owned(),
        None if line.is_empty() => "The AWS CLI failed.".to_owned(),
        None => line.to_owned(),
    }
}

/// An AWS profile name as the CLI allows it, never an option.
fn valid_profile(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('-') && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.@+".contains(&b))
}

/// The AWS CLI saying its SSO session is over.
fn sso_expired(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("token has expired") || e.contains("sso session") || e.contains("aws sso login") || e.contains("error loading sso token")
}

/// What a device sign-in prints, read a line at a time: the one-time code and
/// the page to enter it on, each once; and its last words, to say why it failed.
struct Device {
    code: Option<String>,
    url: Option<String>,
    default_url: &'static str,
    sent: bool,
    last: String,
}

impl Device {
    fn new(default_url: &'static str) -> Self {
        Device { code: None, url: None, default_url, sent: false, last: String::new() }
    }

    /// The code and page once both are known (or the code and the usual page).
    fn feed(&mut self, line: &str) -> Option<(String, String)> {
        if !line.trim().is_empty() {
            self.last = line.trim().to_owned();
        }
        if self.code.is_none() && line.to_lowercase().contains("code") {
            self.code = device_code(line);
        }
        if self.url.is_none() {
            self.url = line.split_whitespace().map(|w| w.trim_end_matches(['.', ',', ')'])).find(|w| w.starts_with("https://")).map(str::to_owned);
        }
        if self.sent {
            return None;
        }
        let url = self.url.clone().or_else(|| (!self.default_url.is_empty()).then(|| self.default_url.to_owned()))?;
        let code = self.code.clone()?;
        self.sent = true;
        Some((code, url))
    }

    fn why(&self, fallback: &str) -> String {
        if self.last.is_empty() { fallback.to_owned() } else { self.last.clone() }
    }
}

/// A device code such as WDJB-MJHT or 1A2B-3C4D in a line.
fn device_code(line: &str) -> Option<String> {
    line.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-'))
        .find(|w| {
            let b = w.as_bytes();
            b.len() == 9 && b[4] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || c.is_ascii_uppercase() || c.is_ascii_digit())
        })
        .map(str::to_owned)
}

/// `gh auth status`: whether the active account is signed in, and its scopes.
fn gh_status(out: &str) -> (bool, Vec<String>) {
    let mut blocks: Vec<(bool, Vec<String>)> = Vec::new();
    for line in out.lines() {
        let l = line.trim();
        if l.contains("Logged in to") {
            blocks.push((false, Vec::new()));
        } else if let Some(b) = blocks.last_mut() {
            if l.contains("Active account: true") {
                b.0 = true;
            } else if let Some(s) = l.split("Token scopes:").nth(1) {
                b.1 = s.split(',').map(|s| s.trim().trim_matches(['\'', '"']).to_owned()).filter(|s| !s.is_empty()).collect();
            }
        }
    }
    // Older gh prints no "Active account" line: one account, the active one.
    match blocks.iter().position(|b| b.0).or((!blocks.is_empty()).then_some(0)) {
        Some(i) => (true, blocks.swap_remove(i).1),
        None => (false, Vec::new()),
    }
}

fn can_read_packages(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == "read:packages" || s == "write:packages")
}

/// What a terminal program prints, as plain lines: colours and cursor moves
/// taken out, split at carriage returns and newlines.
fn lines(output: Receiver<Vec<u8>>) -> impl Iterator<Item = String> {
    let mut pending = String::new();
    let mut chunks = output.into_iter();
    let mut ready: std::collections::VecDeque<String> = Default::default();
    std::iter::from_fn(move || {
        loop {
            if let Some(l) = ready.pop_front() {
                return Some(l);
            }
            match chunks.next() {
                Some(bytes) => {
                    pending.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(i) = pending.find(['\r', '\n']) {
                        let line: String = pending.drain(..=i).collect();
                        ready.push_back(plain(line.trim_end_matches(['\r', '\n'])));
                    }
                    // A prompt waits without a newline: let it be read too.
                    if pending.ends_with("...") || pending.ends_with('…') || pending.ends_with(") ") || pending.ends_with(": ") {
                        ready.push_back(plain(&std::mem::take(&mut pending)));
                    }
                }
                None if !pending.is_empty() => return Some(plain(&std::mem::take(&mut pending))),
                None => return None,
            }
        }
    })
}

/// Text without terminal escape sequences.
fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            if !c.is_control() || c == '\t' {
                out.push(c);
            }
            continue;
        }
        match it.next() {
            // CSI: parameters, then one final byte from @ to ~.
            Some('[') => {
                for c in it.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \.
            Some(']') => {
                while let Some(c) = it.next() {
                    if c == '\x07' || (c == '\x1b' && it.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A CLI the sign-in uses: on PATH, else where its installer puts it (an app
/// started from Finder or the Start menu has a short PATH).
fn tool(name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let path: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    path.into_iter().chain(tool_dirs()).map(|d| d.join(&file)).find(|f| f.is_file())
}

fn tool_dirs() -> Vec<PathBuf> {
    let mut v = logins::helper_dirs();
    if cfg!(windows) {
        for pf in ["ProgramFiles", "ProgramW6432"].iter().filter_map(std::env::var_os).map(PathBuf::from) {
            v.push(pf.join(r"GitHub CLI"));
            v.push(pf.join(r"Amazon\AWSCLIV2"));
        }
    } else {
        v.extend(["/usr/bin", "/snap/bin", "/home/linuxbrew/.linuxbrew/bin"].map(PathBuf::from));
    }
    v
}

/// Runs a CLI without a terminal: what it printed when it succeeds, its
/// message when not. One that hangs is given up on.
fn call(exe: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = exe.parent() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::iter::once(dir.to_path_buf()).chain(logins::helper_dirs()).chain(std::env::split_paths(&path));
        if let Ok(p) = std::env::join_paths(dirs) {
            cmd.env("PATH", p);
        }
    }
    cmd.env("NO_COLOR", "1").env("GH_PROMPT_DISABLED", "1").env("AWS_PAGER", "");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashing up.
        cmd.creation_flags(0x0800_0000);
    }
    let name = exe.file_stem().and_then(|n| n.to_str()).unwrap_or("it").to_owned();
    let child = cmd.spawn().map_err(|e| format!("Couldn't run {name}: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = match rx.recv_timeout(CALL_WAIT) {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("{name} failed: {e}")),
        Err(_) => return Err(format!("{name} didn't answer")),
    };
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_owned();
    if out.status.success() {
        // gh auth status writes to stderr on some versions.
        let (o, e) = (text(&out.stdout), text(&out.stderr));
        Ok(if o.is_empty() { e } else { o })
    } else {
        let e = text(&out.stderr);
        let o = text(&out.stdout);
        Err(if !e.is_empty() {
            e
        } else if !o.is_empty() {
            o
        } else {
            format!("{name} failed")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_logins_code_and_page_are_read_from_its_coloured_output() {
        let out = "\x1b[1m\r\nUSING WEB-BASED LOGIN\x1b[0m\r\n\r\nYour one-time device confirmation code is: \x1b[1mRMCT-BQHZ\r\n\x1b[0m\x1b[1mPress ENTER\x1b[0m to open your browser or submit your device code here: \x1b[4mhttps://login.docker.com/activate\r\n\x1b[0m\r\nWaiting for authentication in the browser…";
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(out.as_bytes().to_vec()).unwrap();
        drop(tx);
        let mut flow = Device::new(DOCKER_DEVICE);
        let got: Vec<_> = lines(rx).filter_map(|l| flow.feed(&l)).collect();
        assert_eq!(got, vec![("RMCT-BQHZ".to_owned(), "https://login.docker.com/activate".to_owned())]);
        assert_eq!(flow.last, "Waiting for authentication in the browser…");
    }

    #[test]
    fn ghs_code_comes_with_githubs_page_when_it_names_none() {
        let mut flow = Device::new(GITHUB_DEVICE);
        assert_eq!(flow.feed("! First copy your one-time code: 1A2B-3C4D"), Some(("1A2B-3C4D".into(), GITHUB_DEVICE.into())));
        assert_eq!(flow.feed("Press Enter to open github.com in your browser..."), None);
    }

    #[test]
    fn words_that_only_look_like_codes_are_not_taken() {
        assert_eq!(device_code("the code is abcd-efgh"), None);
        assert_eq!(device_code("code: ABCDE-FGH"), None);
        assert_eq!(device_code("code (WDJB-MJHT)."), Some("WDJB-MJHT".into()));
    }

    #[test]
    fn gh_status_finds_the_active_accounts_scopes() {
        let out = "github.com\n  ✓ Logged in to github.com account work (keyring)\n  - Active account: false\n  - Token scopes: 'read:packages'\n\n  ✓ Logged in to github.com account me (keyring)\n  - Active account: true\n  - Token scopes: 'gist', 'read:org', 'repo'\n";
        let (signed_in, scopes) = gh_status(out);
        assert!(signed_in);
        assert_eq!(scopes, ["gist", "read:org", "repo"]);
        assert!(!can_read_packages(&scopes));
        assert!(can_read_packages(&["write:packages".to_owned()]));
        assert_eq!(gh_status("You are not logged into any GitHub hosts. To log in, run: gh auth login"), (false, vec![]));
    }

    #[test]
    fn the_mfa_question_names_its_device() {
        assert_eq!(mfa_device("Enter MFA code for arn:aws:iam::123456789012:mfa/my-phone: "), Some("my-phone".into()));
        assert_eq!(mfa_device("Enter MFA code for arn:aws:iam::123456789012:mfa: "), Some(String::new()));
        assert_eq!(mfa_device("123456789012"), None);
    }

    #[test]
    fn a_refused_code_is_told_apart_and_aws_errors_are_said_plainly() {
        let refused = "An error occurred (AccessDenied) when calling the AssumeRole operation: MultiFactorAuthentication failed with invalid MFA one time pass code.";
        assert!(mfa_refused(refused));
        assert_eq!(aws_error(refused), "MultiFactorAuthentication failed with invalid MFA one time pass code.");
        assert_eq!(aws_error("getpass warning\naws: [ERROR]: The config profile (x) could not be found"), "The config profile (x) could not be found");
        assert!(!mfa_refused("An error occurred (ExpiredToken) when calling the GetCallerIdentity operation: The security token included in the request is expired"));
    }

    #[test]
    fn the_mfa_prompt_is_read_though_it_ends_without_a_newline() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(b"Enter MFA code for arn:aws:iam::1:mfa/S24: ".to_vec()).unwrap();
        let mut it = lines(rx);
        assert_eq!(it.next().as_deref(), Some("Enter MFA code for arn:aws:iam::1:mfa/S24: "));
        drop(tx);
    }

    #[test]
    fn a_profile_is_never_taken_as_an_option() {
        assert!(valid_profile("dev-readonly"));
        assert!(!valid_profile("--debug"));
        assert!(!valid_profile("a b"));
        assert!(!valid_profile(""));
    }

    #[test]
    fn an_expired_sso_session_is_recognised() {
        assert!(sso_expired("Error when retrieving token from sso: Token has expired and refresh failed"));
        assert!(!sso_expired("An error occurred (AccessDenied) when calling the GetCallerIdentity operation"));
    }
}
