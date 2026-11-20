#!/bin/bash
# `kiri import` against a real source engine. Usage:
#
#   tests/import.sh orbstack          # or desktop-linux: the source's docker context
#   KIRI=/path/to/kiri DOCKER_CONTEXT=kiri-builds tests/import.sh orbstack
#
# Each section sets up one reported case on the source, imports it into the
# Kiri under test (context $DOCKER_CONTEXT, default kiri) and checks what Kiri
# has. Everything it makes is named imp-* (and the anonymous volumes of its
# own containers) on both engines, and only those are removed again, with
# the images it or the imports brought; both engines are left as found.
# The volume cases need about 14 GB free in Kiri for an 8 GiB file; with
# less they are skipped and said so. Exits non-zero if a check fails.

set -u
SRC=${1:?usage: tests/import.sh orbstack|desktop-linux}
export DOCKER_CONTEXT=${DOCKER_CONTEXT:-kiri}
KIRI=${KIRI:-kiri}
S=(docker --context "$SRC")
K=(docker --context "$DOCKER_CONTEXT")
# An image with a shell, for writing test files on the source. Not one of
# the images the volume copy looks for, so the source needs none of those.
TOOL=${TOOL:-redis:7-alpine}
# The source's image for the digest-pinned container (#377).
PINNED_REPO=mirror.gcr.io/library/hello-world
PASS=0
FAIL=0
FAILED=()

# t NAME EXPECTED COMMAND... : passes when the command's output contains EXPECTED.
t() {
  local name=$1 want=$2
  shift 2
  local got
  got=$(perl -e 'alarm 300; exec @ARGV' "$@" 2>&1 | tr -d '\r')
  if [[ "$got" == *"$want"* ]]; then
    PASS=$((PASS + 1))
    printf '  ok   %s\n' "$name"
  else
    FAIL=$((FAIL + 1))
    FAILED+=("$name")
    printf '  FAIL %s\n    want: %s\n    got:  %s\n' "$name" "$want" "$(echo "$got" | head -12)"
  fi
}
section() { printf '\n== %s\n' "$1"; }
note() { printf '  note %s\n' "$1"; }

# Images fetched here are removed at the end; the mirror, not Docker Hub.
FETCHED_SRC=()
FETCHED_KIRI=()
fetch() { # fetch CONTEXT IMAGE
  local e=(docker --context "$1")
  local m="mirror.gcr.io/library/$2" had=
  "${e[@]}" image inspect "$2" >/dev/null 2>&1 && return 0
  "${e[@]}" image inspect "$m" >/dev/null 2>&1 && had=1
  "${e[@]}" pull -q "$m" >/dev/null && "${e[@]}" tag "$m" "$2" || return 1
  [ -n "$had" ] || "${e[@]}" rmi "$m" >/dev/null
  if [ "$1" = "$SRC" ]; then FETCHED_SRC+=("$2"); else FETCHED_KIRI+=("$2"); fi
}

