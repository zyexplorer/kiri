// parity: macos windows linux
// lang: de
// compose-namewarn.js in German: the warnings in German, alone and with the can't-check one.
const ONE = "${DEPLOY_TOKEN} wird in einem Namen oder Ordnerpfad verwendet. Docker zeigt diese im Klartext.";
const TWO = "${DEPLOY_TOKEN} und ${TAG_SECRET} werden in einem Namen oder Ordnerpfad verwendet. Docker zeigt diese im Klartext.";
const HCHECK = "Kiri kann diese Datei mit verborgenem ${DEPLOY_TOKEN} nicht prüfen. Compose akzeptiert sie, daher funktioniert „Anwenden“ trotzdem.";
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
ui.vm = "running";
const ctr = (p) => ({ Id: p + "-app", Names: [`/${p}-app-1`], Image: "alpine:3.20", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": p, "com.docker.compose.service": "app" } });
kiri.on({ type: "lists", containers: [ctr("one"), ctr("two"), ctr("both")], images: [], volumes: [], networks: [] });
const services = [{ name: "app", image: "alpine:3.20", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
const proj = (name, o = {}) => ({ name, dir: "/w/" + name, files: [`/w/${name}/compose.yaml`], readable: true, updated: 1, services, containers: [{ service: "app", id: name + "-app", name: `${name}-app-1`, image: "alpine:3.20", state: "running", status: "Up" }], ...o });
const nw = (names) => ({ text: "x", errkey: "nameSecret", errargs: { name: names.join(" and "), names } });
const WARN = { text: "x", errkey: "hiddenCheck", errargs: { name: "${DEPLOY_TOKEN}" } };
kiri.on({ type: "composeprojects", logos: {}, projects: [proj("one", { namewarn: nw(["${DEPLOY_TOKEN}"]) }), proj("two", { namewarn: nw(["${DEPLOY_TOKEN}", "${TAG_SECRET}"]) }), proj("both", { warn: WARN, namewarn: nw(["${DEPLOY_TOKEN}"]) })] });
show("compose");
// A project's notices on the list: rows of their own under its row, each its whole text in its title.
const rowWarns = (n) => [...document.querySelectorAll(`#content tr.warnrow[data-cmpnote="${n}"] .name > div[title]`)].map((w) => w.title);
t("the list in German: one, two, and with the other warning", () => {
  eq(rowWarns("one"), [ONE], "one");
  eq(rowWarns("two"), [TWO], "two");
  eq(rowWarns("both"), [HCHECK, ONE], "both");
});
t("the page in German", () => {
  cmpOpen("both");
  eq([...$("cmp-warn").querySelectorAll("p.warn")].map((p) => p.textContent), [HCHECK, ONE], "both");
});
report(R);
