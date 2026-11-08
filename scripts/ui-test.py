#!/usr/bin/env python3
"""Runs the window's page tests (tests/ui/*.js) in headless Chrome.

Each test file runs in the real page (crates/kiri/src/ui/index.html, with its
icons) after tests/ui/fixture.js, and calls report() with lines that start
with PASS or FAIL. Exits non-zero if any test fails or a page errors.

    python3 scripts/ui-test.py [tests/ui/switch.js ...]
"""
import html, os, re, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CHROME = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
UI = os.path.join(ROOT, "crates/kiri/src/ui")


def page(test, os_name=None):
    # As assemble.rs builds it: the icons and xterm.js spliced in.
    xterm = lambda f: open(os.path.join(UI, "xterm", f)).read()
    index = (open(os.path.join(UI, "index.html")).read().replace("/*ICONS*/", open(os.path.join(UI, "icons.js")).read(), 1)
             .replace("/*XTERM_CSS*/", xterm("xterm.css"), 1)
             .replace("/*XTERM_JS*/", "".join(xterm(f) + "\n" for f in ["xterm.js", "addon-fit.js", "addon-search.js", "addon-web-links.js"]), 1))
    if os_name and os_name != "macos":
        index = index.replace("<head>", f'<head><script>window.KIRI_OS = "{os_name}";</script>', 1)
    fixture = open(os.path.join(ROOT, "tests/ui/fixture.js")).read()
    test_js = open(test).read()
    # Wait for gzipped MT language packs (ja/ko/…) before assertions that call tr().
    report = 'const report = (lines) => document.body.insertAdjacentHTML("beforeend", `<pre id="results">${lines.join("\\n")}</pre>`);'
    catch = '<script>window.onerror = (m, u, l) => document.body.insertAdjacentHTML("beforeend", `<pre id="results">FAIL page error: ${m} (line ${l})</pre>`);</script>'
    runner = (
        f"{report}\n"
        "(async () => {\n"
        "  if (typeof i18nReady !== 'undefined') await i18nReady;\n"
        f"{fixture}\n"
        f"{test_js}\n"
        "})().catch((e) => report(['FAIL page error: ' + (e && e.message || e)]));\n"
    )
    return index[: index.rindex("</body>")] + catch + f"<script>{runner}</script></body></html>"


def run(test, os_name=None):
    first = open(test).readline()
    if first.startswith("// parity:") and os_name is None:
        return parity(test, first.split(":", 1)[1].split())
    with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as f:
        f.write(page(test, os_name))
    try:
        dom = subprocess.run([CHROME, "--headless=new", "--disable-gpu", "--virtual-time-budget=3000", "--dump-dom", f"file://{f.name}"],
                             capture_output=True, text=True, timeout=120).stdout
    finally:
        os.unlink(f.name)
    found = re.findall(r'<pre id="results">(.*?)</pre>', dom, re.S)
    return html.unescape(found[-1]).splitlines() if found else ["FAIL no results: the test didn't run"]


def parity(test, systems):
    """A feature ships on every OS at once: the page's ITEM lines must match
    across `systems`, except items tests/ui/parity-allow.txt gives a reason for."""
    allow = {}
    for line in open(os.path.join(ROOT, "tests/ui/parity-allow.txt")):
        if line.strip() and not line.startswith("#"):
            item, _, why = line.partition(" | ")
            allow[item.strip()] = why.strip()
    have, out = {}, []
    for s in systems:
        lines = run(test, s)
        out += [f"{x} ({s})" for x in lines if x.startswith("FAIL")]
        have[s] = {x[5:] for x in lines if x.startswith("ITEM ")}
    every = set().union(*have.values())
    for item in sorted(every):
        missing = [s for s in systems if item not in have[s]]
        if not missing:
            continue
        if item in allow:
            out.append(f"PASS {item}: not on {', '.join(missing)} ({allow[item]})")
        else:
            out.append(f"FAIL {item}: missing on {', '.join(missing)}. Ship it there too, or add it to tests/ui/parity-allow.txt with the reason")
    out += [f"FAIL {item}: allowed in parity-allow.txt but on every OS now; remove the line" for item in sorted(allow) if all(item in have[s] for s in systems)]
    out.append(f"PASS {len(every)} items checked on {', '.join(systems)}")
    return out


def main():
    tests = sys.argv[1:] or sorted(os.path.join(ROOT, "tests/ui", f) for f in os.listdir(os.path.join(ROOT, "tests/ui")) if f.endswith(".js") and f != "fixture.js")
    failed = 0
    for test in tests:
        print(os.path.relpath(test, ROOT))
        for line in run(test):
            print("  " + line)
            failed += line.startswith("FAIL")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
