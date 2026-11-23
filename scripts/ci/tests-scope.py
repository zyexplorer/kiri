#!/usr/bin/env python3
"""Which of the Tests workflow's jobs a pull request needs.

    python3 scripts/ci/tests-scope.py BASE HEAD     # run in the repository

prints, on stdout:

    linux=true|false           unit: the workspace's unit tests on Linux
    windows=true|false         windows-build: the Windows app and CLI compile
    mac=true|false             mac-build: the build and unit tests on a Mac
    scripts=true|false         scripts: the suites' and scripts' own checks
    suites_changed=true|false  the suites changed (the label run-tests runs them)

and, on stderr and in $GITHUB_STEP_SUMMARY when it is set, every changed file
with the jobs it starts and why.

A job starts when the change can break it, and nothing that can break it is
skipped. For Rust the code's own markers decide, not a list of files:

- File level: the module tree, from each crate's roots through `mod x;`
  (x.rs or x/mod.rs, or its #[path]), with the #[cfg(...)] on each `mod` and
  on every module above it. A file reached from several places (kiri-win's
  #[path] includes of crates/kiri/src files) takes the union. A crate counts
  for the jobs that build it (JOBS below, from the workflow's commands).
- Line level: a changed line inside an item, statement or block that
  carries #[cfg(...)] counts only for the systems that cfg allows.
- cfg predicates are worked out for linux, macos and windows; `test`,
  `feature = ...` and anything else this script does not know may be true
  or false, so the answer is never narrower than the truth.
- When in doubt, wider: a change to a cfg attribute itself (unless a whole
  #[cfg] item is added or removed), cfg_attr, include!, a macro_rules body,
  a build script, a file the scanner can't read, code that moves in or out
  of a #[cfg] item, a deleted or renamed file, Cargo.toml, Cargo.lock and
  any file this script does not know start every job the file can reach.
- Lines that are only comments (not doc comments) or blank start nothing.
- Files a crate pulls in with include_str!/include_bytes!/include!, or that
  a build script names in rerun-if-changed, count for the code that does it.
- Other files go by the short list in PATHS.

Standard library only. The workflow runs main's copy of this file, so a pull
request can't decide for itself that it needs no tests. Its tests:

    python3 -m unittest discover -s scripts/ci -p 'test_tests_scope.py'
"""

import difflib
import fnmatch
import os
import posixpath
import re
import subprocess
import sys

try:
    import tomllib
except ImportError:  # Python before 3.11: every crate counts for every job
    tomllib = None

SYSTEMS = ("linux", "windows", "mac")
ALL = frozenset(SYSTEMS)
NONE = frozenset()

# What each system's cfg knows. Anything else (test, debug_assertions,
# feature = "...", target_arch, ...) may be either.
TARGET = {
    "linux": {"target_os": "linux", "target_family": "unix", "unix": True, "windows": False},
    "mac": {"target_os": "macos", "target_family": "unix", "unix": True, "windows": False, "target_vendor": "apple"},
    "windows": {"target_os": "windows", "target_family": "windows", "unix": False, "windows": True},
}

# The crates each job builds, as the workflow's commands build them:
#   unit           cargo test --workspace --exclude kiri-init; cargo test -p kiri-init
#   mac-build      cargo test --workspace --exclude kiri-init
#   windows-build  cargo check --target x86_64-pc-windows-msvc -p kiri-win -p kiri
# with the path dependencies each needs on that system. Change this with them.
JOBS = {
    "linux": {"workspace": True, "exclude": ()},
    "mac": {"workspace": True, "exclude": ("kiri-init",)},
    "windows": {"workspace": False, "packages": ("kiri-win", "kiri")},
}

# Files that are not Rust: the first pattern that matches gives the jobs.
# `*` matches across `/`. Files Rust code includes also count for that code.
PATHS = [
    (("*.md", "docs/*", "LICENSE", "NOTICE", ".gitignore", "brand/*", ".github/ISSUE_TEMPLATE/*"), NONE, "words and pictures"),
    (("crates/kiri/src/ui/index.html", "crates/kiri/src/ui/lang/*", "crates/kiri/src/ui/*.js", "crates/kiri/src/ui/xterm/*",
      "crates/kiri-win/src/*.html", "tests/ui/*"), NONE, "the window's page: the UI tests workflow"),
    (("guest/kernel/*",), NONE, "Kiri's kernel: a runner runs its own kernel and can't start Kiri's VM"),
    (("tests/*", "bench/*"), NONE, "the suites: the label run-tests runs them"),
    ((".github/workflows/codeql.yml", ".github/workflows/perf.yml", ".github/workflows/ui-tests.yml", "scripts/perf*", "scripts/ui-*",
      "scripts/i18n-check.py", "scripts/dmg-*", "scripts/ci/tests-scope.py", "scripts/ci/test_tests_scope.py", "scripts/ci/tests-scope-replay.py",
      "scripts/ci/page-scan.py", "scripts/ci/test_page_scan.py"), NONE, "another workflow's or job's own file"),
    (("crates/kiri/kiri.entitlements", "xtask/dev-icon.swift"), frozenset({"mac"}), "the Mac app's packaging"),
    (("contrib/windows/*", "installer/windows/*"), frozenset({"windows"}), "the Windows app's packaging"),
    (("contrib/linux/*", "contrib/wsl/*", "packaging/linux/*", "scripts/kiri-linux.sh", "scripts/windows-engine.sh", "scripts/ci/linux-*"),
     frozenset({"linux"}), "Linux's packaging and what runs in WSL"),
]


