#!/bin/bash
# Race and load checks: the engine kept busy from several sides at once, as
# the races fixed in #621 and #622 needed to show. Each case runs a counted
# number of rounds with a time limit on every command, says what it
# measured, and passes or fails. Usage:
#
#   tests/stress.sh                          # every case, two to five minutes
#   tests/stress.sh rm-f-exit-code           # only the cases named
#   KIRI_STRESS_ROUNDS=3 tests/stress.sh     # three times the rounds
#   KIRI_STRESS_SECONDS=600 tests/stress.sh  # churn-beside-df for 10 minutes (default 60)
#   KIRI_TEST_CLI=kiri tests/stress.sh       # through kiri's own commands
#
# The cases:
#   starts-beside-df    container starts beside four loops of system df on a
#                       volume of many files: none may hang or fail (#621)
#   rm-f-exit-code      docker run --rm, then rm -f at varied delays: each run
#                       exits 137, never 0; and four waiters on one rm -f (#622)
#   rm-f-during-start   rm -f sent while several containers start at once
#   stop-during-start   stop sent while several containers start at once:
#                       each answers within its limit and nothing is left;
#                       each racing start answers as Docker's does (it
#                       succeeds beside a stop; beside rm -f it succeeds or
#                       says "marked for removal")
#                       (on a Linux host or WSL, no cgroup or interface either)
#                       That part only sees what this shell can see: run from
#                       a container beside the engine, give the container the
#                       host's cgroup view (--cgroupns host) or it counts 0
#                       without having looked.
#   churn-beside-df     create, start, exec, rm and volume churn beside system
#                       df loops: no command over its limit, none failed
#   buildx-bootstrap-loop
#                       buildx docker-container builders made, bootstrapped
#                       and removed: every first bootstrap starts its BuildKit
#                       container (#83; skipped without buildx)
#
# The engine is found as tests/e2e.sh finds it (DOCKER_CONTEXT, default
# kiri); images come from KIRI_TEST_REGISTRY (tests/lib/images.sh). It only
# touches what it makes (names kiri-stress-*, label kiri-stress=1), removes
# it at the end, and never prunes. Exits non-zero if any case fails.

set -u
export DOCKER_CONTEXT=${DOCKER_CONTEXT:-kiri}
D=docker
SOCK=$(docker context inspect "$DOCKER_CONTEXT" --format '{{.Endpoints.docker.Host}}' 2>/dev/null); SOCK=${SOCK#unix://}
[ -S "$SOCK" ] || { echo "stress: no engine socket for context $DOCKER_CONTEXT ($SOCK)"; exit 1; }
export KIRI_SOCKET=$SOCK
# KIRI_TEST_CLI=kiri: the commands go through `kiri`, found as tests/e2e.sh does.
if [ "${KIRI_TEST_CLI:-docker}" = kiri ]; then
  if [ -z "${KIRI:-}" ]; then
    KIRI=$(for p in $(lsof -t "$SOCK" 2>/dev/null); do lsof -a -d txt -Fn -p "$p" 2>/dev/null; done | sed -n 's/^n//p' | grep '/kiri$' | head -1)
    [ -x "$KIRI" ] || KIRI=$(cd "$(dirname "$0")/.." && pwd)/target/release/kiri
  fi
  [ -x "$KIRI" ] || { echo "stress: KIRI_TEST_CLI=kiri, but no kiri binary found (set KIRI)"; exit 1; }
  D=$KIRI
  echo "stress: commands go through $KIRI"
fi
ROUNDS=${KIRI_STRESS_ROUNDS:-1}
CHURN_SECONDS=${KIRI_STRESS_SECONDS:-60}
P=kiri-stress
L=(--label "$P=1")
IMG=alpine:latest
WORK=$(mktemp -d)
PASS=0 FAIL=0 SKIP=0 FAILED=()
T_START=$SECONDS
# $D quoted: the kiri binary's path may hold a space (Kiri Repro.app).
kiri_docker() { "$D" "$@"; }
# shellcheck source=SCRIPTDIR/lib/images.sh
. "$(dirname "$0")/lib/images.sh"
# shellcheck source=SCRIPTDIR/lib/regressions.sh
. "$(dirname "$0")/lib/regressions.sh"

# On macOS the engine's Linux is Kiri's VM, which this shell cannot see; on a
# Linux host and in WSL it is this host, so leftovers there are looked for too.
KVER=$(perl -e 'alarm 30; exec @ARGV' "$D" version --format '{{.Server.KernelVersion}}' 2>/dev/null)
case ${KIRI_TEST_SYSTEM:-} in
  mac | linux | wsl) SYS=$KIRI_TEST_SYSTEM ;;
  *) case $KVER in *-kiri) SYS=mac ;; *[Mm]icrosoft*) SYS=wsl ;; *) SYS=linux ;; esac ;;
