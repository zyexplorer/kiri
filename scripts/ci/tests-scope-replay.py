#!/usr/bin/env python3
"""Replays tests-scope.py over main's history and checks its narrowing.

    python3 scripts/ci/tests-scope-replay.py [--count 150] [--ref origin/main] [--old REV]

For each of the last COUNT non-merge commits on REF it works out the jobs
the commit starts, with the old path list (scripts/ci/tests-scope.sh at REV,
when given) and with tests-scope.py, and prints a table. Then, for every
commit where tests-scope.py skips a system, it checks that system really is
unaffected: for that system the code that survives cfg-stripping, across
every file the system compiles, and every file that code includes, must be
the same before and after. The stripper below shares only the tokenizer and
the cfg evaluation with tests-scope.py; it finds items, modules and includes
its own way, and ends an item as early as it can (so it keeps more code,
which makes the check stricter). Prints UNSOUND and exits 1 if a skipped
system's code changed.
"""
import argparse
import importlib.util
import os
import posixpath
import subprocess
import sys

_here = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("tests_scope", os.path.join(_here, "tests-scope.py"))
ts = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ts)


# ---------------------------------------------------------------------------
# An independent cfg-stripper.


def strip(text, system):
    """The token texts `system` compiles, plus its `mod` declarations
    (name, #[path]s, inline module names) and includes (macro, path)."""
    toks = ts.tokenize(text)
    match = ts.match_brackets(toks)
    out, mods, incs = [], [], []

    def is_p(k, s):
        return k < len(toks) and toks[k].kind == "punct" and toks[k].text == s

    def attr(k, inner):
        o = k + (2 if inner else 1)
        if not is_p(k, "#") or (inner and not is_p(k + 1, "!")) or not is_p(o, "["):
            return None
        close = match[o]
        alive, path = True, None
        if o + 2 < close and toks[o + 1].text == "cfg" and is_p(o + 2, "(") and match[o + 2] == close - 1:
            try:
                tree, _ = ts.parse_pred(toks, o + 3, close - 1)
                alive = ts.eval_pred(tree, system) is not False
            except ts.CfgError:
                alive = True
        if o + 3 < close and toks[o + 1].text == "path" and is_p(o + 2, "="):
            path = ts.str_value(toks[o + 3])
        return close + 1, alive, path

    def seq(lo, hi, inline):
        i = lo
        while i < hi:
            if toks[i].kind == "doc" and toks[i].text.startswith(("//!", "/*!")):
                out.append(toks[i].text)
                i += 1
                continue
            a = attr(i, True)
            if a is None:
                break
            if not a[1]:
                return  # #![cfg(false)]: the whole group goes
            out.extend(t.text for t in toks[i:a[0]])
            i = a[0]
        while i < hi:
            start, alive, paths = i, True, []
            while i < hi:
                if toks[i].kind == "doc":
                    i += 1
                    continue
                a = attr(i, False)
                if a is None:
                    break
                alive = alive and a[1]
                if a[2]:
                    paths.append(a[2])
                i = a[0]
            # The element ends at the first `;` or `,` or `{...}` at its level.
            j = i
            while j < hi:
                t = toks[j]
                if t.kind == "punct" and t.text in "([{":
                    j = match[j] + 1
                    if t.text == "{":
                        break
                    continue
                j += 1
                if t.kind == "punct" and t.text in ";,":
                    break
            end = max(j, start + 1)
            if alive:
                emit(start, end, inline, paths)
            i = end

    def emit(lo, hi, inline, paths):
        k = lo
        while k < hi:
            t = toks[k]
            if t.text == "mod" and t.kind == "ident" and k + 2 < hi and toks[k + 1].kind == "ident":
                if is_p(k + 2, ";"):
                    mods.append((toks[k + 1].text, tuple(paths), inline))
            if t.kind == "ident" and t.text in ("include", "include_str", "include_bytes") and is_p(k + 1, "!") \
                    and k + 3 < hi and toks[k + 3].kind == "str":
                incs.append((t.text, ts.str_value(toks[k + 3])))
            if t.kind == "punct" and t.text in "([{":
                close = match[k]
                out.append(t.text)
                macro = k >= 2 and is_p(k - 1, "!") and toks[k - 2].kind == "ident" or \
                    k >= 3 and toks[k - 3].text == "macro_rules"
                if macro:
                    out.extend(x.text for x in toks[k + 1:close])
                    for m in range(k + 1, close):  # includes inside a macro's input
                        if toks[m].text in ("include", "include_str", "include_bytes") and is_p(m + 1, "!") \
                                and m + 3 < close and toks[m + 3].kind == "str":
                            incs.append((toks[m].text, ts.str_value(toks[m + 3])))
                else:
                    name = toks[k - 1].text if t.text == "{" and k >= 2 and toks[k - 2].text == "mod" else None
                    seq(k + 1, close, inline + (name,) if name else inline)
                out.append(toks[close].text)
                k = close + 1
            else:
                out.append(t.text)
                k += 1

    seq(0, len(toks), ())
    return out, mods, incs


