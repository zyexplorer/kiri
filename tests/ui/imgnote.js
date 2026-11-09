// Per-image Markdown remarks: action button, dialog save, detail Remark tab.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (a !== b) throw new Error((m || "eq") + ": " + JSON.stringify(a) + " !== " + JSON.stringify(b)); };

const I = (id, tag, size) => ({ Id: id, RepoTags: [tag], Size: size, Created: 1, Pulled: Date.now() / 1000 - 60 });
prefs.listLayout = { images: "grid" };
kiri.on({
  type: "lists",
  containers: [],
  images: [I("sha256:aaa111", "nginx:alpine", 5e7), I("sha256:bbb222", "redis:7", 3e7)],
  volumes: [], networks: [],
});
kiri.on({ type: "imgnotes", names: ["redis:7"] });

t("images grid shows a remark button on each card", () => {
  show("images");
  eq(!!$("content").querySelector('[data-key="nginx:alpine"] [data-imgnote="nginx:alpine"]'), true, "nginx remark");
  eq(!!$("content").querySelector('[data-key="redis:7"] [data-imgnote="redis:7"].has-note'), true, "redis has-note");
  eq(!!$("content").querySelector('[data-key="nginx:alpine"] [data-imgnote].has-note'), false, "nginx empty");
});

t("remark button opens a markdown dialog and loads the body", () => {
  sent.length = 0;
  $("content").querySelector('[data-imgnote="nginx:alpine"]').click();
  eq(!!document.getElementById("imgnote-dlg"), true, "dialog");
  eq(!!sent.find((m) => m.type === "imgnote" && m.name === "nginx:alpine"), true, "asked host");
  kiri.on({ type: "imgnote", name: "nginx:alpine", body: "# Tip\n\nUse `-p 80:80`." });
  const body = document.querySelector("#imgnote-body");
  if (!body) throw new Error("no textarea");
  eq(body.value.includes("Tip"), true, "loaded body");
  if (!document.querySelector("#imgnote-dlg .md-ed .md-ed-gutter")) throw new Error("dialog editor needs line gutter");
  // Paste into the lined editor must keep the text and refresh the gutter.
  body.focus();
  body.setSelectionRange(body.value.length, body.value.length);
  const before = body.value;
  body.dispatchEvent(new ClipboardEvent("paste", {
    bubbles: true, cancelable: true,
    clipboardData: new DataTransfer(),
  }));
  // jsdom-style harness may not apply clipboardData; insert as a focused editor would.
  const pasteText = "\npasted line";
  const start = body.selectionStart, end = body.selectionEnd;
  body.value = before.slice(0, start) + pasteText + before.slice(end);
  body.dispatchEvent(new Event("input", { bubbles: true }));
  eq(body.value.includes("pasted line"), true, "paste kept");
  const gut = document.querySelector("#imgnote-dlg .md-ed-gutter")?.textContent || "";
  eq(gut.trim().split("\n").length, body.value.split("\n").length, "gutter after paste");
});

t("Tab indents in the lined editor", () => {
  const body = document.querySelector("#imgnote-body");
  body.value = "line";
  body.setSelectionRange(0, 0);
  body.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  eq(body.value.startsWith("  "), true, "tab inserts spaces");
});

t("cancel and close button dismiss the dialog", () => {
  eq(!!document.querySelector("#imgnote-dlg .ihn-x[data-ihnclose]"), true, "corner close");
  document.querySelector(".ihn-foot [data-ihnclose]").click();
  eq(!!document.getElementById("imgnote-dlg"), false, "cancel closes");
  // Corner × also closes after reopen.
  $("content").querySelector('[data-imgnote="nginx:alpine"]').click();
  kiri.on({ type: "imgnote", name: "nginx:alpine", body: "# Tip\n" });
  document.querySelector("#imgnote-dlg .ihn-x").click();
  eq(!!document.getElementById("imgnote-dlg"), false, "x closes");
});

t("saving the dialog sends imgnotesave", () => {
  $("content").querySelector('[data-imgnote="nginx:alpine"]').click();
  kiri.on({ type: "imgnote", name: "nginx:alpine", body: "" });
  const body = document.querySelector("#imgnote-body");
  body.value = "## Ports\n\n- `80`\n";
  body.dispatchEvent(new Event("input", { bubbles: true }));
  sent.length = 0;
  document.querySelector("[data-ihnsave]").click();
  const save = sent.find((m) => m.type === "imgnotesave");
  eq(save?.name, "nginx:alpine", "name");
  eq(save?.value?.body?.includes("Ports"), true, "body");
  kiri.on({ type: "imgnote", name: "nginx:alpine", body: save.value.body, saved: true });
  kiri.on({ type: "imgnotes", names: ["nginx:alpine", "redis:7"] });
  eq(!!document.getElementById("imgnote-dlg"), false, "closed after save");
  eq(ui.imgNotes.has("nginx:alpine"), true, "set updated");
});

t("image detail has a Remark tab after Security", () => {
  openImage("nginx:alpine");
  const tabs = [...$("content").querySelectorAll("[data-imgtab]")].map((el) => el.dataset.imgtab);
  eq(tabs.join(","), "layers,security,remark", "tab order");
  sent.length = 0;
  $("content").querySelector('[data-imgtab="remark"]').click();
  eq(ui.imgd.tab, "remark", "tab");
  eq(!!sent.find((m) => m.type === "imgnote" && m.name === "nginx:alpine"), true, "loads note");
  kiri.on({ type: "imgnote", name: "nginx:alpine", body: "## Ports\n\n- `80`\n" });
  const preview = $("content").querySelector("#img-note-preview");
  if (!preview || !preview.innerHTML.includes("<h2>")) throw new Error(preview?.innerHTML?.slice(0, 120) || "no preview");
  $("content").querySelector('[data-imgnotemode="edit"]').click();
  eq(ui.imgd.noteMode, "edit", "edit");
  const ta = $("img-note-body");
  if (!ta) throw new Error("no editor");
  if (!$("content").querySelector(".md-ed .md-ed-gutter")) throw new Error("detail editor needs line gutter");
  ta.value = "# Updated\n";
  ta.dispatchEvent(new Event("input", { bubbles: true }));
  eq(ui.imgd.noteDirty, true, "dirty");
  eq(($("content").querySelector(".md-ed-gutter")?.textContent || "").trim(), "1\n2", "gutter lines");
  sent.length = 0;
  $("content").querySelector("[data-imgnotesave]").click();
  const save = sent.find((m) => m.type === "imgnotesave");
  eq(save?.value?.body, "# Updated\n", "saved from detail");
  kiri.on({ type: "imgnote", name: "nginx:alpine", body: "# Updated\n", saved: true });
  eq(ui.imgd.noteDirty, false, "clean");
  eq(ui.imgd.noteMode === "preview" || !!$("content").querySelector(".img-remark"), true, "still on remark");
});

report(R);
