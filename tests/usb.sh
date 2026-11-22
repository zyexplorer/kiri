#!/bin/bash
# USB passthrough, end to end, with no hardware and no root: `kiri usb fake`
# attaches software USB devices through the daemon exactly as `kiri usb
# serve` attaches a captured one, and the guest's real USB stack, cdc-acm
# and usbhid take it from there. Usage:
#
#   DOCKER_CONTEXT=kiri-usb KIRI="target/dev/Kiri Usb.app/Contents/MacOS/kiri" tests/usb.sh
#
# Covers: enumeration, a 1 MiB echo through /dev/ttyACM0, opens and closes
# (each one unlinks pending URBs), line coding, a stalled endpoint, two
# devices at once, detach, a killed helper, a device unplugged mid-transfer,
# every vhci port taken, attaching while the VM sleeps, the VM stopping
# under a device, the VM staying awake while a device is attached, double
# attach, bad requests, a peer that breaks the protocol, and a mouse's
# interrupt endpoint. Exits non-zero if any check fails.

set -u
export DOCKER_CONTEXT=${DOCKER_CONTEXT:-kiri}
K=${KIRI:-kiri}
SOCK=$(docker context inspect "$DOCKER_CONTEXT" --format '{{.Endpoints.docker.Host}}' 2>/dev/null); SOCK=${SOCK#unix://}
[ -S "$SOCK" ] || { echo "usb: no engine socket for context $DOCKER_CONTEXT ($SOCK)"; exit 1; }
# alpine from KIRI_TEST_REGISTRY (tests/lib/images.sh), never from Docker Hub.
# shellcheck source=SCRIPTDIR/lib/images.sh
. "$(dirname "$0")/lib/images.sh"
kiri_seed_images alpine:latest || echo "usb: alpine could not be fetched"
export KIRI_SOCKET=$SOCK
LOGS=$(mktemp -d)
PASS=0 FAIL=0 FAILED=()
PIDS=()

ok() { PASS=$((PASS + 1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL + 1)); FAILED+=("$1"); echo "  FAIL $1${2:+: $2}"; }
check() { local name=$1; shift; if "$@"; then ok "$name"; else bad "$name"; fi; }

api() { curl -s --max-time 30 --unix-socket "$SOCK" -X "$1" "http://kiri$2"; }
attached() { api GET /kiri/usb; }
# In the VM, as root: sysfs and /dev as the guest sees them.
vm() { docker run --rm --privileged alpine sh -c "$1" 2>&1; }

# fake NAME KIND ARGS...: starts a fake device in the background, waits
# until the daemon lists it.
fake() {
  local loc=$1 kind=$2; shift 2
  "$K" usb fake "$kind" --location "$loc" "$@" >"$LOGS/$loc.log" 2>&1 &
  PIDS+=($!)
  eval "PID_${loc//-/_}=$!"
  for _ in $(seq 100); do
    attached | grep -q "\"$loc\"" && return 0
    kill -0 "$!" 2>/dev/null || return 1
    sleep 0.1
  done
  return 1
}
pid_of() { local v="PID_${1//-/_}"; echo "${!v}"; }
# gone LOCATION: the daemon no longer lists it and its helper has exited.
gone() {
  local pid; pid=$(pid_of "$1")
  for _ in $(seq 100); do
    if ! attached | grep -q "\"$1\"" && ! kill -0 "$pid" 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  return 1
}
detach() { api DELETE "/kiri/usb/$1" >/dev/null; }
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT

# The tty node of the test serial device in the VM, after it settles.
acm_count() { vm 'ls /dev/ttyACM* 2>/dev/null | wc -l' | tr -d ' '; }
wait_acm() { for _ in $(seq 50); do [ "$(acm_count)" = "$1" ] && return 0; sleep 0.2; done; return 1; }

# echo_through DEV BYTES: sends random bytes through the echoing tty, and
# checks the same bytes come back.
echo_through() {
  # The tty stays open on fd 3 throughout: its last close would discard
  # input that had arrived but not been read yet.
  docker run --rm --device "$1" alpine sh -c "
    exec 3<>$1
    stty -F $1 raw -echo 115200 || exit 9
    head -c $2 /dev/urandom > /tmp/in
    timeout -s KILL 60 head -c $2 <&3 > /tmp/out & r=\$!
    cat /tmp/in >&3
    wait \$r
    [ \"\$(sha256sum < /tmp/in)\" = \"\$(sha256sum < /tmp/out)\" ] && echo SAME || echo \"DIFFERENT \$(wc -c < /tmp/out)\"
  " 2>&1 | grep -q '^SAME$'
}

# Checks run their command in a child bash: give it the helpers.
export K SOCK LOGS DOCKER_CONTEXT
export -f api attached vm acm_count wait_acm echo_through

echo "== USB passthrough on $DOCKER_CONTEXT"
api POST /kiri/wake >/dev/null
for loc in $(attached | grep -o '"Location":"[^"]*"' | cut -d'"' -f4); do detach "$loc"; done

echo "-- a serial device"
check "serial attaches" fake t-serial serial
check "it enumerates as a CDC ACM tty" wait_acm 1
check "with its own descriptors" bash -c "vm 'cat /sys/bus/usb/devices/*/product' | grep -q 'Kiri test serial'"
check "1 MiB echoes back unchanged" echo_through /dev/ttyACM0 1048576
check "64 KiB echoes back again" echo_through /dev/ttyACM0 65536
opens=$(docker run --rm --device /dev/ttyACM0 alpine sh -c '
  n=0; for i in $(seq 50); do timeout -s KILL 5 sh -c "exec 3<>/dev/ttyACM0; exec 3>&-" && n=$((n+1)); done; echo $n' 2>&1)
check "50 opens and closes, none stuck (each unlinks pending URBs)" [ "$opens" = 50 ]
check "it still echoes after them" echo_through /dev/ttyACM0 4096
check "line coding is set and read back" bash -c "docker run --rm --device /dev/ttyACM0 alpine sh -c 'stty -F /dev/ttyACM0 9600 && stty -F /dev/ttyACM0 speed' 2>&1 | grep -q '^9600$'"
check "a second attach of the same device is refused" bash -c "perl -e 'alarm 30; exec @ARGV' '$K' usb fake serial --location t-serial 2>&1 | grep -q 'already attached'"
detach t-serial
check "detach removes it from the daemon and ends the helper" gone t-serial
check "and from the VM" wait_acm 0

echo "-- faults"
check "a stalling device attaches" fake t-stall serial --stall-once
wait_acm 1
check "data still flows once the driver clears the halt" echo_through /dev/ttyACM0 4096
detach t-stall; gone t-stall
check "the driver cleared the halt" grep -q "[1-9][0-9]* halts cleared" "$LOGS/t-stall.log"

fake t-kill serial; wait_acm 1
kill -9 "$(pid_of t-kill)"
check "a killed helper: the daemon drops the device" gone t-kill
check "a killed helper: the VM drops it" wait_acm 0

fake t-unplug serial --unplug-after 65536; wait_acm 1
started=$(date +%s)
docker run --rm --device /dev/ttyACM0 alpine sh -c 'stty -F /dev/ttyACM0 raw -echo; timeout -s KILL 30 sh -c "head -c 1048576 /dev/urandom > /dev/ttyACM0"' >/dev/null 2>&1
check "unplugged mid-transfer: the writer fails fast instead of hanging" [ $(( $(date +%s) - started )) -lt 25 ]
check "unplugged mid-transfer: the device is gone" gone t-unplug
check "unplugged mid-transfer: and gone from the VM" wait_acm 0
check "the guest kernel logged no oops or warning" bash -c "! vm 'dmesg' | grep -E 'Oops|WARNING:|BUG:|Call trace'"

echo "-- many devices"
fake t-a serial; fake t-b serial
check "two ttys" wait_acm 2
check "each echoes on its own (A)" echo_through /dev/ttyACM0 65536
check "each echoes on its own (B)" echo_through /dev/ttyACM1 65536
detach t-a; detach t-b; gone t-a; gone t-b; wait_acm 0
n=0
for i in $(seq 8); do fake "t-port$i" serial && n=$((n + 1)); done
check "eight devices fill vhci's eight ports" [ "$n" = 8 ]
perl -e 'alarm 30; exec @ARGV' "$K" usb fake serial --location t-port9 >"$LOGS/t-port9.log" 2>&1
check "a ninth is refused with a reason" grep -q "could not attach" "$LOGS/t-port9.log"
for i in $(seq 8); do detach "t-port$i"; done
for i in $(seq 8); do gone "t-port$i"; done
check "and every port frees again" wait_acm 0

echo "-- the VM"
api POST /kiri/sleep >/dev/null
check "attaching wakes a sleeping VM" fake t-wake serial
check "and the device works" bash -c "wait_acm 1 && echo_through /dev/ttyACM0 4096"
api POST /kiri/stop >/dev/null
check "stopping Kiri ends the device's session" gone t-wake
perl -e 'alarm 30; exec @ARGV' "$K" usb fake serial --location t-stopped >"$LOGS/t-stopped.log" 2>&1
check "attaching while Kiri is stopped is refused" grep -q "Kiri did not take the device" "$LOGS/t-stopped.log"
api POST /kiri/wake >/dev/null
if [ -n "${IDLE_SECS:-}" ]; then
  fake t-idle serial; wait_acm 1
  sleep $((IDLE_SECS * 2 + 2))
  check "the VM stays awake under an attached device (${IDLE_SECS}s idle)" echo_through /dev/ttyACM0 4096
  detach t-idle; gone t-idle
fi

echo "-- bad requests"
check "a bad location is refused" bash -c "api POST '/kiri/usb?location=../x&speed=2' | grep -q 'location'"
check "a bad speed is refused" bash -c "api POST '/kiri/usb?location=x&speed=9' | grep -q 'speed'"
check "detaching nothing says so" bash -c "api DELETE /kiri/usb/nothing-here | grep -q 'no USB device'"
python3 - "$SOCK" <<'EOF' >"$LOGS/junk.log" 2>&1
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
s.sendall(b"POST /kiri/usb?location=t-junk&name=junk&speed=2&devid=1 HTTP/1.1\r\nHost: kiri\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 0\r\n\r\n")
head = b""
while b"\r\n\r\n" not in head:  # the reply may come in pieces
    part = s.recv(4096)
    if not part: break
    head += part
print(head.split(b"\r\n")[0].decode())
time.sleep(0.5)
s.sendall(b"\xde\xad\xbe\xef" * 64)  # not USB/IP
time.sleep(2)
s.close()
EOF
check "a peer that breaks the protocol is upgraded first" grep -q "101" "$LOGS/junk.log"
for _ in $(seq 50); do attached | grep -q t-junk || break; sleep 0.1; done
check "then dropped by the guest and the daemon" bash -c "! attached | grep -q t-junk"
check "the guest kernel survived it" bash -c "! vm 'dmesg' | grep -E 'Oops|BUG:|Call trace'"

echo "-- a mouse (interrupt IN)"
check "mouse attaches" fake t-mouse mouse --reports 100000
ev=
for _ in $(seq 50); do
  ev=$(vm 'for d in /sys/class/input/event*; do grep -q "Kiri test mouse" $d/device/name 2>/dev/null && basename $d; done' | head -1)
  [ -n "$ev" ] && break
  sleep 0.2
done
check "it has an input device" [ -n "$ev" ]
bytes=$(docker run --rm --device "/dev/input/$ev" alpine sh -c "timeout -s KILL 2 cat /dev/input/$ev | wc -c" 2>/dev/null | tr -d ' ')
check "movements arrive as input events ($bytes bytes in 2 s)" [ "${bytes:-0}" -gt 240 ]
reads=$(docker run --rm --device "/dev/input/$ev" alpine sh -c "
  n=0; for i in \$(seq 20); do timeout -s KILL 3 sh -c 'head -c 24 /dev/input/$ev >/dev/null' && n=\$((n+1)); done; echo \$n" 2>&1)
check "20 opens, reads and closes of the mouse" [ "$reads" = 20 ]
detach t-mouse
check "the mouse detaches" gone t-mouse

echo
echo "usb: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ] || { printf '  %s\n' "${FAILED[@]}"; echo "logs: $LOGS"; exit 1; }
