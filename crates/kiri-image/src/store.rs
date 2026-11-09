//! Content-addressed blob store and the image index on the host.
//!
//! Layout under the data directory:
//!
//! ```text
//! blobs/sha256/<hex>     compressed layers and configs, exactly as the registry served them
//! images.json            the image index
//! tmp/                   in-flight downloads, renamed into blobs/ once verified
//! ```

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The most an image config may be: real ones are a few KB.
pub const MAX_CONFIG: u64 = 16 << 20;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::reference::Reference;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Layer {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

/// What a container needs from the image config to start.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunConfig {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub working_dir: String,
    #[serde(default)]
    pub user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Image {
    /// Short reference, e.g. `alpine:latest`.
    pub name: String,
    /// Digest of the platform manifest.
    pub digest: String,
    /// Digest of the multi-arch index the manifest was picked from, when the
    /// registry served one. Docker lists it in RepoDigests, and a pull by
    /// that digest must find the image by it.
    #[serde(default)]
    pub index_digest: Option<String>,
    pub config_digest: String,
    pub layers: Vec<Layer>,
    /// Compressed size of all layers, which is what `docker images` reports too.
    pub size: u64,
    pub platform: String,
    /// When the image was built, as written in its config (RFC 3339).
    #[serde(default)]
    pub created: String,
    /// Unix seconds when this image was pulled.
    pub pulled_at: u64,
    pub config: RunConfig,
}

impl Image {
    /// The repository part of `name`: `alpine`, `ghcr.io/o/r`.
    pub fn repo(&self) -> &str {
        if let Some((repo, _)) = self.name.split_once('@') {
            return repo;
        }
        match self.name.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => repo,
            _ => &self.name,
        }
    }

    /// `repo@digest` as Docker shows it, by the index's digest if there was one.
    /// None for an image no registry has yet (built, loaded, committed, tagged
    /// for another repository): its digest is its config's, which a manifest's
    /// can never be, as the manifest holds it.
    pub fn repo_digest(&self) -> Option<String> {
        let d = self.index_digest.as_deref().unwrap_or(&self.digest);
        (d != self.config_digest).then(|| format!("{}@{d}", self.repo()))
    }

    /// This image under another name (`docker tag`). Its registry digests
    /// belong to its repository: in another one it has none until a push.
    pub fn tagged(&self, name: String) -> Image {
        let mut i = Image { name, ..self.clone() };
        if i.repo() != self.repo() {
            i.digest = self.config_digest.clone();
            i.index_digest = None;
        }
        i
    }

    /// True if `r` names this image: its name, or a digest of its repository
    /// that is the image's manifest or the index it came from.
    pub fn matches(&self, r: &Reference) -> bool {
        if !r.is_digest() {
            return self.name == r.short();
        }
        let same_repo = Reference::parse(&self.name).is_ok_and(|n| n.registry == r.registry && n.repository == r.repository);
        same_repo && (self.digest == r.reference || self.index_digest.as_deref() == Some(r.reference.as_str()))
    }
}

#[derive(Debug)]
pub struct Store {
    root: PathBuf,
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The filesystem-safe hexadecimal part of a SHA-256 digest.
pub fn digest_hex(digest: &str) -> io::Result<&str> {
    let h = digest.strip_prefix("sha256:").unwrap_or_default();
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("invalid sha256 digest {digest:?}")));
    }
    Ok(h)
}

