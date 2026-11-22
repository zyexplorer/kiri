#!/bin/bash
# IPv6 conformance: the IPv6 behaviour of a Docker engine, checked with
# nothing but the docker CLI, case by case against what Docker documents and
# what moby's own integration tests assert (each case names its source).
# Runs against any engine - Kiri on a Mac, in WSL2 or on a Linux host, or
# Docker itself as the reference - through DOCKER_HOST or DOCKER_CONTEXT:
#
#   tests/ipv6-conformance.sh                               # current context
#   DOCKER_CONTEXT=kiri-dev tests/ipv6-conformance.sh
#   DOCKER_HOST=tcp://127.0.0.1:2375 tests/ipv6-conformance.sh
#   DOCKER=docker.exe tests/ipv6-conformance.sh             # WSL, Windows' CLI
#
# From pwsh on Windows, with Git for Windows' bash (docker.exe on PATH):
#   $env:DOCKER_HOST='tcp://127.0.0.1:2375'; & 'C:\Program Files\Git\bin\bash.exe' tests/ipv6-conformance.sh
# or WSL's bash with Windows' CLI (needs WSL interop, which systemd distros
# can lose; docker.exe gets -H from the script):
#   wsl -d Ubuntu -- env DOCKER=docker.exe DOCKER_HOST=tcp://127.0.0.1:2375 bash tests/ipv6-conformance.sh
#
# Prints PASS, FAIL or SKIP per case with its source, then a summary; exits 1
# if any case failed. SKIP is for what the engine's host cannot have (no
# IPv6 uplink, no Compose), never for a wrong answer. Everything it makes is
# labelled kiri.ipv6-conformance=<run> and removed at the end, even on ^C;
# nothing else on the engine is touched.
#
# Options (environment):
#   DOCKER=docker.exe     the CLI to run (default docker)
#   V6C_CLIENT=1|0        also curl published ports from this machine
#                         (default: 1 when the engine is a local socket)
#   V6C_KEEP=1            leave the objects for a look (cleanup prints how)
#   V6C_PORT_BASE=18640   first of the 4 host ports the port cases publish
set -u

# The CLI and the engine. A Windows docker.exe run from WSL doesn't see
# Linux environment variables, so the engine is passed as flags.
DOCKER=${DOCKER:-docker}
# Git Bash (MSYS) rewrites arguments that look like paths (/etc/hosts would
# reach docker.exe as C:/Program Files/Git/etc/hosts): not here.
case "${OSTYPE:-}" in msys*|cygwin*) export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' ;; esac
D=("$DOCKER")
if [ -n "${DOCKER_HOST:-}" ]; then D+=(-H "$DOCKER_HOST")
elif [ -n "${DOCKER_CONTEXT:-}" ]; then D+=(--context "$DOCKER_CONTEXT"); fi
d() { "${D[@]}" "$@"; }

RUN=v6c$$
LABEL=kiri.ipv6-conformance=$RUN
N=$RUN                                   # name prefix for everything made here
PB=${V6C_PORT_BASE:-18640}
IMG=alpine:3.21
WEB=nginx:alpine
# Fixed subnets from the documentation-style ULA range the moby tests use;
# picked so they don't collide with an engine's own pool.
S1=fd00:c0de:6c1::                       # user subnet, /64
S2=fd00:c0de:6c2::                       # second network, connect --ip6
S3=fd00:c0de:6c3::                       # IPv6-only network
S4=fd00:c0de:6c4::                       # Compose

# Sources. Each case prints the one(s) it mirrors.
DOC_V6=https://docs.docker.com/engine/daemon/ipv6/
DOC_CREATE=https://docs.docker.com/reference/cli/docker/network/create/
DOC_CONNECT=https://docs.docker.com/reference/cli/docker/network/connect/
DOC_BRIDGE=https://docs.docker.com/engine/network/drivers/bridge/
DOC_PORTS=https://docs.docker.com/engine/network/port-publishing/
DOC_FW=https://docs.docker.com/engine/network/packet-filtering-firewalls/
DOC_DNS=https://docs.docker.com/engine/network/#dns-services
DOC_COMPOSE=https://docs.docker.com/reference/compose-file/networks/#enable_ipv6
DOC_COMPOSE_SVC=https://docs.docker.com/reference/compose-file/services/#ipv4_address-ipv6_address
# moby's integration tests, pinned to the master commit they were read at.
MOBY=https://github.com/moby/moby/blob/9aa9edfa787e08bb2da31694cd8a402876f32232/integration
M_BR=$MOBY/networking/bridge_linux_test.go
M_NETBR=$MOBY/network/bridge/bridge_linux_test.go
M_PORT=$MOBY/networking/port_mapping_linux_test.go
M_HOSTS=$MOBY/networking/etchosts_test.go
M_RESOLV=$MOBY/networking/resolvconf_test.go
M_DNS=$MOBY/network/dns_test.go
TEST_IPV6=https://test-ipv6.com/

