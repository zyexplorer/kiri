#!/bin/bash
# The Mac's Finder mount at ~/Kiri covers only what is safe to cover (issue
# #40): no folder, an empty one, one holding only Finder's .DS_Store and
# .localized, or a mount this engine left behind. A folder with files (also
# ~/kiri on a case-insensitive disk), a file, a link, or someone else's mount
# is left alone, and stopping the engine unmounts only its own mount.
#
# Each case runs a real `kiri daemon` in a throwaway HOME, so the real ~/Kiri
# is never touched. macOS only; needs no VM image or admin rights:
#
#   cargo xtask host && tests/finder-mount.sh [target/release/kiri]
[ "$(uname)" = Darwin ] || { echo "macOS only"; exit 0; }
BIN=${1:-target/release/kiri}
[ -x "$BIN" ] || { echo "no kiri binary at $BIN (cargo xtask host)"; exit 1; }
pass=0; fail=0
ok() { echo "  PASS $*"; pass=$((pass+1)); }
no() { echo "  FAIL $*"; fail=$((fail+1)); }
ours() { mount | grep -F "<$H/.kiri/files/nfs.sock>:/ on $H/Kiri " >/dev/null; }
start() { HOME="$H" KIRI_DATA_DIR="$H/data" "$BIN" daemon >"$H/out.log" 2>&1 & PID=$!
  for i in $(seq 1 60); do grep -qE "container files in Finder|Finder mount failed" "$H/out.log" "$H"/.kiri/*.log 2>/dev/null && return; sleep 0.25; done; echo "  (no Finder line in log)"; }
stop() { kill ${1:-} $PID 2>/dev/null; wait $PID 2>/dev/null; sleep 0.5; }
logline() { grep -hE "container files in Finder|Finder mount failed" "$H/out.log" "$H"/.kiri/*.log 2>/dev/null | tail -1; }
fresh() { H=$(mktemp -d "$HOME/.kiri-e2e-XXXX"); echo "== $1"; }
done_() { mount | grep -F "$H" | awk '{print $3}' | while read m; do umount -f "$m" 2>/dev/null || diskutil unmount force "$m" >/dev/null 2>&1; done; rm -rf "$H"; }

fresh "folder with files"; mkdir "$H/Kiri"; echo keep > "$H/Kiri/keep.txt"; start
[ -f "$H/Kiri/keep.txt" ] && ok "keep.txt still visible while running" || no "keep.txt hidden"
ours && no "mounted over files" || ok "no mount"
logline | grep -q "already has files" && ok "warning logged: $(logline | cut -c1-160)" || no "no clear warning: $(logline)"
stop; [ "$(cat $H/Kiri/keep.txt)" = keep ] && ok "keep.txt intact after stop" || no "keep.txt lost"; done_

fresh "lowercase ~/kiri git clone (case-insensitive disk)"; mkdir -p "$H/kiri/.git"; echo x > "$H/kiri/README.md"; start
[ -f "$H/kiri/README.md" ] && ok "clone visible" || no "clone hidden"; ours && no "mounted" || ok "no mount"; stop; done_

fresh "only hidden files (.git)"; mkdir -p "$H/Kiri/.git"; start; ours && no "mounted over .git" || ok "no mount"; stop; done_

fresh "another mount (disk image)"; hdiutil create -quiet -size 2m -fs HFS+ -volname KTEST "$H/t.dmg"; mkdir "$H/Kiri"
hdiutil attach -quiet -nobrowse -mountpoint "$H/Kiri" "$H/t.dmg"; echo img > "$H/Kiri/img.txt"; start
mount | grep -q " on $H/Kiri (hfs" && ok "disk image still mounted while running" || no "disk image unmounted"
ours && no "nfs mounted over it" || ok "no nfs mount"
logline | grep -q "mounted on it" && ok "warning logged" || no "no warning: $(logline)"
stop; mount | grep -q " on $H/Kiri (hfs" && ok "disk image still mounted after stop" || no "disk image force-unmounted on stop"
hdiutil detach -quiet "$H/Kiri" 2>/dev/null; done_

fresh "empty folder"; mkdir "$H/Kiri"; start; ours && ok "mounted" || no "not mounted: $(logline)"; stop; ours && no "still mounted after stop" || ok "unmounted on stop"; done_

fresh "only .DS_Store and .localized"; mkdir "$H/Kiri"; touch "$H/Kiri/.DS_Store" "$H/Kiri/.localized"; start; ours && ok "mounted" || no "not mounted: $(logline)"; stop; [ -f "$H/Kiri/.DS_Store" ] && ok ".DS_Store kept" || no ".DS_Store gone"; done_

fresh "no folder"; start; [ -d "$H/Kiri" ] && ok "folder made" || no "no folder"; ours && ok "mounted" || no "not mounted: $(logline)"; stop; done_

fresh "file at ~/Kiri"; echo f > "$H/Kiri"; start; [ "$(cat $H/Kiri)" = f ] && ok "file left alone" || no "file changed"; ours && no mounted || ok "no mount"; stop; done_

fresh "symlink at ~/Kiri"; mkdir "$H/elsewhere"; ln -s "$H/elsewhere" "$H/Kiri"; start; [ -L "$H/Kiri" ] && ok "link left alone" || no "link changed"; ours && no mounted || ok "no mount"; stop; done_

fresh "mount left by kill -9"; start; ours && ok "first mount" || no "first not mounted"; stop -9
ours && ok "stale mount left behind (expected after -9)" || echo "  (no stale mount after -9)"
start; [ "$(mount | grep -cF " on $H/Kiri ")" = 1 ] && ours && ok "replaced with one fresh mount" || no "not replaced: $(logline)"
stop; ours && no "still mounted" || ok "unmounted on stop"; done_

echo; echo "passed $pass, failed $fail"; [ $fail = 0 ]
