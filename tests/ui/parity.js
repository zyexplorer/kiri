// parity: macos windows linux
// What the window offers, listed as ITEM lines. ui-test.py runs this page once
// per OS (window.KIRI_OS) and fails when an item is on one and not another,
// unless tests/ui/parity-allow.txt says why.
ui.vm = "running";
ui.containers.push({ Id: "k8s1", Names: ["/kiri-k8s"], Image: "rancher/k3s:v1.37.1-k3s1", State: "running", Ports: [], Labels: { "dev.kiri.k8s": "1" }, KiriStartedAt: Date.now() / 1000 - 600 });
kiri.on({ type: "k8s", state: "running", ready: true, context: "kiri-dev",
  services: [{ ns: "default", name: "web", type: "ClusterIP", clusterIP: "10.43.42.10", ports: [{ port: 80, targetPort: 80, protocol: "TCP" }], selector: { app: "web" }, ingress: [], created: "2026-10-28T00:00:00Z" }],
  pods: [{ ns: "default", name: "web-5fc9", ip: "10.42.0.17", labels: { app: "web" }, owner: "ReplicaSet", status: "Running", phase: "Running", ready: "1/1", allReady: true, restarts: 0, created: "2026-10-28T00:00:00Z",
    containers: [{ name: "c", image: "nginx", ready: true, restarts: 0, state: "running", ports: [{ containerPort: 80, protocol: "TCP" }] }] }] });
const items = [];
const seen = (el) => !!el && !el.closest("[hidden]") && getComputedStyle(el).display !== "none";
for (const b of document.querySelectorAll("nav [data-view]")) if (seen(b)) items.push(`nav ${b.dataset.view}`);
if (seen(document.getElementById("k8s-label"))) items.push("nav Kubernetes heading");
for (const v of ["containers", "pods", "services"]) {
  show(v);
  for (const th of document.querySelectorAll("#content thead th")) { const h = th.textContent.trim(); if (h) items.push(`column ${v}: ${h}`); }
}
// An image's page and its tabs (Layers, Security).
ui.images.push({ Id: "sha256:" + "ab".repeat(32), RepoTags: ["nginx:alpine"], Size: 5e7, Created: Date.now() / 1000 - 86400 });
show("images"); openImage("nginx:alpine");
for (const b of document.querySelectorAll("[data-imgtab]")) if (seen(b)) items.push(`image tab ${b.textContent.trim()}`);
// USB: the same columns everywhere (what the rows offer is the system's).
kiri.on({ type: "usb", can: "attach", devices: [{ key: "k", name: "Zigbee", id: "10c4:ea60", speed: "full", state: "host", path: "" }] });
show("usb");
for (const th of document.querySelectorAll("#content thead th")) { const h = th.textContent.trim(); if (h) items.push(`column usb: ${h}`); }
show("settings");
for (const b of document.querySelectorAll("[data-stab]")) items.push(`settings ${b.dataset.stab}`);
report(items.map((x) => "ITEM " + x));
