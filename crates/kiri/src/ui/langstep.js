/* The languages, and the first start's language step, for both pages that ask:
   the window (index.html) and Windows' setup window (kiri-win's setup.html), so
   the two can't drift. assemble.rs splices this in at each page's LANGSTEP
   placeholder (scripts/ui-test.py does the same). The page brings its own look
   (each styles .lfind, .llist, .lrow… in its own palette) and its own tr(),
   langFor() and saving; this file has the tables, the search and the keys. */

/* Each language in its own name, by script: the Latin alphabet (A to Z by that
   name), Greek, Cyrillic, the Arabic script, Ethiopic, the Indian scripts,
   Thai and Lao, then Japanese, Korean and Chinese. */
const LANGS = [["en", "English"], ["id", "Bahasa Indonesia"], ["ca", "Català"], ["cs", "Čeština"], ["de", "Deutsch"], ["es", "Español"], ["fr", "Français"], ["it", "Italiano"], ["lt", "Lietuvių"], ["hu", "Magyar"], ["nb", "Norsk bokmål"], ["pl", "Polski"], ["pt-BR", "Português (Brasil)"], ["ro", "Română"], ["sq", "Shqip"], ["sl", "Slovenščina"], ["fi", "Suomi"], ["vi", "Tiếng Việt"], ["tr", "Türkçe"], ["el", "Ελληνικά"], ["bg", "Български"], ["ru", "Русский"], ["uk", "Українська"], ["kk", "Қазақша"], ["ar", "العربية"], ["fa", "فارسی"], ["ckb", "کوردیی ناوەندی"], ["am", "አማርኛ"], ["hi", "हिन्दी"], ["bn", "বাংলা"], ["ta", "தமிழ்"], ["te", "తెలుగు"], ["th", "ไทย"], ["lo", "ລາວ"], ["ja", "日本語"], ["ko", "한국어"], ["zh-Hans", "简体中文"], ["zh-Hant", "繁體中文"]];
/* Each language's English name: the language step shows it beside the own
   name and finds a language by it ("German" for Deutsch). One for each in
   LANGS; one missing shows and sorts by its own name (langEn). */
const LANG_EN = { en: "English", id: "Indonesian", ca: "Catalan", cs: "Czech", de: "German", es: "Spanish", fr: "French", it: "Italian", lt: "Lithuanian", hu: "Hungarian", nb: "Norwegian Bokmål", pl: "Polish", "pt-BR": "Portuguese (Brazil)", ro: "Romanian", sq: "Albanian", sl: "Slovenian", fi: "Finnish", vi: "Vietnamese", tr: "Turkish", el: "Greek", bg: "Bulgarian", ru: "Russian", uk: "Ukrainian", kk: "Kazakh", ar: "Arabic", fa: "Persian", ckb: "Kurdish (Sorani)", am: "Amharic", hi: "Hindi", bn: "Bengali", ta: "Tamil", te: "Telugu", th: "Thai", lo: "Lao", ja: "Japanese", ko: "Korean", "zh-Hans": "Chinese (Simplified)", "zh-Hant": "Chinese (Traditional)" };
/* Hello in each, in its own script. */
const HELLO = { en: "Hello", id: "Halo", ca: "Hola", cs: "Ahoj", de: "Hallo", es: "Hola", fr: "Bonjour", it: "Ciao", lt: "Labas", hu: "Szia", nb: "Hei", pl: "Cześć", "pt-BR": "Olá", ro: "Salut", sq: "Përshëndetje", sl: "Živjo", fi: "Hei", vi: "Xin chào", tr: "Merhaba", el: "Γεια σου", bg: "Здравей", ru: "Привет", uk: "Привіт", kk: "Сәлем", ar: "مرحبًا", fa: "سلام", ckb: "سڵاو", am: "ሰላም", hi: "नमस्ते", bn: "নমস্কার", ta: "வணக்கம்", te: "నమస్కారం", th: "สวัสดี", lo: "ສະບາຍດີ", ja: "こんにちは", ko: "안녕하세요", "zh-Hans": "你好", "zh-Hant": "你好" };
/* Other names people search by: what a language is also called in English
   ("Farsi"), in itself ("Castellano") or spelled another way (हिंदी). */
const LANG_ALIAS = { farsi: ["fa"], mandarin: ["zh-Hans", "zh-Hant"], chinese: ["zh-Hans", "zh-Hant"], putonghua: ["zh-Hans"], cantonese: ["zh-Hant"],
  castellano: ["es"], castilian: ["es"], "हिंदी": ["hi"], bangla: ["bn"], brazilian: ["pt-BR"], norwegian: ["nb"], sorani: ["ckb"], bahasa: ["id"] };
