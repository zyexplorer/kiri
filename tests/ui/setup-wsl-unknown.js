// page: setup
// window: 1024x700
// Windows' first start: the setup window opens before kiri-win has said
// whether WSL is on this PC (that check runs wsl.exe and can take a while).
// Until it says, the window is the frame only: no "Welcome to Kiri / Install
// WSL" flashing by on a PC that has WSL. Then WSL there: the language, then
// Choose a Linux; WSL not answering: Choose a Linux with why; WSL missing:
// Install WSL.
(async () => {
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
// The page draws a host state on the next frame (a task here: ui-test.py).
const frame = () => new Promise((r) => setTimeout(() => setTimeout(r, 0), 0));
const main = () => document.getElementById("main");
const text = () => document.querySelector(".app").textContent;
const wslAsk = () => main().querySelector('[data-cmd="setup-install-wsl"]') || /Install WSL|Welcome to Kiri|doesn't have yet/.test(text());
const state = (more) => kiri.on({ type: "setup", sizes: { wsl: 367575040, ubuntu: 418495746, kiri: 9000000 }, existingUbuntu: false, otherDocker: false,
  makeDefault: true, buildKiri: true, autostart: true, progress: null, tasks: [], log: [], error: null, ...more });

// The page has loaded and asked; the host hasn't answered yet.
await frame();
t("before the host's answer: no Welcome or Install WSL", () => {
  if (wslAsk()) throw new Error("shows: " + text().replace(/\s+/g, " ").trim().slice(0, 120));
});
t("before the host's answer: no step is marked current", () => {
  if (document.querySelector('#steps [aria-current="step"]')) throw new Error("current: " + document.querySelector('#steps [aria-current="step"]').textContent);
});
// Still nothing, a moment later (the real check takes about 0.4 s).
await new Promise((r) => setTimeout(r, 50));
await frame();
t("still waiting: still no Install WSL", () => { if (wslAsk()) throw new Error("shows Install WSL"); });

// WSL is on this PC, a first start: the language, then Choose a Linux.
state({ step: "choose", askLang: true });
await frame();
t("WSL there, first start: the language step", () => {
  if (!document.getElementById("lang-list")) throw new Error("not the language step: " + main().textContent.slice(0, 80));
  if (wslAsk()) throw new Error("shows Install WSL");
});
t("WSL there: the rail has WSL done", () => {
  const first = document.querySelector("#steps li");
  if (!first || first.className !== "done") throw new Error("WSL row: " + (first && first.className));
});
document.getElementById("lang-go").click();
await frame();
t("then Choose a Linux, never Install WSL", () => {
  if (!document.getElementById("chooseBtn")) throw new Error("not Choose: " + main().textContent.slice(0, 80));
  if (wslAsk()) throw new Error("shows Install WSL");
});

// WSL there but not answering (wsl.exe killed at its limit): Choose a Linux,
// saying why, never Install WSL.
state({ step: "choose", askLang: false, error: "WSL did not finish in 15 s" });
await frame();
t("WSL not answering: Choose a Linux with why, never Install WSL", () => {
  if (!document.getElementById("chooseBtn")) throw new Error("not Choose: " + main().textContent.slice(0, 80));
  const err = main().querySelector("[data-slot=error] .err-t");
  if (!err || err.textContent !== "WSL did not finish in 15 s") throw new Error("error: " + (err && err.textContent));
  if (wslAsk()) throw new Error("shows Install WSL");
});

// WSL missing: Install WSL.
state({ step: "welcome", askLang: false });
await frame();
t("WSL missing: Welcome with Install WSL", () => {
  const b = main().querySelector('[data-cmd="setup-install-wsl"]');
  if (!b || b.textContent !== "Install WSL") throw new Error("no Install WSL: " + main().textContent.slice(0, 80));
  if (main().querySelector("h1").textContent !== "Welcome to Kiri") throw new Error(main().querySelector("h1").textContent);
});

report([...R]);
})().catch((e) => report(["FAIL " + e.message + " " + e.stack]));
