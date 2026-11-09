//! HostConfig settings the guest applies when a container starts:
//! `--ulimit`, `--sysctl`, `--device`, `--device-cgroup-rule` and
//! `--group-add`. They are kept as the API gave them (for inspect), checked
//! at create with Docker's errors, and handed to the guest at every start as
//! the `opts` lines of `Run` and `Exec` (see [`kiri_proto::RunSpec::opts`]).

use serde_json::{Value, json};

/// The HostConfig keys kept with a container.
const KEYS: [&str; 5] = [
    "Ulimits",
    "Sysctls",
    "Devices",
    "DeviceCgroupRules",
    "GroupAdd",
];

/// `nofile` when the run sets none: what containers get on Docker Desktop
/// and on Docker Engine under containerd's `LimitNOFILE=infinity`. Without
/// it a container inherits the guest init's kernel default (1024:4096), and
/// Elasticsearch, OpenSearch and Cassandra refuse to start.
pub const DEFAULT_NOFILE: u64 = 1_048_576;

/// Linux's RLIMIT_* numbers by Docker's ulimit names.
const RLIMITS: [(&str, u32); 16] = [
    ("cpu", 0),
    ("fsize", 1),
    ("data", 2),
    ("stack", 3),
    ("core", 4),
    ("rss", 5),
    ("nproc", 6),
    ("nofile", 7),
    ("memlock", 8),
    ("as", 9),
    ("locks", 10),
    ("sigpending", 11),
    ("msgqueue", 12),
    ("nice", 13),
    ("rtprio", 14),
    ("rttime", 15),
];

/// RLIM_INFINITY; Docker writes it as -1.
const UNLIMITED: u64 = u64::MAX;

/// What a container keeps of `host` (its HostConfig): the keys above, as given.
pub fn from_host(host: &Value) -> Value {
    let mut m = serde_json::Map::new();
    for k in KEYS {
        if !host[k].is_null() {
            m.insert(k.to_owned(), host[k].clone());
        }
    }
    Value::Object(m)
}

/// The value inspect shows for `key`: as given, else Docker's empty value.
pub fn inspect(opts: &Value, key: &str) -> Value {
    match (&opts[key], key) {
        (Value::Null, "Ulimits" | "Devices") => json!([]),
        (Value::Null, _) => Value::Null,
        (v, _) => v.clone(),
    }
}

/// `--gpus` and Compose `deploy.resources.reservations.devices` arrive as
/// DeviceRequests. Kiri has no GPU to give, and saying so beats a container
/// that silently runs on the CPU.
pub fn check_device_requests(host: &Value) -> Result<(), String> {
    let Some(r) = host["DeviceRequests"].as_array().and_then(|a| a.first()) else {
        return Ok(());
    };
    let caps = r["Capabilities"]
        .as_array()
        .map(|c| {
            c.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Err(format!(
        "could not select device driver {:?} with capabilities: [{caps}]: Kiri does not support GPUs or other device requests",
        r["Driver"].as_str().unwrap_or("")
    ))
}

/// The guest's lines for `opts` (from [`from_host`]), or Docker's error for
/// the first bad setting. `net_host`: the container shares the host's (the
/// VM's) network namespace, so `net.*` sysctls are not its own.
pub fn guest_lines(opts: &Value, net_host: bool) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut nofile = false;
    for u in opts["Ulimits"].as_array().into_iter().flatten() {
        let name = u["Name"].as_str().unwrap_or("");
        let res = RLIMITS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, r)| *r)
            .ok_or_else(|| format!("invalid ulimit type: {name}"))?;
        let value = |k: &str| match u[k].as_i64() {
            Some(v) if v < 0 => UNLIMITED,
            Some(v) => v as u64,
            None => u[k].as_u64().unwrap_or(0),
        };
        let (soft, hard) = (value("Soft"), value("Hard"));
        if soft > hard {
            return Err(format!(
                "ulimit soft limit must be less than or equal to hard limit: {} > {}",
                u["Soft"], u["Hard"]
            ));
        }
        nofile |= res == 7;
        out.push(format!("rlimit\t{res}\t{soft}\t{hard}"));
    }
    if !nofile {
        out.push(format!("rlimit\t7\t{DEFAULT_NOFILE}\t{DEFAULT_NOFILE}"));
    }
    if let Some(map) = opts["Sysctls"].as_object() {
        for (key, value) in map {
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            let (ns, path) = sysctl(key, net_host)?;
            if value.contains(['\t', '\n']) {
                return Err(format!("invalid value for sysctl {key:?}"));
            }
            out.push(format!("sysctl\t{ns}\t{path}\t{value}"));
        }
    }
    for d in opts["Devices"].as_array().into_iter().flatten() {
        let host = d["PathOnHost"].as_str().unwrap_or("");
        let container = d["PathInContainer"]
            .as_str()
            .filter(|p| !p.is_empty())
            .unwrap_or(host);
        let perms = d["CgroupPermissions"]
            .as_str()
            .filter(|p| !p.is_empty())
            .unwrap_or("rwm");
        for p in [host, container] {
            if !p.starts_with('/') {
                return Err(format!("{p} is not an absolute path"));
            }
            if p.contains(['\t', '\n']) || p.split('/').any(|c| c == "..") {
                return Err(format!("invalid device path {p:?}"));
            }
        }
        if !valid_perms(perms) {
            return Err(format!("invalid device mode: {perms}"));
        }
        out.push(format!("device\t{host}\t{container}\t{perms}"));
    }
    for rule in strings(&opts["DeviceCgroupRules"]) {
        let (t, major, minor, perms) =
            device_rule(&rule).ok_or_else(|| format!("invalid device cgroup format '{rule}'"))?;
        out.push(format!("devrule\t{t}\t{major}\t{minor}\t{perms}"));
    }
    for g in strings(&opts["GroupAdd"]) {
        if g.is_empty() || g.contains(['\t', '\n', ':']) {
            return Err(format!("invalid group {g:?}"));
        }
        out.push(format!("group\t{g}"));
    }
    Ok(out)
}

