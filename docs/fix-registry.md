# Fix registry

A log of every merged bug fix in Kiri, newest first, together with any later bug that fix caused so chains of regressions can be traced.

## Adding an entry

- Every merged bug-fix pull request adds one entry at the top of the list below, newest first.
- Include the release version it shipped in, or `unreleased` until tagged.
- List the linked issue(s) under `Fixes`, or `no issue`.
- When a later bug or regression is traced to an earlier fix, add it under that earlier fix's `Caused` line (with what broke and how it was found) and point the new fix back to the earlier pull request under `Caused by`, so the link goes both ways.
- Record a chain only when an issue, pull request, or regression check explicitly traces the bug to the earlier change.
- Under `Check`, list the check name(s) from `tests/regressions.tsv`, `tests/e2e.sh`, or `tests/linux-host.sh` that guard the fix, or `none`.

## Fixes

### #608 Mac: the memory balloon leaves the guest its headroom, so containers don't stall every few seconds (unreleased)
- Fixes: #509 Containers on a user network get TCP resets under sustained load (Supabase local stack: Kong → PostgREST); #646 kiri-balloon oscillates violently on idle VM: 100% CPU, 18+ TB page-in thrashing, bloated host RAM; #210 macOS: containers report hundreds of GB of block reads while the guest has no page cache
- Caused: none known
- Caused by: -
- Check: none

### #430 Windows: hand docker_engine back to Docker Desktop before it starts (unreleased)
- Fixes: #426 Windows: Docker Desktop fails to start ("Access is denied" on \.\pipe\docker_engine) while Kiri serves the pipe
- Caused: none known
- Caused by: -
- Check: none

### #546 Linux: accounts from SSSD, LDAP or Entra ID can use the engine (unreleased)
- Fixes: #349 Setup fails with "no account named <user>" for directory (non-/etc/passwd) users
- Caused: none known
- Caused by: -
- Check: `access-directory-accounts`

### #119 Images: registry mirrors for Docker Hub, in Settings and the CLI (unreleased)
- Fixes: #118 Registry mirrors for Docker Hub (Settings + CLI); #329 Support insecure-registries (plain-HTTP and self-signed private registries), and read Docker's daemon.json; #611 Could support for pulling images from private repositories be added as soon as possible?
- Caused: none known
- Caused by: -
- Check: `insecure-http-unlisted-refused`, `insecure-tls-unlisted-refused`, `insecure-login-unlisted-refused`, `insecure-cli-add`, `insecure-cli-refuses-everything`, `insecure-http-pull`, `insecure-http-push`, `insecure-selfsigned-pull`, `insecure-http-login`, `insecure-http-auth-pull`, `insecure-other-port-still-refused`, `insecure-docker-info`, `insecure-build`, `insecure-selfsigned-build`, `insecure-removed-refused`, `insecure-hand-edit`, `insecure-hand-edit-keeps-keys`, `mirror-http-pull`, `mirror-docker-info`, `mirror-build`, `insecure-cli-remove`, `mirror-cli-add`, `hub-downgrade-refused`, `socket-mount-settings-refused`, `mirror-cli-plain-needs-listing`, `insecure-daemon-json`

### #460 DNS: on a user-defined network, the next --dns server is tried when the first one refuses (unreleased)
- Fixes: #455 User-defined network: embedded resolver does not fall back when the first --dns server refuses (default bridge does)
- Caused: none known
- Caused by: -
- Check: `dns-user-net-next-after-refusal`, `dns-user-net-all-refused-fails-at-once`, `dns-user-net-next-after-silence`, `dns-user-net-fourth-server-unused`

### #712 Windows: enable both WSL optional components in one call (unreleased)
- Fixes: #297 Windows: setup fails with WSL_E_WSL_OPTIONAL_COMPONENT_REQUIRED - the "Subsystem for Linux" optional component is never enabled
- Caused: none known
- Caused by: -
- Check: none

### #468 Engine: replace DNS names when a container is renamed (unreleased)
- Fixes: #467 docker rename leaves stale DNS names on user-defined networks
- Caused: none known
- Caused by: -
- Check: `rename-dns-new`, `rename-dns-new-extra-net`, `rename-dns-old-gone`, `rename-dns-old-gone-extra-net`, `rename-dns-keeps-own-servers`, `rename-dns-alias-extra-net`, `rename-dns-ipv6-only-aaaa`, `rename-dns-ipv6-only-no-a`, `rename-dns-host-name-stays-host`, `rename-name-starts-with-letter`, `rename-dns-before`, `rename-dns-alias`, `rename-dns-ipv6-only-old-gone`

### #730 Registry: sign-in fields sent as JSON null read as empty, as in Docker (unreleased)
- Fixes: #654 Image pull ignores X-Registry-Auth when the JSON has null fields, so Testcontainers (docker-java) pulls from private registries get 401
- Caused: none known
- Caused by: -
- Check: `auth-null-fields`, `auth-null-fields-wrong`, `pull-auth-null-fields`, `pull-auth-null-std-b64`, `pull-auth-all-null`

### #716 Layers: a path named again as another kind takes the later entry, as in Docker (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `layer-same-path-retyped`

### #713 Windows: a wsl.exe that hangs after a wake is ended and docker is served on a new bridge (unreleased)
- Fixes: #93 After long idle / Windows sleep, `docker` fails with "Kiri is stopped: press Start" and the bridge is not rebuilt; #395 Windows: leftovers from the WSL restart fix (#318)
- Caused: none known
- Caused by: #318 (for #395)
- Check: none

### #709 Layers: set folder times faster on layers with many folders (unreleased)
- Fixes: #676 Image layers: directories lose their modification time (every one shows when Kiri unpacked the layer)
- Caused: none known
- Caused by: #677
- Check: `import-dir-mtime`, `layer-dir-times-alpine`, `layer-dir-times-edges`, `layer-dir-times-cp-out`, `layer-dir-times-commit`

### #705 Engine: containers reach the internet on a Linux host whose firewall drops forwarded traffic (unreleased)
- Fixes: #247 Bridge containers have no outbound TCP/UDP when host FORWARD policy is DROP (no accept or MASQUERADE rules for kiri0); #261 Linux: bridge containers can't reach Tailscale/VPN networks (NAT only on default-route NIC, bridges in no firewalld zone)
- Caused: none known
- Caused by: #696
- Check: `fwdrop-internet`, `fwdrop-user-network`, `fwdrop-rules`, `fwdrop-kept-apart`, `fwdrop-ufw-internet`, `fwdrop-ufw-reload`, `fwdrop-nft-warns`, `fwdrop-flush-rebuilt`, `fwdrop-ufw-rules`, `fwdrop-nft-advice-works`, `fwdrop-nft-warning-gone`

