#!/bin/sh
# Kiri Linux's kiri-start (contrib/wsl/kiri-linux), in the Alpine it runs
# on, with a stand-in daemon (python3) that, as the real one, makes its
# socket and, on SIGTERM, removes it first and exits a few seconds later
# (crates/kiri/src/daemon/mod.rs, stopping containers).
# - four started at once start one daemon, all exit 0, and the daemon does
#   not keep their lock;
# - one whose daemon never makes its socket says so (exit 1); one whose
#   daemon exits at once is exit 2;
# - started while the daemon is stopping (Restart engine's kill, then a
#   start), it waits for that one to go and starts another;
# - --restart stops the daemon, waits for it, and starts a new one, also
#   when its shutdown takes 20 s (containers with a long --stop-timeout).
# The checks' own pgrep spell it "[k]iri", so as not to match the script
# they run in. Usage: tests/kiri-start.sh
set -u
D=${D:-docker}
DIR=$(cd "$(dirname "$0")/.." && pwd)
# alpine:3.24 from KIRI_TEST_REGISTRY (tests/lib/images.sh), never from Docker Hub.
# shellcheck source=SCRIPTDIR/lib/images.sh
. "$(dirname "$0")/lib/images.sh"
kiri_seed_images alpine:3.24 || echo "an image could not be fetched"
# The script is for the container's sh: its $ are expanded there.
# shellcheck disable=SC2016
$D run --rm -v "$DIR/contrib/wsl/kiri-linux/kiri-start:/usr/local/bin/kiri-start:ro" alpine:3.24 sh -c '
  apk add -q python3 >/dev/null 2>&1 || { echo "FAIL: no python3"; exit 1; }
  # MODE: up (socket after 0.5 s), nosocket, exits.
  cat >/usr/local/bin/kiri <<"K"
#!/usr/bin/python3
import os, signal, socket, sys, time
if sys.argv[1:] != ["daemon"]: sys.exit(1)
open("/tmp/launches", "a").write("%d\n" % os.getpid())
mode = open("/tmp/mode").read().strip()
if mode == "exits": sys.exit(1)
s = "/var/lib/kiri-daemon/.kiri/run/docker.sock"
def term(*_):
    if os.path.exists(s): os.remove(s)
    time.sleep(int(open("/tmp/stopwait").read()) if os.path.exists("/tmp/stopwait") else 4); sys.exit(0)
signal.signal(signal.SIGTERM, term)
time.sleep(0.5)
if mode == "up":
    os.makedirs(os.path.dirname(s), exist_ok=True)
    l = socket.socket(socket.AF_UNIX); l.bind(s); l.listen(8)
while True: time.sleep(1)
K
  chmod 755 /usr/local/bin/kiri
  S=/var/lib/kiri-daemon/.kiri/run/docker.sock
  daemons() { pgrep -f "/usr/local/bin/[k]iri daemon"; }
  fail=0
  echo up >/tmp/mode
  for n in 1 2 3 4; do { sh /usr/local/bin/kiri-start >/tmp/out$n 2>&1; echo $? >/tmp/code$n; } & done
  wait
  [ "$(wc -l </tmp/launches)" -eq 1 ] || { echo "FAIL: $(wc -l </tmp/launches) daemons started"; fail=1; }
  [ "$(cat /tmp/code* | tr -d "\n")" = 0000 ] || { echo "FAIL: four at once: exit codes $(cat /tmp/code* | tr "\n" " ")"; cat /tmp/out*; fail=1; }
  pid=$(daemons | head -1)
  if [ -z "$pid" ] || [ ! -S $S ]; then
    echo "FAIL: no daemon serves its socket"; fail=1
  elif ls -l "/proc/$pid/fd" | grep -q kiri-start.lock; then
    echo "FAIL: the daemon keeps the lock"; fail=1
  fi
  # Restart engine as it was: the daemon killed, then a start while it
  # stops (its socket gone, its process not yet).
  pkill -f "/usr/local/bin/[k]iri daemon"; sleep 1
  sh /usr/local/bin/kiri-start 2>/tmp/err; code=$?
  new=$(daemons)
  if [ $code != 0 ] || [ -z "$new" ] || [ "$new" = "$pid" ] || [ ! -S $S ]; then
    echo "FAIL: a start while the daemon stops: exit $code, daemon ${new:-none} (was $pid), $(cat /tmp/err)"; fail=1
  fi
  # --restart: the old one stopped and waited for, a new one up.
  pid=$new
  sh /usr/local/bin/kiri-start --restart 2>/tmp/err; code=$?
  new=$(daemons)
  if [ $code != 0 ] || [ -z "$new" ] || [ "$new" = "$pid" ] || [ ! -S $S ]; then
    echo "FAIL: --restart: exit $code, daemon ${new:-none} (was $pid), $(cat /tmp/err)"; fail=1
  fi
  # --restart of a daemon whose shutdown takes longer than 15 s.
  pid=$new
  echo 20 >/tmp/stopwait
  sh /usr/local/bin/kiri-start --restart 2>/tmp/err; code=$?
  rm -f /tmp/stopwait
  new=$(daemons)
  if [ $code != 0 ] || [ -z "$new" ] || [ "$new" = "$pid" ] || [ ! -S $S ]; then
    echo "FAIL: --restart of a slow stop: exit $code, daemon ${new:-none} (was $pid), $(cat /tmp/err)"; fail=1
  fi
  pkill -9 -f "/usr/local/bin/[k]iri daemon"; rm -f $S; sleep 0.2
  # A daemon that never makes its socket: the start says so, exit 1.
  echo nosocket >/tmp/mode
  sh /usr/local/bin/kiri-start 2>/tmp/err; code=$?
  [ $code = 1 ] && grep -q "did not create its socket" /tmp/err || { echo "FAIL: no socket: exit $code, $(cat /tmp/err)"; fail=1; }
  pkill -9 -f "/usr/local/bin/[k]iri daemon"; sleep 0.2
  # A daemon that exits as it starts: exit 2, at once.
  echo exits >/tmp/mode
  sh /usr/local/bin/kiri-start 2>/tmp/err; code=$?
  [ $code = 2 ] && grep -q "exited as it started" /tmp/err || { echo "FAIL: a daemon that exits: exit $code, $(cat /tmp/err)"; fail=1; }
  [ $fail = 0 ] && echo "ok: one daemon for four starts; a start while it stops, and --restart (a slow stop too), start a new one; no socket is exit 1, one that exits is exit 2"
  exit $fail'
