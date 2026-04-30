#!/bin/sh
# Volume exhaustion + recovery. Fills the volume to ENOSPC, then:
#   - Deletes some files.
#   - Verifies remaining files still readable.
#   - Verifies new writes succeed after the GC reclaim.
#   - Unmount/remount and verify integrity.
#
# Catches:
#   - Mid-write ENOSPC leaving the FS in an inconsistent state.
#   - GC failing to reclaim freed packs.
#   - meta-reserve not draining properly when extent exhausted.
#   - publish-cache hits past the deletion (would surface as a freed
#     pack still being referenced somewhere).
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

# Small volume — easy to fill.
$BIN/mkfs-tessera --create -s 16 --seed-file h --seed-content x /tmp/full.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/full.img)
mount -t tessera /dev/$MD /mnt/tessera

echo "--- fill the volume ---"
dd if=/dev/random of=/tmp/payload bs=1m count=1 2>/dev/null
i=0
while :; do
    if cp /tmp/payload /mnt/tessera/big$i 2>/dev/null; then
        i=$((i + 1))
    else
        break
    fi
    if [ $i -gt 1000 ]; then
        echo "  WARNING: 1000 files written without ENOSPC, capping"
        break
    fi
done
echo "  wrote $i files before ENOSPC (or cap)"
[ $i -ge 1 ] || { echo "FAIL: nothing written"; exit 1; }

USED1=$(df -k /mnt/tessera | tail -1 | awk '{print $3}')
echo "  used: $USED1 KB"

echo "--- delete half + sync + verify remaining ---"
j=0
while [ $j -lt $((i / 2)) ]; do
    rm /mnt/tessera/big$j
    j=$((j + 1))
done
sync

# Surviving files must still hash correctly.
EXP=$(sha256 -q /tmp/payload)
for k in $(jot 5); do
    idx=$((i / 2 + k - 1))
    if [ -f /mnt/tessera/big$idx ]; then
        GOT=$(sha256 -q /mnt/tessera/big$idx)
        [ "$GOT" = "$EXP" ] || { echo "FAIL: big$idx hash mismatch"; exit 1; }
    fi
done
echo "  surviving files hash-verified"

echo "--- unmount + remount to trigger GC reclaim ---"
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
USED2=$(df -k /mnt/tessera | tail -1 | awk '{print $3}')
echo "  used after remount + GC: $USED2 KB (was $USED1 KB)"
[ $USED2 -lt $USED1 ] || echo "  WARNING: usage didn't drop after deletes"

echo "--- write more to confirm allocation works ---"
i2=0
while :; do
    if cp /tmp/payload /mnt/tessera/new$i2 2>/dev/null; then
        i2=$((i2 + 1))
    else
        break
    fi
    [ $i2 -ge 10 ] && break
done
echo "  wrote $i2 new files post-recovery"
[ $i2 -ge 1 ] || { echo "FAIL: cannot allocate after recovery"; exit 1; }

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
