// Remove: unused images confirm then rmi; in-use images (and running
// containers) show a locked trash like kiri-k8s — no dialog after a click.
// usersOf must still see a user when ImageID is missing (tag fallback).
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const C = (id, image, imageId, state, code) => ({
  Id: id, Names: ["/" + id], Image: image, ImageID: imageId, State: state,
  Status: state === "running" ? "Up 1 hour" : state === "paused" ? "Up 1 hour (Paused)" : `Exited (${code ?? 0})`,
  Created: 1, Ports: [], Labels: {},
});
const IMG = (id, tag) => ({ Id: id, RepoTags: [tag], Size: 5e7, Created: 1 });
const dlg = () => document.querySelector(".scrim .dialog");
const clearDlg = () => document.querySelectorAll(".scrim").forEach((x) => x.remove());
const trash = (key) => document.querySelector(`#content [data-key="${key}"] .gacts .btn.ic[title], #content tr[data-key="${key}"] .acts-row .btn.ic[aria-label="Remove"], #content tr[data-key="${key}"] .acts-row .btn.ic.locked, #content [data-key="${key}"] .btn.ic.locked, #content [data-key="${key}"] [data-rmi], #content tr[data-key="${key}"] [data-rmi]`);

kiri.on({
  type: "lists", volumes: [], networks: [],
  containers: [
    C("run1", "demo/app:1", "sha256:aaaa111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "running"),
    C("pause1", "demo/app:1", "sha256:aaaa111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "paused"),
    C("stop1", "demo/old:1", "sha256:bbbb111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "exited", 0),
    C("tag1", "demo/tag:1", "", "running"),
    C("short1", "demo/short:1", "ccccccccdddd", "running"),
    C("idle1", "demo/free:1", "sha256:cccc111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "exited", 0),
  ],
  images: [
    IMG("sha256:aaaa111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "demo/app:1"),
    IMG("sha256:bbbb111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "demo/old:1"),
    IMG("sha256:cccc111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "demo/free:1"),
    IMG("sha256:dddd111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "demo/tag:1"),
    IMG("sha256:ccccccccdddd111122223333444455556666777788889999aaaabbbbccccdddd", "demo/short:1"),
    IMG("sha256:eeee111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "demo/lonely:1"),
  ],
});
show("images");
sent.length = 0;

t("usersOf matches by ImageID", () => {
  const img = ui.images.find((i) => i.RepoTags[0] === "demo/app:1");
  eq(usersOf(img).map((c) => c.Id).sort(), ["pause1", "run1"], "running+paused");
});
t("usersOf falls back to Image tag when ImageID is empty", () => {
  const img = ui.images.find((i) => i.RepoTags[0] === "demo/tag:1");
  eq(usersOf(img).map((c) => c.Id), ["tag1"], "by tag");
});
t("usersOf matches a short ImageID to the full digest", () => {
  const img = ui.images.find((i) => i.RepoTags[0] === "demo/short:1");
  eq(usersOf(img).map((c) => c.Id), ["short1"], "short hash");
});

function imgTrash(key) {
  const card = document.querySelector(`#content [data-key="${CSS.escape(key)}"]`);
  const btns = [...(card?.querySelectorAll(".btn.ic") || [])];
  return btns.find((b) => b.querySelector("path") && (b.classList.contains("danger") || b.classList.contains("locked") || b.dataset.rmi || b.disabled));
}
t("in-use image: trash locked, no data-rmi", () => {
  setListLayout("images", "grid");
  const b = imgTrash("demo/app:1");
  eq([!!b, b?.disabled, b?.classList.contains("locked"), !!b?.dataset.rmi], [true, true, true, false], "locked");
});
t("image with only stopped users: still locked (docker rmi)", () => {
  const b = imgTrash("demo/old:1");
  eq([b?.disabled, b?.classList.contains("locked"), !!b?.dataset.rmi], [true, true, false], "locked");
});
t("unused image: trash enabled with data-rmi", () => {
  const b = imgTrash("demo/lonely:1");
  eq([b?.disabled, !!b?.dataset.rmi, b?.classList.contains("danger")], [false, true, true], "active");
});
t("Remove on an in-use image does nothing (button disabled path)", () => {
  clearDlg(); sent.length = 0;
  removeImage("demo/app:1");
  eq([!!dlg(), sent.length], [false, 0], "no dialog, no send");
});
t("Remove unused: confirm then rmi", () => {
  clearDlg(); sent.length = 0;
  removeImage("demo/lonely:1");
  const d = dlg();
  eq(!!d, true, "confirm");
  d.querySelector("[data-go]").click();
  eq(sent.map((s) => [s.type, s.name]), [["rmi", "demo/lonely:1"]], "plain rmi");
});
t("bulk Delete leaves images that have users", () => {
  const L = LISTS.images;
  const del = L.bulk.find(([label]) => label === "Delete");
  const allowed = L.all().filter(del[3]).map(imgKey);
  eq(allowed.includes("demo/lonely:1"), true, "unused ok");
  eq(allowed.some((k) => ["demo/app:1", "demo/old:1", "demo/tag:1", "demo/short:1", "demo/free:1"].includes(k)), false, "in-use left out");
});

// Containers: running trash locked; stopped trash active.
kiri.on({
  type: "lists", volumes: [], networks: [],
  containers: [
    C("run1", "demo/app:1", "sha256:aaaa111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "running"),
    C("stop1", "demo/old:1", "sha256:bbbb111122223333444455556666777788889999aaaabbbbccccddddeeeeffff", "exited", 0),
    { Id: "k8s1", Names: ["/kiri-k8s"], Image: "rancher/k3s", ImageID: "sha256:k8s", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: { "dev.kiri.k8s": "1" } },
  ],
  images: [],
});
show("containers");
setListLayout("containers", "grid");

function ctrTrash(id) {
  const card = document.querySelector(`#content [data-key="${id}"]`);
  const btns = [...(card?.querySelectorAll(".gacts .btn.ic") || [])];
  return btns.find((b) => b.classList.contains("danger") || b.classList.contains("locked"));
}
t("running container: trash locked", () => {
  const b = ctrTrash("run1");
  eq([b?.disabled, b?.classList.contains("locked"), !!b?.dataset.c], [true, true, false], "locked");
});
t("stopped container: trash active", () => {
  const b = ctrTrash("stop1");
  eq([b?.disabled, b?.dataset.c, b?.dataset.id], [false, "remove", "stop1"], "active");
});
t("kiri-k8s: trash locked (settings)", () => {
  const b = ctrTrash("k8s1");
  eq([b?.classList.contains("locked"), b?.hasAttribute("data-ksettings")], [true, true], "k8s locked");
});
t("bulk Delete leaves running containers", () => {
  const L = LISTS.containers;
  const del = L.bulk.find(([label]) => label === "Delete");
  const allowed = L.all().filter(del[3]).map((c) => c.Id);
  eq(allowed, ["stop1"], "only stopped");
});
t("containerAction remove on a running container is a no-op", () => {
  clearDlg(); sent.length = 0;
  containerAction("remove", "run1");
  eq([!!dlg(), sent.length], [false, 0], "refused");
});

report(R);
