// parity: macos windows linux
// window: 900x640
// A project opens at the top of its page, as a container's or pod's does: a
// list scrolled down no longer leaves the name, state and buttons out of sight.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
ui.vm = "running";
kiri.on({ type: "lists", containers: [], images: [], volumes: [], networks: [] });
const svc = (name) => ({ name, image: name + ":latest", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] });
const names = Array.from({ length: 14 }, (_, i) => "app" + String(i).padStart(2, "0"));
kiri.on({ type: "composeprojects", logos: {}, projects: names.map((n) => ({ name: n, dir: "/w/" + n, files: [`/w/${n}/compose.yaml`], readable: true, updated: 1,
  services: [svc("web"), svc("db")], containers: [] })) });
show("compose");
const c = $("content");
t("the list scrolls down, then a project opens at the top", () => {
  c.scrollTop = 400;
  eq(c.scrollTop > 100, true, `the list scrolls (${c.scrollTop})`);
  const link = [...document.querySelectorAll("#content [data-gopen]")].pop();
  eq(!!link, true, "a project's link");
  click(link);
  eq([!!cmp.open, c.scrollTop], [true, 0], "opened at the top");
  const more = c.querySelector("#cmp-head [data-g=remove]"), r = more?.getBoundingClientRect(), box = c.getBoundingClientRect();
  eq([!!more, r && r.top >= box.top && r.bottom <= box.bottom], [true, true], `the head's buttons are in sight (${r && Math.round(r.top)} in ${Math.round(box.top)}-${Math.round(box.bottom)})`);
});
report(R);
