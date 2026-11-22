#!/bin/bash
# End-to-end tests: drives a running `kiri daemon` through the real docker
# CLI (context `kiri`) and checks every result. Usage:
#
#   tests/e2e.sh            # functional tests (DOCKER_CONTEXT=kiri-dev for another Kiri)
#   SOAK=500 tests/e2e.sh   # plus a leak soak of 500 container runs
#   KIRI_TEST_STRICT=1 tests/e2e.sh   # what the setup lacks fails, not skipped
#
# Checks for another system's feature are skipped, as are those needing what
# this setup lacks; the summary counts them by reason (`only`, `needs` below).
#
# Exits non-zero if any check fails.

set -u
export DOCKER_CONTEXT=${DOCKER_CONTEXT:-kiri}
D=docker
PASS=0
FAIL=0
SKIP=0
FAILED=()
# The Kiri under test. A second Kiri (context kiri-dev, kiri-rc...) has its
# own Finder folder (~/Kiri Dev) and no *.kiri.localhost routers: there is
# one pair of ports 80 and 443 on a Mac, and the installed Kiri has them.
FEATURE=${DOCKER_CONTEXT#kiri}; FEATURE=${FEATURE#-}
# The engine under test's socket. Every `kiri` command (machines, k8s) and
# every direct API call goes there, never to another Kiri on this Mac: once,
# the Kubernetes checks ran on the installed Kiri and deleted its cluster.
SOCK=$(docker context inspect "$DOCKER_CONTEXT" --format '{{.Endpoints.docker.Host}}' 2>/dev/null); SOCK=${SOCK#unix://}
[ -S "$SOCK" ] || { echo "e2e: no engine socket for context $DOCKER_CONTEXT ($SOCK)"; exit 1; }
export KIRI_SOCKET=$SOCK
# The `kiri` CLI: $KIRI, else a source build next to these tests, else the
# binary of the engine under test (the process holding its socket), as on a
# Mac that only has the app. Absolute: machine-cwd runs it from another folder.
if [ -z "${KIRI:-}" ]; then
  KIRI=$(cd "$(dirname "$0")/.." && pwd)/target/release/kiri
  [ -x "$KIRI" ] || KIRI=$(lsof -a -d txt -Fn -p "$(lsof -t "$SOCK" 2>/dev/null | head -1)" 2>/dev/null | sed -n 's/^n//p' | head -1)
fi
# KIRI_TEST_CLI=kiri: every check goes through Kiri's own commands (`kiri
# run`, `kiri build`, `kiri compose`...) and not through `docker`. They are
# what people are told to use, so they must pass everything `docker` does.
# One word with no spaces, as $D is used unquoted. `docker import` is
# `kiri image import` there: `kiri import` brings things over from other tools.
if [ "${KIRI_TEST_CLI:-docker}" = kiri ]; then
  [ -x "$KIRI" ] || { echo "e2e: KIRI_TEST_CLI=kiri, but no kiri binary found (set KIRI)"; exit 1; }
  KIRI_AS_D=$(mktemp -d)/kiri
  printf '#!/bin/sh\n[ "$1" = import ] && { shift; set -- image import "$@"; }\nexec %s "$@"\n' "$(printf %q "$KIRI")" > "$KIRI_AS_D"
  chmod +x "$KIRI_AS_D"
  D=$KIRI_AS_D
  trap 'rm -rf "$(dirname "$KIRI_AS_D")"' EXIT
  echo "e2e: commands go through $KIRI"
fi
if [ -z "$FEATURE" ]; then KF="$HOME/Kiri"; else KF="$HOME/Kiri $(printf %s "${FEATURE:0:1}" | tr a-z A-Z)${FEATURE:1}"; fi
KFQ=$(printf %q "$KF")

# t NAME EXPECTED COMMAND... : passes when the command's output (stdout+stderr,
# trimmed) contains EXPECTED.
t() {
  gate "$1" || return 0
  local name=$1 want=$2
  shift 2
  local got
  got=$(perl -e 'alarm 60; exec @ARGV' "$@" 2>&1 | tr -d '\r')
  if [[ "$got" == *"$want"* ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    FAILED+=("$name")
    printf '  FAIL %s\n    want: %s\n    got:  %s\n' "$name" "$want" "$(echo "$got" | head -5)"
  fi
}

# code NAME EXPECTED_EXIT COMMAND... : passes when the exit code matches.
code() {
  gate "$1" || return 0
  local name=$1 want=$2
  shift 2
  perl -e 'alarm 60; exec @ARGV' "$@" >/dev/null 2>&1
  local got=$?
  if [[ $got == "$want" ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    FAILED+=("$name")
    printf '  FAIL %s: exit %s, want %s\n' "$name" "$got" "$want"
  fi
}

section() { printf '\n== %s\n' "$1"; SEC_ONLY=; SEC_NEEDS=; }
# skip NAME REASON: not run here, counted by reason in the summary.
SKIP_WHY=()
skip() { SKIP=$((SKIP + 1)); SKIP_WHY+=("$2"); printf '  SKIP %s (%s)\n' "$1" "$2"; }

# Where each check runs. Kiri runs containers in its own Linux VM on macOS
# (SYS=mac), and on the host's own kernel on Linux (linux) and in WSL 2
# (wsl). The engine says which: the VM's kernel is Kiri's ("6.18-kiri"),
# WSL 2's is Microsoft's. KIRI_TEST_SYSTEM=mac|linux|wsl says it instead.
KVER=$($D version --format '{{.Server.KernelVersion}}' 2>/dev/null)
case ${KIRI_TEST_SYSTEM:-} in
  mac | linux | wsl) SYS=$KIRI_TEST_SYSTEM ;;
  *) case $KVER in *-kiri) SYS=mac ;; *[Mm]icrosoft*) SYS=wsl ;; *) SYS=linux ;; esac ;;
esac
sys_name() { case $1 in mac) echo macOS ;; linux) echo "Linux host" ;; wsl) echo WSL ;; *) echo "$1" ;; esac; }
sys_names() { local out='' s; for s in ${1//,/ }; do out="${out:+$out and }$(sys_name "$s")"; done; echo "$out"; }
# What this setup lacks for some checks: NO_<WHAT> is empty when it is
# there, else the reason. `needs WHAT` skips a check while it is missing;
# KIRI_TEST_STRICT=1 (for runs meant to have everything) fails it instead.
# shellcheck disable=SC2034 # read through `missing`
NO_TTY='' NO_BUILDX='' NO_FINDER='' NO_AMD64='' NO_HOMES_DIR=''
# A terminal: macOS's BSD script (util-linux's takes other arguments), else
# a small pty helper in Python.
if [ "$(uname -s)" = Darwin ] && command -v script >/dev/null; then PTY="script -q /dev/null"
elif python3 -c 'import pty' >/dev/null 2>&1; then PTY="python3 $(printf %q "$(cd "$(dirname "$0")" && pwd)/lib/pty-run.py")"
else PTY=false; NO_TTY="no terminal (neither BSD script nor python3)"; fi
docker buildx version >/dev/null 2>&1 || NO_BUILDX="buildx not installed"
# A session without the Mac's disk access (SSH, a fresh terminal) is refused ~/Kiri.
[ "$SYS" = mac ] && case $(ls "$KF" 2>&1 >/dev/null) in *'Operation not permitted'*) true ;; *) false ;; esac && NO_FINDER="no access to $KF from this session (Full Disk Access)"
# A file's SHA-256, whichever tool the system has.
if command -v shasum >/dev/null; then SHA="shasum -a 256"; else SHA=sha256sum; fi

