#!/usr/bin/env python3
"""Runs the window's page tests (tests/ui/*.js) in headless Chrome.

Each test file runs in the real page (crates/kiri/src/ui/index.html, with its
icons) after tests/ui/fixture.js, and calls report() with lines that start
with PASS or FAIL. Exits non-zero if any test fails or a page errors. A
test's first lines may say `// parity: macos windows linux` (run as each
system and compare), `// lang: de` (the language it opens in), `// window:
760x480` (the window's size) or `// page: setup` (Windows' setup window,
crates/kiri-win/src/setup.html, instead of the main window).

    python3 scripts/ui-test.py [tests/ui/switch.js ...]
"""
import html, json, os, re, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CHROME = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
UI = os.path.join(ROOT, "crates/kiri/src/ui")


# The results, in the page; and to the page around it when a test runs in a
# frame of a given size (`// window:`).
REPORT = ('const report = (lines) => { if (window.parent !== window) parent.postMessage(lines.join("\\n"), "*");'
          ' document.body.insertAdjacentHTML("beforeend", `<pre id="results">${lines.join("\\n")}</pre>`); };')
CATCH = ('<script>window.onerror = (m, u, l) => { const s = `FAIL page error: ${m} (line ${l})`; if (window.parent !== window) parent.postMessage(s, "*");'
         ' document.body.insertAdjacentHTML("beforeend", `<pre id="results">${s}</pre>`); };</script>')


def strings():
    """lang/*.json packed as lang.rs packs them: the English keys once, each language its words in that order."""
    d = os.path.join(UI, "lang")
    files = {f[:-5]: json.load(open(os.path.join(d, f))) for f in sorted(os.listdir(d)) if f.endswith(".json")}
    keys = sorted(set().union(*files.values()))
    return {"en": keys, **{tag: [m.get(k) for k in keys] for tag, m in files.items()}}


def opening(pick, packed):
    """What assemble.rs splices in for `pick` on an en-US system: the keys and
    the one language the page opens in (a file's tag, or English)."""
    return {"en": packed["en"], **({pick: packed[pick]} if pick in packed and pick != "en" else {})}


def host(packed):
    """The host's half of langwords, after the fixture's recorder: it answers as
    lang.rs's words_js does, calling kiriWords with that language's words, at
    once (a real host answers a moment later). hostHold = true keeps the answers
    until hostFlush(), for a test of the moment in between."""
    words = {t: w for t, w in packed.items() if t != "en"}
    return (f"const HOST_WORDS = {json.dumps(words, ensure_ascii=False)};\n"
            "let hostHold = false; const hostHeld = [];\n"
            "const hostFlush = () => hostHeld.splice(0).forEach((f) => f());\n"
            "{ const post = window.webkit.messageHandlers.kiri.postMessage;\n"
            "  window.webkit.messageHandlers.kiri.postMessage = (m) => { post(m); const o = JSON.parse(m);\n"
            "    if (o.cmd !== \"langwords\" || !HOST_WORDS[o.name]) return;\n"
            "    const answer = () => window.kiriWords && kiriWords({ [o.name]: HOST_WORDS[o.name] });\n"
            "    if (hostHold) hostHeld.push(answer); else answer(); }; }\n")


def header(test, name, default=None):
    """`// name: value` in one of a test's first lines."""
    for line in open(test).readlines()[:4]:
        if line.startswith(f"// {name}:"):
            return line.split(":", 1)[1].strip()
    return default


def lang_of(test):
    """A test runs in English, as a system set to US English would, unless
    one of its first lines says `// lang: zh-Hans`."""
    return header(test, "lang", "system")


