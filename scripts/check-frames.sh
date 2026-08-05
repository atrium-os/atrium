#!/bin/sh
# Ratchet for oversized kernel stack frames (#114).
#
# FreeBSD kstack is 4 pages (16 KiB). Three stack objects on the deep GC read
# path overflowed it, and an arm64 overflow faults in a loop rather than
# panicking — 100% CPU, no output, unkillable. This keeps the count from
# growing back.
#
# Usage: check-frames.sh <build-log>   (a kmod build with -Wframe-larger-than)
# Exit 0 if the count is at or below the baseline, 1 if it grew.
set -u
log=${1:?usage: check-frames.sh <build-log>}

# Baseline as of the #114 fixes — the ACTUAL measured count across the whole
# kmod from a FULL CLEAN build. Two earlier attempts understated it: 6 (the
# tessera_fs.c-only figure) and 11 (a partial build that did not recompile all
# of core/*.c). The gate caught both on its own first run. ★ Always set this
# from a clean build — a partial one silently under-counts. LOWER THIS as frames are fixed; never raise
# it without a recorded reason.
#
# Current 11: dead_extent_drain 8400 (moved OUT of gc_data_zone_ex by
# __noinline — fine standalone, it is a leaf), meta_pending_drain 8400,
# kbio_meta_alloc 8384, repack_one_pack 4576, pack_alloc_and_write 4432,
# pack_alloc_rollback 4288, decode_pack_extent_list 4160,
# gc_data_zone_ex 2880, blake3_256 2000, blake3_init_derive_key 1984,
# vop_readdir 1792.
BASELINE=14

n=$(grep -c "stack frame size" "$log" 2>/dev/null || echo 0)
echo "oversized frames: $n (baseline $BASELINE)"
grep -a "stack frame size" "$log" 2>/dev/null | sed -E 's/.*warning: //' | sort -u | sed 's/^/  /'

if [ "$n" -gt "$BASELINE" ]; then
    echo "FAIL: oversized-frame count grew ($n > $BASELINE)."
    echo "A new large stack object on a deep path can wedge the kernel unkillably."
    echo "Heap-allocate it, or __noinline the callee that carries it."
    exit 1
fi
[ "$n" -lt "$BASELINE" ] && echo "NOTE: count improved — lower BASELINE to $n."
exit 0
