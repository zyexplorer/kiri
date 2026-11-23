#!/bin/bash
# Makes a Linux host look like a Windows (WSL 2) install to the engine, or
# puts it back. WSL 2 gives each distro one resolver: an address of its own
# outside 127/8 on `lo` (10.255.255.254), forwarding to Windows. A Linux host
# with systemd-resolved has 127.0.0.53 instead, so checks that depend on the
# WSL shape (#624) pass there whatever the engine does. Usage, as root:
#
#   KIRI_TEST_ALLOW_HOST_CHANGES=1 tests/lib/wsl-shape.sh up      # shape the host
#   KIRI_TEST_ALLOW_HOST_CHANGES=1 tests/lib/wsl-shape.sh down    # put it back
#   tests/lib/wsl-shape.sh status                                 # what is in place
#
# `up` adds the address to `lo` (KIRI_SHAPE_ADDR, default 10.255.255.254),
# starts a small forwarder there on port 53 (UDP and TCP, python3) to the
# host's real upstream resolver (KIRI_SHAPE_UPSTREAM, default the first
# non-loopback one systemd-resolved or /etc/resolv.conf names), and
# bind-mounts a resolv.conf naming only that address over /etc/resolv.conf.
# Nothing in /etc is written: the file or symlink there stays as it is, and
# a reboot undoes the mount, the address and the forwarder by itself.
#
# `down` undoes exactly what `up` did, also after a crash part-way: each
# change is noted in /run/kiri-test-wsl-shape before it is made. While the
# shape is up, systemd-resolved may take the shaped file's address as a
# server of its own; `down` restarts it then, so it forgets it.
#
# The engine reads the host's resolver when its agent starts: start (or
# restart) the engine after `up`, and again after `down`. tests/wsl-shape.sh
# does all of it around the DNS checks of tests/e2e.sh.

set -u
STATE=/run/kiri-test-wsl-shape
ADDR=${KIRI_SHAPE_ADDR:-10.255.255.254}
say() { echo "wsl-shape: $*"; }
die() { echo "wsl-shape: $*" >&2; exit 1; }

guard() {
  [ "$(uname -s)" = Linux ] || die "Linux hosts only"
  [ "${KIRI_TEST_ALLOW_HOST_CHANGES:-0}" = 1 ] || die "this changes the host's DNS; set KIRI_TEST_ALLOW_HOST_CHANGES=1 to allow it"
  [ "$(id -u)" = 0 ] || die "run as root"
  command -v ip >/dev/null || die "needs ip (iproute2)"
}

# The first IPv4 nameserver in file $1 that is not loopback and not ours.
upstream_in() { sed -n 's/^nameserver[[:space:]]*\([0-9.]*\).*/\1/p' "$1" 2>/dev/null | grep -v -e '^127\.' -e "^${ADDR//./\\.}\$" | head -1; }
has_addr() { ip -4 -o addr show dev lo | grep -qF " inet $ADDR/"; }
forwarder_up() { [ -s "$STATE/forwarder.pid" ] && kill -0 "$(cat "$STATE/forwarder.pid")" 2>/dev/null; }
mounted() { [ -s "$STATE/mounted-on" ] && grep -qs " $(cat "$STATE/mounted-on") " /proc/self/mountinfo; }

# The forwarder: each UDP query is sent on to the upstream and its answer
# sent back; each TCP connection is relayed to the upstream's port 53.
FORWARDER='import socket, sys, threading
addr, up = sys.argv[1], sys.argv[2]
def udp_one(srv, data, peer):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(5)
    try:
        s.sendto(data, (up, 53)); srv.sendto(s.recv(65535), peer)
    except OSError: pass
    finally: s.close()
def udp():
    srv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); srv.bind((addr, 53))
    while True:
        data, peer = srv.recvfrom(65535)
        threading.Thread(target=udp_one, args=(srv, data, peer), daemon=True).start()
def pipe(a, b):
    try:
        while True:
            d = a.recv(65535)
            if not d: break
            b.sendall(d)
    except OSError: pass
    finally:
        for s in (a, b):
            try: s.shutdown(socket.SHUT_RDWR)
            except OSError: pass
def tcp_one(c):
    try: u = socket.create_connection((up, 53), timeout=5); u.settimeout(None)
    except OSError: c.close(); return
    threading.Thread(target=pipe, args=(c, u), daemon=True).start(); pipe(u, c)
def tcp():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((addr, 53)); srv.listen(64)
    while True:
        c, _ = srv.accept(); threading.Thread(target=tcp_one, args=(c,), daemon=True).start()
threading.Thread(target=tcp, daemon=True).start()
udp()'
# A lookup of example.com through the forwarder: 0 when it answers.
PROBE='import random, socket, struct, sys
q = struct.pack(">HHHHHH", random.randrange(65536), 0x100, 1, 0, 0, 0) + b"\x07example\x03com\x00\x00\x01\x00\x01"
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(3); s.sendto(q, (sys.argv[1], 53))
sys.exit(0 if s.recv(65535)[:2] == q[:2] else 1)'