PASS=0 FAIL=0 SKIP=0
FAILED=()
SKIPPED=()

# Runs a command with a time limit, wherever we are: coreutils timeout
# (Linux, WSL), else perl's alarm (macOS), else as is.
# The shorthands below (d, run, net) are expanded here: timeout and perl can
# only exec programs.
lim() {
  local s=$1; shift
  case $1 in
    d) shift; set -- "${D[@]}" "$@" ;;
    run) shift; set -- "${D[@]}" run --label "$LABEL" "$@" ;;
    net) shift; set -- "${D[@]}" network create --label "$LABEL" "$@" ;;
  esac
  if command -v timeout >/dev/null 2>&1; then timeout "$s" "$@"
  elif command -v perl >/dev/null 2>&1; then perl -e 'alarm shift; exec @ARGV or exit 127' "$s" "$@"
  else "$@"; fi
}
out() { lim "${T:-90}" "$@" 2>&1 | tr -d '\r'; }

report() { # report RESULT NAME SOURCE [DETAIL]
  local r=$1 name=$2 src=$3 detail=${4:-}
  case $r in
    PASS) PASS=$((PASS + 1)) ;;
    FAIL) FAIL=$((FAIL + 1)); FAILED+=("$name") ;;
    SKIP) SKIP=$((SKIP + 1)); SKIPPED+=("$name") ;;
  esac
  printf '%-4s %-28s %s\n' "$r" "$name" "$src"
  [ -n "$detail" ] && printf '       %s\n' "$detail" | head -6
  return 0
}

# has NAME SOURCE WANT CMD... : PASS when the output contains WANT.
has() {
  local name=$1 src=$2 want=$3; shift 3
  local got; got=$(out "$@")
  if [[ "$got" == *"$want"* ]]; then report PASS "$name" "$src"
  else report FAIL "$name" "$src" "want: $want | got: $(echo "$got" | head -3 | tr '\n' ' ')"; fi
}
# lacks NAME SOURCE UNWANTED CMD... : PASS when the command succeeds and its output doesn't contain UNWANTED.
lacks() {
  local name=$1 src=$2 bad=$3; shift 3
  local got rc; got=$(out "$@"); rc=${PIPESTATUS[0]}
  if [[ "$got" != *"$bad"* ]]; then report PASS "$name" "$src"
  else report FAIL "$name" "$src" "unwanted: $bad | got: $(echo "$got" | head -3 | tr '\n' ' ')"; fi
}
# refused NAME SOURCE CMD... : PASS when the command fails (exit != 0).
refused() {
  local name=$1 src=$2; shift 2
  local got; got=$(lim "${T:-90}" "$@" 2>&1 | tr -d '\r'; echo "rc=${PIPESTATUS[0]}")
  if [[ "$got" != *"rc=0" ]]; then report PASS "$name" "$src"
  else report FAIL "$name" "$src" "accepted, want an error: $(echo "$got" | head -2 | tr '\n' ' ')"; fi
}
# ok NAME SOURCE CMD... : PASS when the command exits 0.
ok() {
  local name=$1 src=$2; shift 2
  local got; got=$(lim "${T:-90}" "$@" 2>&1 | tr -d '\r'; echo "rc=${PIPESTATUS[0]}")
  if [[ "$got" == *"rc=0" ]]; then report PASS "$name" "$src"
  else report FAIL "$name" "$src" "$(echo "$got" | head -3 | tr '\n' ' ')"; fi
}
section() { printf '\n== %s\n' "$1"; }

