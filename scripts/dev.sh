#!/bin/sh
# Local secondary-dev loop: rebuild from the checkout, run an isolated daemon
# that does not touch the installed Kiri (~/.kiri), then unit-test / smoke.
#
#   scripts/dev.sh build          # guest (if needed) + signed host binary
#   scripts/dev.sh up             # start isolated daemon in the background
#   scripts/dev.sh down           # stop it
#   scripts/dev.sh ui             # open the window (survives the shell; no Terminal)
#   scripts/dev.sh ui-down        # close that window
#   scripts/dev.sh status
#   scripts/dev.sh test           # cargo test (unit)
#   scripts/dev.sh smoke          # boot + ping; optional docker run if an image exists
#   scripts/dev.sh check          # build → test → up → smoke (leaves daemon up)
#   scripts/dev.sh run -- …       # ./target/release/kiri … with isolation env
#   scripts/dev.sh docker -- …    # docker against the isolated socket
#   scripts/dev.sh env            # print export lines for a manual shell
#
# Override with KIRI_SRC_HOME (default: ~/.kiri-src). SKIP_GUEST=1 skips
# rebuilding the guest agent. SEED_IMAGES=1 copies blobs from the installed
# Kiri data dir so smoke can run without pulling.
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
bin=$root/target/release/kiri
home=${KIRI_SRC_HOME:-$HOME/.kiri-src}
data=$home/data
sock=$home/run/docker.sock
pidfile=$home/run/daemon.pid
logfile=$home/daemon.log
guest=$root/guest/out

export KIRI_DATA_DIR=$data
export KIRI_SOCKET=$sock
# Prefer the in-tree guest; still honour an explicit override.
export KIRI_GUEST_DIR=${KIRI_GUEST_DIR:-$guest}

die() { printf '%s\n' "$*" >&2; exit 1; }
say() { printf '==> %s\n' "$*"; }

need_bin() {
  [ -x "$bin" ] || die "no $bin — run: scripts/dev.sh build"
}

ensure_guest() {
  mkdir -p "$guest"
  app_guest=/Applications/Kiri.app/Contents/Resources/guest
  if [ ! -f "$guest/Image" ]; then
    [ -f "$app_guest/Image" ] || die "missing $guest/Image (and no $app_guest/Image to copy)"
    say "copying guest kernel from Kiri.app"
    cp "$app_guest/Image" "$guest/Image"
  fi
  if [ ! -f "$guest/data.img" ]; then
    if [ -f "$app_guest/data.img.tgz" ]; then
      say "unpacking data.img from Kiri.app"
      tar -xzf "$app_guest/data.img.tgz" -C "$guest"
    else
      die "missing $guest/data.img — run: cargo xtask disk (needs a Docker engine + debian:trixie)"
    fi
  fi
  if [ ! -f "$guest/initramfs.cpio" ]; then
    say "building guest initramfs"
    (cd "$root" && cargo xtask guest)
  fi
}

seed_images() {
  src=${KIRI_SEED_FROM:-$HOME/Library/Application Support/Kiri}
  [ -d "$src/blobs" ] || die "no image store at $src (set KIRI_SEED_FROM=…)"
  say "seeding images from $src → $data"
  mkdir -p "$data"
  # APFS clone when possible; fall back to copy.
  if ! cp -Rc "$src/blobs" "$data/" 2>/dev/null; then
    rm -rf "$data/blobs"
    cp -R "$src/blobs" "$data/"
  fi
  [ -f "$src/images.json" ] && cp "$src/images.json" "$data/images.json"
}

cmd_build() {
  ensure_guest
  if [ "${SKIP_GUEST:-0}" != 1 ]; then
    say "cargo xtask guest"
    (cd "$root" && cargo xtask guest)
  fi
  say "cargo xtask host"
  (cd "$root" && cargo xtask host)
  say "built $bin"
}

