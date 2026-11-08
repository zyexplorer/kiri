// Images → Run: default name from the repository; optional host port;
// name/port conflicts are caught before send and again after a failed Run.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const nowS = Date.now() / 1000;
const lists = (containers) => kiri.on({ type: "lists", containers, images: [
  { Id: "sha256:" + "a".repeat(64), RepoTags: ["nginx:alpine"], Size: 1e7, Created: nowS - 60 },
  { Id: "sha256:" + "b".repeat(64), RepoTags: ["docker.1ms.run/library/redis:7"], Size: 1e7, Created: nowS - 90 },
  { Id: "sha256:" + "c".repeat(64), RepoTags: ["postgres:17"], Size: 1e8, Created: nowS - 120 },
], volumes: [], networks: [] });
lists([]);
show("images");
const runOf = (tag) => document.querySelector(`[data-run="${CSS.escape(tag)}"]`);
const needPop = () => document.querySelector(".need");
const runsSent = () => sent.filter((m) => m.type === "run");
const pressRun = (tag, vars = []) => {
  sent.length = 0;
  runOf(tag).click();
  if (sent.some((m) => m.type === "needs")) kiri.on({ type: "needs", image: tag, id: ui.images.find((i) => i.RepoTags.includes(tag))?.Id, vars });
};