cleanup() {
  if [ "${V6C_KEEP:-0}" = 1 ]; then
    echo "kept: $DOCKER rm -f \$($DOCKER ps -aq --filter label=$LABEL); $DOCKER network rm \$($DOCKER network ls -q --filter label=$LABEL)"
    return
  fi
  [ -n "${COMPOSE_DIR:-}" ] && { d compose -p "$N" -f "$COMPOSE_DIR/compose.yaml" down -t 1 >/dev/null 2>&1; rm -rf "$COMPOSE_DIR"; }
  local c; c=$(d ps -aq --filter "label=$LABEL" | tr -d '\r')
  [ -n "$c" ] && d rm -f $c >/dev/null 2>&1
  local n; n=$(d network ls -q --filter "label=$LABEL" | tr -d '\r')
  [ -n "$n" ] && d network rm $n >/dev/null 2>&1
  local left; left=$( (d ps -aq --filter "label=$LABEL"; d network ls -q --filter "label=$LABEL") | tr -d '\r' | wc -l | tr -d ' ')
  [ "$left" = 0 ] || echo "cleanup: $left object(s) labelled $LABEL are still there"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# Shorthands: everything made carries the label.
net() { d network create --label "$LABEL" "$@"; }
run() { d run --label "$LABEL" "$@"; }
# A container's address on a network; Docker 29 prints "invalid IP" for none.
v6of() { d inspect -f "{{(index .NetworkSettings.Networks \"$2\").GlobalIPv6Address}}" "$1" 2>/dev/null | tr -d '\r' | sed 's/^invalid IP$//'; }

# --- the engine --------------------------------------------------------------
section "engine"
SV=$(out d version --format '{{.Server.Version}} {{.Server.Os}}/{{.Server.Arch}} API {{.Server.APIVersion}}')
OS=$(out d info --format '{{.OperatingSystem}} | kernel {{.KernelVersion}}')
echo "engine:  $SV"
echo "         $OS"
echo "client:  $(out "$DOCKER" version --format '{{.Client.Version}}') ($DOCKER${DOCKER_HOST:+ -H $DOCKER_HOST}${DOCKER_CONTEXT:+ --context $DOCKER_CONTEXT})"
echo "run:     $RUN (label $LABEL)"
if ! d version >/dev/null 2>&1; then echo "no engine answers"; exit 1; fi
# The two images, from KIRI_TEST_REGISTRY (tests/lib/images.sh), never from
# Docker Hub, whose hourly pull limit a few runs use up.
kiri_docker() { d "$@"; }
# shellcheck source=SCRIPTDIR/lib/images.sh
. "$(dirname "$0")/lib/images.sh"
kiri_seed_images $IMG $WEB || echo "an image could not be fetched; the cases that run it will fail"

# Does the engine's host have IPv6 out? Asked from its own network namespace
# (--network host), the way test-ipv6.com asks a browser's machine.
UPLINK=$(out run --rm --network host $IMG sh -c 'ip -6 route show default | head -1; ping -6 -c 1 -W 3 2001:4860:4860::8888 >/dev/null 2>&1 && echo V6UP')
if [[ "$UPLINK" == *V6UP* ]]; then V6UP=1; echo "uplink:  the engine's host reaches 2001:4860:4860::8888 over IPv6"
else V6UP=0; echo "uplink:  no IPv6 out from the engine's host (egress cases will be SKIP)"; fi

if [ -z "${V6C_CLIENT:-}" ]; then
  V6C_CLIENT=0
  case "${DOCKER_HOST:-}" in
    unix://*) V6C_CLIENT=1 ;;
    "") case "$(out "$DOCKER" context inspect ${DOCKER_CONTEXT:+"$DOCKER_CONTEXT"} -f '{{.Endpoints.docker.Host}}')" in unix://*|npipe://*) V6C_CLIENT=1 ;; esac ;;
  esac
fi
command -v curl >/dev/null 2>&1 || V6C_CLIENT=0

# --- the default bridge -------------------------------------------------------
section "default bridge"
# IPv6 is opt-in: the default bridge is IPv4-only unless the daemon's
# configuration says "ipv6": true.
B6=$(out d network inspect bridge -f '{{.EnableIPv6}}')
if [ "$B6" = false ]; then
  report PASS default-bridge-ipv4-only "$DOC_V6#use-ipv6-for-the-default-bridge-network"
  lacks default-bridge-no-global-v6 "$DOC_V6" "inet6" run --rm $IMG ip -6 addr show scope global
else
  report SKIP default-bridge-ipv4-only "$DOC_V6" "the daemon has IPv6 on the default bridge (EnableIPv6=$B6): configured, not a fault"
fi
# A network made without --ipv6 gets no IPv6 address (moby disables IPv6 on
# the interface).
net "$N-v4" >/dev/null
lacks v4-network-no-global-v6 "$M_BR TestNonIPv6Network" "inet6" run --rm --network "$N-v4" $IMG ip -6 addr show scope global
lacks v4-network-no-inet6-eth0 "$M_BR TestNonIPv6Network" "inet6" run --rm --network "$N-v4" $IMG ip -6 addr show eth0
has v4-network-lo-keeps-v6 "$M_BR TestNonIPv6Network" "::1" run --rm --network "$N-v4" $IMG ip -6 addr show lo
has v4-network-enableipv6-false "$DOC_CREATE" "false" d network inspect "$N-v4" -f '{{.EnableIPv6}}'