### #700 Linux: Kiri's networks keep clear of Docker's, whichever engine starts first (unreleased)
- Fixes: #489 Linux: default bridge takes 172.17.0.0/16 while docker0 already owns it — containers cannot reach their own gateway
- Caused: none known
- Caused by: -
- Check: `taken-default-bridge-moves`, `taken-pool-network-moves`, `taken-user-subnet-refused`, `taken-stop`, `taken-by-other-bridges`, `taken-wake`, `taken-default-bridge-internet`

### #704 Windows: docker brings Kiri back after WSL restarts, a wake or a dead bridge, and only a real Stop says Kiri is stopped (unreleased)
- Fixes: #93 After long idle / Windows sleep, `docker` fails with "Kiri is stopped: press Start" and the bridge is not rebuilt
- Caused: none known
- Caused by: -
- Check: `engine-stopped-says-kiri-start`

### #677 Layers: directories keep the layer's modification time, as in Docker (unreleased)
- Fixes: #676 Image layers: directories lose their modification time (every one shows when Kiri unpacked the layer)
- Caused: #709, walking each folder path from the layer root slowed first docker run on layers with 50,000 folders and kept the earlier mtime for duplicate path spellings (found benchmarking large layers)
- Caused by: -
- Check: `import-dir-mtime`, `layer-dir-times-alpine`, `layer-dir-times-edges`, `layer-dir-times-cp-out`, `layer-dir-times-commit`

### #696 Engine: containers reach the internet on a Linux host that also has Docker (unreleased)
- Fixes: #247 Bridge containers have no outbound TCP/UDP when host FORWARD policy is DROP (no accept or MASQUERADE rules for kiri0); #330 Linux: containers have no DNS on the default bridge; Compose/BuildKit builds can't resolve a digest-pinned base; bun install gets ConnectionRefused
- Caused: #705, rules placed in DOCKER-USER were not re-added after iptables -F DOCKER-USER and a failed iptables-legacy attempt was never retried (found testing firewall reloads)
- Caused by: -
- Check: `dockerfw-internet`, `dockerfw-rules`, `dockerfw-docker-kept-apart`, `dockerfw-docker-reachable`

### #685 Guest: an image with no layers starts (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `load-no-layers-run`, `build-no-layers-run`

### #684 Guest: a file time of 0 in a layer or a docker cp archive stays 0 (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `layer-special-files`, `cp-in-zero-time`

### #683 Guest: containers get runc's /dev/core link (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `dev-core-link`

### #682 Guest: images of 60 or more layers start (unreleased)
- Fixes: #253 Images with 60 or more layers fail to start: "mount …/rootfs: No such file or directory"
- Caused: none known
- Caused by: -
- Check: `build-65-layers-run`, `build-65-layers-lowest`, `layers-59-to-125`, `layers-125-lifecycle`

### #679 Guest: device nodes and fifos in image layers are unpacked as such (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `layer-special-files`, `userns-layer-devices`

### #664 Guest: a numeric user /etc/passwd does not list runs with group 0 (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `user-boundaries`, `user-numeric-unlisted-gid`

### #662 Guest agent: containers and build steps have no process or core-size limit, as on Docker Engine (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `build-ulimits`, `ulimit-default`, `ulimit-set`, `ulimit-set-inspect`

### #661 Image store: a docker rmi during save, load or push no longer deletes a blob in use (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `rmi-during-save`

### #660 Linux units: containers get Docker's process, core-size and task limits (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #659 Engine: on Linux, docker info's NCPU is the CPUs the engine may run on (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `ncpu-engine-cpus`, `ncpu-affinity`

### #658 Engine: a request over the agent's frame limit fails alone (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `frame-limit-request-alone`

### #680 Tests: rm-f-exit-code counts a docker wait that arrives after the removal as late (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `rm-f-exit-code`

### #670 Tests: two timing tests hold on a slow machine (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: #650
- Check: none

### #651 Engine: refused uploads end cleanly, a body that is too large gets an answer, and bind folders are made as Docker makes them (unreleased)
- Fixes: #500 docker load: a load that worked can be reported as failed ("broken pipe")
- Caused: none known
- Caused by: -
- Check: `bind-v-creates`, `bind-create-0200`, `body-too-large-answered`, `body-too-large-chunked`

### #650 Engine: a stop, kill or rm -f during a start waits for the start, as Docker does (unreleased)
- Fixes: no issue
- Caused: #670, start-wait unit tests used a 100 ms timing window that failed on a slow CI runner (found on the first CI run)
- Caused by: #621
- Check: `kill-during-start`, `kill-stuck-start`

### #644 Windows: a set-up Kiri opens its main window when WSL is slow to answer (unreleased)
- Fixes: #427 Windows: Kiri opens on "Install WSL" when WSL is installed but slow to answer
- Caused: none known
- Caused by: -
- Check: none

### #642 DNS: a container starts even when Kiri's listener on the network's gateway cannot be opened (unreleased)
- Fixes: #477 WSL 2 Linux host: user-defined bridge networks fail at gateway DNS bind (EINVAL)
- Caused: none known
- Caused by: -
- Check: none

### #637 UI: Terminal tab — each output lands under its own command (unreleased)
- Fixes: #338 Can't execute commands properly inside built in container terminal
- Caused: none known
- Caused by: -
- Check: none

### #636 UI: Logs tab — Find in logs keeps its focus while lists refresh (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #635 UI: Builds — a build two builders report is listed once (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #632 UI: Update card and Diagnostics › Logs — fit a narrow window (unreleased)
- Fixes: #614 UI: update card and Diagnostics > Logs break in narrow windows
- Caused: none known
- Caused by: -
- Check: none

### #601 Keep terminal command input focused during list refreshes (unreleased)
- Fixes: #586 [UI / Terminal] Input loses focus and cursor disappears every ~5s while typing or idle on Windows
- Caused: none known
- Caused by: -
- Check: none

### #291 UI: apply theme attribute on root element so theme switching works on Linux (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #122 VM disk: space freed inside Linux goes back to the Mac (unreleased)
- Fixes: #123 macOS: VM data.img retains host disk allocation after guest files are deleted
- Caused: none known
- Caused by: -
- Check: `after-trim-run`, `after-trim-user`, `after-trim-exec`

### #504 Mac: ~/.kiri/daemon.log stays under 5 MiB (unreleased)
- Fixes: #441 macOS: ~/.kiri/daemon.log has no size limit and keeps growing
- Caused: none known
- Caused by: -
- Check: none

### #631 Mac: each test app has its own Local Network identity, and a blocked pull says so (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #627 Windows: first run no longer flashes Install WSL, and a WSL that never answers is given 15 s (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `tests/ui/setup-wsl-unknown.js`

