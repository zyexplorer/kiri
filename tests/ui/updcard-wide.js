// parity: macos windows linux
// window: 1280x800
// In a wide window the update card is as it always was: 352 px wide, 16 px from the
// window's end, with Download, Snooze and Skip this version on one line. Snooze's two
// choices (wider) wrap inside it.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
kiri.on({ type: "status", version: "0.2.2", app: "Kiri", ours: "kiri", context: "kiri" });
kiri.on({ type: "update", manual: true, current: "0.2.2", release: { version: "0.2.3", notes: "## New\n- Ports", notes_url: "" } });
const box = (s) => document.querySelector(s).getBoundingClientRect();
t("the card is 352 px wide, 16 px from the end", () => {
  const c = box("#updcard .updcard");
  eq([Math.round(c.width), Math.round(innerWidth - c.right)], [352, 16], "width, gap");
});
t("its buttons are on one line, Skip this version at the end", () => {
  const row = box("#updcard .row"), tops = [...document.querySelectorAll("#updcard .row > button")].map((b) => Math.round(b.getBoundingClientRect().top + b.getBoundingClientRect().height / 2));
  eq(new Set(tops).size, 1, "lines (button middles " + tops + ")");
  eq(Math.round(box("#updcard .skip").right), Math.round(row.right), "Skip's end");
  eq(Math.round(row.height), 32, "row height");
});
t("Snooze's two choices stay inside the card, Skip this version too", () => {
  updateAction("snooze");
  const c = box("#updcard .updcard"), out = [...document.querySelectorAll("#updcard button")].filter((b) => { const r = b.getBoundingClientRect(); return r.left < c.left - 0.5 || r.right > c.right + 0.5; });
  eq(out.map((b) => b.textContent.trim() || b.getAttribute("aria-label")), [], "outside the card");
});
report(R);
