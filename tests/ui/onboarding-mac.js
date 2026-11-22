// The Mac (#250): a first-launch step that failed waits in the same setup
// checklist, over a window that works: its error as the log has it, Try
// again, Skip for what Kiri works without, Show log, and Continue anyway.
const out = [];
const ok = (cond, what) => out.push(`${cond ? "PASS" : "FAIL"} ${what}`);
const row = (k) => document.querySelector(`#onb [data-check="${k}"]`);
const obtn = (sel) => document.querySelector(`#onb ${sel}`);

kiri.on({ type: "onboarding", os: "mac", ready: true, complete: false, skipped: [],
  checks: { context: "ok", commands: "ok", login: "no" },
  errors: { login: "starting the engine at login: launchctl could not load ~/Library/LaunchAgents/dev.kiri.daemon.plist" } });
ok(document.getElementById("onb"), "the checklist shows");
ok(row("context") && row("commands") && row("login") && !row("access"), "the Mac's steps, not Linux's");
ok(row("login").textContent.includes("runs from the app for now"), "says the engine runs from the app meanwhile");
ok(row("login").querySelector(".e")?.textContent.includes("launchctl could not load"), "shows the step's error");
ok(!row("commands").querySelector(".e"), "no error on a step that worked");
ok(obtn('[data-onbfix="login"]')?.textContent.includes("Try again"), "Try again on the failed step");
ok(obtn('[data-onbskip="login"]'), "starting at login can be skipped");
ok(obtn('[data-onb="log"]'), "Show log");
{
  const mid = (el) => { const r = el.getBoundingClientRect(); return r.top + r.height / 2; };
  const skip = obtn('[data-onbskip="login"]'), again = obtn('[data-onbfix="login"]');
  ok(Math.abs(mid(skip) - mid(again)) <= 2, `Skip sits on the button's line under an error (${Math.round(mid(skip) - mid(again))}px apart)`);
}
ok(!obtn('[data-onb="open"]').disabled && obtn('[data-onb="open"]').textContent.includes("Continue anyway"), "Continue anyway: the window works without it");
ok(document.querySelector("#onb .onb-hd").textContent.includes("One step didn't work"), "says one step didn't work");

obtn('[data-onbfix="login"]').click();
ok(sent.at(-1)?.type === "onbfix" && sent.at(-1).name === "login", "Try again asks the app to redo that step");
ok(row("login").querySelector(".st.wait"), "and the step shows it's being tried");
obtn('[data-onb="log"]').click();
ok(sent.at(-1)?.type === "diagfolder", "Show log opens the logs");

// docker's context failed too: that one can't be skipped.
kiri.on({ type: "onboarding", os: "mac", ready: false, complete: false, skipped: [],
  checks: { context: "no", commands: "ok", login: "no" },
  errors: { context: "adding the kiri docker context: ~/.docker: File exists (os error 17)", login: "starting the engine at login: launchctl could not load" } });
ok(!obtn('[data-onbskip="context"]'), "docker's context can't be skipped");
ok(obtn('[data-onbfix="all"]'), "Fix all with two left");
ok(document.querySelector("#onb .onb-hd").textContent.includes("Some steps didn't work"), "says some steps didn't work");

// Tried again and it worked: Kiri is set up.
kiri.on({ type: "onboarding", os: "mac", ready: true, complete: true, skipped: [], checks: { context: "ok", commands: "ok", login: "ok" }, errors: {} });
ok(!obtn('[data-onb="log"]'), "no Show log once nothing failed");
ok(obtn('[data-onb="open"]').textContent.includes("Open Kiri"), "Open Kiri when it's all done");
obtn('[data-onb="open"]').click();
ok(!document.getElementById("onb") && sent.at(-1)?.type === "onbopen", "and it closes");

report(out);