ANON=()
PINNED=
PINNED_OURS=
PINNED_KIRI_OURS=
# What Kiri has before: read once both engines answer. Until then the
# cleanup removes no image from Kiri.
KIRI_HAD=
# A Kiri container of ours goes with the 64-hex volumes the import gave it.
kiri_rm() {
  local c v
  for c in "$@"; do
    v=$("${K[@]}" inspect -f '{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}} {{end}}{{end}}' "$c" 2>/dev/null)
    "${K[@]}" rm -fv "$c" >/dev/null 2>&1
    for v in $v; do [[ "$v" =~ ^[0-9a-f]{64}$ ]] && "${K[@]}" volume rm -f "$v" >/dev/null 2>&1; done
  done
}
OURS_C="imp-digest imp-net-joiner imp-net-target imp-vol-user imp-vol-writer imp-vol-idle imp-vol-cutter imp-vol-kiri-user imp-vol-kiri-paused imp-vol-kiri-again imp-vol-kiri-stopped"
# An image named like an official one, with a creation date ahead: its
# sh, tar, rm and find leave a mark and refuse the work. Never to be run.
LOOKALIKE=localhost/imp-x/library/busybox:9
UNTAGGED_ID=
LA_DIR=
OURS_V="imp-vol-la imp-vol-la-new imp-vol-data imp-vol-named imp-vol-keep imp-vol-busy imp-vol-huge imp-vol-cut imp-probe"
cleanup() {
  kiri_rm $OURS_C
  "${S[@]}" rm -fv $OURS_C >/dev/null 2>&1
  # An import's reader left by a test that was stopped: only with our label.
  for r in $("${S[@]}" ps -aq --filter label=dev.kiri.import-helper=1 --filter name=kiri-import-imp-); do "${S[@]}" rm -fv "$r" >/dev/null 2>&1; done
  for ctx in "$SRC" "$DOCKER_CONTEXT"; do
    docker --context "$ctx" volume rm -f $OURS_V ${ANON[@]+"${ANON[@]}"} >/dev/null 2>&1
  done
  [ -n "$PINNED_KIRI_OURS" ] && "${K[@]}" rmi "$PINNED" >/dev/null 2>&1
  "${S[@]}" rm -fv imp-la-base >/dev/null 2>&1
  for ctx in "$SRC" "$DOCKER_CONTEXT"; do docker --context "$ctx" rmi "$LOOKALIKE" >/dev/null 2>&1; done
  [ -n "$UNTAGGED_ID" ] && "${K[@]}" rmi "$UNTAGGED_ID" >/dev/null 2>&1
  [ -n "$LA_DIR" ] && rm -rf "$LA_DIR"
  rm -f /tmp/imp-cut-seen.$$
  [ -n "$PINNED_OURS" ] && "${S[@]}" rmi "$PINNED" >/dev/null 2>&1
  for i in ${FETCHED_SRC[@]+"${FETCHED_SRC[@]}"}; do "${S[@]}" rmi "$i" >/dev/null 2>&1; done
  for i in ${FETCHED_KIRI[@]+"${FETCHED_KIRI[@]}"}; do "${K[@]}" rmi "$i" >/dev/null 2>&1; done
  # What the imports brought into Kiri: the test image, helper images.
  [ -n "$KIRI_HAD" ] || return 0
  for i in $("${K[@]}" image ls --format '{{.Repository}}:{{.Tag}}' 2>/dev/null); do
    [[ "$KIRI_HAD" == *" $i "* ]] && continue
    [[ "$i" == "$TOOL" || "$i" =~ ^(alpine|busybox|debian|ubuntu): ]] && "${K[@]}" rmi "$i" >/dev/null 2>&1
  done
}
cleanup
trap cleanup EXIT

"${S[@]}" version >/dev/null 2>&1 || { echo "import: the source $SRC does not answer"; exit 1; }
"${K[@]}" version >/dev/null 2>&1 || { echo "import: Kiri ($DOCKER_CONTEXT) does not answer"; exit 1; }
had=$("${K[@]}" image ls --format '{{.Repository}}:{{.Tag}}') || { echo "import: could not list Kiri's images"; exit 1; }
KIRI_HAD=" $(echo "$had" | tr '\n' ' ') "
# The kiri under test must be the Kiri under test: else imports would land
# in another one, perhaps a person's installed Kiri.
"${K[@]}" volume create imp-probe >/dev/null
if ! "$KIRI" volume ls -q 2>/dev/null | grep -qx imp-probe; then
  echo "import: $KIRI does not talk to the engine of context $DOCKER_CONTEXT; set KIRI to its kiri"; exit 1
fi
"${K[@]}" volume rm imp-probe >/dev/null
fetch "$SRC" "$TOOL" || { echo "import: could not get $TOOL on $SRC"; exit 1; }
# Kiri's half of a volume copy runs only an image Kiri pulled from an
# official repository: the mirror's alpine is one, kept while this runs.
if ! "${K[@]}" image inspect mirror.gcr.io/library/alpine >/dev/null 2>&1; then
  "${K[@]}" pull -q mirror.gcr.io/library/alpine >/dev/null || { echo "import: could not get alpine on Kiri"; exit 1; }
  FETCHED_KIRI+=(mirror.gcr.io/library/alpine)
