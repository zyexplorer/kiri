// formatStartCommand: pretty-print kiri.start.command without stray `\` or
// breaks inside a quoted sh -c payload (e.g. mkdir -p).
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, why) => { if (a !== b) throw new Error(`${why || "eq"}: ${JSON.stringify(a)} !== ${JSON.stringify(b)}`); };

t("keeps mkdir -p inside sh -c on one quoted line", () => {
  const raw = "docker run -d --name nginxwebui --restart unless-stopped --entrypoint tini -p 18080:8080 -v /Users/zhuyin/nginxWebUI:/home/nginxWebUI -e BOOT_OPTIONS=--server.port=8080 cym1102/nginxwebui:latest -- sh -c 'mkdir -p /home/nginxWebUI/log; touch /home/nginxWebUI/log/nginxWebUI.log; java -jar /home/nginxWebUI.jar ${BOOT_OPTIONS} >/dev/null 2>&1 & exec tail -F /home/nginxWebUI/log/nginxWebUI.log'";
  const out = formatStartCommand(raw);
  if (/\n\s*\\\s*\n/.test(out)) throw new Error("stray lone backslash line:\n" + out);
  if (/mkdir \\\n/.test(out) || /mkdir\s+\\\n/.test(out)) throw new Error("broke inside sh -c:\n" + out);
  if (!out.includes("mkdir -p /home/nginxWebUI/log")) throw new Error("lost mkdir -p:\n" + out);
  const lines = out.split("\n");
  eq(lines[0], "docker run -d \\", "head");
  if (!lines.some((l) => l.trim() === "-p 18080:8080 \\" || l.trim() === "-p 18080:8080")) throw new Error("missing -p line:\n" + out);
  if (!lines.some((l) => /cym1102\/nginxwebui:latest/.test(l))) throw new Error("missing image:\n" + out);
});

t("does not insert a backslash before the token after -p", () => {
  const out = formatStartCommand("docker run -d -p 8080:80 -v /tmp:/tmp nginx:alpine");
  eq(out, "docker run -d \\\n  -p 8080:80 \\\n  -v /tmp:/tmp \\\n  nginx:alpine");
});

t("flattens an already multiline label then rebreaks cleanly", () => {
  const raw = "docker run -d \\\n  --name web \\\n  -p 80:80 \\\n  nginx:alpine";
  eq(formatStartCommand(raw), "docker run -d \\\n  --name web \\\n  -p 80:80 \\\n  nginx:alpine");
});

report(R);
