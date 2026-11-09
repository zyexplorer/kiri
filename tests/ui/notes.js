// Notes view: sidebar tree, editor chrome, CRUD commands to the host.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (a !== b) throw new Error((m || "") + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };

t("Notes is in the Kiri nav", () => {
  const b = document.querySelector('nav [data-view="notes"]');
  if (!b) throw new Error("no notes button");
  eq(b.querySelector(".lbl")?.textContent, "Notes", "label");
});

t("opening Notes asks the host for the list", () => {
  sent.length = 0;
  show("notes");
  eq($("title").textContent, "Notes", "title");
  if (!sent.some((m) => m.type === "notes")) throw new Error(JSON.stringify(sent));
  if (!$("content").querySelector(".notes")) throw new Error("no notes layout");
});

t("empty list shows root tree without duplicate Notes chrome", () => {
  kiri.on({ type: "notes", items: [], tree: [] });
  if (!$("content").querySelector(".notes-side")) throw new Error("side tree always shown");
  const root = $("content").querySelector(".notes-row.root .t");
  if (!root || root.textContent !== "Notes") throw new Error("missing Notes root");
  if ($("content").querySelector(".notes-side-h .lbl")) throw new Error("side header must not repeat Notes");
  if ($("content").querySelector(".notes-dock")) throw new Error("notes dock should be gone");
  if ($("content").querySelector("[data-notesai]")) throw new Error("AI must not live in notes panel");
  eq($("subtitle").textContent, "", "no redundant subtitle count");
  const text = $("content").textContent || "";
  if (!text.includes("No notes yet")) throw new Error(text.slice(0, 160));
  if (!$("content").querySelector(".notes-side-h [data-notenew]")) throw new Error("side header new note");
  if (!$("hright").querySelector("[data-notenew]")) throw new Error("toolbar new when empty");
});

t("AI sits in the statusbar left of the version", () => {
  renderStatus();
  const bar = $("statusbar");
  const ai = bar.querySelector("[data-notesai]");
  if (!ai) throw new Error("no statusbar AI");
  const ver = [...bar.querySelectorAll(".item")].find((el) => /Kiri/.test(el.textContent || ""));
  if (!ver) throw new Error("no version item");
  eq(ai.nextElementSibling, ver, "AI immediately left of version");
});

t("tree auto-opens the newest note under the root", () => {
  sent.length = 0;
  kiri.on({
    type: "notes",
    items: [
      { id: "n1", title: "Compose tip", updated: Math.floor(Date.now() / 1000) - 120 },
      { id: "ops/n2", title: "docker run nginx", updated: Math.floor(Date.now() / 1000) - 3600 },
    ],
    tree: [
      { kind: "folder", id: "ops", name: "ops", children: [
        { kind: "note", id: "ops/n2", name: "docker run nginx", title: "docker run nginx", updated: 1 },
      ]},
      { kind: "note", id: "n1", name: "Compose tip", title: "Compose tip", updated: 2 },
    ],
  });
  if (!$("content").querySelector(".notes-row.root")) throw new Error("root row");
  if (!$("content").querySelector('[data-notefolder="ops"]')) throw new Error("folder row");
  // Expand the folder so nested notes are visible.
  $("content").querySelector('[data-notetwist="ops"]').click();
  eq($("content").querySelectorAll("[data-noteid]").length, 2, "note rows");
  if ($("hright").querySelector("[data-notenew]")) throw new Error("no toolbar new when notes exist");
  if (!$("content").querySelector(".notes-side-h [data-notenew]")) throw new Error("side new note");
  if (!$("content").querySelector('.notes-side-h [data-notemkdir]')) throw new Error("side new subfolder");
  if (!sent.some((m) => m.type === "noteread" && m.id === "n1")) throw new Error(JSON.stringify(sent));
  kiri.on({ type: "note", id: "n1", title: "Compose tip", body: "# Tip\n\n```yaml\nservices:\n  web:\n    image: nginx\n```\n", updated: 1 });
  eq(ui.notes.mode, "preview", "opens in preview");
  eq($("note-title")?.value, "Compose tip", "title field");
  const preview0 = $("note-preview")?.innerHTML || "";
  if (!preview0.includes("<h1>") || !preview0.includes("nginx")) throw new Error(preview0.slice(0, 200));
  // Copy stays outside the scrolling <pre>, so horizontal scroll doesn't drag it.
  const box = $("note-preview")?.querySelector(".md-code");
  if (!box || box.tagName !== "DIV") throw new Error("md-code should be a wrapper div");
  if (!box.querySelector(":scope > .md-copy") || !box.querySelector(":scope > pre > code")) throw new Error(box.innerHTML.slice(0, 200));
  eq($("content").querySelector('[data-noteid="n1"]')?.getAttribute("aria-current"), "true", "selected");
});
t("code-block Copy reads text from the wrapper and stays pinned in CSS", () => {
  kiri.on({
    type: "note", id: "n1", title: "long", updated: 1,
    body: "```bash\ndocker run -d --name nginxwebui " + "x".repeat(200) + "\n```\n",
  });
  const btn = $("note-preview")?.querySelector("[data-mdcopy]");
  const pre = $("note-preview")?.querySelector(".md-code > pre");
  if (!btn || !pre) throw new Error("missing copy/pre");
  const cs = getComputedStyle(btn);
  if (cs.position !== "absolute") throw new Error("copy should be absolute on wrapper");
  sent.length = 0;
  btn.click();
  const m = sent.find((x) => x.type === "copy");
  if (!m || !String(m.name || "").includes("nginxwebui")) throw new Error(JSON.stringify(sent));
});

