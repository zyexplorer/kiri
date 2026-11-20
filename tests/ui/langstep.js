// parity: macos windows linux
// The first start's language step as one searchable list: a search field,
// then Suggested (the system's language first, the system's other languages,
// then a few common ones up to five) and every language by its English name.
// A search finds a language by its own name, its English name or its code,
// whatever the accents; Up/Down move, Enter picks; Arabic reads from the
// right; Continue and Escape both close and keep the pick.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const dlg = () => document.querySelector('[aria-labelledby="lang-title"]');
const q = () => document.getElementById("lang-q");
const list = () => dlg()?.querySelector('[role="listbox"]');
const rows = (sel = "") => [...(list()?.querySelectorAll(`${sel} [role="option"]`.trim()) || [])];
const ids = (sel) => rows(sel).map((r) => r.dataset.lang);
const groups = () => [...(list()?.querySelectorAll('[role="group"]') || [])];
const type = (s) => { q().value = s; q().dispatchEvent(new Event("input", { bubbles: true })); };
const key = (k) => q().dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true }));
const active = () => document.getElementById(q().getAttribute("aria-activedescendant") || "-")?.dataset.lang;
const picked = () => [...new Set(rows().filter((r) => r.getAttribute("aria-selected") === "true").map((r) => r.dataset.lang))];
const saved = (name) => sent.filter((m) => m.type === "setpref" && m.name === name).at(-1)?.value;
const box = (el) => el.getBoundingClientRect();
const ITEMS = [];

// An English system that also lists German and Portuguese (Portugal).
window.KIRI_LANG.os = ["en-US", "de-DE", "pt-PT"];
setPref("lang", "system");
sent.length = 0;
kiri.on({ type: "langpick" });
ITEMS.push(...[...(dlg()?.querySelectorAll("input, [role=listbox], [role=group], button") || [])].map((e) => `language step: ${e.tagName.toLowerCase()} ${e.id || e.getAttribute("aria-labelledby") || ""}`.trim()));

t("a search field, focused, then one list", () => {
  if (!dlg()) throw new Error("no language step");
  if (!q() || document.activeElement !== q()) throw new Error("the search field doesn't have focus: " + document.activeElement?.tagName);
  if (q().placeholder !== "Search languages" || q().getAttribute("aria-label") !== "Search languages") throw new Error(`placeholder ${q().placeholder}, label ${q().getAttribute("aria-label")}`);
  if (!list() || list().getAttribute("role") !== "listbox") throw new Error("no listbox");
  if (dlg().querySelector(".choices, [role=radio]")) throw new Error("the tiles are still there");
  if (!(box(q()).bottom <= box(list()).top)) throw new Error("the search field isn't above the list");
});
t("Suggested: the system's language first, its other languages, then up to five", () => {
  const [sug, all] = groups();
  if (sug?.querySelector(".lsect")?.textContent !== "Suggested") throw new Error("first section: " + sug?.querySelector(".lsect")?.textContent);
  const got = ids(`#${sug.id}`).join(" ");
  if (got !== "en de pt-BR es fr") throw new Error("suggested: " + got);
  if (all?.querySelector(".lsect")?.textContent !== "All languages · 38") throw new Error("second section: " + all?.querySelector(".lsect")?.textContent);
});
t("All languages: every one, by its English name", () => {
  const all = groups()[1];
  const got = rows(`#${all.id}`);
  if (got.length !== 38 || new Set(got.map((r) => r.dataset.lang)).size !== 38) throw new Error(got.length + " rows");
  const names = got.map((r) => (r.querySelector(".le") || r.querySelector(".ln")).textContent);
  const sorted = [...names].sort((a, b) => a.localeCompare(b, "en"));
  if (names.join() !== sorted.join()) throw new Error("not sorted: " + names.join(", "));
  if (names[0] !== "Albanian" || got[0].dataset.lang !== "sq") throw new Error("first: " + names[0]);
});
t("a row: its own name in its language, its English name, its hello, a check on the one picked", () => {
  const de = list().querySelector('[data-lang="de"]');
  if (de.querySelector(".ln").getAttribute("lang") !== "de" || de.querySelector(".ln").textContent !== "Deutsch") throw new Error(de.outerHTML);
  if (de.querySelector(".le").textContent !== "German" || de.querySelector(".lh").textContent !== "Hallo") throw new Error(de.textContent);
  const ar = list().querySelector('[data-lang="ar"]');
  if (ar.querySelector(".ln").getAttribute("lang") !== "ar" || ar.querySelector(".ln").textContent !== "العربية" || ar.querySelector(".le").textContent !== "Arabic") throw new Error(ar.textContent);
  if (list().querySelector('[data-lang="en"] .le')) throw new Error("English shows its English name twice");
  if (picked().join() !== "en") throw new Error("picked: " + picked());
  const ck = list().querySelector('[data-lang="en"] .lck');
  if (!ck || getComputedStyle(ck).visibility !== "visible") throw new Error("no check on English");
  if (getComputedStyle(de.querySelector(".lck")).visibility === "visible") throw new Error("a check on German");
  if (!(box(de.querySelector(".lh")).left > box(de.querySelector(".ln")).right)) throw new Error("the hello isn't on the end side");
});
t("the system's language is marked Same as the system", () => {
  const marks = rows().filter((r) => r.querySelector(".sys")).map((r) => r.dataset.lang);
  if ([...new Set(marks)].join() !== "en") throw new Error("marked: " + marks);
  if (list().querySelector('[data-lang="en"] .sys').textContent !== `Same as ${OS}`) throw new Error(list().querySelector('[data-lang="en"] .sys').textContent);
});

