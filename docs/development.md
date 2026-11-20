# Development

Building Kiri from source, running it, and how it fits together.

## Build

Needs Rust and, for the kernel and disk template, a Docker engine. Kiri can
build its own kernel: `DOCKER_CONTEXT=kiri cargo xtask kernel`.

```sh
cargo xtask kernel    # guest/out/Image
cargo xtask disk      # guest/out/data.img (sparse 64 GiB ext4 template)
cargo xtask guest     # guest/out/initramfs.cpio
cargo xtask host      # target/release/kiri, signed with the virtualization entitlement
```

`cargo xtask app-dev <feature>` makes `target/dev/Kiri <Feature>.app`, a second
Kiri with its own engine, for trying a change. It builds the host with
`[profile.fast]`: optimized, but on every core and incremental, so the first
build takes about half a minute and a rebuild after an edit a few seconds.
The binary is bigger and a little slower than a release one; `app`, `release`
and speed checks keep `[profile.release]`. The docker CLI, Compose and buildx
downloads are cached once for every checkout, in `~/Library/Caches/kiri-build/cli`
(`KIRI_CLI_CACHE` to move it), and checked against their pinned SHA-256 each time.

`cargo xtask linux` builds the static Linux engine (`kiri`, `kiri-init`) in a
Rust container into `target/linux/release`; `cargo xtask linux-dev` builds the
same with `[profile.fast]` into `target/linux/fast`, for testing a change.

For Intel Macs add `--arch x86_64` to `kernel`, `guest`, `host`, `app`,
`dmg` and `release`: an x86-64 guest in `guest/out/x86_64` (a `bzImage`,
cross-compiled on Apple Silicon), the host for `x86_64-apple-darwin`
(`rustup target add x86_64-apple-darwin x86_64-unknown-linux-musl`), and
`target/intel/Kiri.app` with Intel builds of the docker CLI, Compose and
buildx, released as `Kiri-<version>-x86_64.dmg`/`.tar.gz` with its own feed,
`latest-mac-x86_64.json`. Each Mac gets its own app rather than one universal
app, which would carry both builds of the docker tools. On Intel there is
no Rosetta for Linux: amd64 images run natively, and images with no amd64
build are refused.

On Windows, `cargo xtask windows-dev` builds the Linux engine, Kiri Linux and
Docker tools into `target/kiri-windows-x86_64` with a debug `kiri-win.exe`,
and the engine with `[profile.fast]`.
`cargo run -p kiri-win` uses that bundle automatically. Use `cargo xtask
windows` for the release bundle and `cargo xtask msi` for the installer.
Add `--arch arm64` to any of the three for Windows on Arm
(`target/kiri-windows-aarch64`, `Kiri-<version>-arm64.msi`); testing it in
a VM on a Mac is in [docs/testing-windows-arm.md](docs/testing-windows-arm.md).

## Use

```sh
./target/release/kiri daemon &      # serves ~/.kiri/run/docker.sock; the VM boots on first use
docker context create kiri --docker host=unix://$HOME/.kiri/run/docker.sock
docker --context kiri run --rm alpine echo hello
docker buildx create --name kiri-build --driver docker-container \
  --driver-opt default-load=true --use kiri    # for docker build
./target/release/kiri bench         # Kiri vs OrbStack vs Docker Desktop
SOAK=600 tests/e2e.sh               # every check, then 600 runs watching for leaks
```

Docker's commands also work as `kiri` commands, always on Kiri whatever
docker context is selected: `kiri run`, `kiri ps`, `kiri exec`, `kiri logs`,
`kiri build`, `kiri compose up` and the rest run the docker CLI against
Kiri's socket. (`kiri import` is Kiri's own: it brings images, volumes and
containers over from OrbStack or Docker Desktop.) The daemon-free tools
that boot their own guest are `kiri raw run|pull|images|rmi`.

`kiri` keeps its data in `~/Library/Application Support/Kiri` (override with
`KIRI_DATA_DIR`). It does not touch `/usr/local/bin/docker`.
`/var/run/docker.sock` changes only when you ask: `kiri setup --docker-socket`
(or the password prompt for `*.kiri.local` names) links it to Kiri's socket
when no other engine has it, after macOS asks for an administrator password,
and installs `/Library/LaunchDaemons/dev.kiri.docker-socket.plist` to put the
link back after a restart. A socket another engine owns is never replaced.

## Architecture

