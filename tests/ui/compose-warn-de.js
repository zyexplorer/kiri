// parity: macos windows linux
// lang: de
// compose-warn.js in German: the warning in German, its button named as the German button is.
const WANT = "Kiri kann diese Datei mit verborgenem DB_PASSWORD nicht prüfen. Compose akzeptiert sie, daher funktioniert „Anwenden“ trotzdem.";
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
const ctr = (p) => ({ Id: p + "-db", Names: [`/${p}-db-1`], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": p, "com.docker.compose.service": "db" } });
kiri.on({ type: "lists", containers: [ctr("shop"), ctr("vault")], images: [], volumes: [], networks: [] });
const services = [{ name: "db", image: "postgres:16", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
const proj = (name, o = {}) => ({ name, dir: "/w/" + name, files: [`/w/${name}/compose.yaml`], readable: true, updated: 1, services, containers: [{ service: "db", id: name + "-db", name: `${name}-db-1`, image: "postgres:16", state: "running", status: "Up" }], ...o });
const WARN = { text: "Kiri can't check this file with DB_PASSWORD hidden. Compose accepts it, so Apply still works.", errkey: "hiddenCheck", errargs: { name: "DB_PASSWORD" } };
kiri.on({ type: "composeprojects", logos: {}, projects: [proj("shop"), proj("vault", { warn: WARN })] });
show("compose");
const TABS = ["structure", "services", "file", "logs"];
const each = (name, want) => {
  cmpOpen(name);
  kiri.on({ type: "composefile", name, path: `/w/${name}/compose.yaml`, mtime: 1, hash: "h1", text: "services:\n  db:\n    image: postgres:16\n" });
  return TABS.map((k) => { click(document.querySelector(`[data-cmptab="${k}"]`)); const w = $("cmp-warn"); return [k, w.textContent, [...w.childNodes].filter((n) => n.nodeType === 3).length]; });
};
t("a healthy project: nothing under the tabs on any tab, no stray text node", () => {
  eq(each("shop"), TABS.map((k) => [k, "", 0]), "the warning box on each tab");
});
t("a project Kiri can't check with a secret hidden: the warning in the window's language, on every tab", () => {
  eq(each("vault"), TABS.map((k) => [k, WANT, 0]), "the warning box on each tab");
  eq($("content").textContent.includes("NaN"), false, "no NaN on the page");
});
t("the list: a row under that project's says the warning too, whole in its title; the healthy one has none", () => {
  cmp.open = null; render();
  const w = document.querySelector('#content tr.group[data-group="vault"]').nextElementSibling;
  eq([w.matches(".warnrow"), w.querySelector(".name > div[title]")?.title], [true, WANT], "vault's row, under it");
  eq(!!document.querySelector('#content tr.warnrow[data-cmpnote="shop"]'), false, "shop's row");
  eq($("content").textContent.includes("NaN"), false, "no NaN on the list");
});
t("a service shown by name only: its note says Services, as the tab does", () => {
  eq(tr("Kiri can't show this service's details with its secrets hidden"), "Kiri kann die Details dieses Services mit verborgenen Geheimnissen nicht zeigen", "the note");
});
report(R);