impl Store {
    /// The per-user data directory: `~/Library/Application Support/Kiri` on
    /// macOS, `$XDG_DATA_HOME/kiri` (or `~/.local/share/kiri`) elsewhere.
    /// `KIRI_DATA_DIR` overrides both.
    pub fn default_root() -> io::Result<PathBuf> {
        if let Some(dir) = std::env::var_os("KIRI_DATA_DIR") {
            return Ok(PathBuf::from(dir));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| io::Error::other("HOME is not set"))?;
        Ok(if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Kiri")
        } else if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            PathBuf::from(xdg).join("kiri")
        } else {
            home.join(".local/share/kiri")
        })
    }

    pub fn open(root: impl Into<PathBuf>) -> io::Result<Store> {
        let root = root.into();
        fs::create_dir_all(root.join("blobs/sha256"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// An image config, read whole for parsing. Configs are small JSON; a
    /// registry could serve a huge blob under that name, so anything over
    /// `MAX_CONFIG` is refused rather than loaded into memory.
    pub fn read_config(&self, digest: &str) -> std::io::Result<Vec<u8>> {
        let path = self.blob_path(digest)?;
        let len = std::fs::metadata(&path)?.len();
        if len > MAX_CONFIG {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("image config {digest} is {len} bytes; the limit is {MAX_CONFIG}")));
        }
        std::fs::read(path)
    }

    pub fn blob_path(&self, digest: &str) -> io::Result<PathBuf> {
        Ok(self.root.join("blobs/sha256").join(digest_hex(digest)?))
    }

    /// True if the blob is present with the expected size. Blobs are only
    /// renamed into place after their digest was verified, so presence is proof.
    pub fn has_blob(&self, digest: &str, size: u64) -> bool {
        self.blob_path(digest).is_ok_and(|p| fs::metadata(p).is_ok_and(|m| m.len() == size))
    }

    pub fn blob_writer(&self, digest: &str) -> io::Result<BlobWriter> {
        let tmp = self.root.join("tmp").join(format!("{}.{}.{}", digest_hex(digest)?, std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        Ok(BlobWriter {
            file: File::create(&tmp)?,
            hasher: Sha256::new(),
            tmp,
            dest: self.blob_path(digest)?,
            expected: digest.to_owned(),
            written: 0,
        })
    }

    /// A writer for a blob whose digest is not known yet (`docker load`):
    /// it is named after what was written once `finish` is called.
    pub fn ingest(&self) -> io::Result<Ingest> {
        let tmp = self.root.join("tmp").join(format!("ingest.{}.{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        Ok(Ingest { file: File::create(&tmp)?, hasher: Sha256::new(), tmp, root: self.root.clone(), written: 0 })
    }

    pub fn images(&self) -> io::Result<Vec<Image>> {
        match fs::read(self.root.join("images.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn find(&self, name: &str) -> io::Result<Option<Image>> {
        let r = Reference::parse(name).ok();
        Ok(self.images()?.into_iter().find(|i| i.name == name || i.digest == name || r.as_ref().is_some_and(|r| i.matches(r))))
    }

    /// Adds `image`, replacing any image with the same name.
    pub fn put_image(&self, image: Image) -> io::Result<()> {
        let mut images = self.images()?;
        images.retain(|i| i.name != image.name);
        images.push(image);
        self.write_index(&images)
    }

    /// Records the manifest a push of `name` left in its registry, so
    /// RepoDigests names what the registry has, as Docker's does: deploy tools
    /// pin to it. Every tag of the image in that repository gets it, as there.
    /// Not if `name` became another image during the push.
    pub fn set_pushed(&self, name: &str, config_digest: &str, digest: &str) -> io::Result<()> {
        let mut images = self.images()?;
        let Some(repo) = images.iter().find(|i| i.name == name && i.config_digest == config_digest).map(|i| i.repo().to_owned()) else { return Ok(()) };
        for i in images.iter_mut().filter(|i| i.config_digest == config_digest && i.repo() == repo) {
            i.digest = digest.to_owned();
            i.index_digest = None;
        }
        self.write_index(&images)
    }

    /// Removes the image and deletes blobs no remaining image uses.
    pub fn remove_image(&self, name: &str) -> io::Result<Option<Image>> {
        let mut images = self.images()?;
        let Some(pos) = images.iter().position(|i| i.name == name) else { return Ok(None) };
        let removed = images.remove(pos);
        self.write_index(&images)?;
        let in_use = |d: &str| images.iter().any(|i| i.config_digest == d || i.layers.iter().any(|l| l.digest == d));
        for d in std::iter::once(&removed.config_digest).chain(removed.layers.iter().map(|l| &l.digest)) {
            if !in_use(d)
                && let Ok(path) = self.blob_path(d)
            {
                let _ = fs::remove_file(path);
            }
        }
        Ok(Some(removed))
    }

    fn write_index(&self, images: &[Image]) -> io::Result<()> {
        let tmp = self.root.join("tmp/images.json");
        let bytes = serde_json::to_vec_pretty(images).map_err(io::Error::other)?;
        fs::write(&tmp, bytes)?;
        fs::rename(tmp, self.root.join("images.json"))
    }
}

/// Writes a blob to a temporary file, hashing as it goes, and only moves it
/// into the store if the digest matches.
#[derive(Debug)]
pub struct BlobWriter {
    file: File,
    hasher: Sha256,
    tmp: PathBuf,
    dest: PathBuf,
    expected: String,
    written: u64,
}

impl Write for BlobWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl BlobWriter {
    /// Verifies the digest and publishes the blob. Returns bytes written.
    pub fn finish(mut self) -> io::Result<u64> {
        let actual = format!("sha256:{}", hex(&std::mem::take(&mut self.hasher).finalize()));
        if actual != self.expected {
            let _ = fs::remove_file(&self.tmp);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("digest mismatch: expected {}, got {actual}", self.expected),
            ));
        }
        self.file.sync_all()?;
        fs::rename(&self.tmp, &self.dest)?;
        Ok(self.written)
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        // A writer dropped without finish() leaves no partial blob behind.
        // After a successful finish() the tmp path no longer exists.
        let _ = fs::remove_file(&self.tmp);
    }
}

/// See [`Store::ingest`].
#[derive(Debug)]
pub struct Ingest {
    file: File,
    hasher: Sha256,
    tmp: PathBuf,
    root: PathBuf,
    written: u64,
}

impl Write for Ingest {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Ingest {
    /// Publishes the blob under its digest. Returns (digest, size).
    pub fn finish(mut self) -> io::Result<(String, u64)> {
        let hex = hex(&std::mem::take(&mut self.hasher).finalize());
        self.file.sync_all()?;
        fs::rename(&self.tmp, self.root.join("blobs/sha256").join(&hex))?;
        Ok((format!("sha256:{hex}"), self.written))
    }
}

impl Drop for Ingest {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.tmp);
    }
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_instead_of_digests() {
        let root = std::env::temp_dir().join(format!("kiri-store-path-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let outside = root.with_extension("outside");
        fs::write(&outside, b"do not touch").unwrap();
        let path = outside.to_string_lossy().into_owned();
        assert!(store.blob_path(&path).is_err());
        assert!(store.blob_path("sha256:../outside").is_err());
        assert!(!store.has_blob(&path, 12));
        assert!(store.blob_writer(&path).is_err());
        assert!(store.read_config(&path).is_err());

        let image = Image {
            name: "malicious:latest".into(),
            digest: "sha256:manifest".into(),
            index_digest: None,
            config_digest: path.clone(),
            layers: vec![Layer { digest: path, size: 12, media_type: "application/vnd.oci.image.layer.v1.tar".into() }],
            size: 12,
            platform: "linux/amd64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        };
        store.put_image(image).unwrap();
        store.remove_image("malicious:latest").unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"do not touch");
        fs::remove_dir_all(&root).unwrap();
        fs::remove_file(&outside).unwrap();
    }

    #[test]
    fn finds_a_pull_by_index_digest_by_either_digest() {
        let root = std::env::temp_dir().join(format!("kiri-store-index-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let (index, manifest) = (format!("sha256:{}", "1".repeat(64)), format!("sha256:{}", "2".repeat(64)));
        let image = Image {
            name: format!("kindest/node@{index}"),
            digest: manifest.clone(),
            index_digest: Some(index.clone()),
            config_digest: format!("sha256:{}", "3".repeat(64)),
            layers: Vec::new(),
            size: 0,
            platform: "linux/arm64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        };
        assert_eq!(image.repo(), "kindest/node");
        assert_eq!(image.repo_digest(), Some(format!("kindest/node@{index}")));
        store.put_image(image).unwrap();
        for name in [format!("kindest/node@{index}"), format!("kindest/node:v1.37.0@{index}"), format!("kindest/node@{manifest}"), manifest.clone()] {
            assert!(store.find(&name).unwrap().is_some(), "{name}");
        }
        assert!(store.find(&format!("other/node@{index}")).unwrap().is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_push_leaves_the_registry_manifest_in_repo_digests() {
        let root = std::env::temp_dir().join(format!("kiri-store-push-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let d = |c: &str| format!("sha256:{}", c.repeat(64));
        let image = |name: &str, config: &str, index: Option<String>| Image {
            name: name.into(),
            digest: d(config),
            index_digest: index,
            config_digest: d(config),
            layers: Vec::new(),
            size: 0,
            platform: "linux/arm64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        };
        // Built here: no registry has it yet, so no RepoDigest, as with Docker.
        store.put_image(image("localhost:5000/built:1", "1", None)).unwrap();
        // The same image's other tags: one in its repository, one in another registry.
        store.put_image(image("localhost:5000/built:latest", "1", None)).unwrap();
        store.put_image(image("localhost:5001/built:1", "1", None)).unwrap();
        // Pulled: alpine's index. Tagged in its repository it keeps that; in
        // another it has none there, rather than a digest that one lacks.
        let pulled = Image { digest: d("8"), ..image("alpine:latest", "2", Some(d("9"))) };
        assert_eq!(pulled.tagged("alpine:old".into()).repo_digest(), Some(format!("alpine@{}", d("9"))));
        store.put_image(pulled.tagged("localhost:5000/tagged:1".into())).unwrap();
        store.put_image(image("localhost:5000/other:1", "3", None)).unwrap();
        let repo_digest = |n: &str| store.find(n).unwrap().unwrap().repo_digest();
        for n in ["localhost:5000/built:1", "localhost:5000/tagged:1", "localhost:5000/other:1"] {
            assert_eq!(repo_digest(n), None, "{n}");
        }
        store.set_pushed("localhost:5000/built:1", &d("1"), &d("a")).unwrap();
        store.set_pushed("localhost:5000/tagged:1", &d("2"), &d("b")).unwrap();
        // The name now another image: that one keeps its own.
        store.set_pushed("localhost:5000/other:1", &d("4"), &d("c")).unwrap();
        assert_eq!(repo_digest("localhost:5000/built:1"), Some(format!("localhost:5000/built@{}", d("a"))));
        // Pushing one tag gives the repository's other tags of it the digest; elsewhere it has none.
        assert_eq!(repo_digest("localhost:5000/built:latest"), Some(format!("localhost:5000/built@{}", d("a"))));
        assert_eq!(repo_digest("localhost:5001/built:1"), None);
        assert_eq!(repo_digest("localhost:5000/tagged:1"), Some(format!("localhost:5000/tagged@{}", d("b"))));
        assert_eq!(repo_digest("localhost:5000/other:1"), None);
        // And a pull by the pushed digest finds the image here.
        assert!(store.find(&format!("localhost:5000/built@{}", d("a"))).unwrap().is_some());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn repo_strips_a_tag_but_not_a_port() {
        let named = |name: &str| Image {
            name: name.into(),
            digest: String::new(),
            index_digest: None,
            config_digest: String::new(),
            layers: Vec::new(),
            size: 0,
            platform: String::new(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        };
        assert_eq!(named("alpine:latest").repo(), "alpine");
        assert_eq!(named("localhost:5000/app:1").repo(), "localhost:5000/app");
        assert_eq!(named("localhost:5000/app").repo(), "localhost:5000/app");
        assert_eq!(named(&format!("kindest/node@sha256:{}", "a".repeat(64))).repo(), "kindest/node");
    }
}
