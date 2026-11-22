// parity: macos windows linux
// window: 1440x900
// Anything you can click shows the hand: every button, link, tab, menu item and box, every row that opens
// something, and everything the page's click handlers answer (their [data-…] selectors, read from the page's own
// script), in every section and every page and tab of a container, an image and a Compose project, the Files
// browser's folders, files and path included. Disabled controls, and a busy (rolling) one, are exempt.
const R = [], MISSES = new Map();
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
// What the click handlers answer: t("[data-x]") and closest("[data-x]") in the page's script.
const SRC = [...document.scripts].map((s) => s.textContent).join("\n");
const ATTRS = [...new Set([...SRC.matchAll(/(?:\bt|closest\??\.?)\(\s*"\[(data-[a-z0-9-]+)\]"\s*\)/g)].map((m) => m[1]))];
const CLICKABLE = ["button", "a[href]", "[role=button]", "[role=link]", "[role=tab]", "[role=menuitem]", "[role=menuitemradio]", "[role=checkbox]", "tr[data-rowopen]", ...ATTRS.map((a) => `[${a}]`)].join(", ");
// A list row's data-pick is its key for the boxes, not a click target (the welcome's choices are).
const exempt = (el) => (el.matches("#content tr[data-pick]") && !el.matches(CLICKABLE.replace(", [data-pick]", ""))) || el.disabled || el.closest("[aria-disabled=true], fieldset:disabled, [inert], .xterm") || getComputedStyle(el).pointerEvents === "none";
const onScreen = (el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== "hidden"; };
const label = (el) => `${el.tagName.toLowerCase()}${el.id ? "#" + el.id : ""}${[...el.classList].slice(0, 3).map((c) => "." + c).join("")}${[...el.attributes].filter((a) => a.name.startsWith("data-") || a.name === "role").slice(0, 2).map((a) => `[${a.name}]`).join("")}`;
let checked = 0;
function sweep(where, root = document) {
  for (const el of root.querySelectorAll(CLICKABLE)) {
    if (!shown(el) || exempt(el) || el.closest("#results")) continue;
    checked++;
    const cur = getComputedStyle(el).cursor;
    if (cur !== "pointer") { const k = label(el); if (!MISSES.has(k)) MISSES.set(k, `${where}: ${k} "${(el.getAttribute("aria-label") || el.textContent).trim().slice(0, 30)}" has cursor ${cur}`); }
  }
}
const click = (el) => el && el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const now = Math.floor(Date.now() / 1000);
ui.vm = "running";
kiri.on({ type: "status", version: "0.1.4", dev: true, share: true, app: "Kiri", ours: "kiri", context: "kiri" });
const C = (id, project, service, state) => ({ Id: id, Names: [`/${project}-${service}-1`], Image: `${service}:dev`, State: state, Status: state === "running" ? "Up 1 hour" : "Exited (0)",
  Created: now - 600, Ports: [{ IP: "0.0.0.0", PrivatePort: 80, PublicPort: 8081, Type: "tcp" }], Labels: { "com.docker.compose.project": project, "com.docker.compose.service": service } });
kiri.on({ type: "lists", containers: [...ui.containers, C("b1", "blog", "app", "running"), C("b2", "blog", "worker", "exited")],
  images: [{ Id: "sha256:1", RepoTags: ["nginx:alpine"], Size: 5e7, Created: now - 9000 }, { Id: "sha256:2", RepoTags: ["<none>:<none>"], Size: 1e7, Created: now - 9000 }],
  volumes: [{ Name: "pgdata", Driver: "local", CreatedAt: "2026-11-01T00:00:00Z" }], networks: [{ Id: "n1", Name: "blog_default", Driver: "bridge", Created: "2026-11-01T00:00:00Z", IPAM: { Config: [{ Subnet: "172.20.0.0/16" }] } }] });
