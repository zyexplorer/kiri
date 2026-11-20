// parity: macos windows linux
// Compose's own place: the list (Containers' grouped rows: each project's containers fold under it), a
// project's page has Structure, Services, Compose file and Logs, and Apply sends
// the edited file once the engine says what it will do. Logos go in once.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const L = (svc) => ({ "com.docker.compose.project": "shop", "com.docker.compose.service": svc });
ui.vm = "running";
kiri.on({ type: "lists", containers: [
  { Id: "w1", Names: ["/shop-web-1"], Image: "shop-web", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: L("web") },
  { Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: L("db") }], images: [], volumes: [], networks: [] });
const svc = (name, o) => ({ name, image: "", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [], ...o });
const PROJECTS = { type: "composeprojects", logos: { postgresql: ["4169E1", "M1 1h22v22H1z"], nodedotjs: ["5FA04E", "M2 2h20v20H2z"] }, projects: [{
  name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1,
  services: [svc("db", { image: "postgres:16", tech: { slug: "postgresql", role: "Database", app: false, how: "the image is postgres" } }),
    svc("web", { build: "/w/shop/web", ports: ["3000:3000"], depends: ["db"], tech: { slug: "nodedotjs", lang: "Node.js", fw: "Next.js", role: "Web", app: true, how: "Dockerfile FROM node:22" } })],
  containers: [{ service: "db", id: "d1", name: "shop-db-1", image: "postgres:16", state: "running", status: "Up" }, { service: "web", id: "w1", name: "shop-web-1", image: "shop-web", state: "running", status: "Up" }] }] };

