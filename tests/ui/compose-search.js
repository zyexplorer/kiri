// parity: macos windows linux
// window: 1440x900
// Search in Compose's list takes every row it can show alike: a container as on Containers, a service with no
// container (Not created) by its name or its image, a project by its name (then all its rows). A project's row is
// drawn when any of its rows match, with only the matching ones under it (its notices too); the pills count containers.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const now = Math.floor(Date.now() / 1000);
const C = (id, project, service, state, ago) => ({ Id: id, Names: [`/${project}-${service}-1`], Image: `${service}:dev`, State: state, Status: state === "running" ? "Up 1 hour" : "Exited (0)",
  Created: now - ago, Ports: [], Labels: { "com.docker.compose.project": project, "com.docker.compose.service": service } });
const ctrs = [C("s1", "shop", "api", "running", 600), C("s2", "shop", "db", "running", 700), C("b1", "blog", "app", "running", 300), C("b2", "blog", "worker", "exited", 900)];
const svc = (name, image = name + ":dev") => ({ name, image, build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
const mem = (p) => ctrs.filter((c) => c.Labels["com.docker.compose.project"] === p).map((c) => ({ service: c.Labels["com.docker.compose.service"], id: c.Id, name: c.Names[0].slice(1), image: c.Image, state: c.State, status: c.Status }));
const P = [
  { name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, services: [svc("api"), svc("db"), svc("mailer", "axllent/mailpit:latest")], containers: mem("shop"),
    error: "Compose rejects this file. A required variable may not be set: SHOP_KEY.", errkey: "reqUnset", errargs: { name: "SHOP_KEY" } },
  { name: "blog", dir: "/w/blog", files: ["/w/blog/compose.yaml"], readable: true, services: [svc("app"), svc("worker")], containers: mem("blog") },
  { name: "draft", dir: "/w/draft", files: ["/w/draft/compose.yaml"], readable: true, fresh: true, services: [svc("front", "nginx:alpine")], containers: [] },
];
ui.vm = "running";
kiri.on({ type: "status", version: "0.1.4", dev: true, share: true, app: "Kiri", ours: "kiri", context: "kiri" });
kiri.on({ type: "lists", containers: ctrs, images: [], volumes: [], networks: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: P });
show("compose");
const $$c = (q) => [...document.querySelectorAll("#content " + q)];
// What the list shows: a project's row as "project", and under it each row by its key, service or notice.
const onScreen = () => $$c("tbody tr").map((r) => r.dataset.group ? `[${r.dataset.group}]` : r.dataset.key || r.dataset.svc || (r.dataset.cmpnote ? "notice" : "?"));
const pills = () => [...document.querySelectorAll("#ltabs .ltab")].map((b) => b.textContent);
const sub = (p) => document.querySelector(`#content tr.group[data-group="${p}"] .sub-name`)?.textContent.replace(/‏/g, "");
const find = (q) => { ui.filter = q; render(); };
t("no search: every row", () => eq(onScreen(), ["[shop]", "notice", "s1", "s2", "mailer", "[blog]", "b1", "b2", "[draft]", "front"], "rows"));
t("a not-created service's name: its project's row and that row (and the notice), not its containers", () => {
  find("mailer");
  eq(onScreen(), ["[shop]", "notice", "mailer"], "rows");
  eq(sub("shop"), "Compose · 2 of 2 running", "a started project's line, not 'not started'");
});
t("a not-created service's image finds it too", () => { find("axllent"); eq(onScreen(), ["[shop]", "notice", "mailer"], "rows"); });
t("a never-started project's service is found the same way", () => { find("front"); eq(onScreen(), ["[draft]", "front"], "rows"); });
t("a container's name: it and its project's row", () => { find("shop-api-1"); eq(onScreen(), ["[shop]", "notice", "s1"], "rows"); });
t("a project's name: all that project's rows", () => {
  find("shop"); eq(onScreen(), ["[shop]", "notice", "s1", "s2", "mailer"], "shop");
  find("blog"); eq(onScreen(), ["[blog]", "b1", "b2"], "blog");
  find("draft"); eq(onScreen(), ["[draft]", "front"], "draft");
});
t("nothing that exists: Nothing matches", () => {
  find("zzz-none");
  eq([onScreen(), document.querySelector("#content .empty p")?.textContent], [[], "Nothing matches “zzz-none”."], "empty");
});
t("the pills count containers only", () => {
  find(""); eq(pills(), ["All4", "Running3", "Stopped1"], "no search");
  find("mailer"); eq(pills(), ["All0"], "a not-created service");
  find("shop"); eq(pills(), ["All2", "Running2"], "a project");
});
find("");
report(R);
