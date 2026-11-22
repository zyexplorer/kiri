// parity: macos windows linux
// window: 900x640
// Twenty-four services with real dependencies (levels of 1, 2, 1, 4, 12 and 4):
// after Fit the whole map fits, or Fit stops where names are still readable
// (9 px) and the box scrolls; a level taller than the box wraps into columns,
// and the box shows whole cards, never an empty box or one lone card.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
ui.vm = "running";
const P = "shop";
const D = { db: [], auth: ["db"], payments: ["db"], billing: ["db", "payments"], cache: [], cart: ["db"], catalog: ["db"], checkout: ["db", "payments"], queue: [], indexer: ["queue"], search: ["indexer"],
  api: ["auth", "billing", "cache", "cart", "catalog", "checkout", "db", "queue", "search"], admin: ["api"], analytics: ["cache", "db"], web: ["api"], gateway: ["admin", "api", "web"], inventory: ["db"],
  mailer: ["queue"], metrics: [], notifier: ["queue"], reports: ["cache", "db"], worker: ["queue"], scheduler: ["worker"], shipping: ["db"] };
const names = Object.keys(D);
kiri.on({ type: "lists", containers: names.map((n) => ({ Id: "id-" + n, Names: [`/${P}-${n}-1`], Image: "x", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": P, "com.docker.compose.service": n } })), images: [], volumes: [], networks: [] });
const svc = (name) => ({ name, image: "alpine:3.20", build: "", ports: [], volumes: [], depends: D[name], networks: ["default"], kinds: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: P, dir: "/w/p", files: ["/w/p/compose.yaml"], readable: true, updated: 1,
  services: names.map(svc), containers: names.map((n) => ({ service: n, id: "id-" + n, name: `${P}-${n}-1`, image: "x", state: "running", status: "Up" })) }] });
show("compose"); cmpOpen(P);
document.querySelector('[data-cmpz="fit"]').dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const vp = $("cmp-vp"), v = vp.getBoundingClientRect(), z = +(/scale\(([\d.]+)\)/.exec(vp.querySelector(".canvas").style.transform)?.[1] || 0);
const cards = [...vp.querySelectorAll(".card")], rs = cards.map((c) => c.getBoundingClientRect());
const name = parseFloat(getComputedStyle(cards[0].querySelector(".nmt")).fontSize) * z;
t("names stay readable: at least 9 px", () => eq(name >= 9 - 0.01, true, `name ${name.toFixed(2)} px at zoom ${z}`));
t("the map fits, or Fit stops at readable and the box scrolls", () => {
  const sz = vp.querySelector(".sizer").getBoundingClientRect(), fits = sz.width <= vp.clientWidth + 1 && sz.height <= vp.clientHeight + 1;
  const floor = name < 9.5, scrolls = vp.scrollWidth > vp.clientWidth || vp.scrollHeight > vp.clientHeight;
  eq(fits || (floor && scrolls), true, `zoom ${z}, map ${Math.round(sz.width)}x${Math.round(sz.height)} in ${vp.clientWidth}x${vp.clientHeight}`);
});
t("no level is taller than the box: a tall one wraps into columns", () => {
  const cols = new Map(); rs.forEach((r) => { const x = Math.round(r.left), c = cols.get(x) || { top: r.top, bottom: r.bottom, n: 0 }; c.top = Math.min(c.top, r.top); c.bottom = Math.max(c.bottom, r.bottom); c.n++; cols.set(x, c); });
  const tall = [...cols.values()].filter((c) => c.bottom - c.top > vp.clientHeight + 1).map((c) => c.n);
  eq(tall, [], `columns taller than the ${vp.clientHeight} px box (cards in each)`);
});
t("the box shows whole cards, not an empty box or one lone card", () => {
  const whole = rs.filter((r) => r.left >= v.left - 1 && r.right <= v.left + vp.clientWidth + 1 && r.top >= v.top - 1 && r.bottom <= v.top + vp.clientHeight + 1).length;
  eq(whole >= 3, true, `${whole} whole cards in view`);
});
t("the box ends inside the window", () => eq(v.top + vp.clientHeight <= innerHeight + 1, true, `box bottom ${Math.round(v.top + vp.clientHeight)} in ${innerHeight}`));
report(R);
