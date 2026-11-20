// Switching the language and back leaves the window as it was: every view
// and Settings tab reads the same in English after a round trip through
// Chinese, and through System, as before it; in Chinese nothing is left from
// the English it switched from; and nothing breaks on the way.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
// The clock stands still here: Share says how long a link has been live, and
// on a slow machine the round trips below take long enough for "live 0 s" to
// become "live 1 s", which is no difference between the languages.
{ const now = Date.now(); Date.now = () => now; }
ui.images.push({ Id: "sha256:" + "ab".repeat(32), RepoTags: ["nginx:alpine"], Size: 5e7, Created: Date.now() / 1000 - 86400 });
const pick = (lang) => { show("settings"); document.querySelector('[data-stab="appearance"]').click(); const sel = document.getElementById("pref-lang"); sel.value = lang; sel.dispatchEvent(new Event("change")); };
// What a person reads: the text and the words in attributes, of the whole window.
const words = () => {
  const c = document.body.cloneNode(true);
  c.querySelectorAll("script, #results, .toasts").forEach((x) => x.remove());
  const attrs = [...c.querySelectorAll("[title], [aria-label], [placeholder]")].map((el) => [el.title, el.getAttribute("aria-label"), el.getAttribute("placeholder")].join("|"));
  return c.textContent.replace(/\s+/g, " ") + "\n" + attrs.join("\n");
};
const PLACES = [["containers"], ["images"], ["volumes"], ["networks"], ["builds"], ["share"], ["image", () => { show("images"); openImage("nginx:alpine"); }]];
for (const id of ["appearance", "resources", "network", "builds", "kubernetes", "import", "domains", "diagnostics", "about"])
  PLACES.push([`settings ${id}`, () => { show("settings"); document.querySelector(`[data-stab="${id}"]`).click(); }]);
