// parity: macos windows linux
// window: 1440x900
// Compose's list is drawn by Containers' row code (containerRow, table), and a project's containers are in Compose
// only: a member's row is the row Containers draws for the same container data with no project, but for its box,
// its Remove (button and ⋯ item), its tree line, its tile (24px under its project) and Show in compose file; the
// same columns, header and row heights, and cell padding.
// What only Compose knows comes as rows of the same shape: a project never started, a service with no container
// (Not created), a project's notices (⚠, and ⓘ when managed by name). No boxes, no Remove; folding is shared.
const R = [], ITEMS = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const now = Math.floor(Date.now() / 1000);
const C = (id, project, service, n, state, ago, ports = []) => ({ Id: id, Names: [`/${project}-${service}-${n}`], Image: `${service}:dev`, State: state, Status: state === "running" ? "Up 1 hour" : "Exited (0)",
  Created: now - ago, Ports: ports.map(([a, b]) => ({ IP: "0.0.0.0", PrivatePort: b, PublicPort: a, Type: "tcp" })), Labels: { "com.docker.compose.project": project, "com.docker.compose.service": service } });
const ctrs = [C("b1", "blog", "app", 1, "running", 300, [[8080, 80]]), C("b2", "blog", "worker", 1, "exited", 900),
  C("s1", "shop", "api", 1, "running", 600), C("s2", "shop", "api", 2, "running", 610), C("s3", "shop", "api", 3, "running", 620), C("s4", "shop", "db", 1, "running", 1200),
  C("n1", "named", "web", 1, "running", 1500)];
const svc = (name, image = name + ":dev") => ({ name, image, build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
const mem = (p) => ctrs.filter((c) => c.Labels["com.docker.compose.project"] === p).map((c) => ({ service: c.Labels["com.docker.compose.service"], id: c.Id, name: c.Names[0].slice(1), image: c.Image, state: c.State, status: c.Status }));
const P = [
  { name: "blog", dir: "/w/blog", files: ["/w/blog/compose.yaml"], readable: true, services: [svc("app"), svc("worker")], containers: mem("blog"),
    error: "Compose rejects this file. A required variable may not be set: BLOG_KEY.", errkey: "reqUnset", errargs: { name: "BLOG_KEY" } },
  { name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, services: [svc("api"), svc("db", "postgres:17"), svc("mailer", "axllent/mailpit:latest")], containers: mem("shop") },
  { name: "named", dir: "/w/named", files: ["/w/named/compose.yaml"], readable: false, byname: true, services: [], containers: mem("named"), warn: { errkey: "byName", text: "" } },
  { name: "draft", dir: "/w/draft", files: ["/w/draft/compose.yaml"], readable: true, fresh: true, services: [svc("web"), svc("api")], containers: [] },
];
ui.vm = "running";
kiri.on({ type: "status", version: "0.1.4", dev: true, share: true, app: "Kiri", ours: "kiri", context: "kiri" });
const solo = { Id: "solo", Names: ["/solo"], Image: "nginx", State: "running", Status: "Up", Created: now - 50, Ports: [{ IP: "0.0.0.0", PrivatePort: 80, PublicPort: 8081, Type: "tcp" }], Labels: {} };
// The same containers with no project (and the address Compose's would have): what Containers draws for them.
const looseOf = (c) => ({ ...c, Labels: { "dev.kiri.domain": `${c.Labels["com.docker.compose.service"]}.${c.Labels["com.docker.compose.project"]}` } });
kiri.on({ type: "lists", containers: [...ctrs.map(looseOf), solo], images: [], volumes: [], networks: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: P });
const click = (el, o = {}) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, ...o }));
const $c = (q) => document.querySelector("#content " + q), $$c = (q) => [...document.querySelectorAll("#content " + q)];
const grp = (p) => $c(`tr.group[data-group="${p}"]`), row = (k) => $c(`tr[data-key="${k}"]`);
const btns = (tr) => [...tr.querySelectorAll("td.actions .acts-row > *")].map((b) => b.getAttribute("aria-label") || b.textContent.trim());
const menu = () => [...document.querySelectorAll(".menu button")].map((b) => b.textContent + (b.classList.contains("danger") ? " (danger)" : ""));
// The allowed differences: the box cell and the row's pick key, the Remove button, the tree line (and the row's member
// class, which sizes its tile), and the ⋯'s Remove flag and Show in compose file.
const norm = (h) => h.replace(/<td class="ck">[\s\S]*?<\/td>/g, "").replace(/ data-pick="[^"]*"/g, "").replace(/<button class="btn sm ic danger[^"]*" data-c="remove"[\s\S]*?<\/button>/g, "")
  .replace(/ data-noremove=""/g, "").replace(/ data-mfile="[^"]*" data-svc="[^"]*"/g, "")
  .replace(/<span class="tree[^"]*" aria-hidden="true"><\/span>/g, "").replace(/^<tr class="row member/, '<tr class="row');
