// parity: macos windows linux
// window: 760x480
// Settings › Appearance › Language: a button showing the choice that opens a
// short searchable list (the language step's search, other names and keys),
// not a 39-row menu. The hidden <select id="pref-lang"> stays the choice.
{
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const ITEMS = [];
const lbtn = () => document.getElementById("pref-lang-btn");
const lsel = () => document.getElementById("pref-lang");
const lpop = () => document.getElementById("lang-pop");
const lq = () => document.getElementById("lp-q");
const llist = () => document.getElementById("lp-list");
const lrows = () => [...(llist()?.querySelectorAll('[role="option"]') || [])];
const lids = () => lrows().map((r) => r.dataset.lang);
const L_kd = (el, k, more = {}) => { const e = new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true, ...more }); if (more.keyCode) Object.defineProperty(e, "keyCode", { get: () => more.keyCode }); el.dispatchEvent(e); return e; };
const L_comp = (type, data = "") => lq().dispatchEvent(new CompositionEvent(type, { bubbles: true, data }));
const L_type = (s) => { lq().value = s; lq().dispatchEvent(new Event("input", { bubbles: true })); };
const L_active = () => document.getElementById(lq().getAttribute("aria-activedescendant") || "-")?.dataset.lang;
const L_saved = () => sent.filter((m) => m.type === "setpref" && m.name === "lang").at(-1)?.value;
const L_box = (el) => el.getBoundingClientRect();
const L_need = (ok, msg) => { if (!ok) throw new Error(msg); };
const L_goto = () => { show("settings"); document.querySelector('[data-stab="appearance"]').click(); };
const L_open = () => { if (!lpop()) lbtn().click(); L_need(lpop(), "the list didn't open"); };
const L_shut = () => { while (lpop()) L_kd(lq(), "Escape"); };
const L_inView = (r) => { const l = L_box(llist()), b = L_box(r); return b.top >= l.top - 1 && b.bottom <= l.bottom + 1; };