const langEn = (id, own) => LANG_EN[id] || own;
const langOwn = (id) => (LANGS.find(([l]) => l === id) || [id, id])[1];
/* Languages offered after the system's own, until Suggested holds five. */
const LANG_SUGGEST = ["en", "es", "fr", "pt-BR", "ar", "zh-Hans"];
/* Suggested: the system's language (sysId), the system's other languages
   (tags, as the system lists them), then common ones, five in all. */
function langSuggested(sysId, tags) {
  const out = [sysId];
  for (const l of tags || []) { const id = langFor(l); if (id && !out.includes(id)) out.push(id); }
  for (const id of LANG_SUGGEST) if (out.length < 5 && !out.includes(id)) out.push(id);
  return out.filter((id) => LANGS.some(([l]) => l === id));
}
/* For a search: case and accents dropped ("Portugués" finds Português, Greek
   tonos too); the marks that make up Indian and Thai letters stay. */
const langFold = (s) => String(s).normalize("NFD").replace(/[̀-ͯ]/g, "").toLowerCase();
/* The languages a search finds, by own name, English name, code or another
   name (LANG_ALIAS): the code or a whole other name first ("de", "farsi"),
   then names that start with it, then a word in them that does, then any that
   hold it; each step by English name. */
function langFind(query) {
  const f = langFold(query.trim());
  if (!f) return LANGS.slice();
  const alias = {};
  for (const [name, ids] of Object.entries(LANG_ALIAS)) {
    const n = langFold(name);
    for (const id of ids) {
      const r = n === f ? 0 : f.length >= 3 && n.startsWith(f) ? 1 : -1;
      if (r >= 0 && (alias[id] === undefined || r < alias[id])) alias[id] = r;
    }
  }
  const rank = ([id, own]) => {
    const names = [langFold(own), langFold(langEn(id, own))];
    let r = -1;
    if (langFold(id) === f) r = 0;
    else if (names.some((n) => n.startsWith(f))) r = 1;
    else if (names.some((n) => n.split(/[\s(),]+/).some((w) => w.startsWith(f)))) r = 2;
    else if (names.concat(langFold(id)).some((n) => n.includes(f))) r = 3;
    if (alias[id] !== undefined && (r < 0 || alias[id] < r)) r = alias[id];
    return r;
  };
  return LANGS.map((l) => [rank(l), l]).filter(([r]) => r >= 0)
    .sort((a, b) => a[0] - b[0] || langEn(a[1][0], a[1][1]).localeCompare(langEn(b[1][0], b[1][1]), "en")).map(([, l]) => l);
}
const LANG_FIND_ICON = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true" focusable="false"><circle cx="7" cy="7" r="4.8"/><path d="M10.6 10.6L14 14"/></svg>`;
const LANG_CHECK = `<svg class="lck" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false"><path d="M3.5 8.5l3 3 6-7"/></svg>`;
/* The hello in the step's header, for the language `id`. */
const langHello = (id) => (HELLO[id] || HELLO.en) + (/^(ja|ko|zh)/.test(id) ? "" : "!");

/* Shared by the step and Settings' language menu (index.html's langPop). */
// A key that belongs to an input method (Japanese, Chinese, Korean…) is the
// input method's: Enter commits the word, Down picks a candidate, Escape
// drops the marked text. The Mac's WebKit sends that Enter after
// compositionend, with keyCode 229 and isComposing false. Returns whether a
// key in the field q is the input method's.
function langIme(q) {
  let composing = false;
  q.addEventListener("compositionstart", () => { composing = true; });
  q.addEventListener("compositionend", () => { composing = false; });
  return (ev) => composing || ev.isComposing || ev.keyCode === 229;
}
// Tab or Shift+Tab, without ⌥/⌘/Ctrl. WebKitGTK sends Shift+Tab as key
// "Unidentified" (GTK's ISO_Left_Tab), code "Tab".
const langTab = (ev) => (ev.key === "Tab" || ev.code === "Tab" || ev.keyCode === 9) && !ev.altKey && !ev.metaKey && !ev.ctrlKey;
const LANG_MOVE = ["ArrowDown", "ArrowUp", "PageDown", "PageUp", "Home", "End"];
/* The highlight in a list of .lrow options while the search field q keeps the
   focus (aria-activedescendant). set(el, scroll) highlights el (and scrolls it
   into view, under a sticky .lsect); key(ev) moves it with Up, Down, Page Up,
   Page Down, Home and End, and gives Enter's row to enter(row); act is the
   highlighted row's id. */
