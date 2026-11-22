// parity: macos windows linux
// window: 760x480
// Every <select> is a button that opens a list (ssOpen): a search box for
// long lists, keys, a click outside, RTL, flipping up near the bottom, and
// 1,000 fonts that open at once, each row in its own font once it's in view.
{
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const need = (ok, msg) => { if (!ok) throw new Error(msg); };
const kd = (k, more = {}) => { const e = new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true, ...more }); (document.activeElement || document.body).dispatchEvent(e); return e; };
const pop = () => document.querySelector(".sspop");
const q = () => pop()?.querySelector("input");
const rows = () => [...(pop()?.querySelectorAll('[role="option"]') || [])];
const act = () => document.getElementById((q() || pop().querySelector('[role="listbox"]')).getAttribute("aria-activedescendant"));
const type = (s) => { q().value = s; q().dispatchEvent(new Event("input", { bubbles: true })); ss.open?.flush(); };
const box = (el) => el.getBoundingClientRect();
const shut = () => { while (pop()) kd("Escape"); };
const fam = Array.from({ length: 1000 }, (_, i) => `Family ${String(i).padStart(4, "0")}`);
fam[500] = "Crème Brûlée";
kiri.on({ type: "fonts", families: fam, mono: ["Courier New", "Fira Code", "Menlo", "Monaco"] });
show("settings");
document.querySelector('[data-stab="appearance"]').click();
// The buttons come from a MutationObserver, which runs before the window paints
// but not inside this one script: ssScan() is what it would do.
const font = () => document.getElementById("pref-font"), fbtn = () => (ssScan(), font().ssb);
const mono = () => document.getElementById("pref-mono"), mbtn = () => (ssScan(), mono().ssb);