def setup_page(test):
    """kiri-win's setup window (crates/kiri-win/src/setup.html), for a test whose
    first lines say `// page: setup`: as assemble.rs's words() builds it, with
    the language step and the words for the language it opens in. `sent`
    records what the page asks the host; host() answers langwords."""
    packed, pick = strings(), lang_of(test)
    src = (open(os.path.join(ROOT, "crates/kiri-win/src/setup.html")).read()
           .replace("/*LANGSTEP*/", open(os.path.join(UI, "langstep.js")).read(), 1)
           .replace("/*LANG*/", f"window.KIRI_LANG = {json.dumps({'os': ['en-US'], 'pick': pick})};", 1)
           .replace("/*STRINGS*/{}", json.dumps(opening(pick, packed), ensure_ascii=False), 1))
    # The page draws a host state on the next frame; headless Chrome's frames
    # don't keep pace with --virtual-time-budget (the dump can come first), so
    # a frame here is a task.
    fixture = ('window.requestAnimationFrame = (f) => setTimeout(() => f(performance.now()), 0);\n'
               'const sent = [];\n'
               'window.webkit = { messageHandlers: { kiri: { postMessage: (m) => { const o = JSON.parse(m); sent.push({ type: o.cmd, ...o }); } } } };\n')
    i = src.index("<script>")
    src = src[:i] + CATCH + src[i:]
    return src[: src.rindex("</body>")] + f"<script>{REPORT}\n{fixture}\n{host(packed)}\n{open(test).read()}</script></body></html>"


def lines(t):
    """As Rust's split_inclusive('\\n'): lines end at a newline only (never a lone \\r or U+2028)."""
    return re.findall(r"[^\n]*\n|[^\n]+$", t)


def style_span(index):
    """As assemble.rs's style_span: the plain `<style>` to the `</style>` after it, or None."""
    a = index.find("<style>")
    b = index.find("</style>", a) if a >= 0 else -1
    return (a, b) if b >= 0 else None


def find_tag(text, name, start):
    """As assemble.rs's find_tag: `name` in any case, followed by a space, `/` or `>`; -1 if none."""
    m = re.compile(re.escape(name) + r"(?=[\s/>])", re.I).search(text, start)
    return m.start() if m else -1


def no_css_comments(index):
    """As assemble.rs's without_css_comments: the stylesheet's comments go, its splice points (`/*XTERM_CSS*/`) stay."""
    span = style_span(index)
    if not span:
        return index
    a, b = span
    css, out = index[a:b], [index[:a]]
    while True:
        i = css.find("/*")
        if i < 0: break
        j = css.find("*/", i + 2)
        if j < 0: break
        j += 2
        comment = css[i:j]
        out.append(tight_css(css[:i]))
        if all(c.isascii() and (c.isupper() or c == "_") for c in comment[2:-2]):
            out.append(comment)
        css = css[j:]
    return "".join(out) + tight_css(css) + index[b:]


def tight_css(css):
    """As assemble.rs's tight_css: no space after `;` and `{` or before `{`, outside strings; escapes and custom property values as written."""
    out, quote, i, decl = [], None, 0, True
    while i < len(css):
        if quote is None and decl:
            end = custom_value(css, i)
            if end is not None:
                out.append(css[i:end])
                i, decl = end, False
                continue
        c = css[i]
        out.append(c)
        i += 1
        if c not in CSS_SPACE:
            decl = quote is None and c in "{;"
        if c == "\\":
            out.append(css[i:i + 1])
            i, decl = i + 1, False
        elif quote:
            if c == quote:
                quote = None
        elif c in "\"'":
            quote = c
        elif c in ";{" and css[i:i + 1] == " ":
            i += 1
        elif c == " " and css[i:i + 1] == "{":
            out.pop()
    return "".join(out)


CSS_SPACE = " \t\n\r\x0c"


def custom_value(css, i):
    """As assemble.rs's custom_value: where the `--name:` declaration at i ends, or None."""
    if css[i:i + 2] != "--":
        return None
    j = i + 2
    while j < len(css) and ((css[j].isascii() and css[j].isalnum()) or css[j] in "-_"):
        j += 1
    while j < len(css) and css[j] in CSS_SPACE:
        j += 1
    if css[j:j + 1] != ":":
        return None
    quote, depth = None, 0
    j += 1
    while j < len(css):
        x = css[j]
        if x == "\\":
            j += 1
        elif quote:
            if x == quote:
                quote = None
        elif x in "\"'":
            quote = x
        elif x in "([{":
            depth += 1
        elif x in ")]":
            depth = max(depth - 1, 0)
        elif x in ";}" and depth == 0:
            return j
        elif x == "}":
            depth -= 1
        j += 1
    return len(css)