esac
echo "stress: $SYS (kernel $KVER), rounds x$ROUNDS, churn ${CHURN_SECONDS}s"

# tm LIMIT COMMAND...: runs it with a time limit, printing "EXIT MS TIMEDOUT".
# A command still running at LIMIT seconds is killed (exit 137, TIMEDOUT 1).
# Its output goes to $TM_OUT (default nowhere).
tm() {
  perl -e '
    use Time::HiRes qw(time);
    my $lim = shift; my $t0 = time; my $to = 0;
    my $pid = fork() // die "fork: $!";
    if (!$pid) { open STDOUT, ">>", ($ENV{TM_OUT} || "/dev/null"); open STDERR, ">&STDOUT"; exec @ARGV; exit 127 }
    $SIG{ALRM} = sub { $to = 1; kill "KILL", $pid };
    alarm $lim;
    while (waitpid($pid, 0) == -1 && $!{EINTR}) {}
    my $st = $?; alarm 0;
    printf "%d %d %d\n", ($st & 127) ? 128 + ($st & 127) : $st >> 8, (time - $t0) * 1000, $to;
  ' "$@"
}
# lim SECS COMMAND...: the command, output kept, killed at SECS seconds.
lim() { perl -e 'alarm shift; exec @ARGV' "$@"; }
# stats FILE COLUMN: "p50 X ms, max Y ms" of that column's numbers.
stats() { awk -v c="$2" '{print $c}' "$1" | sort -n | awk '{a[NR]=$1} END {if (NR) printf "p50 %d ms, max %d ms", a[int((NR+1)/2)], a[NR]; else printf "none"}'; }
# Which cases run: the names given, KIRI_TEST_ONLY's pattern, else all.
CASES=" $* "
selected() {
  [ "$CASES" = "  " ] || [[ "$CASES" == *" $1 "* ]] || return 1
  [ -z "${KIRI_TEST_ONLY:-}" ] || [[ $1 =~ $KIRI_TEST_ONLY ]]
}
case_start() { printf '\n== %s: %s\n' "$1" "$2"; }
# check NAME OK MEASURED: the case's verdict (OK is 1 or 0) and what it saw.
check() {
  echo "  measured: $3"
  if [ "$2" = 1 ]; then PASS=$((PASS + 1)); echo "  ok   $1"
  else FAIL=$((FAIL + 1)); FAILED+=("$1"); echo "  FAIL $1"; kiri_guard "$1"; fi
}
# df loops: N background loops asking /system/df (what docker system df
# asks) until $WORK/df.stop exists; each answer's code and time in df.log.
# df_stop ends them and sets DF_SEEN to what they measured.
df_start() {
  rm -f "$WORK/df.stop" "$WORK/df.log"; DF_PIDS=
  for _ in $(seq "$1"); do
    (while [ -d "$WORK" ] && [ ! -e "$WORK/df.stop" ]; do curl -s -m 30 -o /dev/null -w '%{http_code} %{time_total}\n' --unix-socket "$SOCK" http://d/system/df >> "$WORK/df.log"; done) &
    DF_PIDS="$DF_PIDS $!"
  done
}
df_stop() {
  touch "$WORK/df.stop"
  # shellcheck disable=SC2086
  [ -n "${DF_PIDS:-}" ] && wait $DF_PIDS 2>/dev/null; DF_PIDS=
  DF_SEEN=$(awk '$1 == 200 {n++; t = $2 > t ? $2 : t} $1 != 200 {bad++} END {printf "%d df answers (max %.1f s), %d df errors or timeouts", n, t, bad}' "$WORK/df.log" 2>/dev/null)
}
# Linux host and WSL: cgroup directories and host interfaces left for the
# container ids in file $1 (their veths are kv<first 9 of the id>...).
host_leftovers() {
  [ "$SYS" = mac ] && { echo "not looked for (the VM)"; return; }
  local cg=0 ifs=0 id i
  while read -r id; do
    [ -n "$id" ] || continue
    [ -n "$(find /sys/fs/cgroup -type d -name "*$id*" 2>/dev/null | head -1)" ] && cg=$((cg + 1))
    for i in /sys/class/net/kv"${id:0:9}"*; do [ -e "$i" ] && { ifs=$((ifs + 1)); break; }; done
  done < "$1"
  echo "$cg cgroups, $ifs interfaces"
}
# Everything this script made, each removal given a limit (a stuck one is
# reported, not waited for).
cleanup() {
  # Loops still running (the script was stopped): ended before $WORK goes.
  # shellcheck disable=SC2086
  [ -n "${DF_PIDS:-}" ] && { kill $DF_PIDS 2>/dev/null; wait $DF_PIDS 2>/dev/null; DF_PIDS=; }
  # shellcheck disable=SC2086
  [ -n "${WPIDS:-}" ] && { kill $WPIDS 2>/dev/null; wait $WPIDS 2>/dev/null; WPIDS=; }
  local c v left=0
  for c in $(lim 20 "$D" ps -aq --filter "label=$P=1" 2>/dev/null); do
    [ "$(tm 20 "$D" rm -f -v "$c" | cut -d' ' -f1)" = 0 ] || left=$((left + 1))
  done
  for v in $(lim 20 "$D" volume ls -q --filter "label=$P=1" 2>/dev/null); do tm 20 "$D" volume rm -f "$v" >/dev/null; done
  [ "$left" = 0 ] || echo "stress: $left container(s) could not be removed"
}
trap 'cleanup; rm -rf "$WORK"' EXIT
trap 'exit 130' INT TERM

# The volume system df walks: 20,000 empty files, made once a run.
fill_dfwalk() {
  tm 20 "$D" volume create "${L[@]}" $P-dfwalk >/dev/null
  # shellcheck disable=SC2016 # expanded by the container's shell
  tm 180 "$D" run --rm "${L[@]}" -v $P-dfwalk:/d $IMG sh -c 'test -d /d/200 || { cd /d; for i in $(seq 200); do mkdir $i; for j in $(seq 100); do : > $i/$j; done; done; }' >/dev/null
}

kiri_seed_images $IMG || echo "stress: $IMG could not be fetched; every case will fail"
cleanup

# (a) A start beside system df: system df measured on a thread of the agent
# while a container's first process was cloned, and that process could
# inherit the allocator's lock held by the measuring thread and hang for
# good (#621). A volume of 20,000 files makes each measurement take a while.
if selected starts-beside-df; then
  N=$((100 * ROUNDS)) LIM=15
  case_start starts-beside-df "$N starts beside 4 system df loops, $LIM s a start"
  fill_dfwalk
  df_start 4
  : > "$WORK/a.log"; stuck=0
  for i in $(seq "$N"); do
    read -r rc ms to <<<"$(tm $LIM "$D" run -d "${L[@]}" --name "$P-df-$i" $IMG true)"
    echo "$rc $ms $to" >> "$WORK/a.log"
    [ "$to" = 1 ] && { stuck=$((stuck + 1)); echo "  start $i: no answer in $LIM s"; }
    # A wedged engine: three stuck starts say enough.
    [ "$stuck" -ge 3 ] && { echo "  stopped after $stuck stuck starts"; break; }
  done
  df_stop; dfm=$DF_SEEN
  ran=$(wc -l < "$WORK/a.log" | tr -d ' ')
  failed=$(awk '$3 == 0 && $1 != 0' "$WORK/a.log" | wc -l | tr -d ' ')
  for i in $(seq "$ran"); do tm $LIM "$D" rm -f "$P-df-$i" >/dev/null; done
  check starts-beside-df "$([ "$stuck" = 0 ] && [ "$failed" = 0 ] && [ "$ran" = "$N" ] && echo 1)" \
    "$ran starts, $stuck stuck, $failed failed; start $(stats "$WORK/a.log" 2); $dfm"
fi

# (b) rm -f racing docker run --rm: the container's own removal recorded a
# second exit code, 0, over the kill's 137, and the run exited 0 (#622).
if selected rm-f-exit-code; then
  PER=$((8 * ROUNDS)) WAITERS=$((10 * ROUNDS))
  case_start rm-f-exit-code "$PER runs at each of 6 delays, rm -f then; $WAITERS runs with 4 waiters; 60 s a run"
  : > "$WORK/b.log"; B_TIMEOUTS=0
  for d in 0 0.02 0.05 0.1 0.2 0.5; do
    # A wedged engine: three runs that never ended say enough.
    [ "$B_TIMEOUTS" -ge 3 ] && { echo "  stopped after $B_TIMEOUTS runs got no answer"; break; }
    for i in $(seq "$PER"); do
      n=$P-rm-$i
      : > "$WORK/b.out"
      TM_OUT=$WORK/b.out tm 60 "$D" run --rm "${L[@]}" --name "$n" $IMG sleep 300 > "$WORK/b.run" &
      pid=$!
      sleep $d
      # rm -f may land before the container exists: again until the run ends.
      for _ in $(seq 60); do
        tm 15 "$D" rm -f "$n" >/dev/null
        kill -0 $pid 2>/dev/null || break
        sleep 0.3
        kill -0 $pid 2>/dev/null || break
      done
      wait $pid
      read -r rc _ to < "$WORK/b.run"
      # rm -f before the CLI attached or started it: the container is gone
      # or going and the run says so (a 404 or 409, as Docker gives), which
      # is not this race.
      [ "$rc" != 137 ] && [ "$rc" != 0 ] && [ "$to" = 0 ] && grep -qE 'received 404|No such container|is being removed|removal of container|removed before it finished starting' "$WORK/b.out" && rc=gone
      echo "$d $rc $to" >> "$WORK/b.log"
      [ "$to" = 1 ] && B_TIMEOUTS=$((B_TIMEOUTS + 1))
      [ "$rc" = 137 ] || echo "${d}s exit $rc: $(tr -d '\r' < "$WORK/b.out" | grep . | grep -v "^Run '" | tail -1)" >> "$WORK/b.msgs"
    done
  done
  tally=$(awk '{print ($3 == 1 ? "timeout" : $2 == "gone" ? "removed-before-attach" : "exit " $2)}' "$WORK/b.log" | sort | uniq -c | awk '{c=$1; $1=""; printf "%s%s x%d", s, substr($0, 2), c; s=", "}')
  zero=$(awk '$2 == 0' "$WORK/b.log" | wc -l | tr -d ' ')
  bad=$(awk '($2 != 137 && $2 != "gone") || $3 == 1' "$WORK/b.log" | wc -l | tr -d ' ')
  badd=$(awk '($2 != 137 && $2 != "gone") || $3 == 1 {print $1 "s:" $2}' "$WORK/b.log" | sort | uniq -c | awk '{printf "%s%s x%d", s, $2, $1; s=", "}')
  # The same race seen by four waiters at once: wait until removed, until the
  # next exit, until not running (over the API), and docker wait.
  # A wait that reaches the engine after the removal gets "No such
  # container", from Docker too: its ContainerWait looks the container up
  # first and answers 404 when it is gone. The docker CLI takes a while to
  # start (over 0.1 s now and then on a slow VM), so docker wait gets a head
  # start, and if it still came late that round counts as late, not as a
  # wrong code. Late in most rounds means the race was not tested: a fail.
  : > "$WORK/w.log"
  [ "$B_TIMEOUTS" -ge 3 ] && WAITERS=0
  for i in $(seq "$WAITERS"); do
    n=$P-w-$i
    tm 30 "$D" run -d --rm "${L[@]}" --name "$n" $IMG sleep 300 >/dev/null
    for c in removed next-exit not-running; do
      curl -s -m 30 --unix-socket "$SOCK" -X POST "http://d/containers/$n/wait?condition=$c" > "$WORK/w.$c" &
    done
    lim 30 "$D" wait "$n" > "$WORK/w.cli" 2>&1 &
    sleep 0.5
    tm 15 "$D" rm -f "$n" >/dev/null
    wait
    cli=$(tr -d '\r\n' < "$WORK/w.cli")
    case $cli in *"No such container"*) cli=late ;; esac
    echo "removed=$(grep -o '"StatusCode":[0-9]*' "$WORK/w.removed" | cut -d: -f2) next-exit=$(grep -o '"StatusCode":[0-9]*' "$WORK/w.next-exit" | cut -d: -f2) not-running=$(grep -o '"StatusCode":[0-9]*' "$WORK/w.not-running" | cut -d: -f2) docker-wait=$cli" >> "$WORK/w.log"
  done
  W_OK='^removed=137 next-exit=137 not-running=137 docker-wait=(137|late)$'
  wbad=$(grep -Evc "$W_OK" "$WORK/w.log")
  wlate=$(grep -c 'docker-wait=late$' "$WORK/w.log")
  [ $((wlate * 2)) -gt "$WAITERS" ] && wbad=$((wbad + wlate))
  wfirst=$(grep -Ev "$W_OK" "$WORK/w.log" | sort | uniq -c | head -3 | sed 's/^ *//' | paste -sd ';' -)
  [ -s "$WORK/b.msgs" ] && sed "s/$P-rm-[0-9]*/NAME/g" "$WORK/b.msgs" | sort | uniq -c | sort -rn | head -5 | sed 's/^ */  seen /'
  check rm-f-exit-code "$([ "$bad" = 0 ] && [ "$wbad" = 0 ] && echo 1)" \
    "$(wc -l < "$WORK/b.log" | tr -d ' ') runs: $tally; exit 0 in $zero${badd:+ (not 137 at: $badd)}; waiters: $wbad of $WAITERS rounds saw another code${wfirst:+ ($wfirst)}, docker wait late in $wlate"
