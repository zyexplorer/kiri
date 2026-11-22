# shellcheck shell=sh
# The images the test suites run, fetched without Docker Hub. Sourced by the
# suites (POSIX sh: it also runs inside the alpine containers that drive an
# engine started in a container).
#
# Docker Hub allows 100 anonymous pulls an hour for a whole network, which a
# few suite runs use up. So every image a suite needs comes from another
# registry that carries the same images, is tagged with its plain name
# (alpine:3.21), and the registry-qualified tag is removed again:
#
#   KIRI_TEST_REGISTRY   where official images come from, default
#                        public.ecr.aws/docker/library; mirror.gcr.io/library
#                        is tried when that fails. Checks that pull on
#                        purpose pull <KIRI_TEST_REGISTRY>/<name>:<tag>.
#
# The few images outside Docker's official library have their own source in
# kiri_image_sources. An image already in the engine is left as it is, so a
# second run makes no request at all.
#
#   . tests/lib/images.sh
#   kiri_seed_images $KIRI_IMAGES_E2E          # what tests/e2e.sh runs
#   kiri_seed_images --platform linux/amd64 ubuntu:24.04
#   kiri_fetch alpine:latest linux/amd64       # pull again, even if present
#   kiri_image_remote amd64/alpine:3.22        # the first address it comes from
#
# The docker command is kiri_docker: ${D:-docker} unless the script defines
# its own (tests/ipv6-conformance.sh does), so DOCKER_HOST and DOCKER_CONTEXT
# apply as for the rest of the suite.

KIRI_REG=${KIRI_TEST_REGISTRY:-public.ecr.aws/docker/library}
KIRI_REG_FALLBACK=${KIRI_REG_FALLBACK:-mirror.gcr.io/library}

# What each suite runs; tests/lib/check-images.py compares them with the scripts.
KIRI_IMAGES_E2E="alpine:latest alpine:3.22 busybox:1.36 busybox:1.37 nginx:alpine redis:7-alpine postgres:17-alpine python:3.13-alpine node:22-alpine valkey/valkey:latest curlimages/curl:latest docker:cli httpd:2.4-alpine registry:2 moby/buildkit:buildx-stable-1"
# In the engine tests/linux-host.sh starts; the engine around it needs alpine.
KIRI_IMAGES_LINUX_HOST="alpine:latest alpine:3.21 nginx:alpine redis:7-alpine docker:cli moby/buildkit:buildx-stable-1"
# In the engine tests/wsl.sh starts; around it, ubuntu:24.04 (for its platform) and alpine.
KIRI_IMAGES_WSL="hello-world:latest alpine:latest nginx:alpine redis:7-alpine"
KIRI_IMAGES_EDGE="alpine:3.21 busybox:1.36 nginx:alpine"
KIRI_IMAGES_UPGRADE="alpine:3.21 busybox:1.36 nginx:alpine redis:7-alpine postgres:17-alpine"
KIRI_IMAGES_IPV6="alpine:3.21 nginx:alpine"
# Every image a suite uses, the ones its checks pull on purpose included.
# shellcheck disable=SC2034 # read by tests/lib/check-images.py
KIRI_TEST_IMAGES="$KIRI_IMAGES_E2E $KIRI_IMAGES_LINUX_HOST $KIRI_IMAGES_WSL $KIRI_IMAGES_EDGE $KIRI_IMAGES_UPGRADE $KIRI_IMAGES_IPV6 ubuntu:24.04 alpine:3.24 alpine:3.20 alpine:3.21 busybox:latest amd64/alpine:3.22 debian:trixie"

command -v kiri_docker >/dev/null 2>&1 || kiri_docker() { ${D:-docker} "$@"; }

# kiri_image_full IMAGE: with its tag (alpine -> alpine:latest).
kiri_image_full() {
  case ${1##*/} in *:*) echo "$1" ;; *) echo "$1:latest" ;; esac
}

# kiri_image_sources IMAGE: the addresses it is pulled from, in order. Each
# was checked to serve the image (a manifest request) when it was added.
kiri_image_sources() {
  _kis_img=$(kiri_image_full "$1")
  case $_kis_img in
    # The curl project's image; quay.io/curl/curl is the same one.
    curlimages/curl:*) echo "mirror.gcr.io/$_kis_img quay.io/curl/curl:${_kis_img#*:}" ;;
    # Valkey publishes to Amazon's registry itself.
    valkey/valkey:*) echo "public.ecr.aws/$_kis_img mirror.gcr.io/$_kis_img" ;;
    # buildx's BuildKit image, and the amd64-only alpine repository.
    moby/buildkit:* | amd64/*) echo "mirror.gcr.io/$_kis_img" ;;
    # Not an official image and no source known outside Docker Hub.
    */*) ;;
    *) echo "$KIRI_REG/$_kis_img $KIRI_REG_FALLBACK/$_kis_img" ;;
  esac
}

