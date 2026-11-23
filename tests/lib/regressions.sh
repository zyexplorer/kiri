# shellcheck shell=sh
# Which bug a check guards, from tests/regressions.tsv. Sourced by the
# suites; their t and code helpers call kiri_guard NAME on a failure, which
# prints, for a listed check, a line such as
#
#     guards: #621  a container start hung for ever when it ran beside docker system df
#
# and nothing for any other check.

KIRI_REGRESSIONS=${KIRI_REGRESSIONS:-$(cd "$(dirname "$0")" && pwd)/regressions.tsv}

kiri_guard() {
  [ -r "$KIRI_REGRESSIONS" ] || return 0
  awk -F '\t' -v n="$1" '$1 == n { printf "    guards: %s  %s\n", $2, $3 }' "$KIRI_REGRESSIONS"
}