for (const s of ["german", "deu", "de", "GERMAN", "Deutsch"]) t(`a search for "${s}" finds Deutsch, first`, () => {
  type(s);
  if (ids()[0] !== "de") throw new Error(`"${s}" found ${ids().join(" ")}`);
  if (groups().length) throw new Error("still in sections");
});
t("accents don't matter: portugues, cestina, turkce", () => {
  for (const [s, id] of [["portugues", "pt-BR"], ["cestina", "cs"], ["turkce", "tr"], ["Portugués", "pt-BR"]]) {
    type(s);
    if (ids()[0] !== id) throw new Error(`"${s}" found ${ids().join(" ")}`);
  }
});
t("a search in another script: русский, 日本", () => {
  for (const [s, id] of [["русский", "ru"], ["日本", "ja"], ["chinese", "zh-Hans"]]) {
    type(s);
    if (ids()[0] !== id) throw new Error(`"${s}" found ${ids().join(" ")}`);
  }
  type("chinese");
  if (ids().join(" ") !== "zh-Hans zh-Hant") throw new Error("chinese found " + ids().join(" "));
});
t("nothing found says so", () => {
  type("klingon");
  if (rows().length) throw new Error(rows().length + " rows");
  const none = list().querySelector(".lnone")?.textContent;
  if (none !== "No language matches. Try its English name.") throw new Error("message: " + none);
  key("Enter");
  if (saved("lang") !== undefined) throw new Error("Enter picked " + saved("lang"));
});
t("clearing the search brings the sections back", () => {
  type("");
  if (groups().length !== 2 || rows().length !== 43) throw new Error(`${groups().length} sections, ${rows().length} rows`);
});

