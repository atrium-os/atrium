#!/bin/sh
# Crash-recovery across the ro->rw boot path (task: wire up crash recovery on
# the ro->rw transition). Uses the dedicated directsync crash drive /dev/vtbd0
# (host vm/crash-test.img). Driven in two phases by a host wrapper that issues
# a qemu system_reset between them to simulate a power cut.
#
# PHASE=1: mkfs, mount rw, write file A (fully committed, SB updated), then arm
#   kern.tessera.skip_next_sb=1 and write file B + sync so B's transaction is
#   flushed to the btree and a ROOT_UPDATE is journaled + checkpointed but the
#   SB sector write is SKIPPED. On-disk SB still points at A's generation; the
#   journal holds the ROOT_UPDATE that rolls forward to A+B. No umount.
#
# PHASE=2: mount READ-ONLY first (mirrors vfs_mountroot). The ro mount's
#   roots-only replay must roll the in-core SB to the ROOT_UPDATE so BOTH A and
#   B are visible read-only, with no write/panic. Then `mount -u -o rw` upgrades,
#   applies any deferred redo, persists the recovered SB, and flushes. Verify
#   A+B still present + writable, then fsck.
set -u
PHASE=${PHASE:-1}
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
DEV=/dev/vtbd0
MNT=/mnt/tessera

mkdir -p $MNT 2>/dev/null
umount $MNT 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

if [ "$PHASE" = "1" ]; then
    $BIN/mkfs-tessera --create -s 256 $DEV >/dev/null
    mount -t tessera $DEV $MNT
    echo "file-A-content" > $MNT/A
    sync                                   # A fully committed, SB written
    sysctl kern.tessera.skip_next_sb=1 >/dev/null
    echo "file-B-content" > $MNT/B
    sync                                   # B: ROOT_UPDATE journaled, SB write SKIPPED
    echo "PHASE1_DONE (A+B written, B's SB write skipped, no umount)"
    ls -la $MNT
    # NO umount — the host wrapper resets the VM now.
elif [ "$PHASE" = "2" ]; then
    echo "=== mount READ-ONLY (crash recovery must show A AND B) ==="
    mount -t tessera -o ro $DEV $MNT
    mount | grep "$MNT "
    echo "-- ro listing:"; ls -la $MNT
    echo "-- A: $(cat $MNT/A 2>&1)"
    echo "-- B: $(cat $MNT/B 2>&1)"
    ro_a=$(cat $MNT/A 2>/dev/null); ro_b=$(cat $MNT/B 2>/dev/null)
    echo "=== upgrade ro -> rw ==="
    mount -u -o rw $MNT
    mount | grep "$MNT "
    echo "-- write after upgrade:"; echo postrw > $MNT/C && echo "  write OK: $(cat $MNT/C)"
    sync
    echo "-- rw listing:"; ls -la $MNT
    umount $MNT
    echo "=== fsck ==="
    $BIN/tessera-fsck $DEV 2>&1 | tail -4
    if [ "$ro_a" = "file-A-content" ] && [ "$ro_b" = "file-B-content" ]; then
        echo "RESULT: OK — A and B both recovered + visible in the READ-ONLY mount"
    else
        echo "RESULT: FAIL — ro mount did not show recovered state (A='$ro_a' B='$ro_b')"
    fi
fi
