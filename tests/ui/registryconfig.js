// parity: macos windows linux
// Settings → Registries: daemon.json's insecure registries and Docker Hub
// mirrors, the lists `kiri registry` edits. Adding, removing and saving send
// setregistryconfig with only the list changed; the daemon's answer replaces
// the page; what daemon.json had wrong is shown; a daemon that can't be
// reached says to start Kiri.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const cfg = (insecure, mirrors, problems = []) => ({ type: "registryconfig", "insecure-registries": insecure, "registry-mirrors": mirrors, path: "/home/me/.kiri/daemon.json", problems });
// The answer to a save: the file as read after it, and whether it saved.
const saved = (ok, insecure, mirrors) => ({ ...cfg(insecure, mirrors), saved: ok });
const pane = () => document.querySelector("#content .settings");
const open = () => { prefs.tab = "registries"; show("settings"); };
const put = (id, v) => { const el = document.getElementById(id); el.value = v; el.dispatchEvent(new Event("input", { bubbles: true })); };
const save = (k) => document.querySelector(`#content [data-regsave="${k}"]`);
const sentSave = () => sent.filter((x) => x.type === "setregistryconfig").map((x) => x.value);
// Each row's text whole, so a check cannot match a host inside another one.
const rows = (k) => [...document.querySelectorAll(`#content [data-regdel^="${k}:"]`)].map((b) => b.previousElementSibling?.textContent ?? "");
const items = [];

kiri.on(cfg([], []));
open();
for (const k of ["insecure", "mirrors"]) {
  if (document.getElementById(`reg-${k}`)) items.push(`registries ${k} field`);
  if (document.querySelector(`#content [data-regadd="${k}"]`)) items.push(`registries ${k} add`);
  if (save(k)) items.push(`registries ${k} save`);
}

