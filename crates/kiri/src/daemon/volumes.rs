//! Named volumes: directories on the VM's data disk, recorded in
//! `volumes.json`. Anonymous volumes (an image's VOLUME lines, `-v /path`)
//! are ordinary volumes with generated names that `--rm` and `rm -v` delete.

use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where a volume's data lives inside the VM.
pub fn guest_path(name: &str) -> String {
    format!("/var/lib/kiri/volumes/{name}/_data")
}

/// Docker's rule: `[a-zA-Z0-9][a-zA-Z0-9_.-]*`.
pub fn valid_name(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    pub created_ns: u64,
    #[serde(default)]
    pub labels: serde_json::Value,
    #[serde(default)]
    pub anonymous: bool,
    /// The local driver's options (`--opt`, Compose `driver_opts`):
    /// `type`, `device`, `o`. Empty for a plain directory.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct Volumes {
    list: Vec<Volume>,
    path: PathBuf,
}

impl Volumes {
    pub fn load(root: &std::path::Path) -> io::Result<Volumes> {
        let path = root.join("volumes.json");
        let list = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Volumes { list, path })
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_vec(&self.list) {
            let tmp = self.path.with_extension("json.tmp");
            if let Err(e) =
                std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &self.path))
            {
                crate::diag::error!("saving volumes failed", error = e);
            }
        }
    }

    pub fn all(&self) -> &[Volume] {
        &self.list
    }

    pub fn get(&self, name: &str) -> Option<&Volume> {
        self.list.iter().find(|v| v.name == name)
    }

    /// Creates the volume if it does not exist yet; returns it either way.
    pub fn ensure(
        &mut self,
        name: &str,
        labels: serde_json::Value,
        anonymous: bool,
    ) -> Result<Volume, String> {
        self.ensure_with(name, labels, anonymous, BTreeMap::new())
    }

    /// [`Volumes::ensure`] with the local driver's options for a new volume.
    /// An existing volume keeps its own, as Docker's local driver does.
    pub fn ensure_with(
        &mut self,
        name: &str,
        labels: serde_json::Value,
        anonymous: bool,
        options: BTreeMap<String, String>,
    ) -> Result<Volume, String> {
        if !valid_name(name) {
            return Err(format!(
                "invalid volume name {name:?}: only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"
            ));
        }
        if let Some(v) = self.get(name) {
            return Ok(v.clone());
        }
        let v = Volume {
            name: name.to_owned(),
            created_ns: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            labels,
            anonymous,
            options,
        };
        self.list.push(v.clone());
        self.save();
        Ok(v)
    }

    pub fn forget(&mut self, name: &str) -> bool {
        let before = self.list.len();
        self.list.retain(|v| v.name != name);
        let removed = self.list.len() != before;
        if removed {
            self.save();
        }
        removed
    }
}

/// Checks `DriverOpts` for the local driver as Docker's does: only `type`,
/// `device` and `o`, and none of them alone (`size` needs a quota-capable
/// filesystem, which the data disk is not).
pub fn local_options(opts: &serde_json::Value) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let Some(map) = opts.as_object() else {
        return Ok(out);
    };
    for (k, v) in map {
        let v = v.as_str().unwrap_or_default();
        match k.as_str() {
            "type" | "device" | "o" => {}
            "size" => return Err("quota size requested but no quota support".into()),
            _ => return Err(format!("invalid option: {k:?}")),
        }
        if v.contains(['\t', '\n', '\0']) {
            return Err(format!("invalid value for option {k:?}"));
        }
        out.insert(k.clone(), v.to_owned());
    }
    for (opt, needs) in [
        ("device", &["type"][..]),
        ("type", &["device"]),
        ("o", &["device", "type"]),
    ] {
        if out.contains_key(opt) {
            if let Some(missing) = needs.iter().find(|n| !out.contains_key(**n)) {
                return Err(format!("missing required option: {missing:?}"));
            }
        }
    }
    if out.get("type").map(String::as_str) == Some("cifs") {
        let host = out["device"]
            .strip_prefix("//")
            .and_then(|d| d.split('/').next())
            .unwrap_or("");
        if host
            .rsplit_once(':')
            .is_some_and(|(_, p)| p.bytes().all(|b| b.is_ascii_digit()))
            && !host.ends_with(']')
        {
            return Err("port not allowed in CIFS device URL, include 'port' in 'o='".into());
        }
    }
    Ok(out)
}

