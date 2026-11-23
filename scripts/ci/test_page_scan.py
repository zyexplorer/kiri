#!/usr/bin/env python3
"""Tests for page-scan.py: every pattern with a line it flags and an
ordinary line it doesn't, and that moved lines and known addresses pass.

    python3 -m unittest discover -s scripts/ci -p 'test_page_scan.py'
"""
import importlib.util
import os
import unittest

_spec = importlib.util.spec_from_file_location("page_scan", os.path.join(os.path.dirname(os.path.abspath(__file__)), "page-scan.py"))
ps = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ps)

# The page as it is before the change: a bridge, a known address, a few
# ordinary lines.
BASE = {
    "crates/kiri/src/ui/index.html": "\n".join([
        "<script>",
        "const send = (cmd, extra = {}) => window.webkit?.messageHandlers?.kiri?.postMessage(JSON.stringify({ cmd, ...extra }));",
        'const FEEDBACK_URL = "https://github.com/libracoder/kiri/issues/new";',
        'send("refresh");',
        'send("login", { name: server, value: { username: u, secret: tk.value } });',
        "const old = fetch(x); // pretend the page already had this",
        "</script>",
    ]),
}

# name: (a line it flags, an ordinary line it doesn't)
CASES = {
    "url": ('const u = "https://evil.example/collect";', 'const u = "https://github.com/libracoder/kiri/issues/new";'),
    "protocol-relative": ('img.setAttribute("src", "//evil.example/p.gif");', "// a comment about the list"),
    "ip": ('const host = "203.0.113.7";', 'const bind = "0.0.0.0:8080 failed";'),
    "data-blob": ('a.setAttribute("href", "blob:x");', 'const label = tr("data: none");'),
    "xhr-socket": ('const ws = new WebSocket(addr);', "const sockets = list.length;"),
    "beacon": ("navigator.sendBeacon(u, body);", 'send("refresh");'),
    "worker": ("const w = new Worker(src);", "const worker = 1;"),
    "new-image": ("const i = new Image();", "const image = images[0];"),
    "src-assign": ("img.src = u + q;", 'img.alt = "logo";'),
    "script-src": ('<script src="x.js"></script>', "<script>"),
    "frame": ('<iframe srcdoc="x"></iframe>', '<div class="frame"></div>'),
    "link-href": ('<link rel="stylesheet" href="x.css">', '<a data-open="x">link</a>'),
    "img-src": ('<img src="https://x.example/a.png">', '<img alt="" class="ic">'),
    "css-import": ("@import url(x.css);", ".import { color: red; }"),
    "meta-refresh": ('<meta http-equiv="refresh" content="0">', '<meta charset="utf-8">'),
    "form-action": ('<form action="/x">', '<form class="f">'),
    "dynamic-import": ("import(u).then(run);", "const important = 1;"),
    "window-open": ("window.open(u);", 'send("open", { name: u });'),
    "location": ("location.href = u;", "if (location.hash === '#x') go();"),
    "post-message": ('parent.postMessage(data, "*");', "window.webkit.messageHandlers.kiri.postMessage(m);"),
    "eval": ("eval(code);", "const evaluated = 1;"),
    "function-ctor": ('const f = new Function("return 1");', "function f() { return 1; }"),
    "timer-string": ('setTimeout("run()", 10);', "setTimeout(run, 10);"),
    "document-write": ("document.write(x);", "document.title = t;"),
    "html-from-outside": ("el.innerHTML = atob(blob);", "el.innerHTML = `<b>${esc(name)}</b>`;"),
    "html-from-value": ("el.innerHTML = html;", "el.innerHTML = art(title);"),
    "decode-literal": ('const s = atob("ZXZhbChjb2RlKQ==");', "const s = atob(b64);"),
    "base64": ('const p = "' + "QUJD" * 30 + '";', 'const p = "short";'),
    "hex": ('const k = "' + "ab" * 45 + '";', 'const k = "#ffaa00";'),
    "escapes": ('const s = "\\x65\\x76\\x61\\x6c\\x28\\x29";', 'const s = "\\n\\t";'),
    "invisible": ("const a = 1; \u202e// tricky", "const a = 1;"),
    "direction-mark": ("const a\u200f = 1;", "const a = 1;"),
    "cookie": ("const c = document.cookie;", 'const cookieLabel = tr("Cookies");'),
    "storage": ("localStorage.setItem(k, v);", "const stored = 1;"),
    "clipboard-read": ("navigator.clipboard.readText().then(go);", "navigator.clipboard.writeText(t);"),
    "mixed-script": ("f\u0435tch(u);", 'const LANGS = [["ca", "Català"], ["el", "Ελληνικά"]];'),
    "packed": ("x=" + "a+b;" * 600, "const x = 1; // " + "words and spaces " * 100),
    "secret-into-call": ("send(\"diagsend\", { value: ui.token });", 'send("refresh", { name: id });'),
    "bridge-command": ('send("runanything", { name: x });', 'send("refresh");'),
    "bridge-computed": ("send(cmdFromSomewhere, { name: x });", 'send(on ? "refresh" : "login", {});'),
}