const visit = ([name, go]) => (go ? go() : show(name));
// Back to the setting it started with at the end: Appearance shows the choice (System or English) on its Language button.
const was = prefs.lang || "system";
const before = new Map(PLACES.map((p) => { visit(p); return [p[0], words()]; }));
for (const round of ["zh-Hans", "system", "zh-Hans", "en", "zh-Hans"]) {
  pick(round);
  for (const p of PLACES) {
    visit(p);
    // In Chinese, nothing kept from the English it switched from.
    if (round === "zh-Hans") { const left = english(document.body); if (left.length) R.push(`FAIL ${p[0]} in Chinese (switched to, round ${R.length}): English left: ${left.slice(0, 5).map((x) => JSON.stringify(x)).join(", ")}`); }
  }
}
pick(was);
for (const p of PLACES) t(`${p[0]} reads as before after the round trip`, () => {
  visit(p);
  const now = words(), was = before.get(p[0]);
  if (now !== was) {
    let i = 0; while (i < now.length && now[i] === was[i]) i++;
    throw new Error(`differs at …${JSON.stringify(was.slice(Math.max(0, i - 30), i + 50))} → ${JSON.stringify(now.slice(Math.max(0, i - 30), i + 50))}`);
  }
});
t("System picks English for an English system", () => {
  pick("system");
  if (document.documentElement.lang !== "en") throw new Error("lang is " + document.documentElement.lang);
  if (!sent.some((m) => m.type === "setpref" && m.name === "lang" && m.value === "system")) throw new Error("System wasn't saved");
});
// Every language Settings offers: asked for from the host when the page lacks
// it, then the window in it (its sidebar, lang, CJK fonts and wrapping), and
// English again with nothing left behind.
t("every language comes from the host when picked, and shows", () => {
  for (const [id] of LANGS) {
    if (id === "en") continue;
    pick(id);
    if (document.documentElement.lang !== id) throw new Error(`${id}: lang is ${document.documentElement.lang}`);
    const nav = document.querySelector("nav [data-view=containers] .lbl").textContent;
    if (nav !== STRINGS[id]["Containers"]) throw new Error(`${id}: the sidebar says ${nav}`);
    if (document.documentElement.classList.contains("cjk") !== /^(zh|ja|ko)/.test(id)) throw new Error(`${id}: cjk is ${document.documentElement.classList.contains("cjk")}`);
    // Arabic alone turns the window right to left, with its own fonts first.
    const rtlLang = ["ar", "fa", "ckb"].includes(id);
    if (document.documentElement.dir !== (rtlLang ? "rtl" : "ltr")) throw new Error(`${id}: dir is ${document.documentElement.dir}`);
    if (getComputedStyle(document.documentElement).getPropertyValue("--ui-font").includes("Geeza Pro") !== rtlLang) throw new Error(`${id}: Arabic script's fonts`);
    const font = getComputedStyle(document.documentElement).getPropertyValue("--ui-font");
    const want = { ja: "Hiragino Sans", ko: "Malgun Gothic", "zh-Hant": "PingFang TC", "zh-Hans": "PingFang SC", hi: "Kohinoor Devanagari", bn: "Kohinoor Bangla", ta: "Tamil Sangam MN", am: "Kefa", te: "Kohinoor Telugu", th: "Thonburi", lo: "Lao Sangam MN" }[id] || "PingFang SC";
    // Hindi, Bengali and Tamil each with their own script's fonts, and only them.
    if (/^(hi|bn|ta|te)$/.test(id) && !font.includes("Nirmala UI")) throw new Error(`${id}: no Nirmala UI in ${font}`);
    if (!/^(hi|bn|ta|te)$/.test(id) && font.includes("Nirmala UI")) throw new Error(`${id}: Indian fonts in ${font}`);
    if (!font.includes(want)) throw new Error(`${id}: the font is ${font}`);
  }
  pick("en");
  const now = (visit(PLACES[0]), words());
  if (now !== before.get(PLACES[0][0])) throw new Error("containers differ after every language");
  if (document.documentElement.dir !== "ltr") throw new Error("still right to left in English");
});
t("an answer for a language no longer picked changes nothing", () => {
  hostHold = true;
  pick("ko"); pick("en");
  hostHold = false; hostFlush();
  if (document.documentElement.lang !== "en") throw new Error("lang is " + document.documentElement.lang);
});
t("tags map to the languages Kiri has, as lang.rs maps them", () => {
  const want = { zh: "zh-Hans", "zh-CN": "zh-Hans", "zh-SG": "zh-Hans", "zh-Hans-HK": "zh-Hans", zh_TW: "zh-Hant", "zh-TW": "zh-Hant", "zh-HK": "zh-Hant", "zh-MO": "zh-Hant", "zh-Hant": "zh-Hant", "zh-Hant-CN": "zh-Hant",
    zhx: "", in: "id", "in-ID": "id", "id-ID": "id", iw: "", "iw-IL": "", "ja-JP": "ja", "ko-KR": "ko", "ru-RU": "ru", uk: "uk", "uk-UA": "uk", uk_UA: "uk", hi: "hi", "hi-IN": "hi", "bn-IN": "bn", "bn-BD": "bn", "ta-IN": "ta", "ta-LK": "ta", "te-IN": "te", am: "am", "am-ET": "am", "th-TH": "th", "lo-LA": "lo", "ca-ES": "ca", "cs-CZ": "cs", "lt-LT": "lt", "hu-HU": "hu", "nb-NO": "nb", no: "nb", "nn-NO": "nb", "ro-RO": "ro", "sq-AL": "sq", "sl-SI": "sl", "fi-FI": "fi", "el-GR": "el", "bg-BG": "bg", "kk-KZ": "kk", "fa-IR": "fa", ckb: "ckb", "ku-Arab-IQ": "ckb", ku: "", "pl-PL": "pl", "tr-TR": "tr", "vi-VN": "vi", "it-CH": "it", "pt-PT": "pt-BR", "fr-CA": "fr", "en-GB": "en", nl: "", ar: "ar", "ar-SA": "ar", "ar-EG": "ar", ar_AE: "ar", arz: "", he: "" };
  const bad = Object.entries(want).filter(([tag, id]) => langFor(tag) !== id).map(([tag, id]) => `${tag} → ${langFor(tag)}, not ${id}`);
  if (bad.length) throw new Error(bad.join("; "));
});
report(R);