def scripts_job(path):
    """The `scripts` job: the suites' and scripts' shell and Python files."""
    if path.startswith(("tests/", "scripts/")) and path.endswith((".sh", ".py")):
        return True
    return path == "tests/regressions.tsv" or path.startswith("tests/lib/")


def suite_file(path):
    return path.startswith("tests/") and not path.startswith("tests/ui/")


def names(ctx):
    return ", ".join(s for s in SYSTEMS if s in ctx) or "nothing"


# ---------------------------------------------------------------------------
# The scanner: Rust source to tokens. Comments go (doc comments stay: they
# are attributes and can hold doctests); strings, raw strings, chars and
# lifetimes are told apart so braces inside them don't count.


class ScanError(Exception):
    pass


class Tok:
    __slots__ = ("kind", "text", "line", "end")

    def __init__(self, kind, text, line, end):
        self.kind, self.text, self.line, self.end = kind, text, line, end

    def __repr__(self):
        return f"{self.kind}:{self.text!r}@{self.line}"


IDENT = re.compile(r"[^\W\d]\w*")
RAW_STR = re.compile(r'(?:b|c)?r(#*)"')
NUMBER = re.compile(r"\d[\w]*(?:\.\d[\w]*)?")


def tokenize(src):
    toks = []
    i, n, line = 0, len(src), 1
    while i < n:
        c = src[i]
        if c == "\n":
            line += 1
            i += 1
        elif c.isspace():
            i += 1
        elif src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            text = src[i:j]
            if (text.startswith("///") and not text.startswith("////")) or text.startswith("//!"):
                toks.append(Tok("doc", text, line, line))
            i = j
        elif src.startswith("/*", i):
            depth, j, start = 0, i, line
            while True:
                if j >= n:
                    raise ScanError(f"a block comment from line {start} never ends")
                if src.startswith("/*", j):
                    depth += 1
                    j += 2
                elif src.startswith("*/", j):
                    depth -= 1
                    j += 2
                    if depth == 0:
                        break
                else:
                    if src[j] == "\n":
                        line += 1
                    j += 1
            text = src[i:j]
            if (text.startswith("/**") and not text.startswith("/***") and text != "/**/") or text.startswith("/*!"):
                toks.append(Tok("doc", text, start, line))
            i = j
        elif (m := RAW_STR.match(src, i)) is not None:
            close = '"' + m.group(1)
            j = src.find(close, m.end())
            if j < 0:
                raise ScanError(f"a raw string from line {line} never ends")
            text = src[i:j + len(close)]
            toks.append(Tok("str", text, line, line + text.count("\n")))
            line += text.count("\n")
            i = j + len(close)
        elif c == '"' or (c in "bc" and src.startswith('"', i + 1)):
            j = i + (1 if c == '"' else 2)
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            if j >= n:
                raise ScanError(f"a string from line {line} never ends")
            text = src[i:j + 1]
            toks.append(Tok("str", text, line, line + text.count("\n")))
            line += text.count("\n")
            i = j + 1
        elif c == "'" or (c == "b" and src.startswith("'", i + 1)):
            q = i if c == "'" else i + 1
            if src.startswith("\\", q + 1):
                j = src.find("'", q + 3)
                if j < 0 or "\n" in src[q:j]:
                    raise ScanError(f"a char literal on line {line} never ends")
                toks.append(Tok("char", src[i:j + 1], line, line))
                i = j + 1
            elif q + 2 < n and src[q + 2] == "'" and src[q + 1] != "\n":
                toks.append(Tok("char", src[i:q + 3], line, line))
                i = q + 3
            elif c == "'" and (m := IDENT.match(src, q + 1)) is not None:
                toks.append(Tok("life", src[i:m.end()], line, line))
                i = m.end()
            else:
                raise ScanError(f"a quote on line {line} this scanner can't read")
        elif c == "r" and src.startswith("#", i + 1) and (m := IDENT.match(src, i + 2)) is not None:
            toks.append(Tok("ident", src[i:m.end()], line, line))
            i = m.end()
        elif (m := IDENT.match(src, i)) is not None:
            toks.append(Tok("ident", m.group(), line, line))
            i = m.end()
        elif c.isdigit():
            m = NUMBER.match(src, i)
            toks.append(Tok("num", m.group(), line, line))
            i = m.end()
        else:
            toks.append(Tok("punct", c, line, line))
            i += 1
    return toks


OPEN, CLOSE = "([{", ")]}"


def match_brackets(toks):
    match, stack = {}, []
    for k, t in enumerate(toks):
        if t.kind != "punct":
            continue
        if t.text in OPEN:
            stack.append(k)
        elif t.text in CLOSE:
            if not stack or OPEN.index(toks[stack[-1]].text) != CLOSE.index(t.text):
                raise ScanError(f"an unbalanced {t.text!r} on line {t.line}")
            o = stack.pop()
            match[o], match[k] = k, o
    if stack:
        raise ScanError(f"an unclosed {toks[stack[-1]].text!r} from line {toks[stack[-1]].line}")
    return match


def str_value(tok):
    """A string literal's text, for paths (no escapes beyond \\\\ and \\")."""
    t = tok.text
    m = RAW_STR.match(t)
    if m:
        return t[m.end():len(t) - 1 - len(m.group(1))]
    t = t[1:] if t[0] in "bc" else t
    return t[1:-1].replace("\\\\", "\\").replace('\\"', '"')