```
docker CLI ─┐
compose ────┼─ ~/.kiri/run/docker.sock ─► kiri daemon ── vsock ──► guest agent (PID 1)
buildx ─────┤     Docker Engine API          owns the VM            clone3 → container
kiri UI ────┘
```

- **VM**: Apple's Virtualization.framework, driven from Rust through objc2.
  A custom 10 MB kernel (`guest/kernel/`) built from allnoconfig, with one
  patch that cuts virtio-pci reset waits from a scheduler tick to
  microseconds, one that hands big TCP to the host as GSO, one that keeps
  newly created FUSE files owner-readable, and an upstream GRO fix that keeps
  forwarded packets segmentable. Rosetta is shared in over virtiofs and registered with
  binfmt_misc.
- **Guest agent**: a ~900 KB static Rust binary that is PID 1. One `poll()`
  loop serves the host, reaps children, forwards container output, relays
  ports, answers DNS and runs DHCP. `docker cp` and friends run in forked
  children so a large copy never stalls it.
- **Containers**: one `clone3` straight into new namespaces and the
  container's cgroup (`CLONE_INTO_CGROUP`), overlayfs over layers unpacked
  on an ext4 data disk, `pivot_root`, capabilities and a seccomp filter.
  Networking is set up with hand-written rtnetlink and nf_tables messages.
  No runc, no shim, no dockerd.
- **Images**: pulled and pushed on the Mac (`crates/kiri-image`), streamed
  into the guest once and unpacked there.

## Layout

| Path | What |
|------|------|
| `crates/kiri` | the `kiri` binary: daemon, Docker API, CLI, UI, bench |
| `crates/kiri-vmm` | Virtualization.framework VM |
| `crates/kiri-init` | guest PID 1, agent and container runtime |
| `crates/kiri-proto` | host ↔ guest wire protocol (no_std, no allocation per message) |
| `crates/kiri-image` | image references, registry client, blob store, archives |
| `guest/kernel` | kernel config and patches |
| `tests/e2e.sh` | ~350 checks through the real `docker` CLI, plus a leak soak (`K8S=1` adds Kubernetes) |
| `tests/linux-host.sh` | the Linux host build, driven in a privileged container |
| `tests/finder-mount.sh` | the Mac's Finder mount at `~/Kiri`: covers only an empty folder or its own old mount, never your files or another mount |
| `contrib/` | systemd unit, WSL2 and Windows install scripts |
| `installer/windows` | the Windows MSI (WiX 5): `kiri.wxs`, licence, dialog bitmaps |
| `packaging/` | Linux `.deb` and `.rpm` (`kiri` with the docker CLI, `kiri-lite` engine only), the Homebrew plan |
| `xtask` | build tasks |
| `bench/results.json` | measured results; the website keeps a copy |
| `brand/` | brand (the website and raw benchmark output live outside this repository) |

## Unexpected VM stops

The daemon keeps the guest's serial console, kernel oopses included, in
`~/.kiri/logs/guest-console.log` (Kiri Dev: in its own state folder). The file and two
rotations (`.1`, `.2`) hold at most 1 MiB each. Files are private, credential
patterns are redacted, and lines over 16 KiB are omitted. The unexpected-stop
log line names this file. The console stays local; crash reports do not send it.

After a guest crash, Kiri automatically boots a new VM when containers were
running with `always` or `unless-stopped`, then restarts them and increments
their `RestartCount`. Crashes in a row back off from 100 ms to one minute;
a VM that ran ten seconds starts the backoff over. A manual stop, kill or
removal wins, including during that wait, and so does Kiri's Stop (Start
brings them back); `no`, `on-failure` and containers that never ran stay
down. If the new VM does not boot, recovery tries again, waiting longer
each time up to a minute.

Before a replacement opens the data disk, Kiri confirms that the previous
VM has stopped and releases its resources, even if relay threads still
hold old links. It allows ten seconds for shutdown, then up to ten seconds
for a confirmed forced stop. Errors and timeouts keep the previous VM
owned and block replacement; recovery retries with backoff. This wait does
not hold container lifecycle locks, so stop, kill and removal can cancel
pending starts. Linux gives each guest process its own socket directory,
so late destruction of an old link cannot remove its successor's sockets.

The real-VM regression is an opt-in ignored test:
`vm_loss_live_recovers_without_a_request`. Run its signed test binary with
an isolated `HOME` and `KIRI_DATA_DIR` containing `alpine:latest` and no
containers, `KIRI_GUEST_DIR` pointing to matching guest assets, and
`KIRI_LIVE_VM_RECOVERY=1`. It force-stops only the VM it creates.
