// parity: macos windows linux
// lang: de
// "Can't find this project's compose file" in the window's language wherever it
// shows (the plan, Discard's toast), said once on the file tab, and gone as soon
// as a list or a read finds the file; Compose's errors keep their detail lines;
// the "Saved, not applied" note goes once the file on disk is what runs again.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
const lastToast = () => [...document.querySelectorAll(".toast")].pop()?.textContent || "";
ui.vm = "running";
const NF = "Kiri can't find this project's compose file", DE = "Kiri findet die Compose-Datei dieses Projekts nicht";
kiri.on({ type: "lists", containers: [{ Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": "shop", "com.docker.compose.service": "db" } }], images: [], volumes: [], networks: [] });
const services = [{ name: "db", image: "postgres:16", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
const list = (o = {}) => kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1, services, containers: [{ service: "db", id: "d1", name: "shop-db-1", image: "postgres:16", state: "running", status: "Up" }], ...o }] });
const gone = { readable: false, error: NF, errkey: "noFile", errargs: {} };
const F = (o) => kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", mtime: 1, ...o });
const TEXT = "services:\n  db:\n    image: postgres:16\n";
const edit = (from, to) => { const ta = $("cmp-ta"); ta.value = ta.value.replace(from, to); ta.dispatchEvent(new Event("input")); };
const count = () => $("content").textContent.split(DE).length - 1;
list();
show("compose");
t("the file gone: the file tab says so once, in German", () => {
  list(gone);
  cmpOpen("shop");
  F({ error: NF, errkey: "noFile", errargs: {} });
  click(document.querySelector('[data-cmptab="file"]'));
  eq([count(), $("content").textContent.includes(NF)], [1, false], "said once, not in English");
});
t("Discard while the file is gone: the toast is German, and the line still shows once", () => {
  kiri.on({ type: "composerestored", name: "shop", error: NF, errkey: "noFile", errargs: {} });
  F({ error: NF, errkey: "noFile", errargs: {} });
  eq([lastToast().includes(DE), lastToast().includes(NF), count()], [true, false, 1], lastToast());
});
t("the file back: the warning goes with the read that found it, before the list", () => {
  click(document.querySelector('[data-cmptab="structure"]'));
  eq(count(), 1, "the warning on Structure");
  F({ text: TEXT, hash: "h1" });
  eq(count(), 0, "gone once the file is read");
  list();
  eq(count(), 0, "and with the list");
});
t("the plan's can't-find is German", () => {
  click(document.querySelector('[data-cmptab="file"]'));
  edit("postgres:16", "postgres:17");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, error: { line: null, text: NF, errkey: "noFile", errargs: {} } });
  const err = document.querySelector(".plan .err")?.textContent || "";
  eq([err.includes(DE), err.includes(NF)], [true, false], err);
});
t("a Compose error with detail lines shows them, each on its own line", () => {
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, error: { line: null, text: "decoding failed due to the following error(s):\n'services[db].mem_limit' invalid size: 'nonsense'" } });
  const err = document.querySelector(".plan .err");
  eq([getComputedStyle(err).whiteSpace, err.innerText.split("\n").length >= 2, err.textContent.includes("mem_limit")], ["pre-line", true, true], err.innerText);
  list({ readable: false, error: "decoding failed due to the following error(s):\n'services[db].mem_limit' invalid size: 'nonsense'" });
  const w = $("cmp-warn");
  eq([getComputedStyle(w).whiteSpace, w.innerText.includes("\n'services[db].mem_limit'")], ["pre-line", true], w.innerText);
  list();
});
t("Saved, not applied, then the file put back as what runs: the note goes", () => {
  cmp.open = null; cmpOpen("shop"); F({ text: TEXT, hash: "h1" }); click(document.querySelector('[data-cmptab="file"]'));
  edit("postgres:16", "postgres:18");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:18" }], services });
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, saved: true, hash: "h2", error: "docker compose up failed", errkey: "upFailed", errargs: {} });
  F({ text: TEXT.replace("16", "18"), hash: "h2", restore: true });
  eq(!!cmpUnapplied(), true, "the note, after the failed Apply");
  // Put back by hand: the engine has nothing to put back any more.
  F({ text: TEXT, hash: "h1", restore: false, back: true });
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "same", what: [] }], services });
  eq([!!cmpUnapplied(), $("cmp-edstate").textContent.includes("nicht angewendet"), !!document.querySelector("[data-cmprestore]")], [false, false, false], $("cmp-edstate").textContent);
});
t("A failed Apply with nothing to save keeps its reason through later reads", () => {
  cmp.open = null; cmp.unapplied = null; cmpOpen("shop"); F({ text: TEXT, hash: "h1" }); click(document.querySelector('[data-cmptab="file"]'));
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "same", what: [] }], services });
  cmp.applying = { name: "shop", text: TEXT, lines: [], svc: Object.create(null) };
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, saved: true, hash: "h1", error: "Bind for 0.0.0.0:5432 failed" });
  F({ text: TEXT, hash: "h1", restore: false, back: false });
  F({ text: TEXT, hash: "h1", restore: false, back: false });
  eq([cmpUnapplied()?.error, $("cmp-edstate").textContent.includes("Bind for 0.0.0.0:5432 failed")], ["Bind for 0.0.0.0:5432 failed", true], $("cmp-edstate").textContent);
});
report(R);