/// Mount flags by option word, as Docker's `mount.ParseOptions` reads `o`:
/// (word, clears the flag, flag). Everything else is filesystem data.
const MOUNT_FLAGS: [(&str, bool, u64); 25] = [
    ("defaults", false, 0),
    ("ro", false, 1),
    ("rw", true, 1),
    ("suid", true, 2),
    ("nosuid", false, 2),
    ("dev", true, 4),
    ("nodev", false, 4),
    ("exec", true, 8),
    ("noexec", false, 8),
    ("sync", false, 16),
    ("async", true, 16),
    ("remount", false, 32),
    ("mand", false, 64),
    ("nomand", true, 64),
    ("dirsync", false, 128),
    ("atime", true, 1024),
    ("noatime", false, 1024),
    ("diratime", true, 2048),
    ("nodiratime", false, 2048),
    ("bind", false, 4096),
    ("rbind", false, 4096 | 16384),
    ("relatime", false, 1 << 21),
    ("norelatime", true, 1 << 21),
    ("strictatime", false, 1 << 24),
    ("nostrictatime", true, 1 << 24),
];

/// Propagation words: Docker's mount sets them on the mount afterwards;
/// a volume's own mount point is private here, so they change nothing.
const PROPAGATION: [&str; 8] = [
    "private",
    "rprivate",
    "shared",
    "rshared",
    "slave",
    "rslave",
    "unbindable",
    "runbindable",
];

/// `o` as mount(2) takes it: flags and the data string.
pub fn mount_options(o: &str) -> (u64, String) {
    let mut flags = 0;
    let mut data = Vec::new();
    for word in o.split(',').filter(|w| !w.is_empty()) {
        if let Some((_, clear, f)) = MOUNT_FLAGS.iter().find(|(w, _, _)| *w == word) {
            if *clear {
                flags &= !f;
            } else {
                flags |= f;
            }
        } else if !PROPAGATION.contains(&word) {
            data.push(word);
        }
    }
    (flags, data.join(","))
}

