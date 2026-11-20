// parity: macos windows linux
// A file broken in Kiri, applied (it fails after saving) and then put right by
// hand: once it runs as saved, the failed Apply's note goes; put right but
// still not what runs, it reads as any saved change not applied yet.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
kiri.on({ type: "lists", containers: [{ Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": "shop", "com.docker.compose.service": "db" } }], images: [], volumes: [], networks: [] });
const services = [{ name: "db", image: "postgres:16", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1, services, containers: [{ service: "db", id: "d1", name: "shop-db-1", image: "postgres:16", state: "running", status: "Up" }] }] });
show("compose");
const F = (text, hash, o = {}) => kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text, hash, mtime: 1, ...o });
const TEXT = "services:\n  db:\n    image: postgres:16\n", BAD = TEXT + "    ports: ['99999:80']\n";
const state = () => $("cmp-edstate").textContent;
const fail = () => {
  cmpOpen("shop"); F(TEXT, "h1"); click(document.querySelector('[data-cmptab="file"]'));
  const ta = $("cmp-ta"); ta.value = BAD; ta.dispatchEvent(new Event("input"));
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["ports"] }], services });
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, saved: true, hash: "h2", error: "invalid hostPort: 99999" });
  F(BAD, "h2", { restore: true });
  eq(state(), "Saved, not applied: invalid hostPort: 99999", "the failure is told");
};
t("put right by hand as it runs: the note goes, and Apply has nothing to do", () => {
  fail();
  F(TEXT + "# fixed\n", "h3", { restore: true });
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "same", what: [] }], services, runs: true });
  eq([state(), cmpUnapplied(), document.querySelector("[data-cmpapply]").disabled, !!document.querySelector("[data-cmprestore]")], ["Saved, matches what runs", null, true, false], "settled");
});
t("put right by hand but not as it runs: a saved change not applied, not the old failure", () => {
  fail();
  F(TEXT.replace("16", "17"), "h4", { restore: true });
  eq(state().includes("99999"), false, "no stale reason before the plan: " + state());
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:17" }], services, runs: false });
  eq([state(), document.querySelector("[data-cmpapply]").disabled], ["Saved, not applied", false], "pending");
});
report(R);
