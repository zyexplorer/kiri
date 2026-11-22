// parity: macos windows linux
// A container's log can be selected and copied, without the line numbers.
kiri.on({ type: "status", version: "0.1.4", dev: true, app: "Kiri Dev", ours: "kiri-dev", context: "kiri-dev" });
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
show("containers"); openDetail("a1"); setTab("logs"); render();
const d = () => ui.detail;
kiri.on({ type: "logs", id: "a1", parts: [{ e: false, t: "[INFO] Started worker-1\n" }, { e: true, t: "ERROR: engine failed\n" }] });
t("selecting the log copies its lines, without the numbers", () => {
  const log = document.querySelector(".console.numbered");
  getSelection().selectAllChildren(log);
  eq(getSelection().toString().trim().split("\n"), ["[INFO] Started worker-1", "ERROR: engine failed"], "copied");
});
const MAC = !WIN && !LINUX;
const copyKey = (el) => el.dispatchEvent(new KeyboardEvent("keydown", { key: "c", metaKey: MAC, ctrlKey: !MAC, bubbles: true }));
t("the copy key sends the selected log to the clipboard, or leaves it to the Mac's Edit menu", () => {
  const log = document.querySelector(".console.numbered");
  getSelection().selectAllChildren(log);
  sent.length = 0;
  copyKey(log);
  const copies = sent.filter((m) => m.cmd === "term" && m.name === "copy");
  eq(copies.length, MAC ? 0 : 1, "copies sent");
  if (!MAC) eq(copies[0].value.trim().split("\n"), ["[INFO] Started worker-1", "ERROR: engine failed"], "text");
});
t("the copy key leaves text fields and an empty selection alone", () => {
  sent.length = 0;
  getSelection().removeAllRanges();
  copyKey(document.body);
  const field = document.querySelector("input");
  getSelection().selectAllChildren(document.querySelector(".console.numbered"));
  copyKey(field);
  eq(sent.filter((m) => m.cmd === "term" && m.name === "copy").length, 0, "copies sent");
});
t("the search field's text can be selected, though the window's can't", () => {
  const style = getComputedStyle(document.querySelector(".gfind input"));
  eq(style.userSelect || style.webkitUserSelect, "text", "user-select");
});
t("the level menu is a button whose short list picks a level, as the native menu did", () => {
  ssScan();
  const sel = document.getElementById("log-level"), b = sel.ssb;
  if (!b || getComputedStyle(sel).display !== "none") throw new Error("the level menu is still a native select");
  b.click();
  const pop = document.querySelector(".sspop");
  if (!pop || pop.querySelector("input")) throw new Error("no list, or a search box for five levels");
  const errors = [...pop.querySelectorAll('[role="option"]')].find((r) => r.textContent === tr("Error"));
  errors.click();
  eq([sel.value, d().logLevel, b.querySelector(".ssv").textContent, !!document.querySelector(".sspop")], ["error", "error", tr("Error"), false], "picked");
  const lines = [...document.querySelectorAll(".console.numbered .ln, .console.numbered > *")].filter((l) => l.offsetParent && l.textContent.trim()).map((l) => l.textContent);
  if (lines.some((l) => l.includes("[INFO]"))) throw new Error("info lines still show: " + lines.join(" | "));
});
report(R);
