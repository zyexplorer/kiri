// In a container's Terminal tab each reply lands under its own command, whichever
// answers first, and a command that never ends does not hold the next (#338).
kiri.on({ type: "status", version: "0.2.2", app: "Kiri", ours: "kiri", context: "kiri" });
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
show("containers"); openDetail("a1"); setTab("terminal"); render();
const execs = () => sent.filter((s) => s.type === "exec" && s.path !== ui.detail.term.probed);
const type = (cmd) => { $("term-in").value = cmd; $("term-form").requestSubmit(); };
const bodies = () => [...$("term-out").children].map((b) => b.querySelector(".body").textContent);
t("a command typed while another runs is sent too, and each reply lands under its own command", () => {
  type("sleep 5; echo first");
  type("echo second");
  eq(execs().length, 2, "commands sent");
  eq($("term-in").value, "", "box emptied");
  // The second answers first.
  kiri.on({ type: "exec", id: "a1", cmd: execs()[1].path, code: 0, output: "second\n" });
  eq(bodies(), ["running…", "second"], "the early reply");
  kiri.on({ type: "exec", id: "a1", cmd: execs()[0].path, code: 0, output: "first\n" });
  eq(bodies(), ["first", "second"], "outputs");
});
t("a command that never ends does not hold the next one", () => {
  type("clear");
  type("tail -f /var/log/x");
  type("ls");
  eq(execs().length, 4, "ls sent");
  kiri.on({ type: "exec", id: "a1", cmd: execs()[3].path, code: 0, output: "bin\n" });
  eq(bodies(), ["running…", "bin"], "ls answered while tail runs");
});
t("clear lets go of a command that never ends, and its late reply goes nowhere", () => {
  type("psql -U postgres");
  const stuck = execs().pop().path;
  type("clear");
  type("ls");
  eq(execs().pop().path !== stuck, true, "ls sent");
  kiri.on({ type: "exec", id: "a1", cmd: stuck, code: -1, output: "timed out\n" });
  eq(bodies(), ["running…"], "the late reply took the ls block");
});
report(R);
