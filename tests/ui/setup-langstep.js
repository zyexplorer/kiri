// page: setup
// window: 1024x700
// Windows' first start opens on the setup window, which asks the language
// before Welcome: the same step as the main window's (langstep.js), a search
// field and one list (Suggested, then every language by its English name),
// the same keys, and a pick that redraws the page in place, search and focus
// kept. Continue saves langAsked, so the main window doesn't ask again.
(async () => {
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
// The page draws a host state on the next frame (a task here: ui-test.py).
const frame = () => new Promise((r) => setTimeout(() => setTimeout(r, 0), 0));
const main = () => document.getElementById("main");
const q = () => document.getElementById("lang-q");
const list = () => document.getElementById("lang-list");
const rows = (sel = "") => [...(list()?.querySelectorAll(`${sel} [role="option"]`.trim()) || [])];
const ids = (sel) => rows(sel).map((r) => r.dataset.lang);
const groups = () => [...(list()?.querySelectorAll('[role="group"]') || [])];
const type = (s) => { q().value = s; q().dispatchEvent(new Event("input", { bubbles: true })); };
const kd = (el, k, more = {}) => { const e = new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true, ...more }); if (more.keyCode) Object.defineProperty(e, "keyCode", { get: () => more.keyCode }); el.dispatchEvent(e); return e; };
const key = (k, more) => kd(q(), k, more);
const active = () => document.getElementById(q().getAttribute("aria-activedescendant") || "-")?.dataset.lang;
const picked = () => [...new Set(rows().filter((r) => r.getAttribute("aria-selected") === "true").map((r) => r.dataset.lang))];
const saved = (name) => sent.filter((m) => m.type === "setpref" && m.name === name).at(-1)?.value;
const box = (el) => el.getBoundingClientRect();
const comp = (type, data = "") => q().dispatchEvent(new CompositionEvent(type, { bubbles: true, data }));
const state = (more) => kiri.on({ type: "setup", step: "welcome", sizes: { wsl: 367575040, ubuntu: 418495746, kiri: 9000000 }, tasks: [], log: [], ...more });

// An English system that also lists German and Portuguese (Portugal).
window.KIRI_LANG.os = ["en-US", "de-DE", "pt-PT"];
state({ askLang: true });
await frame();