### #625 docker top: long formats, sorting, column widths and Docker's rule on titles (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `top-l`, `top-l-values`, `top-efl-cmd`, `top-sort`, `top-sort-k`, `top-width`, `top-title-refused`, `top-title-no-pid`

### #624 DNS: a container can run its own server on port 53, and builds on Windows use Kiri's resolver (unreleased)
- Fixes: #209 Windows: docker build hangs when a RUN step needs network (BuildKit has no usable DNS)
- Caused: none known
- Caused by: -
- Check: `dns-default-bridge-gateway`, `dns-default-bridge-no-loopback`, `build-resolvconf`, `build-no-public-resolver`, `user-net-own-dns-server`, `user-net-own-dns-names`, `user-net-own-dns-upstream`

### #623 Engine: prune and image list filters as Docker reads them; a label filter no longer deletes what lacks the label (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `image-ls-untagged-row`, `image-ls-dangling`, `image-ls-dangling-false`, `image-ls-until`, `system-prune-label`, `image-prune-label-keeps`, `image-prune-all-label-keeps`, `network-prune-until`, `volume-prune-keeps-opts`, `build-prune-until-date`

### #622 Engine: docker run --rm keeps exit code 137 when rm -f kills the container (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `rm-f-exit-code`

### #621 Engine: a container start can no longer hang beside docker system df, and a stuck start can be removed (unreleased)
- Fixes: #439 Linux: an agent crash goes unnoticed while an `--init` container runs, and the engine hangs until restarted
- Caused: #650, stop, kill and rm -f cancelled an in-flight start with 409 instead of waiting for it to finish and answering 204 as Docker does (found comparing start races with Docker)
- Caused by: -
- Check: `starts-beside-df`, `rm-f-stuck-start`, `agent-dies-beside-stuck-start`, `rm-f-during-start`, `stop-during-start`, `churn-beside-df`

### #628 Tests: two checks no longer pass or fail on a registry's refusal (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #615 Tests: Windows skips the kernel features WSL lacks, and a check the registry refused is run again (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #613 kiri update with a container is Docker's update; the suite can run through kiri's commands (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `update-memory`, `update-cpus`, `update-restart-policy`, `update-kept`

### #612 Engine: nine more answers as Docker gives them (unreleased)
- Fixes: #410 Docker Desktop import drops each container's writable layer (data outside volumes is lost); docker diff fails with chroot error
- Caused: none known
- Caused by: -
- Check: `limit-shares-default`, `limit-shares-inspect`, `status-tag`, `status-connect`, `status-disconnect`, `status-exec-detached`, `status-start-still-204`, `api-too-old`, `api-too-old-status`, `api-min-served`, `api-newer-served`, `cp-proc-by-link`, `cp-sys-by-link`, `cp-link-still-copies`, `cp-tar-mode`, `diff-never-started`, `commit-never-started`, `create-missing-network`, `missing-network-inspect`, `start-missing-network`, `start-missing-network-status`, `run-missing-network`, `run-missing-network-no-pull`, `run-missing-network-removed`

### #600 Engine: a missing container named at create is a 400, as Docker answers, so the docker CLI no longer pulls the image first (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `link-missing`, `netns-missing`, `pid-container-unknown`, `volumes-from-unknown`

### #596 Engine: image inspect reports uncompressed layer IDs, and image labels are read once per image (unreleased)
- Fixes: #573 image inspect reports compressed manifest digests in RootFS.Layers instead of uncompressed diff_ids (breaks trivy image --image-src docker)
- Caused: none known
- Caused by: #393
- Check: `image-inspect-rootfs-diffid`, `image-labels-inspect`, `image-labels-filter`, `image-labels-filter-not`, `image-labels-api-empty`, `container-labels-inherited`, `container-labels-override`, `commit-keeps-labels`

### #360 A user's own group is one of its supplementary groups, as with Docker (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `user-groups-primary`, `user-groups-exec`

### #276 Images: `docker images localhost:5000/app` finds the image (a registry port is not a tag) (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #552 Engine: a taken container name says which container has it, as Docker does (Testcontainers reaper) (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #592 Engine: a pull sent with a JSON body no longer breaks the next request (Testcontainers for Node) (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #393 Engine: expose image Config.Labels and inherit image labels on container create (unreleased)
- Fixes: #206 Image inspect omits Config.Labels even though labels exist in the exported image config
- Caused: #596, reading each image's config from disk on every listing slowed 200-image docker images calls from ~0.4 ms to ~4-5 ms (found while updating image inspect)
- Caused by: -
- Check: none

### #590 Stop on a stopped container, and removing a build already gone, are no longer errors (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `machine-stop-again`, `machine-start-again`

### #588 Engine: eight Docker behaviour differences fixed (--volumes-from, --pid, docker top, exposed ports, connection close) (unreleased)
- Fixes: #347 Engine: `--pid container:NAME` and `--pid host` are dropped without an error; #348 API: `docker top` ignores the ps arguments (`ps_args`); #423 Kiri reports a custom bridge name that it does not use; #440 Images: a damaged images.json is repaired without a warning, and the image made last drops out of the list; #450 Engine: `--volumes-from` is ignored without a word, so backups made with it are empty; #479 .NET Testcontainers 4.11.0 hangs after container-start 204 response on Kiri 0.2.1; #492 docker inspect leaves unpublished EXPOSE ports out of NetworkSettings.Ports, so Traefik's docker provider cannot find a container's port; #520 Daemon ignores `Connection: close` on Windows named pipe; Docker.DotNet / Testcontainers hang on 204 responses; #551 Engine exec inspect returns status code null for not finished commands
- Caused: none known
- Caused by: #242 (for #440)
- Check: `connection-close-start`, `http10-start-closes`, `keep-alive-start-stays`, `connection-close-said`, `http10-ping-closes`, `pid-mode-inspect`, `pid-container-sees`, `pid-container-pid1`, `pid-host-sees-more`, `pid-own-default`, `pid-container-stopped`, `pid-container-unknown`, `bridge-name-refused`, `bridge-name-nothing-made`, `bridge-name-empty`, `port-exposed-null`, `port-exposed-stopped`, `volumes-from-running`, `volumes-from-ro`, `volumes-from-inspect`, `volumes-from-stopped`, `volumes-from-unknown`, `volumes-from-anon-kept`, `top-aux`, `top-aux-master`, `top-eo`, `top-eo-rss`, `top-ps-args-refused`, `ij-setup`, `ij-info-warns`, `ij-info-names-lost`, `ij-layers-kept`, `ij-layers-kept-next-start`, `ij-warning-gone`