t("edit, save, preview, delete", () => {
  $("content").querySelector('[data-notemode="edit"]').click();
  eq(ui.notes.mode, "edit", "edit mode");
  const body = $("note-body");
  if (!body) throw new Error("no textarea");
  body.value = "# Tip\n\nhello";
  body.dispatchEvent(new Event("input", { bubbles: true }));
  eq(ui.notes.dirty, true, "dirty");
  sent.length = 0;
  $("content").querySelector("[data-notesave]").click();
  const save = sent.find((m) => m.type === "notesave");
  if (!save || save.id !== "n1" || save.value?.body !== "# Tip\n\nhello") throw new Error(JSON.stringify(sent));
  kiri.on({ type: "notesaved", id: "n1", title: "Compose tip", updated: 2 });
  kiri.on({ type: "note", id: "n1", title: "Compose tip", body: "# Tip\n\nhello", updated: 2 });
  eq(ui.notes.dirty, false, "clean after save");
  $("content").querySelector('[data-notemode="preview"]').click();
  eq(ui.notes.mode, "preview", "preview mode");
  sent.length = 0;
  $("content").querySelector("[data-notedel]").click();
  $("content").querySelector("[data-notedelok]").click();
  if (!sent.some((m) => m.type === "notedelete" && m.id === "n1")) throw new Error(JSON.stringify(sent));
  kiri.on({ type: "notedeleted", id: "n1" });
  sent.length = 0;
  kiri.on({
    type: "notes",
    items: [{ id: "ops/n2", title: "docker run nginx", updated: 1 }],
    tree: [{ kind: "folder", id: "ops", name: "ops", children: [
      { kind: "note", id: "ops/n2", name: "docker run nginx", title: "docker run nginx", updated: 1 },
    ]}],
  });
  eq(ui.notes.id, "ops/n2", "auto-opens remaining");
  eq(ui.notes.mode, "preview", "remaining opens in preview");
  if (!sent.some((m) => m.type === "noteread" && m.id === "ops/n2")) throw new Error(JSON.stringify(sent));
});

