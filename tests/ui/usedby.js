// Images' Used by column: how many containers, a box and count per state
// (none for a state with none), its words as the tooltip, and the spark
// that only plays when a lit box is on screen.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const C = (id, img, state, code) => ({ Id: id, Names: ["/" + id], Image: "x", ImageID: img, State: state, Status: state === "running" ? "Up 1 hour" : `Exited (${code})`, Created: 1, Ports: [], Labels: {} });
const IMG = (id, tag) => ({ Id: id, RepoTags: [tag], Size: 5e7, Created: 1 });
kiri.on({ type: "lists", volumes: [], networks: [],
  containers: [C("r1", "sha256:mix", "running"), C("r2", "sha256:mix", "running"), C("s1", "sha256:mix", "exited", 0), C("c1", "sha256:mix", "created"),
    C("f1", "sha256:mix", "exited", 1), C("f2", "sha256:mix", "dead"), C("s2", "sha256:off", "exited", 0), C("f3", "sha256:bad", "exited", 2)],
  images: [IMG("sha256:mix", "kiri-test/mix:1"), IMG("sha256:off", "kiri-test/off:1"), IMG("sha256:bad", "kiri-test/bad:1"), IMG("sha256:none", "kiri-test/none:1"), IMG("sha256:dangle", "<none>:<none>")] });
show("images");
const row = (k) => document.querySelector(`#content tr[data-key="${k}"]`);
const cell = (k) => row(k).querySelector("td.used");
const groups = (k) => [...cell(k).querySelectorAll(".l2 > span")].map((s) => [s.querySelector(".cube").classList[1], s.textContent.trim()]);
t("a Used by column sits between Tag and ID", () => {
  eq([...document.querySelectorAll("#content thead th:not(.ck)")].map((th) => th.textContent.trim()).slice(0, 5), ["Name", "Repository", "Tag", "Used by", "ID"], "head");
});
t("the top line counts the containers, the second a box and count per state", () => {
  eq(cell("kiri-test/mix:1").querySelector(".l1").textContent, "6 containers", "top line");
  eq(groups("kiri-test/mix:1"), [["run", "2"], ["stop", "2"], ["fail", "2"]], "running, stopped (clean exit, created), failed (non-zero, dead)");
  eq(cell("kiri-test/bad:1").querySelector(".l1").textContent, "1 container", "one");
});
t("a state with none is left out", () => {
  eq(groups("kiri-test/off:1"), [["stop", "1"]], "stopped only");
  eq(groups("kiri-test/bad:1"), [["fail", "1"]], "failed only");
});
t("the tooltip says the counts in words", () => {
  eq(cell("kiri-test/mix:1").title, "2 running containers · 2 stopped containers · 2 failed containers", "several");
  eq(cell("kiri-test/bad:1").title, "1 failed container", "one");
});
t("an unused image: one grey box and not in use; its row is not dimmed", () => {
  const c = cell("kiri-test/none:1");
  eq([c.querySelectorAll(".cube").length, c.querySelector(".cube").classList.contains("off"), c.textContent.trim(), c.querySelector(".l2")], [1, true, "not in use", null], "cell");
  eq(getComputedStyle(row("kiri-test/none:1").querySelector(".tile")).opacity, "1", "not dimmed");
});
t("an untagged image says so in amber, and its name is one clean line", () => {
  const k = row(imgKey(ui.images.find((i) => i.Id === "sha256:dangle")));
  eq(k.querySelector(".tag.none")?.textContent, "untagged", "tag");
  eq(document.querySelectorAll("#content .name .sub-name").length, 0, "no used-by line under names");
});
t("only green boxes can spark, staggered by row", () => {
  eq(document.querySelectorAll("#content .cube .sp").length, document.querySelectorAll("#content .cube.run").length, "a light per lit box");
  eq(document.querySelector("#content .cube.run").style.getPropertyValue("--i"), "0", "first in the list");
});
t("the spark only marks the list when a lit box is on screen", () => {
  spark();
  eq($("content").classList.contains("spark"), true, "lit box: marked");
  $("content").classList.remove("spark");
  ui.containers = ui.containers.filter((c) => c.State !== "running"); render();
  spark();
  eq($("content").classList.contains("spark"), false, "no lit box: nothing");
});
report(R);
