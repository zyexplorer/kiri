// Images → Run: Default = name + port; Custom = validated docker run paste.
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
const switchCustom = () => {
  needPop().querySelector('[data-needmode="custom"]').click();
  if (ui.need?.mode !== "custom") throw new Error("not custom mode");
};

t("empty name defaults to the repository's last segment", () => {
  pressRun("nginx:alpine");
  if (needPop().querySelector("[data-needcname]").placeholder !== "nginx") throw new Error("bad placeholder");
  if (!needPop().querySelector("[data-needport]")) throw new Error("no host port field");
  if (needPop().querySelector("[data-needcmd]")?.offsetParent) throw new Error("command field should be hidden in default");
  needPop().querySelector("[data-needgo]").click();
  const m0 = runsSent()[0];
  if (m0?.container !== "nginx" || m0?.port || m0?.command) throw new Error(JSON.stringify(m0));
  kiri.on({ type: "ran", name: "nginx:alpine", error: false });
});
t("custom mode accepts a multiline docker run", () => {
  pressRun("nginx:alpine");
  switchCustom();
  const f = needPop().querySelector("[data-needcmd]");
  if (!f || f.tagName !== "TEXTAREA") throw new Error("no custom textarea");
  const pasted = "docker run -d -it -P \\\n  --name web \\\n  -p 8080:80 \\\n  nginx:alpine";
  f.value = pasted;
  f.dispatchEvent(new Event("input", { bubbles: true }));
  if (needPop().querySelector("[data-needgo]").disabled) throw new Error("valid docker run disabled Run");
  needPop().querySelector("[data-needgo]").click();
  const m = runsSent()[0];
  if (m?.container !== "web" || m?.port !== 8080 || m?.target_port !== 80 || m?.command) throw new Error(JSON.stringify(m));
  kiri.on({ type: "ran", name: "nginx:alpine", error: false });
});
t("custom mode accepts -p and image on the same line", () => {
  pressRun("nginx:alpine");
  switchCustom();
  const f = needPop().querySelector("[data-needcmd]");
  // As copied from an older single-line start-command label after soft-wrap.
  f.value = "docker run -d -it -P \\\n  --name web2 \\\n  -p 80:80 nginx:alpine";
  f.dispatchEvent(new Event("input", { bubbles: true }));
  if (needPop().querySelector("[data-needgo]").disabled) throw new Error("same-line image rejected");
  needPop().querySelector("[data-needgo]").click();
  const m = runsSent()[0];
  if (m?.container !== "web2" || m?.port !== 80 || m?.target_port !== 80 || m?.command) throw new Error(JSON.stringify(m));
  kiri.on({ type: "ran", name: "nginx:alpine", error: false });
});
t("custom mode rejects a command for another image", () => {
  pressRun("nginx:alpine");
  switchCustom();
  const f = needPop().querySelector("[data-needcmd]");
  f.value = "docker run -d --name x alpine";
  f.dispatchEvent(new Event("input", { bubbles: true }));
  if (!needPop().querySelector("[data-needgo]").disabled) throw new Error("accepted wrong image");
  if (!/must use/i.test(needPop().querySelector("[data-cmdhint]")?.textContent || "")) throw new Error("no image hint");
  needClose();
});
t("custom mode keeps the image after --flag=value options", () => {
  // Docs often end with `--restart=always image:tag`; `=` must not steal the image.
  kiri.on({ type: "lists", containers: [], images: [
    { Id: "sha256:" + "e".repeat(64), RepoTags: ["cym1102/nginxwebui:latest"], Size: 1e7, Created: nowS - 60 },
  ], volumes: [], networks: [] });
  show("images");
  pressRun("cym1102/nginxwebui:latest");
  switchCustom();
  const f = needPop().querySelector("[data-needcmd]");
  f.value = [
    "docker run --name nginxwebui -itd \\",
    "  -p 18080:8080",
    "  -v /home/nginxWebUI:/home/nginxWebUI \\",
    "  -e BOOT_OPTIONS=\"--server.port=8080\" \\",
    "  --net=host \\",
    "  --restart=always \\",
    "  cym1102/nginxwebui:latest",
  ].join("\n");
  f.dispatchEvent(new Event("input", { bubbles: true }));
  if (needPop().querySelector("[data-needgo]").disabled) throw new Error("flag=value ate the image");
  needPop().querySelector("[data-needgo]").click();
  const m = runsSent()[0];
  if (m?.container !== "nginxwebui" || m?.port !== 18080 || m?.target_port !== 8080) throw new Error(JSON.stringify(m));
  if (!m?.binds?.includes("/home/nginxWebUI:/home/nginxWebUI")) throw new Error("binds " + JSON.stringify(m));
  if (m?.restart !== "always") throw new Error("restart " + JSON.stringify(m));
  if (!String(m?.start_command || "").includes("cym1102/nginxwebui:latest")) throw new Error("start_command " + JSON.stringify(m));
  kiri.on({ type: "ran", name: "cym1102/nginxwebui:latest", error: false });
  lists([]);
});
t("custom mode keeps entrypoint, target port and sh -c for images without EXPOSE", () => {
  kiri.on({ type: "lists", containers: [], images: [
    { Id: "sha256:" + "e".repeat(64), RepoTags: ["cym1102/nginxwebui:latest"], Size: 1e7, Created: nowS - 60 },
  ], volumes: [], networks: [] });
  show("images");
  pressRun("cym1102/nginxwebui:latest");
  switchCustom();
  const f = needPop().querySelector("[data-needcmd]");
  f.value = [
    "docker run -d --name nginxwebui --restart unless-stopped --network host --entrypoint tini \\",
    "  -p 18080:8080 -v /Users/zhuyin/nginxWebUI:/home/nginxWebUI \\",
    "  -e BOOT_OPTIONS=--server.port=8080 cym1102/nginxwebui:latest \\",
    "  -- sh -c 'mkdir -p /home/nginxWebUI/log; exec tail -F /home/nginxWebUI/log/nginxWebUI.log'",
  ].join("\n");
  f.dispatchEvent(new Event("input", { bubbles: true }));
  if (needPop().querySelector("[data-needgo]").disabled) throw new Error("valid paste disabled");
  needPop().querySelector("[data-needgo]").click();
  const m = runsSent()[0];
  if (m?.port !== 18080 || m?.target_port !== 8080) throw new Error("ports " + JSON.stringify(m));
  if (m?.entrypoint !== "tini") throw new Error("entrypoint " + JSON.stringify(m));
  if (m?.restart !== "unless-stopped") throw new Error("restart " + JSON.stringify(m));
  if (m?.network !== "host") throw new Error("network " + JSON.stringify(m));
  // The sh -c script must stay one quoted token (spaces inside), not split into mkdir -p …
  if (!/'mkdir -p \/home\/nginxWebUI\/log;/.test(String(m?.command || ""))) throw new Error("command lost quotes: " + JSON.stringify(m));
  if (!m?.binds?.some((b) => b.includes("/Users/zhuyin/nginxWebUI"))) throw new Error("binds " + JSON.stringify(m));
  kiri.on({ type: "ran", name: "cym1102/nginxwebui:latest", error: false });
  lists([]);
});
t("run failure message is translated via tr()", () => {
  const prev = prefs.lang;
  prefs.lang = "zh";
  try {
    const s = tr("this image exposes no ports to publish on the host");
    if (!/端口|镜像/.test(s)) throw new Error(s);
  } finally {
    prefs.lang = prev;
  }
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
  if (!needPop()?.querySelector('[data-needmode="custom"]')) throw new Error("missing custom mode");
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