setPref("lang", "system");
sent.length = 0;
L_goto();
t("the window is 760×480", () => L_need(innerWidth === 760 && innerHeight === 480, `${innerWidth}×${innerHeight}`));
t("a button shows the choice and says it opens a list; the menu itself is hidden", () => {
  const b = lbtn();
  L_need(b && b.tagName === "BUTTON", "no button");
  L_need(b.textContent.trim() === `${tr("System")} (English)`, "button reads " + JSON.stringify(b.textContent));
  L_need(b.getAttribute("aria-haspopup") === "listbox" && b.getAttribute("aria-expanded") === "false", "aria-haspopup/aria-expanded");
  L_need(b.querySelector("svg"), "no chevron");
  const s = lsel();
  L_need(s && s.value === "system" && s.options.length === LANGS.length + 1, "the select is gone or changed");
  L_need(s.getAttribute("aria-hidden") === "true" && s.inert && s.tabIndex === -1, "the select is still in reach");
  L_need(L_box(s).width <= 1 && L_box(s).height <= 1, "the select still shows");
  L_need(L_box(b).height <= 30, "button is " + L_box(b).height + "px high");
});
t("a click opens a short list: search focused, System first, the choice ticked, about 8 rows that scroll", () => {
  L_open();
  { const p = L_box(lpop()), b = L_box(lbtn());
    L_need(Math.abs(p.left - b.left) < 2 || (Math.abs(innerWidth - 8 - p.right) < 1 && p.left < b.left), `list ${p.left}–${p.right} vs button ${b.left}–${b.right}: not lined up with its left edge`); }
  L_need(lbtn().getAttribute("aria-expanded") === "true", "aria-expanded not true");
  L_need(document.activeElement === lq(), "focus is on " + document.activeElement?.id);
  L_need(lq().getAttribute("role") === "combobox" && lq().getAttribute("aria-controls") === "lp-list" && llist().getAttribute("role") === "listbox", "combobox/listbox roles");
  L_need(lids()[0] === "system" && lids().length === LANGS.length + 1, "rows: " + lids().slice(0, 3));
  L_need(lrows()[0].getAttribute("aria-selected") === "true" && lrows().filter((r) => r.getAttribute("aria-selected") === "true").length === 1, "System isn't the one ticked");
  L_need(L_active() === "system", "highlight on " + L_active());
  const h = lrows()[1].offsetHeight, n = llist().clientHeight / h;
  L_need(n >= 6 && n <= 9, `the list is ${n.toFixed(1)} rows tall`);
  L_need(llist().scrollHeight > llist().clientHeight, "the list doesn't scroll");
  const de = lrows().find((r) => r.dataset.lang === "de");
  L_need(de.querySelector('[lang="de"]').textContent === "Deutsch" && de.textContent.includes("German"), "a row shows its own and English name");
  ITEMS.push(...[lbtn(), lq(), llist(), document.getElementById("lp-none")].map((e) => `language menu: ${e.tagName.toLowerCase()} ${e.id} ${e.getAttribute("role") || ""}`.trim()));
});
t("Escape with nothing typed closes, and the focus goes back to the button", () => {
  const e = L_kd(lq(), "Escape");
  L_need(e.defaultPrevented && !lpop(), "still open");
  L_need(document.activeElement === lbtn() && lbtn().getAttribute("aria-expanded") === "false", "focus on " + document.activeElement?.id);
});
t("Down on the button opens it too", () => { lbtn().focus(); L_kd(lbtn(), "ArrowDown"); L_need(lpop() && document.activeElement === lq(), "didn't open"); L_shut(); });
t("typing finds by own name, English name, accents, code and other names", () => {
  L_open();
  for (const [s, want] of [["farsi", "fa"], ["castellano", "es"], ["हिंदी", "hi"], ["german", "de"], ["portugues", "pt-BR"], ["deutsch", "de"], ["ja", "ja"]]) {
    L_type(s);
    L_need(lids()[0] === want, `${s} finds ${lids().slice(0, 3)}`);
    L_need(L_active() === want, `${s}: highlight on ${L_active()}`);
  }
  L_type("mandarin");
  L_need(lids().includes("zh-Hans") && lids().includes("zh-Hant") && lids().length === 2, "mandarin finds " + lids());
  L_type("sys");
  L_need(lids()[0] === "system", "sys finds " + lids());
});
t("nothing found: the list says so and so does the live region", () => {
  L_type("zzqx");
  L_need(!lrows().length && llist().textContent.includes(tr("No language matches. Try its English name.")), "no message in the list");
  const n = document.getElementById("lp-none");
  L_need(n.getAttribute("aria-live") === "polite" && n.getAttribute("role") === "status" && n.textContent === tr("No languages match"), "live region: " + n.textContent);
  L_type("");
  L_need(n.textContent === "" && lrows().length === LANGS.length + 1, "clearing doesn't bring the list back");
});
t("Down, Up, Page Down, Page Up, Home and End move the highlight and keep it in view", () => {
  L_kd(lq(), "Home"); L_need(L_active() === "system", "Home → " + L_active());
  L_kd(lq(), "ArrowDown"); L_need(L_active() === LANGS[0][0], "Down → " + L_active());
  L_kd(lq(), "ArrowUp"); L_need(L_active() === "system", "Up → " + L_active());
  L_kd(lq(), "PageDown"); const i = lids().indexOf(L_active()); L_need(i >= 4 && i <= 9, "Page Down → row " + i);
  L_kd(lq(), "PageUp"); L_need(L_active() === "system", "Page Up → " + L_active());
  L_kd(lq(), "End"); L_need(L_active() === LANGS.at(-1)[0], "End → " + L_active());
  L_need(L_inView(document.getElementById(lq().getAttribute("aria-activedescendant"))), "the last row isn't in view");
  L_need(lpop() && !L_saved(), "a move picked or closed");
});
t("input-method keys are left alone (Mac WebKit's Enter after compositionend, keyCode 229)", () => {
  L_type("");
  L_comp("compositionstart");
  const d = L_kd(lq(), "ArrowDown", { isComposing: true, keyCode: 229 });
  const esc1 = L_kd(lq(), "Escape", { isComposing: true, keyCode: 229 });
  L_need(!d.defaultPrevented && !esc1.defaultPrevented && lpop(), "the list took a composing key");
  lq().value = "日本"; L_comp("compositionend", "日本"); lq().dispatchEvent(new Event("input", { bubbles: true }));
  const e = L_kd(lq(), "Enter", { keyCode: 229 });
  L_need(!e.defaultPrevented && lpop() && !L_saved(), "Enter that commits the word picked a language");
  L_need(lids()[0] === "ja", "日本 finds " + lids());
  L_kd(lq(), "Escape");
});
t("Escape clears the search first, then closes", () => {
  L_type("fr");
  L_kd(lq(), "Escape");
  L_need(lpop() && lq().value === "" && document.activeElement === lq() && lrows().length === LANGS.length + 1, "first Escape didn't just clear");
  L_kd(lq(), "Escape");
  L_need(!lpop() && document.activeElement === lbtn(), "second Escape didn't close");
});
t("Tab and WebKitGTK's Shift+Tab (key Unidentified, code Tab) close to the button", () => {
  L_open();
  const e = L_kd(lq(), "Unidentified", { code: "Tab", keyCode: 9, shiftKey: true });
  L_need(e.defaultPrevented && !lpop() && document.activeElement === lbtn(), "GTK Shift+Tab left focus on " + document.activeElement?.id);
  L_open();
  const e2 = L_kd(lq(), "Tab");
  L_need(e2.defaultPrevented && !lpop() && document.activeElement === lbtn(), "Tab");
});
t("a click outside closes; a click on the button closes it too", () => {
  L_open();
  document.querySelector('[data-stab="about"]').dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, button: 0 }));
  document.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, button: 0 }));
  L_need(!lpop() && document.activeElement === lbtn(), "click outside");
  L_open();
  lbtn().dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, button: 0 }));
  document.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, button: 0 }));
  L_need(lpop(), "pointerdown on the button closed it");
  lbtn().click();
  L_need(!lpop() && lbtn().getAttribute("aria-expanded") === "false", "second click left it open");
});
t("Enter picks: saved, the menu's value set, the window in that language, closed, focus on the button", () => {
  sent.length = 0;
  L_open(); L_type("deutsch");
  L_kd(lq(), "Enter");
  L_need(L_saved() === "de" && prefs.lang === "de", "saved " + L_saved());
  L_need(lsel().value === "de", "select is " + lsel().value);
  L_need(!lpop() && document.activeElement === lbtn(), "not closed to the button");
  L_need(document.documentElement.lang === "de" && tr("Appearance") !== "Appearance" && document.querySelector("#content .pane h3").textContent === tr("Appearance"), "window isn't in German");
  L_need(lbtn().textContent.trim() === "Deutsch", "button reads " + lbtn().textContent);
  L_open();
  L_need(lrows().find((r) => r.dataset.lang === "de").getAttribute("aria-selected") === "true" && L_active() === "de", "Deutsch not ticked on reopening");
});
t("a click on a row picks it", () => {
  lrows().find((r) => r.dataset.lang === "en").click();
  L_need(L_saved() === "en" && lsel().value === "en" && !lpop() && document.documentElement.lang === "en", "click didn't pick English");
});
t("the menu still works by itself (value + change), and the button follows", () => {
  lsel().value = "fr"; lsel().dispatchEvent(new Event("change"));
  L_need(lbtn().textContent.trim() === "Français" && L_saved() === "fr", "button reads " + lbtn().textContent);
  lsel().value = "system"; lsel().dispatchEvent(new Event("change"));
  L_need(lbtn().textContent.trim().startsWith(tr("System")), "back to System");
});
t("right to left: in Arabic the list hangs from the button's right edge", () => {
  lsel().value = "ar"; lsel().dispatchEvent(new Event("change"));
  L_goto(); L_open();
  L_need(document.documentElement.dir === "rtl" && getComputedStyle(lq()).direction === "rtl", "not rtl");
  const p = L_box(lpop()), b = L_box(lbtn());
  // Lined up with the button's right edge, unless that would cross the window's left edge (8 px kept).
  L_need(Math.abs(p.right - b.right) < 2 || (Math.abs(p.left - 8) < 1 && p.right > b.right), `list ${p.left}–${p.right} vs button ${b.left}–${b.right}`);
  L_need(p.right >= b.right - 1, "the list starts left of the button's right edge");
  L_need(p.left >= 0 && p.right <= innerWidth, "off screen");
  L_shut();
  lsel().value = "system"; lsel().dispatchEvent(new Event("change"));
  L_goto();
});
// The list all on screen, nothing of it under something else.
const L_fits = (label) => t(`${label}: the open list is inside the window and shows at least 3 rows`, () => {
  L_open();
  const p = L_box(lpop());
  L_need(p.left >= 0 && p.top >= 0 && p.right <= innerWidth && p.bottom <= innerHeight, `list at ${Math.round(p.left)},${Math.round(p.top)}–${Math.round(p.right)},${Math.round(p.bottom)} in ${innerWidth}×${innerHeight}`);
  for (const [x, y] of [[p.left + 4, p.top + 4], [p.right - 4, p.top + 4], [p.left + 4, p.bottom - 4], [p.right - 4, p.bottom - 4]])
    L_need(lpop().contains(document.elementFromPoint(x, y)), `at ${Math.round(x)},${Math.round(y)}: ${document.elementFromPoint(x, y)?.className}`);
  L_need(L_box(llist()).height >= 3 * L_box(lrows()[0]).height - 1, "list shows under 3 rows");
  L_need(document.documentElement.scrollWidth <= innerWidth, "the page scrolls sideways");
  L_shut();
});
// The button near the window's foot: the list opens above it.
const L_flips = (label) => t(`${label}: near the window's foot the list opens above the button`, () => {
  const pref = lbtn().closest(".pref"), z = L_box(lbtn()).height / lbtn().offsetHeight;
  pref.style.marginTop = `${(innerHeight - 40 - L_box(lbtn()).bottom) / z}px`;
  L_need(L_box(lbtn()).bottom <= innerHeight && L_box(lbtn()).bottom > innerHeight - 60, "couldn't put the button low: " + L_box(lbtn()).bottom);
  L_open();
  const p = L_box(lpop()), b = L_box(lbtn());
  L_need(p.bottom <= b.top + 1 && p.top >= 0, `list ${Math.round(p.top)}–${Math.round(p.bottom)}, button top ${Math.round(b.top)}`);
  L_need(L_box(llist()).height >= 3 * L_box(lrows()[0]).height - 1, "list shows under 3 rows");
  L_shut();
  pref.style.marginTop = "";
});
L_fits("100%");
L_flips("100%");
setPref("scale", 1.25);
L_goto();
L_fits("125%");
L_flips("125%");
setPref("lang", "de");
L_goto();
L_fits("125%, in German");
setPref("scale", 1);
setPref("lang", "system");
report([...R, ...ITEMS.map((i) => "ITEM " + i)]);
}
