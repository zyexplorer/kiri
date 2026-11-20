// lang: ar
// The window in Arabic, which reads right to left: the sidebar is on the
// right and tables start there; every view and Settings tab reads Arabic;
// names, sizes and ports keep their own order inside Arabic sentences; logs
// stay left to right; arrows and menus follow the reading direction; and
// English puts everything back.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const box = (sel) => { const el = typeof sel === "string" ? document.querySelector(sel) : sel; if (!el) throw new Error("no " + sel); return el.getBoundingClientRect(); };
// Arabic laid out left to right. Either an element's own text opens with a
// Latin letter and no right-to-left mark (what an island holds, U+2068 to
// U+2069, doesn't say which way its line runs); or it has no letters of its
// own, only pieces side by side (two buttons, a title and its note), one of
// them Arabic, and the first piece is on the left.
const leftToRight = () => {
  const out = [];
  for (const el of document.querySelectorAll("body *")) {
    if (el.closest("[hidden], script, style, svg, .console, .xterm, .term, pre, .yard, .is-art, .sh-lane, .lcloud") || !el.getClientRects().length) continue;
    const own = [...el.childNodes].filter((n) => n.nodeType === 3).map((n) => n.nodeValue).join(" ").replace(/\u2068[^\u2069]*\u2069/g, " ");
    const first = own.match(/[A-Za-z\u00C0-\u024F\u0600-\u06FF\u200F]/)?.[0];
    if (/[\u0600-\u06FF]/.test(own) && /[A-Za-z\u00C0-\u024F]/.test(first)) out.push(own.trim().replace(/\s+/g, " ").slice(0, 60));
    if (first || !/[\u0600-\u06FF]/.test(el.textContent) || /flex|grid|table|contents/.test(getComputedStyle(el).display)) continue;
    const [a, b] = [...el.children].filter((c) => !/^(br|svg)$/i.test(c.tagName) && /inline/.test(getComputedStyle(c).display) && getComputedStyle(c).position !== "absolute" && c.textContent.trim()).map((c) => c.getBoundingClientRect());
    if (b && Math.abs(a.top - b.top) < a.height && a.left < b.left) out.push("pieces: " + el.textContent.trim().replace(/\s+/g, " ").slice(0, 60));
  }
  return out;
};
const check = (where) => t(`${where} reads Arabic`, () => {
  const left = english(document.body);
  if (left.length) throw new Error("English left: " + left.slice(0, 8).map((s) => JSON.stringify(s)).join(", "));
  const ltr = leftToRight();
  if (ltr.length) throw new Error("Arabic laid out left to right: " + ltr.slice(0, 5).map((s) => JSON.stringify(s)).join(", "));
});

