//! What the page shows of `kubectl get pods,services -A -o json`, and the
//! names it may send back. Shared by the Mac window (ui/k8s.rs) and the
//! Windows one (crates/kiri-win/src/k8s.rs), which include it as a child
//! module; each parent provides `brand` (an image's logo) and `brands`.

use serde_json::{Value, json};

use super::{brand, brands};

/// A Kubernetes name (namespace, pod, container): DNS labels and subdomains.
pub(super) fn valid(name: &str) -> bool {
    name.len() <= 253
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// What the page shows of `kubectl get pods,services -A -o json`.
pub(super) fn summarize(list: &Value, starting: bool) -> Value {
    let items = list["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let pods: Vec<Value> = items
        .iter()
        .filter(|i| i["kind"] == "Pod")
        .map(pod)
        .collect();
    let services: Vec<Value> = items
        .iter()
        .filter(|i| i["kind"] == "Service")
        .map(service)
        .collect();
    let logo = brands::BRANDS
        .binary_search_by_key(&"kubernetes", |(s, _, _)| s)
        .ok()
        .map(|i| [brands::BRANDS[i].1, brands::BRANDS[i].2]);
    json!({"type": "k8s", "state": "running", "ready": true, "starting": starting, "icon": logo, "pods": pods, "services": services})
}

/// One pod, with the status `kubectl get pods` would print for it.
fn pod(p: &Value) -> Value {
    let (meta, spec, st) = (&p["metadata"], &p["spec"], &p["status"]);
    let statuses = st["containerStatuses"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let containers: Vec<Value> = spec["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let name = c["name"].as_str().unwrap_or_default();
            let s = statuses.iter().find(|s| s["name"] == name).cloned().unwrap_or(Value::Null);
            let (state, reason, since) = match s["state"].as_object().and_then(|o| o.iter().next()) {
                Some((k, v)) => (k.as_str(), v["reason"].as_str().unwrap_or_default(), v["startedAt"].as_str().or(v["finishedAt"].as_str()).unwrap_or_default()),
                None => ("waiting", "", ""),
            };
            json!({
                "name": name, "image": c["image"], "ready": s["ready"].as_bool().unwrap_or(false),
                "restarts": s["restartCount"].as_u64().unwrap_or(0), "state": state, "reason": reason,
                "since": since, "exit": s["state"]["terminated"]["exitCode"], "message": s["state"]["waiting"]["message"],
                "ports": c["ports"],
            })
        })
        .collect();
    let ready = statuses.iter().filter(|s| s["ready"] == true).count();
    let restarts: u64 = statuses
        .iter()
        .filter_map(|s| s["restartCount"].as_u64())
        .sum();
    let image = spec["containers"][0]["image"].as_str().unwrap_or_default();
    json!({
        "ns": meta["namespace"], "name": meta["name"], "uid": meta["uid"],
        "status": status_of(p), "phase": st["phase"],
        "ready": format!("{ready}/{}", containers.len()), "allReady": ready == containers.len() && !containers.is_empty(),
        "restarts": restarts, "created": meta["creationTimestamp"], "ip": st["podIP"], "node": spec["nodeName"],
        "owner": meta["ownerReferences"][0]["kind"], "labels": meta["labels"], "containers": containers,
        "icon": brand(image),
    })
}

/// The STATUS column of `kubectl get pods`: the phase, or what holds it up
/// (an init container, a waiting or failed container, a deletion).
fn status_of(p: &Value) -> String {
    let st = &p["status"];
    let mut reason = st["reason"]
        .as_str()
        .or(st["phase"].as_str())
        .unwrap_or("Unknown")
        .to_owned();
    let inits = st["initContainerStatuses"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let total = p["spec"]["initContainers"].as_array().map_or(0, Vec::len);
    for (i, c) in inits.iter().enumerate() {
        let s = &c["state"];
        if s["terminated"]["exitCode"] == 0 && s["terminated"].is_object() {
            continue;
        }
        reason = if let Some(r) = s["terminated"]["reason"].as_str() {
            format!("Init:{r}")
        } else if s["terminated"].is_object() {
            format!("Init:ExitCode:{}", s["terminated"]["exitCode"])
        } else if let Some(r) = s["waiting"]["reason"]
            .as_str()
            .filter(|r| *r != "PodInitializing")
        {
            format!("Init:{r}")
        } else {
            format!("Init:{i}/{total}")
        };
        return deleting(p, reason);
    }
    let mut running = false;
    for c in st["containerStatuses"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
    {
        let s = &c["state"];
        if let Some(r) = s["waiting"]["reason"].as_str() {
            reason = r.to_owned();
        } else if let Some(r) = s["terminated"]["reason"].as_str() {
            reason = r.to_owned();
        } else if s["terminated"].is_object() {
            reason = format!("ExitCode:{}", s["terminated"]["exitCode"]);
        } else if s["running"].is_object() && c["ready"] == true {
            running = true;
        }
    }
    if reason == "Completed" && running {
        reason = "Running".into();
    }
    deleting(p, reason)
}

fn deleting(p: &Value, reason: String) -> String {
    if p["metadata"]["deletionTimestamp"].is_string() {
        "Terminating".into()
    } else {
        reason
    }
}

fn service(s: &Value) -> Value {
    let (meta, spec) = (&s["metadata"], &s["spec"]);
    let ingress: Vec<&str> = s["status"]["loadBalancer"]["ingress"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["ip"].as_str().or(i["hostname"].as_str()))
        .collect();
    json!({
        "ns": meta["namespace"], "name": meta["name"], "type": spec["type"], "clusterIP": spec["clusterIP"],
        "ports": spec["ports"], "ingress": ingress, "selector": spec["selector"], "created": meta["creationTimestamp"],
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn pod_status_reads_as_kubectl_prints_it() {
        let running = json!({"metadata": {}, "spec": {"containers": [{"name": "a"}]}, "status": {"phase": "Running",
            "containerStatuses": [{"name": "a", "ready": true, "restartCount": 0, "state": {"running": {"startedAt": "2026-01-01T00:00:00Z"}}}]}});
        assert_eq!(super::status_of(&running), "Running");
        let crash = json!({"metadata": {}, "spec": {"containers": [{"name": "a"}]}, "status": {"phase": "Running",
            "containerStatuses": [{"name": "a", "ready": false, "restartCount": 4, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}});
        assert_eq!(super::status_of(&crash), "CrashLoopBackOff");
        let creating = json!({"metadata": {}, "spec": {"containers": [{"name": "a"}]}, "status": {"phase": "Pending",
            "containerStatuses": [{"name": "a", "ready": false, "state": {"waiting": {"reason": "ContainerCreating"}}}]}});
        assert_eq!(super::status_of(&creating), "ContainerCreating");
        let done = json!({"metadata": {}, "spec": {"containers": [{"name": "a"}]}, "status": {"phase": "Succeeded",
            "containerStatuses": [{"name": "a", "ready": false, "state": {"terminated": {"reason": "Completed", "exitCode": 0}}}]}});
        assert_eq!(super::status_of(&done), "Completed");
        let init = json!({"metadata": {}, "spec": {"initContainers": [{"name": "i"}, {"name": "j"}], "containers": [{"name": "a"}]}, "status": {"phase": "Pending",
            "initContainerStatuses": [{"name": "i", "state": {"terminated": {"exitCode": 0, "reason": "Completed"}}}, {"name": "j", "state": {"running": {}}}]}});
        assert_eq!(super::status_of(&init), "Init:1/2");
        let going = json!({"metadata": {"deletionTimestamp": "2026-01-01T00:00:00Z"}, "spec": {"containers": [{"name": "a"}]}, "status": {"phase": "Running"}});
        assert_eq!(super::status_of(&going), "Terminating");
    }

    #[test]
    fn summarizes_pods_and_services() {
        let list = json!({"items": [
            {"kind": "Pod", "metadata": {"namespace": "default", "name": "web-1", "labels": {"app": "web"}, "ownerReferences": [{"kind": "ReplicaSet"}]},
             "spec": {"nodeName": "kiri", "containers": [{"name": "nginx", "image": "nginx"}, {"name": "side", "image": "busybox"}]},
             "status": {"phase": "Running", "podIP": "10.42.0.9", "containerStatuses": [
                {"name": "nginx", "ready": true, "restartCount": 1, "state": {"running": {"startedAt": "t"}}},
                {"name": "side", "ready": false, "restartCount": 2, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}},
            {"kind": "Service", "metadata": {"namespace": "default", "name": "web"}, "spec": {"type": "LoadBalancer", "clusterIP": "10.43.0.5",
             "ports": [{"port": 8080, "targetPort": 80, "protocol": "TCP"}]}, "status": {"loadBalancer": {"ingress": [{"ip": "172.17.0.2"}]}}},
        ]});
        let v = super::summarize(&list, false);
        let p = &v["pods"][0];
        assert_eq!(
            (
                p["ready"].as_str(),
                p["restarts"].as_u64(),
                p["status"].as_str()
            ),
            (Some("1/2"), Some(3), Some("CrashLoopBackOff"))
        );
        assert_eq!(
            (p["owner"].as_str(), p["containers"][1]["reason"].as_str()),
            (Some("ReplicaSet"), Some("CrashLoopBackOff"))
        );
        assert!(p["icon"].is_array(), "nginx has a logo");
        let s = &v["services"][0];
        assert_eq!(
            (
                s["type"].as_str(),
                s["ports"][0]["port"].as_u64(),
                s["ingress"][0].as_str()
            ),
            (Some("LoadBalancer"), Some(8080), Some("172.17.0.2"))
        );
        assert!(v["icon"].is_array(), "the Kubernetes logo");
    }

    #[test]
    fn only_kubernetes_names_reach_kubectl() {
        assert!(
            super::valid("web-5fc9f4bf66-cdl9f")
                && super::valid("kube-system")
                && super::valid("a.b")
        );
        assert!(
            !super::valid("")
                && !super::valid("--all")
                && !super::valid("Web")
                && !super::valid("a b")
                && !super::valid("a/b")
        );
    }
}
