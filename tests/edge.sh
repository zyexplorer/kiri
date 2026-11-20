#!/bin/bash
# Edge cases a release must not get wrong: nothing a person keeps is lost,
# nothing is left behind on the disk, a container cannot hurt the machine,
# the network carries every byte, and a broken request or a crash leaves the
# engine answering. Run on the last release and on the release candidate:
# every check that passes on the release must pass on the candidate.
#
#   tests/edge.sh                 # against the engine the docker CLI points at
#   tests/edge.sh storage net     # only these parts: storage net api limits crash
#
# Only touches things it names edge-* (or labels edge=1). For a test engine:
# it starts about a hundred short-lived containers, and the crash part also
# removes unused unnamed volumes that appeared while it ran.
#
# The crash part runs only when the engine can be killed and started from here:
#   EDGE_KILL='sudo systemctl kill -s KILL kiri'
#   EDGE_START='sudo systemctl reset-failed kiri; sudo systemctl start kiri'
# (systemd stops restarting a service that dies five times in ten seconds.)
# EDGE_VM names a command that runs `sh -c` where the engine keeps its data
# (/var/lib/kiri): unset, it is found by itself (here on a Linux host; inside
# the VM, through a privileged container, on a Mac or Windows).
#
# Images never come from Docker Hub, so a run does not use up its pull limit:
# the ones the checks run from KIRI_TEST_REGISTRY (tests/lib/images.sh, then
# EDGE_MIRROR), the ones the checks pull from EDGE_MIRROR (default
# mirror.gcr.io/library, the same images as Docker Hub's).
#
# Exits non-zero if any check fails.
set -u
D=${D:-docker}
PASS=0; FAIL=0; SKIP=0; KNOWN=0; FAILED=()
T=$(mktemp -d "${TMPDIR:-/tmp}/kiri-edge.XXXXXX")
# A bind-mounted folder must be one the engine can see: under the home folder on a Mac.
BIND=${EDGE_BIND:-$HOME/.cache/kiri-edge-test}
trap 'rm -rf "$T"' EXIT

if command -v timeout >/dev/null 2>&1; then limit() { timeout "$@"; }
elif command -v perl >/dev/null 2>&1; then limit() { local s=$1; shift; perl -e 'alarm shift; exec @ARGV' "$s" "$@"; }
else limit() { shift; "$@"; }; fi
section() { printf '\n== %s\n' "$1"; }
skip() { SKIP=$((SKIP + 1)); printf '  SKIP %s (%s)\n' "$1" "$2"; }
pass() { PASS=$((PASS + 1)); }
fail() { FAIL=$((FAIL + 1)); FAILED+=("$1"); printf '  FAIL %s\n    want: %s\n    got:  %s\n' "$1" "$2" "$(echo "$3" | head -5)"; }
# The output of a check's script as one line: line ends and runs of spaces become one space.
out() { limit 120 bash -c "$1" 2>&1 | tr -d '\r' | tr '\n' ' ' | sed 's/  */ /g; s/^ //; s/ $//'; }
# t NAME EXPECTED SCRIPT: passes when the script's output contains EXPECTED.
t() { local got; got=$(out "$3"); if [[ "$got" == *"$2"* ]]; then pass; else fail "$1" "$2" "$got"; fi; }
# eq NAME EXPECTED SCRIPT: the whole output must be EXPECTED.
eq() { local got; got=$(out "$3"); if [ "$got" = "$2" ]; then pass; else fail "$1" "$2" "$got"; fi; }
# known ISSUE NAME EXPECTED SCRIPT: an open bug. Reported, not counted.
known() { local got; got=$(out "$4"); if [[ "$got" == *"$3"* ]]; then printf '  NOTE %s passes now: issue #%s may be fixed\n' "$2" "$1"; pass; else KNOWN=$((KNOWN + 1)); printf '  KNOWN %s (open issue #%s): %s\n' "$2" "$1" "$(echo "$got" | head -1)"; fi; }
alive() { limit 30 $D version --format '{{.Server.Version}}' >/dev/null 2>&1; }
sum256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -d' ' -f1; }
export -f sum256
M=${EDGE_MIRROR:-mirror.gcr.io/library}
# shellcheck disable=SC2034 # read by lib/images.sh
KIRI_REG_FALLBACK=$M
# shellcheck source=SCRIPTDIR/lib/images.sh
. "$(dirname "$0")/lib/images.sh"
# An image that is here already, or fetched without Docker Hub.
get() { kiri_seed_images "$1"; }
export D BIND T M