t("empty name defaults to the repository's last segment", () => {
  pressRun("nginx:alpine");
  if (needPop().querySelector("[data-needcname]").placeholder !== "nginx") throw new Error("bad placeholder");
  if (!needPop().querySelector("[data-needport]")) throw new Error("no host port field");
  needPop().querySelector("[data-needgo]").click();
  if (runsSent()[0]?.container !== "nginx" || runsSent()[0]?.port) throw new Error(JSON.stringify(runsSent()[0]));
  kiri.on({ type: "ran", name: "nginx:alpine", error: false });
});
t("a nested repository still uses the last segment", () => {
  pressRun("docker.1ms.run/library/redis:7");
  needPop().querySelector("[data-needgo]").click();
  if (runsSent()[0]?.container !== "redis") throw new Error(JSON.stringify(runsSent()[0]));
  kiri.on({ type: "ran", name: "docker.1ms.run/library/redis:7", error: false });
});
t("a chosen name and host port are sent", () => {
  pressRun("nginx:alpine");
  const f = needPop().querySelector("[data-needcname]");
  f.value = "my-web"; f.dispatchEvent(new Event("input", { bubbles: true }));
  const p = needPop().querySelector("[data-needport]");
  p.value = "8080"; p.dispatchEvent(new Event("input", { bubbles: true }));
  needPop().querySelector("[data-needgo]").click();
  const m = runsSent()[0];
  if (m?.container !== "my-web" || m?.port !== 8080) throw new Error(JSON.stringify(m));
  kiri.on({ type: "ran", name: "nginx:alpine", error: false });
});
t("an invalid name or port disables Run", () => {
  pressRun("nginx:alpine");
  const f = needPop().querySelector("[data-needcname]");
  f.value = "bad name"; f.dispatchEvent(new Event("input", { bubbles: true }));
  if (!needPop().querySelector("[data-needgo]").disabled) throw new Error("accepted a space");
  f.value = "ok"; f.dispatchEvent(new Event("input", { bubbles: true }));
  const p = needPop().querySelector("[data-needport]");
  p.value = "99999"; p.dispatchEvent(new Event("input", { bubbles: true }));
  if (!needPop().querySelector("[data-needgo]").disabled) throw new Error("accepted a bad port");
  if (!/1 to 65535/.test(needPop().querySelector("[data-porthint]")?.textContent || "")) throw new Error("no port hint");
  needClose();
});
t("a name already in the list disables Run before send", () => {
  lists([{ Id: "c1", Names: ["/nginx"], Image: "nginx:alpine", State: "running", Status: "Up", Created: nowS, Ports: [{ PublicPort: 80, PrivatePort: 80, Type: "tcp" }], Labels: {} }]);
  pressRun("nginx:alpine");
  if (!needPop().querySelector("[data-needgo]").disabled) throw new Error("allowed a taken default name");
  if (!/already in use/.test(needPop().querySelector("[data-cnamehint]")?.textContent || "")) throw new Error("no name hint");
  const f = needPop().querySelector("[data-needcname]");
  f.value = "nginx-2"; f.dispatchEvent(new Event("input", { bubbles: true }));
  if (needPop().querySelector("[data-needgo]").disabled) throw new Error("still blocked");
  needClose();
  lists([]);
});
t("two images sharing a last-segment default still conflict on that name", () => {
  // First image already started as "nginx"; a different repo path still defaults to "nginx".
  kiri.on({ type: "lists", containers: [
    { Id: "c1", Names: ["/nginx"], Image: "nginx:alpine", State: "running", Status: "Up", Created: nowS, Ports: [], Labels: {} },
  ], images: [
    { Id: "sha256:" + "a".repeat(64), RepoTags: ["nginx:alpine"], Size: 1e7, Created: nowS - 60 },
    { Id: "sha256:" + "d".repeat(64), RepoTags: ["docker.1ms.run/library/nginx:latest"], Size: 1e7, Created: nowS - 30 },
  ], volumes: [], networks: [] });
  show("images");
  pressRun("docker.1ms.run/library/nginx:latest");
  if (needPop().querySelector("[data-needcname]").placeholder !== "nginx") throw new Error("bad default");
  if (!needPop().querySelector("[data-needgo]").disabled) throw new Error("allowed shared default name");
  if (!/already in use/.test(needPop().querySelector("[data-cnamehint]")?.textContent || "")) throw new Error("no name hint");
  needClose();
  lists([]);
});
t("a host port already published disables Run before send", () => {
  lists([{ Id: "c1", Names: ["/web"], Image: "nginx:alpine", State: "running", Status: "Up", Created: nowS, Ports: [{ PublicPort: 8080, PrivatePort: 80, Type: "tcp" }], Labels: {} }]);
  pressRun("nginx:alpine");
  const p = needPop().querySelector("[data-needport]");
  p.value = "8080"; p.dispatchEvent(new Event("input", { bubbles: true }));
  if (!needPop().querySelector("[data-needgo]").disabled) throw new Error("allowed a taken port");
  if (!/8080 is already in use/.test(needPop().querySelector("[data-porthint]")?.textContent || "")) throw new Error("no port hint");
  needClose();
  lists([]);
});
t("a name conflict from the engine reopens the popover", () => {
  pressRun("nginx:alpine");
  needPop().querySelector("[data-needcname]").value = "taken";
  needPop().querySelector("[data-needcname]").dispatchEvent(new Event("input", { bubbles: true }));
  needPop().querySelector("[data-needgo]").click();
  if (!needPop()?.querySelector("[data-needgo]")?.disabled) throw new Error("not busy");
  kiri.on({ type: "ran", name: "nginx:alpine", error: true, message: 'Conflict. The container name "/taken" is already in use. You have to remove (or rename) that container to be able to reuse that name.' });
  const pop = needPop();
  if (!pop || pop.querySelector("[data-needgo]").disabled) throw new Error("popover closed or still busy");
  if (!/already in use/.test(pop.querySelector("[data-neederr]")?.textContent || "")) throw new Error(pop.querySelector("[data-neederr]")?.textContent);
  if (!pop.querySelector("[data-needcname]").classList.contains("bad")) throw new Error("name not marked");
  needClose(true);
});
t("a port conflict from the engine highlights the port field", () => {
  pressRun("nginx:alpine");
  needPop().querySelector("[data-needport]").value = "80";
  needPop().querySelector("[data-needport]").dispatchEvent(new Event("input", { bubbles: true }));
  needPop().querySelector("[data-needgo]").click();
  kiri.on({ type: "ran", name: "nginx:alpine", error: true, message: "Bind for 0.0.0.0:80 failed: port is already allocated" });
  const pop = needPop();
  if (!pop || !/Host port 80 is already in use/.test(pop.querySelector("[data-neederr]")?.textContent || "")) throw new Error(pop?.querySelector("[data-neederr]")?.textContent);
  if (!pop.querySelector("[data-needport]").classList.contains("bad")) throw new Error("port not marked");
  needClose(true);
});
t("postgres popover also has name and port fields", () => {
  pressRun("postgres:17");
  if (!needPop()?.querySelector("[data-needcname]") || !needPop()?.querySelector("[data-needport]")) throw new Error("missing fields");
  needPop().querySelector("[data-needcname]").value = "pg";
  needPop().querySelector("[data-needcname]").dispatchEvent(new Event("input", { bubbles: true }));
  needPop().querySelector("[data-needport]").value = "5432";
  needPop().querySelector("[data-needport]").dispatchEvent(new Event("input", { bubbles: true }));
  needPop().querySelector("[data-needgo]").click();
  const m = runsSent()[0];
  if (m?.container !== "pg" || m?.port !== 5432 || !m.value?.some((e) => e.startsWith("POSTGRES_PASSWORD="))) throw new Error(JSON.stringify(m));
  kiri.on({ type: "ran", name: "postgres:17", error: false });
});
report(R);
