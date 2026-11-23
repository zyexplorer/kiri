#!/usr/bin/env python3
"""Flags lines a pull request adds to the window's page that need a careful
read: new addresses, new ways to reach the network or leave the page, code
that builds or hides code, reads of things worth stealing, and new commands
to the app. The page is JavaScript that ships to every user; tests don't
catch code that sends data out, a read does.

    python3 scripts/ci/page-scan.py BASE HEAD [--accepted]   # scan, in the repository
    python3 scripts/ci/page-scan.py --changed BASE HEAD      # prints page=true|false

Only ADDED lines are read (BASE..HEAD), and a line already in BASE's page
files, word for word, is not flagged: moving code is not new code. An
address already in BASE's page files is not new either.

Exits 1 when anything is flagged, unless --accepted (the workflow passes it
when a maintainer adds the label page-read). Writes a table to stdout and to
$GITHUB_STEP_SUMMARY when set. Standard library only; the workflow runs
main's copy. Its tests:

    python3 -m unittest discover -s scripts/ci -p 'test_page_scan.py'

What it can't catch: logic that misuses the existing bridge to the app with
commands the page already sends (for example a `container` command that
mounts the user's home folder), or harm spread over lines that each look
ordinary. Only a read finds those.
"""

import fnmatch
import os
import re
import subprocess
import sys
import unicodedata

# The files that end up in the page: index.html and what
# crates/kiri/src/ui/assemble.rs splices into it (icons.js, langstep.js, the
# vendored xterm.js), the translations (put into the page as strings, and
# often into innerHTML), and the Windows setup page. Any other file that
# assemble.rs, lang.rs or kiri-win's app.rs include_str!s is added from
# their own text (page_files below), so a new spliced file is not missed.
PAGE_FILES = (
    "crates/kiri/src/ui/*.html",
    "crates/kiri/src/ui/*.js",
    "crates/kiri/src/ui/xterm/*.js",
    "crates/kiri/src/ui/xterm/*.css",
    "crates/kiri/src/ui/lang/*",
    "crates/kiri-win/src/*.html",
)
ASSEMBLERS = ("crates/kiri/src/ui/assemble.rs", "crates/kiri/src/ui/lang.rs", "crates/kiri/src/ui/mod.rs", "crates/kiri-win/src/app.rs")
INCLUDE = re.compile(r'include_str!\(\s*"([^"]+)"\s*\)')

# ---------------------------------------------------------------------------
# The patterns: (name, group, regex, reason). One place, one line of reason
# each. Groups: address, network, code (builds or hides code), secret.