# ---------------------------------------------------------------------------
# cfg predicates.


class CfgError(Exception):
    pass


def parse_pred(toks, i, j):
    """toks[i:j] is one predicate: returns (tree, next index)."""
    if i >= j or toks[i].kind != "ident":
        raise CfgError("expected a name")
    name = toks[i].text
    if i + 1 < j and toks[i + 1].text == "(" and name in ("all", "any", "not"):
        k, args = i + 2, []
        while k < j and toks[k].text != ")":
            arg, k = parse_pred(toks, k, j)
            args.append(arg)
            if k < j and toks[k].text == ",":
                k += 1
            elif k < j and toks[k].text != ")":
                raise CfgError("expected , or )")
        if k >= j:
            raise CfgError("unclosed (")
        if name == "not" and len(args) != 1:
            raise CfgError("not() takes one predicate")
        return (name, args), k + 1
    if i + 2 < j and toks[i + 1].text == "=" and toks[i + 2].kind == "str":
        return ("kv", name, str_value(toks[i + 2])), i + 3
    if i + 1 < j and toks[i + 1].text not in (",", ")"):
        raise CfgError(f"unexpected {toks[i + 1].text!r}")
    return ("name", name), i + 1


def eval_pred(tree, system):
    """True, False, or None (could be either) on that system."""
    known = TARGET[system]
    kind = tree[0]
    if kind == "name":
        v = known.get(tree[1]) if tree[1] in ("unix", "windows") else None
        return v
    if kind == "kv":
        key, value = tree[1], tree[2]
        if key in ("target_os", "target_family") or key in known:
            return known.get(key) == value
        return None
    vals = [eval_pred(a, system) for a in tree[1]]
    if kind == "not":
        return None if vals[0] is None else not vals[0]
    if kind == "all":
        return False if False in vals else (None if None in vals else True)
    return True if True in vals else (None if None in vals else False)  # any


def cfg_systems(tree):
    return frozenset(s for s in SYSTEMS if eval_pred(tree, s) is not False)


def cfg_from_text(text):
    """A cfg predicate as written in Cargo.toml (`cfg(unix)`), or None."""
    toks = tokenize(text)
    if len(toks) < 4 or toks[0].text != "cfg" or toks[1].text != "(" or toks[-1].text != ")":
        return None
    tree, k = parse_pred(toks, 2, len(toks) - 1)
    if k != len(toks) - 1:
        raise CfgError("trailing tokens")
    return cfg_systems(tree)


# ---------------------------------------------------------------------------
# Structure: which tokens each #[cfg] covers, the `mod` declarations and the
# files included.

F_CFG, F_CFGATTR, F_MACRO, F_INCLUDE = 1, 2, 4, 8
FLAG_WHY = {
    F_CFG: "changes a #[cfg] attribute",
    F_CFGATTR: "touches cfg_attr",
    F_MACRO: "is inside a macro_rules! body",
    F_INCLUDE: "touches include!",
}
# Words that start an item; such an element ends at `;` or its body's `}`,
# never at a `,` (which can only be inside its generics or where clause).
ITEM_KW = {"fn", "impl", "struct", "enum", "union", "trait", "type", "const", "static", "mod", "use", "extern", "unsafe",
           "async", "macro_rules", "let"}
NOT_MACRO = {"if", "while", "match", "return", "in", "let", "else", "for", "loop", "break", "continue", "as", "mut", "move", "yield"}


class Attr:
    __slots__ = ("start", "end", "cfg", "cfg_text", "cfg_attr", "paths")

    def __init__(self, start, end):
        self.start, self.end = start, end
        self.cfg = None  # frozenset of systems when this is #[cfg(...)]
        self.cfg_text = None
        self.cfg_attr = False
        self.paths = []  # #[path = "..."], and cfg_attr(..., path = "...")


class ModDecl:
    __slots__ = ("name", "ctx", "paths", "inline", "line", "why")

    def __init__(self, name, ctx, paths, inline, line, why):
        self.name, self.ctx, self.paths, self.inline, self.line, self.why = name, ctx, paths, inline, line, why


