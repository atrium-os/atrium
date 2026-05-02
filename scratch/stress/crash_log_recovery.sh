#!/bin/sh
# v2.6 Phase B.2 crash-recovery test, run in two phases via the
# host wrapper.
#
# Storage: a dedicated raw virtio-blk drive (/dev/vtbd0, host file
# ~/src/bsd/vm/crash-test.img, attached with cache=directsync). 9p
# is NOT usable here — its VFS layer collapses repeated writes to
# the same offset, which silently drops most journal-header updates
# even though bwrite() returns success.
#
# PHASE=1 (default): mkfs the device, mount it, create N files (NO
# fsync, NO umount), wait one group-commit interval, then exit. The
# host wrapper then issues a qemu HMP system_reset to simulate a
# power cut.
#
# PHASE=2: re-mount the same device. Mount-time replay reads the
# DIR_INSERT records out of the journal, re-creates the in-memory
# log, then the first post-mount flush applies them to BTREE.
# Count visible files; should match the phase-1 N.
set -u
PHASE=${PHASE:-1}
N=50
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
DEV=/dev/vtbd0

mkdir -p /mnt/tessera 2>/dev/null
umount /mnt/tessera 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

if [ "$PHASE" = "1" ]; then
    $BIN/mkfs-tessera --create -s 256 --seed-file h --seed-content x \
        $DEV >/dev/null
    mount -t tessera $DEV /mnt/tessera

    sysctl kern.tessera.journal_log_interval_ms=20 >/dev/null

    cd /mnt/tessera
    echo "creating $N files..."
    i=0
    while [ $i -lt $N ]; do
        : > f$i
        i=$((i + 1))
    done
    # Wait for the group-commit callout to drain pending records.
    # Don't sync, don't umount — that's the whole point.
    sleep 1
    echo "PHASE1_DONE files=$N dev=$DEV"
    sysctl kern.tessera.journal_log_records
    sysctl kern.tessera.journal_head
    sysctl kern.tessera.journal_tail
    sysctl kern.tessera.sb_commits
    cd /
elif [ "$PHASE" = "2" ]; then
    mount -t tessera $DEV /mnt/tessera
    cd /mnt/tessera
    sync
    sleep 1
    after=$(ls f* 2>/dev/null | wc -l | tr -d ' ')
    echo "PHASE2_DONE files=$after expected=$N"
    sysctl kern.tessera.journal_log_replays
    cd /
    umount /mnt/tessera
    if [ "$after" = "$N" ]; then
        echo "OK — all $N files survived crash + recovery"
    else
        echo "FAIL — expected $N, got $after"
        exit 1
    fi
fi
