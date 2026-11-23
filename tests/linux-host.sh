#!/bin/bash
# Kiri on a Linux host: runs the Linux build (cargo xtask linux) as the host
# engine inside a privileged container on the current Docker engine (Kiri
# works), then drives it with the docker CLI from inside. No VM is involved:
# containers run on that Linux kernel directly.
set -u
D=${D:-docker}
BIN=${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/linux/release}
# The engine's container (and its volumes): another name runs a second copy beside this one.
N=${N:-kiri-linux-host}
[ -x "$BIN/kiri" ] || { echo "build first: cargo xtask linux"; exit 1; }
# Images come from KIRI_TEST_REGISTRY (tests/lib/images.sh), never from Docker
# Hub: this engine's alpine here, the inner engine's once it answers.
# shellcheck source=SCRIPTDIR/lib/images.sh
. "$(dirname "$0")/lib/images.sh"
kiri_seed_images alpine:latest || echo "linux-host: alpine could not be fetched"
$D rm -f "$N" >/dev/null 2>&1
$D run -d --name "$N" --privileged --tmpfs /run -v "$N":/var/lib/kiri -v "$N"-store:/root/.local/share/kiri \
  -v "$BIN:/opt/kiri:ro" alpine sh -c 'apk add -q docker-cli curl python3 >/dev/null 2>&1; exec /opt/kiri/kiri daemon' >/dev/null
# Up to 3 minutes: the container installs its tools with apk first, which can take over a minute.
up() { for _ in $(seq 180); do $D exec "$N" sh -c 'DOCKER_HOST=unix:///root/.kiri/run/docker.sock docker version' >/dev/null 2>&1 && break; sleep 1; done; }
up
$D cp "$(dirname "$0")/testcontainers-api.py" "$N":/testcontainers-api.py >/dev/null
$D cp "$(dirname "$0")/lib/images.sh" "$N":/images.sh >/dev/null
$D exec -e KIRI_TEST_REGISTRY="$KIRI_REG" "$N" sh -c '. /images.sh; export DOCKER_HOST=unix:///root/.kiri/run/docker.sock; kiri_seed_images $KIRI_IMAGES_LINUX_HOST' ||
  echo "linux-host: an image could not be fetched; the checks that run it will fail"
PASS=0; FAIL=0
# A failing check that guards a known bug prints which (tests/regressions.tsv).
# shellcheck source=SCRIPTDIR/lib/regressions.sh
. "$(dirname "$0")/lib/regressions.sh"
t() {
  local name=$1 want=$2; shift 2
  local got; got=$($D exec "$N" sh -c "export DOCKER_HOST=unix:///root/.kiri/run/docker.sock; $*" 2>&1 | tr -d '\r')
  if [[ "$got" == *"$want"* ]]; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); printf '  FAIL %s\n    want: %s\n    got:  %s\n' "$name" "$want" "$(echo "$got" | head -3)"; kiri_guard "$name"; fi
}
t server-os "linux" "docker version --format '{{.Server.Os}}'"
# NCPU is the CPUs the engine may run on, as dockerd counts them (#659): it
# was the Mac VM's default of 2. The engine runs in this container, with its
# CPUs. A second engine started under taskset with three CPUs (in mount and
# network namespaces of its own, so it shares nothing with the first) says 3,
# where the old count said 2; a host with fewer than three CPUs can't tell.
t ncpu-engine-cpus "same" "[ \"\$(docker info --format '{{.NCPU}}')\" = \"\$(nproc)\" ] && echo same || echo \"NCPU \$(docker info --format '{{.NCPU}}') nproc \$(nproc)\""
if [ "$($D exec "$N" nproc)" -ge 3 ]; then
  t ncpu-affinity "NCPU=3" "apk add -q util-linux util-linux-misc iproute2 >/dev/null 2>&1; unshare --mount --propagation private --net --fork sh -c 'mount -t tmpfs t /var/lib/kiri && mount -t tmpfs t /root && ip link set lo up; taskset -c 0-2 /opt/kiri/kiri daemon >/dev/null 2>&1 & p=\$!; for i in \$(seq 80); do docker version >/dev/null 2>&1 && break; sleep 0.25; done; echo NCPU=\$(docker info --format {{.NCPU}}); kill \$p; wait \$p'"
else
  echo "  SKIP ncpu-affinity: the engine's container has fewer than 3 CPUs"
fi
t run "3." "docker run --rm alpine cat /etc/alpine-release"
t home "/root" "docker run --rm alpine sh -c 'echo \$HOME'"
t internet "Example Domain" "docker run --rm alpine wget -qO- -T 10 http://example.com"
t port "Welcome to nginx" "docker run -d --name web -p 18081:80 nginx:alpine >/dev/null; for i in \$(seq 30); do curl -s localhost:18081 && break; sleep 0.3; done"
t exec "nginx version" "docker exec web nginx -v"
t port-inspect '{"80/tcp":[{"HostIp":"","HostPort":"18081"}]}' "docker inspect -f '{{json .HostConfig.PortBindings}}' web"
# What the registry serves now, for update checkers (#257). Asked of
# KIRI_TEST_REGISTRY, which serves Docker Hub's official images as they are.
t distribution '"mediaType":"application/vnd.oci.image.index.v1+json"' "curl -s --unix-socket /root/.kiri/run/docker.sock http://d/v1.47/distribution/$KIRI_REG/alpine:3.21/json"
t distribution-missing "404" "curl -s -o /dev/null -w '%{http_code}' --unix-socket /root/.kiri/run/docker.sock http://d/v1.47/distribution/$KIRI_REG/no-such-image-kiri-test/json"
# A copy into a created container, then its start, mounts the same upper
# dir twice in a row: kernels with overlayfs index on (Arch) refused the
# second with EBUSY (#227). A renamed directory must survive a commit.
t cp-then-start "failed 0" "f=0; for i in 1 2 3 4 5; do id=\$(docker create alpine sleep 30); docker cp /etc/hostname \$id:/f >/dev/null 2>&1 || f=\$((f+1)); docker start \$id >/dev/null 2>&1 || f=\$((f+1)); docker rm -f \$id >/dev/null; done; echo failed \$f"
# A copy still running when its container starts shares the container's
# root mount: two overlay mounts of one writable layer at once are undefined
# behaviour to the kernel (#227). slow-tar holds the copy open for 3 s.
$D exec -i "$N" sh -c 'cat > /slow-tar.py' <<'PY'
import sys, tarfile, time
t = tarfile.TarInfo("slow")
t.size = 2 << 20
o = sys.stdout.buffer
o.write(t.tobuf())
o.write(b"x" * (1 << 20))
o.flush()
time.sleep(3)
o.write(b"x" * (1 << 20) + bytes(1024))
PY
mounts='cat /proc/[0-9]*/mountinfo 2>/dev/null | grep "$id/upper" | cut -d" " -f3 | sort -u | wc -l'
t cp-during-start-one-mount "during=1 copied=2097152 after=0" "id=\$(docker create alpine sleep 30); python3 /slow-tar.py | docker cp - \$id:/ & sleep 1; docker start \$id >/dev/null; d=\$($mounts); wait; c=\$(docker exec \$id stat -c %s /slow); docker rm -f \$id >/dev/null; for i in \$(seq 20); do [ \$($mounts) = 0 ] && break; sleep 0.1; done; echo during=\$d copied=\$c after=\$($mounts)"
# A restart while a copy into the running container still writes: both
# finish and nothing stays mounted. (The copy then holds the root, so the
# restart shares it; mountinfo can't show an old mount whose namespace has
# gone, so roots.rs's unit tests check that part.)
t cp-during-restart "during=1 after=0" "id=\$(docker run -d alpine sleep 60); python3 /slow-tar.py | docker cp - \$id:/tmp & sleep 1; docker restart -t 0 \$id >/dev/null; d=\$($mounts); wait; docker rm -f \$id >/dev/null; for i in \$(seq 20); do [ \$($mounts) = 0 ] && break; sleep 0.1; done; echo during=\$d after=\$($mounts)"
# A start that fails once the container's process exists gives its root
# back: rm then deletes it and leaves nothing mounted.
t failed-start-frees-root "gone mounts=0" "docker rm -f kiri-badsysctl >/dev/null 2>&1; docker run -d --name kiri-badsysctl --sysctl net.ipv4.kiri_no_such_key=1 alpine true >/dev/null 2>&1; id=\$(docker inspect -f '{{.Id}}' kiri-badsysctl); docker rm -f kiri-badsysctl >/dev/null; for i in \$(seq 20); do test -e /var/lib/kiri/containers/\$id || break; sleep 0.1; done; test -e /var/lib/kiri/containers/\$id && printf kept || printf gone; echo \" mounts=\$($mounts)\""
# ...and leaves no name in the network's DNS (#325).
t failed-start-no-dns "none" "docker network create kiri-dnsfail >/dev/null 2>&1; docker rm -f kiri-dnsbad >/dev/null 2>&1; docker run -d --name kiri-dnsbad --network kiri-dnsfail --sysctl net.ipv4.kiri_no_such_key=1 alpine sleep 60 >/dev/null 2>&1; docker run --rm --network kiri-dnsfail alpine sh -c 'nslookup kiri-dnsbad 2>&1 | grep -qE \"^Address: [0-9.]+\$\" && echo answered || echo none'; docker rm -f kiri-dnsbad >/dev/null 2>&1; docker network rm kiri-dnsfail >/dev/null"
t cp-stopped-unmounts "after=0" "id=\$(docker create alpine true); docker cp /etc/hostname \$id:/f >/dev/null; for i in \$(seq 20); do [ \$($mounts) = 0 ] && break; sleep 0.1; done; echo after=\$($mounts); docker rm \$id >/dev/null"
t commit-renamed-dir "kept" "docker rm -f kiri-rename >/dev/null 2>&1; docker run --name kiri-rename alpine mv /etc/apk /etc/apk2; docker commit kiri-rename kiri-rename-test >/dev/null; docker rm -f kiri-rename >/dev/null; docker run --rm kiri-rename-test sh -c 'test -f /etc/apk2/repositories && echo kept'; docker rmi kiri-rename-test >/dev/null"
# A container and an exec each lead their own session, so killpg from inside
# never reaches the daemon (#191).
t session-own "SID=1" "docker run --rm alpine sh -c 'echo SID=\$(cut -d\" \" -f6 /proc/1/stat)'"
t exec-session-own "own" "docker exec web sh -c 's=\$(cut -d\" \" -f6 /proc/\$\$/stat); [ \"\$s\" = \"\$\$\" ] && echo own || echo \"session \$s\"'"
t cp "copied" "echo copied > /tmp/x && docker cp /tmp/x web:/tmp/x && docker exec web cat /tmp/x"
# What Testcontainers needs (#222, #121); see tests/testcontainers-api.py.
for c in version cmd keepalive close follow follow-created timestamps; do
  t "testcontainers-$c" "ok $c" "for i in \$(seq 60); do command -v python3 >/dev/null && break; sleep 1; done; python3 /testcontainers-api.py /root/.kiri/run/docker.sock $c"
