#!/bin/sh
# #134 / #108: does the GC walk's visited set actually cut pass cost?
#
# MEASURED 2026-08-09 on /dev/vtbd2 (4 arms, identical construction per arm):
#
#   shape   visited  files  fetches  disk_ops  skips
#   unique     0      480      527      1518      0
#   unique     1      480      527      1515      0     <- control: unchanged
#   shared     0      480      527        78      0
#   shared     1      480       48        78    479     <- 11x fewer fetches
#
# 527 - 48 == 479, so the skips account for the drop exactly.
#
# ★ AND THE LIMIT, which the same table shows: disk_ops is 78 in BOTH shared
# arms. On this volume the repeated manifests sit in one already-cached pack,
# so the redundant fetches were served from memory — the saving is CPU/parse,
# NOT I/O. A visited set cannot reduce the number of UNIQUE blobs, and unique
# blobs at ~2 serialized disk ops each are what makes a real pass take 847 s.
# Do not cite this as an I/O win.
#
# The walk used to fetch every hash it popped with no record of what it had
# already walked, so a manifest reachable N times was fetched and re-descended
# N times. The fix added a visited set. It shipped defaulted ON and was never
# measured — this measures it.
#
# CONTROL is the point: on unique content there is nothing to skip, so the
# visited set must change nothing. If "shared" improves and "unique" also
# moves, the effect is not what the label says.
set -u
S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
DEV=/dev/vtbd2; MNT=/mnt/g134; O=/root/gc134.out; : > $O
mkdir -p $MNT

build_unique() {
    d=0; while [ $d -lt 8 ]; do mkdir -p $MNT/u$d
        i=0; while [ $i -lt 60 ]; do
            dd if=/dev/random of=$MNT/u$d/f$i bs=8k count=1 2>/dev/null; i=$((i+1)); done
        d=$((d+1)); done
}
build_shared() {
    # one payload, copied many times: identical content dedups to identical
    # manifests, so the same manifest hash is reachable from many inodes —
    # exactly the shape the visited set is supposed to collapse.
    dd if=/dev/random of=/tmp/payload bs=8k count=1 2>/dev/null
    d=0; while [ $d -lt 8 ]; do mkdir -p $MNT/s$d
        i=0; while [ $i -lt 60 ]; do
            cp /tmp/payload $MNT/s$d/f$i; i=$((i+1)); done
        d=$((d+1)); done
}

run_arm() {           # $1 = shape, $2 = visited on/off
    shape="$1"; vis="$2"
    umount $MNT 2>/dev/null
    /root/mkfs-tessera $DEV >/dev/null 2>&1 || { echo "mkfs FAILED" >> $O; return; }
    mount -t tessera $DEV $MNT || { echo "mount FAILED" >> $O; return; }
    build_$shape
    sync; sleep 2; sync
    nf=$(find $MNT -type f | wc -l | tr -d ' ')
    umount $MNT; sysctl kern.tessera.gc_walk_visited=$vis >/dev/null
    mount -t tessera $DEV $MNT       # cold: the pass pays real reads

    f0=$(S blob_fetches); o0=$(S disk_rd_ops); k0=$(S gc_walk_dedup_skips)
    /root/tq $MNT >/dev/null 2>&1
    df=$(( $(S blob_fetches) - f0 )); do_=$(( $(S disk_rd_ops) - o0 ))
    dk=$(( $(S gc_walk_dedup_skips) - k0 ))
    echo "$shape visited=$vis files=$nf fetches=$df disk_ops=$do_ skips=$dk" >> $O
    umount $MNT 2>/dev/null
}

run_arm unique 0
run_arm unique 1
run_arm shared 0
run_arm shared 1
sysctl kern.tessera.gc_walk_visited=1 >/dev/null
echo DONE >> $O
