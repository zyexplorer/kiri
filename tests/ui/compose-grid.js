// Compose list ↔ grid: one card per project; grid preference is kept.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + (e && e.message)); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error((m || "eq") + ": " + JSON.stringify(a) + " !== " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const L = (svc) => ({ "com.docker.compose.project": "shop", "com.docker.compose.service": svc });
ui.vm = "running";
prefs.listLayout = {};
kiri.on({
  type: "lists",
  containers: [
    { Id: "w1", Names: ["/shop-web-1"], Image: "shop-web", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: L("web") },
    { Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: L("db") },
  ],
  images: [], volumes: [], networks: [],
});
const PROJECTS = {
  type: "composeprojects", logos: {}, projects: [{
    name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1,
    services: [
      { name: "db", image: "postgres:16", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] },
      { name: "web", image: "", build: "/w/shop/web", ports: ["3000:3000"], volumes: [], depends: ["db"], networks: ["default"], kinds: [] },
    ],
    containers: [
      { service: "db", id: "d1", name: "shop-db-1", image: "postgres:16", state: "running", status: "Up" },
      { service: "web", id: "w1", name: "shop-web-1", image: "shop-web", state: "running", status: "Up" },
    ],
  }],
};

t("Compose toolbar has a list/grid toggle", () => {
  show("compose");
  kiri.on(PROJECTS);
  const mode = $("hright").querySelector(".vmode");
  if (!mode) throw new Error("no vmode");
  eq(mode.querySelector('[data-vmode="list"]').getAttribute("aria-pressed"), "true", "list on");
  eq(!!$("content").querySelector("table[data-view=compose]"), true, "table");
  eq(!!$("content").querySelector("tr.group[data-group=shop]"), true, "project row");
});

t("switching to grid shows a project card", () => {
  click($("hright").querySelector('[data-vmode="grid"]'));
  eq(listLayout("compose"), "grid", "pref");
  eq(!!$("content").querySelector(".ggrid-wrap[data-view=compose] .ggrid"), true, "grid");
  eq($("content").querySelectorAll(".gcard.cmp[data-gopen=shop]").length, 1, "one project card");
  const card = $("content").querySelector(".gcard.cmp[data-key=shop]");
  if (!card) throw new Error("no card");
  eq(card.querySelector(".ghead .link")?.textContent, "shop", "name");
  eq(!!card.querySelector(".gsvcs .chip"), true, "service chips");
  eq(!!card.querySelector('.gacts [data-g="stop"]'), true, "stop all");
  eq(!!$("content").querySelector("table"), false, "no table");
});

t("clicking the card opens the project page", () => {
  sent.length = 0;
  click($("content").querySelector(".gcard.cmp[data-gopen=shop] .link"));
  eq(cmp.open, "shop", "opened");
  eq(sent.some((m) => m.type === "composefile"), true, "asks for file");
});

t("grid preference is kept when leaving and returning", () => {
  cmp.open = null;
  show("images");
  eq(listLayout("compose"), "grid", "kept");
  show("compose");
  kiri.on(PROJECTS);
  eq(!!$("content").querySelector(".gcard.cmp"), true, "still grid");
});

t("Start all from a card does not open the project", () => {
  prefs.listLayout = { compose: "grid" };
  cmp.open = null;
  show("compose");
  kiri.on({
    type: "lists",
    containers: [
      { Id: "w1", Names: ["/shop-web-1"], Image: "shop-web", State: "exited", Status: "Exited", Created: 1, Ports: [], Labels: L("web") },
      { Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "exited", Status: "Exited", Created: 1, Ports: [], Labels: L("db") },
    ],
    images: [], volumes: [], networks: [],
  });
  kiri.on(PROJECTS);
  sent.length = 0;
  const start = $("content").querySelector('.gcard.cmp [data-g="start"]');
  if (!start) throw new Error("no start");
  click(start);
  eq(cmp.open, null, "stays on list");
  eq(sent.some((m) => m.type === "composeaction" && m.value?.action === "start"), true, "composeaction");
});

t("switching back to list restores the table", () => {
  click($("hright").querySelector('[data-vmode="list"]'));
  eq(listLayout("compose"), "list", "list pref");
  eq(!!$("content").querySelector("table[data-view=compose]"), true, "table again");
});

report(R);
