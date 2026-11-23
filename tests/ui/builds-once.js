// Two builders that share one BuildKit (kiri and kiri-build) each report its builds:
// the Builds list shows each build once, and only one row is selected.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const at = new Date(Date.now() - 3 * 86400e3).toISOString(), end = new Date(Date.now() - 3 * 86400e3 + 93e3).toISOString();
const build = (builder, id, name) => ({ builder, id, name, status: "Completed", started: at, completed: end, steps: 12, done: 12, cached: 4 });
show("builds");
kiri.on({ type: "builds", builds: [build("kiri", "b1", "portal"), build("kiri-build", "b1", "portal"), build("kiri", "b2", "soc (siem)"), build("kiri-build", "b2", "soc (siem)"), build("kiri", "b3", "docs")] });
const rows = () => [...document.querySelectorAll(".bl-item")];
t("a build two builders report is listed once", () => {
  eq(rows().map((r) => r.querySelector("b").textContent), ["portal", "soc (siem)", "docs"], "rows");
  eq(document.getElementById("n-builds").textContent, "3", "the count in the sidebar");
});
t("one row is selected, and picking another moves the selection", () => {
  eq(rows().filter((r) => r.getAttribute("aria-selected") === "true").length, 1, "selected rows");
  rows()[1].click();
  eq(rows().map((r) => r.getAttribute("aria-selected")), ["false", "true", "false"], "after picking the second");
});
t("the build's details are asked from the first builder that reported it", () => {
  eq(sent.filter((s) => s.type === "build").map((s) => [s.name, s.id]), [["kiri", "b1"], ["kiri", "b2"]], "asked");
});
report(R);
