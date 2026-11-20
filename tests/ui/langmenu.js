// Picking a language from the menu with the mouse: the system's menu takes
// the button's release, so the page sees a pointerdown on the <select> and
// never a pointerup. Settings must still redraw in the new language at once,
// not wait for the next click anywhere; and a press held on a button still
// holds its redraw.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
show("settings"); document.querySelector('[data-stab="appearance"]').click();
t("Settings is in Spanish straight after picking Español from the menu", () => {
  const sel = document.getElementById("pref-lang");
  sel.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, button: 0 }));
  sel.value = "es"; sel.dispatchEvent(new Event("change", { bubbles: true }));
  const h = document.querySelector("#content .pane h3").textContent;
  if (h !== tr("Appearance") || h === "Appearance") throw new Error("pane heading is " + JSON.stringify(h));
  const tab = document.querySelector('[data-stab="resources"]').textContent;
  if (tab === "Resources") throw new Error("tabs still English");
});
t("A press held on a button still holds the redraw until it's released", () => {
  const b = document.querySelector('[data-stab="about"]');
  b.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, button: 0 }));
  const was = document.querySelector("#content .settings");
  render(); settingsShown.html = ""; render();
  if (document.querySelector("#content .settings") !== was) throw new Error("redrew under the pointer");
  b.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, button: 0 }));
});
setPref("lang", "en");
report(R);
