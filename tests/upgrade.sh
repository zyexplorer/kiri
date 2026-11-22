#!/bin/bash
# The update test: what a person has on one version of Kiri must still be
# there, byte for byte, and working after the engine is replaced by another
# version. Run before every release: `populate` on the last release, switch
# the engine to the release candidate on the same data (as the updater or
# the package does), then `verify`. Switching back and running `verify`
# again covers a person who downgrades.
#
#   tests/upgrade.sh populate    # on the old engine: make images, volumes, containers
#   tests/upgrade.sh verify      # on the new engine, same data: check all of it
#   tests/upgrade.sh clean       # remove what populate made
#
# Talks to the engine the docker CLI points at (DOCKER_CONTEXT, DOCKER_HOST)
# and only touches things it names upg-* or upg/*. What populate recorded is
# kept in $GATE_STATE (default ~/.cache/kiri-upgrade-test), with the folder
# it bind-mounts: on a Mac it must be under the home folder.
#
#   GATE_SWITCHED=0   verify without an engine restart in between (a dry
#                     run): skips the checks of what comes back by itself.
#   GATE_BIG_MB=200   size of the large file in the data volume.
#   GATE_MIRROR=...   where an image comes from when KIRI_TEST_REGISTRY
#                     (tests/lib/images.sh) fails: default mirror.gcr.io/library.
#                     Never from Docker Hub, whose hourly limit runs out.
#
# Exits non-zero if any check fails.
set -u
D=${D:-docker}
STATE=${GATE_STATE:-$HOME/.cache/kiri-upgrade-test}
X=$STATE/expect
BIG=${GATE_BIG_MB:-200}
PORT=${GATE_PORT:-18431}
# shellcheck disable=SC2034 # read by lib/images.sh
KIRI_REG_FALLBACK=${GATE_MIRROR:-mirror.gcr.io/library}
# shellcheck source=SCRIPTDIR/lib/images.sh
. "$(dirname "$0")/lib/images.sh"
PASS=0; FAIL=0; SKIP=0; FAILED=()

