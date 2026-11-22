// parity: macos windows linux
// window: 1440x600
// Folding or unfolding a project keeps its row where it was, under the pointer, also near the end of the list
// (where folding leaves less to scroll, and the list was pulled down and the row away from the pointer). The gap
// that holds the bottom is gone once the list is scrolled back up past it, and on any other page.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const near = (a, b, m) => { if (Math.abs(a - b) > 1) throw new Error(`${m}: got ${a} want ${b}`); };
const now = Math.floor(Date.now() / 1000);
const C = (id, project, service, ago) => ({ Id: id, Names: [`/${project}-${service}-1`], Image: `${service}:dev`, State: "running", Status: "Up 1 hour",
  Created: now - ago, Ports: [], Labels: { "com.docker.compose.project": project, "com.docker.compose.service": service } });
const shop = Array.from({ length: 14 }, (_, i) => C(`s${i}`, "shop", `svc${i}`, 600 + i));
const ctrs = [C("a1", "alpha", "web", 100), C("a2", "alpha", "db", 120), ...shop, C("z1", "store", "web", 900), C("z2", "store", "db", 950)];
const svc = (name) => ({ name, image: name + ":dev", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
const proj = (name) => { const m = ctrs.filter((c) => c.Labels["com.docker.compose.project"] === name);
  return { name, dir: `/w/${name}`, files: [`/w/${name}/compose.yaml`], readable: true, services: m.map((c) => svc(c.Labels["com.docker.compose.service"])),
    containers: m.map((c) => ({ service: c.Labels["com.docker.compose.service"], id: c.Id, name: c.Names[0].slice(1), image: c.Image, state: c.State, status: c.Status })) }; };
ui.vm = "running";
kiri.on({ type: "status", version: "0.1.4", dev: true, share: true, app: "Kiri", ours: "kiri", context: "kiri" });
kiri.on({ type: "lists", containers: ctrs, images: [], volumes: [], networks: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: ["alpha", "shop", "store"].map(proj) });
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const content = $("content"), grp = (p) => content.querySelector(`tr.group[data-group="${CSS.escape(p)}"]`);
const rowTop = (p) => grp(p).getBoundingClientRect().top, hold = () => parseFloat(content.style.getPropertyValue("--hold")) || 0;
const scroll = (y) => { content.scrollTop = y; content.dispatchEvent(new Event("scroll")); };
const bottom = () => scroll(content.scrollHeight);

// Shop's row 80px below the window's top, scrolled as far as that goes.
const endWithRowInView = () => { scroll(content.scrollHeight); scroll(content.scrollTop + rowTop("shop") - content.getBoundingClientRect().top - 80); };
show("compose");
t("the list scrolls (the test needs a project taller than the window)", () => {
  eq(content.scrollHeight > content.clientHeight + 400, true, "scrolls");
});
t("folding a scrolled list: the row stays put and a gap holds the bottom", () => {
  endWithRowInView();
  const before = rowTop("shop");
  click(grp("shop").querySelector("button.chev"));
  eq(grp("shop").getAttribute("aria-expanded"), "false", "folded");
  near(rowTop("shop"), before, "the row's place");
  eq(hold() > 0, true, "the list now shorter than the window, so a gap holds the bottom");
});
t("scrolling back up past the gap drops it, once (not resized on every scroll)", () => {
  const h = hold();
  scroll(content.scrollTop - 10);
  eq(hold(), h, "still in view: left as it is");
  scroll(0);
  eq([hold(), content.dataset.hold ?? null], [0, null], "gone at the top");
});
t("unfolding keeps the row in place too", () => {
  scroll(content.scrollHeight);
  const before = rowTop("shop");
  click(grp("shop").querySelector("button.chev"));
  eq(grp("shop").getAttribute("aria-expanded"), "true", "open");
  near(rowTop("shop"), before, "the row's place");
});
t("a gap is gone on another page", () => {
  endWithRowInView();
  click(grp("shop").querySelector("button.chev"));
  eq(hold() > 0, true, "a gap");
  show("images");
  eq([hold(), content.dataset.hold ?? null], [0, null], "Images");
  show("compose");
  eq(hold(), 0, "back in Compose");
});
report(R);
