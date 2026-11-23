//! The systemd units that run the engine on a Linux host (the one in
//! contrib/ and the one the packages install) give the daemon, and so every
//! container under it, the limits Docker's units give: no process or
//! core-size limit, no task limit on the unit whose cgroup holds every
//! container, and the cgroups delegated.

use std::path::Path;

fn service(rel: &str) -> Vec<(String, String)> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let mut in_service = false;
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_service = line == "[Service]";
        } else if in_service
            && !line.starts_with('#')
            && let Some((k, v)) = line.split_once('=')
        {
            out.push((k.to_owned(), v.to_owned()));
        }
    }
    out
}

#[test]
fn the_units_give_containers_dockers_limits() {
    for unit in ["contrib/linux/kiri.service", "packaging/linux/kiri.service"] {
        let s = service(unit);
        let get = |k: &str| s.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        for (k, want) in [("LimitNPROC", "infinity"), ("LimitCORE", "infinity"), ("TasksMax", "infinity"), ("Delegate", "yes")] {
            assert_eq!(get(k), Some(want), "{unit}: {k}");
        }
        // Containers inherit the daemon's; Docker gives a container 0.
        assert_eq!(get("OOMScoreAdjust"), None, "{unit}: OOMScoreAdjust");
    }
}
