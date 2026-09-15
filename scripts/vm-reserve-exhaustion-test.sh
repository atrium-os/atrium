#!/bin/sh
# Regression test: running the metadata reserve into its emergency band must
# fail CLEANLY — refused and retried operations, never a corrupt volume.
#
# THE BUGS THIS PINS DOWN
#
#   1. B+tree mutations freed each superseded node as soon as its replacement
#      was written, but could still fail further up (the parent's allocation).
#      The caller then kept the OLD root, which still referenced those nodes.
#      At the band the allocator's epoch sweep recycles same-flush frees at
#      once, so still-live nodes went to other trees: "sector N holds a inode
#      node but was reached as snapshot", pinscan aborting on every pass (so
#      the reserve never recovered), and fsck problems. Core fix: frees are
#      applied only when the mutation succeeds (core/tests/test_btree_txn.c).
#   2. A failed commit_extent was reported and ignored, so commit_sb wrote a
#      superblock whose free-extent root predated the flush's allocations.
#
# HOW IT GETS THERE, on the scratch disk only (guest half:
# scripts/guest-reserve-exhaustion.sh):
#   1. a PART_MB (default 256) GPT partition of the scratch disk — reserve
#      4,096 sectors, soft 3,584 — and DIRS x PER_DIR files (40 x 500), or
#      PART_MB=0 for the whole 4 GiB disk with 800 x 500 files,
#   2. 4 workers for SECS (default 120): each round dirties every inode in
#      its 10 directories (touch -c, not admission-gated), creates 50 files,
#      removes the previous round's 50, and syncs,
#   3. default tunables unless asked: SLOW_RECLAIM=1 (duty 1%, no tight bypass),
#      STARVE=1 (also pressure kicks and preflight off — global, starves the
#      ROOT too), TRIGGER=1 (mark_dirty_meta_trigger, on a kmod that has it);
#      WORKLOAD=spread for many small commits instead of bulk dirtying,
#   4. a recovery phase that must bring the volume back to admitting.
#
# PASS: the band was hit (dead-arm guard: meta_band_refusals rose), no STALE
# root reads, no pinscan aborts, no worker stuck 120 s after stop, the volume
# recovers and unmounts, fsck is clean, and every directory's last file exists.
#
#   sh scripts/vm-reserve-exhaustion-test.sh
#   HANGDUMP=/path/hang.core sh scripts/vm-reserve-exhaustion-test.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
SECS=${SECS:-120}
VSSH="$BSD/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHO="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
# HANGDUMP=<path>: if the guest stops answering ssh mid-run, dump its memory
# there (QEMU monitor) BEFORE anything resets it — see
# reference_hung_guest_memory_dump in project memory for reading the dump.
HANGDUMP=${HANGDUMP:-}

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
k=$($VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
echo "=== RESERVE-EXHAUSTION TEST $(date) guest_kmod=$k tree_kmod=$KMOD secs=$SECS part_mb=${PART_MB:-256} workload=${WORKLOAD:-bulk} starve=${STARVE:-0} slow_reclaim=${SLOW_RECLAIM:-0} trigger=${TRIGGER:-0} ==="
[ "$k" = "$KMOD" ] || echo "NOTE: guest module differs from the tree (baseline run against an older kmod?)"

timeout 60 scp $SSHO -P 2222 "$BSD/scripts/guest-reserve-exhaustion.sh" root@localhost:/root/guest-reserve-exhaustion.sh >/dev/null \
    || { echo "FAIL — could not copy the guest script"; exit 1; }
# A previous run that died mid-way must not share this one.
timeout 300 $VSSH "sh /root/guest-reserve-exhaustion.sh cleanup" 2>&1 | tr -d '\r'

WATCH=""
if [ -n "$HANGDUMP" ]; then
    (
        fails=0
        while [ ! -f /tmp/rx-host.done.$$ ]; do
            if timeout 15 ssh $SSHO -o ConnectTimeout=10 -p 2222 root@localhost 'echo ok' 2>/dev/null | grep -q ok; then
                fails=0
            else
                fails=$((fails+1))
            fi
            if [ $fails -ge 3 ]; then
                echo "HANG: guest unreachable — dumping memory to $HANGDUMP"
                python3 - "$HANGDUMP" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect("/tmp/qmp.sock"); s.settimeout(2)
time.sleep(0.3)
try: s.recv(65536)
except Exception: pass
reg = b""
s.sendall(b"info registers -a\n"); time.sleep(1.5)
try:
    while True:
        d = s.recv(1 << 20)
        if not d: break
        reg += d
except Exception: pass
open(sys.argv[1] + ".regs", "wb").write(reg)
s.sendall(("dump-guest-memory %s\n" % sys.argv[1]).encode())
s.settimeout(900); buf = b""
while b"(qemu)" not in buf[-20:]:
    d = s.recv(4096)
    if not d: break
    buf += d
PY
                echo "HANG: dump written"
                exit 0
            fi
            sleep 10
        done
    ) &
    WATCH=$!
fi

GUEST_BOUND=$((SECS + 1500))
OUT=$(timeout $((GUEST_BOUND + 120)) $VSSH "SECS=$SECS PART_MB=${PART_MB:-256} ${DIRS:+DIRS=$DIRS} ${PER_DIR:+PER_DIR=$PER_DIR} WORKLOAD=${WORKLOAD:-bulk} STARVE=${STARVE:-0} SLOW_RECLAIM=${SLOW_RECLAIM:-0} TRIGGER=${TRIGGER:-0} timeout $GUEST_BOUND sh /root/guest-reserve-exhaustion.sh run" 2>&1 | tr -d '\r')
hrc=$?
touch /tmp/rx-host.done.$$
[ -n "$WATCH" ] && wait $WATCH 2>/dev/null
rm -f /tmp/rx-host.done.$$
echo "$OUT"
# Whatever happened, leave nothing running on the guest.
timeout 300 $VSSH "sh /root/guest-reserve-exhaustion.sh cleanup" 2>&1 | tr -d '\r'

case "$OUT" in *REFUSING_ident*|*MKFS_FAIL*|*PART_FAIL*|*NO_TRIGGER_KNOB*) echo "FAIL — harness could not run"; exit 1;; esac
v() { echo "$OUT" | sed -n "s/.*$1=\([0-9A-Z_]*\).*/\1/p" | head -1; }
[ -n "$(v band_refusals)" ] || { echo "FAIL — the run did not finish (host rc=$hrc); see output above"; exit 1; }
rc=0
[ "$(v band_refusals)" -gt 0 ] 2>/dev/null || { echo "INCONCLUSIVE — the band was never hit, so nothing was tested"; exit 2; }
[ "$(v stale)" = 0 ]          || { echo "FAIL — $(v stale) STALE root reads: live metadata was recycled"; rc=1; }
[ "$(v pinscan_aborts)" = 0 ] || { echo "FAIL — $(v pinscan_aborts) pinscan aborts: reclaim could not complete"; rc=1; }
[ "$(v stuck_workers)" = 0 ]  || { echo "FAIL — $(v stuck_workers) workers still blocked 120 s after stop"; rc=1; }
[ "$(v recovered)" = 1 ]      || { echo "FAIL — the volume never returned to admitting after the load stopped"; rc=1; }
[ "$(v recovered_passive)" = 1 ] || echo "NOTE — recovery needed sync/probe traffic; it did not come back passively after one refused create"
[ "$(v umount_ok)" = 1 ]      || { echo "FAIL — the volume did not unmount"; rc=1; }
[ "$(v fsck_problems)" = 0 ]  || { echo "FAIL — fsck: $(v fsck_problems)"; rc=1; }
[ "$(v survivor_missing)" = 0 ] || { echo "FAIL — $(v survivor_missing) directories lost their last populated file"; rc=1; }
[ $rc = 0 ] && echo "PASS — the band was hit $(v band_refusals) times and the volume failed cleanly: no stale roots, reclaim intact, fsck clean"
exit $rc
