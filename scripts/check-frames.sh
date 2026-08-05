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
# Current 5. pack_alloc_and_write went too (its `prev` pel, tessera_fs.c:16982).
# The repack-path pel locals took repack_one_pack and pack_alloc_rollback.
# write_journal_header and pack_open joined pack_finalize —
# their 4096 B on-disk structs are heap now too.
# All THREE ~8.4 KiB frames are gone — dead_extent_drain,
# meta_pending_drain and kbio_meta_alloc (the last via its inlinee
# meta_epoch_sweep). Worst remaining is pack_finalize 8336.
# Former entries, kept for context:
# kbio_meta_alloc 8384, repack_one_pack 4576, pack_alloc_and_write 4432,
# pack_alloc_rollback 4288, decode_pack_extent_list 4160,
# gc_data_zone_ex 2880, blake3_256 2000, blake3_init_derive_key 1984,
# vop_readdir 1792.
BASELINE=5

# ★ A GATE THAT CANNOT FAIL IS NOT A GATE. Two ways this one could not:
#
# 1. `n=$(grep -c ... || echo 0)` — grep -c PRINTS "0" and EXITS 1 when there
#    are no matches, so the fallback appended a second line. `n` became "0\n0",
#    the numeric comparison errored, and the script fell through to exit 0.
#    Every clean-of-warnings build "passed" for the wrong reason.
# 2. An INCREMENTAL build only recompiles what changed, so the log carries a
#    handful of warnings or none. Zero matches then reads as "no oversized
#    frames" when it means "did not look". The BASELINE comment already warned
#    that partial builds under-count; nothing enforced it.
# Anchor on the -c flag, NOT on "clang .* <file>": bmake echoes one enormous
# command that the log wraps across physical lines, so the compiler name and
# the source name are usually not on the same line and `.*` cannot bridge them.
# grep -a because these logs carry non-text bytes.
#
# Both a kmod TU and a core TU must appear — the baseline counts frames across
# both, so a build that recompiled only one of them still under-counts.
for tu in tessera_fs.c btree.c; do
    # Core TUs are compiled by absolute path (-c .../core/src/btree.c), the
    # kmod TU by bare name, so match the suffix rather than the whole token.
    grep -aq -- "-c [^ ]*$tu" "$log" 2>/dev/null || {
        echo "REFUSING: $log has no '-c $tu' line — this is a partial or"
        echo "incremental build log, and its warning count means nothing."
        echo "Re-run after: rm -f *.o *.ko  (a FULL build), then re-check."
        exit 2
    }
done
n=$(grep -c "stack frame size" "$log" 2>/dev/null) || n=0
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
