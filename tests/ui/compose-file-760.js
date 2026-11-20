// parity: windows linux
// window: 760x520
// The Compose file tab in a narrow window: the editor takes the page's width,
// the plan sits under it, and nothing is blank or over anything else.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const apart = (a, b) => { const x = a.getBoundingClientRect(), y = b.getBoundingClientRect(); return x.right <= y.left + 1 || y.right <= x.left + 1 || x.bottom <= y.top + 1 || y.bottom <= x.top + 1; };
const L = (svc) => ({ "com.docker.compose.project": "shop", "com.docker.compose.service": svc });
ui.vm = "running";
const names = ["web", "api", "db"];
kiri.on({ type: "lists", containers: names.map((n) => ({ Id: n + "1", Names: [`/shop-${n}-1`], Image: n, State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: L(n) })), images: [], volumes: [], networks: [] });
const svc = (name) => ({ name, image: name + ":latest", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1,
  services: names.map(svc), containers: names.map((n) => ({ service: n, id: n + "1", name: `shop-${n}-1`, image: n, state: "running", status: "Up" })) }] });
show("compose"); cmpOpen("shop");
kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n" + names.map((n) => `  ${n}:\n    image: ${n}:latest\n`).join(""), hash: "h", mtime: 1 });
click(document.querySelector('[data-cmptab="file"]'));
const ta = $("cmp-ta"); ta.value = ta.value.replace("web:latest", "web:2"); ta.dispatchEvent(new Event("input"));
kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "web", act: "chg", what: ["image"], from: "web:latest", to: "web:2" }, { n: "api", act: "same", what: [] }], services: [] });
t("the editor takes the page's width", () => {
  const file = document.querySelector(".cmp-file").getBoundingClientRect(), ed = document.querySelector(".cmp-edcol .ed").getBoundingClientRect();
  eq([ed.width >= file.width * 0.9, ed.width > 400, ed.height > 200], [true, true, true], `editor ${Math.round(ed.width)}x${Math.round(ed.height)} in ${Math.round(file.width)}`);
  eq(getComputedStyle($("cmp-outline")).display, "none", "no outline");
});
t("nothing is blank: the text area and the plan have room, and the plan shows its steps", () => {
  const r = ta.getBoundingClientRect(), p = $("cmp-plan").getBoundingClientRect();
  eq([r.width > 300, r.height > 150, p.width > 300, p.height > 100, !!document.querySelector("#cmp-plan li.a-chg")], [true, true, true, true, true], "sizes");
  eq(document.querySelector("#cmp-plan li.a-chg").getBoundingClientRect().height > 0, true, "a step shows");
});
t("nothing over anything else", () => {
  eq([apart(document.querySelector(".cmp-edcol"), $("cmp-plan")), apart($("cmp-edstate"), $("cmp-plan"))], [true, true], "editor and plan apart");
  const c = $("content").getBoundingClientRect(), f = document.querySelector(".cmp-file").getBoundingClientRect();
  eq([f.left >= c.left - 1, f.right <= c.right + 1], [true, true], "inside the page");
});
report(R);
