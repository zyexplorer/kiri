// parity: macos windows linux
// window: 1200x1650
// The Compose file tab and the Logs tab fill the window down to its bottom,
// as the Structure box does (at least 360 px): the outline, the editor and
// the plan stretch, the editor and the log scroll inside themselves, and the
// page doesn't scroll. In a narrow window the plan sits under the editor, the
// editor keeps a sensible height, and the page may scroll.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
kiri.on({ type: "lists", containers: [{ Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": "shop", "com.docker.compose.service": "db" } }], images: [], volumes: [], networks: [] });
const services = [{ name: "db", image: "postgres:16", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1, services, containers: [{ service: "db", id: "d1", name: "shop-db-1", image: "postgres:16", state: "running", status: "Up" }] }] });
show("compose"); cmpOpen("shop");
const text = "services:\n" + Array.from({ length: 80 }, (_, i) => `  s${i}:\n    image: alpine\n`).join("");
kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text, hash: "h1", mtime: 1 });
const narrow = innerWidth <= 900;
const fills = (el, what) => {
  const c = $("content"), r = el.getBoundingClientRect(), cr = c.getBoundingClientRect();
  eq([r.height >= 360, cr.bottom - r.bottom < 40, c.scrollHeight <= c.clientHeight + 1], [true, true, true], `${what} ${Math.round(r.height)} px, ${Math.round(cr.bottom - r.bottom)} px from the bottom, page ${c.scrollHeight}/${c.clientHeight}`);
};
t("the file tab fills the window; the editor scrolls inside itself", () => {
  click(document.querySelector('[data-cmptab="file"]'));
  const ta = $("cmp-ta"), ed = document.querySelector(".cmp-file .ed");
  if (narrow) eq([ed.getBoundingClientRect().height >= 300, ta.scrollHeight > ta.clientHeight], [true, true], `editor ${Math.round(ed.getBoundingClientRect().height)} px`);
  else {
    fills(document.querySelector(".cmp-file"), "file tab");
    const f = document.querySelector(".cmp-file").getBoundingClientRect();
    for (const sel of [".outline", ".ed", ".plan"]) {
      const r = document.querySelector(".cmp-file " + sel).getBoundingClientRect();
      eq(f.bottom - r.bottom < 4, true, `${sel} reaches the bottom: ${Math.round(r.bottom)} of ${Math.round(f.bottom)}`);
    }
    eq(ta.scrollHeight > ta.clientHeight, true, "the editor scrolls itself");
  }
});
t("the Logs tab fills the window; the log scrolls inside itself", () => {
  click(document.querySelector('[data-cmptab="logs"]'));
  kiri.on({ type: "composelogs", name: "shop", lines: Array.from({ length: 300 }, (_, i) => ({ svc: "db", t: "2026-11-16T13:02:01.5Z", text: "line " + i })) });
  const box = document.querySelector(".cmp-logs");
  fills(box, "logs");
  eq(box.scrollHeight > box.clientHeight, true, "the log scrolls itself");
});
report(R);
