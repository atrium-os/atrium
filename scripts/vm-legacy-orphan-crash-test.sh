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
# The sweep runs in the background, so verify also races it: with
# fault_legacy_sweep_pause the sweep stops after reading /, the live legacy
# file and directory are renamed from an unread directory into /, and the
# rename barrier must keep them (legacy_sweep_batch=7 also exercises the
# batched tree walk). NOBARRIER=1 disables the barrier — that run is expected
# to FAIL (survivors-wrong), proving the race is really exercised.
#
#   sh scripts/vm-legacy-orphan-crash-test.sh            # ROUNDS=2 N=40
#   NOBARRIER=1 ROUNDS=1 sh scripts/vm-legacy-orphan-crash-test.sh   # must FAIL
#   MODE=churn ROUNDS=2 sh scripts/vm-legacy-orphan-crash-test.sh
#     1200-dir tree, the sweep slowed per directory while a churner renames
#     30 live legacy files + 10 live legacy dirs between random directories
#     (publishing with sync): exactly the 20 orphans freed, all 40 kept.
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
ROUNDS=${ROUNDS:-2}; N=${N:-40}; MODE=${MODE:-race}
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
. "$BSD/scripts/lib/guest-ident.sh"   # GUEST_KMOD_HASH: the LOADED module, only on Laminar/RLC/tessera root
k=$(timeout 20 $VSSH "$GUEST_KMOD_HASH" 2>/dev/null | tr -d '\r')
echo "=== LEGACY-ORPHAN CRASH TEST $(date) guest_kmod=$k rounds=$ROUNDS N=$N ==="

r=1
while [ $r -le $ROUNDS ]; do
    timeout 60 scp $SSHO -P 2222 "$BSD/scripts/guest-legacy-orphan.sh" root@localhost:/root/guest-legacy-orphan.sh >/dev/null \
        || { echo "FAIL — could not copy the guest script"; exit 1; }
    if [ "$MODE" = churn ]; then
        arm=$(timeout 900 $VSSH "sh /root/guest-legacy-orphan.sh churn-arm" 2>&1 | tr -d '\r')
    else
        arm=$(timeout 600 $VSSH "N=$N sh /root/guest-legacy-orphan.sh arm" 2>&1 | tr -d '\r')
    fi
    echo "$arm" | sed "s/^/round $r: /"
    [ "$MODE" = churn ] && want_hooked=60 || want_hooked=$((N + 7))
    case "$arm" in *armed:*hooked=$want_hooked*) ;; *) echo "FAIL — round $r: arm did not complete"; exit 1;; esac
    power_cut_and_relaunch || { echo "FAIL — round $r: VM did not come back after the power cut"; exit 1; }
    [ "$MODE" = churn ] && vmode=churn-verify || vmode=verify
    ver=$(timeout 900 $VSSH "NOBARRIER=${NOBARRIER:-0} sh /root/guest-legacy-orphan.sh $vmode" 2>&1 | tr -d '\r')
    echo "$ver" | sed "s/^/round $r: /"
    v() { echo "$ver" | sed -n "s/.*$1=\([0-9A-Z_]*\).*/\1/p" | head -1; }
    if [ "$MODE" = churn ]; then
        bad=""
        [ "$(v mount_ok)" = 1 ]        || bad="$bad mount-failed"
        [ "$(v done)" = 1 ]            || bad="$bad sweep-never-finished"
        [ "$(v aborted)" = 0 ]         || bad="$bad sweep-aborted"
        [ "$(v barrier_notes)" -ge 10 ] 2>/dev/null || bad="$bad barrier_notes=$(v barrier_notes)(churn-missed-the-walk)"
        [ "$(v legacy_freed)" = 20 ]   || bad="$bad legacy_freed=$(v legacy_freed)!=20"
        [ "$(v legacy_kept)" = 40 ]    || bad="$bad legacy_kept=$(v legacy_kept)!=40"
        [ "$(v bad_legacy)" = 0 ]      || bad="$bad bad_legacy=$(v bad_legacy)"
        [ "$(v fsck_problems)" = 0 ]   || bad="$bad fsck=$(v fsck_problems)"
        [ -z "$bad" ] || { echo "FAIL — round $r:$bad"; exit 1; }
        r=$((r+1)); continue
    fi
    bad=""
    [ "$(v mount_ok)" = 1 ]            || bad="$bad mount-failed"
    [ "$(v mounted_during_sweep)" = 1 ] || bad="$bad sweep-finished-before-mount-returned(not-background)"
    [ "$(v paused)" = 1 ]              || bad="$bad race-window-never-opened"
    [ "$(v moved_rc)" = 0 ]            || bad="$bad rename-failed"
    [ "$(v still_paused_after_sync)" = 1 ] || bad="$bad sweep-resumed-before-the-move-published"
    [ "$(v done)" = 1 ]                || bad="$bad sweep-never-finished"
    [ "$(v barrier_notes)" -ge 2 ] 2>/dev/null || bad="$bad barrier_notes=$(v barrier_notes)"
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
[ "$MODE" = churn ] && { echo "PASS — $ROUNDS churn rounds: 20 orphans freed, 40 live legacy records kept under concurrent renames, fsck clean"; exit 0; }
echo "PASS — $ROUNDS rounds: old-kmod orphans freed by the one-time sweep, live nlink=0 records kept, sweep ran once, fsck clean"
