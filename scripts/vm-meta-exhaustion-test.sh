#!/bin/sh
# Metadata-exhaustion regression test (#73 follow-up, 2026-09-07).
#
# THE BUG THIS GUARDS. The meta reserve (the btree region: inode tree, pack
# registry, extent tree, snapshots) had no admission term. space_admit charges
# only the DATA zone, and it is reached only from the CONTENT-write paths — so
# a pure-namespace workload (empty files) was never admission-tested at all and
# ran until the ALLOCATOR refused mid-flush. That is not a clean stop:
#   - the refusal fails dirty_inodes_drain, which short-circuits the flush
#     BEFORE commit_sb, so the #135 cannot-commit latch (which fires only in
#     the commit_sb arm) never fires;
#   - writes keep being admitted, and repeated failed flushes advance
#     in-memory roots while reclaim recycles underneath.
# Observed on this fixture before the fix: EIO / "Not a directory" on LIVE
# directories while writes still "succeeded", an inode root left pointing at a
# snapshot node ("that root is STALE and its tree's contents are LOST"), and
# 440 on-disk problems (126 double-state extents + ~314 bad-CRC / blob_count
# packs) that fsck --repair cannot repair at ANY reserve level — the crash-safe
# recovery ladder was exhausted and only repack --force (NOT crash-safe) could
# restore the volume.
#
# THE FIX (tessera_fs_meta_admit): charge the meta reserve at admission, from
# the namespace-ADDING vops (create/mkdir/symlink/link) as well as space_admit.
# Available = headroom below the band-limited soft ceiling + the recycle list
# (meta_pending excluded: it needs a pinscan swap). The REMOVE paths do NOT
# call it, so unlink still unsticks a full volume. Self-clearing, no latch.
#
# PASS = the volume fills to ENOSPC with meta_admit_refusals>0,
#        meta_band_refusals==0, no STALE/drain messages, and fsck CLEAN.
#
# Needs a SMALL volume: the band is min(meta_emergency_band, reserve/8), so on
# a big volume it is unreachable. Uses a 96 MiB partition of the SCRATCH disk
# (GEOM ident atrium-scratch, #129) — NOT md-over-root, which deadlocks.
set -u
DEV=/dev/vtbd2; PART=/dev/vtbd2p1; M=/mnt/v73

diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo "REFUSING: $DEV is not atrium-scratch"; exit 2; }
S(){ sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }

mount | grep -q " $M " && umount $M 2>/dev/null
gpart destroy -F vtbd2 >/dev/null 2>&1
gpart create -s gpt vtbd2 >/dev/null
gpart add -t freebsd-ufs -s 96M -i 1 vtbd2 >/dev/null
mkfs-tessera $PART >/dev/null || { echo "mkfs failed"; exit 1; }
mkdir -p $M && mount -t tessera $PART $M || { echo "mount failed"; exit 1; }

a0=$(S meta_admit_refusals); b0=$(S meta_band_refusals); d0=$(dmesg | grep -ciE 'STALE|drain failed')
echo "=== filling with empty files (pure namespace metadata) ==="
i=0
while [ $i -lt 700 ]; do
  mkdir -p $M/e$i 2>/dev/null || break
  j=0; while [ $j -lt 50 ]; do touch $M/e$i/f$j 2>/dev/null || break; j=$((j+1)); done
  [ $j -lt 50 ] && break
  i=$((i+1))
done
sync
files=$(find $M -type f 2>/dev/null | wc -l | tr -d ' ')
adm=$(( $(S meta_admit_refusals) - a0 )); band=$(( $(S meta_band_refusals) - b0 ))
trav=$(find $M 2>&1 >/dev/null | wc -l | tr -d ' ')
echo "files=$files admit_refusals=$adm band_refusals=$band traversal_errors=$trav"
umount $M
stale=$(( $(dmesg | grep -ciE 'STALE|drain failed') - d0 ))
fsck=$(tessera-fsck $PART 2>&1 | grep -c '^    - ')
echo "stale_or_drain_msgs=$stale fsck_problems=$fsck"

rc=0
[ "$adm"  -gt 0 ] || { echo "FAIL: expected a clean ENOSPC (meta_admit_refusals>0)"; rc=1; }
[ "$band" -eq 0 ] || { echo "FAIL: allocator refused mid-flush ($band) — admission did not stop it in time"; rc=1; }
[ "$trav" -eq 0 ] || { echo "FAIL: $trav traversal errors on a live mount"; rc=1; }
[ "$stale" -eq 0 ] || { echo "FAIL: $stale STALE-root / failed-drain messages"; rc=1; }
[ "$fsck" -eq 0 ] || { echo "FAIL: fsck found $fsck problems (pre-fix baseline: 440)"; rc=1; }
[ $rc -eq 0 ] && echo "PASS — metadata exhaustion degraded to ENOSPC, volume CLEAN"
exit $rc
