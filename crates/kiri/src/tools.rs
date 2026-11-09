//! Kiri Lite's docker CLI, Compose and buildx. Kiri.app bundles them; Kiri
//! Lite downloads the same pinned versions (cli_pins.rs) on first launch
//! into `~/Library/Application Support/Kiri/cli`, checks each against its
//! SHA-256 before anything is put in place, and moves it there atomically.
//! Downloads go through Kiri's proxy settings (Settings → Network).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::cli_pins::MacArch;

/// Where Kiri Lite keeps the tools.
pub fn dir() -> io::Result<PathBuf> {
    Ok(kiri_image::Store::default_root()?.join("cli"))
}

/// The three files, as they sit in `dir()`.
const FILES: [&str; 3] = [
    "docker",
    "cli-plugins/docker-compose",
    "cli-plugins/docker-buildx",
];

pub fn installed() -> bool {
    dir().is_ok_and(|d| FILES.iter().all(|f| d.join(f).is_file()))
}

/// Whether this is Kiri Lite: the app bundle without the bundled CLI.
pub fn lite() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.canonicalize().ok())
        .and_then(|e| e.parent().map(|m| m.join("../Resources")))
        .is_some_and(|r| r.join("guest/Image").is_file() && !r.join("bin/docker").is_file())
}

pub(crate) fn agent() -> ureq::Agent {
    let mut cfg = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent(concat!("kiri/", env!("CARGO_PKG_VERSION")));
    let e = crate::daemon::proxy::effective();
    if let Some(p) = e.https.or(e.http).and_then(|u| ureq::Proxy::new(&u).ok()) {
        cfg = cfg.proxy(Some(p));
    }
    cfg.build().into()
}

/// The file's SHA-256, as lowercase hex.
pub fn sha256(path: &Path) -> io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Refuses a file whose SHA-256 is not the pinned one, and deletes it.
pub fn verify(path: &Path, want: &str) -> io::Result<()> {
    let got = sha256(path)?;
    if got != want {
        let _ = std::fs::remove_file(path);
        return Err(io::Error::other(format!(
            "{}: checksum {got} is not the expected {want}",
            path.file_name().unwrap_or_default().to_string_lossy()
        )));
    }
    Ok(())
}

/// One download into `to`, reporting percent as it goes.
pub(crate) fn download(
    agent: &ureq::Agent,
    url: &str,
    to: &Path,
    progress: &mut dyn FnMut(u8),
) -> io::Result<()> {
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| io::Error::other(format!("{url}: {e}")))?;
    let total: Option<u64> = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let mut body = resp.into_body().into_reader();
    let mut out = std::fs::File::create(to)?;
    let (mut got, mut last) = (0u64, 255u8);
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        got += n as u64;
        if let Some(t) = total.filter(|t| *t > 0) {
            let pct = (got * 100 / t).min(100) as u8;
            if pct != last {
                last = pct;
                progress(pct);
            }
        }
    }
    out.sync_all()?;
    Ok(())
}

/// Downloads whatever is missing. `progress(file, index, count, percent)`.
pub fn ensure(progress: &mut dyn FnMut(&str, usize, usize, u8)) -> io::Result<()> {
    let dir = dir()?;
    std::fs::create_dir_all(dir.join("cli-plugins"))?;
    let tmp = dir.join(format!(".download-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let result = (|| {
        let agent = agent();
        // This Mac's builds: Apple Silicon or Intel.
        let mac = MacArch::HOST;
        let [(dv, dsum), (cv, csum), (bv, bsum)] = mac.tools();
        let items = [
            ("docker", mac.docker_url(dv), dsum, FILES[0]),
            ("Compose", mac.compose_url(cv), csum, FILES[1]),
            ("buildx", mac.buildx_url(bv), bsum, FILES[2]),
        ];
        for (i, (label, url, sum, file)) in items.iter().enumerate() {
            let dest = dir.join(file);
            if dest.is_file() {
                progress(label, i + 1, items.len(), 100);
                continue;
            }
            let part = tmp.join(format!("{i}.part"));
            download(&agent, url, &part, &mut |pct| {
                progress(label, i + 1, items.len(), pct)
            })?;
            verify(&part, sum)?;
            let ready = if *file == "docker" {
                // The CLI is docker/docker inside the release tarball.
                let st = std::process::Command::new("/usr/bin/tar")
                    .arg("-xzf")
                    .arg(&part)
                    .arg("-C")
                    .arg(&tmp)
                    .arg("docker/docker")
                    .status()?;
                if !st.success() {
                    return Err(io::Error::other("could not unpack the docker CLI"));
                }
                tmp.join("docker/docker")
            } else {
                part
            };
            std::fs::set_permissions(&ready, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
            std::fs::rename(&ready, &dest)?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn refuses_a_wrong_checksum() {
        let dir = std::env::temp_dir().join(format!("kiri-tools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x");
        std::fs::write(&f, b"hello").unwrap();
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(super::verify(&f, good).is_ok());
        assert!(super::verify(&f, &"0".repeat(64)).is_err());
        assert!(!f.exists(), "a refused file is deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Downloads the real tools into `KIRI_DATA_DIR`:
    /// `KIRI_LIVE_TOOLS=1 KIRI_DATA_DIR=/tmp/x cargo test -p kiri tools_download -- --nocapture`.
    #[test]
    fn tools_download() {
        if std::env::var_os("KIRI_LIVE_TOOLS").is_none() {
            return;
        }
        let mut seen = 0;
        super::ensure(&mut |file, i, n, pct| {
            if pct % 25 == 0 {
                eprintln!("{file} ({i} of {n}): {pct}%");
            }
            seen += 1;
        })
        .unwrap();
        assert!(seen > 3 && super::installed());
    }
}