const cs = (el) => getComputedStyle(el);
// The row's padding: the Name cell's, and the row's start (a box's cell in Containers, the Name cell in Compose).
const look = () => {
  const tbl = $c("table[data-view]"), r = row("b1"), name = r.querySelector(".name").closest("td");
  return { head: Math.round(tbl.tHead.rows[0].getBoundingClientRect().height), row: Math.round(r.getBoundingClientRect().height), pad: [cs(name).paddingTop, cs(name).paddingRight, cs(name).paddingBottom, cs(r.cells[0]).paddingLeft].join(" "),
    cols: [...tbl.tHead.rows[0].cells].filter((c) => !c.classList.contains("ck")).map((c) => c.firstChild.textContent) };
};
// A tile's size, corner and icon; a loose container's Kiri badge, where it sits off the tile's corner and its size.
const tileLook = (el) => { const t = el.getBoundingClientRect(), i = el.querySelector("svg").getBoundingClientRect(); return [t.width, t.height, cs(el).borderRadius, i.width, i.height]; };
const badgeLook = (wrap) => { const t = wrap.querySelector(".tile").getBoundingClientRect(), b = wrap.querySelector(".kbadge").getBoundingClientRect(), d = wrap.querySelector(".kbadge .kdot").getBoundingClientRect();
  return [b.right - t.right, b.bottom - t.bottom, d.width, d.height]; };