### #584 Networks: a container can leave its first network, as with Docker (unreleased)
- Fixes: #418 Kiri refuses to disconnect a container from its first network, and .NET Aspire (DCP) fails
- Caused: none known
- Caused by: -
- Check: `leave-first-network`, `leave-first-inspect`, `leave-first-default-route`, `leave-first-dns`, `leave-first-no-eth1`, `leave-first-running-refused`, `leave-last-network`, `leave-last-reconnect`, `leave-first-network-rm`

### #582 Engine: an empty --entrypoint clears the image's, as Docker does (unreleased)
- Fixes: #445 docker run --entrypoint= (empty) fails: exec "/usr/local/bin/": is a directory (Docker resets the entrypoint)
- Caused: none known
- Caused by: -
- Check: `entrypoint-empty-eq`, `entrypoint-empty-arg`, `entrypoint-empty-no-cmd`

### #581 DNS: a container's networks are searched by network name, as Docker does (unreleased)
- Fixes: #294 Embedded DNS answers from the first-joined network, so Compose service aliases resolve to another project's container
- Caused: none known
- Caused by: -
- Check: `dns-own-project-first`, `dns-shared-still-searched`

### #562 Engine: docker load accepts gzip-compressed archives (unreleased)
- Fixes: #528 Windows: docker load fails on gzip-compressed archives: "numeric field did not have utf-8 text ... when getting cksum"
- Caused: none known
- Caused by: -
- Check: `load-gzip`

### #451 Engine: the ancestor filter matches the image a name means, as in Docker, not every tag of it (unreleased)
- Fixes: #442 Engine: `docker ps --filter ancestor=alpine` also lists containers of other alpine tags
- Caused: none known
- Caused by: -
- Check: `filter-ancestor-tag`, `filter-ancestor-id`, `filter-ancestor-no-glob`

### #428 Engine: --add-host overrides host.docker.internal, as on Docker (unreleased)
- Fixes: #425 `--add-host host.docker.internal:<ip>` is ignored: Kiri's own entry comes first in /etc/hosts
- Caused: none known
- Caused by: -
- Check: `add-host-override`

### #507 Networks: a container on the default bridge that joins a user network resolves its names (unreleased)
- Fixes: #449 A container on the default bridge that joins a user network can't resolve that network's names (k3d's load balancer fails)
- Caused: none known
- Caused by: -
- Check: `connect-running-resolv`, `connect-running-dns`, `connect-before-start-dns`, `connect-dns-internet`

### #448 Networks: the id filter is a regular expression, as in Docker, so k3d finds its network (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `filter-network-id-regex`, `filter-network-id-short`

### #351 Networks: the address pool skips only on-link routes, as Docker does (unreleased)
- Fixes: #228 Linux: `docker network create` without an explicit subnet always fails with "no free subnet left" (breaks kind)
- Caused: none known
- Caused by: -
- Check: none

### #564 Localization: Korean's Compose Discard is not Cancel (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #576 Localization: Italian's Switch back is not Go back (unreleased)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #561 UI: First launch — a failed setup step waits in a checklist instead of closing Kiri (0.2.2)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #558 UI: Compose — Remove project takes it off the list, and an empty section says so every time (0.2.2)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #548 UI: Lists — a refresh no longer lays out every row again (0.2.2)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #547 UI: Compose — folding a project keeps its row under the pointer (0.2.2)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #540 Networks: Testcontainers for .NET joins only its own network, and aliases resolve (0.2.2)
- Fixes: #407 Container created with only NetworkingConfig.EndpointsConfig also joins bridge; network aliases don't resolve (breaks Testcontainers WithNetworkAliases)
- Caused: none known
- Caused by: -
- Check: `testcontainers-endpoints`

### #521 Engine: a named pipe read, or a writer that never stops, no longer holds the agent (0.2.2)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #519 Windows: the window opens whatever the page's size (0.2.2)
- Fixes: #495 Windows: Blank screen on launch - assembled page exceeds WebView2 NavigateToString 2MB limit; #281 [Windows] RTL languages (Arabic and Persian) cause blank UI after restart
- Caused: none known
- Caused by: -
- Check: none

### #506 Security: import runs only an official image as its volume helper (0.2.2)
- Fixes: no issue
- Caused: none known
- Caused by: #491
- Check: none

### #505 Windows: launching Kiri at login no longer undoes a Stop (0.2.2)
- Fixes: #394 Windows: launching Kiri at login undoes a Stop
- Caused: none known
- Caused by: -
- Check: none

### #491 Import: volumes, shared-network containers and digest-pinned images come across; a volume in use is not copied (0.2.2)
- Fixes: #345 kiri import copies a volume while a running container is writing to it, and reports done; #375 kiri import: containers using --network container:<id> fail with "conflicting options: hostname and the network mode" and drop container options; #376 kiri import: volume import fails when source lacks 5 hardcoded helper images, and container import leaves empty volumes marked "Already in Kiri"; #377 kiri import: containers using digest-pinned untagged images (repo@sha256:...) fail with "No such image"
- Caused: #506, the volume-import helper was chosen by image name and author-set creation date rather than an official pull digest (found in a review of merges since 0.2.1)
- Caused by: -
- Check: none

### #334 Container files in Finder show volumes and bind mounts (0.2.2)
- Fixes: #317 macOS: a container's Files tab shows only the image's files; volumes, bind mounts and files written at runtime show "This folder is empty"
- Caused: none known
- Caused by: -
- Check: `finder-volume`, `finder-bind`

### #332 Mac: the traffic lights stay in the strip after switching themes (0.2.2)
- Fixes: #187 macOS: window traffic-light buttons shift upward after switching themes
- Caused: none known
- Caused by: -
- Check: none

### #352 Mac: ⌘C, ⌘V, ⌘X, ⌘A and ⌘Z work in the window's text fields (0.2.2)
- Fixes: #340 Unable to copy & paste anything from/to textfield in setting panel
- Caused: none known
- Caused by: -
- Check: none

### #60 Security: Linux, WSL: containers reach the internet and are firewalled on kernels before 6.3 (0.2.2)
- Fixes: #44 Bridge-network containers have no outbound connectivity on Windows/WSL 2 (no MASQUERADE rule for bridge subnets); #432 Linux host on kernel 5.15: containers get no network ("kiri-init: no uplink: Invalid argument")
- Caused: none known
- Caused by: -
- Check: none

### #406 Ports: a published TCP connection stays open however long it is quiet (0.2.2)
- Fixes: #388 Published ports close TCP connections after ~300 s of inactivity
- Caused: none known
- Caused by: -
- Check: none