t("New note uses note-pencil icon and opens in edit mode", () => {
  kiri.on({ type: "note", id: "ops/n2", title: "docker run nginx", body: "x", updated: 1 });
  // Create is only allowed under a selected folder/root, not a note.
  notesSelectFolder("ops");
  const btn = $("content").querySelector(".notes-side-h [data-notenew]");
  if (!btn) throw new Error("no side new");
  if (btn.disabled) throw new Error("new note disabled on folder");
  const d = btn.querySelector("svg path")?.getAttribute("d") || "";
  if (d === PH.sparkle) throw new Error("still sparkle");
  if (d === PH.logs) throw new Error("still logs/article icon");
  if (d !== PH.notepencil) throw new Error("want note-pencil icon");
  sent.length = 0;
  btn.click();
  if (!sent.some((m) => m.type === "notesave" && m.id === "" && m.value?.title)) throw new Error(JSON.stringify(sent));
  kiri.on({ type: "notesaved", id: "n9", title: "Untitled", updated: 3 });
  kiri.on({ type: "note", id: "n9", title: "Untitled", body: "", updated: 3 });
  kiri.on({
    type: "notes",
    items: [{ id: "n9", title: "Untitled", updated: 3 }, { id: "ops/n2", title: "docker run nginx", updated: 1 }],
    tree: [
      { kind: "folder", id: "ops", name: "ops", children: [
        { kind: "note", id: "ops/n2", name: "docker run nginx", title: "docker run nginx", updated: 1 },
      ]},
      { kind: "note", id: "n9", name: "Untitled", title: "Untitled", updated: 3 },
    ],
  });
  eq(ui.notes.id, "n9", "selected new");
  eq(ui.notes.mode, "edit", "new note edits");
  if (!$("note-body")) throw new Error("textarea for new note");
});

t("folder twist toggles open state", () => {
  const before = !!ui.notes.open.ops;
  $("content").querySelector('[data-notetwist="ops"]').click();
  eq(!!ui.notes.open.ops, !before, "toggled");
});

t("root has expand/collapse that closes folders to level 1", () => {
  kiri.on({
    type: "notes",
    items: [
      { id: "ops/run/n1", title: "deep", updated: 2 },
      { id: "ops/n2", title: "mid", updated: 1 },
    ],
    tree: [{ kind: "folder", id: "ops", name: "ops", children: [
      { kind: "folder", id: "ops/run", name: "run", children: [
        { kind: "note", id: "ops/run/n1", name: "deep", title: "deep", updated: 2 },
      ]},
      { kind: "note", id: "ops/n2", name: "mid", title: "mid", updated: 1 },
    ]}],
  });
  ui.notes.open = { ops: true, "ops/run": true };
  renderNotes();
  const fold = $("content").querySelector("[data-notestreefold]");
  if (!fold) throw new Error("no root fold button");
  // Deep note visible while nested folders open.
  if (!$("content").querySelector('[data-noteid="ops/run/n1"]')) throw new Error("deep note should show");
  fold.click();
  eq(!!ui.notes.open.ops, false, "ops closed");
  eq(!!ui.notes.open["ops/run"], false, "nested closed");
  if (!$("content").querySelector('[data-notefolder="ops"]')) throw new Error("level-1 folder still shown");
  if ($("content").querySelector('[data-noteid="ops/run/n1"]')) throw new Error("deep note hidden after collapse");
  if (!$("content").querySelector(".notes-row.root")) throw new Error("root stays");
  fold.click();
  eq(!!ui.notes.open.ops, true, "ops reopened");
  eq(!!ui.notes.open["ops/run"], true, "nested reopened");
});

