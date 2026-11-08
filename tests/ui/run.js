// Run on an image: a yellow spinner while it starts, then the play button
// again, with no check in between. (env [] skips the name popover.)
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const runNow = () => runBtn("nginx:alpine");
runImage("nginx:alpine", []);
t("starting shows the spinner, not a check", () => {
  if (!/rolling/.test(runNow()) || /Started/.test(runNow())) throw new Error(runNow());
});
kiri.on({ type: "ran", name: "nginx:alpine" });
t("once up, it's the play button again", () => {
  if (/rolling|Started/.test(runNow()) || !/aria-label="Run"/.test(runNow())) throw new Error(runNow());
});
report(R);