### #476 Engine: a tmpfs volume keeps its owner and mode; e2e checks for healthchecks, Compose and locked-down runs (0.2.2)
- Fixes: #296 Engine: a tmpfs volume loses its uid, gid and mode when the image doesn't have the mount target; #358 e2e has no checks for healthchecks, Compose depends_on conditions, up --wait or up --build
- Caused: none known
- Caused by: -
- Check: `health-healthy`, `health-unhealthy`, `health-streak`, `health-timeout`, `health-start-interval`, `health-filter-unhealthy`, `health-filter-healthy`, `compose-wait`, `compose-db-healthy`, `compose-built`, `compose-host-to-ctr`, `compose-ctr-to-host`, `compose-down-keeps`, `locked-tmpfs`, `locked-ro-root`, `locked-caps`, `locked-nnp`, `locked-not-pid1`, `locked-only-lo`, `tmpfs-volume-owner`, `tmpfs-volume-owner-restart`, `volume-copy-up-owner`

### #452 Engine: a damaged container list no longer costs containers their logs, files or volumes (0.2.2)
- Fixes: #438 Engine: a damaged containers.json makes the engine delete every container's logs and files
- Caused: #493 (open PR), jsonfile::sync opened a read-only handle before calling sync_all(), failing with PermissionDenied (os error 5) on Windows (found by jsonfile::tests::replace_leaves_only_the_new_file)
- Caused by: -
- Check: `cj-copy-kept`, `cj-info-warns`, `cj-readable-listed`, `cj-nothing-deleted`, `cj-nothing-deleted-after-run`, `cj-readable-logs`, `cj-readable-files`, `cj-prune-refused`, `cj-prune-kept-volumes`, `cj-volume-rm-refused`, `cj-volume-rm-kept-volumes`, `cj-next-start-deletes-nothing`

### #447 Engine: the agent does not wait on an old process's output when its process ID is reused (0.2.2)
- Fixes: no issue
- Caused: none known
- Caused by: #341
- Check: none

### #363 Engine: an --init container's PID 1 keeps none of the agent's descriptors (0.2.2)
- Fixes: #248 Removing a network leaks its DNS listener: next network on the same subnet fails with 'dns bind: Address in use'; #439 Linux: an agent crash goes unnoticed while an `--init` container runs, and the engine hangs until restarted
- Caused: none known
- Caused by: -
- Check: `network-rm-init-gateway-dns`, `network-rm-failed-start-gateway-dns`, `network-rm-init-same-subnet`, `network-rm-init-dns`, `init-stdin-eof`

### #255 Engine: loop devices in the guest kernel (0.2.2)
- Fixes: #254 losetup fails in privileged containers: the guest kernel has no loop devices
- Caused: none known
- Caused by: -
- Check: none

### #372 /info reports the daemon's time, so Compose 5.7 attaches (0.2.2)
- Fixes: #346 `/info` returns an empty `SystemTime`, which breaks attached `docker compose up` and `docker compose logs -f` on Compose 5.7+
- Caused: none known
- Caused by: -
- Check: `info-system-time`

### #313 Fixes from reviewing the 0.2.2 merges: VM boot, docker rm, import and setup edge cases (0.2.2)
- Fixes: #408 Kiri `import` command not working
- Caused: none known
- Caused by: #267, #306, #307, #312
- Check: `crash-rm-frees-layer`, `created-rm-frees-layer`

### #398 Fix layer extraction of hard links to symlinks (0.2.2)
- Fixes: #302 Layers with an absolute hard-link target fail to unpack (failed to unpack usr/bin/perl5.38.2); #409 Unpack fails when a hard link targets a symlink (breaks Nix images such as supabase/postgres)
- Caused: none known
- Caused by: -
- Check: `layer-hardlink-symlinks`

### #390 Testcontainers' Mongo, and other exec-based containers, start on Kiri (0.2.2)
- Fixes: #387 exec start / attach answer 101 to clients that didn't ask to upgrade; Testcontainers reads no ExitCode
- Caused: none known
- Caused by: -
- Check: `testcontainers-exec`

### #198 Engine: close the connection when the client sends `Connection: close` (0.2.2)
- Fixes: #225 Windows: Docker API ignores Connection: close, so a body-less 204 (container start) hangs Testcontainers for .NET
- Caused: none known
- Caused by: -
- Check: `testcontainers-close`

### #343 Containers: exec straight after start finds the image's users (0.2.2)
- Fixes: #311 Linux: immediate docker exec fails with unknown user after starting a container with a fresh named volume; #416 Named volume is only partly populated when the container is reported started; #527 PUT /containers/{id}/archive right after /start answers 200 but the file never appears (breaks Testcontainers LocalStack)
- Caused: none known
- Caused by: -
- Check: `testcontainers-copy-after-start`, `volume-user-ready`

### #242 image store: atomic writes, a write lock, and fail-soft index recovery (0.2.2)
- Fixes: #224 image store: a shorter rewrite leaves trailing bytes in images.json, and every pull fails
- Caused: #440 (fixed in #588), repairing a damaged images.json happened silently with no docker info warning and let the next start delete unpacked layers of dropped images (found reproducing index recovery)
- Caused by: -
- Check: none

### #403 Engine: images list newest first, and a missing working directory is made for non-root and read-only containers (0.2.2)
- Fixes: #364 Image list order differs from Docker Engine: older images appear first; #370 Non-root container with a missing WorkingDir fails to start (os error 2); breaks DDEV
- Caused: none known
- Caused by: -
- Check: `images-newest-first`, `workdir-created-nonroot`, `workdir-created-nonroot-mode`, `workdir-created-readonly`, `workdir-created-readonly-nonroot`

### #392 Engine: the name filter is a regular expression, as in Docker, so k3d finds its nodes (0.2.2)
- Fixes: #385 Engine: `name` filter is not a regex (`^[/]?name$` matches nothing), so `k3d cluster create` fails
- Caused: none known
- Caused by: -
- Check: `filter-name-regex-k3d`, `filter-name-regex-optional-slash`, `filter-name-regex-class`, `filter-name-regex-anchored`

### #396 Tests: kiri-init unit tests compile again after #371 (0.2.2)
- Fixes: no issue
- Caused: none known
- Caused by: #341, #371
- Check: none

### #318 Windows: Docker works again after WSL restarts (0.2.2)
- Fixes: #93 After long idle / Windows sleep, `docker` fails with "Kiri is stopped: press Start" and the bridge is not rebuilt
- Caused: #395 (open; items 1 and 4 addressed in #713), four leftovers in the Windows WSL restart path: bridge stays on an old engine for a session, a dead socket waits 25 s per start, back-off errors outlast their cause, and Start's retry loop can hold the engine thread (filed after #318)
- Caused by: -
- Check: none

### #298 Fix Linux container restart overlayfs EBUSY when rootfs mount stays pinned (0.2.2)
- Fixes: #230 Linux: container overlay mounts leak across a daemon restart — nothing can start until manually unmounted
- Caused: none known
- Caused by: -
- Check: `start-after-service-restart`