status() {
  if [ -L /etc/resolv.conf ]; then echo "/etc/resolv.conf: a symlink to $(readlink /etc/resolv.conf)"; else echo "/etc/resolv.conf: a file"; fi
  sed -n 's/^\(nameserver.*\)/  \1/p' /etc/resolv.conf
  echo "lo: $(ip -4 -o addr show dev lo | awk '{printf "%s ", $4}')"
  if [ -d "$STATE" ]; then
    echo "shape: up (upstream $(cat "$STATE/upstream" 2>/dev/null); forwarder $(forwarder_up && echo running || echo stopped); resolv.conf mounted over $(cat "$STATE/mounted-on" 2>/dev/null): $(mounted && echo yes || echo no))"
  else
    echo "shape: down"
  fi
  command -v systemctl >/dev/null && echo "systemd-resolved: $(systemctl is-active systemd-resolved 2>/dev/null)"
  return 0
}

up() {
  guard
  command -v python3 >/dev/null || die "needs python3 for the forwarder"
  local up_ns target
  if [ -s "$STATE/upstream" ]; then up_ns=$(cat "$STATE/upstream")
  else
    up_ns=${KIRI_SHAPE_UPSTREAM:-$(upstream_in /run/systemd/resolve/resolv.conf)}
    [ -n "$up_ns" ] || up_ns=$(upstream_in /etc/resolv.conf)
    [ -n "$up_ns" ] || die "no upstream resolver found; set KIRI_SHAPE_UPSTREAM"
  fi
  if has_addr && forwarder_up && mounted; then say "already up"; status; return 0; fi
  # The file the mount covers: /etc/resolv.conf, or where its symlink leads.
  target=$(readlink -f /etc/resolv.conf)
  [ -f "$target" ] || die "/etc/resolv.conf leads to $target, which is not a file"
  say "changes: address $ADDR on lo; a DNS forwarder on $ADDR:53 to $up_ns; a resolv.conf naming only $ADDR mounted over $target (the /etc/resolv.conf in use); undo with: down"
  mkdir -p "$STATE"
  echo "$up_ns" > "$STATE/upstream"
  command -v systemctl >/dev/null && systemctl is-active -q systemd-resolved 2>/dev/null && touch "$STATE/resolved-was-active"
  if ! has_addr; then
    touch "$STATE/addr-added"
    ip addr add "$ADDR/32" dev lo || die "could not add $ADDR to lo"
  elif [ ! -e "$STATE/addr-added" ]; then
    say "$ADDR was already on lo: left there on down"
  fi
  if ! forwarder_up; then
    printf '%s\n' "$FORWARDER" > "$STATE/forwarder.py"
    setsid python3 "$STATE/forwarder.py" "$ADDR" "$up_ns" > "$STATE/forwarder.log" 2>&1 < /dev/null &
    echo $! > "$STATE/forwarder.pid"
    local ok=
    for _ in $(seq 20); do python3 -c "$PROBE" "$ADDR" 2>/dev/null && { ok=1; break; }; sleep 0.25; done
    [ -n "$ok" ] || { say "the forwarder does not answer: $(tail -2 "$STATE/forwarder.log")"; down; exit 1; }
  fi
  if ! grep -qs " $target " /proc/self/mountinfo; then
    printf '# made by tests/lib/wsl-shape.sh, as WSL 2 generates it\nnameserver %s\n' "$ADDR" > "$STATE/resolv.conf"
    echo "$target" > "$STATE/mounted-on"
    mount --bind "$STATE/resolv.conf" "$target" || { say "could not mount over $target"; down; exit 1; }
  fi
  status
}

down() {
  guard
  [ -d "$STATE" ] || { say "nothing to undo"; status; return 0; }
  say "putting the host back"
  local target
  if [ -s "$STATE/mounted-on" ]; then
    target=$(cat "$STATE/mounted-on")
    while grep -qs " $target " /proc/self/mountinfo; do umount "$target" || die "could not unmount $target"; done
  fi
  if [ -s "$STATE/forwarder.pid" ]; then
    kill "$(cat "$STATE/forwarder.pid")" 2>/dev/null
    for _ in $(seq 20); do forwarder_up || break; sleep 0.1; done
  fi
  if [ -e "$STATE/addr-added" ] && has_addr; then ip addr del "$ADDR/32" dev lo || die "could not remove $ADDR from lo"; fi
  # systemd-resolved reads a resolv.conf that is not its own as servers of
  # its own; it kept the shaped address after the file came back.
  if [ -e "$STATE/resolved-was-active" ] && command -v resolvectl >/dev/null && resolvectl dns 2>/dev/null | grep -qF "$ADDR"; then
    say "systemd-resolved took $ADDR as a server: restarting it"
    systemctl restart systemd-resolved
  fi
  rm -rf "$STATE"
  status
}

case ${1:-} in
  up) up ;;
  down) down ;;
  status) status ;;
  *) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