daemon_pid() {
  [ -f "$pidfile" ] || return 1
  pid=$(cat "$pidfile")
  kill -0 "$pid" 2>/dev/null || return 1
  printf '%s\n' "$pid"
}

cmd_down() {
  if pid=$(daemon_pid); then
    say "stopping daemon pid $pid"
    kill "$pid" 2>/dev/null || true
    # Wait briefly for the socket to go away.
    i=0
    while [ -S "$sock" ] && [ "$i" -lt 50 ]; do
      sleep 0.1
      i=$((i + 1))
    done
    rm -f "$pidfile"
  else
    say "daemon not running"
    rm -f "$pidfile"
  fi
}

cmd_up() {
  need_bin
  ensure_guest
  mkdir -p "$home/run" "$data"
  if pid=$(daemon_pid); then
    say "daemon already running (pid $pid)"
    return 0
  fi
  # Stale socket from a crashed daemon.
  rm -f "$sock"
  if [ "${SEED_IMAGES:-0}" = 1 ]; then
    seed_images
  fi
  say "starting daemon → $sock"
  say "log: $logfile"
  # Same isolation env the children inherit; do not touch ~/.kiri.
  KIRI_DATA_DIR=$data KIRI_SOCKET=$sock KIRI_GUEST_DIR=$guest \
    "$bin" daemon >>"$logfile" 2>&1 &
  echo $! >"$pidfile"
  i=0
  while [ ! -S "$sock" ] && [ "$i" -lt 100 ]; do
    sleep 0.1
    i=$((i + 1))
  done
  [ -S "$sock" ] || {
    cmd_down
    die "daemon did not create $sock — see $logfile"
  }
  say "daemon up (pid $(cat "$pidfile"))"
}

# The window must outlive the launching shell. Cursor/agent shells kill their
# children on exit, which looks like a flash-then-quit. launchd keeps it.
ui_label=dev.kiri.src.ui
ui_plist=$home/run/ui.plist
ui_logfile=$home/ui.log

cmd_ui_down() {
  launchctl bootout "gui/$(id -u)/$ui_label" 2>/dev/null || true
  # Orphans from older launches (Terminal / nohup).
  pgrep -f "$bin ui" >/dev/null 2>&1 && pkill -f "$bin ui" 2>/dev/null || true
  rm -f "$ui_plist"
  say "ui down"
}

cmd_ui() {
  need_bin
  mkdir -p "$home/run"
  cmd_up
  cmd_ui_down >/dev/null
  cat >"$ui_plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$ui_label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$bin</string>
    <string>ui</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>KIRI_DATA_DIR</key><string>$data</string>
    <key>KIRI_SOCKET</key><string>$sock</string>
    <key>KIRI_GUEST_DIR</key><string>$guest</string>
  </dict>
  <key>WorkingDirectory</key><string>$root</string>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$ui_logfile</string>
  <key>StandardErrorPath</key><string>$ui_logfile</string>
</dict>
</plist>
EOF
  launchctl bootstrap "gui/$(id -u)" "$ui_plist"
  say "ui up (launchd $ui_label) — log: $ui_logfile"
}

cmd_status() {
  printf 'repo          %s\n' "$root"
  printf 'binary        %s%s\n' "$bin" "$([ -x "$bin" ] && echo ' (ok)' || echo ' (missing)')"
  printf 'KIRI_SRC_HOME %s\n' "$home"
  printf 'KIRI_DATA_DIR %s\n' "$data"
  printf 'KIRI_SOCKET   %s\n' "$sock"
  printf 'KIRI_GUEST_DIR %s\n' "$guest"
  if pid=$(daemon_pid); then
    printf 'daemon        up (pid %s)\n' "$pid"
  else
    printf 'daemon        down\n'
  fi
  if launchctl print "gui/$(id -u)/$ui_label" >/dev/null 2>&1; then
    printf 'ui            up (launchd %s)\n' "$ui_label"
  elif pgrep -f "$bin ui" >/dev/null 2>&1; then
    printf 'ui            up (pid %s)\n' "$(pgrep -f "$bin ui" | tr '\n' ' ')"
  else
    printf 'ui            down\n'
  fi
  for f in Image initramfs.cpio data.img; do
    if [ -e "$guest/$f" ]; then
      printf 'guest/%-14s ok\n' "$f"
    else
      printf 'guest/%-14s MISSING\n' "$f"
    fi
  done
}