t("the page is in Arabic, right to left", () => {
  const root = document.documentElement;
  if (root.lang !== "ar" || root.dir !== "rtl") throw new Error(`lang ${root.lang}, dir ${root.dir}`);
  if (!/[\u0600-\u06FF]/.test(document.querySelector("nav [data-view=containers] .lbl").textContent)) throw new Error("the sidebar isn't Arabic");
  if (document.getElementById("strip-term").getAttribute("aria-label") !== STRINGS.ar["Terminal"]) throw new Error("the terminal button isn't");
});
show("containers");
// New elements get their direction from a mutation observer, which runs once this script yields.
Promise.resolve().then(() => {
t("the sidebar is on the right, and the strip starts there", () => {
  if (!(box("aside").left > box("main").left)) throw new Error("the sidebar is on the left");
  if (!(box("#rail-toggle").left > box("#strip-settings").left)) throw new Error("the strip runs left to right");
  if (!(box("#nav-back").left > box("#nav-fwd").left)) throw new Error("Back isn't before Forward");
});
t("a table starts on the right: names first, actions last", () => {
  const row = document.querySelector("tr.row");
  if (!(box(row.cells[1]).left > box(row.querySelector("td.actions")).left)) throw new Error("the name is left of the actions");
  const th = document.querySelector("th:not(.ck)");
  if (getComputedStyle(th).textAlign !== "right") throw new Error("headings align " + getComputedStyle(th).textAlign);
  // A Latin name sits on the right of its column, like the Arabic heading over it.
  const name = row.cells[1].querySelector("a, .nm, b, span") || row.cells[1];
  if (Math.abs(box(name).right - box(th).right) > 80) throw new Error(`the name ends at ${box(name).right}, its heading at ${box(th).right}`);
});
t("back points right and forward left; a caret that opens downward still does", () => {
  const m = (sel) => getComputedStyle(document.querySelector(sel)).transform;
  if (m("#nav-fwd svg") !== "matrix(-1, 0, 0, 1, 0, 0)") throw new Error("forward: " + m("#nav-fwd svg"));
  if (m("#nav-back svg") !== "none") throw new Error("back: " + m("#nav-back svg"));
});
t("a name, a size or an address filled into a sentence keeps its own order", () => {
  const cap = tr("of {cap}", { cap: "5.0 GB" });
  if (!cap.includes("\u20685.0 GB\u2069")) throw new Error(JSON.stringify(cap));
  // A bare number needs no island.
  const n = tr("{n} of {total}", { n: 3, total: 4 });
  if (/[\u2068\u2069]/.test(n)) throw new Error(JSON.stringify(n));
  // An Arabic sentence that opens with a Latin name says which way it runs.
  const latinFirst = Object.entries(STRINGS.ar).filter(([, w]) => /^[A-Za-z]/.test(w) && /[\u0600-\u06FF]/.test(w));
  if (latinFirst.length) throw new Error("no right-to-left mark: " + latinFirst.slice(0, 3).map(([k]) => k).join(" | "));
  if (!Object.values(STRINGS.ar).some((w) => /^\u200F[A-Za-z]/.test(w))) throw new Error("no sentence was marked");
  // English fills as before.
  const en = "Started {name}".replace(/\{(\w+)\}/g, "web");
  if (en !== "Started web") throw new Error(en);
});
t("every element takes its own letters' direction; logs stay left to right", () => {
  const cell = document.querySelector("tr.row td");
  if (getComputedStyle(cell).unicodeBidi !== "plaintext") throw new Error("cells: " + getComputedStyle(cell).unicodeBidi);
  document.body.append(logEl);
  const { direction, textAlign } = getComputedStyle(logEl);
  logEl.remove();
  if (direction !== "ltr" || textAlign !== "left") throw new Error(`the log: ${direction}, ${textAlign}`);
});
t("text that can be cut short takes its own direction, so a long Latin name is cut at its end", () => {
  // The address under a name: Latin, so left to right.
  const addr = document.querySelector("tr.row .sub-addr");
  if (addr.dir !== "auto" || getComputedStyle(addr).direction !== "ltr") throw new Error(`the address: dir ${addr.dir}, ${getComputedStyle(addr).direction}`);
  // A list refresh redraws the row in place and leaves that as it is.
  kiri.on({ type: "lists", containers: ui.containers.map((c) => ({ ...c, Status: "Up 3 hours" })), images: ui.images, volumes: ui.volumes, networks: ui.networks });
  const again = document.querySelector("tr.row .sub-addr");
  if (again !== addr || again.dir !== "auto") throw new Error(`after a refresh: ${again === addr ? "same element" : "another element"}, dir ${again.dir}`);
  // An Arabic label reads right to left.
  const lbl = document.querySelector("nav [data-view=containers] .lbl");
  if (lbl.dir !== "auto" || getComputedStyle(lbl).direction !== "rtl") throw new Error(`the sidebar: dir ${lbl.dir}, ${getComputedStyle(lbl).direction}`);
  // A cell with an icon beside its name keeps the window's direction: the icon stays on the right.
  const cell = document.querySelector("tr.row").cells[1];
  if (cell.dir || getComputedStyle(cell).direction !== "rtl") throw new Error(`the name's cell: dir ${cell.dir}`);
  if (!(box(cell.querySelector(".tile")).left > box(cell.querySelector(".link")).left)) throw new Error("the icon is left of the name");
  // Nothing moved for it: no element's own spacing changed sides.
  const moved = [...document.querySelectorAll("[dir=auto]")].filter((el) => { const was = ownSides(el); el.removeAttribute("dir"); const now = ownSides(el); el.dir = "auto"; return was !== now; });
  if (moved.length) throw new Error("spacing changed sides on " + moved.slice(0, 3).map((el) => el.className).join(", "));
});
t("a row's menu opens under its button, from the button's left edge", () => {
  const more = [...document.querySelector("tr.row td.actions").querySelectorAll("button")].at(-1);
  more.click();
  const menu = document.querySelector(".menu");
  if (!menu) throw new Error("no menu");
  const d = Math.abs(box(menu).left - box(more).left);
  document.body.click();
  if (d > 1) throw new Error(`${d}px from the button's left edge`);
});
t("dragging a column's edge to the left widens it: its far edge is the left one", () => {
  const grip = document.querySelector("th .col-resize"), x = box(grip).left + 2, y = box(grip).top + 4;
  const ptr = (type, dx) => new PointerEvent(type, { bubbles: true, button: 0, clientX: x + dx, clientY: y, pointerId: 1 });
  grip.dispatchEvent(ptr("pointerdown", 0));
  document.dispatchEvent(ptr("pointermove", -30));
  document.dispatchEvent(ptr("pointerup", -30));
  // The first column took room from the second (left to right, it would have given some away).
  const [first, second] = widths().containers.cols;
  if (!(first > second * 1.2)) throw new Error(`the first column has ${first}, the second ${second}`);
  delete widths().containers; fitColumns();
});

// A Compose project, and another engine behind `docker`: two lines the page joins itself.
const member = (id, name, state) => ({ Id: id, Names: ["/" + name], Image: "nginx:alpine", State: state, Status: state === "running" ? "Up 1 hour" : "Exited (0)", Created: Date.now() / 1000 - 3600, Ports: [], Labels: { "com.docker.compose.project": "shop" } });
kiri.on({ type: "lists", containers: [...ui.containers, member("c1", "shop_web_1", "running"), member("c2", "shop_db_1", "exited")], images: ui.images, volumes: ui.volumes, networks: ui.networks });
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, services: [], containers: [] }] });
kiri.on({ ...ui.status, type: "status", app: "Kiri Dev", ours: "kiri", context: "default" });
// The project's row is in Compose.
show("compose");
t("a line the page joins itself keeps its Arabic words in order", () => {
  // "Compose", then Arabic words: marked, so it starts on the right like the rows around it.
  const sub = document.querySelector("tr.group .sub-name");
  if (!sub.textContent.startsWith("\u200FCompose")) throw new Error("the project's line: " + JSON.stringify(sub.textContent));
  // The engine's name is Arabic words around a Latin one: an island, after docker and its arrow.
  const chip = document.querySelector(".statusbar .ctx-warn").textContent;
  if (!chip.includes("docker → \u2068" + tr("Docker's default engine") + "\u2069")) throw new Error("the status bar: " + JSON.stringify(chip));
  // A link whose container has gone: the arrow points left, the way the row reads.
  const gone = shTarget({ source: "old-web", target: "localhost:8080" });
  if (!gone.includes("\u200F← localhost:8080")) throw new Error("a link's target: " + gone);
});
t("a dialog's two buttons run right to left: Cancel first, on the right", () => {
  containerAction("remove", ui.containers[0].Id);
  const cancel = document.querySelector(".dialog .row-end [data-close]"), go = document.querySelector(".dialog .row-end [data-go]");
  if (!cancel || !go) throw new Error("no dialog: " + [...document.querySelectorAll(".scrim, .dialog")].map((e) => e.className).join(", "));
  const ok = box(cancel).left > box(go).left, ltr = leftToRight();
  cancel.click();
  if (!ok) throw new Error("Cancel is left of the action");
  if (ltr.length) throw new Error("in the dialog: " + ltr.map((s) => JSON.stringify(s)).join(", "));
});
for (const v of ["containers", "images", "volumes", "networks", "builds", "share"]) { show(v); check(`the ${v} view`); }
show("usb");
kiri.on({ type: "usb", can: "attach", devices: [
  { key: "a", name: "CP2102N", id: "10c4:ea60", speed: "full", state: "kiri", kind: "serial" },
  { key: "b", name: "SONOFF ZBDongle-E", id: "1a86:55d4", speed: "full", state: "host", kind: "radio" }] });
