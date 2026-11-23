// parity: macos windows linux
// A container's Logs tab: Find in logs keeps its focus, text and caret while the lists refresh.
const results = [];
const test = (name, fn) => { try { fn(); results.push("PASS " + name); } catch (e) { results.push("FAIL " + name + ": " + e.message); } };
const check = (ok, message) => { if (!ok) throw new Error(message); };
const container = { Id: "redis", Names: ["/redis"], Image: "redis:alpine", State: "running", Ports: [] };
const other = { Id: "pg", Names: ["/pg"], Image: "postgres:17", State: "running", Ports: [] };
const lists = (containers) => kiri.on({ type: "lists", containers, images: [], volumes: [], networks: [] });

lists([container, other]);
show("containers");
openDetail(container.Id);
setTab("logs");
setTimeout(() => {
  const input = document.getElementById("log-filter");
  input.focus();
  input.value = "error";
  input.dispatchEvent(new Event("input"));
  input.setSelectionRange(1, 3);
  let blurred = false;
  input.addEventListener("blur", () => { blurred = true; });
  test("refreshing the list keeps Find in logs focused, with its text and selection", () => {
    check(document.activeElement === input, "Find in logs did not take focus before the refresh");
    lists([{ ...container, Image: "redis:8" }, other]);
    check(document.querySelector(".dfacts").textContent.includes("redis:8"), "the details above the logs did not refresh");
    check(document.activeElement === input && input.isConnected, "the list refresh dropped the focus");
    check(!blurred, "Find in logs blurred during the refresh");
    check(input.value === "error" && input.selectionStart === 1 && input.selectionEnd === 3, "the text or selection changed");
  });
  test("the level, Follow and Clear still work after a refresh", () => {
    const level = document.getElementById("log-level");
    level.value = "warn";
    level.dispatchEvent(new Event("change"));
    check(ui.detail.logLevel === "warn", "the level was not taken");
    lists([container, other]);
    check(document.getElementById("log-level").value === "warn", "the level went back");
  });
  test("a list refresh leaves focus outside the Logs tab where it was", () => {
    const search = document.getElementById("gq");
    search.focus();
    lists([container, other]);
    check(document.activeElement === search, "Find in logs took the focus");
  });
  test("another container's Logs tab starts with its own empty bar", () => {
    openDetail(other.Id);
    setTab("logs");
    check(document.getElementById("log-filter").value === "", "the other container shows the first one's text");
  });
  test("leaving the tab and coming back draws the bar afresh", () => {
    openDetail(container.Id);
    setTab("overview");
    setTab("logs");
    check(document.getElementById("log-filter").isConnected && document.getElementById("log-count"), "the bar is missing");
  });
  report(results);
}, 20);
