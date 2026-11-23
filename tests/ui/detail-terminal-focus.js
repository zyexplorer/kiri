// parity: macos windows linux
const results = [];
const test = (name, fn) => { try { fn(); results.push("PASS " + name); } catch (e) { results.push("FAIL " + name + ": " + e.message); } };
const check = (ok, message) => { if (!ok) throw new Error(message); };
const container = { Id: "redis", Names: ["/redis"], Image: "redis:alpine", State: "running", Ports: [] };
const lists = (containers) => kiri.on({ type: "lists", containers, images: [], volumes: [], networks: [] });

lists([container]);
show("containers");
openDetail(container.Id);
setTab("terminal");
setTimeout(() => {
  const input = document.getElementById("term-in");
  input.focus();
  input.value = "echo hello";
  input.setSelectionRange(5, 10);
  let blurred = false;
  input.addEventListener("blur", () => { blurred = true; });
  test("refreshing the list preserves terminal focus and the selection", () => {
    check(document.activeElement === input, "the prompt did not receive focus before the refresh");
    lists([{ ...container, Image: "redis:8" }]);
    check(document.querySelector(".dfacts").textContent.includes("redis:8"), "the details above the prompt did not refresh");
    check(document.activeElement === input, "the list refresh dropped prompt focus");
    check(!blurred, "the prompt blurred during the refresh");
    check(input.value === "echo hello" && input.selectionStart === 5 && input.selectionEnd === 10, "the draft command or selection changed");
  });
  test("a list refresh leaves focus outside the Terminal where it was", () => {
    const search = document.getElementById("gq");
    search.focus();
    lists([container]);
    check(document.activeElement === search, "the prompt took focus");
  });
  test("a stopped container has no active command prompt", () => {
    input.focus();
    lists([{ ...container, State: "exited" }]);
    check(!input.isConnected && /Start the container/.test(document.getElementById("dbody").textContent), "the prompt is still available");
  });
  report(results);
}, 20);
