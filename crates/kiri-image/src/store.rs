//! Content-addressed blob store and the image index on the host.
//!
//! Layout under the data directory:
//!
//! ```text
//! blobs/sha256/<hex>     compressed layers and configs, exactly as the registry served them
//! images.json            the image index
//! images.lock            held while the index is read, changed and rewritten
//! tmp/                   in-flight downloads, renamed into blobs/ once verified
//! ```
//!
//! The index is only ever replaced whole, by renaming a finished temporary
//! file over it, so a reader sees the old index or the new one, never a mix.
//! A change reads the index and writes it back with the lock held, so two
//! pulls or builds finishing together cannot lose each other's image. An
//! index that does not parse (written by an older Kiri, or damaged) is moved
//! aside as `images.json.corrupt-*` and what can be read from it is kept.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

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
    /// The Docker Hub mirror (`host[:port]`) a pull took the image from,
    /// when it did not come from the registry its name says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
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

/// Serializes index changes between the threads of this process; the lock
/// file does it between processes (the CLI opens the store too).
static INDEX_LOCK: Mutex<()> = Mutex::new(());

/// Taken around a blob's rename into place by a writer that holds it
/// ([`Ingest::finish_pinned`]) and around a removal's check of the holds
/// and its delete, so the two never interleave.
static BLOB_LOCK: Mutex<()> = Mutex::new(());

/// Blobs something in this process is using that an image's removal must
/// not delete (a save or a push still sending them, a load until its images
/// are recorded), by path, with how many hold each. One table for the
/// process, so every `Store` of one folder shares it.
static PINNED: Mutex<std::collections::BTreeMap<PathBuf, usize>> = Mutex::new(std::collections::BTreeMap::new());

/// A blob kept from deletion until this is dropped ([`Store::pin`]).
#[derive(Debug)]
pub struct Pin(Option<PathBuf>);

impl Pin {
    fn take(path: PathBuf) -> Pin {
        *PINNED.lock().unwrap_or_else(|e| e.into_inner()).entry(path.clone()).or_insert(0) += 1;
        Pin(Some(path))
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        let Some(p) = self.0.take() else { return };
        let mut m = PINNED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&p) {
            *n -= 1;
            if *n == 0 {
                m.remove(&p);
            }
        }
    }
}

fn pinned(p: &Path) -> bool {
    PINNED.lock().unwrap_or_else(|e| e.into_inner()).contains_key(p)
}

/// Where the program writes error-level lines.
pub type ErrorLog = fn(&str);
static ERROR_LOG: OnceLock<ErrorLog> = OnceLock::new();

/// Called once by the daemon, so a damaged index is reported in its log.
pub fn set_error_log(f: ErrorLog) {
    let _ = ERROR_LOG.set(f);
}

fn log_error(message: &str) {
    match ERROR_LOG.get() {
        Some(log) => log(message),
        None => eprintln!("kiri: {message}"),
    }
}

/// Held while the index is changed. The file goes first so the process-wide
/// lock is the last one released.
#[derive(Debug)]
struct IndexLock {
    _file: Option<File>,
    _guard: MutexGuard<'static, ()>,
}

/// What `images.json` holds.
enum Index {
    Valid(Vec<Image>),
    Corrupt(Vec<u8>, serde_json::Error),
}

/// The images that can still be read from a damaged index, see
/// [`crate::jsonfile::salvage`].
fn salvage(bytes: &[u8]) -> Vec<Image> {
    crate::jsonfile::salvage(bytes)
}

/// The name of every image record in an index, damaged or not, including a
/// record cut short after its name: what a person needs to know was lost.
/// A record's `name` is its first field, so only a `"name"` right after a
/// record's `{` (itself after `[` or `,`) counts, not a label called name.
fn record_names(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let before = |end: usize| text[..end].trim_end().chars().next_back();
    let mut names = Vec::new();
    for (at, _) in text.match_indices("\"name\"") {
        let brace = text[..at].trim_end().len();
        if before(brace) != Some('{') || !matches!(before(brace - 1), Some('[' | ',')) {
            continue;
        }
        let Some(value) = text[at + 6..].trim_start().strip_prefix(':') else { continue };
        if let Some(Ok(name)) = serde_json::Deserializer::from_str(value).into_iter::<String>().next() {
            names.push(name);
        }
    }
    names
}

