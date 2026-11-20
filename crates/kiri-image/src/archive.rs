//! `docker load` and `docker save`: image tarballs in Docker's format
//! (`manifest.json` naming a config and layer files per image), which is
//! also what `docker buildx build --load` produces. Archives are streamed:
//! each file goes straight into the blob store as it is read.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::reference::Reference;
use crate::store::{Image, Layer, RunConfig, Store};

/// Largest JSON file kept in memory while reading an archive.
const MAX_JSON: u64 = 4 << 20;

/// Reads an archive into the store. Returns the images it named.
pub fn load(store: &Store, archive: impl Read) -> io::Result<Vec<Image>> {
    let mut tar = tar::Archive::new(archive);
    // Archive path → blob digest and size, for every regular file.
    let mut files: HashMap<String, (String, u64)> = HashMap::new();
    let mut manifest: Option<Vec<u8>> = None;
    let before: Vec<Image> = store.images()?;
    let existed = |d: &str| before.iter().any(|i| i.config_digest == d || i.layers.iter().any(|l| l.digest == d));
    let mut ingested: Vec<String> = Vec::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().trim_start_matches("./").to_owned();
        if path == "manifest.json" {
            let mut buf = Vec::new();
            (&mut entry).take(MAX_JSON).read_to_end(&mut buf)?;
            manifest = Some(buf);
            continue;
        }
        // Every other file is kept, `.json` ones too: in the classic layout
        // (podman save, older docker save) the image config is a top-level
        // `<hex>.json`. Files no image names are removed after the manifest.
        let mut w = store.ingest()?;
        io::copy(&mut entry, &mut w)?;
        let (digest, size) = w.finish()?;
        if !existed(&digest) {
            ingested.push(digest.clone());
        }
        files.insert(path, (digest, size));
    }
    let manifest = manifest.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an image archive: no manifest.json"))?;
    let entries: Vec<Value> = serde_json::from_slice(&manifest).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("manifest.json: {e}")))?;
    let lookup = |p: &str| files.get(p.trim_start_matches("./")).cloned().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("archive is missing {p}")));
    let mut loaded = Vec::new();
    for m in &entries {
        let (config_digest, _) = lookup(m["Config"].as_str().unwrap_or_default())?;
        let config: Value = serde_json::from_slice(&store.read_config(&config_digest)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("image config: {e}")))?;
        let mut layers = Vec::new();
        for l in m["Layers"].as_array().into_iter().flatten() {
            let (digest, size) = lookup(l.as_str().unwrap_or_default())?;
            let media_type = media_type_of(&store.blob_path(&digest)?)?;
            layers.push(Layer { digest, size, media_type: media_type.to_owned() });
        }
        let names: Vec<String> = m["RepoTags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|t| Reference::parse(t).map(|r| r.short()).unwrap_or_else(|_| t.to_owned()))
            .collect();
        // An untagged image is known by its id.
        let names = if names.is_empty() { vec![config_digest.clone()] } else { names };
        for name in names {
            let image = from_config(name, &config_digest, &config, layers.clone());
            store.put_image(image.clone())?;
            loaded.push(image);
        }
    }
    // Files no image uses (OCI index blobs and the like) do not stay.
    let used = |d: &str| loaded.iter().any(|i| i.config_digest == d || i.layers.iter().any(|l| l.digest == d));
    for d in ingested.iter().filter(|d| !used(d)) {
        if let Ok(path) = store.blob_path(d) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(loaded)
}

/// Writes `images` as one archive `docker load` reads back.
pub fn save(store: &Store, images: &[Image], out: impl Write) -> io::Result<()> {
    let mut tar = tar::Builder::new(out);
    let mut written: Vec<String> = Vec::new();
    let mut manifest = Vec::new();
    // Images that share a config are one entry with several tags.
    let mut by_config: Vec<(&Image, Vec<String>)> = Vec::new();
    for i in images {
        match by_config.iter_mut().find(|(first, _)| first.config_digest == i.config_digest) {
            Some((_, tags)) => tags.push(i.name.clone()),
            None => by_config.push((i, if i.name.starts_with("sha256:") { vec![] } else { vec![i.name.clone()] })),
        }
    }
    for (image, tags) in &by_config {
        let blob = |d: &str| format!("blobs/sha256/{}", d.strip_prefix("sha256:").unwrap_or(d));
        for d in std::iter::once(&image.config_digest).chain(image.layers.iter().map(|l| &l.digest)) {
            if written.contains(d) {
                continue;
            }
            let path = store.blob_path(d)?;
            let mut f = std::fs::File::open(&path)?;
            let mut h = tar::Header::new_ustar();
            h.set_size(f.metadata()?.len());
            h.set_mode(0o644);
            h.set_mtime(0);
            h.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut h, blob(d), &mut f)?;
            written.push(d.clone());
        }
        manifest.push(json!({
            "Config": blob(&image.config_digest),
            "RepoTags": tags.iter().map(|t| tagged(t)).collect::<Vec<_>>(),
            "Layers": image.layers.iter().map(|l| blob(&l.digest)).collect::<Vec<_>>(),
        }));
    }
    let bytes = serde_json::to_vec(&manifest).map_err(io::Error::other)?;
    let mut h = tar::Header::new_ustar();
    h.set_size(bytes.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(0);
    h.set_entry_type(tar::EntryType::Regular);
    tar.append_data(&mut h, "manifest.json", bytes.as_slice())?;
    tar.into_inner()?.flush()
}

/// `alpine` → `alpine:latest`, as RepoTags spell it.
fn tagged(name: &str) -> String {
    let last = name.rsplit('/').next().unwrap_or(name);
    if last.contains(':') || name.contains('@') { name.to_owned() } else { format!("{name}:latest") }
}