done
t socket-mount-reaches-kiri "Kiri" "docker run --rm -v /root/.kiri/run/docker.sock:/var/run/docker.sock docker:cli docker version --format '{{.Server.Platform.Name}}'"
# A container given the engine's socket uses Docker's API, not Kiri's own
# settings: the same CLI that changes them here is refused from inside one.
t socket-mount-settings-refused "not from a container" "docker run --rm -v /opt/kiri/kiri:/kiri:ro -v /root/.kiri/run/docker.sock:/var/run/docker.sock -e KIRI_SOCKET=/var/run/docker.sock alpine /kiri registry insecure add 10.9.9.9:5000 2>&1"
t stats "web" "docker stats --no-stream --format '{{.Name}}'"
t network-dns "PONG" "docker network create lhn >/dev/null 2>&1; docker run -d --name db --network lhn redis:7-alpine >/dev/null; sleep 1; docker run --rm --network lhn redis:7-alpine redis-cli -h db ping"
t bind-host-path "3." "docker run --rm -v /etc:/h:ro alpine cat /h/alpine-release"
t volume "kept" "docker run --rm -v lhv:/d alpine sh -c 'echo kept > /d/f' && docker run --rm -v lhv:/d alpine cat /d/f"
# A fresh named volume is filled from the image while the container starts.
# `docker run -d` must not return before the container's root is in place, or
# an immediate exec resolves users against the host's /etc/passwd (#311).
t volume-user-ready "passed passed passed" "docker run --name ready-seed alpine sh -c 'adduser -D dev && mkdir /payload && dd if=/dev/zero of=/payload/blob bs=1M count=64' >/dev/null && docker commit -c 'USER dev' ready-seed kiri-test/ready:1 >/dev/null && docker rm -f ready-seed >/dev/null; for i in 1 2 3; do v=ready-vol-\$i; c=\$(docker run -d --mount type=volume,source=\$v,target=/payload kiri-test/ready:1 sleep 60); if docker exec --user dev \$c id >/dev/null 2>&1; then printf 'passed '; else printf 'failed '; fi; docker rm -f \$c >/dev/null; docker volume rm \$v >/dev/null 2>&1; done; docker rmi kiri-test/ready:1 >/dev/null; echo"
# A start that fills a new volume from a big image folder still waits for the
# fill (#311), but the engine keeps serving every other container meanwhile;
# an exec sent to it meanwhile waits for it, a pause, a commit and a
# container joining its network sent meanwhile all finish, and a start that
# fails during the fill is still a start error with a whole volume
# (tests/start-stall.sh).
$D cp "$(dirname "$0")/start-stall.sh" "$N":/start-stall.sh >/dev/null
# Here the engine's own root is the test's: a copy into the container while
# it is still starting must not land in it.
t start-stall-exec "pass:" "sh /start-stall.sh cp"
t memory-limit "67108864" "docker run --rm --memory 64m alpine cat /sys/fs/cgroup/memory.max"
# A working directory through one of the agent's descriptors is refused, and
# before that nothing is made outside the container: the missing directory
# was created as root while the container's cgroup directory was still open.
t workdir-made-inside-root "outside 0" "find /sys/fs/cgroup /var/lib/kiri /run -name kiri-wd-probe -exec rmdir {} + 2>/dev/null; for n in \$(seq 3 25); do for p in kiri-wd-probe ../kiri-wd-probe; do docker run --rm -w /proc/self/fd/\$n/\$p alpine true >/dev/null 2>&1; done; done; echo outside \$(find /sys/fs/cgroup /var/lib/kiri /run -name kiri-wd-probe 2>/dev/null | wc -l)"
t seccomp "Seccomp:	2" "docker run --rm alpine grep Seccomp: /proc/self/status"
t udp "10.9.8.7" "docker run -d --name dns -p 15354:53/udp alpine sh -c 'apk add -q dnsmasq >/dev/null 2>&1; dnsmasq -k --address=/kiri.test/10.9.8.7' >/dev/null; for i in \$(seq 40); do out=\$(nslookup -port=15354 kiri.test 127.0.0.1 2>/dev/null) && break; sleep 0.5; done; echo \"\$out\" | grep -o 10.9.8.7"
t ipv6-network "true" "docker network create --ipv6 --subnet fd00:4c:1::/64 lh6 >/dev/null && docker network inspect lh6 -f '{{.EnableIPv6}}'"
t ipv6-ping "1 packets received" "docker run -d --name v6a --network lh6 nginx:alpine >/dev/null; docker run --rm --network lh6 alpine ping -6 -c 1 -W 2 v6a"
t ipv6-wget "Welcome to nginx" "docker run --rm --network lh6 alpine wget -qO- -T 5 'http://[fd00:4c:1::2]'"
t ipv6-host-nat "masquerade" "apk add -q nftables >/dev/null 2>&1; nft list table ip6 kiri"
t ipv6-port "Welcome to nginx" "docker run -d --name web6 -p 18082:80 nginx:alpine >/dev/null; for i in \$(seq 30); do curl -sg 'http://[::1]:18082' && break; sleep 0.3; done"
t ipv6-cleanup "gone" "docker rm -f v6a web6 >/dev/null; docker network rm lh6 >/dev/null; for i in \$(seq 20); do nft list table ip6 kiri >/dev/null 2>&1 || { echo gone; break; }; sleep 0.1; done"
t ipv6-forwarding-restored "0" "cat /proc/sys/net/ipv6/conf/all/forwarding"
# A build's --add-host host-gateway is the bridge gateway, as for docker run (not the Mac VM's 198.19.248.254).
t build-host-gateway "GW-OK" "gw=\$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}'); mkdir -p /bhg && printf 'FROM $KIRI_REG/alpine:3.21\\nRUN grep -w hgw /etc/hosts\\n' > /bhg/Dockerfile && DOCKER_BUILDKIT=0 docker build --no-cache --add-host hgw:host-gateway /bhg 2>&1 | grep -q \"\$gw\" && echo GW-OK"
# A privileged container must not touch the host's /dev. The kernel has one
# devtmpfs, so mount it, put ptmx back as the device it should be, run a
# privileged container and look again: once, ptmx became a link into the
# host's /dev/pts and no user on the machine could open a terminal.
t privileged-host-ptmx "character special file" "mkdir -p /hostdev && mount -t devtmpfs none /hostdev && { [ -c /hostdev/ptmx ] || { rm -f /hostdev/ptmx && mknod -m 666 /hostdev/ptmx c 5 2 && chgrp 5 /hostdev/ptmx; }; } && docker run --rm --privileged alpine true && stat -c %F /hostdev/ptmx; umount /hostdev"
t privileged-devices "DEVS-OK" "docker run --rm --privileged alpine sh -c 'test -c /dev/kmsg && test -c /dev/null && test -L /dev/ptmx && test -c /dev/pts/ptmx && echo DEVS-OK'"
t privileged-tty "/dev/pts/0" "docker run --rm -t --privileged alpine tty"
t cleanup "web" "docker rm -f web db dns"
# Kiri's Stop is the engine's stop, not the user's, as stopping Docker's
# daemon is: Start, also after the engine restarted, brings back what
# `always` and `unless-stopped` kept running, but not one the user stopped
# (docker stop) while Kiri was stopped, which answers 204 (#181, #216).
POST="curl -s -o /dev/null -w %{http_code} --unix-socket /root/.kiri/run/docker.sock -X POST"
RUNNING="docker inspect -f '{{.Name}}={{.State.Running}}' ss-us ss-al ss-user ss-no | tr -d / | tr '\\n' ' '"
BACK="for i in \$(seq 100); do [ \"\$(docker inspect -f '{{.State.Running}}' ss-us ss-al | tr '\\n' ' ')\" = 'true true ' ] && break; sleep 0.2; done; $RUNNING"
ALL='ss-us ss-al ss-user ss-no ss-al1 ss-al2 ss-al3 ss-al4 ss-al5'
t ss-setup "ok" "docker run -d --name ss-us --restart unless-stopped alpine sleep 1d && docker run -d --name ss-al --restart always alpine sleep 1d && docker run -d --name ss-user --restart unless-stopped alpine sleep 1d && docker run -d --name ss-no alpine sleep 1d && for i in 1 2 3 4 5; do docker run -d --name ss-al\$i --restart always alpine sleep 1d || exit; done >/dev/null && echo ok"
# All at once, as a daemon shutdown stops them: nine `sleep 1d`s (PID 1
# ignores SIGTERM) take one 10 s timeout, not nine.
t ss-stop "204 in one timeout" "s=\$(date +%s); c=\$($POST http://d/kiri/stop); e=\$(( \$(date +%s) - s )); [ \$e -lt 25 ] && echo \"\$c in one timeout\" || echo \"\$c in \${e}s\""
t ss-stop-all-down "ss-us=false ss-al=false ss-user=false ss-no=false" "$RUNNING"
t ss-docker-stop-while-stopped "204" "$POST http://d/containers/ss-user/stop"
t ss-start "204" "$POST http://d/kiri/wake"
t ss-start-brings-back "ss-us=true ss-al=true ss-user=false ss-no=false" "$BACK"
# Start, then Stop at once, after the engine restarted: the restarts under
# way stop too, and nothing runs while Kiri is stopped (#181).
t ss-stop-again "204" "$POST http://d/kiri/stop"
$D restart -t 20 "$N" >/dev/null; up
t ss-start-then-stop "204 204" "echo \$($POST http://d/kiri/wake) \$($POST http://d/kiri/stop)"
t ss-start-then-stop-none-run "running: 0" "sleep 8; echo running: \$(docker ps -q | wc -l)"
t ss-start-then-stop-refused "Kiri is stopped" "docker run --rm alpine true"
t ss-restarted-start "204" "$POST http://d/kiri/wake"
t ss-restarted-brings-back "ss-us=true ss-al=true ss-user=false ss-no=false" "$BACK"
t ss-restarted-all-always "running: 7" "for i in \$(seq 100); do [ \$(docker ps -q | wc -l) -ge 7 ] && break; sleep 0.2; done; echo running: \$(docker ps -q | wc -l)"
t ss-cleanup "ss-us" "docker rm -f $ALL"
# Docker took a subnet Kiri had chosen while Kiri was stopped (or before a
# network's first container, when Kiri makes its bridge): its docker0 or
# br-… holds the address, with no carrier yet. Kiri's network moves to a
# free subnet, and its containers reach their gateway, DNS and the internet,
# not the other engine's bridge; one whose --subnet the user gave is refused
# with the reason (#489). The default bridge's subnet is read, not assumed:
# this engine's container is itself on a 172.17 bridge.
GW="docker network inspect -f '{{(index .IPAM.Config 0).Gateway}}'"
t taken-setup "ok" "docker network create lh489 >/dev/null && docker network create --subnet 10.89.240.0/24 lh489u >/dev/null && echo ok"
OLD_BRIDGE=$($D exec "$N" sh -c "DOCKER_HOST=unix:///root/.kiri/run/docker.sock $GW bridge")
OLD_POOL=$($D exec "$N" sh -c "DOCKER_HOST=unix:///root/.kiri/run/docker.sock $GW lh489")
t taken-stop "204" "$POST http://d/kiri/stop"
t taken-by-other-bridges "ok" "apk add -q iproute2 >/dev/null 2>&1; for b in docker489:$OLD_BRIDGE/16 br489:$OLD_POOL/16 br489u:10.89.240.1/24; do ip link add \${b%%:*} type bridge && ip addr add \${b#*:} dev \${b%%:*} && ip link set \${b%%:*} up || exit; done; echo ok"
t taken-wake "204" "$POST http://d/kiri/wake"
REACH="gw=\\\$(ip route | awk '/^default/ {print \\\$3}'); ping -c1 -W2 \\\$gw >/dev/null && r=gateway-ok || r=gateway-unreachable; echo \\\$r \\\$gw"
t taken-default-bridge-moves "gateway-ok" "docker run --rm alpine sh -c \"$REACH\" | grep -v ' $OLD_BRIDGE\$'"
t taken-default-bridge-internet "Example Domain" "docker run --rm alpine wget -qO- -T 10 http://example.com"
t taken-pool-network-moves "gateway-ok" "docker run --rm --network lh489 alpine sh -c \"$REACH\" | grep -v ' $OLD_POOL\$'"
t taken-user-subnet-refused "is also on this host's interface br489u" "docker run --rm --network lh489u alpine true"
t taken-cleanup "ok" "docker network rm lh489 lh489u >/dev/null; ip link del docker489; ip link del br489; ip link del br489u; echo ok"
# A command that may not be run is 126, a missing one 127, as with Docker (#183).
t exit-126-not-executable "126" "docker run --rm alpine /etc/passwd; echo \$?"
t not-executable-msg 'exec: "/etc/passwd": permission denied' "docker run --rm alpine /etc/passwd"
t exit-127-missing "127" "docker run --rm alpine /no/such/file; echo \$?"
# Linux refuses a directory with EACCES; runc names it.
t dir-msg 'exec: "/etc": is a directory: permission denied' "docker run --rm alpine /etc"
t exec-dir-msg 'exec: "/etc": is a directory: permission denied' "docker run -d --name xd alpine sleep 60 >/dev/null; docker exec xd /etc; docker rm -f xd >/dev/null"
# runc's exec fails after the start: 255, in its own words on stderr. tini
# (--init) hands a script without a shebang to /bin/sh, as execvp does.
XS="mkdir -p /xs && printf 'echo hi\\n' > /xs/nosb && printf '#!/no/such/sh\\necho hi\\n' > /xs/badsb && chmod +x /xs/*"
t no-shebang "exec /xs/nosb: exec format error 255" "$XS; o=\$(docker run --rm -v /xs:/xs alpine /xs/nosb 2>&1); echo \"\$o \$?\""
t missing-interpreter "exec /xs/badsb: no such file or directory 255" "$XS; o=\$(docker run --rm -v /xs:/xs alpine /xs/badsb 2>&1); echo \"\$o \$?\""
t exec-no-shebang "exec /xs/nosb: exec format error 255" "$XS; docker run -d --name xsb -v /xs:/xs alpine sleep 60 >/dev/null; o=\$(docker exec xsb /xs/nosb 2>&1); echo \"\$o \$?\"; docker rm -f xsb >/dev/null"
t init-no-shebang "hi 0" "$XS; o=\$(docker run --rm --init -v /xs:/xs alpine /xs/nosb 2>&1); echo \"\$o \$?\""
t init-missing-interpreter "127" "$XS; docker run --rm --init -v /xs:/xs alpine /xs/badsb >/dev/null 2>&1; echo \$?"
t exit-126-exec "126" "docker run -d --name x126 alpine sleep 60 >/dev/null; docker exec x126 /etc/passwd; echo \$?; docker rm -f x126 >/dev/null"
# The engine stopped with containers running (systemctl stop kiri): a stop
# that was asked for, "the VM stopped" at info, not an ERROR.
t clean-stop-setup "ok" "docker run -d --name cs alpine sleep 1d >/dev/null && echo ok"
$D restart -t 20 "$N" >/dev/null; up
if $D logs "$N" 2>&1 | grep -q 'stopped unexpectedly'; then FAIL=$((FAIL + 1)); echo "  FAIL clean-stop-not-error: $($D logs "$N" 2>&1 | grep 'stopped unexpectedly' | head -1)"; else PASS=$((PASS + 1)); fi
t start-after-service-restart "true" "docker start cs >/dev/null && docker inspect -f '{{.State.Running}}' cs"
t clean-stop-cleanup "cs" "docker rm -f cs"
# The VM (here the agent, kiri-init under the daemon) dies with nobody asking
# (#50): `always` and `unless-stopped` come back by themselves, with no
# request to boot a new one; no policy, or a `docker stop`, stays down.
t crash-setup "ok" "for p in always unless-stopped no; do docker run -d --name cr-\$p --restart \$p alpine sleep 1d >/dev/null; done; docker run -d --name cr-user --restart always alpine sleep 1d >/dev/null; docker stop -t 0 cr-user >/dev/null && echo ok"
t crash-kill "killed" "kill -9 \$(ps -o pid,ppid,comm | awk '\$2 == 1 && \$3 == \"kiri-init\" {print \$1}') && echo killed"
t crash-recovers "cr-always=true cr-unless-stopped=true cr-no=false cr-user=false" "sleep 4; for c in cr-always cr-unless-stopped cr-no cr-user; do printf '%s=%s ' \$c \$(docker inspect -f '{{.State.Running}}' \$c); done"
t crash-restart-count "1 1" "echo \$(docker inspect -f '{{.RestartCount}}' cr-always cr-unless-stopped)"
# Removing a container the crash stopped, or one never started, deletes its
# writable layer from the engine's disk.
t crash-rm-frees-layer "gone" "id=\$(docker inspect -f '{{.Id}}' cr-no); docker rm cr-no >/dev/null; for i in \$(seq 20); do test -e /var/lib/kiri/containers/\$id || break; sleep 0.1; done; test -e /var/lib/kiri/containers/\$id && echo kept || echo gone"
t created-rm-frees-layer "gone" "id=\$(docker create alpine true); docker cp /etc/hostname \$id:/f >/dev/null; docker rm \$id >/dev/null; for i in \$(seq 20); do test -e /var/lib/kiri/containers/\$id || break; sleep 0.1; done; test -e /var/lib/kiri/containers/\$id && echo kept || echo gone"
t crash-cleanup "cr-always" "docker rm -f cr-always cr-unless-stopped cr-user"
# `docker system df` measures the engine's disk in a child process of the
# agent, never on a thread: the agent starts containers with clone3, and a
# start that raced a measuring thread hung for good (its first process
# inherited the allocator's lock held by that thread), and so did its stop,
# exec and rm. A volume of 20,000 files makes each measurement take a while;
# 100 starts run beside four loops of system df, each start given 15 s.
t starts-beside-df "stuck 0" "docker run --rm -v dfwalk:/d alpine sh -c 'cd /d; for i in \$(seq 200); do mkdir \$i; for j in \$(seq 100); do : > \$i/\$j; done; done'; S=/root/.kiri/run/docker.sock; rm -f /tmp/dfstop; l() { while [ ! -e /tmp/dfstop ]; do curl -s -m 30 -o /dev/null --unix-socket \$S http://d/system/df; done; }; l & l & l & l & s=0; for i in \$(seq 100); do timeout -s KILL 15 docker run -d --name dfs\$i alpine true >/dev/null 2>&1; [ \$? = 137 ] && s=\$((s+1)); done; touch /tmp/dfstop; wait; for i in \$(seq 100); do timeout -s KILL 5 docker rm -f dfs\$i >/dev/null 2>&1; done; docker volume rm dfwalk >/dev/null; echo stuck \$s"
# A start whose first process never gets its root in place (here frozen by
# the cgroup freezer from its first instruction, as a child that hung there
# was) held the container's lifecycle lock for good: rm -f, kill and stop
# never answered. rm -f waits 2 s for it, then cancels it (the child is
# killed, the start fails) and leaves no cgroup, mount or container behind.
t rm-f-stuck-start "removed, leftovers: none" "cg=\$(docker run -d alpine sleep 60); p=\$(docker inspect -f '{{.State.Pid}}' \$cg); K=/sys/fs/cgroup\$(sed -n 's/^0:://p' /proc/\$p/cgroup); K=\${K%/*}; docker rm -f \$cg >/dev/null; id=\$(docker create alpine true); echo 1 > \$K/cgroup.freeze; (timeout -s KILL 30 docker start \$id >/dev/null 2>&1 &); sleep 2; t0=\$(date +%s); timeout -s KILL 15 docker rm -f \$id >/dev/null 2>&1; rc=\$?; t=\$(( \$(date +%s) - t0 )); echo 0 > \$K/cgroup.freeze; sleep 1; left=; [ -d \$K/\$id ] && left=\"\$left cgroup\"; grep -q \$id /proc/mounts && left=\"\$left mount\"; docker ps -aq --no-trunc | grep -q \$id && left=\"\$left container\"; [ \$rc = 0 ] && [ \$t -le 5 ] && echo \"removed, leftovers:\${left:- none}\" || echo \"rm -f rc=\$rc after \$t s\""
# The same stuck start met by a kill: a kill (or stop) that comes while a
# container starts waits for the start, as Docker's does, so a healthy start
# answers 204 and the signal then reaches the process. One still unfinished
# after 10 s is taken as stuck and cancelled as rm -f cancels it: the kill
# answers, the start answers 409, the exit code is a kill's (137), and
# nothing is left behind.
t kill-stuck-start "killed after the wait, start refused, exited 137, leftovers: none" "cg=\$(docker run -d alpine sleep 60); p=\$(docker inspect -f '{{.State.Pid}}' \$cg); K=/sys/fs/cgroup\$(sed -n 's/^0:://p' /proc/\$p/cgroup); K=\${K%/*}; docker rm -f \$cg >/dev/null; id=\$(docker create alpine true); echo 1 > \$K/cgroup.freeze; (timeout -s KILL 40 docker start \$id > /tmp/kss.out 2>&1 &); sleep 2; t0=\$(date +%s); timeout -s KILL 30 docker kill \$id >/dev/null 2>&1; rc=\$?; t=\$(( \$(date +%s) - t0 )); echo 0 > \$K/cgroup.freeze; sleep 1; left=; [ -d \$K/\$id ] && left=\"\$left cgroup\"; grep -q \$id /proc/mounts && left=\"\$left mount\"; st=\$(docker inspect -f '{{.State.Status}} {{.State.ExitCode}}' \$id); grep -q 'stopped before it finished starting' /tmp/kss.out && s='start refused' || s=\"start said: \$(head -1 /tmp/kss.out)\"; docker rm -f \$id >/dev/null; [ \$rc = 0 ] && [ \$t -ge 9 ] && [ \$t -le 15 ] && echo \"killed after the wait, \$s, \$st, leftovers:\${left:- none}\" || echo \"kill rc=\$rc after \$t s\""
# The agent dies while a container's first process it cloned is stuck
# before closing what it inherited (frozen from its first instruction):
# that child held the agent's end of the daemon's connection, so nothing
# heard the agent was gone and every request hung, and its lock, so no new
# agent could start. Its exit now ends the connection, and the next agent
# stops what the old one left and takes the lock: a run answers again.
t agent-dies-beside-stuck-start "answered, leftovers: none" "cg=\$(docker run -d alpine sleep 60); p=\$(docker inspect -f '{{.State.Pid}}' \$cg); K=/sys/fs/cgroup\$(sed -n 's/^0:://p' /proc/\$p/cgroup); K=\${K%/*}; docker rm -f \$cg >/dev/null; id=\$(docker create alpine true); echo 1 > \$K/cgroup.freeze; (timeout -s KILL 60 docker start \$id >/dev/null 2>&1 &); for i in \$(seq 50); do [ -s \$K/\$id/cgroup.procs ] && break; sleep 0.1; done; c=\$(cat \$K/\$id/cgroup.procs); echo 1 > \$K/\$id/cgroup.freeze; echo 0 > \$K/cgroup.freeze; kill -9 \$(pgrep kiri-init | grep -vx \"\$c\" | head -1); t0=\$(date +%s); rc=1; while [ \$(( \$(date +%s) - t0 )) -lt 30 ]; do timeout -s KILL 30 docker run --rm alpine true >/dev/null 2>&1 && { rc=0; break; }; sleep 0.5; done; t=\$(( \$(date +%s) - t0 )); left=; [ -d \$K/\$id ] && { left=\"\$left cgroup\"; echo 0 > \$K/\$id/cgroup.freeze; }; docker rm -f \$id >/dev/null 2>&1; [ \$rc = 0 ] && [ \$t -le 10 ] && echo \"answered, leftovers:\${left:- none}\" || echo \"no answer after \$t s, leftovers:\$left\""
# A containers.json cut short (a crash, a full disk) is kept aside as
# containers.json.corrupt-*, the records still readable are loaded, and
# nothing is deleted because of the damaged list: no log folder and no
# writable layer, also once another container has started and after the
# next start, which finds a readable but shorter list (#438).
ST=/root/.local/share/kiri
CJ="export DOCKER_HOST=unix:///root/.kiri/run/docker.sock"
t cj-setup "ok" "docker run --name cj1 -v cjv1:/v alpine sh -c 'echo line-one; echo precious > /data' >/dev/null && docker run --name cj2 -v cjv2:/v alpine sh -c 'echo line-two; echo precious > /data' >/dev/null && echo ok"
CJ_IDS=$($D exec "$N" sh -c "$CJ; docker inspect -f '{{.Id}}' cj1 cj2" | tr '\n' ' ')
$D stop -t 20 "$N" >/dev/null
# Cut inside the record of whichever of the two comes later in the file, so
# the other one can still be read; print that one's id.
CJ_READ=$($D run --rm -i --name "$N-edit" -e "IDS=$CJ_IDS" -v "$N"-store:/s alpine sh <<'SH'
f=/s/containers.json
set -- $IDS
a=$(awk -v id="$1" '{print index($0, "{\"id\":\"" id "\"")}' $f)
b=$(awk -v id="$2" '{print index($0, "{\"id\":\"" id "\"")}' $f)
[ "$a" -gt 0 ] && [ "$b" -gt 0 ] || exit
if [ "$a" -lt "$b" ]; then cut=$b read=$1; else cut=$a read=$2; fi
truncate -s $((cut + 20)) $f && echo "$read"
SH
)
$D start "$N" >/dev/null; up
CJ_KEPT="n=0; for id in $CJ_IDS; do test -d $ST/containers/\$id && test -d /var/lib/kiri/containers/\$id && n=\$((n+1)); done; echo kept \$n"
t cj-copy-kept "containers.json.corrupt-" "ls $ST"
t cj-info-warns "containers.json.corrupt-" "docker info --format '{{json .Warnings}}'"
t cj-readable-listed "${CJ_READ:-no id read}" "docker ps -a -q --no-trunc"
t cj-nothing-deleted "kept 2" "$CJ_KEPT"
t cj-nothing-deleted-after-run "kept 2" "docker run --rm alpine true; $CJ_KEPT"
t cj-readable-logs "line-" "docker logs ${CJ_READ:-none}"
t cj-readable-files "precious" "docker cp ${CJ_READ:-none}:/data - | tar -xO"
# The cut container's volume is used by no listed container: neither a
# prune nor a removal without --force takes it while the list is damaged.
t cj-prune-refused "were not pruned" "docker volume prune -af 2>&1"
t cj-prune-kept-volumes "volumes 2" "echo volumes \$(docker volume ls -q | grep -c '^cjv')"
t cj-volume-rm-refused "was not removed" "docker volume rm cjv1 cjv2 2>&1"
t cj-volume-rm-kept-volumes "volumes 2" "echo volumes \$(docker volume ls -q | grep -c '^cjv')"
$D restart -t 20 "$N" >/dev/null; up
t cj-next-start-deletes-nothing "kept 2" "docker run --rm alpine true; $CJ_KEPT"
t cj-cleanup "done" "docker rm -f cj1 cj2 >/dev/null 2>&1; docker volume rm -f cjv1 cjv2 >/dev/null 2>&1; for id in $CJ_IDS; do rm -rf $ST/containers/\$id /var/lib/kiri/containers/\$id; done; rm -f $ST/containers.json.corrupt-*; echo done"
# An images.json cut short loses its newest image (a commit here). The
# repair is a docker info warning naming the copy and the lost image, and
# the lost image's unpacked layers are kept, also at the next start (#440).
t ij-setup "ok" "docker run --name ij alpine:3.21 sh -c 'echo local > /local' >/dev/null && docker commit ij local/ij:1 >/dev/null && docker rm ij >/dev/null && echo ok"
IJ_LAYERS=$($D exec "$N" sh -c 'ls /var/lib/kiri/layers | wc -l' | tr -d ' ')
$D stop -t 20 "$N" >/dev/null
$D run --rm --name "$N-edit" -v "$N"-store:/s alpine sh -c 'f=/s/images.json; truncate -s $(( $(stat -c %s $f) - 40 )) $f' >/dev/null
$D start "$N" >/dev/null; up
t ij-info-warns "images.json.corrupt-" "docker images -q >/dev/null; docker info --format '{{json .Warnings}}'"
t ij-info-names-lost "local/ij:1" "docker info --format '{{json .Warnings}}'"
t ij-layers-kept "layers $IJ_LAYERS" "docker run --rm alpine:3.21 true; echo layers \$(ls /var/lib/kiri/layers | wc -l)"
$D restart -t 20 "$N" >/dev/null; up
t ij-layers-kept-next-start "layers $IJ_LAYERS" "docker run --rm alpine:3.21 true; echo layers \$(ls /var/lib/kiri/layers | wc -l)"
t ij-cleanup "done" "rm -f $ST/images.json.corrupt-*; echo done"
t ij-warning-gone "[]" "docker info --format '{{json .Warnings}}'"
# Docker's insecure-registries and registry-mirrors, in Kiri's own
# ~/.kiri/daemon.json (#329, #611, #118). The registries serve one image
# store, pushed to over plain HTTP on loopback: kiri-trusted.test:5055 is
# the plain-HTTP registry at a name that isn't loopback, :5056 the same
# store behind a password, kiri-other.test:5444 TLS with a self-signed
# certificate. Unlisted, each is refused (and no password goes to it);
# listed with `kiri registry insecure add`, pull, push, login and build
# work, for those hosts only. A hand edit of daemon.json applies without a
# restart. A Docker Hub image only a plain-HTTP mirror has pulls through the
# mirror.
$D exec "$N" sh -c 'grep -q kiri-trusted.test /etc/hosts || echo "$(hostname -i | cut -d" " -f1) kiri-trusted.test kiri-other.test" >> /etc/hosts'
REGS="apk add -q openssl >/dev/null 2>&1; mkdir -p /tls; [ -f /tls/other.pem ] || openssl req -x509 -newkey rsa:2048 -nodes -days 3 -subj /CN=kiri-other.test -addext subjectAltName=DNS:kiri-other.test -keyout /tls/other.key -out /tls/other.pem 2>/dev/null; chmod 644 /tls/other.key
docker rm -f kiri-reg-http kiri-reg-other >/dev/null 2>&1
docker run -d --name kiri-reg-http -p 5055:5000 -v kiri-regdata:/var/lib/registry registry:2 >/dev/null
docker run -d --name kiri-reg-other -p 5444:5000 -v kiri-regdata:/var/lib/registry -v /tls:/tls:ro -e REGISTRY_HTTP_TLS_CERTIFICATE=/tls/other.pem -e REGISTRY_HTTP_TLS_KEY=/tls/other.key registry:2 >/dev/null
for i in \$(seq 60); do curl -s -o /dev/null localhost:5055/v2/ && break; sleep 0.25; done"
t registry-setup "pushed" "$REGS; docker tag alpine:3.21 localhost:5055/team/alp:1 && docker push -q localhost:5055/team/alp:1 >/dev/null && docker rmi localhost:5055/team/alp:1 >/dev/null && echo pushed"
RIP=$($D exec "$N" sh -c 'hostname -i | cut -d" " -f1')
REGAUTH="apk add -q apache2-utils >/dev/null 2>&1; mkdir -p /regauth && htpasswd -Bbn kiriuser kiripass > /regauth/htpasswd
docker rm -f kiri-reg-auth kiri-reg-spare kiri-reg-80 >/dev/null 2>&1; docker run -d --name kiri-reg-spare -p 5057:5000 -v kiri-regdata:/var/lib/registry registry:2 >/dev/null; docker run -d --name kiri-reg-auth -p 5056:5000 -v kiri-regdata:/var/lib/registry -v /regauth:/auth:ro -e REGISTRY_AUTH=htpasswd -e REGISTRY_AUTH_HTPASSWD_REALM=kiri -e REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd registry:2 >/dev/null
for i in \$(seq 60); do curl -s -o /dev/null localhost:5056/v2/ && curl -s -o /dev/null localhost:5057/v2/ && break; sleep 0.25; done"
PULLED="&& echo pulled || echo refused"
t insecure-setup "ok" "$REGAUTH; rm -f /root/.kiri/daemon.json; docker tag alpine:3.21 localhost:5055/library/kiri-mirror-only:1 && docker push -q localhost:5055/library/kiri-mirror-only:1 >/dev/null && docker rmi localhost:5055/library/kiri-mirror-only:1 >/dev/null && echo ok"
t insecure-http-unlisted-refused "refused" "docker pull kiri-trusted.test:5055/team/alp:1 >/dev/null 2>&1 $PULLED"
t insecure-tls-unlisted-refused "refused" "docker pull kiri-other.test:5444/team/alp:1 >/dev/null 2>&1 $PULLED"
t insecure-login-unlisted-refused "refused" "echo kiripass | docker login kiri-trusted.test:5056 -u kiriuser --password-stdin >/dev/null 2>&1 && echo signed in || echo refused"
t insecure-cli-add "kiri-other.test:5444" "/opt/kiri/kiri registry insecure add kiri-trusted.test:5055 kiri-trusted.test:5056 kiri-other.test:5444 $RIP:5055 2>&1"
t insecure-cli-refuses-everything "internet" "/opt/kiri/kiri registry insecure add 0.0.0.0/0 2>&1"
t insecure-daemon-json "kiri-trusted.test:5056" "cat /root/.kiri/daemon.json"
t insecure-http-pull "Downloaded newer image" "docker pull kiri-trusted.test:5055/team/alp:1 2>&1"
t insecure-http-push "pushed" "docker tag alpine:3.21 kiri-trusted.test:5055/team/pushed:1 && docker push kiri-trusted.test:5055/team/pushed:1 >/dev/null 2>&1 && curl -s localhost:5055/v2/team/pushed/tags/list | grep -q '\"1\"' && echo pushed"
t insecure-selfsigned-pull "Downloaded newer image" "docker pull kiri-other.test:5444/team/alp:1 2>&1"
t insecure-http-login "Login Succeeded" "echo kiripass | docker login kiri-trusted.test:5056 -u kiriuser --password-stdin 2>&1"
t insecure-http-auth-pull "Downloaded newer image" "docker pull kiri-trusted.test:5056/team/alp:1 2>&1"
# :5057 answers (a plain-HTTP registry holding the same image) but only
# :5055 and :5056 of the host are listed: only a refusal by port can stop it.
t insecure-other-port-listening "200" "curl -s -o /dev/null -w '%{http_code}' kiri-trusted.test:5057/v2/"
t insecure-other-port-still-refused "refused" "docker pull kiri-trusted.test:5057/team/alp:1 >/dev/null 2>&1 $PULLED"
t insecure-docker-info "kiri-trusted.test:5055" "docker info 2>/dev/null | sed -n '/Insecure Registries/,/^ [A-Z]/p'"
# The builder is a container: it reaches the registry container on the
# bridge at its own address (a published port on the engine's address is not
# reachable from a container in every host's network).
t insecure-build "erucesni-tliub" "gw=\$(docker inspect kiri-reg-http --format '{{.NetworkSettings.IPAddress}}'):5000; /opt/kiri/kiri registry insecure add \$gw >/dev/null 2>&1; mkdir -p /bins && printf \"FROM \$gw/team/alp:1\\nRUN echo built-insecure | rev\\n\" > /bins/Dockerfile && DOCKER_BUILDKIT=0 docker build --no-cache /bins >/tmp/b.log 2>&1; grep -m1 -o erucesni-tliub /tmp/b.log || grep -i -m3 'error\|fail' /tmp/b.log"
# The same from a registry on self-signed HTTPS, cold: unchecked HTTPS,
# not plain HTTP to its TLS port.
t insecure-selfsigned-build "dengis-fles" "gw=\$(docker inspect kiri-reg-other --format '{{.NetworkSettings.IPAddress}}'):5000; /opt/kiri/kiri registry insecure add \$gw >/dev/null 2>&1; printf \"FROM \$gw/team/alp:1\\nRUN echo self-signed | rev\\n\" > /bins/Dockerfile && DOCKER_BUILDKIT=0 docker build --no-cache /bins >/tmp/b.log 2>&1; grep -m1 -o dengis-fles /tmp/b.log || grep -i -m3 'error\|fail' /tmp/b.log"
t insecure-cli-remove "kiri-trusted.test:5056" "/opt/kiri/kiri registry insecure remove kiri-trusted.test:5055 kiri-other.test:5444 2>&1"
t insecure-removed-refused "refused" "docker rmi kiri-other.test:5444/team/alp:1 >/dev/null 2>&1; docker pull kiri-other.test:5444/team/alp:1 >/dev/null 2>&1 $PULLED"
t insecure-hand-edit "Downloaded newer image" "printf '{\"insecure-registries\": [\"kiri-other.test:5444\", \"$RIP:5055\"], \"debug\": true}\\n' > /root/.kiri/daemon.json; docker pull kiri-other.test:5444/team/alp:1 2>&1"
t insecure-hand-edit-keeps-keys "kept" "/opt/kiri/kiri registry mirror add http://$RIP:5055 >/dev/null 2>&1; f=/root/.kiri/daemon.json; grep -q '\"debug\": true' \$f && grep -q 'http://$RIP:5055' \$f && grep -q kiri-other.test:5444 \$f && echo kept"
t mirror-http-pull "Downloaded newer image" "docker pull kiri-mirror-only:1 2>&1"
t mirror-docker-info "http://$RIP:5055/" "docker info 2>/dev/null | sed -n '/Registry Mirrors/,/^ [A-Z]/p'"
t mirror-build "rorrim-tliub" "gw=\$(docker inspect kiri-reg-http --format '{{.NetworkSettings.IPAddress}}'):5000; /opt/kiri/kiri registry mirror remove http://$RIP:5055 >/dev/null 2>&1; /opt/kiri/kiri registry insecure add \$gw >/dev/null 2>&1; /opt/kiri/kiri registry mirror add http://\$gw >/dev/null 2>&1; docker rmi kiri-mirror-only:1 >/dev/null 2>&1; printf 'FROM kiri-mirror-only:1\\nRUN echo built-mirror | rev\\n' > /bins/Dockerfile && DOCKER_BUILDKIT=0 docker build --no-cache /bins >/tmp/b.log 2>&1; grep -m1 -o rorrim-tliub /tmp/b.log || grep -i -m3 'error\|fail' /tmp/b.log"
# Docker Hub is reached over verified HTTPS whatever the list covers: a
# name server (here /etc/hosts) that puts its name inside a listed network,
# in front of a plain-HTTP registry on port 80, must not turn the pull into
# one over HTTP.
t hub-downgrade-refused "refused" "docker run -d --name kiri-reg-80 -p 80:5000 -v kiri-regdata:/var/lib/registry registry:2 >/dev/null; for i in \$(seq 60); do curl -s -o /dev/null $RIP:80/v2/ && break; sleep 0.25; done
printf '{\"insecure-registries\": [\"$RIP/32\"]}\n' > /root/.kiri/daemon.json; echo '$RIP registry-1.docker.io' >> /etc/hosts
docker pull team/alp:1 >/dev/null 2>&1 $PULLED; grep -v registry-1.docker.io /etc/hosts > /tmp/hosts.new; cat /tmp/hosts.new > /etc/hosts"
t insecure-cleanup "done" "rm -f /root/.kiri/daemon.json; docker rm -f kiri-reg-auth kiri-reg-spare kiri-reg-80 kiri-reg-http kiri-reg-other >/dev/null 2>&1; echo done"
# Docker on the same host (#247): its firewall makes iptables' FORWARD
# policy drop, and a forwarded packet must pass every chain on the hook, so
# Kiri's own table accepting it was not enough and containers had no
# internet. Made here as Docker makes it, in the engine's network namespace
# while the engine runs, with a container behind a bridge of "Docker's"
# (docker0, a namespace serving on port 8080). Kiri's containers reach the
# internet again through rules of its own in DOCKER-USER, and still not
# Docker's container: no new connection into Docker's bridges passes.
t dockerfw-setup "ok" "apk add -q iptables ip6tables iproute2 >/dev/null 2>&1 && for t in iptables ip6tables; do \$t -N DOCKER-USER && \$t -I FORWARD -j DOCKER-USER && \$t -P FORWARD DROP || exit 1; done && ip netns add dk && ip link add docker0 type bridge && ip link add dkh type veth peer name dkp && ip link set dkp netns dk && ip link set dkh master docker0 && ip addr add 172.31.250.1/24 dev docker0 && ip link set docker0 up && ip link set dkh up && ip -n dk addr add 172.31.250.2/24 dev dkp && ip -n dk link set dkp up && ip -n dk route add default via 172.31.250.1 && (ip netns exec dk python3 -m http.server 8080 >/dev/null 2>&1 &) && echo ok"
t dockerfw-internet "Example Domain" "docker run --rm alpine wget -qO- -T 10 http://example.com"
t dockerfw-rules "3 3" "echo \$(iptables -S DOCKER-USER | grep -c kiri) \$(ip6tables -S DOCKER-USER | grep -c kiri)"
t dockerfw-docker-reachable "Directory listing" "wget -qO- -T 3 http://172.31.250.2:8080/"
t dockerfw-docker-kept-apart "kept apart" "docker run --rm alpine sh -c 'wget -qO- -T 3 http://172.31.250.2:8080/ >/dev/null 2>&1 && echo reached || echo kept apart'"
t dockerfw-cleanup "ok" "pkill -f '[h]ttp.server 8080'; ip netns del dk; ip link del docker0; for t in iptables ip6tables; do \$t -P FORWARD ACCEPT; \$t -D FORWARD -j DOCKER-USER; \$t -F DOCKER-USER; \$t -X DOCKER-USER; done; echo ok"
# A FORWARD policy of drop without Docker (#247): set by hand, or by ufw.
# Kiri's rules go at the top of FORWARD, for IPv4 and IPv6, and still let
# no Kiri container into another program's bridge (virbr0, a namespace
# serving on port 8080, as libvirt's guests sit behind it).
t fwdrop-setup "ok" "apk add -q nftables >/dev/null 2>&1; for t in iptables ip6tables; do \$t -P FORWARD DROP || exit 1; done && ip netns add vm && ip link add virbr0 type bridge && ip link add vmh type veth peer name vmp && ip link set vmp netns vm && ip link set vmh master virbr0 && ip addr add 172.31.251.1/24 dev virbr0 && ip link set virbr0 up && ip link set vmh up && ip -n vm addr add 172.31.251.2/24 dev vmp && ip -n vm link set vmp up && ip -n vm route add default via 172.31.251.1 && (ip netns exec vm python3 -m http.server 8080 >/dev/null 2>&1 &) && echo ok"
t fwdrop-internet "Example Domain" "docker run --rm alpine wget -qO- -T 10 http://example.com"
t fwdrop-user-network "Example Domain" "docker network create fwdrop >/dev/null && docker run --rm --network fwdrop alpine wget -qO- -T 10 http://example.com; docker network rm fwdrop >/dev/null"
t fwdrop-rules "3 3" "echo \$(iptables -S FORWARD | grep -c kiri) \$(ip6tables -S FORWARD | grep -c kiri)"
t fwdrop-kept-apart "kept apart" "docker run --rm alpine sh -c 'wget -qO- -T 3 http://172.31.251.2:8080/ >/dev/null 2>&1 && echo reached || echo kept apart'"
# ufw turned on while the engine runs, and reloaded: its policy is drop
# and it reloads only its own chains.
t fwdrop-ufw-setup "ok" "for t in iptables ip6tables; do \$t -F FORWARD; \$t -P FORWARD ACCEPT; done; apk add -q ufw >/dev/null 2>&1 && ufw --force enable >/dev/null && iptables -S FORWARD | grep -q 'P FORWARD DROP' && echo ok"
t fwdrop-ufw-internet "Example Domain" "docker run --rm alpine wget -qO- -T 10 http://example.com"
t fwdrop-ufw-reload "Example Domain" "ufw reload >/dev/null; docker run --rm alpine wget -qO- -T 10 http://example.com"
t fwdrop-ufw-rules "3 3" "echo \$(iptables -S FORWARD | grep -c kiri) \$(ip6tables -S FORWARD | grep -c kiri)"
# An nftables chain of the administrator's own with policy drop: Kiri does
# not change it, and docker info says which rules let its containers through.
t fwdrop-nft-warns "nft insert rule inet own forward iifname" "ufw --force disable >/dev/null; nft add table inet own && nft add chain inet own forward '{ type filter hook forward priority 0; policy drop; }' && sleep 1 && docker info --format '{{json .Warnings}}'"
t fwdrop-nft-advice-works "Example Domain" "nft insert rule inet own forward oifname 'kiri*' accept && nft insert rule inet own forward iifname 'kiri*' ct state established,related accept && nft insert rule inet own forward iifname 'kiri*' oifname eth0 accept && docker run --rm alpine wget -qO- -T 10 http://example.com"
t fwdrop-nft-warning-gone "gone" "docker info --format '{{json .Warnings}}' | grep -q 'inet own' && echo still there || echo gone"
# A flush of the whole ruleset (nftables.service starting or reloading)
# takes Kiri's own table: it is built again, or containers had no NAT.
t fwdrop-flush-rebuilt "Example Domain" "nft flush ruleset; sleep 1; docker run --rm alpine wget -qO- -T 10 http://example.com"
t fwdrop-cleanup "ok" "pkill -f '[h]ttp.server 8080'; ip netns del vm; ip link del virbr0; echo ok"
# Directory accounts and NSS groups outside /etc/passwd and /etc/group (#349):
# the static engine uses getent (with clean env and bounded duration), admits
# local, directory, merged local+NSS, initgroups-only (including high signed
# GIDs), host-access canonicalized, and NSS-only group members, refuses
# non-members and unknown uids, survives transient exit-2 outages without
# losing ACLs or live connections, and revokes removed accounts with open
# connections even while another user's getent lookup hangs.
$D exec -i "$N" sh -c 'cat > /access-test.sh' <<'SH'
#!/bin/sh
set -u
had_getent=1
[ -e /usr/bin/getent ] || { had_getent=0; : > /usr/bin/getent; }
had_gpasswd=1
[ -e /usr/bin/gpasswd ] || { had_gpasswd=0; : > /usr/bin/gpasswd; }
out=$(unshare --mount --propagation private --net --fork sh -c '
  set -u
  mount -t tmpfs t /var/lib/kiri && mount -t tmpfs t /root && mount -t tmpfs t /dev/shm && ip link set lo up
  mkdir -p /dev/shm/acc /dev/shm/acc/run && chmod 755 /dev/shm/acc /dev/shm/acc/run
  mount --bind /dev/shm/acc/run /run
  mkdir -p /run/kiri && chmod 755 /run/kiri
  cp /etc/passwd /dev/shm/acc/passwd && cp /etc/group /dev/shm/acc/group
  echo "klocal349:x:21001:21001::/:/sbin/nologin" >> /dev/shm/acc/passwd
  echo "kgrp349:x:21100:klocal349,kdir349" >> /dev/shm/acc/group
  echo "kiri:x:21300:" >> /dev/shm/acc/group
  mount --bind /dev/shm/acc/passwd /etc/passwd
  mount --bind /dev/shm/acc/group /etc/group
  cat > /dev/shm/acc/getent <<"GE"
#!/bin/sh
[ -z "${LD_PRELOAD:-}" ] || exit 9
[ "$1" = "--" ] && shift
db="$1"; shift
etc_lookup() {
  file="$1"; want="$2"
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      "$want":*) printf "%s\n" "$line"; return 0 ;;
    esac
  done < "$file"
  return 1
}
hang() {
  [ -x /bin/sleep ] && exec /bin/sleep 60 || exec /usr/bin/sleep 60
}
[ -f /dev/shm/acc/hang-all ] && hang
case "$db" in
  passwd)
    rc=0
    for key in "$@"; do
      if [ -f /dev/shm/acc/hang-dir ] && { [ "$key" = kdir349 ] || [ "$key" = 21002 ]; }; then hang; fi
      if [ -f /dev/shm/acc/outage-dir ] && { [ "$key" = kdir349 ] || [ "$key" = 21002 ]; }; then rc=2; continue; fi
      case "$key" in
        kdir349|21002) echo "kdir349:x:21002:21002::/:/bin/sh" ;;
        kinit349|21003) echo "kinit349:x:21003:21003::/:/bin/sh" ;;
        kout349|21004) echo "kout349:x:21004:21004::/:/bin/sh" ;;
        KCanon349|kcanon349|kcanon349@corp.example|21005) echo "kcanon349@corp.example:x:21005:21005::/:/bin/sh" ;;
        kmerged349|21006) echo "kmerged349:x:21006:21006::/:/bin/sh" ;;
        *)
          if line=$(etc_lookup /etc/passwd "$key"); then printf "%s\n" "$line"; else rc=2; fi ;;
      esac
    done
    exit "$rc" ;;
  group)
    rc=0
    for key in "$@"; do
      case "$key" in
        kgrp349)
          if line=$(etc_lookup /etc/group kgrp349); then
            case "$line" in
              *:) printf "%skmerged349\n" "$line" ;;
              *)  printf "%s,kmerged349\n" "$line" ;;
            esac
          else
            rc=2
          fi ;;
        knssgrp349) echo "knssgrp349:x:21200:klocal349,kdir349" ;;
        *)
          if line=$(etc_lookup /etc/group "$key"); then printf "%s\n" "$line"; else rc=2; fi ;;
      esac
    done
    exit "$rc" ;;
  initgroups)
    key="$1"
    if [ -f /dev/shm/acc/hang-dir ] && [ "$key" = kdir349 ]; then hang; fi
    if [ -f /dev/shm/acc/outage-dir ] && [ "$key" = kdir349 ]; then exit 2; fi
    case "$key" in
      kinit349) echo "kinit349 -1000 21100"; exit 0 ;;
      kdir349|kout349|klocal349|kmerged349|kcanon349@corp.example) echo "$key"; exit 0 ;;
      *) exit 2 ;;
    esac ;;
  *) exit 2 ;;