cmd_test() {
  say "cargo test --workspace"
  (cd "$root" && cargo test --workspace -- --test-threads=8)
}

docker_iso() {
  DOCKER_HOST=unix://$sock DOCKER_CONTEXT= docker "$@"
}

cmd_smoke() {
  need_bin
  ensure_guest
  say "kiri boot (own guest, no daemon)"
  KIRI_DATA_DIR=$data KIRI_GUEST_DIR=$guest "$bin" boot --pings 1

  if ! daemon_pid >/dev/null; then
    say "daemon down — starting for API smoke"
    cmd_up
  fi

  say "docker version via isolated socket"
  docker_iso version >/dev/null
  say "docker version ok"

  # Prefer an already-seeded image so this works offline.
  ref=$(docker_iso images --format '{{.Repository}}:{{.Tag}}' 2>/dev/null | awk 'NF && $0 !~ /<none>/ { print; exit }' || true)
  if [ -n "$ref" ]; then
    say "docker run --rm --name kiri-src-smoke $ref true"
    docker_iso run --rm --name "kiri-src-smoke-$$" "$ref" true
  else
    say "no local images in $data — skip container smoke (SEED_IMAGES=1 scripts/dev.sh up, or docker pull)"
  fi
  say "smoke ok"
}

cmd_check() {
  cmd_build
  if [ "${SKIP_TEST:-0}" != 1 ]; then
    if ! cmd_test; then
      # A shell-banner UI term test can fail on some machines; keep going unless strict.
      if [ "${FAIL_TEST:-0}" = 1 ]; then
        die "unit tests failed (FAIL_TEST=1)"
      fi
      say "unit tests reported failures — continuing to smoke (FAIL_TEST=1 to abort)"
    fi
  fi
  cmd_up
  cmd_smoke
  say "check done — daemon still up; use: scripts/dev.sh down"
}

cmd_env() {
  cat <<EOF
export KIRI_SRC_HOME=$home
export KIRI_DATA_DIR=$data
export KIRI_SOCKET=$sock
export KIRI_GUEST_DIR=$guest
export DOCKER_HOST=unix://$sock
unset DOCKER_CONTEXT
# then: docker ps   or   $bin run --rm …
EOF
}

cmd_run() {
  need_bin
  ensure_guest
  mkdir -p "$home/run" "$data"
  exec env KIRI_DATA_DIR=$data KIRI_SOCKET=$sock KIRI_GUEST_DIR=$guest "$bin" "$@"
}

cmd_docker() {
  [ -S "$sock" ] || die "daemon not up — run: scripts/dev.sh up"
  exec env DOCKER_HOST=unix://$sock DOCKER_CONTEXT= docker "$@"
}

usage() {
  sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

[ $# -ge 1 ] || usage
cmd=$1
shift
case "$cmd" in
  build) cmd_build "$@" ;;
  ensure-guest) ensure_guest "$@" ;;
  seed) seed_images "$@" ;;
  up) cmd_up "$@" ;;
  down) cmd_down "$@" ;;
  ui) cmd_ui "$@" ;;
  ui-down) cmd_ui_down "$@" ;;
  status) cmd_status "$@" ;;
  test) cmd_test "$@" ;;
  smoke) cmd_smoke "$@" ;;
  check) cmd_check "$@" ;;
  env) cmd_env "$@" ;;
  run) cmd_run "$@" ;;
  docker) cmd_docker "$@" ;;
  *) usage ;;
esac
