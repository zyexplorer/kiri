# Packaging

How Kiri reaches people on each platform, and what each piece of this
directory (and `installer/windows`) does.

| Platform | Package | Built by | Contents |
| --- | --- | --- | --- |
| Linux (Debian, Ubuntu) | `kiri_<v>_amd64.deb`, `_arm64.deb` | `packaging/linux/build.sh` | engine + docker CLI, Compose, buildx; the window on x86-64 |
| Linux (Debian, Ubuntu) | `kiri-lite_<v>_amd64.deb`, `_arm64.deb` | `packaging/linux/build.sh` | engine only |
| Linux (Fedora, RHEL, openSUSE) | `kiri-<v>-1.x86_64.rpm`, `.aarch64.rpm`, `kiri-lite-…rpm` | `packaging/linux/build.sh` | same two flavours |
| Windows | `Kiri-<v>-x86_64.msi` | `cargo xtask msi` | engine for WSL2, `kiri-win.exe`, docker CLI, Compose, buildx |
| Windows on Arm | `Kiri-<v>-arm64.msi` | `cargo xtask msi --arch arm64` | the same, arm64 throughout (docker CLI built from source) |
| macOS | `Kiri.app` in `Kiri-<v>-arm64.dmg` (`-x86_64` for Intel), Kiri Lite | `cargo xtask release` | app + CLI tools; Lite downloads the tools |

Two flavours everywhere, with the same meaning: **Kiri** brings the docker
CLI, Compose and buildx so `docker run` works right after installing;
**Kiri Lite** is the engine alone for people who already have a `docker`.
The CLI versions and their SHA-256s are pinned once, in
`crates/kiri/src/cli_pins.rs`, for all three platforms.

## Linux

```sh
packaging/linux/fetch-cli.sh target/linux-cli          # docker, compose, buildx, verified against cli_pins.rs
packaging/linux/build.sh target/linux-x86_64-musl target/packages x86_64 target/linux-cli
```

A release builds all eight packages (both flavours, .deb and .rpm, x86-64
and arm64) with `scripts/ci/linux-build.sh` on Ubuntu, zig cross-compiling
the arm64 engine; then installs the x86-64 .debs under systemd and runs
containers through them (`packaging/linux/test-host.sh`), and the .rpms the
same way on Fedora. Each file goes up with `SHA256SUMS-linux`.

A `v*` tag builds Mac, Windows and Linux; each build puts its files on
download.runkiri.com with its feeds staged, and the release moves every
feed and updates the website once all three are up (RELEASES.md). On a Mac, `scripts/ci/linux-packages.sh` builds
the same packages in a Linux container.

`build.sh` needs `dpkg-deb` and `rpmbuild` (`apt install rpm` on Debian;
both are in WSL Ubuntu). Without the CLI directory it builds only
`kiri-lite`. The binaries come from `cargo xtask linux` or
`scripts/windows-engine.sh` (static musl, so one package runs on every
distro). What the packages do:

- `/usr/bin/kiri`, `/usr/bin/kiri-init`, `kiri.service` (installed, not
  started: installing alone runs nothing. The engine starts, and starts at
  boot, when someone sets Kiri up: the window's Allow access, or
  `sudo kiri host-access $USER`. An upgrade leaves a running engine as it
  is), the `kiri` group that owns
  `/run/kiri/docker.sock`. Members of the kiri group can run containers as
  root on this machine; treat it like the docker group. State in
  `/var/lib/kiri-daemon` (0700), images and containers in `/var/lib/kiri`
  (0710); removal keeps them, `apt purge` deletes them.
