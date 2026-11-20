#!/usr/bin/env python3
"""Checks the window's translations (crates/kiri/src/ui/lang/*.json) against
the words in use.

English is the key: the pages' tr("…") and trn(n, "…", "…") (index.html, langstep.js and
kiri-win's setup.html), index.html's data-i18n* attributes, and the Rust's
tr("…") and trf("…") (the Mac's menus, the Windows tray, setup steps and the
notices the app sends the page). Each language file must have every key, no
key nothing uses, the same {placeholders} and tags as its key, and no ASCII
double quote (the words go into HTML attributes).

    python3 scripts/i18n-check.py            # check
    python3 scripts/i18n-check.py --missing  # print the keys a file lacks, as JSON
"""
import json, os, re, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UI = os.path.join(ROOT, "crates/kiri/src/ui")
PAGES = [os.path.join(UI, "index.html"), os.path.join(UI, "langstep.js"), os.path.join(ROOT, "crates/kiri-win/src/setup.html")]
# The Rust that draws native words or sends the page words (notices, setup steps).
# (lang.rs is the lookup itself: its tests call tr(lang, words).)
NATIVE = sorted([os.path.join(UI, f) for f in os.listdir(UI) if f.endswith(".rs") and f != "lang.rs"]
                + [os.path.join(ROOT, "crates/kiri-win/src", f) for f in os.listdir(os.path.join(ROOT, "crates/kiri-win/src")) if f.endswith(".rs")])
# Words the native side looks up without a literal tr("…"): the engine states
# shown in the menus ("Linux VM: Running").
NATIVE_STATES = ["Not running", "Running", "Stopped", "Asleep"]

STR = r'"((?:[^"\\\n]|\\.)*)"'


def unquote(lit):
    return json.loads(f'"{lit}"')


def page_keys(src):
    keys = {}
    for m in re.finditer(r"(?<![\w.$])tr\(\s*" + STR, src):
        keys.setdefault(unquote(m.group(1)), src.count("\n", 0, m.start()) + 1)
    for m in re.finditer(r"(?<![\w.$])(?<!function )trn\(", src):
        lits = re.findall(STR, src[m.end(): m.end() + 400])[:2]
        for lit in lits:
            keys.setdefault(unquote(lit), src.count("\n", 0, m.start()) + 1)
    for m in re.finditer(r'data-i18n(?:-title|-label|-ph)?="([^"]*)"', src):
        keys.setdefault(m.group(1), src.count("\n", 0, m.start()) + 1)
    return keys


def native_keys():
    keys = {}
    for path in NATIVE:
        src = open(path).read()
        for m in re.finditer(r"(?<![\w.$])(?:tr|trf|app)\(\s*" + STR, src):
            keys.setdefault(unquote(m.group(1)), (os.path.relpath(path, ROOT), src.count("\n", 0, m.start()) + 1))
    for k in NATIVE_STATES:
        keys.setdefault(k, ("menus", 0))
    return keys


def holes(s):
    return sorted(re.findall(r"\{\w+\}", s))


def tags(s):
    return sorted(re.findall(r"</?([a-zA-Z][\w-]*)", s))


def main():
    keys = {}
    for page in PAGES:
        for k, line in page_keys(open(page).read()).items():
            keys.setdefault(k, (os.path.relpath(page, ROOT), line))
    for k, v in native_keys().items():
        keys.setdefault(k, v)
    files = sorted(f for f in os.listdir(os.path.join(UI, "lang")) if f.endswith(".json"))
    if "--missing" in sys.argv:
        for f in files:
            have = json.load(open(os.path.join(UI, "lang", f)))
            print(json.dumps({k: "" for k in keys if k not in have}, ensure_ascii=False, indent=2))
        return 0
    bad = []
    for f in files:
        path = os.path.join(UI, "lang", f)
        try:
            have = json.load(open(path))
        except ValueError as e:
            bad.append(f"{f}: not JSON ({e})")
            continue
        for k, (where, line) in sorted(keys.items(), key=lambda x: x[1]):
            if k not in have:
                bad.append(f"{f}: no translation for {json.dumps(k, ensure_ascii=False)} ({where}:{line})")
        for k, v in have.items():
            if k not in keys:
                bad.append(f"{f}: {json.dumps(k, ensure_ascii=False)} isn't used any more; remove it")
            elif not isinstance(v, str) or not v.strip():
                bad.append(f"{f}: {json.dumps(k, ensure_ascii=False)} has no words")
            else:
                if holes(v) != holes(k):
                    bad.append(f"{f}: {json.dumps(k, ensure_ascii=False)}: placeholders {holes(v)} should be {holes(k)}")
                if tags(v) != tags(k):
                    bad.append(f"{f}: {json.dumps(k, ensure_ascii=False)}: tags {tags(v)} should be {tags(k)}")
                if '"' in v and '"' not in k:
                    bad.append(f"{f}: {json.dumps(k, ensure_ascii=False)}: use “ ” rather than \" in the words")
    for line in bad:
        print("FAIL " + line)
    print(f"{'FAIL' if bad else 'PASS'} {len(keys)} keys, {len(files)} language file(s), {len(bad)} problem(s)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