# --- creating IPv6 networks ---------------------------------------------------
section "network create --ipv6"
# No subnet: Docker allocates a /64 from its ULA pool (fd00::/8) beside the IPv4 one.
has create-ipv6 "$DOC_CREATE" "" net --ipv6 "$N-ula"
has enable-ipv6-true "$DOC_V6#create-an-ipv6-network" "true" d network inspect "$N-ula" -f '{{.EnableIPv6}}'
ULA=$(out d network inspect "$N-ula" -f '{{range .IPAM.Config}}{{if eq (len (split .Subnet ":")) 1}}{{else}}{{.Subnet}}{{end}}{{end}}')
case "$ULA" in fd*::/64|fd*:/64) report PASS ula-auto-subnet "$DOC_V6#dynamic-ipv6-subnet-allocation" ;;
  *) report FAIL ula-auto-subnet "$DOC_V6#dynamic-ipv6-subnet-allocation" "want an fdxx:.../64, got: $ULA" ;; esac
has dual-stack-ipam "$M_NETBR TestCreateWithIPv6DefaultsToULAPrefix" "." d network inspect "$N-ula" -f '{{range .IPAM.Config}}{{if eq (len (split .Subnet ":")) 1}}{{.Subnet}}{{end}}{{end}}'
net --ipv6 "$N-ula2" >/dev/null
ULA2=$(out d network inspect "$N-ula2" -f '{{range .IPAM.Config}}{{if eq (len (split .Subnet ":")) 1}}{{else}}{{.Subnet}}{{end}}{{end}}')
if [ -n "$ULA2" ] && [ "$ULA2" != "$ULA" ]; then report PASS ula-distinct-subnets "$DOC_V6#dynamic-ipv6-subnet-allocation"
else report FAIL ula-distinct-subnets "$DOC_V6#dynamic-ipv6-subnet-allocation" "$ULA vs $ULA2"; fi

# A given subnet; the gateway defaults to its first address.
net --ipv6 --subnet "${S1}/64" "$N-sub" >/dev/null
has user-subnet "$DOC_CREATE" "${S1}/64" d network inspect "$N-sub" -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}'
has default-gateway-first "$DOC_BRIDGE" "${S1}1" d network inspect "$N-sub" -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}'
lacks gateway-no-cidr-suffix "$M_BR TestNetworkInspectGateway" "/" d network inspect "$N-sub" -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}'
has explicit-gateway "$DOC_CREATE" "${S2}fe" sh -c "$(printf '%q ' "${D[@]}") network create --label $LABEL --ipv6 --subnet ${S2}/64 --gateway ${S2}fe $N-gw >/dev/null && $(printf '%q ' "${D[@]}") network inspect $N-gw -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}'"
refused subnet-overlap-refused "$DOC_CREATE" net --ipv6 --subnet "${S1}/80" "$N-dup"
refused bad-ipv6-subnet-refused "$DOC_CREATE" net --ipv6 --subnet fd00:zz::/64 "$N-bad"

# --- containers on an IPv6 network --------------------------------------------
section "containers"
run -d --name "$N-web" --network "$N-sub" $WEB >/dev/null
WEB6=$(v6of "$N-web" "$N-sub")
case "$WEB6" in ${S1}*) report PASS container-gets-v6 "$DOC_V6#create-an-ipv6-network" ;;
  *) report FAIL container-gets-v6 "$DOC_V6#create-an-ipv6-network" "GlobalIPv6Address=$WEB6" ;; esac