check("the USB view");
ui.images.push({ Id: "sha256:" + "ab".repeat(32), RepoTags: ["nginx:alpine"], Size: 5e7, Created: Date.now() / 1000 - 86400 });
show("images"); openImage("nginx:alpine"); check("an image's page");
t("a page's breadcrumb points the way it reads", () => {
  const sep = document.querySelector(".crumb > span[aria-hidden]");
  if (getComputedStyle(sep).transform !== "matrix(-1, 0, 0, 1, 0, 0)") throw new Error(getComputedStyle(sep).transform);
  if (!(box(".crumb > button").left > box(sep).left)) throw new Error("Images isn't first");
});
show("settings");
for (const id of [...document.querySelectorAll("[data-stab]")].map((b) => b.dataset.stab)) {
  document.querySelector(`[data-stab="${id}"]`).click();
  check(`Settings → ${id}`);
}
t("the language step's rows read from the right", () => {
  kiri.on({ type: "langpick" });
  const part = (id, c) => box(`#lang-list [data-lang="${id}"] .${c}`);
  if (!(part("en", "ln").left > part("en", "lh").right)) throw new Error("English's hello isn't on the left");
  if (!(part("de", "ln").left > part("de", "lh").right)) throw new Error("German's hello isn't on the left");
  document.getElementById("lang-go").click();
});
t("English again from Settings: left to right, as before", () => {
  show("settings");
  document.querySelector('[data-stab="appearance"]').click();
  const sel = document.getElementById("pref-lang"); sel.value = "en"; sel.dispatchEvent(new Event("change"));
  const root = document.documentElement;
  if (root.lang !== "en" || root.dir !== "ltr") throw new Error(`lang ${root.lang}, dir ${root.dir}`);
  if (!(box("aside").left < box("main").left)) throw new Error("the sidebar stayed on the right");
  if (getComputedStyle(document.querySelector(".settings .pane h3")).unicodeBidi === "plaintext") throw new Error("still plaintext");
  if (/[\u200F\u2068\u2069]/.test(tr("of {cap}", { cap: "5.0 GB" }))) throw new Error("marks left in English");
  if (document.querySelector("[dir=auto]")) throw new Error("dir=auto left on " + document.querySelector("[dir=auto]").className);
  show("compose");
  const joined = document.querySelector("tr.group .sub-name").textContent + document.querySelector(".statusbar .ctx-warn").textContent + shTarget({ source: "old-web", target: "localhost:8080" });
  if (/[\u200F\u2068\u2069←]/.test(joined) || !joined.includes("docker → Docker's default engine")) throw new Error("the joined lines: " + JSON.stringify(joined));
  if (getComputedStyle(document.querySelector("th:not(.ck)")).textAlign !== "left") throw new Error("headings didn't go back");
});
report(R);
});
