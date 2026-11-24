// parity: macos windows linux
// Projects editor: select-all / copy / cut / Tab work despite the transparent overlay.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const MOD = WIN || LINUX ? { ctrlKey: true } : { metaKey: true };
const key = (el, k, o = {}) => el.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true, ...o }));

kiri.on({ type: "status", version: "0.1.4", dev: true, app: "Kiri Dev", ours: "kiri-dev", context: "kiri-dev" });
kiri.on({
  type: "projects",
  projects: [{ id: "p1", name: "demo", summary: "", root: "/tmp/demo", source: "workspace", icon: "folder" }],
});
show("projects");
projOpen("p1");
kiri.on({
  type: "projecttree",
  id: "p1",
  entries: [{ name: "app.py", path: "app.py", dir: false }],
});
kiri.on({ type: "projectfile", id: "p1", path: "app.py", text: "print('hi')\nprint('bye')\n" });

const ta = () => document.getElementById("proj-editor");

t("the project editor opts into text selection", () => {
  const el = ta();
  if (!el) throw new Error("no editor");
  const style = getComputedStyle(el);
  eq(style.userSelect || style.webkitUserSelect, "text", "user-select");
});

t("Ctrl/Cmd+A selects the whole file", () => {
  const el = ta();
  el.focus();
  el.setSelectionRange(0, 0);
  key(el, "a", MOD);
  eq([el.selectionStart, el.selectionEnd], [0, el.value.length], "all selected");
});

t("copy from the transparent overlay puts the real text on the clipboard", () => {
  const el = ta();
  el.focus();
  el.setSelectionRange(0, 5);
  let got = null;
  const onCopy = (ev) => { got = ev.clipboardData.getData("text/plain"); };
  el.addEventListener("copy", onCopy);
  const ev = new ClipboardEvent("copy", { bubbles: true, cancelable: true, clipboardData: new DataTransfer() });
  el.dispatchEvent(ev);
  el.removeEventListener("copy", onCopy);
  eq(got, el.value.slice(0, 5), "copied slice");
  eq(ev.defaultPrevented, true, "overlay handler took it");
});

t("Tab indents instead of leaving the field", () => {
  const el = ta();
  el.value = "x";
  el.setSelectionRange(0, 0);
  key(el, "Tab");
  eq(el.value.startsWith("  "), true, "two spaces");
});

t("cut removes the selection and keeps the text for paste", () => {
  const el = ta();
  el.value = "abcdef";
  el.focus();
  el.setSelectionRange(2, 5);
  let got = null;
  const onCut = (ev) => { got = ev.clipboardData.getData("text/plain"); };
  el.addEventListener("cut", onCut);
  const ev = new ClipboardEvent("cut", { bubbles: true, cancelable: true, clipboardData: new DataTransfer() });
  el.dispatchEvent(ev);
  el.removeEventListener("cut", onCut);
  eq(got, "cde", "cut text");
  eq(el.value, "abf", "remaining");
});

report(R);