/// The images a damaged index held that are not in `images`.
fn missing(bytes: &[u8], images: &[Image]) -> Vec<String> {
    let mut names = record_names(bytes);
    names.retain(|n| !images.iter().any(|i| &i.name == n));
    names.dedup();
    names
}

/// The copies of damaged indexes kept beside `images.json`, and the images
/// they name that the index no longer has. See [`Store::index_damage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDamage {
    pub copies: Vec<PathBuf>,
    pub missing: Vec<String>,
}

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

    /// The image index. One that does not parse is repaired first, see
    /// [`Store::recover`], rather than failing every caller from then on.
    pub fn images(&self) -> io::Result<Vec<Image>> {
        match self.read_index()? {
            Index::Valid(images) => Ok(images),
            Index::Corrupt(..) => {
                let _lock = self.lock_index()?;
                self.read_locked()
            }
        }
    }

    fn read_index(&self) -> io::Result<Index> {
        match fs::read(self.root.join("images.json")) {
            Ok(bytes) => Ok(match serde_json::from_slice(&bytes) {
                Ok(images) => Index::Valid(images),
                Err(e) => Index::Corrupt(bytes, e),
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Index::Valid(Vec::new())),
            Err(e) => Err(e),
        }
    }

    /// Takes the index lock. Everything that rewrites the index holds it from
    /// reading the index to renaming the new one into place.
    fn lock_index(&self) -> io::Result<IndexLock> {
        let guard = INDEX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join("images.lock"))?;
        let file = match file.lock() {
            Ok(()) => Some(file),
            // A filesystem without file locks still has the process-wide one.
            Err(e) if e.kind() == io::ErrorKind::Unsupported => None,
            Err(e) => return Err(e),
        };
        Ok(IndexLock {
            _file: file,
            _guard: guard,
        })
    }

    /// The index, read with the lock held, repairing it if it is damaged.
    fn read_locked(&self) -> io::Result<Vec<Image>> {
        match self.read_index()? {
            Index::Valid(images) => Ok(images),
            Index::Corrupt(bytes, error) => Ok(self.recover(&bytes, &error)),
        }
    }

    /// Moves a damaged index aside as `images.json.corrupt-<seconds>-<n>`,
    /// writes back the images that can still be read from it and logs at
    /// error level. Never fails: with the index unwritable the caller still
    /// gets what could be read, and its own write reports the problem.
    ///
    /// Blobs are untouched, so an image lost with the index is pulled again
    /// from the cached layers. The registry's manifests, and so the names and
    /// layer lists, are not kept on the host: only the index itself can say
    /// which image a blob belongs to.
    fn recover(&self, bytes: &[u8], error: &serde_json::Error) -> Vec<Image> {
        let moved = crate::jsonfile::move_aside(&self.root.join("images.json"));
        let images = salvage(bytes);
        let written = if images.is_empty() {
            Ok(())
        } else {
            self.write_index(&images)
        };
        let kept = match &moved {
            Ok(aside) => format!("moved to {}", aside.display()),
            Err(e) => format!("not moved aside: {e}"),
        };
        let wrote = match &written {
            Ok(()) => String::new(),
            Err(e) => format!("; the repaired index was not written: {e}"),
        };
        let lost = missing(bytes, &images);
        let lost = if lost.is_empty() {
            String::new()
        } else {
            format!("; dropped from the list: {} (their layers are kept while the copy is)", lost.join(", "))
        };
        log_error(&format!(
            "image index unreadable ({error}): {kept}, {} image(s) recovered{lost}{wrote}",
            images.len()
        ));
        images
    }

    /// The `images.json.corrupt-*` copies kept beside the index, and the
    /// images they name that it no longer lists; `None` with no copy. While
    /// a copy is kept, the layers of the images it may still hold are not
    /// someone else's to delete: the daemon keeps every unpacked layer and
    /// says so in `docker info`. Removing the copies ends it.
    pub fn index_damage(&self) -> io::Result<Option<IndexDamage>> {
        // Read first: a damaged index is moved aside by this very read.
        let images = self.images()?;
        let copies = crate::jsonfile::kept_copies(&self.root.join("images.json"))?;
        if copies.is_empty() {
            return Ok(None);
        }
        let mut names = Vec::new();
        for c in &copies {
            for n in missing(&fs::read(c).unwrap_or_default(), &images) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        Ok(Some(IndexDamage { copies, missing: names }))
    }

    pub fn find(&self, name: &str) -> io::Result<Option<Image>> {
        let r = Reference::parse(name).ok();
        Ok(self.images()?.into_iter().find(|i| i.name == name || i.digest == name || r.as_ref().is_some_and(|r| i.matches(r))))
    }

    /// Adds `image`, replacing any image with the same name.
    pub fn put_image(&self, image: Image) -> io::Result<()> {
        let _lock = self.lock_index()?;
        let mut images = self.read_locked()?;
        images.retain(|i| i.name != image.name);
        images.push(image);
        self.write_index(&images)
    }

    /// Records the manifest a push of `name` left in its registry, so
    /// RepoDigests names what the registry has, as Docker's does: deploy tools
    /// pin to it. Every tag of the image in that repository gets it, as there.
    /// Not if `name` became another image during the push.
    pub fn set_pushed(&self, name: &str, config_digest: &str, digest: &str) -> io::Result<()> {
        let _lock = self.lock_index()?;
        let mut images = self.read_locked()?;
        let Some(repo) = images.iter().find(|i| i.name == name && i.config_digest == config_digest).map(|i| i.repo().to_owned()) else { return Ok(()) };
        for i in images.iter_mut().filter(|i| i.config_digest == config_digest && i.repo() == repo) {
            i.digest = digest.to_owned();
            i.index_digest = None;
        }
        self.write_index(&images)
    }

    /// Keeps the blob `digest` from deletion while the pin lives. Pin first,
    /// then check the blob is there: a pin taken after the check could miss
    /// a removal in between.
    pub fn pin(&self, digest: &str) -> Pin {
        match self.blob_path(digest) {
            Ok(p) => Pin::take(p),
            Err(_) => Pin(None),
        }
    }

    /// Removes the image and deletes blobs no remaining image uses and
    /// nothing holds ([`Store::pin`]).
    pub fn remove_image(&self, name: &str) -> io::Result<Option<Image>> {
        let _lock = self.lock_index()?;
        let mut images = self.read_locked()?;
        let Some(pos) = images.iter().position(|i| i.name == name) else { return Ok(None) };
        let removed = images.remove(pos);
        self.write_index(&images)?;
        let in_use = |d: &str| images.iter().any(|i| i.config_digest == d || i.layers.iter().any(|l| l.digest == d));
        for d in std::iter::once(&removed.config_digest).chain(removed.layers.iter().map(|l| &l.digest)) {
            if !in_use(d) {
                self.remove_unpinned(d);
            }
        }
        Ok(Some(removed))
    }

    /// Deletes the blob `digest` if no image uses it and nothing holds it,
    /// as [`Store::remove_image`] deletes the blobs it frees: for a writer
    /// that put a blob in that nothing came to name.
    pub fn remove_if_unused(&self, digest: &str) -> io::Result<()> {
        let _lock = self.lock_index()?;
        let images = self.read_locked()?;
        if !images.iter().any(|i| i.config_digest == digest || i.layers.iter().any(|l| l.digest == digest)) {
            self.remove_unpinned(digest);
        }
        Ok(())
    }

    /// The hold check and the delete under the blob lock, which a writer's
    /// pin and rename take too ([`Ingest::finish_pinned`]): a blob written
    /// or found again by a writer between the two is never deleted.
    fn remove_unpinned(&self, digest: &str) {
        let Ok(path) = self.blob_path(digest) else { return };
        let _one = BLOB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if !pinned(&path) {
            let _ = fs::remove_file(path);
        }
    }

    /// Replaces the index with `images`, see [`crate::jsonfile::replace`].
    /// Callers hold the index lock.
    fn write_index(&self, images: &[Image]) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(images).map_err(io::Error::other)?;
        crate::jsonfile::replace(&self.root.join("images.json"), &bytes, crate::jsonfile::Durable::Now)
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

    /// As [`Ingest::finish`], the blob held ([`Store::pin`]) from before
    /// it is in place: a removal of the last image using a blob of the same
    /// digest cannot delete it between the rename and the writer recording
    /// an image that names it (`docker load`).
    pub fn finish_pinned(mut self) -> io::Result<(String, u64, Pin)> {
        let hex = hex(&std::mem::take(&mut self.hasher).finalize());
        self.file.sync_all()?;
        let dest = self.root.join("blobs/sha256").join(&hex);
        let _one = BLOB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let pin = Pin::take(dest.clone());
        fs::rename(&self.tmp, &dest)?;
        Ok((format!("sha256:{hex}"), self.written, pin))
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

    /// A blob something still uses (a save, a push, a load not yet
    /// recorded) outlives the removal of the image that named it.
    #[test]
    fn a_pinned_blob_outlives_the_removal_of_its_image() {
        let root = std::env::temp_dir().join(format!("kiri-store-pin-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let mut ingest = store.ingest().unwrap();
        ingest.write_all(b"a layer a save is sending").unwrap();
        let (layer, size) = ingest.finish().unwrap();
        let image = |name: &str| Image { mirror: None,
            name: name.into(),
            digest: "sha256:manifest".into(),
            index_digest: None,
            config_digest: format!("sha256:{}", "3".repeat(64)),
            layers: vec![Layer { digest: layer.clone(), size, media_type: "application/vnd.oci.image.layer.v1.tar".into() }],
            size,
            platform: "linux/amd64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        };
        store.put_image(image("a:1")).unwrap();
        let pin = store.pin(&layer);
        store.remove_image("a:1").unwrap();
        assert!(store.has_blob(&layer, size), "something still holds it");
        drop(pin);
        store.put_image(image("b:1")).unwrap();
        store.remove_image("b:1").unwrap();
        assert!(!store.has_blob(&layer, size), "no one holds it any more");
        fs::remove_dir_all(&root).unwrap();
    }

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

        let image = Image { mirror: None,
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
        let image = Image { mirror: None,
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
        let image = |name: &str, config: &str, index: Option<String>| Image { mirror: None,
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

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kiri-store-{tag}-test-{}", std::process::id()))
    }

    fn named(name: &str) -> Image {
        Image { mirror: None,
            name: name.into(),
            digest: format!("sha256:{}", "1".repeat(64)),
            index_digest: None,
            config_digest: format!("sha256:{}", "2".repeat(64)),
            layers: Vec::new(),
            size: 0,
            platform: "linux/arm64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        }
    }

    fn names(store: &Store) -> Vec<String> {
        let mut n: Vec<String> = store
            .images()
            .unwrap()
            .into_iter()
            .map(|i| i.name)
            .collect();
        n.sort();
        n
    }

    fn corrupt_files(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("images.json.corrupt-")
            })
            .collect()
    }

    #[test]
    fn a_shorter_rewrite_over_a_longer_index_is_recovered() {
        let root = scratch("interleaved");
        let _ = fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let all: Vec<String> = (0..8).map(|i| format!("app{i}:latest")).collect();
        for n in &all {
            store.put_image(named(n)).unwrap();
        }
        // What two interleaved writers left: a shorter document written over
        // the longer one in place, without truncating, so the old tail is still there.
        let path = root.join("images.json");
        let long = fs::read(&path).unwrap();
        let shorter = serde_json::to_vec_pretty(&store.images().unwrap()[..7]).unwrap();
        let mut torn = long.clone();
        torn[..shorter.len()].copy_from_slice(&shorter);
        assert!(torn.len() > shorter.len() && serde_json::from_slice::<Vec<Image>>(&torn).is_err());
        fs::write(&path, &torn).unwrap();

        // The index reads again, with what its first document held, and the damaged file is kept.
        assert_eq!(names(&store), all[..7]);
        let aside = corrupt_files(&root);
        assert_eq!(aside.len(), 1);
        assert_eq!(fs::read(&aside[0]).unwrap(), torn);
        assert!(serde_json::from_slice::<Vec<Image>>(&fs::read(&path).unwrap()).is_ok());
        // And it takes new images as before.
        store.put_image(named("after:1")).unwrap();
        assert_eq!(names(&store).len(), 8);
        assert_eq!(corrupt_files(&root).len(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_truncated_index_keeps_its_complete_records() {
        let root = scratch("truncated");
        let _ = fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        for n in ["a:1", "b:1", "c:1"] {
            store.put_image(named(n)).unwrap();
        }
        let path = root.join("images.json");
        let bytes = fs::read(&path).unwrap();
        assert_eq!(store.index_damage().unwrap(), None, "no copy, no damage");
        fs::write(&path, &bytes[..bytes.len() - 40]).unwrap();
        // The image cut off is named while the copy is kept (#440), the
        // first time the index is read too, and is no longer missing once it
        // is back in the list.
        let damage = store.index_damage().unwrap().unwrap();
        assert_eq!(names(&store), ["a:1", "b:1"]);
        assert_eq!(corrupt_files(&root).len(), 1);
        assert_eq!((damage.copies, damage.missing), (corrupt_files(&root), vec!["c:1".to_owned()]));
        store.put_image(named("c:1")).unwrap();
        assert!(store.index_damage().unwrap().unwrap().missing.is_empty());
        for c in corrupt_files(&root) {
            fs::remove_file(c).unwrap();
        }
        assert_eq!(store.index_damage().unwrap(), None, "removing the copy ends it");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn record_names_are_the_records_own() {
        let bytes = serde_json::to_vec_pretty(&vec![named("a:1"), named("b:1"), named("c:1")]).unwrap();
        assert_eq!(record_names(&bytes), ["a:1", "b:1", "c:1"]);
        // A field inside a record that is called name too is not a record.
        assert_eq!(record_names(br#"[{"name": "a:1", "config": {"name": "no"}, "x": [1, {"y": 2}]}, {"name":"b:1"}]"#), ["a:1", "b:1"]);
        let compact = serde_json::to_vec(&vec![named("a:1"), named("b:1")]).unwrap();
        assert_eq!(record_names(&compact), ["a:1", "b:1"]);
        // Cut inside the last record, after its name: still named.
        let at = bytes.windows(9).rposition(|w| w == b"\"digest\":").unwrap();
        assert_eq!(missing(&bytes[..at], &[named("a:1")]), ["b:1", "c:1"]);
        // Cut inside the name itself: no half name.
        let at = bytes.windows(5).rposition(|w| w == b"c:1\",").unwrap() + 1;
        assert_eq!(record_names(&bytes[..at]), ["a:1", "b:1"]);
    }

    #[test]
    fn an_unreadable_index_starts_over_without_failing_requests() {
        let root = scratch("garbage");
        let _ = fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        fs::write(root.join("images.json"), b"not json at all").unwrap();
        assert!(store.images().unwrap().is_empty());
        assert!(store.find("alpine:3.20").unwrap().is_none());
        assert_eq!(corrupt_files(&root).len(), 1);
        store.put_image(named("alpine:3.20")).unwrap();
        assert_eq!(names(&store), ["alpine:3.20"]);
        // A change that finds the damage first repairs it too.
        fs::write(root.join("images.json"), b"[{").unwrap();
        store.put_image(named("b:1")).unwrap();
        assert_eq!(names(&store), ["b:1"]);
        assert_eq!(corrupt_files(&root).len(), 2);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn concurrent_writes_all_land_and_leave_no_trailing_bytes() {
        let root = scratch("concurrent");
        let _ = fs::remove_dir_all(&root);
        Store::open(&root).unwrap();
        // Each writer has its own Store, as the daemon and the CLI do.
        let writers: Vec<_> = (0..16)
            .map(|t| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let store = Store::open(&root).unwrap();
                    for i in 0..10_u32 {
                        store.put_image(named(&format!("t{t}/img:{i}"))).unwrap();
                        if i % 3 == 0 {
                            store
                                .remove_image(&format!("t{t}/img:{}", i.saturating_sub(1)))
                                .unwrap();
                        }
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        let bytes = fs::read(root.join("images.json")).unwrap();
        let images: Vec<Image> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(bytes, serde_json::to_vec_pretty(&images).unwrap());
        // Per writer: 10 added; at i = 0, 3, 6, 9 one is removed (img:0, 2, 5, 8).
        assert_eq!(images.len(), 16 * (10 - 4));
        assert!(corrupt_files(&root).is_empty());
        let stray: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn repo_strips_a_tag_but_not_a_port() {
        let named = |name: &str| Image { mirror: None,
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
