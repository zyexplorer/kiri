// parity: macos windows linux
// window: 1280x800
// Settings › Diagnostics › Logs in a wide window is as it always was: time, level, part
// and message side by side in columns 84, 38 and 46 px wide.
kiri.on({ type: "status", version: "0.2.2", app: "Kiri", ours: "kiri", context: "kiri" });
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
prefs.tab = "diagnostics"; show("settings");
kiri.on({ type: "diaglogs", part: "", op: "", lines: [{ time: "2026-11-20T09:41:00Z", level: "warn", part: "daemon", msg: "pull of postgres:16 is slow, retrying the layer", fields: {} }] });
t("one line, four columns", () => {
  const row = document.querySelector("#dg-log .dg-row"), b = (s) => row.querySelector(s).getBoundingClientRect();
  eq(new Set([".t", ".lv", ".p", ".m"].map((s) => Math.round(b(s).top))).size, 1, "lines");
  eq([".t", ".lv", ".p"].map((s) => Math.round(b(s).width)), [84, 38, 46], "column widths");
});
report(R);