/// A list of strings, or one string, as the API sends either.
fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn valid_perms(p: &str) -> bool {
    !p.is_empty() && p.len() <= 3 && p.chars().all(|c| "rwm".contains(c))
}

/// `c 10:200 rwm`, `b 8:* r`, `a *:* rwm`: Docker's
/// `^([acb]) ([0-9]+|\*):([0-9]+|\*) ([rwm]{1,3})$`.
fn device_rule(rule: &str) -> Option<(&str, &str, &str, &str)> {
    let mut parts = rule.split(' ');
    let (t, numbers, perms) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !matches!(t, "a" | "b" | "c") || !valid_perms(perms) {
        return None;
    }
    let (major, minor) = numbers.split_once(':')?;
    let number = |n: &str| {
        n == "*"
            || (!n.is_empty()
                && n.len() <= 10
                && n.bytes().all(|b| b.is_ascii_digit())
                && n.parse::<u32>().is_ok())
    };
    (number(major) && number(minor)).then_some((t, major, minor, perms))
}

/// Whether the `Sysctls` in `opts` turn IPv6 off on `ifname`:
/// `net.ipv6.conf.all`, `.default` or `.<ifname>` `.disable_ipv6` set to 1
/// (dots or slashes, as Docker takes both).
pub fn ipv6_disabled(opts: &Value, ifname: &str) -> bool {
    let Some(map) = opts["Sysctls"].as_object() else {
        return false;
    };
    map.iter().any(|(key, value)| {
        let key = key.replace('/', ".");
        let on = value.as_str().map(str::trim) == Some("1") || value.as_i64() == Some(1);
        on && ["all", "default", ifname]
            .iter()
            .any(|i| key == format!("net.ipv6.conf.{i}.disable_ipv6"))
    })
}

