// parity: macos windows linux
// window: 800x600
// The project page's tab bar and its buttons never overlap, in every language: the edited
// file (Apply and Discard showing), and the Services tab with its pills. Each tab's centre
// is the tab itself, and no button's box crosses a tab's.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const ALL = LANGS.map((l) => l[0]);
t("every language", () => { if (ALL.length < 38 || !ALL.includes("fr")) throw new Error(ALL.length + " languages"); });
const now = Math.floor(Date.now() / 1000);
const svc = (name) => ({ name, image: "alpine:3.20", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
const st = { web: "running", api: "running", jobs: "exited", mail: "exited" };
ui.vm = "running";
kiri.on({ type: "lists", images: [], volumes: [], networks: [], containers: Object.entries(st).map(([s, x]) => ({ Id: "shop-" + s, Names: [`/shop-${s}-1`], Image: "alpine:3.20", State: x, Created: now - 99,
  Status: x === "running" ? "Up 3 hours" : "Exited (1) 2 minutes ago", KiriExitCode: x === "running" ? 0 : 1, Ports: [], Labels: { "com.docker.compose.project": "shop", "com.docker.compose.service": s } })) });
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: now - 99, services: [...Object.keys(st), "idle"].map(svc),
  containers: Object.entries(st).map(([s, x]) => ({ service: s, id: "shop-" + s, name: `shop-${s}-1`, image: "alpine:3.20", state: x, status: "Up" })) }] });
const lang_ = (l) => { prefs.lang = l; setLang(l); setLang(l); if (LANG !== l) throw new Error("the window isn't in " + l); };
const hit = (a, b) => a.left < b.right - 0.5 && b.left < a.right - 0.5 && a.top < b.bottom - 0.5 && b.top < a.bottom - 0.5;
const check = (where) => {
  const bar = $("cmp-tabs"), tabs = [...bar.querySelectorAll(":scope > button[data-cmptab]")], btns = [...bar.querySelectorAll("#cmp-acts button, .cmp-sf button")].filter((b) => b.offsetWidth);
  if (!tabs.length) throw new Error(where + ": no tabs");
  const bad = [];
  for (const tb of tabs) {
    const r = tb.getBoundingClientRect(), at = document.elementFromPoint((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    if (at?.closest("[data-cmptab]") !== tb) bad.push(`"${tb.textContent}" centre is ${at?.outerHTML.slice(0, 60)}`);
    for (const b of btns) if (hit(r, b.getBoundingClientRect())) bad.push(`"${tb.textContent}" crossed by "${b.textContent || b.title}"`);
  }
  for (const b of btns) { const r = b.getBoundingClientRect(); if (r.right > innerWidth + 0.5 || r.left < -0.5) bad.push(`"${b.textContent}" off the window`); }
  if (bad.length) throw new Error(`${where} (${innerWidth} px): ${bad.join("; ")}`);
};
for (const lang of ALL) {
  t(`${lang}: the edited file, and the Services tab with its pills`, () => {
    lang_(lang);
    cmpOpen("shop");
    kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", mtime: 1, hash: "h1", text: "services:\n  web:\n    image: alpine:3.20\n" });
    click(document.querySelector('[data-cmptab="file"]'));
    const ta = $("cmp-ta"); ta.value = ta.value.replace("3.20", "3.21"); ta.dispatchEvent(new Event("input"));
    if (!document.querySelector("#cmp-acts [data-cmpdiscard]")) throw new Error("no Discard");
    check("file, edited");
    click(document.querySelector('[data-cmptab="services"]'));
    if (!document.querySelector("#cmp-tabs .cmp-sf button")) throw new Error("no pills");
    check("services, edited");
    click(document.querySelector("#cmp-acts [data-cmpdiscard]"));
    check("services");
  });
}
report(R);