t("with nothing listed, both lists say what that means", () => {
  const s = pane().textContent;
  if (!s.includes("every registry is checked over HTTPS")) throw new Error("insecure: " + s);
  if (!s.includes("No mirrors yet")) throw new Error("mirrors: " + s);
  if (!save("insecure").disabled || !save("mirrors").disabled) throw new Error("save enabled with nothing changed");
  if (!s.includes("/home/me/.kiri/daemon.json")) throw new Error("no path");
});
t("opening Settings asks for the lists", () => {
  sent.length = 0;
  ui.regcfg = null;
  show("settings");
  if (!sent.some((m) => m.type === "registryconfig")) throw new Error("sent " + sent.map((m) => m.type).join(","));
  kiri.on(cfg([], []));
});
t("adding an insecure host, then Save, sends that list only", () => {
  put("reg-insecure", "registry.lan:5000");
  document.querySelector('#content [data-regadd="insecure"]').click();
  if (JSON.stringify(rows("insecure")) !== JSON.stringify(["registry.lan:5000"])) throw new Error("rows " + rows("insecure"));
  if (rows("mirrors").length) throw new Error("mirror row");
  sent.length = 0;
  save("insecure").click();
  if (JSON.stringify(sentSave()) !== JSON.stringify([{ "insecure-registries": ["registry.lan:5000"] }])) throw new Error("sent " + JSON.stringify(sent));
  kiri.on(saved(true, ["registry.lan:5000"], []));
});
t("Enter in the field adds, as the button does", () => {
  put("reg-mirrors", "https://mirror.example.com");
  document.getElementById("reg-mirrors").dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  if (JSON.stringify(rows("mirrors")) !== JSON.stringify(["https://mirror.example.com"])) throw new Error("rows " + rows("mirrors"));
  sent.length = 0;
  save("mirrors").click();
  if (JSON.stringify(sentSave()) !== JSON.stringify([{ "registry-mirrors": ["https://mirror.example.com"] }])) throw new Error("sent " + JSON.stringify(sent));
  kiri.on(saved(true, ["registry.lan:5000"], ["https://mirror.example.com"]));
});
t("saving one list keeps what is typed into the other", () => {
  kiri.on(cfg(["registry.lan:5000"], ["https://mirror.example.com"]));
  open();
  put("reg-mirrors", "https://second.example.com");
  document.querySelector('#content [data-regadd="mirrors"]').click();
  put("reg-insecure", "other.lan:5000");
  document.querySelector('#content [data-regadd="insecure"]').click();
  put("reg-insecure", "half-typed.lan");
  sent.length = 0;
  save("insecure").click();
  // The daemon saved the insecure list.
  kiri.on(saved(true, ["registry.lan:5000", "other.lan:5000"], ["https://mirror.example.com"]));
  if (JSON.stringify(rows("mirrors")) !== JSON.stringify(["https://mirror.example.com", "https://second.example.com"])) throw new Error("mirror edits lost: " + rows("mirrors"));
  if (!save("mirrors") || save("mirrors").disabled) throw new Error("mirror Save no longer offered");
  if (!save("insecure").disabled) throw new Error("saved list still shows changes");
  if (JSON.stringify(rows("insecure")) !== JSON.stringify(["registry.lan:5000", "other.lan:5000"])) throw new Error("rows " + rows("insecure"));
  // Saved too, so the next check starts clean.
  save("mirrors").click();
  kiri.on(saved(true, ["registry.lan:5000", "other.lan:5000"], ["https://mirror.example.com", "https://second.example.com"]));
});
t("a save the daemon refused keeps what was typed", () => {
  kiri.on(cfg(["registry.lan:5000"], ["https://mirror.example.com"]));
  open();
  put("reg-insecure", "bad host");
  document.querySelector('#content [data-regadd="insecure"]').click();
  save("insecure").click();
  // The error notice comes with the file as it was.
  kiri.on(saved(false, ["registry.lan:5000"], ["https://mirror.example.com"]));
  if (JSON.stringify(rows("insecure")) !== JSON.stringify(["registry.lan:5000", "bad host"])) throw new Error("edit erased: " + rows("insecure"));
  // Taken out again, as a person would, so the next check starts clean.
  document.querySelector('#content [data-regdel="insecure:1"]').click();
  kiri.on(cfg(["registry.lan:5000"], ["https://mirror.example.com"]));
});
t("a save the daemon wrote in the file's own spelling leaves nothing to save", () => {
  kiri.on(cfg(["registry.lan:5000"], ["https://mirror.example.com"]));
  open();
  put("reg-insecure", "Registry.LAN:5000");
  document.querySelector('#content [data-regadd="insecure"]').click();
  put("reg-mirrors", "https://mirror.example.com/");
  document.querySelector('#content [data-regadd="mirrors"]').click();
  save("insecure").click();
  // Saved, and the file reads as it did: the same host.
  kiri.on(saved(true, ["registry.lan:5000"], ["https://mirror.example.com"]));
  if (JSON.stringify(rows("insecure")) !== JSON.stringify(["registry.lan:5000"])) throw new Error("rows " + rows("insecure"));
  if (!save("insecure").disabled) throw new Error("Save still offered for a saved list");
  if (save("mirrors").disabled) throw new Error("the other list's edit was dropped");
  save("mirrors").click();
  kiri.on(saved(true, ["registry.lan:5000"], ["https://mirror.example.com"]));
  if (!save("mirrors").disabled) throw new Error("Save still offered for the saved mirrors");
});
t("a daemon gone during a save keeps what was typed", () => {
  kiri.on(cfg(["registry.lan:5000"], []));
  open();
  put("reg-insecure", "other.lan:5000");
  document.querySelector('#content [data-regadd="insecure"]').click();
  save("insecure").click();
  kiri.on({ type: "registryconfig", error: "connection refused", saved: false });
  if (JSON.stringify(ui.regDraft.insecure) !== JSON.stringify(["registry.lan:5000", "other.lan:5000"])) throw new Error("draft " + JSON.stringify(ui.regDraft.insecure));
  // Back, the edit is still there to save.
  kiri.on(cfg(["registry.lan:5000"], []));
  if (JSON.stringify(rows("insecure")) !== JSON.stringify(["registry.lan:5000", "other.lan:5000"])) throw new Error("rows " + rows("insecure"));
  document.querySelector('#content [data-regdel="insecure:1"]').click();
});
t("a read that crosses a save, and a refresh, keep half-typed text and the save's answer", () => {
  kiri.on(cfg(["registry.lan:5000"], []));
  open();
  put("reg-insecure", "other.lan:5000");
  document.querySelector('#content [data-regadd="insecure"]').click();
  put("reg-mirrors", "https://half");
  save("insecure").click();
  // Settings opened meanwhile: its read answers first, with the file before the save.
  kiri.on(cfg(["registry.lan:5000"], []));
  if (ui.regSaving !== "insecure") throw new Error("a read ended the save: " + ui.regSaving);
  if (ui.regAdd.mirrors !== "https://half") throw new Error("typed text erased by a read: " + ui.regAdd.mirrors);
  kiri.on(saved(true, ["registry.lan:5000", "other.lan:5000"], []));
  if (!save("insecure").disabled) throw new Error("saved list still shows changes");
  if (ui.regAdd.mirrors !== "https://half") throw new Error("the other field was cleared by this save: " + ui.regAdd.mirrors);
  if (document.getElementById("reg-mirrors").value !== "https://half") throw new Error("field " + document.getElementById("reg-mirrors").value);
  put("reg-mirrors", "");
});
t("the daemon's answer resets the page to what was saved", () => {
  kiri.on(cfg(["registry.lan:5000", "10.0.0.0/8"], ["https://mirror.example.com"]));
  open();
  if (JSON.stringify(rows("insecure")) !== JSON.stringify(["registry.lan:5000", "10.0.0.0/8"])) throw new Error("rows " + rows("insecure"));
  if (!save("insecure").disabled || !save("mirrors").disabled) throw new Error("save enabled");
});
t("removing one takes it out of the list Save sends", () => {
  document.querySelector('#content [data-regdel="insecure:0"]').click();
  sent.length = 0;
  save("insecure").click();
  if (JSON.stringify(sentSave()) !== JSON.stringify([{ "insecure-registries": ["10.0.0.0/8"] }])) throw new Error("sent " + JSON.stringify(sent));
});
t("what daemon.json had wrong is shown", () => {
  kiri.on(cfg(["registry.lan:5000"], [], ["insecure-registries: 0.0.0.0/0 is every address"]));
  open();
  if (!pane().textContent.includes("0.0.0.0/0 is every address")) throw new Error(pane().textContent);
});
t("with no daemon the page says to start Kiri", () => {
  kiri.on({ type: "registryconfig", error: "connection refused" });
  open();
  if (document.getElementById("reg-insecure") || document.getElementById("reg-mirrors")) throw new Error("field shown");
  if (!pane().textContent.includes("Start Kiri")) throw new Error("no hint");
});
t("without sign-ins in the app, the tab still holds both lists", () => {
  const was = ui.loginsOn;
  ui.loginsOn = false;
  kiri.on(cfg([], []));
  open();
  if (!document.getElementById("reg-insecure") || !document.getElementById("reg-mirrors")) throw new Error("lists missing");
  if (document.querySelector("[data-regopen]")) throw new Error("sign-in shown");
  ui.loginsOn = was;
});
report([...R, ...items.map((x) => "ITEM " + x)]);
