// parity: macos windows linux
// window: 1440x900
// A project's row, in Compose (Containers lists no projects): its fold is a button you reach with Tab,
// Return and Space fold and unfold it, aria-expanded and its words follow, and focus stays on it through a
// refresh; Pods' namespace rows fold the same way. Start all, Stop all and the ⋯ (Restart all, Remove project)
// roll while the engine works, till its answer or a list that shows the change. The tabs count containers: a
// service with no container, and a project never started, show under All only.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const now = Math.floor(Date.now() / 1000);
const C = (id, project, service, state, ago) => ({ Id: id, Names: [`/${project}-${service}-1`], Image: `${service}:dev`, State: state, Status: state === "running" ? "Up 1 hour" : "Exited (0)",
  Created: now - ago, Ports: [], Labels: { "com.docker.compose.project": project, "com.docker.compose.service": service } });
let ctrs = [C("b1", "blog", "app", "running", 300), C("b2", "blog", "worker", "exited", 900), C("s1", "shop", "api", "running", 600), C("s2", "shop", "db", "exited", 1200)];
const lists = () => kiri.on({ type: "lists", containers: ctrs, images: [], volumes: [], networks: [] });
const svc = (name) => ({ name, image: name + ":dev", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
const mem = (p) => ctrs.filter((c) => c.Labels["com.docker.compose.project"] === p).map((c) => ({ service: c.Labels["com.docker.compose.service"], id: c.Id, name: c.Names[0].slice(1), image: c.Image, state: c.State, status: c.Status }));
ui.vm = "running";
kiri.on({ type: "status", version: "0.1.4", dev: true, share: true, app: "Kiri", ours: "kiri", context: "kiri" });
lists();
kiri.on({ type: "composeprojects", logos: {}, projects: [
  { name: "blog", dir: "/w/blog", files: ["/w/blog/compose.yaml"], readable: true, services: [svc("app"), svc("worker")], containers: mem("blog") },
  { name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, services: [svc("api"), svc("db"), svc("mailer")], containers: mem("shop") },
  { name: "draft", dir: "/w/draft", files: ["/w/draft/compose.yaml"], readable: true, fresh: true, services: [svc("web"), svc("api")], containers: [] }] });
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const $c = (q) => document.querySelector("#content " + q), $$c = (q) => [...document.querySelectorAll("#content " + q)];
const grp = (p) => $c(`tr.group[data-group="${CSS.escape(p)}"]`), fold = (p) => grp(p).querySelector("button.chev");
// What Tab reaches in a row, in order.
const TABBABLE = "button:not(:disabled), a[href], [tabindex]:not([tabindex='-1'])";
const order = (tr) => [...tr.querySelectorAll(TABBABLE)].filter((el) => el.tabIndex >= 0).map((el) => el.matches("button.chev") ? "fold" : el.matches("[data-gopen]") ? "name" : el.matches("[data-gpick]") ? "box" : el.getAttribute("aria-label") || el.textContent.trim());
// A key on the focused control: no handler of the page's may take it (the button's own Return and Space then click it).
const key = (el, k) => { const ev = new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true }); el.dispatchEvent(ev); if (ev.defaultPrevented) throw new Error(k + " was taken by the page"); el.click(); };
const members = (p) => $$c(`tr.member`).filter((r) => { let g = r.previousElementSibling; while (g && !g.matches(".group")) g = g.previousElementSibling; return g?.dataset.group === p; }).length;

function keyboardFold(view, p, tabs) {
  show(view);
  t(`${view}: Tab reaches the fold, a button, then ${view === "compose" ? "the name, " : ""}the row's buttons`, () => {
    const b = fold(p);
    eq([b.tagName, b.type, b.tabIndex, b.textContent, b.getAttribute("aria-expanded"), b.getAttribute("aria-label")], ["BUTTON", "button", 0, "▾", "true", `Hide ${p}'s containers`], "the button");
    eq(order(grp(p)), tabs, "tab order");
  });
  t(`${view}: Return folds and Space unfolds; aria-expanded, the arrow and the words follow; focus stays on it`, () => {
    fold(p).focus();
    const n = members(p);
    key(document.activeElement, "Enter");
    eq([document.activeElement === fold(p), fold(p).getAttribute("aria-expanded"), grp(p).getAttribute("aria-expanded"), fold(p).textContent, fold(p).getAttribute("aria-label"), members(p)],
      [true, "false", "false", "▸", `Show ${p}'s containers`, 0], "folded");
    key(document.activeElement, " ");
    eq([document.activeElement === fold(p), fold(p).getAttribute("aria-expanded"), fold(p).textContent, members(p)], [true, "true", "▾", n], "open again");
  });
  t(`${view}: focus stays on the fold through a refresh`, () => {
    fold(p).focus();
    lists(); render();
    eq(document.activeElement === fold(p) && document.activeElement.isConnected, true, "after a list");
    key(document.activeElement, "Enter"); lists(); render();
    eq([document.activeElement === fold(p), fold(p).getAttribute("aria-expanded")], [true, "false"], "folded, after a list");
    key(document.activeElement, " ");
  });
}
keyboardFold("compose", "blog", ["fold", "name", "Start all", "Stop all", "More actions for blog"]);

