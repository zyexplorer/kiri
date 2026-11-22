// parity: macos windows linux
// A project Kiri manages by name (Compose can't list its file's variables): one
// notice on the row, the page and where the plan would be; its services and
// state from Docker; the file read-only, with no Apply, Discard or plan, and no
// plan asked for. Its actions by name still work.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const NOTE = "Kiri can't check this file's variables, so it manages this project by name. Apply changes from a terminal.", EN = "Kiri can't check this file's variables, so it manages this project by name. Apply changes from a terminal.";
const NAMES = "Names Docker shows may contain secret values.";
ui.vm = "running";
kiri.on({ type: "lists", containers: [{ Id: "d1", Names: ["/loose-db-1"], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": "loose", "com.docker.compose.service": "db" } }], images: [], volumes: [], networks: [] });
const warn = { text: EN, errkey: "byName", errargs: {} };
const row = (o = {}) => ({ name: "loose", dir: "/w/loose", files: ["/w/loose/compose.yaml"], readable: false, error: null, byname: true, warn, updated: 1,
  services: [{ name: "db", image: "postgres:16", build: "", ports: [], volumes: [], depends: [], networks: [], kinds: [] }],
  containers: [{ service: "db", id: "d1", name: "loose-db-1", image: "postgres:16", state: "running", status: "Up" }],
  namewarn: { text: "Names Docker shows may contain secret values.", errkey: "namesShown", errargs: {} }, ...o });
const fresh = { name: "idle", dir: "/w/idle", files: ["/w/idle/compose.yaml"], readable: false, error: null, byname: true, warn, services: [], containers: [], fresh: true };
kiri.on({ type: "composeprojects", logos: {}, projects: [row(), fresh] });
show("compose");
const said = (el) => (el?.textContent || "").split(NOTE).length - 1;
// The list's notices: rows of their own under the project's row, each its whole text in its title.
const notes = (p) => [...document.querySelectorAll(`#content tr.warnrow[data-cmpnote="${p}"]`)].map((r) => r.querySelector(".name > div[title]").title);
t("the row says the notice once, in the window's language", () => {
  const w = notes("loose");
  eq([w.filter((x) => x === NOTE).length, w.includes(NAMES), true || !w.join().includes(EN), document.querySelector('#content tr.warnrow[data-cmpnote="loose"] .tile.note') != null], [1, true, true, true], w.join(" | "));
});
t("with no containers: no services and not started", () => {
  const r = document.querySelector('#content tr.group[data-group="idle"]');
  eq([r.querySelector(".sub-name").textContent.replace(/\u200f/g, ""), notes("idle").filter((x) => x === NOTE).length, document.querySelectorAll('#content tr.unmade[data-cmpsvc="idle"]').length],
    [`Compose · ${tr("not started")} · ${trn(0, "{n} service", "{n} services")}`, 1, 0], r.textContent);
});
t("the page says it once, and its project's buttons act by name", () => {
  cmpOpen("loose");
  eq(said($("content")), 1, $("content").textContent);
  eq([!!document.querySelector('#cmp-head [data-g="restart"]'), !!document.querySelector('#cmp-head [data-g="stop"]'), !!document.querySelector('#cmp-head [data-g="remove"]')], [true, true, true], "Restart, Stop, Remove");
  eq(document.querySelector("[data-cmpapply]"), null, "no Apply");
  sent.length = 0;
  click(document.querySelector('#cmp-head [data-g="stop"]'));
  const s = sent.find((m) => m.type === "composeaction");
  eq([s?.name, s?.value?.action], ["loose", "stop"], JSON.stringify(sent));
});
t("Structure: its services as plain cards, no lines", () => {
  click(document.querySelector('[data-cmptab="structure"]'));
  eq([document.querySelectorAll("#cmp-tab [data-cmpcard]").length, document.querySelectorAll("#cmp-tab svg.wires path[data-a]").length], [1, 0], $("cmp-tab").innerHTML.slice(0, 300));
  eq(said($("content")), 1, "the notice");
});
t("the file tab: read-only, the notice where the plan would be, no plan asked", () => {
  sent.length = 0;
  click(document.querySelector('[data-cmptab="file"]'));
  kiri.on({ type: "composefile", name: "loose", path: "/w/loose/compose.yaml", mtime: 1, text: "include:\n  - ${SUBDIR:-sub}/compose.yaml\n", hash: "h1" });
  const ta = $("cmp-ta");
  eq([ta?.readOnly, said($("cmp-plan")), said($("content"))], [true, 1, 1], $("content").textContent);
  eq([!!document.querySelector("[data-cmpapply]"), !!document.querySelector("[data-cmpdiscard]"), !!document.querySelector("#cmp-plan h4")], [false, false, false], "no Apply, Discard or plan");
  ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  eq(ta.value.includes("  include"), false, "Tab writes nothing");
  return new Promise((r) => setTimeout(r, 500));
});
setTimeout(() => {
  t("no plan was asked for", () => eq(sent.filter((m) => m.type === "composeplan").length, 0, JSON.stringify(sent)));
  report(R);
}, 600);