# The engine's socket, for requests the docker CLI would never send.
SOCK=${DOCKER_HOST:-$($D context inspect "$($D context show 2>/dev/null)" --format '{{.Endpoints.docker.Host}}' 2>/dev/null)}; SOCK=${SOCK#unix://}
export SOCK
# Where the engine keeps its data, to count what is left behind.
vm() {
  if [ -n "${EDGE_VM:-}" ]; then $EDGE_VM "$1"
  elif [ -d /var/lib/kiri/containers ] && [ -r /var/lib/kiri/containers ]; then sh -c "$1"
  elif [ -d /var/lib/kiri/containers ] && sudo -n true 2>/dev/null; then sudo sh -c "$1"
  else $D run --rm --privileged --pid=host --label edge=1 alpine:3.21 nsenter -t 1 -m sh -c "$1"; fi
}
left() { vm 'echo "containers=$(ls /var/lib/kiri/containers 2>/dev/null | wc -l) mounts=$(grep -c " /var/lib/kiri/containers/" /proc/mounts)"' 2>/dev/null | tr -d ' \r' | tail -1; }
counts() { echo "c=$($D ps -aq | wc -l | tr -d ' ') v=$($D volume ls -q | wc -l | tr -d ' ') n=$($D network ls -q | wc -l | tr -d ' ')"; }
cleanup() {
  $D rm -f -v $($D ps -aq --filter name=edge-) >/dev/null 2>&1
  $D volume rm $($D volume ls -q --filter name=edge-) >/dev/null 2>&1
  $D network rm $($D network ls -q --filter name=edge-) >/dev/null 2>&1
  $D rmi $($D images --format '{{.Repository}}:{{.Tag}}' | grep '^edge/') >/dev/null 2>&1
  rm -rf "$BIND"
}

alive || { echo "edge: the engine does not answer"; exit 1; }
PARTS=${*:-storage net api limits crash}
cleanup
mkdir -p "$BIND"
for i in $KIRI_IMAGES_EDGE; do get $i || { echo "edge: cannot get $i"; exit 1; }; done
echo "engine: $($D version --format '{{.Server.Version}} {{range .Server.Components}}{{.Name}}={{.Version}} {{end}}' 2>&1)"
BASE=$(counts); LEFT0=$(left); VOLS0=$($D volume ls -q | sort)

storage() {
  section "storage: what a person keeps is never lost"
  $D volume create --label edge=1 edge-keep >/dev/null
  $D run --rm -v edge-keep:/d alpine:3.21 sh -c 'echo precious > /d/file; dd if=/dev/urandom of=/d/blob bs=1M count=20 2>/dev/null; sha256sum /d/blob | cut -d" " -f1 > /d/blob.sha'
  SUM=$($D run --rm -v edge-keep:/d alpine:3.21 cat /d/blob.sha); export SUM
  t volume-outlives-its-container "precious" '$D run --name edge-v1 -v edge-keep:/d alpine:3.21 true; $D rm -v edge-v1 >/dev/null; $D run --rm -v edge-keep:/d alpine:3.21 cat /d/file'
  t rm-v-removes-only-anonymous-volumes "edge-keep" '$D run --name edge-v2 -v edge-keep:/d -v /anon alpine:3.21 true; $D rm -v edge-v2 >/dev/null; $D volume ls -q --filter name=edge-keep'
  t volume-in-use-is-not-removed "in use" '$D run -d --name edge-v3 -v edge-keep:/d alpine:3.21 sleep 300 >/dev/null; $D volume rm edge-keep'
  t volume-in-use-by-a-stopped-container-is-not-removed "in use" '$D stop -t 1 edge-v3 >/dev/null; $D volume rm edge-keep'
  # Prune, kept to this script's own label: the unused volume goes, the one a stopped container uses stays.
  eq volume-prune-takes-the-unused-and-keeps-the-one-in-use "edge-keep" '$D volume create --label edge=1 edge-unused >/dev/null; $D volume prune -af --filter label=edge=1 >/dev/null 2>&1; echo $($D volume ls -q --filter name=edge-keep) $($D volume ls -q --filter name=edge-unused)'
  t volume-data-is-intact-after-all-that "$SUM" '$D rm -f edge-v3 >/dev/null; $D run --rm -v edge-keep:/d alpine:3.21 sha256sum /d/blob'
  t container-prune-keeps-volumes-and-running-containers "edge-run edge-keep" '$D run -d --name edge-run --label edge=1 alpine:3.21 sleep 300 >/dev/null; $D run --name edge-done --label edge=1 -v edge-keep:/d alpine:3.21 true; $D container prune -f --filter label=edge=1 >/dev/null; echo $($D ps --format "{{.Names}}" --filter name=edge-run) $($D volume ls -q --filter name=edge-keep)'
  # A bind-mounted folder is the person's own: nothing the engine does removes it.
  mkdir -p "$BIND/mine/sub"; echo "my file" > "$BIND/mine/a.txt"; echo "deep" > "$BIND/mine/sub/b.txt"; ln -s a.txt "$BIND/mine/link"
  BSUM=$(cd "$BIND/mine" && find . -type f | sort | xargs cat | sum256); export BSUM
  t bind-mount-survives-rm-v "$BSUM" '$D run --name edge-b1 -v "$BIND/mine":/m -v /anon alpine:3.21 true; $D rm -v edge-b1 >/dev/null; cd "$BIND/mine" && find . -type f | sort | xargs cat | sum256'
  t bind-mount-survives-prune-commit-and-export "$BSUM" '$D run --name edge-b2 --label edge=1 -v "$BIND/mine":/m alpine:3.21 true; $D commit edge-b2 edge/commit:1 >/dev/null; $D export edge-b2 | wc -c >/dev/null; $D container prune -f --filter label=edge=1 >/dev/null; $D volume prune -af --filter label=edge=1 >/dev/null 2>&1; cd "$BIND/mine" && find . -type f | sort | xargs cat | sum256'
  t a-commit-does-not-take-the-bind-mounts-files "No such file" '$D run --rm edge/commit:1 cat /m/a.txt'
  t read-only-bind-mount-cannot-be-written "Read-only" '$D run --rm -v "$BIND/mine":/m:ro alpine:3.21 sh -c "echo x > /m/a.txt"'
  eq read-only-bind-mount-is-unchanged "my file" 'cat "$BIND/mine/a.txt"'
  # A link inside a mount that points out of it is read inside the container.
  ln -s /etc/hostname "$BIND/mine/out"; HN=$(hostname)
  t a-link-out-of-a-bind-mount-stays-in-the-container "edge-host" '$D run --rm --hostname edge-host -v "$BIND/mine":/m alpine:3.21 cat /m/out'
  rm -f "$BIND/mine/out"

  section "storage: docker cp cannot leave the container"
  $D run -d --name edge-cp alpine:3.21 sleep 300 >/dev/null
  echo "from host" > "$T/in.txt"
  t cp-in-and-out-round-trip "from host" '$D cp "$T/in.txt" edge-cp:/in.txt && $D cp edge-cp:/in.txt - | tar -xO'
  rm -rf /tmp/edge-escape
  t cp-through-a-link-pointing-up-stays-inside "inside" '$D exec edge-cp sh -c "mkdir -p /tmp/edge-escape && ln -s ../../../../../../../../../../tmp/edge-escape /esc"; $D cp "$T/in.txt" edge-cp:/esc/pwn.txt; $D exec edge-cp sh -c "test -f /tmp/edge-escape/pwn.txt && echo inside"'
  eq cp-wrote-nothing-on-the-host "none" 'test -e /tmp/edge-escape/pwn.txt && echo WRITTEN-ON-HOST || echo none'
  t cp-out-through-an-absolute-link-reads-the-container "edge-cp-name" '$D exec edge-cp sh -c "echo edge-cp-name > /etc/hostname-copy; ln -s /etc/hostname-copy /abs"; $D cp -L edge-cp:/abs - | tar -xO'
  t cp-from-a-missing-path-is-an-error "Could not find the file" '$D cp edge-cp:/nope/nothing "$T/x" 2>&1; echo'
  t cp-a-tree-with-links-out-and-back "target one" '$D exec edge-cp sh -c "mkdir -p /tree/d && echo one > /tree/d/f && ln -s d/f /tree/l && ln /tree/d/f /tree/h"; rm -rf "$T/tree"; $D cp edge-cp:/tree "$T/tree" && $D cp "$T/tree" edge-cp:/tree2 && $D exec edge-cp sh -c "readlink /tree2/l | sed s,d/f,target,; cat /tree2/h" | tr "\n" " "'
  $D rm -f edge-cp >/dev/null

  section "storage: images"
  t image-in-use-is-not-removed "kept" '$D tag busybox:1.36 edge/used:1; $D run -d --name edge-i1 edge/used:1 sleep 300 >/dev/null; $D rmi edge/used:1 >/dev/null 2>&1; $D exec edge-i1 echo kept'
  t removing-one-tag-keeps-the-shared-layers "ok" '$D rm -f edge-i1 >/dev/null; $D tag busybox:1.36 edge/twin:1; $D rmi edge/twin:1 edge/used:1 >/dev/null 2>&1; $D run --rm --pull never busybox:1.36 echo ok'
  t five-pulls-of-one-image-at-once "5 ok" '$D rmi $M/busybox:1.35 >/dev/null 2>&1; n=$(for i in 1 2 3 4 5; do ( $D pull -q $M/busybox:1.35 >/dev/null 2>&1 && echo ok ) & done | wc -l | tr -d " "); wait; echo $n $($D run --rm --pull never $M/busybox:1.35 echo ok); $D rmi $M/busybox:1.35 >/dev/null 2>&1'
  t pulls-of-different-images-at-once-leave-a-sound-index "3.18 3.19 3.20" 'for v in 3.18 3.19 3.20; do $D rmi $M/alpine:$v >/dev/null 2>&1; $D pull -q $M/alpine:$v >/dev/null 2>&1 & done; wait; for v in 3.18 3.19 3.20; do $D run --rm --pull never $M/alpine:$v cut -c1-4 /etc/alpine-release; $D rmi $M/alpine:$v >/dev/null 2>&1; done | tr "\n" " "'
  t save-and-load-round-trip "edge/saved:1" '$D tag busybox:1.36 edge/saved:1; $D save edge/saved:1 -o "$T/s.tar" && $D rmi edge/saved:1 >/dev/null && $D load -q -i "$T/s.tar" >/dev/null 2>&1; $D images --format "{{.Repository}}:{{.Tag}}" edge/saved'
  t a-broken-archive-is-refused-and-the-engine-answers "alive" 'head -c 300000 /dev/urandom > "$T/junk.tar"; $D load -i "$T/junk.tar" >/dev/null 2>&1; head -c 1000 "$T/s.tar" > "$T/cut.tar"; $D load -i "$T/cut.tar" >/dev/null 2>&1; $D version >/dev/null && echo alive'
  t images-still-list-and-run-after-bad-loads "ok" '$D images -q >/dev/null && $D run --rm --pull never busybox:1.36 echo ok'

  section "storage: nothing is left on the disk"
  local before; before=$(counts); local l0; l0=$(left)
  t thirty-runs-with-anonymous-volumes-leave-nothing "$before" 'for i in $(seq 30); do $D run --rm -v /anon -v /anon2 alpine:3.21 sh -c "echo x > /anon/f" || echo run-failed; done; echo "c=$($D ps -aq | wc -l | tr -d " ") v=$($D volume ls -q | wc -l | tr -d " ") n=$($D network ls -q | wc -l | tr -d " ")"'
  t twenty-runs-at-once-all-succeed "20" 'for i in $(seq 20); do ( $D run --rm alpine:3.21 true && echo ok ) & done | grep -c ok; wait'
  t create-start-kill-rm-in-a-loop "$before" 'for i in $(seq 20); do c=$($D create --name edge-loop alpine:3.21 sleep 60) && $D start edge-loop >/dev/null && $D rm -f edge-loop >/dev/null || echo failed-$i; done; echo "c=$($D ps -aq | wc -l | tr -d " ") v=$($D volume ls -q | wc -l | tr -d " ") n=$($D network ls -q | wc -l | tr -d " ")"'
  t a-container-removed-ten-times-at-once "gone alive" '$D run -d --name edge-race alpine:3.21 sleep 300 >/dev/null; for i in $(seq 10); do $D rm -f edge-race >/dev/null 2>&1 & done; wait; $D inspect edge-race >/dev/null 2>&1 && echo still || echo gone; $D version >/dev/null && echo alive'
  t start-stop-and-rm-racing-leave-a-clean-name "reused" 'for i in 1 2 3 4 5; do $D run -d --name edge-race2 alpine:3.21 sleep 300 >/dev/null 2>&1; ( $D stop -t 0 edge-race2 & $D rm -f edge-race2 & $D start edge-race2 & wait ) >/dev/null 2>&1; $D rm -f edge-race2 >/dev/null 2>&1; done; $D run --rm --name edge-race2 alpine:3.21 echo reused'
  if [ -n "$l0" ]; then sleep 3; eq container-folders-and-mounts-are-back-to-what-they-were "$l0" 'true; '"$(declare -f vm left)"'; left'; else skip container-folders "cannot look where the engine keeps its data"; fi
  t a-sparse-file-in-a-volume-takes-no-space "small" '$D run --rm -v edge-sparse:/d alpine:3.21 sh -c "truncate -s 2G /d/sparse && [ \$(du -m /d/sparse | cut -f1) -lt 10 ] && echo small"; $D volume rm edge-sparse >/dev/null'
  t twenty-thousand-small-files-and-their-removal "20000 gone" '$D run --rm -v edge-many:/d alpine:3.21 sh -c "i=0; while [ \$i -lt 20000 ]; do : > /d/f\$i; i=\$((i+1)); done; ls /d | wc -l"; $D volume rm edge-many >/dev/null && { $D volume inspect edge-many >/dev/null 2>&1 && echo still || echo gone; }'
  t a-full-tmpfs-does-not-stop-the-container-or-engine "full alive" '$D run --rm --tmpfs /t:size=1m alpine:3.21 sh -c "dd if=/dev/zero of=/t/f bs=1M count=5 2>/dev/null || echo full; echo alive"'
}

net() {
  section "network: reach and isolation"
  $D network create edge-n1 >/dev/null; $D network create edge-n2 >/dev/null; $D network create --internal edge-in >/dev/null
  $D run -d --name edge-a --network edge-n1 --network-alias aaa alpine:3.21 sleep 600 >/dev/null
  $D run -d --name edge-b --network edge-n1 alpine:3.21 sleep 600 >/dev/null
  $D run -d --name edge-c --network edge-n2 alpine:3.21 sleep 600 >/dev/null
  $D run -d --name edge-i --network edge-in alpine:3.21 sleep 600 >/dev/null
  t internet-by-name "Example Domain" 'for i in 1 2 3; do $D exec edge-a wget -qO- -T 10 http://example.com && break; sleep 2; done'
  t same-network-by-name-and-alias "1 packets received 1 packets received" 'echo $($D exec edge-b ping -c 1 -W 3 edge-a | grep received | cut -d, -f2) $($D exec edge-b ping -c 1 -W 3 aaa | grep received | cut -d, -f2)'
  IPC=$($D inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' edge-c); export IPC
  t another-network-is-not-reachable "0 packets received" '$D exec edge-a ping -c 1 -W 2 $IPC 2>&1 | grep -E "received|unreachable"'
  t another-networks-names-do-not-resolve "bad address" '$D exec edge-a ping -c 1 -W 2 edge-c 2>&1'
  t an-internal-network-has-no-internet "no-internet" '$D exec edge-i wget -qO- -T 4 http://example.com >/dev/null 2>&1 && echo HAS-INTERNET || echo no-internet'
  # Only loopback may have an address, and nothing may lead out. (Some kernels, WSL 2's among
  # them, put unused tunnel devices such as tunl0 and sit0 in every new network namespace.)
  t network-none-has-only-loopback "addresses=0 routes=0" '$D run --rm --network none alpine:3.21 sh -c "echo addresses=\$(ip -o addr | grep -vc \" lo \") routes=\$(ip route | wc -l)"'
  t connect-gives-reach-disconnect-takes-it-away "yes no" '$D network connect edge-n2 edge-a; a=$($D exec edge-a ping -c 1 -W 3 edge-c >/dev/null 2>&1 && echo yes || echo no); $D network disconnect edge-n2 edge-a; b=$($D exec edge-a ping -c 1 -W 2 $IPC >/dev/null 2>&1 && echo yes || echo no); echo $a $b'
  t still-reaches-its-first-network-after-that "1 packets received" '$D exec edge-a ping -c 1 -W 3 edge-b'
  t a-removed-containers-name-stops-resolving "bad address" '$D rm -f edge-b >/dev/null; $D exec edge-a ping -c 1 -W 2 edge-b 2>&1'
  t a-failed-start-leaves-no-name-behind "bad address" '$D run -d --name edge-bad --network edge-n1 alpine:3.21 /no/such/command >/dev/null 2>&1; $D exec edge-a ping -c 1 -W 2 edge-bad 2>&1; $D rm -f edge-bad >/dev/null 2>&1'
  t a-name-is-taken-over-by-its-next-container "1 packets received" '$D run -d --name edge-b --network edge-n1 alpine:3.21 sleep 600 >/dev/null; $D exec edge-a ping -c 1 -W 3 edge-b'
  t a-network-in-use-is-not-removed "active endpoints" '$D network rm edge-n1 2>&1'
  t add-host-and-dns-options "10.9.8.7 1.2.3.4" '$D run --rm --add-host mybox:10.9.8.7 --dns 1.2.3.4 alpine:3.21 sh -c "grep mybox /etc/hosts | cut -f1; grep nameserver /etc/resolv.conf | cut -d\" \" -f2" | tr "\n" " "'

  section "network: published ports carry every byte"
  $D run -d --name edge-web -p 127.0.0.1:18441:80 -v "$BIND":/usr/share/nginx/html:ro nginx:alpine >/dev/null
  head -c 104857600 /dev/urandom > "$BIND/big.bin"; echo "edge page" > "$BIND/index.html"
  BIGSUM=$(sum256 < "$BIND/big.bin"); export BIGSUM
  t port-answers "edge page" 'for i in $(seq 30); do curl -s -m 5 http://127.0.0.1:18441/ && break; sleep 0.5; done'
  t a-100-MB-download-through-a-port-is-intact "$BIGSUM" 'curl -s -m 100 http://127.0.0.1:18441/big.bin | sum256'
  t four-large-downloads-at-once-are-intact "4" 'for i in 1 2 3 4; do ( curl -s -m 110 http://127.0.0.1:18441/big.bin | sum256 | grep -c "$BIGSUM" ) & done | grep -c 1; wait'
  t three-hundred-requests-one-after-another "300" 'n=0; for i in $(seq 300); do curl -s -m 5 -o /dev/null -w "%{http_code}\n" http://127.0.0.1:18441/; done | grep -c 200'
  t sixty-requests-at-once "60" 'for i in $(seq 60); do curl -s -m 20 -o /dev/null -w "%{http_code}\n" http://127.0.0.1:18441/ & done | grep -c 200; wait'
  t a-100-MB-transfer-between-two-containers-is-intact "$BIGSUM" '$D network connect --alias web edge-n1 edge-web; $D exec edge-a sh -c "wget -qO- -T 100 http://web/big.bin | sha256sum"'
  t the-same-host-port-twice-is-refused-and-the-first-still-works "allocated edge page" '$D run -d --name edge-web2 -p 127.0.0.1:18441:80 nginx:alpine 2>&1 | grep -o allocated | head -1; $D rm -f edge-web2 >/dev/null 2>&1; curl -s -m 5 http://127.0.0.1:18441/'
  t a-port-is-free-again-the-moment-its-container-goes "edge page two" '$D rm -f edge-web >/dev/null; mkdir -p "$BIND/two"; echo "edge page two" > "$BIND/two/index.html"; $D run -d --name edge-web -p 127.0.0.1:18441:80 -v "$BIND/two":/usr/share/nginx/html:ro nginx:alpine >/dev/null && for i in $(seq 30); do curl -s -m 5 http://127.0.0.1:18441/ && break; sleep 0.5; done'
  t a-random-port-is-reported-and-answers "edge page two" '$D run -d --name edge-rand -p 127.0.0.1::80 -v "$BIND/two":/usr/share/nginx/html:ro nginx:alpine >/dev/null; p=$($D port edge-rand 80 | head -1 | sed "s/.*://"); for i in $(seq 30); do curl -s -m 5 http://127.0.0.1:$p/ && break; sleep 0.5; done'
  # socat for the next two, from a small image made here; without it (no way to fetch it) they are skipped.
  $D run --name edge-tools alpine:3.21 apk add -q --no-cache socat >/dev/null 2>&1 && $D commit edge-tools edge/tools:1 >/dev/null 2>&1; $D rm -f edge-tools >/dev/null 2>&1
  if $D run --rm --pull never edge/tools:1 socat -V >/dev/null 2>&1; then
    $D run -d --name edge-udp -p 127.0.0.1:18442:9999/udp edge/tools:1 socat UDP-LISTEN:9999,fork EXEC:cat >/dev/null
    $D run -d --name edge-hc -p 127.0.0.1:18443:7000 edge/tools:1 sh -c 'printf "#!/bin/sh\ncat >/dev/null\necho half-close-ok\n" > /srv.sh; chmod +x /srv.sh; exec socat TCP-LISTEN:7000,fork,reuseaddr EXEC:/srv.sh' >/dev/null
    sleep 1
    t a-udp-port-echoes "udp-echo" 'python3 -c "
import socket
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(2)
for i in range(10):
    try:
        s.sendto(b\"udp-echo\",(\"127.0.0.1\",18442)); print(s.recv(100).decode()); break
    except socket.timeout: pass"'
    t a-half-closed-connection-still-gets-its-answer "half-close-ok" 'python3 -c "
import socket, time
for i in range(5):
    try:
        s=socket.create_connection((\"127.0.0.1\",18443),5); s.settimeout(10); s.sendall(b\"x\"*100000); s.shutdown(socket.SHUT_WR); print(s.recv(100).decode()); break
    except OSError as e: err=e; time.sleep(1)
else: print(\"failed:\", err)"'
  else skip udp-and-half-close "socat could not be fetched"; fi

  section "network: nothing is left behind"
  local nb; nb=$($D network ls -q | wc -l | tr -d ' ')
  t thirty-networks-made-and-removed "$nb alive" 'for i in $(seq 30); do $D network create edge-cyc >/dev/null && $D run --rm --network edge-cyc alpine:3.21 true && $D network rm edge-cyc >/dev/null || echo failed-$i; done; echo "$($D network ls -q | wc -l | tr -d " ") $($D version >/dev/null && echo alive)"'
  t the-same-subnet-can-be-used-again-after-its-network-is-removed "resolved" 'for i in 1 2 3; do $D network create --subnet 10.213.7.0/24 edge-sub >/dev/null 2>&1 && $D run -d --name edge-s1 --network edge-sub alpine:3.21 sleep 60 >/dev/null 2>&1 && $D run --rm --network edge-sub alpine:3.21 ping -c 1 -W 3 edge-s1 >/dev/null 2>&1 || { echo failed-round-$i; $D rm -f edge-s1 >/dev/null 2>&1; $D network rm edge-sub >/dev/null 2>&1; exit; }; $D rm -f edge-s1 >/dev/null; $D network rm edge-sub >/dev/null; done; $D rm -f edge-s1 >/dev/null 2>&1; $D network rm edge-sub >/dev/null 2>&1; echo resolved'
  $D rm -f edge-a edge-b edge-c edge-i edge-web edge-rand edge-udp edge-hc >/dev/null 2>&1
  $D network rm edge-n1 edge-n2 edge-in >/dev/null 2>&1
}

api() {
  section "api: a bad request never stops the engine"
  if [ ! -S "$SOCK" ]; then skip raw-requests "no unix socket for this engine here"; else
  t junk-and-broken-requests-get-an-answer-or-a-close "survived" 'python3 - <<PY
import socket, os
sock = os.environ["SOCK"]
cases = {
 "garbage": os.urandom(4096),
 "no-version": b"GET /version\r\n\r\n",
 "huge-header": b"GET /version HTTP/1.1\r\nHost: d\r\nX-Big: " + b"a" * 2000000 + b"\r\n\r\n",
 "many-headers": b"GET /version HTTP/1.1\r\nHost: d\r\n" + b"".join(b"X-%d: y\r\n" % i for i in range(20000)) + b"\r\n",
 "negative-length": b"POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Length: -5\r\n\r\n",
 "huge-length": b"POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: 99999999999999999999\r\n\r\n{}",
 "length-without-body": b"POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: 500\r\n\r\n{",
 "bad-chunk": b"POST /containers/create HTTP/1.1\r\nHost: d\r\nTransfer-Encoding: chunked\r\n\r\nZZZZ\r\n{}\r\n0\r\n\r\n",
 "bad-json": b"POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: 9\r\n\r\n{\"Image\":",
 "deep-json": b"POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: 200000\r\n\r\n" + b"[" * 200000,
 "nul-in-path": b"GET /containers/a%00b/json HTTP/1.1\r\nHost: d\r\n\r\n",
 "dots-in-path": b"GET /containers/%2e%2e%2f%2e%2e%2fetc%2fpasswd/json HTTP/1.1\r\nHost: d\r\n\r\n",
 "bad-percent": b"GET /images/%zz%/json HTTP/1.1\r\nHost: d\r\n\r\n",
 "long-path": b"GET /containers/" + b"a" * 100000 + b"/json HTTP/1.1\r\nHost: d\r\n\r\n",
 "two-in-one": b"GET /_ping HTTP/1.1\r\nHost: d\r\n\r\nGET /_ping HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n",
}
for name, data in cases.items():
    s = socket.socket(socket.AF_UNIX); s.settimeout(8)
    try:
        s.connect(sock)
        try: s.sendall(data)
        except OSError: pass
        try: out = s.recv(200)
        except socket.timeout: out = b"(no answer in 8 s)"
        except OSError as e: out = b"(closed)"
        print(name, (out.split(b"\r\n")[0] or b"(closed)").decode("latin1")[:40])
    finally: s.close()
PY
$D version >/dev/null && echo survived'
  t two-requests-on-one-connection-both-answer "2" 'python3 - <<PY
import socket, os
s = socket.socket(socket.AF_UNIX); s.settimeout(8); s.connect(os.environ["SOCK"])
s.sendall(b"GET /_ping HTTP/1.1\r\nHost: d\r\n\r\nGET /_ping HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n")
out = b""
try:
    while True:
        b = s.recv(4096)
        if not b: break
        out += b
except socket.timeout: pass
print(out.count(b"HTTP/1.1 200"))
PY'
  t fifty-requests-on-one-kept-connection "50" 'u=""; for i in $(seq 50); do u="$u http://d/_ping"; done; curl -s --unix-socket "$SOCK" -o /dev/null -w "%{http_code}\n" $u | grep -c 200'
  t a-slow-client-does-not-hold-up-others "fast" 'python3 -c "
import socket, os, time
s=socket.socket(socket.AF_UNIX); s.connect(os.environ[\"SOCK\"]); s.sendall(b\"GET /version HTTP/1.1\r\nHost: d\r\n\"); time.sleep(6)" & s=$(date +%s); $D version >/dev/null; e=$(date +%s); [ $((e - s)) -lt 4 ] && echo fast; wait'
  fi
  t an-encoded-image-name-resolves "alpine:3.21" 'curl -s --unix-socket "$SOCK" "http://d/images/alpine%3A3.21/json" | grep -o "alpine:3.21" | head -1'
  t an-encoded-path-cannot-climb-out "404" 'curl -s --unix-socket "$SOCK" -o /dev/null -w "%{http_code}" "http://d/containers/%2e%2e%2f%2e%2e%2fversion/json"'

  section "api: filters, exec and logs under load"
  $D run -d --name edge-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab alpine:3.21 sleep 600 >/dev/null
  $D run -d --name edge.dot alpine:3.21 sleep 600 >/dev/null
  t a-costly-pattern-in-the-name-filter-answers-at-once "quick" 's=$(date +%s); $D ps --filter "name=(a+)+\$" >/dev/null 2>&1; $D ps --filter "name=(a|aa)+\$" >/dev/null 2>&1; $D ps --filter "name=(.*a){25}" >/dev/null 2>&1; e=$(date +%s); [ $((e - s)) -lt 6 ] && echo quick'
  t a-broken-pattern-is-an-error-not-a-crash "alive" '$D ps --filter "name=[" >/dev/null 2>&1; $D ps --filter "name=(" >/dev/null 2>&1; $D ps --filter "name=*" >/dev/null 2>&1; $D version >/dev/null && echo alive'
  t a-plain-name-filter-still-finds-its-container "edge.dot" '$D ps --filter name=edge.dot --format "{{.Names}}"'
  t a-part-of-a-name-still-matches "edge.dot" '$D ps --filter name=dge.do --format "{{.Names}}"'
  $D run -d --name edge-x alpine:3.21 sleep 600 >/dev/null
  t thirty-execs-at-once-all-answer "30" 'for i in $(seq 30); do $D exec edge-x echo out-$i & done | grep -c out-; wait'
  t a-10-MB-exec-output-is-complete "10485760" '$D exec edge-x sh -c "head -c 10485760 /dev/zero" | wc -c | tr -d " "'
  t exec-exit-codes-come-through "0 7 127" 'a=$($D exec edge-x true; echo $?); b=$($D exec edge-x sh -c "exit 7"; echo $?); c=$($D exec edge-x /no/such 2>/dev/null; echo $?); echo $a $b $c'
  t exec-in-a-stopped-container-is-refused "not running" '$D stop -t 0 edge-x >/dev/null; $D exec edge-x true 2>&1'
  t signals-to-a-stopped-container-are-refused "not running not running" 'echo $($D kill edge-x 2>&1 | grep -o "not running") $($D kill -s HUP edge-x 2>&1 | grep -o "not running")'
  t exec-works-again-after-a-restart "back" '$D start edge-x >/dev/null && $D exec edge-x echo back'
  t five-MB-of-logs-are-all-kept "5242880" '$D run --name edge-log alpine:3.21 sh -c "head -c 5242880 /dev/zero | tr \"\\000\" a" >/dev/null; $D logs edge-log 2>/dev/null | wc -c | tr -d " "'
  t logs-tail-and-follow-end "line-3 ended" '$D run -d --name edge-fol alpine:3.21 sh -c "echo line-1; echo line-2; echo line-3; sleep 2" >/dev/null; $D logs -f --tail 1 edge-fol | tr "\n" " "; echo ended'
  t stop-kills-a-container-that-ignores-the-signal "137 quick" '$D run -d --name edge-stubborn alpine:3.21 sh -c "trap \"\" TERM; while :; do sleep 1; done" >/dev/null; s=$(date +%s); $D stop -t 2 edge-stubborn >/dev/null; e=$(date +%s); echo $($D inspect -f "{{.State.ExitCode}}" edge-stubborn) $([ $((e - s)) -lt 12 ] && echo quick)'
  t on-failure-gives-up-after-its-limit "exited 3" '$D run -d --name edge-of --restart on-failure:3 alpine:3.21 sh -c "exit 5" >/dev/null; for i in $(seq 60); do [ "$($D inspect -f "{{.State.Status}} {{.RestartCount}}" edge-of)" = "exited 3" ] && break; sleep 0.5; done; $D inspect -f "{{.State.Status}} {{.RestartCount}}" edge-of'
  t always-comes-back-each-time-its-process-dies "came-back" '$D run -d --name edge-al --restart always alpine:3.21 sh -c "sleep 1; exit 3" >/dev/null; for i in $(seq 40); do [ "$($D inspect -f "{{.RestartCount}}" edge-al)" -ge 2 ] && { echo came-back; break; }; sleep 0.5; done'
  t a-stopped-always-container-stays-stopped "false same" '$D stop -t 0 edge-al >/dev/null; a=$($D inspect -f "{{.RestartCount}}" edge-al); sleep 4; b=$($D inspect -f "{{.RestartCount}}" edge-al); echo $($D inspect -f "{{.State.Running}}" edge-al) $([ "$a" = "$b" ] && echo same || echo "restarted-again:$a->$b")'
  $D rm -f $($D ps -aq --filter name=edge) >/dev/null 2>&1
}

limits() {
  section "limits: a container cannot hurt the machine"
  $D run -d --name edge-bystander alpine:3.21 sleep 600 >/dev/null
  t a-memory-limit-stops-a-runaway "true 137" '$D run --name edge-oom --memory 48m --memory-swap 48m alpine:3.21 sh -c "a=\$(head -c 400m /dev/zero | tr \"\\000\" x); echo \${#a}" >/dev/null 2>&1; $D inspect -f "{{.State.OOMKilled}} {{.State.ExitCode}}" edge-oom'
  t the-engine-and-other-containers-live-through-it "true alive" '$D inspect -f "{{.State.Running}}" edge-bystander; $D version >/dev/null && echo alive'
  t a-process-limit-stops-a-fork-bomb "contained" '$D run --rm --name edge-fork --pids-limit 60 alpine:3.21 sh -c "f() { f | f & }; f 2>/dev/null; sleep 3; echo contained" 2>/dev/null | tail -1'
  t the-engine-answers-after-the-fork-bomb "true alive" '$D rm -f edge-fork >/dev/null 2>&1; $D inspect -f "{{.State.Running}}" edge-bystander; $D version >/dev/null && echo alive'
  # Opened for writing but never written to: a write would act on the machine.
  t the-kernels-switches-cannot-be-opened-for-writing "ro ro ro" '$D run --rm alpine:3.21 sh -c "for f in /proc/sysrq-trigger /proc/sys/kernel/panic /proc/sys/vm/drop_caches; do ( : >> \$f ) 2>/dev/null && echo WRITABLE:\$f || echo ro; done" | tr "\n" " "'
  t a-container-cannot-restart-the-machine "true alive" '$D run --rm alpine:3.21 reboot -f >/dev/null 2>&1; sleep 2; echo $($D inspect -f "{{.State.Running}}" edge-bystander) $($D version >/dev/null && echo alive)'
  t a-container-sees-no-disks-or-kernel-memory "none" '$D run --rm alpine:3.21 sh -c "for f in /dev/vda /dev/sda /dev/nvme0n1 /dev/mem /dev/kmem /dev/port; do test -e \$f && echo \$f; done; head -c 1 /dev/kmsg >/dev/null 2>&1 && echo kmsg; head -c 1 /proc/kcore 2>/dev/null | wc -c | grep -qx 1 && echo kcore; echo none" | head -1'
  # Docker's default set: no module loading, clock, raw disk, reboot or admin rights.
  t the-default-capabilities-are-dockers "00000000a80425fb" '$D run --rm alpine:3.21 grep CapEff /proc/self/status'
  t a-container-cannot-mount-or-set-the-clock "denied denied" '$D run --rm alpine:3.21 sh -c "mount -t tmpfs none /mnt 2>/dev/null && echo MOUNTED || echo denied; date -s \"\$(date +%Y-%m-%d\ %H:%M:%S)\" 2>&1 | grep -q \"not permitted\" && echo denied || echo CLOCK-SET"'
  t dropped-capabilities-and-no-new-privileges-hold "0000000000000000 1" '$D run --rm --cap-drop ALL --security-opt no-new-privileges alpine:3.21 sh -c "grep CapEff /proc/self/status | cut -f2; grep NoNewPrivs /proc/self/status | cut -f2" | tr "\n" " "'
  t a-read-only-root-refuses-writes-and-tmpfs-takes-them "Read-only ok" '$D run --rm --read-only --tmpfs /scratch alpine:3.21 sh -c "touch /x 2>&1 | grep -o Read-only; touch /scratch/y && echo ok" | tr "\n" " "'
  t a-user-container-is-not-root "1000 denied" '$D run --rm --user 1000:1000 alpine:3.21 sh -c "id -u; cat /etc/shadow >/dev/null 2>&1 && echo READ || echo denied" | tr "\n" " "'
  t the-docker-socket-is-not-in-a-container-by-default "none" '$D run --rm alpine:3.21 sh -c "ls /var/run/docker.sock /run/docker.sock /run/kiri 2>/dev/null; echo none" | head -1'
  $D rm -f edge-bystander edge-oom >/dev/null 2>&1
}

crash() {
  section "crash: the engine is killed while it works"
  if [ -z "${EDGE_KILL:-}" ] || [ -z "${EDGE_START:-}" ]; then skip crash "set EDGE_KILL and EDGE_START to run it"; return; fi
  kill_engine() { sh -c "$EDGE_KILL" >/dev/null 2>&1; }
  back() { sh -c "$EDGE_START" >/dev/null 2>&1; for _ in $(seq 240); do $D version >/dev/null 2>&1 && return 0; sleep 0.5; done; return 1; }
  export -f back kill_engine; export EDGE_KILL EDGE_START
  $D volume create edge-crash >/dev/null
  $D run --rm -v edge-crash:/d alpine:3.21 sh -c 'dd if=/dev/urandom of=/d/done bs=1M count=10 2>/dev/null; sync; sha256sum /d/done | cut -d" " -f1 > /d/done.sha; sync'
  CSUM=$($D run --rm -v edge-crash:/d alpine:3.21 cat /d/done.sha); export CSUM
  t killed-during-pulls-three-times-the-index-is-sound "sound" 'for n in 1 2 3; do $D rmi $M/alpine:3.17 $M/alpine:3.16 $M/alpine:3.15 >/dev/null 2>&1; for v in 3.17 3.16 3.15; do $D pull -q $M/alpine:$v >/dev/null 2>&1 & done; sleep 0.$((RANDOM % 9 + 1)); kill_engine; wait; back || { echo engine-did-not-return-$n; exit; }; done; $D images -q >/dev/null || { echo images-fails; exit; }; b=$(for i in $($D images --format "{{.Repository}}:{{.Tag}}" | grep -v "<none>"); do $D run --rm --pull never --entrypoint true $i >/dev/null 2>&1 || $D run --rm --pull never $i true >/dev/null 2>&1 || echo "listed-but-broken:$i"; done); [ -z "$b" ] && echo sound || echo "$b"'
  t images-pull-again-after-those-kills "3.17" '$D pull -q $M/alpine:3.17 >/dev/null 2>&1; $D run --rm --pull never $M/alpine:3.17 cut -c1-4 /etc/alpine-release; $D rmi $M/alpine:3.17 $M/alpine:3.16 $M/alpine:3.15 >/dev/null 2>&1'
  t killed-during-container-churn-everything-can-be-removed "clean" '( for i in $(seq 40); do $D run -d --name edge-churn-$i -v /anon alpine:3.21 sleep 300 >/dev/null 2>&1; [ $((i % 3)) = 0 ] && $D rm -f -v edge-churn-$((i - 1)) >/dev/null 2>&1; done ) & sleep 4; kill_engine; wait; back || { echo engine-did-not-return; exit; }; $D ps -a >/dev/null || { echo ps-fails; exit; }; for c in $($D ps -aq --filter name=edge-churn); do $D rm -f -v $c >/dev/null 2>&1 || echo "cannot-remove:$c"; done; [ -z "$($D ps -aq --filter name=edge-churn)" ] && echo clean'
  t a-name-used-before-the-kill-can-be-used-again "reused" '$D run --rm --name edge-churn-1 alpine:3.21 echo reused'
  t killed-while-a-volume-is-written-finished-files-are-intact "$CSUM" '$D run -d --name edge-writer -v edge-crash:/d alpine:3.21 sh -c "while :; do dd if=/dev/urandom of=/d/partial bs=1M count=50 2>/dev/null; done" >/dev/null; sleep 3; kill_engine; back || { echo engine-did-not-return; exit; }; $D rm -f edge-writer >/dev/null 2>&1; $D run --rm -v edge-crash:/d alpine:3.21 sha256sum /d/done'
  t killed-five-times-in-a-row-it-comes-back "5" 'n=0; for i in 1 2 3 4 5; do kill_engine; back && n=$((n + 1)); done; echo $n'
  t after-the-kills-a-plain-run-works "ok" '$D run --rm alpine:3.21 echo ok'
  $D rm -f -v $($D ps -aq --filter name=edge-) >/dev/null 2>&1; $D volume rm edge-crash >/dev/null 2>&1
  # A kill can land between a container's volume being made and the container being recorded:
  # that volume belongs to nothing. Only unnamed volumes that were not here when this run began.
  for v in $(comm -13 <(echo "$VOLS0") <($D volume ls -q | sort) | grep -E '^[0-9a-f]{64}$'); do $D volume rm "$v" >/dev/null 2>&1; done
}

for p in $PARTS; do
  case $p in storage|net|api|limits|crash) $p ;; *) echo "edge: no part named $p"; exit 2 ;; esac
  alive || { FAIL=$((FAIL + 1)); FAILED+=("engine-alive-after-$p"); echo "  FAIL the engine stopped answering during: $p"; break; }
done

section "afterwards"
cleanup
sleep 2
eq everything-made-here-is-gone "$BASE" 'echo "c=$($D ps -aq | wc -l | tr -d " ") v=$($D volume ls -q | wc -l | tr -d " ") n=$($D network ls -q | wc -l | tr -d " ")"'
# Not more than before: an engine may also clear away what an older version left behind.
num() { echo "$1" | sed -E "s/.*$2=([0-9]+).*/\\1/"; }
if [ -n "$LEFT0" ]; then
  LEFT1=$(left)
  if [ "$(num "$LEFT1" containers)" -le "$(num "$LEFT0" containers)" ] 2>/dev/null && [ "$(num "$LEFT1" mounts)" -le "$(num "$LEFT0" mounts)" ] 2>/dev/null; then pass
  else fail no-container-folder-or-mount-is-left "at most $LEFT0" "$LEFT1"; fi
fi

printf '\n%s passed, %s failed, %s skipped, %s known\n' "$PASS" "$FAIL" "$SKIP" "$KNOWN"
[ "$FAIL" = 0 ] || { printf 'failed: %s\n' "${FAILED[@]}"; exit 1; }
