// parity: macos windows linux
// Settings → Domains → Ports: the routers' ports, on every system. Changing
// them sends setrouterports; a port an environment variable fixes can't be
// edited; a paused router says why; links carry a port that isn't 80 or 443.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const rpEvent = (http, https, extra = {}) => ({ type: "routerports",
  http: { port: http, default: 80, serving: true, env: null, ...(extra.http || {}) },
  https: { port: https, default: 443, serving: true, env: null, ...(extra.https || {}) } });
const domains = () => { prefs.tab = "domains"; show("settings"); };
const put = (id, v) => { const el = document.getElementById(id); el.value = v; el.dispatchEvent(new Event("input", { bubbles: true })); };
const items = [];

t("the list asks for the ports once the engine answers", () => {
  sent.length = 0; ui.rports = null; ui.rpAt = 0;
  kiri.on({ type: "lists", containers: [], images: [], volumes: [], networks: [] });
  if (!sent.some((m) => m.type === "routerports")) throw new Error("sent " + sent.map((m) => m.type).join(","));
});

kiri.on(rpEvent(80, 443));
domains();
for (const id of ["rp-http", "rp-https"]) if (document.getElementById(id)) items.push(`domains ports field ${id}`);
if (document.querySelector("#content [data-saverp]")) items.push("domains ports button");

t("the boxes show the ports, and the button waits for a change", () => {
  if (document.getElementById("rp-http").value !== "80" || document.getElementById("rp-https").value !== "443") throw new Error("wrong values");
  if (!document.querySelector("#content [data-saverp]").disabled) throw new Error("button enabled with nothing changed");
});
t("a new port enables the button and sends both ports", () => {
  put("rp-http", "8080");
  const b = document.querySelector("#content [data-saverp]");
  if (b.disabled) throw new Error("still disabled");
  sent.length = 0;
  b.click();
  const m = sent.find((x) => x.type === "setrouterports");
  if (!m || m.value.http !== 8080 || m.value.https !== 443) throw new Error("sent " + JSON.stringify(sent));
});
t("the answer resets the boxes to what the engine has", () => {
  kiri.on(rpEvent(8080, 443));
  domains();
  if (document.getElementById("rp-http").value !== "8080") throw new Error("box shows " + document.getElementById("rp-http").value);
  if (!document.querySelector("#content [data-saverp]").disabled) throw new Error("button enabled");
  if (!/Names answer on 8080 and 443/.test(document.querySelector("#content .settings").textContent)) throw new Error("description keeps the old ports");
});
t("links carry a port that isn't the default", () => {
  if (!localUrl({ Labels: {} }, "web").endsWith("web.kiri.localhost:8080")) throw new Error(localUrl({ Labels: {} }, "web"));
  kiri.on(rpEvent(80, 443));
  if (!localUrl({ Labels: {} }, "web").endsWith("web.kiri.localhost")) throw new Error(localUrl({ Labels: {} }, "web"));
});
t("a port set by an environment variable can't be edited, and says so", () => {
  kiri.on(rpEvent(8880, 443, { http: { env: "KIRI_HTTP_PORT" } }));
  domains();
  if (!document.getElementById("rp-http").disabled) throw new Error("box editable");
  if (!document.querySelector("#content .rp-notes").textContent.includes("KIRI_HTTP_PORT")) throw new Error("no note");
});
t("a router a container moved aside says it's paused", () => {
  kiri.on(rpEvent(80, 443, { http: { running: true, serving: false } }));
  domains();
  if (!/paused: a container/.test(document.querySelector("#content .rp-notes").textContent)) throw new Error("no note");
});
t("a router that couldn't start says its port is in use", () => {
  kiri.on(rpEvent(80, 443, { http: { running: false, serving: false } }));
  domains();
  const n = document.querySelector("#content .rp-notes").textContent;
  if (!/off: port 80 is in use/.test(n) || /paused/.test(n)) throw new Error(n);
});
t("with no engine, the row says to start Kiri", () => {
  kiri.on({ type: "routerports", error: "connection refused" });
  domains();
  if (document.getElementById("rp-http")) throw new Error("boxes shown");
  if (!/Start Kiri/.test(document.querySelector("#content .settings").textContent)) throw new Error("no hint");
});
report([...R, ...items.map((x) => "ITEM " + x)]);
