// Pull image: the popover (recent pulls, Docker Hub as you type, a full
// reference, the tag), pulls as rows at the top of Images, their errors, and
// the status bar's count.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const key = (k, o = {}) => window.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, ...o }));
const type = (v) => { const i = $("ppq"); i.value = v; i.dispatchEvent(new Event("input", { bubbles: true })); };
const now = Date.now() / 1000;
kiri.on({ type: "lists", containers: ui.containers, volumes: [], networks: [],
  images: [{ Id: "sha256:aaa111", RepoTags: ["nginx:alpine"], Size: 5e7, Created: 1, Pulled: now - 3600 }, { Id: "sha256:ccc333", RepoTags: ["postgres:17"], Size: 4e8, Created: 1, Pulled: now - 60 }] });
const pop = $("pp");
t("⌘⇧P / Ctrl+Shift+P opens it from any page, on Images, with the field focused", () => {
  show("volumes");
  key("p", { metaKey: !WIN, ctrlKey: WIN, shiftKey: true });
  eq([ui.view, pop.hidden, document.activeElement?.id], ["images", false, "ppq"], "open");
  eq(document.querySelector("#hright [data-ppopen]").getAttribute("aria-expanded"), "true", "button says so");
});
t("empty, it lists recent pulls, newest first", () => {
  eq([...pop.querySelectorAll(".pp-it.one b")].map((b) => b.textContent), ["postgres:17", "nginx:alpine"], "recent");
});
t("typing asks Docker Hub (once) and shows its answer, with what you have", () => {
  sent.length = 0;
  type("nginx");
  eq(pop.querySelector(".sec").textContent.trim(), "Searching Docker Hub", "searching");
  pp.timer && clearTimeout(pp.timer); hubAsk("search", "nginx");
  eq(sent.filter((m) => m.type === "hubsearch").map((m) => m.name), ["nginx"], "asked");
  kiri.on({ type: "hub", kind: "search", key: "nginx", items: [{ name: "nginx", description: "Official build of Nginx.", stars: 20900, official: true }, { name: "bitnami/nginx", description: "Bitnami", stars: 195 }] });
  const items = [...pop.querySelectorAll(".pp-it")];
  eq(items.map((b) => b.querySelector(".m b").textContent), ["nginxOfficial", "bitnami/nginx"], "results");
  eq(/Pulled 1 h ago · alpine/.test(items[0].textContent), true, "have");
  eq(items[0].getAttribute("aria-selected"), "true", "first picked");
});
t("arrows move, Enter goes to its tags, latest picked", () => {
  sent.length = 0;
  key("ArrowDown"); key("ArrowUp"); key("Enter");
  eq(pp.mode, "tags", "tags");
  eq(sent.filter((m) => m.type === "hubtags").map((m) => m.name), ["nginx"], "asked tags");
  kiri.on({ type: "hub", kind: "tags", key: "nginx", items: ["latest", "1.27", "alpine"] });
  eq([...pop.querySelectorAll(".tagr")].map((b) => [b.dataset.pptag, b.getAttribute("aria-checked")]), [["latest", "true"], ["1.27", "false"], ["alpine", "false"]], "tags");
  eq(pop.querySelector("[data-ppgo]").textContent.trim(), "Pull latest", "button");
});
t("a tag you have says so, and the button pulls again", () => {
  key("ArrowDown"); key("ArrowDown");
  eq(pp.tag, "alpine", "tag");
  eq(/Have · pulled 1 h ago/.test(pop.querySelector('[data-pptag="alpine"]').textContent), true, "have");
  eq(pop.querySelector("[data-ppgo]").textContent.trim(), "Pull again", "again");
  eq(!!pop.querySelector(".pp-note"), true, "note");
});
t("Esc goes back to the search, Esc again closes", () => {
  key("Escape");
  eq([pp.mode, document.activeElement?.id], ["search", "ppq"], "back");
  key("Escape");
  eq(pop.hidden, true, "closed");
});
t("a full reference is pulled exactly as written, and its row is at the top at once", () => {
  sent.length = 0;
  openPull("ghcr.io/kiri-dev/api:0.4");
  const first = pop.querySelector(".pp-it");
  eq([first.classList.contains("exact"), first.querySelector("small").textContent], [true, "ghcr.io/kiri-dev/api:0.4"], "exact");
  eq(pop.querySelector(".reg").textContent, "ghcr.io", "registry");
  key("Enter");
  eq(sent.filter((m) => m.type === "pull").map((m) => m.name), ["ghcr.io/kiri-dev/api:0.4"], "sent");
  eq(pop.hidden, true, "closed");
  const row = document.querySelector('#content tr.prow');
  eq(row.dataset.key, "pull:ghcr.io/kiri-dev/api:0.4", "row");
  eq(/Asking ghcr.io for kiri-dev\/api:0.4/.test(row.textContent), true, "asking");
  eq(!!row.querySelector(".tile .hspin") && !row.querySelector("button .hspin"), true, "spinner in the tile");
});
t("Hub names are pulled as the engine names them", () => {
  eq([normRef("nginx"), normRef("docker.io/library/redis"), normRef("localhost:5000/app"), normRef("a/b@sha256:abc")], ["nginx:latest", "redis:latest", "localhost:5000/app:latest", "a/b@sha256:abc"], "norm");
});
t("progress fills the row in place and the status bar counts the pulls", () => {
  pullRef("node:22");
  kiri.on({ type: "pull", state: "started", name: "node:22" });
  kiri.on({ type: "pull", state: "progress", name: "node:22", got: 2e7, total: 8e7 });
  const cell = [...document.querySelectorAll("[data-pcell]")].find((c) => c.dataset.pcell === "node:22");
  eq(cell.querySelector(".pbar span").style.width, "25%", "bar");
  eq(/20.0 MB of 80.0 MB · 25%/.test(cell.textContent), true, "numbers");
  eq($("sb-pullt").textContent, "Pulling 2 · 25%", "status bar");
  kiri.on({ type: "pull", state: "progress", name: "node:22", got: 4e7, total: 8e7 });
  eq(cell.isConnected && cell.querySelector(".pbar span").style.width, "50%", "same cell");
  const rows = [...document.querySelectorAll("#content tr.prow")].map((r) => r.dataset.key);
  eq(rows, ["pull:node:22", "pull:ghcr.io/kiri-dev/api:0.4"], "several at once, newest first");
});
t("the status bar item takes you to Images", () => {
  show("containers");
  document.querySelector('[data-status-go="images"]').click();
  eq(ui.view, "images", "images");
});
t("done: a toast says Pulled with Run, and the row goes once the image is listed", () => {
  kiri.on({ type: "pull", state: "done", name: "node:22" });
  kiri.on({ type: "pulled", name: "node:22", ms: 4200 });
  const toast = [...document.querySelectorAll(".toast.pulled")].pop();
  eq(toast.querySelector("b").textContent, "Pulled node:22", "toast");
  eq(!!toast.querySelector('[data-run="node:22"]'), true, "Run");
  kiri.on({ type: "lists", containers: ui.containers, volumes: [], networks: [], images: [...ui.images, { Id: "sha256:ddd444", RepoTags: ["node:22"], Size: 4e8, Created: 1, Pulled: now }] });
  eq(!!document.querySelector('tr[data-key="pull:node:22"]'), false, "row gone");
  eq(!!document.querySelector('tr[data-key="node:22"]'), true, "image row");
});
t("not found: the row says so, with Docker Hub's best guess", () => {
  sent.length = 0;
  pullRef("ngnix");
  kiri.on({ type: "pull", state: "done", name: "ngnix:latest", error: "Couldn't find an image called ngnix. Check the name; if it's private, sign in with docker login." });
  eq(sent.filter((m) => m.type === "hubsearch").map((m) => m.name), ["ngnix"], "asks Hub");
  kiri.on({ type: "hub", kind: "search", key: "ngnix", items: [{ name: "someone/ngnix" }, { name: "nginx", official: true }] });
  const row = document.querySelector('tr[data-key="pull:ngnix:latest"]');
  eq(row.classList.contains("bad"), true, "red");
  eq(/Not found\. Docker Hub has no image called ngnix\. Did you mean nginx\?/.test(row.textContent.replace(/\s+/g, " ")), true, row.textContent);
  row.querySelector("[data-psuggest]").click();
  eq([pop.hidden, $("ppq").value], [false, "nginx"], "searches it");
  eq(!!document.querySelector('tr[data-key="pull:ngnix:latest"]'), false, "row dismissed");
  closePull();
});
t("connection lost: Retry pulls it again", () => {
  kiri.on({ type: "pull", state: "done", name: "ghcr.io/kiri-dev/api:0.4", error: "read tcp 10.0.0.2:5123->140.82.1.1:443: connection reset by peer" });
  const row = document.querySelector('tr[data-key="pull:ghcr.io/kiri-dev/api:0.4"]');
  eq([row.classList.contains("warn"), /Connection lost/.test(row.textContent)], [true, true], "lost");
  sent.length = 0;
  row.querySelector("[data-pretry]").click();
  eq(sent.filter((m) => m.type === "pull").map((m) => m.name), ["ghcr.io/kiri-dev/api:0.4"], "again");
});
t("a private registry: sign-in needed, said in the row", () => {
  kiri.on({ type: "pull", state: "done", name: "ghcr.io/kiri-dev/api:0.4", error: "Couldn't find an image called ghcr.io/kiri-dev/api. Check the name; if it's private, sign in with docker login." });
  const row = document.querySelector('tr[data-key="pull:ghcr.io/kiri-dev/api:0.4"]');
  eq(/Sign-in needed\. ghcr.io wants a sign-in for kiri-dev\/api\./.test(row.textContent.replace(/\s+/g, " ")), true, row.textContent);
  // Signing in from the window is held back: the row says docker login, then Retry.
  eq([!!row.querySelector("[data-signin]"), !!row.querySelector("[data-pretry]"), row.textContent.includes("docker login ghcr.io")], [false, true, true], "docker login, then Retry");
  row.querySelector("[data-pdismiss]").click();
  eq(!!document.querySelector('tr[data-key="pull:ghcr.io/kiri-dev/api:0.4"]'), false, "dismissed");
  eq(document.getElementById("sb-pullt"), null, "nothing pulling");
});
t("a click outside closes it; the row's Pull button pulls that image", () => {
  openPull();
  document.body.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
  document.body.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
  eq(pop.hidden, true, "closed");
  sent.length = 0;
  document.querySelector('[data-pull="postgres:17"]').click();
  eq(sent.filter((m) => m.type === "pull").map((m) => m.name), ["postgres:17"], "pulled");
  eq(document.querySelector('[data-pull="postgres:17"]').disabled, true, "busy");
});
t("on GitHub, owner/image is a row on ghcr.io, and Enter lists its tags from the registry", () => {
  openPull();
  $("ppreg").click();
  $("ppregmenu").querySelector('[data-reg="ghcr"]').click();
  eq([pp.reg, $("ppreg").textContent.trim(), /owner\/image/.test($("ppq").placeholder)], ["ghcr", "ghcr.io", true], "chosen");
  eq(/home-assistant\/home-assistant/.test(pop.querySelector(".pp-hint").textContent), true, "hint names an example");
  type("acme");
  eq([pop.querySelectorAll(".pp-it").length, !!pop.querySelector(".pp-hint")], [0, true], "half a name: the hint");
  sent.length = 0;
  type("acme/web");
  const row = pop.querySelector(".pp-it.reg");
  eq([row.querySelector("small").textContent, sent.some((m) => m.type === "hubsearch")], ["ghcr.io/acme/web", false], "row, and Docker Hub not searched");
  key("Enter");
  eq([pp.mode, pp.repo], ["tags", "ghcr.io/acme/web"], "tags");
  eq(sent.filter((m) => m.type === "hubtags").map((m) => m.name), ["ghcr.io/acme/web"], "asked the registry");
  kiri.on({ type: "hub", kind: "tags", key: "ghcr.io/acme/web", items: ["latest", "2.1", "2.0"] });
  key("ArrowDown");
  eq(pop.querySelector("[data-ppgo]").textContent.trim(), "Pull 2.1", "button");
  sent.length = 0;
  key("Enter");
  eq(sent.filter((m) => m.type === "pull").map((m) => m.name), ["ghcr.io/acme/web:2.1"], "pulled");
});
t("a private GitHub image whose tags can't be listed says to sign in, and latest can still be pulled", () => {
  openPull("acme/secret");
  key("Enter");
  kiri.on({ type: "hub", kind: "tags", key: "ghcr.io/acme/secret", error: "403" });
  eq([pop.querySelector(".sec .r").textContent, pop.querySelector(".pp-note code").textContent, pop.querySelector("[data-ppgo]").textContent.trim()], ["Couldn't list the tags", "docker login ghcr.io", "Pull latest"], "private");
  closePull();
});
t("on Amazon ECR, owner/image is its public gallery, and the hint says how to sign in to your own", () => {
  openPull();
  $("ppreg").click();
  $("ppregmenu").querySelector('[data-reg="ecr"]').click();
  eq([$("ppreg").textContent.trim() === "public.ecr.aws", /aws ecr get-login-password/.test(pop.querySelector(".pp-hint").textContent)], [true, true], "chosen");
  type("nginx/nginx:1.27");
  eq(pop.querySelector(".pp-it.reg small").textContent, "public.ecr.aws/nginx/nginx:1.27", "row");
  type("123456789012.dkr.ecr.eu-west-1.amazonaws.com/app:3");
  const first = pop.querySelector(".pp-it");
  eq([first.classList.contains("exact"), $("ppreg").textContent.trim()], [true, "123456789012.dkr.ecr.eu-west-1.amazonaws.com"], "your own registry, pulled as typed, under ECR's logo");
  closePull();
  pp.reg = "docker";
});
report(R);
