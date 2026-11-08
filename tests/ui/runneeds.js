// Images → Run on an image that can't start as it is: a popover on its Run
// button says what it needs, filled in where it can be, and Run sends that.
// Once its container is up, the button is Stop, beside how long it's been up.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const nowS = Date.now() / 1000;
const imgRow = (tag, i) => ({ Id: "sha256:" + String(i).repeat(64), RepoTags: [tag], Size: 1e8, Created: nowS - 3600 * i });
const RUN_TAGS = ["postgres:17", "mysql:latest", "opensearchproject/opensearch:3.3.0", "nginx:alpine", "mcr.microsoft.com/mssql/server:2022-latest", "acme/api:1", "kiri-test/needs-password:1", "acme/two:1"];
const imgLists = (containers) => kiri.on({ type: "lists", containers, images: RUN_TAGS.map((tag, i) => imgRow(tag, i + 1)), volumes: [], networks: [] });
imgLists([]);
show("images");
const runOf = (tag) => document.querySelector(`[data-run="${CSS.escape(tag)}"]`);
const needPop = () => document.querySelector(".need");
const runsSent = () => sent.filter((m) => m.type === "run");
// The engine answers Run's look at the start script with `vars`.
const pressRun = (tag, vars = []) => {
  sent.length = 0;
  runOf(tag).click();
  if (sent.some((m) => m.type === "needs")) kiri.on({ type: "needs", image: tag, id: ui.images.find((i) => i.RepoTags.includes(tag))?.Id, vars });
};

