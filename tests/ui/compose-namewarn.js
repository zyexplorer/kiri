// parity: macos windows linux
// A secret used in a container name or image: the project says so on its row and its page, alone and
// together with the can't-check warning; the names from Docker are shown as Docker has them, the same
// on the row, its tooltip, its link and Containers' Remove confirm.
const ONE = "${DEPLOY_TOKEN} is used in a name or folder path. Docker shows those in plain text.";
const TWO = "${DEPLOY_TOKEN} and ${TAG_SECRET} are used in a name or folder path. Docker shows those in plain text.";
const HCHECK = "Kiri can't check this file with ${DEPLOY_TOKEN} hidden. Compose accepts it, so Apply still works.";
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
const NAME = "x-Dpl0y-Tok-77aa99";
const ctr = (p, n) => ({ Id: p + "-app", Names: [`/${n}`], Image: "alpine:3.20", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": p, "com.docker.compose.service": "app" } });
kiri.on({ type: "lists", containers: [ctr("one", NAME), ctr("two", "two-app-1"), ctr("both", "both-app-1"), ctr("plain", "plain-app-1")], images: [], volumes: [], networks: [] });
const services = [{ name: "app", image: "alpine:3.20", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
const proj = (name, cname, o = {}) => ({ name, dir: "/w/" + name, files: [`/w/${name}/compose.yaml`], readable: true, updated: 1, services, containers: [{ service: "app", id: name + "-app", name: cname, image: "alpine:3.20", state: "running", status: "Up" }], ...o });
const nw = (names) => ({ text: "x", errkey: "nameSecret", errargs: { name: names.join(" and "), names } });
const WARN = { text: "x", errkey: "hiddenCheck", errargs: { name: "${DEPLOY_TOKEN}" } };
kiri.on({ type: "composeprojects", logos: {}, projects: [proj("one", NAME, { namewarn: nw(["${DEPLOY_TOKEN}"]) }), proj("two", "two-app-1", { namewarn: nw(["${DEPLOY_TOKEN}", "${TAG_SECRET}"]) }), proj("both", "both-app-1", { warn: WARN, namewarn: nw(["${DEPLOY_TOKEN}"]) }), proj("plain", "plain-app-1")] });
show("compose");
// A project's notices on the list: rows of their own under its row, each its whole text in its title (its first sentence, then the rest).
const rowWarns = (n) => [...document.querySelectorAll(`#content tr.warnrow[data-cmpnote="${n}"] .name > div[title]`)].map((w) => [w.querySelector(".wtitle").textContent + " " + (w.querySelector(".sub-name")?.textContent || ""), w.title].map((x) => x.trim()));
t("the list: one variable, two, with the other warning, and none", () => {
  eq(rowWarns("one"), [[ONE, ONE]], "one");
  eq(rowWarns("two"), [[TWO, TWO]], "two");
  eq(rowWarns("both"), [[HCHECK, HCHECK], [ONE, ONE]], "both");
  eq(rowWarns("plain"), [], "plain");
});
t("the page: the warning under the tabs, with the other one too", () => {
  for (const [n, want] of [["one", [ONE]], ["both", [HCHECK, ONE]], ["plain", []]]) {
    cmpOpen(n);
    eq([...$("cmp-warn").querySelectorAll("p.warn")].map((p) => p.textContent), want, n);
  }
  eq($("content").textContent.includes("NaN"), false, "no NaN");
});
t("the plan box says it for a draft, and says plainly when it has no plan", () => {
  cmpOpen("both");
  kiri.on({ type: "composefile", name: "both", path: "/w/both/compose.yaml", mtime: 1, hash: "h1", text: "services:\n  app:\n    image: alpine:3.20\n" });
  click(document.querySelector('[data-cmptab="file"]'));
  cmp.draft = "services:\n  app:\n    image: alpine:3.21\n";
  kiri.on({ type: "composeplan", name: "both", seq: cmp.seq, steps: [], services, warn: WARN, namewarn: nw(["${DEPLOY_TOKEN}"]) });
  const el = $("cmp-plan");
  eq(el.querySelector(".lede").textContent, "Kiri can't show the plan with ${DEPLOY_TOKEN} hidden. Apply will run docker compose up.", "the lede");
  eq(el.textContent.includes("0 unchanged"), false, "no 0 unchanged");
  eq([...el.querySelectorAll(".err.warn")].map((e) => e.textContent), [HCHECK, ONE], "both warnings");
});
t("the row, its tooltip, its link and the Remove confirm name the container as Docker does", () => {
  kiri.on({ type: "composeprojects", logos: {}, projects: [proj("one", NAME, { namewarn: nw(["${DEPLOY_TOKEN}"]) })] });
  cmp.open = null; render();
  const name = document.querySelector('#content tr.member[data-key="one-app"] .name > div[title]');
  eq(name.querySelector(".link").textContent, NAME, "the row: " + name.textContent);
  eq(name.title, NAME, "the tooltip: " + name.title);
  cmpOpen("one");
  click(document.querySelector('[data-cmptab="services"]'));
  const links = [...document.querySelectorAll(`#content [data-detail="one-app"] .link`)].map((l) => l.textContent).filter((x) => x !== "app");
  eq(links.length > 0 && links.every((x) => x === NAME), true, "the links: " + links);
  eq($("content").innerHTML.includes("x-${DEPLOY_TOKEN}"), false, "never a variable in the name");
  // Compose has no Remove: Containers' asks.
  eq(document.querySelector('#content [data-c="remove"]'), null, "no Remove here");
  containerAction("remove", "one-app");
  const dlg = document.querySelector(".dialog.alert h3");
  eq(dlg?.textContent, `Remove ${NAME}?`, "the confirm");
  const labelled = [...document.querySelectorAll("#content [aria-label]")].map((b) => b.getAttribute("aria-label")).filter((l) => l.includes("Dpl0y"));
  eq(labelled.every((l) => l.includes(NAME)), true, "aria-labels: " + labelled);
  document.querySelector(".scrim")?.remove();
});
report(R);
