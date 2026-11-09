//! Proxies, for networks that need one. Settings → Network picks the mode
//! (`~/.kiri/proxy.json`): the system's own settings (the default: macOS's;
//! on Windows, Windows' as kiri-win sends them; on a Linux host, the proxy
//! variables), a custom proxy, or none. Image pulls, pushes, search and login go through it, and
//! when "pass to containers" is on, new containers get `HTTP_PROXY`,
//! `HTTPS_PROXY` and `NO_PROXY`, as Docker Desktop does. VPNs need nothing
//! here: container traffic leaves through macOS's own networking.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The proxy in effect: URLs for http and https, and the hosts that skip it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effective {
    pub http: Option<String>,
    pub https: Option<String>,
    pub no_proxy: Vec<String>,
    /// macOS uses a PAC file, which Kiri does not run.
    pub pac: bool,
}

pub fn path() -> Option<PathBuf> {
    super::state_dir().ok().map(|d| d.join("proxy.json"))
}

pub fn settings() -> Value {
    let saved = path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null);
    json!({
        "mode": saved["mode"].as_str().filter(|m| ["system", "custom", "none"].contains(m)).unwrap_or("system"),
        "http": saved["http"].as_str().unwrap_or(""),
        "https": saved["https"].as_str().unwrap_or(""),
        "no_proxy": saved["no_proxy"].as_str().unwrap_or(""),
        "containers": saved["containers"].as_bool().unwrap_or(false),
    })
}

pub fn save(v: &Value) -> std::io::Result<()> {
    let path = path().ok_or_else(|| std::io::Error::other("HOME is not set"))?;
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, v.to_string().as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

/// The last answer and the settings it came from, so a burst of layer
/// downloads asks `scutil` once, and changed settings apply at once.
static CACHE: Mutex<Option<(Instant, Value, Effective)>> = Mutex::new(None);

pub fn effective() -> Effective {
    let s = settings();
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, from, e)) = c.as_ref() {
        if at.elapsed() < Duration::from_secs(10) && *from == s {
            return e.clone();
        }
    }
    let list = |v: &str| {
        v.split([',', ' '])
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let url = |v: &str| {
        (!v.trim().is_empty()).then(|| {
            if v.contains("://") {
                v.trim().to_owned()
            } else {
                format!("http://{}", v.trim())
            }
        })
    };
    let e = match s["mode"].as_str() {
        Some("custom") => Effective {
            http: url(s["http"].as_str().unwrap_or("")),
            https: url(s["https"].as_str().unwrap_or("")),
            no_proxy: list(s["no_proxy"].as_str().unwrap_or("")),
            pac: false,
        },
        Some("none") => Effective::default(),
        _ => system(),
    };
    *c = Some((Instant::now(), s, e.clone()));
    e
}

/// The host's own settings when the daemon runs in WSL2: kiri-win reads
/// Windows' and sends them (`PUT /kiri/proxy/system`).
static HOST: Mutex<Option<Effective>> = Mutex::new(None);

pub fn set_host(e: Effective) {
    *HOST.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Windows' settings if kiri-win sent them, else this machine's proxy
/// variables, as Linux programs take them.
#[cfg(not(target_os = "macos"))]
fn system() -> Effective {
    if let Some(e) = HOST.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return e;
    }
    let var = |k: &str| {
        std::env::var(k)
            .or_else(|_| std::env::var(k.to_ascii_lowercase()))
            .ok()
            .filter(|v| !v.trim().is_empty())
    };
    let no_proxy = var("NO_PROXY")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Effective {
        http: var("HTTP_PROXY"),
        https: var("HTTPS_PROXY"),
        no_proxy,
        pac: false,
    }
}