fi
fetch "$DOCKER_CONTEXT" alpine || { echo "import: could not get alpine on Kiri"; exit 1; }
src_sh() { "${S[@]}" run --rm --network none --entrypoint sh "$@"; }
# A volume's files as text: name, owner, mode, type, and for what is not a
# directory its link count and size; a file's mtime and checksum, a link's
# target. (Directory link counts and symlink times differ between file
# systems and tar programs, not between copies.)
LIST='cd /d && find . | sort | while read -r f; do printf "%s" "$(stat -c "%n %u:%g %a %F" "$f")"; [ -d "$f" ] || printf " %s" "$(stat -c "%h %s" "$f")"; if [ -L "$f" ]; then printf " -> %s" "$(readlink "$f")"; elif [ -f "$f" ]; then printf " %s %s" "$(stat -c %Y "$f")" "$(md5sum < "$f" | cut -c1-32)"; fi; echo; done'

section "import helper: an image only named like an official one is never run"
# The look-alike, made from the source's test image: its files, then a layer
# whose sh, tar, rm and find mark any folder they are given and fail.
LA_DIR=$(mktemp -d /tmp/imp-la.XXXXXX)
sha() { { sha256sum "$1" 2>/dev/null || shasum -a 256 "$1"; } | command cut -c1-64; }
"${S[@]}" create --name imp-la-base "$TOOL" >/dev/null && "${S[@]}" export imp-la-base > "$LA_DIR/base.tar" && "${S[@]}" rm -v imp-la-base >/dev/null
mkdir -p "$LA_DIR/top/bin" "$LA_DIR/top/usr/bin"
printf '#!/bin/busybox sh\nfor d in /to /d /v/* /kiri-import*; do [ -d "$d" ] && /bin/busybox touch "$d/look-alike-ran" 2>/dev/null; done\necho "the look-alike ran" >&2\nexit 3\n' > "$LA_DIR/top/bin/sh"
chmod 755 "$LA_DIR/top/bin/sh"
for f in bin/tar bin/rm usr/bin/find; do cp "$LA_DIR/top/bin/sh" "$LA_DIR/top/$f"; done
(cd "$LA_DIR/top" && COPYFILE_DISABLE=1 tar -cf ../top.tar bin usr)
arch=$("${S[@]}" image inspect -f '{{.Architecture}}' "$TOOL")
printf '{"architecture":"%s","os":"linux","created":"2099-01-01T00:00:00Z","config":{"Entrypoint":["/bin/sh"],"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"]},"rootfs":{"type":"layers","diff_ids":["sha256:%s","sha256:%s"]}}' \
  "$arch" "$(sha "$LA_DIR/base.tar")" "$(sha "$LA_DIR/top.tar")" > "$LA_DIR/config.json"
