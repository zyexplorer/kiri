// lang: zh-Hans
// The window in Simplified Chinese: every view, Settings tab and page the
// fixture can open reads Chinese, with no English sentence left over from a
// string that missed tr(). Names, images, commands and product names stay as
// they are, so only runs of ordinary English words count.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const check = (where) => t(`${where} reads Chinese`, () => {
  const left = english(document.body);
  if (left.length) throw new Error("English left: " + left.slice(0, 8).map((s) => JSON.stringify(s)).join(", "));
});

t("the page is in Chinese", () => {
  if (document.documentElement.lang !== "zh-Hans") throw new Error("lang is " + document.documentElement.lang);
  if (document.getElementById("strip-term").getAttribute("aria-label") !== "终端") throw new Error("the terminal button isn't");
  if (!/[一-鿿]/.test(document.querySelector("nav [data-view=containers]").textContent)) throw new Error("the sidebar isn't");
});
for (const v of ["containers", "images", "volumes", "networks", "builds", "share"]) { show(v); check(`the ${v} view`); }
// The USB view, with its beta note, a device in Kiri and one on the Mac (the
// note under the list is the host's, worded in the window's language there).
show("usb");
kiri.on({ type: "usb", can: "attach", devices: [
  { key: "a", name: "CP2102N", id: "10c4:ea60", speed: "full", state: "kiri", kind: "serial" },
  { key: "b", name: "SONOFF ZBDongle-E", id: "1a86:55d4", speed: "full", state: "host", kind: "radio" }] });
check("the USB view");
kiri.on({ type: "usb", devices: [] }); check("the USB view with nothing plugged in");
ui.images.push({ Id: "sha256:" + "ab".repeat(32), RepoTags: ["nginx:alpine"], Size: 5e7, Created: Date.now() / 1000 - 86400 });
show("images"); openImage("nginx:alpine"); check("an image's page");
show("settings");
for (const id of [...document.querySelectorAll("[data-stab]")].map((b) => b.dataset.stab)) {
  document.querySelector(`[data-stab="${id}"]`).click();
  check(`Settings → ${id}`);
}
t("a dev build's empty list names its own context, in Chinese", () => {
  kiri.on({ type: "status", dev: true, ours: "kiri-lang" });
  kiri.on({ type: "lists", containers: [], images: [], volumes: [], networks: [] });
  show("containers");
  const p = document.querySelector("#content .empty p, #content p")?.innerHTML || "";
  if (!/kiri-lang/.test(p) || !/[\u4e00-\u9fff]/.test(p)) throw new Error("it says " + p);
});
t("English again from Settings, without a reload", () => {
  show("settings");
  document.querySelector('[data-stab="appearance"]').click();
  const sel = document.getElementById("pref-lang"); sel.value = "en"; sel.dispatchEvent(new Event("change"));
  if (document.documentElement.lang !== "en") throw new Error("lang is " + document.documentElement.lang);
  const nav = document.querySelector("nav [data-view=containers]").textContent;
  if (!/Containers/.test(nav)) throw new Error("the sidebar says " + nav);
  if (!/Appearance/.test(document.querySelector(".settings .pane h3")?.textContent || "")) throw new Error("Settings didn't redraw");
  if (!sent.some((m) => m.type === "setpref" && m.name === "lang" && m.value === "en")) throw new Error("the pick wasn't saved");
  // Words drawn once: the terminal's header and the strip's button.
  const term = document.getElementById("strip-term"), split = document.querySelector('[data-ht="sr"]');
  if (term.getAttribute("aria-label") !== "Terminal" || !/^Split right \(/.test(split.title)) throw new Error(`the terminal says ${term.getAttribute("aria-label")} / ${split.title}`);
});
report(R);
