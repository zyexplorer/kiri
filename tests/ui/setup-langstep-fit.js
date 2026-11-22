// page: setup
// window: 800x520
// Windows' setup window, short (800×520), opened on a language far down the
// list (Lao), in Hungarian words: the header, the search field and Continue
// stay on screen, the list takes what's left and scrolls, the picked row is in
// view, and nothing runs out of its box. Then Continue: the setup carries on.
(async () => {
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
// The page draws a host state on the next frame (a task here: ui-test.py).
const frame = () => new Promise((r) => setTimeout(() => setTimeout(r, 0), 0));
const main = () => document.getElementById("main");
const box = (el) => el.getBoundingClientRect();
const saved = (name) => sent.filter((m) => m.type === "setpref" && m.name === name).at(-1)?.value;
const onScreen = (el, what) => {
  const r = box(el);
  for (const [x, y] of [[(r.left + r.right) / 2, (r.top + r.bottom) / 2], [r.left + 4, r.top + 4], [r.right - 4, r.bottom - 4]]) {
    const at = document.elementFromPoint(x, y);
    if (!el.contains(at)) throw new Error(`${what} isn't on screen: ${Math.round(r.top)}–${Math.round(r.bottom)} in ${innerWidth}×${innerHeight}; at ${Math.round(x)},${Math.round(y)}: ${at?.id || at?.className || at?.tagName}`);
  }
};
window.KIRI_LANG.os = ["hu-HU", "en-US"];
pickLang("lo");
await new Promise((r) => setTimeout(r, 0));
kiri.on({ type: "setup", step: "welcome", askLang: true, sizes: {}, tasks: [], log: [] });
await frame();
t("the window is 800×520", () => { if (innerWidth !== 800 || innerHeight !== 520) throw new Error(`${innerWidth}×${innerHeight}`); });
t("Continue, the search field and the header are on screen", () => {
  onScreen(document.getElementById("lang-go"), "Continue");
  onScreen(document.getElementById("lang-q"), "the search field");
  onScreen(document.getElementById("lang-title"), "the title");
  if (main().scrollHeight > main().clientHeight + 1) throw new Error(`the page scrolls ${main().scrollHeight - main().clientHeight}px`);
});
t("the list shows rows and scrolls, Lao picked and in view", () => {
  const l = document.getElementById("lang-list"), lo = l.querySelector('#lg-a [data-lang="lo"]');
  if (!(l.clientHeight >= 2 * lo.offsetHeight) || !(l.scrollHeight > l.clientHeight)) throw new Error(`list ${l.clientHeight}px of ${l.scrollHeight}`);
  if (lo.getAttribute("aria-selected") !== "true") throw new Error("Lao isn't picked");
  const a = box(l), r = box(lo);
  if (!(r.top >= a.top && r.bottom <= a.bottom)) throw new Error(`row ${r.top}-${r.bottom}, list ${a.top}-${a.bottom}`);
});
t("the system's language, Hungarian, picked", () => {
  pickLang("system");
  document.getElementById("lang-q").dispatchEvent(new Event("input"));
  kiri.on({ type: "setup", step: "welcome", askLang: true, sizes: {}, tasks: [], log: [] });
});
await frame();
t("(Hungarian words in place)", () => { if (document.getElementById("lang-go").textContent !== STRINGS.hu?.["Continue"]) throw new Error(document.getElementById("lang-go").textContent); });
t("in Hungarian, nothing runs out of its box or past the list", () => {
  const out = [], l = box(document.getElementById("lang-list"));
  for (const el of main().querySelectorAll(".lbody *")) {
    if (el.closest("svg") || getComputedStyle(el).position === "absolute") continue;
    const r = box(el), p = box(el.parentElement);
    if (r.width && (r.right > p.right + 0.5 || r.left < p.left - 0.5)) out.push(`${el.className || el.tagName} "${el.textContent.trim().slice(0, 20)}"`);
    if (el.closest("#lang-list") && r.width && (r.right > l.right + 0.5 || r.left < l.left - 0.5)) out.push(`${el.className} past the list`);
  }
  if (out.length) throw new Error([...new Set(out)].join("; "));
});
t("Continue saves langAsked, and the setup carries on", () => {
  document.getElementById("lang-go").click();
  if (saved("langAsked") !== true) throw new Error("langAsked " + saved("langAsked"));
  if (document.getElementById("lang-list")) throw new Error("the step is still there");
});
window.KIRI_LANG.os = ["en-US"];
report(R);
})().catch((e) => report(["FAIL " + e.message + " " + e.stack]));