fi

# (c) rm -f or stop sent while several containers start at once, beside
# system df so that a start can hang as in (a). A start that hung held its
# container's lock, so neither answered (#621). Every command must answer
# and nothing may be left behind. The racing start answers as Docker's
# does: beside a stop, it succeeds (the stop waits for it, then stops the
# container); beside rm -f, it succeeds, or the removal got there first and
# it fails with Docker's "marked for removal" (or "No such container").
during_start() { # NAME VERB
  local name=$1 verb=$2 K=6 R=$((3 * ROUNDS)) r i d pids slow=0 left=0 cmds=0 rc msg
  local s_ok=0 s_removal=0 s_gone=0 s_bad=0
  : > "$WORK/s.bad"
  case_start "$name" "$R rounds of $K starts at once beside 4 system df loops, $verb sent meanwhile; 20 s a create, 30 s a start, 15 s a $verb"
  : > "$WORK/c.ids"; : > "$WORK/c.log"
  fill_dfwalk
  df_start 4
  for r in $(seq "$R"); do
    for i in $(seq $K); do
      : > "$WORK/c.id"
      echo "create $(TM_OUT=$WORK/c.id tm 20 "$D" create "${L[@]}" --name "$P-c-$i" $IMG sleep 300)" >> "$WORK/c.log"
      tr -d '\r' < "$WORK/c.id" | grep -E '^[0-9a-f]{12,}$' >> "$WORK/c.ids"
    done
    pids=
    for i in $(seq $K); do
      (
        : > "$WORK/s.$i"
        res=$(TM_OUT=$WORK/s.$i tm 30 "$D" start "$P-c-$i")
        echo "start $res" >> "$WORK/c.log"
        echo "${res%% *} $(tr -d '\r' < "$WORK/s.$i" | paste -sd ' ' -)" > "$WORK/s.$i.res"
      ) &
      pids="$pids $!"
    done
    # Into the start at a different moment each round.
    d=$(awk -v r="$r" 'BEGIN {printf "%.2f", (r % 4) * 0.05}')
    sleep "$d"
    for i in $(seq $K); do
      if [ "$verb" = stop ]; then (echo "stop $(tm 15 "$D" stop -t 1 "$P-c-$i")" >> "$WORK/c.log") &
      else (echo "rm $(tm 15 "$D" rm -f "$P-c-$i")" >> "$WORK/c.log") &
      fi
      pids="$pids $!"
    done
    # Not a bare wait: the df loops run on until df_stop.
    # shellcheck disable=SC2086
    wait $pids
    for i in $(seq $K); do
      read -r rc msg < "$WORK/s.$i.res"
      if [ "$rc" = 0 ]; then s_ok=$((s_ok + 1))
      elif [ "$verb" = rm ] && [[ $msg == *"marked for removal"* ]]; then s_removal=$((s_removal + 1))
      elif [ "$verb" = rm ] && [[ $msg == *"No such container"* ]]; then s_gone=$((s_gone + 1))
      else s_bad=$((s_bad + 1)); echo "exit $rc: ${msg//$P-c-[0-9]/NAME}" >> "$WORK/s.bad"
      fi
    done
    # A stopped container is then removed, which must also answer.
    if [ "$verb" = stop ]; then
      for i in $(seq $K); do echo "rm $(tm 15 "$D" rm -f "$P-c-$i")" >> "$WORK/c.log"; done
    fi
    # Gone answers "No such container"; no answer at all counts as left too.
    for i in $(seq $K); do lim 15 "$D" container inspect "$P-c-$i" 2>&1 | grep -q 'No such' || left=$((left + 1)); done
    # A wedged engine: one round with commands over their limit says enough.
    awk '$4 == 1 {f=1} END {exit !f}' "$WORK/c.log" && { echo "  stopped after round $r: commands over their limit"; break; }
  done
  local dfm; df_stop; dfm=$DF_SEEN
  # Interfaces and cgroups go a moment after the answer: up to 10 s.
  local host hleft
  for _ in $(seq 20); do
    host=$(host_leftovers "$WORK/c.ids"); hleft=$(echo "$host" | awk '{print $1 + $3}')
    { [ "$SYS" = mac ] || [ "$hleft" = 0 ]; } && break
    sleep 0.5
  done
  cmds=$(wc -l < "$WORK/c.log" | tr -d ' ')
  slow=$(awk '$4 == 1' "$WORK/c.log" | wc -l | tr -d ' ')
  local slows; slows=$(awk '$4 == 1 {print $1}' "$WORK/c.log" | sort | uniq -c | awk '{printf "%s%s x%d", s, $2, $1; s=", "}')
  local sfirst; sfirst=$(sed 's/[0-9a-f]\{12,64\}/ID/g' "$WORK/s.bad" | sort | uniq -c | sort -rn | head -3 | sed 's/^ *//' | paste -sd ';' -)
  check "$name" "$([ "$slow" = 0 ] && [ "$left" = 0 ] && [ "$s_bad" = 0 ] && { [ "$SYS" = mac ] || [ "$hleft" = 0 ]; } && echo 1)" \
    "$cmds commands, $slow over their limit${slows:+ ($slows)}; start $(awk '$1 == "start"' "$WORK/c.log" | stats /dev/stdin 3), $verb $(awk -v v="$verb" '$1 == v' "$WORK/c.log" | stats /dev/stdin 3); racing starts: $s_ok started$([ "$verb" = rm ] && echo ", $s_removal marked for removal, $s_gone gone"), $s_bad otherwise${sfirst:+ ($sfirst)}; left: $left containers, $host (given 10 s to go); $dfm"
}
selected rm-f-during-start && during_start rm-f-during-start rm
selected stop-during-start && during_start stop-during-start stop