def corpus(snap, system, tree):
    """Everything `system` compiles: {path: stripped tokens}, and the
    included files' contents. Crate roots and the jobs come from the
    Tree's crate list; modules are followed through the stripper."""
    files, seen = {}, set()
    work = []
    for d in tree.crates:
        if system not in tree.crate_sys[d]:
            continue
        roots, builds = tree._roots(d)
        for r in roots + builds:
            work.append((r, {posixpath.dirname(r)}))
        for b in builds:  # a build script's inputs
            text = snap.read(b)
            for t in ts.tokenize(text):
                if t.kind == "str" and "rerun-if-changed=" in t.text:
                    p = posixpath.normpath(posixpath.join(posixpath.dirname(b), ts.str_value(t).split("=", 1)[1]))
                    files[p] = snap.files.get(p)
    while work:
        path, dirs = work.pop()
        key = (path, frozenset(dirs))
        if key in seen or path not in snap.files:
            continue
        seen.add(key)
        text = snap.read(path)
        try:
            toks, mods, incs = strip(text, system)
        except ts.ScanError:
            toks, mods, incs = [text], [], []
        files[path] = toks
        here = posixpath.dirname(path)
        for name, paths, inline in mods:
            for base in dirs:
                inner = posixpath.join(base, *inline)
                for c in [posixpath.normpath(posixpath.join(here, p)) for p in paths] + \
                         [posixpath.normpath(posixpath.join(inner, p)) for p in paths] + \
                         [posixpath.join(inner, name + ".rs"), posixpath.join(inner, name, "mod.rs")]:
                    if c in snap.files:
                        by_path = c.endswith(tuple(paths)) if paths else False
                        nd = {posixpath.dirname(c), c[:-3]} if by_path else \
                            {posixpath.dirname(c)} if c.endswith("/mod.rs") else {c[:-3]}
                        work.append((c, nd))
        for macro, rel in incs:
            p = posixpath.normpath(posixpath.join(here, rel))
            if macro == "include" and p.endswith(".rs"):
                work.append((p, {posixpath.dirname(p)}))
            else:
                files[p] = snap.files.get(p)
    return files


def unaffected(git, base_rev, head_rev, system):
    """None when `system` compiles the same before and after, else why not."""
    base, head = git.snapshot(base_rev), git.snapshot(head_rev)
    for p in ("Cargo.toml", "Cargo.lock"):
        if base.files.get(p) != head.files.get(p):
            return f"{p} changed"
    for p in set(base.files) | set(head.files):
        if p.endswith("/Cargo.toml") and base.files.get(p) != head.files.get(p):
            return f"{p} changed"
    a = corpus(base, system, ts.Tree(base))
    b = corpus(head, system, ts.Tree(head))
    if set(a) != set(b):
        return f"files compiled differ: {sorted(set(a) ^ set(b))[:4]}"
    for p in a:
        if a[p] != b[p]:
            return f"{p} differs after stripping for {system}"
    return None


# ---------------------------------------------------------------------------


def old_scope(git, old_script, rev):
    names = git.run("diff", "--name-only", "--no-renames", f"{rev}^", rev)
    r = subprocess.run(["sh", old_script], input=names, stdout=subprocess.PIPE, check=True)
    vals = dict(line.split("=") for line in r.stdout.decode().split())
    return {s for s in ts.SYSTEMS if vals.get(s) == "true"}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=150)
    ap.add_argument("--ref", default="origin/main")
    ap.add_argument("--old", help="a revision holding scripts/ci/tests-scope.sh, to compare with")
    ap.add_argument("--no-check", action="store_true")
    args = ap.parse_args()
    git = ts.Git()
    revs = git.run("rev-list", "--no-merges", f"--max-count={args.count}", args.ref).decode().split()
    old_script = None
    if args.old:
        old_script = os.path.join(os.environ.get("TMPDIR", "/tmp"), "tests-scope-old.sh")
        with open(old_script, "wb") as f:
            f.write(git.run("show", f"{args.old}:scripts/ci/tests-scope.sh"))
    letter = {"linux": "L", "windows": "W", "mac": "M"}
    fmt = lambda s: "".join(letter[x] if x in s else "-" for x in ts.SYSTEMS)
    totals = {"old": 0, "new": 0, "all": 0, "scripts": 0}
    narrowed = []
    print(f"{'commit':8}  {'old':3}  {'new':3}  {'files':>5}  subject")
    for rev in revs:
        subject = git.run("log", "-1", "--format=%s", rev).decode().strip()
        changes, _ = ts.scope(git.snapshot(f"{rev}^"), git.snapshot(rev))
        res = ts.outputs(changes)
        new = {s for s in ts.SYSTEMS if res[s]}
        old = old_scope(git, old_script, rev) if old_script else set(ts.SYSTEMS)
        totals["old"] += len(old)
        totals["new"] += len(new)
        totals["all"] += 3
        totals["scripts"] += res["scripts"]
        print(f"{rev[:8]}  {fmt(old)}  {fmt(new)}  {len(changes):5}  {subject[:90]}")
        if new != set(ts.SYSTEMS):
            narrowed.append((rev, subject, old, new, changes))
    print()
    print(f"job-starts over {len(revs)} commits: every job {totals['all']}, old list {totals['old']}, tests-scope.py {totals['new']}"
          f" (plus the scripts job {totals['scripts']} times)")
    print(f"commits where tests-scope.py starts fewer than all three: {len(narrowed)}")
    bad = 0
    for rev, subject, old, new, changes in narrowed:
        print()
        print(f"== {rev[:8]} {subject[:100]}")
        print(f"   old {fmt(old)}  new {fmt(new)}")
        for c in changes:
            print(f"   {c.path} -> {ts.names(c.systems)}")
            for r in c.reasons:
                print(f"       {r}")
        if args.no_check:
            continue
        for s in ts.SYSTEMS:
            if s in new:
                continue
            why = unaffected(git, f"{rev}^", rev, s)
            if why is None:
                print(f"   check {s}: unaffected (same code after cfg-stripping)")
            else:
                bad += 1
                print(f"   check {s}: UNSOUND: {why}")
    if bad:
        print(f"\n{bad} skipped system(s) were affected")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
