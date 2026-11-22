#!/bin/sh
# A container that starts with a new volume over a big image folder fills the
# volume before its root is in place. Its start must wait for that (#311),
# but the engine must keep serving every other container meanwhile.
#
# Prints one line, beginning "pass:" or "fail:". It fails when:
# - while the slow start had not returned, an exec in a bystander container
#   failed, or took both half that start's own time and at least 250 ms (a
#   stall: see `stalled` below);
# - an exec into the starting container itself, sent during the fill, did
#   not wait for the start and then run as the image's user;
# - right after the start returned, an exec as the image's user failed, or
#   the volume does not hold the whole folder;
# - a `docker pause`, a `docker commit` and a container that joins its
#   network (`--network container:`), all sent during the fill, did not
#   each finish, the start with them, or `docker unpause`, `stop` and `rm`
#   did not work on it afterwards;
# - a second container started on the same new volume during the fill did
#   not run, or counted fewer files than the folder holds (it must wait
#   for the fill, not run on a half filled volume), or the first start
#   failed with it;
# - a start that fails after the fill began (an unknown sysctl) left the
#   volume half filled;
# - a start whose fill runs out of space did not fail with "No space left",
#   or its restart policy started it anyway.
# A disk so fast that even the biggest payload fills in under 500 ms cannot
# show a stall either way: "pass: not measured", as when the time runs out.
# Every check that needs no timing still runs then.
#
# `start-stall.sh cp`, where the engine's own root is this shell's (a Linux
# host engine): also copies a uniquely named marker into the volume's path
# while the container is still starting. The copy must succeed and the
# marker be inside the container once it runs. Nothing may enter a container
# before its root is in place, so the marker must never appear in the
# engine's own root. When the fill could be measured, the copy and the exec
# into the starting container must have been sent before the start
# returned, or they prove nothing: if they were not, the try is made once
# more with a bigger payload, and it fails if they are late again.
#
# Run by tests/linux-host.sh, tests/wsl.sh and tests/e2e.sh (under its
# 60 s alarm): plain POSIX shell, the docker CLI ($D, default docker), at
# most START_STALL_BUDGET seconds (default 40) of tries started, then the
# checks on a failing fill, and no step waited for longer than the alarm
# leaves room for.
set -u
D=${D:-docker}
mode=${1:-}
budget=${START_STALL_BUDGET:-40}
# Milliseconds (10 ms steps on Linux, where busybox date has no %N; perl
# on macOS, which has no /proc).
now() {
  if [ -r /proc/uptime ]; then
    read -r up _ </proc/uptime
    echo $((${up%.*} * 1000 + 1${up#*.} * 10 - 1000))
  else
    perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'
  fi
}
begun=$(now)
run=$(date +%s)$$
tmp=${TMPDIR:-/tmp}/start-stall-$run
mkdir -p "$tmp"
names=
cleanup() {
  for n in $names; do
    case $n in
      c:*) $D rm -f "${n#c:}" ;;
      v:*) $D volume rm -f "${n#v:}" ;;
      i:*) $D rmi -f "${n#i:}" ;;
    esac
  done >/dev/null 2>&1
  rm -rf "$tmp"
}
trap cleanup EXIT
# Cut short (e2e.sh's alarm, ^C): still clean up.
trap 'exit 1' INT TERM HUP ALRM
say() {
  echo "$1"
  exit 0
}

# `job NAME CMD...` runs CMD in the background: its output goes to
# $tmp/NAME.out and, once it ends, its exit status to $tmp/NAME. It holds
# none of this script's own streams, so one that never ends cannot keep a
# caller reading them (e2e.sh) waiting after the verdict.
job() {
  j=$1
  shift
  rm -f "$tmp/$j" "$tmp/$j.out"
  ( "$@" >"$tmp/$j.out" 2>&1; echo $? >"$tmp/$j" ) </dev/null >/dev/null 2>&1 &
}
# `await NAME DEADLINE`: true once job NAME has ended, false if it has not
# by DEADLINE (milliseconds, as `now` counts).
await() {
  while [ ! -f "$tmp/$1" ]; do
    [ "$(now)" -lt "$2" ] || return 1
    sleep 0.1
  done
}
# A job's exit status ("none" if it never ran or has not ended) and the
# start of its output.
status() { cat "$tmp/$1" 2>/dev/null || echo none; }
said() { head -c 200 "$tmp/$1.out" 2>/dev/null; }
# The start, timed: $tmp/slow holds its milliseconds once it returns.
timed() {
  s=$(now)
  "$@"
  r=$?
  echo $(($(now) - s)) >"$tmp/slow"
  return $r
}
# A pause or a commit (which pauses too, then unpauses) refused because the
# other has the container paused at that moment has finished all the same.
finished() { [ "$(status "$1")" = 0 ] || grep -q "already paused" "$tmp/$1.out" 2>/dev/null; }

