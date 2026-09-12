#!/bin/sh
# Full Tessera crash/exhaustion regression suite. Runs the three harnesses in
# SEQUENCE — they all drive the same VM, so they must never overlap.
#
#   1. vm-crash-soak.sh          delete-heavy power cuts (unlink atomicity,
#                                reader epoch, journal replay)
#   2. vm-crash-soak-create.sh   create-heavy power cuts (create/mkdir/symlink/
#                                link/rename atomicity)
#   3. vm-meta-exhaustion-test.sh  metadata exhaustion degrades to ENOSPC
#
# Each phase gates on the guest running the module built from THIS tree, so a
# stale kmod cannot produce a meaningless green run. Logs land in $OUT.
#
# NOT covered here: the fsck --repair paths (c56c21a5). A clean soak by design
# produces no damage for them to repair; they need purpose-built damage
# fixtures, so verify those separately rather than assuming this suite covers
# them.
#
#   sh scripts/vm-soak-suite.sh                 # defaults: 50 + 30 cuts
#   CYCLES1=10 CYCLES2=5 sh scripts/vm-soak-suite.sh    # quick pass
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
OUT=${OUT:-/tmp/tessera-soak}
CYCLES1=${CYCLES1:-50}; CYCLES2=${CYCLES2:-30}
mkdir -p "$OUT"

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
[ -f "$KO" ] || { echo "no $KO — build it first (bootstrap-atrium.sh --only kmod)"; exit 1; }
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
export KMOD

rc=0
echo "########## SUITE START $(date) kmod=$KMOD ##########"

# ★ Ride along with the stall probe. The rare multi-second stall that fails
# ssh's banner exchange has never been reproduced on demand
# (project_tessera_guest_spin_on_damaged_volume), so the only realistic way to
# catch it is to have the instrument already running during hours of soak.
# It uses its OWN ssh with a LONG ConnectTimeout — scripts/vssh's
# ConnectTimeout=3 turns every stall into an indistinguishable "failure".
STALL_OUT="$OUT/stall-probe"
if [ "${STALL_PROBE:-1}" = "1" ]; then
    OUT="$STALL_OUT" INTERVAL=${STALL_INTERVAL:-10} sh "$BSD/scripts/vm-stall-probe.sh" \
        >"$OUT/stall-probe.log" 2>&1 &
    STALL_PID=$!
    echo "stall probe: armed (pid $STALL_PID) -> $STALL_OUT"
fi

echo "########## 1/5 DELETE-HEAVY SOAK ($CYCLES1 cuts) ##########"
CYCLES=$CYCLES1 sh "$BSD/scripts/vm-crash-soak.sh" 2>&1 | tee "$OUT/delete.log" \
    | grep -E "SOAK|FSCK-DIRTY|FAILED|ABORT|stopping" || rc=1

echo "########## 2/5 CREATE-HEAVY SOAK ($CYCLES2 cuts) ##########"
CYCLES=$CYCLES2 sh "$BSD/scripts/vm-crash-soak-create.sh" 2>&1 | tee "$OUT/create.log" \
    | grep -E "CREATE-SOAK|FSCK-DIRTY|FAILED|ABORT|stopping" || rc=1

echo "########## 3/5 META-EXHAUSTION REGRESSION ##########"
# Stage from the repo rather than trusting whatever is already in the guest —
# same reason the phases gate on the kmod hash.
scp -i "$HOME/.ssh/fresco_bsd_ed25519" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222 \
    "$BSD/scripts/vm-meta-exhaustion-test.sh" root@localhost:/root/ >/dev/null \
    || { echo "could not stage the meta test"; rc=1; }
"$BSD/scripts/vssh" 'sh /root/vm-meta-exhaustion-test.sh' 2>&1 | tee "$OUT/meta.log" | tail -4

echo "########## 4/5 NAMESPACE-CHURN REGRESSION ##########"
# Covers the five defects fixed 2026-09-12 (rmdir stale vnode, lifecycle
# asymmetry, and the GC-collected format-time constants). Committed earlier but
# never wired into the suite, which is precisely how a regression test stops
# being run.
CHURN_SECS=${CHURN_SECS:-300} SECS=${CHURN_SECS:-300} sh "$BSD/scripts/vm-namespace-churn-test.sh" \
    2>&1 | tee "$OUT/churn.log" | grep -E "^broken_dirs|^fsck_problems|^PASS|^FAIL|WARNING|^NOTE" || rc=1
grep -q "^PASS" "$OUT/churn.log" || rc=1

echo "########## 5/5 WORKLOAD SHAPES ##########"
# rename / mixed / multimount — the coverage gap that produced four of those
# five defects. See the header of vm-workload-shapes.sh.
SHAPE=all SECS=${SHAPES_SECS:-300} sh "$BSD/scripts/vm-workload-shapes.sh" \
    2>&1 | tee "$OUT/shapes.log" | grep -E "^=== SHAPE|iters=|mounts=|fsck=|cleanup=|PASS|FAIL|WARNING" || rc=1
grep -q "FAIL" "$OUT/shapes.log" && rc=1

if [ "${STALL_PROBE:-1}" = "1" ]; then
    touch /tmp/stall-probe.stop; sleep 8
    echo "########## STALL PROBE ##########"
    echo "  host ssh stalls >1s : $(wc -l < "$STALL_OUT/ssh-stalls.log" 2>/dev/null | tr -d " ")"
    echo "  guest exec stalls   : $(grep -c "SLOW exec" "$STALL_OUT/guest-stalls.log" 2>/dev/null || echo 0)"
    # The wchan histogram under a SLOW exec is the whole point — it names the
    # blocking resource. Print it if anything landed.
    grep -A6 "SLOW exec" "$STALL_OUT/guest-stalls.log" 2>/dev/null | head -30
    rm -f /tmp/stall-probe.stop
fi

echo "########## SUITE DONE $(date) ##########"
echo "delete: $(grep -E 'SOAK DONE' "$OUT/delete.log" | tail -1)"
echo "create: $(grep -E 'CREATE-SOAK DONE' "$OUT/create.log" | tail -1)"
echo "meta:   $(grep -E 'PASS|FAIL' "$OUT/meta.log" | tail -1)"
echo "churn:  $(grep -E '^PASS|^FAIL' "$OUT/churn.log" | tail -1)"
echo "shapes: $(grep -cE 'PASS \[' "$OUT/shapes.log") of 3 shapes passed"
grep -q PASS "$OUT/meta.log" || rc=1
exit $rc