function langNav(q, list, enter) {
  const nav = { act: "" };
  nav.rows = () => [...list.querySelectorAll(".lrow")];
  nav.set = (el, scroll) => {
    list.querySelector(".lrow.act")?.classList.remove("act");
    nav.act = el?.id || "";
    if (!el) { q.removeAttribute("aria-activedescendant"); return; }
    el.classList.add("act");
    q.setAttribute("aria-activedescendant", nav.act);
    if (!scroll) return;
    const head = el.parentElement.querySelector(".lsect")?.offsetHeight || 0;
    if (el.offsetTop - head < list.scrollTop) list.scrollTop = el.offsetTop - head;
    else if (el.offsetTop + el.offsetHeight > list.scrollTop + list.clientHeight) list.scrollTop = el.offsetTop + el.offsetHeight - list.clientHeight;
  };
  nav.key = (ev) => {
    if (ev.key !== "Enter" && !LANG_MOVE.includes(ev.key)) return;
    if (ev.altKey || ev.metaKey || ev.ctrlKey) return;
    ev.preventDefault(); ev.stopPropagation();
    const all = nav.rows();
    const i = all.findIndex((r) => r.id === nav.act);
    if (ev.key === "Enter") { if (all[i]) enter(all[i]); return; }
    if (!all.length) return;
    const page = Math.max(1, Math.floor(list.clientHeight / (all[0].offsetHeight || 1)) - 1);
    const to = { ArrowDown: i + 1, ArrowUp: i - 1, PageDown: i + page, PageUp: i < 0 ? 0 : i - page, Home: 0, End: all.length - 1 }[ev.key];
    nav.set(all[Math.min(all.length - 1, Math.max(0, to))], true);
  };
  return nav;
}

/* The step itself, in `box`: box holds a .lhello, an element #lang-title and an
   empty .lbody, which this fills with a search field, one list and the foot
   with Continue. o: sysId (the system's language), os ("macOS", "Windows"…),
   tags (the system's languages, in order), ctrl (Ctrl rather than ⌘ is the
   shortcut key), current() (the language shown now), pick(id) (show and save
   it) and done() (Continue). Returns draw() (again, in the page's language
   now), open() (focus and show the picked row; once box is on screen) and
   close().

   A search field and one list: Suggested (the system's language, picked and
   marked, then its other languages and a few common ones), then every
   language by its English name. The search field keeps the focus, and Up,
   Down, Page Up, Page Down, Home and End move a highlight through the list
   (aria-activedescendant); Enter picks the row highlighted, and Enter on the
   row already picked is Continue. Tab goes between the search field and
   Continue and nowhere else (WebKit's Tab skips buttons unless the Mac has
   full keyboard access, and the window behind the step is out of reach);
   ⌘F / Ctrl+F is this search's. Escape clears a search, then closes, keeping
   the language shown, as Continue does. */