esac
GE
  chmod 755 /dev/shm/acc/getent
  mount --bind /dev/shm/acc/getent /usr/bin/getent
  cat > /dev/shm/acc/gpasswd <<"GP"
#!/bin/sh
[ -z "${LD_PRELOAD:-}" ] || exit 9
[ "$1" = "-a" ] || exit 2
user="$2"; grp="$3"
out=""
found=0
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    "$grp":*)
      found=1
      case "$line" in
        *:) line="${line}${user}" ;;
        *)  line="${line},${user}" ;;
      esac ;;
  esac
  out="${out}${line}
"
done < /etc/group
[ "$found" = 1 ] || exit 1
printf "%s" "$out" > /etc/group
exit 0
GP
  chmod 755 /dev/shm/acc/gpasswd
  mount --bind /dev/shm/acc/gpasswd /usr/bin/gpasswd
  ping() {
    sock=$1; uid=$2; gid=$3; grp=${4:-}; max=${5:-3}
    if [ -n "$grp" ]; then
      code=$(setpriv --reuid="$uid" --regid="$gid" --groups="$grp" curl -s -m "$max" -o /dev/null -w "%{http_code}" --unix-socket "$sock" http://d/_ping 2>/dev/null)
    else
      code=$(setpriv --reuid="$uid" --regid="$gid" --clear-groups curl -s -m "$max" -o /dev/null -w "%{http_code}" --unix-socket "$sock" http://d/_ping 2>/dev/null)
    fi
    echo "${code:-000}"
  }
  cat > /dev/shm/acc/hold.py <<"PY"
import socket, sys
sock, st = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
try:
    s.connect(sock)
    s.sendall(b"GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n")
    data = s.recv(512)
    if b" 200 " not in data:
        open(st, "w").write("denied")
        sys.exit(1)
    open(st, "w").write("ready")
    s.settimeout(40)
    while True:
        chunk = s.recv(64)
        if not chunk:
            open(st, "w").write("closed")
            sys.exit(0)
except Exception:
    open(st, "w").write("closed")
PY
  hold_conn() {
    sock=$1; uid=$2; gid=$3; st=$4
    : > "$st"
    chmod 666 "$st"
    setpriv --reuid="$uid" --regid="$gid" --clear-groups python3 /dev/shm/acc/hold.py "$sock" "$st" >/dev/null 2>&1 &
    echo $!
  }
  wait_state() {
    st=$1; want=$2; tries=$3
    for _ in $(seq "$tries"); do
      [ "$(cat "$st" 2>/dev/null)" = "$want" ] && return 0
      sleep 0.25
    done
    return 1
  }
  set_grp() {
    grep -v "^kgrp349:" /etc/group > /dev/shm/acc/group.new
    echo "kgrp349:x:21100:$1" >> /dev/shm/acc/group.new
    cat /dev/shm/acc/group.new > /etc/group
  }
  # 1. Start daemon during a transient exit-2 outage for kdir349.
  KIRI_BIN=${KIRI_BIN:-/opt/kiri/kiri}
  touch /dev/shm/acc/outage-dir
  S1=/dev/shm/acc/loc.sock
  mkdir -p /dev/shm/acc/h1
  LD_PRELOAD=/nonexistent/kiri-test.so HOME=/dev/shm/acc/h1 KIRI_SOCKET=$S1 KIRI_SOCKET_GROUP=kgrp349 "$KIRI_BIN" daemon >/dev/shm/acc/d1.log 2>&1 &
  P1=$!
  for i in $(seq 80); do [ -S "$S1" ] && [ "$(ping "$S1" 0 0)" = 200 ] && break; sleep 0.2; done
  r_local=$(ping "$S1" 21001 21001)
  r_merged=$(ping "$S1" 21006 21006)
  r_init=$(ping "$S1" 21003 21003 21100)
  r_out=$(ping "$S1" 21004 21004 21100)
  r_unk=$(ping "$S1" 21099 21099 21100)
  # Outage clears: kdir349 must recover its socket ACL automatically.
  rm -f /dev/shm/acc/outage-dir
  r_boot=000
  for i in $(seq 24); do
    r_boot=$(ping "$S1" 21002 21002)
    [ "$r_boot" = 200 ] && break
    sleep 0.25
  done
  # 2. Live directory connection survives a brief exit-2 outage.
  C_DIR=$(hold_conn "$S1" 21002 21002 /dev/shm/acc/st-dir)
  if wait_state /dev/shm/acc/st-dir ready 20; then
    touch /dev/shm/acc/outage-dir
    sleep 2.8
    rm -f /dev/shm/acc/outage-dir
    if [ "$(cat /dev/shm/acc/st-dir)" = ready ]; then r_outage=open; else r_outage=closed; fi
    sleep 2.5
  else
    r_outage=no-conn
  fi
  # 3. Removed user with an open connection is closed even while getent hangs on another user.
  C_LOC=$(hold_conn "$S1" 21001 21001 /dev/shm/acc/st-loc)
  if wait_state /dev/shm/acc/st-loc ready 20; then
    touch /dev/shm/acc/hang-dir
    set_grp "kdir349"
    if wait_state /dev/shm/acc/st-loc closed 40; then r_hang_close=closed; else r_hang_close=stuck; fi
  else
    r_hang_close=no-conn
  fi
  kill "$C_LOC" 2>/dev/null; wait "$C_LOC" 2>/dev/null
  # 4. Global hung getent fails new uncached connections promptly instead of hanging.
  touch /dev/shm/acc/hang-all
  t0=$(date +%s)
  r_hn_code=$(ping "$S1" 21098 21098 21100 12)
  dt=$(($(date +%s) - t0))
  if [ "$r_hn_code" = 000 ] && [ "$dt" -lt 9 ]; then r_hang_new=000-fast; else r_hang_new="${r_hn_code}-${dt}s"; fi
  rm -f /dev/shm/acc/hang-all /dev/shm/acc/hang-dir
  # 5. Removing kdir349 closes its open connection and refuses new ones.
  set_grp ""
  if wait_state /dev/shm/acc/st-dir closed 48; then r_rev_open=closed; else r_rev_open=stuck; fi
  kill "$C_DIR" 2>/dev/null; wait "$C_DIR" 2>/dev/null
  r_rev=$(ping "$S1" 21002 21002 21100)
  kill "$P1" 2>/dev/null; wait "$P1" 2>/dev/null
  # 6. kiri host-access with a canonicalized directory account.
  S3=/run/kiri/docker.sock
  mkdir -p /dev/shm/acc/h3
  HOME=/dev/shm/acc/h3 KIRI_SOCKET=$S3 KIRI_SOCKET_GROUP=kiri "$KIRI_BIN" daemon >/dev/shm/acc/d3.log 2>&1 &
  P3=$!
  for i in $(seq 80); do [ -S "$S3" ] && [ "$(ping "$S3" 0 0)" = 200 ] && break; sleep 0.2; done
  if LD_PRELOAD=/nonexistent/kiri-test.so "$KIRI_BIN" host-access KCanon349 >/dev/shm/acc/ha.log 2>&1; then
    r_ha=0
  else
    r_ha=1
  fi
  if grep -q "^kiri:.*kcanon349@corp.example" /etc/group; then r_ha_name=canon; else r_ha_name=raw; fi
  r_canon=$(ping "$S3" 21005 21005)
  kill "$P3" 2>/dev/null; wait "$P3" 2>/dev/null
  # 7. NSS-only group outside /etc/group.
  S2=/dev/shm/acc/nss.sock
  mkdir -p /dev/shm/acc/h2
  HOME=/dev/shm/acc/h2 KIRI_SOCKET=$S2 KIRI_SOCKET_GROUP=knssgrp349 "$KIRI_BIN" daemon >/dev/shm/acc/d2.log 2>&1 &
  P2=$!
  for i in $(seq 40); do kill -0 "$P2" 2>/dev/null || break; [ -S "$S2" ] && [ "$(ping "$S2" 0 0)" = 200 ] && break; sleep 0.2; done
  if [ -S "$S2" ] && [ "$(ping "$S2" 0 0)" = 200 ]; then
    r_nl=$(ping "$S2" 21001 21001)
    r_nd=$(ping "$S2" 21002 21002)
    r_no=$(ping "$S2" 21004 21004 21200)
  else
    r_nl=down; r_nd=down; r_no=down
  fi
  kill "$P2" 2>/dev/null; wait "$P2" 2>/dev/null
  echo "local=$r_local merged=$r_merged init=$r_init out=$r_out unk=$r_unk boot=$r_boot outage=$r_outage hang-close=$r_hang_close hang-new=$r_hang_new rev-open=$r_rev_open rev=$r_rev ha=$r_ha ha-name=$r_ha_name canon=$r_canon nss-local=$r_nl nss-dir=$r_nd nss-out=$r_no"
')
[ "$had_getent" = 0 ] && rm -f /usr/bin/getent
[ "$had_gpasswd" = 0 ] && rm -f /usr/bin/gpasswd
echo "$out"
SH
t access-directory-accounts "local=200 merged=200 init=200 out=000 unk=000 boot=200 outage=open hang-close=closed hang-new=000-fast rev-open=closed rev=000 ha=0 ha-name=canon canon=200 nss-local=200 nss-dir=200 nss-out=000" "apk add -q util-linux util-linux-misc iproute2 shadow >/dev/null 2>&1; sh /access-test.sh"
$D rm -f "$N" >/dev/null
# An engine in a user namespace (nested Docker, Sysbox, Coder, CI; #597),
# where the kernel refuses mknod of a device: an image layer with a
# character and a block device, a fifo and a file still unpacks, the devices
# left out as Docker leaves them out and the fifo made. Before, the whole
# layer failed ("mknod: Operation not permitted"). That engine runs alone in
# its own container, in a new user namespace (root mapped to root, 65536
# ids, so /proc/self/uid_map is not "0 0 4294967295") with its own mount
# and network namespaces, and only unpacks: docker load, then docker create.
U="$N-userns"
# t, for output got elsewhere than the engine's container.
check() {
  if [[ "$3" == *"$2"* ]]; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); printf '  FAIL %s\n    want: %s\n    got:  %s\n' "$1" "$2" "$(echo "$3" | head -3)"; kiri_guard "$1"; fi
}
$D rm -f "$U" >/dev/null 2>&1
$D run -d --name "$U" --privileged --tmpfs /run -v "$BIN:/opt/kiri:ro" alpine sleep 1d >/dev/null
$D exec "$U" sh -c 'apk add -q util-linux-misc python3 docker-cli >/dev/null 2>&1'
$D exec -i "$U" sh -c 'cat > /mkimage.py' <<'PY'
import hashlib, io, json, tarfile
layer = io.BytesIO()
with tarfile.open(fileobj=layer, mode="w", format=tarfile.GNU_FORMAT) as t:
    for name, typ, mode, major, minor in [("dev/", tarfile.DIRTYPE, 0o755, 0, 0), ("dev/kiri-null", tarfile.CHRTYPE, 0o666, 1, 3),
                                          ("dev/kiri-loop", tarfile.BLKTYPE, 0o660, 7, 9), ("pipe", tarfile.FIFOTYPE, 0o640, 0, 0)]:
        i = tarfile.TarInfo(name); i.type, i.mode, i.devmajor, i.devminor = typ, mode, major, minor; t.addfile(i)
    i = tarfile.TarInfo("after"); i.size = 3; t.addfile(i, io.BytesIO(b"ok\n"))
lb = layer.getvalue()
cfg = json.dumps({"os": "linux", "config": {"Cmd": ["/x"]}, "rootfs": {"type": "layers", "diff_ids": ["sha256:" + hashlib.sha256(lb).hexdigest()]}}).encode()
c = hashlib.sha256(cfg).hexdigest() + ".json"
man = json.dumps([{"Config": c, "RepoTags": ["kiri-nodes:1"], "Layers": ["layer.tar"]}]).encode()
with tarfile.open("/devices.tar", "w") as t:
    for n, d in [("layer.tar", lb), (c, cfg), ("manifest.json", man)]:
        i = tarfile.TarInfo(n); i.size = len(d); t.addfile(i, io.BytesIO(d))
PY
if ! $D exec "$U" unshare -U -r true 2>/dev/null; then
  echo "  SKIP userns-layer-devices: no user namespace can be made on this host"
else
  got=$($D exec "$U" unshare --user --map-users=0:0:65536 --map-groups=0:0:65536 --setuid=0 --setgid=0 --mount --propagation private --net --fork sh -c '
    python3 /mkimage.py
    mkdir -p /var/lib/kiri && mount -t tmpfs t /var/lib/kiri && mount -t tmpfs t /root && ip link set lo up
    /opt/kiri/kiri daemon > /tmp/engine.log 2>&1 &
    export DOCKER_HOST=unix:///root/.kiri/run/docker.sock
    for i in $(seq 120); do docker version >/dev/null 2>&1 && break; sleep 0.25; done
    docker load -i /devices.tar >/dev/null && docker create kiri-nodes:1 >/dev/null || exit
    cd /var/lib/kiri/layers/*/ && for f in dev/kiri-null dev/kiri-loop pipe after; do printf "%s=" $f; stat -c %F $f 2>/dev/null || echo none; done | tr "\n" " "
    kill %1' 2>&1 | tr -d '\r')
  check userns-layer-devices "dev/kiri-null=none dev/kiri-loop=none pipe=fifo after=regular file" "$got"
fi
$D rm -f "$U" >/dev/null
echo "$PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
