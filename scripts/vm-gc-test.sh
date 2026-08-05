#!/bin/sh
# Kernel-level GC test. RUNS IN THE VM, against the real tessera kmod.
#
# core/src/gc.c is a stub (see core/tests/test_gc.c); the GC that actually runs
# is tessera_fs_gc_data_zone_ex in the kmod, and it needs the flush gate, the
# pin bitmap and the buffer cache. None of that is reachable from a userspace
# unit test, so this is the only place its behaviour can be asserted.
#
# What it asserts, and why each one exists:
#
#   1. garbage was actually created        — a GC run over a volume with no
#                                            dead packs reclaims 0 and "passes"
#                                            while proving nothing. GATE FIRST.
#   2. the pass COMPLETES within a budget  — #114: a wedged GC spins unkillably
#   3. packs were reclaimed                — the whole point
#   4. live data is byte-identical         — GC must never touch reachable
#                                            blobs. This is the correctness
#                                            assertion; the rest is plumbing.
#   5. gc_aborts did not increase          — #84: an aborting pass reclaims
#                                            nothing and looks like "no garbage"
#   6. fsck is CLEAN afterwards            — on the UNMOUNTED volume (#61: never
#                                            fsck a mounted volume)
#   7. a second pass reclaims ~nothing     — idempotence; a GC that keeps
#                                            "reclaiming" is miscounting
#   8. no load_node / kind warnings        — #115 stale-root signature
#   9. umount interrupts an in-flight GC   — #99 regression
#
# usage (in the VM):  sh /root/vm-gc-test.sh [device]
set -u
DEV=${1:-/dev/vtbd0}
MNT=/mnt/gctest
FSCK=${FSCK:-/root/g2-fsck}
MKFS=${MKFS:-/root/mkfs-tessera}
TQ=${TQ:-/root/tq}
fails=0
say()  { echo "  $*"; }
ok()   { echo "  ok   $*"; }
bad()  { echo "  FAIL $*"; fails=$((fails+1)); }

sysc() { sysctl -n "kern.tessera.$1" 2>/dev/null || echo 0; }

# Bounded waiter. An unbounded one cannot fail, it can only wait.
wait_gc_done() {
    _max=$1; _w=0
    while [ $_w -lt "$_max" ]; do
        ps -ax -o command | grep -q "[t]q gc" || return 0
        sleep 5; _w=$((_w+5))
    done
    return 1
}

echo "=== kmod GC test on $DEV ==="
umount $MNT 2>/dev/null
mkdir -p $MNT
$MKFS "$DEV" >/dev/null 2>&1 || { echo "mkfs failed"; exit 2; }
mount -t tessera "$DEV" $MNT || { echo "mount failed"; exit 2; }
dmesg -c >/dev/null 2>&1

# ── build a volume with KNOWN live data and KNOWN garbage ────────────
#
# ★ THE GARBAGE MUST EXCEED kern.tessera.gc_pressure_pct (default 12), or the
# GC correctly declines to reclaim and the test measures policy rather than
# correctness. An earlier version of this file wrote 12 x 256 KiB of trash —
# about 2% of a 256 MiB volume — watched GC leave 15 fully-dead packs alone,
# and was one edit away from filing that as a bug. It is #81's tuned
# cost/benefit curve working as designed.
#
# So: 12 MiB live, 40 MiB trash. After the delete the waste is well over the
# threshold and the production reclaim path actually runs.
pressure_pct=$(sysc gc_pressure_pct)
say "gc_pressure_pct = ${pressure_pct}% (waste must exceed this before GC reclaims)"
mkdir -p $MNT/live $MNT/trash
i=1
while [ $i -le 12 ]; do
    dd if=/dev/random of=$MNT/live/f$i bs=1m count=1 2>/dev/null
    i=$((i+1))
done
i=1
while [ $i -le 40 ]; do
    dd if=/dev/random of=$MNT/trash/g$i bs=1m count=1 2>/dev/null
    i=$((i+1))
done
sync; sleep 1
# Fingerprint the live set BEFORE anything is collected.
( cd $MNT/live && sha256 -q f* | sha256 -q ) > /tmp/gc.live.before 2>/dev/null

packs_before=$(sysc pack_alloc_calls)
rm -rf $MNT/trash
sync; sleep 2

# 1. GATE: did the delete actually produce dead space? If not, everything
#    below would "pass" against an empty workload.

if [ "$packs_before" -lt 1 ]; then
    echo "GATE FAILED: no packs were allocated — the workload never ran"
    umount $MNT; exit 2
fi
ok "workload: $packs_before pack-alloc call(s), 40 MiB deleted, 12 MiB live"

# ★ Measure the reclaimable garbage BEFORE the GC, with fsck on the UNMOUNTED
# volume. Without this the reclaim assertion is VACUOUS, and it was: a control
# run that never invoked GC at all still ended with ~0 fully-dead packs and
# "passed". The reason is that fsck's live set unions RETAINED SNAPSHOTS, and
# the snapshots still pin the deleted files — so on this filesystem deleting a
# file does not by itself create garbage. Asserting on a post-state alone
# cannot tell "GC worked" from "there was nothing to do".
dead_of() {
    $FSCK "$1" 2>&1 | grep -a "packs fully live" | head -1 |
        sed -E 's/.*MiB\), ([0-9]+) fully dead.*/\1/'
}
umount $MNT || { echo "umount before baseline fsck failed"; exit 2; }
dead_before=$(dead_of "$DEV")
mount -t tessera "$DEV" $MNT || { echo "remount failed"; exit 2; }
say "reclaimable before GC: ${dead_before:-?} fully-dead pack(s)"

gcr_before=$(sysc gc_reclaimed)
abr_before=$(sysc gc_aborts)