### #371 Security: seccomp profiles are applied, and the default filter matches Docker's (0.2.2)
- Fixes: #130 Container security: custom seccomp profiles without weakening Kiri host isolation; #327 Default seccomp filter denies read-only adjtimex (modes=0); #48 Gaps against OrbStack: engine doesn't restart after update, docker build fails (buildkit layer not unpacked), no inotify on bind mounts, no default seccomp
- Caused: #396, adding the seccomp field to Process broke compilation of Process::for_test from #341 (found running cargo test -p kiri-init on Linux)
- Caused by: -
- Check: `seccomp-info`, `seccomp-custom-profile`, `seccomp-custom-rest-allowed`, `seccomp-bad-profile-refused`, `seccomp-bad-profile-says-why`, `seccomp-vsock-custom`, `seccomp-exec-profile`, `seccomp-privileged-keeps-profile`, `seccomp-profile-denying-seccomp-runs`, `seccomp-profile-killing-seccomp-runs`, `seccomp-overlapping-rules-higher-index-first`, `seccomp-profile-late-under-no-new-privileges`, `seccomp-profile-early-without-no-new-privileges`

### #341 Engine: signals only reach running processes, and a failed start leaves no DNS name (0.2.2)
- Fixes: #324 Linux: a signal can reach a reused process ID after a container exits; #325 Linux: a failed start leaves the container's name in the embedded DNS
- Caused: #396, Process::for_test added here stopped compiling on Linux once #371 added the seccomp field to Process (found running cargo test -p kiri-init on Linux); #447, draining an exited process's pipes with a blocking read on PID reuse could hang the single-threaded agent if a leftover child still held the pipe open (found before 0.2.2 shipped)
- Caused by: -
- Check: `dns-failed-start-gone`, `failed-start-no-dns`

### #320 Bind mounts: saves made on the Mac reach file watchers in containers (0.2.2)
- Fixes: #310 Files changed on the Mac send no inotify events into containers (hot reload doesn't fire); #48 Gaps against OrbStack: engine doesn't restart after update, docker build fails (buildkit layer not unpacked), no inotify on bind mounts, no default seccomp
- Caused: none known
- Caused by: -
- Check: `bind-inotify`

### #321 One shared root mount per container, so docker cp never mounts it twice (0.2.2)
- Fixes: #227 Linux: every `docker build` fails — Kiri's internal builder container cannot start (mount rootfs: Resource busy)
- Caused: none known
- Caused by: -
- Check: `cp-during-start-one-mount`, `cp-during-restart`, `failed-start-frees-root`, `cp-stopped-unmounts`

### #312 Containers start and commit correctly on kernels with overlayfs index on (Arch, CachyOS) (0.2.2)
- Fixes: #227 Linux: every `docker build` fails — Kiri's internal builder container cannot start (mount rootfs: Resource busy); #246 buildx docker-container builder fails on first start: mount rootfs Resource busy (os error 16); #230 Linux: container overlay mounts leak across a daemon restart — nothing can start until manually unmounted
- Caused: #313, two concurrent first mounts of a fresh container could mark its layer plain and mount with kernel defaults (found reviewing the 0.2.2 merges)
- Caused by: -
- Check: `commit-renamed-dir`, `cp-then-start`

### #309 GET /distribution/{name}/json: image update checks (Watchtower, Arcane) work on Kiri (0.2.2)
- Fixes: #257 GET /distribution/{name}/json is missing, so image-update checks in Docker tools fail
- Caused: none known
- Caused by: -
- Check: `distribution`, `distribution-missing`

### #307 Opening Kiri from Finder no longer quits silently when a shell profile is a link to nothing (0.2.2)
- Fixes: #250 macOS: launching Kiri from Finder/Dock exits silently (open returns 0), and the log records command="" instead of what failed; #457 App exits with "Permission denied (os error 13)" when ~/.zshrc / ~/.zprofile are read-only
- Caused: #313, some setup steps still failed without naming the step, and unit tests wrote into ~/.kiri/logs (found reviewing the 0.2.2 merges)
- Caused by: -
- Check: none

### #306 kiri import: say plainly when OrbStack or Docker Desktop isn't running, and use whichever is (0.2.2)
- Fixes: #266 kiri import --list fails on a fresh machine when the default source context does not exist; #408 Kiri `import` command not working
- Caused: #313, kiri import --list with an item, unknown flags or --help ran a full import instead of showing usage (found reviewing the 0.2.2 merges)
- Caused by: -
- Check: none

### #273 docker inspect reports a container's port bindings and exposed ports (0.2.2)
- Fixes: #293 docker inspect returns HostConfig.PortBindings: null for published ports; #422 docker inspect omits HostConfig.PortBindings for published ports
- Caused: none known
- Caused by: -
- Check: `port-inspect`

### #270 Windows: Settings → Import brings images, volumes and containers over from Docker Desktop (0.2.2)
- Fixes: #268 [Bug] Docker image/container import fails on Windows
- Caused: none known
- Caused by: -
- Check: none

### #267 Engine: always and unless-stopped containers come back after the VM crashes (0.2.2)
- Fixes: #50 macOS: the VM stops several times a day under load (guest kernel BUG in skb_segment on ip_forward); --restart always containers stay down
- Caused: #313, putting the guest console log on the VM boot path blocked boot when the log was unwritable, and docker rm stopped deleting writable layers of crash-stopped, shutdown-stopped or never-started containers (found reviewing the 0.2.2 merges)
- Caused by: -
- Check: `crash-kill`, `crash-recovers`, `crash-restart-count`

### #264 Engine: restarts on the new version after every update (0.2.2)
- Fixes: #48 Gaps against OrbStack: engine doesn't restart after update, docker build fails (buildkit layer not unpacked), no inotify on bind mounts, no default seccomp
- Caused: none known
- Caused by: -
- Check: none

### #240 daemon: percent-decode the request path before routing (0.2.2)
- Fixes: #223 Docker API image inspect returns 404 when the repository slash is URL-encoded
- Caused: none known
- Caused by: -
- Check: `image-inspect-encoded-slash`, `image-inspect-plain-slash`

### #243 Engine: Testcontainers works — Docker engine version, kept-alive copies, the image's command for an empty Cmd (0.2.1)
- Fixes: #222 Testcontainers still unusable: /version reports 0.2.0, and wait strategies never see container logs; #83 fix(buildx): bootstrap can reset the container-start connection; #121 Command is ignored when starting a container from testcontainers (using http api)
- Caused: none known
- Caused by: -
- Check: `testcontainers-version`, `testcontainers-cmd`, `testcontainers-keepalive`, `testcontainers-follow`, `testcontainers-follow-created`, `testcontainers-timestamps`, `buildx-copy-then-start`, `buildx-bootstrap-loop`, `version-kiri-component`, `socket-mount-reaches-kiri`