t("Up and Down move through the rows, from the one picked", () => {
  if (active() !== "en") throw new Error("starts on " + active());
  key("ArrowDown"); key("ArrowDown");
  if (active() !== "pt-BR") throw new Error("two down is " + active());
  key("ArrowUp");
  if (active() !== "de") throw new Error("one up is " + active());
  if (!document.getElementById(q().getAttribute("aria-activedescendant")).classList.contains("act")) throw new Error("the row doesn't show it");
  if (saved("lang") !== undefined) throw new Error("moving picked " + saved("lang"));
});
t("Enter picks it: the window switches and saves it", () => {
  key("Enter");
  if (saved("lang") !== "de" || document.documentElement.lang !== "de") throw new Error(`saved ${saved("lang")}, lang ${document.documentElement.lang}`);
  if (picked().join() !== "de") throw new Error("picked: " + picked());
  if (dlg().querySelector("#lang-title").textContent !== STRINGS.de["Choose your language"]) throw new Error(dlg().querySelector("#lang-title").textContent);
  if (q().placeholder !== STRINGS.de["Search languages"] || groups()[0].querySelector(".lsect").textContent !== STRINGS.de["Suggested"]) throw new Error("the step's own words stayed English");
  if (document.activeElement !== q()) throw new Error("focus left the search field");
});
t("typing filters and Enter picks the first match", () => {
  type("fran");
  if (active() !== "fr") throw new Error("active " + active());
  key("Enter");
  if (saved("lang") !== "fr" || document.documentElement.lang !== "fr") throw new Error(`saved ${saved("lang")}, lang ${document.documentElement.lang}`);
  if (q().value !== "fran" || ids().join() !== "fr") throw new Error("the search went: " + q().value);
});
t("a click picks too, and the system's own row saves System", () => {
  type("");
  list().querySelector('[data-lang="en"]').click();
  if (saved("lang") !== "system" || document.documentElement.lang !== "en") throw new Error(`saved ${saved("lang")}, lang ${document.documentElement.lang}`);
  if (document.activeElement !== q()) throw new Error("focus left the search field");
});

const kd = (el, k, more = {}) => { const e = new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true, ...more }); if (more.keyCode) Object.defineProperty(e, "keyCode", { get: () => more.keyCode }); el.dispatchEvent(e); return e; };
const comp = (type, data = "") => q().dispatchEvent(new CompositionEvent(type, { bubbles: true, data }));
t("an input method's keys stay its own: Down picks a candidate, Enter commits the word", () => {
  const was = active();
  comp("compositionstart");
  kd(q(), "ArrowDown", { isComposing: true, keyCode: 229 });
  if (active() !== was) throw new Error("Down in the candidates moved the highlight to " + active());
  // The Mac's WebKit: compositionend, then Enter with keyCode 229 and isComposing false.
  q().value = "日本"; comp("compositionend", "日本"); q().dispatchEvent(new Event("input", { bubbles: true }));
  kd(q(), "Enter", { keyCode: 229 });
  if (saved("lang") !== "system" || document.documentElement.lang !== "en") throw new Error(`committing the word picked: saved ${saved("lang")}, lang ${document.documentElement.lang}`);
  if (ids()[0] !== "ja") throw new Error("日本 found " + ids().join(" "));
});
t("Escape that drops the marked text leaves the step open", () => {
  comp("compositionstart");
  kd(q(), "Escape", { isComposing: true, keyCode: 229 });
  comp("compositionend");
  if (!dlg()) throw new Error("the step closed");
  if (saved("langAsked") !== undefined) throw new Error("langAsked saved");
  type("");
});
t("clearing the search after a pick keeps the highlight on the row picked", () => {
  type("lith"); key("Enter");
  if (saved("lang") !== "lt") throw new Error("saved " + saved("lang"));
  type("");
  if (active() !== "lt") throw new Error("highlight on " + active());
  const lt = document.getElementById(q().getAttribute("aria-activedescendant"));
  const l = box(list()), r = box(lt);
  if (!lt.closest("#lg-a") || !(r.top >= l.top && r.bottom <= l.bottom)) throw new Error(`not in view: row ${r.top}-${r.bottom}, list ${l.top}-${l.bottom}`);
});
t("Up and Down from a picked row further down move from it", () => {
  key("ArrowDown");
  if (active() !== "nb") throw new Error("Down from Lithuanian is " + active());
  key("ArrowUp"); key("ArrowUp");
  if (active() !== "lo") throw new Error("two up is " + active());
});
t("the list isn't a Tab stop", () => { if (list().tabIndex !== -1) throw new Error("tabIndex " + list().tabIndex); });
t("after a click on the step's text the keys still work", () => {
  q().blur();
  dlg().querySelector("#lang-title").click();
  if (document.activeElement !== q()) throw new Error("focus on " + document.activeElement.tagName);
  q().blur();
  kd(document.body, "ArrowDown");
  if (active() !== "lt") throw new Error("Down from the body: " + active());
  kd(document.body, "x");
  if (document.activeElement !== q()) throw new Error("a letter didn't go to the search");
});
t("on Continue, Space and Enter are Continue's", () => {
  const go = $("lang-go"); go.focus();
  for (const k of [" ", "Enter"]) {
    const e = kd(go, k);
    if (document.activeElement !== go) throw new Error(JSON.stringify(k) + " moved the focus to " + document.activeElement.id);
    if (e.defaultPrevented) throw new Error(JSON.stringify(k) + " was stopped");
    if (saved("lang") !== "lt") throw new Error(JSON.stringify(k) + " picked " + saved("lang"));
  }
  q().focus();
  list().querySelector('[data-lang="en"]').click();
});

