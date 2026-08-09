#!/bin/sh
# #137 regression test — journal REDO replay must be generation-guarded.
#
# WHY THIS EXISTS. An unguarded redo replay destroyed the dev root: a stale
# TESSERA_INODE_WRITE for inode 2 was re-applied at mount and restored a
# manifest_hash whose blob had since been moved, leaving the root directory
# dangling and all 215304 other inodes orphaned. The checkpoint path
# (ROOT_UPDATE) had always been guarded; the redo path never was.
#
# TWO PROPERTIES, and the second is the one that bites if you get the
# comparison wrong:
#
#   A. STALE DROPPED  — records older than the committed superblock must not
#                       be applied.
#   B. RECOVERY KEPT  — records at the committed generation MUST still be
#                       applied, or every crash loses the un-checkpointed tail.
#
# The guard is `record.generation < sb.generation`, STRICTLY less than. A redo
# record is stamped with the generation it was logged ON TOP OF, so records
# needed after a crash carry exactly the committed generation; `<=` passes A
# and silently fails B. (tessera_jrec_sb_commit is the opposite — its
# generation is the one it commits TO, so ROOT_UPDATE's `<=` is correct.)
#
# usage, on a VM with a SCRATCH disk (never the root — see #129):
#
#   phase 1a:  sh vm-journal-guard-test.sh crash-setup /dev/vtbd1
#              then HARD-RESET the VM (qemu monitor `system_reset`)
#   phase 1b:  sh vm-journal-guard-test.sh crash-check /dev/vtbd1
#              expect pending_after == 60   (property B)
#
#   phase 2a:  sh vm-journal-guard-test.sh stale-setup /dev/vtbd1
#              then HARD-RESET, and on the HOST bump the volume's superblock
#              generation in BOTH slots (offsets: generation @24, crc32 @4060,
#              CRC32 over bytes 0..4060) WITHOUT touching the journal — this
#              is what an offline fsck/repack used to leave behind.
#   phase 2b:  sh vm-journal-guard-test.sh stale-check /dev/vtbd1
#              expect redo_stale > 0, redo_applied == 0, keep_after == 60
#
# Verified 2026-08-09: property B pending_after=40/40; property A
# redo_stale=2, redo_applied=0, keep_after=60, root directory intact.
set -u
S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
MODE="${1:?mode}"; DEV="${2:?device}"; MNT=/mnt/jguard
mkdir -p $MNT

populate() {   # $1 = dir name, $2 = count
    mkdir -p $MNT/$1
    i=0; while [ $i -lt $2 ]; do echo "$1-$i" > $MNT/$1/f$i; i=$((i+1)); done
}

case "$MODE" in
crash-setup|stale-setup)
    umount $MNT 2>/dev/null
    /root/mkfs-tessera $DEV >/dev/null 2>&1 || { echo "mkfs FAILED"; exit 2; }
    mount -t tessera $DEV $MNT || { echo "mount FAILED"; exit 2; }
    populate keep 60
    sync; sleep 2; sync
    echo "keep_count=$(find $MNT/keep -type f | wc -l | tr -d ' ')"
    if [ "$MODE" = crash-setup ]; then
        populate pending 40        # logged, deliberately never synced
        echo "pending_count=$(find $MNT/pending -type f | wc -l | tr -d ' ')"
    fi
    # NO umount: a clean unmount trims the ring and the test proves nothing.
    echo "READY — hard-reset the VM now"
    ;;
crash-check|stale-check)
    s0=$(S journal_redo_stale); r0=$(S journal_redo_refused); a0=$(S journal_log_replays)
    mount -t tessera $DEV $MNT || { echo "mount FAILED"; exit 2; }
    echo "keep_after=$(find $MNT/keep -type f 2>/dev/null | wc -l | tr -d ' ')"
    [ "$MODE" = crash-check ] && \
      echo "pending_after=$(find $MNT/pending -type f 2>/dev/null | wc -l | tr -d ' ')"
    echo "redo_stale=$(( $(S journal_redo_stale)   - s0 ))"
    echo "redo_refused=$(( $(S journal_redo_refused) - r0 ))"
    echo "redo_applied=$(( $(S journal_log_replays)  - a0 ))"
    echo "root_entries=$(ls $MNT | tr '\n' ' ')"
    umount $MNT 2>/dev/null
    ;;
*) echo "unknown mode: $MODE"; exit 2 ;;
esac