/// What kind of tar a layer file is, from its first bytes.
fn media_type_of(path: &std::path::Path) -> io::Result<&'static str> {
    let mut magic = [0u8; 4];
    let n = std::fs::File::open(path)?.read(&mut magic)?;
    Ok(match &magic[..n] {
        [0x1f, 0x8b, ..] => "application/vnd.oci.image.layer.v1.tar+gzip",
        [0x28, 0xb5, 0x2f, 0xfd] => "application/vnd.oci.image.layer.v1.tar+zstd",
        _ => "application/vnd.oci.image.layer.v1.tar",
    })
}

/// An image record from its config JSON.
pub fn from_config(name: String, config_digest: &str, config: &Value, layers: Vec<Layer>) -> Image {
    let c = &config["config"];
    let strings = |v: &Value| v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let os = config["os"].as_str().unwrap_or("linux");
    // A config that names no architecture was made for this machine.
    let host = crate::registry::Platform::host_linux();
    let arch = config["architecture"].as_str().unwrap_or(&host.architecture);
    Image {
        name,
        // Loaded images have no registry manifest; the config names them.
        digest: config_digest.to_owned(),
        index_digest: None,
        config_digest: config_digest.to_owned(),
        size: layers.iter().map(|l| l.size).sum(),
        layers,
        platform: format!("{os}/{arch}"),
        created: text(&config["created"]),
        pulled_at: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        config: RunConfig {
            env: strings(&c["Env"]),
            entrypoint: strings(&c["Entrypoint"]),
            cmd: strings(&c["Cmd"]),
            working_dir: text(&c["WorkingDir"]),
            user: text(&c["User"]),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        // Tests run in parallel: each gets its own directory.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("kiri-archive-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = scratch();
        let store = Store::open(&dir).unwrap();
        let put = |bytes: &[u8]| {
            let mut w = store.ingest().unwrap();
            w.write_all(bytes).unwrap();
            w.finish().unwrap()
        };
        let config = json!({"os": "linux", "architecture": "arm64", "created": "2026-01-01T00:00:00Z", "config": {"Cmd": ["echo", "hi"], "Env": ["A=1"]}});
        let (cfg, _) = put(&serde_json::to_vec(&config).unwrap());
        let (layer, size) = put(b"\x1f\x8bnot really gzip");
        let image = from_config("demo:1".into(), &cfg, &config, vec![Layer { digest: layer.clone(), size, media_type: media_type_of(&store.blob_path(&layer).unwrap()).unwrap().into() }]);
        store.put_image(image.clone()).unwrap();

        let mut tarball = Vec::new();
        save(&store, &[image.clone()], &mut tarball).unwrap();

        let other = Store::open(dir.join("other")).unwrap();
        let loaded = load(&other, tarball.as_slice()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "demo:1");
        assert_eq!(loaded[0].config.cmd, ["echo", "hi"]);
        assert_eq!(loaded[0].layers[0].digest, layer);
        assert!(loaded[0].layers[0].media_type.ends_with("gzip"));
        assert_eq!(other.find("demo:1").unwrap().unwrap().config_digest, cfg);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_archives_without_a_manifest() {
        let dir = scratch();
        let store = Store::open(&dir).unwrap();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_ustar();
        h.set_size(1);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, "x", &b"x"[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        assert!(load(&store, bytes.as_slice()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The classic docker-archive layout (podman save, older docker save):
    /// the config is a top-level `<hex>.json`, layers are `<hex>.tar`, with
    /// a `repositories` file and, in the legacy layout, per-layer json.
    #[test]
    fn loads_classic_archives_with_a_top_level_config_json() {
        let dir = scratch();
        let store = Store::open(&dir).unwrap();
        let add = |b: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]| {
            let mut h = tar::Header::new_ustar();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, name, data).unwrap();
        };
        let mut layer = tar::Builder::new(Vec::new());
        add(&mut layer, "etc/hello.txt", b"hello kiri\n");
        let layer = layer.into_inner().unwrap();
        let config = json!({"architecture": "arm64", "os": "linux", "created": "2026-11-09T00:00:00Z",
            "config": {"Env": ["PATH=/usr/bin"], "Cmd": ["/bin/true"]},
            "rootfs": {"type": "layers", "diff_ids": ["sha256:00"]}});
        let config = serde_json::to_vec(&config).unwrap();
        let manifest = json!([{"Config": "91df036f.json", "RepoTags": ["minirepro:flat"], "Layers": ["5e1a.tar"]}]);

        let mut b = tar::Builder::new(Vec::new());
        add(&mut b, "91df036f.json", &config); // before manifest.json, as tar sorts it
        add(&mut b, "5e1a.tar", &layer);
        add(&mut b, "5e1a/json", b"{\"id\":\"5e1a\"}");
        add(&mut b, "index.json", b"{\"schemaVersion\":2}");
        add(&mut b, "manifest.json", &serde_json::to_vec(&manifest).unwrap());
        add(&mut b, "repositories", b"{\"minirepro\":{\"flat\":\"5e1a\"}}");
        let bytes = b.into_inner().unwrap();

        let images = load(&store, bytes.as_slice()).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].name, "minirepro:flat");
        assert_eq!(store.read_config(&images[0].config_digest).unwrap(), config);
        assert_eq!(images[0].layers.len(), 1);
        // Only the config and the layer stay in the store.
        let blobs = std::fs::read_dir(store.blob_path(&images[0].config_digest).unwrap().parent().unwrap()).unwrap().count();
        assert_eq!(blobs, 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tags_get_latest() {
        assert_eq!(tagged("alpine"), "alpine:latest");
        assert_eq!(tagged("localhost:5000/app"), "localhost:5000/app:latest");
        assert_eq!(tagged("app:1"), "app:1");
    }
}