t("Arabic reads from the right", () => {
  type("arabic"); key("Enter");
  const root = document.documentElement;
  if (root.lang !== "ar" || root.dir !== "rtl") throw new Error(`lang ${root.lang}, dir ${root.dir}`);
  type("");
  const de = list().querySelector('[data-lang="de"]');
  if (!(box(de.querySelector(".ln")).left > box(de.querySelector(".lh")).right)) throw new Error("German's hello isn't on the left");
  if (!(box(de.querySelector(".ln")).left - box(de.querySelector(".le")).right >= 4)) throw new Error("German's English name isn't after it, on the left, with a gap");
  const en = list().querySelector('[data-lang="en"]');
  if (!(box(en.querySelector(".ln")).left - box(en.querySelector(".sys")).right >= 4)) throw new Error("Same as the system isn't after English, on the left, with a gap");
  const icon = q().parentElement.querySelector("svg");
  if (!(box(icon).left > box(q()).left)) throw new Error("the search icon isn't on the right");
  const head = groups()[0].querySelector(".lsect");
  if (head.textContent !== STRINGS.ar["Suggested"]) throw new Error(head.textContent);
  const r = document.createRange(); r.selectNodeContents(head);
  if (!(box(head).right - r.getBoundingClientRect().right < 20)) throw new Error("Suggested isn't on the right");
  if (!(box(document.getElementById("lang-go")).left < box(dlg().querySelector(".lfoot p")).left)) throw new Error("Continue isn't on the left");
});
t("Kurdish and Persian read from the right too", () => {
  for (const s of ["kurdish", "persian"]) {
    type(s); key("Enter");
    if (document.documentElement.dir !== "rtl") throw new Error(s + ": " + document.documentElement.dir);
  }
});

type("german"); key("Enter");
dlg().querySelector("#lang-go").click();
t("Continue closes and keeps the pick", () => {
  if (dlg()) throw new Error("still open");
  if (saved("langAsked") !== true) throw new Error("langAsked " + saved("langAsked"));
  if (document.documentElement.lang !== "de" || saved("lang") !== "de") throw new Error(`lang ${document.documentElement.lang}, saved ${saved("lang")}`);
});

// Opened again with a pick far down the list: that row is in view.
setPref("lang", "lo");
kiri.on({ type: "langpick" });
t("the picked row is scrolled into view on open", () => {
  const lo = list().querySelector('[data-lang="lo"]');
  const l = box(list()), r = box(lo);
  if (lo.getAttribute("aria-selected") !== "true") throw new Error("Lao isn't picked");
  if (!(r.top >= l.top && r.bottom <= l.bottom)) throw new Error(`row ${r.top}-${r.bottom}, list ${l.top}-${l.bottom}`);
  if (active() !== "lo") throw new Error("active " + active());
});
sent.length = 0;
type("ital"); key("Enter");
const escKey = () => kd(document.activeElement, "Escape");
escKey();
t("Escape with a search typed clears the search and stays open", () => {
  if (!dlg()) throw new Error("the step closed");
  if (q().value !== "" || groups().length !== 2) throw new Error(`search "${q().value}", ${groups().length} sections`);
  if (document.activeElement !== q()) throw new Error("focus on " + document.activeElement.id);
  if (active() !== "it") throw new Error("highlight on " + active());
  if (saved("langAsked") !== undefined) throw new Error("langAsked saved");
});
escKey();
t("a second Escape closes as Continue does, keeping the pick", () => {
  if (dlg()) throw new Error("still open");
  if (saved("langAsked") !== true) throw new Error("langAsked " + saved("langAsked"));
  if (document.documentElement.lang !== "it" || saved("lang") !== "it") throw new Error(`lang ${document.documentElement.lang}, saved ${saved("lang")}`);
});

