// parity: macos windows linux
// An empty Compose section, and Containers when every container is in a
// project, say so each time they are shown: after another section's list has
// been in the window, and after the only project is removed. The message is
// not put in again while it is still there (a refresh every 2 s would restart
// the spinner).
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const content = () => document.querySelector("#content");
const shows = () => (content().querySelector(".empty h3")?.textContent || (content().querySelector("table") ? "a table" : content().textContent)).replace(/\s+/g, " ").trim();
const NONE = "No Compose projects yet";
ui.vm = "running";
const img = [{ Id: "sha256:1", RepoTags: ["nginx:alpine"], Size: 1e7, Created: 1 }];
const C = (svc, id) => ({ Id: id, Names: [`/shop-${svc}-1`], Image: "nginx:alpine", State: "running", Status: "Up 1 minute", Created: 1, Ports: [], Labels: { "com.docker.compose.project": "shop", "com.docker.compose.service": svc } });
const P = { name: "shop", dir: "/w/shop", files: ["/w/shop/compose.yaml"], readable: true, updated: 1,
  services: [{ name: "web", image: "nginx:alpine", build: "", ports: [], volumes: [], depends: [], networks: ["default"], kinds: [] }],
  containers: [{ service: "web", id: "w1", name: "shop-web-1", image: "nginx:alpine", state: "running", status: "Up" }] };
const lists = (containers) => kiri.on({ type: "lists", containers, images: img, volumes: [], networks: [] });
const projects = (ps, ask) => kiri.on({ type: "composeprojects", ask, logos: {}, projects: ps });

lists([]);
show("compose"); projects([], cmp.askN);
t("no project: Compose says so", () => eq(shows(), NONE, "compose"));
t("a refresh leaves the message as it is", () => { const was = content().firstChild; render(); render(); eq(content().firstChild === was, true, "same node"); });
// Something wrapped around what was put there (a menu given its own button) is still the same message.
t("a wrapper around the message's first part is not a reason to put it in again", () => {
  const first = content().firstChild, wrap = document.createElement("div");
  first.replaceWith(wrap); wrap.append(first);
  render();
  eq([content().firstChild === wrap, wrap.contains(first)], [true, true], "left as it was");
  wrap.replaceWith(first);
});
show("images");
t("Images shows its list", () => eq(shows(), "a table", "images"));
show("compose"); projects([], cmp.askN);
t("back on Compose: its own message, not the Images list", () => eq(shows(), NONE, "compose after images"));

// A project appears (its file was opened and applied), then it is removed: the only one.
lists([C("web", "w1")]); projects([P]);
t("the project is listed", () => eq([shows(), !!content().querySelector('tr.group')], ["a table", true], "listed"));
projectAction("shop", "remove");
lists([]); kiri.on({ type: "composeaction", name: "shop", removed: true }); projects([]);
t("the only project removed: the empty message again, no row left", () => eq([shows(), content().querySelectorAll("tr").length], [NONE, 0], "after remove"));

// Containers, when every container is in a project, after another section.
lists([C("web", "w1")]); projects([P]);
show("containers");
const all = shows();
t("Containers says its only container is in a project", () => eq(/Compose/.test(all) && !content().querySelector("table"), true, "containers: " + all));
show("images"); show("containers");
t("back on Containers: the same message, not the Images list", () => eq(shows(), all, "containers after images"));
report(R);
