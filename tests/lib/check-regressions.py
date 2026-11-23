#!/usr/bin/env python3
"""tests/regressions.tsv must name only checks that still exist. Each line
is a check name, a tab, the issue or pull request (#N), a tab, and one line
on what broke. A check exists when a test script runs it by that name
(t NAME or code NAME; a case of tests/stress.sh, selected NAME); a name
with a / is a file under the repo.
Reads the files only (no engine, no network); prints what is wrong and
exits 1 if anything is.

    python3 tests/lib/check-regressions.py
"""
import pathlib
import re
import sys

TESTS = pathlib.Path(__file__).resolve().parent.parent
TABLE = TESTS / "regressions.tsv"


def defined():
    names = set()
    run = re.compile(r"(?:^|[\s;(&|])(?:t|code|check|selected)\s+\"?([a-z0-9][a-z0-9_-]*)\"?\s")
    for p in sorted(TESTS.glob("*.sh")):
        for line in p.read_text().splitlines():
            if line.lstrip().startswith("#"):
                continue
            names.update(m.group(1) for m in run.finditer(line + " "))
    return names


def main():
    have = defined()
    bad = 0
    seen = set()
    rows = 0
    for n, line in enumerate(TABLE.read_text().splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        rows += 1
        cols = line.split("\t")
        where = f"tests/regressions.tsv:{n}"
        if len(cols) != 3 or not re.fullmatch(r"#[0-9]+", cols[1]) or not cols[2].strip():
            print(f"{where}: want NAME<tab>#N<tab>what broke, got {line!r}")
            bad += 1
            continue
        name = cols[0]
        if name in seen:
            print(f"{where}: {name} is listed twice")
            bad += 1
        seen.add(name)
        if "/" in name:
            if not (TESTS.parent / name).is_file():
                print(f"{where}: no file {name}")
                bad += 1
        elif name not in have:
            print(f"{where}: no check named {name} in tests/*.sh")
            bad += 1
    print(f"{rows} regression checks listed, {bad} problem(s)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
