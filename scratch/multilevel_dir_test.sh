#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/mldir.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/mldir.img)
mount -t tessera /dev/$MD /mnt/tessera

mkdir /mnt/tessera/big

echo "--- create 200 entries (should cross 4 KiB threshold around ~150) ---"
for i in $(jot 200); do
    touch /mnt/tessera/big/entry_$i
done
echo "  files: $(ls /mnt/tessera/big | wc -l | awk '{print $1}')"

echo "--- spot-check lookup of entries from various positions ---"
for n in 1 50 100 150 199 200; do
    if [ -f /mnt/tessera/big/entry_$n ]; then
        echo "  entry_$n found"
    else
        echo "  entry_$n MISSING (FAIL)"
        exit 1
    fi
done

echo "--- readdir count ---"
N=$(ls /mnt/tessera/big | wc -l | awk '{print $1}')
echo "  readdir returned $N entries (expected 200)"
[ "$N" -eq 200 ] || { echo "FAIL"; exit 1; }

echo "--- remount + verify ---"
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
N=$(ls /mnt/tessera/big | wc -l | awk '{print $1}')
echo "  remount readdir: $N (expected 200)"
[ "$N" -eq 200 ] || { echo "FAIL"; exit 1; }

# Spot-check after remount.
for n in 1 100 200; do
    [ -f /mnt/tessera/big/entry_$n ] || { echo "FAIL post-remount $n"; exit 1; }
done
echo "  remount lookup OK"

echo "--- remove half the entries, lookup remaining ---"
for i in $(jot 100); do
    rm /mnt/tessera/big/entry_$i
done
N=$(ls /mnt/tessera/big | wc -l | awk '{print $1}')
echo "  after rm: $N (expected 100)"
[ "$N" -eq 100 ] || { echo "FAIL"; exit 1; }

# Verify the right ones remain.
for n in 101 150 200; do
    [ -f /mnt/tessera/big/entry_$n ] || { echo "FAIL: entry_$n should remain"; exit 1; }
done
for n in 1 50 100; do
    [ ! -f /mnt/tessera/big/entry_$n ] || { echo "FAIL: entry_$n should be gone"; exit 1; }
done
echo "  partial removal correct"

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