t("pods: a namespace's row folds the same way, by a button with its words", () => {
  kiri.on({ type: "k8s", state: "running", ready: true, services: [], context: "kiri", pods: [{ ns: "default", name: "web-1", ip: "10.42.0.9", labels: {}, status: "Running", phase: "Running", ready: "1/1", allReady: true, restarts: 0, created: "2026-10-28T00:00:00Z",
    containers: [{ name: "c", image: "nginx", ready: true, restarts: 0, state: "running", ports: [] }] }] });
  show("pods");
  const g = $c('tr.group[data-group="ns:default"]'), b = g?.querySelector("button.chev");
  eq([b?.tagName, b?.getAttribute("aria-expanded"), b?.getAttribute("aria-label")], ["BUTTON", "true", "Hide default's pods"], "the button");
  b.focus(); key(b, "Enter");
  const b2 = $c('tr.group[data-group="ns:default"] button.chev');
  eq([document.activeElement === b2, b2.getAttribute("aria-expanded"), b2.getAttribute("aria-label")], [true, "false", "Show default's pods"], "folded");
  key(b2, " ");
});

// The busy state.
const btnOf = (p, sel) => grp(p).querySelector(sel);
for (const view of ["compose"]) {
  t(`${view}: Start all rolls till the engine is done`, () => {
    show(view); sent.length = 0;
    click(btnOf("blog", '[data-g="start"]'));
    eq([btnOf("blog", '[data-g="start"]').classList.contains("rolling"), sent.map((s) => s.type)], [true, [view === "compose" ? "composeaction" : "container"]], "rolling, sent");
    lists(); render();
    eq(btnOf("blog", '[data-g="start"]').classList.contains("rolling"), true, "a list with no change keeps it rolling");
    if (view === "compose") kiri.on({ type: "composeaction", name: "blog" });
    else { ctrs = ctrs.map((c) => (c.Id === "b2" ? { ...c, State: "running", Status: "Up 1 second" } : c)); lists(); }
    render();
    eq([ui.gacting.has("blog"), !!btnOf("blog", '[data-g="start"]')], [false, view === "compose"], "done");
    if (view === "containers") { ctrs = ctrs.map((c) => (c.Id === "b2" ? { ...c, State: "exited", Status: "Exited (0)" } : c)); lists(); }
  });
  t(`${view}: Stop all rolls, and the ⋯ while Restart all or Remove project runs`, () => {
    show(view);
    click(btnOf("shop", '[data-g="stop"]'));
    eq(btnOf("shop", '[data-g="stop"]').classList.contains("rolling"), true, "Stop all");
    eq(btnOf("blog", '[data-g="stop"]').classList.contains("rolling"), false, "only that project's");
    if (view === "compose") kiri.on({ type: "composeaction", name: "shop" });
    else { ctrs = ctrs.map((c) => (c.Id === "s1" ? { ...c, State: "exited", Status: "Exited (0)" } : c)); lists(); }
    render();
    eq(ui.gacting.has("shop"), false, "done");
    projectAction("blog", "restart");
    eq(btnOf("blog", "[data-gmore]").classList.contains("rolling"), true, "⋯ for Restart all");
    if (view === "compose") kiri.on({ type: "composeaction", name: "blog" });
    else { ctrs = ctrs.map((c) => (c.Id === "b1" ? { ...c, Status: "Up 1 second" } : c)); lists(); }
    render();
    eq(btnOf("blog", "[data-gmore]").classList.contains("rolling"), false, "done");
    ctrs = ctrs.map((c) => (c.Id === "s1" ? { ...c, State: "running", Status: "Up 1 hour" } : c)); lists();
  });
}
t("a rolling button looks as Containers' own: a ring in place of its icon, and no click", () => {
  show("compose"); projectAction("shop", "stop");
  const b = btnOf("shop", '[data-g="stop"]'), m = (projectAction("blog", "remove"), btnOf("blog", "[data-gmore]"));
  eq([getComputedStyle(b).pointerEvents, getComputedStyle(b.querySelector("svg")).display, getComputedStyle(b, "::after").borderTopStyle], ["none", "none", "solid"], "Stop all");
  eq([getComputedStyle(m).pointerEvents, getComputedStyle(m, "::after").borderTopStyle, m.getBoundingClientRect().width], ["none", "solid", 30], "⋯");
  ui.gacting.clear(); render();
});

// The tabs.
t("Compose's tabs count containers: a service with no container, and a project never started, show under All only", () => {
  show("compose");
  const n = (k) => +(document.querySelector(`[data-ltab="${k}"] .n`)?.textContent ?? 0);
  eq([n("all"), n("running"), n("stopped"), n("failed")], [4, 2, 2, 0], "counts: containers only");
  const svcs = () => $$c("tr.unmade").map((r) => r.dataset.svc), groups = () => $$c("tr.group").map((r) => r.dataset.group);
  eq([groups(), svcs()], [["blog", "shop", "draft"], ["mailer", "web", "api"]], "All");
  eq(grp("shop").querySelector(".sub-name").textContent.replace(/‏/g, ""), "Compose · 1 of 2 running", "x of y counts containers, not services");
  for (const k of ["running", "stopped"]) {
    click(document.querySelector(`[data-ltab="${k}"]`));
    eq([groups().includes("draft"), svcs()], [false, []], k);
  }
  click(document.querySelector('[data-ltab="all"]'));
  eq(groups().includes("draft"), true, "back under All");
});
t("a project never started: Start all says what a not-created service's Start says; its name and its services' rows open the page", () => {
  show("compose");
  const g = grp("draft"), start = g.querySelector(".btn.go"), svcStart = $c('tr.unmade[data-svc="mailer"] .btn.go');
  eq([start.disabled, start.title, start.getAttribute("aria-label")], [true, "Apply the compose file to create it", "Apply the compose file to create it"], "Start all");
  eq([svcStart.disabled, svcStart.title], [true, start.title], "the same words as a service's Start");
  click(g.querySelector("[data-gopen]"));
  eq(cmp.open, "draft", "the name opens the page");
  cmp.open = null; render();
  click($c('tr.unmade[data-svc="web"] td.col-state'));
  eq([cmp.open, cmp.tab], ["draft", "services"], "a service's row opens it at Services");
  cmp.open = null; render();
});
report(R);
