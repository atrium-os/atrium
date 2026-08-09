#!/bin/sh
# #135 regression test — a volume that cannot commit must SAY SO.
#
# WHY. The dev root spent an unknown period unable to seal a superblock. Every
# write landed in cache, every sync(2) returned success, and on reboot the
# volume rolled back to its last good commit. Measured: a kmod stat'd 450856
# bytes before `shutdown -r now` and 447072 after. Nothing anywhere reported an
# error. Silent rollback is worse than EIO, because the operator never learns.
#
# Uses kern.tessera.fault_commit_fail to force the failure: manufacturing a
# real one means exhausting the meta-reserve, which is slow and not reliably
# repeatable, and the behaviour under test is what happens AFTER a commit fails.
#
# Run on a SCRATCH volume (never the root — #129):
#     sh vm-commit-fail-test.sh /dev/vtbd1
set -u
DEV="${1:?device}"; MNT=/mnt/cfail
#
# ★ REFUSE ANYTHING THAT IS NOT THE SCRATCH DISK (#129). This script runs
# mkfs. The scratch disk is vtbd1 under the test harness and vtbd2 under
# run-vm.sh, so "the device I used last time" is not a safe habit — passing
# the wrong one here aims mkfs at the apps volume or the root. Gate on the
# GPT/GEOM ident, not the device number, and never on a mounted device.
#
if mount | grep -q "^${DEV} \|^${DEV}p"; then
    echo "REFUSING: $DEV is mounted"; exit 2
fi
_ident=$(geom disk list "$(basename "$DEV")" 2>/dev/null | awk '/ident:/{print $2}')
if [ "${FORCE_DEV:-0}" != "1" ] && [ "$_ident" != "atrium-scratch" ]; then
    echo "REFUSING: $DEV has ident '${_ident:-none}', expected 'atrium-scratch'."
    echo "          (set FORCE_DEV=1 only if you are certain it is disposable)"
    exit 2
fi
S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
fail=0
ok()  { echo "  ok   $*"; }
bad() { echo "  FAIL $*"; fail=$((fail+1)); }

sysctl kern.tessera.fault_commit_fail=0 >/dev/null 2>&1
umount $MNT 2>/dev/null; mkdir -p $MNT
/root/mkfs-tessera $DEV >/dev/null 2>&1 || { echo "mkfs failed"; exit 2; }
mount -t tessera $DEV $MNT || { echo "mount failed"; exit 2; }

# ── healthy baseline: fsync must SUCCEED ─────────────────────────────
#
# NOTE: test fsync(2), NOT sync(2). On FreeBSD sync(2) returns no error to
# userland ever — it cannot express failure — so `sync; echo $?` is an
# unobservable and would pass no matter how broken the volume is. dd's
# conv=fsync gives a real fsync whose failure is visible. VFS_SYNC still
# propagates (unmount checks it), but fsync is the contract a program sees.
dd if=/dev/random of=$MNT/a bs=4k count=1 conv=fsync 2>/dev/null
[ $? -eq 0 ] && ok "fsync succeeds on a healthy volume" \
             || bad "fsync failed on a healthy volume"

c0=$(S commit_failed); a0=$(S commit_failed_admit)

# ── inject: every commit now fails ───────────────────────────────────
sysctl kern.tessera.fault_commit_fail=1 >/dev/null
dd if=/dev/random of=$MNT/b bs=4k count=1 conv=fsync 2>/dev/null
if [ $? -ne 0 ]; then ok "fsync reports EIO while the volume cannot commit"
else                  bad "fsync STILL reports success — #135 not fixed"; fi

[ "$(S commit_failed)" -gt "$c0" ] \
  && ok "cannot-commit state latched (commit_failed=$(S commit_failed))" \
  || bad "cannot-commit state did NOT latch"

# writes must now be refused rather than accepted and lost
i=0; refused=0
while [ $i -lt 200 ]; do
    dd if=/dev/random of=$MNT/doomed$i bs=64k count=1 2>/dev/null || refused=1
    i=$((i+1))
done
if [ "$(S commit_failed_admit)" -gt "$a0" ] || [ $refused -eq 1 ]; then
    ok "new allocations refused (admit_refused=$(( $(S commit_failed_admit) - a0 )))"
else
    bad "writes still ACCEPTED on a volume that cannot commit — they would be lost"
fi

# ── clear the fault: the volume must recover and say so ──────────────
# ── clearing the fault must NOT silently resume: the latch is STICKY ──
sysctl kern.tessera.fault_commit_fail=0 >/dev/null
dd if=/dev/random of=$MNT/c bs=4k count=1 conv=fsync 2>/dev/null
if [ $? -ne 0 ]; then ok "still refusing after the fault cleared (sticky, as intended)"
else                  bad "volume silently resumed — a degraded mount must stay degraded"; fi

# ── the operator clear must restore service IN PLACE ─────────────────
# The root filesystem cannot be unmounted, so "sticky until unmount" would
# mean "sticky until reboot" on the volume that needs this most. This is the
# `zpool clear` equivalent.
sysctl kern.tessera.commit_failed_clear=1 >/dev/null
dd if=/dev/random of=$MNT/d bs=4k count=1 conv=fsync 2>/dev/null
if [ $? -eq 0 ]; then ok "operator clear restores durability in place"
else                  bad "operator clear did NOT restore service"; fi

# ── clearing while STILL broken must re-latch, not paper over it ─────
sysctl kern.tessera.fault_commit_fail=1 >/dev/null
dd if=/dev/random of=$MNT/e bs=4k count=1 conv=fsync 2>/dev/null
sysctl kern.tessera.commit_failed_clear=1 >/dev/null
dd if=/dev/random of=$MNT/f bs=4k count=1 conv=fsync 2>/dev/null
if [ $? -ne 0 ]; then ok "clearing a still-broken volume re-latches"
else                  bad "clear papered over a volume that still cannot commit"; fi
sysctl kern.tessera.fault_commit_fail=0 >/dev/null

# ── and a REMOUNT also restores service ──────────────────────────────
umount $MNT 2>/dev/null
mount -t tessera $DEV $MNT || { echo "  FAIL remount failed"; fail=$((fail+1)); }
dd if=/dev/random of=$MNT/g bs=4k count=1 conv=fsync 2>/dev/null
if [ $? -eq 0 ]; then ok "remount also clears the state"
else                  bad "remount did NOT restore service"; fi

umount $MNT 2>/dev/null
sysctl kern.tessera.fault_commit_fail=0 >/dev/null
echo
[ $fail -eq 0 ] && echo "#135: ALL CHECKS PASSED" || echo "#135: $fail CHECK(S) FAILED"
exit $fail
