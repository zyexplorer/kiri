// The strip across the top: the sidebar button, back and forward through
// the places opened, Settings, and the search box.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
t("the strip spans the window above the sidebar and the page", () => {
  const s = $("strip").getBoundingClientRect(), a = document.querySelector("aside").getBoundingClientRect();
  if (s.top !== 0 || Math.round(s.height) !== 44 || Math.round(a.top) !== 44 || Math.round(s.width) !== innerWidth) throw new Error(JSON.stringify([s.top, s.height, a.top, s.width]));
});
t("back and forward go through the views opened", () => {
  show("containers"); show("images"); show("volumes");
  if ($("nav-back").disabled || !$("nav-fwd").disabled) throw new Error("buttons");
  $("nav-back").click();
  if (ui.view !== "images") throw new Error(ui.view);
  $("nav-back").click();
  if (ui.view !== "containers") throw new Error(ui.view);
  $("nav-fwd").click();
  if (ui.view !== "images" || $("nav-fwd").disabled) throw new Error(ui.view);
  show("networks");
  if (!$("nav-fwd").disabled) throw new Error("forward kept after a new place");
});
t("the keys go back too", () => {
  document.dispatchEvent(new KeyboardEvent("keydown", WIN ? { key: "ArrowLeft", altKey: true, bubbles: true } : { key: "[", metaKey: true, bubbles: true }));
  if (ui.view !== "images") throw new Error(ui.view);
  show("containers");
});
t("Settings opens Settings", () => {
  $("strip-settings").click();
  if (ui.view !== "settings") throw new Error(ui.view);
  show("containers");
});
t("the strip theme button sits left of Terminal and toggles", () => {
  const theme = $("strip-theme"), term = $("strip-term"), settings = $("strip-settings");
  if (!theme || !term || !settings) throw new Error("missing strip buttons");
  if (!(theme.compareDocumentPosition(term) & Node.DOCUMENT_POSITION_FOLLOWING)) throw new Error("theme should be left of terminal");
  if (!(term.compareDocumentPosition(settings) & Node.DOCUMENT_POSITION_FOLLOWING)) throw new Error("terminal should be left of settings");
  const before = prefs.theme;
  prefs.theme = "dark"; applyPrefs();
  theme.click();
  if (prefs.theme !== "light") throw new Error("want light, got " + prefs.theme);
  theme.click();
  if (prefs.theme !== "dark") throw new Error("want dark, got " + prefs.theme);
  prefs.theme = before; applyPrefs();
});
t("the strip drags the window, its buttons don't", () => {
  sent.length = 0;
  document.querySelector(".strip .drag").dispatchEvent(new MouseEvent("mousedown", { button: 0, detail: 1, bubbles: true }));
  $("nav-back").dispatchEvent(new MouseEvent("mousedown", { button: 0, detail: 1, bubbles: true }));
  if (sent.filter((m) => m.type === "drag").length !== 1) throw new Error(JSON.stringify(sent));
});
t("the page has no Filter box: the strip's search narrows the list", () => {
  if ($("search")) throw new Error("Filter box still there");
  show("containers");
  ui.containers = [{ Id: "a1", Names: ["/alpha"], Image: "nginx", State: "running", Status: "Up", Ports: [], Labels: {} }, { Id: "b1", Names: ["/beta"], Image: "redis", State: "running", Status: "Up", Ports: [], Labels: {} }];
  const q = $("gq");
  q.value = "alp"; q.dispatchEvent(new Event("input", { bubbles: true }));
  const names = [...document.querySelectorAll("#content tbody tr")].map((r) => r.textContent);
  if (names.length !== 1 || !/alpha/.test(names[0])) throw new Error(JSON.stringify(names));
  if (q.placeholder !== "Search containers…") throw new Error(q.placeholder);
  q.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  if (q.value || ui.filter) throw new Error("not cleared");
});
t("⌘K / Ctrl+K goes to it", () => {
  document.body.focus();
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "k", metaKey: !WIN, ctrlKey: WIN, bubbles: true }));
  if (document.activeElement !== $("gq")) throw new Error(document.activeElement?.id);
  $("gq").blur();
});
report(R);
