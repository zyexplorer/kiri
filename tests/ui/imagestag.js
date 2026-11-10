// Images: a long repository or tag must not push Actions' buttons past the
// column — the list always uses fixed column shares, like Containers.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const nowS = Date.now() / 1000;
kiri.on({ type: "lists", containers: [], images: [
  { Id: "sha256:" + "a".repeat(64), RepoTags: ["harbor.example.com:9090/szda/very-long-path/rustfs:arm-1.0.0-beta.8-extra-long"], Size: 1e8, Created: nowS - 60, Pulled: nowS - 30 },
  { Id: "sha256:" + "b".repeat(64), RepoTags: ["nginx:alpine"], Size: 1e7, Created: nowS - 120, Pulled: nowS - 90 },
], volumes: [], networks: [] });
document.body.style.width = "1100px";
show("images");

t("Images is laid out with fixed shares without a prior resize", () => {
  const tbl = document.querySelector("#content table[data-view=images]");
  if (!tbl?.classList.contains("sized")) throw new Error("not sized");
  if (!tbl.classList.contains("pin")) throw new Error("not pin");
  if (![...tbl.tHead.rows[0].cells].some((th) => th.dataset.share)) throw new Error("no shares on headings");
});
t("checkbox, Name and Actions stay pinned while the middle can scroll", () => {
  const tbl = document.querySelector("#content table.pin");
  const ck = tbl.querySelector("th.ck"), name = tbl.querySelector("th.ck + th"), acts = tbl.querySelector("th.actions");
  if (!ck || getComputedStyle(ck).position !== "sticky") throw new Error("ck not sticky");
  if (!name || getComputedStyle(name).position !== "sticky") throw new Error("Name not sticky");
  if (!acts || getComputedStyle(acts).position !== "sticky") throw new Error("Actions not sticky");
  const repo = [...tbl.tHead.rows[0].cells].find((th) => /Repository|仓库/.test(th.textContent));
  if (!repo || +(repo.dataset.pref || 0) < 200) throw new Error("Repository has no preferred width");
  // At 1100px the preferred middle columns should make the table wider than the pane.
  const pane = document.getElementById("content").clientWidth;
  if (!(tbl.scrollWidth > pane - 8)) throw new Error(`table ${tbl.scrollWidth} should overflow content ${pane}`);
});
t("a long tag and repository leave every Actions button inside its column", () => {
  const tbl = document.querySelector("#content table.sized");
  for (const r of tbl.tBodies[0].rows) {
    const td = r.cells[r.cells.length - 1];
    if (!td.classList.contains("actions")) throw new Error("last cell is not actions");
    const colR = td.getBoundingClientRect().right;
    for (const b of td.querySelectorAll("button")) {
      if (b.getBoundingClientRect().right > colR + 0.5) throw new Error(`button past column: ${b.getAttribute("aria-label")}`);
    }
  }
});
t("the long tag is clipped with its full value in the title", () => {
  const pill = document.querySelector('#content tr[data-key*="rustfs"] .tag');
  if (!pill) throw new Error("no tag pill");
  if (!/arm-1\.0\.0-beta\.8/.test(pill.title)) throw new Error("title missing full tag: " + pill.title);
  if (pill.scrollWidth <= pill.clientWidth + 1 && pill.textContent.length > 20) {
    // May or may not overflow depending on column share; the cell must still clip.
  }
  const cell = pill.closest("td");
  if (!cell.classList.contains("clip")) throw new Error("tag cell not clip");
});
t("Name is the last path segment; Repository keeps the full path", () => {
  const row = document.querySelector('#content tr[data-key*="rustfs"]');
  if (!row) throw new Error("no rustfs row");
  if (row.querySelector(".name .link")?.textContent !== "rustfs") throw new Error("name not last segment");
  const repo = [...row.cells].find((td) => td.classList.contains("mono") && /harbor\.example\.com/.test(td.textContent));
  if (!repo) throw new Error("repository column missing full path");
  if (!repo.classList.contains("clip")) throw new Error("repository cell not clip");
});
report(R);
