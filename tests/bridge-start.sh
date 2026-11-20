#!/bin/sh
# `kiri bridge` starting the engine when it is gone (#93), in the Alpine
# Kiri Linux is: the Linux build (cargo xtask linux, or BIN=) as the bridge
# and the daemon, and contrib/wsl/kiri-linux/kiri-start, in a privileged
# container standing in for the distro after a WSL restart (no daemon, its
# socket file left behind). A stand-in kiri-win sends three connections at
# once: kiri-start runs once, one daemon starts, and all three get the
# engine's answer. Then, the daemon gone and kiri-start failing: the
# connection is closed with kiri-start's reason, and the next one within
# 5 s gets it too without another start. Last, a daemon that creates its
# socket and exits as it boots (Engine::boot failing): four connections
# 1 s apart, as kiri-win's panel polls, start it once, not four times.
# Usage: tests/bridge-start.sh
set -u
D=${D:-docker}
DIR=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(cd "${BIN:-$DIR/target/linux/release}" 2>/dev/null && pwd) || BIN=/nonexistent
[ -x "$BIN/kiri" ] || { echo "build first: cargo xtask linux (or BIN=<dir with a Linux kiri>)"; exit 1; }
# alpine:3.24 from KIRI_TEST_REGISTRY (tests/lib/images.sh), never from Docker Hub.
# shellcheck source=SCRIPTDIR/lib/images.sh
. "$(dirname "$0")/lib/images.sh"
kiri_seed_images alpine:3.24 || echo "an image could not be fetched"
# The script is for the container's sh: its $ are expanded there.
# shellcheck disable=SC2016
$D run --rm --privileged --tmpfs /run -v "$BIN/kiri:/opt/kiri:ro" \
  -v "$DIR/contrib/wsl/kiri-linux/kiri-start:/opt/kiri-start:ro" alpine:3.24 sh -c '
  apk add -q python3 >/dev/null 2>&1 || { echo "FAIL: no python3"; exit 1; }
  install -m 755 /opt/kiri /usr/local/bin/kiri
  install -m 755 /opt/kiri-start /usr/local/bin/kiri-start
  S=/var/lib/kiri-daemon/.kiri/run/docker.sock
  mkdir -p "$(dirname $S)"
  # A socket file nobody listens on, as a daemon that died leaves it.
  python3 -c "import socket,sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])" $S
  cat >/tmp/kiriwin.py <<PY
import signal, struct, subprocess, sys, time
signal.alarm(40)
n, sock = int(sys.argv[1]), sys.argv[2]
# With a gap: one connection at a time, each once the last has ended.
gap = float(sys.argv[3]) if len(sys.argv) > 3 else 0
p = subprocess.Popen(["/usr/local/bin/kiri", "bridge", "--socket", sock], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def send(kind, cid, data=b""):
    p.stdin.write(struct.pack(">BII", kind, cid, len(data)) + data)
    p.stdin.flush()
req = b"GET /_ping HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n"
def opened(cid):
    send(1, cid, b"\x01")
    send(2, cid, req)
    send(3, cid)
got, done = {}, {}
def wait(m):
    while len(done) < m:
        h = p.stdout.read(9)
        if len(h) < 9:
            sys.exit("the bridge ended")
        kind, cid, size = struct.unpack(">BII", h)
        data = p.stdout.read(size)
        if kind == 2:
            got[cid] = got.get(cid, b"") + data
        elif kind == 3:
            done[cid] = "reply: " + got.get(cid, b"").decode(errors="replace").split("\r\n")[0] + " / " + got.get(cid, b"").decode(errors="replace").rsplit("\r\n", 1)[-1]
        elif kind == 4:
            done[cid] = "close: " + data.decode(errors="replace")
for cid in range(1, n + 1):
    opened(cid)
    if gap:
        wait(cid)
        time.sleep(gap)
wait(n)
for cid in sorted(done):
    print(done[cid])
PY
  fail=0
  out=$(python3 /tmp/kiriwin.py 3 $S 2>&1)
  [ "$(echo "$out" | grep -c "^reply: HTTP/1.1 200 OK / OK$")" = 3 ] || { echo "FAIL: not all three served:"; echo "$out"; fail=1; }
  [ "$(grep -c "kiri bridge: engine not answering on its socket; starting it" /var/log/kiri.log)" = 1 ] || { echo "FAIL: not one start:"; grep "kiri bridge" /var/log/kiri.log; fail=1; }
  grep -q "kiri bridge: engine started" /var/log/kiri.log || { echo "FAIL: the start was not logged"; fail=1; }
  [ "$(pgrep -f "/usr/local/bin/[k]iri daemon" | wc -l)" = 1 ] || { echo "FAIL: $(pgrep -f "/usr/local/bin/[k]iri daemon" | wc -l) daemons"; fail=1; }
  # Up now: a new bridge (kiri-win after a WSL restart) starts nothing.
  python3 /tmp/kiriwin.py 1 $S | grep -q "^reply: HTTP/1.1 200 OK" || { echo "FAIL: not served once up"; fail=1; }
  [ "$(grep -c "starting it" /var/log/kiri.log)" = 1 ] || { echo "FAIL: started again while up"; fail=1; }
  # The daemon crashes, and kiri-start cannot start it.
  pkill -9 -f "/usr/local/bin/[k]iri daemon"; sleep 0.5
  printf "#!/bin/sh\necho x >>/tmp/starts\necho \"kiri daemon exited as it started; see /var/log/kiri.log\" >&2\nexit 2\n" >/usr/local/bin/kiri-start
  out=$(python3 /tmp/kiriwin.py 2 $S 2>&1)
  [ "$(echo "$out" | grep -c "^close: kiri daemon exited as it started; see /var/log/kiri.log$")" = 2 ] || { echo "FAIL: no reason:"; echo "$out"; fail=1; }
  [ "$(wc -l </tmp/starts)" = 1 ] || { echo "FAIL: $(wc -l </tmp/starts) failing starts for two connections"; fail=1; }
  grep -q "kiri bridge: engine did not start: kiri daemon exited as it started" /var/log/kiri.log || { echo "FAIL: the failure was not logged"; fail=1; }
  # A daemon that creates its socket, then exits as it boots: the kernel
  # queues the connection of the start, which then closes unanswered.
  cat >/tmp/dies.py <<PY
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(8); time.sleep(0.3)
PY
  printf "#!/bin/sh\necho x >>/tmp/boots\nrm -f $S\nnohup python3 /tmp/dies.py $S >/dev/null 2>&1 &\ni=0; while [ \$i -lt 50 ]; do [ -S $S ] && exit 0; sleep 0.1; i=\$((i+1)); done; exit 1\n" >/usr/local/bin/kiri-start
  out=$(python3 /tmp/kiriwin.py 4 $S 1 2>&1)
  [ "$(wc -l </tmp/boots)" = 1 ] || { echo "FAIL: $(wc -l </tmp/boots) starts for four connections to a daemon that dies as it boots:"; echo "$out"; fail=1; }
  [ "$(echo "$out" | grep -c "^close: it started, then stopped before answering")" = 3 ] || { echo "FAIL: no reason for the daemon that died:"; echo "$out"; fail=1; }
  [ $fail = 0 ] && echo "ok: three connections, one start, all served; a failed start is said, once; a daemon dying as it boots is not started again at once"
  exit $fail'
