#!/usr/bin/env python3
"""Every image the test scripts name must be in tests/lib/images.sh's list
and have a source outside Docker Hub. Reads the scripts only (no engine, no
network); prints what is missing and exits 1 if anything is.

    python3 tests/lib/check-images.py
"""
import pathlib
import re
import subprocess
import sys

TESTS = pathlib.Path(__file__).resolve().parent.parent
IMAGES_SH = TESTS / "lib" / "images.sh"

# The repositories the scripts use. A bare name counts only where the
# repository is never also a command or a word in prose (docker, node...).
REPOS = ["alpine", "busybox", "nginx", "redis", "postgres", "python", "node", "ubuntu", "debian",
         "hello-world", "docker", "httpd", "registry", "valkey/valkey", "curlimages/curl",
         "moby/buildkit", "amd64/alpine", "mongo", "mysql", "mariadb", "golang", "traefik",
         "testcontainers/ryuk", "rancher/k3s", "fedora"]
BARE = {"alpine", "busybox", "hello-world", "valkey/valkey", "curlimages/curl", "amd64/alpine"}
REF = re.compile(r"(?<![\w./:-])(" + "|".join(re.escape(r) for r in sorted(REPOS, key=len, reverse=True))
                 + r")(?::([A-Za-z0-9][A-Za-z0-9._-]*))?(?![\w/-]|\.[a-z])")
# Not images: a script's own words that look like one.
IGNORE = {"python:3", "alpine:3", "busybox:1", "docker:cli-plugins"}


def listed():
    out = subprocess.run(["sh", "-c", '. "$1"; for i in $KIRI_TEST_IMAGES; do printf "%s %s\\n" "$(kiri_image_full "$i")" "$(kiri_image_remote "$i")"; done', "_", str(IMAGES_SH)],
                         capture_output=True, text=True, check=True).stdout
    return {line.split()[0]: (line.split() + [""])[1] for line in out.splitlines() if line.strip()}


def main():
    have = listed()
    bad = 0
    for name, src in sorted(have.items()):
        if not src:
            print(f"no source outside Docker Hub: {name}")
            bad += 1
    files = sorted(p for p in TESTS.glob("*") if p.suffix in (".sh", ".py") and p.is_file())
    for p in files:
        for n, line in enumerate(p.read_text().splitlines(), 1):
            if line.lstrip().startswith("#"):
                continue
            for m in REF.finditer(line):
                repo, tag = m.group(1), m.group(2)
                if not tag and repo not in BARE:
                    continue
                ref = f"{repo}:{tag or 'latest'}"
                if f"{repo}:{tag}" in IGNORE or ref in have or repo == "rancher/k3s":
                    continue
                print(f"not in tests/lib/images.sh: {ref}  ({p.relative_to(TESTS.parent)}:{n})")
                bad += 1
    print(f"{len(have)} images listed, {bad} problem(s)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