t("Compose is in the sidebar under Docker, and asks the engine for its projects", () => {
  const b = document.querySelector('nav button[data-view="compose"]');
  eq([!!b, $("n-compose").textContent], [true, "1"], "a button, counting the projects");
  sent.length = 0; click(b);
  eq([ui.view, sent.some((m) => m.type === "composeprojects")], ["compose", true], "it asks");
});
t("a project's row folds its containers open under it, as on Containers; logos go into the page once", () => {
  kiri.on(PROJECTS); kiri.on(PROJECTS);
  eq([document.querySelectorAll("#cmp-logos symbol").length, [...document.querySelectorAll("#content tr.member")].map((r) => r.dataset.key).sort()], [2, ["d1", "w1"]], "two symbols, two container rows");
  const g = document.querySelector('#content tr.group[data-group="shop"]');
  eq(getComputedStyle(g).cursor, "pointer", "the row folds");
  click(g.querySelector("td[data-fill]"));
  eq([document.querySelectorAll("#content tr.member").length, document.querySelectorAll("#content tr.group .tile").length], [0, 1], "folded: its containers hidden, the project's own tile only");
  click(document.querySelector('#content tr.group[data-group="shop"] td[data-fill]'));
});
t("a project's name opens its page; a service's card can be lit", () => {
  sent.length = 0;
  click(document.querySelector('#content tr.group [data-gopen="shop"]'));
  eq([cmp.open, cmp.map.focus, sent.some((m) => m.type === "composefile")], ["shop", null, true], "opens the page");
  cmpOpen("shop", "web");
  eq([...document.querySelectorAll("#cmp-tabs [data-cmptab]")].map((b) => b.dataset.cmptab), ["structure", "services", "file", "logs"], "four tabs");
  eq(document.querySelectorAll("#content .card").length, 2, "two cards on the map");
  eq(document.querySelector('.card[data-cmpcard="db"]').classList.contains("dim"), false, "what web needs stays lit");
  const lit = document.querySelector('.card[data-cmpcard="web"]');
  eq([lit.classList.contains("sel"), getComputedStyle(lit).cursor], [true, "pointer"], "the selected card still shows the hand");
});
t("the file's tab: an edit asks what Apply will do, and Apply lights up only once the engine says", () => {
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  db:\n    image: postgres:16\n  web:\n    build: ./web\n", hash: "s1" });
  click(document.querySelector('[data-cmptab="file"]'));
  eq(document.querySelector("#cmp-outline").textContent.includes("web"), true, "the outline lists the services");
  const go = document.querySelector('#cmp-outline [data-cmpgo="4"]');
  eq([go?.querySelector(".ln").textContent, go?.title], ["line 4", "Starts on line 4 · click to jump there"], "the number reads as the line a service starts on");
  const ta = $("cmp-ta"); ta.value = ta.value.replace("postgres:16", "postgres:17"); ta.dispatchEvent(new Event("input"));
  eq(document.querySelector("[data-cmpapply]").disabled, false, "edited: Apply is on");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, error: { line: 3, text: "bad" } });
  eq([document.querySelector("[data-cmpapply]").disabled, !!document.querySelector("#cmp-gut .bad")], [true, true], "a broken file stops Apply and marks its line");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:17" }, { n: "web", act: "same", what: [] }], services: PROJECTS.projects[0].services });
  eq(document.querySelector("#cmp-plan li.a-chg .what").textContent, "db", "db is recreated");
  sent.length = 0; click(document.querySelector("[data-cmpapply]"));
  const a = sent.find((m) => m.type === "composeapply");
  eq([a?.name, a?.value.text.includes("postgres:17")], ["shop", true], "Apply sends the edited file");
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: true, hash: "ok1" });
  eq([cmp.applying, cmpDirty()], [null, false], "applied: the file is saved");
});
t("the plan is compared with what runs: a stopped service starts, an edit made elsewhere or a newer image recreates", () => {
  const ta = $("cmp-ta"); ta.value = ta.value + "\n"; ta.dispatchEvent(new Event("input"));
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["outside"], from: "postgres:16", to: "postgres:16" }, { n: "web", act: "start", what: [] }, { n: "old", act: "rm", what: [] }], services: PROJECTS.projects[0].services });
  eq(document.querySelector("#cmp-plan li.a-chg .why").textContent, "the file changed outside Kiri", "says why db is recreated");
  eq(document.querySelector("#cmp-plan li.a-start .act").textContent, "Start", "a stopped service is started");
  eq(document.querySelector("#cmp-plan li.a-rm .why").textContent.includes("volumes stay"), true, "a removed service keeps its volumes");
});
t("Apply shows where each service is, not Compose's lines", () => {
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "shop", svc: ["db"], state: "recreating" });
  kiri.on({ type: "composeapply", name: "shop", svc: ["web"], state: "started" });
  eq([document.querySelector("#cmp-plan li.a-chg .act").textContent, document.querySelector("#cmp-plan li.a-start .act").textContent], ["Recreating…", "Started"], "each service's state");
  eq(!!document.querySelector("#cmp-plan .cmp-run"), false, "no raw lines while there are none to show");
  click(document.querySelector('[data-cmptab="structure"]'));
  eq(document.querySelector('.card[data-cmpcard="db"] .tag')?.textContent, "Recreating…", "and on its map card");
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: true, hash: "ok2" });
});
t("Apply that leaves a service stopped says which one, with a link to its logs", () => {
  click(document.querySelector('[data-cmptab="file"]'));
  const ta = $("cmp-ta"); ta.value = ta.value.replace("postgres:17", "postgres:18"); ta.dispatchEvent(new Event("input"));
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:17", to: "postgres:18" }], services: PROJECTS.projects[0].services });
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "shop", svc: ["db"], state: "started" });
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: true, failed: [{ svc: "db", id: "d1", status: "Exited (1) 2 seconds ago" }], hash: "ok4" });
  const toastEl = [...document.querySelectorAll(".toast")].pop();
  eq([toastEl?.classList.contains("error"), toastEl?.textContent.includes("Applied shop, but db stopped")], [true, true], "not a plain success toast");
  const box = document.querySelector("#cmp-plan .cmp-failed");
  eq([!!box, box?.textContent.includes("db stopped after Apply"), box?.textContent.includes("Exited (1)")], [true, true, true], "the plan names the service and its state");
  const link = box.querySelector("[data-cmplogof]");
  eq(getComputedStyle(link).cursor, "pointer", "the logs link is clickable");
  click(link);
  eq([ui.detail?.id, ui.detail?.tab], ["d1", "logs"], "it opens db's logs");
  const on = document.querySelector('#content .dtabs [data-dtab="logs"]');
  eq([ui.view, on?.getAttribute("aria-current"), !!document.querySelector("#content .logbar"), $("title").textContent.includes("shop-db-1")], ["compose", "true", true, true], "db's Logs tab is on screen, in Compose");
  closeDetail(); show("compose"); cmpOpen("shop");
});
t("the Services tab's container link opens that container's page on screen", () => {
  click(document.querySelector('[data-cmptab="services"]'));
  const link = document.querySelector('#content [data-detail="w1"]');
  eq([!!link, getComputedStyle(link).cursor], [true, "pointer"], "a link");
  click(link);
  eq([ui.view, ui.detail?.id, !!document.querySelector("#content .dtabs"), $("title").textContent.includes("shop-web-1")], ["compose", "w1", true, true], "web's page is drawn, in Compose");
  closeDetail(); show("compose"); cmpOpen("shop");
});
t("a long state ends in … on its map card", () => {
  const long = { ...PROJECTS, projects: [{ ...PROJECTS.projects[0], containers: PROJECTS.projects[0].containers.map((c) => ({ ...c, status: "Up Less than a second" })) }] };
  kiri.on({ type: "lists", containers: [
    { Id: "w1", Names: ["/shop-web-1"], Image: "shop-web", State: "running", Status: "Up Less than a second", Created: 1, Ports: [], Labels: L("web") },
    { Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "running", Status: "Up Less than a second", Created: 1, Ports: [], Labels: L("db") }], images: [], volumes: [], networks: [] });
  kiri.on(long);
  click(document.querySelector('[data-cmptab="structure"]'));
  const card = document.querySelector('.card[data-cmpcard="web"]'), pill = card.querySelector(".nm .pill"), nm = card.querySelector(".nm");
  const cs = getComputedStyle(pill);
  eq([cs.textOverflow, cs.overflow, nm.scrollWidth <= nm.clientWidth + 1], ["ellipsis", "hidden", true], "the pill ends in … inside its card");
});
t("back to the list", () => { click(document.querySelector("[data-cmpback]")); });
t("Open a compose file… asks the app for the system's dialog; the page never sends a path", () => {
  const b = document.querySelector("[data-cmpopenfile]");
  eq([!!b, getComputedStyle(b).cursor], [true, "pointer"], "a button in the list's bar");
  sent.length = 0; click(b);
  eq(sent.map((m) => [m.type, m.name || "", JSON.stringify(m.value ?? null)]), [["composeopen", "", "null"]], "only the command");
});
const BLOG = { name: "blog", dir: "/w/blog", files: ["/w/blog/compose.yaml"], readable: true, fresh: true, updated: null,
  services: [svc("app", { image: "ghost:5", depends: ["mysql"] }), svc("mysql", { image: "mysql:8" })], containers: [] };