has prefix-len-64 "$DOC_V6" "64" d inspect -f "{{(index .NetworkSettings.Networks \"$N-sub\").GlobalIPv6PrefixLen}}" "$N-web"
has ipv6-gateway-field "$DOC_V6" "${S1}1" d inspect -f "{{(index .NetworkSettings.Networks \"$N-sub\").IPv6Gateway}}" "$N-web"
has ipv4-too "$DOC_V6 (dual stack)" "." d inspect -f "{{(index .NetworkSettings.Networks \"$N-sub\").IPAddress}}" "$N-web"
has network-lists-member "$DOC_V6" "$WEB6/64" d network inspect "$N-sub" -f '{{range .Containers}}{{.IPv6Address}} {{end}}'
has eth0-global-v6 "$M_BR TestBridgeICC" "inet6 $WEB6" d exec "$N-web" ip -6 addr show eth0
has eth0-link-local "$M_BR TestBridgeICC (SLAAC LL)" "inet6 fe80:" d exec "$N-web" ip -6 addr show eth0
has default-route-v6 "$DOC_V6" "default via ${S1}1" run --rm --network "$N-sub" $IMG ip -6 route
has etc-hosts-v6 "$DOC_DNS" "$S1" run --rm --network "$N-sub" $IMG cat /etc/hosts
has etc-hosts-ip6-localhost "$M_HOSTS TestEtcHostsIpv6" "ip6-localhost" run --rm --network "$N-sub" $IMG cat /etc/hosts
has resolv-embedded-dns "$M_RESOLV TestResolvConfLocalhostIPv6" "nameserver 127.0.0.11" run --rm --network "$N-sub" $IMG cat /etc/resolv.conf
has icc-ping6-ip "$M_BR TestBridgeICC" "1 packets received" run --rm --network "$N-sub" $IMG ping -6 -c 1 -W 3 "$WEB6"
has icc-ping6-name "$M_BR TestBridgeICC" "1 packets received" run --rm --network "$N-sub" $IMG ping -6 -c 1 -W 3 "$N-web"
has icc-http-v6 "$M_BR TestBridgeICC" "Welcome to nginx" run --rm --network "$N-sub" $IMG wget -qO- -T 5 "http://[$WEB6]/"
has dns-aaaa "$M_BR TestBridgeICC / $DOC_DNS" "$WEB6" run --rm --network "$N-sub" $IMG nslookup -type=AAAA "$N-web"
has dns-a-still "$DOC_DNS" "Address" run --rm --network "$N-sub" $IMG nslookup -type=A "$N-web"
# An address on a ULA (auto) network too.
run -d --name "$N-ula-c" --network "$N-ula" $IMG sleep 300 >/dev/null
U6=$(v6of "$N-ula-c" "$N-ula")
case "$U6" in fd*) report PASS ula-container-gets-v6 "$DOC_V6#dynamic-ipv6-subnet-allocation" ;;
  *) report FAIL ula-container-gets-v6 "$DOC_V6#dynamic-ipv6-subnet-allocation" "GlobalIPv6Address=$U6 (subnet $ULA)" ;; esac
# A container that turns IPv6 off gets no address and no AAAA record.
run -d --name "$N-nov6" --network "$N-sub" --sysctl net.ipv6.conf.all.disable_ipv6=1 $IMG sleep 300 >/dev/null
NOV6=$(v6of "$N-nov6" "$N-sub")
if [ -z "$NOV6" ] && d inspect "$N-nov6" >/dev/null 2>&1; then report PASS sysctl-disable-ipv6-no-addr "$M_BR TestContainerDisabledIPv6"
else report FAIL sysctl-disable-ipv6-no-addr "$M_BR TestContainerDisabledIPv6" "GlobalIPv6Address=${NOV6:-<no container>}"; fi
lacks sysctl-disable-ipv6-no-aaaa "$M_BR TestContainerDisabledIPv6" "$S1" run --rm --network "$N-sub" $IMG sh -c "nslookup -type=AAAA $N-nov6 2>&1 | grep -v '^Server\|^Address.*#53'"
# An --internal IPv6 network: the gateway answers, nothing beyond it is routed.
net --internal --ipv6 --subnet "fd00:c0de:6c5::/64" "$N-int" >/dev/null
has internal-ping-gateway "$M_BR TestInternalNwConnectivity" "1 packets received" run --rm --network "$N-int" $IMG ping -6 -c 1 -W 3 fd00:c0de:6c5::1
has internal-no-route-out "$M_BR TestInternalNwConnectivity" "unreachable" run --rm --network "$N-int" $IMG sh -c 'ping -6 -c 1 -W 2 2001:4860:4860::8888 2>&1 || true'

# --- fixed addresses ----------------------------------------------------------
section "--ip6 and network connect"
run -d --name "$N-fixed" --network "$N-sub" --ip6 "${S1}99" $IMG sleep 300 >/dev/null
has ip6-static "https://docs.docker.com/reference/cli/docker/container/run/#network" "${S1}99 ${S1}99" d inspect -f "{{(index .NetworkSettings.Networks \"$N-sub\").GlobalIPv6Address}} {{(index .NetworkSettings.Networks \"$N-sub\").IPAMConfig.IPv6Address}}" "$N-fixed"
has ip6-static-on-eth0 "$DOC_CONNECT" "inet6 ${S1}99/64" d exec "$N-fixed" ip -6 addr show eth0
refused ip6-in-use-refused "$DOC_CONNECT" run --rm --network "$N-sub" --ip6 "${S1}99" $IMG true
refused ip6-outside-subnet-refused "$DOC_CONNECT" run --rm --network "$N-sub" --ip6 fd00:dead:beef::9 $IMG true
refused ip6-on-v4-network-refused "$DOC_CONNECT" run --rm --network "$N-v4" --ip6 "${S1}98" $IMG true
# Docker (#51569) lets --ip6 pick from an automatically allocated subnet.
ULAPFX=${ULA%%::/64}
has ip6-on-auto-subnet "$M_BR TestSetIPWithNoConfiguredSubnet" "${ULAPFX}::77" run --rm --network "$N-ula" --ip6 "${ULAPFX}::77" $IMG ip -6 addr show eth0
run -d --name "$N-late" $IMG sleep 300 >/dev/null
ok connect-ip6 "$DOC_CONNECT" d network connect --ip6 "${S2}42" "$N-gw" "$N-late"
has connect-ip6-inspect "$DOC_CONNECT" "${S2}42" d inspect -f "{{(index .NetworkSettings.Networks \"$N-gw\").GlobalIPv6Address}}" "$N-late"
has connect-ip6-in-container "$DOC_CONNECT" "inet6 ${S2}42/64" d exec "$N-late" ip -6 addr
has connect-route-reach "$DOC_CONNECT" "1 packets received" run --rm --network "$N-gw" $IMG ping -6 -c 1 -W 3 "${S2}42"
ok disconnect "$DOC_CONNECT" d network disconnect "$N-gw" "$N-late"
lacks disconnect-removes-v6 "$DOC_CONNECT" "${S2}42" d exec "$N-late" ip -6 addr

