// Containers and Images: list ↔ grid layout toggle, cards in grid mode.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (a !== b) throw new Error((m || "eq") + ": " + JSON.stringify(a) + " !== " + JSON.stringify(b)); };
const C = (id, name, state) => ({
  Id: id, Names: ["/" + name], Image: "nginx:alpine", ImageID: "sha256:aaa", State: state,
  Status: state === "running" ? "Up 1 hour" : "Exited (0)", Created: Date.now() / 1000 - 600,
  Ports: state === "running" ? [{ PublicPort: 8080, PrivatePort: 80, Type: "tcp" }] : [], Labels: {},
});
const I = (id, tag, size) => ({ Id: id, RepoTags: [tag], Size: size, Created: 1, Pulled: Date.now() / 1000 - 60 });

prefs.listLayout = {};
kiri.on({
  type: "lists",
  containers: [C("c1", "web", "running"), C("c2", "api", "exited")],
  images: [I("sha256:aaa111", "nginx:alpine", 5e7), I("sha256:bbb222", "harbor.example.com:9090/szda/redis:7", 3e7)],
  volumes: [], networks: [],
});

t("toolbar has a list/grid toggle on Containers", () => {
  show("containers");
  const mode = $("hright").querySelector(".vmode");
  if (!mode) throw new Error("no vmode");
  eq(mode.querySelector('[data-vmode="list"]').getAttribute("aria-pressed"), "true", "list on");
  eq(mode.querySelector('[data-vmode="grid"]').getAttribute("aria-pressed"), "false", "grid off");
  eq(!!$("content").querySelector("table[data-view=containers]"), true, "table");
});

t("switching to grid shows container cards", () => {
  $("hright").querySelector('[data-vmode="grid"]').click();
  eq(listLayout("containers"), "grid", "pref");
  eq(!!$("content").querySelector(".ggrid-wrap[data-view=containers] .ggrid"), true, "grid");
  eq(!!$("content").querySelector(".ggrid-wrap .ggrid-tools [data-pickhead]"), true, "grid select-all");
  eq($("content").querySelectorAll(".gcard.ctr[data-rowopen=detail]").length, 2, "two cards");
  eq(!!$("content").querySelector("table"), false, "no table");
  const card = $("content").querySelector('[data-key="c1"]');
  if (!card?.querySelector(".link")) throw new Error("card missing name");
  if (!card.querySelector(".gstatus .pill")) throw new Error("status badge");
  if (!card.querySelector(".gstats")) throw new Error("stats grid");
  const stats = [...card.querySelectorAll(".gstats .k")].map((el) => el.textContent);
  eq(stats.join(","), "ID,Ports,Size,Created", "labeled stats");
  eq(card.querySelectorAll(".gstats .cell").length, 4, "two-by-two stats");
  eq(card.querySelector(".gstats .cell .mono.sel")?.textContent, "c1", "short container id");
  eq(card.querySelector(".gstats .cell button.copy")?.dataset.copy, "c1", "copy container id");
  // Header subtitle is always the image (with copy), even when running.
  eq(card.querySelector(".ghead .sub .nm")?.textContent, "nginx:alpine", "subtitle is image");
  eq(card.querySelector(".ghead .sub button.copy")?.dataset.copy, "nginx:alpine", "copy image:tag in header");
  eq(!!card.querySelector(".gstatus > .pill"), true, "status on its own line");
  eq(!!card.querySelector(".gstatus .greach .gaddr"), true, "reachable addr under status");
  if (!card.querySelector(".gacts .gacts-l [data-c]")) throw new Error("card missing actions");
  if (!card.querySelector("[data-rpick]")) throw new Error("card missing pick");
});

t("grid preference is kept when leaving and returning", () => {
  show("images");
  eq(listLayout("containers"), "grid", "kept");
  show("containers");
  eq(!!$("content").querySelector(".ggrid"), true, "still grid");
});

t("Images grid shows image cards and can switch back to list", () => {
  show("images");
  eq(listLayout("images"), "list", "images start list");
  $("hright").querySelector('[data-vmode="grid"]').click();
  eq(listLayout("images"), "grid", "images grid pref");
  eq(!!$("content").querySelector(".ggrid-wrap[data-view=images] .ggrid-tools [data-pickhead]"), true, "images grid select-all");
  eq($("content").querySelectorAll(".gcard.img[data-imgcard]").length, 2, "two image cards");
  const card = $("content").querySelector('[data-key="nginx:alpine"]');
  if (!card?.querySelector("[data-run], .run-up, .gacts")) throw new Error("image card actions");
  if (!card.querySelector(".gbadge")) throw new Error("usage badge");
  if (!card.querySelector(".gstats")) throw new Error("stats grid");
  if (!card.querySelector(".gtags .tag, .gtags .tag.none")) throw new Error("tag chip");
  eq(card.querySelector(".ghead[data-img]")?.dataset.img, "nginx:alpine", "header opens detail");
  eq(card.querySelector(".ghead .link")?.textContent, "nginx", "card title is last path segment");
  eq(!!card.querySelector(".ghead .sub"), false, "short repo has no path subtitle");
  const long = $("content").querySelector('[data-key="harbor.example.com:9090/szda/redis:7"]');
  eq(long?.querySelector(".ghead .link")?.textContent, "redis", "long path title is last segment");
  eq(long?.querySelector(".ghead .sub")?.textContent, "harbor.example.com:9090/szda/redis", "path under title");
  const tagCopy = card.querySelector(".gtags button.copy");
  eq(tagCopy?.dataset.copy, "nginx:alpine", "copy repo:tag");
  eq(tagCopy?.getAttribute("title"), "Copy image:tag", "tooltip");
  const stats = [...card.querySelectorAll(".gstats .k")].map((el) => el.textContent);
  eq(stats.join(","), "ID,Created,Size,Pulled", "ID above Size");
  eq(!!card.querySelector(".gacts .gacts-l"), true, "actions grouped");
  sent.length = 0;
  tagCopy.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  eq(!!ui.imgd, false, "copy does not open detail");
  const copy = sent.find((m) => m.type === "copy");
  eq(copy?.name, "nginx:alpine", "copied repo:tag");
  $("hright").querySelector('[data-vmode="list"]').click();
  eq(!!$("content").querySelector("table[data-view=images]"), true, "back to table");
});