def no_comment_lines(index):
    """As assemble.rs's without_js_comment_lines: the script's whole-line `// ` comments go."""
    span = style_span(index)
    if not span:
        return index
    i = span[1]
    return index[:i] + "".join(l for l in lines(index[i:]) if not l.lstrip().startswith("// "))


def line_states(script):
    """As assemble.rs's line_states: the state each line starts in, and whether the script ends in code."""
    before = {"return", "typeof", "case", "do", "else", "in", "of", "void", "throw", "new", "delete", "yield", "await"}
    ident = lambda c: c.isalnum() or c in "_$"
    stack, depth, s, braces, prev, raw, word, out = [], [], "code", 0, "\n", "\n", "", []
    prev2, parens, after_cond = "\n", [], False
    for line in lines(script):
        out.append(s)
        i = 0
        while i < len(line):
            c = line[i]
            nxt = line[i + 1] if i + 1 < len(line) else "\0"
            was_ident = False
            if s == "code":
                if c == "/" and nxt == "/": s = "line"
                elif c == "/" and nxt == "*": s, i = "block", i + 1
                elif c == "/" and (prev in "(,=:[!&|?{};\n><*%~^" or (prev in "+-" and prev2 != prev) or (prev == ")" and after_cond) or (prev.isalnum() and word in before)): s = "regex"
                elif c == "(": parens.append(word in ("if", "while", "for", "with"))
                elif c == ")": after_cond = parens.pop() if parens else False
                elif c in "'\"": s = "q" + c
                elif c == "`": s = "tpl"
                elif c == "{": braces += 1
                elif c == "}" and depth and depth[-1] == braces: depth.pop(); s = stack.pop() if stack else "code"
                elif c == "}": braces = max(braces - 1, 0)
                elif ident(c):
                    if not ident(raw): word = ""
                    word += c
                    was_ident = True
            elif s.startswith("q"):
                if c == "\\": i += 2 if line[i + 1:i + 3] == "\r\n" else 1
                elif c == s[1] or c == "\n": s = "code"
            elif s == "tpl":
                if c == "\\": i += 2 if line[i + 1:i + 3] == "\r\n" else 1
                elif c == "`": s = "code"
                elif c == "$" and nxt == "{": stack.append("tpl"); depth.append(braces); s, i = "code", i + 1
            elif s == "line":
                if c == "\n": s = "code"
            elif s == "block":
                if c == "*" and nxt == "/": s, i = "code", i + 1
            elif s.startswith("regex"):
                if c == "\\": i += 1
                elif s == "regex" and c == "[": s = "regexc"
                elif s == "regexc" and c == "]": s = "regex"
                elif s == "regex" and c in "/\n": s = "code"
            if not was_ident and not c.isspace(): word = ""
            raw = c
            last = line[i] if i < len(line) else c
            if not last.isspace(): prev2, prev = ("\n" if last == "\n" else prev), last
            if last == "\n": prev = "\n"
            i += 1
    return out, s == "code" and not stack


def no_indent(index):
    """As assemble.rs's without_indent: the stylesheet's and scripts' lines lose their indentation where it is code."""
    trim = lambda l: l.lstrip(" \t")
    out, at = [], 0
    span = style_span(index)
    a, b = span if span else (-1, -1)
    if span:
        a += len("<style>")
        out.append(index[:a])
        cont = False
        for l in lines(index[a:b]):
            out.append(l if cont else trim(l))
            cont = l.rstrip("\r\n").endswith("\\")
        at = b
    pos = max(b, 0)
    while True:
        i = find_tag(index, "<script", pos)
        if i < 0: break
        o = index.find(">", i)
        if o < 0: break
        o += 1
        c = find_tag(index, "</script", o)
        c = len(index) if c < 0 else c
        pos = c
        if o < at: continue
        out.append(index[at:o])
        script = index[o:c]
        states, closed = line_states(script)
        out.append("".join(trim(l) if st == "code" else l for l, st in zip(lines(script), states)) if closed else script)
        at = c
    out.append(index[at:])
    return "".join(out)


