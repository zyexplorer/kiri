// parity: macos windows linux
// The Compose file tab keeps the person's edits and says what's true: Reload
// before the outside file has come waits for it, a file that goes away while
// being edited keeps the draft (and, back again, offers Reload or keep the
// draft), and a file broken as saved marks its line and never says it matches
// what runs.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
kiri.on({ type: "lists", containers: [{ Id: "d1", Names: ["/shop-db-1"], Image: "postgres:16", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "com.docker.compose.project": "shop", "com.docker.compose.service": "db" } }], images: [], volumes: [], networks: [] });
const services = [{ name: "db", image: "postgres:16", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }];
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1, services, containers: [{ service: "db", id: "d1", name: "shop-db-1", image: "postgres:16", state: "running", status: "Up" }] }] });
show("compose");
const F = (text, hash, o = {}) => kiri.on({ type: "composefile", name: "shop", path: "/w/shop/compose.yaml", text, hash, mtime: 1, ...o });
const edit = (from, to) => { const ta = $("cmp-ta"); ta.value = ta.value.replace(from, to); ta.dispatchEvent(new Event("input")); };
const open = (text, hash) => { cmpOpen("shop"); F(text, hash); click(document.querySelector('[data-cmptab="file"]')); };
const errs = [];
window.addEventListener("error", (e) => errs.push(e.message));
t("Reload clicked after Apply said the file changed, before the file itself came: waits for it, then shows it", () => {
  open("services:\n  db:\n    image: postgres:16\n", "h1");
  edit("postgres:16", "postgres:17");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, steps: [{ n: "db", act: "chg", what: ["image"], from: "postgres:16", to: "postgres:17" }], services });
  click(document.querySelector("[data-cmpapply]"));
  kiri.on({ type: "composeapply", name: "shop", done: true, ok: false, conflict: true, hash: "h2", error: "The file changed outside Kiri" });
  sent.length = 0;
  let threw = null;
  try { click(document.querySelector("#cmp-conflict [data-cmpreload]")); } catch (e) { threw = e.message; }
  eq([threw, errs, document.querySelector("#content").textContent.includes("undefined")], [null, [], false], "no error, no undefined");
  eq(sent.some((m) => m.type === "composefile"), true, "asks for the file");
  F("services:\n  db:\n    image: mysql:8\n", "h2");
  eq([$("cmp-ta")?.value.includes("mysql:8"), cmpDirty(), cmp.file.hash], [true, false, "h2"], "the outside version, once it's here");
});
t("the file goes while being edited: the draft stays, with a note, and Apply waits", () => {
  open("services:\n  db:\n    image: postgres:16\n", "h1");
  edit("postgres:16", "postgres:18");
  F(undefined, undefined, { error: "Kiri can't find this project's compose file" });
  eq([$("cmp-ta")?.value.includes("postgres:18"), cmpDirty()], [true, true], "the edits are still there");
  const note = document.querySelector("#cmp-conflict .cmp-conflict");
  eq([!!note, note?.textContent.includes("gone")], [true, true], "says the file is gone");
  eq(document.querySelector("[data-cmpapply]").disabled, true, "no Apply meanwhile");
});
t("the file comes back different: Reload or keep the draft", () => {
  F("services:\n  db:\n    image: mysql:8\n", "h5");
  const box = document.querySelector("#cmp-conflict .cmp-conflict");
  eq([!!box, box?.textContent.includes("back"), $("cmp-ta").value.includes("postgres:18")], [true, true, true], "asks, edits kept");
  const r = box.querySelector("[data-cmpreload]"), k = box.querySelector("[data-cmpkeep]");
  eq([getComputedStyle(r).cursor, getComputedStyle(k).cursor], ["pointer", "pointer"], "both clickable");
  click(k);
  eq([!!document.querySelector("#cmp-conflict .cmp-conflict"), $("cmp-ta").value.includes("postgres:18")], [false, true], "kept");
});
t("the file comes back as the draft: nothing to ask", () => {
  open("services:\n  db:\n    image: postgres:16\n", "h1");
  edit("postgres:16", "postgres:19");
  F(undefined, undefined, { error: "Kiri can't find this project's compose file" });
  F($("cmp-ta").value, "h6");
  eq([!!document.querySelector("#cmp-conflict .cmp-conflict"), cmpDirty(), cmp.file.hash], [false, false, "h6"], "saved as it is");
});
t("a file broken as saved: its line is marked, and it never says it matches what runs", () => {
  open("services:\n  db:\n    image: postgres:16\n    ports: [\"abc:80\"]\n", "h7");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, error: { line: 4, text: "invalid hostPort: abc" } });
  eq([$("cmp-edstate").textContent.includes("matches what runs"), $("cmp-edstate").textContent], [false, "Saved, but line 4 has an error"], "the editor's state");
  eq([document.querySelector("#cmp-gut .bad")?.textContent, !!document.querySelector("#cmp-pre .chl.bad")], ["4", true], "line 4 is marked");
  eq(document.querySelector("#cmp-plan .err")?.textContent.includes("Line 4"), true, "the plan says where");
});
t("an error in an included file: no line of this one is marked, and it says which file", () => {
  open("include:\n  - ./sub/compose.yaml\nservices:\n  db:\n    image: postgres:16\n    ports: [\"80:80\"]\n    environment:\n      A: b\n    restart: always\n", "h8");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, error: { line: null, text: "Compose rejects this file: YAML error in an included file, line 9", errkey: "yamlInc", errargs: { n: "9" } } });
  eq([$("cmp-edstate").textContent, !!document.querySelector("#cmp-gut .bad"), !!document.querySelector("#cmp-pre .chl.bad")], ["Saved, but an included file has an error", false, false], "saved note, no line marked");
  edit("postgres:16", "postgres:17");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, error: { line: null, text: "Compose rejects this file: YAML error in sub/compose.yaml, line 9", errkey: "yamlIn", errargs: { n: "9", file: "sub/compose.yaml" } } });
  const err = document.querySelector("#cmp-plan .err")?.textContent || "";
  eq([err, !!document.querySelector("#cmp-gut .bad"), /Line 9/.test(err)], ["Compose rejects this file: YAML error in sub/compose.yaml, line 9", false, false], "the plan box");
  kiri.on({ type: "composeplan", name: "shop", seq: cmp.seq, error: { line: null, text: "x", errkey: "yamlInc", errargs: { n: "9" } } });
  eq(document.querySelector("#cmp-plan .err")?.textContent, "Compose rejects this file: YAML error in an included file, line 9", "names only: no path");
});
report(R);