printf '[{"Config":"config.json","RepoTags":["%s"],"Layers":["base.tar","top.tar"]}]' "$LOOKALIKE" > "$LA_DIR/manifest.json"
(cd "$LA_DIR" && COPYFILE_DISABLE=1 tar -cf look-alike.tar manifest.json config.json base.tar top.tar)
"${K[@]}" load -i "$LA_DIR/look-alike.tar" >/dev/null && "${S[@]}" load -i "$LA_DIR/look-alike.tar" >/dev/null
t la-in-kiri "2099" "${K[@]}" image inspect -f '{{.Created}}' "$LOOKALIKE"
t la-on-source "2099" "${S[@]}" image inspect -f '{{.Created}}' "$LOOKALIKE"
# An image with no name at all in Kiri (as a build without -t leaves one).
sed 's/2099-01-01/2098-01-01/' "$LA_DIR/config.json" > "$LA_DIR/untagged.json"
printf '[{"Config":"untagged.json","RepoTags":null,"Layers":["base.tar","top.tar"]}]' > "$LA_DIR/manifest.json"
(cd "$LA_DIR" && COPYFILE_DISABLE=1 tar -cf untagged.tar manifest.json untagged.json base.tar top.tar)
"${K[@]}" load -i "$LA_DIR/untagged.tar" >/dev/null && UNTAGGED_ID=sha256:$(sha "$LA_DIR/untagged.json")
t la-untagged-in-kiri "2098" "${K[@]}" image inspect -f '{{.Created}}' "${UNTAGGED_ID:-none}"
# The names in Kiri whose own record was pulled from an official repository.
official_names() {
  local n
  for n in $("${K[@]}" image ls --format '{{.Repository}}:{{.Tag}}'); do
    grep -qxE '((docker\.io/)?(library/)?|mirror\.gcr\.io/library/|public\.ecr\.aws/docker/library/)(alpine|busybox|debian|ubuntu)' <<<"${n%:*}" &&
      "${K[@]}" image inspect -f '{{json .RepoDigests}}' "$n" 2>/dev/null | grep -qF "\"${n%:*}@sha256:" && echo "$n"
  done
}
# helpers SINCE: the image each import helper Kiri made since then was given.
helpers() { "${K[@]}" events --since "$1" --until "$(($(date +%s) + 1))" --filter event=create --filter label=dev.kiri.import-helper=1 --format '{{index .Actor.Attributes "image"}}' 2>&1; }
# by_name SINCE: each helper since then was given an official image's name.
by_name() {
  local from names f
  from=$(helpers "$1"); names=$(official_names)
  [ -n "$from" ] || { echo "no helper was made"; return; }
  for f in $from; do grep -qxF "$f" <<<"$names" || { echo "made from $f (official: $(echo $names))"; return; }; done
  echo "all by the name of an official image"
}
"${S[@]}" volume create imp-vol-la >/dev/null
src_sh -v imp-vol-la:/d "$TOOL" -c 'echo real > /d/f && mkdir /d/sub && echo deep > /d/sub/g && chown -R 1234:5678 /d/sub'
# Shared by both engines, empty in Kiri: the list looks into it.
"${K[@]}" volume create imp-vol-la >/dev/null
since=$(date +%s); sleep 1
t la-list-works "imp-vol-la" "$KIRI" import "$SRC" --list
res=$(by_name "$since")
t la-list-ran-no-look-alike "all by the name of an official image" echo "$res"
since=$(date +%s); sleep 1
t la-volume-imports "volume imp-vol-la: done" "$KIRI" import "$SRC" volume imp-vol-la
t la-volume-exact "real deep 1234:5678" "${K[@]}" run --rm --network none -v imp-vol-la:/d:ro alpine sh -c 'echo "$(cat /d/f) $(cat /d/sub/g) $(stat -c %u:%g /d/sub/g)"'
t la-volume-no-mark "no mark" "${K[@]}" run --rm --network none -v imp-vol-la:/d:ro alpine sh -c 'find /d -name look-alike-ran | grep -q . && echo marked || echo "no mark"'
res=$(by_name "$since")
t la-volume-ran-no-look-alike "all by the name of an official image" echo "$res"
# Not in Kiri yet: the first helper to run is the one that writes it.
"${S[@]}" volume create imp-vol-la-new >/dev/null
src_sh -v imp-vol-la-new:/d "$TOOL" -c 'echo new > /d/f'
t la-new-volume-imports "volume imp-vol-la-new: done" "$KIRI" import "$SRC" volume imp-vol-la-new
t la-new-volume-no-mark "new no mark" "${K[@]}" run --rm --network none -v imp-vol-la-new:/d:ro alpine sh -c 'echo "$(cat /d/f 2>/dev/null) $(find /d -name look-alike-ran | grep -q . && echo marked || echo "no mark")"'

section "#376 volumes need none of five particular images on the source"
helpers=$(for i in alpine:latest busybox:latest alpine:3.22 debian:trixie ubuntu:24.04; do "${S[@]}" image inspect "$i" >/dev/null 2>&1 && echo "$i"; done | tr '\n' ' ')
[ -n "$helpers" ] && note "$SRC has $helpers- the case of a source without them is not shown here"
"${S[@]}" volume create imp-vol-data >/dev/null
src_sh -v imp-vol-data:/d "$TOOL" -c '
  cd /d && mkdir -p sub/deep owned sticky && echo hi > sub/deep/file && echo secret > owned/key
  chown 1234:5678 owned/key && chmod 640 owned/key && chown 1:1 owned && chmod 2755 owned
  chmod 1777 sticky && echo x > suid && chmod 4755 suid && echo y > sgid && chmod 2711 sgid
  ln -s sub/deep/file link && ln -s /nowhere dangling && ln sub/deep/file hard && mkfifo fifo
  : > empty && echo s > "with space" && head -c 5000000 /dev/urandom > big
  echo far > far && chown 3000000:3000000 far
  chown 70:70 /d && chmod 750 /d'