_NOT_LITERAL = r"""(?!\s*(?:"[^"$]*"|'[^']*'|`[^`$]*`)\s*[;,)]?\s*$)"""  # what follows `=` is not a plain literal
PATTERNS = [
    # -- addresses (flagged only when the address is not already in the page)
    ("url", "address", r"""\b(?:https?|wss?|ftp)://[^\s"'`<>)\\]+""", "a web address the page could talk to or open"),
    ("protocol-relative", "address", r"""(?<=["'`(=])\s*//[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+(?:[:/][^\s"'`)]*)?""", "a protocol-relative address (//host)"),
    ("ip", "address", r"""(?<![\w.])(?!0\.0\.0\.0|127\.|255\.255\.255\.255)(?:\d{1,3}\.){3}\d{1,3}(?![\w.])|\[(?:[0-9A-Fa-f]{0,4}:){2,}[0-9A-Fa-f]{0,4}\]""",
     "an IP address (not 0.0.0.0, 127.x or broadcast)"),
    ("data-blob", "address", r"""["'`(]\s*(?:data|blob):(?!\s)[^"'`)\s]{0,40}""", "a data: or blob: URL used as a source"),
    # -- reaching the network or leaving the page
    ("fetch", "network", r"""\bfetch\s*\(""", "fetch() reaches the network"),
    ("xhr-socket", "network", r"""\b(?:XMLHttpRequest|WebSocket|EventSource|WebTransport|RTCPeerConnection)\b""", "a network connection from the page"),
    ("beacon", "network", r"""\bsendBeacon\b|\bserviceWorker\b""", "sends data out in the background"),
    ("worker", "network", r"""\bnew\s+(?:Shared)?Worker\s*\(""", "a worker runs code the page loads"),
    ("new-image", "network", r"""\bnew\s+Image\s*\(""", "an image request can carry data out"),
    ("src-assign", "network", r"""\.(?:src|href|action|srcdoc)\s*=(?!=)""" + _NOT_LITERAL, "a source or link set from a value"),
    ("script-src", "network", r"""<script\b[^>]*\bsrc\s*=""", "loads a script from elsewhere"),
    ("frame", "network", r"""<(?:iframe|frame|object|embed|base|portal)\b""", "embeds or redirects to another page"),
    ("link-href", "network", r"""<link\b[^>]*\bhref\s*=""", "loads a stylesheet or resource from elsewhere"),
    ("img-src", "network", r"""<(?:img|video|audio|source)\b[^>]*\bsrc\s*=\s*\\?["']?\s*(?:https?:|//|\$\{)""", "an image or media from a non-local source"),
    ("css-import", "network", r"""@import\b""", "a stylesheet that loads another"),
    ("meta-refresh", "network", r"""http-equiv\s*=\s*["']?refresh""", "a redirect"),
    ("form-action", "network", r"""<form\b[^>]*\baction\s*=""", "a form that posts somewhere"),
    ("dynamic-import", "network", r"""\bimport\s*\((?!\s*["'][^"']*["']\s*\))""", "imports code from a computed place"),
    ("window-open", "network", r"""\bwindow\.open\s*\(""", "opens another window"),
    ("location", "network", r"""\blocation(?:\.href)?\s*=(?!=)|\blocation\.(?:assign|replace)\s*\(""", "navigates the page away"),
    ("post-message", "network", r"""\.postMessage\s*\((?<!messageHandlers\.kiri\.postMessage\()""", "a message to another window or origin (the app bridge is messageHandlers.kiri)"),
    # -- code that builds or hides code
    ("eval", "code", r"""\beval\s*\(""", "runs a string as code"),
    ("function-ctor", "code", r"""\bnew\s+Function\s*\(|(?<![\w.])Function\s*\(\s*["'`]""", "builds code from a string"),
    ("timer-string", "code", r"""\bset(?:Timeout|Interval)\s*\(\s*["'`]""", "a timer that runs a string as code"),
    ("document-write", "code", r"""\bdocument\.write(?:ln)?\s*\(""", "writes markup into the page"),
    ("html-from-outside", "code", r"""(?:\.(?:innerHTML|outerHTML)\s*\+?=|\binsertAdjacentHTML\s*\(\s*["'][^"']*["']\s*,)[^;]*?(?:\batob\s*\(|\bdecodeURI|\bunescape\s*\(|\.data\b|\blocation\b|\bdocument\.cookie|\blocalStorage|\.response(?:Text)?\b)""",
     "markup from decoded, stored or received data"),
    ("html-from-value", "code", r"""(?:\.(?:innerHTML|outerHTML)\s*\+?=|\binsertAdjacentHTML\s*\(\s*["'][^"']*["']\s*,)\s*(?![\s`"']|[A-Za-z_$][\w$]*\s*\(|$)""",
     "markup from a variable, not a template or one of the page's builders (which escape with esc())"),
    ("decode-literal", "code", r"""\b(?:atob|btoa|unescape|escape|decodeURIComponent|decodeURI)\s*\(\s*["'`]|String\.fromCharCode\s*\(\s*\d""", "decodes a literal: hidden text or code"),
    ("base64", "code", r"""["'`](?:data:[\w/+.-]+;base64,)?[A-Za-z0-9+/]{100,}={0,2}["'`]""", "a long base64 literal"),
    ("hex", "code", r"""["'`](?:[0-9a-fA-F]{2}){40,}["'`]""", "a long hex literal"),
    ("escapes", "code", r"""(?:\\x[0-9a-fA-F]{2}){6,}|(?:\\u(?:[0-9a-fA-F]{4}|\{[0-9a-fA-F]+\})){6,}""", "a run of escapes: hidden text"),
    # Trojan Source characters: they reorder or hide code. ZWNJ and ZWJ
    # (U+200C, U+200D) are left out: Indic and Persian words need them.
    ("invisible", "code", "[\\u00ad\\u180e\\u200b\\u202a-\\u202e\\u2060-\\u2064\\u2066-\\u2069\\ufeff]", "an invisible or bidirectional character"),
    # Direction marks (ALM, LRM, RLM): right-to-left translations need them
    # (not flagged in lang/*.json); code doesn't.
    ("direction-mark", "code", "[\\u061c\\u200e\\u200f]", "a direction mark in code"),
    # mixed-script names and packed lines are checked in code (see scan_line)
    # -- reading things worth stealing
    ("cookie", "secret", r"""\bdocument\.cookie\b""", "reads cookies"),
    ("storage", "secret", r"""\b(?:localStorage|sessionStorage|indexedDB)\b""", "browser storage"),
    ("clipboard-read", "secret", r"""\bclipboard\.read(?:Text)?\s*\(""", "reads the clipboard"),
]
# Words for what the page holds that is worth stealing: registry logins,
# tokens, passwords (the page's Sign in sends `login` with a secret).
SECRET_WORDS = re.compile(r"\b\w*(?:password|passwd|secret|token|credential|auth|cookie|apikey|api_key)\w*\b", re.I)
SINKS = re.compile(r"\b(?:send|fetch|sendBeacon|postMessage|open|WebSocket)\s*\(|\.src\s*=|location\s*=")
# Lines this long with this little space are minified or packed code.
PACKED_LEN, PACKED_SPACE = 1500, 0.06
# The bridge to the app: send("cmd", ...) (index.html), send('cmd', ...) and
# { cmd: '...' } (setup.html). New commands are flagged.
BRIDGE = re.compile(r"""\bsend\s*\(\s*(["'`])([A-Za-z_][\w-]*)\1|\bcmd\s*:\s*(["'`])([A-Za-z_][\w-]*)\3""")
BRIDGE_COMPUTED = re.compile(r"""\bsend\s*\(\s*(?!["'`])[^)\s]""")
BRIDGE_TERNARY = re.compile(r"""(["'`])([A-Za-z_][\w-]*)\1""")