# (d) Everyday commands beside system df for a while: every command must
# answer within its limit, and none may fail.
if selected churn-beside-df; then
  W=3
  case_start churn-beside-df "${CHURN_SECONDS}s of $W workers (volume create, run, exec, rm -f, volume rm) beside 4 system df loops; 30 s a command"
  fill_dfwalk
  df_start 4
  end=$((SECONDS + CHURN_SECONDS)) WPIDS=
  for w in $(seq $W); do
    (
      k=0
      while [ $SECONDS -lt $end ]; do
        k=$((k + 1)); n=$P-ch-$w-$k v=$P-chv-$w-$k
        echo "volume-create $(tm 30 "$D" volume create "${L[@]}" "$v")"
        echo "run $(tm 30 "$D" run -d "${L[@]}" --name "$n" -v "$v:/data" $IMG sh -c 'echo hi > /data/f; sleep 30')"
        echo "exec $(tm 30 "$D" exec "$n" cat /data/f)"
        echo "rm $(tm 30 "$D" rm -f "$n")"
        echo "volume-rm $(tm 30 "$D" volume rm "$v")"
      done > "$WORK/d.$w.log"
    ) &
    WPIDS="$WPIDS $!"
  done
  # shellcheck disable=SC2086
  wait $WPIDS; WPIDS=
  df_stop; dfm=$DF_SEEN
  cat "$WORK"/d.*.log > "$WORK/d.log"
  cmds=$(wc -l < "$WORK/d.log" | tr -d ' ')
  over=$(awk '$4 == 1' "$WORK/d.log" | wc -l | tr -d ' ')
  errs=$(awk '$4 == 0 && $2 != 0' "$WORK/d.log" | wc -l | tr -d ' ')
  per=$(for op in volume-create run exec rm volume-rm; do printf '%s %s; ' "$op" "$(awk -v o=$op '$1 == o' "$WORK/d.log" | stats /dev/stdin 3)"; done)
  which=$(awk '$4 == 1 || $2 != 0 {print $1 ($4 == 1 ? ":timeout" : ":exit" $2)}' "$WORK/d.log" | sort | uniq -c | awk '{printf "%s%s x%d", s, $2, $1; s=", "}')
  check churn-beside-df "$([ "$cmds" -gt 0 ] && [ "$over" = 0 ] && [ "$errs" = 0 ] && echo 1)" \
    "$cmds commands, $over over their limit, $errs failed${which:+ ($which)}; ${per}$dfm"
