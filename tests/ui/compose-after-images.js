// Images grid then Compose: setHTML must not keep the grid (morph invalidates its cache).
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + (e && e.message)); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error((m || "eq") + ": " + JSON.stringify(a) + " !== " + JSON.stringify(b)); };
const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
prefs.listLayout = { images: "grid" };
ui.vm = "running";
kiri.on({
  type: "lists",
  containers: [],
  images: [
    { Id: "sha256:aaa111", RepoTags: ["rancher/k3s:latest"], Size: 84900000, Created: Date.now() / 1000 - 86400 * 50 },
    { Id: "sha256:bbb222", RepoTags: ["redis:7"], Size: 3e7, Created: Date.now() / 1000 - 86400 },
  ],
  volumes: [], networks: [],
});
// Compose first (empty → setHTML), then Images grid (morph), then Compose again.
show("compose");
kiri.on({ type: "composeprojects", logos: {}, projects: [] });
t("compose empty first", () => {
  eq(!!$("content").querySelector(".empty"), true);
  eq($("content").querySelectorAll(".gcard.img").length, 0);
});
show("images");
t("images grid", () => eq($("content").querySelectorAll(".gcard.img").length, 2));
click(document.querySelector('nav button[data-view="compose"]'));
t("compose after images: no image cards", () => {
  eq(ui.view, "compose");
  eq($("content").querySelectorAll(".gcard.img").length, 0, "grid cleared");
  eq(!!$("content").querySelector(".empty"), true, "empty compose");
  eq(!!$("hright").querySelector("[data-cmpopenfile]"), true, "open button");
});
// Spinner path too: leave compose before projects answer, visit images, return.
cmp.list = null; cmp.asked = 0;
show("compose");
t("spinner", () => eq(!!$("content").querySelector(".empty"), true));
show("images");
show("compose");
t("compose spinner after images", () => {
  eq($("content").querySelectorAll(".gcard.img").length, 0);
  eq(!!$("content").querySelector(".empty"), true);
});
report(R);