COMPILED = [(n, g, re.compile(r), why) for n, g, r, why in PATTERNS]
WORD = re.compile(r"[^\W\d_]+")


def is_page_file(path, extra=()):
    return path in extra or any(fnmatch.fnmatchcase(path, p) for p in PAGE_FILES)


def mixed_script(line):
    """Words with ASCII letters and letters of another script together, such
    as `fetch` written with a Cyrillic e (U+0435): names that look alike. Accented Latin
    (Català, Français) is one script and passes."""
    out = []
    for m in WORD.finditer(line):
        w = m.group()
        if any(c.isascii() for c in w) and any(not c.isascii() and not unicodedata.name(c, "LATIN").startswith("LATIN") for c in w):
            out.append(w)
    return out


# Listed in the report, but don't fail the job: a new command needs its
# handler in the app's Rust code, which the same pull request changes and a
# reviewer reads anyway. The list tells them which commands to look for.
# The same for markup built from a variable: that is how this page draws its
# views (`el.innerHTML = html`); markup from decoded, stored or received data
# (html-from-outside) does fail.
INFO = {"bridge-command", "html-from-value"}


class Finding:
    def __init__(self, path, line, name, group, why, text, new_kind=False):
        self.path, self.line, self.name, self.group, self.why, self.text, self.new_kind = path, line, name, group, why, text, new_kind
        self.info = name in INFO


def scan_line(text, known, is_code=True):
    """[(name, group, why, match)] for one added line. `known` holds what
    the page already has: addresses, bridge commands and pattern names."""
    hits = []
    for name, group, rx, why in COMPILED:
        for m in rx.finditer(text):
            value = m.group().strip()
            if group == "address":
                value = value.rstrip(".,;:")
                if value in known["addresses"]:  # exact: a known host as a prefix is not enough
                    continue
            hits.append((name, group, why, value))
            break
    if is_code:
        mixed = mixed_script(text)
        if mixed:
            hits.append(("mixed-script", "code", "a name mixing scripts: it can look like another", ", ".join(mixed[:3])))
        if len(text) >= PACKED_LEN and text.count(" ") / len(text) < PACKED_SPACE:
            hits.append(("packed", "code", f"a {len(text)}-character line with almost no spaces: minified or packed code", text[:60]))
        if SECRET_WORDS.search(text) and SINKS.search(text):
            hits.append(("secret-into-call", "secret", "something named like a secret goes into a call", SECRET_WORDS.search(text).group()))
        for m in BRIDGE.finditer(text):
            cmd = m.group(2) or m.group(4)
            if cmd not in known["commands"]:
                hits.append(("bridge-command", "network", f"a new command to the app: {cmd}", cmd))
        for m in BRIDGE_COMPUTED.finditer(text):
            rest = text[m.start():]
            cmds = [c for _, c in BRIDGE_TERNARY.findall(rest.split(",")[0])]
            new = [c for c in cmds if c not in known["commands"]]
            if not cmds:
                hits.append(("bridge-computed", "network", "a command to the app that isn't written out", rest[:60]))
            elif new:
                hits.append(("bridge-command", "network", f"a new command to the app: {', '.join(new)}", ", ".join(new)))
    return hits