/// macOS's proxy settings for the current network, from `scutil --proxy`.
#[cfg(target_os = "macos")]
fn system() -> Effective {
    let Ok(out) = std::process::Command::new("/usr/sbin/scutil")
        .arg("--proxy")
        .output()
    else {
        return Effective::default();
    };
    parse_scutil(&String::from_utf8_lossy(&out.stdout))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_scutil(text: &str) -> Effective {
    let field = |k: &str| {
        text.lines().find_map(|l| {
            let (key, v) = l.split_once(" : ")?;
            (key.trim() == k).then(|| v.trim().to_owned())
        })
    };
    let proxy = |kind: &str| {
        (field(&format!("{kind}Enable")).as_deref() == Some("1")).then(|| ())?;
        let host = field(&format!("{kind}Proxy"))?;
        let port = field(&format!("{kind}Port")).unwrap_or_else(|| "8080".into());
        Some(format!("http://{host}:{port}"))
    };
    // ExceptionsList : <array> { 0 : *.local ... }
    let mut no_proxy = Vec::new();
    let mut inside = false;
    for l in text.lines() {
        if l.contains("ExceptionsList") {
            inside = true;
        } else if inside && l.trim() == "}" {
            inside = false;
        } else if inside {
            if let Some((_, v)) = l.split_once(" : ") {
                no_proxy.push(v.trim().to_owned());
            }
        }
    }
    Effective {
        http: proxy("HTTP"),
        https: proxy("HTTPS"),
        no_proxy,
        pac: field("ProxyAutoConfigEnable").as_deref() == Some("1"),
    }
}

/// For kiri-image: the proxy registry requests use. Registries are https.
pub fn for_registry() -> Option<Option<(String, Vec<String>)>> {
    let e = effective();
    Some(e.https.or(e.http).map(|url| (url, e.no_proxy)))
}

/// The variables a new container gets when "pass to containers" is on. A
/// proxy on this Mac's loopback is `host.docker.internal` from inside.
pub fn container_env() -> Vec<String> {
    if settings()["containers"] != true {
        return Vec::new();
    }
    let e = effective();
    let inside = |u: &str| {
        u.replace("://127.0.0.1", "://host.docker.internal")
            .replace("://localhost", "://host.docker.internal")
    };
    let mut env = Vec::new();
    for (k, v) in [
        ("HTTP_PROXY", e.http.as_deref()),
        ("HTTPS_PROXY", e.https.as_deref().or(e.http.as_deref())),
    ] {
        if let Some(v) = v {
            env.push(format!("{k}={}", inside(v)));
            env.push(format!("{}={}", k.to_ascii_lowercase(), inside(v)));
        }
    }
    if !env.is_empty() {
        let mut skip = e.no_proxy.clone();
        skip.extend(
            [
                "localhost",
                "127.0.0.1",
                ".kiri.local",
                ".kiri.localhost",
                "host.docker.internal",
            ]
            .map(str::to_owned),
        );
        let skip = skip.join(",");
        env.push(format!("NO_PROXY={skip}"));
        env.push(format!("no_proxy={skip}"));
    }
    env
}

pub fn report() -> Value {
    let e = effective();
    json!({"settings": settings(), "effective": {"http": e.http, "https": e.https, "no_proxy": e.no_proxy, "pac": e.pac}})
}

/// Writes a file only this user can read: proxy settings may carry a
/// `http://user:pass@host` credential.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_scutil() {
        let text = "<dictionary> {\n  ExceptionsList : <array> {\n    0 : *.local\n    1 : 169.254/16\n  }\n  FTPPassive : 1\n  HTTPEnable : 1\n  HTTPPort : 3128\n  HTTPProxy : proxy.corp\n  HTTPSEnable : 1\n  HTTPSPort : 3129\n  HTTPSProxy : proxy.corp\n}\n";
        let e = super::parse_scutil(text);
        assert_eq!(e.http.as_deref(), Some("http://proxy.corp:3128"));
        assert_eq!(e.https.as_deref(), Some("http://proxy.corp:3129"));
        assert_eq!(e.no_proxy, ["*.local", "169.254/16"]);
        assert!(!e.pac);
        let none = super::parse_scutil(
            "<dictionary> {\n  ExceptionsList : <array> {\n    0 : *.local\n  }\n  FTPPassive : 1\n}\n",
        );
        assert_eq!((none.http, none.https), (None, None));
    }
}
