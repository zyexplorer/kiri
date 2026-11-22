// The first start on Linux: the setup checklist and the language step arrive
// together. The language step is on top (answered first), and a pick redraws
// the checklist behind it in that language.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const dlg = () => document.querySelector('[aria-labelledby="lang-title"]');
kiri.on({ type: "langpick" });
kiri.on({ type: "onboarding", ready: false, user: "ada", checks: { engine: "ok", running: "ok", access: "no", docker: "off", builds: "off" } });
t("the language step is above the setup checklist", () => {
  if (!dlg()) throw new Error("no language step");
  const r = dlg().getBoundingClientRect();
  const hit = document.elementFromPoint(r.left + r.width / 2, r.top + 20);
  if (!dlg().contains(hit)) throw new Error("the point shows " + (hit?.closest("[id]")?.id || hit?.tagName));
});
const onbText = () => document.getElementById("onb").textContent;
const before = onbText();
t("picking German redraws the checklist in German", () => {
  dlg().querySelector('[data-lang="de"]').click();
  const now = onbText();
  if (now === before) throw new Error("the checklist didn't change");
  if (/Set up Kiri/.test(now)) throw new Error("still English: " + now.slice(0, 80));
});
t("Continue leaves the checklist, in German", () => {
  document.getElementById("lang-go").click();
  if (dlg()) throw new Error("the language step is still open");
  if (!document.getElementById("onb")) throw new Error("the checklist went too");
  if (/Set up Kiri/.test(onbText())) throw new Error("English again");
});
report(R);
