// An image pulled by digest: the repository without the digest, and the
// digest as a short tag (algorithm and 8 hex) that can't stretch the column.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const D = "sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5", REF = "kindest/node@" + D;
kiri.on({ type: "lists", volumes: [], networks: [],
  containers: [{ Id: "k1", Names: ["/kind"], Image: REF, ImageID: "sha256:kind", State: "running", Status: "Up 1 hour", Created: 1, Ports: [], Labels: {} }],
  images: [{ Id: "sha256:kind", RepoTags: [REF], Size: 5e8, Created: 1 }, { Id: "sha256:ng", RepoTags: ["nginx:" + "x".repeat(80)], Size: 5e7, Created: 1 }] });
show("images");
const row = (k) => document.querySelector(`#content tr[data-key="${k}"]`);
t("split takes a digest reference apart at the @", () => {
  eq(split(REF), ["kindest/node", D], "digest");
  eq(split("kindest/node:v1.29@" + D), ["kindest/node", D], "tag and digest");
  eq(split("localhost:5000/app:1"), ["localhost:5000/app", "1"], "a host's port isn't a tag");
});
t("the row: Name is the last path segment, Repository the full path, digest tag short", () => {
  const r = row(REF);
  eq(r.querySelector(".name .link").textContent, "node", "name");
  eq(r.cells[r.querySelector("td.ck") ? 2 : 1].textContent.trim(), "kindest/node", "repository");
  const tag = r.querySelector(".tag");
  eq([tag.textContent, tag.title, tag.classList.contains("mono")], ["sha256:a1ed56cf", D, true], "tag");
});
t("no tag stretches its column", () => {
  const tag = row("nginx:" + "x".repeat(80)).querySelector(".tag"), cs = getComputedStyle(tag);
  eq([cs.maxWidth, cs.textOverflow, tag.title], ["min(160px, 100%)", "ellipsis", "x".repeat(80)], "capped, whole in its title");
  eq(tag.getBoundingClientRect().width <= 160, true, "fits");
});
t("a container's image line and the image's page say it the same short way", () => {
  show("containers");
  const c = document.querySelector('#content tr[data-key="k1"] .name .sub-name');
  eq([c.textContent, c.title], ["kindest/node@sha256:a1ed56cf", REF], "image line");
  show("images");
  openImage(REF);
  const crumb = $("title").querySelector(".crumb > span:last-child");
  eq([crumb.textContent, crumb.querySelector(".dg").title], ["kindest/node @ sha256:a1ed56cf", D], "crumb");
  const h = $("content").querySelector(".dhead .who b");
  eq([h.textContent, h.querySelector(".dg .mono").textContent, h.querySelector("[data-copy]").dataset.copy], ["kindest/node @ sha256:a1ed56cf", "sha256:a1ed56cf", D], "heading, with a copy button for the digest");
  eq($("content").querySelector(".dhead .who .ttl > [data-copy]")?.dataset.copy, REF, "copy button for the whole image name");
  ui.imgd = null;
});
t("the image's page names two of its users, then how many more", () => {
  const C = (n) => ({ Id: n, Names: ["/" + n], Image: REF, ImageID: "sha256:kind", State: "exited", Status: "Exited (0)", Created: 1, Ports: [], Labels: {} });
  ui.containers.push(...["noble_ramanujan", "zesty_kalam", "eager_x", "brave_y"].map(C));
  openImage(REF);
  const u = [...$("content").querySelectorAll(".dhead .who .muted span[title]")].find((s) => /used by/.test(s.textContent));
  eq(u.textContent, "used by kind, noble_ramanujan +3 more", "two, then +3 more");
  eq(u.title, "kind, noble_ramanujan, zesty_kalam, eager_x, brave_y", "all of them on hover");
  ui.imgd = null;
});
t("a commit-hash tag shows its first 12, the whole one on hover; short tags stay whole", () => {
  const sha = "6dae942a51a4474c0b2f8e3d1a9b7c6d5e4f3a2b";
  const box = document.createElement("div");
  box.innerHTML = tagPill(sha) + tagPill("3.22") + tagPill("dev-amd64");
  const [h, v, d] = box.querySelectorAll(".tag");
  eq([h.textContent, h.title, h.classList.contains("mono")], ["6dae942a51a4", sha, true], "hash tag");
  eq([v.textContent, d.textContent], ["3.22", "dev-amd64"], "ordinary tags");
});
t("clicking a tag copies the whole tag and doesn't open the image", () => {
  ui.imgd = null; show("images");
  const n = sent.length;
  const tag = row(REF).querySelector(".tag");
  tag.click();
  eq(sent.slice(n).map((m) => [m.type, m.name]), [["copy", D]], "copied the whole digest");
  eq(ui.imgd, null, "image page not opened");
});
report(R);