t vol-no-helper-imports "volume imp-vol-data: done" "$KIRI" import "$SRC" volume imp-vol-data
want=$(src_sh -v imp-vol-data:/d:ro "$TOOL" -c "$LIST")
got=$("${K[@]}" run --rm --network none -v imp-vol-data:/d:ro alpine sh -c "$LIST" 2>&1)
t vol-exact-copy "same files" bash -c '[ "$1" = "$2" ] && echo "same files" || diff <(echo "$1") <(echo "$2")' _ "$want" "$got"
t vol-copy-has-files "hard" echo "$got"

section "#376 an empty volume in Kiri is not \"already in Kiri\"; a container brings its volumes"
"${S[@]}" volume create imp-vol-named >/dev/null
"${S[@]}" run --name imp-vol-user --network none --entrypoint sh -v imp-vol-named:/named -v /anon "$TOOL" -c 'echo named > /named/f; echo anon > /anon/f'
ANON=($("${S[@]}" inspect imp-vol-user --format '{{range .Mounts}}{{if eq .Destination "/anon"}}{{.Name}}{{end}}{{end}}'))
# What a container's create or a failed copy leaves: the volume, empty.
"${K[@]}" volume create imp-vol-named >/dev/null
t vol-empty-offered-again "offered" bash -c '"$1" import "$2" --list | grep -q "imp-vol-named  (already in Kiri)" && echo "listed as already in Kiri" || echo offered' _ "$KIRI" "$SRC"
t vol-user-imports "container imp-vol-user: done" "$KIRI" import "$SRC" container imp-vol-user
note "$TOOL is $("${S[@]}" image inspect -f '{{.Id}}' "$TOOL" | cut -c1-19) on $SRC, $("${K[@]}" image inspect -f '{{.Id}}' "$TOOL" 2>/dev/null | cut -c1-19) in Kiri"
t vol-named-has-data "named" "${K[@]}" run --rm --network none -v imp-vol-named:/n alpine cat /n/f
t vol-anon-has-data "anon" "${K[@]}" run --rm --network none -v "${ANON[0]:-none}:/a" alpine cat /a/f
t vol-anon-same-name "${ANON[0]:-none}" "${K[@]}" inspect imp-vol-user --format '{{range .Mounts}}{{.Name}} {{end}}'
t vol-listed-once-filled "imp-vol-named  (already in Kiri)" "$KIRI" import "$SRC" --list
# A Kiri volume with files is never written over.
"${S[@]}" volume create imp-vol-keep >/dev/null
src_sh -v imp-vol-keep:/d "$TOOL" -c 'echo source > /d/f'
"${K[@]}" volume create imp-vol-keep >/dev/null
"${K[@]}" run --rm --network none -v imp-vol-keep:/d alpine sh -c 'echo kiri > /d/f'
t vol-keep-refused "left as it is" "$KIRI" import "$SRC" volume imp-vol-keep
t vol-keep-untouched "kiri" "${K[@]}" run --rm --network none -v imp-vol-keep:/d alpine cat /d/f

section "#376 files of 8 GiB and more, a copy cut half-way"
free_kiri=$("${K[@]}" run --rm --network none alpine df -k / | awk 'NR==2 {print $4}')
free_src=$(src_sh "$TOOL" -c "df -k / | awk 'NR==2 {print \$4}'")
note "free: Kiri $((free_kiri / 1048576)) GB, $SRC $((free_src / 1048576)) GB"
if [ "${free_kiri:-0}" -lt 14000000 ] || [ "${free_src:-0}" -lt 2000000 ]; then
  note "not enough free disk for these cases here: skipped"
