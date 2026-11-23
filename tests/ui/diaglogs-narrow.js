// parity: macos windows linux
// window: 640x520
// Settings › Diagnostics › Logs in a narrow window: each line's time, level and part stay
// on top and its message takes a full-width line of its own under them, instead of a
// column a few words wide.
kiri.on({ type: "status", version: "0.2.2", app: "Kiri", ours: "kiri", context: "kiri" });
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
prefs.tab = "diagnostics"; show("settings");
const base = Date.parse("2026-11-20T09:41:00Z");
kiri.on({ type: "diaglogs", part: "", op: "", lines: Array.from({ length: 16 }, (_, i) => ({ time: new Date(base + i * 2317).toISOString(), level: ["info", "warn", "error", "debug"][i % 4], part: ["daemon", "ui", "cli"][i % 3],
  msg: "container shop-db-1 exited with code 1: could not bind to port 5432, the address is already in use", fields: i % 2 ? { container: "shop-db-1" } : {} })) });
const rows = () => [...document.querySelectorAll("#dg-log .dg-row")];
t("16 lines", () => { if (rows().length !== 16) throw new Error(rows().length + " rows"); });
t("each message is under its time, as wide as the line", () => {
  const bad = [];
  for (const row of rows()) {
    const r = row.getBoundingClientRect(), tm = row.querySelector(".t").getBoundingClientRect(), m = row.querySelector(".m").getBoundingClientRect();
    const inner = r.width - 24; // the row's padding
    if (m.top < tm.bottom - 0.5) bad.push(`message beside the time (${Math.round(m.top)} < ${Math.round(tm.bottom)})`);
    if (m.width < inner - 1) bad.push(`message ${Math.round(m.width)} px of ${Math.round(inner)}`);
  }
  if (bad.length) throw new Error(`${innerWidth} px: ${[...new Set(bad)].slice(0, 3).join("; ")}`);
});
t("time, level and part stay on one line", () => {
  for (const row of rows()) {
    const tops = [".t", ".lv", ".p"].map((s) => Math.round(row.querySelector(s).getBoundingClientRect().top));
    if (new Set(tops).size !== 1) throw new Error("tops " + tops);
  }
});
report(R);
