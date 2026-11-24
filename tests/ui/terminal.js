// parity: macos windows linux
// The built-in terminal: a panel under the page, with tabs, splits, find and
// its own text size, the same on every system (only the keys differ). Runs
// as each OS; what the panel offers is listed as ITEM lines.
const R = [], items = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const ok = (c, m) => { if (!c) throw new Error(m); };
const terms = (name) => sent.filter((s) => s.type === "term" && s.name === name);
const decode = (b64) => new TextDecoder().decode(Uint8Array.from(atob(b64), (c) => c.charCodeAt(0)));
const b64 = (s) => btoa(String.fromCharCode(...new TextEncoder().encode(s)));
const drawer = document.getElementById("htdrawer");
const panes = () => [...drawer.querySelectorAll(".htbody .htpane")];
const key = (target, o) => target.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...o }));
const OSK = WIN ? { sd: { key: "_", code: "Minus", altKey: true, shiftKey: true }, close: { key: "W", ctrlKey: true, shiftKey: true }, find: { key: "F", ctrlKey: true, shiftKey: true }, zin: { key: "=", ctrlKey: true } }
  : LINUX ? { sd: { key: ")", ctrlKey: true, shiftKey: true }, close: { key: "W", ctrlKey: true, shiftKey: true }, find: { key: "F", ctrlKey: true, shiftKey: true }, zin: { key: "=", ctrlKey: true } }
  : { sd: { key: "d", metaKey: true, shiftKey: true }, close: null, find: { key: "f", metaKey: true }, zin: { key: "=", metaKey: true } };