// Enter: on a new row it picks; on the row already picked it is Continue.
const reopen = (pick = "system") => { setPref("lang", pick); sent.length = 0; kiri.on({ type: "langpick" }); };
reopen();
t("Enter on the row picked when the step opens is Continue", () => {
  if (active() !== "en") throw new Error("starts on " + active());
  key("Enter");
  if (dlg()) throw new Error("still open");
  if (saved("langAsked") !== true || document.documentElement.lang !== "en") throw new Error(`langAsked ${saved("langAsked")}, lang ${document.documentElement.lang}`);
});
reopen();
t("Enter picks a search's first match; Enter again is Continue", () => {
  type("fran"); key("Enter");
  if (!dlg() || saved("lang") !== "fr") throw new Error(`open ${!!dlg()}, saved ${saved("lang")}`);
  key("Enter");
  if (dlg()) throw new Error("the second Enter didn't continue");
  if (saved("langAsked") !== true || saved("lang") !== "fr") throw new Error(`langAsked ${saved("langAsked")}, lang ${saved("lang")}`);
});

// Tab stays in the step: the search field, then Continue, then back.
reopen();
t("Tab and Shift+Tab go between the search field and Continue, and nowhere else", () => {
  const go = $("lang-go");
  const seq = [];
  for (const shift of [false, false, false, true, true]) {
    const e = kd(document.activeElement, "Tab", { shiftKey: shift });
    if (!e.defaultPrevented) throw new Error("Tab was left to the browser (WebKit skips the button; the sidebar is behind)");
    seq.push(document.activeElement === q() ? "search" : document.activeElement === go ? "continue" : document.activeElement.id || document.activeElement.tagName);
  }
  if (seq.join(" ") !== "continue search continue search continue") throw new Error(seq.join(" "));
  q().blur();
  kd(document.body, "Tab");
  if (document.activeElement !== q()) throw new Error("Tab from outside went to " + document.activeElement.tagName);
  kd(document.body, "Tab", { shiftKey: true });
  if (document.activeElement !== go) throw new Error("Shift+Tab from the search went to " + document.activeElement.tagName);
  if (go.tabIndex !== 0) throw new Error("Continue's tabIndex " + go.tabIndex);
  q().focus();
});
t("Shift+Tab as WebKitGTK sends it (key Unidentified, code Tab) stays in the step", () => {
  const go = $("lang-go");
  q().focus();
  const e = kd(q(), "Unidentified", { code: "Tab", keyCode: 9, shiftKey: true });
  if (!e.defaultPrevented || document.activeElement !== go) throw new Error("went to " + (document.activeElement.id || document.activeElement.tagName));
  const e2 = kd(go, "Unidentified", { code: "Tab", keyCode: 9, shiftKey: true });
  if (!e2.defaultPrevented || document.activeElement !== q()) throw new Error("from Continue went to " + (document.activeElement.id || document.activeElement.tagName));
});
t(`${WIN || LINUX ? "Ctrl" : "⌘"}+F goes to the step's search, not the window's`, () => {
  $("lang-go").focus();
  type("x");
  const e = kd(document.activeElement, "f", WIN || LINUX ? { ctrlKey: true } : { metaKey: true });
  if (document.activeElement !== q()) throw new Error("focus on " + (document.activeElement.id || document.activeElement.tagName));
  if (!e.defaultPrevented) throw new Error("not stopped");
  if (q().selectionStart !== 0 || q().selectionEnd !== 1) throw new Error("the search isn't selected");
  type("");
});

