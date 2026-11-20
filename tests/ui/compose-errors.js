// lang: de
// Kiri's own Compose errors come as a key and its values, and the page says
// them in the window's language; Compose's and Docker's own words pass as
// they are, and a key the page doesn't know (or an object's own name) falls
// back to the words sent.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const lastToast = () => [...document.querySelectorAll(".toast")].pop()?.textContent || "";
ui.vm = "running";
kiri.on({ type: "lists", containers: [], images: [], volumes: [], networks: [] });
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, fresh: true, updated: null, services: [], containers: [] }] });
show("compose"); cmpOpen("shop");
t("Discard with nothing to put back: said in German", () => {
  kiri.on({ type: "composerestored", name: "shop", error: "Kiri has nothing to put back", errkey: "nothingToRestore", errargs: {} });
  eq(lastToast().includes("Kiri hat nichts zum Zurücksetzen"), true, lastToast());
});
t("a name clash names the project and its folder, in German", () => {
  kiri.on({ type: "composeopened", error: "A project named shop already runs from /w/other.", errkey: "nameClash", errargs: { name: "shop", dir: "/w/other" } });
  eq([lastToast().includes("Ein Projekt namens shop läuft bereits aus /w/other"), lastToast().includes("A project named")], [true, false], lastToast());
});
t("Docker's own words pass as they are; an unknown key or an object's own name falls back to them", () => {
  kiri.on({ type: "composeaction", name: "shop", error: "Error response from daemon: boom" });
  eq(lastToast().includes("Error response from daemon: boom"), true, lastToast());
  kiri.on({ type: "composeaction", name: "shop", error: "plain words", errkey: "constructor" });
  eq(lastToast().includes("plain words"), true, lastToast());
  kiri.on({ type: "composeaction", name: "shop", error: "other words", errkey: "noSuchKey" });
  eq(lastToast().includes("other words"), true, lastToast());
});
report(R);