// As main draws a tile outside a project (the base .tile, .kbadge and .kdot rules): 30px, 8px corner, 18px icon; the badge 3px off, 8 by 9.
const MAIN_TILE = [30, 30, "8px", 18, 18], MAIN_BADGE = [3, 3, 8, 9];
show("containers");
t("Containers: the same containers with no project are loose rows, main's 30px tile; no project rows", () => {
  eq([tileLook(row("b1").querySelector(".tile")), tileLook(row("solo").querySelector(".tile")), badgeLook(row("solo").querySelector(".tile-wrap"))], [MAIN_TILE, MAIN_TILE, MAIN_BADGE], "tiles and badge");
  eq($$c("tr.group, tr.member, .tree").length, 0, "no groups");
});
const keys = ctrs.map((c) => c.Id), was = Object.fromEntries(keys.map((k) => [k, norm(row(k).outerHTML)])), wasLook = look();
kiri.on({ type: "lists", containers: [...ctrs, solo], images: [], volumes: [], networks: [] });
show("compose");
t("every member's row is the loose row of the same container, but for its box, its Remove, its tree line and Show in compose file", () => {
  for (const k of keys) eq(norm(row(k).outerHTML), was[k], k);
  eq(keys.map((k) => [row(k).matches(".member"), !!row(k).querySelector(".name > .tree")]), keys.map(() => [true, true]), "each under its project, on a tree line");
  // shop-db-1's tree line goes on to the service with no container after it, so it isn't the last there.
  eq([!!row("s4").querySelector(".tree.last"), row("s4").nextElementSibling.dataset.svc], [false, "mailer"], "the tree line goes on");
});
t("a project's row: its stack tile, its name (a link to its page) and its line; as tall as its containers'", () => {
  eq([grp("blog").querySelector("b.gopen").dataset.gopen, grp("blog").querySelector(".sub-name").textContent.replace(/‏/g, "")], ["blog", "Compose · 1 of 2 running"], "name and line");
  eq(Math.round(grp("blog").getBoundingClientRect().height), wasLook.row, "height");
});
t("the same columns (no box), header and row heights, tile and cell padding", () => {
  const now = look();
  eq(now, wasLook, "looks");
  eq(now.cols, ["Name", "State", "Ports", "Share", "Size", "Created", "Actions"], "columns");
  // A member's tile is 24px under its project's 30px stack; Not created and notice rows under a project the same.
  const sz = (el) => [el.offsetWidth, el.offsetHeight];
  eq([sz(row("b1").querySelector(".tile")), sz(grp("blog").querySelector(".tile")), sz($c('tr.unmade[data-svc="mailer"] .tile')), sz($c("tr.warnrow .tile"))], [[24, 24], [30, 30], [24, 24], [24, 24]], "tiles");
});
t("no box anywhere and no bulk bar; the list's keys are off", () => {
  eq([$$c(".ck, .ckbox, [data-pickhead], [data-gpick], [data-rpick], [data-pick]").length, !!document.querySelector("#hright .bulk"), $c("table").hasAttribute("tabindex")], [0, false, false], "boxes");
});
t("a project's containers have no Remove: no trash, none in ⋯, Show in compose file in its place", () => {
  eq($$c('tr.member [data-c="remove"], tr.member [data-ksettings]').length, 0, "no trash");
  eq(btns(row("b2")), ["Start it to restart it", "Start", "More actions for blog-worker-1"], "Restart, Start, ⋯");
  click(row("b1").querySelector("[data-more]"));
  eq(menu(), ["Open http://app.blog.kiri.localhost", "Ports…", "Pause", "Show in compose file"], "⋯");
  closeMenu();
});
t("replicas: one row each", () => {
  eq(["s1", "s2", "s3"].map((k) => row(k)?.querySelector(".link").textContent), ["shop-api-1", "shop-api-2", "shop-api-3"], "api-1..3");
});
t("a service with no container: Not created, its tile and image, Share grey, Size and Created —, Start is Apply's", () => {
  const r = $c('tr.unmade[data-cmpsvc="shop"][data-svc="mailer"]');
  eq([r.matches(".member"), !!r.querySelector(".tree.last"), r.previousElementSibling.dataset.key, r.querySelector(".link").textContent, r.querySelector(".sub-name").textContent], [true, true, "s4", "mailer", "axllent/mailpit:latest"], "row");
  eq([r.querySelector(".col-state").textContent, r.querySelector(".pill").className, r.querySelector(".shc").className, r.querySelector(".shc").title, r.cells[4].textContent, r.cells[5].textContent], ["Not created", "pill created", "shc off", "Start it to share it", "—", "—"], "cells");
  const play = r.querySelector('.go[aria-label="Apply the compose file to create it"]');
  eq([btns(r), play.disabled, play.title], [["Apply the compose file to create it", "Apply the compose file to create it", "More actions for mailer"], true, "Apply the compose file to create it"], "buttons");
  eq(r.offsetHeight, row("s4").offsetHeight, "as tall as a container's row");
  click(r.querySelector("[data-smore]"));
  eq(menu(), ["Open project", "Show in compose file"], "its ⋯");
  closeMenu();
});
t("a project never started: its row, its line, its services Not created; Start all is Apply's; ⋯ removes it from the list", () => {
  const g = grp("draft");
  eq([g.querySelector(".sub-name").textContent.replace(/‏/g, ""), btns(g)], ["Compose · not started · 2 services", ["Apply the compose file to create it", "More actions for draft"]], "row");
  eq(g.querySelector(".go").disabled, true, "Start all waits for Apply");
  eq([g.nextElementSibling.dataset.svc, g.nextElementSibling.nextElementSibling.dataset.svc, !!g.nextElementSibling.nextElementSibling.querySelector(".tree.last")], ["web", "api", true], "its services");
  eq($$c("tr").at(-1).dataset.svc, "api", "after the rest");
  click(g.querySelector("[data-gmore]"));
  eq(menu(), ["Open project", "Remove from the list"], "⋯");
  closeMenu();
  click(grp("blog").querySelector("[data-gmore]"));
  eq(menu(), ["Open project", "Restart all", "Remove project (danger)"], "a started one's ⋯");
  closeMenu();
});
t("a project's notices: rows of the same shape under its row, ⚠ or ⓘ, the first sentence, the rest under it", () => {
  const w = grp("blog").nextElementSibling, n = grp("named").nextElementSibling;
  eq([w.matches("tr.member.warnrow"), !!w.querySelector(".tile.warn"), w.querySelector(".wtitle").textContent, w.querySelector(".sub-name").textContent, btns(w)],
    [true, true, "Compose rejects this file.", "A required variable may not be set: BLOG_KEY.", ["Show in file"]], "rejects");
  eq([n.matches(".warnrow.info"), !!n.querySelector(".tile.note"), n.querySelector(".wtitle").textContent, btns(n)], [true, true, "Kiri can't check this file's variables, so it manages this project by name.", []], "by name");
  eq(w.offsetHeight, row("b1").offsetHeight, "as tall as a container's row");
  eq(w.nextElementSibling.dataset.key, "b1", "its containers after it");
  eq($$c('tr.unmade[data-cmpsvc="named"]').length, 0, "by name, no service rows");
});
t("search matches the project's name; Containers' search doesn't find a project's containers", () => {
  ui.filter = "shop"; render();
  eq([...new Set($$c("tr.group").map((r) => r.dataset.group))], ["shop"], "compose");
  show("containers"); ui.filter = "shop"; render();
  eq($$c("tbody tr").length, 0, "containers");
  show("compose"); ui.filter = ""; render();
});
t("the name opens the project's page; the rest of the row folds it, and the fold stays", () => {
  click(grp("shop").querySelector("[data-gopen]"));
  eq([cmp.open, cmp.tab], ["shop", "structure"], "page");
  cmp.open = null; render();
  click(grp("shop").querySelector("td[data-fill]"));
  eq([grp("shop").getAttribute("aria-expanded"), $$c('tr[data-key="s1"], tr[data-cmpsvc="shop"]').length], ["false", 0], "folded");
  show("containers"); show("compose");
  eq(grp("shop").getAttribute("aria-expanded"), "false", "still folded");
  click(grp("shop").querySelector("td[data-fill]"));
  eq(grp("shop").getAttribute("aria-expanded"), "true", "unfolded");
});
t("the project's Services tab: the same rows, no project row, no boxes, the same columns", () => {
  cmpOpen("shop"); cmp.tab = "services"; render();
  const tbl = $c("table[data-view=cmpsvcs]");
  eq([...tbl.tHead.rows[0].cells].map((c) => c.firstChild.textContent), ["Name", "State", "Ports", "Share", "Size", "Created", "Actions"], "columns");
  eq([...tbl.tBodies[0].rows].map((r) => r.dataset.key || r.dataset.svc), ["s1", "s2", "s3", "s4", "mailer"], "rows");
  eq([tbl.querySelectorAll(".ck, tr.group, [data-c=remove]").length, btns(row("s1")).length], [0, 3], "no boxes, no Remove");
  eq([row("s1").querySelector(".tile").offsetWidth, row("s1").querySelector(".tile").offsetHeight, row("s1").offsetHeight], [24, 24, wasLook.row], "the list's tile and height");
  eq(norm(row("s1").outerHTML.replace(/ member"/, '"').replace(/<span class="tree[^"]*" aria-hidden="true"><\/span>/, "")), was.s1.replace(/ member"/, '"').replace(/<span class="tree[^"]*" aria-hidden="true"><\/span>/, ""), "a container's row");
  cmp.open = null; render();
});
// Show in compose file: the file's tab, at the service's line once the file has come.
cmp.open = null; render();
click(row("s4").querySelector("[data-more]"));
click([...document.querySelectorAll(".menu button")].find((b) => b.textContent === "Show in compose file"));
t("Show in compose file opens the project's file tab and waits for the file", () => eq([cmp.open, cmp.tab, cmp.jump?.svc], ["shop", "file", "db"], "jump"));
kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  api:\n    image: api:dev\n  db:\n    image: postgres:17\n  mailer:\n    image: axllent/mailpit:latest\n", hash: "h1" });
setTimeout(() => {
  t("…and lands on the service's line once it has", () => {
    const ta = document.getElementById("cmp-ta"), line = ta.value.slice(0, ta.selectionStart).split("\n").length;
    eq([cmp.jump, line], [null, 4], "line");
  });
  cmp.open = null; render();
  for (const r of $$c("tbody tr")) ITEMS.push(`compose row: ${r.className.replace(/\s+/g, " ")} | ${[...r.cells].map((c) => c.className || "-").join(" ")} | ${btns(r).length} buttons`);
  // Outside a project every tile is main's: an Images row's and a Pods row's.
  kiri.on({ type: "lists", containers: [...ctrs], images: [{ Id: "sha256:1", RepoTags: ["nginx:alpine"], Size: 5e7, Created: now - 9000 }], volumes: [], networks: [] });
  kiri.on({ type: "k8s", state: "running", ready: true, context: "kiri", services: [],
    pods: [{ ns: "default", name: "web-1", ip: "10.42.0.9", labels: { app: "web" }, status: "Running", phase: "Running", ready: "1/1", allReady: true, restarts: 0, created: "2026-10-28T00:00:00Z",
      containers: [{ name: "c", image: "nginx", ready: true, restarts: 0, state: "running", ports: [{ containerPort: 80, protocol: "TCP" }] }] }] });
  t("an Images row's tile and a Pods row's tile are main's", () => {
    show("images"); const img = tileLook($c("tbody tr.row .tile"));
    show("pods"); const pod = tileLook($c('tbody tr.row.member[data-key="default/web-1"] .tile'));
    eq([img, pod], [MAIN_TILE, MAIN_TILE], "tiles");
  });
  report([...R, ...ITEMS.map((i) => "ITEM " + i)]);
}, 100);
