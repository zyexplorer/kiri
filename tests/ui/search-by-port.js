// parity: macos windows linux
// window: 1440x900
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const C = (id, image, ports, project) => ({ Id: id, Names: ["/" + id], Image: image, State: "running", Status: "Up 1 hour",
  Ports: ports, Labels: project ? { "com.docker.compose.project": project, "com.docker.compose.service": id } : {} });
const webPorts = [{ IP: "127.0.0.1", PrivatePort: 443, PublicPort: 18443, Type: "tcp" }];
const dbPorts = [{ PrivatePort: 5432, Type: "tcp" }];
ui.vm = "running";
kiri.on({ type: "lists", containers: [C("gateway", "nginx", webPorts), C("storage", "postgres", dbPorts),
  C("proxy", "nginx", webPorts, "shop"), C("db", "postgres", dbPorts, "shop")], images: [], volumes: [], networks: [] });
const rowsFor = (query) => {
  ui.filter = query;
  render();
  return [...document.querySelectorAll("#content tbody tr")].map((r) => r.dataset.key || r.dataset.group);
};
show("containers");
t("a host port finds only its container", () => eq(rowsFor("18443"), ["gateway"], "rows"));
t("a private-only port matches a substring", () => eq(rowsFor("543"), ["storage"], "rows"));
t("name and image still match case-insensitive substrings", () => {
  eq(rowsFor("ATEW"), ["gateway"], "name");
  eq(rowsFor("STGR"), ["storage"], "image");
});
t("an unmatched port shows the existing empty state", () => {
  eq(rowsFor("65535"), [], "rows");
  eq(document.querySelector("#content .empty p")?.textContent, "Nothing matches “65535”.", "empty state");
});
kiri.on({ type: "composeprojects", logos: {}, projects: [{ name: "shop", services: [] }] });
show("compose");
t("a port shows the project and only its matching member", () => {
  eq(rowsFor("5432"), ["shop", "db"], "container port");
  eq(rowsFor("18443"), ["shop", "proxy"], "host port");
});
report(R);
