#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true

$BIN/mkfs-tessera --create -s 16 --seed-file hello --seed-inode 1000 \
    --seed-content "Hello, Tessera!" /tmp/test.img >/dev/null
echo "MKFS_OK"

MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera
echo "Initial free sectors:"
df -k /mnt/tessera 2>/dev/null || true

# Do many mutations — each create+remove cycle leaks a couple of packs.
i=0
while [ $i -lt 50 ]; do
    : > /mnt/tessera/throwaway_$i
    rm /mnt/tessera/throwaway_$i
    i=$((i + 1))
done
echo "50 create-remove cycles done"

umount /mnt/tessera
mdconfig -d -u 0

# Remount → GC should find many orphaned packs and free them.
echo "--- remount triggers GC:"
MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera
ls /mnt/tessera/
cat /mnt/tessera/hello
echo
echo "--- dmesg GC line:"
dmesg | grep -E "GC reclaimed|reclaimed" | tail -3
umount /mnt/tessera
mdconfig -d -u 0
echo "DONE"