# only SYSTEMS [CHECK...]: the check (t NAME ... or code NAME ...) runs on
# SYSTEMS (mac, linux, wsl, or a comma list), and elsewhere is skipped as not
# there by design. Without a check: the rest of the section.
# needs WHAT [CHECK...]: the same for what the setup lacks (NO_<WHAT> above).
on() { [[ ",$1," == *",$SYS,"* ]]; }
only() {
  [ $# -eq 1 ] && { SEC_ONLY=$1; return; }
  local systems=$1; shift
  if on "$systems"; then "$@"; else skip "$2" "$(sys_names "$systems") only"; fi
}
missing() { local v; v=NO_$(printf %s "$1" | tr a-z- A-Z_); printf %s "${!v:-}"; }
needs() {
  [ $# -eq 1 ] && { SEC_NEEDS=$1; return; }
  local why; why=$(missing "$1"); shift
  if [ -z "$why" ]; then "$@"; else skip_setup "$2" "$why"; fi
}
# skip_setup NAME REASON: missing in this setup, a failure under KIRI_TEST_STRICT=1.
skip_setup() {
  if [ "${KIRI_TEST_STRICT:-0}" = 1 ]; then
    FAIL=$((FAIL + 1)); FAILED+=("$1"); printf '  FAIL %s (strict: %s)\n' "$1" "$2"
  else skip "$@"; fi
}
# Whether check $1 runs in this section: called first by t and code.
gate() {
  if [ -n "${SEC_ONLY:-}" ] && ! on "$SEC_ONLY"; then skip "$1" "$(sys_names "$SEC_ONLY") only"; return 1; fi
  if [ -n "${SEC_NEEDS:-}" ] && [ -n "$(missing "$SEC_NEEDS")" ]; then skip_setup "$1" "$(missing "$SEC_NEEDS")"; return 1; fi
}
# What a check expects where it differs by system. In the VM: the default
# bridge's gateway is 172.17.0.1 and is also its nameserver (the agent
# answers there); the VM's eth0 is on Apple's NAT (192.168.64.x); the VM's
# own network has the kernel's ip_unprivileged_port_start, 1024. On a Linux
# host or in WSL these are the host's, read from this shell, which runs on
# that host (the engine's socket is local) and shares its network and its
# /etc/resolv.conf.
BRIDGE_GW=172.17.0.1 DEFAULT_NS=172.17.0.1 HOSTNET_ADDR=192.168.64. HOSTNET_IF=eth0 HOST_UPS=1024
if [ "$SYS" != mac ]; then
  # Docker's 172.17.0.0/16 unless the host already routes it (Kiri in another
  # engine's container): then the first free subnet (daemon/network.rs).
  if ip -4 route 2>/dev/null | grep -v ' dev kiri0 ' | grep -q '^172\.17\.'; then
    BRIDGE_GW=$($D network inspect bridge -f '{{range .IPAM.Config}}{{.Gateway}}{{end}}' 2>/dev/null)
  fi
  # The default bridge names the host's resolvers, as the agent reads them
  # (kiri-init transport.rs host_dns): /etc/resolv.conf's but loopback, else
  # systemd-resolved's upstreams; a resolver at one of the host's own
  # addresses (WSL's forwarder) is asked through the agent at 127.0.0.11
  # (container.rs embedded_dns). Docker on Linux also names the host's.
  ns() { sed -n 's/^nameserver[[:space:]]*\([0-9.]*\).*/\1/p' "$1" 2>/dev/null | grep -v '^127\.' | head -1; }
  HOST_NS=$(ns /etc/resolv.conf); [ -n "$HOST_NS" ] || HOST_NS=$(ns /run/systemd/resolve/resolv.conf)
  if [ -n "$HOST_NS" ] && ip -4 -o addr 2>/dev/null | grep -qF " inet $HOST_NS/"; then DEFAULT_NS=127.0.0.11; elif [ -n "$HOST_NS" ]; then DEFAULT_NS=$HOST_NS; fi
  # --network host is the host's own network: its address towards the internet.
  HOSTNET_ADDR="inet $(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p')/" HOSTNET_IF=
  HOST_UPS=$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start 2>/dev/null)
fi
BRIDGE_NET=$(echo "$BRIDGE_GW" | cut -d. -f1-2)
echo "e2e: $(sys_name "$SYS") (kernel $KVER); terminal: ${NO_TTY:-yes}; buildx: ${NO_BUILDX:-yes}$([ "$SYS" = mac ] && echo "; Finder: ${NO_FINDER:-yes}")"
# Waits up to 10 s for a URL to answer, as you would for any freshly started server.
ready() { for _ in $(seq 50); do curl -s -m 1 -o /dev/null "$1" && return 0; sleep 0.2; done; return 1; }
# Waits until the nginx in container $1 serves (it starts a moment after run -d returns).
nginx_up() { for _ in $(seq 50); do $D exec "$1" wget -qO- -T 1 http://127.0.0.1/ >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }
# The containers and networks that were here before the suite started are
# the user's: cleanup removes only what the suite made.
KEEP_C=" $($D ps -aq --no-trunc | tr '\n' ' ') "
KEEP_N=" $($D network ls -q --no-trunc --filter type=custom 2>/dev/null | tr '\n' ' ') "
cleanup() {
  for c in $($D ps -aq --no-trunc); do [[ "$KEEP_C" == *" $c "* ]] || $D rm -f "$c" >/dev/null 2>&1; done
  for n in $($D network ls -q --no-trunc --filter type=custom 2>/dev/null); do [[ "$KEEP_N" == *" $n "* ]] || $D network rm "$n" >/dev/null 2>&1; done
}

cleanup

# Images come from KIRI_TEST_REGISTRY (tests/lib/images.sh), never from
# Docker Hub, whose hourly limit a few runs use up. The anonymous pulls left
# on Docker Hub now and at the end show whether the run made any.
IMAGES_SH="$(cd "$(dirname "$0")" && pwd)/lib/images.sh"
# shellcheck source=SCRIPTDIR/lib/images.sh
. "$IMAGES_SH"
HUB_LEFT0=$(kiri_hub_left)
kiri_seed_images $KIRI_IMAGES_E2E || echo "e2e: an image could not be fetched; the checks that run it will fail"
# Each Kiri's own builder: <context>-build (kiri-build for context kiri, and so on),
# so builds go to the Kiri under test and not to another one on this Mac.
BX=${KIRI_BUILDER:-$DOCKER_CONTEXT-build}
# buildx pulls its BuildKit image every time it makes the builder's
# container, also when the image is here. A builder this suite makes takes
# the image from its registry-qualified address; for Kiri's own builder, Kiri
# makes the container from the image already seeded (moby/buildkit), asked
# here before buildx would.
docker buildx inspect "$BX" >/dev/null 2>&1 || docker buildx create --name "$BX" --driver docker-container --driver-opt default-load=true --driver-opt "image=$(kiri_image_remote moby/buildkit:buildx-stable-1)" "$DOCKER_CONTEXT" >/dev/null
BXC=buildx_buildkit_${BX}0
if ! $D container inspect "$BXC" >/dev/null 2>&1 && [ "$(curl -s --unix-socket "$SOCK" http://d/kiri/builder | python3 -c 'import json,sys; print(json.load(sys.stdin).get("container", ""))' 2>/dev/null)" = "$BXC" ]; then
  curl -s -o /dev/null --unix-socket "$SOCK" -X POST -H 'Content-Type: application/json' -d '{}' "http://d/v1.47/containers/create?name=$BXC"
fi
# After a check pulled KIRI_TEST_REGISTRY's IMAGE: tagged IMAGE, the qualified tag removed.
as_plain() { $D tag "$KIRI_REG/$1" "$1" >/dev/null 2>&1 && $D rmi "$KIRI_REG/$1" >/dev/null 2>&1; }

section "engine"
t version-server "Kiri" $D version
t version-api "1.47" $D version --format '{{.Server.APIVersion}}'
# What Testcontainers needs (#222, #121), asked as docker-java asks: a Docker
# Engine version (Kiri's own beside it), the image's CMD for "Cmd": [], the
# connection kept after a copy, log follow, one timestamp a line.
t version-kiri-component "Kiri=" $D version --format '{{range .Server.Components}}{{.Name}}={{.Version}} {{end}}'
for c in version cmd keepalive close follow follow-created timestamps exec endpoints copy-after-start; do
  t "testcontainers-$c" "ok $c" python3 "$(dirname "$0")/testcontainers-api.py" "$SOCK" "$c"
done
# A request that asks for the close is answered, then closed: Docker.DotNet
# reads a 204 to the end of the stream and hung until its timeout (#520, #479).
# curl knows a 204 has no body and would not notice, so the probe sends the
# raw request and reads to the end: "STATUS closed", or "open" after 3 s.
PROBE='import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(sys.argv[2].encode().decode("unicode_escape").encode())
s.settimeout(0.5); got, end, closed = b"", time.time() + 3, False
while time.time() < end and not closed:
    try:
        d = s.recv(65536); closed = not d; got += d
    except socket.timeout:
        pass
print(got.split(b" ")[1].decode() if got.startswith(b"HTTP/") else "no-answer", "closed" if closed else "open")'
$D rm -f e2e-close >/dev/null 2>&1; $D create --name e2e-close alpine sleep 600 >/dev/null 2>&1
t connection-close-start "204 closed" python3 -c "$PROBE" "$SOCK" 'POST /v1.44/containers/e2e-close/start HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n'
t http10-start-closes "304 closed" python3 -c "$PROBE" "$SOCK" 'POST /v1.44/containers/e2e-close/start HTTP/1.0\r\n\r\n'
t keep-alive-start-stays "304 open" python3 -c "$PROBE" "$SOCK" 'POST /v1.44/containers/e2e-close/start HTTP/1.1\r\nHost: d\r\n\r\n'
t connection-close-said "Connection: close" curl -s -m 5 -o /dev/null -D - --unix-socket "$SOCK" -H 'Connection: close' http://d/_ping
t http10-ping-closes "200 closed" python3 -c "$PROBE" "$SOCK" 'GET /_ping HTTP/1.0\r\n\r\n'
$D rm -f e2e-close >/dev/null 2>&1
# Ryuk mounts the engine's socket and talks to it from inside.
t socket-mount-reaches-kiri "Kiri" $D run --rm -v "$SOCK:/var/run/docker.sock" docker:cli docker version --format '{{.Server.Platform.Name}}'
t info-os "Kiri" $D info --format '{{.OperatingSystem}}'
t info-cgroup "2" $D info --format '{{.CgroupVersion}}'
# Compose's `up` and `logs -f` read the daemon's time before they attach (#346).
t info-system-time "$(date -u +%Y-%m-%dT)" $D info --format '{{.SystemTime}}'

section "images"
# Real pulls, from KIRI_TEST_REGISTRY (the same official images as Docker Hub).
t pull-alpine "alpine:latest" $D pull "$KIRI_REG/alpine:latest"
as_plain alpine:latest
t pull-busybox "busybox" $D pull "$KIRI_REG/busybox"
as_plain busybox:latest
t pull-tag "3.21" $D pull "$KIRI_REG/alpine:3.21"
t images-list "alpine" $D images
# Newest first, as Docker lists them (#364).
t images-newest-first "sorted" bash -c "$D images --format '{{.CreatedAt}}' | sort -rc && echo sorted"
t image-inspect-arch "$($D version -f '{{.Server.Arch}}')" $D image inspect -f '{{.Architecture}}' alpine
t image-inspect-cmd "/bin/sh" $D image inspect -f '{{.Config.Cmd}}' alpine
# RootFS.Layers are the config's uncompressed diff_ids, not the manifest's compressed blob digests.
t image-inspect-rootfs-diffid "same" bash -c "cfg=blobs/sha256/\$($D image inspect -f '{{.Id}}' alpine | sed 's/^sha256://'); want=\$($D save alpine | tar -xOf - \"\$cfg\" | tr -d '\n\r ' | sed -n 's/.*\"diff_ids\":\[\"\(sha256:[0-9a-f]*\)\".*/\1/p'); [ -n \"\$want\" ] && [ \"\$($D image inspect -f '{{index .RootFS.Layers 0}}' alpine)\" = \"\$want\" ] && echo same"
code pull-missing 1 $D pull "$KIRI_REG/kiri-does-not-exist/nope:none"
t rmi "Untagged" $D rmi "$KIRI_REG/alpine:3.21"
code rmi-missing 1 $D rmi "$KIRI_REG/alpine:3.21"
# A client may send the repository slash percent-encoded (Java Testcontainers
# does); both forms must name the same image (issue #223).
$D tag alpine mongodb/mongodb-atlas-local:8.0 >/dev/null
t image-inspect-encoded-slash "200" curl --unix-socket "$SOCK" --path-as-is -sS -o /dev/null -w '%{http_code}' "http://localhost/v1.44/images/mongodb%2Fmongodb-atlas-local:8.0/json"
t image-inspect-plain-slash "200" curl --unix-socket "$SOCK" --path-as-is -sS -o /dev/null -w '%{http_code}' "http://localhost/v1.44/images/mongodb/mongodb-atlas-local:8.0/json"
$D rmi mongodb/mongodb-atlas-local:8.0 >/dev/null 2>&1

section "run: output and exit codes"
code lifecycle-response-status 0 python3 "$(dirname "$0")/lifecycle-status.py" "$SOCK" alpine
t echo "hello" $D run --rm alpine echo hello
t args "a b c" $D run --rm alpine echo a b c
code exit-0 0 $D run --rm alpine true
code exit-1 1 $D run --rm alpine false
code exit-42 42 $D run --rm alpine sh -c 'exit 42'
code exit-127-missing 127 $D run --rm alpine no-such-binary
t missing-msg "executable file not found" $D run --rm alpine no-such-binary
# A command that may not be run is 126, as with Docker (#183).
code exit-126-not-executable 126 $D run --rm alpine /etc/passwd
t not-executable-msg 'exec: "/etc/passwd": permission denied' $D run --rm alpine /etc/passwd
code exit-127-missing-path 127 $D run --rm alpine /no/such/file
t stderr "to-err" $D run --rm alpine sh -c 'echo to-err >&2'
t big-output "100000" bash -c "$D run --rm alpine sh -c 'seq 1 100000' | tail -1"
t output-bytes "5488895" bash -c "$D run --rm alpine sh -c 'seq 1 800000' | wc -c | tr -d ' '"
t killed-by-signal "137" $D run --rm alpine sh -c 'sh -c "kill -9 \$\$"; echo $?'

section "run: config"
t env "bar" $D run --rm -e FOO=bar alpine sh -c 'echo $FOO'
t env-image "PATH=/usr/local/sbin" $D run --rm alpine env
t workdir "/tmp" $D run --rm -w /tmp alpine pwd
t workdir-created "/made/here" $D run --rm -w /made/here alpine pwd
# A non-root user gets the missing directory too, made by root as 0755 (#370).
t workdir-created-nonroot "/made/for/1000" $D run --rm -u 1000:1000 -w /made/for/1000 alpine pwd
t workdir-created-nonroot-mode "755 root" $D run --rm -u 1000:1000 -w /made/x alpine stat -c '%a %U' /made/x
# ... and with a read-only root, made before the root goes read-only, as in Docker.
t workdir-created-readonly "/made/ro" $D run --rm --read-only -w /made/ro alpine pwd
t workdir-created-readonly-nonroot "/made/ro/1000" $D run --rm --read-only -u 1000:1000 -w /made/ro/1000 alpine pwd
# ... and inside the container, through the image's own links: alpine's
# /var/run is a link to /run.
t workdir-created-through-link "at /run/made" $D run --rm -w /var/run/made alpine sh -c 'echo "at $(pwd -P)"'
# The agent's descriptors (the container's cgroup directory among them) must
# not be a way out: a working directory of /proc/self/fd/N must never start
# (CVE-2024-21626 in runc).
t workdir-leaked-fd "started 0" bash -c "s=0; for n in \$(seq 3 25); do $D run --rm -w /proc/self/fd/\$n alpine true >/dev/null 2>&1 && s=\$((s+1)); done; echo started \$s"
t user-numeric "uid=1000" $D run --rm -u 1000 alpine id
t user-name "uid=70(postgres)" $D run --rm -u postgres postgres:17-alpine id
t user-group "gid=0(root)" $D run --rm -u 1000:0 alpine id
# The user's own group is one of its supplementary groups, as Docker sets them.
t user-groups-primary "1000 2000" $D run --rm -u 1000:1000 --group-add 2000 alpine sh -c 'grep Groups /proc/self/status | cut -f2'
t user-groups-exec "65534" bash -c "$D run -d --name groupsexec alpine sleep 60 >/dev/null && $D exec -u nobody groupsexec sh -c 'grep Groups /proc/self/status | cut -f2'; $D rm -f groupsexec >/dev/null"
t entrypoint "x" $D run --rm --entrypoint echo alpine x
t entrypoint-image "nginx version" $D run --rm nginx:alpine nginx -v
# Image labels (#393): shown by inspect and the list, filtered on, inherited
# by a container (its own --label wins) and kept by commit, as on Docker.
t image-labels-inspect "docker-maint@nginx.com" $D image inspect -f '{{index .Config.Labels "maintainer"}}' nginx:alpine
t image-labels-filter "nginx only" bash -c "r=\$($D images --filter label=maintainer --format '{{.Repository}}'); grep -qx nginx <<<\"\$r\" && ! grep -qx alpine <<<\"\$r\" && echo nginx only"
t image-labels-filter-not "alpine only" bash -c "r=\$($D images --filter 'label!=maintainer' --format '{{.Repository}}'); grep -qx alpine <<<\"\$r\" && ! grep -qx nginx <<<\"\$r\" && echo alpine only"
t image-labels-api-empty '"Labels":{}' bash -c "curl -s --unix-socket \"$SOCK\" 'http://d/images/json?filters=%7B%22reference%22%3A%5B%22alpine%22%5D%7D'"
t container-labels-inherited "docker-maint@nginx.com" bash -c "$D create --name lbl-inherit nginx:alpine >/dev/null && $D inspect -f '{{index .Config.Labels \"maintainer\"}}' lbl-inherit; $D rm -f lbl-inherit >/dev/null"
t container-labels-override "me 1" bash -c "$D create --name lbl-own --label maintainer=me --label x=1 nginx:alpine >/dev/null && $D inspect -f '{{index .Config.Labels \"maintainer\"}} {{index .Config.Labels \"x\"}}' lbl-own; $D rm -f lbl-own >/dev/null"
t commit-keeps-labels "docker-maint@nginx.com> 1" bash -c "$D rm -f lbl-commit >/dev/null 2>&1; $D run --name lbl-commit --label x=1 nginx:alpine true >/dev/null && $D commit lbl-commit kiri-test/labels:1 >/dev/null && $D image inspect -f '{{index .Config.Labels \"maintainer\"}} {{index .Config.Labels \"x\"}}' kiri-test/labels:1; $D rm -f lbl-commit >/dev/null; $D rmi kiri-test/labels:1 >/dev/null 2>&1"
# An empty --entrypoint clears the image's (the CLI sends [""]) and runs the
# command alone; with no command there is nothing to run, as on Docker.
t entrypoint-empty-eq "ok" $D run --rm --entrypoint= alpine /bin/sh -c 'echo ok'
t entrypoint-empty-arg "ok" $D run --rm --entrypoint "" alpine /bin/sh -c 'echo ok'
t entrypoint-empty-no-cmd "no command specified" $D run --rm --entrypoint= alpine
t hostname-is-id "" $D run --rm alpine hostname
t hostname-file "" $D run --rm alpine cat /etc/hostname
# Docker takes almost any --hostname (Compose files use underscores); only a
# control character, which would add a line to /etc/hosts, is refused.
t hostname-underscore "db_master" $D run --rm --hostname db_master alpine hostname
t hostname-newline-refused "invalid hostname" $D run --rm --hostname $'db\n10.0.0.9 evil' alpine cat /etc/hosts
t add-host-underscore "10.1.2.3" $D run --rm --add-host my_app.local:10.1.2.3 alpine getent hosts my_app.local
t pid1 "1" $D run --rm alpine sh -c 'echo $$'
t proc "PID" $D run --rm alpine ps
t dev-null "ok" $D run --rm alpine sh -c 'echo x > /dev/null && echo ok'
t dev-null-nonroot "ok" $D run --rm -u 1000 alpine sh -c 'echo x > /dev/null && head -c 1 /dev/urandom >/dev/null && echo ok'
# A non-root user can reopen its own stdio by path, as php-fpm and nginx
# open their logs; the pipes are the user's, as runc makes them.
t stdio-reopen-nonroot "in-ok" bash -c "echo in | $D run -i --rm -u 1000 alpine sh -c 'read x < /proc/self/fd/0 && : > /dev/stdout && : > /dev/stderr && : > /proc/self/fd/2 && echo \$x-ok'"
t stdio-reopen-exec "ok" bash -c "$D run -d --name stdioexec alpine sleep 60 >/dev/null && $D exec -u 1000 stdioexec sh -c 'echo ok > /dev/stderr'; $D rm -f stdioexec >/dev/null"
# Without -i, stdin is /dev/null, which stays root's and 0666.
t stdio-dev-null-kept "0:0:666 0:0:666" $D run --rm -u 1000 alpine sh -c 'stat -L -c %u:%g:%a /proc/self/fd/0 /dev/null | tr "\n" " "'
t dev-modes "crw-rw-rw-" $D run --rm alpine ls -l /dev/null
t dev-urandom "16" $D run --rm alpine sh -c 'head -c 16 /dev/urandom | wc -c'
t caps-dropped "Operation not permitted" $D run --rm alpine sh -c 'hostname newname 2>&1 || true; mount -t tmpfs t /mnt 2>&1'
t caps-chown "ok" $D run --rm alpine sh -c 'touch /f && chown 1:1 /f && echo ok'
t cgroup-view "cgroup2" $D run --rm alpine sh -c 'mount | grep cgroup2'
t name "named-one" $D run --rm --name named-one alpine sh -c 'echo named-one'
# `--pid container:` joins the other container's PID namespace and `--pid
# host` the VM's (a Linux host's own); inspect keeps PidMode (#347).
$D rm -f pid-a pid-b pid-h >/dev/null 2>&1
$D run -d -i --name pid-a busybox:1.37 cat >/dev/null
$D run -d -i --name pid-b --pid container:pid-a busybox:1.37 cat >/dev/null
$D run -d -i --name pid-h --pid host busybox:1.37 cat >/dev/null
t pid-mode-inspect "pid-b=container:$($D inspect -f '{{.Id}}' pid-a) pid-h=host" bash -c "echo pid-b=\$($D inspect -f '{{.HostConfig.PidMode}}' pid-b) pid-h=\$($D inspect -f '{{.HostConfig.PidMode}}' pid-h)"
t pid-container-sees "2" bash -c "$D exec pid-b ps -o comm | grep -cx cat"
t pid-container-pid1 "cat" bash -c "$D exec pid-b cat /proc/1/comm"
t pid-host-sees-more "many" bash -c "n=\$($D exec pid-h ps -o pid | wc -l); [ \$n -gt 5 ] && echo many || echo \$n"
t pid-own-default "1 cat" bash -c "$D exec pid-a sh -c 'echo \$(ps -o pid,comm | grep -w cat)'"
t pid-container-stopped "cannot join PID of a non running container" bash -c "$D stop -t 0 pid-a >/dev/null; $D run --rm --pid container:pid-a busybox:1.37 true"
# A missing container at create is a 400, not a 404 the CLI takes for a
# missing image and pulls for: the output is shown only when no pull was.
t pid-container-unknown "No such container: pid-nosuch" bash -c "o=\$($D run --rm --pid container:pid-nosuch busybox:1.37 true 2>&1); grep -q 'Unable to find image' <<<\"\$o\" || echo \"\$o\""
$D rm -f pid-a pid-b pid-h >/dev/null 2>&1
t python "3.13" $D run --rm python:3.13-alpine python -c 'import sys;print(sys.version)'
t node "v22" $D run --rm node:22-alpine node -v
t debian-image "Valkey server" $D run --rm valkey/valkey valkey-server --version

section "stdin and terminals"
t stdin-pipe "3" bash -c "printf 'a\nb\nc\n' | $D run -i --rm alpine wc -l"
t stdin-cat "round trip" bash -c "echo 'round trip' | $D run -i --rm alpine cat"
t stdin-large "1000000" bash -c "head -c 1000000 /dev/zero | $D run -i --rm alpine wc -c"
needs tty t tty-is-pty "/dev/pts/0" bash -c "$PTY $D run -it --rm alpine tty < /dev/null"
needs tty t tty-term "xterm" bash -c "$PTY $D run -it --rm alpine sh -c 'echo \$TERM' < /dev/null"
# Type after the shell is up: an interactive shell discards input typed before its first prompt.
needs tty t tty-shell "typed-42" bash -c "(sleep 1; printf 'echo typed-\$((6*7))\nexit\n') | $PTY $D run -it --rm alpine sh"
needs tty code tty-exit-code 5 bash -c "(sleep 1; printf 'exit 5\n') | $PTY $D run -it --rm alpine sh"
# The client half-closes right behind its input, and the process must see the end.
# A Mac can lose that on the daemon's socket, 1 to 5 runs in 100, hence the many
# runs. The hang is timed here: an alarm does not stop a docker client. Each run
# must also exit 0 and print its input, or runs that all fail would pass this.
if [ "${SLOW:-0}" = 1 ]; then
  hung=0 wrong=0 out=$(mktemp)
  for _ in $(seq 300); do
    echo in | $D run -i --rm --name e2e-stdin-end alpine cat >"$out" 2>&1 &
    p=$!
    for _ in $(seq 150); do kill -0 $p 2>/dev/null || break; sleep 0.1; done
    if kill -0 $p 2>/dev/null; then
      hung=$((hung + 1))
      kill -9 $p
      wait $p 2>/dev/null
      $D rm -f e2e-stdin-end >/dev/null 2>&1
    elif ! wait $p || [ "$(cat "$out")" != in ]; then
      wrong=$((wrong + 1))
      $D rm -f e2e-stdin-end >/dev/null 2>&1
    fi
  done
  rm -f "$out"
  t stdin-end-every-time "hung 0, wrong 0" echo "hung $hung, wrong $wrong"
fi

section "detached, logs, exec, lifecycle"
$D run -d --name web nginx:alpine >/dev/null
t ps-running "web" $D ps --format '{{.Names}} {{.Status}}'
t ps-up "Up" $D ps --format '{{.Status}}'
t inspect-running "true" $D inspect -f '{{.State.Running}}' web
# nginx prints its start-up lines a moment after run -d returns.
nginx_up web
t logs "Configuration complete" $D logs web
t exec "nginx version" $D exec web nginx -v
code exec-exit 3 $D exec web sh -c 'exit 3'
t exec-env "e-val" $D exec -e E=e-val web sh -c 'echo $E'
t exec-user "nginx" $D exec -u nginx web id -un
t exec-workdir "/etc" $D exec -w /etc web pwd
t exec-workdir-leaked-fd "started 0" bash -c "s=0; for n in \$(seq 3 25); do $D exec -w /proc/self/fd/\$n web true >/dev/null 2>&1 && s=\$((s+1)); done; echo started \$s"
t exec-stdin "into exec" bash -c "echo 'into exec' | $D exec -i web cat"
needs tty t exec-tty "/dev/pts" bash -c "$PTY $D exec -it web tty < /dev/null"
t exec-sees-pid1 "nginx" $D exec web sh -c 'cat /proc/1/comm'
# Each container and each exec leads a session of its own, as with runc, so a
# program that signals its whole process group (Apache prefork's stop does)
# can't reach the engine's own processes (#191).
t session-own "SID=1" $D run --rm alpine sh -c 'echo SID=$(cut -d" " -f6 /proc/1/stat)'
t exec-session-own "own" $D exec web sh -c 's=$(cut -d" " -f6 /proc/$$/stat); [ "$s" = "$$" ] && echo own || echo "session $s, pid $$"'
t exec-same-fs "shared" bash -c "$D exec web sh -c 'echo shared > /tmp/x' && $D exec web cat /tmp/x"
t stop "web" $D stop web
t stopped-status "Exited" $D ps -a --format '{{.Status}}' --filter name=web
code exec-stopped 1 $D exec web true
t start "web" $D start web
t restarted-running "true" $D inspect -f '{{.State.Running}}' web
t restart "web" $D restart web
t kill "web" $D kill web
t killed-code "137" $D inspect -f '{{.State.ExitCode}}' web
code rm-running-refused 1 bash -c "$D start web >/dev/null && $D rm web"
t rm-force "web" $D rm -f web
code gone 1 $D inspect web
t create-then-start "created" bash -c "$D create --name c1 alpine echo hi >/dev/null && $D ps -a --filter name=c1 --format '{{.State}}'"
t start-attach "hi" $D start -a c1
t wait "0" bash -c "$D run -d --name w1 alpine sleep 0.3 >/dev/null; $D wait w1"
t name-conflict "already in use by container" bash -c "$D run -d --name dup alpine sleep 5 >/dev/null; $D run -d --name dup alpine sleep 5"
code inspect-missing 1 $D inspect no-such-container
t prefix-id "true" bash -c "id=\$($D run -d alpine sleep 5); $D inspect -f '{{.State.Running}}' \${id:0:6}"

section "restart policies and persistence"
t on-failure "2" bash -c "$D run -d --name flaky --restart on-failure:2 alpine sh -c 'exit 1' >/dev/null; sleep 2; $D inspect -f '{{.RestartCount}}' flaky"
t always "true" bash -c "$D run -d --name alw --restart always alpine sh -c 'sleep 0.2' >/dev/null; sleep 1.5; $D inspect -f '{{gt .RestartCount 0}}' alw"
t stop-ends-always "Exited" bash -c "$D stop alw >/dev/null; sleep 1; $D ps -a --filter name=alw --format '{{.Status}}'"
code rm-with-restart-refused 125 $D run --rm --restart always alpine true
# A stopped container whose image layer the guest lost (it deletes an
# unpacked layer that isn't whole at boot) starts again with its own data
# (the file it wrote on the first run is still there on the second):
# the layer is sent again, not "layer ... is not unpacked". busybox's single
# layer is the one with bin/busybox and no alpine-release.
# The layers are on the VM's disk (/dev/vda); on a Linux host or in WSL, in
# /var/lib/kiri/layers on the host's, reached through its first process.
if [ "$SYS" = mac ]; then
  t start-restores-layer "KEPT AGAIN" bash -c "$D rm -f relayer >/dev/null 2>&1; $D run --name relayer busybox:1.36 sh -c '[ -e /kept ] && echo AGAIN >> /kept || echo KEPT > /kept' && $D run --rm --privileged alpine sh -c 'mkdir -p /m && mount -t ext4 /dev/vda /m && for d in /m/layers/*/; do [ -e \$d/bin/busybox ] && [ ! -e \$d/etc/alpine-release ] && rm -rf \$d \${d%/}.done; done; umount /m' && $D start -a relayer >/dev/null && $D cp relayer:/kept - | tar -xO | tr '\\n' ' '; $D rm -f relayer >/dev/null"
else
  t start-restores-layer "KEPT AGAIN" bash -c "$D rm -f relayer >/dev/null 2>&1; $D run --name relayer busybox:1.36 sh -c '[ -e /kept ] && echo AGAIN >> /kept || echo KEPT > /kept' && $D run --rm --privileged --pid host alpine nsenter -t 1 -m -- sh -c 'for d in /var/lib/kiri/layers/*/; do [ -e \$d/bin/busybox ] && [ ! -e \$d/etc/alpine-release ] && rm -rf \$d \${d%/}.done; done; true' && $D start -a relayer >/dev/null && $D cp relayer:/kept - | tar -xO | tr '\\n' ' '; $D rm -f relayer >/dev/null"
fi

section "engine Stop and Start"
# Kiri's Stop is the engine's stop, not the user's, as stopping Docker's
# daemon is: Start brings back what `always` and `unless-stopped` kept
# running, but not one the user stopped (docker stop) while Kiri was
# stopped, which answers 204 (#181, #216). A Stop right after Start stops
# the restarts under way too: nothing runs while Kiri is stopped (#181).
SQ=$(printf %q "$SOCK")
POST="curl -s -o /dev/null -w %{http_code} --unix-socket $SQ -X POST"
RUNNING="$D inspect -f '{{.Name}}={{.State.Running}}' ss-us ss-al ss-user ss-no | tr -d / | tr '\n' ' '"
BACK="for i in \$(seq 100); do [ \"\$($D inspect -f '{{.State.Running}}' ss-us ss-al | tr '\n' ' ')\" = 'true true ' ] && break; sleep 0.2; done; $RUNNING"
$D rm -f ss-us ss-al ss-user ss-no ss-al1 ss-al2 ss-al3 ss-al4 >/dev/null 2>&1
$D run -d --name ss-us --restart unless-stopped alpine sleep 1d >/dev/null
$D run -d --name ss-al --restart always alpine sleep 1d >/dev/null
$D run -d --name ss-user --restart unless-stopped alpine sleep 1d >/dev/null
$D run -d --name ss-no alpine sleep 1d >/dev/null
t engine-stop "204" $POST http://d/kiri/stop
t engine-stop-all-down "ss-us=false ss-al=false ss-user=false ss-no=false" bash -c "$RUNNING"
t engine-stop-then-docker-stop "204" $POST http://d/containers/ss-user/stop
t engine-start "204" $POST http://d/kiri/wake
t engine-start-brings-back "ss-us=true ss-al=true ss-user=false ss-no=false" bash -c "$BACK"
for i in 1 2 3 4; do $D run -d --name ss-al$i --restart always alpine sleep 1d >/dev/null; done
t engine-start-then-stop "204 204 204" bash -c "echo \$($POST http://d/kiri/stop) \$($POST http://d/kiri/wake) \$($POST http://d/kiri/stop)"
t engine-start-then-stop-none-run "running: 0" bash -c "sleep 5; echo running: \$($D ps -q --filter name=ss- | wc -l | tr -d ' ')"
t engine-start-then-stop-refused "Kiri is stopped" $D run --rm alpine true
t engine-start-again "204" $POST http://d/kiri/wake
t engine-start-again-brings-back "6" bash -c "for i in \$(seq 100); do n=\$($D ps -q --filter name=ss-al | wc -l | tr -d ' '); [ \$n = 5 ] && [ \"\$($D inspect -f '{{.State.Running}}' ss-us)\" = true ] && break; sleep 0.2; done; echo \$((n + 1))"
$D rm -f ss-us ss-al ss-user ss-no ss-al1 ss-al2 ss-al3 ss-al4 >/dev/null 2>&1

section "networking"
t default-ip "$BRIDGE_NET." $D run --rm alpine sh -c 'ip -4 addr show eth0'
t default-route "default via $BRIDGE_GW" $D run --rm alpine ip route
t lo "127.0.0.1" $D run --rm alpine sh -c 'ip -4 addr show lo'
t internet-http "Example Domain" $D run --rm alpine wget -qO- http://example.com
t internet-dns "Address" $D run --rm alpine nslookup example.com
# The default bridge in the VM names its gateway, not 127.0.0.11: BuildKit
# drops loopback nameservers from build steps and puts 8.8.8.8 in their place.
# On a Linux host it names the host's resolvers (DEFAULT_NS, at the top).
t dns-default-bridge-gateway "nameserver $DEFAULT_NS" $D run --rm alpine cat /etc/resolv.conf
code dns-default-bridge-no-loopback 1 $D run --rm alpine grep -q 127.0.0.11 /etc/resolv.conf
t internet-https "200" $D run --rm curlimages/curl:latest -s -o /dev/null -w '%{http_code}' https://www.google.com
# Each container gets the last one's address and MAC: its first download
# broke mid-TLS or stalled while the old one's veth lingered, and one that
# keeps running lost its gateway whenever the bridge's MAC moved.
APKIDX=https://dl-cdn.alpinelinux.org/alpine/v3.22/main/aarch64/APKINDEX.tar.gz
t first-download-x8 "8" bash -c "n=0; for i in \$(seq 8); do $D run --rm curlimages/curl:latest -s -m 20 -o /dev/null -w '%{exitcode}' $APKIDX | grep -qx 0 && n=\$((n+1)); done; echo \$n"
$D run -d --name dl curlimages/curl:latest sh -c "while true; do curl -s -m 10 -o /dev/null -w '%{exitcode} ' $APKIDX; done" >/dev/null
for _ in $(seq 6); do $D run --rm alpine true; sleep 1; done
code download-while-others-start 1 bash -c "$D logs dl 2>&1 | grep -qE '(^| )[1-9][0-9]* '"
$D rm -f dl >/dev/null
t none-net "lo" $D run --rm --network none alpine ip link
code none-no-internet 1 $D run --rm --network none alpine wget -qO- -T 2 http://example.com
t host-net "$HOSTNET_ADDR" $D run --rm --network host alpine ip -4 addr show ${HOSTNET_IF:+"$HOSTNET_IF"}
t hosts-file "localhost" $D run --rm alpine cat /etc/hosts
t add-host "1.2.3.4" $D run --rm --add-host foo:1.2.3.4 alpine getent hosts foo
t add-host-override "10.9.8.7" $D run --rm --add-host host.docker.internal:10.9.8.7 alpine getent hosts host.docker.internal
$D run -d --name peer nginx:alpine >/dev/null
nginx_up peer
PEER_IP=$($D inspect -f '{{.NetworkSettings.IPAddress}}' peer)
t container-to-container "Welcome to nginx" $D run --rm alpine wget -qO- "http://$PEER_IP"
t default-bridge-no-names "bad address" $D run --rm alpine wget -qO- -T 2 http://peer
t network-create "" $D network create appnet
t network-ls "appnet" $D network ls
# The first free 172.x subnet: 172.18 on an empty engine, later if the user has networks.
t network-subnet "172." $D network inspect appnet -f '{{range .IPAM.Config}}{{.Subnet}}{{end}}'
$D run -d --name db --network appnet --network-alias database redis:7-alpine >/dev/null
$D run -d --name svc --network appnet nginx:alpine >/dev/null
sleep 0.5
t dns-name "PONG" $D run --rm --network appnet redis:7-alpine redis-cli -h db ping
t dns-alias "PONG" $D run --rm --network appnet redis:7-alpine redis-cli -h database ping
t dns-other "Welcome to nginx" $D run --rm --network appnet alpine wget -qO- http://svc
t dns-upstream "Address" $D run --rm --network appnet alpine nslookup example.com
t dns-resolvconf "nameserver 127.0.0.11" $D run --rm --network appnet alpine cat /etc/resolv.conf
# What a kind node's pods are given instead: host.docker.internal's address
# (198.19.248.254 in the VM; on a Linux host the network's gateway, #229).
if [ "$SYS" = mac ]; then
  t dns-via-host-addr "Address" $D run --rm --network appnet alpine nslookup svc 198.19.248.254
else
  t dns-via-host-addr "Address" $D run --rm --network appnet alpine sh -c 'nslookup svc $(getent hosts host.docker.internal | cut -d" " -f1)'
fi
t network-members "db" $D network inspect appnet -f '{{range .Containers}}{{.Name}} {{end}}'
# A start that fails once its network is up leaves no name behind (#325).
$D run -d --name dnsbad --network appnet --sysctl net.ipv4.kiri_no_such_key=1 alpine sleep 60 >/dev/null 2>&1
t dns-failed-start-gone "none" $D run --rm --network appnet alpine sh -c 'nslookup dnsbad 2>&1 | grep -qE "^Address: [0-9.]+$" && echo answered || echo none'
code network-rm-in-use 1 $D network rm appnet
code network-dup 1 $D network create appnet
$D rm -f db svc dnsbad >/dev/null
t network-rm "appnet" $D network rm appnet
code network-rm-builtin 1 $D network rm bridge
# A network removed while an --init container runs frees its DNS address
# for the next network on that subnet (#248).
$D network create initnet >/dev/null
initsub=$($D network inspect initnet -f '{{range .IPAM.Config}}{{.Subnet}}{{end}}')
$D run -d --name initnetc --network initnet alpine sleep 60 >/dev/null
$D run -d --name initkeep --init alpine sleep 60 >/dev/null
$D rm -f initnetc >/dev/null; $D network rm initnet >/dev/null
$D network create initnet2 >/dev/null
t network-rm-init-same-subnet "$initsub" $D network inspect initnet2 -f '{{range .IPAM.Config}}{{.Subnet}}{{end}}'
code network-rm-init-dns 0 $D run --rm --network initnet2 alpine true
$D rm -f initkeep >/dev/null; $D network rm initnet2 >/dev/null
# --init's PID 1 doesn't keep its own stdin open: cat sees the end of it
# (a held one would leave cat to the timeout, exit 143).
code init-stdin-eof 0 bash -c "echo x | $D run -i --rm --init alpine timeout 10 cat"
# --internal: no default route, so anything beyond the network fails at once
# (as with Docker), while the network itself still works.
$D network create --internal intnet >/dev/null
$D run -d --name intweb --network intnet nginx:alpine >/dev/null
nginx_up intweb
t internal-same-net "Welcome to nginx" $D run --rm --network intnet alpine wget -qO- -T 5 http://intweb
t internal-no-route "unreachable" $D run --rm --network intnet alpine sh -c 'ping -c 1 -W 2 1.1.1.1 2>&1; true'
t internal-no-default "none" $D run --rm --network intnet alpine sh -c 'ip route | grep -q default && echo default || echo none'
# An IPv4-only network: IPv6 is off on eth0 (no link-local), lo keeps ::1.
t v4-net-no-inet6 "none" $D run --rm --network intnet alpine sh -c 'ip -6 addr show eth0 | grep -q inet6 && echo some || echo none'
$D rm -f intweb >/dev/null; $D network rm intnet >/dev/null

section "networking options"
# IPAM: a chosen subnet, gateway, range and reserved address.
t ipam-create "" $D network create --subnet 10.231.0.0/24 --gateway 10.231.0.254 --ip-range 10.231.0.128/25 --aux-address router=10.231.0.200 -o com.example.x=1 --label team=net ipamnet
t ipam-inspect "10.231.0.0/24 10.231.0.254 10.231.0.128/25 map[router:10.231.0.200]" $D network inspect ipamnet -f '{{range .IPAM.Config}}{{.Subnet}} {{.Gateway}} {{.IPRange}} {{.AuxiliaryAddresses}}{{end}}'
t ipam-options "map[com.example.x:1]" $D network inspect ipamnet -f '{{.Options}}'
t ipam-from-range "10.231.0.128" $D run --rm --network ipamnet alpine ip -4 addr show eth0
t ipam-route "default via 10.231.0.254" $D run --rm --network ipamnet alpine ip route
t ipam-overlap "Pool overlaps with other one on this address space" $D network create --subnet 10.231.0.0/16 ipamnet2
t ipam-bad-gateway "no matching subnet for gateway" $D network create --subnet 10.232.0.0/24 --gateway 10.9.0.1 ipamnet3
# A bridge name of one's own is refused, not reported and left unapplied:
# Kiri's firewall knows its bridges by name (#423). Empty is Docker's unset.
t bridge-name-refused "does not support the network option com.docker.network.bridge.name" $D network create -o com.docker.network.bridge.name=cp-isolated --subnet 10.234.0.0/24 bridgename
code bridge-name-nothing-made 1 $D network inspect bridgename
t bridge-name-empty "map[]" bash -c "$D network create -o com.docker.network.bridge.name= bridgename >/dev/null && $D network inspect -f '{{.Options}}' bridgename; $D network rm bridgename >/dev/null"
# Fixed addresses (--ip, Compose ipv4_address) and MAC addresses.
$D run -d --name fixed --network ipamnet --ip 10.231.0.10 --mac-address 02:42:0a:e7:00:0a nginx:alpine >/dev/null
t static-ip "10.231.0.10" $D inspect -f '{{.NetworkSettings.Networks.ipamnet.IPAddress}}' fixed
t static-ipamconfig "10.231.0.10" $D inspect -f '{{.NetworkSettings.Networks.ipamnet.IPAMConfig.IPv4Address}}' fixed
t static-prefix "24" $D inspect -f '{{.NetworkSettings.Networks.ipamnet.IPPrefixLen}}' fixed
t static-mac "02:42:0a:e7:00:0a" $D exec fixed cat /sys/class/net/eth0/address
t static-in-use "Address already in use" $D run --rm --network ipamnet --ip 10.231.0.10 alpine true
t static-outside "no configured subnet" $D run --rm --network ipamnet --ip 10.9.0.10 alpine true
t static-default-bridge "user defined networks only" $D run --rm --ip 172.17.0.99 alpine true
$D network create --subnet 10.233.0.0/24 ipamnet4 >/dev/null
t static-connect "10.233.0.77" bash -c "$D network connect --ip 10.233.0.77 ipamnet4 fixed && $D exec fixed ip -4 addr show eth1"
# --network container: (Compose network_mode: service:x).
t netns-shared-ip "10.231.0.10" $D run --rm --network container:fixed alpine ip -4 addr show eth0
t netns-localhost "Welcome to nginx" $D run --rm --network container:fixed alpine wget -qO- http://127.0.0.1
FIXED_ID=$($D inspect -f '{{.Id}}' fixed)
FIXED_HOST=$($D inspect -f '{{.Config.Hostname}}' fixed)
t netns-hostname "$FIXED_HOST" $D run --rm --network container:fixed alpine hostname
t netns-hosts "10.231.0.10" $D run --rm --network container:fixed alpine cat /etc/hosts
t netns-mode "container:$FIXED_ID" bash -c "$D create --name side --network container:fixed alpine sleep 60 >/dev/null; $D inspect -f '{{.HostConfig.NetworkMode}}' side"
t netns-settings "map[]" $D inspect -f '{{.NetworkSettings.Networks}}' side
t netns-ports "conflicting options: port publishing and the container type network mode" $D run --rm -p 18190:80 --network container:fixed alpine true
t netns-hostname-refused "conflicting options: hostname and the network mode" $D run --rm -h x --network container:fixed alpine true
t netns-missing "joining network namespace of container: No such container: nope" bash -c "o=\$($D run --rm --network container:nope alpine true 2>&1); grep -q 'Unable to find image' <<<\"\$o\" || echo \"\$o\""
$D stop fixed >/dev/null
t netns-not-running "cannot join network of a non running container" $D start side
$D rm -f side >/dev/null
$D start fixed >/dev/null
# DNS settings: the default bridge gets the servers; user networks keep the
# embedded DNS at 127.0.0.11, which forwards to them.
t dns-default-bridge "nameserver 9.9.9.9" $D run --rm --dns 9.9.9.9 --dns-search corp.example --dns-option ndots:2 alpine cat /etc/resolv.conf
t dns-search "search corp.example" $D run --rm --dns-search corp.example alpine cat /etc/resolv.conf
t dns-options "options ndots:2" $D run --rm --dns-option ndots:2 alpine cat /etc/resolv.conf
t dns-user-net "nameserver 127.0.0.11" $D run --rm --network ipamnet --dns 9.9.9.9 alpine cat /etc/resolv.conf
t dns-user-net-forwards "Address" $D run --rm --network ipamnet --dns 9.9.9.9 alpine nslookup example.com
t dns-user-net-names "10.231.0.10" $D run --rm --network ipamnet --dns 9.9.9.9 alpine nslookup fixed
t dns-inspect "[9.9.9.9]" bash -c "$D create --name dnsc --dns 9.9.9.9 alpine true >/dev/null; $D inspect -f '{{.HostConfig.Dns}}' dnsc; $D rm dnsc >/dev/null"
t domainname "web.example.com" $D run --rm -h web --domainname example.com alpine sh -c 'hostname -f 2>/dev/null || cat /etc/hosts'
t domainname-uts "example.com" $D run --rm --domainname example.com alpine cat /proc/sys/kernel/domainname
# --link on the default bridge, which has no DNS.
$D run -d --name linked nginx:alpine >/dev/null
nginx_up linked
t link-alias "Welcome to nginx" $D run --rm --link linked:webby alpine wget -qO- http://webby
t link-name "Welcome to nginx" $D run --rm --link linked alpine wget -qO- http://linked
t link-hosts "webby" $D run --rm --link linked:webby alpine cat /etc/hosts
t link-inspect "/linked:/" bash -c "$D create --name linker --link linked:webby alpine true >/dev/null; $D inspect -f '{{.HostConfig.Links}}' linker; $D rm linker >/dev/null"
t link-missing "could not get container for nope: No such container: nope" bash -c "o=\$($D run --rm --link nope:x alpine true 2>&1); grep -q 'Unable to find image' <<<\"\$o\" || echo \"\$o\""
$D rm -f fixed linked >/dev/null
$D network rm ipamnet ipamnet4 >/dev/null

section "IPv6 networks"
# --ipv6 without a subnet: a /64 from the engine's ULA pool, beside IPv4.
t ipv6-create "" $D network create --ipv6 v6net
t ipv6-enabled "true" $D network inspect v6net -f '{{.EnableIPv6}}'
t ipv6-pool "/64" $D network inspect v6net -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}'
t ipv6-pool-ula " fd" $D network inspect v6net -f '{{range .IPAM.Config}} {{.Subnet}}{{end}}'
# A chosen subnet and gateway, as kind and Compose's ipam ask.
t ipv6-subnet "fd00:db8:7::/64 fd00:db8:7::fe" bash -c "$D network create --ipv6 --subnet fd00:db8:7::/64 --gateway fd00:db8:7::fe v6sub >/dev/null && $D network inspect v6sub -f '{{range .IPAM.Config}}{{.Subnet}} {{.Gateway}} {{end}}'"
t ipv6-overlap "Pool overlaps" $D network create --ipv6 --subnet fd00:db8:7::/80 v6dup
# IPv6-only (--ipv4=false), as Docker 28 has: no IPv4 address on eth0.
t ipv6-only-create "false" bash -c "$D network create --ipv6 --ipv4=false v6only >/dev/null && $D network inspect v6only -f '{{.EnableIPv4}}'"
t ipv6-only-no-v4 "inet6 0" $D run --rm --network v6only alpine sh -c 'echo $(ip -6 addr show eth0 scope global | grep -o inet6 | head -1) $(ip -4 addr show eth0 | grep -c inet)'
$D network rm v6only >/dev/null
$D run -d --name v6a --network v6sub --network-alias v6web nginx:alpine >/dev/null
sleep 0.5
V6A=$($D inspect -f '{{.NetworkSettings.Networks.v6sub.GlobalIPv6Address}}' v6a)
t ipv6-address "fd00:db8:7::1" echo "$V6A"
t ipv6-prefix "64" $D inspect -f '{{.NetworkSettings.Networks.v6sub.GlobalIPv6PrefixLen}}' v6a
t ipv6-gateway "fd00:db8:7::fe" $D inspect -f '{{.NetworkSettings.Networks.v6sub.IPv6Gateway}}' v6a
t ipv6-ipv4-too "." $D inspect -f '{{.NetworkSettings.Networks.v6sub.IPAddress}}' v6a
t ipv6-members "$V6A/64" $D network inspect v6sub -f '{{range .Containers}}{{.IPv6Address}}{{end}}'
t ipv6-eth0 "inet6 fd00:db8:7::" $D exec v6a ip -6 addr show eth0
t ipv6-route "default via fd00:db8:7::fe" $D run --rm --network v6sub alpine ip -6 route
t ipv6-hosts "fd00:db8:7::" $D run --rm --network v6sub alpine cat /etc/hosts
t ipv6-ping "1 packets received" $D run --rm --network v6sub alpine ping -6 -c 1 -W 2 v6a
t ipv6-wget "Welcome to nginx" $D run --rm --network v6sub alpine wget -qO- -T 5 "http://[$V6A]"
t ipv6-aaaa "$V6A" $D run --rm --network v6sub alpine nslookup -type=AAAA v6web
t ipv6-a-still "Address" $D run --rm --network v6sub alpine nslookup -type=A v6a
# Fixed addresses (--ip6, Compose ipv6_address).
t ipv6-static "fd00:db8:7::99" bash -c "$D run -d --name v6fixed --network v6sub --ip6 fd00:db8:7::99 alpine sleep 60 >/dev/null; $D inspect -f '{{.NetworkSettings.Networks.v6sub.GlobalIPv6Address}} {{.NetworkSettings.Networks.v6sub.IPAMConfig.IPv6Address}}' v6fixed"
t ipv6-static-in-use "Address already in use" $D run --rm --network v6sub --ip6 fd00:db8:7::99 alpine true
t ipv6-static-outside "no configured subnet" $D run --rm --network v6sub --ip6 fd00:9::9 alpine true
# On an automatically allocated subnet too, as with Docker 29 (moby#51569).
t ipv6-static-pool "ok" bash -c "p=\$($D network inspect v6net -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' | grep -o '[0-9a-f:]*::/64' | sed 's|::/64||'); $D run --rm --network v6net --ip6 \${p}::9 alpine ip -6 addr show eth0 | grep -q \${p}::9/64 && echo ok"
t ipv6-static-v4-net "does not have IPv6 enabled" bash -c "$D network create v4only >/dev/null; $D run --rm --network v4only --ip6 fd00::9 alpine true; $D network rm v4only >/dev/null"
t ipv6-connect "inet6 fd00:db8:7::" bash -c "$D run -d --name v6late alpine sleep 60 >/dev/null && $D network connect v6sub v6late && $D exec v6late ip -6 addr show eth1"
# Egress: reaches IPv6 sites when the host has IPv6, fails fast otherwise.
code ipv6-egress-no-hang 0 bash -c "$D run --rm --network v6sub alpine sh -c 'wget -qO- -T 5 http://ipv6.google.com >/dev/null 2>&1; true'"
# The default bridge stays IPv4-only.
t ipv6-default-bridge "0" bash -c "$D run --rm alpine ip -6 addr show scope global | grep -c inet6"
$D rm -f v6a v6fixed v6late >/dev/null
t ipv6-rm "v6sub" $D network rm v6sub
$D network rm v6net >/dev/null

section "published ports"
$D run -d --name pub -p 18180:80 nginx:alpine >/dev/null; ready http://localhost:18180
t port-curl "Welcome to nginx" curl -s -m 5 http://localhost:18180
t port-ps "0.0.0.0:18180->80/tcp" $D ps --filter name=pub --format '{{.Ports}}'
t port-cmd "18180" $D port pub
t port-inspect '{"80/tcp":[{"HostIp":"","HostPort":"18180"}]} {"80/tcp":{}}' $D inspect -f '{{json .HostConfig.PortBindings}} {{json .Config.ExposedPorts}}' pub
t distribution '"Platforms":[{"architecture":' curl -s --unix-socket "$SOCK" "http://d/v1.47/distribution/$KIRI_REG/alpine:3.21/json"
t distribution-missing "404" curl -s -o /dev/null -w '%{http_code}' --unix-socket "$SOCK" "http://d/v1.47/distribution/$KIRI_REG/no-such-image-kiri-test/json"
t port-many "50" bash -c "for i in \$(seq 50); do curl -s -m 5 -o /dev/null -w '%{http_code}\n' http://localhost:18180; done | grep -c 200"
t port-parallel "20" bash -c "for i in \$(seq 20); do curl -s -m 10 -o /dev/null -w '%{http_code}\n' http://localhost:18180 & done; wait"
t port-big "10485760" bash -c "$D exec pub sh -c 'head -c 10485760 /dev/urandom > /usr/share/nginx/html/big.bin' && curl -s -m 30 -o /dev/null -w '%{size_download}' http://localhost:18180/big.bin"
t port-clash "port is already allocated" $D run -d --name pub2 -p 18180:80 nginx:alpine
t port-publish-all "80/tcp ->" bash -c "$D run -d --name puball -P nginx:alpine >/dev/null; $D port puball"
t port-ipv6-listed "[::]:18180" $D port pub 80
t port-ipv6-curl "Welcome to nginx" curl -sg -m 5 'http://[::1]:18180'
t port-ipv6-only "[::1]:18183" bash -c "$D run -d --name pub6 -p '[::1]:18183:80' nginx:alpine >/dev/null; $D port pub6"
t port-ipv6-only-curl "Welcome to nginx" bash -c "for i in \$(seq 50); do curl -sg -m 1 'http://[::1]:18183' && break; sleep 0.2; done"
code port-ipv6-only-not-v4 7 curl -s -m 2 http://127.0.0.1:18183
# An exposed port that is not published is a null key, as Docker shows it;
# Traefik's docker provider picks the port from these keys (#492).
t port-exposed-null '{"80/tcp":null,"9000/tcp":null}' bash -c "$D run -d --name pubx --expose 9000 nginx:alpine >/dev/null; $D inspect -f '{{json .NetworkSettings.Ports}}' pubx"
t port-exposed-stopped '{}' bash -c "$D stop -t 1 pubx >/dev/null; $D inspect -f '{{json .NetworkSettings.Ports}}' pubx"
$D rm -f pub pub2 puball pub6 pubx >/dev/null
code port-closed-after-stop 7 curl -s -m 2 http://localhost:18180

section "host.docker.internal"
# In the VM, host.docker.internal is relayed to the Mac's 127.0.0.1. On a
# Linux host it is the bridge's gateway, as Docker's host-gateway: it reaches
# the host's services on 0.0.0.0 and, by design, not those on 127.0.0.1
# alone (kiri-init container.rs write_etc_files, net.rs host_uplink).
if [ "$SYS" = mac ]; then HTTP_BIND=127.0.0.1; else HTTP_BIND=0.0.0.0; fi
HTTP_DIR=$(mktemp -d)
python3 -m http.server 18999 --bind $HTTP_BIND --directory "$HTTP_DIR" >/dev/null 2>&1 &
HTTP_PID=$!
[ "$SYS" = mac ] || { python3 -m http.server 18998 --bind 127.0.0.1 --directory "$HTTP_DIR" >/dev/null 2>&1 & HTTP_LO_PID=$!; }
sleep 0.7
t host-internal "Directory listing" $D run --rm alpine wget -qO- http://host.docker.internal:18999/
t host-gateway "Directory listing" $D run --rm --add-host mac:host-gateway alpine wget -qO- http://mac:18999/
t host-internal-hostnet "Directory listing" $D run --rm --network host alpine wget -qO- http://host.docker.internal:18999/
only linux,wsl t host-internal-not-loopback "Connection refused" $D run --rm alpine wget -qO- -T 3 http://host.docker.internal:18998/
kill "$HTTP_PID" ${HTTP_LO_PID:+"$HTTP_LO_PID"}; rm -rf "$HTTP_DIR"

section "container domains"
if [ -z "$FEATURE" ]; then
  $D run -d --name dom nginx:alpine >/dev/null; ready http://dom.kiri.localhost
  t domain-http "Welcome to nginx" curl -s -m 5 http://dom.kiri.localhost
  t domain-unknown "No running container" curl -s -m 5 http://nope.kiri.localhost
  # A container that publishes the router's port gets it (Laravel Sail's
  # 80:80); the router takes it back once the container is gone.
  t domain-port-yield "Welcome to nginx" bash -c "$D run -d --name p80 -p 80:80 nginx:alpine >/dev/null && sleep 1 && curl -s -m 5 http://127.0.0.1/"
  $D rm -f p80 >/dev/null
  t domain-port-back "Welcome to nginx" bash -c "for i in \$(seq 20); do curl -s -m 2 http://dom.kiri.localhost | grep -q nginx && break; sleep 0.5; done; curl -s -m 5 http://dom.kiri.localhost"
  $D rm -f dom >/dev/null
else
  skip domain-http "only the installed Kiri serves *.kiri.localhost"
  skip domain-unknown "only the installed Kiri serves *.kiri.localhost"
  skip domain-port-yield "only the installed Kiri has the router on port 80"
  skip domain-port-back "only the installed Kiri has the router on port 80"
fi

# Settings → Domains moves the routers with no restart; a taken port
# changes nothing. Kiri Dev has no routers until given ports, so there the
# move starts them, and its settings file goes afterwards.
section "domain router ports"
dport() { curl -s --unix-socket "$SOCK" http://d/kiri/domains | python3 -c "import json,sys; print(json.load(sys.stdin)['$1']['port'])"; }
OLD_HTTP=$(dport http) OLD_HTTPS=$(dport https)
$D run -d --name dmv nginx:alpine >/dev/null
$D run -d --name dhold -p 18799:80 nginx:alpine >/dev/null
t domain-ports-same "different ports" curl -s --unix-socket "$SOCK" -X POST -d '{"http":18780,"https":18780}' http://d/kiri/domains
t domain-ports-zero "1 to 65535" curl -s --unix-socket "$SOCK" -X POST -d '{"http":0,"https":18743}' http://d/kiri/domains
t domain-ports-taken "18799 is in use" curl -s --unix-socket "$SOCK" -X POST -d '{"http":18799,"https":18743}' http://d/kiri/domains
t domain-ports-unchanged "$OLD_HTTP" bash -c "curl -s --unix-socket '$SOCK' http://d/kiri/domains | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"http\"][\"port\"])'"
t domain-ports-move '"port":18780' curl -s --unix-socket "$SOCK" -X POST -d '{"http":18780,"https":18743}' http://d/kiri/domains
t domain-ports-http "Welcome to nginx" bash -c "for i in \$(seq 20); do curl -s -m 2 http://dmv.kiri.localhost:18780 | grep -q nginx && break; sleep 0.5; done; curl -s -m 5 http://dmv.kiri.localhost:18780"
t domain-ports-https "Welcome to nginx" curl -sk -m 5 https://dmv.kiri.localhost:18743
# The "no such container" page lists the running containers only when asked
# by one of this machine's own names: a page reached through DNS rebinding
# asks with its own domain as the Host and could otherwise read the list.
t domain-list-local "dmv.kiri.local" curl -s -m 5 -H 'Host: nope.kiri.localhost' http://127.0.0.1:18780
t domain-list-foreign-host "listed 0" bash -c "curl -s -m 5 -H 'Host: rebind.example.com' http://127.0.0.1:18780 | grep -c dmv | sed 's/^/listed /'"
# The old port is free afterwards: only the installed Kiri's (Kiri Dev's
# old port 80 is the installed Kiri's own router).
if [ -z "$FEATURE" ]; then
  t domain-ports-old-free "closed" bash -c "curl -s -m 2 -o /dev/null http://dmv.kiri.localhost:$OLD_HTTP && echo open || echo closed"
else
  skip domain-ports-old-free "port $OLD_HTTP is the installed Kiri's"
fi
# A container can still take the moved port, and gives it back.
t domain-ports-yield "Welcome to nginx" bash -c "$D run -d --name d18780 -p 18780:80 nginx:alpine >/dev/null && sleep 1 && curl -s -m 5 http://127.0.0.1:18780/"
$D rm -f d18780 >/dev/null
t domain-ports-back "Welcome to nginx" bash -c "for i in \$(seq 20); do curl -s -m 2 http://dmv.kiri.localhost:18780 | grep -q nginx && break; sleep 0.5; done; curl -s -m 5 http://dmv.kiri.localhost:18780"
if [ -z "$FEATURE" ]; then
  # t execs its command, so this is curl itself: a shell function there would never run.
  t domain-ports-restore "\"port\":$OLD_HTTP" curl -s --unix-socket "$SOCK" -X POST -H 'Content-Type: application/json' -d "{\"http\":$OLD_HTTP,\"https\":$OLD_HTTPS}" http://d/kiri/domains
  t domain-ports-restored "Welcome to nginx" bash -c "for i in \$(seq 20); do curl -s -m 2 http://dmv.kiri.localhost:$OLD_HTTP | grep -q nginx && break; sleep 0.5; done; curl -s -m 5 http://dmv.kiri.localhost:$OLD_HTTP"
else
  rm -f "$HOME/.kiri-$FEATURE/domains.json"
fi
$D rm -f dmv dhold >/dev/null

section "container files in Finder ($KF)"
# The Mac's own: the engine serves them over NFS and mounts them (daemon/mod.rs, macOS only).
only mac
$D run -d --name fsv nginx:alpine >/dev/null; sleep 0.7
needs finder t finder-lists "fsv" ls "$KF"
needs finder t finder-root "docker-entrypoint.sh" ls "$KF/fsv"
needs finder t finder-read "Alpine Linux" cat "$KF/fsv/etc/os-release"
needs finder t finder-hash "match" bash -c "$D exec fsv sh -c 'head -c 3000000 /dev/urandom > /tmp/r.bin'; a=\$($D exec fsv sha256sum /tmp/r.bin | cut -c1-64); b=\$($SHA $KFQ/fsv/tmp/r.bin | cut -c1-64); [ \"\$a\" = \"\$b\" ] && echo match"
needs finder t finder-symlink-inside "docker-entrypoint.sh" bash -c "$D exec fsv ln -s / /loop && ls $KFQ/fsv/loop/"
needs finder code finder-readonly 1 bash -c "echo x > $KFQ/fsv/tmp/written"
t finder-mounted "nfs" bash -c "mount | grep -F \" on $KF \""
# A file rewritten in place (same path, same size) must never read stale.
needs finder t finder-fresh "match match match" bash -c "for i in 1 2 3; do $D exec fsv sh -c 'head -c 100000 /dev/urandom > /tmp/same.bin'; a=\$($D exec fsv sha256sum /tmp/same.bin | cut -c1-64); b=\$($SHA $KFQ/fsv/tmp/same.bin | cut -c1-64); [ \"\$a\" = \"\$b\" ] && printf 'match '; done"
needs finder t finder-new-file "late" bash -c "$D exec fsv sh -c 'echo late > /tmp/late.txt'; cat $KFQ/fsv/tmp/late.txt"
needs finder t finder-deleted "No such file" bash -c "$D exec fsv rm /tmp/late.txt; cat $KFQ/fsv/tmp/late.txt"
needs finder t finder-big "match" bash -c "$D exec fsv sh -c 'head -c 50000000 /dev/urandom > /tmp/big.bin'; a=\$($D exec fsv sha256sum /tmp/big.bin | cut -c1-64); b=\$($SHA $KFQ/fsv/tmp/big.bin | cut -c1-64); [ \"\$a\" = \"\$b\" ] && echo match"
needs finder t finder-perms-readable "ok" bash -c "$D exec fsv sh -c 'echo s > /tmp/secret; chmod 600 /tmp/secret'; [ \"\$(cat $KFQ/fsv/tmp/secret)\" = s ] && echo ok"
needs finder code finder-no-mkdir 1 mkdir "$KF/fsv/tmp/newdir"
$D rm -f fsv >/dev/null
# A volume and a bind mount list their own files, not the empty folders
# under them in the container's layer (#317).
FSB=$(mktemp -d); echo from-mac > "$FSB/mac.txt"
$D run -d --name fsm -v kiri-e2e-fsm:/data -v "$FSB":/bind:ro alpine sh -c 'echo hello > /data/hello.txt; sleep 300' >/dev/null; sleep 0.7
needs finder t finder-volume "hello.txt" ls "$KF/fsm/data"
needs finder t finder-bind "from-mac" cat "$KF/fsm/bind/mac.txt"
$D rm -f fsm >/dev/null; $D volume rm kiri-e2e-fsm >/dev/null; rm -rf "$FSB"
needs finder t finder-gone "gone" bash -c "ls $KFQ | grep -qx fsv || echo gone"

section "volumes and bind mounts"
BIND=$(mktemp -d); echo "from the mac" > "$BIND/hello.txt"
t bind-read "from the mac" $D run --rm -v "$BIND:/data" alpine cat /data/hello.txt
t bind-write "from the container" bash -c "$D run --rm -v $BIND:/data alpine sh -c 'echo from the container > /data/out.txt' && cat $BIND/out.txt"
# A new file without owner read (Node's fs.cpSync creates with 0200) is made
# and ends with that mode (guest/kernel/patches/0003).
t bind-create-0200 "200 hi" bash -c "$D run --rm -v $BIND:/data alpine sh -c 'umask 577; echo hi > /data/w0200' && perl -e 'printf q(%o ), (stat shift)[2] & 07777' $BIND/w0200 && chmod u+r $BIND/w0200 && cat $BIND/w0200"
t bind-ro "Read-only file system" $D run --rm -v "$BIND:/data:ro" alpine sh -c 'echo x > /data/nope'
t bind-mount-flag "from the mac" $D run --rm --mount "type=bind,source=$BIND,target=/m" alpine cat /m/hello.txt
code bind-mount-missing 125 $D run --rm --mount type=bind,source=/kiri/does/not/exist,target=/m alpine true
t bind-v-creates "made" bash -c "$D run --rm -v $BIND/new/dir:/n alpine true && [ -d $BIND/new/dir ] && echo made"
t bind-file "from the mac" $D run --rm -v "$BIND/hello.txt:/etc/greeting" alpine cat /etc/greeting
# A file made, changed and deleted on the Mac raises inotify events in the
# container, as one made there does: file watchers (Vite, webpack, nodemon)
# pick up saves made in an editor on the Mac. inotifyd prints n (created),
# c (modified), w (closed after writing) and d (deleted), each once here.
t bind-inotify "n f c f w f d f d old" bash -c "mkdir -p $BIND/watched && echo o > $BIND/watched/old && $D rm -f watcher >/dev/null 2>&1; $D run -d --name watcher -v $BIND/watched:/w alpine inotifyd - /w:ncwd >/dev/null && sleep 2 && echo a > $BIND/watched/f && sleep 1 && echo b >> $BIND/watched/f && sleep 1 && rm $BIND/watched/f && sleep 1 && rm $BIND/watched/old && sleep 1.5; $D logs watcher 2>&1 | awk '{print \$1, \$3}' | awk '!(\$0 in s) {s[\$0]; printf \"%s \", \$0}'; $D rm -f watcher >/dev/null; rm -rf $BIND/watched"
# A linked source is followed at every start, as Docker does: re-pointing a
# `current` release link and restarting mounts the new target.
t bind-link-repointed "one two one" bash -c "mkdir -p $BIND/rel/r1 $BIND/rel/r2 && echo one > $BIND/rel/r1/v && echo two > $BIND/rel/r2/v && ln -sfn r1 $BIND/rel/current && $D rm -f relink >/dev/null 2>&1; $D run -d --name relink -v $BIND/rel/current:/app alpine sleep 300 >/dev/null && a=\$($D exec relink cat /app/v) && ln -sfn r2 $BIND/rel/current && sleep 1.5 && $D restart -t 0 relink >/dev/null && b=\$($D exec relink cat /app/v) && $D stop -t 0 relink >/dev/null && ln -sfn r1 $BIND/rel/current && sleep 1.5 && $D start relink >/dev/null && c=\$($D exec relink cat /app/v); $D rm -f relink >/dev/null; rm -rf $BIND/rel; echo \$a \$b \$c"
t vol-persist "kept" bash -c "$D run --rm -v tvol:/v alpine sh -c 'echo kept > /v/f' && $D run --rm -v tvol:/v alpine cat /v/f"
t vol-ls "tvol" $D volume ls
t vol-inspect "/var/lib/kiri/volumes/tvol/_data" $D volume inspect -f '{{.Mountpoint}}' tvol
t vol-copyup "index.html" $D run --rm -v thtml:/usr/share/nginx/html nginx:alpine ls /usr/share/nginx/html
t vol-nocopy-empty "0" $D run --rm --mount type=volume,source=tempty,target=/usr/share/nginx/html,volume-nocopy nginx:alpine sh -c 'ls /usr/share/nginx/html | wc -l'
# A new volume takes the image directory's owner, mode and files, as Docker
# does; redis runs as user redis and must be able to write /data.
t vol-copyup-owner "same:redis:redis 755" bash -c "i=\$($D run --rm redis:7-alpine stat -c '%U:%G %a' /data); v=\$($D run --rm -v towner:/data redis:7-alpine stat -c '%U:%G %a' /data); [ \"\$i\" = \"\$v\" ] && echo same:\$v || echo image \$i volume \$v"
t vol-copyup-tree "same" bash -c "i=\$($D run --rm alpine sh -c 'cd /etc/apk && { find . -exec stat -c \"%n %A %u %g\" {} +; find . -type f -exec md5sum {} +; } | sort | md5sum'); v=\$($D run --rm -v tapk:/etc/apk alpine sh -c 'cd /etc/apk && { find . -exec stat -c \"%n %A %u %g\" {} +; find . -type f -exec md5sum {} +; } | sort | md5sum'); [ \"\$i\" = \"\$v\" ] && echo same"
t vol-copyup-not-over "1 mine" bash -c "$D run --rm -v tkeep:/usr/share/nginx/html nginx:alpine sh -c 'rm /usr/share/nginx/html/*; echo mine > /usr/share/nginx/html/mine' && $D run --rm -v tkeep:/usr/share/nginx/html nginx:alpine sh -c 'echo \$(ls /usr/share/nginx/html | wc -l) \$(cat /usr/share/nginx/html/mine)'"
t redis-volume "PONG OK running" bash -c "$D run -d --name rv -v tredis:/data redis:7-alpine >/dev/null; for i in \$(seq 50); do $D exec rv redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 0.2; done; p=\$($D exec rv redis-cli ping); s=\$($D exec rv redis-cli save); sleep 1; echo \$p \$s \$($D inspect -f '{{.State.Status}}' rv)"
# A fresh named volume is filled from the image while the container starts.
# `docker run -d` must not return before the container's root is in place, or
# an immediate exec resolves users against the host's /etc/passwd (#311).
t volume-user-ready "passed passed passed" bash -c "$D run --name ready-seed alpine sh -c 'adduser -D dev && mkdir /payload && dd if=/dev/zero of=/payload/blob bs=1M count=64' >/dev/null && $D commit -c 'USER dev' ready-seed kiri-test/ready:1 >/dev/null && $D rm -f ready-seed >/dev/null; for i in 1 2 3; do v=ready-vol-\$i; c=\$($D run -d --mount type=volume,source=\$v,target=/payload kiri-test/ready:1 sleep 60); if $D exec --user dev \$c id >/dev/null 2>&1; then printf 'passed '; else printf 'failed '; fi; $D rm -f \$c >/dev/null; $D volume rm \$v >/dev/null 2>&1; done; $D rmi kiri-test/ready:1 >/dev/null; echo"
# A start that fills a new volume from a big image folder still waits for the
# fill (#311), but the engine keeps serving every other container meanwhile;
# an exec sent to it meanwhile waits for it, a pause, a commit and a
# container joining its network sent meanwhile all finish, and a start that
# fails during the fill is still a start error with a whole volume
# (tests/start-stall.sh).
t start-stall-exec "pass:" sh "$(dirname "$0")/start-stall.sh"
# A volume over part of a bind (`./:/app` and `/app/node_modules`) is filled
# from the image, as Docker does, never from the host's folder under it.
t vol-copyup-under-bind "index.html" bash -c "h=\$(mktemp -d); mkdir \$h/html; echo mac > \$h/html/from-host.txt; $D run --rm -v \$h:/usr/share/nginx -v /usr/share/nginx/html nginx:alpine sh -c 'cd /usr/share/nginx/html; [ -e from-host.txt ] && echo from the host || ls'; rm -rf \$h"
$D rm -f rv >/dev/null 2>&1; $D volume rm towner tapk tkeep tredis >/dev/null 2>&1
t vol-in-use "in use" bash -c "$D run -d --name vu -v tvol:/v alpine sleep 30 >/dev/null; $D volume rm tvol"
$D rm -f vu >/dev/null
t vol-rm "tvol" $D volume rm tvol thtml tempty
t vol-create "made-vol" $D volume create made-vol
t tmpfs "size=10240k" $D run --rm --tmpfs /scratch:size=10m alpine sh -c 'mount | grep scratch'
t tmpfs-mount "tmpfs on /t" $D run --rm --mount type=tmpfs,target=/t alpine sh -c 'mount | grep " /t "'
# `--volumes-from` lends another container's volumes and binds at the same
# paths, from a running or a stopped one, read-only with `:ro` (#450).
$D rm -f vfsrc vfdst >/dev/null 2>&1
$D run -d --name vfsrc -v tvfrom:/vol -v /anon -v "$BIND:/bind" alpine sh -c 'echo n > /vol/f; echo a > /anon/f; sleep 300' >/dev/null; sleep 1
VFANON=$($D inspect -f '{{range .Mounts}}{{if eq .Destination "/anon"}}{{.Name}}{{end}}{{end}}' vfsrc 2>/dev/null)
t volumes-from-running "n a from the mac" bash -c "$D run --rm --volumes-from vfsrc alpine sh -c 'echo \$(cat /vol/f /anon/f /bind/hello.txt)'"
t volumes-from-ro "Read-only file system" $D run --rm --volumes-from vfsrc:ro alpine sh -c 'echo x > /vol/f'
t volumes-from-inspect "/anon /bind /vol [vfsrc]" bash -c "$D create --name vfdst --volumes-from vfsrc alpine true >/dev/null && $D inspect -f '{{range .Mounts}}{{.Destination}} {{end}}{{.HostConfig.VolumesFrom}}' vfdst | tr ' ' '\n' | LC_ALL=C sort | tr '\n' ' ' | sed 's/^ *//'"
$D stop -t 0 vfsrc >/dev/null
t volumes-from-stopped "got:n a" bash -c "$D run --rm --volumes-from vfsrc alpine sh -c 'echo got:\$(cat /vol/f /anon/f)'"
t volumes-from-unknown "No such container: vfnosuch" bash -c "o=\$($D run --rm --volumes-from vfnosuch alpine true 2>&1); grep -q 'Unable to find image' <<<\"\$o\" || echo \"\$o\""
# The anonymous volume stays while another container still uses it.
t volumes-from-anon-kept "got:a" bash -c "$D rm -v vfsrc >/dev/null; $D run --rm --volumes-from vfdst alpine sh -c 'echo got:\$(cat /anon/f)'"
$D rm -f -v vfsrc vfdst >/dev/null 2>&1; $D volume rm tvfrom $VFANON >/dev/null 2>&1
t anon-from-image "volume:/var/lib/postgresql/data" bash -c "$D run -d --name pgv -e POSTGRES_PASSWORD=x postgres:17-alpine >/dev/null; $D inspect -f '{{range .Mounts}}{{.Type}}:{{.Destination}}{{end}}' pgv"
# Waits for the entrypoint's "init process complete" and then a working query,
# not for pg_isready: on first start Postgres runs a temporary server to
# initialise, which also answers, and then restarts.
t postgres-works "42" bash -c "for i in \$(seq 60); do $D logs pgv 2>&1 | grep -q 'init process complete' && $D exec pgv psql -U postgres -tAc 'select 1' >/dev/null 2>&1 && break; sleep 0.5; done; $D exec pgv psql -U postgres -tAc 'select 42'"
t rm-v-anon "gone" bash -c "n=\$($D volume ls -q | wc -l); $D rm -f -v pgv >/dev/null; m=\$($D volume ls -q | wc -l); [ \$m -lt \$n ] && echo gone"
t rm-anon-default-kept "kept" bash -c "$D run -d --name pgk -e POSTGRES_PASSWORD=x postgres:17-alpine >/dev/null; $D inspect -f '{{range .Mounts}}{{.Name}}{{end}}' pgk > /tmp/kiri-e2e-pgk-vol; n=\$($D volume ls -q | wc -l); $D rm -f pgk >/dev/null; m=\$($D volume ls -q | wc -l); [ \$m -eq \$n ] && echo kept"
$D volume rm "$(cat /tmp/kiri-e2e-pgk-vol 2>/dev/null)" >/dev/null 2>&1; rm -f /tmp/kiri-e2e-pgk-vol
# Prune only what this suite labels: never the user's own volumes.
t vol-prune "prunev" bash -c "$D volume create --label kiri-e2e=1 prunev >/dev/null; $D volume prune -af --filter label=kiri-e2e=1"
$D volume rm made-vol >/dev/null 2>&1
rm -rf "$BIND"

section "filters"
$D run -d --name fa --label team=a --label tier=web alpine sleep 60 >/dev/null
$D run -d --name fb --label team=b alpine sleep 60 >/dev/null
$D run --name fx alpine sh -c 'exit 3' >/dev/null 2>&1
# Another tag of alpine: `ancestor=alpine` is alpine:latest only (#442).
$D run -d --name fo alpine:3.22 sleep 60 >/dev/null
t filter-label "fa" $D ps --filter label=team=a --format '{{.Names}}'
t filter-label-none "0" bash -c "$D ps -q --filter label=team=zzz | wc -l | tr -d ' '"
t filter-label-and "1" bash -c "$D ps -q --filter label=team=a --filter label=tier=web | wc -l | tr -d ' '"
t filter-label-key "2" bash -c "$D ps -q --filter label=team | wc -l | tr -d ' '"
t filter-name "fb" $D ps --filter name=fb --format '{{.Names}}'
# The name filter is a regular expression, as in Docker (#385): k3d finds its
# nodes with `^[/]?<node>$` together with label filters.
t filter-name-regex-k3d "fb" $D ps --filter 'name=^[/]?fb$' --filter label=team=b --format '{{.Names}}'
t filter-name-regex-optional-slash "fa" $D ps --filter 'name=^/?fa$' --format '{{.Names}}'
t filter-name-regex-class "2" bash -c "$D ps -q --filter 'name=^f[ab]$' | wc -l | tr -d ' '"
t filter-name-regex-anchored "0" bash -c "$D ps -q --filter 'name=^[/]?f$' | wc -l | tr -d ' '"
t filter-status-exited "fx" $D ps --filter status=exited --format '{{.Names}}'
t filter-exited-code "fx" $D ps -a --filter exited=3 --format '{{.Names}}'
t filter-ancestor "2" bash -c "$D ps -q --filter ancestor=alpine | wc -l | tr -d ' '"
t filter-ancestor-tag "fo" $D ps --filter ancestor=alpine:3.22 --format '{{.Names}}'
t filter-ancestor-id "fa" bash -c "$D ps --filter ancestor=\$($D image inspect -f '{{.Id}}' alpine | cut -c8-19) --filter name=fa --format '{{.Names}}'"
t filter-ancestor-no-glob "0" bash -c "$D ps -q --filter 'ancestor=alp*' | wc -l | tr -d ' '"
t filter-network "2" bash -c "$D ps -q --filter network=bridge --filter label=team | wc -l | tr -d ' '"
t filter-network-other "0" bash -c "$D ps -q --filter network=none --filter label=team | wc -l | tr -d ' '"
t filter-id "fa" bash -c "$D ps --filter id=\$($D inspect -f '{{.Id}}' fa | cut -c1-12) --format '{{.Names}}'"
code filter-invalid 1 $D ps --filter bogus=1
t filter-images-ref "alpine" $D images --filter reference=alpine --format '{{.Repository}}'
t filter-images-ref-miss "0" bash -c "$D images -q --filter reference=nosuchimage | wc -l | tr -d ' '"
t filter-images-dangling "0" bash -c "$D images -q --filter dangling=true | wc -l | tr -d ' '"
$D network create --label team=a fnet >/dev/null
t filter-network-label "fnet" $D network ls --filter label=team=a --format '{{.Name}}'
t filter-network-type "fnet" $D network ls --filter type=custom --format '{{.Name}}'
t filter-network-builtin "bridge" $D network ls --filter type=builtin --format '{{.Name}}'
# A network's id filter is a regular expression, as in Docker: k3d finds its
# cluster network with `^/?<id>$` together with its name.
t filter-network-id-regex "fnet" bash -c "id=\$($D network inspect -f '{{.Id}}' fnet); $D network ls --filter \"id=^/?\$id\$\" --filter 'name=^/?fnet$' --format '{{.Name}}'"
t filter-network-id-short "fnet" bash -c "$D network ls --filter id=\$($D network inspect -f '{{.Id}}' fnet | cut -c1-12) --format '{{.Name}}'"
$D network rm fnet >/dev/null
$D volume create --label team=a fvol >/dev/null
$D volume create fvol2 >/dev/null
t filter-volume-label "fvol" $D volume ls --filter label=team=a --format '{{.Name}}'
t filter-volume-label-only "1" bash -c "$D volume ls -q --filter label=team=a | wc -l | tr -d ' '"
t filter-volume-dangling "fvol2" $D volume ls --filter dangling=true --format '{{.Name}}'
t filter-volume-prune-all "fvol" $D volume prune -af --filter label=team=a
t filter-volume-prune-label-only "fvol2" $D volume ls --format '{{.Name}}'
$D volume rm fvol2 >/dev/null
$D rm -f fa fb fx fo >/dev/null

section "compose"
# A container outside the project must not confuse Compose.
$D run -d --name bystander nginx:alpine >/dev/null
CP=$(mktemp -d)
cat > "$CP/compose.yaml" <<'YAML'
services:
  web:
    image: nginx:alpine
    ports: ["18380:80"]
    volumes: ["./html:/usr/share/nginx/html:ro"]
    depends_on: [cache]
  cache:
    image: redis:7-alpine
    volumes: ["cachedata:/data"]
volumes:
  cachedata: {}
YAML
mkdir -p "$CP/html" && echo "compose on kiri" > "$CP/html/index.html"
t compose-up "Started" bash -c "cd $CP && $D compose -p kiritest up -d 2>&1"
ready http://localhost:18380
t compose-port "compose on kiri" curl -s -m 5 http://localhost:18380
t compose-dns "PONG" bash -c "cd $CP && $D compose -p kiritest exec -T web sh -c 'apk add -q redis >/dev/null 2>&1; redis-cli -h cache ping || { cat /etc/resolv.conf; nslookup cache; ip addr show eth0; }'"
t compose-ps "running" bash -c "cd $CP && $D compose -p kiritest ps --format '{{.State}}'"
if [ -z "$FEATURE" ]; then t compose-domain "compose on kiri" curl -s -m 5 http://web.kiritest.kiri.localhost; else skip compose-domain "only the installed Kiri serves *.kiri.localhost"; fi
t compose-down "Removed" bash -c "cd $CP && $D compose -p kiritest down -v 2>&1"
t compose-clean "0" bash -c "$D ps -aq --filter label=com.docker.compose.project=kiritest | wc -l | tr -d ' '"
t compose-bystander "bystander" $D ps --format '{{.Names}}'
$D rm -f bystander >/dev/null
rm -rf "$CP"

section "healthchecks and a Compose dev stack (#358)"
# Kiri runs healthchecks itself; Compose's depends_on conditions, --wait and
# local bind volumes are built on them.
$D run -d --name hc-ok --health-cmd true --health-interval 1s alpine sleep 120 >/dev/null
$D run -d --name hc-bad --health-cmd 'exit 1' --health-interval 1s --health-retries 3 alpine sleep 120 >/dev/null
$D run -d --name hc-slow --health-cmd 'sleep 5' --health-interval 1s --health-timeout 1s alpine sleep 120 >/dev/null
$D run -d --name hc-start --health-cmd true --health-interval 30s --health-start-period 20s --health-start-interval 1s alpine sleep 120 >/dev/null
sleep 5
t health-healthy "healthy" $D inspect -f '{{.State.Health.Status}}' hc-ok
t health-unhealthy "unhealthy" $D inspect -f '{{.State.Health.Status}}' hc-bad
t health-streak "ok" bash -c "[ \$($D inspect -f '{{.State.Health.FailingStreak}}' hc-bad) -ge 3 ] && echo ok"
t health-timeout "exceeded timeout (1s)" $D inspect -f '{{range .State.Health.Log}}{{.Output}}{{end}}' hc-slow
t health-start-interval "healthy" $D inspect -f '{{.State.Health.Status}}' hc-start
t health-filter-unhealthy "hc-bad" $D ps --filter health=unhealthy --format '{{.Names}}'
t health-filter-healthy "hc-ok" $D ps --filter health=healthy --format '{{.Names}}'
$D rm -f hc-ok hc-bad hc-slow hc-start >/dev/null
HC=$(mktemp -d); mkdir -p "$HC/app" "$HC/shared"
# FROM a registry-qualified alpine: BuildKit asks the registry for a FROM
# image's metadata even when the engine has it, and plain alpine is Docker Hub.
printf 'FROM %s\nRUN echo built > /built\n' "$KIRI_REG/alpine:latest" > "$HC/app/Dockerfile"
cat > "$HC/compose.yaml" <<YAML
services:
  db:
    image: alpine
    command: [sh, -c, "sleep 2; touch /ready; date +%s > /tmp/t; sleep 300"]
    healthcheck: {test: [CMD, test, -f, /ready], interval: 1s, retries: 30}
  app:
    build: ./app
    command: [sh, -c, "date +%s > /shared/app-started; echo from-container > /shared/c.txt; sleep 300"]
    depends_on: {db: {condition: service_healthy}}
    volumes: [shared:/shared]
volumes:
  shared:
    driver_opts: {type: none, o: bind, device: "$HC/shared"}
YAML
echo from-host > "$HC/shared/h.txt"
# The build goes to the suite's builder (BX, set up at the top), whatever
# the context's current builder is: buildx would otherwise make its own
# BuildKit container and pull moby/buildkit by its Docker Hub name.
t compose-wait "0" bash -c "cd $HC && BUILDX_BUILDER=$BX $D compose -p kirihc up -d --build --wait >/dev/null 2>&1; echo \$?"
t compose-db-healthy "healthy" bash -c "cd $HC && $D compose -p kirihc ps db --format '{{.Health}}'"
t compose-built "built" bash -c "cd $HC && $D compose -p kirihc exec -T app cat /built"
t compose-host-to-ctr "from-host" bash -c "cd $HC && $D compose -p kirihc exec -T app cat /shared/h.txt"
t compose-ctr-to-host "from-container" cat "$HC/shared/c.txt"
t compose-down-keeps "2" bash -c "cd $HC && $D compose -p kirihc down -v >/dev/null 2>&1; ls $HC/shared | grep -c txt"
rm -rf "$HC"
# A locked-down run, as hardened Compose services use.
LOCK="$D run --rm --read-only --cap-drop ALL --security-opt no-new-privileges --init --network none --user 1000:1000 --tmpfs /x:uid=1000,gid=1000,mode=0700 alpine sh -c"
t locked-tmpfs "1000 700" $LOCK "touch /x/f && stat -c '%u %a' /x"
t locked-ro-root "Read-only" $LOCK "touch /y 2>&1"
t locked-caps "CapEff:	0000000000000000" $LOCK "grep CapEff /proc/self/status"
t locked-nnp "NoNewPrivs:	1" $LOCK "grep NoNewPrivs /proc/self/status"
t locked-not-pid1 "ok" $LOCK 'test $$ -ne 1 && echo ok'
# Only loopback may have an address, and nothing may lead out. (Some kernels, WSL 2's
# among them, put unused tunnel devices such as tunl0 in every new network namespace.)
t locked-only-lo "addresses=0 routes=0" $LOCK "echo addresses=\$(ip -o addr | grep -vc ' lo ') routes=\$(ip route | wc -l)"
# A tmpfs volume's root keeps its uid=, gid= and mode= when the image has
# nothing there to copy up (#296), on the first start and on a later one.
$D volume create -d local -o type=tmpfs -o device=tmpfs -o o=size=1m,uid=10001,gid=10050,mode=2750 tv-owner >/dev/null
t tmpfs-volume-owner "10001:10050 2750" $D run --rm -v tv-owner:/t alpine stat -c '%u:%g %a' /t
$D create --name tv-again -v tv-owner:/t alpine stat -c '%u:%g %a' /t >/dev/null
$D start -a tv-again >/dev/null 2>&1
t tmpfs-volume-owner-restart "10001:10050 2750" $D start -a tv-again
$D rm -f tv-again >/dev/null; $D volume rm tv-owner >/dev/null
# ...and a volume over an image folder still gets that folder's owner.
t volume-copy-up-owner "70:70" $D run --rm -v tv-pg:/var/lib/postgresql/data postgres:17-alpine stat -c '%u:%g' /var/lib/postgresql/data
$D volume rm tv-pg >/dev/null 2>&1

section "docker cp"
CPD=$(mktemp -d); mkdir -p "$CPD/dir/sub"; echo one > "$CPD/a.txt"; echo two > "$CPD/dir/sub/b.txt"
$D run -d --name cpc alpine sleep 120 >/dev/null
t cp-in-file "one" bash -c "$D cp $CPD/a.txt cpc:/tmp/ && $D exec cpc cat /tmp/a.txt"
t cp-in-dir "two" bash -c "$D cp $CPD/dir cpc:/tmp/ && $D exec cpc cat /tmp/dir/sub/b.txt"
t cp-in-rename "one" bash -c "$D cp $CPD/a.txt cpc:/tmp/renamed.txt && $D exec cpc cat /tmp/renamed.txt"
t cp-in-root-owner "0 0" bash -c "$D exec cpc stat -c '%u %g' /tmp/a.txt"
t cp-out-file "fromctr" bash -c "$D exec cpc sh -c 'echo fromctr > /etc/motd2' && $D cp cpc:/etc/motd2 $CPD/ && cat $CPD/motd2"
t cp-out-dir "deep" bash -c "$D exec cpc sh -c 'mkdir -p /opt/x/y; echo deep > /opt/x/y/z' && $D cp cpc:/opt/x $CPD/xx && cat $CPD/xx/y/z"
t cp-out-stdout "alpine-release" bash -c "$D cp cpc:/etc/alpine-release - | tar -t"
t cp-out-symlink "-> " bash -c "$D exec cpc ln -sf /etc/alpine-release /tmp/rel && $D cp cpc:/tmp/rel - | tar -tv"
t cp-big "match" bash -c "head -c 30000000 /dev/urandom > $CPD/big && $D cp $CPD/big cpc:/tmp/big && a=\$($SHA $CPD/big | cut -c1-64); b=\$($D exec cpc sha256sum /tmp/big | cut -c1-64); [ \$a = \$b ] && echo match"
code cp-missing 1 $D cp cpc:/nope "$CPD/"
code cp-into-missing-dir 1 $D cp "$CPD/a.txt" cpc:/nodir/
$D create --name cpcreated alpine cat /tmp/a.txt >/dev/null
t cp-into-created "one" bash -c "$D cp $CPD/a.txt cpcreated:/tmp/ && $D start -a cpcreated"
t cp-from-stopped "a.txt" bash -c "$D cp cpcreated:/tmp/a.txt - | tar -t"
$D rm -f cpc cpcreated >/dev/null; rm -rf "$CPD"

section "privileged"
t priv-caps "000001ffffffffff" $D run --rm --privileged alpine grep CapEff /proc/self/status
t priv-mount "mounted" $D run --rm --privileged alpine sh -c 'mount -t tmpfs t /mnt && echo mounted'
t priv-devices "fuse" $D run --rm --privileged alpine ls /dev
t priv-cgroup-rw "ok" $D run --rm --privileged alpine sh -c 'mkdir /sys/fs/cgroup/sub && echo ok'
t unpriv-no-mount "denied" $D run --rm alpine sh -c 'mount -t tmpfs t /mnt 2>/dev/null || echo denied'
t priv-inspect "true" bash -c "$D run -d --name pv --privileged alpine sleep 30 >/dev/null; $D inspect -f '{{.HostConfig.Privileged}}' pv"
t priv-exec-caps "000001ffffffffff" $D exec pv grep CapEff /proc/self/status
$D rm -f pv >/dev/null

section "kernel: netfilter, tc and interfaces"
# Every line of tests/kernel-features.sh (nftables expressions, the iptables
# matches and targets, ipset, IPVS, tc, ss, macvlan/WireGuard/IPsec...) is
# one check: Kiri's guest kernel builds them all in, as it loads no modules.
KFEAT=$(perl -e 'alarm 180; exec @ARGV' $D run --rm --privileged -v "$(cd "$(dirname "$0")" && pwd)/kernel-features.sh:/k.sh:ro" alpine:3.22 sh /k.sh 2>&1 | tr -d '\r')
# One check per line; the line itself is what shows when it fails.
# On a Linux host or in WSL the kernel is the host's, and a feature its
# kernel lacks (not built, module not loaded: what a container can't load)
# is the host's, not Kiri's; Docker there would fail it the same way.
kernel_lacks() { printf '%s' "$1" | grep -qE 'FAIL: .*(Unknown device type|Not supported|not supported|Protocol not available|Could not process rule: No such file or directory|Can.t initialize ipvs|RTNETLINK answers: Operation not supported)'; }
for name in $(printf '%s\n' "$KFEAT" | grep -E '^[a-z0-9-]+ (ok|FAIL)' | cut -d' ' -f1); do
  line=$(printf '%s\n' "$KFEAT" | grep -E "^$name (ok|FAIL)" | head -1)
  if [ "$SYS" != mac ] && kernel_lacks "$line"; then
    skip_setup "kernel-$name" "the host's kernel lacks it"
    printf '    %s\n' "$(printf %s "$line" | cut -c1-160)"
    continue
  fi
  t "kernel-$name" "$name ok" bash -c 'printf "%s\n" "$1" | grep -E "^$2 (ok|FAIL)"' _ "$KFEAT" "$name"
done
printf '%s\n' "$KFEAT" | grep -qE '^[a-z0-9-]+ (ok|FAIL)' || t kernel-features "ok" printf '%s\n' "$KFEAT"

section "images: tag, save, load"
IMD=$(mktemp -d)
t tag "kiri-test/alp:v1" bash -c "$D tag alpine kiri-test/alp:v1 && $D images --format '{{.Repository}}:{{.Tag}}'"
t tag-same-id "same" bash -c "[ \"\$($D images -q alpine:latest)\" = \"\$($D images -q kiri-test/alp:v1)\" ] && echo same"
t save "manifest.json" bash -c "$D save -o $IMD/a.tar kiri-test/alp:v1 && tar -tf $IMD/a.tar"
t rmi-tag-only "Untagged" $D rmi kiri-test/alp:v1
t alpine-still-there "alpine" $D images --format '{{.Repository}}'
t load "Loaded image: kiri-test/alp:v1" $D load -i "$IMD/a.tar"
t load-runs "3." $D run --rm kiri-test/alp:v1 cat /etc/alpine-release
t load-stdin "Loaded image" bash -c "$D rmi kiri-test/alp:v1 >/dev/null; $D load < $IMD/a.tar"
t load-gzip "Loaded image: kiri-test/alp:v1" bash -c "$D rmi kiri-test/alp:v1 >/dev/null; gzip -c $IMD/a.tar | $D load"
# zstd too, as Docker takes it (skipped where the zstd tool is not installed).
if command -v zstd >/dev/null; then
  t load-zstd "Loaded image: kiri-test/alp:v1" bash -c "$D rmi kiri-test/alp:v1 >/dev/null; zstd -q -c $IMD/a.tar | $D load"
fi
code load-garbage 1 bash -c "echo nope | $D load"
# A hard link's target is read from the layer's root, so a leading / is the
# layer's, as with Docker (issue 302); `..` still can't leave the layer.
python3 "$(dirname "$0")/hardlink-image.py" "$IMD/hl.tar" kiri-test/hl:abs /bin/tool
python3 "$(dirname "$0")/hardlink-image.py" "$IMD/hl-esc.tar" kiri-test/hl:esc /../../etc/hostname
t load-hardlink-absolute "hello from tool" bash -c "$D load -q -i $IMD/hl.tar >/dev/null && id=\$($D create kiri-test/hl:abs) && $D cp \$id:/bin/tool-1.0 - | tar -xO; $D rm \$id >/dev/null"
t load-hardlink-escape "escapes the layer" bash -c "$D load -q -i $IMD/hl-esc.tar >/dev/null && $D create kiri-test/hl:esc"
$D rmi kiri-test/alp:v1 kiri-test/hl:abs kiri-test/hl:esc >/dev/null 2>&1; rm -rf "$IMD"

section "docker build (BuildKit in the VM)"
BD=$(mktemp -d)
# BX, the builder, is set up at the top (beside the images).
# FROM lines name alpine at KIRI_TEST_REGISTRY: BuildKit asks the registry for
# a FROM image's metadata even when the engine has it.
FA=$KIRI_REG/alpine:latest
printf 'FROM %s\nRUN echo built > /built\nARG MSG=default\nRUN echo "$MSG" > /msg\nCOPY data.txt /data.txt\nCMD ["cat","/built","/msg","/data.txt"]\n' "$FA" > "$BD/Dockerfile"
echo "from context" > "$BD/data.txt"
# Without buildx the CLI's classic builder still makes the image the next
# two checks run, as before; only BuildKit's own output is skipped.
[ -n "$NO_BUILDX" ] && (cd "$BD" && perl -e 'alarm 300; exec @ARGV' $D build --build-arg MSG=hello -t kiri-test/built . >/dev/null 2>&1)
needs buildx t build "importing to docker" bash -c "cd $BD && BUILDX_BUILDER=$BX perl -e 'alarm 300; exec @ARGV' $D build --build-arg MSG=hello -t kiri-test/built . 2>&1"
t build-runs "from context" $D run --rm kiri-test/built
t build-arg "hello" $D run --rm kiri-test/built
needs buildx t build-cached "CACHED" bash -c "cd $BD && BUILDX_BUILDER=$BX $D build --progress plain --build-arg MSG=hello -t kiri-test/built . 2>&1"
t build-network "curl" bash -c "cd $BD && printf 'FROM $FA\nRUN apk add --no-cache curl\n' > Dockerfile.net && BUILDX_BUILDER=$BX perl -e 'alarm 300; exec @ARGV' $D build -f Dockerfile.net -t kiri-test/net . >/dev/null 2>&1 && $D run --rm kiri-test/net curl --version"
# Build steps keep Kiri's resolver (and with it the Mac's): no 8.8.8.8. On a
# Linux host, the default bridge's, the host's resolvers (DEFAULT_NS).
needs buildx t build-resolvconf "nameserver $DEFAULT_NS" bash -c "cd $BD && printf 'FROM $FA\nRUN cat /etc/resolv.conf\n' > Dockerfile.dns && BUILDX_BUILDER=$BX perl -e 'alarm 300; exec @ARGV' $D build --no-cache --progress plain -f Dockerfile.dns . 2>&1"
needs buildx code build-no-public-resolver 1 bash -c "cd $BD && BUILDX_BUILDER=$BX perl -e 'alarm 300; exec @ARGV' $D build --no-cache --progress plain -f Dockerfile.dns . 2>&1 | grep -q 'nameserver 8.8.8.8'"
needs buildx t build-dns "Address" bash -c "cd $BD && printf 'FROM $FA\nRUN nslookup example.com\n' > Dockerfile.lookup && BUILDX_BUILDER=$BX perl -e 'alarm 300; exec @ARGV' $D build --no-cache --progress plain -f Dockerfile.lookup . 2>&1"
t build-fails "exit code: 3" bash -c "cd $BD && printf 'FROM $FA\nRUN exit 3\n' > Dockerfile.bad && BUILDX_BUILDER=$BX $D build -f Dockerfile.bad . 2>&1"
$D rmi kiri-test/built kiri-test/net >/dev/null 2>&1; rm -rf "$BD"

section "registry: login, push, pull"
REG=$(mktemp -d); export DOCKER_CONFIG_SAVED=${DOCKER_CONFIG:-}
# A throwaway client config: no keychain, nothing of the user's touched.
mkdir -p "$REG/cfg"; echo '{"auths":{}}' > "$REG/cfg/config.json"; ln -s ~/.docker/cli-plugins "$REG/cfg/cli-plugins"; ln -s ~/.docker/contexts "$REG/cfg/contexts"
$D run --rm httpd:2.4-alpine htpasswd -Bbn kiriuser kiripass > "$REG/htpasswd" 2>/dev/null
$D run -d --name reg -p 5055:5000 -v "$REG:/auth" -e REGISTRY_AUTH=htpasswd -e REGISTRY_AUTH_HTPASSWD_REALM=kiri -e REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd registry:2 >/dev/null
ready http://localhost:5055/v2/
R="env DOCKER_CONFIG=$REG/cfg $D"
t login-wrong "incorrect username or password" bash -c "echo nope | $R login localhost:5055 -u kiriuser --password-stdin 2>&1"
t login "Login Succeeded" bash -c "echo kiripass | $R login localhost:5055 -u kiriuser --password-stdin 2>&1"
t push "digest: sha256:" bash -c "$R tag alpine localhost:5055/team/alp:1 && $R push localhost:5055/team/alp:1 2>&1"
t push-again "already exists" bash -c "$R push localhost:5055/team/alp:1 2>&1"
# Deploy tools pin to the RepoDigest after a push: it must be the manifest the
# registry has, not the image ID or the index alpine was pulled from.
t push-repo-digest "same" bash -c "d=\$($R push localhost:5055/team/alp:1 2>&1 | sed -n 's/.*digest: \(sha256:[0-9a-f]*\).*/\1/p'); [ -n \"\$d\" ] && [ \"\$($R image inspect localhost:5055/team/alp:1 --format '{{index .RepoDigests 0}}')\" = localhost:5055/team/alp@\$d ] && echo same"
t pull-auth "3." bash -c "$R rmi localhost:5055/team/alp:1 >/dev/null && $R pull -q localhost:5055/team/alp:1 >/dev/null && $R run --rm localhost:5055/team/alp:1 cat /etc/alpine-release"
t pull-no-auth "pull access denied" bash -c "$R logout localhost:5055 >/dev/null; $R rmi localhost:5055/team/alp:1 >/dev/null; $R pull localhost:5055/team/alp:1 2>&1"
t push-no-auth "authentication required" bash -c "$R tag alpine localhost:5055/team/alp:2 && $R push localhost:5055/team/alp:2 2>&1"
# Not pushed, so no RepoDigest: not alpine's from Docker Hub, nor one made up.
t push-failed-no-digest "[]" $R image inspect localhost:5055/team/alp:2 --format '{{json .RepoDigests}}'
$D rmi localhost:5055/team/alp:2 >/dev/null 2>&1; $D rm -f reg >/dev/null; rm -rf "$REG"

section "limits, stats, top, pause, update"
$D run -d --name lim --memory 128m --cpus 0.5 --pids-limit 50 --cpu-shares 512 nginx:alpine >/dev/null; sleep 0.5
t limit-memory "134217728" $D exec lim cat /sys/fs/cgroup/memory.max
t limit-cpus "50000 100000" $D exec lim cat /sys/fs/cgroup/cpu.max
t limit-pids "50" $D exec lim cat /sys/fs/cgroup/pids.max
t limit-shares "59" $D exec lim cat /sys/fs/cgroup/cpu.weight
# 1024 shares, Docker's default, is the kernel's default weight (not less
# than a container that set nothing), and inspect gives the shares back.
t limit-shares-default "100 100" bash -c "echo \$($D run --rm --cpu-shares 1024 alpine cat /sys/fs/cgroup/cpu.weight) \$($D run --rm alpine cat /sys/fs/cgroup/cpu.weight)"
t limit-shares-inspect "512" $D inspect -f '{{.HostConfig.CpuShares}}' lim
t limit-inspect "134217728 500000000 50" $D inspect -f '{{.HostConfig.Memory}} {{.HostConfig.NanoCpus}} {{.HostConfig.PidsLimit}}' lim
t oom-kill "137" bash -c "$D run --rm --memory 32m alpine sh -c 'tail /dev/zero' >/dev/null 2>&1; echo \$?"
t stats "128MiB" $D stats --no-stream --format '{{.MemUsage}}' lim
t stats-pids "PIDS" $D stats --no-stream lim
t stats-stream "lim" bash -c "perl -e 'alarm 3; exec @ARGV' $D stats --format '{{.Name}}' lim 2>/dev/null | head -1"
t top "nginx: master process" $D top lim
t top-ps-titles "PPID" $D top lim
# ps_args picks ps's columns, as Docker runs ps with them (#348).
t top-aux "%MEM" bash -c "$D top lim aux | head -1"
t top-aux-master "nginx: master process" $D top lim aux
t top-eo '["PID","RSS","COMMAND"]' curl -s --unix-socket "$SOCK" 'http://d/v1.47/containers/lim/top?ps_args=-eo+pid%2Crss%2Cargs'
t top-eo-rss "ok" bash -c "$D top lim -eo pid,rss,args | awk 'NR>1 && \$2 ~ /^[0-9]+\$/ && \$2 > 0 {print \"ok\"; exit}'"
t top-ps-args-refused "not supported" $D top lim -l
t pause "paused" bash -c "$D pause lim >/dev/null && $D inspect -f '{{.State.Status}}' lim"
t pause-ps "(Paused)" $D ps --filter name=lim --format '{{.Status}}'
t pause-exec-refused "is paused" $D exec lim true
t pause-filter "lim" $D ps --filter status=paused --format '{{.Names}}'
t unpause "running" bash -c "$D unpause lim >/dev/null && $D inspect -f '{{.State.Status}}' lim"
t unpause-works "Welcome" $D exec lim wget -qO- http://127.0.0.1
t update-memory "268435456" bash -c "$D update --memory 256m lim >/dev/null && $D exec lim cat /sys/fs/cgroup/memory.max"
t update-cpus "200000 100000" bash -c "$D update --cpus 2 lim >/dev/null && $D exec lim cat /sys/fs/cgroup/cpu.max"
t update-kept "268435456" bash -c "$D restart -t 1 lim >/dev/null && $D exec lim cat /sys/fs/cgroup/memory.max"
t update-restart-policy "always" bash -c "$D update --restart always lim >/dev/null && $D inspect -f '{{.HostConfig.RestartPolicy.Name}}' lim"
t stop-paused "exited" bash -c "$D pause lim >/dev/null; $D stop -t 2 lim >/dev/null && $D inspect -f '{{.State.Status}}' lim"
$D rm -f lim >/dev/null

section "rename, diff, export, commit, history"
$D run -d --name ren alpine sleep 120 >/dev/null
t rename "ren2" bash -c "$D rename ren ren2 && $D ps --format '{{.Names}}' --filter name=ren2"
code rename-taken 1 bash -c "$D run -d --name ren3 alpine sleep 60 >/dev/null; $D rename ren2 ren3"
$D rm -f ren3 >/dev/null
t diff-added "A /added" bash -c "$D exec ren2 sh -c 'echo hi > /added; rm /etc/motd; echo c >> /etc/profile' && $D diff ren2"
t diff-deleted "D /etc/motd" $D diff ren2
t diff-changed "C /etc/profile" $D diff ren2
t export "bin/busybox" bash -c "$D export ren2 | tar -t | grep -x bin/busybox"
t export-has-change "added" bash -c "$D export ren2 | tar -t | grep -x added"
# docker import (#461): the export back as a one-layer image, plain or gzip.
t import "sha256:" bash -c "$D export ren2 | $D import -c 'CMD [\"cat\",\"/added\"]' -c 'ENV KIRI=1' -m 'from ren2' - kiri-test/imported:1"
t import-runs "hi" $D run --rm kiri-test/imported:1
t import-env "KIRI=1" $D run --rm kiri-test/imported:1 env
t import-history "from ren2" $D history kiri-test/imported:1
t import-gzip "hi" bash -c "$D export ren2 | gzip | $D import - kiri-test/imported:2 >/dev/null && $D run --rm kiri-test/imported:2 cat /added"
if command -v zstd >/dev/null; then
  t import-zstd "hi" bash -c "$D export ren2 | zstd -q | $D import - kiri-test/imported:5 >/dev/null && $D run --rm kiri-test/imported:5 cat /added"
fi
t import-platform "linux/amd64" bash -c "$D export ren2 | $D import --platform linux/amd64 - kiri-test/imported:3 >/dev/null && $D image inspect -f '{{.Os}}/{{.Architecture}}' kiri-test/imported:3"
t import-not-tar "isn't a tar archive" bash -c "echo nope | $D import - kiri-test/imported:4"
t import-bad-change "not a valid change command" bash -c "$D export ren2 | $D import -c 'FROM alpine' - kiri-test/imported:4"
$D rmi kiri-test/imported:1 kiri-test/imported:2 kiri-test/imported:3 >/dev/null 2>&1
t commit "sha256:" $D commit -c 'CMD ["cat","/added"]' -c 'ENV KIRI=1' ren2 kiri-test/committed:1
t commit-runs "hi" $D run --rm kiri-test/committed:1
t commit-env "KIRI=1" $D run --rm kiri-test/committed:1 env
t commit-whiteout "gone" bash -c "$D run --rm kiri-test/committed:1 sh -c 'test -e /etc/motd || echo gone'"
# A renamed directory survives a commit, and a copy straight before a start
# mounts the upper dir twice in a row (overlayfs index, redirect_dir, #227).
t commit-renamed-dir "kept" bash -c "$D rm -f renamed >/dev/null 2>&1; $D run --name renamed alpine mv /etc/apk /etc/apk2; $D commit renamed kiri-test/renamed:1 >/dev/null; $D rm -f renamed >/dev/null; $D run --rm kiri-test/renamed:1 sh -c 'test -f /etc/apk2/repositories && echo kept'"
t cp-then-start "failed 0" bash -c "f=0; for i in 1 2 3 4 5; do id=\$($D create alpine sleep 30); $D cp /etc/hosts \$id:/f >/dev/null 2>&1 || f=\$((f+1)); $D start \$id >/dev/null 2>&1 || f=\$((f+1)); $D rm -f \$id >/dev/null; done; echo failed \$f"
t history "docker commit" $D history kiri-test/committed:1
t history-base "<missing>" $D history kiri-test/committed:1
t commit-stopped "sha256:" bash -c "$D stop -t 1 ren2 >/dev/null && $D commit ren2 kiri-test/committed:2"
$D rm -f ren2 >/dev/null; $D rmi kiri-test/committed:1 kiri-test/committed:2 kiri-test/renamed:1 >/dev/null 2>&1

section "network connect and disconnect"
$D network create na >/dev/null; $D network create nb >/dev/null
$D run -d --name multi --network na nginx:alpine >/dev/null; sleep 0.3
t connect "eth1" bash -c "$D network connect --alias web2 nb multi && $D exec multi ip -o link"
t connect-dns-alias "Welcome" $D run --rm --network nb alpine wget -qO- -T 3 http://web2
t connect-dns-name "Welcome" $D run --rm --network nb alpine wget -qO- -T 3 http://multi
t connect-inspect "na nb" bash -c "$D inspect -f '{{range \$k, \$v := .NetworkSettings.Networks}}{{\$k}} {{end}}' multi"
t connect-members "multi" $D network inspect nb -f '{{range .Containers}}{{.Name}}{{end}}'
t connect-survives-restart "eth1" bash -c "$D restart -t 1 multi >/dev/null && $D exec multi ip -o link"
code connect-twice 1 $D network connect nb multi
t disconnect "no-eth1" bash -c "$D network disconnect nb multi && ($D exec multi ip -o link | grep -q eth1 && echo still || echo no-eth1)"
t disconnect-dns-gone "bad address" $D run --rm --network nb alpine wget -qO- -T 2 http://web2
t compose-multi-net "PONG" bash -c "d=\$(mktemp -d); printf 'services:\n  db:\n    image: redis:7-alpine\n    networks: [back]\n  app:\n    image: redis:7-alpine\n    command: sh -c \"sleep 1; redis-cli -h db ping; sleep 30\"\n    networks: [front, back]\nnetworks:\n  front: {}\n  back: {}\n' > \$d/compose.yaml; cd \$d && $D compose -p kirinet up -d >/dev/null 2>&1; sleep 3; $D compose -p kirinet logs app; $D compose -p kirinet down >/dev/null 2>&1; rm -rf \$d"
$D rm -f multi >/dev/null; $D network rm na nb >/dev/null
# Disconnecting leaves a gap in the eth numbers: each network keeps its own
# interface, a later one takes the free number, and every address goes back.
$D network create nc1 >/dev/null; $D network create nc2 >/dev/null; $D network create nc3 >/dev/null
$D run -d --name gaps alpine sleep 300 >/dev/null
t disconnect-first-then-second "ok" bash -c "$D network connect nc1 gaps && $D network connect nc2 gaps && $D network disconnect nc1 gaps && $D network disconnect nc2 gaps && echo ok"
t connect-into-gap "eth1 eth2" bash -c "$D network connect nc1 gaps && $D network connect nc2 gaps && $D network disconnect nc1 gaps && $D network connect nc3 gaps && $D exec gaps sh -c 'ip -4 -o addr | grep -o \"eth[12] \" | sort | tr -d \"\\n\"'"
t connect-into-gap-addresses "2" bash -c "$D exec gaps ip -4 -o addr | grep -c 'eth[12] '"
t disconnect-all-in-any-order "ok" bash -c "$D network disconnect nc2 gaps && $D network disconnect nc3 gaps && echo ok"
$D rm -f gaps >/dev/null
t network-rm-after-disconnects "all removed" bash -c "$D network rm nc1 nc2 nc3 >/dev/null 2>&1; [ -z \"\$($D network ls -q --filter name=nc1 --filter name=nc2 --filter name=nc3)\" ] && echo all removed"
# A container on the default bridge that joins a user network resolves that
# network's names, running or not yet started (k3d's load balancer), as on
# Docker (#449).
$D network create ndr >/dev/null; $D run -d --name ndr-srv --network ndr alpine sleep 300 >/dev/null
t connect-running-resolv "127.0.0.11" bash -c "$D run -d --name ndr-a alpine sleep 300 >/dev/null && $D network connect ndr ndr-a && $D exec ndr-a cat /etc/resolv.conf"
t connect-running-dns "Name:" $D exec ndr-a nslookup ndr-srv
t connect-before-start-dns "Name:" bash -c "$D create --name ndr-c alpine sh -c 'nslookup ndr-srv; sleep 300' >/dev/null && $D network connect ndr ndr-c && $D start ndr-c >/dev/null && sleep 1 && $D logs ndr-c"
t connect-dns-internet "Address" $D exec ndr-a nslookup example.com
$D rm -f ndr-srv ndr-a ndr-c >/dev/null; $D network rm ndr >/dev/null
# Two projects on a shared network, each with a `db` on its own network too
# (Compose and DDEV do this): `db` is the project's own, as on Docker, which
# searches a container's networks by network name, not in the order joined (#294).
$D network create dno_shared >/dev/null; for p in a b; do $D network create dno-${p}_own >/dev/null; for s in web db; do $D create --name dno-$p-$s --network dno_shared --network-alias $s alpine sleep 300 >/dev/null; $D network connect --alias $s dno-${p}_own dno-$p-$s; $D start dno-$p-$s >/dev/null; done; done
t dns-own-project-first "own" bash -c "want=\$($D inspect -f '{{(index .NetworkSettings.Networks \"dno-a_own\").IPAddress}}' dno-a-db); got=\$($D exec dno-a-web nslookup -type=a db 2>/dev/null | grep '^Address' | grep -v ':53' | cut -d' ' -f2 | tr '\n' ' '); [ \"\$got\" = \"\$want \" ] && echo own || echo \"got \$got want \$want\""
t dns-shared-still-searched "Name:" $D exec dno-a-web nslookup -type=a dno-b-db
$D rm -f dno-a-web dno-a-db dno-b-web dno-b-db >/dev/null; $D network rm dno_shared dno-a_own dno-b_own >/dev/null
# A container that is not running leaves its first network, as on Docker:
# .NET Aspire creates every container on bridge, connects it to a session
# network, disconnects it from bridge, then starts it (#418). It then runs
# on the session network only, with its default route and DNS there.
$D rm -f probe probe-peer >/dev/null 2>&1; $D network rm probe-net >/dev/null 2>&1
$D network create probe-net >/dev/null; $D run -d --name probe-peer --network probe-net alpine sleep 300 >/dev/null
t leave-first-network "ok" bash -c "$D create --name probe alpine sleep 300 >/dev/null && $D network connect probe-net probe && $D network disconnect bridge probe && $D start probe >/dev/null && echo ok"
t leave-first-inspect "[probe-net ]" bash -c "echo \"[\$($D inspect -f '{{range \$k, \$v := .NetworkSettings.Networks}}{{\$k}} {{end}}' probe)]\""
t leave-first-default-route "default via" $D exec probe ip route
t leave-first-dns "Name:" $D exec probe nslookup probe-peer
t leave-first-no-eth1 "eth0 only" bash -c "$D exec probe ip -o link | grep -q eth1 && echo also-eth1 || echo eth0 only"
t leave-first-running-refused "stop the container first" bash -c "$D network connect bridge probe && $D network disconnect probe-net probe 2>&1"
t leave-last-network "lo only" bash -c "$D rm -f probe >/dev/null; $D create --name probe alpine sleep 300 >/dev/null && $D network disconnect bridge probe && $D start probe >/dev/null && ($D exec probe ip -o link | grep -q eth0 && echo has-eth0 || echo lo only)"
t leave-last-reconnect "Name:" bash -c "$D stop -t 0 probe >/dev/null && $D network connect probe-net probe && $D start probe >/dev/null && $D exec probe nslookup probe-peer"
$D rm -f probe probe-peer >/dev/null; t leave-first-network-rm "removed" bash -c "$D network rm probe-net >/dev/null && echo removed"

section "events, prune, system df, search"
t events "start" bash -c "($D events --filter container=evt --format '{{.Action}}' & p=\$!; sleep 0.5; $D run --name evt alpine true >/dev/null; sleep 0.5; kill \$p) 2>/dev/null"
# The CLI rounds --since/--until durations to whole seconds; -1s is "a second from now".
t events-since "die" bash -c "$D events --since 30s --until=-1s --filter container=evt --format '{{.Action}} {{.Actor.Attributes.exitCode}}'"
t events-type "destroy" bash -c "$D rm evt >/dev/null; $D events --since 30s --until=-1s --filter type=container --filter event=destroy --format '{{.Action}}'"
t events-json "timeNano" bash -c "$D events --since 30s --until=-1s --format '{{json .}}' | head -1"
# Prune only what this suite labels: never the user's stopped containers.
t container-prune "Total reclaimed space" bash -c "$D create --name prunable --label kiri-e2e=1 alpine >/dev/null; $D container prune -f --filter label=kiri-e2e=1"
code pruned-gone 1 $D inspect prunable
t system-df "Images" $D system df
t system-df-v "VOLUME NAME" $D system df -v
# Real sizes, measured on the engine's disk: a volume and a container that
# each hold 20 MiB (it once reported every volume as unknown, every
# container as 0 and images at their download size).
t system-df-sizes "SIZES-OK" bash -c "$D rm -f e2e-dfc >/dev/null 2>&1; $D volume rm e2e-dfv >/dev/null 2>&1; $D run --rm -v e2e-dfv:/d alpine sh -c 'head -c 20971520 /dev/urandom > /d/f' && $D run --name e2e-dfc alpine sh -c 'head -c 20971520 /dev/urandom > /f' && curl -s --unix-socket $(printf %q "$SOCK") http://kiri/system/df | python3 -c 'import json,sys; d=json.load(sys.stdin); v=[x[\"UsageData\"][\"Size\"] for x in d[\"Volumes\"] if x[\"Name\"]==\"e2e-dfv\"]; c=[x[\"SizeRw\"] for x in d[\"Containers\"] if \"/e2e-dfc\" in x[\"Names\"]]; i=[x for x in d[\"Images\"] if x[\"Size\"]<=0]; print(\"SIZES-OK\" if v and v[0]>=20971520 and c and c[0]>=20971520 and not i and d[\"LayersSize\"]>0 else (v, c, len(i)))'; $D rm -f e2e-dfc >/dev/null; $D volume rm e2e-dfv >/dev/null"
t search "alpine" $D search alpine --limit 3
t search-official "[OK]" $D search --filter is-official=true alpine --limit 3

section "amd64 images (Rosetta)"
# The amd64 alpine and node, fetched from KIRI_TEST_REGISTRY and tagged with
# their plain names, as a --platform run would pull them from Docker Hub.
kiri_fetch alpine:latest linux/amd64
kiri_fetch node:22-alpine linux/amd64
# On a Mac, Kiri's VM runs them with Rosetta. On a Linux host or in WSL the
# host's kernel runs them: natively on an amd64 machine, elsewhere only with
# an emulator registered for them (binfmt_misc, qemu-user-static), as with
# Docker; without one the kernel answers "exec format error".
if [ "$SYS" != mac ] && [ "$($D version -f '{{.Server.Arch}}' 2>/dev/null)" != amd64 ] &&
  $D run --rm --platform linux/amd64 alpine true 2>&1 | grep -q 'exec format error'; then
  # shellcheck disable=SC2034 # read through `missing`
  NO_AMD64="no amd64 emulator on this host (binfmt_misc)"
fi
needs amd64 t rosetta-uname "x86_64" $D run --rm --platform linux/amd64 alpine uname -m
needs amd64 t rosetta-binary "x86-64" bash -c "$D run --rm --platform linux/amd64 alpine sh -c 'apk add -q --no-cache file >/dev/null 2>&1; file /bin/busybox'"
t rosetta-inspect "linux/amd64" $D image inspect -f '{{.Os}}/{{.Architecture}}' alpine
# A repository with an amd64 image only, run with no --platform: the run's own
# pull finds no arm64 image and takes the amd64 one.
AMD64_ONLY=$(kiri_image_remote amd64/alpine:3.22)
needs amd64 t rosetta-amd64-only-repo "x86_64" $D run --rm "$AMD64_ONLY" uname -m
needs amd64 t rosetta-node "v22" $D run --rm --platform linux/amd64 node:22-alpine node --version
# Rosetta on macOS 15 stops on startup entries it doesn't know ("unhandled
# auxillary vector type 29", #199): the kernel gives AT_HWCAP3 (29) only when
# it says something, which it never does in Kiri's VM. alpine is the amd64
# image here, so on Apple silicon this reads what Rosetta was handed. On a
# Linux host the kernel is the host's, not Kiri's.
only mac t rosetta-no-auxv-29 "no type 29" $D run --rm alpine sh -c 'od -An -v -tu8 -w16 /proc/self/auxv | awk "\$1 == 29 { n++ } END { print n ? \"type 29 given\" : \"no type 29\" }"'
# Back to the engine's own architecture for the rest (an Intel Mac's is amd64).
ARCH=$($D version -f '{{.Server.Arch}}' 2>/dev/null)
t native-again "$([ "$ARCH" = amd64 ] && echo x86_64 || echo aarch64)" bash -c ". '$IMAGES_SH'; kiri_fetch alpine:latest linux/$ARCH && kiri_fetch node:22-alpine linux/$ARCH && $D run --rm alpine uname -m"
$D rmi "$AMD64_ONLY" >/dev/null 2>&1

section "security: seccomp, capabilities, read-only"
# Docker's two defaults in a container's own network: any port and ping
# without root. --sysctl wins; host networking keeps the VM's own (the
# host's own on a Linux host: HOST_UPS, at the top).
t unpriv-port-start "0" $D run --rm alpine cat /proc/sys/net/ipv4/ip_unprivileged_port_start
t unpriv-ping-range "0	2147483647" $D run --rm alpine cat /proc/sys/net/ipv4/ping_group_range
t unpriv-listen-80 "listening" $D run --rm -u 1000 alpine sh -c 'nc -lk -p 80 >/dev/null 2>&1 & sleep 0.5; netstat -ltn | grep -q ":80 " && echo listening'
t unpriv-sysctl-wins "1024" $D run --rm --sysctl net.ipv4.ip_unprivileged_port_start=1024 alpine cat /proc/sys/net/ipv4/ip_unprivileged_port_start
t unpriv-port-host-net "$HOST_UPS" $D run --rm --network host alpine cat /proc/sys/net/ipv4/ip_unprivileged_port_start
t seccomp-on "Seccomp:	2" $D run --rm alpine grep Seccomp: /proc/self/status
t seccomp-userns "not permitted" $D run --rm alpine unshare -U true
t seccomp-keyctl "not permitted" bash -c "$D run --rm alpine sh -c 'apk add -q keyutils >/dev/null 2>&1; keyctl show 2>&1'"
t seccomp-unconfined "userns-ok" $D run --rm --security-opt seccomp=unconfined alpine unshare -U echo userns-ok
t seccomp-privileged-boundary "Seccomp:	2" $D run --rm --privileged alpine grep Seccomp: /proc/self/status
t vsock-unconfined "Address family not supported" $D run --rm --security-opt seccomp=unconfined python:3.13-alpine python -c 'import socket; socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)'
t vsock-privileged "Address family not supported" $D run --rm --privileged python:3.13-alpine python -c 'import socket; socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)'
t seccomp-exec "Seccomp:	2" bash -c "$D run -d --name sec alpine sleep 30 >/dev/null; $D exec sec grep Seccomp: /proc/self/status"
$D rm -f sec >/dev/null
# docker info says the filter is on; a profile of the user's own is applied
# (the CLI sends the file's JSON), under exec too, and one Kiri can't apply
# fails the run instead of quietly giving the default.
SCD=$(mktemp -d)
echo '{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir","mkdirat"],"action":"SCMP_ACT_ERRNO"}]}' > $SCD/deny-mkdir.json
echo '{"defaultAction":"SCMP_ACT_NOPE"}' > $SCD/bad.json
t seccomp-info "name=seccomp,profile=builtin" $D info --format '{{.SecurityOptions}}'
t seccomp-custom-profile "not permitted" $D run --rm --security-opt seccomp=$SCD/deny-mkdir.json alpine mkdir /tmp/x
t seccomp-custom-rest-allowed "hi" $D run --rm --security-opt seccomp=$SCD/deny-mkdir.json alpine echo hi
t seccomp-bad-profile-refused "refused" bash -c "$D run --rm --security-opt seccomp=$SCD/bad.json alpine echo kiri-ran-it 2>&1 | grep -q '^kiri-ran-it' && echo ran || echo refused"
t seccomp-bad-profile-says-why "SCMP_ACT_NOPE" $D run --rm --security-opt seccomp=$SCD/bad.json alpine echo kiri-ran-it
t seccomp-vsock-custom "Address family not supported" $D run --rm --security-opt seccomp=$SCD/deny-mkdir.json python:3.13-alpine python -c 'import socket; socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)'
t seccomp-exec-profile "not permitted" bash -c "$D run -d --name secx --security-opt seccomp=$SCD/deny-mkdir.json alpine sleep 30 >/dev/null; $D exec secx mkdir /tmp/y"
$D rm -f secx >/dev/null
# --privileged only drops the default filter: an explicit profile still applies.
t seccomp-privileged-keeps-profile "not permitted" $D run --rm --privileged --security-opt seccomp=$SCD/deny-mkdir.json alpine mkdir /tmp/x
# A profile that denies seccomp() itself still starts: the host boundary goes
# in first and the profile is the last seccomp() call, as under runc.
echo '{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["seccomp"],"action":"SCMP_ACT_ERRNO"}]}' > $SCD/deny-seccomp.json
echo '{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["seccomp"],"action":"SCMP_ACT_KILL_PROCESS"}]}' > $SCD/kill-seccomp.json
t seccomp-profile-denying-seccomp-runs "hi" $D run --rm --security-opt seccomp=$SCD/deny-seccomp.json alpine echo hi
t seccomp-profile-killing-seccomp-runs "hi" $D run --rm --security-opt seccomp=$SCD/kill-seccomp.json alpine echo hi
# Two conditional rules for one call: libseccomp tests the higher argument
# index first whatever the profile order (checked on Docker 29.8.2), so the
# type rule on arg1 (95) beats the domain rule on arg0 (13) for AF_INET6 DGRAM.
echo '{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":95,"args":[{"index":1,"value":15,"valueTwo":2,"op":"SCMP_CMP_MASKED_EQ"}]},{"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":13,"args":[{"index":0,"value":10,"op":"SCMP_CMP_EQ"}]}]}' > $SCD/overlap.json
t seccomp-overlapping-rules-higher-index-first "v6dgram-errno95 v6stream-errno13 v4dgram-errno95" $D run --rm --security-opt seccomp=$SCD/overlap.json python:3.13-alpine python -c '
import socket
def tr(f,t):
    try: socket.socket(f, t); return "ok"
    except OSError as e: return "errno%d" % e.errno
print("v6dgram-"+tr(socket.AF_INET6, socket.SOCK_DGRAM), "v6stream-"+tr(socket.AF_INET6, socket.SOCK_STREAM), "v4dgram-"+tr(socket.AF_INET, socket.SOCK_DGRAM))'
# As runc orders it: with no-new-privileges the profile goes in just before
# the program starts, after the user switch, so a profile that denies setuid
# still lets --user work; without it the profile goes in first and the
# switch fails.
echo '{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["setuid","setgid","setgroups"],"action":"SCMP_ACT_ERRNO","errnoRet":1}]}' > $SCD/deny-setuid.json
t seccomp-profile-late-under-no-new-privileges "uid=1000" $D run --rm --user 1000 --security-opt no-new-privileges --security-opt seccomp=$SCD/deny-setuid.json alpine id
t seccomp-profile-early-without-no-new-privileges "not permitted" bash -c "$D run --rm --user 1000 --security-opt seccomp=$SCD/deny-setuid.json alpine id 2>&1; true"
rm -rf $SCD
t seccomp-threads "threads ok" $D run --rm python:3.13-alpine python -c 'import threading; t=threading.Thread(target=print,args=("threads ok",)); t.start(); t.join()'
t stdin-200mb "200000000" bash -c "head -c 200000000 /dev/zero | $D run --rm -i alpine wc -c"
t stdin-slow-reader "30000000" bash -c "head -c 30000000 /dev/zero | $D run --rm -i alpine sh -c 'sleep 2; wc -c'"
t seccomp-fork "node ok" $D run --rm node:22-alpine node -e 'require("child_process").execSync("true"); console.log("node ok")'
t cap-default "00000000a80425fb" $D run --rm alpine grep CapEff /proc/self/status
t cap-drop-all "0000000000000000" $D run --rm --cap-drop ALL alpine grep CapEff /proc/self/status
# ping needs no NET_RAW: it uses ping sockets, which ping_group_range opens
# to every group, as on Docker. Without them, dropping NET_RAW stops it.
t cap-drop-net-raw-ping "round-trip" $D run --rm --cap-drop NET_RAW alpine ping -c1 -W1 127.0.0.1
t cap-drop-net-raw "permission denied" $D run --rm --cap-drop NET_RAW --sysctl net.ipv4.ping_group_range="1 0" alpine ping -c1 -W1 127.0.0.1
t cap-add-net-admin "ok" bash -c "$D run --rm --cap-add NET_ADMIN alpine sh -c 'ip link set lo mtu 1400 && echo ok'"
t cap-add-sys-admin "mounted" $D run --rm --cap-add SYS_ADMIN alpine sh -c 'mount -t tmpfs t /mnt && echo mounted'
t cap-inspect "NET_ADMIN]" bash -c "$D create --name capi --cap-add NET_ADMIN alpine >/dev/null; $D inspect -f '{{.HostConfig.CapAdd}}' capi; $D rm capi >/dev/null"
code cap-unknown 125 $D run --rm --cap-add NOPE alpine true
t read-only "Read-only file system" $D run --rm --read-only alpine touch /x
t read-only-tmpfs "ok" bash -c "$D run --rm --read-only --tmpfs /tmp alpine sh -c 'touch /tmp/x && echo ok'"
t read-only-volume "ok" bash -c "$D run --rm --read-only -v rovol:/data alpine sh -c 'touch /data/x && echo ok'; $D volume rm rovol >/dev/null"
t shm-size "256.0M" $D run --rm --shm-size 256m alpine df -h /dev/shm
t shm-default "64.0M" $D run --rm alpine df -h /dev/shm
t no-new-privs "NoNewPrivs:	1" $D run --rm --security-opt no-new-privileges alpine grep NoNewPrivs /proc/self/status

section "UDP ports"
$D run -d --name dns -p 15354:53/udp -p 15354:53/tcp alpine sh -c 'apk add -q dnsmasq >/dev/null 2>&1; dnsmasq -k --address=/kiri.test/10.9.8.7' >/dev/null
for _ in $(seq 40); do dig +short +time=1 +tries=1 @127.0.0.1 -p 15354 kiri.test 2>/dev/null | grep -q 10.9.8.7 && break; sleep 0.3; done
t udp-dns "10.9.8.7" dig +short +time=2 +tries=1 @127.0.0.1 -p 15354 kiri.test
t udp-dns-tcp-same-port "10.9.8.7" dig +short +tcp +time=2 @127.0.0.1 -p 15354 kiri.test
t udp-many "20" bash -c "n=0; for i in \$(seq 20); do dig +short +time=2 +tries=1 @127.0.0.1 -p 15354 kiri.test | grep -q 10.9.8.7 && n=\$((n+1)); done; echo \$n"
t udp-port-listed "53/udp -> 0.0.0.0:15354" $D port dns
t udp-inspect "53/udp" $D inspect -f '{{json .NetworkSettings.Ports}}' dns
$D rm -f dns >/dev/null
code udp-port-free-after 0 bash -c "python3 -c 'import socket; s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind((\"0.0.0.0\", 15354))'"

section "Linux machines"
# The `kiri` CLI is $KIRI, found at the top.
MACHINE_TESTS="machine-create machine-list machine-user machine-mac-files machine-cwd machine-hostname machine-sudo machine-stop machine-stop-again machine-start machine-start-again machine-delete machine-bad-name"
# The binary as one shell word, for commands passed to bash -c (app paths have spaces).
KQ=$(printf %q "$KIRI")
if [ -x "$KIRI" ]; then
# The image a machine is made from, by its registry-qualified name: `--distro
# alpine` pulls alpine:3.22 from Docker Hub every time.
MACHINE_IMG=$KIRI_REG/alpine:3.22
t machine-create "is running" "$KIRI" machine create e2e-m --image "$MACHINE_IMG"
t machine-list "e2e-m" "$KIRI" machine list
t machine-user "$(whoami) $(id -u)" bash -c "$KQ machine shell e2e-m -- sh -c 'echo \$(whoami) \$(id -u)' < /dev/null"
# Machines see the homes folder (/Users on a Mac, /home on Linux: machine.rs
# HOMES), not /private/tmp, so these use a folder under it: under $HOME, or
# under /home itself for root, whose home (/root) is not shared.
case $SYS:$HOME in mac:/Users/* | linux:/home/* | wsl:/home/*) MD=$(mktemp -d "$HOME/.kiri-e2e-machine.XXXXXX") ;; mac:*) MD= ;; *) MD=$(mktemp -d /home/.kiri-e2e-machine.XXXXXX 2>/dev/null) ;; esac
# shellcheck disable=SC2034 # read through `missing`
[ -n "$MD" ] && touch "$MD/kiri-e2e-marker" || NO_HOMES_DIR="no folder of this user's under the homes folder machines share"
needs homes-dir t machine-mac-files "kiri-e2e-marker" bash -c "$KQ machine shell e2e-m -- ls $MD < /dev/null"
needs homes-dir t machine-cwd "$MD" bash -c "cd $MD && $KQ machine shell e2e-m -- pwd < /dev/null"
[ -n "$MD" ] && rm -rf "$MD"
t machine-hostname "e2e-m" bash -c "$KQ machine shell e2e-m -- hostname < /dev/null"
t machine-sudo "root" bash -c "for i in \$(seq 30); do out=\$($KQ machine shell e2e-m -- sudo -n whoami < /dev/null 2>/dev/null); [ \"\$out\" = root ] && break; sleep 1; done; echo \$out"
t machine-stop "Exited" bash -c "$KQ machine stop e2e-m && $KQ machine list"
# Stopped already, or running already: done, not an empty `kiri: ` and a failure.
code machine-stop-again 0 "$KIRI" machine stop e2e-m
t machine-start "Up" bash -c "$KQ machine start e2e-m && $KQ machine list"
code machine-start-again 0 "$KIRI" machine start e2e-m
t machine-delete "deleted" "$KIRI" machine delete e2e-m
$D rmi "$MACHINE_IMG" >/dev/null 2>&1
code machine-bad-name 1 "$KIRI" machine create 'bad_name!'
else
  for n in $MACHINE_TESTS; do skip_setup "$n" "no kiri CLI found: set KIRI=/path/to/kiri"; done
fi
t stop-signal "got USR1" bash -c "$D run -d --name sig --stop-signal USR1 alpine sh -c 'trap \"echo got USR1; exit 0\" USR1; while :; do sleep 0.1; done' >/dev/null; sleep 0.5; $D stop -t 5 sig >/dev/null; $D logs sig; $D rm -f sig >/dev/null"
t stop-signal-inspect "exited (0)" bash -c "$D run -d --name sig2 --stop-signal SIGRTMIN+3 alpine sh -c 'trap \"exit 0\" 37; while :; do sleep 0.1; done' >/dev/null; sleep 0.5; $D stop -t 5 sig2 >/dev/null; $D ps -a --filter name=sig2 --format '{{.Status}}' | tr A-Z a-z | cut -c1-10; $D rm -f sig2 >/dev/null"
if [ "${SLOW:-0}" = 1 ] && [ -x "$KIRI" ]; then
  t machine-systemd "running" bash -c "$KQ machine create e2e-sd --image $KIRI_REG/ubuntu:24.04 --systemd >/dev/null; for i in \$(seq 60); do st=\$($KQ machine shell e2e-sd -- systemctl is-system-running < /dev/null 2>/dev/null); case \"\$st\" in running|degraded) break;; esac; sleep 3; done; echo \$st"
  t machine-systemd-stop "Exited" bash -c "$KQ machine stop e2e-sd && $KQ machine list | grep e2e-sd"
  "$KIRI" machine delete e2e-sd >/dev/null
  $D rmi "$KIRI_REG/ubuntu:24.04" >/dev/null 2>&1
fi

if [ "${K8S:-0}" = 1 ]; then
  section "Kubernetes (K8S=1)"
  KH=$(mktemp -d); mkdir -p "$KH/.kube"
  KK="env HOME=$KH KIRI_SOCKET=$SOCK $KQ k8s"
  t k8s-start "is ready" bash -c "$KK start"
  # The context `kiri k8s start` wrote: named after the Kiri build, not always `kiri`.
  KCTX=$(env HOME="$KH" kubectl config get-contexts -o name 2>/dev/null | head -1); KCTX=${KCTX:-kiri}
  t k8s-node "Ready" env HOME="$KH" kubectl --context $KCTX get nodes
  # The pods' images come from KIRI_TEST_REGISTRY. k3s itself (rancher/k3s) is
  # the one image a run still pulls from Docker Hub: `kiri k8s start` pulls it.
  t k8s-deploy "successfully rolled out" bash -c "env HOME=$KH kubectl --context $KCTX create deployment web --image=$KIRI_REG/nginx:alpine >/dev/null && env HOME=$KH kubectl --context $KCTX rollout status deploy/web --timeout=120s"
  # CoreDNS may still be starting: ask a few times, as a client would.
  t k8s-service-dns "Welcome to nginx" bash -c "env HOME=$KH kubectl --context $KCTX expose deploy web --port 80 >/dev/null; for i in 1 2 3 4 5 6; do out=\$(env HOME=$KH kubectl --context $KCTX run t\$i --rm -i --restart=Never --image=$KIRI_REG/alpine:latest -- wget -qO- -T 5 http://web.default.svc.cluster.local 2>/dev/null); echo \"\$out\" | grep -q Welcome && break; sleep 3; done; echo \"\$out\""
  t k8s-loadbalancer "Welcome to nginx" bash -c "env HOME=$KH kubectl --context $KCTX expose deploy web --name web-lb --type=LoadBalancer --port 18091 --target-port 80 >/dev/null; for i in \$(seq 60); do curl -s -m 2 http://localhost:18091 | grep -q Welcome && break; sleep 2; done; curl -s -m 2 http://localhost:18091"
  t k8s-loadbalancer-gone "gone" bash -c "env HOME=$KH kubectl --context $KCTX delete svc web-lb >/dev/null; sleep 7; curl -s -m 2 http://localhost:18091 || echo gone"
  t k8s-stop "exited" bash -c "$KK stop >/dev/null && $KK status"
  t k8s-resume "is ready" bash -c "$KK start"
  t k8s-delete "deleted" bash -c "$KK delete"
  t k8s-context-gone "0" bash -c "env HOME=$KH kubectl config get-contexts -o name | grep -c kiri"
  rm -rf "$KH"
fi

section "the API's answers, as Docker gives them"
API="curl -s -m 20 --unix-socket $SOCK"
$D rm -f ans ans0 ans-n >/dev/null 2>&1; $D network rm ansnet >/dev/null 2>&1
$D run -d --name ans alpine sh -c 'mkdir /data; ln -s / /data/root; echo hi > /f.txt; chmod 644 /f.txt; sleep 300' >/dev/null; sleep 0.5
$D network create ansnet >/dev/null
# An empty success keeps Docker's status; client libraries check it.
t status-tag "201" $API -o /dev/null -w '%{http_code}' -X POST "http://d/images/alpine/tag?repo=anstag&tag=x"
t status-connect "200" $API -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{"Container":"ans"}' http://d/networks/ansnet/connect
t status-disconnect "200" $API -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{"Container":"ans"}' http://d/networks/ansnet/disconnect
t status-exec-detached "200" bash -c "id=\$(curl -s --unix-socket $SOCK -X POST -H 'Content-Type: application/json' -d '{\"Cmd\":[\"true\"]}' http://d/containers/ans/exec | sed 's/.*\"Id\":\"\([^\"]*\)\".*/\1/'); curl -s -o /dev/null -w '%{http_code}' --unix-socket $SOCK -X POST -H 'Content-Type: application/json' -d '{\"Detach\":true}' http://d/exec/\$id/start"
t status-start-still-204 "204" bash -c "$D create --name ans0 alpine true >/dev/null; curl -s -o /dev/null -w '%{http_code}' --unix-socket $SOCK -X POST http://d/containers/ans0/start"
# An API version older than the minimum is refused; a newer one is served.
t api-too-old "client version 1.12 is too old. Minimum supported API version is 1.24" $API http://d/v1.12/version
t api-too-old-status "400" $API -o /dev/null -w '%{http_code}' http://d/v1.12/containers/json
t api-min-served "200" $API -o /dev/null -w '%{http_code}' http://d/v1.24/version
t api-newer-served "200" $API -o /dev/null -w '%{http_code}' http://d/v1.99/version
# docker cp: /proc and /sys stay empty however they are reached, and the
# archive's mode field holds permission bits only.
t cp-proc-by-link "1" bash -c "$D cp ans:/data/root/proc - | tar -t | wc -l | tr -d ' '"
t cp-sys-by-link "1" bash -c "$D cp ans:/data/root/sys - | tar -t | wc -l | tr -d ' '"
t cp-link-still-copies "hi" bash -c "$D cp ans:/data/root/f.txt - | tar -xO"
t cp-tar-mode "0000644" bash -c "$D cp ans:/f.txt - | dd bs=1 skip=100 count=7 2>/dev/null"
# A container that never started: nothing changed yet, and it commits.
t diff-never-started "0" bash -c "$D rm -f ans0 >/dev/null; $D create --name ans0 alpine true >/dev/null; $D diff ans0 | wc -l | tr -d ' '"
t commit-never-started "committed" bash -c "$D commit ans0 anstag:c >/dev/null && $D run --rm anstag:c echo committed"
# A network that doesn't exist: created, refused at start, and the CLI
# does not read it as a missing image and pull.
t create-missing-network "201" $API -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{"Image":"alpine","HostConfig":{"NetworkMode":"ans-nosuch"}}' 'http://d/containers/create?name=ans-n'
t missing-network-inspect "ans-nosuch created" $D inspect -f '{{.HostConfig.NetworkMode}} {{.State.Status}}' ans-n
t start-missing-network "network ans-nosuch not found" $API -X POST http://d/containers/ans-n/start
t start-missing-network-status "404" $API -o /dev/null -w '%{http_code}' -X POST http://d/containers/ans-n/start
t run-missing-network "network ans-nosuch not found" $D run --rm --name ans-r --network ans-nosuch alpine true
t run-missing-network-no-pull "0" bash -c "$D run --rm --name ans-r --network ans-nosuch alpine true 2>&1 | grep -ci 'pull\|unable to find image'"
t run-missing-network-removed "0" bash -c "$D ps -aq --filter name=ans-r | wc -l | tr -d ' '"
$D rm -f ans ans0 ans-n ans-r >/dev/null 2>&1; $D network rm ansnet >/dev/null 2>&1; $D rmi anstag:x anstag:c >/dev/null 2>&1

section "errors"
t bad-image-name "invalid" $D run --rm 'UPPER/case' true
code no-such-image-pull 125 $D run --rm "$KIRI_REG/kiri-test/no-such-image:none" true

cleanup

if [ "${SOAK:-0}" -gt 0 ]; then
  section "soak: $SOAK containers, checking for leaks"
  DPID=$(pgrep -f 'release/kiri daemon' | head -1)
  fp() { footprint "$DPID" 2>/dev/null | sed -n 's/.*Footprint: \([0-9.]* [KMG]*B\).*/\1/p' | head -1; }
  fds() { lsof -p "$DPID" 2>/dev/null | wc -l | tr -d ' '; }
  guest() { $D run --rm --network host alpine sh -c 'echo "$(ls /var/lib/kiri/containers 2>/dev/null | wc -l) $(ls -d /sys/fs/cgroup/kiri/*/ 2>/dev/null | wc -l) $(ip -o link | grep -c " kv") $(ls /run/kiri 2>/dev/null | wc -l) $(awk "/MemTotal/{t=\$2} /MemAvailable/{a=\$2} END{print t-a}" /proc/meminfo)"' 2>/dev/null; }
  # The balloon's pages count as used inside the guest; take them out.
  ballooned() { curl -s --unix-socket "$SOCK" http://d/info | python3 -c 'import json,sys; d=json.load(sys.stdin); print((d["KiriMemoryCeiling"] - d["KiriMemoryTarget"]) // 1024)'; }
  # Warm up, then take the baseline.
  for i in $(seq 20); do $D run --rm alpine true >/dev/null 2>&1; done
  read -r c0 g0 v0 r0 m0 <<<"$(guest)"; m0=$((m0 - $(ballooned)))
  f0=$(fds); fp0=$(fp)
  echo "  start: daemon $fp0, $f0 fds; guest: $c0 container dirs, $g0 cgroups, $v0 veths, $r0 run dirs, $m0 kB used"
  t0=$(date +%s)
  failures=0
  al() { perl -e 'alarm 20; exec @ARGV' "$@"; }
  for i in $(seq "$SOAK"); do
    case $((i % 5)) in
      0) al $D run --rm --network none alpine true >/dev/null 2>&1 || failures=$((failures + 1)) ;;
      1) al $D run --rm alpine sh -c 'echo x' >/dev/null 2>&1 || failures=$((failures + 1)) ;;
      2)
        out=$(echo in | al $D run -i --rm alpine cat 2>&1)
        if [ "$out" != "in" ]; then
          failures=$((failures + 1))
          echo "  run $i: stdin case got [$out]; still running:"
          for c in $($D ps -q); do
            echo "    $($D inspect -f '{{.Name}} {{.Config.Cmd}}' "$c"): $(al $D exec "$c" ls -l /proc/1/fd 2>&1 | tr '\n' ' ')"
          done
        fi
        ;;
      3) id=$(al $D run -d alpine sleep 30) && al $D exec "$id" true >/dev/null 2>&1 && al $D rm -f "$id" >/dev/null 2>&1 || failures=$((failures + 1)) ;;
      4) al $D run --rm -p 18190:80 alpine true >/dev/null 2>&1 || failures=$((failures + 1)) ;;
    esac
  done
  secs=$(($(date +%s) - t0))
  sleep 1
  read -r c1 g1 v1 r1 m1 <<<"$(guest)"; m1=$((m1 - $(ballooned)))
  f1=$(fds); fp1=$(fp)
  echo "  end:   daemon $fp1, $f1 fds; guest: $c1 container dirs, $g1 cgroups, $v1 veths, $r1 run dirs, $m1 kB used"
  echo "  $SOAK runs in ${secs}s, $failures failed"
  t soak-no-failures "0" echo "$failures"
  t soak-container-dirs "$c0" echo "$c1"
  t soak-cgroups "$g0" echo "$g1"
  # veths from just before the soak may still be on their way out; never more.
  t soak-veths "ok" bash -c "[ $v1 -le $v0 ] && echo ok || echo 'veths $v0 -> $v1'"
  t soak-run-dirs "$r0" echo "$r1"
  # Allow some slack for kernel caches; a leak of a few hundred containers
  # would show as far more than 16 extra fds or 32 MB.
  t soak-fds "ok" bash -c "[ $f1 -le $((f0 + 16)) ] && echo ok || echo 'fds $f0 -> $f1'"
  t soak-guest-memory "ok" bash -c "[ $m1 -le $((m0 + 32768)) ] && echo ok || echo 'guest used $m0 -> $m1 kB'"