t("a file opened that has never run: listed as Not started, and its page opens", () => {
  sent.length = 0;
  kiri.on({ type: "composeopened", name: "blog" });
  eq(cmp.open, null, "waits for the list");
  kiri.on({ ...PROJECTS, projects: [...PROJECTS.projects, BLOG] });
  eq([cmp.open, sent.some((m) => m.type === "composefile" && m.name === "blog")], ["blog", true], "the project's page, asking for its file");
  eq(document.querySelector("#cmp-head .pill").textContent, "Not started", "its state");
  eq([...document.querySelectorAll("#cmp-tabs [data-cmptab]")].length, 4, "the same four tabs");
  eq(document.querySelectorAll("#content .card").length, 2, "Structure maps its services");
  eq($("n-compose").textContent, "2", "the sidebar counts it");
});
t("its plan is every service new, and Apply brings it up", () => {
  const text = "services:\n  app:\n    image: ghost:5\n    depends_on: [mysql]\n  mysql:\n    image: mysql:8\n";
  kiri.on({ type: "composefile", name: "blog", path: "/w/blog/compose.yaml", text, hash: "b1" });
  sent.length = 0;
  click(document.querySelector('[data-cmptab="file"]'));
  eq($("cmp-edstate").textContent, "Saved, not started yet", "the editor's state");
  eq(document.querySelector("[data-cmpapply]").disabled, false, "Apply is on with nothing edited");
  kiri.on({ type: "composeplan", name: "blog", seq: cmp.seq, steps: [{ n: "mysql", act: "add", what: [], from: "", to: "mysql:8" }, { n: "app", act: "add", what: [], from: "", to: "ghost:5" }], services: BLOG.services });
  eq([...document.querySelectorAll("#cmp-plan li")].map((li) => li.className), ["a-add", "a-add"], "every service is new");
  click(document.querySelector('[data-cmptab="structure"]'));
  eq(document.querySelectorAll("#content .card.add").length, 2, "and marked new on the map");
  click(document.querySelector('[data-cmptab="file"]'));
  sent.length = 0; click(document.querySelector("[data-cmpapply]"));
  const a = sent.find((m) => m.type === "composeapply");
  eq([a?.name, a?.value.text], ["blog", text], "Apply sends the saved file");
  kiri.on({ type: "composeapply", name: "blog", done: true, ok: true, failed: [], hash: "ok3" });
  eq(cmp.applying, null, "applied");
});
t("an opened project that never ran can be taken off the list", () => {
  kiri.on({ ...PROJECTS, projects: [...PROJECTS.projects, BLOG] });
  const rm = document.querySelector('#cmp-head [data-cmpforget="blog"]');
  eq([!!rm, getComputedStyle(rm).cursor], [true, "pointer"], "on its page");
  click(document.querySelector("[data-cmpback]"));
  const items = (p) => { click(document.querySelector(`#content tr.group[data-group="${p}"] [data-gmore]`)); const m = [...document.querySelectorAll(".menu button")]; return m; };
  const shop = items("shop").map((b) => b.textContent); closeMenu();
  const blog = items("blog");
  eq([blog.map((b) => b.textContent), shop], [["Open project", "Remove from the list"], ["Open project", "Restart all", "Remove project"]], "in its row's ⋯, and only on a project that never ran");
  eq(document.querySelector('#content tr.group[data-group="blog"] .sub-name').textContent.replace(/‏/g, ""), "Compose · not started · 2 services", "its row's line");
  sent.length = 0; click(blog[1]);
  eq(sent.map((m) => [m.type, m.name]), [["composeforget", "blog"]], "asks the engine to forget it");
});
t("with no projects, the empty page, and Open a compose file… in the bar", () => {
  const was = ui.containers.slice();
  kiri.on({ type: "lists", containers: [], images: [], volumes: [], networks: [] });
  kiri.on({ type: "composeprojects", logos: {}, projects: [] });
  eq([!!document.querySelector("#content .empty"), !!document.querySelector("#hright [data-cmpopenfile]")], [true, true], "the empty page, the button in the bar");
  kiri.on({ type: "lists", containers: was, images: [], volumes: [], networks: [] });
  kiri.on(PROJECTS);
});
t("a Compose container is in Compose only: under its project, its address under its name; Containers says where it is", () => {
  kiri.on({ type: "lists", containers: [
    { Id: "w1", Names: ["/shop-web-1"], Image: "shop-web", State: "running", Status: "Up 1 hour", Created: 1, Ports: [{ PrivatePort: 3000, PublicPort: 3000, Type: "tcp" }], Labels: L("web") }], images: [], volumes: [], networks: [] });
  show("compose");
  const g = document.querySelector('#content tr.group[data-group="shop"]'), r = document.querySelector('#content tr.member[data-key="w1"]');
  eq([!!g, !!r?.querySelector(".tree"), !!r?.querySelector(".sub-name .sub-addr"), document.querySelectorAll("[data-goproj]").length], [true, true, true, 0], "under its project");
  show("containers");
  eq([document.querySelector('#content tr[data-key="w1"]'), document.querySelector("#content .empty h3")?.textContent], [null, "Your only container is in a Compose project"], "not in Containers");
  show("compose");
});
t("Apply sends what the draft was made from; a file changed outside Kiri offers Reload or Keep editing", () => {
  cmpOpen("shop"); kiri.on(PROJECTS);
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  db:\n    image: postgres:16\n", hash: "h1", mtime: 1 });
  click(document.querySelector('[data-cmptab="file"]'));
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "same", what: [] }], services: PROJECTS.projects[0].services });
  eq([$("cmp-edstate").textContent, document.querySelector("[data-cmpapply]").disabled], ["Saved, matches what runs", true], "unedited, and the plan says so");
  // Unedited: a change outside Kiri is taken in, and planned against what runs.
  sent.length = 0;
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  db:\n    image: postgres:17\n", hash: "h2", mtime: 2 });
  eq([$("cmp-ta").value.includes("postgres:17"), $("cmp-edstate").textContent === "Saved, matches what runs"], [true, false], "the new file, no claim yet");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["outside"], from: "postgres:16", to: "postgres:17" }], services: PROJECTS.projects[0].services });
  eq([$("cmp-edstate").textContent, document.querySelector("[data-cmpapply]").disabled], ["Saved, not applied", false], "saved but not what runs: Apply is on");
  const ta = $("cmp-ta"); ta.value = ta.value.replace("postgres:17", "postgres:18"); ta.dispatchEvent(new Event("input"));
  sent.length = 0; click(document.querySelector("[data-cmpapply]"));
  eq(sent.find((m) => m.type === "composeapply")?.value.base, "h2", "the hash it was made from");
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, conflict: true, error: "The file changed outside Kiri" });
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  db:\n    image: mysql:8\n", hash: "h3", mtime: 3 });
  const box = document.querySelector("#cmp-conflict .cmp-conflict");
  eq([!!box, $("cmp-ta").value.includes("postgres:18"), document.querySelector("[data-cmpapply]").disabled], [true, true, true], "asks; the edits stay; no Apply meanwhile");
  eq([getComputedStyle(box.querySelector("[data-cmpreload]")).cursor, getComputedStyle(box.querySelector("[data-cmpkeep]")).cursor], ["pointer", "pointer"], "both clickable");
  click(box.querySelector("[data-cmpkeep]"));
  eq([!!document.querySelector("#cmp-conflict .cmp-conflict"), cmp.file.hash, $("cmp-ta").value.includes("postgres:18")], [false, "h2", true], "keep editing: still based on the file it was made from");
  // The next read of the same outside file doesn't nag; Apply still refuses (the engine checks) and says so again.
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  db:\n    image: mysql:8\n", hash: "h3", mtime: 3 });
  eq(!!document.querySelector("#cmp-conflict .cmp-conflict"), false, "no banner for the version kept over");
  sent.length = 0; click(document.querySelector("[data-cmpapply]"));
  eq(sent.find((m) => m.type === "composeapply")?.value.base, "h2", "Apply after Keep editing: still the old base, so it can't overwrite");
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, conflict: true, hash: "h3", error: "The file changed outside Kiri" });
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  db:\n    image: mysql:8\n", hash: "h3", mtime: 3 });
  const again = document.querySelector("#cmp-conflict .cmp-conflict");
  eq([!!again, $("cmp-ta").value.includes("postgres:18")], [true, true], "the conflict shows again; the edits stay");
  const over = again.querySelector("[data-cmpoverwrite]");
  eq([!!over, over && getComputedStyle(over).cursor, over?.classList.contains("danger")], [true, "pointer", true], "Overwrite their changes: a destructive choice");
  sent.length = 0; click(over);
  eq(sent.some((m) => m.type === "composeapply"), false, "only after a confirm");
  const dlg = document.querySelector(".scrim [data-go]");
  eq(!!dlg, true, "asks first");
  click(document.querySelector(".scrim [data-close]"));
  eq([sent.some((m) => m.type === "composeapply"), !!document.querySelector("#cmp-conflict .cmp-conflict")], [false, true], "Cancel: nothing written");
  click(document.querySelector("#cmp-conflict [data-cmpoverwrite]")); click(document.querySelector(".scrim [data-go]"));
  eq(sent.find((m) => m.type === "composeapply")?.value.base, "h3", "confirmed: over the version they saw");
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: true, hash: "h3b" });
  eq([cmp.file.hash, cmpDirty()], ["h3b", false], "the next Apply's base comes with the answer, never null");
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: $("cmp-ta").value, hash: "h3b", mtime: 3 });
  const t2 = $("cmp-ta"); t2.value = t2.value.replace("postgres:18", "postgres:19"); t2.dispatchEvent(new Event("input"));
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  db:\n    image: mysql:8\n", hash: "h3", mtime: 3 });
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  db:\n    image: mariadb:11\n", hash: "h4", mtime: 4 });
  click(document.querySelector("#cmp-conflict [data-cmpreload]"));
  eq([$("cmp-ta").value.includes("mariadb:11"), cmpDirty(), cmp.file.hash], [true, false, "h4"], "reload: the file as it is now");
});
t("a failed Apply keeps the file saved, says so, keeps Apply on, and Discard puts the old file back", () => {
  const ta = $("cmp-ta"); ta.value = ta.value.replace("mariadb:11", "mariadb:12"); ta.dispatchEvent(new Event("input"));
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "mariadb:11", to: "mariadb:12" }], services: PROJECTS.projects[0].services });
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "shop", line: "Network shop_default Creating" });
  kiri.on({ type: "composeapply", name: "shop", line: "Network shop_default Creating" });
  eq(cmp.applying.lines, ["Network shop_default Creating"], "a line Compose says twice shows once");
  eq(!!document.querySelector("[data-cmpcancel]"), true, "a long Apply can be cancelled");
  sent.length = 0;
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, saved: true, error: "failed to solve: exit code 1", hash: "h5" });
  eq([cmpDirty(), $("cmp-edstate").textContent, document.querySelector("[data-cmpapply]").disabled, cmp.file.hash], [false, "Saved, not applied: failed to solve: exit code 1", false, "h5"], "saved, not applied, Apply still on, with the saved file's hash");
  eq(sent.some((m) => m.type === "composefile"), true, "reads the saved file's hash again");
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: $("cmp-ta").value, hash: "h5", mtime: 5, restore: true });
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["outside"], from: "mariadb:11", to: "mariadb:12" }], services: PROJECTS.projects[0].services });
  eq($("cmp-edstate").textContent.startsWith("Saved, not applied"), true, "never 'matches what runs' while it doesn't");
  const back = document.querySelector("[data-cmprestore]");
  eq([!!back, back && getComputedStyle(back).cursor], [true, "pointer"], "Discard offers the old file back");
  sent.length = 0; click(back);
  eq(sent.map((m) => [m.type, m.name]), [["composerestore", "shop"]], "asks the engine to put it back");
  kiri.on({ type: "composerestored", name: "shop" });
  eq(cmp.unapplied, null, "no longer unapplied");
});
t("Cancel says Apply was cancelled, not the last line Compose wrote", () => {
  cmpOpen("shop"); kiri.on(PROJECTS);
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  db:\n    image: postgres:16\n", hash: "c1", mtime: 1 });
  click(document.querySelector('[data-cmptab="file"]'));
  const ta = $("cmp-ta"); ta.value = ta.value.replace("postgres:16", "postgres:17"); ta.dispatchEvent(new Event("input"));
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:17" }], services: PROJECTS.projects[0].services });
  click(document.querySelector("[data-cmpapply]"));
  sent.length = 0; click(document.querySelector("[data-cmpcancel]"));
  eq(sent.map((m) => [m.type, m.name]), [["composecancel", "shop"]], "asks the engine to stop");
  kiri.on({ type: "composeapply", name: "shop", line: "Container shop-db-1 Stopping" });
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, cancelled: true, changed: ["db"], saved: true, error: "Apply cancelled", hash: "c2" });
  const msg = [...document.querySelectorAll(".toast")].map((x) => x.textContent).join(" | ");
  eq([msg.includes("Apply cancelled. db had already changed"), msg.includes("Stopping"), $("cmp-edstate").textContent], [true, false, "Saved, not applied: Apply cancelled"], "says it was cancelled");
});
t("the file's outline takes a service named like an object's own key", () => {
  eq([...cmpLines("services:\n  constructor:\n    image: a\n  toString:\n    image: b\n")], [["constructor", 2], ["toString", 4]], "both, with their lines");
});
t("the map's links have arrowheads, pointing at what a service needs", () => {
  kiri.on(PROJECTS); cmpOpen("shop");
  const w = document.querySelector('.wires path[data-a="web"][data-b="db"]');
  const id = (w?.getAttribute("marker-end") || "").replace(/^url\(#|\)$/g, "");
  eq([!!id, !!document.getElementById(id)?.querySelector("path")], [true, true], "a marker at its end");
});
t("Start all and Remove project go to Compose by the project's name", () => {
  sent.length = 0; click(document.querySelector('#cmp-head [data-g="start"], #cmp-head [data-g="stop"]'));
  const m = sent.find((x) => x.type === "composeaction");
  eq([m?.name, ["start", "stop"].includes(m?.value.action), sent.some((x) => x.type === "container")], ["shop", true, false], "one command, not one per container");
});
t("a project whose file is gone says so plainly, and its logs still load", () => {
  kiri.on({ type: "composefile", name: "shop", error: "Kiri can't find this project's compose file" });
  click(document.querySelector('[data-cmptab="file"]'));
  eq($("cmp-tab").textContent.trim(), "Kiri can't find this project's compose file. Its logs, Start, Stop and Remove still work.", "no raw error");
  sent.length = 0; click(document.querySelector('[data-cmptab="logs"]'));
  eq(sent.some((m) => m.type === "composelogs" && m.name === "shop"), true, "asks for its logs");
});
report(R);