t("hidden at first, with a toggle in the status bar and the strip", () => {
  ok(drawer.hidden, "drawer showing");
  ok(document.querySelector("#statusbar [data-htterm]"), "no status bar toggle");
  ok(document.getElementById("strip-term"), "no strip button");
  items.push("terminal: status bar toggle", "terminal: strip button");
});
t("the first open asks for the shells, then starts the default one", () => {
  document.querySelector("#statusbar [data-htterm]").click();
  ok(terms("shells").length === 1, "didn't ask for shells");
  kiri.on({ type: "term", ev: "shells", default: "/bin/zsh", shells: [{ id: "/bin/zsh", label: "zsh", group: "" }, { id: "/bin/bash", label: "bash", group: "" }, { id: "wsl:Ubuntu", label: "WSL · Ubuntu", group: "WSL" }] });
  ok(!drawer.hidden, "drawer still hidden");
  const open = terms("open");
  ok(open.length === 1 && open[0].value.shell === "/bin/zsh", "open: " + JSON.stringify(open));
  ok(open[0].value.cols > 10 && open[0].value.rows > 2, "size: " + JSON.stringify(open[0].value));
  ok(drawer.querySelector(".httab.on")?.textContent.includes("zsh"), "tab label");
  // The tab's close button is drawn: its icon has a stroke and a size.
  const xs = getComputedStyle(drawer.querySelector(".httab.on .x svg"));
  ok(xs.stroke !== "none" && parseFloat(xs.width) > 0, `close icon: stroke ${xs.stroke}, width ${xs.width}`);
  ok(document.getElementById("strip-term").getAttribute("aria-pressed") === "true", "strip button not pressed");
  for (const b of drawer.querySelectorAll(".hthead [data-ht]")) items.push(`terminal button: ${b.getAttribute("aria-label")}`);
});
t("strip and status bar buttons toggle the drawer closed again", () => {
  ok(!drawer.hidden, "need an open drawer");
  document.getElementById("strip-term").click();
  ok(drawer.hidden && document.getElementById("strip-term").getAttribute("aria-pressed") === "false", "strip did not hide");
  document.querySelector("#statusbar [data-htterm]").click();
  ok(!drawer.hidden && document.getElementById("strip-term").getAttribute("aria-pressed") === "true", "status bar did not show");
  document.querySelector("#statusbar [data-htterm]").click();
  ok(drawer.hidden && document.getElementById("strip-term").getAttribute("aria-pressed") === "false", "status bar did not hide");
  document.getElementById("strip-term").click();
  ok(!drawer.hidden, "strip did not reopen");
});
t("Ctrl+` toggles the drawer even when the terminal has focus", () => {
  ok(!drawer.hidden, "need an open drawer");
  const ta = ht.panes.get(ht.focus)?.term?.textarea || document.body;
  ta.focus?.();
  key(ta, { key: "`", code: "Backquote", ctrlKey: true });
  ok(drawer.hidden, "shortcut did not hide while terminal focused (double-toggle?)");
  key(document.body, { key: "`", code: "Backquote", ctrlKey: true });
  ok(!drawer.hidden, "shortcut did not show again");
});
const first = () => ht.panes.get(terms("open")[0].id);
t("what's typed goes to the shell as bytes", () => {
  first().term.input("ls ✓\r");
  const last = terms("input").pop();
  ok(last && last.id === first().id && decode(last.value) === "ls ✓\r", "input: " + JSON.stringify(last));
});
t("the new-tab menu lists the shells, grouped", () => {
  drawer.querySelector('[data-ht="menu"]').click();
  const m = drawer.querySelector(".htmenu");
  ok(!m.hidden, "menu hidden");
  ok([...m.querySelectorAll("[data-htshell]")].map((b) => b.dataset.htshell).join() === "/bin/zsh,/bin/bash,wsl:Ubuntu", "items");
  ok(m.querySelector(".ml")?.textContent === "WSL", "group heading");
  m.querySelector('[data-htshell="/bin/bash"]').click();
  ok(ht.tabs.length === 2 && terms("open").pop().value.shell === "/bin/bash", "second tab");
  items.push("terminal: shell menu");
  drawer.querySelector('[data-httab="0"]').click();
});
t("right-click a tab to pin it: first in the row, no ×, not closed by accident", () => {
  const second = drawer.querySelector('[data-httab="1"]');
  second.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 40, clientY: 10 }));
  const menu = drawer.querySelector(".htctx");
  ok(!menu.hidden && /Pin tab/.test(menu.textContent), "no menu");
  menu.querySelector("[data-htpin]").click();
  ok(ht.tabs[0].pinned && ht.tabs[0].shell === "/bin/bash", "not first");
  const tab0 = drawer.querySelector('[data-httab="0"]');
  ok(tab0.querySelector(".pin") && !tab0.querySelector("[data-htx]"), "still has ×");
  const closes = terms("close").length;
  ht.at = 0; htRender();
  htDo("close");
  ok(ht.tabs.length === 2 && terms("close").length === closes, "pinned tab closed");
  drawer.querySelector('[data-httab="0"]').dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 40, clientY: 10 }));
  ok(/Unpin tab/.test(menu.textContent), "no unpin");
  menu.querySelector("[data-htpin]").click();
  ok(!ht.tabs.some((t) => t.pinned), "still pinned");
  items.push("terminal: pin a tab");
  drawer.querySelector('[data-httab="0"]').click();
});
t("split right, then split down from the keyboard", () => {
  drawer.querySelector('[data-ht="sr"]').click();
  ok(panes().length === 2 && drawer.querySelector(".htbody").classList.contains("many"), "split right");
  key(ht.panes.get(ht.focus).term.textarea, OSK.sd);
  ok(panes().length === 3, `split down: ${panes().length} panes`);
  ok(drawer.querySelector(".htsplit.v"), "no vertical split");
});
t("each split pane has its own ×", () => {
  const before = panes().length, el = panes()[0], id = el.dataset.id;
  ok(getComputedStyle(el.querySelector(".htpx svg")).stroke !== "none", "× not drawn");
  el.querySelector("[data-htpx]").click();
  ok(panes().length === before - 1 && terms("close").some((s) => s.id === id), "× didn't close the pane");
  drawer.querySelector('[data-ht="sr"]').click();
});
t("a paste of several lines asks first", () => {
  const p = ht.panes.get(ht.focus), before = terms("input").length;
  kiri.on({ type: "term", ev: "paste", id: p.id, text: "echo a\necho b\n" });
  const ask = p.el.querySelector(".htask");
  ok(ask && /Paste 2 lines/.test(ask.textContent), "no question");
  ok(terms("input").length === before, "pasted before asking");
  ask.querySelector('[data-htask="yes"]').click();
  ok(decode(terms("input").pop().value).includes("echo a\recho b"), "pasted text");
});
t("one line pastes straight in", () => {
  const p = ht.panes.get(ht.focus);
  kiri.on({ type: "term", ev: "paste", id: p.id, text: "docker ps" });
  ok(!p.el.querySelector(".htask") && decode(terms("input").pop().value) === "docker ps", "single line");
});
t("text size: its own, from the keys and the window's ⌘+ while focused", () => {
  const fs = htFs();
  key(ht.panes.get(ht.focus).term.textarea, OSK.zin);
  ok(htFs() === fs + 1, "key: " + htFs());
  ht.panes.get(ht.focus).term.focus();
  const scale = prefs.scale;
  kiri.on({ type: "textsize", step: 1 });
  ok(htFs() === fs + 2 && prefs.scale === scale, "menu step went to the window");
  ok(sent.some((s) => s.type === "setpref" && s.name === "termFs"), "not saved");
  kiri.on({ type: "textsize", step: 0 });
  ok(htFs() === 12, "reset");
});
t("find opens from its key", () => {
  key(ht.panes.get(ht.focus).term.textarea, OSK.find);
  ok(!drawer.querySelector(".htfind").hidden, "find hidden");
  drawer.querySelector('[data-ht="findx"]').click();
  ok(drawer.querySelector(".htfind").hidden, "find still open");
});
t("closing a pane ends its shell", () => {
  const id = ht.focus;
  if (OSK.close) key(ht.panes.get(id).term.textarea, OSK.close);
  else { ht.panes.get(id).term.focus(); kiri.on({ type: "closekey" }); }
  ok(terms("close").some((s) => s.id === id), "no close sent");
  ok(panes().length === 2, `${panes().length} panes`);
});
t("⌘W away from the terminal closes the window", () => {
  document.activeElement?.blur();
  kiri.on({ type: "closekey" });
  ok(sent.some((s) => s.type === "closewindow"), "no closewindow");
});
t("a shell that exits closes its pane; one that can't start says why", () => {
  const id = ht.focus;
  kiri.on({ type: "term", ev: "exit", id, code: 0 });
  ok(panes().length === 1 && !ht.panes.has(id), "pane stayed");
  ok(!terms("close").some((s) => s.id === id), "closed an ended shell again");
  const left = ht.focus;
  kiri.on({ type: "term", ev: "exit", id: left, error: "No such file" });
  ok(ht.panes.has(left) && ht.panes.get(left).exited, "a failed start vanished");
});
t("hiding keeps the shells running", () => {
  const closes = terms("close").length;
  drawer.querySelector('[data-ht="hide"]').click();
  ok(drawer.hidden && terms("close").length === closes, "hide closed shells");
  document.getElementById("strip-term").click();
  ok(!drawer.hidden && ht.tabs.length === 2, "didn't come back as it was");
});
t("maximise fills the window, and restores", () => {
  drawer.querySelector('[data-ht="max"]').click();
  ok(drawer.classList.contains("max"), "not maximised");
  drawer.querySelector('[data-ht="max"]').click();
  ok(!drawer.classList.contains("max"), "not restored");
});
t("output reaches the screen", () => {
  window.liveId = [...ht.panes.values()].find((p) => !p.exited).id;
  kiri.on({ type: "term", ev: "data", id: liveId, b64: b64("hello from the shell") });
});
setTimeout(() => {
  t("output reaches the screen (drawn)", () => {
    const p = ht.panes.get(liveId);
    ok(p.term.buffer.active.getLine(0).translateToString().includes("hello from the shell"), "not written");
  });
  report([...R, ...items.map((x) => "ITEM " + x)]);
}, 300);