t("collected notes show a solid star at the start of the row", () => {
  kiri.on({
    type: "notes",
    items: [
      { id: "c1", title: "nginxwebui 的启动命令", updated: 2, collected: true },
      { id: "n9", title: "Untitled", updated: 1 },
    ],
    tree: [
      { kind: "note", id: "c1", name: "nginxwebui 的启动命令", title: "nginxwebui 的启动命令", updated: 2, collected: true },
      { kind: "note", id: "n9", name: "Untitled", title: "Untitled", updated: 1 },
    ],
  });
  const starred = $("content").querySelector('[data-noteid="c1"]');
  const plain = $("content").querySelector('[data-noteid="n9"]');
  const star = starred?.querySelector("svg.ic.star");
  if (!star) throw new Error("collected row missing star");
  if (plain?.querySelector("svg.ic.star")) throw new Error("plain note should not have star");
  eq(starred.getAttribute("data-collected"), "1", "collected attr");
  // Star leads the row (left gutter); notepad icons stay siblings of .twist.
  if (starred.firstElementChild !== star) throw new Error("star should lead the row");
  const starredNote = starred.querySelector("svg.ic:not(.star) path")?.getAttribute("d");
  const plainNote = plain.querySelector("svg.ic path")?.getAttribute("d");
  eq(starredNote, PH.notepad, "starred notepad");
  eq(plainNote, PH.notepad, "plain notepad");
  // Save-to-Notes marks collected on create.
  sent.length = 0;
  notesCollect("Command for web", "```bash\ndocker run\n```");
  const save = sent.find((m) => m.type === "notesave");
  if (!save?.value?.collected) throw new Error(JSON.stringify(save));
});
t("note uses notepad icon for memos", () => {
  kiri.on({
    type: "notes",
    items: [{ id: "n1", title: "Compose tip", updated: 2 }],
    tree: [{ kind: "note", id: "n1", name: "Compose tip", title: "Compose tip", updated: 2 }],
  });
  const ic = $("content").querySelector('[data-noteid="n1"] > svg.ic path');
  const d = ic?.getAttribute("d") || "";
  if (d === PH.logs) throw new Error("still logs/article icon");
  if (d === PH.note) throw new Error("still folded-note icon");
  if (d !== PH.notepad) throw new Error("want notepad icon");
});

t("note context menu can reveal file location", () => {
  kiri.on({
    type: "notes",
    items: [{ id: "Compose-tip-n1", title: "Compose tip", updated: 2 }],
    tree: [{ kind: "note", id: "Compose-tip-n1", name: "Compose tip", title: "Compose tip", updated: 2 }],
  });
  const row = $("content").querySelector('[data-noteid="Compose-tip-n1"]');
  row.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 40, clientY: 80 }));
  const reveal = document.querySelector('#notes-ctx [data-ctx="reveal"]');
  if (!reveal) throw new Error("missing Open location");
  if (!reveal.textContent.includes("Open location")) throw new Error(reveal.textContent);
  if (!reveal.querySelector("svg")) throw new Error("menu item needs icon");
  sent.length = 0;
  reveal.click();
  if (!sent.some((m) => m.type === "notereveal" && m.id === "Compose-tip-n1")) {
    throw new Error(JSON.stringify(sent));
  }
});

t("folder context menu can reveal folder location", () => {
  kiri.on({
    type: "notes",
    items: [{ id: "ops/n2", title: "docker run nginx", updated: 1 }],
    tree: [{ kind: "folder", id: "ops", name: "ops", children: [
      { kind: "note", id: "ops/n2", name: "docker run nginx", title: "docker run nginx", updated: 1 },
    ]}],
  });
  ui.notes.open.ops = true;
  renderNotes();
  const folder = $("content").querySelector('[data-notefolder="ops"]');
  folder.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 40, clientY: 80 }));
  const reveal = document.querySelector('#notes-ctx [data-ctx="reveal"]');
  if (!reveal) throw new Error("folder missing Open location");
  if (!reveal.querySelector("svg")) throw new Error("folder menu needs icons");
  const neu = document.querySelector('#notes-ctx [data-ctx="newnote"]');
  if (!neu?.querySelector("svg")) throw new Error("newnote icon");
  sent.length = 0;
  reveal.click();
  if (!sent.some((m) => m.type === "notereveal" && m.id === "ops")) {
    throw new Error(JSON.stringify(sent));
  }
});

