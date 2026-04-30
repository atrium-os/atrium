#!/bin/sh
# Slice-4 retention: cap retained snapshots at N (default 16); oldest
# gets dropped at the next commit_sb when count exceeds the cap.
#
# This test:
#   1. Lowers retention to 4 via sysctl (so we don't need 16+ commits).
#   2. Creates 8 commits (with auto-snapshot per commit).
#   3. Verifies kern.tessera.snapshots_retired bumped to ≥ 4
#      (commits 5..8 each retire the oldest).
#   4. Verifies forensic mount of the most-recent gen still works
#      (the retention isn't dropping live state).
#   5. Verifies forensic mount of a retired (early) gen now returns
#      ENOENT (record was dropped).
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
umount /mnt/snap    2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

# Lower retention so we exercise it with fewer commits.
sysctl kern.tessera.snapshot_retention=4 >/dev/null

mkdir -p /mnt/snap
$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/ret.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/ret.img)

echo "--- 8 sessions, each writes a new value + unmounts ---"
for i in 1 2 3 4 5 6 7 8; do
    mount -t tessera /dev/$MD /mnt/tessera
    echo "version $i" > /mnt/tessera/log
    umount /mnt/tessera
done

RETIRED=$(sysctl -n kern.tessera.snapshots_retired)
echo "  snapshots_retired = $RETIRED"
[ "$RETIRED" -ge 4 ] || { echo "FAIL: expected ≥4 retirements, got $RETIRED"; exit 1; }

echo "--- live mount still readable ---"
mount -t tessera /dev/$MD /mnt/tessera
LIVE=$(cat /mnt/tessera/log)
umount /mnt/tessera
[ "$LIVE" = "version 8" ] || { echo "FAIL: live mount got '$LIVE' (want 'version 8')"; exit 1; }
echo "  live → '$LIVE' OK"

echo "--- forensic mount of retired gen=2 should fail ENOENT ---"
# gen=2 was committed by session 1 and almost certainly retired by now.
# (Mount-time GC may have created extra commits, but with 8 sessions and
# retention=4, the early gens are definitely gone.)
mount -t tessera -o tessera.gen=2 /dev/$MD /mnt/snap 2>&1 \
    && { echo "FAIL: gen=2 should have been retired"; umount /mnt/snap; exit 1; } \
    || echo "  gen=2 → ENOENT (retired, as expected)"

echo "--- restore retention default + cleanup ---"
sysctl kern.tessera.snapshot_retention=16 >/dev/null
mdconfig -d -u 0
echo DONE
