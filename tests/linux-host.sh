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
up() { for _ in $(seq 60); do $D exec "$N" sh -c 'DOCKER_HOST=unix:///root/.kiri/run/docker.sock docker version' >/dev/null 2>&1 && break; sleep 1; done; }
up
$D cp "$(dirname "$0")/testcontainers-api.py" "$N":/testcontainers-api.py >/dev/null
$D cp "$(dirname "$0")/lib/images.sh" "$N":/images.sh >/dev/null
$D exec -e KIRI_TEST_REGISTRY="$KIRI_REG" "$N" sh -c '. /images.sh; export DOCKER_HOST=unix:///root/.kiri/run/docker.sock; kiri_seed_images $KIRI_IMAGES_LINUX_HOST' ||
  echo "linux-host: an image could not be fetched; the checks that run it will fail"
PASS=0; FAIL=0
t() {
  local name=$1 want=$2; shift 2
  local got; got=$($D exec "$N" sh -c "export DOCKER_HOST=unix:///root/.kiri/run/docker.sock; $*" 2>&1 | tr -d '\r')
  if [[ "$got" == *"$want"* ]]; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); printf '  FAIL %s\n    want: %s\n    got:  %s\n' "$name" "$want" "$(echo "$got" | head -3)"; fi
}
t server-os "linux" "docker version --format '{{.Server.Os}}'"
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
$D rm -f "$N" >/dev/null
echo "$PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
