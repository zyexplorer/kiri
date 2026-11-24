// What Kiri's window costs, beside its version in the status bar.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const usageEv = () => kiri.on({ type: "usage", mem: 382 * 1e6, cpu: 1.24, cores: 10, parts: [
  { name: "Window", mem: 35 * 1e6, cpu: 0.2, n: 1 },
  { name: "Helpers", mem: 100 * 1e6, cpu: 0, n: 9 },
  { name: "Graphics", mem: 250 * 1e6, cpu: 0.9, n: 1 },
  { name: "Page", mem: 97 * 1e6, cpu: 0.14, n: 1 },
] });
t("the window asks what it costs, and shows it beside the version", () => {
  prefs.lang = "en";
  if (typeof paintChrome === "function") paintChrome();
  usageEv();
  const el = document.querySelector(".statusbar .use");
  if (!/382(\.0)? MB · 1\.2% CPU/.test(el?.textContent || "")) throw new Error(el?.textContent);
  if (!/Graphics: 250(\.0)? MB/.test(el.title) || !/not counting the Linux VM/.test(el.title)) throw new Error(el.title);
  if (!/Helpers \(9 processes\)/.test(el.title)) throw new Error(el.title);
});
t("usage tip is Chinese when the window is", () => {
  prefs.lang = "zh";
  if (typeof paintChrome === "function") paintChrome();
  usageEv();
  const el = document.querySelector(".statusbar .use");
  const tip = el?.title || "";
  // \u escapes keep this file ASCII so Windows' default open() can read it.
  if (!tip.includes("\u5360\u7528") || !tip.includes("\u8f85\u52a9\u8fdb\u7a0b") || !tip.includes("\u56fe\u5f62") || !tip.includes("\u9875\u9762")) throw new Error(tip);
  if (!tip.includes("9 \u4e2a\u8fdb\u7a0b") || !tip.includes("\u5168\u90e8 10 \u4e2a\u6838\u5fc3")) throw new Error(tip);
  prefs.lang = "en";
  if (typeof paintChrome === "function") paintChrome();
});
t("VM memory tip is Chinese when the window is", () => {
  prefs.lang = "zh";
  if (typeof paintChrome === "function") paintChrome();
  // \u: 容器 / 占用 / 设置 (and traditional 設定 for zh-TW is not required here).
  const tip = tr("Kiri's containers, engine and agent hold {used} of the {total} WSL has (.wslconfig sets that).", { used: "3.0 GB", total: "3.8 GB" });
  if (!tip.includes("\u5bb9\u5668") || !tip.includes("\u5360\u7528") || !tip.includes("WSL") || !tip.includes("3.0 GB") || !tip.includes("3.8 GB")) throw new Error(tip);
  if (tip.includes("hold ") || tip.includes("engine and agent")) throw new Error(tip);
  prefs.lang = "en";
  if (typeof paintChrome === "function") paintChrome();
});
t("docker context tip is Chinese when the window is", () => {
  prefs.lang = "zh";
  if (typeof paintChrome === "function") paintChrome();
  const tip = tr("The docker command uses {app}", { app: "Kiri" });
  // \u: 命令 / 使用
  if (!tip.includes("docker") || !tip.includes("Kiri") || !tip.includes("\u547d\u4ee4") || !tip.includes("\u4f7f\u7528")) throw new Error(tip);
  if (tip.includes("The docker command")) throw new Error(tip);
  prefs.lang = "en";
  if (typeof paintChrome === "function") paintChrome();
});
report(R);
