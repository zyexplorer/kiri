// Signing in to a registry (docker login) from the window: in the row of a
// pull that needs it, in the Pull image popover, and in Settings → Registries.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
const key = (el, k) => el.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true }));
const fill = (form, u, tk) => { form.querySelector('[name="u"]').value = u; form.querySelector('[name="t"]').value = tk; };
const logins = (items, store = "Saved with docker-credential-osxkeychain") => kiri.on({ type: "logins", items, store, stores: {} });
const REF = "ghcr.io/acme/api:1.2";
const row = () => document.querySelector(`tr[data-key="pull:${REF}"]`);
const form = () => row()?.querySelector("[data-signin]");
// Switched on here; the app has it off and says docker login instead (the last tests).
ui.loginsOn = true;
logins([]);
show("images");
pullRef(REF);
kiri.on({ type: "pull", state: "done", name: REF, error: "Couldn't find an image called ghcr.io/acme/api. Check the name; if it's private, sign in with docker login." });
t("a pull that needs a sign-in asks for it in its row, saying where it's kept", () => {
  const f = form();
  eq([!!f, f.querySelector('[name="t"]').type, f.querySelector("[data-signin-go]").textContent], [true, "password", "Sign in and pull"], "form");
  eq(f.querySelector("small").textContent, "Saved with docker-credential-osxkeychain", "where");
  eq(row().querySelector("[data-pretry]"), null, "no Retry: signing in pulls");
});
t("empty fields aren't sent: the empty one is marked", () => {
  sent.length = 0;
  form().querySelector("[data-signin-go]").click();
  eq([sent.filter((m) => m.type === "login").length, form().querySelector('[name="u"]').classList.contains("need")], [0, true], "not sent");
});
t("Sign in and pull sends it, and the button says so without a spinner inside", () => {
  sent.length = 0;
  fill(form(), "octo", "ghp_wrong");
  form().querySelector("[data-signin-go]").click();
  eq(sent.filter((m) => m.type === "login").map((m) => [m.name, m.value.username, m.value.secret]), [["ghcr.io", "octo", "ghp_wrong"]], "sent");
  const b = form().querySelector("[data-signin-go]");
  eq([b.disabled, b.textContent, !!b.querySelector(".hspin")], [true, "Signing in…", false], "busy");
});
t("typing survives the lists coming again", () => {
  kiri.on({ type: "lists", containers: ui.containers, volumes: [], networks: [], images: [] });
  eq(form().querySelector('[name="u"]').value, "octo", "kept");
});
t("a wrong password says so under the fields, which keep what was typed", () => {
  kiri.on({ type: "login", server: "ghcr.io", error: "ghcr.io didn't accept that username and password or token." });
  eq(row().querySelector(".signin-err").textContent, "ghcr.io didn't accept that username and password or token.", "error");
  eq([form().querySelector('[name="u"]').value, form().querySelector("[data-signin-go]").disabled], ["octo", false], "again");
});
t("Enter in the token field signs in; once signed in, the pull goes again", () => {
  sent.length = 0;
  form().querySelector('[name="t"]').value = "ghp_right";
  key(form().querySelector('[name="t"]'), "Enter");
  eq(sent.filter((m) => m.type === "login").map((m) => m.value.secret), ["ghp_right"], "sent");
  kiri.on({ type: "login", server: "ghcr.io", username: "octo", store: "Saved with docker-credential-osxkeychain" });
  logins([{ server: "ghcr.io", username: "octo", via: "docker-credential-osxkeychain" }]);
  eq(sent.filter((m) => m.type === "pull").map((m) => m.name), [REF], "pulled again");
  eq([!!form(), /Starting|Asking/.test(row().textContent)], [false, true], "row pulling");
});
t("Docker Hub's not found offers a sign-in for a private image", () => {
  pullRef("acme/secret");
  kiri.on({ type: "pull", state: "done", name: "acme/secret:latest", error: "Couldn't find an image called acme/secret. Check the name; if it's private, sign in with docker login." });
  const r = document.querySelector('tr[data-key="pull:acme/secret:latest"]');
  r.querySelector("[data-pauth]").click();
  const f = document.querySelector('tr[data-key="pull:acme/secret:latest"] [data-signin]');
  eq([!!f, f.dataset.host, f.querySelector('[name="t"]').placeholder], [true, "https://index.docker.io/v1/", "Access token"], "Hub's form");
  document.querySelector('tr[data-key="pull:acme/secret:latest"] [data-pdismiss]').click();
});
t("Pull image: another registry says who you're signed in as", () => {
  openPull("ghcr.io/acme/web:2");
  eq(/Signed in as octo\./.test($("pplist").textContent), true, $("pplist").textContent);
});
t("Pull image: not signed in, it offers to sign in first, right there", () => {
  sent.length = 0;
  openPull("quay.io/acme/db:5");
  $("pplist").querySelector("[data-ppsignin]").click();
  const f = $("pplist").querySelector('[data-signin="pp"]');
  eq([f.dataset.host, document.activeElement?.name], ["quay.io", "u"], "form, focused");
  fill(f, "robot", "tok");
  key(f.querySelector('[name="t"]'), "Enter");
  eq([sent.filter((m) => m.type === "login").map((m) => m.name), sent.filter((m) => m.type === "pull").length, $("pp").hidden], [["quay.io"], 0, false], "signs in, doesn't pull");
  kiri.on({ type: "login", server: "quay.io", username: "robot" });
  logins([{ server: "ghcr.io", username: "octo" }, { server: "quay.io", username: "robot" }]);
  eq([/Signed in as robot\./.test($("pplist").textContent), !!$("pplist").querySelector("[data-signin]")], [true, false], "signed in");
  closePull();
});
t("Settings → Registries lists them, with Sign out", () => {
  logins([{ server: "https://index.docker.io/v1/", username: "me", via: "the Keychain" }, { server: "ghcr.io", username: "octo", via: "docker-credential-osxkeychain" }]);
  show("settings");
  document.querySelector('[data-stab="registries"]').click();
  const rows = [...document.querySelectorAll(".pane .pref")].map((p) => p.textContent.replace(/\s+/g, " ").trim());
  eq(rows, ["Docker Hub Signed in as me · kept in the Keychain Sign out", "ghcr.io Signed in as octo · saved with docker-credential-osxkeychain Sign out"], "rows");
  sent.length = 0;
  document.querySelector('[data-logout="ghcr.io"]').click();
  eq(sent.filter((m) => m.type === "logout").map((m) => m.name), ["ghcr.io"], "sent");
  eq(document.querySelector('[data-logout="ghcr.io"]').textContent, "Signing out…", "says so");
  logins([{ server: "https://index.docker.io/v1/", username: "me", via: "the Keychain" }]);
  eq(document.querySelectorAll("[data-logout]").length, 1, "gone");
});
t("Sign in to a registry: Docker Hub unless named, with a hint for each", () => {
  document.querySelector("[data-regopen]").click();
  const f = document.querySelector('[data-signin="settings"]'), s = f.querySelector('[name="s"]');
  eq([s.placeholder, document.activeElement === s, /Docker Hub personal access token/.test($("reg-hint").textContent)], ["Docker Hub", true, true], "Hub");
  s.value = "ghcr.io"; s.dispatchEvent(new Event("input", { bubbles: true }));
  eq(/read:packages/.test($("reg-hint").textContent), true, "GitHub's");
  s.value = "123456789.dkr.ecr.eu-west-1.amazonaws.com"; s.dispatchEvent(new Event("input", { bubbles: true }));
  eq(/takes a token/.test($("reg-hint").textContent), true, "ECR's");
  s.value = "GHCR.io"; s.dispatchEvent(new Event("input", { bubbles: true }));
  sent.length = 0;
  fill(f, "octo", "ghp_x");
  f.querySelector("[data-signin-go]").click();
  eq(sent.filter((m) => m.type === "login").map((m) => [m.name, m.value.username]), [["ghcr.io", "octo"]], "sent");
  kiri.on({ type: "login", server: "ghcr.io", error: "ghcr.io didn't accept that username and password or token." });
  const g = document.querySelector('[data-signin="settings"]');
  eq([g.querySelector(".signin-err").textContent.startsWith("ghcr.io didn't accept"), g.querySelector('[name="u"]').value, g.querySelector('[name="s"]').value], [true, "octo", "GHCR.io"], "error, kept");
  g.querySelector("[data-signin-go]").click();
  kiri.on({ type: "login", server: "ghcr.io", username: "octo" });
  logins([{ server: "https://index.docker.io/v1/", username: "me" }, { server: "ghcr.io", username: "octo" }]);
  eq([!!document.querySelector('[data-signin="settings"]'), !!document.querySelector("[data-regopen]"), document.querySelectorAll("[data-logout]").length], [false, true, 2], "done");
});
t("before the app says, the note names the system's store", () => {
  ui.logins = null;
  eq(regStore("ghcr.io"), WIN ? "Kept in Credential Manager" : "Kept in the Keychain", "store");
});
t("held back: the app has no Registries tab and says docker login in the row", () => {
  ui.loginsOn = false;
  show("settings");
  eq(document.querySelector('[data-stab="registries"]'), null, "no tab");
  show("images");
  const R2 = "ghcr.io/acme/web:1";
  pullRef(R2);
  kiri.on({ type: "pull", state: "done", name: R2, error: "Couldn't find an image called ghcr.io/acme/web. Check the name; if it's private, sign in with docker login." });
  const r = document.querySelector(`tr[data-key="pull:${R2}"]`);
  eq([!!r.querySelector("[data-signin]"), !!r.querySelector("[data-pretry]"), r.textContent.includes("docker login ghcr.io")], [false, true, true], "row");
  r.querySelector("[data-pdismiss]")?.click();
  ui.loginsOn = true;
});
t("the pull popover's registry chip opens the list: Docker Hub picked, GitHub and Amazon ECR choosable, the rest Soon", () => {
  openPull();
  $("ppreg").click();
  const m = $("ppregmenu");
  eq([m.hidden, m.querySelectorAll("[data-reg]").length, m.querySelector('[aria-checked="true"]').textContent, m.querySelectorAll(".soon").length], [false, 7, "Docker Hub", 4], "menu");
  eq([...m.querySelectorAll("[data-reg]:not(:disabled)")].map((b) => b.dataset.reg), ["docker", "ghcr", "ecr"], "choosable");
  m.querySelector('[data-reg="docker"]').click();
  eq(m.hidden, true, "closes");
  closePull();
});
report(R);
