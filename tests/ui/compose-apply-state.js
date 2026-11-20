// parity: macos windows linux
// lang: de
// After Apply: every service that stopped is named. Discard while the file is
// gone clears the note, and the file back as it was offers no Overwrite. A
// window reloaded mid-Apply shows the running Apply with Cancel. A project
// that never appears says so. Kiri's own errors in the plan and the list are
// said in the window's language.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const lastToast = () => [...document.querySelectorAll(".toast")].pop()?.textContent || "";
ui.vm = "running";
kiri.on({ type: "lists", containers: [{ Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": "shop", "com.docker.compose.service": "db" } }], images: [], volumes: [], networks: [] });
const services = [{ name: "db", image: "postgres:16", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
const list = (o = {}, ask) => kiri.on({ type: "composeprojects", ask, logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1, services, containers: [{ service: "db", id: "d1", name: "shop-db-1", image: "postgres:16", state: "running", status: "Up" }], ...o }] });
list();
show("compose");
const F = (text, hash, o = {}) => kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text, hash, mtime: 1, ...o });
const edit = (from, to) => { const ta = $("cmp-ta"); ta.value = ta.value.replace(from, to); ta.dispatchEvent(new Event("input")); };
const open = (text, hash) => { cmpOpen("shop"); F(text, hash); click(document.querySelector('[data-cmptab="file"]')); };
const GONE = { error: "Kiri can't find this project's compose file" };
const TEXT = "services:\n  db:\n    image: postgres:16\n";

t("two services stop after Apply: both are named, each with its logs", () => {
  open(TEXT, "h1");
  edit("postgres:16", "postgres:17");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:17" }], services });
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: true, hash: "h2", failed: [{ svc: "a", id: "ia", status: "Exited (2)" }, { svc: "b", id: "ib", status: "Exited (4)" }] });
  eq([lastToast().includes("a und b"), lastToast().includes("shop")], [true, true], lastToast());
  eq([...document.querySelectorAll("[data-cmplogof]")].map((b) => b.dataset.cmplogof), ["ia", "ib"], "a logs link each");
});
t("Discard while the file is gone: the note goes, and the file back as it was can be applied", () => {
  open(TEXT, "h1");
  edit("postgres:16", "postgres:18");
  F(undefined, undefined, GONE);
  eq(!!document.querySelector("#cmp-conflict .cmp-conflict"), true, "the gone note");
  click(document.querySelector("[data-cmpdiscard]"));
  eq([cmp.gone, !!document.querySelector("#cmp-conflict .cmp-conflict")], [false, false], "the note is gone with the edits");
  F(undefined, undefined, GONE);
  F(TEXT, "h1");
  eq([!!cmp.conflict, !!document.querySelector("[data-cmpoverwrite]"), cmpDirty()], [false, false, false], "no Overwrite: nothing changed");
});
t("the file back as it was before it went: no Overwrite, the edits can be applied", () => {
  open(TEXT, "h1");
  edit("postgres:16", "postgres:19");
  F(undefined, undefined, GONE);
  F(TEXT, "h1");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:19" }], services });
  eq([!!cmp.conflict, cmp.gone, cmpDirty(), document.querySelector("[data-cmpapply]").disabled], [false, false, true, false], "Apply on");
});
t("reloaded mid-Apply: the running Apply shows, with a Cancel that works", () => {
  open(TEXT, "h1");
  edit("postgres:16", "postgres:20");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:20" }], services });
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, busy: true, error: "shop is already being applied", errkey: "busy", errargs: { name: "shop" } });
  eq([cmp.applying?.name, !!document.querySelector("[data-cmpcancel]")], ["shop", true], "running, with Cancel");
  kiri.on({ type: "composeapply", name: "shop", svc: ["db"], state: "recreating" });
  eq(cmp.applying?.svc.db, "recreating", "its progress shows");
  sent.length = 0;
  click(document.querySelector("[data-cmpcancel]"));
  eq(sent.some((m) => m.type === "composecancel" && m.name === "shop"), true, "Cancel asks the engine");
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, cancelled: true, changed: [], saved: true, hash: "h9", error: "Apply cancelled", errkey: "cancelled", errargs: {} });
  eq([cmp.applying, sent.some((m) => m.type === "composefile")], [null, true], "ended; the file read afresh");
});
t("a project that never appears: said, and not left waiting", () => {
  cmp.open = null;
  cmpOpen("gone-one");
  list({}, cmp.askN);
  eq([cmp.opening, lastToast().includes("Dieses Projekt gibt es nicht mehr")], [null, true], lastToast());
});
t("Kiri's own errors in the list and the plan: in the window's language", () => {
  list({ readable: false, error: "docker compose took longer than 30 seconds", errkey: "slow", errargs: { secs: "30" } });
  eq(cmp.list.projects[0].error.includes("länger als 30 Sekunden"), true, cmp.list.projects[0].error);
  list({ readable: false, error: "couldn't read env file /w/shop/.env: it isn't a local regular file", errkey: "badEnv", errargs: { file: "/w/shop/.env" } });
  eq(cmp.list.projects[0].error, "Die env-Datei /w/shop/.env konnte nicht gelesen werden: Sie ist keine normale lokale Datei", "an env file Kiri won't read");
  list();
  open(TEXT, "h1");
  edit("postgres:16", "postgres:21");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, error: { line: null, text: "no docker CLI found", errkey: "noCli", errargs: {} } });
  eq([cmp.plan.error.text.includes("docker-CLI"), cmp.plan.error.text.includes("no docker CLI found")], [true, false], cmp.plan.error.text);
});
t("reloaded mid-Apply with an edit of one's own: the edit stays, Apply stays on, and the toast says so", () => {
  cmp.open = null; cmp.file = null;
  open(TEXT, "h1");
  edit("postgres:16", "postgres:22");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:22" }], services });
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, busy: true, error: "shop is already being applied", errkey: "busy", errargs: { name: "shop" } });
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: true, saved: true, hash: "h7" });
  const toastText = lastToast();
  // The other Apply saved its own file.
  F(TEXT.replace("postgres:16", "postgres:17"), "h7");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:17", to: "postgres:22" }], services });
  eq([/postgres:22/.test(cmp.draft || ""), cmpDirty(), document.querySelector("[data-cmpapply]")?.disabled], [true, true, false], "the edit, with Apply on");
  eq([/angewendet$/.test(toastText), toastText.includes("noch nicht")], [false, true], toastText);
});
report(R);