t("the setup window asks with a search field, focused, and one list, not tiles", () => {
  if (!q() || !list()) throw new Error("no search field or list: " + main().textContent.slice(0, 80));
  if (main().querySelector(".langs, .lang, [role=radio]")) throw new Error("the tiles are still there");
  if (document.activeElement !== q()) throw new Error("focus on " + document.activeElement.tagName);
  if (q().placeholder !== "Search languages" || q().getAttribute("aria-label") !== "Search languages") throw new Error(q().placeholder);
  if (list().getAttribute("role") !== "listbox") throw new Error("no listbox");
  if (document.getElementById("lang-title").textContent !== "Choose your language") throw new Error(document.getElementById("lang-title").textContent);
  if (main().querySelector(".lhello").textContent !== "Hello!") throw new Error("hello: " + main().querySelector(".lhello").textContent);
  if (!(box(q()).bottom <= box(list()).top && box(list()).bottom <= box(document.getElementById("lang-go")).top)) throw new Error("not search, list, Continue");
});
t("Suggested, then All languages · 38 by English name, as in the main window", () => {
  const [sug, all] = groups();
  if (sug?.querySelector(".lsect").textContent !== "Suggested") throw new Error(sug?.textContent.slice(0, 40));
  if (ids(`#${sug.id}`).join(" ") !== "en de pt-BR es fr") throw new Error("suggested: " + ids(`#${sug.id}`).join(" "));
  if (all?.querySelector(".lsect").textContent !== "All languages · 38") throw new Error(all?.querySelector(".lsect").textContent);
  const got = rows(`#${all.id}`), names = got.map((r) => (r.querySelector(".le") || r.querySelector(".ln")).textContent);
  if (got.length !== 38 || names.join() !== [...names].sort((a, b) => a.localeCompare(b, "en")).join()) throw new Error(names.join(", "));
  const de = list().querySelector('[data-lang="de"]');
  if (de.querySelector(".ln").textContent !== "Deutsch" || de.querySelector(".le").textContent !== "German" || de.querySelector(".lh").textContent !== "Hallo") throw new Error(de.textContent);
  if (picked().join() !== "en") throw new Error("picked " + picked());
  const marks = [...new Set(rows().filter((r) => r.querySelector(".sys")).map((r) => r.dataset.lang))];
  if (marks.join() !== "en" || list().querySelector('[data-lang="en"] .sys').textContent !== "Same as Windows") throw new Error("marked " + marks);
});
t("a search finds by own name, English name, code and other names, whatever the accents", () => {
  for (const [s, want] of [["german", "de"], ["deu", "de"], ["portugues", "pt-BR"], ["русский", "ru"], ["farsi", "fa"], ["castellano", "es"], ["हिंदी", "hi"], ["mandarin", "zh-Hans zh-Hant"]]) {
    type(s);
    if (ids().slice(0, want.split(" ").length).join(" ") !== want) throw new Error(`"${s}" found ${ids().join(" ")}`);
  }
  type("klingon");
  if (rows().length || list().querySelector(".lnone")?.textContent !== "No language matches. Try its English name.") throw new Error("no message");
  if (document.getElementById("lang-none").textContent !== "No languages match") throw new Error("not announced");
  type("");
  if (groups().length !== 2) throw new Error("the sections didn't come back");
});
t("Up, Down, Page Down, Home and End move the highlight; nothing is picked", () => {
  if (active() !== "en") throw new Error("starts on " + active());
  key("ArrowDown"); key("ArrowDown"); key("ArrowUp");
  if (active() !== "de") throw new Error("down, down, up: " + active());
  key("End");
  if (q().getAttribute("aria-activedescendant") !== rows().at(-1).id) throw new Error("End: " + active());
  key("Home"); key("PageDown");
  if (!(rows().findIndex((r) => r.id === q().getAttribute("aria-activedescendant")) >= 2)) throw new Error("Page Down: " + active());
  key("PageUp");
  if (q().getAttribute("aria-activedescendant") !== rows()[0].id) throw new Error("Page Up: " + active());
  if (saved("lang") !== undefined) throw new Error("moving picked " + saved("lang"));
});
t("an input method's keys stay its own", () => {
  comp("compositionstart");
  key("ArrowDown", { isComposing: true, keyCode: 229 });
  key("Escape", { isComposing: true, keyCode: 229 });
  comp("compositionend");
  key("Enter", { keyCode: 229 });
  if (active() !== "en" || saved("lang") !== undefined || !q()) throw new Error(`active ${active()}, saved ${saved("lang")}`);
});
t("Enter picks: the whole window switches language, the step stays with its search and focus", () => {
  type("ger"); key("Enter");
  if (saved("lang") !== "de" || document.documentElement.lang !== "de") throw new Error(`saved ${saved("lang")}, lang ${document.documentElement.lang}`);
  if (q().value !== "ger" || document.activeElement !== q()) throw new Error(`search "${q().value}", focus ${document.activeElement.tagName}`);
  if (document.getElementById("lang-title").textContent !== STRINGS.de["Choose your language"] || q().placeholder !== STRINGS.de["Search languages"]) throw new Error("the step stayed English");
  if (document.getElementById("lang-go").textContent !== STRINGS.de["Continue"]) throw new Error("Continue stayed English");
  if (main().querySelector(".lhello").textContent !== "Hallo!") throw new Error(main().querySelector(".lhello").textContent);
  if (picked().join() !== "de") throw new Error("picked " + picked());
});
await frame();
t("a state from the host meanwhile doesn't rebuild the step", () => {
  state({ askLang: true, tasks: [] });
  if (q().value !== "ger" || document.activeElement !== q()) throw new Error(`search "${q().value}", focus ${document.activeElement.tagName}`);
});
await frame();
t("Tab goes between the search field and Continue only", () => {
  const go = document.getElementById("lang-go"), seq = [];
  for (const shift of [false, false, true]) {
    const e = kd(document.activeElement, "Tab", { shiftKey: shift });
    if (!e.defaultPrevented) throw new Error("Tab was left to the browser");
    seq.push(document.activeElement === q() ? "search" : document.activeElement === go ? "continue" : document.activeElement.tagName);
  }
  if (seq.join(" ") !== "continue search continue") throw new Error(seq.join(" "));
});
t("Ctrl+F goes to the search", () => {
  const e = kd(document.activeElement, "f", { ctrlKey: true });
  if (document.activeElement !== q() || !e.defaultPrevented) throw new Error("focus on " + document.activeElement.tagName);
});
t("Escape clears the search first", () => {
  kd(q(), "Escape");
  if (!q() || q().value !== "" || groups().length !== 2) throw new Error("the search wasn't cleared, or the step closed");
  if (active() !== "de") throw new Error("highlight on " + active());
  if (saved("langAsked") !== undefined) throw new Error("langAsked saved");
});
t("Arabic reads from the right", () => {
  type("arabic"); key("Enter");
  if (document.documentElement.dir !== "rtl") throw new Error("dir " + document.documentElement.dir);
  type("");
  const de = list().querySelector('[data-lang="de"]');
  if (!(box(de.querySelector(".ln")).left > box(de.querySelector(".lh")).right)) throw new Error("German's hello isn't on the left");
  if (!(box(de.querySelector(".ln")).left > box(de.querySelector(".le")).right)) throw new Error("German's English name isn't after it");
  if (!(box(q().parentElement.querySelector("svg")).left > box(q()).left)) throw new Error("the search icon isn't on the right");
  if (!(box(document.getElementById("lang-go")).left < box(main().querySelector(".lfoot p")).left)) throw new Error("Continue isn't on the left");
});
t("a language whose words come from the host later: the step redraws when they do", () => {
  hostHold = true;
  type("japanese"); key("Enter");
  if (saved("lang") !== "ja") throw new Error("saved " + saved("lang"));
  hostFlush();
  if (document.documentElement.lang !== "ja" || q().placeholder !== STRINGS.ja["Search languages"]) throw new Error(`lang ${document.documentElement.lang}, placeholder ${q().placeholder}`);
  if (q().value !== "japanese") throw new Error("the search went");
  hostHold = false;
});
t("Enter on the row already picked is Continue: langAsked saved, Welcome next, in that language", () => {
  key("Enter");
  if (saved("langAsked") !== true) throw new Error("langAsked " + saved("langAsked"));
  if (q()) throw new Error("the step is still there");
  if (document.documentElement.lang !== "ja") throw new Error("lang " + document.documentElement.lang);
});
await frame();
t("after the step its keys are gone: Escape and Tab are the page's again", () => {
  const e = kd(document.body, "Tab");
  if (e.defaultPrevented) throw new Error("Tab still caught");
  if (!main().querySelector("h1") || main().querySelector("#lang-list")) throw new Error("not Welcome: " + main().textContent.slice(0, 60));
});

report([...R]);
})().catch((e) => report(["FAIL " + e.message + " " + e.stack]));
