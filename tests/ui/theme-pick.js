// window: 1000x640
// #291: picking a theme in Settings > Appearance recolours the page by itself, whatever scheme the
// engine reports (WebKitGTK does not flip prefers-color-scheme when the window asks GTK for its dark
// variant). After each pick: the page background, a panel (the sidebar's engine card), the text, the
// root's color-scheme and the terminal's colours are the chosen theme's. "system" follows the engine.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const sysDark = matchMedia("(prefers-color-scheme: dark)").matches;
const WANT = { dark: { bg: "rgb(15, 20, 25)", panel: "rgb(23, 29, 36)", text: "rgb(242, 240, 235)", scheme: "dark", term: "#0B0F13" },
               light: { bg: "rgb(245, 247, 250)", panel: "rgb(255, 255, 255)", text: "rgb(15, 20, 25)", scheme: "light", term: "#FFFFFF" } };
kiri.on({ type: "prefs", prefs: { theme: "dark", tab: "appearance" } });
prefs.tab = "appearance"; show("settings");
const pick = (v) => document.querySelector(`[data-theme-pick="${v}"]`).dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const seen = () => { const cs = (el) => getComputedStyle(el), root = document.documentElement;
  return { bg: cs(document.body).backgroundColor, panel: cs(document.querySelector(".vm")).backgroundColor,
           text: cs(document.querySelector(".pane h3")).color, scheme: cs(root).colorScheme, term: cs(root).getPropertyValue("--term-bg").trim() }; };
R.push(`INFO engine reports ${sysDark ? "dark" : "light"}`);
for (const v of ["light", "dark", "light", "system", "dark", "system"]) {
  const want = v === "system" ? (sysDark ? "dark" : "light") : v;
  t(`after picking ${v}: the page is ${want} (background, panel, text, color-scheme, terminal)`, () => { pick(v); eq(seen(), WANT[want], v); });
  t(`after picking ${v}: the theme card marked In use is ${want}`, () => eq(document.querySelector('.tcard[aria-pressed="true"]')?.dataset.themePick, want, "in use"));
  t(`after picking ${v}: the strip's theme button agrees`, () => eq(darkNow(), want === "dark", "darkNow"));
}
t("the host was told each pick (setpref theme)", () => eq(sent.filter((s) => s.type === "setpref" && s.name === "theme").map((s) => s.value), ["light", "dark", "light", "system", "dark", "system"], "sent"));
// First paint: before the app's prefs arrive the root carries no theme of its own, so the engine's scheme
// (the window's background, set by the host) decides, as before.
report(R);
