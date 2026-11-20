// Every Copy says "Copied" the same way: a bubble above the button that was
// pressed, with the button left as it is (no "Copied" text swapped in).
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const bubbled = (b, label) => {
  const before = b.textContent.trim();
  document.querySelectorAll(".copied-tip").forEach((x) => x.remove());
  b.click();
  const tip = document.querySelector(".copied-tip"), r = b.getBoundingClientRect(), tr_ = tip?.getBoundingClientRect();
  eq([tip?.textContent.trim(), !!tip?.querySelector("svg"), tr_ ? tr_.bottom <= r.top : false], ["Copied", true, true], label + ": bubble");
  eq(b.textContent.trim(), before, label + ": the button is unchanged");
};

t("a failed operation's toast: Copy", () => {
  toast("error", "Pull failed: no such image", "op-1");
  const el = [...document.querySelectorAll("#toasts .toast")].at(-1);
  bubbled(el.querySelector("[data-tcopy]"), "toast");
  el.remove();
});
t("the error's details: Copy, and Copy ID once sent", () => {
  detailsDialog("Pull failed: no such image", "op-2");
  bubbled(document.querySelector('#dt [data-dt="copy"]'), "copy");
  dt.state = "sent"; dt.id = "KR-123"; drawDetails();
  sent.length = 0;
  bubbled(document.querySelector('#dt [data-dt="copyid"]'), "copy id");
  eq(sent.filter((m) => m.type === "copy").map((m) => m.name), ["KR-123"], "copies the id");
  document.querySelector("#dt").remove(); dt = null;
});
t("a sent report: Copy its ID", () => {
  reportDialog("send");
  rp.state = "sent"; rp.id = "KR-456"; drawReport();
  bubbled(document.querySelector("#rp [data-rpcopy]"), "report id");
  rp.close();
});
t("a copy button on a page: the same bubble", () => {
  $("content").insertAdjacentHTML("beforeend", `<div id="cp-host">${copyBtn("--device /dev/ttyACM0", "docker flag")}</div>`);
  bubbled(document.querySelector("#cp-host .copy"), "icon button");
  $("cp-host").remove();
});
report(R);
