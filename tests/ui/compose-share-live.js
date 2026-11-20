// parity: macos windows linux
// window: 1440x900
// A share that goes live shows at once on Compose's list and a project's Services tab, as on Containers:
// the share's own event redraws them, with no list refresh from the engine.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const now = Math.floor(Date.now() / 1000);
const svc = (name, ports) => ({ name, image: "nginx:latest", build: "", ports, volumes: [], depends: [], networks: ["default"], kinds: [] });
const m = (svcName, n, state) => ({ service: svcName, id: `c-${svcName}-${n}`, name: `shop-${svcName}-${n}`, image: "nginx", state, status: "Up" });
const P = [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: now - 60,
  services: [svc("web", ["18080:80"]), svc("api", ["19101:9101", "19102:9102"]), svc("worker", []), svc("ghost", []), svc("busy", ["18081:80"]), svc("bad", ["18082:80"])],
  containers: [m("web", 1, "exited"), m("web", 2, "running"), m("api", 1, "running"), m("worker", 1, "exited"), m("busy", 1, "running"), m("bad", 1, "running")] },
  { name: "quiet", dir: "/w/quiet", files: ["/w/quiet/compose.yaml"], readable: true, updated: now - 60, services: [svc("app", [])], containers: [m("app", 1, "exited")] }];
const port = (p) => ({ IP: "0.0.0.0", PrivatePort: +p.split(":")[1], PublicPort: +p.split(":")[0], Type: "tcp" });
const lists = () => kiri.on({ type: "lists", images: [], volumes: [], networks: [], containers: P.flatMap((p) => p.containers.map((c) => ({ Id: c.id, Names: ["/" + c.name], Image: c.image, State: c.state, Status: c.state === "running" ? "Up 1 hour" : "Exited (0)", Created: 1,
  Ports: c.state === "running" ? (p.services.find((s) => s.name === c.service).ports || []).map(port) : [], Labels: { "com.docker.compose.project": p.name, "com.docker.compose.service": c.service } }))) });
const on = (share) => kiri.on({ type: "status", version: "0.1.4", dev: share, share, app: "Kiri", ours: "kiri", context: "kiri" });
ui.vm = "running"; on(true); lists();
kiri.on({ type: "composeprojects", logos: {}, projects: P });
sh.account = { signedIn: true };
show("compose");
// shop-web-2's row: Containers' row, the same on the list and the Services tab.
const webCell = (tbl) => document.querySelector(`${tbl} tbody tr[data-key="c-web-2"] td.col-share`);
t("the list: a link going live shows at once", () => {
  eq(!!webCell("table[data-view=compose]")?.querySelector(".shc.live"), false, "not shared yet");
  kiri.on({ type: "share", id: 61, source: "shop-web-2", state: "connecting" });
  kiri.on({ type: "share", id: 61, source: "shop-web-2", state: "live", url: "https://web-y.runkiri.cloud" });
  eq(!!webCell("table[data-view=compose]")?.querySelector(".shc.live"), true, "live, without a list refresh");
  kiri.on({ type: "share", id: 61, source: "shop-web-2", state: "ended" });
  eq(!!webCell("table[data-view=compose]")?.querySelector(".shc.live"), false, "its end shows too");
});
t("a project's Services tab: the same", () => {
  cmpOpen("shop"); click(document.querySelector('[data-cmptab="services"]'));
  kiri.on({ type: "share", id: 62, source: "shop-web-2", state: "live", url: "https://web-z.runkiri.cloud" });
  eq(!!webCell("table[data-view=cmpsvcs]")?.querySelector(".shc.live"), true, "live, without a list refresh");
  cmp.open = null; render();
});
report(R);