function langStep(box, o) {
  const body = box.querySelector(".lbody");
  body.innerHTML = `<label class="lfind">${LANG_FIND_ICON}<input id="lang-q" type="text" spellcheck="false" autocomplete="off" role="combobox" aria-controls="lang-list" aria-expanded="true" aria-autocomplete="list"></label>
      <div class="llist" id="lang-list" role="listbox" aria-labelledby="lang-title" tabindex="-1"></div>
      <div class="lfoot"><p></p><button type="button" class="btn primary" id="lang-go" tabindex="0"></button></div>
      <div id="lang-none" role="status" aria-live="polite" style="position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap"></div>`;
  const q = body.querySelector("#lang-q"), list = body.querySelector("#lang-list"), go = body.querySelector("#lang-go"), none = body.querySelector("#lang-none");
  const suggested = langSuggested(o.sysId, o.tags);
  let shown = "", open = true;
  const row = (key, [id, own], cur) => {
    const en = langEn(id, own);
    return `<div class="lrow" role="option" id="lo-${key}-${id}" data-lang="${id}" aria-selected="${id === cur}">
    <span class="lnm"><bdi class="ln" lang="${id}">${own}</bdi>${en !== own ? `<bdi class="le" lang="en">${en}</bdi>` : ""}${id === o.sysId ? `<bdi class="sys">${tr("Same as {os}", { os: o.os })}</bdi>` : ""}</span>
    <bdi class="lh" lang="${id}">${HELLO[id] || ""}</bdi>${LANG_CHECK}</div>`;
  };
  const group = (key, title, langs, cur) => `<div role="group" id="lg-${key}" aria-labelledby="lgh-${key}"><div class="lsect" id="lgh-${key}">${title}</div>${langs.map((l) => row(key, l, cur)).join("")}</div>`;
  // The row Up, Down and Enter work on: the search field keeps focus, and
  // aria-activedescendant says which row that is. Enter on the row already
  // picked is Continue.
  const nav = langNav(q, list, (r) => { if (r.dataset.lang === o.current()) finish(); else pick(r); });
  const setAct = nav.set;
  const drawList = (cur) => {
    const top = list.scrollTop;
    const query = q.value.trim();
    let missing = "";
    if (query) {
      const found = langFind(query);
      list.innerHTML = found.length ? found.map((l) => row("f", l, cur)).join("") : `<div class="lnone" role="presentation">${tr("No language matches. Try its English name.")}</div>`;
      if (!found.length) missing = tr("No languages match");
    } else {
      const byEnglish = [...LANGS].sort((a, b) => langEn(a[0], a[1]).localeCompare(langEn(b[0], b[1]), "en"));
      list.innerHTML = group("s", tr("Suggested"), suggested.map((id) => [id, langOwn(id)]), cur)
        + group("a", tr("All languages · {n}", { n: LANGS.length }), byEnglish, cur);
    }
    if (none.textContent !== missing) none.textContent = missing;
    list.scrollTop = top;
  };
  const draw = () => {
    const cur = o.current();
    if (shown !== cur) {
      // A new element, so the hello comes in again.
      const h = box.querySelector(".lhello"), n = document.createElement("div");
      n.className = "lhello"; n.lang = cur; n.setAttribute("aria-hidden", "true"); n.textContent = langHello(cur);
      h.replaceWith(n);
      shown = cur;
    }
    box.querySelector("#lang-title").textContent = tr("Choose your language");
    q.placeholder = tr("Search languages");
    q.setAttribute("aria-label", tr("Search languages"));
    body.querySelector(".lfoot p").textContent = tr("You can change it later in Settings → Appearance.");
    go.textContent = tr("Continue");
    drawList(cur);
    setAct(document.getElementById(nav.act) || list.querySelector(".lrow"), false);
  };
  const pick = (el) => {
    o.pick(el.dataset.lang);
    nav.act = el.id;
    draw();
  };
  // The row picked, highlighted and in view: on open, and when a search is cleared.
  const showPicked = () => {
    const on = list.querySelector(`[data-lang="${o.current()}"]`);
    if (!on) return;
    setAct(on, false);
    list.scrollTop = Math.max(0, on.offsetTop - (list.clientHeight - on.offsetHeight) / 2);
  };
  const close = () => { open = false; document.removeEventListener("keydown", onKey, true); };
  const finish = () => { if (!open) return; close(); o.done(); };
  const searched = () => {
    drawList(o.current());
    if (q.value.trim()) { list.scrollTop = 0; setAct(list.querySelector(".lrow"), false); } else showPicked();
  };
  // Input-method keys are left alone (langIme).
  const ime = langIme(q);
  // The list's keys, wherever the focus is in the step except on Continue
  // (Enter and Space press it).
  const listKey = nav.key;
  const onKey = (ev) => {
    // (The window's own Escape mustn't see the input method's either.)
    if (ime(ev)) { if (ev.key === "Escape") ev.stopPropagation(); return; }
    if (ev.key === "Escape") {
      ev.stopPropagation(); ev.preventDefault();
      if (q.value) { q.value = ""; searched(); q.focus(); } else finish();
      return;
    }
    // (langTab: WebKitGTK's Shift+Tab is key "Unidentified", code "Tab".)
    if (langTab(ev)) {
      ev.preventDefault(); ev.stopPropagation();
      (document.activeElement === q ? go : document.activeElement === go ? q : ev.shiftKey ? go : q).focus();
      return;
    }
    if ((o.ctrl ? ev.ctrlKey && !ev.metaKey : ev.metaKey && !ev.ctrlKey) && !ev.altKey && !ev.shiftKey && ["f", "k"].includes(ev.key.toLowerCase())) {
      ev.preventDefault(); ev.stopPropagation();
      q.focus(); q.select();
      return;
    }
    const on = document.activeElement;
    if (on === go) return;
    if (on === q) { listKey(ev); return; }
    // A letter typed with the focus off the field (on the step's text) goes to the search.
    if (ev.key.length === 1 && ev.key !== " " && !ev.metaKey && !ev.ctrlKey && !ev.altKey) { q.focus(); return; }
    listKey(ev);
  };
  document.addEventListener("keydown", onKey, true);
  q.addEventListener("input", searched);
  // A click picks and leaves the focus in the search field; a double click is
  // Continue too; a click on the step's text puts the focus back in the field.
  list.addEventListener("mousedown", (ev) => ev.preventDefault());
  // The first click redraws the list, so the second click of a double click lands on a new row:
  // count clicks on the click itself (detail) rather than waiting for a dblclick on the old one.
  list.addEventListener("click", (ev) => { const r = ev.target.closest(".lrow"); if (!r) return; pick(r); if (ev.detail >= 2) finish(); });
  box.addEventListener("click", (ev) => { if (open && q.isConnected && !ev.target.closest("button, input") && document.activeElement !== q) q.focus(); });
  go.addEventListener("click", finish);
  draw();
  return { draw, close, open: () => { q.focus(); showPicked(); } };
}
