#!/bin/sh
# Regression: does `deferred` dedup still close the §20.1 existence oracle?
#
# WHY THIS IS A STANDING TEST AND NOT A ONE-OFF
#
#   tessera-fs.md §20.1 is a SECURITY property, and it is the kind that decays
#   silently: any future optimisation that short-circuits a publish on a
#   registry hit re-opens the oracle, and nothing else in the suite would
#   notice. §20.2 states the rule the write path must keep — a deferred domain
#   MUST NOT skip the append on a hit, MUST NOT skip journal/pack I/O, and
#   SHOULD avoid any hit-dependent branch with measurable latency before fsync.
#
#   The oracle is observable as a FREE-SPACE delta (channel 1, the noise-free
#   one): write a 4 MiB duplicate and a 4 MiB novel file and compare cost.
#
#       GLOBAL    duplicate  20 K  vs novel 4156 K   -> 207x, oracle OPEN
#       DEFERRED  duplicate 4156 K vs novel 4156 K   ->   1x, content-independent
#
#   Both arms run, because the GLOBAL arm is the positive control: if it stops
#   showing a large ratio, the test has stopped measuring anything and a
#   DEFERRED pass would be meaningless.
#
#   It also checks the dedup is PRESERVED, not lost — the duplicate extents
#   must be reclaimed by drain + GC and the bytes must still read back
#   identical. A "closed oracle" that silently doubles storage forever is not
#   the feature.
#
#   sh scripts/vm-dedup-oracle-test.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$BSD/scripts/vssh"
KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
[ -f "$KO" ] || { echo "no $KO — build it first"; exit 1; }
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
k=$($VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != tree [$KMOD]"; exit 1; }

echo "=== DEDUP EXISTENCE-ORACLE TEST $(date) kmod=$KMOD ==="
# ★ Stage the guest half every run rather than trusting whatever is in /root.
# A test that silently runs a stale copy is worse than one that fails loudly.
scp -i "$HOME/.ssh/fresco_bsd_ed25519" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222 \
    "$BSD/scripts/guest-dedup-oracle.sh" root@localhost:/root/dedup-oracle.sh \
    >/dev/null || { echo "ABORT: could not stage the guest script"; exit 1; }
# tdedup(1) is built from source in-guest; it is the only way to set a policy.
$VSSH '[ -x /root/tdedup ] || cc -o /root/tdedup /root/tdedup.c 2>/dev/null; [ -x /root/tdedup ] || echo NO_TDEDUP' 2>/dev/null | grep -q NO_TDEDUP && {
    echo "ABORT: /root/tdedup missing (build it from scripts/guest-tdedup.c)"; exit 1; }
OUT=$($VSSH 'sh /root/dedup-oracle.sh' 2>&1 | tr -d '\r')
echo "$OUT" | sed 's/^/  /'

rc=0
echo "$OUT" | grep -q "ARM 0" || { echo "FAIL — harness did not run"; exit 1; }
# positive control: GLOBAL must still show the oracle, or we measure nothing
echo "$OUT" | awk '/ARM 0/,/ARM 1/' | grep -q "ORACLE OPEN" || {
    echo "FAIL — GLOBAL arm no longer shows the oracle: the TEST is broken, not the fix"; rc=1; }
# the property under test
echo "$OUT" | awk '/ARM 1/,0' | grep -q "oracle closed" || {
    echo "FAIL — DEFERRED did not close the oracle (§20.1 channel 1 re-opened)"; rc=1; }
echo "$OUT" | grep -q "DUPLICATE CONTENT CORRUPTED" && { echo "FAIL — duplicate bytes differ"; rc=1; }
echo "$OUT" | grep -q "fsck_problems=0" || { echo "FAIL — fsck problems"; rc=1; }
# dedup must be PRESERVED: the duplicate's space has to come back
REC=$(echo "$OUT" | sed -n 's/.*recovered=\(-*[0-9]*\)K.*/\1/p')
[ "${REC:-0}" -gt 1000 ] 2>/dev/null || { echo "FAIL — only ${REC:-?}K reclaimed after drain+GC; deferred is doubling storage"; rc=1; }

[ $rc = 0 ] && echo "PASS — deferred closes the oracle AND the space is reclaimed (${REC}K)"
exit $rc
