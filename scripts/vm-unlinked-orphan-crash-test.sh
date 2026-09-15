#!/bin/sh
# Crash test: files and directories unlinked while still referenced must not
# outlive a power cut as nlink==0 orphans.
#
# vop_remove / rmdir keep the record at nlink=0 until the last reference
# closes (POSIX). Power loss in that window used to leave a durable record no
# name reaches — fsck: "orphan inode N" + "inode N: nlink == 0". The next
# writable mount now frees them (tessera_fs_reap_unlinked).
#
# Per round: guest-unlinked-orphan.sh arm holds N files and 5 directories
# open, unlinks them, lets the unlink commit; power is cut (QMP quit); verify
# mounts, requires reaped >= N+5 (dead-arm guard: the orphans really were
# durable), survivors byte-exact (incl. a hardlink whose other name went),
# nothing left in the removed dirs, and a clean fsck. Stops at the first bad
# round.
#
#   sh scripts/vm-unlinked-orphan-crash-test.sh            # ROUNDS=2 N=40
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
    ( cd "$BSD" && ./scripts/run-vm.sh >/tmp/uorph-boot.log 2>&1 </dev/null & ) >/dev/null 2>&1 </dev/null
    sleep 20
    wait_ready
}

wait_ready || { echo "FAIL — VM not reachable"; exit 1; }
k=$(timeout 20 $VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
echo "=== UNLINKED-ORPHAN CRASH TEST $(date) guest_kmod=$k rounds=$ROUNDS N=$N ==="

r=1
while [ $r -le $ROUNDS ]; do
    timeout 60 scp $SSHO -P 2222 "$BSD/scripts/guest-unlinked-orphan.sh" root@localhost:/root/guest-unlinked-orphan.sh >/dev/null \
        || { echo "FAIL — could not copy the guest script"; exit 1; }
    arm=$(timeout 600 $VSSH "N=$N sh /root/guest-unlinked-orphan.sh arm" 2>&1 | tr -d '\r')
    echo "$arm" | sed "s/^/round $r: /"
    case "$arm" in *armed:*) ;; *) echo "FAIL — round $r: arm did not complete"; exit 1;; esac
    power_cut_and_relaunch || { echo "FAIL — round $r: VM did not come back after the power cut"; exit 1; }
    ver=$(timeout 900 $VSSH "sh /root/guest-unlinked-orphan.sh verify" 2>&1 | tr -d '\r')
    echo "$ver" | sed "s/^/round $r: /"
    v() { echo "$ver" | sed -n "s/.*$1=\([0-9A-Z_]*\).*/\1/p" | head -1; }
    bad=""
    [ "$(v mount_ok)" = 1 ]            || bad="$bad mount-failed"
    [ "$(v reaped)" -ge $((N + 5)) ] 2>/dev/null || bad="$bad reaped=$(v reaped)<$((N + 5))"
    [ "$(v keep_ok)" = 1 ]             || bad="$bad survivors-wrong"
    [ "$(v gone_left)" = 0 ]           || bad="$bad gone_left=$(v gone_left)"
    [ "$(v hl_nlink)" = 1 ]            || bad="$bad hl_nlink=$(v hl_nlink)"
    [ "$(v fsck_problems)" = 0 ]       || bad="$bad fsck=$(v fsck_problems)"
    [ -z "$bad" ] || { echo "FAIL — round $r:$bad"; exit 1; }
    r=$((r+1))
done
echo "PASS — $ROUNDS rounds: every unlinked-while-open file and directory was freed after power loss, survivors intact, fsck clean"
