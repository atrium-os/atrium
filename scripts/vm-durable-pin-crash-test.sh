#!/bin/sh
# Crash test: a node the DURABLE superblock still references must never be
# released while a failed flush has left the in-memory roots ahead of it.
#
# THE GAP THIS PINS DOWN
#
#   After a failed flush, the drains that ran have copy-on-write freed, into
#   meta_pending, nodes the on-disk superblock still references.
#   tessera_fs_meta_pending_drain — called by the flush preflight and the
#   allocator's emergency path, with no durable commit behind them — released
#   every pending sector the latest pinscan had not pinned, and pinscan pinned
#   only the in-memory roots and retained snapshot records (which cover the
#   inode, pack-registry and free-extent trees). A durable blob-index node was
#   therefore reusable; power loss then left a superblock pointing at a sector
#   holding something else.
#
# Natural load almost never produces "failed flush AND a drain" together (the
# preflight keeps the reserve alive), so this drives it with test hooks:
#   fault_commit_fail=2      commits on the scratch volume fail after the drains
#   fault_pinscan_drain=1    run the scan + drain a failed flush's preflight does
# then writes more to reuse whatever was released, cuts power (QMP quit), and
# after the reboot mounts (replay), re-reads all committed data, and fscks.
#
# PASS per round: mounted; committed data and checksummed files byte-exact; no
# read/walk errors; no STALE reads; fsck clean. Stops at the first bad round.
# Needs a kmod with the test hooks (kern.tessera.fault_pinscan_drain).
#
#   sh scripts/vm-durable-pin-crash-test.sh            # ROUNDS=3
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
ROUNDS=${ROUNDS:-3}
VSSH="$BSD/scripts/vssh"; SOCK=/tmp/qmp.sock
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHO="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"

wait_ready() { i=0; while [ $i -lt 60 ]; do timeout 15 $VSSH 'echo ready' 2>/dev/null | grep -q ready && return 0; i=$((i+1)); sleep 5; done; return 1; }
power_cut_and_relaunch() {
    echo quit | nc -U -w2 $SOCK >/dev/null 2>&1
    i=0; while [ $i -lt 15 ]; do pgrep -f qemu-system-aarch64 >/dev/null || break; i=$((i+1)); sleep 2; done
    pgrep -f qemu-system-aarch64 >/dev/null && pkill -9 -f qemu-system-aarch64; sleep 2
    ( cd "$BSD" && ./scripts/run-vm.sh >/tmp/dpin-boot.log 2>&1 </dev/null & ) >/dev/null 2>&1 </dev/null
    sleep 20
    wait_ready
}

wait_ready || { echo "FAIL — VM not reachable"; exit 1; }
k=$(timeout 20 $VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
echo "=== DURABLE-PIN CRASH TEST $(date) guest_kmod=$k rounds=$ROUNDS ==="

r=1
while [ $r -le $ROUNDS ]; do
    timeout 60 scp $SSHO -P 2222 "$BSD/scripts/guest-durable-pin.sh" root@localhost:/root/guest-durable-pin.sh >/dev/null \
        || { echo "FAIL — could not copy the guest script"; exit 1; }
    arm=$(timeout 1200 $VSSH "sh /root/guest-durable-pin.sh arm" 2>&1 | tr -d '\r')
    echo "$arm" | sed "s/^/round $r: /"
    case "$arm" in *armed:*) ;; *) echo "FAIL — round $r: arm did not complete"; timeout 200 $VSSH "sh /root/guest-durable-pin.sh cleanup" >/dev/null 2>&1; exit 1;; esac
    cfail=$(echo "$arm" | sed -n 's/.*commit_failures=\([0-9]*\).*/\1/p')
    [ "${cfail:-0}" -gt 0 ] || { echo "FAIL — round $r: no commit failed; the fault hook did not engage, nothing was tested"; exit 1; }
    power_cut_and_relaunch || { echo "FAIL — round $r: VM did not come back after the power cut"; exit 1; }
    ver=$(timeout 1500 $VSSH "sh /root/guest-durable-pin.sh verify" 2>&1 | tr -d '\r')
    echo "$ver" | sed "s/^/round $r: /"
    v() { echo "$ver" | sed -n "s/.*$1=\([0-9A-Z_]*\).*/\1/p" | head -1; }
    bad=""
    [ "$(v mount_ok)" = 1 ]      || bad="$bad mount-failed"
    [ "$(v committed_ok)" = 1 ]  || bad="$bad committed-data-wrong"
    [ "$(v keep_ok)" = 1 ]       || bad="$bad keep-files-wrong"
    [ "$(v read_errors)" = 0 ]   || bad="$bad read_errors=$(v read_errors)"
    [ "$(v walk_errors)" = 0 ]   || bad="$bad walk_errors=$(v walk_errors)"
    [ "$(v stale)" = 0 ]         || bad="$bad stale=$(v stale)"
    [ "$(v fsck_problems)" = 0 ] || bad="$bad fsck=$(v fsck_problems)"
    [ -z "$bad" ] || { echo "FAIL — round $r: the durable state was damaged:$bad"; exit 1; }
    r=$((r+1))
done
echo "PASS — $ROUNDS rounds: durable state intact after failed flushes, a forced release, reuse, and power loss"
