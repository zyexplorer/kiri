// parity: macos windows linux
// Compose shows Kiri's spinner while the engine takes its time: the first list in the page (never an empty
// table or the empty state before it), a later slow list by the title (after 600 ms, gone with the answer, with
// nothing moving; a fast one never shows it), the file and a slow plan in the plan box, and Logs. A row's
// action button rolls till its answer.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const later = (ms) => new Promise((r) => setTimeout(r, ms));
const spin = (root) => root?.querySelector('[role="status"] .sh-spin');
ui.vm = "running";
const ctr = { Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": "shop", "com.docker.compose.service": "db" } };
kiri.on({ type: "lists", containers: [ctr], images: [], volumes: [], networks: [] });
const services = [{ name: "db", image: "postgres:16", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
const list = (o = {}) => kiri.on({ type: "composeprojects", logos: {}, ...o, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1, services, containers: [{ service: "db", id: "d1", name: "shop-db-1", image: "postgres:16", state: "running", status: "Up" }] }] });
const hw = () => $("cmp-hwait");
(async () => {
  show("compose");
  t("first load: the spinner where the table will be, no empty state", () => {
    eq([!!spin($("content")), !!document.querySelector("#content .empty h3"), !!document.querySelector("#content table"), $("content").textContent.includes(tr("Loading…"))], [true, false, false, true], $("content").innerHTML);
    eq(spin($("content")).parentElement.getAttribute("aria-label"), tr("Loading…"), "its label");
  });
  list();
  t("the list: no spinner left", () => eq([!!spin($("content")), hw().hidden], [false, true], "spinners"));
  const top = () => document.querySelector("#content table").getBoundingClientRect().top;
  const before = top(), titleBox = $("title").getBoundingClientRect();
  // A slow refresh.
  cmp.asked = 0; cmpAsk();
  await later(300);
  t("a refresh under 600 ms shows no spinner", () => eq(hw().hidden, true, "hidden at 300 ms"));
  await later(450);
  t("a slow refresh shows the header spinner, nothing moves", () => {
    eq([hw().hidden, !!spin(hw().parentElement)], [false, true], "shown at 750 ms");
    if (Math.abs(top() - before) > 1) throw new Error(`the table moved ${before} → ${top()}`);
    const tb = $("title").getBoundingClientRect(); if (Math.abs(tb.left - titleBox.left) > 1 || Math.abs(tb.width - titleBox.width) > 1) throw new Error("the title moved");
  });
  list({ ask: cmp.askN });
  t("the answer takes it away, nothing moves", () => { eq(hw().hidden, true, "hidden"); if (Math.abs(top() - before) > 1) throw new Error("moved"); });
  // A fast one.
  cmp.asked = 0; cmpAsk(); await later(100); list({ ask: cmp.askN }); await later(700);
  t("a fast refresh never shows it", () => eq(hw().hidden, true, "hidden"));
  // A row's button rolls till its answer.
  sent.length = 0;
  click(document.querySelector('tr.group [data-g="restart"]'));
  t("Restart all rolls till its answer", () => {
    eq(sent.filter((m) => m.type === "composeaction").map((m) => m.value.action), ["restart"], JSON.stringify(sent));
    eq(document.querySelector('tr.group [data-g="restart"]').classList.contains("rolling"), true, "rolling");
    kiri.on({ type: "composeaction", name: "shop", done: true });
    eq(document.querySelector('tr.group [data-g="restart"]').classList.contains("rolling"), false, "done");
  });
  t("a service's Stop rolls till the list shows it stopped", () => {
    click(document.querySelector('tr.member [data-c="stop"]'));
    eq(document.querySelector('tr.member [data-c="stop"]').classList.contains("rolling"), true, "rolling");
    kiri.on({ type: "lists", containers: [{ ...ctr, State: "exited", Status: "Exited (0)" }], images: [], volumes: [], networks: [] });
    eq(document.querySelector('tr.member [data-c="stop"].rolling'), null, "done");
    kiri.on({ type: "lists", containers: [ctr], images: [], volumes: [], networks: [] });
  });
  // The page: the file, then a slow plan, then Logs.
  cmpOpen("shop");
  click(document.querySelector('[data-cmptab="file"]'));
  t("the file tab waits on the file with the spinner", () => eq(!!spin($("cmp-tab")), true, $("cmp-tab").innerHTML));
  kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", mtime: 1, text: "services:\n  db:\n    image: postgres:16\n", hash: "h1" });
  const ta = $("cmp-ta"); ta.value = ta.value.replace("16", "17"); ta.dispatchEvent(new Event("input"));
  await later(350 + 250);
  t("a plan under 600 ms: no spinner", () => eq(!!spin($("cmp-plan")), false, $("cmp-plan").innerHTML));
  await later(700);
  t("a slow plan: the spinner in the plan box", () => eq(!!spin($("cmp-plan")), true, $("cmp-plan").innerHTML));
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:17" }], services });
  t("the plan's answer takes it away", () => eq(!!spin($("cmp-plan")), false, $("cmp-plan").innerHTML));
  click(document.querySelector('[data-cmptab="logs"]'));
  t("Logs wait with the spinner", () => eq(!!spin($("cmp-tab")), true, $("cmp-tab").innerHTML));
  kiri.on({ type: "composelogs", name: "shop", lines: [] }); render();
  t("the logs' answer takes it away", () => eq(!!spin($("cmp-tab")), false, $("cmp-tab").innerHTML));
  t("reduced motion: the spinner slows, as Kiri's other spinners do", () => {
    const rules = [...document.styleSheets].flatMap((s) => [...s.cssRules]).filter((r) => r.media?.mediaText?.includes("reduce")).map((r) => r.cssText).join(" ");
    eq(/\.sh-spin\s*\{[^}]*animation-duration/.test(rules), true, "a reduced-motion rule for .sh-spin");
  });
  report(R);
})();
