// parity: macos windows linux
// Container logs: selectable text, scoped Ctrl/⌘A, and Copy — same on every OS.
kiri.on({ type: "status", version: "0.1.4", dev: true, app: "Kiri Dev", ours: "kiri-dev", context: "kiri-dev" });
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const item = (name) => R.push("ITEM " + name);

show("containers");
openDetail("a1");
setTab("logs");
kiri.on({ type: "logs", id: "a1", parts: [
  { e: false, t: "2026-11-10T01:00:00Z INFO hello from line one\n" },
  { e: false, t: "2026-11-10T01:00:01Z WARN second line stays\n" },
] });
// Follow off: copying must work without live scroll.
ui.detail.follow = false;
const box = $("log-follow"); if (box) box.checked = false;

t("the log console allows text selection", () => {
  const cs = getComputedStyle(logEl);
  eq([cs.userSelect || cs.webkitUserSelect, logEl.isConnected], ["text", true], "select");
});
item("log console text selection");

t("a lists refresh keeps the same log node mounted", () => {
  const before = logEl;
  const line = logEl.firstElementChild;
  kiri.on({ type: "lists", containers: ui.containers, images: [], volumes: [], networks: [] });
  eq([logEl === before, $("dbody")?.contains(logEl), line?.isConnected, line?.dataset.raw?.includes("hello")], [true, true, true, true], "kept");
});
item("log console survives list refresh");

t("Copy sends the visible lines through the large clipboard path", () => {
  sent.length = 0;
  document.querySelector("[data-logcopy]").click();
  const copy = sent.find((x) => x.type === "term" && x.name === "copy");
  eq(!!copy, true, "sent");
  eq(/hello from line one/.test(copy.value) && /second line stays/.test(copy.value), true, "text");
});
item("log Copy button");

t("select-all stays inside the log console", () => {
  kiri.on({ type: "logs", id: "a1", parts: [
    { e: false, t: "line-a for select-all\n" },
    { e: false, t: "line-b for select-all\n" },
  ] });
  selectAllLogs();
  const sel = getSelection();
  eq([!!sel && !sel.isCollapsed, logEl.contains(sel.anchorNode), logEl.contains(sel.focusNode)], [true, true, true], "in console");
  eq(/line-a for select-all/.test(String(sel)) && /line-b for select-all/.test(String(sel)), true, "both lines");
  eq(String(sel).includes("Find in logs") || String(sel).includes("All levels") || String(sel).includes("Follow"), false, "no toolbar");
});
item("log select-all scoped to console");

t("primary+A / primary+C use Ctrl on Windows and Linux, ⌘ on the Mac", () => {
  const useCtrl = WIN || LINUX;
  // Wrong modifier must not select-all or copy.
  sent.length = 0;
  getSelection().removeAllRanges();
  logEl.dispatchEvent(new KeyboardEvent("keydown", {
    key: "a", bubbles: true, cancelable: true,
    ctrlKey: !useCtrl, metaKey: useCtrl,
  }));
  eq(getSelection().isCollapsed, true, "wrong modifier ignored");

  document.dispatchEvent(new KeyboardEvent("keydown", {
    key: "a", bubbles: true, cancelable: true,
    ctrlKey: useCtrl, metaKey: !useCtrl,
  }));
  const sel = getSelection();
  eq([!sel.isCollapsed, logEl.contains(sel.anchorNode)], [true, true], "primary+A selects logs");

  sent.length = 0;
  document.dispatchEvent(new KeyboardEvent("keydown", {
    key: "c", bubbles: true, cancelable: true,
    ctrlKey: useCtrl, metaKey: !useCtrl,
  }));
  const copy = sent.find((x) => x.type === "term" && x.name === "copy");
  eq(!!copy && /line-a for select-all/.test(copy.value || ""), true, "primary+C copies");
});
item("log primary+A and primary+C");

t("Clear empties the console", () => {
  document.querySelector("[data-logclear]").click();
  eq(logEl.childElementCount, 0, "empty");
});

report(R);
