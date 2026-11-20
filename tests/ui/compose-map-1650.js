// parity: macos windows linux
// window: 1200x1650
// The Structure box fills the page down to the window's bottom, so the page
// doesn't scroll; Fit fits the box it really has. A long service name ends in
// … before its state does: the state stays readable, whole in its title. Fit
// never shrinks many services in one column past readable.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
const P = "customer-analytics-and-reporting-platform-staging";
const LONG = "a-service-with-a-name-that-goes-on-and-on-and-on-for-a-very-long-way-until-it-reaches-one-hundred-ch".slice(0, 100);
const names = [LONG, "web", "db", "cache"];
const status = { [LONG]: ["running", "Up 1 hour"], web: ["restarting", "Restarting (1) 3 seconds ago"], db: ["exited", "Exited (1) 2 minutes ago"], cache: ["running", "Up 1 hour"] };
kiri.on({ type: "lists", containers: names.map((n) => ({ Id: "id-" + n.length + n[0], Names: [`/${P}-${n}-1`], Image: n, State: status[n][0], Status: status[n][1], Created: 1, Ports: [], Labels: { "com.docker.compose.project": P, "com.docker.compose.service": n } })), images: [], volumes: [], networks: [] });
const svc = (name, depends = []) => ({ name, image: "x:1", build: "", ports: [], volumes: [], depends, networks: ["default"], kinds: [] });
const proj = (services) => ({ type: "composeprojects", logos: {}, projects: [{ name: P, dir: "/w/p", files: ["/w/p/compose.yaml"], readable: true, updated: 1, services,
  containers: services.map((s) => ({ service: s.name, id: "id-" + s.name.length + s.name[0], name: `${P}-${s.name}-1`, image: "x", state: status[s.name]?.[0] || "running", status: status[s.name]?.[1] || "Up" })) }] });
kiri.on(proj([svc(LONG, ["db"]), svc("web", ["db", "cache"]), svc("db"), svc("cache")]));
show("compose"); cmpOpen(P);
t("the Structure box reaches the window's bottom and the page doesn't scroll", () => {
  const c = $("content"), m = c.querySelector(".map"), r = m.getBoundingClientRect(), cr = c.getBoundingClientRect();
  eq([c.scrollHeight <= c.clientHeight + 1, r.height >= 360, cr.bottom - r.bottom < 40], [true, true, true], `map ${Math.round(r.height)} px, ${Math.round(cr.bottom - r.bottom)} px from the bottom, page ${c.scrollHeight}/${c.clientHeight}`);
  const foot = m.querySelector(".mapfoot").getBoundingClientRect();
  eq(foot.bottom <= r.bottom + 1, true, "the Network/Volumes footer is inside it");
});
t("Fit uses the box it has", () => {
  const vp = $("cmp-vp"), sz = vp.querySelector(".sizer").getBoundingClientRect();
  eq([sz.width <= vp.clientWidth, sz.height <= vp.clientHeight], [true, true], "the whole map shows");
});
t("a 100-character name ends in … and the state stays readable, whole in its title", () => {
  const card = document.querySelector(`.card[data-cmpcard="${LONG}"]`), nm = card.querySelector(".nmt"), pill = card.querySelector(".nm .pill");
  eq([getComputedStyle(nm).textOverflow, nm.scrollWidth > nm.clientWidth], ["ellipsis", true], "the name is cut with …");
  eq([pill.scrollWidth <= pill.clientWidth + 1, pill.textContent], [true, "Up 1 h"], "the state whole");
  eq(pill.title.length > 0, true, "the full state in its title");
});
t("a restarting service says Restarting on its card, the rest in its title", () => {
  const pill = document.querySelector('.card[data-cmpcard="web"] .nm .pill');
  eq([pill.textContent, pill.scrollWidth <= pill.clientWidth + 1, pill.title.includes("Restarting")], ["Restarting", true, true], "short and whole");
  const db = document.querySelector('.card[data-cmpcard="db"] .nm .pill');
  eq([db.textContent, db.scrollWidth <= db.clientWidth + 1], ["Failed (1)", true], "its exit, whole");
});
t("Fit keeps fourteen services in one column readable: the map scrolls instead", () => {
  const many = Array.from({ length: 14 }, (_, i) => svc("svc" + i));
  kiri.on(proj(many)); render();
  const z = +(/scale\(([\d.]+)\)/.exec(document.querySelector("#cmp-vp .canvas").style.transform)?.[1] || 0);
  eq(z >= 0.6, true, "zoom " + z);
});
report(R);