ok() { PASS=$((PASS + 1)); }
bad() { FAIL=$((FAIL + 1)); FAILED+=("$1"); printf '  FAIL %s\n' "$1"; shift; [ $# -gt 0 ] && printf '    %s\n' "$@"; }
skip() { SKIP=$((SKIP + 1)); printf '  SKIP %s (%s)\n' "$1" "$2"; }
section() { printf '\n== %s\n' "$1"; }
# No check waits for ever: coreutils' timeout where there is one, else perl (a Mac).
if command -v timeout >/dev/null 2>&1; then limit() { timeout 120 "$@"; }
elif command -v perl >/dev/null 2>&1; then limit() { perl -e 'alarm 120; exec @ARGV' "$@"; }
else limit() { "$@"; }; fi
# same NAME EXPECTED GOT: both shown when they differ.
same() { if [ "$2" = "$3" ]; then ok; else bad "$1" "want: $(echo "$2" | head -6)" "got:  $(echo "$3" | head -6)"; fi; }
# has NAME EXPECTED COMMAND...: passes when the output contains EXPECTED.
has() {
  local name=$1 want=$2; shift 2
  local got; got=$(limit "$@" 2>&1 | tr -d '\r')
  if [[ "$got" == *"$want"* ]]; then ok; else bad "$name" "want: $want" "got:  $(echo "$got" | head -6)"; fi
}
# An image that is here already, or fetched without Docker Hub (tests/lib/images.sh).
get() { kiri_seed_images "$1"; }
# build TAG FOLDER: docker build, and the image must be there afterwards. A builder that runs in
# a container keeps its result in the build cache unless it is asked to load it.
build() {
  limit $D build -q -t "$1" "$2" >"$STATE/build.log" 2>&1
  $D image inspect "$1" >/dev/null 2>&1 || limit $D build -q --load -t "$1" "$2" >>"$STATE/build.log" 2>&1
  $D image inspect "$1" >/dev/null 2>&1
}
state() { $D inspect -f '{{.State.Status}}' "$1" 2>&1; }
wait_engine() { for _ in $(seq 240); do $D version >/dev/null 2>&1 && return 0; sleep 0.5; done; echo "the engine does not answer"; exit 1; }
wait_state() { for _ in $(seq 120); do [ "$(state "$1")" = "$2" ] && return 0; sleep 0.5; done; return 1; }
pg_ready() { for _ in $(seq 120); do $D exec upg-pg pg_isready -U postgres >/dev/null 2>&1 && return 0; sleep 0.5; done; return 1; }
pg_sum() { $D exec upg-pg psql -U postgres -Atc "select count(*) || ' ' || md5(string_agg(id || ':' || v, ',' order by id)) from upg" 2>&1; }
vol_sums() { $D run --rm --pull never -v "$1":/d:ro alpine:3.21 sh -c 'cd /d && find . -type f | sort | xargs sha256sum' 2>&1; }
images() { $D images --no-trunc --format '{{.Repository}}:{{.Tag}} {{.ID}}' | grep -E '^(alpine:3\.21|busybox:1\.36|nginx:alpine|redis:7-alpine|postgres:17-alpine|upg/)' | grep -v '^upg/tmp' | sort; }
containers() { $D ps -a --no-trunc --filter name=upg- --format '{{.Names}} {{.ID}} {{.Image}}' | grep -v '^upg-tmp' | sort; }
shape() { $D inspect -f '{{.Name}} {{.Config.Image}} restart={{.HostConfig.RestartPolicy.Name}} mounts={{range .Mounts}}{{.Type}}:{{.Name}}:{{.Destination}},{{end}} nets={{range $k, $v := .NetworkSettings.Networks}}{{$k}},{{end}}' "$@" 2>&1 | sort; }
ALL="upg-web upg-us upg-plain upg-stopped upg-created upg-health upg-userstop upg-a upg-b upg-pg upg-redis"
engine() { $D version --format '{{.Server.Version}} {{range .Server.Components}}{{.Name}}={{.Version}} {{end}}' 2>&1; }

clean() {
  $D rm -f $ALL $($D ps -aq --filter name=upg-tmp) >/dev/null 2>&1
  $D volume rm upg-data upg-pg upg-copyup upg-redis >/dev/null 2>&1
  $D network rm upg-net >/dev/null 2>&1
  $D rmi $($D images --format '{{.Repository}}:{{.Tag}}' | grep '^upg/') >/dev/null 2>&1
  rm -rf "$STATE"
}

populate() {
  wait_engine
  clean
  mkdir -p "$X" "$STATE/bind/www" "$STATE/build"
  engine > "$X/populated-by"
  echo "populating on: $(cat "$X/populated-by")"
  section "images"
  for i in $KIRI_IMAGES_UPGRADE; do
    err=$(get "$i" 2>&1) || bad "pull $i" "$(echo "$err" | tail -2 | cut -c1-200)"
  done
  # The builder's own image, for the build below (its absence is a note there, not a failure).
  get moby/buildkit:buildx-stable-1 >/dev/null 2>&1
  $D tag alpine:3.21 upg/alias:a && $D tag alpine:3.21 upg/alias:b
  head -c 262144 /dev/urandom > "$STATE/build/payload.bin"
  # FROM a registry-qualified alpine: BuildKit asks the registry for a FROM
  # image's metadata even when the engine has it, and plain alpine is Docker Hub.
  printf 'FROM %s\nRUN adduser -D app && mkdir -p /srv/app\nWORKDIR /srv/app\nCOPY payload.bin .\nRUN echo layer-two > two.txt && ln two.txt two-hard.txt && ln -s two.txt two-sym.txt\nUSER app\nCMD ["sh", "-c", "id -un; pwd; sha256sum payload.bin two-hard.txt; readlink two-sym.txt"]\n' "$KIRI_REG/alpine:3.21" > "$STATE/build/Dockerfile"
  if build upg/built:1 "$STATE/build"; then
    $D run --rm --pull never upg/built:1 > "$X/built.out" 2>&1
  else
    echo "  note: docker build is not available on this engine; the built image is left out ($(tail -1 "$STATE/build.log"))"; rm -f "$X/built.out"
  fi

  section "volumes"
  $D run --rm -v upg-data:/d alpine:3.21 sh -c "i=0; while [ \$i -lt 200 ]; do mkdir -p /d/dir\$((i % 10)); echo \"file \$i \$(head -c 12 /dev/urandom | base64)\" > /d/dir\$((i % 10))/f\$i; i=\$((i + 1)); done; dd if=/dev/urandom of=/d/big bs=1M count=$BIG 2>/dev/null; ln -s big /d/big-link; chown 1000:1000 /d/dir3; chmod 2750 /d/dir3" >/dev/null 2>&1
  vol_sums upg-data > "$X/vol-data"
  $D run --rm --pull never -v upg-data:/d:ro alpine:3.21 sh -c 'stat -c "%u:%g %a" /d/dir3; readlink /d/big-link' > "$X/vol-data-meta" 2>&1
  $D run --rm -v upg-copyup:/usr/share/nginx/html nginx:alpine true >/dev/null 2>&1
  vol_sums upg-copyup > "$X/vol-copyup"

  section "containers"
  echo "written on the host" > "$STATE/bind/host.txt"
  echo "upgrade test page $(head -c 9 /dev/urandom | base64 | tr -d /+)" > "$STATE/bind/www/index.html"
  $D network create upg-net >/dev/null
  $D run -d --name upg-web --restart always -p 127.0.0.1:$PORT:80 -v "$STATE/bind/www":/usr/share/nginx/html:ro nginx:alpine >/dev/null
  $D run -d --name upg-us --restart unless-stopped alpine:3.21 sleep 1000000 >/dev/null
  $D run -d --name upg-plain alpine:3.21 sleep 1000000 >/dev/null
  $D run -d --name upg-health --restart always --health-cmd 'test -e /tmp' --health-interval 2s alpine:3.21 sleep 1000000 >/dev/null
  $D run -d --name upg-userstop --restart unless-stopped alpine:3.21 sleep 1000000 >/dev/null && $D stop -t 1 upg-userstop >/dev/null
  $D run -d --name upg-a --restart always --network upg-net alpine:3.21 sleep 1000000 >/dev/null
  $D run -d --name upg-b --restart always --network upg-net --network-alias other alpine:3.21 sleep 1000000 >/dev/null
  $D run -d --name upg-redis --restart always --network upg-net -v upg-redis:/data redis:7-alpine redis-server --appendonly yes >/dev/null
  $D run -d --name upg-pg --restart unless-stopped -e POSTGRES_PASSWORD=upg -v upg-pg:/var/lib/postgresql/data postgres:17-alpine >/dev/null
  # Changes in a container's own layer: a new file, a changed one, a removed one.
  $D run --name upg-stopped -v "$STATE/bind":/b alpine:3.21 sh -c 'test -f /data.txt || { head -c 4096 /dev/urandom | base64 > /data.txt; echo changed >> /etc/motd; rm -f /etc/issue; echo "written in the container" > /b/ctr.txt; }; sha256sum /data.txt' > "$X/stopped.out" 2>&1
  $D create --name upg-created alpine:3.21 echo created-ok >/dev/null
  $D commit upg-stopped upg/committed:1 >/dev/null
  pg_ready || bad "postgres starts" "$($D logs --tail 5 upg-pg 2>&1)"
  sleep 1; pg_ready
  $D exec upg-pg psql -U postgres -qc "create table upg (id int primary key, v text); insert into upg select i, md5(i::text || random()::text) from generate_series(1, 10000) i; checkpoint" >/dev/null 2>&1
  pg_sum > "$X/pg"
  for i in $(seq 60); do $D exec upg-redis redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 0.5; done
  $D exec upg-redis sh -c 'i=0; while [ $i -lt 500 ]; do redis-cli set k$i v$i-$RANDOM >/dev/null; i=$((i+1)); done; redis-cli --raw keys "k*" | sort | while read k; do echo "$k $(redis-cli get $k)"; done | sha256sum' > "$X/redis" 2>&1

  section "what is here"
  images > "$X/images"; containers > "$X/containers"; shape $ALL > "$X/shape"
  $D volume ls --format '{{.Name}}' | grep '^upg-' | sort > "$X/volumes"
  $D network inspect upg-net --format '{{.Id}} {{range .IPAM.Config}}{{.Subnet}}{{end}}' > "$X/network"
  $D diff upg-stopped | sort > "$X/diff"
  $D logs upg-stopped > "$X/logs" 2>&1
  $D cp upg-stopped:/data.txt - | tar -xO | sha256sum > "$X/cp"
  cat "$STATE/bind/www/index.html" > "$X/page"
  $D system df > "$X/df" 2>&1
  # (An engine may list more, such as the folder it made for the bind mount.)
  same "the stopped container's changes are recorded" "A /data.txt
C /etc/motd
D /etc/issue" "$(grep -xE 'A /data.txt|C /etc/motd|D /etc/issue' "$X/diff")"
  has "the page is served" "$(cat "$X/page")" curl -s -m 10 "http://127.0.0.1:$PORT/"
  [ "$(wc -l < "$X/vol-data" | tr -d ' ')" = 201 ] && ok || bad "the data volume has its 201 files" "$(wc -l < "$X/vol-data")"
  grep -q '^10000 ' "$X/pg" && ok || bad "postgres has its 10000 rows" "$(cat "$X/pg")"
  printf '\nimages %s, containers %s, volumes %s\n' "$(wc -l < "$X/images" | tr -d ' ')" "$(wc -l < "$X/containers" | tr -d ' ')" "$(wc -l < "$X/volumes" | tr -d ' ')"
}

verify() {
  [ -d "$X" ] || { echo "nothing recorded in $STATE: run populate on the old engine first"; exit 1; }
  local t0=$SECONDS; wait_engine
  echo "populated on: $(cat "$X/populated-by")"
  echo "verifying on: $(engine)   (answered after $((SECONDS - t0)) s)"
  local switched=${GATE_SWITCHED:-1}

  section "images are all still here, with the same IDs"
  same "image list" "$(cat "$X/images")" "$(images)"
  has "alpine runs without a pull" "ok" $D run --rm --pull never alpine:3.21 echo ok
  has "busybox runs without a pull" "ok" $D run --rm --pull never busybox:1.36 echo ok
  has "both tags of one image run" "a b" sh -c "echo \$($D run --rm --pull never upg/alias:a echo a) \$($D run --rm --pull never upg/alias:b echo b)"
  if [ -f "$X/built.out" ]; then same "the built image runs as before (user, folder, files, links)" "$(cat "$X/built.out")" "$($D run --rm --pull never upg/built:1 2>&1)"; else skip "built image" "no docker build on the old engine"; fi
  same "the committed image has the container's file" "$(cut -d' ' -f1 "$X/stopped.out")" "$($D run --rm --pull never upg/committed:1 sha256sum /data.txt 2>&1 | cut -d' ' -f1)"

  section "what comes back by itself"
  if [ "$switched" = 1 ]; then
    for c in upg-web upg-us upg-health upg-a upg-b upg-redis upg-pg; do
      wait_state $c running && ok || bad "$c is running again (it restarts by itself)" "state: $(state $c)" "$($D inspect -f '{{.State.Error}} exit={{.State.ExitCode}}' $c 2>&1)"
    done
    sleep 2
    same "a container with no restart policy stays stopped" "exited" "$(state upg-plain)"
    same "an unless-stopped container the person stopped stays stopped" "exited" "$(state upg-userstop)"
  else
    skip "restart policies" "GATE_SWITCHED=0"
  fi
  same "containers: same names, IDs and images" "$(cat "$X/containers")" "$(containers)"
  same "containers: same image, restart policy, mounts and networks" "$(cat "$X/shape")" "$(shape $ALL)"
  has "the published port serves the same page" "$(cat "$X/page")" sh -c "for i in \$(seq 30); do curl -s -m 5 http://127.0.0.1:$PORT/ && break; sleep 1; done"
  for _ in $(seq 30); do [ "$($D inspect -f '{{.State.Health.Status}}' upg-health 2>/dev/null)" = healthy ] && break; sleep 1; done
  same "the healthcheck runs again" "healthy" "$($D inspect -f '{{.State.Health.Status}}' upg-health 2>&1)"

  section "volumes: every byte"
  same "volume list" "$(cat "$X/volumes")" "$($D volume ls --format '{{.Name}}' | grep '^upg-' | sort)"
  same "data volume: 200 files and the large one" "$(cat "$X/vol-data")" "$(vol_sums upg-data)"
  same "data volume: owner, mode and link kept" "$(cat "$X/vol-data-meta")" "$($D run --rm --pull never -v upg-data:/d:ro alpine:3.21 sh -c 'stat -c "%u:%g %a" /d/dir3; readlink /d/big-link' 2>&1)"
  same "volume filled from an image folder" "$(cat "$X/vol-copyup")" "$(vol_sums upg-copyup)"
  pg_ready && ok || bad "postgres accepts connections on its old volume" "$($D logs --tail 8 upg-pg 2>&1)"
  same "postgres: all 10000 rows, unchanged" "$(cat "$X/pg")" "$(pg_sum)"
  for _ in $(seq 60); do $D exec upg-redis redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 0.5; done
  same "redis: all 500 keys, unchanged" "$(cat "$X/redis")" "$($D exec upg-redis sh -c 'redis-cli --raw keys "k*" | sort | while read k; do echo "$k $(redis-cli get $k)"; done | sha256sum' 2>&1)"

  section "a stopped container's own files"
  same "docker diff is unchanged" "$(cat "$X/diff")" "$($D diff upg-stopped 2>&1 | sort)"
  same "its logs are kept" "$(cat "$X/logs")" "$($D logs upg-stopped 2>&1 | head -1)"
  same "docker cp out gives the same bytes" "$(cat "$X/cp")" "$($D cp upg-stopped:/data.txt - 2>/dev/null | tar -xO | sha256sum)"
  same "it starts again and still has its file" "$(cat "$X/stopped.out")" "$($D start -a upg-stopped 2>&1 | tail -1)"
  same "docker diff is unchanged after that start" "$(cat "$X/diff")" "$($D diff upg-stopped 2>&1 | sort)"
  has "a container created and never started starts" "created-ok" $D start -a upg-created
  $D rmi upg/tmp:commit >/dev/null 2>&1
  has "a commit of the old container has its file" "$(cut -d' ' -f1 "$X/stopped.out")" sh -c "$D commit upg-stopped upg/tmp:commit >/dev/null && $D run --rm --pull never upg/tmp:commit sha256sum /data.txt; $D rmi upg/tmp:commit >/dev/null"

  section "networks and the bind mount"
  same "the network is the same one" "$(cat "$X/network")" "$($D network inspect upg-net --format '{{.Id}} {{range .IPAM.Config}}{{.Subnet}}{{end}}' 2>&1)"
  has "containers find each other by name" "1 packets received" $D exec upg-a ping -c 1 -W 3 upg-b
  has "and by alias" "1 packets received" $D exec upg-a ping -c 1 -W 3 other
  has "redis answers over the network" "PONG" $D exec upg-a sh -c 'printf "PING\r\n" | nc -w 3 upg-redis 6379'
  has "a container reaches the internet" "Example Domain" sh -c "for i in 1 2 3; do $D exec upg-a wget -qO- -T 10 http://example.com && break; sleep 2; done"
  same "bind mount: the host's file" "written on the host" "$(cat "$STATE/bind/host.txt" 2>&1)"
  same "bind mount: the container's file" "written in the container" "$(cat "$STATE/bind/ctr.txt" 2>&1)"
  has "bind mount: read from a new container" "written on the host" $D run --rm --pull never -v "$STATE/bind":/b:ro alpine:3.21 cat /b/host.txt

  section "new work on the old data"
  $D rm -f $($D ps -aq --filter name=upg-tmp) >/dev/null 2>&1
  has "exec in a container that was running before" "upg-us" $D exec upg-us sh -c 'hostname >/dev/null; echo upg-us'
  has "a new container reads the old volume" "$(head -1 "$X/vol-data" | cut -d' ' -f1)" sh -c "$D run --rm --pull never --name upg-tmp-read -v upg-data:/d:ro alpine:3.21 sha256sum /d/$(head -1 "$X/vol-data" | awk '{print $2}' | sed 's|^\./||')"
  has "a new container writes the old volume and the data stays" "kept" sh -c "$D run --rm --pull never --name upg-tmp-w -v upg-copyup:/d alpine:3.21 sh -c 'echo kept > /d/upg-tmp.txt' && $D run --rm --pull never -v upg-copyup:/d alpine:3.21 sh -c 'cat /d/upg-tmp.txt; rm /d/upg-tmp.txt'"
  $D rmi alpine:3.20 >/dev/null 2>&1; get alpine:3.20
  has "a new image can be pulled" "3.20" $D run --rm --pull never alpine:3.20 cat /etc/alpine-release
  if [ -f "$X/built.out" ]; then
    if build upg/tmp:built "$STATE/build"; then ok; else bad "the image builds again" "$(tail -3 "$STATE/build.log" | cut -c1-200)"; fi
    $D rmi upg/tmp:built >/dev/null 2>&1
  fi
  has "a tag can be added and removed without losing the image" "ok" sh -c "$D tag alpine:3.21 upg/tmp:t && $D rmi upg/tmp:t >/dev/null && $D run --rm --pull never alpine:3.21 echo ok"
  has "a new container can be made and removed" "gone" sh -c "$D run -d --name upg-tmp-c alpine:3.21 sleep 60 >/dev/null && $D rm -f upg-tmp-c >/dev/null && { $D inspect upg-tmp-c >/dev/null 2>&1 && echo still || echo gone; }"
  same "after all that, the images are still the same" "$(cat "$X/images")" "$(images)"
  same "and the containers" "$(cat "$X/containers")" "$(containers)"
  # For the next verify (another engine): as populate left them.
  $D stop -t 1 upg-stopped upg-created >/dev/null 2>&1
  if [ "$switched" = 1 ]; then $D start upg-plain >/dev/null 2>&1; fi

  printf '\ndocker system df, then and now:\n'; paste -d'|' "$X/df" <($D system df 2>&1) | sed 's/|/   |   /' | head -8
}

case "${1:-}" in
  populate) populate ;;
  verify) verify ;;
  clean) clean; echo cleaned; exit 0 ;;
  *) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
printf '\n%s passed, %s failed, %s skipped\n' "$PASS" "$FAIL" "$SKIP"
[ "$FAIL" = 0 ] || { printf 'failed: %s\n' "${FAILED[@]}"; exit 1; }
