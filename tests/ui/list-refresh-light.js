// parity: macos windows linux
// window: 1440x900
// A refresh is light on a long list. The same list again changes nothing on screen; a list with one container
// changed changes that row only, and keeps the columns' sizing (the table's "sized", its headings' widths and a
// wide cell's span): taken off by each refresh, the table laid out every row by its contents, then was measured
// and sized again, 76 ms for 500 images. Folding a project takes its rows out without moving the rest. The
// columns still follow the list's width.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const now = Math.floor(Date.now() / 1000);
const C = (id, project, service, i, state = "running") => ({ Id: id, ImageID: `sha256:${i % 300}`, Names: [`/${project}-${service}-1`], Image: `${service}:dev`, State: state,
  Status: state === "running" ? "Up 1 hour" : "Exited (0)", Created: now - i * 60, Ports: [], Labels: { "com.docker.compose.project": project, "com.docker.compose.service": service } });
let ctrs = []; for (let p = 0; p < 12; p++) for (let s = 0; s < 6; s++) ctrs.push(C(`c${p}-${s}`, `proj${p}`, `svc${s}`, p * 6 + s));
const imgs = Array.from({ length: 300 }, (_, i) => ({ Id: `sha256:${i}`, RepoTags: [`repo${i}:latest`], Size: 1e7 * (i % 50 + 1), Created: now - i * 3600, Containers: 0 }));
const lists = () => kiri.on({ type: "lists", containers: ctrs, images: imgs, volumes: [{ Name: "v1", Driver: "local", Mountpoint: "/v/1" }], networks: [] });
const svc = (name) => ({ name, image: name + ":dev", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
const projects = () => kiri.on({ type: "composeprojects", logos: {}, projects: Array.from({ length: 12 }, (_, p) => { const m = ctrs.filter((c) => c.Labels["com.docker.compose.project"] === `proj${p}`);
  return { name: `proj${p}`, dir: `/w/p${p}`, files: [`/w/p${p}/compose.yaml`], readable: true, services: m.map((c) => svc(c.Labels["com.docker.compose.service"])),
    containers: m.map((c) => ({ service: c.Labels["com.docker.compose.service"], id: c.Id, name: c.Names[0].slice(1), image: c.Image, state: c.State, status: c.Status })) }; }) });
ui.vm = "running";
kiri.on({ type: "status", version: "0.1.4", dev: true, share: true, app: "Kiri", ours: "kiri", context: "kiri" });
lists(); projects();
const content = $("content"), tbl = () => content.querySelector("table[data-view]");
// What a refresh changes on screen, from a MutationObserver over the list.
function changes(fn) {
  const mo = new MutationObserver(() => {}); mo.observe(content, { subtree: true, childList: true, attributes: true, characterData: true, attributeOldValue: true });
  fn(); const rs = mo.takeRecords(); mo.disconnect();
  return rs.map((r) => ({ type: r.type, attr: r.attributeName, old: r.oldValue, now: r.attributeName && r.target.getAttribute(r.attributeName), node: r.target.nodeName, row: r.target.closest?.("tr")?.dataset.key ?? r.target.parentElement?.closest("tr")?.dataset.key ?? null }));
}
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const sizing = () => { const t = tbl(); return [t.classList.contains("sized"), [...t.tHead.rows[0].cells].map((th) => th.style.width)]; };

for (const v of ["images", "compose", "containers"]) {
  t(`${v}: the same list again changes nothing on screen`, () => {
    show(v); render();
    eq(changes(() => { lists(); projects(); render(); render(); }), [], "changes");
  });
}
t("images: their columns are sized (the rest of this test needs it)", () => { show("images"); eq(sizing()[0], true, "sized"); });
t("compose: one container stopping changes its row and its project's, and the sizing stays", () => {
  show("compose"); render();
  const before = sizing();
  const ch = changes(() => { ctrs = ctrs.map((c) => (c.Id === "c3-2" ? { ...c, State: "exited", Status: "Exited (0)" } : c)); lists(); projects(); render(); });
  eq(ch.filter((c) => c.node === "TABLE" || c.node === "TH" || (c.attr === "colspan")), [], "nothing on the table, its headings or a span");
  const rows = [...new Set(ch.map((c) => c.row))];
  eq(rows.every((r) => r === "c3-2" || r === null), true, "rows touched: " + JSON.stringify(rows));
  eq(content.querySelector('tr[data-key="c3-2"]').textContent.includes("Stopped"), true, "the row shows it");
  eq(sizing(), before, "sizing");
});
t("compose: folding a project takes its rows out and moves none of the rows below", () => {
  show("compose"); render();
  const mo = new MutationObserver(() => {}); mo.observe(content.querySelector("tbody"), { childList: true });
  click(content.querySelector('tr.group[data-group="proj3"] button.chev'));
  const rs = mo.takeRecords(); mo.disconnect();
  eq([rs.reduce((a, r) => a + r.removedNodes.length, 0), rs.reduce((a, r) => a + r.addedNodes.length, 0)], [6, 0], "rows taken out, rows put in");
  click(content.querySelector('tr.group[data-group="proj3"] button.chev'));
});
t("a list whose columns aren't sized keeps no widths from the one before", () => {
  show("images"); eq(sizing()[0], true, "images sized");
  show("volumes");
  const tv = tbl();
  eq([tv.dataset.view, tv.classList.contains("sized")], ["volumes", false], "volumes, not sized");
  eq([...tv.tHead.rows[0].cells].map((th) => th.style.width).filter(Boolean), [], "no widths left over");
});
// The list's width, changed by a panel beside it rather than the window: the columns fit again.
show("images"); render();
const w0 = tbl().tHead.rows[0].cells[1].style.width;
content.style.marginInlineEnd = "300px";
setTimeout(() => {
  t("the columns fit again when the list narrows without the window resizing", () => {
    const w1 = tbl().tHead.rows[0].cells[1].style.width;
    eq(parseFloat(w1) < parseFloat(w0), true, `first column ${w0} → ${w1}`);
  });
  content.style.marginInlineEnd = "";
  report(R);
}, 300);
