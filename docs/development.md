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
Docker tools into `target/kiri-windows-x86_64` with a debug `kiri-win.exe`.
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
  microseconds. Rosetta is shared in over virtiofs and registered with
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
