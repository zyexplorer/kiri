// Services may be named any word, even one an object already has
// (constructor, toString, valueOf, __proto__): Structure, the plan and Apply's
// progress draw them like any other.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
const names = ["constructor", "toString", "valueOf", "__proto__", "hasOwnProperty"];
const svc = (name, depends = []) => ({ name, image: "x", build: "", ports: [], volumes: [], depends, networks: ["default"], kinds: [] });
const services = [svc("constructor", ["toString"]), svc("toString", ["valueOf"]), svc("valueOf"), svc("__proto__", ["constructor"]), svc("hasOwnProperty")];
kiri.on({ type: "lists", containers: [], images: [], volumes: [], networks: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "odd", dir: "/w/odd", files: ["/w/odd/compose.yaml"], readable: true, fresh: true, updated: null, services, containers: [] }] });
show("compose");
t("Structure draws a card for each, with its links", () => {
  cmpOpen("odd");
  eq([...document.querySelectorAll("#content .card")].map((c) => c.dataset.cmpcard).sort(), [...names].sort(), "five cards");
});
t("the plan and Apply's progress name them", () => {
  kiri.on({ type: "composefile", name: "odd", path: "/w/odd/compose.yaml", text: "services:\n" + names.map((n) => `  ${n}:\n    image: x\n`).join(""), hash: "h" });
  click(document.querySelector('[data-cmptab="file"]'));
  kiri.on({ type: "composeplan", name: "odd", seq: cmp.seq, steps: names.map((n) => ({ n, act: n === "valueOf" ? "chg" : "add", what: [], from: "", to: "x" })), services });
  eq(document.querySelectorAll("#cmp-plan li").length, 5, "five steps");
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "odd", svc: ["constructor", "toString"], state: "started" });
  click(document.querySelector('[data-cmptab="structure"]'));
  eq(document.querySelector('.card[data-cmpcard="constructor"] .tag')?.textContent, "Started", "its card says where it is");
  eq(document.querySelector('.card[data-cmpcard="valueOf"] .tag')?.textContent, "CHANGES", "an untouched one keeps its plan's tag");
});
report(R);
