// The list's head: count tabs that filter (with the strip's search), the
// clean-up held behind Undo, and picking rows (a box in a column before the
// icon, ⌘A, the arrow keys, Space) for the bulk bar.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const C = (id, name, state, code) => ({ Id: id, Names: ["/" + name], Image: "kiri-test/x:1", State: state, Status: state === "running" ? "Up 1 hour" : `Exited (${code})`, Created: Date.now() / 1000 - 600, Ports: [], Labels: {} });
kiri.on({ type: "lists", containers: [C("r1", "web", "running"), C("r2", "api", "running"), C("s1", "done", "exited", 0), C("f1", "bad_one", "exited", 1), C("f2", "bad_two", "exited", 2), C("f3", "worse", "exited", 3)],
  images: [{ Id: "sha256:aaa111", RepoTags: ["nginx:alpine"], Size: 5e7, Created: 1, Pulled: Date.now() / 1000 - 60 }, { Id: "sha256:bbb222", RepoTags: ["<none>:<none>"], Size: 4e8, Created: 1 }], volumes: [], networks: [] });
show("containers");
const tabs = () => [...document.querySelectorAll("#ltabs .ltab")].map((b) => b.textContent.replace(/\s+/g, " ").trim());
const lastUndo = () => [...document.querySelectorAll(".toast.undo")].pop();
const row = (k) => document.querySelector(`#content tr[data-key="${k}"]`);
const box = (k) => document.querySelector(`#content tr[data-key="${k}"] td.ck [data-rpick]`);
const head = () => document.querySelector("#content thead th.ck [data-pickhead]");
// A click on a row's box, or on its plain cell (its image, its state), with Shift if asked.
const click = (el, o = {}) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, ...o }));
const clickBox = (k, o) => click(box(k), o);
const clickRow = (k, o) => click(row(k).cells[2], o);
const MODKEY = WIN ? "ctrlKey" : "metaKey";
const key = (k, o = {}, at = document.body) => at.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true, ...o }));
const picked = () => [...ui.pick.keys].sort();
const order = () => [...document.querySelectorAll("#content tr[data-pick]")].map((r) => r.dataset.pick);
// The columns a row takes on screen: a hidden cell takes none, a wide one its span.
const span = (tr) => [...tr.cells].reduce((a, c) => a + (getComputedStyle(c).display === "none" ? 0 : c.colSpan), 0);
const rows = () => [...document.querySelectorAll("#content tbody tr")].map((r) => r.querySelector(".name")?.textContent.trim());
t("the counts beside the title are tabs, a state with none hidden, Failed's count red", () => {
  eq(tabs(), ["All6", "Running2", "Stopped1", "Failed3"], "tabs");
  eq(document.querySelector('[data-ltab="failed"]').classList.contains("bad"), true, "red");
  eq($("subtitle").textContent, "", "no count line");
});
t("a tab filters the list, and the strip's search narrows it further", () => {
  document.querySelector('[data-ltab="failed"]').click();
  eq(rows(), ["bad_one", "bad_two", "worse"].map((x) => rows().find((r) => r.includes(x))), "failed only");
  eq(rows().length, 3, "three");
  const q = $("gq"); q.value = "bad"; q.dispatchEvent(new Event("input", { bubbles: true }));
  eq(rows().length, 2, "search within the tab");
  eq(tabs()[0], "All2", "counts follow the search");
  q.value = ""; q.dispatchEvent(new Event("input", { bubbles: true }));
  document.querySelector('[data-ltab="all"]').click();
  eq(rows().length, 6, "all again");
});
t("Clear N failed shows when there's junk and asks first", () => {
  sent.length = 0;
  const b = document.querySelector("[data-junk]");
  eq(b.textContent.trim(), "Clear 3 failed", "label");
  eq(b.querySelector("svg.cue path")?.getAttribute("d"), PH.broom, "a broom before it");
  b.click();
  const dlg = document.querySelector(".scrim .dialog");
  eq([!!dlg, dlg?.querySelector("h3").textContent, rows().length], [true, "Clear 3 failed?", 6], "asks first, nothing gone yet");
  dlg.querySelector("[data-close]").click();
  eq([rows().length, ui.batches.length], [6, 0], "Cancel keeps them");
});
t("confirmed, they go at once: no Undo, progress beside the buttons as each goes, then a toast", () => {
  const toasts = document.querySelectorAll(".toast").length;
  document.querySelector("[data-junk]").click();
  document.querySelector(".scrim [data-go]").click();
  eq(rows().length, 3, "rows leave at once");
  eq(document.querySelectorAll(".toast").length, toasts, "no Undo toast");
  const b = ui.batches[0];
  eq(b?.phase, "going", "going");
  // Sent 60 ms apart: run them now.
  b.items.forEach((x) => LISTS.containers.remove(x));
  eq(sent.filter((m) => m.type === "container" && m.action === "remove").map((m) => m.id).sort(), ["f1", "f2", "f3"], "removes");
  const busy = document.querySelector("#hright .hbusy");
  eq(busy && busy.textContent.trim(), "Removing 0 of 3…", "progress");
  eq(!!busy.closest("button"), false, "never inside a button");
  ui.containers = ui.containers.filter((c) => c.Id !== "f1"); render();
  eq(document.querySelector("#hright .hbusy").textContent.trim(), "Removing 1 of 3…", "one gone");
  ui.containers = ui.containers.filter((c) => c.Id !== "f2"); render();
  eq(document.querySelector("#hright .hbusy").textContent.trim(), "Removing 2 of 3…", "two gone");
  ui.containers = ui.containers.filter((c) => c.Id !== "f3"); render();
  eq([ui.batches.length, document.querySelector("#hright .hbusy")], [0, null], "done");
  const t = [...document.querySelectorAll(".toast.removed")].pop();
  eq([t.querySelector("b").textContent, t.querySelector("[data-undo]"), t.classList.contains("undo")], ["Removed 3 failed containers", null, false], "a plain toast");
});
t("a column before the icon holds the boxes, always showing; the name is a link", () => {
  const r = row("s1");
  eq([r.cells[0].classList.contains("ck"), r.cells[0].querySelectorAll(".cb").length, r.cells[1].querySelector(".name .tile") !== null], [true, 1, true], "the box's cell, then the name");
  const cb = r.cells[0].querySelector(".cb");
  eq([getComputedStyle(cb).opacity, cb.getBoundingClientRect().width, r.cells[0].getBoundingClientRect().width], ["1", 16, 34], "shows at rest, 16 px in 34");
  eq([!!head(), head().getAttribute("aria-checked"), head().querySelector(".cb").className, getComputedStyle(head().querySelector(".cb")).opacity], [true, "false", "cb ", "1"], "the head's box, none picked");
  const link = r.querySelector(".name .link");
  eq([!!link, getComputedStyle(link).color !== getComputedStyle(r.cells[2]).color], [true, true], "the name is a link");
  eq(document.querySelector("#content table").getAttribute("tabindex"), "0", "the list takes focus, for its keys");
});
t("a box picks its row, without opening it; the right side becomes the bulk bar", () => {
  clickBox("s1");
  eq([ui.view, ui.detail], ["containers", null], "no page");
  eq([picked(), ui.pick.cur], [["s1"], "s1"], "picked, and the keys' row");
  eq([row("s1").classList.contains("chosen"), row("s1").classList.contains("cur"), box("s1").getAttribute("aria-checked")], [true, true, "true"], "ticked, tinted and ringed");
  const td = getComputedStyle(row("s1").cells[0]);
  eq([td.boxShadow.includes("inset"), td.backgroundColor !== "rgba(0, 0, 0, 0)"], [true, true], "a blue edge, inside, and a tint");
  eq(getComputedStyle(row("s1").cells[1]).borderBottomWidth, getComputedStyle(row("r1").cells[1]).borderBottomWidth, "nothing shifts");
  eq(document.body.classList.contains("picking"), true, "picking");
  const bar = document.querySelector("#hright .bulk");
  eq(/1 selected/.test(bar.textContent), true, "count");
  eq([...bar.querySelectorAll("[data-bulk]")].map((b) => [b.textContent.trim(), b.disabled]), [["Start", false], ["Stop", true], ["Delete", false]], "actions");
  eq(bar.querySelector("[data-pickall]").textContent, "Select all 3", "select all");
  clickBox("r1");
  eq([picked(), ui.pick.cur], [["r1", "s1"], "r1"], "another box adds its row, the first stays");
  clickBox("s1");
  eq(picked(), ["r1"], "a second click drops it");
});
t("Select all, then Start acts on the ones it fits", () => {
  sent.length = 0;
  document.querySelector("[data-pickall]").click();
  eq(ui.pick.keys.size, 3, "all three");
  const start = document.querySelector('[data-bulk="0"]');
  eq(start.textContent.trim(), "Start 1", "only the stopped one");
  start.click();
  eq(sent.filter((m) => m.type === "container").map((m) => [m.action, m.id]), [["start", "s1"]], "sent");
  eq(document.querySelector("#hright .bulk"), null, "picks let go");
});
t("the head's box: a dash when some are picked, ticked when all, and it picks all or none", () => {
  clickBox("r1");
  eq([head().getAttribute("aria-checked"), head().querySelector(".cb").classList.contains("some")], ["mixed", true], "some");
  click(head());
  eq([ui.pick.keys.size, head().getAttribute("aria-checked"), head().querySelector(".cb").classList.contains("all")], [3, "true", true], "all three");
  click(head());
  eq([ui.pick.keys.size, document.body.classList.contains("picking")], [0, false], "none");
});
t("Shift-click on a box adds the rows from the last one clicked", () => {
  const ks = order();
  eq(ks.length, 3, "three rows");
  clickBox(ks[0]);
  clickBox(ks[2], { shiftKey: true });
  eq(picked(), [...ks].sort(), "the first to the third");
  unpick();
});
t("a click beside the box in its cell picks too, and doesn't open the row", () => {
  click(row("r1").cells[0]);
  eq([picked(), ui.detail], [["r1"], null], "picked, no page");
  unpick();
});
t("⌘A (Ctrl+A) picks every row on screen, but not while typing", () => {
  const q = $("gq");
  q.focus();
  key("a", { [MODKEY]: true }, q);
  eq(ui.pick.keys.size, 0, "the search box keeps its ⌘A");
  q.blur();
  document.querySelector("#content table").focus();
  key("a", { [MODKEY]: true }, document.activeElement);
  eq(picked(), [...order()].sort(), "all");
});
t("Esc lets the picks go", () => {
  eq(ui.pick.keys.size, 3, "picked");
  key("Escape");
  eq([ui.pick.keys.size, ui.pick.cur, document.body.classList.contains("picking")], [0, "", false], "cleared");
});
t("↑ and ↓ move the ring and pick that row; Shift with them picks on", () => {
  const ks = order();
  key("ArrowDown");
  eq([picked(), ui.pick.cur], [[ks[0]], ks[0]], "the first");
  key("ArrowDown");
  eq([picked(), ui.pick.cur], [[ks[1]], ks[1]], "the second");
  key("ArrowDown", { shiftKey: true });
  eq([picked(), ui.pick.cur], [[ks[1], ks[2]].sort(), ks[2]], "and the third");
  key("ArrowDown");
  eq(ui.pick.cur, ks[2], "stays on the last");
  key("ArrowUp", { shiftKey: true });
  eq([picked(), ui.pick.cur], [[ks[1], ks[2]].sort(), ks[1]], "Shift picks on from where ↓ left it");
  key("ArrowUp", { shiftKey: true });
  eq(picked(), [...ks].sort(), "up to the first");
  eq(row(ks[0]).classList.contains("cur"), true, "ringed");
  unpick();
});
t("Space ticks the ringed row, or the focused box, and leaves the others", () => {
  const ks = order();
  key("ArrowDown");
  key(" ");
  eq([picked(), ui.pick.cur], [[], ks[0]], "the ringed row dropped");
  key(" ");
  eq(picked(), [ks[0]], "and picked again");
  box(ks[2]).focus();
  key(" ", {}, document.activeElement);
  eq(picked(), [ks[0], ks[2]].sort(), "the focused box ticks its row");
  head().focus();
  key(" ", {}, document.activeElement);
  eq(picked(), [...ks].sort(), "the head's box picks all");
  document.activeElement.blur();
  unpick();
});
t("Delete or Backspace on picked rows asks first", () => {
  sent.length = 0;
  // Failed were cleared earlier; running can't be deleted (docker rm).
  ui.containers.push(C("s2", "idle", "exited", 0));
  render();
  clickBox("s1");
  clickBox("s2");
  key("Delete");
  const dlg = document.querySelector(".scrim .dialog");
  eq([dlg?.querySelector("h3").textContent, dlg?.querySelector("[data-go]").textContent, rows().length], ["Delete 2 containers?", "Delete", 4], "asks, nothing gone yet");
  dlg.querySelector("[data-close]").click();
  eq([document.querySelector(".scrim"), picked(), sent.length], [null, ["s1", "s2"], 0], "Cancel keeps them");
  key("Backspace");
  eq(!!document.querySelector(".scrim .dialog"), true, "Backspace too");
  document.querySelector(".scrim [data-close]").click();
  unpick();
  key("Delete");
  eq(document.querySelector(".scrim"), null, "nothing picked, nothing asked");
  // Running picks alone: Delete does nothing (no removable selection).
  clickBox("r1");
  key("Delete");
  eq(document.querySelector(".scrim"), null, "running left alone");
  unpick();
});
t("a click on the row opens it, as do its name and Return; the box only picks", () => {
  clickBox("r1");
  eq([ui.detail, picked()], [null, ["r1"]], "the box picks");
  click(row("r1").querySelector(".link"));
  eq(ui.detail?.id, "r1", "the name opens it");
  closeDetail(); render();
  clickRow("s1");
  eq(ui.detail?.id, "s1", "a click elsewhere on the row opens it");
  closeDetail(); render();
  clickBox("r2");
  key("Enter", {}, document.querySelector("#content table"));
  eq(ui.detail?.id, "r2", "Return opens the ringed row");
  closeDetail(); render();
  unpick();
});
t("a row's buttons do what they do and pick nothing; a click below the rows lets the picks go", () => {
  sent.length = 0;
  clickBox("r1");
  click(row("s1").querySelector('[data-c="start"]'));
  eq([picked(), sent.filter((m) => m.type === "container").map((m) => [m.action, m.id])], [["r1"], [["start", "s1"]]], "Start, and the picks stay");
  click($("content"));
  eq([ui.pick.keys.size, ui.pick.cur], [0, ""], "cleared");
});
t("Delete on picked rows asks first; Cancel deletes nothing, Delete goes", () => {
  sent.length = 0;
  // s2 may already be on the list from the keyboard Delete test.
  if (!ui.containers.some((c) => c.Id === "s2")) { ui.containers.push(C("s2", "idle", "exited", 0)); render(); }
  clickBox("s1");
  clickBox("s2");
  document.querySelector('[data-bulk="2"]').click();
  const dlg = document.querySelector(".scrim .dialog");
  const n = rows().length;
  eq([!!dlg, dlg?.querySelector("h3").textContent, rows().length, sent.length], [true, "Delete 2 containers?", n, 0], "asks, nothing gone");
  dlg.querySelector("[data-close]").click();
  eq([document.querySelector(".scrim"), rows().length, sent.length], [null, n, 0], "Cancel keeps them");
  document.querySelector('[data-bulk="2"]').click();
  document.querySelector(".scrim [data-go]").click();
  eq(rows().length, n - 2, "stopped ones leave once confirmed; running stay");
  ui.batches = []; render();
});
t("Images: tabs, Remove untagged and the Pull image button", () => {
  show("images");
  eq(tabs(), ["All2", "Unused1", "Untagged1"], "tabs");
  eq(document.querySelector("[data-junk]").textContent.trim(), "Remove 1 untagged", "junk");
  eq(document.querySelector("#hright [data-ppopen]").textContent.trim(), "Pull image", "primary");
  eq(document.getElementById("pullbox"), null, "the old search box is gone");
  eq($("subtitle").textContent, "450.0 MB", "size beside the tabs");
});
t("Cancel in the question clears nothing", () => {
  sent.length = 0;
  const before = document.querySelectorAll("#content tr.row").length;
  document.querySelector("[data-junk]").click();
  document.querySelector(".scrim [data-close]").click();
  eq([document.querySelector(".scrim"), document.querySelectorAll("#content tr.row").length, ui.batches.filter((b) => b.phase === "held").length], [null, before, 0], "untouched");
});
t("an untagged image's removal is by its ID, says nothing of its own, and the toast says what it freed", () => {
  sent.length = 0;
  document.querySelector("[data-junk]").click();
  document.querySelector(".scrim [data-go]").click();
  ui.batches.find((b) => b.view === "images").items.forEach((x) => LISTS.images.remove(x));
  eq(sent.filter((m) => m.type === "rmi").map((m) => m.name), ["bbb222"], "rmi");
  const before = document.querySelectorAll(".toast.removed").length;
  kiri.on({ type: "removed", name: "bbb222", deleted: true });
  eq(document.querySelectorAll(".toast.removed").length, before, "no toast per image");
  ui.images = ui.images.filter((i) => i.Id !== "sha256:bbb222"); render();
  const t = [...document.querySelectorAll(".toast.removed")].pop();
  eq([document.querySelectorAll(".toast.removed").length, t.querySelector("b").textContent, t.querySelector(".sub").textContent], [before + 1, "Removed bbb222", "Frees 400.0 MB"], "done");
});
t("a Compose project's row and its containers line up; the project's box picks them all", () => {
  const P = (id, name) => ({ ...C(id, name, "running"), Labels: { "com.docker.compose.project": "shop" } });
  ui.containers.push(P("p1", "shop-web"), P("p2", "shop-db"));
  show("containers");
  const g = document.querySelector('#content tr.group[data-group="shop"]');
  const gb = () => document.querySelector('#content tr.group[data-group="shop"] [data-gpick]');
  eq([g.cells[0].className, !!gb(), g.hasAttribute("data-pick")], ["ck", true, false], "group: a box of its own, not a row key");
  click(gb());
  eq([[...ui.pick.keys].sort(), gb().getAttribute("aria-checked")], [["p1", "p2"], "true"], "its box picks every container in it");
  click(box("p2"));
  eq(gb().getAttribute("aria-checked"), "mixed", "a dash when only some are");
  click(gb()); click(gb());
  eq(ui.pick.keys.size, 0, "ticked again, then cleared");
  eq([!!box("p1"), row("p1").dataset.pick], [true, "p1"], "its containers have boxes");
  eq(span(g), span(document.querySelector("#content thead tr")), "same columns as the head");
  eq(span(row("p1")), span(g), "and as its rows");
  click(g.cells[1]);
  eq([ui.pick.keys.size, g.getAttribute("aria-expanded")], [0, "false"], "a click folds it, picks nothing");
  eq([order().includes("p1"), !!box("p1")], [false, false], "a folded row can't be picked");
  click(document.querySelector('#content tr.group[data-group="shop"]').cells[1]);
  ui.containers = ui.containers.filter((c) => !c.Id.startsWith("p")); render();
});
t("a built-in network has no box; one you made does", () => {
  ui.networks = [{ Id: "n1", Name: "bridge", Driver: "bridge" }, { Id: "n2", Name: "mine", Driver: "bridge" }];
  show("networks");
  eq([row("n1").cells[0].innerHTML, row("n1").hasAttribute("data-pick"), !!box("n2")], ["", false, true], "bridge empty, mine boxed");
  click(row("n1").cells[0]);
  eq(ui.pick.keys.size, 0, "a click on bridge's cell picks nothing");
  key("a", { [MODKEY]: true });
  eq([...ui.pick.keys], ["n2"], "⌘A leaves bridge");
  unpick();
  click(head());
  eq([...ui.pick.keys], ["n2"], "so does the head's box");
  unpick();
  show("containers");
});
t("a pull's row on Images lines up: an empty first cell, the head's columns", () => {
  show("images");
  ui.pulls.set("redis:7", { name: "redis:7", phase: "pulling", got: 1, total: 4 }); render();
  const p = row("pull:redis:7");
  eq([p.cells[0].className, p.cells[0].innerHTML, p.hasAttribute("data-pick")], ["ck", "", false], "empty first cell, no pick");
  eq(span(p), span(document.querySelector("#content thead tr")), "same columns as the head");
  eq(span(row("nginx:alpine")), span(p), "and as an image's");
  ui.pulls.clear(); render();
});
t("lists that can't be picked have no boxes' column and take no keys", () => {
  show("builds");
  eq([document.querySelectorAll("#content th.ck, #content td.ck, #content tr[data-pick]").length, document.querySelector("#content table")?.hasAttribute("tabindex") || false], [0, false], "builds");
  show("containers");
});
t("pages that aren't lists have no tabs", () => {
  show("settings");
  eq([$("ltabs").hidden, $("hright").innerHTML], [true, ""], "settings");
  show("containers");
  eq($("ltabs").hidden, false, "back");
});
report(R);
