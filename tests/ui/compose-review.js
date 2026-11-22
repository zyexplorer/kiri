// parity: macos windows linux
// window: 1440x900
// A project's page asks for its file and its plan on whichever tab it opens on, so Apply is the same on every tab
// (busy, with a tooltip, while the plan is worked out). The Beta tag by Compose's title survives every refresh. A
// notice's row is as tall as a container's; its Show in file goes to its line. A not-created service's Restart says
// Apply's words; the page counts what the list counts.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const now = Math.floor(Date.now() / 1000);
const C = (id, project, service, n, state) => ({ Id: id, Names: [`/${project}-${service}-${n}`], Image: `${service}:dev`, State: state, Status: state === "running" ? "Up 1 hour" : "Exited (0)",
  Created: now - 600, Ports: [], Labels: { "com.docker.compose.project": project, "com.docker.compose.service": service } });
const ctrs = [C("s1", "shop", "api", 1, "running"), C("s2", "shop", "api", 2, "running"), C("s3", "shop", "db", 1, "exited"), C("b1", "blog", "app", 1, "running"), C("o1", "odd", "app", 1, "running")];
const svc = (name, image = name + ":dev") => ({ name, image, build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
const mem = (p) => ctrs.filter((c) => c.Labels["com.docker.compose.project"] === p).map((c) => ({ service: c.Labels["com.docker.compose.service"], id: c.Id, name: c.Names[0].slice(1), image: c.Image, state: c.State, status: c.Status }));
const P = [
  { name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, services: [svc("api"), svc("db"), svc("mailer", "axllent/mailpit:latest")], containers: mem("shop"),
    namewarn: { text: "${API_TOKEN} is used in a name or folder path. Docker shows those in plain text.", errkey: "nameSecret", errargs: { name: "${API_TOKEN}", names: ["${API_TOKEN}"] } } },
  { name: "blog", dir: "/w/blog", files: ["/w/blog/compose.yaml"], readable: true, services: [], containers: mem("blog"),
    error: "Compose rejects this file: YAML error on line 3", errkey: "yamlLine", errargs: { n: "3" } },
  { name: "lone", dir: "/w/lone", files: ["/w/lone/compose.yaml"], readable: true, services: [svc("web")], containers: [],
    error: "Compose rejects this file. A required variable may not be set: LONE_KEY.", errkey: "reqUnset", errargs: { name: "LONE_KEY" } },
  // One sentence, nothing under it.
  { name: "odd", dir: "/w/odd", files: ["/w/odd/compose.yaml"], readable: true, services: [svc("app")], containers: mem("odd"), error: "invalid compose project" },
];
const SHOP = "services:\n  api:\n    image: api:dev\n    container_name: shop-${API_TOKEN}\n  db:\n    image: db:dev\n  mailer:\n    image: axllent/mailpit:latest\n";
const BLOG = "services:\n  app:\n    image: app:dev\n  bad: [\n";
const LONE = "services:\n  web:\n    image: web:dev\n    environment:\n      KEY: ${LONE_KEY:?set it}\n";
ui.vm = "running";
kiri.on({ type: "status", version: "0.1.4", dev: true, share: true, app: "Kiri", ours: "kiri", context: "kiri" });
const lists = () => kiri.on({ type: "lists", containers: ctrs, images: [], volumes: [], networks: [] });
const projects = () => kiri.on({ type: "composeprojects", logos: {}, projects: P });
lists(); projects();
const $c = (q) => document.querySelector("#content " + q), $$c = (q) => [...document.querySelectorAll("#content " + q)];
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const apply = () => document.querySelector("[data-cmpapply]");
const applyState = () => { const b = apply(); return b && { disabled: b.disabled, busy: b.getAttribute("aria-busy") === "true", title: b.title, spin: !!b.querySelector(".hspin") }; };
const plans = () => sent.filter((m) => m.type === "composeplan" && m.name === "shop");
const file = (name, text) => kiri.on({ type: "composefile", name, path: `/w/${name}/compose.yaml`, text, hash: "h-" + name });
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const beta = () => !!document.querySelector("#title .beta");

(async () => {
  show("compose");
  t("the Beta tag by the title stays through every refresh", () => {
    eq(beta(), true, "first");
    for (let i = 0; i < 4; i++) { lists(); projects(); render(); eq([beta(), document.getElementById("title").textContent], [true, "ComposeBeta"], "refresh " + i); }
    show("containers"); show("compose"); eq(beta(), true, "after another view");
  });
  t("the header keeps its pills and Open button through the refreshes too", () => {
    eq([[...document.querySelectorAll("#ltabs .ltab")].map((b) => b.textContent), !!document.querySelector("#hright [data-cmpopenfile]")], [["All5", "Running4", "Stopped1"], true], "header");
  });
  t("a notice's row is as tall as a container's, one sentence or two", () => {
    const one = $c('tr.warnrow[data-cmpnote="odd"]'), two = $c('tr.warnrow[data-cmpnote="shop"]'), ctr = $c('tr[data-key="s1"]');
    eq([one.querySelector(".wtitle").textContent, one.querySelector(".sub-name")?.textContent.trim() || ""], ["invalid compose project", ""], "one sentence");
    eq([one.offsetHeight, two.offsetHeight], [ctr.offsetHeight, ctr.offsetHeight], "heights");
  });
  t("a not-created service's dimmed Restart says what its Start says", () => {
    const r = $c('tr.unmade[data-svc="mailer"]');
    eq([...r.querySelectorAll(".acts-row [aria-label]")].slice(0, 2).map((b) => b.getAttribute("aria-label")), ["Apply the compose file to create it", "Apply the compose file to create it"], "words");
  });
  for (const tab of ["structure", "services"]) {
    sent.length = 0;
    cmpOpen("shop"); if (tab === "services") { cmp.tab = "services"; render(); }
    t(`opened on ${tab}: Apply is busy with a tooltip while the file and its plan come`, () => {
      eq(sent.some((m) => m.type === "composefile" && m.name === "shop"), true, "file asked for");
      eq(applyState(), { disabled: true, busy: true, title: "Loading…", spin: true }, "before the file");
    });
    file("shop", SHOP);
    await wait(450);
    t(`opened on ${tab}: the plan is asked for without the file tab`, () => {
      eq([cmp.tab, plans().length], [tab, 1], "plan asked for");
      eq(applyState().busy, true, "still busy till it comes");
    });
    kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "api", act: "same" }, { n: "db", act: "same" }, { n: "mailer", act: "add", to: "axllent/mailpit:latest" }], services: P[0].services });
    t(`opened on ${tab}: Apply enables when the plan comes`, () => eq(applyState(), { disabled: false, busy: false, title: "", spin: false }, "Apply"));
    cmp.open = null; render();
  }
  t("the page counts what the list counts: containers running, and the services it lists", () => {
    cmpOpen("shop");
    const list = (() => { cmp.open = null; render(); return $c('tr.group[data-group="shop"] .sub-name').textContent.replace(/‏/g, ""); })();
    cmpOpen("shop");
    const dd = [...document.querySelectorAll("#cmp-head .dfacts div")].map((d) => [d.querySelector("dt").textContent, d.querySelector("dd").textContent]);
    eq([list, $c("#cmp-head .pill").textContent], ["Compose · 2 of 3 running", "2 of 3 running"], "same count");
    eq(dd.find(([k]) => k === "Services")[1], "3", "api, db and mailer");
    cmp.open = null; render();
  });
  t("a file Kiri couldn't read: Services says — when the page lists none", () => {
    cmpOpen("lone");
    eq([...document.querySelectorAll("#cmp-head .dfacts div")].map((d) => [d.querySelector("dt").textContent, d.querySelector("dd").textContent]).find(([k]) => k === "Services")[1], "1", "lone lists web");
    cmp.open = null; render();
    const was = P[2].services; P[2].services = []; projects(); cmpOpen("lone");
    eq([...document.querySelectorAll("#cmp-head .dfacts div")].map((d) => [d.querySelector("dt").textContent, d.querySelector("dd").textContent]).find(([k]) => k === "Services")[1], "—", "none read");
    P[2].services = was; cmp.open = null; projects(); render();
  });
  // Show in file on a notice: its line once the file has come, or the first line using its variable.
  const line = () => { const ta = document.getElementById("cmp-ta"); return ta.value.slice(0, ta.selectionStart).split("\n").length; };
  for (const [proj, text, want] of [["blog", BLOG, 3], ["shop", SHOP, 4], ["lone", LONE, 5]]) {
    cmp.open = null; render();
    click($c(`tr.warnrow[data-cmpnote="${proj}"] [data-mfile]`));
    file(proj, text);
    await wait(50);
    t(`Show in file on ${proj}'s notice goes to line ${want} once the file has come`, () => eq([cmp.open, cmp.tab, line()], [proj, "file", want], "line"));
  }
  cmp.open = null; render();
  report(R);
})();