# --- IPv6-only networks (Docker 28+) -------------------------------------------
section "IPv6-only network (--ipv4=false)"
G=$(out net --ipv4=false --ipv6 --subnet "${S3}/64" "$N-v6only")
if d network inspect "$N-v6only" >/dev/null 2>&1; then
  report PASS ipv6-only-create "$DOC_V6#create-an-ipv6-only-network"
  has ipv6-only-enableipv4-false "$DOC_CREATE" "false" d network inspect "$N-v6only" -f '{{.EnableIPv4}}'
  has ipv6-only-no-v4-addr "$M_BR TestDisableIPv4" "inet6 ${S3}" run --rm --network "$N-v6only" $IMG sh -c 'ip -4 addr show eth0; ip -6 addr show eth0'
  lacks ipv6-only-no-inet4 "$M_BR TestDisableIPv4" "inet " run --rm --network "$N-v6only" $IMG ip -4 addr show eth0
  run -d --name "$N-v6only-web" --network "$N-v6only" $WEB >/dev/null
  # nginx was started just above: give it the moment it needs to listen.
  has ipv6-only-dns-http "$M_BR TestDisableIPv4" "Welcome to nginx" run --rm --network "$N-v6only" $IMG sh -c "for i in 1 2 3 4 5 6 7 8 9 10; do wget -qO- -T 5 http://$N-v6only-web/ && break; sleep 0.5; done"
  has ipv6-only-lo-v4-v6 "$M_BR TestDisableIPv4" "127.0.0.1 ::1" run --rm --network "$N-v6only" $IMG sh -c "echo \$(ip -4 addr show lo | grep -o '127.0.0.1') \$(ip -6 addr show lo | grep -o '::1')"
  has ipv6-only-ext-dns "$M_DNS TestExtDNSInIPv6OnlyNw" "Address" run --rm --network "$N-v6only" $IMG nslookup -type=A example.com
else
  report FAIL ipv6-only-create "$DOC_V6#create-an-ipv6-only-network" "$(echo "$G" | head -2 | tr '\n' ' ')"
fi
refused no-family-refused "https://docs.docker.com/engine/release-notes/28/#2800" net --ipv4=false "$N-nofamily"