t("clicking a container card opens its detail", () => {
  show("containers");
  prefs.listLayout = { ...(prefs.listLayout || {}), containers: "grid" };
  render();
  $("content").querySelector('[data-key="c1"] .link').click();
  eq(ui.detail?.id || ui.detail?.name ? "open" : "", "open", "detail opened");
  // Close detail for later tests.
  if (typeof closeDetail === "function") closeDetail();
  ui.detail = null;
  show("containers");
});

t("picking works on a grid card", () => {
  prefs.listLayout = { containers: "grid", images: "list" };
  show("containers");
  const box = $("content").querySelector('[data-key="c2"] [data-rpick]');
  box.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  eq([...ui.pick.keys].join(), "c2", "picked");
  eq($("content").querySelector('[data-key="c2"]').classList.contains("chosen"), true, "chosen");
  eq(!!$("hright").querySelector(".bulk"), true, "bulk bar");
  const mode = $("hright").querySelector(".vmode");
  eq(!!mode, true, "list/grid toggle stays while picking");
  eq(mode === $("hright").lastElementChild, true, "list/grid toggle stays at row end");
});

t("Images list: only Name/Repository open; other cells pick", () => {
  prefs.listLayout = { containers: "list", images: "list" };
  ui.pick = { view: "images", keys: new Set(), last: "", cur: "" };
  ui.imgd = null;
  show("images");
  const row = $("content").querySelector('tr[data-imgrow][data-key="nginx:alpine"]');
  if (!row) throw new Error("no image row");
  row.querySelector("td[data-img] .link").dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  eq(ui.imgd?.name || ui.imgd?.id ? "open" : "", "open", "name opens");
  ui.imgd = null;
  show("images");
  const again = $("content").querySelector('tr[data-imgrow][data-key="nginx:alpine"]');
  const idCell = [...again.cells].find((td) => td.classList.contains("col-id"));
  idCell.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  eq(!!ui.imgd, false, "id cell does not open");
  eq([...ui.pick.keys].join(), "nginx:alpine", "id cell picks");
});

t("Images grid: header opens; stats pick", () => {
  prefs.listLayout = { containers: "list", images: "grid" };
  ui.pick = { view: "images", keys: new Set(), last: "", cur: "" };
  ui.imgd = null;
  show("images");
  const card = $("content").querySelector('.gcard.img[data-key="nginx:alpine"]');
  if (!card) throw new Error("no image card");
  card.querySelector(".gopen .link").dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  eq(ui.imgd?.name || ui.imgd?.id ? "open" : "", "open", "title opens");
  ui.imgd = null;
  show("images");
  const again = $("content").querySelector('.gcard.img[data-key="nginx:alpine"]');
  again.querySelector(".gstats").dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  eq(!!ui.imgd, false, "stats do not open");
  eq([...ui.pick.keys].join(), "nginx:alpine", "stats pick");
});

t("k8s mark sits on the same title line as the container name", () => {
  const k8s = C("k8s1", "kiri-k8s", "running");
  k8s.Image = "rancher/k3s:v1.37.1-k3s1";
  k8s.Ports = [{ PublicPort: 6443, PrivatePort: 6443, Type: "tcp" }];
  kiri.on({ type: "lists", containers: [k8s, C("c1", "web", "running")], images: [], volumes: [], networks: [] });
  prefs.listLayout = { containers: "grid" };
  show("containers");
  const card = $("content").querySelector('[data-key="k8s1"]');
  if (!card) throw new Error("no kiri-k8s card");
  const title = card.querySelector(".ghead .gtitle");
  if (!title) throw new Error("missing gtitle row");
  eq(title.querySelector(".link")?.textContent, "kiri-k8s", "name in title");
  eq(!!title.querySelector(".k8s-mark"), true, "mark beside name");
  eq(title.contains(card.querySelector(".k8s-mark")), true, "mark inside gtitle");
  eq(!!card.querySelector(".ghead .who > .k8s-mark"), false, "mark not a who sibling");
});

t("layout preference is saved for the next launch", () => {
  prefs.listLayout = {};
  sent.length = 0;
  setListLayout("containers", "grid");
  const save = sent.find((m) => m.type === "setpref" && m.name === "listLayout");
  eq(save?.value?.containers, "grid", "persists containers grid");
  // Simulate a fresh prefs load from disk after relaunch.
  Object.keys(prefs).forEach((k) => { delete prefs[k]; });
  kiri.on({ type: "prefs", prefs: { listLayout: { containers: "grid", images: "list" } } });
  show("containers");
  eq(listLayout("containers"), "grid", "restored containers");
  eq(!!$("content").querySelector(".ggrid-wrap[data-view=containers] .ggrid"), true, "grid after relaunch");
  show("images");
  eq(listLayout("images"), "list", "restored images list");
});

report(R);