/// The extra fields a volume mount carries to the guest when the volume
/// has options: `\ttype\tdevice\tflags(hex)\tdata`; empty when it has none.
/// `host_path` maps a bind device to the guest's path for it; `resolve`
/// turns an NFS or CIFS server name into an address (None leaves it, and
/// the guest reports it at start, as Docker does).
pub fn guest_mount_fields(
    options: &BTreeMap<String, String>,
    host_path: impl Fn(&str) -> String,
    resolve: impl Fn(&str) -> Option<String>,
) -> String {
    let Some(kind) = options.get("type").filter(|t| !t.is_empty()) else {
        return String::new();
    };
    let device = options.get("device").cloned().unwrap_or_default();
    let (flags, mut data) = mount_options(options.get("o").map_or("", String::as_str));
    let device = if flags & 4096 != 0 {
        // A bind of a host directory. TODO(windows): a `C:\...` device from
        // the Windows CLI needs the same drive-letter translation as binds.
        host_path(&device)
    } else if kind == "cifs" {
        cifs_device(&device, &resolve)
    } else {
        device
    };
    if kind.starts_with("nfs") {
        data = data
            .split(',')
            .map(|w| match w.strip_prefix("addr=") {
                Some(a) if a.parse::<std::net::IpAddr>().is_err() => {
                    resolve(a).map_or_else(|| w.to_owned(), |ip| format!("addr={ip}"))
                }
                _ => w.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(",");
    }
    format!("\t{kind}\t{device}\t{flags:x}\t{data}")
}

/// `//server/share` with the server as an address: the kernel's cifs
/// client takes an address, not a name, without a userspace resolver.
fn cifs_device(device: &str, resolve: &impl Fn(&str) -> Option<String>) -> String {
    let Some(rest) = device.strip_prefix("//") else {
        return device.to_owned();
    };
    let (host, share) = rest.split_once('/').unwrap_or((rest, ""));
    if host.parse::<std::net::IpAddr>().is_ok() {
        return device.to_owned();
    }
    match resolve(host) {
        Some(ip) if share.is_empty() => format!("//{ip}"),
        Some(ip) => format!("//{ip}/{share}"),
        None => device.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn local_options_like_docker() {
        let o = local_options(&json!({"type": "none", "o": "bind", "device": "/Users/me/data"}))
            .unwrap();
        assert_eq!(o.len(), 3);
        assert!(local_options(&json!({})).unwrap().is_empty());
        assert!(local_options(&serde_json::Value::Null).unwrap().is_empty());
        assert_eq!(
            local_options(&json!({"type": "nfs"})).unwrap_err(),
            "missing required option: \"device\""
        );
        assert_eq!(
            local_options(&json!({"device": ":/x"})).unwrap_err(),
            "missing required option: \"type\""
        );
        assert_eq!(
            local_options(&json!({"o": "addr=1.2.3.4", "type": "nfs"})).unwrap_err(),
            "missing required option: \"device\""
        );
        assert_eq!(
            local_options(&json!({"foo": "bar"})).unwrap_err(),
            "invalid option: \"foo\""
        );
        assert_eq!(
            local_options(&json!({"size": "1g"})).unwrap_err(),
            "quota size requested but no quota support"
        );
        assert!(local_options(&json!({"type": "cifs", "device": "//nas:445/share"})).is_err());
        assert!(
            local_options(&json!({"type": "cifs", "device": "//nas/share", "o": "port=445"}))
                .is_ok()
        );
    }

    #[test]
    fn o_splits_into_flags_and_data() {
        assert_eq!(mount_options("bind"), (4096, String::new()));
        assert_eq!(mount_options("rbind,ro"), (4096 | 16384 | 1, String::new()));
        assert_eq!(
            mount_options("size=100m,uid=1000"),
            (0, "size=100m,uid=1000".into())
        );
        assert_eq!(
            mount_options("addr=10.0.0.5,rw,nfsvers=4,noexec"),
            (8, "addr=10.0.0.5,nfsvers=4".into())
        );
        assert_eq!(mount_options("ro,rw,rprivate"), (0, String::new()));
    }

    #[test]
    fn guest_fields() {
        let opts = |v: serde_json::Value| local_options(&v).unwrap();
        let mac = |p: &str| format!("/mnt/mac{p}");
        let dns = |h: &str| (h == "nas.local").then(|| "10.0.0.9".to_owned());
        assert_eq!(guest_mount_fields(&BTreeMap::new(), mac, dns), "");
        assert_eq!(
            guest_mount_fields(
                &opts(json!({"type": "none", "o": "bind", "device": "/Users/me/d"})),
                mac,
                dns
            ),
            "\tnone\t/mnt/mac/Users/me/d\t1000\t"
        );
        assert_eq!(
            guest_mount_fields(
                &opts(json!({"type": "tmpfs", "device": "tmpfs", "o": "size=100m,uid=1000"})),
                mac,
                dns
            ),
            "\ttmpfs\ttmpfs\t0\tsize=100m,uid=1000"
        );
        assert_eq!(
            guest_mount_fields(
                &opts(
                    json!({"type": "nfs", "device": ":/export", "o": "addr=nas.local,rw,nfsvers=4"})
                ),
                mac,
                dns
            ),
            "\tnfs\t:/export\t0\taddr=10.0.0.9,nfsvers=4"
        );
        // A name that does not resolve is left for the guest to report.
        assert_eq!(
            guest_mount_fields(
                &opts(json!({"type": "nfs", "device": ":/e", "o": "addr=gone"})),
                mac,
                dns
            ),
            "\tnfs\t:/e\t0\taddr=gone"
        );
        assert_eq!(
            guest_mount_fields(
                &opts(
                    json!({"type": "cifs", "device": "//nas.local/share", "o": "username=u,password=p"})
                ),
                mac,
                dns
            ),
            "\tcifs\t//10.0.0.9/share\t0\tusername=u,password=p"
        );
    }
}