class Parsed:
    """One Rust file: per line, the systems of each token on it."""

    def __init__(self, text):
        self.error = None
        self.line_ctx = {}  # line -> tuple of each token's systems
        self.line_flags = {}  # line -> F_* bits
        self.line_why = {}  # line -> set of the innermost #[cfg] items' descriptions
        self.cfg_elems = {}  # line -> [(first line, last line, definitive)] of #[cfg] items whose attribute is on it
        self.mods = []
        self.includes = []  # (macro, path, systems, line)
        self.rerun = []  # rerun-if-changed paths, for build scripts
        try:
            self._parse(text)
        except (ScanError, RecursionError) as e:
            self.error = str(e) if isinstance(e, ScanError) else "nesting too deep"
            self.line_ctx, self.line_flags, self.line_why, self.cfg_elems = {}, {}, {}, {}
            self.mods, self.includes = [], []

    # -- the walk ------------------------------------------------------------
    def _parse(self, text):
        self.toks = toks = tokenize(text)
        self.match = match_brackets(toks)
        self.ctx = [ALL] * len(toks)
        self.why = [None] * len(toks)
        self.flags = [0] * len(toks)
        self.elem_of = {}
        self._walk(0, len(toks), ALL, None, ())
        for k, t in enumerate(toks):
            if t.kind == "ident" and t.text in ("include", "include_str", "include_bytes") and self._at(k + 1, "!") \
                    and k + 2 < len(toks) and toks[k + 2].text in OPEN:
                close = self.match[k + 2]
                if t.text == "include":
                    for m in range(k, close + 1):
                        self.flags[m] |= F_INCLUDE
                if close == k + 4 and toks[k + 3].kind == "str":
                    self.includes.append((t.text, str_value(toks[k + 3]), self.ctx[k], t.line))
            if t.kind == "str" and "rerun-if-changed=" in t.text:
                self.rerun.append(str_value(t).split("rerun-if-changed=", 1)[1])
        for k, t in enumerate(toks):
            for ln in range(t.line, t.end + 1):
                self.line_ctx.setdefault(ln, []).append(self.ctx[k])
                if self.flags[k]:
                    self.line_flags[ln] = self.line_flags.get(ln, 0) | self.flags[k]
                if self.why[k]:
                    self.line_why.setdefault(ln, set()).add(self.why[k])
                if k in self.elem_of:
                    self.cfg_elems.setdefault(ln, []).append(self.elem_of[k])
        self.line_ctx = {ln: tuple(v) for ln, v in self.line_ctx.items()}
        del self.toks, self.match, self.ctx, self.why, self.flags, self.elem_of

    def _at(self, k, text):
        return k < len(self.toks) and self.toks[k].kind == "punct" and self.toks[k].text == text

    def _attr(self, k, inner):
        toks = self.toks
        if not self._at(k, "#"):
            return None
        o = k + 2 if inner else k + 1
        if inner and not self._at(k + 1, "!"):
            return None
        if not self._at(o, "["):
            return None
        close = self.match[o]
        a = Attr(k, close + 1)
        body = range(o + 1, close)
        if len(body) and toks[o + 1].kind == "ident":
            head = toks[o + 1].text
            if head == "cfg" and self._at(o + 2, "(") and self.match[o + 2] == close - 1:
                try:
                    tree, nxt = parse_pred(toks, o + 3, close - 1)
                    if nxt != close - 1:
                        raise CfgError("trailing tokens")
                    a.cfg = cfg_systems(tree)
                except CfgError:
                    a.cfg = None  # can't read it: no narrowing, and touching it is shared
                a.cfg_text = "#[cfg(" + _join(toks[o + 3:close - 1]) + ")]"
                for m in range(k, close + 1):
                    self.flags[m] |= F_CFG
            elif head == "cfg":
                for m in range(k, close + 1):
                    self.flags[m] |= F_CFG
            elif head == "cfg_attr":
                a.cfg_attr = True
                for m in range(k, close + 1):
                    self.flags[m] |= F_CFGATTR
                for m in body:
                    if toks[m].text == "path" and self._at(m + 1, "=") and m + 2 < close and toks[m + 2].kind == "str":
                        a.paths.append(str_value(toks[m + 2]))
            elif head == "path" and self._at(o + 2, "=") and o + 3 < close and toks[o + 3].kind == "str":
                a.paths.append(str_value(toks[o + 3]))
        return a

    def _skip_vis(self, k, hi):
        toks = self.toks
        if k < hi and toks[k].text == "pub":
            k += 1
            if self._at(k, "("):
                k = self.match[k] + 1
        return k

    def _end(self, head, hi):
        """Where the element starting at `head` ends, and whether that end is
        certain (an item's `;` or body). When unsure it ends early: the rest
        then counts for the enclosing code, which is wider."""
        toks = self.toks
        j = self._skip_vis(head, hi)
        first = toks[j].text if j < hi and toks[j].kind == "ident" else None
        nxt = toks[j + 1].text if j + 1 < hi else None
        semi_only = first in ("use", "let", "static", "type") or (first == "const" and nxt not in ("fn", "unsafe", "async", "extern", "{"))
        item = first in ITEM_KW
        k = head
        while k < hi:
            t = toks[k]
            if t.kind == "punct":
                if t.text in OPEN:
                    close = self.match[k]
                    if t.text == "{" and not semi_only:
                        return close + 1, item and first != "let"
                    k = close + 1
                    continue
                if t.text == ";":
                    return k + 1, item
                if t.text == "," and not (item or semi_only):
                    return k + 1, False
            k += 1
        return hi, False

    def _label(self, head, end):
        toks = self.toks
        j = self._skip_vis(head, end)
        while j < end and toks[j].text in ("unsafe", "async", "extern", "default") and toks[j].kind == "ident":
            j += 1 + (j + 1 < end and toks[j + 1].kind == "str")
        if j >= end:
            return "item"
        w = toks[j].text
        if w == "const" and j + 1 < end and toks[j + 1].text == "fn":
            j, w = j + 1, "fn"
        if w in ("fn", "struct", "enum", "trait", "mod", "type", "const", "static", "union") and j + 1 < end:
            return f"{w} {toks[j + 1].text}"
        if w == "macro_rules":
            return f"macro_rules! {toks[j + 2].text}" if j + 2 < end else w
        if w in ("impl", "use", "let"):
            stop = next((m for m in range(j, end) if toks[m].text in ("{", ";", "=")), end)
            s = _join(toks[j:stop])
            return s if len(s) <= 60 else s[:57] + "..."
        s = _join(toks[j:min(end, j + 6)])
        return "statement " + (s if len(s) <= 40 else s[:37] + "...")

    def _set(self, lo, hi, c, why):
        for m in range(lo, hi):
            self.ctx[m] = c
            self.why[m] = why

    def _walk(self, lo, hi, c, why, inline):
        toks = self.toks
        i = lo
        # Inner attributes and inner doc comments: they apply to the whole group.
        while i < hi:
            if toks[i].kind == "doc" and toks[i].text.startswith(("//!", "/*!")):
                i += 1
                continue
            a = self._attr(i, inner=True)
            if a is None:
                break
            if a.cfg is not None:
                c = c & a.cfg
                why = a.cfg_text.replace("#[", "#![", 1) + " (the whole " + ("file" if lo == 0 else "block") + ")"
            i = a.end
        self._set(lo, hi, c, why)
        while i < hi:
            start = i
            attrs = []
            while i < hi:
                if toks[i].kind == "doc":
                    i += 1
                    continue
                a = self._attr(i, inner=False)
                if a is None:
                    break
                attrs.append(a)
                i = a.end
            head = i
            end, definitive = self._end(head, hi) if head < hi else (hi, False)
            end = max(end, start + 1)
            ec, ew = c, why
            cfgs = [a for a in attrs if a.cfg is not None]
            if cfgs:
                for a in cfgs:
                    ec = ec & a.cfg
                ew = " ".join(a.cfg_text for a in cfgs) + " " + self._label(head, end)
            self._set(start, end, ec, ew)
            if any(self.flags[a.start] & F_CFG for a in attrs):
                first, last = toks[start].line, toks[end - 1].end
                for a in attrs:
                    if self.flags[a.start] & F_CFG:
                        for m in range(a.start, a.end):
                            self.elem_of[m] = (first, last, definitive)
            # mod declarations, and the name of an inline module's body
            body_of = None
            j = self._skip_vis(head, end)
            if j + 2 < len(toks) and toks[j].text == "mod" and toks[j].kind == "ident" and toks[j + 1].kind == "ident":
                name = toks[j + 1].text
                if self._at(j + 2, ";"):
                    paths = [p for a in attrs for p in a.paths]
                    self.mods.append(ModDecl(name, ec, paths, inline, toks[j].line, ew))
                elif self._at(j + 2, "{"):
                    body_of = (j + 2, name)
            k = head
            while k < end:
                t = toks[k]
                if t.kind == "punct" and t.text in OPEN:
                    close = self.match[k]
                    if k >= 3 and toks[k - 1].kind == "ident" and self._at(k - 2, "!") and toks[k - 3].text == "macro_rules":
                        for m in range(k, close + 1):
                            self.flags[m] |= F_MACRO
                    elif k >= 2 and self._at(k - 1, "!") and toks[k - 2].kind == "ident" and toks[k - 2].text not in NOT_MACRO:
                        pass  # a macro's input: its attributes are the macro's to apply
                    else:
                        sub = inline + (body_of[1],) if body_of and body_of[0] == k else inline
                        self._walk(k + 1, close, ec, ew, sub)
                    k = close + 1
                else:
                    k += 1
            i = end


