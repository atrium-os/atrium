#!/bin/sh
# Regression test: a journal replay that REFUSES an inode must not leave its
# directory entry behind.
#
# THE BUG THIS PINS DOWN
#
#   The journal coalesces an inode's pending redo records to its latest state
#   and writes it in the same transaction as the directory entry. For a file
#   created and written in the window before a flush, that latest state names
#   a content manifest that is still only in RAM. The redo buffers reach the
#   disk on the buffer syncer's schedule; the manifest reaches a pack only when
#   Tessera flushes. Cut power in between and replay refuses the inode
#   ("journal replay REFUSED inode N — its manifest is in no pack"), which is
#   right, but used to replay the DIR_INSERT anyway. The first checkpoint then
#   committed a name for an inode that does not exist: ENOENT on lookup, EN0ENT
#   on O_CREAT of the same name, and `dangling dirent` from fsck. On the dev
#   root it took out /var/log/messages.
#
# HOW IT REPRODUCES IT, deterministically:
#   1. suppress the periodic flush (kern.tessera.flush_interval_sec),
#   2. create files, a populated directory and a hard link, and overwrite a
#      file that was already committed,
#   3. wait past kern.metadelay so the syncer writes the redo buffers,
#   4. power-cut exactly like vm-crash-soak-create.sh (QMP `quit`), relaunch.
#
# PASS = the replay refused and DROPPED the entries (mechanism counters, so a
# run where the scenario never reproduced cannot pass), none of the lost names
# survive, the committed file rolled back to its committed content, and fsck is
# CLEAN — in particular no "dangling dirent".
#
# The flush sysctl is global, so the ROOT volume is also unflushed for ~45 s
# before the cut. That is the same exposure the original incident had; with
# the fix, the root's replay drops rather than dangles too. Reboot restores the
# sysctl default.
#
# Destructive to the SCRATCH disk only (GEOM ident gate, #129).
#
#   sh scripts/vm-replay-refusal-test.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$BSD/scripts/vssh"; SOCK=/tmp/qmp.sock
DEV=/dev/vtbd2; M=/mnt/scratch

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
[ -f "$KO" ] || { echo "no $KO — build it first"; exit 1; }
# KMOD=<16-hex> overrides, to run this against a different module on purpose.
KMOD=${KMOD:-$(shasum -a 256 "$KO" | cut -c1-16)}

# Same readiness + power-cut + relaunch as vm-crash-soak-create.sh.
wait_ready(){ i=0; while [ $i -lt 150 ]; do $VSSH 'echo ready' >/dev/null 2>&1 && return 0; i=$((i+1)); sleep 3; done; echo "FATAL: VM never ready"; return 1; }
relaunch(){
  echo quit | nc -U -w2 $SOCK >/dev/null 2>&1
  i=0; while [ $i -lt 15 ]; do pgrep -f qemu-system-aarch64 >/dev/null || break; i=$((i+1)); sleep 2; done
  pgrep -f qemu-system-aarch64 >/dev/null && pkill -9 -f qemu-system-aarch64; sleep 2
  ( cd "$BSD" && ./scripts/run-vm.sh >/tmp/replayref-vmboot.log 2>&1 </dev/null & ) >/dev/null 2>&1 </dev/null
  sleep 1
  wait_ready
}
GATE="diskinfo -s $DEV | grep -q '^atrium-scratch\$' || { echo REFUSING_ident; exit 2; }"