else
  # Mostly zeros (sparse on the source), a mark at the end, a file after.
  "${S[@]}" volume create imp-vol-huge >/dev/null
  src_sh -v imp-vol-huge:/d "$TOOL" -c 'cd /d && truncate -s 8G a-huge && printf END >> a-huge && echo after > b-after'
  t huge-imports "volume imp-vol-huge: done" "$KIRI" import "$SRC" volume imp-vol-huge
  t huge-exact "8589934595 END after" "${K[@]}" run --rm --network none -v imp-vol-huge:/d:ro alpine sh -c 'echo "$(stat -c %s /d/a-huge) $(tail -c 3 /d/a-huge) $(cat /d/b-after)"'
  "${K[@]}" volume rm imp-vol-huge >/dev/null 2>&1; "${S[@]}" volume rm imp-vol-huge >/dev/null 2>&1
  # cut CMD...: runs an import, and stops the source's side of its copy
  # once it is under way.
  cut() {
    "$@" > /tmp/imp-cut.$$ 2>&1 &
    local pid=$! i
    for i in $(seq 300); do pgrep -f "kiri-import-imp-vol-cut:|imp-vol-cut:/from" >/dev/null && break; sleep 0.1; done
    sleep 1
    # Kiri's helpers while the copy runs: what each was given and runs.
    for c in $("${K[@]}" ps -q --filter label=dev.kiri.import-helper=1); do
      "${K[@]}" inspect -f '{{.Config.Image}} {{.Image}} {{json .Config.Entrypoint}}' "$c"
    done > /tmp/imp-cut-seen.$$ 2>&1
    pkill -f "kiri-import-imp-vol-cut:|imp-vol-cut:/from"
    wait $pid
    cat /tmp/imp-cut.$$; rm -f /tmp/imp-cut.$$
  }
  "${S[@]}" volume create imp-vol-cut >/dev/null
  src_sh -v imp-vol-cut:/d "$TOOL" -c 'cd /d && truncate -s 4G a-big && echo after > b-after'
  "${S[@]}" create --name imp-vol-cutter --network none -v imp-vol-cut:/d "$TOOL" >/dev/null
  out=$(cut "$KIRI" import "$SRC" volume imp-vol-cut)
  t cut-said "unfinished in Kiri" echo "$out"
  seen=$(cat /tmp/imp-cut-seen.$$ 2>/dev/null); rm -f /tmp/imp-cut-seen.$$
  t cut-helper-by-name-with-entrypoint "by name, entrypoint sh" bash -c 'read -r given ran ep <<<"$1"; grep -qxF "$given" <<<"$2" && [ "$ep" = "[\"sh\"]" ] && echo "by name, entrypoint sh" || echo "seen: $1 (official: $2)"' _ "$seen" "$(official_names)"
  t cut-marked "kiri-import-unfinished" "${K[@]}" run --rm --network none -v imp-vol-cut:/d:ro alpine ls -A /d
  t cut-offered-again "offered" bash -c '"$1" import "$2" --list | grep -q "imp-vol-cut  (already in Kiri)" && echo "listed as already in Kiri" || echo offered' _ "$KIRI" "$SRC"
  # Used in Kiri since, by a container in any state: what it wrote there
  # is not emptied away, not even with --force (that is about the source).
  # "${kept[@]}" FILE: the volume still has FILE, and is still unfinished.
  kept=("${K[@]}" run --rm --network none -v imp-vol-cut:/d:ro alpine sh -c 'echo "$(cat "/d/$0") $(ls -A /d | grep -c kiri-import-unfinished)"')
  "${K[@]}" run -d --name imp-vol-kiri-paused --network none -v imp-vol-cut:/d alpine sh -c 'echo paused > /d/paused; sleep 1d' >/dev/null
  sleep 1; "${K[@]}" pause imp-vol-kiri-paused >/dev/null
  t cut-paused-in-kiri-refused "Kiri has imp-vol-kiri-paused using it" "$KIRI" import "$SRC" volume imp-vol-cut
  t cut-paused-kept "paused 1" "${kept[@]}" paused
  "${K[@]}" unpause imp-vol-kiri-paused >/dev/null 2>&1; kiri_rm imp-vol-kiri-paused
  # Exits at once, so it spends its time waiting to be started again.
  "${K[@]}" run -d --name imp-vol-kiri-again --restart always --network none -v imp-vol-cut:/d alpine sh -c 'if [ -e /d/own ]; then echo found; else echo own > /d/own; echo made; fi' >/dev/null
  sleep 3
  t cut-restarting-in-kiri-refused "Kiri has imp-vol-kiri-again using it" "$KIRI" import "$SRC" volume imp-vol-cut
  t cut-restarting-kept "own 1" "${kept[@]}" own
  t cut-restarting-finds-its-file "found" bash -c 'n=$("$@" logs imp-vol-kiri-again 2>&1 | wc -l); for i in $(seq 90); do [ "$("$@" logs imp-vol-kiri-again 2>&1 | wc -l)" -gt "$n" ] && break; sleep 1; done; "$@" logs imp-vol-kiri-again 2>&1 | tail -1' _ "${K[@]}"
  kiri_rm imp-vol-kiri-again
  "${K[@]}" run --name imp-vol-kiri-stopped --network none -v imp-vol-cut:/d alpine sh -c 'echo stopped > /d/stopped' >/dev/null
  t cut-stopped-in-kiri-refused "Kiri has imp-vol-kiri-stopped using it" "$KIRI" import "$SRC" volume imp-vol-cut
  t cut-stopped-kept "stopped 1" "${kept[@]}" stopped
  kiri_rm imp-vol-kiri-stopped
  "${K[@]}" run -d --name imp-vol-kiri-user --network none -v imp-vol-cut:/d alpine sh -c 'echo mine > /d/mine; sleep 1d' >/dev/null
  sleep 1
  t cut-used-in-kiri-refused "Kiri has imp-vol-kiri-user using it" "$KIRI" import "$SRC" volume imp-vol-cut
  t cut-used-in-kiri-force-refused "Kiri has imp-vol-kiri-user using it" "$KIRI" import "$SRC" --force volume imp-vol-cut
  t cut-used-in-kiri-kept "mine" "${K[@]}" exec imp-vol-kiri-user cat /d/mine
  kiri_rm imp-vol-kiri-user
  out=$(cut "$KIRI" import "$SRC" container imp-vol-cutter)
  t cut-container-stops "unfinished in Kiri" echo "$out"
  t cut-container-not-made "absent" bash -c '"$@" inspect imp-vol-cutter >/dev/null 2>&1 && echo present || echo absent' _ "${K[@]}"
  t cut-repaired "volume imp-vol-cut: done" "$KIRI" import "$SRC" volume imp-vol-cut
  t cut-exact "4294967296 after | a-big b-after |" "${K[@]}" run --rm --network none -v imp-vol-cut:/d:ro alpine sh -c 'echo "$(stat -c %s /d/a-big) $(cat /d/b-after) | $(ls -A /d | tr "\n" " ")|"'
  t cut-container-then "container imp-vol-cutter: done" "$KIRI" import "$SRC" container imp-vol-cutter
  kiri_rm imp-vol-cutter; "${S[@]}" rm -fv imp-vol-cutter >/dev/null
  "${K[@]}" volume rm imp-vol-cut >/dev/null 2>&1; "${S[@]}" volume rm imp-vol-cut >/dev/null 2>&1