t("every select but the language one is a button; the select stays, hidden", () => {
  ssScan();
  const all = [...document.querySelectorAll("select")];
  need(all.length >= 3, "selects: " + all.length);
  for (const s of all) {
    if (s.classList.contains("lsrc")) { need(!s.ssb, "the language select got a button too"); continue; }
    need(s.ssb && s.previousElementSibling === s.ssb, "no button before " + (s.id || s.className));
    need(getComputedStyle(s).display === "none", (s.id || s.className) + " still shows");
  }
});
t("the button looks like a select: 32 px, the chevron, the value, the hand cursor", () => {
  const b = fbtn(), cs = getComputedStyle(b);
  need(Math.round(box(b).height) === 32, "height " + box(b).height);
  need(cs.cursor === "pointer" && cs.borderTopWidth === "1px" && cs.borderRadius === "7px", `${cs.cursor} ${cs.borderTopWidth} ${cs.borderRadius}`);
  need(b.querySelector("svg.lchev"), "no chevron");
  need(b.textContent.trim() === tr("System|font"), "reads " + b.textContent);
  need(b.getAttribute("role") === "combobox" && b.getAttribute("aria-haspopup") === "listbox" && b.getAttribute("aria-expanded") === "false", "roles");
  need(b.getAttribute("aria-label") === tr("Interface font"), "label " + b.getAttribute("aria-label"));
  R.push(`ITEM select button: pref-font ${b.getAttribute("role")}`, `ITEM select button: pref-mono ${mbtn().getAttribute("role")}`);
});
t("a value set from code shows on the button", () => {
  font().value = "Family 0003";
  need(fbtn().textContent.trim() === "Family 0003", "reads " + fbtn().textContent);
  font().value = "System";
});
let took = 0;
t("1,000 fonts open in under 100 ms, drawing only the rows in view", () => {
  const t0 = performance.now();
  fbtn().click();
  document.body.offsetHeight;
  took = performance.now() - t0;
  need(pop(), "didn't open");
  need(took < 100, `took ${took.toFixed(1)} ms`);
  need(rows().length > 3 && rows().length < 40, rows().length + " rows in the page");
  need(rows().every((r) => r.getAttribute("aria-setsize") === "1001"), "aria-setsize");
});
t("the search is focused, with combobox and listbox roles", () => {
  need(document.activeElement === q(), "focus on " + document.activeElement?.className);
  const list = document.getElementById(q().getAttribute("aria-controls"));
  need(q().getAttribute("role") === "combobox" && list?.getAttribute("role") === "listbox", "roles");
  need(fbtn().getAttribute("aria-expanded") === "true", "aria-expanded");
  need(act()?.getAttribute("aria-selected") === "true" && act().textContent === tr("System|font"), "highlight on " + act()?.textContent);
});
t("typing filters, case and accents dropped", () => {
  type("CREME brulee");
  need(rows().length === 1 && rows()[0].textContent === "Crème Brûlée", rows().map((r) => r.textContent).join());
  type("family 099");
  need(rows().length === 10 && act().textContent === "Family 0990", rows().length + " " + act()?.textContent);
});
t("No matches when nothing does", () => {
  type("zzzz");
  need(!rows().length && pop().querySelector('[role="status"]').textContent === tr("No matches"), "no notice");
});
t("Escape clears the search, then closes and gives the button the focus", () => {
  kd("Escape");
  need(pop() && q().value === "" && rows().length > 3, "didn't clear");
  kd("Escape");
  need(!pop() && document.activeElement === fbtn() && fbtn().getAttribute("aria-expanded") === "false", "didn't close");
});
t("arrows and Enter pick, sending the change as a <select> does", () => {
  sent.length = 0;
  kd("ArrowDown");
  need(pop(), "Down didn't open");
  kd("ArrowDown"); kd("ArrowDown"); kd("ArrowUp");
  need(act().textContent === "Family 0000", "on " + act().textContent);
  kd("End");
  need(act().textContent === "Family 0999", "End on " + act().textContent);
  kd("Home"); kd("ArrowDown");
  kd("Enter");
  need(!pop() && font().value === "Family 0000" && fbtn().textContent.trim() === "Family 0000", "value " + font().value);
  need(sent.some((m) => m.type === "setpref" && m.name === "font" && m.value === "Family 0000"), "no setpref");
});
t("a click outside closes; a click on a row picks", () => {
  fbtn().click();
  document.body.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
  need(!pop(), "still open");
  fbtn().click();
  type("family 0007");
  rows()[0].click();
  need(!pop() && font().value === "Family 0007", "value " + font().value);
});
t("a short list has no search box; letters jump, Tab closes and moves on", () => {
  mbtn().click();
  need(pop() && !q() && rows().length === 5, "rows " + rows().length);
  need(document.activeElement === pop().querySelector('[role="listbox"]'), "focus on the list");
  kd("m");
  need(act().textContent === "Menlo", "on " + act().textContent);
  kd("Tab");
  need(!pop() && document.activeElement === mbtn(), "Tab didn't close to the button");
});
t("near the bottom the list opens upwards, all of it in the window", () => {
  const c = document.getElementById("content");
  c.scrollTop = 0;
  const y = box(fbtn()).bottom;
  c.scrollTop = Math.max(0, y - innerHeight + 24);
  need(box(fbtn()).bottom > innerHeight - 140, "button at " + box(fbtn()).bottom + " of " + innerHeight + " (can't scroll it low enough)");
  fbtn().click();
  const p = box(pop()), b = box(fbtn());
  need(pop().classList.contains("up") && p.bottom <= b.top + 1, `list ${p.top}–${p.bottom}, button top ${b.top}`);
  need(p.top >= 0 && p.left >= 0 && p.right <= innerWidth && p.bottom <= innerHeight, `list ${p.left},${p.top} ${p.right},${p.bottom}`);
  shut(); c.scrollTop = 0;
});
t("in Arabic the list lines up with the button's right edge", () => {
  document.documentElement.dir = "rtl";
  fbtn().click();
  const p = box(pop()), b = box(fbtn());
  need(Math.abs(p.right - b.right) < 2 || p.left <= 8.5, `list ${p.left}–${p.right} vs button ${b.left}–${b.right}`);
  need(getComputedStyle(rows()[0]).direction === "rtl", "rows aren't right to left");
  shut();
  document.documentElement.dir = "ltr";
});
R.push(`PASS (1,000 fonts opened in ${took.toFixed(1)} ms)`);
// Font previews: only the rows in view, in their own font, once scrolling stops.
// (After the scrolls above have landed: a scroll outside the list closes it.)
setTimeout(() => {
  fbtn().click();
  const list = pop().querySelector('[role="listbox"]');
  list.scrollTop = 300 * 26;
  list.dispatchEvent(new Event("scroll"));
  setTimeout(() => {
    t("font rows show in their own font, only those in view", () => {
      const styled = rows().filter((r) => r.firstChild.style.fontFamily);
      const inView = rows().filter((r) => { const y = r.offsetTop - list.scrollTop; return y + r.offsetHeight > 0 && y < list.clientHeight; });
      need(styled.length && styled.length <= inView.length && inView.length <= 12, `${styled.length} styled, ${inView.length} in view`);
      need(styled.every((r) => r.firstChild.style.fontFamily.includes(r.textContent)), "a row in another font");
      need(rows().some((r) => r.textContent === "Family 0299"), "the rows in view weren't drawn");
    });
    shut();
    // A section drawn again while its list is open (setHTML, a list on a timer, a running build).
    t("setHTML that draws the same leaves an open list alone; a new drawing hands it to the new select", () => {
      const host = document.createElement("div"), h1 = '<select id="ss-redraw"><option value="a">Ay</option><option value="b">Bee</option></select>';
      document.body.append(host);
      setHTML(host, h1); ssScan();
      const b1 = $("ss-redraw").ssb;
      b1.click();
      setHTML(host, h1); ssScan();
      need(pop() && ss.open.b === b1 && b1.isConnected && b1.getAttribute("aria-expanded") === "true", "the same drawing moved the list");
      setHTML(host, h1.replace("Bee", "Bea").replace("</select>", '<option value="c">Sea</option></select>')); ssScan();
      const b2 = $("ss-redraw").ssb;
      need(b2 && b2 !== b1 && !b1.isConnected, "no new button");
      need(pop() && ss.open.b === b2 && b2.getAttribute("aria-expanded") === "true", "the list closed or kept the old button");
      need(rows().map((r) => r.textContent).join() === "Ay,Bea,Sea", "rows " + rows().map((r) => r.textContent).join());
      let changed = 0;
      $("ss-redraw").addEventListener("change", () => changed++);
      rows()[2].click();
      need(!pop() && $("ss-redraw").value === "c" && changed === 1 && b2.querySelector(".ssv").textContent === "Sea", `value ${$("ss-redraw").value}, ${changed} changes`);
      host.remove();
    });
    t("Which steps stays open while a running build redraws Builds, and still picks", () => {
      const at = new Date().toISOString();
      kiri.on({ type: "builds", builds: [{ id: "b1", builder: "kiri", name: "app", status: "running", started: at }] });
      show("builds");
      const step = (done) => kiri.on({ type: "build", id: "b1", started: at, total: 3, done, steps: [
        { name: "[1/3] FROM alpine", started: at, completed: at, cached: true },
        { name: "[2/3] RUN make", started: at, ...(done > 1 ? { completed: at } : {}) },
        ...(done > 1 ? [{ name: "[3/3] COPY . /app", started: at }] : [])] });
      step(1); ssScan();
      const sel = () => document.querySelector("[data-blsteps]");
      need(sel()?.ssb, "no Which steps button");
      const b1 = sel().ssb;
      b1.click();
      need(pop() && rows().length === 4, "didn't open");
      step(2); ssScan();
      need(!b1.isConnected && sel().ssb !== b1, "Builds wasn't drawn again");
      need(pop() && ss.open.b === sel().ssb && sel().ssb.getAttribute("aria-expanded") === "true", "the list closed on the redraw");
      const p = box(pop()), b = box(sel().ssb);
      need(Math.abs(p.top - b.bottom) < 8 || Math.abs(p.bottom - b.top) < 8, `list at ${p.top}–${p.bottom}, button ${b.top}–${b.bottom}`);
      const ran = rows().find((r) => r.textContent === tr("Ran"));
      need(ran, "rows " + rows().map((r) => r.textContent).join());
      ran.click(); ssScan();
      need(!pop() && ui.bl.steps === "ran" && sel().value === "ran" && sel().ssb.querySelector(".ssv").textContent === tr("Ran"), `steps ${ui.bl.steps}, value ${sel().value}`);
    });
    report(R);
  }, 400);
}, 200);
}