# --- published ports -----------------------------------------------------------
section "published ports"
# A port published without an address listens on 0.0.0.0 and [::]; checked
# from the engine host's own network namespace, and from this machine when
# the engine is local.
run -d --name "$N-pub" --network "$N-sub" -p "$PB:80" $WEB >/dev/null
# Control: are published ports served in the engine host's namespace at all?
# (Docker Desktop forwards them to the client machine instead.)
HOSTNS=0
[[ "$(out run --rm --network host $IMG sh -c "for i in 1 2 3 4 5 6 7 8 9 10; do wget -qO- -T 2 http://127.0.0.1:$PB/ && break; sleep 0.5; done")" == *nginx* ]] && HOSTNS=1
engine_port() { # engine_port NAME SOURCE PORT
  if [ "$HOSTNS" = 1 ]; then has "$1" "$2" "Welcome to nginx" run --rm --network host $IMG sh -c "for i in 1 2 3 4 5 6 7 8 9 10; do wget -qO- -T 2 http://[::1]:$3/ && break; sleep 0.5; done"
  else report SKIP "$1" "$2" "published ports aren't served in the engine host's namespace (127.0.0.1:$PB doesn't answer there either)"; fi
}
has port-lists-v6 "$DOC_PORTS" "[::]:$PB" d port "$N-pub" 80
has port-lists-v4 "$DOC_PORTS" "0.0.0.0:$PB" d port "$N-pub" 80
has port-inspect-hostip-any6 "$M_PORT TestMixAnyWithSpecificHostAddrs" ":: $PB" d inspect -f '{{range (index .NetworkSettings.Ports "80/tcp")}}{{.HostIp}} {{.HostPort}};{{end}}' "$N-pub"
engine_port port-v6-loopback-engine "$M_PORT TestAccessPublishedPortFromHost" "$PB"
run -d --name "$N-pub6" -p "[::1]:$((PB + 1)):80" $WEB >/dev/null
has port-bind-v6-only-listed "$DOC_PORTS" "[::1]:$((PB + 1))" d port "$N-pub6" 80
lacks port-bind-v6-only-not-v4 "$DOC_PORTS" "0.0.0.0" d port "$N-pub6" 80
engine_port port-bind-v6-only-engine "$M_PORT TestAccessPublishedPortFromHost" "$((PB + 1))"
# An IPv4-only container published on [::1] (docker-proxy, 4-to-6).
run -d --name "$N-pub46" --network "$N-v4" -p "[::1]:$((PB + 2)):80" $WEB >/dev/null
engine_port port-proxy-4to6 "$M_PORT TestProxy4To6" "$((PB + 2))"
# A container on an IPv6 network reaches a published port of another over the
# host's address (hairpin) - skipped: needs a routable host address.
if [ "$V6C_CLIENT" = 1 ]; then
  has port-v6-client "$DOC_PORTS (from the client host)" "Welcome to nginx" sh -c "for i in 1 2 3 4 5 6 7 8 9 10; do curl -sg -m 2 'http://[::1]:$PB/' && break; sleep 0.5; done"
  has port-v4-client "$DOC_PORTS (from the client host)" "Welcome to nginx" curl -s -m 3 "http://127.0.0.1:$PB/"
  has port-bind-v6-only-client "$DOC_PORTS (from the client host)" "Welcome to nginx" curl -sg -m 3 "http://[::1]:$((PB + 1))/"
  has port-proxy-4to6-client "$M_PORT TestProxy4To6 (from the client host)" "Welcome to nginx" curl -sg -m 3 "http://[::1]:$((PB + 2))/"
else
  report SKIP port-v6-client "$DOC_PORTS" "engine isn't local (V6C_CLIENT=1 to force)"
fi

# --- egress (NAT66) -------------------------------------------------------------
section "egress"
# Docker masquerades a ULA network's traffic out of the host's address
# (ip6tables, on by default since 27.0), so a container reaches IPv6 sites
# whenever the host can. Without a host uplink it must fail at once, not hang.
GOT=$(out run --rm --network "$N-sub" $IMG nslookup -type=AAAA ipv6.google.com)
if [[ "$GOT" == *2001:4860* ]]; then report PASS dns-external-aaaa "$TEST_IPV6 (AAAA over the embedded DNS)"
elif [ "$V6UP" = 0 ]; then report SKIP dns-external-aaaa "$TEST_IPV6" "no AAAA answer; engines may filter AAAA when the host has no IPv6 (Docker Desktop's DNS filter)"
else report FAIL dns-external-aaaa "$TEST_IPV6" "$(echo "$GOT" | tail -2 | tr '\n' ' ')"; fi
if [ "$V6UP" = 1 ]; then
  has egress-ping6 "$DOC_FW#ipv6 / $TEST_IPV6" "1 packets received" run --rm --network "$N-sub" $IMG ping -6 -c 1 -W 5 2001:4860:4860::8888
  has egress-http-v6-only-site "$TEST_IPV6" "<" run --rm --network "$N-sub" $IMG wget -qO- -T 10 http://ipv6.google.com/
  has egress-ula-auto "$DOC_V6#dynamic-ipv6-subnet-allocation" "1 packets received" run --rm --network "$N-ula" $IMG ping -6 -c 1 -W 5 2001:4860:4860::8888
  # The source address seen outside is the host's (masqueraded), not the ULA.
  SEEN=$(out run --rm --network "$N-sub" $IMG wget -qO- -T 10 http://api6.ipify.org)
  case "$SEEN" in fd*|"") report FAIL egress-masquerade "$DOC_FW#ipv6" "seen as: ${SEEN:-nothing}" ;;
    *:*) report PASS egress-masquerade "$DOC_FW#ipv6" ;;
    *) report SKIP egress-masquerade "$DOC_FW#ipv6" "api6.ipify.org said: $(echo "$SEEN" | head -1)" ;; esac