fi

section "#345 a volume a running container writes to is not copied"
"${S[@]}" volume create imp-vol-busy >/dev/null
"${S[@]}" run -d --name imp-vol-writer --network none --entrypoint sh -v imp-vol-busy:/d "$TOOL" -c 'while true; do date >> /d/log; sleep 0.05; done' >/dev/null
"${S[@]}" create --name imp-vol-idle --network none -v imp-vol-busy:/d "$TOOL" >/dev/null
sleep 2
t busy-refused "imp-vol-busy is in use by imp-vol-writer" "$KIRI" import "$SRC" volume imp-vol-busy
t busy-nothing-copied "absent" bash -c '[ -z "$("$@" volume ls -q --filter name=^imp-vol-busy$)" ] && echo absent || echo present' _ "${K[@]}"
t busy-force-copies "volume imp-vol-busy: done" "$KIRI" import "$SRC" --force volume imp-vol-busy
"${S[@]}" stop -t1 imp-vol-writer >/dev/null
"${K[@]}" volume rm imp-vol-busy >/dev/null
# Stopped, and a container that is only created: an exact copy.
t busy-stopped-imports "volume imp-vol-busy: done" "$KIRI" import "$SRC" volume imp-vol-busy
src_lines=$(src_sh -v imp-vol-busy:/d:ro "$TOOL" -c 'wc -l < /d/log')
t busy-stopped-exact "$src_lines lines" "${K[@]}" run --rm --network none -v imp-vol-busy:/d:ro alpine sh -c 'echo "$(wc -l < /d/log) lines"'