fi

# (e) buildx's docker-container builder made, bootstrapped and removed, round
# after round, each with a fresh name: every first bootstrap must start its
# BuildKit container, with no retry. Kiri 0.1.18 closed the connection after
# the copy into the container without saying so, and the start sent next on
# it failed with "connection reset by peer" in about one round in six (#83).
if selected buildx-bootstrap-loop; then
  N=$((20 * ROUNDS)) LIM=120
  case_start buildx-bootstrap-loop "$N buildx create, bootstrap, rm rounds; $LIM s a bootstrap"
  if ! docker buildx version >/dev/null 2>&1; then
    SKIP=$((SKIP + 1)); echo "  skip: buildx not installed"
  else
    BK=$(kiri_image_remote moby/buildkit:buildx-stable-1)
    : > "$WORK/e.log"; e_bad=0
    for i in $(seq "$N"); do
      b=$P-bx-$i
      TM_OUT=$WORK/e.out tm 30 docker buildx create --name "$b" --driver docker-container --driver-opt default-load=true --driver-opt "image=$BK" "$DOCKER_CONTEXT" >/dev/null
      : > "$WORK/e.out"
      read -r rc ms to <<<"$(TM_OUT=$WORK/e.out tm $LIM docker buildx inspect "$b" --bootstrap)"
      st=$(lim 20 "$D" container inspect "buildx_buildkit_${b}0" --format '{{.State.Status}}' 2>&1)
      echo "$rc $ms $to $st" >> "$WORK/e.log"
      if [ "$rc" != 0 ] || [ "$st" != running ]; then
        e_bad=$((e_bad + 1))
        [ "$e_bad" -le 3 ] && echo "  round $i: exit $rc, container $st: $(grep -i error "$WORK/e.out" | tail -1 | sed "s/$b/NAME/g")"
      fi
      tm 60 docker buildx rm -f "$b" >/dev/null
      lim 20 "$D" rm -f "buildx_buildkit_${b}0" >/dev/null 2>&1
      # A wedged engine: three bootstraps over their limit say enough.
      [ "$(awk '$3 == 1' "$WORK/e.log" | wc -l)" -ge 3 ] && { echo "  stopped after 3 bootstraps over $LIM s"; break; }
    done
    ran=$(wc -l < "$WORK/e.log" | tr -d ' ')
    check buildx-bootstrap-loop "$([ "$ran" = "$N" ] && [ "$e_bad" = 0 ] && echo 1)" \
      "$ran bootstraps, $e_bad failed or left the container not running; bootstrap $(stats "$WORK/e.log" 2)"
  fi
fi

printf '\n%d passed, %d failed, %d skipped in %d s\n' "$PASS" "$FAIL" "$SKIP" $((SECONDS - T_START))
for f in "${FAILED[@]+"${FAILED[@]}"}"; do echo "  failed: $f"; done
[ "$FAIL" -eq 0 ]
