// Opening a project started since Compose last asked (from its container's page, as the breadcrumb's project does):
// Compose asks again and opens that project once the list has it.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
const C = (p, n) => ({ Id: p + n, Names: [`/${p}-${n}-1`], Image: n, State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": p, "com.docker.compose.service": n } });
const proj = (p) => ({ name: p, dir: "/w/" + p, files: [`/w/${p}/compose.yaml`], readable: true, updated: 1, services: [{ name: "web", image: "web", build: "", ports: [], volumes: [], depends: [], networks: [], kinds: [] }], containers: [{ service: "web", id: p + "web", name: `${p}-web-1`, image: "web", state: "running", status: "Up" }] });
kiri.on({ type: "lists", containers: [C("alpha", "web")], images: [], volumes: [], networks: [] });
show("compose");
kiri.on({ type: "composeprojects", logos: {}, projects: [proj("alpha")] });
show("containers");
// A second project starts (docker compose up in a terminal) while the person is on Containers.
kiri.on({ type: "lists", containers: [C("alpha", "web"), C("beta", "web")], images: [], volumes: [], networks: [] });
t("opening a project started since the last look at Compose opens that project", () => {
  eq(document.querySelector("#content .empty h3")?.textContent, "All 2 containers are in Compose projects", "Containers counts it");
  openDetail("betaweb");
  eq([ui.view, ui.detail?.id], ["compose", "betaweb"], "its container's page, in Compose");
  click(document.querySelector('#title [data-cdproj="beta"]'));
  kiri.on({ type: "composeprojects", logos: {}, projects: [proj("alpha"), proj("beta")] });
  eq([ui.view, cmp.open], ["compose", "beta"], "beta's page");
  eq(!!document.querySelector("#cmp-tabs"), true, "its tabs show");
});
t("a file just opened: a list asked for before it, without the project, says nothing; the next one opens it", () => {
  show("compose"); cmp.open = null; render();
  cmp.asked = 0; cmpAsk(); const before = cmp.askN;
  kiri.on({ type: "composeopened", name: "gamma" });
  const toasts = () => [...document.querySelectorAll(".toast")].map((x) => x.textContent).filter((x) => x.includes("gone"));
  kiri.on({ type: "composeprojects", ask: before, logos: {}, projects: [proj("alpha"), proj("beta")] });
  // One no one asked for (after an action elsewhere), without it: says nothing either.
  kiri.on({ type: "composeprojects", logos: {}, projects: [proj("alpha"), proj("beta")] });
  eq([toasts(), cmp.opening], [[], "gamma"], "still waiting, nothing said");
  kiri.on({ type: "composeprojects", logos: {}, projects: [proj("alpha"), proj("beta"), proj("gamma")] });
  eq([cmp.open, cmp.opening, toasts()], ["gamma", null, []], "gamma open");
});
t("a project asked for that the next list doesn't have: said once", () => {
  cmp.open = null; render();
  kiri.on({ type: "composeopened", name: "delta" });
  kiri.on({ type: "composeprojects", ask: cmp.askN, logos: {}, projects: [proj("alpha")] });
  eq([cmp.opening, [...document.querySelectorAll(".toast")].some((x) => x.textContent.includes("That project is gone"))], [null, true], "gone");
});
report(R);
