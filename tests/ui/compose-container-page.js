// parity: macos windows linux
// window: 1440x900
// A Compose container's page opens inside Compose, from the list, the project's Services tab or anywhere else: the
// sidebar stays on Compose, the breadcrumb reads Compose › project › container (Compose and the project are links),
// Back returns to where it was opened from, and there's no Remove (the file says what runs): its ⋯ has Show in
// compose file. A loose container's page is as before: Containers › name, with Remove.
const R = [], ITEMS = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const now = Math.floor(Date.now() / 1000);
const C = (id, name, state, project, service) => ({ Id: id, Names: ["/" + name], Image: "nginx:alpine", State: state, Status: state === "running" ? "Up 1 hour" : "Exited (0)", Created: now - 600, Ports: [],
  Labels: project ? { "com.docker.compose.project": project, "com.docker.compose.service": service } : {} });
const ctrs = [C("solo", "solo", "running"), C("w1", "shop-web-1", "running", "shop", "web"), C("d1", "shop-db-1", "exited", "shop", "db")];
const svc = (name) => ({ name, image: "nginx:alpine", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
ui.vm = "running";
kiri.on({ type: "status", version: "0.1.4", dev: true, share: true, app: "Kiri", ours: "kiri", context: "kiri" });
kiri.on({ type: "lists", containers: ctrs, images: [], volumes: [], networks: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, services: [svc("web"), svc("db")],
  containers: ctrs.filter((c) => c.Labels["com.docker.compose.project"]).map((c) => ({ service: c.Labels["com.docker.compose.service"], id: c.Id, name: c.Names[0].slice(1), image: c.Image, state: c.State, status: c.Status })) }] });
const click = (el, o = {}) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, ...o }));
const $c = (q) => document.querySelector("#content " + q);
const side = () => document.querySelector('nav button[aria-current="page"]')?.dataset.view;
const crumb = () => [...document.querySelectorAll("#title .crumb > :not([aria-hidden])")].map((e) => `${e.tagName === "BUTTON" ? "link " : ""}${e.textContent}`);
const acts = () => [...document.querySelectorAll("#content .dacts > *")].map((b) => b.getAttribute("aria-label") || b.textContent.trim());
const menu = () => [...document.querySelectorAll(".menu button")].map((b) => b.textContent + (b.classList.contains("danger") ? " (danger)" : ""));
const page = (id) => [ui.view, side(), ui.detail?.id, !!$c(".dtabs")];
const back = () => click(document.getElementById("nav-back"));
show("compose");
t("from the Compose list: its page opens in Compose, the sidebar on Compose", () => {
  click($c('tr[data-key="w1"] [data-detail]'));
  eq(page(), ["compose", "compose", "w1", true], "the page");
  eq(crumb(), ["link Compose", "link shop", "shop-web-1"], "the breadcrumb");
  eq([...document.querySelectorAll("#title .crumb button")].map((b) => getComputedStyle(b).cursor), ["pointer", "pointer"], "its links");
});
t("no Remove on the page; its ⋯ has Show in compose file, and no Remove", () => {
  eq([document.querySelectorAll('#content [data-c="remove"]').length, acts().includes("Remove")], [0, false], "no Remove");
  click($c(".dacts [data-more]"));
  eq(menu(), ["Ports…", "Pause", "Show in compose file"], "⋯");
  click([...document.querySelectorAll(".menu button")].find((b) => b.textContent === "Show in compose file"));
  eq([ui.view, ui.detail, cmp.open, cmp.tab], ["compose", null, "shop", "file"], "the project's file");
  cmp.open = null; render();
});
t("Back returns to the Compose list", () => {
  click($c('tr[data-key="w1"] [data-detail]'));
  eq(ui.detail?.id, "w1", "open");
  back();
  eq([ui.view, side(), ui.detail, cmp.open, !!$c('tr.group[data-group="shop"]')], ["compose", "compose", null, null, true], "the list");
});
t("each part of the breadcrumb goes there: the project's page, and Compose's list", () => {
  click($c('tr[data-key="d1"]').cells[1]);
  eq(page(), ["compose", "compose", "d1", true], "a row's click opens it too");
  click(document.querySelector('#title [data-cdproj="shop"]'));
  eq([ui.view, ui.detail, cmp.open, !!document.getElementById("cmp-tabs")], ["compose", null, "shop", true], "the project");
  click($c('[data-cmptab="services"]'));
  click($c('tr[data-key="w1"] [data-detail]'));
  click(document.querySelector("#title [data-cmpback]"));
  eq([ui.view, ui.detail, cmp.open, !!$c('tr.group[data-group="shop"]')], ["compose", null, null, true], "Compose");
});
t("from the project's Services tab: in Compose, the same breadcrumb; Back returns to the Services tab", () => {
  cmpOpen("shop"); click($c('[data-cmptab="services"]'));
  click($c('table[data-view=cmpsvcs] tr[data-key="w1"] [data-detail]'));
  eq(page(), ["compose", "compose", "w1", true], "the page");
  eq(crumb(), ["link Compose", "link shop", "shop-web-1"], "the breadcrumb");
  eq(document.querySelectorAll('#content [data-c="remove"]').length, 0, "no Remove");
  back();
  eq([ui.view, ui.detail, cmp.open, cmp.tab, !!$c("table[data-view=cmpsvcs]")], ["compose", null, "shop", "services", true], "the Services tab");
});
t("from elsewhere (an image's used by, a toast, Share): Compose, not Containers", () => {
  show("images");
  openDetail("d1");
  eq([...page(), cmp.open], ["compose", "compose", "d1", true, null], "openDetail");
  eq(crumb(), ["link Compose", "link shop", "shop-db-1"], "the breadcrumb");
  back();
  eq([ui.view, ui.detail], ["images", null], "Back to Images");
  viewContainer("w1", "logs");
  eq([...page(), ui.detail?.tab], ["compose", "compose", "w1", true, "logs"], "viewContainer");
  show("containers");
});
t("a loose container's page is as before: Containers › name, Remove", () => {
  click($c('tr[data-key="solo"] [data-detail]'));
  eq(page(), ["containers", "containers", "solo", true], "the page");
  eq(crumb(), ["link Containers", "solo"], "the breadcrumb");
  eq(acts().includes("Remove"), true, "Remove");
  click($c(".dacts [data-more]"));
  eq(menu(), ["Ports…", "Pause", "Remove (danger)"], "⋯");
  closeMenu();
  click(document.querySelector("#title [data-back]"));
  eq([ui.view, ui.detail, !!$c('tr[data-key="solo"]')], ["containers", null, true], "back to Containers");
});
t("the Kubernetes node container's page keeps its own rule: no Remove", () => {
  kiri.on({ type: "lists", containers: [...ctrs, { ...C("k1", "kiri-k8s", "running"), Labels: { "io.kiri.k8s": "1" } }], images: [], volumes: [], networks: [] });
  if (!isKiriK8s(ui.containers.find((c) => c.Id === "k1"))) return;
  openDetail("k1");
  eq([ui.view, crumb()[0], document.querySelectorAll('#content [data-c="remove"]').length], ["containers", "link Containers", 0], "kiri-k8s");
  closeDetail(); render();
});
// The page's head, as each OS draws it.
openDetail("w1");
ITEMS.push(`compose container page: ${crumb().join(" › ")} | ${acts().join(", ")}`);
report([...R, ...ITEMS.map((i) => "ITEM " + i)]);
