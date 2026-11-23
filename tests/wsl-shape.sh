#!/bin/bash
# The DNS checks of tests/e2e.sh on a Linux host shaped like a Windows (WSL
# 2) install: the host's only resolver an address of its own on `lo`
# (tests/lib/wsl-shape.sh). These are the checks that failed on a real
# Windows install before #624 and pass on a plain Linux host either way.
# Usage, as root, with the engine on this host:
#
#   KIRI_TEST_ALLOW_HOST_CHANGES=1 KIRI_ENGINE_RESTART='systemctl restart kiri' tests/wsl-shape.sh
#
# It shapes the host, restarts the engine with KIRI_ENGINE_RESTART (a shell
# command; the engine reads the resolver when its agent starts), runs the
# checks (KIRI_TEST_ONLY, below) and, also when they fail or it is stopped,
# puts the host back and restarts the engine again. Without
# KIRI_ENGINE_RESTART, start the engine yourself while the shape is up
# (tests/lib/wsl-shape.sh up); the wrapper refuses an engine whose agent
# started before the shape. Everything else is passed on to tests/e2e.sh
# (DOCKER_CONTEXT, KIRI_TEST_REGISTRY, KIRI_TEST_STRICT...). Exits as it does.

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
SHAPE=$HERE/lib/wsl-shape.sh
CHECKS='^(dns-default-bridge-gateway|dns-default-bridge-no-loopback|build-resolvconf|build-no-public-resolver|build-dns|udp-dns|udp-dns-tcp-same-port|udp-many|udp-port-listed|udp-inspect|user-net-own-dns-server|user-net-own-dns-names|user-net-own-dns-upstream)$'
export DOCKER_CONTEXT=${DOCKER_CONTEXT:-kiri}
SOCK=$(docker context inspect "$DOCKER_CONTEXT" --format '{{.Endpoints.docker.Host}}' 2>/dev/null); SOCK=${SOCK#unix://}

restart_engine() {
  echo "wsl-shape: restarting the engine: $KIRI_ENGINE_RESTART"
  bash -c "$KIRI_ENGINE_RESTART" || return 1
  for _ in $(seq 120); do docker version >/dev/null 2>&1 && return 0; sleep 0.5; done
  echo "wsl-shape: the engine does not answer after the restart"; return 1
}
# The engine's agents (kiri-init children of the process serving $SOCK)
# that started before the shape went up, and so read the old resolver.
stale_agents() {
  local epid since a
  epid=$(ss -xlpn 2>/dev/null | grep -F " $SOCK " | sed -n 's/.*pid=\([0-9]*\).*/\1/p' | head -1)
  [ -n "$epid" ] || return 0
  since=$(( $(date +%s) - $(stat -c %Y /run/kiri-test-wsl-shape) ))
  for a in $(pgrep -P "$epid" -x kiri-init); do
    [ "$(ps -o etimes= -p "$a" | tr -d ' ')" -gt "$since" ] && echo "$a"
  done
}

finish() {
  local rc=$?
  trap - EXIT INT TERM
  "$SHAPE" down
  [ -n "${KIRI_ENGINE_RESTART:-}" ] && restart_engine
  exit "$rc"
}

"$SHAPE" up || exit 1
trap finish EXIT
trap 'exit 130' INT TERM
if [ -n "${KIRI_ENGINE_RESTART:-}" ]; then
  restart_engine || exit 1
elif [ -n "$(stale_agents)" ]; then
  echo "wsl-shape: the engine's agent started before the shape and still uses the old resolver; restart the engine, or set KIRI_ENGINE_RESTART"
  exit 1
fi
KIRI_TEST_ONLY=${KIRI_TEST_ONLY:-$CHECKS} "$HERE/e2e.sh"
