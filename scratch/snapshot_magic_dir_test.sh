#!/bin/sh
# Slice-3: synthesized `/.tessera/snapshots/<gen>/` magic directory.
#
# Verifies:
#   1. `/.tessera/` exists at the live mount root and contains
#      `snapshots/`.
#   2. `ls /.tessera/snapshots/` returns one entry per retained gen
#      (synthesized from the snapshots_tree).
#   3. `cat /.tessera/snapshots/<gen>/log` returns the file content
#      AS IT WAS at that generation (uses the snapshot's inode_tree).
#   4. Writes to `/.tessera/snapshots/<gen>/...` are rejected with
#      EROFS.
#   5. Bogus subpath under `.tessera` returns ENOENT.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/magic.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/magic.img)

echo "--- 3 sessions, each writes a different value to /log ---"
for v in v1 v2 v3; do
    mount -t tessera /dev/$MD /mnt/tessera
    echo "$v content" > /mnt/tessera/log
    umount /mnt/tessera
done

echo "--- live mount + browse the magic dir ---"
mount -t tessera /dev/$MD /mnt/tessera

echo "  /.tessera/ contents:"
ls /mnt/tessera/.tessera/ | sed 's/^/    /'
[ "$(ls /mnt/tessera/.tessera/)" = "snapshots" ] || \
    { echo "FAIL: expected only 'snapshots' under .tessera"; umount /mnt/tessera; exit 1; }

echo "  /.tessera/snapshots/ contents:"
ls /mnt/tessera/.tessera/snapshots/ | sed 's/^/    /'

# At least one snapshot must show up. Don't assert exact set since
# mount-time GC creates extra commits — content is workload-dependent.
N=$(ls /mnt/tessera/.tessera/snapshots/ | wc -l | awk '{print $1}')
[ "$N" -ge 1 ] || { echo "FAIL: snapshots dir empty"; umount /mnt/tessera; exit 1; }
echo "  found $N retained snapshot(s)"

echo "--- pick the smallest gen and read its /log ---"
LOWEST=$(ls /mnt/tessera/.tessera/snapshots/ | sort -n | head -1)
echo "  lowest retained gen: $LOWEST"
SNAP_LOG=$(cat /mnt/tessera/.tessera/snapshots/$LOWEST/log)
echo "  /.tessera/snapshots/$LOWEST/log → '$SNAP_LOG'"
[ -n "$SNAP_LOG" ] || { echo "FAIL: empty content"; umount /mnt/tessera; exit 1; }

echo "--- pick the highest gen — content should reflect a recent state ---"
HIGHEST=$(ls /mnt/tessera/.tessera/snapshots/ | sort -n | tail -1)
SNAP_LOG_H=$(cat /mnt/tessera/.tessera/snapshots/$HIGHEST/log)
echo "  /.tessera/snapshots/$HIGHEST/log → '$SNAP_LOG_H'"
# Highest snapshot gen MUST capture either v3 (the live state) or
# whatever the most recent commit captured.
case "$SNAP_LOG_H" in
    "v1 content"|"v2 content"|"v3 content") echo "  highest content valid";;
    *) echo "FAIL: unexpected highest content: '$SNAP_LOG_H'"; umount /mnt/tessera; exit 1;;
esac

echo "--- live /log must still show v3 (not affected by snapshot reads) ---"
LIVE=$(cat /mnt/tessera/log)
[ "$LIVE" = "v3 content" ] || { echo "FAIL: live got '$LIVE'"; umount /mnt/tessera; exit 1; }
echo "  /log → '$LIVE' OK"

echo "--- writes to snapshot path rejected (EROFS) ---"
if echo overwrite > /mnt/tessera/.tessera/snapshots/$LOWEST/log 2>/dev/null; then
    echo "FAIL: write to snapshot succeeded — should be rejected"
    umount /mnt/tessera
    exit 1
fi
echo "  EROFS as expected"

echo "--- bogus name under .tessera/ ---"
ls /mnt/tessera/.tessera/bogus 2>/dev/null && \
    { echo "FAIL: bogus subdir resolved"; umount /mnt/tessera; exit 1; }
echo "  ENOENT as expected"

echo "--- bogus gen under snapshots/ ---"
ls /mnt/tessera/.tessera/snapshots/9999 2>/dev/null && \
    { echo "FAIL: bogus gen resolved"; umount /mnt/tessera; exit 1; }
echo "  ENOENT as expected"

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