t("note row keeps rename/delete inline with pencil icon", () => {
  kiri.on({
    type: "notes",
    items: [{ id: "n1", title: "Compose tip", updated: 2 }],
    tree: [{ kind: "note", id: "n1", name: "Compose tip", title: "Compose tip", updated: 2 }],
  });
  const row = $("content").querySelector('[data-noteid="n1"]');
  if (!row) throw new Error("no note row");
  if (row.tagName !== "DIV") throw new Error("note row must be div, got " + row.tagName);
  const rename = row.querySelector("[data-noterenameui]");
  const del = row.querySelector("[data-notedeltree]");
  if (!rename || !del) throw new Error("acts must stay inside the note row");
  const d = rename.querySelector("svg path")?.getAttribute("d") || "";
  if (d === PH.textaa) throw new Error("still Aa/textaa icon");
  if (d !== PH.pencil) throw new Error("want pencil icon");
  // Acts are siblings of the title inside the same row (not wrapped below).
  eq(rename.parentElement, del.parentElement, "same acts container");
  eq(rename.closest("[data-noteid]"), row, "rename inside row");
  sent.length = 0;
  del.click();
  const dlg = document.querySelector(".scrim .dialog [data-go]");
  if (!dlg) throw new Error("no confirm dialog");
  dlg.click();
  if (!sent.some((m) => m.type === "notedelete" && m.id === "n1")) throw new Error(JSON.stringify(sent));
});

t("long note names expose full title on hover", () => {
  const long = "未命名很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长";
  kiri.on({
    type: "notes",
    items: [{ id: "long-n1", title: long, updated: 2 }],
    tree: [{ kind: "note", id: "long-n1", name: long, title: long, updated: 2 }],
  });
  kiri.on({ type: "note", id: "long-n1", title: long, body: "x", updated: 2 });
  const tip = $("content").querySelector('[data-noteid="long-n1"] .t')?.getAttribute("title") || "";
  eq(tip, long, "tree tip");
  const title = $("note-title");
  if (!title) throw new Error("no title field");
  eq(title.getAttribute("title"), long, "header tip");
});

t("clicking rename input stays in edit mode", () => {
  kiri.on({
    type: "notes",
    items: [{ id: "Compose-tip-n1", title: "Compose tip", updated: 2 }],
    tree: [{ kind: "note", id: "Compose-tip-n1", name: "Compose tip", title: "Compose tip", updated: 2 }],
  });
  const btn = $("content").querySelector('[data-noteid="Compose-tip-n1"] [data-noterenameui]');
  if (!btn) throw new Error("no rename button");
  btn.click();
  const input = $("note-rename");
  if (!input) throw new Error("rename input missing after pencil");
  eq(ui.notes.renaming?.id, "Compose-tip-n1", "renaming set");
  // A click on the input used to select the row and clear renaming.
  input.click();
  if (!$("note-rename")) throw new Error("rename input destroyed by click");
  eq(ui.notes.renaming?.id, "Compose-tip-n1", "still renaming after input click");
  input.value = "New title";
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  if (!sent.some((m) => m.type === "noterename" && m.id === "Compose-tip-n1" && m.value?.name === "New title")) {
    throw new Error(JSON.stringify(sent));
  }
});

t("right-click New subfolder uses inline create then notemkdir", () => {
  kiri.on({
    type: "notes",
    items: [{ id: "ops/n2", title: "docker run nginx", updated: 1 }],
    tree: [{ kind: "folder", id: "ops", name: "ops", children: [
      { kind: "note", id: "ops/n2", name: "docker run nginx", title: "docker run nginx", updated: 1 },
    ]}],
  });
  ui.notes.open.ops = true;
  renderNotes();
  const folder = $("content").querySelector('[data-notefolder="ops"]');
  if (!folder) throw new Error("no ops folder");
  folder.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 40, clientY: 80 }));
  const menu = document.getElementById("notes-ctx");
  if (!menu || menu.hidden) throw new Error("context menu closed");
  const mk = menu.querySelector('[data-ctx="newfolder"]');
  if (!mk) throw new Error("no new subfolder");
  eq(mk.textContent, "New subfolder", "menu label");
  mk.click();
  const input = $("note-create");
  if (!input) throw new Error("no inline create input");
  eq(ui.notes.creating?.parent, "ops", "parent folder");
  input.value = "runbooks";
  sent.length = 0;
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  const mkdir = sent.find((m) => m.type === "notemkdir");
  if (!mkdir || mkdir.value?.parent !== "ops" || mkdir.value?.name !== "runbooks") throw new Error(JSON.stringify(sent));
  kiri.on({ type: "notefolder", op: "mkdir", id: "ops/runbooks" });
  kiri.on({
    type: "notes",
    items: [{ id: "n9", title: "Untitled", updated: 3 }, { id: "ops/n2", title: "docker run nginx", updated: 1 }],
    tree: [
      { kind: "folder", id: "ops", name: "ops", children: [
        { kind: "folder", id: "ops/runbooks", name: "runbooks", children: [] },
        { kind: "note", id: "ops/n2", name: "docker run nginx", title: "docker run nginx", updated: 1 },
      ]},
      { kind: "note", id: "n9", name: "Untitled", title: "Untitled", updated: 3 },
    ],
  });
  if (!$("content").querySelector('[data-notefolder="ops/runbooks"]')) throw new Error("subfolder missing in tree");
  eq(ui.notes.creating, null, "create row cleared");
});