else
  for c in egress-ping6 egress-http-v6-only-site egress-ula-auto egress-masquerade; do report SKIP $c "$TEST_IPV6" "no IPv6 uplink on the engine's host"; done
fi
# Fails fast either way (no 2-minute hang on an unreachable route).
T0=$(date +%s)
out run --rm --network "$N-sub" $IMG sh -c 'wget -qO- -T 8 http://[2001:4860:4860::8888]:81/ >/dev/null 2>&1; true' >/dev/null
DT=$(( $(date +%s) - T0 ))
if [ "$DT" -le 20 ]; then report PASS egress-no-hang "$DOC_V6" "${DT}s"
else report FAIL egress-no-hang "$DOC_V6" "took ${DT}s"; fi
# An IPv4 network has no IPv6 route out at all.
has v4-network-v6-unreachable "$DOC_V6" "unreachable" run --rm --network "$N-v4" $IMG sh -c 'ping -6 -c 1 -W 2 2001:4860:4860::8888 2>&1 || true'

# --- Compose -------------------------------------------------------------------------
section "compose enable_ipv6"
if d compose version >/dev/null 2>&1; then
  COMPOSE_DIR=$(mktemp -d)
  cat > "$COMPOSE_DIR/compose.yaml" <<EOF
name: $N
services:
  web:
    image: $WEB
    labels: ["$LABEL"]
    networks:
      fixed:
        ipv6_address: ${S4}10
      auto: {}
networks:
  fixed:
    enable_ipv6: true
    labels: ["$LABEL"]
    ipam:
      config:
        - subnet: ${S4}/64
  auto:
    enable_ipv6: true
    labels: ["$LABEL"]
EOF
  # docker.exe reads a Windows path: cygpath (Git Bash) or wslpath (WSL) gives it one.
  CF=$COMPOSE_DIR/compose.yaml
  if command -v cygpath >/dev/null 2>&1; then CF=$(cygpath -w "$CF")
  else case "$DOCKER" in *.exe) command -v wslpath >/dev/null 2>&1 && CF=$(wslpath -w "$CF") ;; esac; fi
  T=180 ok compose-up "$DOC_COMPOSE" d compose -p "$N" -f "$CF" up -d --quiet-pull
  has compose-enable-ipv6 "$DOC_COMPOSE" "true" d network inspect "${N}_fixed" -f '{{.EnableIPv6}}'
  has compose-subnet "$DOC_COMPOSE" "${S4}/64" d network inspect "${N}_fixed" -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}'
  has compose-ipv6-address "$DOC_COMPOSE_SVC" "${S4}10" d inspect -f "{{(index .NetworkSettings.Networks \"${N}_fixed\").GlobalIPv6Address}}" "${N}-web-1"
  has compose-auto-ula "$DOC_COMPOSE (no ipam: engine pool)" "fd" d network inspect "${N}_auto" -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}'
  has compose-dns-aaaa "$DOC_COMPOSE" "${S4}10" run --rm --network "${N}_fixed" $IMG nslookup -type=AAAA web
  ok compose-down "$DOC_COMPOSE" d compose -p "$N" -f "$CF" down -t 1
  lacks compose-network-gone "$DOC_COMPOSE" "${N}_fixed" d network ls --format '{{.Name}}'
  rm -rf "$COMPOSE_DIR"; COMPOSE_DIR=
else
  report SKIP compose "$DOC_COMPOSE" "no docker compose"
fi

# --- removal ---------------------------------------------------------------------------
section "removal"
# The engine host's routes, seen from its own namespace: the subnet's route
# goes with the network, and the subnet can be used again.
has host-route-present "$DOC_BRIDGE" "${S1}/64" run --rm --network host $IMG ip -6 route
d rm -f $(d ps -aq --filter "network=$N-sub" | tr -d '\r') >/dev/null 2>&1
ok network-rm "$DOC_CREATE" d network rm "$N-sub"
lacks host-route-gone "$DOC_BRIDGE" "${S1}/64" run --rm --network host $IMG ip -6 route
ok subnet-reusable "$DOC_CREATE (subnet released)" net --ipv6 --subnet "${S1}/64" "$N-sub2"
ok network-rm-again "$DOC_CREATE" d network rm "$N-sub2"

# --- summary ---------------------------------------------------------------------------
printf '\n%s passed, %s failed, %s skipped  (%s)\n' "$PASS" "$FAIL" "$SKIP" "$SV"
[ "$FAIL" = 0 ] || printf 'failed: %s\n' "${FAILED[*]}"
[ "$SKIP" = 0 ] || printf 'skipped: %s\n' "${SKIPPED[*]}"
[ "$FAIL" = 0 ]