t("Page Down, Page Up, Home and End move the highlight", () => {
  const all = rows().map((r) => r.id);
  key("Home");
  if (q().getAttribute("aria-activedescendant") !== all[0]) throw new Error("Home: " + active());
  key("End");
  if (q().getAttribute("aria-activedescendant") !== all.at(-1)) throw new Error("End: " + active());
  const lr = box(list()), last = box(document.getElementById(all.at(-1)));
  if (!(last.bottom <= lr.bottom + 1)) throw new Error("the last row isn't in view");
  key("Home"); key("PageDown");
  const i = all.indexOf(q().getAttribute("aria-activedescendant"));
  if (!(i >= 2)) throw new Error("Page Down moved to row " + i);
  key("PageUp");
  if (q().getAttribute("aria-activedescendant") !== all[0]) throw new Error("Page Up: " + active());
  if (saved("lang") !== undefined) throw new Error("moving picked " + saved("lang"));
  type("an");
  key("End");
  if (active() !== ids().at(-1)) throw new Error("End in a search: " + active());
  type("");
});

t("other names: farsi, mandarin, chinese, castellano, हिंदी", () => {
  for (const [s, want] of [["farsi", "fa"], ["Farsi", "fa"], ["mandarin", "zh-Hans zh-Hant"], ["chinese", "zh-Hans zh-Hant"], ["castellano", "es"], ["हिंदी", "hi"], ["हिन्दी", "hi"]]) {
    type(s);
    const got = ids().slice(0, want.split(" ").length).join(" ");
    if (got !== want) throw new Error(`"${s}" found ${ids().join(" ")}`);
  }
  type("");
});
t("a search that finds nothing says so to a screen reader", () => {
  const live = $("lang-none");
  if (!live || live.getAttribute("aria-live") !== "polite") throw new Error("no polite live region");
  if (live.closest("#lang-list")) throw new Error("the live region is redrawn with the list");
  type("klingon");
  if (live.textContent !== "No languages match") throw new Error(`says "${live.textContent}"`);
  type("ger");
  if (live.textContent !== "") throw new Error(`still says "${live.textContent}"`);
  type("");
});

t("the tables agree: an English name and a hello for each language, nothing else", () => {
  const ids = LANGS.map(([id]) => id).sort().join();
  if (Object.keys(LANG_EN).sort().join() !== ids) throw new Error("LANG_EN: " + Object.keys(LANG_EN).filter((k) => !LANGS.some(([id]) => id === k)).concat(LANGS.map(([id]) => id).filter((id) => !(id in LANG_EN))));
  if (Object.keys(HELLO).sort().join() !== ids) throw new Error("HELLO: " + Object.keys(HELLO).filter((k) => !LANGS.some(([id]) => id === k)).concat(LANGS.map(([id]) => id).filter((id) => !(id in HELLO))));
  if ($("pref-lang") && [...$("pref-lang").options].length - 1 !== LANGS.length) throw new Error("Settings offers another list");
  for (const ids of Object.values(LANG_ALIAS)) for (const id of ids) if (!LANG_EN[id]) throw new Error("an alias for " + id);
});
dlg().querySelector("#lang-go").click();
// A language added to LANGS without an English name or a hello yet.
LANGS.push(["xx", "Xxish"]);
let opened = null;
try { reopen(); opened = dlg(); } catch (e) { R.push("FAIL a language without an English name: " + e.message); }
t("a language without an English name or a hello still shows, by its own name", () => {
  if (!opened) throw new Error("the step didn't open");
  const xx = list().querySelector('#lg-a [data-lang="xx"]');
  if (!xx || xx.querySelector(".ln").textContent !== "Xxish" || xx.querySelector(".le")) throw new Error(xx?.outerHTML || "no row");
  if (list().querySelector("#lg-a .lsect").textContent !== "All languages · 39") throw new Error(list().querySelector("#lg-a .lsect").textContent);
  type("xxi");
  if (ids().join() !== "xx") throw new Error("found " + ids());
  type("");
});
LANGS.pop();
dlg()?.querySelector("#lang-go").click();
t("a double click on a row picks it and continues, though the first click redraws the list", () => {
  reopen();
  const row = () => document.querySelector('#lang-list [data-lang="fi"]');
  row().dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 1 }));
  row().dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 2 }));
  if (saved("langAsked") !== true) throw new Error("langAsked " + saved("langAsked"));
  if (prefs.lang !== "fi") throw new Error("picked " + prefs.lang);
  reopen();
});

window.KIRI_LANG.os = ["en-US"];
setPref("lang", "system");
report([...R, ...ITEMS.map((i) => "ITEM " + i)]);
