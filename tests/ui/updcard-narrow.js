// parity: macos windows linux
// window: 640x520
// The update card fits a narrow window in every language: the card stays inside the window,
// its buttons wrap inside it, and no button's words are cut off (Skip this version too),
// whether it offers a release, shows Snooze's two choices, is ready to install or asks to
// restart the engine.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const ALL = LANGS.map((l) => l[0]);
t("every language", () => { if (ALL.length < 38 || !ALL.includes("ar")) throw new Error(ALL.length + " languages"); });
kiri.on({ type: "status", version: "0.2.2", app: "Kiri", ours: "kiri", context: "kiri" });
const release = { version: "0.2.3", notes: "## New\n- Compose projects show each service's ports\n## Fixes\n- Logs keep their place", notes_url: "https://runkiri.com/releases/0.2.3/" };
const states = {
  available: () => { ui.engineStale = null; ui.upd = null; kiri.on({ type: "update", manual: true, current: "0.2.2", release }); },
  snoozing: () => { states.available(); updateAction("snooze"); },
  ready: () => { ui.engineStale = null; ui.upd = { phase: "ready", version: "0.2.3" }; renderUpdate(); },
  engine: () => { ui.upd = null; ui.engineStale = "0.2.1"; ui.staleHidden = false; renderUpdate(); },
};
const check = (where) => {
  const card = document.querySelector("#updcard .updcard");
  if (!card) throw new Error(where + ": no card");
  const c = card.getBoundingClientRect(), bad = [];
  if (c.left < -0.5 || c.right > innerWidth + 0.5) bad.push(`card ${Math.round(c.left)}–${Math.round(c.right)} off the window`);
  for (const b of card.querySelectorAll("button")) {
    const r = b.getBoundingClientRect(), name = b.textContent.trim() || b.getAttribute("aria-label");
    if (!r.width) continue;
    if (r.left < c.left - 0.5 || r.right > c.right + 0.5) bad.push(`"${name}" ${Math.round(r.left)}–${Math.round(r.right)} outside the card`);
    if (b.scrollWidth > b.clientWidth + 1) bad.push(`"${name}" cut off (${b.scrollWidth} > ${b.clientWidth})`);
  }
  if (bad.length) throw new Error(`${where} (${innerWidth} px): ${bad.join("; ")}`);
};
for (const lang of ALL) {
  t(`${lang}: the card and its buttons fit the window`, () => {
    prefs.lang = lang; setLang(lang); setLang(lang);
    if (LANG !== lang) throw new Error("the window isn't in " + lang);
    for (const [name, set] of Object.entries(states)) { set(); check(name); }
  });
}
report(R);
