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

echo "########## 1/3 DELETE-HEAVY SOAK ($CYCLES1 cuts) ##########"
CYCLES=$CYCLES1 sh "$BSD/scripts/vm-crash-soak.sh" 2>&1 | tee "$OUT/delete.log" \
    | grep -E "SOAK|FSCK-DIRTY|FAILED|ABORT|stopping" || rc=1

echo "########## 2/3 CREATE-HEAVY SOAK ($CYCLES2 cuts) ##########"
CYCLES=$CYCLES2 sh "$BSD/scripts/vm-crash-soak-create.sh" 2>&1 | tee "$OUT/create.log" \
    | grep -E "CREATE-SOAK|FSCK-DIRTY|FAILED|ABORT|stopping" || rc=1

echo "########## 3/3 META-EXHAUSTION REGRESSION ##########"
# Stage from the repo rather than trusting whatever is already in the guest —
# same reason the phases gate on the kmod hash.
scp -i "$HOME/.ssh/fresco_bsd_ed25519" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222 \
    "$BSD/scripts/vm-meta-exhaustion-test.sh" root@localhost:/root/ >/dev/null \
    || { echo "could not stage the meta test"; rc=1; }
"$BSD/scripts/vssh" 'sh /root/vm-meta-exhaustion-test.sh' 2>&1 | tee "$OUT/meta.log" | tail -4

echo "########## SUITE DONE $(date) ##########"
echo "delete: $(grep -E 'SOAK DONE' "$OUT/delete.log" | tail -1)"
echo "create: $(grep -E 'CREATE-SOAK DONE' "$OUT/create.log" | tail -1)"
echo "meta:   $(grep -E 'PASS|FAIL' "$OUT/meta.log" | tail -1)"
grep -q PASS "$OUT/meta.log" || rc=1
exit $rc
