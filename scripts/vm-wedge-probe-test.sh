#!/bin/sh
# Drive scripts/guest-wedge-probe.sh on the dev VM and print its verdict.
#
# This is the harness that found, in order: nothing retried a failed flush
# (fixed 84586b60), GC freeing packs whose registry delete had failed (same
# commit), the ENOENT over-correction that stopped reclaim (b1c95154), and the
# pinscan releasing live snapshot roots (7a62fea2).
#
# It does NOT pass/fail: the metadata-exhaustion wedge itself is still OPEN
# (rm reports success and frees nothing on a full volume), so this reports
# state for a human to read. Counters worth reading with it:
#   pinscan_late_snaps / pinscan_late_snap_fail   snapshot pinning
#   epoch_snap_birth_kept                          epoch-sweep birth pin
#   flush_retry_armed, preflight_scans             flush progress
#   gc_reclaimed / gc_delete_absent / gc_entry_moved
#
#   sh scripts/vm-wedge-probe-test.sh     # STARVE=1 PART_MB=128 WAIT_S=600
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$BSD/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHO="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
STARVE=${STARVE:-1}; PART_MB=${PART_MB:-128}; NDEL=${NDEL:-2000}; WAIT_S=${WAIT_S:-600}

timeout 15 $VSSH 'echo ready' 2>/dev/null | grep -q ready || { echo "FAIL — VM not reachable"; exit 1; }
# A repro that shares the scratch disk with another run measures nothing.
timeout 20 $VSSH "pgrep -q -f 'guest-wedge-probe|rxworker' && echo BUSY || echo FREE" 2>/dev/null \
    | tr -d '\r' | grep -q FREE || { echo "FAIL — a probe or its workers are already running in the guest"; exit 1; }
for f in guest-wedge-probe.sh guest-reserve-exhaustion.sh; do
    timeout 60 scp $SSHO -P 2222 "$BSD/scripts/$f" root@localhost:/root/$f >/dev/null \
        || { echo "FAIL — could not copy $f"; exit 1; }
done
. "$BSD/scripts/lib/guest-ident.sh"   # GUEST_KMOD_HASH: the LOADED module, only on Laminar/RLC/tessera root
echo "=== WEDGE PROBE $(date) kmod=$(timeout 20 $VSSH "$GUEST_KMOD_HASH" | tr -d '\r') STARVE=$STARVE PART_MB=$PART_MB NDEL=$NDEL ==="
timeout 3300 $VSSH "STARVE=$STARVE PART_MB=$PART_MB NDEL=$NDEL WAIT_S=$WAIT_S sh /root/guest-wedge-probe.sh" 2>&1 | tr -d '\r'
timeout 30 $VSSH "sysctl kern.tessera.pinscan_late_snaps kern.tessera.pinscan_late_snap_fail \
    kern.tessera.epoch_snap_birth_kept kern.tessera.flush_retry_armed \
    kern.tessera.gc_reclaimed kern.tessera.gc_delete_absent kern.tessera.gc_entry_moved" 2>&1 | tr -d '\r'
