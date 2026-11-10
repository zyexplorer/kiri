// Volumes: Mount shows host + VM paths, unused tab, and Remove unused.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + (e && e.message || e)); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error((m || "eq") + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const V = (name, mp, opts, ref) => ({
  Name: name, Driver: "local", Mountpoint: mp, CreatedAt: "2024-01-01T00:00:00Z",
  Options: opts || {}, UsageData: { Size: -1, RefCount: ref },
});
const C = (id, vol) => ({
  Id: id, Names: ["/" + id], Image: "alpine", ImageID: "sha256:x", State: "running",
  Status: "Up 1 hour", Created: 1, Ports: [], Labels: {},
  Mounts: [{ Type: "volume", Name: vol, Destination: "/data" }],
});

window.KIRI_WSL = "kiri";
kiri.on({ type: "status", version: "0", dev: true, share: true, app: "Kiri Dev", files: "/Users/me/Kiri Dev" });
kiri.on({
  type: "lists",
  containers: [C("web", "data")],
  images: [],
  volumes: [
    V("data", "/var/lib/kiri/volumes/data/_data", {}, 1),
    V("cache", "/var/lib/kiri/volumes/cache/_data", {}, 0),
    V("src", "/Users/me/src", { type: "none", device: "/Users/me/src", o: "bind" }, 0),
    V("wslbind", "/mnt/c/Users/me/proj", { type: "none", device: "/mnt/c/Users/me/proj", o: "bind" }, 0),
  ],
  networks: [],
});

show("volumes");
const row = (k) => document.querySelector(`#content tr[data-key="${k}"]`);
const mountText = (k) => [...row(k).querySelectorAll(".vol-mount .path")].map((el) => el.textContent);

t("Volumes list has Mount and Status columns", () => {
  eq([...document.querySelectorAll("#content thead th:not(.ck)")].map((th) => th.textContent.trim()).slice(0, 3),
    ["Name", "Mount", "Status"], "head");
});

t("named volume in use shows Host via Finder mount and VM path", () => {
  if (WIN || LINUX) {
    eq(mountText("data")[0].includes("data"), true, "guest path");
    return;
  }
  eq(mountText("data"), ["/Users/me/Kiri Dev/web/data", "/var/lib/kiri/volumes/data/_data"], "host + guest");
  eq(row("data").querySelector(".moss")?.textContent.includes("1"), true, "in use");
  eq(row("data").querySelector(".vol-lbl")?.textContent, "Host:", "host label");
});

t("unused named volume still shows Host and VM on Mac", () => {
  if (WIN || LINUX) return;
  eq(mountText("cache"), ["/Users/me/Kiri Dev/volumes/cache", "/var/lib/kiri/volumes/cache/_data"], "host + guest");
  eq(row("cache").querySelector(".off")?.textContent, "not in use", "status");
});

t("Mac bind volume shows Host and VM paths", () => {
  if (WIN || LINUX) return;
  eq(mountText("src"), ["/Users/me/src", "/mnt/mac/Users/me/src"], "host + guest");
  eq(volHostPath("/mnt/mac/Users/me/src"), "/Users/me/src", "strip /mnt/mac");
  eq(volGuestPath({ Options: { device: "/Users/me/src" }, Mountpoint: "/Users/me/src" }),
    "/mnt/mac/Users/me/src", "guest under /mnt/mac");
});

t("Windows bind volume maps /mnt/c to a drive letter", () => {
  if (!WIN) return;
  eq(volHostPath("/mnt/c/Users/me/proj"), "C:\\Users\\me\\proj", "drive");
  eq(volHostPath("/var/lib/kiri/volumes/data/_data"),
    "\\\\wsl$\\kiri\\var\\lib\\kiri\\volumes\\data\\_data", "unc");
});

t("Unused tab and Remove unused target only unused volumes", () => {
  const unused = document.querySelector('[data-ltab="unused"]');
  if (!unused) throw new Error("no Unused tab");
  unused.click();
  const keys = [...document.querySelectorAll("#content tr[data-key]")].map((tr) => tr.dataset.key).sort();
  eq(keys.includes("cache"), true, "cache listed");
  eq(keys.includes("data"), false, "in-use hidden");
  const junk = document.querySelector("[data-junk]");
  if (!junk) throw new Error("no Remove unused");
  eq(/unused/i.test(junk.textContent), true, "junk label");
});

report(R);