t("Chinese chrome for Notes", () => {
  prefs.lang = "zh";
  paintChrome();
  show("notes");
  kiri.on({ type: "notes", items: [], tree: [] });
  eq($("title").textContent, "便笺", "title zh");
  eq(document.querySelector('[data-view="notes"] .lbl')?.textContent, "便笺", "nav zh");
  const root = $("content").querySelector(".notes-row.root .t");
  eq(root?.textContent, "便笺", "root zh");
  const text = $("content").textContent || "";
  if (!text.includes("还没有便笺")) throw new Error(text.slice(0, 160));
  // Right-click root → 新建子目录
  $("content").querySelector(".notes-row.root").dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 20, clientY: 60 }));
  eq(document.querySelector('#notes-ctx [data-ctx="newfolder"]')?.textContent, "新建子目录", "zh menu");
  // Host error strings (notice.text) are translated via tr().
  eq(tr("Folder already exists"), "文件夹已存在", "folder exists zh");
  eq(tr("Note already exists"), "便笺已存在", "note exists zh");
  eq(tr("Name required"), "请填写名称", "name required zh");
  prefs.lang = "en";
  paintChrome();
  notesCloseCtx();
});

t("clicking a folder selects it and enables create under it", () => {
  kiri.on({
    type: "notes",
    items: [
      { id: "ops/n2", title: "docker run nginx", updated: 1 },
      { id: "n1", title: "Compose tip", updated: 2 },
    ],
    tree: [
      { kind: "folder", id: "ops", name: "ops", children: [
        { kind: "note", id: "ops/n2", name: "docker run nginx", title: "docker run nginx", updated: 1 },
      ]},
      { kind: "note", id: "n1", name: "Compose tip", title: "Compose tip", updated: 2 },
    ],
  });
  kiri.on({ type: "note", id: "n1", title: "Compose tip", body: "x", updated: 2 });
  eq(ui.notes.id, "n1", "note open");
  const mkdir0 = $("content").querySelector(".notes-side-h [data-notemkdir]");
  const new0 = $("content").querySelector(".notes-side-h [data-notenew]");
  if (!mkdir0.disabled || !new0.disabled) throw new Error("create must be disabled while a note is selected");
  $("content").querySelector('[data-notefolder="ops"]').click();
  eq(ui.notes.id, "", "note cleared");
  eq(ui.notes.folder, "ops", "folder selected");
  eq($("content").querySelector('[data-notefolder="ops"]')?.getAttribute("aria-current"), "true", "folder aria");
  eq($("content").querySelector('[data-noteid="n1"]')?.getAttribute("aria-current"), "false", "note not current");
  const mkdir1 = $("content").querySelector(".notes-side-h [data-notemkdir]");
  const new1 = $("content").querySelector(".notes-side-h [data-notenew]");
  if (mkdir1.disabled || new1.disabled) throw new Error("create enabled on folder");
  eq(mkdir1.getAttribute("data-notemkdir"), "ops", "mkdir targets ops");
  // Right pane offers New note for the selected folder.
  const paneNew = $("content").querySelector(".notes-main [data-notenew]");
  if (!paneNew) throw new Error("folder selection should show New note shortcut");
  if (!($("content").textContent || "").includes("ops")) throw new Error("folder name missing in pane");
  sent.length = 0;
  new1.click();
  const save = sent.find((m) => m.type === "notesave");
  if (!save || save.value?.folder !== "ops") throw new Error(JSON.stringify(sent));
});