fi

# Docker Hub's anonymous allowance used during the run. It is counted per
# network address, so another machine's pulls meanwhile count too: a number
# above 0 is not proof that this run pulled from Docker Hub. Target: 0.
HUB_LEFT1=$(kiri_hub_left)
if [ -n "$HUB_LEFT0" ] && [ -n "$HUB_LEFT1" ]; then
  HUB_MSG="Docker Hub allowance used during the run (whole network)"
  if [ "$HUB_LEFT0" = 0 ]; then echo "$HUB_MSG: unknown (none was left when the run began)"
  elif [ "$HUB_LEFT1" -gt "$HUB_LEFT0" ]; then echo "$HUB_MSG: unknown (Docker Hub's hourly count moved on during the run)"
  else echo "$HUB_MSG: $((HUB_LEFT0 - HUB_LEFT1))"; fi
fi
printf '\n%d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP"
# Skips by reason, so they show and can't quietly grow.
[ "$SKIP" -gt 0 ] && echo "skipped: $(printf '%s\n' "${SKIP_WHY[@]}" | sort | uniq -c | sort -rn | sed 's/^ *\([0-9]*\) /\1 /' | paste -sd, - | sed 's/,/, /g')"
for f in "${FAILED[@]+"${FAILED[@]}"}"; do echo "  failed: $f"; done
[ "$FAIL" -eq 0 ]
