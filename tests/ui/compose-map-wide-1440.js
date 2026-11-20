// parity: macos windows linux
// window: 1440x900
// Twenty-four services seven levels deep (1, 2, 1, 4, 6, 6 and 4): a map a
// little wider than the box at readable size, with half its height free. Fit
// fits it whole: every card in view, none cut at the right edge.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
ui.vm = "running";
const P = "big";
const L4 = ["inventory", "audit-log", "recommendations", "payments", "webhooks", "exports-worker"], L3 = ["billing", "auth-service", "search-indexer", "catalog"], L5 = ["reports", "sms-gateway", "email-sender", "admin-console", "web-frontend", "notifications"];
const D = { postgres: [], migrations: ["postgres"], pgbouncer: ["postgres"], "core-api": ["migrations", "pgbouncer"] };
L3.forEach((n) => D[n] = ["core-api"]);
L4.forEach((n, i) => D[n] = [L3[i % 4]]);
L5.forEach((n, i) => D[n] = [L4[i], L4[(i + 1) % 6]]);
["public-docs", "edge-gateway", "synthetic-monitor", "status-page"].forEach((n, i) => D[n] = [L5[i], L5[i + 2]]);
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
t("Fit fits the whole map: every card whole in the box", () => {
  const cut = rs.filter((r) => !(r.left >= v.left - 1 && r.right <= v.left + vp.clientWidth + 1 && r.top >= v.top - 1 && r.bottom <= v.top + vp.clientHeight + 1)).length;
  eq([cut, vp.scrollWidth <= vp.clientWidth + 1], [0, true], `${cards.length} cards at zoom ${z}, map ${vp.scrollWidth} in ${vp.clientWidth}`);
});
report(R);