def _join(toks):
    out = ""
    for t in toks:
        if out and (t.kind in ("ident", "num", "str", "char", "life") and (out[-1].isalnum() or out[-1] in "_\"'")):
            out += " "
        elif out and t.text == "=" or out.endswith((",", "=")):
            out += " "
        out += t.text
    return out


# ---------------------------------------------------------------------------
# Snapshots of the repository: a commit, or (in tests) a dict.


class Snapshot:
    def __init__(self, files, read):
        self.files = files  # path -> a key that changes when the content does
        self._read = read

    def read(self, path):
        if path not in self.files:
            return None
        return self._read(path)


def dict_snapshot(files):
    return Snapshot({p: ("text", hash(t)) for p, t in files.items()}, files.get)


class Git:
    def __init__(self, cwd=None):
        self.cwd = cwd
        self._cat = None

    def run(self, *args):
        return subprocess.run(("git",) + args, cwd=self.cwd, check=True, stdout=subprocess.PIPE).stdout

    def blob(self, sha):
        if self._cat is None:
            self._cat = subprocess.Popen(("git", "cat-file", "--batch"), cwd=self.cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self._cat.stdin.write(sha.encode() + b"\n")
        self._cat.stdin.flush()
        head = self._cat.stdout.readline().split()
        if len(head) < 3 or head[1] == b"missing":
            raise RuntimeError(f"git has no object {sha}")
        data = self._cat.stdout.read(int(head[2]))
        self._cat.stdout.read(1)
        return data

    def snapshot(self, commit):
        files = {}
        for entry in self.run("ls-tree", "-r", "-z", commit).split(b"\0"):
            if not entry:
                continue
            meta, path = entry.split(b"\t", 1)
            mode, _, sha = meta.split()
            files[path.decode("utf-8", "surrogateescape")] = (mode.decode(), sha.decode())

        def read(path):
            mode, sha = files[path]
            if mode in ("120000", "160000"):
                return None
            return self.blob(sha).decode("utf-8", "replace")

        return Snapshot(files, read)


_PARSED = {}


def parsed(snap, path):
    key = snap.files.get(path)
    if key is None:
        return None
    if key not in _PARSED:
        text = snap.read(path)
        _PARSED[key] = Parsed(text) if text is not None else None
    return _PARSED[key]


# ---------------------------------------------------------------------------
# The module tree of every crate, and which jobs build each file.


def _toml(snap, path):
    if tomllib is None:
        return None
    text = snap.read(path)
    if text is None:
        return None
    try:
        return tomllib.loads(text)
    except (tomllib.TOMLDecodeError, ValueError):
        return None


class Tree:
    def __init__(self, snap):
        self.snap = snap
        self.files = {}  # .rs path -> systems
        self.how = {}  # .rs path -> how it is reached (first few ways)
        self.whole = set()  # build scripts and include!d Rust: no line-level narrowing
        self.included = {}  # path -> systems, for files Rust includes or build scripts read
        self.inc_how = {}
        self.notes = []
        self._dirs = {}  # path -> set of directories its `mod x;` look in
        self._crates()
        self._walk()

    # -- crates and the jobs that build them -----------------------------------
    def _crates(self):
        snap = self.snap
        manifests = {p[: -len("/Cargo.toml")] for p in snap.files if p.endswith("/Cargo.toml")}
        root = _toml(snap, "Cargo.toml")
        members = set()
        if root and isinstance(root.get("workspace"), dict):
            ws = root["workspace"]
            for pat in ws.get("members", []):
                members |= {d for d in manifests if fnmatch.fnmatchcase(d, pat) and d.count("/") == pat.count("/")}
            for pat in ws.get("exclude", []):
                members -= {d for d in manifests if fnmatch.fnmatchcase(d, pat)}
        if not members:
            members = manifests
            self.notes.append("the workspace's members could not be read: every crate counts as built by every job")
        if "Cargo.toml" in snap.files and root and "package" in root:
            members.add("")
        self.crates = {}  # dir -> (name, manifest)
        for d in sorted(members):
            man = _toml(snap, f"{d}/Cargo.toml" if d else "Cargo.toml")
            name = (man or {}).get("package", {}).get("name", d)
            self.crates[d] = (name, man)
        by_name = {name: d for d, (name, _) in self.crates.items()}

        def deps(d, system):
            man = self.crates[d][1]
            if man is None:
                return set(self.crates)  # can't read it: it may need anything
            out = set()
            tables = [man]
            for key, tgt in (man.get("target") or {}).items():
                try:
                    allowed = cfg_from_text(key) if key.startswith("cfg(") else ALL
                except (CfgError, ScanError):
                    allowed = ALL
                if allowed is None or system in allowed:
                    tables.append(tgt)
            for t in tables:
                for sec in ("dependencies", "dev-dependencies", "build-dependencies"):
                    for name, spec in (t.get(sec) or {}).items():
                        if isinstance(spec, dict) and "path" in spec:
                            p = posixpath.normpath(posixpath.join(d, spec["path"])) if d else posixpath.normpath(spec["path"])
                            out.add(p if p in self.crates else by_name.get(spec.get("package", name), p))
            return {p for p in out if p in self.crates}

        self.crate_sys = {d: set() for d in self.crates}
        for system, job in JOBS.items():
            if tomllib is None or not root:
                todo = set(self.crates)
            elif job["workspace"]:
                todo = {d for d, (name, _) in self.crates.items() if name not in job["exclude"]}
            else:
                todo = {by_name[n] for n in job["packages"] if n in by_name}
            seen = set()
            while todo:
                d = todo.pop()
                if d in seen:
                    continue
                seen.add(d)
                todo |= deps(d, system)
            for d in seen:
                self.crate_sys[d].add(system)
        self.crate_sys = {d: frozenset(s) for d, s in self.crate_sys.items()}

    def _roots(self, d):
        files = self.snap.files
        pre = f"{d}/" if d else ""
        man = self.crates[d][1] or {}
        roots = []
        for p in (f"{pre}src/lib.rs", f"{pre}src/main.rs"):
            if p in files:
                roots.append(p)
        for p in files:
            rest = p[len(pre):] if p.startswith(pre) else None
            if rest is None or not rest.endswith(".rs"):
                continue
            parts = rest.split("/")
            if (len(parts) == 3 and parts[0] == "src" and parts[1] == "bin") or \
                    (len(parts) == 4 and parts[:2] == ["src", "bin"] and parts[3] == "main.rs") or \
                    (len(parts) == 2 and parts[0] in ("tests", "benches", "examples")) or \
                    (len(parts) == 3 and parts[0] in ("tests", "benches", "examples") and parts[2] == "main.rs"):
                roots.append(p)
        for key in ("lib", "bin", "test", "bench", "example"):
            v = man.get(key)
            for t in (v if isinstance(v, list) else [v] if isinstance(v, dict) else []):
                if isinstance(t, dict) and isinstance(t.get("path"), str):
                    roots.append(posixpath.normpath(pre + t["path"]))
        build = (man.get("package") or {}).get("build")
        builds = [posixpath.normpath(pre + build)] if isinstance(build, str) else [f"{pre}build.rs"]
        return roots, [b for b in builds if b in files]

    # -- the walk ----------------------------------------------------------------
    def _walk(self):
        work = []
        for d, (name, _) in self.crates.items():
            sys_ = self.crate_sys[d]
            roots, builds = self._roots(d)
            for r in roots:
                work.append((r, sys_, f"a root of crate {name}", {posixpath.dirname(r)}, False))
            for b in builds:
                work.append((b, sys_, f"the build script of crate {name}", {posixpath.dirname(b)}, True))
                pb = parsed(self.snap, b)
                for rel in (pb.rerun if pb else []):
                    p = posixpath.normpath(posixpath.join(posixpath.dirname(b), rel))
                    self._include(p, sys_, f"{b} reads it (rerun-if-changed)")
        done = {}
        while work:
            path, ctx, how, dirs, whole = work.pop()
            if path not in self.snap.files:
                continue
            old_ctx, old_dirs, old_whole = done.get(path, (NONE, set(), False))
            new_ctx, new_dirs, new_whole = old_ctx | ctx, old_dirs | dirs, old_whole or whole
            self.how.setdefault(path, [])
            if len(self.how[path]) < 3 and how not in self.how[path]:
                self.how[path].append(how)
            if (new_ctx, new_dirs, new_whole) == (old_ctx, old_dirs, old_whole):
                continue
            done[path] = (new_ctx, new_dirs, new_whole)
            self.files[path] = new_ctx
            if new_whole:
                self.whole.add(path)
            p = parsed(self.snap, path)
            if p is None:
                continue
            if p.error:
                self.notes.append(f"{path}: the scanner can't read it ({p.error}); modules it declares are not followed")
                continue
            here = posixpath.dirname(path)
            for m in p.mods:
                mctx = new_ctx & m.ctx
                if not mctx:
                    continue
                cands = set()
                for base in new_dirs:
                    inner = posixpath.join(base, *m.inline) if m.inline else base
                    # A #[path] is relative to this file's folder (inside an
                    # inline module, to that module's folder): both are tried.
                    for rel in m.paths:
                        cands.add((posixpath.normpath(posixpath.join(here, rel)), True))
                        cands.add((posixpath.normpath(posixpath.join(inner, rel)), True))
                    # The plain x.rs or x/mod.rs too, even with a path: a
                    # cfg_attr(..., path = ...) may not apply. Wider, never narrower.
                    cands.add((posixpath.normpath(posixpath.join(inner, m.name + ".rs")), False))
                    cands.add((posixpath.normpath(posixpath.join(inner, m.name, "mod.rs")), False))
                found = False
                for c, by_path in sorted(cands):
                    if c not in self.snap.files:
                        continue
                    found = True
                    if by_path:
                        cdirs = {posixpath.dirname(c), posixpath.splitext(c)[0]}
                    elif c.endswith("/mod.rs"):
                        cdirs = {posixpath.dirname(c)}
                    else:
                        cdirs = {posixpath.splitext(c)[0]}
                    cfg = f" under {m.why}" if m.why else ""
                    work.append((c, mctx, f"{path}:{m.line} mod {m.name}{' (#[path])' if by_path else ''}{cfg}", cdirs, new_whole))
                if not found:
                    self.notes.append(f"{path}:{m.line}: `mod {m.name};` names no file this script can find")
            for macro, rel, ictx, line in p.includes:
                target = posixpath.normpath(posixpath.join(here, rel))
                ictx = new_ctx & ictx
                if not ictx:
                    continue
                if macro == "include" and target.endswith(".rs"):
                    work.append((target, ictx, f"{path}:{line} include!", {posixpath.dirname(target)}, True))
                self._include(target, ictx, f"{path}:{line} {macro}!")

    def _include(self, path, ctx, how):
        self.included[path] = self.included.get(path, NONE) | ctx
        lst = self.inc_how.setdefault(path, [])
        if len(lst) < 3 and how not in lst:
            lst.append(how)


# ---------------------------------------------------------------------------
# One changed Rust file, line by line.


def rust_hunks(pa, pb, a_text, b_text, file_ctx):
    """Systems a modified Rust file's change can reach, with the reasons."""
    if pa.error or pb.error:
        return file_ctx, [f"the scanner can't read it ({pa.error or pb.error}): {names(file_ctx)}"]
    a, b = a_text.split("\n"), b_text.split("\n")
    sm = difflib.SequenceMatcher(None, a, b, autojunk=False)
    ops = sm.get_opcodes()
    for tag, i1, i2, j1, j2 in ops:
        if tag != "equal":
            continue
        for x, y in zip(range(i1 + 1, i2 + 1), range(j1 + 1, j2 + 1)):
            if pa.line_ctx.get(x, ()) != pb.line_ctx.get(y, ()):
                return file_ctx, [f"line {y} (was {x}) is unchanged but now counts for other systems: code moved in or out of a #[cfg] "
                                  f"item, or a string or comment opened: {names(file_ctx)}"]
    total, reasons = NONE, []
    for tag, i1, i2, j1, j2 in ops:
        if tag == "equal":
            continue
        sides = ((pa, range(i1 + 1, i2 + 1)), (pb, range(j1 + 1, j2 + 1)))
        ctx, flags, whys, code = NONE, 0, set(), False
        wide = None
        for p, lines in sides:
            lineset = set(lines)
            for ln in lines:
                toks = p.line_ctx.get(ln)
                if toks:
                    code = True
                    for t in toks:
                        ctx |= t
                f = p.line_flags.get(ln, 0)
                flags |= f
                whys |= p.line_why.get(ln, set())
                if f & F_CFG:
                    for first, last, definitive in p.cfg_elems.get(ln, [(0, 0, False)]):
                        if not definitive or not set(range(first, last + 1)) <= lineset:
                            wide = wide or FLAG_WHY[F_CFG]
                for bit in (F_CFGATTR, F_MACRO, F_INCLUDE):
                    if f & bit:
                        wide = wide or FLAG_WHY[bit]
        where = f"lines {j1 + 1}-{j2}" if j2 > j1 + 1 else f"line {j1 + 1}" if j2 > j1 else f"lines {i1 + 1}-{i2} removed" if i2 > i1 + 1 else f"line {i1 + 1} removed"
        if wide:
            hs = file_ctx
            reasons.append(f"{where}: {wide}: {names(hs)}")
        elif not code:
            hs = NONE
            reasons.append(f"{where}: comments or blank lines only: nothing")
        else:
            hs = ctx & file_ctx
            if whys:
                reasons.append(f"{where}: inside {'; '.join(sorted(whys))}: {names(hs)}")
            else:
                reasons.append(f"{where}: {names(hs)}")
        total |= hs
    return total, reasons


# ---------------------------------------------------------------------------
# A whole change.


class Change:
    def __init__(self, path, status, systems, reasons):
        self.path, self.status, self.systems, self.reasons = path, status, systems, reasons


def scope(base, head):
    """Every changed file's systems and reasons, between two snapshots."""
    changed = sorted(p for p in set(base.files) | set(head.files) if base.files.get(p) != head.files.get(p))
    trees = None
    out = []
    for path in changed:
        status = "added" if path not in base.files else "deleted" if path not in head.files else "modified"
        if trees is None:
            trees = (Tree(base), Tree(head))
        tb, th = trees
        systems, reasons, known = NONE, [], False
        if path.endswith(".rs"):
            fc = tb.files.get(path, NONE) | th.files.get(path, NONE)
            how = (th.how.get(path) or tb.how.get(path) or [])[:2]
            reached = path in tb.files or path in th.files
            if reached:
                known = True
                via = "; ".join(how)
                if path in tb.whole or path in th.whole:
                    systems = fc
                    reasons.append(f"a build script or include!d by one ({via}): {names(fc)}")
                elif status != "modified":
                    systems = fc
                    reasons.append(f"{status} ({via}): {names(fc)}")
                else:
                    pa, pb = parsed(base, path), parsed(head, path)
                    if pa is None or pb is None:
                        systems = fc
                        reasons.append(f"not a plain file: {names(fc)}")
                    else:
                        if fc != ALL:
                            reasons.append(f"the file is built for {names(fc)} ({via})")
                        s, rs = rust_hunks(pa, pb, base.read(path), head.read(path), fc)
                        systems |= s
                        reasons += rs
        inc = tb.included.get(path, NONE) | th.included.get(path, NONE)
        if path in tb.included or path in th.included:
            known = True
            systems |= inc
            reasons.append(f"included by Rust code ({'; '.join((th.inc_how.get(path) or tb.inc_how.get(path))[:2])}): {names(inc)}")
        if not path.endswith(".rs"):
            for pats, rule, why in PATHS:
                if any(fnmatch.fnmatchcase(path, p) for p in pats):
                    known = True
                    systems |= rule
                    reasons.append(f"{why}: {names(rule)}")
                    break
        if not known:
            systems = ALL
            if path.endswith(".rs"):
                reasons.append("in no crate's module tree (a new crate, or a file nothing declares): every job")
            else:
                reasons.append("a file this script doesn't know: every job")
        out.append(Change(path, status, systems, reasons))
    notes = []
    if trees:
        for n in trees[0].notes + trees[1].notes:
            if n not in notes:
                notes.append(n)
    return out, notes


def outputs(changes):
    systems = NONE
    for c in changes:
        systems |= c.systems
    res = {s: s in systems for s in SYSTEMS}
    res["scripts"] = any(scripts_job(c.path) for c in changes)
    res["suites_changed"] = any(suite_file(c.path) for c in changes)
    return res


def report(changes, notes, res):
    lines = ["Jobs: " + ", ".join(f"{k} {'yes' if res[k] else 'no'}" for k in (*SYSTEMS, "scripts")), ""]
    for c in changes:
        lines.append(f"{c.path} ({c.status}) -> {names(c.systems)}")
        lines += [f"    {r}" for r in c.reasons]
    if notes:
        lines.append("")
        lines += [f"note: {n}" for n in notes]
    return "\n".join(lines)


def summary(changes, notes, res):
    job = {"linux": "unit", "windows": "windows-build", "mac": "mac-build"}
    out = ["### Which jobs this pull request starts", ""]
    out.append(" · ".join(f"{job[s]}: **{'yes' if res[s] else 'no'}**" for s in SYSTEMS) + f" · scripts: **{'yes' if res['scripts'] else 'no'}**")
    out += ["", "| file | starts | why |", "|---|---|---|"]
    for c in changes:
        why = "<br>".join(r.replace("|", "\\|") for r in c.reasons)
        out.append(f"| `{c.path}` ({c.status}) | {', '.join(job[s] for s in SYSTEMS if s in c.systems) or 'nothing'} | {why} |")
    if notes:
        out.append("")
        out += [f"- {n}" for n in notes]
    return "\n".join(out) + "\n"


def main(argv):
    if len(argv) != 3:
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    try:
        git = Git()
        changes, notes = scope(git.snapshot(argv[1]), git.snapshot(argv[2]))
        res = outputs(changes)
        text, md = report(changes, notes, res), summary(changes, notes, res)
    except Exception as e:  # never skip a job because this script failed
        import traceback
        traceback.print_exc()
        res = {"linux": True, "windows": True, "mac": True, "scripts": True, "suites_changed": False}
        text = md = f"tests-scope.py failed ({e!r}): every job runs\n"
    print(text, file=sys.stderr)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as f:
            f.write(md)
    for k, v in res.items():
        print(f"{k}={'true' if v else 'false'}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
