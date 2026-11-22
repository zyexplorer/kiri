// lang: bg
// The plan box's "can't show the plan" sentence names Apply with the button's own word, in each language.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
ui.vm = "running";
const services = [{ name: "app", image: "alpine:3.20", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
const WARN = { text: "x", errkey: "hiddenCheck", errargs: { name: "${DEPLOY_TOKEN}" } };
kiri.on({ type: "lists", containers: [{ Id: "both-app", Names: ["/both-app-1"], Image: "alpine:3.20", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": "both", "com.docker.compose.service": "app" } }], images: [], volumes: [], networks: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "both", dir: "/w/both", files: ["/w/both/compose.yaml"], readable: true, updated: 1, services, warn: WARN, containers: [{ service: "app", id: "both-app", name: "both-app-1", image: "alpine:3.20", state: "running", status: "Up" }] }] });
show("compose");
t("the sentence says the Apply button's own word", () => {
  cmpOpen("both");
  kiri.on({ type: "composefile", name: "both", path: "/w/both/compose.yaml", mtime: 1, hash: "h1", text: "services:\n  app:\n    image: alpine:3.20\n" });
  document.querySelector('[data-cmptab="file"]').dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  cmp.draft = "services:\n  app:\n    image: alpine:3.21\n";
  kiri.on({ type: "composeplan", name: "both", seq: cmp.seq, steps: [], services, warn: WARN });
  const lede = $("cmp-plan").querySelector(".lede").textContent, word = tr("Apply");
  eq(word, "Прилагане", "the button's word");
  eq(lede.includes(word), true, "the lede: " + lede);
  eq(document.querySelector("[data-cmpapply]").textContent.trim(), word, "the button");
});
report(R);
