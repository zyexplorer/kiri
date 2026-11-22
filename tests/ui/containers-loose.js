// parity: macos windows linux
// window: 1440x900
// Containers lists the containers of no Compose project: a project's are in Compose only, where its file says what
// runs. No project rows; the tabs, the sidebar's count, Select all, the bulk bar and the search take the loose ones
// only; one quiet line under the table says how many are in Compose projects, with a link there, and when every
// container is in one the empty state says so. The status bar's count is still every container.
const R = [], ITEMS = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const C = (id, name, state, project, ago) => ({ Id: id, Names: ["/" + name], Image: "nginx:alpine", State: state, Status: state === "running" ? "Up 1 hour" : "Exited (0)", Created: Math.floor(Date.now() / 1000) - ago, Ports: [],
  Labels: project ? { "com.docker.compose.project": project, "com.docker.compose.service": name.split("-")[1] } : {} });
const loose = [C("solo", "solo", "running", "", 60), C("idle", "idle", "exited", "", 120)];
const members = [C("b1", "blog-app-1", "running", "blog", 300), C("s1", "shop-web-1", "running", "shop", 600), C("b2", "blog-worker-1", "exited", "blog", 900), C("s2", "shop-db-1", "running", "shop", 1200)];
ui.vm = "running";
kiri.on({ type: "status", version: "0.1.4", dev: true, share: true, app: "Kiri", ours: "kiri", context: "kiri" });
kiri.on({ type: "lists", containers: [...loose, ...members], images: [], volumes: [], networks: [] });
show("containers");
const click = (el, o = {}) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, ...o }));
const body = () => [...document.querySelectorAll("#content tbody tr")];
const keys = () => body().map((r) => r.dataset.key || "group " + r.dataset.group);
const btns = (tr) => [...tr.querySelectorAll("td.actions .acts-row > *")].map((b) => b.getAttribute("aria-label") || b.textContent.trim());
const menu = () => [...document.querySelectorAll(".menu button")].map((b) => b.textContent + (b.classList.contains("danger") ? " (danger)" : ""));
const tabs = () => [...document.querySelectorAll("#ltabs .ltab")].map((b) => b.textContent);
t("only the loose containers, no project rows, no tree lines", () => {
  eq(keys(), ["solo", "idle"], "rows");
  eq(document.querySelectorAll("#content tr.group, #content tr.member, #content .tree, #content [data-gpick]").length, 0, "no groups");
});
t("under the table, one quiet line: how many are in Compose projects, a link there", () => {
  const p = document.querySelector("#content table + p.ctr-more");
  eq([!!p, p?.textContent, p?.closest("tbody")], [true, "4 more in Compose projects", null], "the line, not a row");
  const a = p.querySelector("button.lnk[data-ctrcompose]");
  eq([a.textContent, getComputedStyle(a).cursor], ["Compose projects", "pointer"], "the link");
  click(a);
  eq([ui.view, document.querySelector('nav button[aria-current="page"]').dataset.view], ["compose", "compose"], "it opens Compose");
  show("containers");
});
t("the tabs and the sidebar count the loose ones; Compose's count is its projects; the status bar is every container", () => {
  eq(tabs(), ["All2", "Running1", "Stopped1"], "tabs");
  eq([document.getElementById("n-containers").textContent, document.getElementById("n-compose").textContent], ["2", "2"], "sidebar");
  eq(document.getElementById("statusbar").textContent.includes("4 of 6 running"), true, "status bar");
});
t("Select all and the bulk bar take the loose ones only", () => {
  eq(pickables("containers").map((c) => c.Id), ["solo", "idle"], "pickables");
  click(document.querySelector("#content [data-pickhead]"));
  eq([...ui.pick.keys].sort(), ["idle", "solo"], "Select all");
  const bulk = document.querySelector("#hright .bulk");
  eq([...bulk.querySelectorAll("button")].map((b) => b.textContent.trim()).filter((x) => /Start|Stop|Delete/.test(x)), ["Start 1", "Stop 1", "Delete"], "the bulk bar");
  click(document.querySelector("#content [data-pickhead]"));
  eq(ui.pick.keys.size, 0, "cleared");
});
t("the search finds loose containers only, not a project's by its name", () => {
  ui.filter = "solo"; render();
  eq(keys(), ["solo"], "solo");
  ui.filter = "blog"; render();
  eq([keys(), document.querySelector("#content .empty p")?.textContent], [[], "Nothing matches “blog”."], "a project's name");
  ui.filter = ""; render();
});
t("a loose container's buttons and ⋯ keep Remove, said plainly", () => {
  eq(btns(document.querySelector('#content tr[data-key="solo"]')), ["Restart", "Stop", "Remove", "More actions for solo"], "buttons");
  click(document.querySelector('#content tr[data-key="solo"] [data-more]'));
  eq(menu(), ["Ports…", "Pause", "Remove (danger)"], "⋯");
  closeMenu();
});
t("sorting orders the loose ones", () => {
  click(document.querySelector('#content th[data-sort="name"]'));
  eq(keys(), ["idle", "solo"], "A to Z");
  delete ui.sort.containers; render();
});
// What each OS draws must be the same: each row's kind and its cells' classes.
for (const r of body()) ITEMS.push(`containers row: ${r.className.replace(/\s+/g, " ")} | ${[...r.cells].map((c) => c.className || "-").join(" ")} | ${btns(r).length} buttons`);
t("no line when no container is in a project", () => {
  kiri.on({ type: "lists", containers: loose, images: [], volumes: [], networks: [] });
  eq([keys(), !!document.querySelector("#content .ctr-more")], [["solo", "idle"], false], "no line");
});
t("every container in a project: the empty state says so, with the link, in place of No containers yet", () => {
  kiri.on({ type: "lists", containers: members, images: [], volumes: [], networks: [] });
  const e = document.querySelector("#content .empty h3");
  eq([e?.textContent, !!document.querySelector("#content table"), document.getElementById("n-containers").textContent, document.getElementById("ltabs").hidden], ["All 4 containers are in Compose projects", false, "0", true], "empty state");
  kiri.on({ type: "lists", containers: members.slice(0, 1), images: [], volumes: [], networks: [] });
  eq(document.querySelector("#content .empty h3")?.textContent, "Your only container is in a Compose project", "one");
  click(document.querySelector("#content .empty [data-ctrcompose]"));
  eq(ui.view, "compose", "the link opens Compose");
  show("containers");
  kiri.on({ type: "lists", containers: [], images: [], volumes: [], networks: [] });
  eq(document.querySelector("#content .empty h3")?.textContent, "No containers yet", "none at all");
});
report([...R, ...ITEMS.map((i) => "ITEM " + i)]);