# kiri_image_remote IMAGE: the first address it is pulled from.
kiri_image_remote() {
  # shellcheck disable=SC2046
  set -- $(kiri_image_sources "$1")
  echo "${1:-}"
}

# kiri_fetch IMAGE [PLATFORM]: pulls it from its sources (the first that
# answers), tags it IMAGE and removes the source's tag, unless that tag was
# there before. Pulls even when IMAGE is present.
kiri_fetch() {
  _kf_img=$(kiri_image_full "$1")
  _kf_plat=${2:-}
  _kf_err=
  for _kf_src in $(kiri_image_sources "$_kf_img"); do
    _kf_had=
    kiri_docker image inspect "$_kf_src" >/dev/null 2>&1 && _kf_had=1
    set -- pull -q
    [ -z "$_kf_plat" ] || set -- "$@" --platform "$_kf_plat"
    if _kf_out=$(kiri_docker "$@" "$_kf_src" 2>&1) && kiri_docker tag "$_kf_src" "$_kf_img" >/dev/null 2>&1; then
      [ -n "$_kf_had" ] || kiri_docker rmi "$_kf_src" >/dev/null 2>&1
      return 0
    fi
    _kf_err="$_kf_src: $(printf '%s\n' "$_kf_out" | tr -d '\r' | tail -1)"
  done
  echo "images: could not fetch $_kf_img (${_kf_err:-no source outside Docker Hub})" >&2
  return 1
}

# kiri_seed_images [--platform PLATFORM] IMAGE...: fetches each one the
# engine does not have yet (or has for another platform). Quiet, and with no
# network request, when it has them all; non-zero if one could not be had.
kiri_seed_images() {
  _ks_plat=
  if [ "${1:-}" = --platform ]; then _ks_plat=$2; shift 2; fi
  _ks_have=" $(kiri_docker image ls --format '{{.Repository}}:{{.Tag}}' 2>/dev/null | tr -d '\r' | tr '\n' ' ') "
  _ks_got=0
  _ks_bad=0
  for _ks_img in "$@"; do
    _ks_img=$(kiri_image_full "$_ks_img")
    case $_ks_have in
      *" $_ks_img "*)
        [ -n "$_ks_plat" ] || continue
        [ "$(kiri_docker image inspect -f '{{.Os}}/{{.Architecture}}' "$_ks_img" 2>/dev/null | tr -d '\r')" = "$_ks_plat" ] && continue
        ;;
    esac
    if kiri_fetch "$_ks_img" "$_ks_plat"; then _ks_got=$((_ks_got + 1)); else _ks_bad=$((_ks_bad + 1)); fi
  done
  [ "$_ks_got" = 0 ] || echo "images: fetched $_ks_got without Docker Hub${_ks_plat:+ ($_ks_plat)}"
  [ "$_ks_bad" = 0 ]
}

# kiri_hub_left: the anonymous Docker Hub pulls this network has left this
# hour, from a HEAD request (which uses none); nothing when offline. Docker
# Hub counts per address, so other machines' pulls on the network show too.
kiri_hub_left() {
  _kh_tok=$(curl -s -m 5 'https://auth.docker.io/token?service=registry.docker.io&scope=repository:ratelimitpreview/test:pull' 2>/dev/null |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])' 2>/dev/null) || return 0
  [ -n "$_kh_tok" ] || return 0
  curl -s -m 5 -I -H "Authorization: Bearer $_kh_tok" https://registry-1.docker.io/v2/ratelimitpreview/test/manifests/latest 2>/dev/null |
    tr -d '\r' | grep -i '^ratelimit-remaining:' | head -1 | sed 's/^[^:]*: *\([0-9]*\).*/\1/'
}
