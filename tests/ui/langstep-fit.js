// parity: macos windows linux
// window: 760x480
// The language step in a small window (760×480) with Text size at 125%:
// Continue stays on screen, the list shrinks and scrolls, and nothing of the
// dialog is cut off at an edge.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const dlg = () => document.querySelector('[aria-labelledby="lang-title"]');
const box = (el) => el.getBoundingClientRect();
const zoom = () => Number(document.body.style.zoom || 1);
// Whether the middle of el, and each of its corners i px inside (past a rounded corner), is el on screen.
const onScreen = (el, what, i = 3) => {
  const r = box(el);
  if (!(r.width > 0 && r.height > 0)) throw new Error(`${what} has no size`);
  for (const [x, y] of [[(r.left + r.right) / 2, (r.top + r.bottom) / 2], [r.left + i, r.top + i], [r.right - i, r.top + i], [r.left + i, r.bottom - i], [r.right - i, r.bottom - i]]) {
    const at = document.elementFromPoint(x, y);
    if (!el.contains(at)) throw new Error(`${what} isn't all on screen: ${Math.round(r.left)},${Math.round(r.top)}–${Math.round(r.right)},${Math.round(r.bottom)} in a ${innerWidth}×${innerHeight} window (zoom ${zoom()}); at ${Math.round(x)},${Math.round(y)}: ${at?.id || at?.className || at?.tagName}`);
  }
};
const check = (label) => {
  t(`${label}: Continue is on screen`, () => onScreen(document.getElementById("lang-go"), "Continue"));
  t(`${label}: the search field is on screen`, () => onScreen(document.getElementById("lang-q"), "the search field"));
  t(`${label}: the list shows rows and scrolls`, () => {
    const l = document.getElementById("lang-list");
    if (!(l.clientHeight >= 2 * l.querySelector(".lrow").offsetHeight)) throw new Error(`list ${l.clientHeight}px high`);
    if (!(l.scrollHeight > l.clientHeight)) throw new Error("the list doesn't scroll");
  });
  t(`${label}: the dialog is inside the window, nothing in it cut off`, () => {
    const d = dlg();
    onScreen(d, "the dialog", 16);
    if (d.scrollHeight > d.clientHeight + 1) throw new Error(`the dialog cuts off ${d.scrollHeight - d.clientHeight}px`);
  });
};
t("the window is 760×480", () => { if (innerWidth !== 760 || innerHeight !== 480) throw new Error(`${innerWidth}×${innerHeight}`); });
kiri.on({ type: "langpick" });
check("100%");
dlg().querySelector("#lang-go").click();
setPref("scale", 1.25);
kiri.on({ type: "langpick" });
check("125%");
setPref("lang", "de");
check("125%, in German");
dlg().querySelector("#lang-go").click();
setPref("scale", 1);
setPref("lang", "system");
report(R);
