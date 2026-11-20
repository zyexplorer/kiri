// parity: windows linux
// window: 760x560
// A long error with no spaces (a pull_policy pattern) wraps inside its box:
// the plan's error, the project's warning and the Logs tab's error.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
const LONG = "services.web.pull_policy must match pattern '^(always|never|build|if_not_present|missing|refresh|daily|weekly|every_([0-9]+[wdhms])+)$'" + "_x".repeat(60);
const svc = { name: "web", image: "web", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] };
const list = (o) => kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1, services: [svc], containers: [], ...o }] });
kiri.on({ type: "lists", containers: [], images: [], volumes: [], networks: [] });
list({});
show("compose"); cmpOpen("shop");
kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text: "services:\n  web:\n    image: web\n", hash: "h", mtime: 1 });
const inside = (sel, what) => {
  const el = document.querySelector(sel);
  if (!el) throw new Error(what + ": no " + sel);
  const r = el.getBoundingClientRect(), c = $("content").getBoundingClientRect();
  eq([el.scrollWidth <= el.clientWidth + 1, r.right <= c.right + 1, $("content").scrollWidth <= $("content").clientWidth + 1], [true, true, true], `${what}: ${el.scrollWidth}/${el.clientWidth}, right ${Math.round(r.right)} in ${Math.round(c.right)}`);
};
t("the plan's error wraps in its box", () => {
  click(document.querySelector('[data-cmptab="file"]'));
  const ta = $("cmp-ta"); ta.value += "    pull_policy: sometimes\n"; ta.dispatchEvent(new Event("input"));
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, error: { line: 4, text: LONG } });
  inside("#cmp-plan .err", "plan");
});
t("the project's warning wraps", () => {
  list({ readable: false, error: LONG });
  inside("#cmp-warn .warn", "warning");
});
t("the Logs tab's error wraps", () => {
  list({});
  click(document.querySelector('[data-cmptab="logs"]'));
  kiri.on({ type: "composelogs", name: "shop", error: LONG });
  inside(".cmp-logs .warn", "logs");
});
report(R);
