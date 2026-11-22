// Issue 188: the sidebar's Linux VM card, the status bar and the main area
// all show the engine's real state, whichever control started or stopped it:
// the card, the main area's Start Kiri, Settings, or the menu bar / tray
// (which reach the engine without the page). Asleep is still on: the VM
// rests to save memory and wakes on the next docker command.
kiri.on({ type: "status", version: "0.2.0", dev: false, app: "Kiri", ours: "kiri", context: "kiri" });
kiri.on({ type: "prefs", prefs: {} });
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const vbtn = () => document.getElementById("vm-toggle");
const card = () => [document.getElementById("vm-state").textContent, vbtn().textContent];
const bar = () => document.getElementById("statusbar").textContent;
const main = () => document.getElementById("empty-start") ? "not running" : "list";
// Read as a person reads it: the card keeps a number with its unit by no-break spaces.
const mem = () => document.getElementById("vm-mem").textContent.replace(/\u00a0/g, " ");
const ON = ["Running", "Stop"], OFF = ["Off", "Start"];
const state = (s) => kiri.on({ type: "state", state: s });
const vm = (awake) => kiri.on({ type: "vm", awake, used_mib: awake ? 900 : 0, target_mib: awake ? 1200 : 0, ceiling_mib: 6144, running: 0, cpus: 4 });
const everywhere = () => [card(), bar().includes(VM + " running") ? "running" : bar().includes(VM + " off") ? "off" : bar(), main()];
const RUNNING = [ON, "running", "list"], STOPPED = [OFF, "off", "not running"];
// No container of yours running: only the switches decide.
kiri.on({ type: "lists", containers: [], images: [], volumes: [], networks: [] });
show("containers");
state("stopped");

t("Start from the card, then Stop from the card: all three say off", () => {
  vbtn().click(); eq(sent.at(-1)?.type, "start", "sent");
  state("booting"); vm(true); state("running");
  eq(everywhere(), RUNNING, "after Start");
  vbtn().click(); eq(sent.at(-1)?.type, "stop", "sent");
  state("stopping"); state("stopped");
  eq(everywhere(), STOPPED, "after Stop");
});
t("then Start Kiri in the main area: card, status bar and main area all say running", () => {
  document.getElementById("empty-start").click();
  eq(sent.at(-1)?.type, "start", "sent");
  state("booting");
  eq(card(), ["Starting…", "Starting…"], "starting");
  vm(true); state("running");
  eq(everywhere(), RUNNING, "running");
});
t("the card's memory row shows what the VM uses, not —", () => eq([mem() !== "—", /GB/.test(mem())], [true, true], "memory: " + mem()));
t("the card's Stop then stops it", () => {
  vbtn().click(); eq(sent.at(-1)?.type, "stop", "sent");
  state("stopping"); state("stopped");
  eq(everywhere(), STOPPED, "stopped");
});

t("Start from the card, Stop from Settings › Resources: all three say off", () => {
  vbtn().click(); state("booting"); vm(true); state("running");
  prefs.tab = "resources"; show("settings");
  document.querySelector("[data-vmpower]").click();
  eq(sent.at(-1)?.type, "stop", "sent");
  state("stopping"); state("stopped");
  show("containers");
  eq(everywhere(), STOPPED, "stopped");
});
t("Start from the menu bar or tray (the page sends nothing): all three say running", () => {
  const n = sent.length;
  state("booting"); vm(true); state("running");
  eq(sent.length, n, "page sent nothing");
  eq(everywhere(), RUNNING, "running");
});
t("Stop from the menu bar or tray: all three say off", () => {
  state("stopping"); state("stopped");
  eq(everywhere(), STOPPED, "stopped");
});
t("Settings › Resources agrees with the card after a Start from the main area", () => {
  document.getElementById("empty-start").click(); state("booting"); vm(true); state("running");
  prefs.tab = "resources"; show("settings");
  const b = document.querySelector("[data-vmpower]");
  eq([document.querySelector(".pref .t")?.textContent, b?.dataset.vmpower], ["Running", "stop"], "settings");
  show("containers");
});

// Asleep: the daemon is up and not stopped, the VM rests. docker run works.
t("asleep: card, status bar and main area all say running", () => {
  vm(false); state("asleep");
  eq(everywhere(), RUNNING, "asleep");
});
t("asleep: the memory row shows the VM holds none, not —", () => eq(mem(), "0 MB / 6 GB", "memory"));
t("asleep after a Stop from the card then a Start from the main area still reads running", () => {
  vbtn().click(); state("stopping"); state("stopped");
  document.getElementById("empty-start").click(); state("booting"); vm(true); state("running");
  vm(false); state("asleep");
  eq(everywhere(), RUNNING, "asleep");
});
t("waking from asleep reads running, not Starting…", () => {
  state("booting");
  eq(card(), ON, "waking");
  vm(true); state("running");
});
t("a fresh Kiri whose engine is up and asleep reads running", () => {
  vm(false); state("asleep");
  eq(everywhere(), RUNNING, "fresh asleep");
});
report(R);
