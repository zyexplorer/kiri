# Roadmap

Where Kiri is going next. Kiri ships a release most weeks, so this page
changes often.

- The [milestones](https://github.com/libracoder/kiri/milestones) list every
  issue and pull request in each release.
- [RELEASES.md](RELEASES.md) and the [release notes](docs/releases) show what
  has already shipped.

Plans are tied to versions, not dates. They move when people report problems,
and reported problems usually come first. To push something up the list, add a
👍 to its issue, or [open one](https://github.com/libracoder/kiri/issues/new/choose).

## Now: 0.2.3

Friday's release
([milestone](https://github.com/libracoder/kiri/milestone/6)):

- On Windows, the leftovers from the WSL restart fix
  ([#395](https://github.com/libracoder/kiri/issues/395)).
- Settings › Network lets you choose the addresses containers get
  ([#217](https://github.com/libracoder/kiri/pull/217)).

0.2.2 is out: security fixes, a Compose section in the window, searchable
menus and a Homebrew tap ([release notes](docs/releases/0.2.2.md)).

## 0.3.0: share your way

Kiri Share gives a container a public link with nothing to set up. 0.3.0 adds
two more ways to share containers and pods
([milestone](https://github.com/libracoder/kiri/milestone/7)):

- **Tailscale.** Only devices on your tailnet can open it, or turn on Funnel
  to make it public.
- **Cloudflare Tunnel.** Your own domain, through your Cloudflare account.
  Kiri creates the tunnel and the DNS record for you
  ([#488](https://github.com/libracoder/kiri/issues/488)).
- **Choose your share name** from the window, not only from the CLI
  ([#436](https://github.com/libracoder/kiri/issues/436)).

Kiri runs `tailscaled` and `cloudflared` inside its own VM, so there is
nothing extra to install. Before this ships, containers on Linux also need to
reach Tailscale and VPN networks
([#261](https://github.com/libracoder/kiri/issues/261)).

## 0.4.0: Coolify

[Coolify](https://coolify.io) can use Kiri as its container engine, and you
can deploy a project from Kiri to your Coolify server
([milestone](https://github.com/libracoder/kiri/milestone/8)).

What stops Coolify today, found by testing it on Kiri
([#471](https://github.com/libracoder/kiri/issues/471)):

- Images with 60 or more layers fail to start
  ([#253](https://github.com/libracoder/kiri/issues/253)).
- `--pid host`, and `docker builder prune` while the builder is idle.
- Traefik, Coolify's proxy, can't find a container's port from the image's
  `EXPOSE` ([#492](https://github.com/libracoder/kiri/issues/492)).

## Also coming

**A built-in builder.** Kiri will serve Docker's own build endpoints, so
buildx uses its docker driver against Kiri as it does against Docker Desktop.
This brings `host-gateway` support to BuildKit builds, and images load
straight into Kiri ([#49](https://github.com/libracoder/kiri/issues/49)).