const svc = (name) => ({ name, image: name + ":dev", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "blog", dir: "/w/blog", files: ["/w/blog/compose.yaml"], readable: true, services: [svc("app"), svc("worker"), svc("cache")],
  containers: [{ service: "app", id: "b1", name: "blog-app-1", image: "app:dev", state: "running", status: "Up" }, { service: "worker", id: "b2", name: "blog-worker-1", image: "worker:dev", state: "exited", status: "Exited (0)" }] }] });
kiri.on({ type: "k8s", state: "running", ready: true, context: "kiri",
  services: [{ ns: "default", name: "web", type: "LoadBalancer", clusterIP: "10.43.42.10", ports: [{ port: 18080, targetPort: 80, protocol: "TCP" }], selector: { app: "web" }, ingress: [], created: "2026-10-28T00:00:00Z" }],
  pods: [{ ns: "default", name: "web-1", ip: "10.42.0.9", labels: { app: "web" }, status: "Running", phase: "Running", ready: "1/1", allReady: true, restarts: 0, created: "2026-10-28T00:00:00Z",
    containers: [{ name: "c", image: "nginx", ready: true, restarts: 0, state: "running", ports: [{ containerPort: 80, protocol: "TCP" }] }] }] });

for (const v of ["containers", "images", "volumes", "networks", "builds", "pods", "services", "usb", "share", "compose"]) { show(v); sweep(v); }
// A container's page and its tabs; the Files browser with a folder, a file, the path and a preview.
show("containers"); openDetail("b1");
for (const tab of ["overview", "logs", "terminal", "files"]) {
  setTab(tab);
  if (tab === "files") {
    kiri.on({ type: "files", name: ui.detail.name, path: "/etc", entries: [{ name: "nginx", dir: true, mtime: now - 60 }, { name: "hosts", dir: false, size: 120, mtime: now - 60 }] });
    t("the Files browser: a folder's row, a file's row and the path's parts show the hand, across the whole row", () => {
      const rows = [...document.querySelectorAll("table.files tbody tr")];
      const cur = (el) => getComputedStyle(el).cursor;
      const want = rows.flatMap((r) => [r, ...r.cells, r.querySelector(".fname span")]).map(cur);
      if (want.some((c) => c !== "pointer")) throw new Error("rows: " + want.join(" "));
      if ([...document.querySelectorAll(".crumbs button")].some((b) => cur(b) !== "pointer")) throw new Error("path");
    });
    sweep("container files");
    click(document.querySelector("table.files tr[data-file]"));
    kiri.on({ type: "preview", name: ui.detail.name, path: "/etc/hosts", text: "127.0.0.1 localhost" });
  }
  sweep("container " + tab);
}
closeDetail?.();
// An image's page.
show("images"); try { openImage("nginx:alpine"); sweep("image page"); } catch (e) { R.push("FAIL image page: " + e.message); }
// Settings, every tab.
show("settings");
for (const b of [...document.querySelectorAll("[data-stab]")].map((b) => b.dataset.stab)) { click(document.querySelector(`[data-stab="${b}"]`)); sweep("settings " + b); }
// A Compose project's page, every tab.
show("compose"); cmpOpen("blog");
kiri.on({ type: "composefile", name: "blog", path: "/w/blog/compose.yaml", text: "services:\n  app:\n    image: app:dev\n", hash: "h1" });
for (const tab of ["structure", "services", "file", "logs"]) { click(document.querySelector(`[data-cmptab="${tab}"]`)); sweep("compose " + tab); }
cmp.open = null;
// The menus: a container's ⋯ and a project's ⋯.
show("containers"); click(document.querySelector('#content [data-more]')); sweep("container menu"); closeMenu();
click(document.querySelector('#content [data-gmore]')); sweep("project menu"); closeMenu();
t(`every clickable thing shows the hand (${ATTRS.length} handled attributes)`, () => { if (checked < 200) throw new Error("only " + checked + " checked"); if (MISSES.size) throw new Error([...MISSES.values()].join("\n  ")); });
report(R);