- The full package adds `/usr/libexec/kiri/bin/docker` with Compose and buildx
  in `/usr/libexec/kiri/cli-plugins`, and offers `docker` on PATH through
  `update-alternatives` at priority 10, only when `/usr/bin/docker` is absent
  or already an alternatives link: a docker CLI another package owns keeps
  winning, as on Windows. Compose and buildx go into
  `/usr/lib/docker/cli-plugins` the same way, one alternatives link each, so
  the package installs beside `docker-compose-plugin`, `docker-buildx-plugin`
  and `docker-desktop` (whose plugins Kiri's docker then uses). A link is
  made only when Docker's own copy isn't in any of the CLI's plugin folders
  (`/usr/lib`, `/usr/libexec`, `/usr/local/lib`, `/usr/local/libexec`
  `…/docker/cli-plugins`), and a dpkg trigger (rpm `%filetriggerin` and
  `%filetriggerpostun`) checks again whenever another package changes those
  folders, so Docker's plugins installed or removed after Kiri still win. Its `docker` talks to Kiri's socket unless
  `DOCKER_HOST`, `DOCKER_CONTEXT` or a context of your own says otherwise.
- The x86-64 full package also has the window: `/usr/lib/kiri/kiri-window`
  (kiri built with `--features window`, linking GTK 3 and WebKitGTK 4.1, so
  the package depends on them and on the build machine's glibc, Ubuntu
  24.04's in CI), its launcher in the app menu and the icons. `kiri ui`
  starts it; where it isn't installed (kiri-lite, the arm64 packages, which
  zig cross-compiles), `kiri ui` says so. `linux-build.sh` builds the window
  for its own machine's architecture when `libwebkit2gtk-4.1-dev` is there.
- Upgrading the package leaves the running engine as it is (stopping it would
  end every container); run `systemctl restart kiri` afterwards to get the
  new version.

Tests: `packaging/linux/test.sh` (package listings, install/remove in
privileged Ubuntu and Fedora containers on any Docker-compatible engine, Kiri
included; nested, so it proves the files and scripts, not container runs) and
`packaging/linux/test-host.sh` (on a real host or VM with systemd: install,
run containers, publish a port, remove; it stops a kiri daemon already running
there, so it wants `KIRI_TEST_KILL=1`).

Not yet: an apt/dnf repository (today the packages are release assets;
a repository is what gives `apt upgrade`), and signing.

## Homebrew (planned, not built)

The Mac already has the app and the dmg; Homebrew is the third way in, for
people who live in the terminal. Plan:

1. **A tap, not homebrew-core, to start**: a `homebrew-kiri` repository (not
   made yet) with `Formula/kiri.rb`. Users run `brew install libracoder/kiri/kiri`.
   homebrew-core wants a stable, notable, open project with no vendored
   binaries, so that comes later, if at all.
2. **Formula = Kiri Lite from source or binary.** Simplest and most Homebrew-like:
   build from the release tarball with `cargo install --path crates/kiri`
   (`depends_on "rust" => :build`), install `kiri` to `bin`, and let
   `brew services start kiri` run `kiri daemon` as a user LaunchAgent (a
   `service do` block with `run [opt_bin/"kiri", "daemon"]`, `keep_alive true`,
   log files under `var/log`). The CLI tools stay Homebrew's business:
   `depends_on "docker"` and `"docker-compose"`, `"docker-buildx"` as
   recommended deps, which is exactly the Lite split.
3. **A cask for the app**: `Casks/kiri.rb` pointing at the dmg with its
   SHA-256, `app "Kiri.app"`, `zap` listing `~/.kiri` and the LaunchAgent, for
   people who want the window. `brew install --cask libracoder/kiri/kiri`.
4. **Automate the bump**: Kiri CI's release step opens a PR on the tap with
   the new version and checksums (formula from the source tarball, cask from
   the dmg) whenever it publishes a release; `brew bump-formula-pr` does most
   of this.

Open questions before building it: whether the formula should build the
guest (`cargo xtask guest` needs the aarch64 musl target and a kernel image,
which Homebrew will not do) or download Kiri's prebuilt guest image at first
run as Lite does today; and whether the virtualization entitlement survives
a Homebrew-built binary (it needs signing with the entitlement, which the
formula can do with `codesign --sign -` ad hoc for local use).
