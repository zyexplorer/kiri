# Features

What Kiri supports today, through the `kiri` Docker context.

## Platforms

Apple silicon and Intel Macs (macOS 14+), Windows 10 and 11 (x86-64 and Arm, through WSL 2), and Linux (.deb and .rpm, tested on Ubuntu and Fedora). 
## What works

- **Containers**: `run` (`--rm`, `-d`, `-it`, `-e`, `-u`, `-w`, `--name`,
  `--restart`, `--entrypoint`), `exec`, `logs`, `attach`, `stop`, `kill`,
  `restart`, `rm`, `wait`, `rename`, `pause`/`unpause`, `update`, `stats`,
  `top`, `diff`, `export`, `commit`, `cp` (both ways, running or not),
  `container prune`; records survive a daemon restart; filters on every list
- **Limits and security**: `--memory`, `--cpus`, `--cpu-shares`,
  `--pids-limit`, `--shm-size`; Docker's default capabilities and a default
  seccomp filter; `--cap-add`/`--cap-drop`, `--privileged`, `--read-only`,
  `--security-opt seccomp=unconfined` and `no-new-privileges`. Even
  privileged and unconfined containers keep the host boundary: no vsock
  sockets (the way to the Mac) and no io_uring
- **Networking**: a default bridge, user networks with DNS by name and
  alias, `network connect`/`disconnect`, `--network host|none`, `-p`/`-P`
  (TCP and UDP) on all interfaces (as Docker does; `-p 127.0.0.1:8080:80`
  keeps a port local), `--add-host`, `host.docker.internal`,
  `--network container:<name>` (Compose `network_mode: service:x`; as with
  Docker, if that container restarts, the ones sharing its network keep the
  old, now empty namespace until they are restarted too), IPAM (`--subnet`,
  `--gateway`, `--ip-range`, `--aux-address`), `--ip`, `--mac-address`,
  `--dns`, `--dns-search`, `--dns-option`, `--domainname`, `--link`,
  `--ipv6` (Compose `enable_ipv6`: an IPv6 subnet beside the IPv4 one, from
  `--subnet` or a /64 of Kiri's own under fd00::/8; the default bridge stays
  IPv4-only, as Docker's does), and
  `http://<name>.kiri.local` once you turn it on (the app asks on first open;
  Settings → Domains changes it), or `http://<name>.kiri.localhost` with no
  setup. Compose services answer at `<service>.<project>.kiri.local`, and a
  `dev.kiri.domain=api.myapp` label picks a container's own names; HTTPS too
  (`https://<name>.kiri.local`), with Kiri's own certificate authority in
  `~/.kiri/tls`, whose name constraints allow only `*.kiri.local` and
  `*.kiri.localhost` (Settings → Domains → HTTPS asks macOS to trust it)
- **Storage**: named and anonymous volumes, bind mounts of Mac paths
  (virtiofs), `--tmpfs`, `--mount`, `volume` commands
- **Images**: `pull` (with `docker login` credentials), `push`, `tag`,
  `save`, `load`, `history`, `search`, `image prune`, `system df`;
  **amd64 images run through Rosetta**, `--platform` honoured;
  Docker Hub pulls honour `registry-mirrors` in `~/.kiri/daemon.json`
  (Settings → Network); `insecure-registries` and
  `max-concurrent-downloads` / `max-concurrent-uploads` apply as in Docker's
  `daemon.json`
- **Builds**: `docker build` / `buildx build` through BuildKit running in a
  Kiri container (buildx's docker-container driver), stopped when idle;
  on Apple silicon, amd64 build steps run through Rosetta too
- **Compose**: `up`, `down`, `ps`, `logs`, `exec`, multiple networks, events
- **Mac integration**: container files at `~/Kiri` in Finder (read-only
  NFS, always current), SSH agent forwarding at
  `/run/host-services/ssh-auth.sock`
- **Memory**: a balloon gives unused memory back while running, and the VM
  sleeps after 60 s idle (the daemon stays at ~15 MB; waking takes ~0.15 s)
- **The window**: containers, images, volumes and networks, with Compose
  projects grouped (start, stop, restart or remove a whole project); each
  container has its own page with details, live logs (find, level filter),
  a command line (and "Open in Terminal" for a full shell) and its files
  (preview, Save to Downloads, Show in Finder); Settings for fonts, text
  size, the VM's CPUs and memory (`~/.kiri/vm.json`, used from the VM's
  next start; `KIRI_CPUS` and `KIRI_MEMORY_MIB` override it), a proxy,
  `daemon.json` registry settings (`~/.kiri/daemon.json`) and local names
- **Menu bar**: Kiri stays in the menu bar when its window closes, with the
  VM's state, running containers (click one to open it) and Start/Stop
- **Kubernetes**: `kiri k8s start` — a single-node k3s cluster in one
  container, ready in ~20 s, as the `kiri` kubectl context
- **Linux machines**: `kiri machine create dev --distro ubuntu` and
  `kiri machine shell dev` — your user, sudo, and `/Users` at the same path
- **The app**: `cargo xtask app` builds `Kiri.app` with the `docker` CLI,
  Compose and buildx inside; opening it runs `kiri setup` (docker context,
  LaunchAgent, build builder) and the window; `kiri update` installs new
  releases after checking their SHA-256 and signature
- **Linux hosts**: the same `kiri` with no VM — the agent runs on the host's
  kernel (`cargo xtask linux`, `contrib/linux/kiri.service`); arm64 and
  x86-64; `tests/linux-host.sh` checks it. Packaged as `.deb` and `.rpm`
  (`packaging/linux`): `kiri` with the docker CLI, Compose and buildx, or
  `kiri-lite` for the engine alone. Membership of the `kiri` group is
  root-equivalent, exactly like the `docker` group
- **Windows**: the x86-64 Linux build in a WSL2 distro, and `kiri-win.exe`,
  which serves the API on `npipe:////./pipe/kiri` (the `kiri` docker
  context) and carries it into WSL over one `wsl.exe … kiri bridge`, so it
  needs no change to WSL's or Windows' network settings (`cargo xtask
  msi` builds `target/Kiri-<version>-x86_64.msi`, a per-user installer
  around `target/kiri-windows-x86_64`; `kiri.ps1` in that folder installs
  the same without an MSI). Not TCP:
  WSL's localhost relay drops half-closes, and `docker run` loses its
  output

Kubernetes LoadBalancer services are published on `localhost`, and
`kiri machine create NAME --systemd` boots systemd as the machine's init.

## Releases

The app and DMGs are signed with a Developer ID and notarized. Releases and
the update feeds are at `https://download.runkiri.com`, and every file and
feed names its architecture: `Kiri-<version>-arm64.dmg`, `Kiri-<version>-arm64.tar.gz`
and `latest-mac-arm64.json` for Apple silicon (`-x86_64` for Intel Macs),
`Kiri-<version>-x86_64.msi` / `-arm64.msi` with `latest-windows-x86_64.json` /
`latest-windows-arm64.json` for Windows. Each app reads only its own feed, and
each platform is published on its own. Two DMGs: `Kiri-<version>-arm64.dmg`
bundles the docker CLI, Compose and buildx; `Kiri-Lite-<version>-arm64.dmg`
(`cargo xtask app-lite`) leaves them out and downloads the same pinned
versions on first launch (`crates/kiri/src/cli_pins.rs`, checked by SHA-256)
unless the Mac already has a docker CLI.

## Known issues

- **`docker build --add-host name:host-gateway` with BuildKit fails** with
  `host-gateway is not supported by the docker-container driver`. buildx
  refuses `host-gateway` for every builder except Docker's built-in one, and
  Kiri's builder is a docker-container builder, so the build stops before
  Kiri sees it. Docker's own docker-container builders do the same. Until
  Kiri has a built-in builder (planned), use the address itself
  (`--add-host name:172.18.0.1` on Windows and Linux, the default bridge's
  gateway; `198.19.248.254` on a Mac) or `DOCKER_BUILDKIT=0`, where Kiri
  maps `host-gateway` itself.