# Patterns that BASE already uses, so their hits say "a new kind of call"
# only for the others.
PAGE = "crates/kiri/src/ui/index.html"


def flagged(line, path=PAGE, base=BASE):
    return {f.name for f in ps.scan(base, [(path, 1, line)])}


class Patterns(unittest.TestCase):
    def test_every_pattern_has_a_case(self):
        names = {p[0] for p in ps.PATTERNS} | {"mixed-script", "packed", "secret-into-call", "bridge-command", "bridge-computed"}
        self.assertEqual(names - set(CASES) - {"fetch"}, set())

    def test_each_pattern(self):
        for name, (bad, good) in CASES.items():
            with self.subTest(name=name):
                self.assertIn(name, flagged(bad), bad)
                self.assertNotIn(name, flagged(good), good)

    def test_fetch(self):
        base = {PAGE: "<script>\n</script>"}
        self.assertIn("fetch", flagged("fetch(u, { method: 'POST', body });", base=base))
        self.assertNotIn("fetch", flagged("const fetched = list;", base=base))

    def test_new_kind_of_call(self):
        base = {PAGE: "<script>\n</script>"}
        f = ps.scan(base, [(PAGE, 1, "fetch(u);")])
        self.assertTrue(f[0].new_kind)
        self.assertIn("new kind of call", f[0].why)
        f = ps.scan(BASE, [(PAGE, 1, "fetch(y);")])  # BASE has a fetch already
        self.assertFalse(f[0].new_kind)


class NotNew(unittest.TestCase):
    def test_a_moved_line_is_not_flagged(self):
        line = 'send("login", { name: server, value: { username: u, secret: tk.value } });'
        self.assertEqual(flagged("    " + line), set())  # same line, moved and indented

    def test_a_known_address_moved_is_not_flagged(self):
        self.assertEqual(flagged('link("https://github.com/libracoder/kiri/issues/new")'), set())

    def test_a_known_host_as_a_prefix_is_not_enough(self):
        self.assertIn("url", flagged('x("https://github.com/libracoder/kiri/issues/new.evil.example")'))

    def test_translations(self):
        lang = "crates/kiri/src/ui/lang/fa.json"
        self.assertEqual(flagged('"Applied {name}": "\u200f{name} \u0627\u0639\u0645\u0627\u0644 \u0634\u062f",', lang), set())
        self.assertIn("img-src", flagged('"Hi": "<img src=\\"https://x.example/p\\">",', lang))
        self.assertIn("invisible", flagged('"Hi": "\u202eolleh",', lang))


class Files(unittest.TestCase):
    def test_page_files(self):
        self.assertTrue(ps.is_page_file("crates/kiri/src/ui/index.html"))
        self.assertTrue(ps.is_page_file("crates/kiri/src/ui/icons.js"))
        self.assertTrue(ps.is_page_file("crates/kiri/src/ui/xterm/xterm.js"))
        self.assertTrue(ps.is_page_file("crates/kiri/src/ui/lang/de.json"))
        self.assertTrue(ps.is_page_file("crates/kiri-win/src/setup.html"))
        self.assertTrue(ps.is_page_file("crates/kiri/src/ui/new.txt", extra={"crates/kiri/src/ui/new.txt"}))
        self.assertFalse(ps.is_page_file("crates/kiri/src/ui/xterm/README.md"))
        self.assertFalse(ps.is_page_file("crates/kiri/src/ui/mod.rs"))

    def test_includes_found_in_the_assemblers(self):
        self.assertEqual(ps.INCLUDE.findall('const X: &str = include_str!("extra.js");'), ["extra.js"])


if __name__ == "__main__":
    unittest.main()