t("empty note shows No content; body shows preview", () => {
  kiri.on({
    type: "notes",
    items: [{ id: "n1", title: "Empty", updated: 2 }],
    tree: [{ kind: "note", id: "n1", name: "Empty", title: "Empty", updated: 2 }],
  });
  kiri.on({ type: "note", id: "n1", title: "Empty", body: "", updated: 2 });
  eq(ui.notes.mode, "preview", "preview");
  if (!($("content").textContent || "").includes("No content")) throw new Error("want No content placeholder");
  if ($("content").querySelector(".notes-preview")) throw new Error("no preview for empty body");
  const goEdit = $("content").querySelector('.notes-blank [data-notemode="edit"]');
  if (!goEdit) throw new Error("edit shortcut on empty note");
  goEdit.click();
  eq(ui.notes.mode, "edit", "edit from empty");
  if (!$("note-body")) throw new Error("textarea in edit");
  // With content, preview renders markdown.
  ui.notes.mode = "preview";
  kiri.on({ type: "note", id: "n1", title: "Empty", body: "hello **world**", updated: 3 });
  if (($("content").textContent || "").includes("No content")) throw new Error("content should hide placeholder");
  if (!$("content").querySelector(".notes-preview strong")) throw new Error("preview markdown");
});

t("clicking a note selects it and disables create", () => {
  kiri.on({
    type: "notes",
    items: [{ id: "ops/n2", title: "docker run nginx", updated: 1 }],
    tree: [{ kind: "folder", id: "ops", name: "ops", children: [
      { kind: "note", id: "ops/n2", name: "docker run nginx", title: "docker run nginx", updated: 1 },
    ]}],
  });
  ui.notes.open.ops = true;
  notesSelectFolder("ops");
  eq(ui.notes.folder, "ops", "folder first");
  if ($("content").querySelector(".notes-side-h [data-notenew]")?.disabled) throw new Error("create should work on folder");
  sent.length = 0;
  $("content").querySelector('[data-noteid="ops/n2"]').click();
  if (!sent.some((m) => m.type === "noteread" && m.id === "ops/n2")) throw new Error(JSON.stringify(sent));
  kiri.on({ type: "note", id: "ops/n2", title: "docker run nginx", body: "x", updated: 1 });
  eq(ui.notes.id, "ops/n2", "note selected");
  eq($("content").querySelector('[data-noteid="ops/n2"]')?.getAttribute("aria-current"), "true", "note aria");
  eq($("content").querySelector('[data-notefolder="ops"]')?.getAttribute("aria-current"), "false", "folder not current");
  const mkdir = $("content").querySelector(".notes-side-h [data-notemkdir]");
  const neu = $("content").querySelector(".notes-side-h [data-notenew]");
  if (!mkdir.disabled || !neu.disabled) throw new Error("create disabled on note");
});

t("clicking root selects root and creates at top level", () => {
  kiri.on({
    type: "notes",
    items: [{ id: "n1", title: "Compose tip", updated: 2 }],
    tree: [{ kind: "note", id: "n1", name: "Compose tip", title: "Compose tip", updated: 2 }],
  });
  kiri.on({ type: "note", id: "n1", title: "Compose tip", body: "x", updated: 2 });
  $("content").querySelector(".notes-row.root").click();
  eq(ui.notes.id, "", "cleared note");
  eq(ui.notes.folder, "", "root folder");
  eq($("content").querySelector(".notes-row.root")?.getAttribute("aria-current"), "true", "root current");
  const neu = $("content").querySelector(".notes-side-h [data-notenew]");
  if (neu.disabled) throw new Error("create on root");
  sent.length = 0;
  neu.click();
  const save = sent.find((m) => m.type === "notesave");
  if (!save || save.value?.folder) throw new Error("want root note: " + JSON.stringify(sent));
});

report(R);