### #238 UI: Sidebar — the Linux VM card fits in every language (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #234 Engine: Start and Stop no longer race; Stop is parallel; exit codes match Docker (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: #181, #183
- Check: `exit-126-not-executable`, `not-executable-msg`, `exit-127-missing-path`, `engine-stop`, `engine-stop-all-down`, `engine-stop-then-docker-stop`, `engine-start`, `engine-start-brings-back`, `engine-start-then-stop`, `engine-start-then-stop-none-run`, `engine-start-then-stop-refused`, `engine-start-again`, `engine-start-again-brings-back`, `ss-stop`, `ss-stop-all-down`, `ss-docker-stop-while-stopped`, `ss-start`, `ss-start-brings-back`, `ss-stop-again`, `ss-start-then-stop`, `ss-start-then-stop-none-run`, `ss-start-then-stop-refused`, `ss-restarted-start`, `ss-restarted-brings-back`, `ss-restarted-all-always`, `exit-127-missing`, `dir-msg`, `exec-dir-msg`, `no-shebang`, `missing-interpreter`, `exec-no-shebang`, `init-no-shebang`, `init-missing-interpreter`, `exit-126-exec`

### #233 USB: devices Kiri waits for are listed, can be let go, and are never lost on a quick drop (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: #212
- Check: none

### #232 Networks: disconnecting from two networks works, and the address is always given back (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `disconnect-first-then-second`, `connect-into-gap`, `connect-into-gap-addresses`, `disconnect-all-in-any-order`, `network-rm-after-disconnects`

### #231 Mounts: a linked bind folder follows its link again after a restart (Mac) (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: #178
- Check: `bind-link-repointed`

### #216 Containers: a stop that cancels a pending restart wins over that restart (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: #142
- Check: none

### #142 Containers: finish stopping before answering lifecycle requests (0.2.1)
- Fixes: no issue
- Caused: #216, making stop answer 304 when nothing is running left restarts held while Kiri was stopped uncancellable, and a stop racing a backoff restart still let the container start (found in follow-up testing)
- Caused by: -
- Check: `lifecycle-response-status`

### #212 USB: a device that drops off the Mac's bus is attached again when it comes back (0.2.1)
- Fixes: #202 USB device detaches by itself after ~8 minutes idle (ESP32-S3)
- Caused: #233, a USB device that returned and disconnected again before the watcher marked it back was lost from the wait list, and the wait list could not be seen or cancelled (found in review)
- Caused by: -
- Check: none

### #208 Guest kernel: amd64 containers start again under macOS 15's Rosetta (0.2.1)
- Fixes: #199 Every linux/amd64 container crashes with "rosetta error: unhandled auxillary vector type 29" (SIGTRAP) on Apple Silicon
- Caused: none known
- Caused by: -
- Check: `rosetta-no-auxv-29`

### #197 UI: Linux VM card — show the engine's real state after any Start or Stop (0.2.1)
- Fixes: #188 UI: the sidebar's VM card shows Off after Start from the main area's empty state
- Caused: none known
- Caused by: -
- Check: none

### #196 Give every container and exec a session of its own (0.2.1)
- Fixes: #191 Linux host: daemon gets SIGTERM and the whole engine stops on `docker compose stop` of containers restarted at boot
- Caused: none known
- Caused by: -
- Check: `session-own`, `exec-session-own`

### #186 Ports: say once why the VM's NAT network is unreachable (0.2.1)
- Fixes: #170 请看提交的报告
- Caused: none known
- Caused by: -
- Check: none

### #185 Terminal: a resize that macOS puts back is set again (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #184 Linux: stopping the engine removes its bridges and nftables table (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #183 Containers: a start failure names its real cause and gets Docker's exit code (0.2.1)
- Fixes: #170 请看提交的报告
- Caused: #234, failed-start exit codes still returned 127 instead of 126 for non-executable files and differed from Docker for directories and broken scripts
- Caused by: -
- Check: none

### #181 Stop: the daemon stays up, so launchd stops restarting it every 10 s and Start works (0.2.1)
- Fixes: #170 请看提交的报告
- Caused: #234, starting held containers in a background thread without locking against Stop let Start followed quickly by Stop leave containers running while Kiri reported stopped
- Caused by: -
- Check: none

### #178 Mounts: /etc paths work on a Mac, and a linked source is followed (0.2.1)
- Fixes: #170 请看提交的报告
- Caused: #231, resolving macOS bind-mount symlinks only at create meant repointing a link and restarting the container still mounted the old folder (regression from 0.2.0)
- Caused by: -
- Check: none

### #176 Ports: "already allocated" names the program that has the port (0.2.1)
- Fixes: #170 请看提交的报告
- Caused: none known
- Caused by: -
- Check: none

### #174 kiri on the PATH finds the guest kernel (0.2.1)
- Fixes: #170 请看提交的报告
- Caused: none known
- Caused by: -
- Check: none

### #161 CI: speed check rebuilds the change instead of keeping main's binary (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #120 A language picked from the menu shows everywhere at once (0.2.1)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #144 usb: don't poison PENDING when a device is already attached (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #52 Guest kernel: backport the upstream GRO frag_list fix behind the skb_segment BUG (0.2.0)
- Fixes: #50 macOS: the VM stops several times a day under load (guest kernel BUG in skb_segment on ip_forward); --restart always containers stay down
- Caused: none known
- Caused by: -
- Check: none

### #106 Non-root containers own their stdio, so php-fpm and nginx can open their logs (0.2.0)
- Fixes: #98 Non-root containers can't reopen stdout/stderr (/proc/self/fd/2, /dev/stderr → EACCES), which breaks php-fpm and nginx images
- Caused: none known
- Caused by: -
- Check: `stdio-reopen-nonroot`, `stdio-reopen-exec`, `stdio-dev-null-kept`

### #117 Windows: setup carries on after a failed engine step (0.2.0)
- Fixes: #92 `setup.json` `done:false` leaves Kiri silently dead — no retry, engine never starts
- Caused: none known
- Caused by: -
- Check: none

### #114 Start: send a container's layers again if the engine lost them (0.2.0)
- Fixes: #48 Gaps against OrbStack: engine doesn't restart after update, docker build fails (buildkit layer not unpacked), no inotify on bind mounts, no default seccomp
- Caused: none known
- Caused by: #2
- Check: `start-restores-layer`

### #113 docker system df: real sizes for images, containers and volumes (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `system-df-sizes`