section "#375 --network container: and the options import dropped"
"${S[@]}" create --name imp-net-target --dns 9.9.9.9 --entrypoint sleep "$TOOL" 1d >/dev/null
"${S[@]}" create --name imp-net-joiner --network container:imp-net-target --device /dev/null:/dev/imp-null --group-add 1234 \
  --health-cmd true --health-interval 30s --entrypoint sleep "$TOOL" 1d >/dev/null
t net-joiner-imports "container imp-net-joiner: done" "$KIRI" import "$SRC" container imp-net-joiner
t net-target-brought-first "imp-net-target" "${K[@]}" ps -a --format '{{.Names}}' --filter name=imp-net-target
t net-mode-is-kiris "container:$("${K[@]}" inspect -f '{{.Id}}' imp-net-target 2>/dev/null)" "${K[@]}" inspect -f '{{.HostConfig.NetworkMode}}' imp-net-joiner
t net-devices "/dev/imp-null" "${K[@]}" inspect -f '{{json .HostConfig.Devices}}' imp-net-joiner
t net-group-add "1234" "${K[@]}" inspect -f '{{json .HostConfig.GroupAdd}}' imp-net-joiner
t net-healthcheck '"true"' "${K[@]}" inspect -f '{{json .Config.Healthcheck}}' imp-net-joiner
t net-dns "9.9.9.9" "${K[@]}" inspect -f '{{json .HostConfig.Dns}}' imp-net-target
"${K[@]}" start imp-net-target imp-net-joiner >/dev/null 2>&1
t net-shared-namespace "same" bash -c 'a=$("$@" exec imp-net-joiner hostname) && b=$("$@" exec imp-net-target hostname) && [ "$a" = "$b" ] && echo same || echo "different: $a / $b"' _ "${K[@]}"
t net-device-there "/dev/imp-null" "${K[@]}" exec imp-net-joiner ls /dev/imp-null
kiri_rm imp-net-joiner imp-net-target

section "#377 a container whose image is pinned by digest"
PINNED=$("${S[@]}" image ls --digests --format '{{.Repository}}@{{.Digest}}' | grep "^$PINNED_REPO@sha256:" | head -1)
if [ -n "$PINNED" ]; then
  note "$SRC already has $PINNED; using it as it is"
else
  PINNED_OURS=1
  "${S[@]}" pull -q "$PINNED_REPO:latest" >/dev/null
  PINNED=$("${S[@]}" image inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$PINNED_REPO:latest" | grep "^$PINNED_REPO@" | head -1)
  # Untagged, known only by its digest, as an image pulled by digest is.
  # When the image had no other name, untagging removed it: it comes back
  # by digest.
  "${S[@]}" rmi "$PINNED_REPO:latest" >/dev/null
  "${S[@]}" image inspect "$PINNED" >/dev/null 2>&1 || "${S[@]}" pull -q "$PINNED" >/dev/null
fi
"${K[@]}" image inspect "$PINNED" >/dev/null 2>&1 || PINNED_KIRI_OURS=1
"${S[@]}" create --name imp-digest "$PINNED" >/dev/null
t digest-listed "a row of its own" bash -c '"$1" import "$2" --list | grep -qxE "  $3(  \(already in Kiri\))?" && echo "a row of its own"' _ "$KIRI" "$SRC" "$PINNED"
t digest-container-imports "container imp-digest: done" "$KIRI" import "$SRC" container imp-digest
t digest-kept-on-container "$PINNED" "${K[@]}" inspect -f '{{.Config.Image}}' imp-digest
t digest-image-by-reference "[\"$PINNED\"]" "${K[@]}" image inspect -f '{{json .RepoTags}}' "$PINNED"
t digest-runs "Hello from Docker" "${K[@]}" start -a imp-digest

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || { printf 'failed: %s\n' "${FAILED[*]}"; exit 1; }
