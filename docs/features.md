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
  alias, `network connect`/`disconnect` (as with Docker, a container can
  leave any of its networks, its first one too: another of its networks
  then gives its default route, and with none left it has no network, as
  with `--network none`; to leave its first network a running container is
  stopped first), `--network host|none`, `-p`/`-P`
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
  `*.kiri.localhost` (Settings → Domains → HTTPS asks macOS to trust it).
  The routers' ports from the terminal: `kiri domains` shows them,
  `kiri domains set --http 8080 --https 8443` moves them with no restart,
  `kiri domains reset` puts back 80 and 443
- **Storage**: named and anonymous volumes, bind mounts of Mac paths
  (virtiofs), `--tmpfs`, `--mount`, `volume` commands
- **Images**: `pull` (with `docker login` credentials), `push`, `tag`,
  `save`, `load`, `history`, `search`, `image prune`, `system df`;
  **amd64 images run through Rosetta**, `--platform` honoured
- **Private registries**: Kiri's own `~/.kiri/daemon.json` takes Docker's
  `insecure-registries` (hosts such as `registry.lan:5000`, or networks
  such as `10.0.0.0/8`, reached with a self-signed certificate or plain
  HTTP: HTTPS without checking the certificate first, then HTTP) and
  `registry-mirrors` (Docker Hub pull-through mirrors, tried in order,
  each over the scheme it is written with: `http://` straight over plain
  HTTP, `https://` never down to it, as in Docker).
  They apply to `pull`, `push`, `login` and `build`, only for the hosts
  listed, and an edit applies without a restart. Docker Hub is always
  reached directly over verified HTTPS, whatever the file lists or its
  name resolves to, and a registry on this machine (`localhost` or a
  loopback address; not a name under `.localhost`) is tried over HTTPS
  without a certificate check, then HTTP, as in Docker. Where
  Kiri is stricter than Docker: a network covers registries named by an
  address in it (`10.1.2.3:5000`), not host names that resolve into it
  (list those by name), and must be a private range or a small one (a
  /16 or longer for IPv4, /48 for IPv6); a plain-HTTP (`http://`) mirror
  is used only when its host is listed as insecure too; a pull signed in
  to Docker Hub asks the mirrors only for official (`library/`) images; a
  mirror that fails mid-pull, or serves an image for another platform,
  hands the pull to the next mirror and then Docker Hub, and an image a
  mirror served records which; a plain-HTTP token service is asked only
  on the registry's own host and port; and no redirect goes from HTTPS down to
  plain HTTP except to a listed host. BuildKit's settings name hosts, not
  networks, so a network reaches `docker build` for a registry once an
  image from it has been pulled (until then such a `FROM` fails rather
  than going unchecked): pull from a new registry in a listed network once,
  or list it by name. The lists, like Kiri's other settings, are changed
  from this machine (the window, `kiri registry` or the file). Containers
  can't change Kiri's settings through the engine's API, even with its
  socket. As with Docker, a container you give writable host folders can
  change your files there, Kiri's settings among them, and a container
  that can create containers with `--privileged`, `--pid=host`,
  `SYS_ADMIN`, `--device` or a mount of the host's root is as good as root
  on this machine. A daemon.json larger than
  256 KiB, or not a regular file, is ignored, and at most 256 hosts and
  16 mirrors are read. Settings → Registries and `kiri registry
  insecure|mirror add|remove` edit the same file; Docker Desktop's
  daemon.json is never read or changed. A registry with a self-signed
  certificate is reached only once it is listed, as with Docker
- **Builds**: `docker build` / `buildx build` through BuildKit running in a
  Kiri container (buildx's docker-container driver), stopped when idle;
  on Apple silicon, amd64 build steps run through Rosetta too
- **Compose**: `up`, `down`, `ps`, `logs`, `exec`, multiple networks, events
- **USB devices (beta)**: the USB tab hands a device (a serial adapter, a
  Zigbee stick, a development board) to your containers on Mac, Windows and
  Linux, and shows where it is inside Linux with the `--device` line to
  copy. On a Mac, a device that drops off the bus while attached (the board
  resets, or the Mac sleeps) is attached again when it comes back
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
  next start; `KIRI_CPUS` and `KIRI_MEMORY_MIB` override it) and local names
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

- **Mac: an attached USB device leaves your containers while the Mac
  sleeps.** macOS takes USB devices off the bus when it sleeps, so a device
  such as `/dev/ttyACM0` is gone from containers until the Mac wakes. Kiri
  then attaches it again by itself. Kiri doesn't keep the Mac awake for a
  device, since that would drain a laptop's battery. For a long job that
  runs unattended (flashing boards, logging a serial port), keep the Mac
  awake while it runs: `caffeinate -s` in a terminal (on power), or turn on
  "Prevent automatic sleeping when the display is off" in System Settings
  (Battery › Options on a laptop, Energy on a desktop Mac).

- **`docker build --add-host name:host-gateway` with BuildKit fails** with
  `host-gateway is not supported by the docker-container driver`. buildx
  refuses `host-gateway` for every builder except Docker's built-in one, and
  Kiri's builder is a docker-container builder, so the build stops before
  Kiri sees it. Docker's own docker-container builders do the same. Until
  Kiri has a built-in builder (planned), use the address itself
  (`--add-host name:172.18.0.1` on Windows and Linux, the default bridge's
  gateway; `198.19.248.254` on a Mac) or `DOCKER_BUILDKIT=0`, where Kiri
  maps `host-gateway` itself.