def strip(index):
    """index.html as assemble.rs's page() has it before the splices: the same bytes."""
    return no_indent(no_comment_lines(no_css_comments(index)))


def page(test, os_name=None):
    # As assemble.rs builds it: the icons and xterm.js spliced in, and the words
    # for the language it opens in; the rest come through langwords (host()).
    packed, pick = strings(), lang_of(test)
    xterm = lambda f: open(os.path.join(UI, "xterm", f)).read()
    index = (strip(open(os.path.join(UI, "index.html")).read()).replace("/*ICONS*/", open(os.path.join(UI, "icons.js")).read(), 1)
             .replace("/*LANGSTEP*/", open(os.path.join(UI, "langstep.js")).read(), 1)
             .replace("/*XTERM_CSS*/", xterm("xterm.css"), 1)
             .replace("/*XTERM_JS*/", "".join(xterm(f) + "\n" for f in ["xterm.js", "addon-fit.js", "addon-search.js", "addon-web-links.js"]), 1)
             .replace("/*LANG*/", f"window.KIRI_LANG = {json.dumps({'os': ['en-US'], 'pick': pick})};", 1)
             .replace("/*STRINGS*/{}", json.dumps(opening(pick, packed), ensure_ascii=False), 1))
    if os_name and os_name != "macos":
        index = index.replace("<head>", f'<head><script>window.KIRI_OS = "{os_name}";</script>', 1)
    fixture = open(os.path.join(ROOT, "tests/ui/fixture.js")).read()
    test_js = open(test).read()
    # Wait for gzipped MT language packs (ja/ko/…) before assertions that call tr().
    # host() is main's fixture hook (other languages, parent-frame results).
    runner = (
        "(async () => {\n"
        "  if (typeof i18nReady !== 'undefined') await i18nReady;\n"
        f"{test_js}\n"
        "})().catch((e) => report(['FAIL page error: ' + (e && e.message || e)]));\n"
    )
    return index[: index.rindex("</body>")] + CATCH + f"<script>{REPORT}\n{fixture}\n{host(packed)}\n{runner}</script></body></html>"


def run(test, os_name=None):
    first = open(test).readline()
    if first.startswith("// parity:") and os_name is None:
        return parity(test, first.split(":", 1)[1].split())
    with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as f:
        f.write(setup_page(test) if header(test, "page") == "setup" else page(test, os_name))
    # `// window: 760x480`: the page in a frame that size (headless Chrome's
    # window is wider than 500 and has its own bar), which passes the results out.
    size, load = header(test, "window"), f.name
    if size:
        w, h = size.split("x")
        with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as fr:
            fr.write(f'<!doctype html><body style="margin:0"><iframe src="file://{f.name}" style="border:0;display:block;width:{w}px;height:{h}px"></iframe>'
                     '<script>addEventListener("message", (e) => { const p = document.createElement("pre"); p.id = "results"; p.textContent = e.data; document.body.append(p); });</script></body>')
        load = fr.name
    try:
        dom = subprocess.run([CHROME, "--headless=new", "--disable-gpu", "--virtual-time-budget=3000", "--window-size=1400,1000", "--dump-dom", f"file://{load}"],
                             capture_output=True, text=True, timeout=120).stdout
    finally:
        os.unlink(f.name)
        if load != f.name: os.unlink(load)
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
    # `--strip`: the page on stdin as strip() gives it, for assemble.rs's test that both builders agree.
    if sys.argv[1:] == ["--strip"]:
        sys.stdout.buffer.write(strip(sys.stdin.buffer.read().decode("utf-8")).encode("utf-8"))
        return
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
