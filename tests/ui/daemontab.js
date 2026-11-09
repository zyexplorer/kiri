// Settings → Network: daemon.json textarea indents with Tab / Shift+Tab.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (a !== b) throw new Error((m || "eq") + ": " + JSON.stringify(a) + " !== " + JSON.stringify(b)); };

ui.proxy = {
  settings: { mode: "system", http: "", https: "", no_proxy: "", containers: false },
  effective: { http: "", https: "", no_proxy: [], pac: false },
};
ui.pxDraft = { ...ui.proxy.settings };
ui.daemon = { settings: { "registry-mirrors": [], "insecure-registries": [], "max-concurrent-downloads": 4, "max-concurrent-uploads": 5 } };
ui.mirrorsDraft = formatDaemonDraft(ui.daemon.settings);
prefs.tab = "network";
settingsShown.html = "";
settingsShown.el = null;
show("settings");

const ta = () => $("px-mirrors");

t("daemon.json field is present on Network", () => {
  if (!ta()) throw new Error("no px-mirrors; pane=" + ($("content")?.textContent || "").slice(0, 120));
});

t("Tab inserts two spaces at the caret", () => {
  const el = ta();
  el.value = "{\n}";
  el.focus();
  el.setSelectionRange(2, 2); // after "{\n"
  el.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  eq(el.value, "{\n  }", "indented");
  eq(el.selectionStart, 4, "caret after pad");
  eq(ui.mirrorsDraft, "{\n  }", "draft updated");
});

t("Shift+Tab removes leading indent on the line", () => {
  const el = ta();
  el.value = "{\n  \"x\": 1\n}";
  el.focus();
  el.setSelectionRange(4, 4); // on the indented line
  el.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true }));
  eq(el.value, "{\n\"x\": 1\n}", "unindented");
});

t("Tab on a selection indents every line", () => {
  const el = ta();
  el.value = "a\nb\n";
  el.focus();
  el.setSelectionRange(0, 3);
  el.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  eq(el.value, "  a\n  b\n", "block indent");
});

report(R);