def knowledge(base_texts):
    """What BASE's page already has: its lines, addresses, bridge commands,
    and which pattern names occur in it."""
    lines, addresses, commands, kinds = set(), set(), set(), set()
    for text in base_texts:
        for line in text.split("\n"):
            s = line.strip()
            if s:
                lines.add(s)
        for name, group, rx, _ in COMPILED:
            for m in rx.finditer(text):
                if group == "address":
                    addresses.add(m.group().strip().rstrip(".,;:"))
                kinds.add(name)
        for m in BRIDGE.finditer(text):
            commands.add(m.group(2) or m.group(4))
        for m in BRIDGE_COMPUTED.finditer(text):
            commands.update(c for _, c in BRIDGE_TERNARY.findall(text[m.start():m.start() + 200].split(",")[0]))
    return {"lines": lines, "addresses": addresses, "commands": commands, "kinds": kinds}


def scan(base_files, added):
    """base_files: {path: text} of BASE's page files. added: [(path, line
    number, text)] of the lines the change adds to page files."""
    known = knowledge(base_files.values())
    out = []
    for path, num, text in added:
        if text.strip() in known["lines"]:
            continue  # moved, not new
        is_code = not path.endswith(".json")
        for name, group, why, value in scan_line(text, known, is_code):
            if name == "direction-mark" and not is_code:
                continue
            new_kind = group in ("network", "code", "secret") and name not in known["kinds"] and not name.startswith("bridge")
            out.append(Finding(path, num, name, group, why + (" (a new kind of call for this page)" if new_kind else ""), text, new_kind))
    return out


# ---------------------------------------------------------------------------
# git


def git(*args):
    return subprocess.run(("git",) + args, check=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL).stdout.decode("utf-8", "replace")


def page_files(rev):
    names = git("ls-tree", "-r", "--name-only", rev).split("\n")
    extra = set()
    for a in ASSEMBLERS:
        try:
            text = git("show", f"{rev}:{a}")
        except subprocess.CalledProcessError:
            continue
        for rel in INCLUDE.findall(text):
            extra.add(os.path.normpath(os.path.join(os.path.dirname(a), rel)))
    return {n for n in names if n and is_page_file(n, extra)}


def changed_page_files(base, head):
    files = page_files(base) | page_files(head)
    changed = [p for p in git("diff", "--name-only", "--no-renames", base, head).split("\n") if p]
    return [p for p in changed if p in files]


HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")


def added_lines(base, head, paths):
    if not paths:
        return []
    diff = git("-c", "core.quotePath=false", "diff", "-U0", "--no-color", "--no-ext-diff", "--no-renames", "--src-prefix=a/", "--dst-prefix=b/",
               base, head, "--", *paths)
    out, path, num = [], None, 0
    for line in diff.split("\n"):
        if line.startswith("+++ "):
            path = line[6:] if line.startswith("+++ b/") else None
        elif line.startswith("@@"):
            m = HUNK.match(line)
            num = int(m.group(1)) if m else 0
        elif line.startswith("+") and path:
            out.append((path, num, line[1:]))
            num += 1
    return out


def table(findings):
    rows = ["| where | what | line |", "|---|---|---|"]
    for f in findings:
        text = f.text.strip()
        text = text if len(text) <= 160 else text[:157] + "..."
        rows.append(f"| `{f.path}:{f.line}` | {f.why} | `{text.replace('|', '¦').replace('`', 'ˋ')}` |")
    return "\n".join(rows)


def main(argv):
    args = [a for a in argv[1:] if not a.startswith("--")]
    flags = {a for a in argv[1:] if a.startswith("--")}
    if len(args) != 2:
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    base, head = args
    if "--changed" in flags:
        print(f"page={'true' if changed_page_files(base, head) else 'false'}")
        return 0
    paths = changed_page_files(base, head)
    base_texts = {}
    for p in page_files(base):
        try:
            base_texts[p] = git("show", f"{base}:{p}")
        except subprocess.CalledProcessError:
            pass
    found = scan(base_texts, added_lines(base, head, paths))
    findings = [f for f in found if not f.info]
    info = [f for f in found if f.info]
    accepted = "--accepted" in flags
    extra = ("\n\nFor the reader, not a failure: new commands to the app (read their handlers in the Rust code)"
             " and markup built from a variable:\n\n" + table(info)) if info else ""
    if not findings:
        msg = f"Nothing flagged in the {len(paths)} page file(s) this pull request changes."
        body = msg + extra
    elif accepted:
        msg = f"{len(findings)} line(s) flagged, read and accepted by a maintainer (label page-read)."
        body = msg + "\n\n" + table(findings) + extra
    else:
        msg = "This pull request changes the window's page in a way that needs a careful read."
        body = msg + " A maintainer who has read it adds the label page-read.\n\n" + table(findings) + extra
    print(body)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as f:
            f.write("### The window's page\n\n" + body + "\n")
    return 1 if findings and not accepted else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
