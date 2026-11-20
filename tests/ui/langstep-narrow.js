// parity: macos windows linux
// window: 360x640
// The language step in a narrow window (360 px), in the languages with the
// longest words (Hungarian, the system's here, Finnish, German, Tamil…): nothing runs past the
// dialog's edge or out of its own box, and nothing scrolls sideways.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const dlg = () => document.querySelector('[aria-labelledby="lang-title"]');
const box = (el) => el.getBoundingClientRect();
// Text past its box: a range over the element's own text, wider than the box.
const over = () => {
  const d = dlg(), dr = box(d), out = [];
  for (const el of d.querySelectorAll("*")) {
    if (el.closest(".lcloud, svg") || getComputedStyle(el).display === "none") continue;
    const r = box(el);
    if (!r.width) continue;
    if (r.right > dr.right + 0.5 || r.left < dr.left - 0.5) out.push(`${el.id || el.className || el.tagName} ${Math.round(r.left - dr.left)}…${Math.round(r.right - dr.right)}`);
    // Inside its parent's box, unless placed on its own.
    const pa = el.parentElement, pr = box(pa), ps = getComputedStyle(pa);
    if (pa !== d && getComputedStyle(el).position !== "absolute" && (r.right > pr.right - parseFloat(ps.paddingRight) - parseFloat(ps.borderRightWidth) + 0.5 || r.left < pr.left + parseFloat(ps.paddingLeft) + parseFloat(ps.borderLeftWidth) - 0.5))
      out.push(`${el.className || el.tagName} "${el.textContent.trim().slice(0, 24)}" runs out of ${pa.id || pa.className || pa.tagName}`);
    if (el.scrollWidth > el.clientWidth + 1 && ["auto", "scroll"].includes(getComputedStyle(el).overflowX) && el.clientWidth) out.push(`${el.id || el.className || el.tagName} scrolls ${el.scrollWidth - el.clientWidth}px sideways`);
    const tx = [...el.childNodes].filter((n) => n.nodeType === 3 && n.textContent.trim());
    if (tx.length) {
      const rg = document.createRange(); rg.setStartBefore(tx[0]); rg.setEndAfter(tx.at(-1));
      const tr = rg.getBoundingClientRect(), cs = getComputedStyle(el);
      const inR = r.right - parseFloat(cs.borderRightWidth), inL = r.left + parseFloat(cs.borderLeftWidth);
      if (tr.right > inR + 0.5 || tr.left < inL - 0.5) out.push(`${el.id || el.className || el.tagName}'s text "${el.textContent.trim().slice(0, 24)}" runs ${Math.round(Math.max(tr.right - inR, inL - tr.left))}px out`);
    }
  }
  if (d.scrollWidth > d.clientWidth + 1) out.push(`the dialog scrolls ${d.scrollWidth - d.clientWidth}px sideways`);
  if (document.documentElement.scrollWidth > innerWidth) out.push("the window scrolls sideways");
  return [...new Set(out)];
};
t("the window is 360 wide", () => { if (innerWidth !== 360) throw new Error(innerWidth + "px"); });
// A Hungarian system: its row carries "Same as the system's (…)", the longest mark.
window.KIRI_LANG.os = ["hu-HU", "en-US"];
setPref("lang", "system");
kiri.on({ type: "langpick" });
for (const lang of ["system", "fi", "de", "ta", "ru", "el", "ar", "en"]) {
  setPref("lang", lang);
  for (const q of ["", "a"]) {
    const f = document.getElementById("lang-q"); f.value = q; f.dispatchEvent(new Event("input"));
    t(`${lang}${q ? ", searching" : ""}: nothing runs out of the dialog or its box`, () => { const o = over(); if (o.length) throw new Error(o.join("; ")); });
  }
}
dlg().querySelector("#lang-go").click();
window.KIRI_LANG.os = ["en-US"];
setPref("lang", "system");
report(R);