# A stall: an exec in another container that waited for the fill. On main a
# bystander exec sent during the fill waits for all of what is left of it,
# so it takes most of the start (the first is sent about 100 to 200 ms in);
# with the fix one takes a few tens of milliseconds, and on a loaded
# machine one may take 200 ms. So both: half the start or more (a ratio, so
# a long start is not judged by a short exec's rules) and 250 ms or more (a
# floor, so one slow exec on a loaded machine is not a stall). Only starts
# of 500 ms or more are judged: on main their bystander exec waits at
# least 300 ms, above the floor.
measurable=500
stalled() { [ $(($1 * 2)) -ge "$2" ] && [ "$1" -ge 250 ]; }

# Checks that need no timing, on a start that fails during its fill.
# Appends to $fails.
fill_checks() {
  img=$1 files=$2 tag=$3
  names="$names c:stall-cut-$tag c:stall-full-$tag v:stall-cut-$tag v:stall-full-$tag"
  # A start that fails after its fill began must not leave the volume half
  # filled: no longer empty, it would never be filled again.
  if $D run -d --name stall-cut-$tag --sysctl net.ipv4.kiri_no_such_key=1 --mount type=volume,source=stall-cut-$tag,target=/payload "$img" sleep 300 >"$tmp/cut.out" 2>&1; then
    fail "a start with an unknown sysctl did not fail"
  fi
  got=$($D run --rm --mount type=volume,source=stall-cut-$tag,target=/payload "$img" sh -c "$sum" 2>&1)
  [ "$got" = "$want" ] || fail "a start that failed after its fill began left the volume half filled"
  # A fill that runs out of room (inodes for half the files) fails the
  # start with the reason, and a restart policy does not start it anyway.
  $D volume create --opt type=tmpfs --opt device=tmpfs --opt o=size=512m,nr_inodes=$((files / 2)) stall-full-$tag >/dev/null
  if $D run -d --name stall-full-$tag --restart always --mount type=volume,source=stall-full-$tag,target=/payload "$img" sleep 300 >"$tmp/full.out" 2>&1; then
    fail "a start whose volume ran out of room did not fail"
  elif ! grep -q "No space left" "$tmp/full.out"; then
    fail "a start whose volume ran out of room did not say why: $(head -c 200 "$tmp/full.out")"
  fi
  sleep 2
  state=$($D inspect -f '{{.RestartCount}} {{.State.Running}}' stall-full-$tag 2>&1)
  [ "$state" = "0 false" ] || fail "a failed start was restarted by its restart policy (restarts, running: $state)"
}
fails=
fail() { fails="$fails; $1"; }