/// Which namespace a sysctl belongs to (`net`, `ipc` or `uts`) and its path
/// under /proc/sys, or runc's error for one that is not namespaced: those
/// would change the whole VM (or, on Linux, the host).
fn sysctl(key: &str, net_host: bool) -> Result<(&'static str, String), String> {
    let name = key.replace('/', ".");
    let path = if key.contains('/') {
        key.to_owned()
    } else {
        key.replace('.', "/")
    };
    if name.is_empty()
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:@+".contains(&b))
    {
        return Err(format!("invalid sysctl {key:?}"));
    }
    const IPC: [&str; 8] = [
        "kernel.msgmax",
        "kernel.msgmnb",
        "kernel.msgmni",
        "kernel.sem",
        "kernel.shmall",
        "kernel.shmmax",
        "kernel.shmmni",
        "kernel.shm_rmid_forced",
    ];
    if IPC.contains(&name.as_str()) || name.starts_with("fs.mqueue.") {
        return Ok(("ipc", path));
    }
    if name == "kernel.domainname" {
        return Ok(("uts", path));
    }
    if name == "kernel.hostname" {
        return Err(format!(
            "sysctl {name:?} is not allowed as it conflicts with the OCI \"hostname\" field"
        ));
    }
    if name.starts_with("net.") {
        if net_host {
            return Err(format!(
                "sysctl {name:?} not allowed in host network namespace"
            ));
        }
        return Ok(("net", path));
    }
    Err(format!(
        "sysctl {name:?} is not in a separate kernel namespace"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulimits_and_the_nofile_default() {
        let lines = guest_lines(&json!({}), false).unwrap();
        assert_eq!(lines, ["rlimit\t7\t1048576\t1048576"]);
        let opts = json!({"Ulimits": [{"Name": "nofile", "Soft": 65536, "Hard": 65536}, {"Name": "memlock", "Soft": -1, "Hard": -1}]});
        assert_eq!(
            guest_lines(&opts, false).unwrap(),
            [
                "rlimit\t7\t65536\t65536",
                &format!("rlimit\t8\t{UNLIMITED}\t{UNLIMITED}")
            ]
        );
        let opts = json!({"Ulimits": [{"Name": "nproc", "Soft": 10, "Hard": 20}]});
        assert_eq!(
            guest_lines(&opts, false).unwrap(),
            ["rlimit\t6\t10\t20", "rlimit\t7\t1048576\t1048576"]
        );
        assert_eq!(
            guest_lines(
                &json!({"Ulimits": [{"Name": "files", "Soft": 1, "Hard": 1}]}),
                false
            )
            .unwrap_err(),
            "invalid ulimit type: files"
        );
        assert_eq!(
            guest_lines(
                &json!({"Ulimits": [{"Name": "nofile", "Soft": 10, "Hard": 5}]}),
                false
            )
            .unwrap_err(),
            "ulimit soft limit must be less than or equal to hard limit: 10 > 5"
        );
        // -1 soft under a finite hard is above it.
        assert!(
            guest_lines(
                &json!({"Ulimits": [{"Name": "core", "Soft": -1, "Hard": 5}]}),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn ipv6_off_by_sysctl() {
        let on = |k: &str, v: serde_json::Value, i: &str| {
            super::ipv6_disabled(&json!({"Sysctls": {k: v}}), i)
        };
        assert!(on("net.ipv6.conf.all.disable_ipv6", json!("1"), "eth0"));
        assert!(on("net.ipv6.conf.default.disable_ipv6", json!("1"), "eth0"));
        assert!(on("net/ipv6/conf/eth1/disable_ipv6", json!("1"), "eth1"));
        assert!(!on("net.ipv6.conf.eth1.disable_ipv6", json!("1"), "eth0"));
        assert!(!on("net.ipv6.conf.all.disable_ipv6", json!("0"), "eth0"));
        assert!(!super::ipv6_disabled(&json!({}), "eth0"));
    }

    #[test]
    fn only_namespaced_sysctls() {
        let ok = |k: &str| guest_lines(&json!({"Sysctls": {k: "1"}}), false).map(|l| l[1].clone());
        assert_eq!(
            ok("net.core.somaxconn").unwrap(),
            "sysctl\tnet\tnet/core/somaxconn\t1"
        );
        assert_eq!(
            ok("net.ipv4.ip_unprivileged_port_start").unwrap(),
            "sysctl\tnet\tnet/ipv4/ip_unprivileged_port_start\t1"
        );
        assert_eq!(
            ok("kernel.shmmax").unwrap(),
            "sysctl\tipc\tkernel/shmmax\t1"
        );
        assert_eq!(ok("kernel.sem").unwrap(), "sysctl\tipc\tkernel/sem\t1");
        assert_eq!(
            ok("fs.mqueue.msg_max").unwrap(),
            "sysctl\tipc\tfs/mqueue/msg_max\t1"
        );
        assert_eq!(
            ok("kernel.domainname").unwrap(),
            "sysctl\tuts\tkernel/domainname\t1"
        );
        // An interface name with a dot, given with slashes as runc allows.
        assert_eq!(
            ok("net/ipv4/conf/eth0.100/forwarding").unwrap(),
            "sysctl\tnet\tnet/ipv4/conf/eth0.100/forwarding\t1"
        );
        assert_eq!(
            ok("kernel.core_pattern").unwrap_err(),
            "sysctl \"kernel.core_pattern\" is not in a separate kernel namespace"
        );
        assert_eq!(
            ok("vm.overcommit_memory").unwrap_err(),
            "sysctl \"vm.overcommit_memory\" is not in a separate kernel namespace"
        );
        assert!(ok("kernel.hostname").is_err());
        assert!(ok("net/../kernel/core_pattern").is_err());
        assert!(ok("net..core").is_err());
        assert_eq!(
            guest_lines(&json!({"Sysctls": {"net.core.somaxconn": "1024"}}), true).unwrap_err(),
            "sysctl \"net.core.somaxconn\" not allowed in host network namespace"
        );
        // kernel.sem's value has spaces; numbers given as numbers still work.
        assert_eq!(
            guest_lines(
                &json!({"Sysctls": {"kernel.sem": "250 32000 100 128"}}),
                false
            )
            .unwrap()[1],
            "sysctl\tipc\tkernel/sem\t250 32000 100 128"
        );
        assert_eq!(
            guest_lines(&json!({"Sysctls": {"net.core.somaxconn": 1024}}), false).unwrap()[1],
            "sysctl\tnet\tnet/core/somaxconn\t1024"
        );
    }

    #[test]
    fn devices_rules_and_groups() {
        let opts = json!({
            "Devices": [
                {"PathOnHost": "/dev/fuse", "PathInContainer": "/dev/fuse", "CgroupPermissions": "rwm"},
                {"PathOnHost": "/dev/net/tun", "PathInContainer": "", "CgroupPermissions": ""},
                {"PathOnHost": "/dev/fuse", "PathInContainer": "/dev/myfuse", "CgroupPermissions": "r"},
            ],
            "DeviceCgroupRules": ["c 10:229 rwm", "b 8:* r", "a *:* m"],
            "GroupAdd": ["video", "1234"],
        });
        assert_eq!(
            guest_lines(&opts, false).unwrap()[1..],
            [
                "device\t/dev/fuse\t/dev/fuse\trwm",
                "device\t/dev/net/tun\t/dev/net/tun\trwm",
                "device\t/dev/fuse\t/dev/myfuse\tr",
                "devrule\tc\t10\t229\trwm",
                "devrule\tb\t8\t*\tr",
                "devrule\ta\t*\t*\tm",
                "group\tvideo",
                "group\t1234",
            ]
        );
        let bad = |o: Value| guest_lines(&o, false).unwrap_err();
        assert_eq!(
            bad(json!({"Devices": [{"PathOnHost": "/dev/fuse", "CgroupPermissions": "rwx"}]})),
            "invalid device mode: rwx"
        );
        assert_eq!(
            bad(json!({"Devices": [{"PathOnHost": "dev/fuse"}]})),
            "dev/fuse is not an absolute path"
        );
        assert_eq!(
            bad(
                json!({"Devices": [{"PathOnHost": "/dev/fuse", "PathInContainer": "/dev/../etc/x"}]})
            ),
            "invalid device path \"/dev/../etc/x\""
        );
        for rule in [
            "c 10:229",
            "x 1:2 r",
            "c 1:2 rwmr",
            "c a:2 r",
            "c 1:2 q",
            "c  1:2 r",
            "c 1:2 r extra",
        ] {
            assert_eq!(
                bad(json!({"DeviceCgroupRules": [rule]})),
                format!("invalid device cgroup format '{rule}'")
            );
        }
    }

    #[test]
    fn device_requests_are_refused() {
        assert!(check_device_requests(&json!({})).is_ok());
        assert!(check_device_requests(&json!({"DeviceRequests": []})).is_ok());
        let e = check_device_requests(
            &json!({"DeviceRequests": [{"Driver": "", "Count": -1, "Capabilities": [["gpu"]]}]}),
        )
        .unwrap_err();
        assert!(
            e.starts_with("could not select device driver \"\" with capabilities: [[\"gpu\"]]"),
            "{e}"
        );
    }

    #[test]
    fn kept_for_inspect() {
        let host = json!({"Ulimits": [{"Name": "nofile", "Soft": 1, "Hard": 2}], "Memory": 5});
        let kept = from_host(&host);
        assert_eq!(
            kept,
            json!({"Ulimits": [{"Name": "nofile", "Soft": 1, "Hard": 2}]})
        );
        assert_eq!(inspect(&kept, "Devices"), json!([]));
        assert_eq!(inspect(&kept, "GroupAdd"), Value::Null);
        assert_eq!(inspect(&Value::Null, "Ulimits"), json!([]));
    }
}