### #111 kiri machine: shell works on Linux hosts and on Windows (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #110 Domain router: a container that publishes port 80 or 443 gets it (0.2.0)
- Fixes: #97 Domain router always binds ports 80/443, even with Domains turned off, and the app-started daemon ignores KIRI_HTTP_PORT
- Caused: none known
- Caused by: -
- Check: `domain-port-yield`, `domain-port-back`

### #109 CI: let runs on main finish instead of cancelling each other (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #76 Bind mounts: a new file without owner read can be created, as on Linux (0.2.0)
- Fixes: #72 Bind mounts (macOS): open(O_CREAT|O_WRONLY, 0200) fails with EACCES, breaking Node's native fs.cpSync
- Caused: none known
- Caused by: -
- Check: `bind-create-0200`

### #107 VM clock: keep it on the Mac's time after the Mac sleeps (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #108 machine shell: reach the same engine as the other machine commands (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #104 Copy into containers: accept gzip archives, so DDEV starts (0.2.0)
- Fixes: #99 Getting issue with DDEV
- Caused: none known
- Caused by: -
- Check: none

### #103 docker load: images saved by podman and older Docker load again (0.2.0)
- Fixes: #85 docker load / import always fail (400): "archive is missing <config>.json" though the file is in the tar — cannot ingest local images
- Caused: none known
- Caused by: -
- Check: none

### #102 Engine API: report the real kernel on Linux (0.2.0)
- Fixes: #79 fix(api): native Linux reports a hardcoded kernel version
- Caused: none known
- Caused by: -
- Check: none

### #101 Windows: setup reaches a proxy on Windows' loopback, and says why a pull failed (0.2.0)
- Fixes: #94 Setup failure is undiagnosable: "the test container did not run" hides the real cause; #91 Setup failure is undiagnosable: "the test container did not run" hides the real cause
- Caused: none known
- Caused by: -
- Check: none

### #96 USB passthrough (beta): USB devices in containers on macOS, Windows and Linux (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #43 docker run -i: see the end of stdin every time (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `stdin-end-every-time`

### #75 Mac: the Finder mount never covers your files or another mount at ~/Kiri (0.2.0)
- Fixes: #40 macOS: the Finder mount hides an existing ~/Kiri folder and force-unmounts other mounts
- Caused: none known
- Caused by: -
- Check: none

### #22 Linux: the window no longer flickers with WebKitGTK 2.54 (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #47 Linux packages install beside Docker's own Compose and buildx (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #67 docker push: RepoDigests names what the registry has, as with Docker (0.2.0)
- Fixes: #64 After `docker push`, RepoDigests holds the image ID instead of the registry's manifest digest
- Caused: none known
- Caused by: -
- Check: `push-repo-digest`, `push-failed-no-digest`

### #70 Builds: amd64 steps run through Rosetta on Apple silicon, not QEMU (0.2.0)
- Fixes: #66 bun install aborts under linux/amd64 emulation on Apple Silicon (QEMU SIGABRT), works on OrbStack
- Caused: none known
- Caused by: -
- Check: none

### #56 UI: keep Images Actions visible when a tag is long (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #63 Guest kernel: per-process I/O accounting, so Typesense starts (0.2.0)
- Fixes: #41 Crash on Typesense startup (SIGSEGV): kernel missing CONFIG_TASK_IO_ACCOUNTING (/proc/self/io)
- Caused: none known
- Caused by: -
- Check: none

### #51 Linux: revoke removed users' engine access (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #24 Views open at the top, not where the last page was scrolled to (0.2.0)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #37 Volumes over a bind get the image's files, as on Docker (0.1.18)
- Fixes: #30 bug: Sub-volumes under host bind mounts inherit host directory instead of base image layer
- Caused: none known
- Caused by: -
- Check: `vol-copyup-under-bind`

### #34 e2e: test the engine under test, never the installed Kiri (0.1.18)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #28 Linux: privileged containers get their own /dev, never the host's (0.1.18)
- Fixes: #26 Linux Host: Starting privileged container (kiri k8s) unlinks /dev/ptmx, breaking terminal creation
- Caused: none known
- Caused by: -
- Check: `privileged-host-ptmx`, `privileged-devices`, `privileged-tty`

### #21 e2e: wait for nginx on the internal network; ping without NET_RAW works, as on Docker (0.1.18)
- Fixes: no issue
- Caused: none known
- Caused by: #19
- Check: `cap-drop-net-raw`, `internal-same-net`, `cap-drop-net-raw-ping`

### #20 Guest kernel: the rest of netfilter, tc, socket diagnostics and the usual interfaces (0.1.18)
- Fixes: #14 Guest kernel: missing nftables expressions (reject, queue, ct label, socket, tproxy, log, limit, quota)
- Caused: none known
- Caused by: -
- Check: `kernel-features`

### #19 Containers can listen on any port and ping without root, as on Docker (0.1.18)
- Fixes: #18 Set `net.ipv4.ip_unprivileged_port_start` to 0 in containers
- Caused: #21, setting ping_group_range let ping work without NET_RAW, so the cap-drop-net-raw e2e check failed on main after #19 merged
- Caused by: -
- Check: `unpriv-port-start`, `unpriv-ping-range`, `unpriv-listen-80`, `unpriv-sysctl-wins`, `unpriv-port-host-net`

### #17 Containers' connections no longer break as others start and stop (0.1.18)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: `first-download-x8`, `download-while-others-start`

### #15 UI: Settings › Resources — panes beside the tabs, same sliders on every system, no needless restart on Windows (0.1.18)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #12 Security: Build steps keep the Mac's DNS; tighter limits on the DNS relay (0.1.18)
- Fixes: #27 Container DNS fails on Macs with Cisco Umbrella
- Caused: none known
- Caused by: -
- Check: `dns-default-bridge-gateway`, `dns-default-bridge-no-loopback`, `build-resolvconf`, `build-no-public-resolver`, `build-dns`

### #11 Container DNS survives the Mac's VM resolver going down; a deleted Kiri hands docker build back (0.1.18)
- Fixes: #27 Container DNS fails on Macs with Cisco Umbrella
- Caused: none known
- Caused by: -
- Check: none

### #9 Generated passwords: draw fresh unbiased random numbers for each choice (0.1.18)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #8 Crash reports go without your account and image names; redact more (0.1.17)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #5 Keep the update switch off, send only new crashes, redact more secrets (0.1.16)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #3 Commands end quietly when their output is closed, instead of crashing (0.1.15)
- Fixes: no issue
- Caused: none known
- Caused by: -
- Check: none

### #2 Keep unpacked layers whole across VM stops (0.1.15)
- Fixes: no issue
- Caused: #114, VM boot deleted unpacked layers without the new marker, so containers created earlier failed to start with 'layer ... is not unpacked' (found updating from 0.1.13 to main; also reported in #48)
- Caused by: -
- Check: none