# One try with a payload of $1 MB in one file and $2 small files in folders
# of 10,000, plus this run's own tag, so its layer is never one an earlier
# run (or another build) already unpacked. Sets $result: "slow" when the
# fill was too quick to measure and every other check passed, else the line
# to print. Leaves $img, $tag, $sum and $want for `fill_checks`.
attempt() {
  tag=$run-$2
  img=kiri-test/stall:$tag by=stall-by-$tag big=stall-big-$tag vol=stall-vol-$tag join=stall-join-$tag second=stall-second-$tag
  names="$names c:stall-seed-$tag c:$join c:$second c:$by c:$big v:$vol i:$img i:$img-c"
  marker=kiri-start-stall-marker-$tag
  $D run --name stall-seed-$tag alpine sh -c "adduser -D dev && mkdir -p /payload && echo $tag > /payload/tag &&
    dd if=/dev/zero of=/payload/big bs=1M count=$1 2>/dev/null &&
    seq $2 | xargs -n 1000 sh -c 'd=/payload/d\$((\$1 / 10000)); mkdir -p \$d; for f; do echo \$f > \$d/f\$f; done' _ &&
    chown -R dev /payload" >/dev/null 2>"$tmp/err" || say "fail: could not make the test image: $(head -c 300 "$tmp/err")"
  $D commit -c 'USER dev' stall-seed-$tag "$img" >/dev/null 2>"$tmp/err" || say "fail: could not commit the test image: $(head -c 300 "$tmp/err")"
  $D rm -f stall-seed-$tag >/dev/null
  sum="cd /payload && find . -type f ! -name 'kiri-start-stall-marker-*' | sort | xargs md5sum | md5sum"
  want=$($D run --rm "$img" sh -c "$sum")
  $D run -d --name "$by" alpine sleep 300 >/dev/null
  $D exec "$by" true || say "fail: the bystander container cannot exec"
  rm -f "$tmp/slow"
  job start timed $D run -d --name "$big" --mount type=volume,source="$vol",target=/payload "$img" sleep 300
  begin=$(now)
  max=0 execs=0 bad=0 w=
  # Sent once it shows as running, while it still starts (or right after,
  # if no pass saw it): an exec into it, a pause, a commit, a container
  # joining its network, and in `cp` mode a copy into its volume. In the
  # background, so a request that waits does not stop the timing.
  send() {
    [ -f "$tmp/slow" ] && w=after || w=before
    job inner $D exec --user dev "$big" id -un
    job pause $D pause "$big"
    job commit $D commit "$big" "$img-c"
    job join $D run -d --name "$join" --network container:"$big" alpine sleep 300
    # A second container on the same new volume counts what it sees once it
    # runs: its start must wait for the fill (the marker of `cp` is not
    # part of the folder).
    job second $D run --rm --name "$second" --mount type=volume,source="$vol",target=/payload "$img" sh -c "find /payload -type f ! -name 'kiri-start-stall-marker-*' | wc -l"
    if [ "$mode" = cp ]; then
      echo marker >"$tmp/$marker"
      job cp $D cp "$tmp/$marker" "$big:/payload/$marker"
    fi
  }
  # Every exec begun before the start returned counts, however long it
  # took. A start still not back after 15 s is frozen (a pause or a commit
  # during its fill froze the child filling the volume): thawed, so the
  # clean-up can go on, and failed.
  while [ ! -f "$tmp/slow" ]; do
    if [ $(($(now) - begin)) -ge 15000 ]; then
      $D unpause "$big" >/dev/null 2>&1
      say "fail: the start did not return within 15 s, with a pause, a commit and a container joining its network sent during its fill; execs=$execs exec-max=${max}ms"
    fi
    if [ -z "$w" ] && [ "$($D inspect -f '{{.State.Status}}' "$big" 2>/dev/null)" = running ]; then
      send
    fi
    s=$(now)
    $D exec "$by" true >/dev/null 2>&1 || bad=$((bad + 1))
    e=$(($(now) - s))
    execs=$((execs + 1))
    [ $e -gt $max ] && max=$e
    # A pause, so the execs measure the engine rather than load it.
    sleep 0.1
  done
  [ -n "$w" ] || send
  slow=$(cat "$tmp/slow")
  await start $(($(now) + 5000))
  [ "$(status start)" = 0 ] && [ "$($D inspect -f '{{.State.Running}}' "$big" 2>/dev/null)" = true ] ||
    say "fail: the slow start failed: $(said start)"
  # Every check runs, and every failure is listed.
  fails=
  late=
  # The pause and the commit first: an exec or a copy that started while
  # the container was paused goes on only once it is unpaused.
  until=$(($(now) + 10000))
  for j in pause commit; do
    await $j $until || late="$late $j"
  done
  finished pause || fail "a pause sent $w the start returned failed: $(said pause)"
  finished commit || fail "a commit sent $w the start returned failed: $(said commit)"
  if [ "$($D inspect -f '{{.State.Paused}}' "$big" 2>&1)" = true ]; then
    $D unpause "$big" >"$tmp/unpause.out" 2>&1 || fail "docker unpause afterwards failed: $(said unpause)"
  fi
  jobs="inner join second"
  [ "$mode" = cp ] && jobs="$jobs cp"
  for j in $jobs; do
    await $j $until || late="$late $j"
  done
  [ -z "$late" ] || fail "sent during the start, these never finished:$late"
  # The pause sent with it can get there first: the exec is then refused
  # because the container is paused, which says nothing about the start.
  # It must work now that the container is unpaused.
  if [ "$(status inner)" != 0 ] && said inner | grep -q 'is paused'; then
    $D exec --user dev "$big" id -un >"$tmp/inner.out" 2>&1
    echo $? >"$tmp/inner"
  fi
  line="start=${slow}ms exec-max=${max}ms execs=$execs sent=$w"
  [ $bad -eq 0 ] || fail "$bad bystander exec(s) failed"
  if [ "$slow" -ge $measurable ] && stalled "$max" "$slow"; then
    fail "an exec in another container waited for the fill (a stall)"
  fi
  user=$($D exec --user dev "$big" id -un 2>&1)
  got=$($D exec "$big" sh -c "$sum" 2>&1)
  [ "$user" = dev ] || fail "an exec as the image's user right after the start failed: $user"
  [ "$got" = "$want" ] || fail "the volume does not hold the whole folder"
  if [ "$(status inner)" = 0 ] && [ "$(said inner)" = dev ]; then
    line="$line inner-exec=$w"
  else
    fail "an exec into the starting container, sent $w the start returned, did not wait for it and run as its user: $(said inner)"
  fi
  if [ "$(status join)" = 0 ] && [ "$($D inspect -f '{{.State.Running}}' "$join" 2>&1)" = true ]; then
    :
  else
    fail "a container joining the starting container's network, sent $w the start returned, did not start: $(said join)"
  fi
  # The folder holds the tag file, the big file and $2 small ones.
  seen=$(said second | tr -d ' \n')
  if [ "$(status second)" != 0 ] || [ "$seen" != "$(($2 + 2))" ]; then
    fail "a second container on the volume, started $w the first start returned, saw ${seen:-nothing} of $(($2 + 2)) files: $(said second)"
  fi
  if [ "$mode" = cp ]; then
    [ "$(status cp)" = 0 ] || fail "a copy into the starting container, sent $w the start returned, failed: $(said cp)"
    if [ -e "/payload/$marker" ] || [ -e "/$marker" ]; then
      rm -f "/payload/$marker" "/$marker"
      fail "a copy into the starting container landed in the engine's own root"
    fi
    $D exec "$big" test -e "/payload/$marker" || fail "a copy into the starting container is not in its volume"
    line="$line cp=$w"
  fi
  # Nothing is left holding it: stop and rm go through.
  $D rm -f "$join" "$second" >/dev/null 2>&1
  until=$(($(now) + 10000))
  job stop $D stop -t 0 "$big"
  await stop $until && [ "$(status stop)" = 0 ] || fail "docker stop afterwards did not finish: $(said stop)"
  job rm $D rm "$big"
  await rm $until && [ "$(status rm)" = 0 ] || fail "docker rm afterwards did not finish: $(said rm)"
  $D rm -f "$by" >/dev/null 2>&1
  $D volume rm -f "$vol" >/dev/null 2>&1
  $D rmi -f "$img-c" >/dev/null 2>&1
  if [ -n "$fails" ]; then
    result="fail: ${fails#; }; $line"
  elif [ "$slow" -lt $measurable ]; then
    result=slow
  elif [ "$mode" = cp ] && [ "$w" = after ]; then
    result=late
  else
    fill_checks "$img" "$2" "$tag"
    if [ -n "$fails" ]; then
      result="fail: ${fails#; }; $line"
    else
      result="pass: $line"
    fi
  fi
}

# Bigger payloads when the disk is too fast to measure, while time allows
# (a try takes longer than the last one, so the next is started only when
# twice the last one's time is still left).
prev='' retried=''
sent_late="the exec and the copy into the starting container were sent only after its start returned"
for size in "32 30000" "64 60000" "128 100000"; do
  [ -z "$prev" ] || $D rmi -f "$prev" >/dev/null 2>&1
  t0=$(now)
  attempt $size
  case $result in
    slow) ;;
    late)
      [ -z "$retried" ] || say "fail: $sent_late, twice, so they prove nothing; $line"
      retried=1
      ;;
    *) say "$result" ;;
  esac
  prev=$img last=${size#* }
  took=$(($(now) - t0))
  [ $(($(now) - begun + 2 * took)) -lt $((budget * 1000)) ] || break
done
[ -z "$retried" ] || say "fail: $sent_late, and there was no time or bigger payload left to try again; $line"
# Not measured: what needs no timing still has to hold.
fails=
fill_checks "$prev" "$last" "$tag"
[ -z "$fails" ] || say "fail: ${fails#; }; not measured"
say "pass: not measured (the disk filled even the biggest volume tried in under ${measurable} ms, or time ran out)"
