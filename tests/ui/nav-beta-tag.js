// parity: macos windows linux
// The sidebar's Compose and USB entries, each with a two-digit count, in each of the 38 languages: both carry
// the plain outlined Beta tag in the language's word; the name is never cut by it; the tag is whole or hidden,
// never clipped; no New tag is left.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
ui.vm = "running";
const ctrs = Array.from({ length: 26 }, (_, i) => ({ Id: "c" + i, Names: [`/p${i}-web-1`], Image: "nginx", State: "running", Status: "Up 1 hour", Created: 1, Ports: [],
  Labels: { "com.docker.compose.project": "p" + i, "com.docker.compose.service": "web" } }));
kiri.on({ type: "lists", containers: ctrs, images: [], volumes: [], networks: [] });
kiri.on({ type: "usb", devices: Array.from({ length: 12 }, (_, i) => ({ key: "0x0" + i, name: "Dongle " + i, state: "kiri" })) });
const entry = (v) => { const b = document.querySelector(`nav button[data-view="${v}"]`); return { b, lbl: b.querySelector(".lbl"), tag: b.querySelector(".beta"), count: b.querySelector(".count") }; };
const how = {};
t("no New tag is left", () => { if (document.querySelector("nav .new, .beta.new")) throw new Error(document.querySelector("nav .new, .beta.new").outerHTML); });
for (const [id] of LANGS) t(`${id}: Compose and USB whole beside their Beta tags and counts`, () => {
  prefs.lang = id; setLang(id); setLang(id); render();
  if (LANG !== id) throw new Error("the window isn't in " + id);
  if (document.querySelector("nav .new")) throw new Error("a New tag");
  for (const [v, n] of [["compose", "26"], ["usb", "12"]]) {
    const { b, lbl, tag, count } = entry(v);
    if (count.textContent !== n) throw new Error(`${v} count ${count.textContent}`);
    if (!tag || tag.className.trim() !== "beta" + (tag.classList.contains("gone") ? " gone" : "")) throw new Error(`${v} tag ${tag?.outerHTML}`);
    if (lbl.scrollWidth > lbl.clientWidth) throw new Error(`${v} "${lbl.textContent}" cut: ${lbl.scrollWidth} > ${lbl.clientWidth}`);
    if (v === "compose" && lbl.textContent !== "Compose") throw new Error(lbl.textContent);
    const r = (el) => el.getBoundingClientRect(), box = r(b);
    for (const el of [lbl, tag, count]) if (el.offsetWidth && (r(el).left < box.left - 0.5 || r(el).right > box.right + 0.5)) throw new Error(`${v} ${el.className} leaves the entry`);
    if (tag.classList.contains("gone")) { if (tag.offsetWidth) throw new Error(`${v}: gone but shown`); how[`${id}/${v}`] = "gone"; continue; }
    if (tag.textContent !== tr("Beta")) throw new Error(`${v} tag "${tag.textContent}", want "${tr("Beta")}"`);
    if (tag.scrollWidth > tag.clientWidth || tag.offsetWidth < 8) throw new Error(`${v} tag "${tag.textContent}" clipped: ${tag.scrollWidth} > ${tag.clientWidth}`);
    how[`${id}/${v}`] = "word";
  }
});
t("both tags look the same", () => {
  prefs.lang = "en"; setLang("en"); render();
  const a = getComputedStyle(entry("compose").tag), b = getComputedStyle(entry("usb").tag);
  for (const k of ["color", "borderTopColor", "borderTopWidth", "fontSize", "fontWeight", "paddingLeft", "height", "backgroundColor", "marginInlineStart"]) if (a[k] !== b[k]) throw new Error(`${k}: ${a[k]} vs ${b[k]}`);
});
t("English has room for both words", () => { if (how["en/compose"] !== "word" || how["en/usb"] !== "word") throw new Error(JSON.stringify(how)); });
R.push("INFO hidden: " + (Object.entries(how).filter(([, v]) => v === "gone").map(([k]) => k).join(" ") || "none"));
report(R);
