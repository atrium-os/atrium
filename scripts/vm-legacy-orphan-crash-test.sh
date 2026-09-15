#!/bin/sh
# Crash test: nlink=0 orphans written by kmods older than the UNLINKED inode
# flag must be freed by the one-time legacy sweep — and live records whose
# nlink is merely 0-as-unset must survive it.
#
# guest-legacy-orphan.sh arm unlinks N files + 5 dirs while held open, then
# uses kern.tessera.fault_legacy_nlink0 to strip their UNLINKED flag (the
# shape an old kmod left) and to turn a linked file and a directory with
# contents into nlink=0 records; it clears the volume's feature bit. Power is
# cut. verify requires: legacy_freed == N+5, flagged_reaped == 0 (so the
# sweep, not the flag reaper, freed them), legacy_kept >= 2, survivors (incl.
# the legacy file and a file inside the legacy dir) byte-exact, no sweep on a
# second mount (bit persisted), fsck clean apart from the two legacy records.
#
#   sh scripts/vm-legacy-orphan-crash-test.sh            # ROUNDS=2 N=40
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
ROUNDS=${ROUNDS:-2}; N=${N:-40}
VSSH="$BSD/scripts/vssh"; SOCK=/tmp/qmp.sock
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHO="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"

wait_ready() { i=0; while [ $i -lt 60 ]; do timeout 15 $VSSH 'echo ready' 2>/dev/null | grep -q ready && return 0; i=$((i+1)); sleep 5; done; return 1; }
power_cut_and_relaunch() {
    echo quit | nc -U -w2 $SOCK >/dev/null 2>&1
    i=0; while [ $i -lt 15 ]; do pgrep -f qemu-system-aarch64 >/dev/null || break; i=$((i+1)); sleep 2; done
    pgrep -f qemu-system-aarch64 >/dev/null && pkill -9 -f qemu-system-aarch64; sleep 2
    ( cd "$BSD" && ./scripts/run-vm.sh >/tmp/lorph-boot.log 2>&1 </dev/null & ) >/dev/null 2>&1 </dev/null
    sleep 20
    wait_ready
}

wait_ready || { echo "FAIL — VM not reachable"; exit 1; }
k=$(timeout 20 $VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
echo "=== LEGACY-ORPHAN CRASH TEST $(date) guest_kmod=$k rounds=$ROUNDS N=$N ==="

r=1
while [ $r -le $ROUNDS ]; do
    timeout 60 scp $SSHO -P 2222 "$BSD/scripts/guest-legacy-orphan.sh" root@localhost:/root/guest-legacy-orphan.sh >/dev/null \
        || { echo "FAIL — could not copy the guest script"; exit 1; }
    arm=$(timeout 600 $VSSH "N=$N sh /root/guest-legacy-orphan.sh arm" 2>&1 | tr -d '\r')
    echo "$arm" | sed "s/^/round $r: /"
    case "$arm" in *armed:*hooked=$((N + 7))*) ;; *) echo "FAIL — round $r: arm did not complete"; exit 1;; esac
    power_cut_and_relaunch || { echo "FAIL — round $r: VM did not come back after the power cut"; exit 1; }
    ver=$(timeout 900 $VSSH "sh /root/guest-legacy-orphan.sh verify" 2>&1 | tr -d '\r')
    echo "$ver" | sed "s/^/round $r: /"
    v() { echo "$ver" | sed -n "s/.*$1=\([0-9A-Z_]*\).*/\1/p" | head -1; }
    bad=""
    [ "$(v mount_ok)" = 1 ]            || bad="$bad mount-failed"
    [ "$(v aborted)" = 0 ]             || bad="$bad sweep-aborted"
    [ "$(v legacy_freed)" = $((N + 5)) ] || bad="$bad legacy_freed=$(v legacy_freed)!=$((N + 5))"
    [ "$(v flagged_reaped)" = 0 ]      || bad="$bad flagged_reaped=$(v flagged_reaped)(flag-strip-dead)"
    [ "$(v legacy_kept)" -ge 2 ] 2>/dev/null || bad="$bad legacy_kept=$(v legacy_kept)"
    [ "$(v keep_ok)" = 1 ]             || bad="$bad survivors-wrong"
    [ "$(v gone_left)" = 0 ]           || bad="$bad gone_left=$(v gone_left)"
    [ "$(v resweep)" = 0 ]             || bad="$bad resweep=$(v resweep)"
    [ "$(v fsck_problems)" = 0 ]       || bad="$bad fsck=$(v fsck_problems)"
    [ -z "$bad" ] || { echo "FAIL — round $r:$bad"; exit 1; }
    r=$((r+1))
done
echo "PASS — $ROUNDS rounds: old-kmod orphans freed by the one-time sweep, live nlink=0 records kept, sweep ran once, fsck clean"
