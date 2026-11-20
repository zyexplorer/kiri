// Pull from's Sign in: each registry that works says whether it's signed in,
// and signs in through the registry's own page (ui/weblogin.rs), never a
// password typed into the window: Docker Hub's and GitHub's one-time code
// with Open, ECR through an AWS profile.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const menu = () => $("ppregmenu");
const row = (id) => menu().querySelector(`[data-reg="${id}"]`).closest(".rrow");
const panel = () => menu().querySelector(".wpanel");
const web = (reg, state, more = {}) => kiri.on({ type: "weblogin", reg, state, ...more });
const logins = (items) => kiri.on({ type: "logins", items, store: "Kept in the Keychain", stores: {} });
const opened = () => { openPull(); if (menu().hidden) $("ppreg").click(); };
logins([]);
show("images");
t("signed out, Docker Hub and GitHub offer Sign in and ECR its AWS profile; the rest are Soon", () => {
  opened();
  eq(["docker", "ghcr", "ecr"].map((id) => row(id).querySelector("[data-websign]")?.textContent), ["Sign in", "Sign in", "Use AWS profile"], "buttons");
  eq(row("ecr").querySelector(".rsub").textContent, "Public images need no sign-in", "ECR says public needs nothing");
  eq(["quay", "gar", "gitlab"].map((id) => !!row(id).querySelector("[data-websign]")), [false, false, false], "Soon ones have none");
  eq(menu().querySelectorAll('input[type="password"]').length, 0, "no password field anywhere");
});
t("Sign in on Docker Hub starts docker's own sign-in and opens under its row, keeping the menu open", () => {
  sent.length = 0;
  row("docker").querySelector("[data-websign]").click();
  eq([sent.map((m) => [m.type, m.name]), menu().hidden, !!panel(), panel().previousElementSibling === row("docker")], [[["weblogin", "docker"]], false, true, true], "started");
  eq(/Starting/.test(panel().textContent), true, "says it's starting");
});
t("the one-time code shows with Copy and Open Docker Hub, and says the password goes to Docker", () => {
  web("docker", "code", { code: "WDJB-MJHT", url: "https://login.docker.com/activate" });
  eq([panel().querySelector(".wcode b").textContent, panel().querySelector("[data-webopen]").textContent.trim()], ["WDJB-MJHT", "Open Docker Hub"], "code");
  eq(/go to Docker, not Kiri/.test(panel().textContent), true, "safe line");
  sent.length = 0;
  panel().querySelector("[data-webcopy]").click();
  eq(sent.map((m) => [m.type, m.name]), [["copy", "WDJB-MJHT"]], "copied");
});
t("Open has the CLI open its page, then it waits naming the code, with the page again and Cancel", () => {
  sent.length = 0;
  panel().querySelector("[data-webopen]").click();
  eq(sent.map((m) => m.type), ["webloginopen"], "opened");
  eq([/approve WDJB-MJHT/.test(panel().textContent), !!panel().querySelector("[data-webagain]"), !!panel().querySelector("[data-webcancel]")], [true, true, true], "waiting");
  sent.length = 0;
  panel().querySelector("[data-webagain]").click();
  eq(sent.map((m) => [m.type, m.name]), [["open", "https://login.docker.com/activate"]], "the page again");
});
t("done: the panel goes and the row names who's signed in, with no Sign in", () => {
  web("docker", "done", { user: "libracoder", server: "https://index.docker.io/v1/" });
  logins([{ server: "https://index.docker.io/v1/", username: "libracoder", via: "osxkeychain" }]);
  eq([!!panel(), row("docker").querySelector(".rsub").textContent, !!row("docker").querySelector("[data-websign]")], [false, "libracoder", false], "signed in");
});
t("signed in, the row has a sign-out icon instead of Sign in, naming the registry", () => {
  const b = row("docker").querySelector("[data-webask]");
  eq([!!b, b.getAttribute("aria-label"), b.getAttribute("aria-expanded")], [true, "Sign out of Docker Hub", "false"], "icon");
});
t("the icon asks first, under the row: who, what it means for the docker command; Cancel keeps it", () => {
  sent.length = 0;
  row("docker").querySelector("[data-webask]").click();
  const p = panel();
  eq([!!p, p.previousElementSibling === row("docker"), p.querySelector(".wt").textContent, sent.length], [true, true, "Sign out of Docker Hub?", 0], "asks, sends nothing");
  eq([/libracoder's sign-in is forgotten/.test(p.textContent), /docker command is signed out too/.test(p.textContent), row("docker").querySelector("[data-webask]").getAttribute("aria-expanded")], [true, true, "true"], "says what");
  p.querySelector('[data-webask=""]').click();
  eq([!!panel(), sent.length], [false, 0], "cancelled");
});
t("Sign out signs out that registry, and the row offers Sign in again", () => {
  row("docker").querySelector("[data-webask]").click();
  sent.length = 0;
  panel().querySelector("[data-webout]").click();
  eq(sent.map((m) => [m.type, m.name]), [["logout", "https://index.docker.io/v1/"]], "logout");
  logins([]);
  eq([!!panel(), row("docker").querySelector("[data-websign]")?.textContent], [false, "Sign in"], "signed out");
  logins([{ server: "https://index.docker.io/v1/", username: "libracoder", via: "osxkeychain" }]);
});
t("GitHub: its code asks only to read packages; Cancel stops it", () => {
  row("ghcr").querySelector("[data-websign]").click();
  web("ghcr", "code", { code: "1A2B-3C4D", url: "https://github.com/login/device" });
  eq([panel().querySelector("[data-webopen]").textContent.trim(), /read your packages/.test(panel().textContent), /only read packages/.test(panel().textContent)], ["Open GitHub", true, true], "code");
  sent.length = 0;
  panel().querySelector("[data-webcancel]").click();
  eq([sent.map((m) => m.type), !!panel(), !!row("ghcr").querySelector("[data-websign]")], [["weblogincancel"], false, true], "cancelled");
});
t("an event for a sign-in the page moved on from is ignored", () => {
  web("ghcr", "failed", { error: "late" });
  eq(!!panel(), false, "nothing opens");
});
t("no GitHub CLI: it says so plainly, with no form", () => {
  row("ghcr").querySelector("[data-websign]").click();
  web("ghcr", "notool", { tool: "gh" });
  eq([/GitHub CLI \(gh\)/.test(panel().textContent), panel().querySelectorAll("input").length], [true, 0], "notool");
  panel().querySelector("[data-webcancel]").click();
});
t("a failure says why, with Try again", () => {
  row("ghcr").querySelector("[data-websign]").click();
  web("ghcr", "failed", { error: "GitHub's sign-in didn't finish" });
  eq([panel().querySelector(".werr").textContent, !!panel().querySelector('[data-websign="ghcr"]')], ["GitHub's sign-in didn't finish", true], "failed");
  sent.length = 0;
  panel().querySelector('[data-websign="ghcr"]').click();
  eq(sent.map((m) => [m.type, m.name]), [["weblogin", "ghcr"]], "again");
  panel().querySelector("[data-webcancel]").click();
});
t("ECR asks the AWS CLI for profiles, then uses the chosen one", () => {
  sent.length = 0;
  row("ecr").querySelector("[data-websign]").click();
  eq(sent.map((m) => m.type), ["awsprofiles"], "asked");
  kiri.on({ type: "awsprofiles", items: ["default", "dev-readonly"], tool: true });
  const sel = panel().querySelector("[data-webprof]");
  eq([...sel.options].map((o) => o.textContent), ["default", "dev-readonly"], "profiles");
  sel.value = "dev-readonly";
  sent.length = 0;
  panel().querySelector("[data-webprofile]").click();
  eq(sent.map((m) => [m.type, m.name, m.value?.profile]), [["weblogin", "ecr", "dev-readonly"]], "used");
});
t("an expired AWS SSO session offers Sign in with AWS, which runs aws sso login for that profile", () => {
  web("ecr", "sso", { profile: "dev-readonly" });
  eq(/expired/.test(panel().textContent), true, "says expired");
  sent.length = 0;
  panel().querySelector("[data-websso]").click();
  eq(sent.map((m) => [m.type, m.value?.profile]), [["webloginsso", "dev-readonly"]], "sso");
  web("ecr", "done", { user: "dev-readonly", server: "123456789012.dkr.ecr.eu-west-1.amazonaws.com" });
  logins([{ server: "https://index.docker.io/v1/", username: "libracoder" }, { server: "123456789012.dkr.ecr.eu-west-1.amazonaws.com", username: "AWS" }]);
  eq([!!panel(), row("ecr").querySelector(".rsub").textContent], [false, "AWS · 123456789012.dkr.ecr.eu-west-1"], "signed in");
});
t("ECR's sign-out says signing in again uses the AWS profile", () => {
  row("ecr").querySelector("[data-webask]").click();
  eq([panel().querySelector(".wt").textContent, /AWS's sign-in is forgotten/.test(panel().textContent), /uses your AWS profile/.test(panel().textContent)], ["Sign out of Amazon ECR?", true, true], "ecr");
  sent.length = 0;
  panel().querySelector("[data-webout]").click();
  eq(sent.map((m) => [m.type, m.name]), [["logout", "123456789012.dkr.ecr.eu-west-1.amazonaws.com"]], "logout");
});
t("a profile with an MFA device: six boxes under the row naming the device; the sixth digit sends the code", () => {
  logins([]);
  row("ecr").querySelector("[data-websign]").click();
  kiri.on({ type: "awsprofiles", items: ["platform-admin"], tool: true });
  panel().querySelector("[data-webprofile]").click();
  web("ecr", "mfa", { profile: "platform-admin", device: "S24", bad: false });
  const boxes = () => [...panel().querySelectorAll("[data-mfa]")];
  eq([boxes().length, panel().querySelector(".wt").textContent, /MFA device S24/.test(panel().textContent), document.activeElement === boxes()[0]], [6, "Enter your MFA code", true, true], "asks");
  sent.length = 0;
  const b0 = boxes()[0];
  b0.value = "123";
  b0.dispatchEvent(new Event("input", { bubbles: true }));
  eq([boxes().map((b) => b.value), document.activeElement === boxes()[3], sent.length], [["1", "2", "3", "", "", ""], true, 0], "three typed, not sent");
  const b3 = boxes()[3];
  b3.value = "456";
  b3.dispatchEvent(new Event("input", { bubbles: true }));
  eq(sent.map((m) => [m.type, m.value?.code]), [["webloginmfa", "123456"]], "sent at six");
  eq([/Checking the code with AWS/.test(panel().textContent), boxes().length], [true, 0], "checking");
});
t("a code AWS turns down asks again, saying so; Enter in the boxes doesn't pick a search row", () => {
  web("ecr", "mfa", { profile: "platform-admin", device: "S24", bad: true });
  eq([panel().querySelectorAll("[data-mfa]").length, /didn't accept that code/.test(panel().textContent), !!panel().querySelector(".wotp.bad")], [6, true, true], "again");
  sent.length = 0;
  panel().querySelector("[data-mfa]").dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  eq(sent.filter((m) => m.type === "pull").length, 0, "no pull");
  panel().querySelector("[data-webcancel]").click();
});
t("no AWS CLI: ECR says public images still pull", () => {
  logins([]);
  row("ecr").querySelector("[data-websign]").click();
  kiri.on({ type: "awsprofiles", items: [], tool: false });
  eq(/Public images still pull/.test(panel().textContent), true, "notool");
  panel().querySelector("[data-webcancel]").click();
  closePull();
});
report(R);