t("Run and a list refresh change the row in place, not the table", () => {
  const row = runOf("nginx:alpine").closest("tr"), other = runOf("postgres:17").closest("tr"), tileEl = other.querySelector(".tile");
  imgLists([]);
  if (!row.isConnected || !other.isConnected || !tileEl.isConnected) throw new Error("rows redrawn on refresh");
  ui.running.add("nginx:alpine"); render();
  if (!row.isConnected || !row.querySelector(".rolling")) throw new Error("row redrawn for its spinner");
  ui.running.delete("nginx:alpine"); render();
});
t("an image that needs nothing opens a name popover", () => {
  pressRun("nginx:alpine");
  if (runsSent().length) throw new Error("ran without asking");
  if (!/Run nginx:alpine/.test(needPop()?.textContent) || !needPop().querySelector("[data-needcname]")) throw new Error(needPop()?.textContent);
  needPop().querySelector("[data-needgo]").click();
  const m = runsSent()[0];
  if (runsSent().length !== 1 || m.value.length || m.container !== "nginx" || m.port) throw new Error(JSON.stringify(runsSent()));
  kiri.on({ type: "ran", name: "nginx:alpine", error: false });
});
t("postgres: a popover on its Run button, with a password made", () => {
  pressRun("postgres:17");
  if (runsSent().length) throw new Error("ran without asking");
  if (!/postgres:17 needs a password to start/.test(needPop()?.textContent)) throw new Error(needPop()?.textContent);
  const b = runOf("postgres:17").getBoundingClientRect(), r = needPop().getBoundingClientRect();
  if (Math.abs(r.top - (b.bottom + 9)) > 2 && Math.abs(r.bottom - (b.top - 9)) > 2) throw new Error(`popover at ${r.top}, button ${b.top}-${b.bottom}`);
  const f = needPop().querySelector("[data-needpw]");
  if (f.type !== "password" || f.value.length !== 20) throw new Error("field " + f.type + " " + f.value.length);
  if (needPop().querySelector("[data-needgo]").disabled) throw new Error("Run off");
});
t("the eye shows it, More settings opens", () => {
  needPop().querySelector("[data-needeye]").click();
  if (needPop().querySelector("[data-needpw]").type !== "text") throw new Error("still hidden");
  needPop().querySelector("[data-needdisc]").click();
  if (needPop().querySelector(".more").hidden) throw new Error("more stayed shut");
  const db = needPop().querySelector('[data-needmore="POSTGRES_DB"]');
  db.value = "shop"; db.dispatchEvent(new Event("input", { bubbles: true }));
});
t("Run sends the password and the extra setting, then the popover goes", () => {
  const v = needPop().querySelector("[data-needpw]").value;
  sent.length = 0;
  needPop().querySelector("[data-needgo]").click();
  if (JSON.stringify(runsSent()[0]?.value) !== JSON.stringify([`POSTGRES_PASSWORD=${v}`, "POSTGRES_DB=shop"])) throw new Error(JSON.stringify(runsSent()));
  if (!/Starting…/.test(needPop()?.textContent)) throw new Error("not busy");
  kiri.on({ type: "ran", name: "postgres:17", error: false });
  if (needPop()) throw new Error("popover stayed");
});
t("once it runs, Run is Stop with how long it's been up, which opens it", () => {
  kiri.on({ type: "notice", kind: "ok", text: "Started brave_hopper", open: "pg1", image: "postgres:17" });
  imgLists([{ Id: "pg1", Names: ["/brave_hopper"], Image: "postgres:17", State: "running", Status: "Up 1 second", Created: nowS, Ports: [], Labels: {} }]);
  runSay("pg1"); // still up a few seconds later
  const stop = document.querySelector('[data-c="stop"][data-id="pg1"]');
  if (!stop || !stop.classList.contains("halt")) throw new Error("no Stop");
  const up = stop.previousElementSibling;
  if (!up?.matches(".run-up[data-golive]") || !/^Up /.test(up.textContent)) throw new Error("no uptime beside it: " + up?.outerHTML);
  if (runOf("postgres:17")) throw new Error("still a Run");
  up.click();
  if (ui.detail?.id !== "pg1") throw new Error("didn't open it");
});
t("back on Images, it is play again", () => {
  show("images");
  if (!runOf("postgres:17") || document.querySelector("[data-golive], .run-up")) throw new Error("still Stop");
});
t("mysql: a random root password is one choice", () => {
  pressRun("mysql:latest");
  if (!/needs a root password/.test(needPop()?.textContent)) throw new Error(needPop()?.textContent);
  needPop().querySelector('[name=need-way][value=random]').click();
  needPop().querySelector("[data-needgo]").click();
  if (JSON.stringify(runsSent()[0]?.value) !== JSON.stringify(["MYSQL_RANDOM_ROOT_PASSWORD=yes"])) throw new Error(JSON.stringify(runsSent()));
  kiri.on({ type: "ran", name: "mysql:latest", error: false });
});
t("opensearch: a weak password can't run; security off can", () => {
  pressRun("opensearchproject/opensearch:3.3.0");
  const f = needPop().querySelector("[data-needpw]");
  f.value = "weak"; f.dispatchEvent(new Event("input", { bubbles: true }));
  if (!needPop().querySelector("[data-needgo]").disabled) throw new Error("weak accepted");
  needPop().querySelector("[data-needoff]").click();
  if (needPop().querySelector("[data-needgo]").disabled || !needPop().classList.contains("sec-off")) throw new Error("security off refused");
  needPop().querySelector("[data-needgo]").click();
  const v = runsSent()[0]?.value || [];
  if (!v.includes("DISABLE_SECURITY_PLUGIN=true") || !v.includes("discovery.type=single-node") || v.some((e) => e.startsWith("OPENSEARCH_INITIAL"))) throw new Error(JSON.stringify(v));
  kiri.on({ type: "ran", name: "opensearchproject/opensearch:3.3.0", error: false });
});
t("SQL Server: Run waits for the licence", () => {
  pressRun("mcr.microsoft.com/mssql/server:2022-latest");
  if (!needPop().querySelector("[data-needgo]").disabled) throw new Error("ran without the licence");
  needPop().querySelector("[data-needaccept]").click();
  if (needPop().querySelector("[data-needgo]").disabled) throw new Error("still off");
});
t("Esc closes it, and nothing runs", () => {
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  if (needPop() || runsSent().length) throw new Error("open or ran");
});
t("an image whose container stopped for a variable asks for it next time", () => {
  needLearn("acme/api:1", "API_TOKEN");
  pressRun("acme/api:1");
  if (!/acme\/api:1 needs a password to start/.test(needPop()?.textContent) || !/API_TOKEN wasn't set/.test(needPop().textContent)) throw new Error(needPop()?.textContent);
  needClose();
  try { localStorage.removeItem("kiri.needs"); } catch {}
});
t("the first stop for a variable is enough: Run asks from then on", () => {
  ui.containers.push({ Id: "learn1", Names: ["/crisp_test"], Image: "acme/api:1", State: "exited" });
  kiri.on({ type: "lastlogs", id: "learn1", lines: ["Error: APP_PASSWORD is not specified.", "       You must specify APP_PASSWORD to start this app."] });
  ui.containers = ui.containers.filter((x) => x.Id !== "learn1");
  pressRun("acme/api:1");
  if (!/APP_PASSWORD wasn't set/.test(needPop()?.textContent || "")) throw new Error(needPop()?.textContent || "no popover; it ran");
  needClose();
  try { localStorage.removeItem("kiri.needs"); } catch {}
});
t("any other image: Run reads its start script first, and asks for what it exits without", () => {
  sent.length = 0;
  runOf("kiri-test/needs-password:1").click();
  if (!sent.some((m) => m.type === "needs" && m.name === "kiri-test/needs-password:1")) throw new Error("didn't look: " + JSON.stringify(sent));
  if (runsSent().length || !runOf("kiri-test/needs-password:1").classList.contains("rolling")) throw new Error("ran, or no spinner while looking");
  kiri.on({ type: "needs", image: "kiri-test/needs-password:1", id: ui.images.find((i) => i.RepoTags.includes("kiri-test/needs-password:1")).Id, vars: [{ name: "APP_PASSWORD", alts: [], instead: [], why: "APP_PASSWORD is not specified." }] });
  const pop = needPop();
  if (!pop || !/needs a password/.test(pop.textContent) || !/APP_PASSWORD is not specified/.test(pop.textContent)) throw new Error(pop?.textContent || "no popover");
  pop.querySelector("[data-needgo]").click();
  if (runsSent().length !== 1 || !/^APP_PASSWORD=.{8,}/.test(runsSent()[0].value[0])) throw new Error(JSON.stringify(runsSent()));
  kiri.on({ type: "ran", name: "kiri-test/needs-password:1", error: false });
  needClose(true);
});
t("it looks once per image build", () => {
  sent.length = 0;
  runOf("kiri-test/needs-password:1").click();
  if (sent.some((m) => m.type === "needs") || !needPop()) throw new Error("looked again, or no popover");
  needClose();
});
t("two variables: both fields, Run waits for both", () => {
  runOf("acme/two:1").scrollIntoView();
  pressRun("acme/two:1", [{ name: "DB_PASSWORD", alts: [], instead: [], why: "" }, { name: "DB_HOST", alts: [], instead: [], why: "" }]);
  const pop = needPop(), host = pop?.querySelector('[data-needalso="DB_HOST"]');
  if (!host || !pop.querySelector("[data-needgo]").disabled) throw new Error("no DB_HOST field, or Run without it");
  host.value = "db"; host.dispatchEvent(new Event("input", { bubbles: true }));
  if (pop.querySelector("[data-needgo]").disabled) throw new Error("still off");
  needClose();
});
t("the container Run looks with never shows in Containers", () => {
  kiri.on({ type: "lists", containers: [{ Id: "probe1", Names: ["/x"], Image: "acme/two:1", State: "created", Labels: { "io.kiri.probe": "1" } }], images: RUN_TAGS.map((tag, i) => imgRow(tag, i + 1)), volumes: [], networks: [] });
  if (ui.containers.length) throw new Error("listed");
});
report(R);