wait_ready || exit 1
. "$BSD/scripts/lib/guest-ident.sh"   # GUEST_KMOD_HASH: the LOADED module, only on Laminar/RLC/tessera root
k=$($VSSH "$GUEST_KMOD_HASH" | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != tree [$KMOD]"; exit 1; }
echo "=== REPLAY-REFUSAL TEST $(date) kmod=$KMOD ==="

out=$($VSSH "$GATE
  mount | grep -q ' $M ' && umount $M; mkdir -p $M
  mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV $M || { echo MKFS_FAIL; exit 3; }
  mkdir $M/base && echo committed > $M/base/old
  head -c 300000 /dev/random > $M/base/committed_move
  head -c 300000 /dev/random > $M/base/committed_link
  sha256 -q $M/base/committed_move > /root/rr.move.sum; sha256 -q $M/base/committed_link > /root/rr.link.sum
  sync && sync
  sysctl -q kern.tessera.flush_interval_sec=100000
  # ★ The flush timer is ARMED ONCE and keeps its deadline (mark_dirty only
  # arms it when not already pending), so a callout the seeding sync armed at
  # the old 5 s interval still fires and flushes everything below. The first
  # version of this test lost exactly that race; wait it out.
  sleep 7
  # >4 MiB files: past kern.tessera.dirty_content_file_max, so each write
  # publishes its manifest into the in-RAM pending tier and journals an
  # INODE_WRITE naming it — the state the incident needed. Small files stay in
  # per-inode RAM buffers until a flush and journal no such manifest.
  for i in 1 2 3 4; do head -c 6000000 /dev/random > $M/base/new\$i; done
  for i in 5 6 7 8; do head -c 70000 /dev/random > $M/base/new\$i; done
  # Make newdir DETERMINISTICALLY unrestorable. The journal coalesces an
  # inode's pending records until the next drain, so if mkdir's record
  # (whose manifest is the durable empty-directory constant) drains on its
  # own, replay restores newdir as an empty directory and the drop path is
  # never exercised — a timing-dependent test, which is how an earlier run
  # passed the invariants but skipped the mechanism. Stretch the drain
  # interval, build the directory, then force the checkpoint that republishes
  # its manifest into the in-RAM pending tier (readdir drives one), so the
  # only INODE_WRITE for newdir that ever reaches the ring names that
  # unpacked manifest.
  JI=\$(sysctl -n kern.tessera.journal_log_interval_ms)
  sysctl -q kern.tessera.journal_log_interval_ms=8000; sleep 2
  mkdir $M/base/newdir
  for i in 1 2; do head -c 6000000 /dev/random > $M/base/newdir/c\$i; done
  for i in 3 4; do head -c 70000 /dev/random > $M/base/newdir/c\$i; done
  ln $M/base/newdir/c1 $M/base/hardlink_to_c1
  # Committed files touched by ops INTO the lost directory. Neither op can
  # commit, so both files must come back exactly as they were: the move
  # undone (not the file deleted), the link count restored.
  mv $M/base/committed_move $M/base/newdir/moved_in
  ln $M/base/committed_link $M/base/newdir/linked_in
  ls $M/base/newdir >/dev/null
  sysctl -q kern.tessera.journal_log_interval_ms=\$JI
  echo overwritten-after-commit > $M/base/old
  echo \"before_cut entries=\$(find $M/base | wc -l | tr -d ' ') journal_records=\$(sysctl -n kern.tessera.journal_log_records)\"
  sleep 45
  echo \"after_wait journal_records=\$(sysctl -n kern.tessera.journal_log_records) sb_commits=\$(sysctl -n kern.tessera.sb_commits)\"
" 2>&1 | tr -d '\r')
echo "$out"
case "$out" in *MKFS_FAIL*|*REFUSING*) echo "FAIL — harness could not run"; exit 1;; esac
echo "--- POWER CUT"
relaunch || { echo "FAIL — VM did not come back"; exit 1; }

res=$($VSSH "TRACE_REPLAY=${TRACE_REPLAY:-0}; $GATE
  # Record what replay actually re-creates, so a surprising result can be
  # explained from evidence instead of guessed at. Optional: skipped if
  # dtrace is unavailable.
  if [ \"\${TRACE_REPLAY:-0}\" = 1 ] && { kldstat | grep -q dtraceall || kldload dtraceall; }; then
    D=/root/rr.d; [ -f \$D ] || echo 'fbt:tessera_fs:tessera_fs_dirent_log_append:entry { printf(\"LOGAPPEND parent=%d op=%d ino=%d %s\\n\", arg1, arg2, arg5, substr(stringof((char *)arg3), 0, arg4)); }' > \$D
    dtrace -q -s \$D -o /root/rr.trace -c \"mount -t tessera $DEV $M\" || { echo MOUNT_FAIL; exit 4; }
    echo \"--- replay trace (base=inode \$(stat -f %i $M/base 2>/dev/null)):\"; sed 's/^/  /' /root/rr.trace | head -120
  else
    mount -t tessera $DEV $M 2>/dev/null || { echo MOUNT_FAIL; exit 4; }
  fi
  echo \"refused=\$(dmesg | grep -c 'journal replay REFUSED') dropped=\$(sysctl -n kern.tessera.journal_replay_dropped_dirents)\"
  dmesg | grep -E 'journal replay (REFUSED|dropped)' | tail -3 | sed 's/^/  /'
  echo \"entries=\$(find $M/base | sort | tr '\n' ' ')\"
  for f in \$(find $M/base -type f | sort); do echo \"  \$f size=\$(stat -f %z \$f)\"; done
  echo \"old=[\$(cat $M/base/old 2>&1)]\"
  # The committed files must be intact under EXACTLY ONE of the names the
  # interrupted ops could leave: the rename either replayed or was undone.
  n=0; for f in $M/base/committed_move $M/base/newdir/moved_in; do
    [ -e \$f ] && { n=\$((n+1)); [ \"\$(sha256 -q \$f)\" = \"\$(cat /root/rr.move.sum)\" ] || echo move_content=BAD; }; done
  echo \"move_names=\$n\"
  [ \"\$(sha256 -q $M/base/committed_link 2>/dev/null)\" = \"\$(cat /root/rr.link.sum)\" ] && echo link_content=yes || echo link_content=NO
  ln_names=1; [ -e $M/base/newdir/linked_in ] && ln_names=2
  echo \"link_nlink=\$(stat -f %l $M/base/committed_link 2>/dev/null) link_names=\$ln_names\"
  ls -laR $M/base >/dev/null 2>/root/rr.lsr; echo \"lsR_errors=\$(wc -l < /root/rr.lsr | tr -d ' ')\"
  dmesg | grep -o '[0-9]* move(s) into a lost directory undone' | tail -1 | sed 's/^/undone=/' 
  touch $M/base/new1 2>&1 && echo recreate_new1=ok
  ls $M/base >/dev/null 2>/root/rr.ls; echo \"ls_errors=\$(wc -l < /root/rr.ls | tr -d ' ')\"
  # ★ fsck ONLY on a successfully unmounted volume: a live-volume fsck fails
  # toward FALSE POSITIVES (measured 466 -> 2 -> 2 -> 2 mounted, CLEAN unmounted).
  umount $M || { echo \"fsck_problems=SKIPPED_MOUNTED\"; exit 0; }
  tessera-fsck $DEV > /root/rr.fsck 2>&1
  echo \"fsck_problems=\$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/rr.fsck)\"
  grep -E 'result:|^    - ' /root/rr.fsck | head -6 | sed 's/^/  /'
  echo \"root_dangling_lookups=\$(dmesg | grep -c 'damaged directory entry')\"
" 2>&1 | tr -d '\r')
echo "$res"
case "$res" in *MOUNT_FAIL*|*REFUSING*) echo "FAIL — recovery mount failed"; exit 1;; esac

RF=$(echo "$res" | sed -n 's/^refused=\([0-9]*\).*/\1/p')
DR=$(echo "$res" | sed -n 's/.*dropped=\([0-9]*\).*/\1/p')
FP=$(echo "$res" | sed -n 's/^fsck_problems=\([0-9]*\).*/\1/p')
LE=$(echo "$res" | sed -n 's/^ls_errors=\([0-9]*\).*/\1/p')
rc=0
# ★ DEAD-ARM GUARD. If replay refused nothing, the redo buffers never reached
# the disk before the cut (or everything got flushed) and this run tested
# nothing; a clean fsck would then be meaningless.
[ "${RF:-0}" -gt 0 ] 2>/dev/null || { echo "FAIL — replay refused nothing: the scenario did not reproduce"; rc=1; }
[ "${DR:-0}" -gt 0 ] 2>/dev/null || { echo "FAIL — no entries were dropped: the fix did not engage"; rc=1; }
[ "${FP:-1}" = 0 ] || { echo "FAIL — fsck found $FP problems"; rc=1; }
[ "${LE:-1}" = 0 ] || { echo "FAIL — listing base produced $LE errors (a wedged name survived)"; rc=1; }
echo "$res" | grep -q 'recreate_new1=ok' || { echo "FAIL — a lost name could not be recreated"; rc=1; }
echo "$res" | grep -q 'move_names=1' || { echo "FAIL — the committed file touched by an interrupted rename is not under exactly one name"; rc=1; }
echo "$res" | grep -q 'move_content=BAD' && { echo "FAIL — the renamed committed file's content changed"; rc=1; }
echo "$res" | grep -q 'link_content=yes' || { echo "FAIL — a committed file touched by an interrupted link was damaged"; rc=1; }
LN=$(echo "$res" | sed -n 's/.*link_nlink=\([0-9]*\) link_names=\([0-9]*\).*/\1 \2/p')
set -- $LN; [ "${1:-x}" = "${2:-y}" ] || { echo "FAIL — link count ${1:-?} does not match ${2:-?} surviving names"; rc=1; }
LR=$(echo "$res" | sed -n 's/^lsR_errors=\([0-9]*\).*/\1/p')
[ "${LR:-1}" = 0 ] || { echo "FAIL — a recursive listing hit $LR errors"; rc=1; }
[ $rc = 0 ] && echo "PASS — refused inodes took their names with them; fsck CLEAN"
exit $rc
