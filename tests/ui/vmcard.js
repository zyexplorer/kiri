// The Linux VM card at the foot of the sidebar fits the sidebar in every
// language and engine state, and keeps its words whole: when the name and the
// state don't fit side by side (Polish's "Maszyna wirtualna Linux", Kazakh's
// "Жұмыс істеп тұр"), the state moves to its own line. Long readings wrap
// rather than widen the card. Only a word longer than the whole card is cut,
// and then it says itself on hover.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const pick = (lang) => { show("settings"); document.querySelector('[data-stab="appearance"]').click(); const sel = document.getElementById("pref-lang"); sel.value = lang; sel.dispatchEvent(new Event("change")); };
const langs = LANGS.map(([id]) => id);
const STATES = ["stopped", "booting", "running", "stopping"];
const reading = () => kiri.on({ type: "vm", awake: true, used_mib: 12698, target_mib: 13000, ceiling_mib: 65536, running: 128, cpus: 16, cpu_pct: 100 });
const card = document.querySelector(".vm");
const cut = (el) => el.scrollWidth > el.clientWidth + 1;
const wordWidth = (el) => { const r = document.createRange(); r.selectNodeContents(el); return r.getBoundingClientRect().width; };
// A word (split at ordinary spaces; a no-break space joins its parts) drawn on two lines has two boxes.
const splitWords = (el) => {
  const out = [], t = el.firstChild;
  if (!t || t.nodeType !== 3) return out;
  // Words are what's between spaces. Thai, Lao, Khmer and Burmese write no spaces between words,
  // so there a line may break between two words: split those runs into words with Intl.Segmenter.
  const noSpace = /[\u0E00-\u0EFF\u1000-\u109F\u1780-\u17FF]/;
  const seg = new Intl.Segmenter(document.documentElement.lang || undefined, { granularity: "word" });
  const words = [];
  const re = /[^ ]+/g; let m;
  while ((m = re.exec(t.data))) {
    if (!noSpace.test(m[0])) { words.push(m); continue; }
    for (const w of seg.segment(m[0])) if (w.isWordLike) { const x = [w.segment]; x.index = m.index + w.index; words.push(x); }
  }
  for (m of words) {
    const r = document.createRange(); r.setStart(t, m.index); r.setEnd(t, m.index + m[0].length);
    const tops = new Set([...r.getClientRects()].filter((b) => b.width > 0).map((b) => Math.round(b.top)));
    if (tops.size > 1) out.push(m[0]);
  }
  return out;
};
const bad = [];
let checked = 0;
for (const lang of langs) {
  pick(lang);
  for (const st of STATES) {
    setVm(st);
    reading();
    checked++;
    const side = document.querySelector("aside").getBoundingClientRect();
    const box = card.getBoundingClientRect();
    const pad = parseFloat(getComputedStyle(card).paddingLeft);
    const inner = { left: box.left + pad - 0.5, right: box.right - pad + 0.5 };
    const where = `${lang} ${st}`;
    if (box.left < side.left - 0.5 || box.right > side.right + 0.5) bad.push(`${where}: card past the sidebar`);
    for (const el of [$("vm-name"), $("vm-state"), $("vm-running"), $("vm-cpus"), $("vm-mem"), $("vm-toggle")]) {
      const r = el.getBoundingClientRect();
      if (r.left < inner.left || r.right > inner.right) bad.push(`${where}: ${el.id} past the card's edge`);
    }
    const name = $("vm-name"), state = $("vm-state");
    const full = inner.right - inner.left;
    if (state.getBoundingClientRect().width < 1 && state.textContent) bad.push(`${where}: the state is gone`);
    if (cut(state) && wordWidth(state) < full - 1) bad.push(`${where}: state cut though it fits the card (${state.textContent})`);
    if (cut(name)) bad.push(`${where}: name cut (${name.textContent})`);
    if (name.getClientRects().length && name.getBoundingClientRect().height < 1) bad.push(`${where}: the name is gone`);
    for (const el of [name, state]) if (!!el.title !== cut(el)) bad.push(`${where}: ${el.id} title ${el.title ? "on a whole word" : "missing on a cut word"}`);
    for (const el of [$("vm-cpus"), $("vm-mem")]) if (cut(el)) bad.push(`${where}: ${el.id} cut`);
    for (const el of [name, state, $("vm-mem"), ...card.querySelectorAll("dt")]) { const w = splitWords(el); if (w.length) bad.push(`${where}: ${el.id || el.textContent} split inside ${JSON.stringify(w)}`); }
    // The dot sits on the name's first line, never alone above it.
    const dot = card.querySelector(".dot").getBoundingClientRect(), first = name.getClientRects()[0];
    if (first && ((dot.top + dot.bottom) / 2 < first.top || (dot.top + dot.bottom) / 2 > first.bottom)) bad.push(`${where}: the dot is not on the name's first line`);
  }
}
t(`${langs.length} languages × ${STATES.length} states (${checked}): the card fits and its words are whole`, () => { if (bad.length) throw new Error(bad.slice(0, 12).join("; ") + (bad.length > 12 ? ` … and ${bad.length - 12} more` : "")); });
// The big readings and other fonts the last judge found splitting "GB": every language, Running.
const bad2 = [];
for (const font of ["", "Verdana", "Courier New", "DejaVu Sans"]) {
  setPref("font", font);
  for (const lang of langs) {
    pick(lang); setVm("running");
    kiri.on({ type: "vm", awake: true, used_mib: 102400, target_mib: 110000, ceiling_mib: 262144, running: 128, cpus: 16, cpu_pct: 100 });
    const side = document.querySelector("aside").getBoundingClientRect(), box = card.getBoundingClientRect();
    if (box.right > side.right + 0.5) bad2.push(`${font || "default"} ${lang}: card past the sidebar`);
    const w = splitWords($("vm-mem")); if (w.length) bad2.push(`${font || "default"} ${lang}: memory split inside ${JSON.stringify(w)}`);
    if (cut($("vm-mem"))) bad2.push(`${font || "default"} ${lang}: memory cut`);
  }
}
setPref("font", "");
t("100 GB of 256 GB in four fonts: the reading is whole and inside", () => { if (bad2.length) throw new Error(bad2.slice(0, 10).join("; ")); });
t("Polish: the state is shown and in Polish after the switch", () => {
  pick("en"); setVm("running"); pick("pl");
  if ($("vm-state").textContent !== tr("Running") || tr("Running") === "Running") throw new Error(JSON.stringify($("vm-state").textContent));
  if ($("vm-toggle").textContent !== tr("Stop")) throw new Error($("vm-toggle").textContent);
  if ($("vm-state").getBoundingClientRect().width < 1) throw new Error("state not shown");
});
t("English: name and state share one line, no titles", () => {
  pick("en"); setVm("running");
  const a = $("vm-name").getBoundingClientRect(), b = $("vm-state").getBoundingClientRect();
  if (Math.abs(a.top - b.top) > 4) throw new Error("state wrapped in English");
  if ($("vm-name").title || $("vm-state").title) throw new Error("title on a whole word");
});
t("memory wraps before the slash, never between a number and its unit", () => {
  pick("pl"); setVm("running");
  kiri.on({ type: "vm", awake: true, used_mib: 812, target_mib: 1200, ceiling_mib: 8192, running: 3, cpus: 2, cpu_pct: 4 });
  const m = $("vm-mem").textContent;
  if (m !== "812\u00a0MB /\u00a08\u00a0GB") throw new Error(JSON.stringify(m));
  pick("en");
});
report(R);