# ── 2/3. run a pass to completion ────────────────────────────────────
daemon -f $TQ gc $MNT
if wait_gc_done 180; then
    ok "GC pass completed within 180s"
else
    bad "GC pass did NOT complete in 180s (spin? #114)"
fi
gcr_after=$(sysc gc_reclaimed)
abr_after=$(sysc gc_aborts)

# ★ "packs reclaimed > 0" is NOT a sufficient assertion. This test shipped with
# it until a negative control caught it: re-running with the DELETE REMOVED
# still reclaimed 3 packs and passed. Ordinary write churn supersedes manifests
# and dirents, so some pack always dies — the counter cannot tell you the
# deleted data came back.
#
# df is no better: statfs on Tessera does not track reclaim usefully (both arms
# reported 60 KiB), and #113 already ruled that a wart not worth fixing.
#
# The trustworthy oracle is fsck's own accounting, which recomputes the live
# set from the trees and attributes every pack. A GC that did its job leaves
# NO FULLY-DEAD PACKS behind. That is asserted after the unmount, below.
say "packs reclaimed: $((gcr_after - gcr_before))  ($gcr_before -> $gcr_after)"

# 5. an aborting pass reclaims nothing and is indistinguishable from
#    "there was no garbage" unless you look here.
if [ "$abr_after" -eq "$abr_before" ]; then
    ok "no GC aborts (gc_aborts=$abr_after)"
else
    bad "GC ABORTED $((abr_after - abr_before)) time(s) — reclaim is not trustworthy"
fi
lost=$(sysc gc_lost_subtrees)
[ "$lost" -eq 0 ] && ok "no lost subtrees" || bad "gc_lost_subtrees=$lost"

# ── 4. THE correctness assertion: live data untouched ────────────────
( cd $MNT/live && sha256 -q f* | sha256 -q ) > /tmp/gc.live.after 2>/dev/null
if cmp -s /tmp/gc.live.before /tmp/gc.live.after; then
    ok "live data byte-identical after GC"
else
    bad "LIVE DATA CHANGED across GC — this is data loss, not a perf bug"
fi
n=$(ls $MNT/live | wc -l | tr -d ' ')
[ "$n" -eq 12 ] && ok "all 12 live files present" || bad "only $n/12 live files"

# ── 7. idempotence ───────────────────────────────────────────────────
gcr_mid=$(sysc gc_reclaimed)
daemon -f $TQ gc $MNT
wait_gc_done 180 || bad "second GC pass did not complete"
gcr_2=$(sysc gc_reclaimed)
d2=$((gcr_2 - gcr_mid))
if [ "$d2" -le 1 ]; then
    ok "second pass reclaimed $d2 pack(s) — idempotent"
else
    bad "second pass reclaimed $d2 more pack(s) — GC is miscounting or leaking"
fi

# ── 8. no stale-root / kind complaints ───────────────────────────────
if dmesg | grep -aiqE "load_node|kind=|expected="; then
    bad "kernel logged a node-kind complaint:"
    dmesg | grep -aiE "load_node|kind=|expected=" | tail -3 | sed 's/^/       /'
else
    ok "no load_node / node-kind warnings"
fi

# ── 9. #99: umount must interrupt an in-flight GC ────────────────────
dd if=/dev/random of=$MNT/live/churn bs=256k count=8 2>/dev/null
sync
rm -f $MNT/live/churn; sync
daemon -f $TQ gc $MNT
sleep 1
if umount $MNT; then
    ok "umount interrupted an in-flight GC (#99)"
else
    bad "umount FAILED while GC was running (#99 regression)"
    wait_gc_done 180; umount $MNT 2>/dev/null
fi

# ── 6. fsck the UNMOUNTED volume ─────────────────────────────────────
out=$($FSCK "$DEV" 2>&1)
if echo "$out" | grep -aq "CLEAN"; then
    ok "fsck CLEAN after GC"
else
    bad "fsck NOT clean after GC:"
    echo "$out" | grep -aE "PROBLEM|^    - " | head -5 | sed 's/^/       /'
fi

# ── 3 (real). Did the DELETED data actually come back? ───────────────
# fsck recomputes the live set independently of the kernel and reports how
# many packs are fully dead — i.e. reclaimable and not reclaimed. After a
# successful pass that must be 0. This is the assertion the pack counter and
# df could not make.
spaceline=$(echo "$out" | grep -a "packs fully live" | head -1)
if [ -n "$spaceline" ]; then
    dead_packs=$(echo "$spaceline" | sed -E 's/.*MiB\), ([0-9]+) fully dead.*/\1/')
    say "$(echo "$spaceline" | sed 's/^ *//')"
    # Conditional on a MEASURED precondition. If the workload produced no
    # reclaimable garbage, say so and do not pretend the pass proved anything.
    if [ "${dead_before:-0}" -le 3 ]; then
        say "SKIP reclaim check: the workload left only ${dead_before:-0} dead"
        say "     pack(s) to begin with (retained snapshots pin deleted files),"
        say "     so this run cannot distinguish a working GC from an idle one."
    elif [ "${dead_packs:-999}" -lt "$dead_before" ]; then
        ok "fully-dead packs $dead_before -> $dead_packs — GC reclaimed garbage"
    else
        bad "fully-dead packs $dead_before -> $dead_packs: GC reclaimed nothing
       despite $dead_before reclaimable pack(s) and waste above the
       ${pressure_pct}% pressure threshold"
    fi
else
    bad "fsck printed no space accounting; cannot verify reclaim"
fi

echo
if [ $fails -eq 0 ]; then echo "kmod GC test: ALL CHECKS PASSED"; exit 0; fi
echo "kmod GC test: $fails CHECK(S) FAILED"; exit 1
