# Contributing to Kiri

Thanks for helping. Issues and pull requests are welcome.

## Before you start

For anything larger than a small fix, open an issue first so we can agree on
the approach, and wait for that before you write the code. Kiri aims to be
small and fast: changes that add a dependency, a background process or
startup time need a good reason.

Bug reports need the Kiri version and system, the exact commands you ran and
the output you got, as the issue form asks. A report we can't reproduce, or a
problem found only by reading the code, may be closed until it can be shown
on a real machine.

Keep at most 3 pull requests open at a time. Each one takes a maintainer's
time to review, and a few at a time get reviewed sooner. Pull requests beyond
that, or larger changes without an agreed issue, are closed; you're welcome
to reopen them one at a time later.

## Building and testing

```sh
cargo xtask host      # target/release/kiri, signed with the virtualization entitlement
cargo test            # unit tests
tests/e2e.sh          # the Docker compatibility suite, against a running `kiri daemon`
```

Use `cargo xtask host`, not plain `cargo build`, for a binary you run as the
daemon: without the virtualization entitlement the VM will not start. The
guest needs `cargo xtask kernel`, `cargo xtask disk` and `cargo xtask guest`
once; see the README.

`tests/e2e.sh` only removes containers, networks and volumes it created.

`tests/e2e.sh` runs on macOS, on a Linux host and in WSL 2, and asks the
engine which one it is talking to. A check for a feature another system has
alone (Finder, Rosetta) is marked `only mac` and skipped elsewhere; one that
needs something the setup may lack (a terminal, buildx, Finder access from
this session, an amd64 emulator, a kernel module on a Linux host) is marked
`needs ...` and skipped while it is missing. Where the right answer differs
by system (the default bridge's nameserver, `--network host`'s address), the
check expects each system's. The summary counts the skips by reason. Set
`KIRI_TEST_STRICT=1` for a run that should have everything: what the setup
lacks then fails instead of being skipped.

The test suites never pull from Docker Hub, which allows only 100 anonymous
pulls an hour for a whole network. Each image they run comes from
`KIRI_TEST_REGISTRY` (default `public.ecr.aws/docker/library`, the same
official images; `mirror.gcr.io/library` is tried when it fails) and is tagged
with its plain name, so a second run fetches nothing. The list and the
fetching are in `tests/lib/images.sh`; `python3 tests/lib/check-images.py`
fails when a script names an image the list lacks. `tests/e2e.sh` ends with
`Docker Hub allowance used during the run (whole network): N`, which should
be 0; other machines on the same network count against it too.

Every bug fix adds a check that fails without the fix and passes with it (a
line in `tests/e2e.sh` or `tests/linux-host.sh`, a case in `tests/stress.sh`,
or a unit test), and names it in `tests/regressions.tsv` with the issue or
pull request and one line on what broke. A listed check that fails says
which bug it guards; `python3 tests/lib/check-regressions.py` fails when the
table names a check that is gone. `KIRI_TEST_ONLY='^udp-'` runs only the
`tests/e2e.sh` checks whose names match. `tests/stress.sh` runs the races
(starts beside `docker system df`, `rm -f` against `docker run --rm` and
against starts, a minute of churn) in two to five minutes;
`KIRI_STRESS_ROUNDS` and `KIRI_STRESS_SECONDS` make it longer. On a Linux
test host, `sudo KIRI_TEST_ALLOW_HOST_CHANGES=1 KIRI_ENGINE_RESTART='...'
tests/wsl-shape.sh` runs the DNS checks with the host shaped like WSL 2 (its
resolver an address of its own on `lo`) and puts the host back afterwards.

## Every feature ships on macOS, Windows and Linux together

- A feature comes in one pull request, for all three systems, built and
  tested on each of them before it's merged.
- No `if (WIN)` or Kiri Dev (`flavor::dev()`) gate that hides a feature from
  a system or from Kiri. Words may differ by system ("Show in Finder"); what
  the window offers may not. Plumbing (contexts, update URLs) may differ.
- `python3 scripts/ui-test.py` must pass. Its parity test opens the window as
  each system and fails when one shows something another doesn't, unless
  `tests/ui/parity-allow.txt` gives the reason. A new line there needs a
  maintainer's OK. It also runs on every pull request; on Linux, point
  `CHROME` at your Chrome or Chromium.

## Speed is a feature

Kiri is small and fast on purpose, and every pull request is checked for it.
The Speed check builds main and your change, runs both engines side by side
on one machine and compares them: `docker run`, API round trips, the daemon's
memory, the system calls one container costs, and the binaries' size. It
fails when the change is slower than main past the limits in
`scripts/perf.py`; the table is in the check's summary. To run it yourself on
Linux, build both with `cargo xtask linux` and run
`scripts/perf-run.sh <main's build> <your build> <work folder>`.

## Pull requests

- Keep each pull request to one change, with a message that says what it does
  and why.
- Run `cargo fmt`, `cargo clippy` and the tests. After changing Rust
  dependencies or bundled tool versions, run `scripts/ci/notices.sh` and commit
  the updated NOTICE.
- Sign your commits and the [Contributor License Agreement](CLA.md) when CLA
  Assistant asks for it on your first pull request.

By submitting a contribution you agree it is licensed under Apache-2.0, as
described in the README and the Contributor License Agreement.
