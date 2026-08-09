#!/bin/sh
# #108: does caching SMALL packs cut the walk's read amplification?
#
# MEASURED 2026-08-09, identical volume construction per arm, cold mount:
#
#   arm              fetches  disk_ops  ops/fetch  MiB  pack_hits  pack_miss
#   OLD(bulk-only)     541      1527      2.82      5       0          0
#   NEW(all packs)     537        64      0.11      4     473         16
#
#   24x fewer read operations, and BYTES went DOWN too (5 -> 4 MiB), so
#   caching whole small packs costs no read amplification. pack_hits going
#   0 -> 473 is the mechanism counter: in the OLD arm the cache was never
#   even consulted (hits AND misses both zero), which is what made this
#   invisible for so long.
#
# ★ AND THEN MEASURED ON THE REAL ROOT (128k packs, 14 GB, cold boot each
# arm, same tree both times — `tar cf /dev/null /usr/src/sys`):
#
#   arm              wall  fetches   disk_ops   MiB   pack_hits  pack_miss
#   OLD(bulk-only)    14s   151263    125490    678       0          0
#   NEW(all packs)    13s    52112     61200    677   21984       3588
#
#   2.05x fewer read operations for IDENTICAL bytes, and an 85% pack-cache
#   hit rate at 128k packs. NOT the 24x the 16-pack scratch volume showed —
#   that fit entirely in the cache and the real root does not. Quote 2x.
#
#   Wall time moved only 14s -> 13s because this VM is SSD-backed, where op
#   COUNT is nearly free; the saving shows up on slower media or a deep queue.
#   Do not sell this as a wall-clock win on this rig.
#
#   Unexplained and worth a look: the fetch COUNT also fell 151263 -> 52112.
#   Same files, same bytes, so something upstream is re-entering the fetch
#   path fewer times when the pack is cached. Not investigated.
#
# Run on the SCRATCH disk only (vtbd1 under the test harness, vtbd2 under
# run-vm.sh — check the ident, see #129).
set -u
S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
DEV=/dev/vtbd1; MNT=/mnt/g108; O=/root/gc108.out; : > $O
mkdir -p $MNT
arm() {
    minsec="$1"; label="$2"
    umount $MNT 2>/dev/null
    /root/mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV $MNT || { echo FAIL >> $O; return; }
    d=0; while [ $d -lt 8 ]; do mkdir -p $MNT/u$d
      i=0; while [ $i -lt 60 ]; do dd if=/dev/random of=$MNT/u$d/f$i bs=8k count=1 2>/dev/null; i=$((i+1)); done
      d=$((d+1)); done
    sync; sleep 2; sync
    umount $MNT
    sysctl kern.tessera.pack_cache_min_sectors=$minsec >/dev/null
    mount -t tessera $DEV $MNT                      # cold
    f0=$(S blob_fetches); o0=$(S disk_rd_ops); b0=$(S disk_rd_bytes)
    ph0=$(S cas_pack_hits); pm0=$(S cas_pack_misses)
    /root/tq $MNT >/dev/null 2>&1
    df=$(( $(S blob_fetches) - f0 )); do_=$(( $(S disk_rd_ops) - o0 )); db=$(( $(S disk_rd_bytes) - b0 ))
    echo "$label min_sectors=$minsec fetches=$df ops=$do_ ops_per_fetch=$(( df>0 ? do_*100/df : 0 ))/100 MiB=$(( db/1048576 )) pack_hits=$(( $(S cas_pack_hits) - ph0 )) pack_miss=$(( $(S cas_pack_misses) - pm0 ))" >> $O
    umount $MNT 2>/dev/null
}
arm 999999 "OLD(bulk-only)"
arm 1      "NEW(all packs)"
sysctl kern.tessera.pack_cache_min_sectors=1 >/dev/null
echo DONE >> $O
