#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 16 --seed-file hello --seed-inode 1000 \
    --seed-content "Hello, Tessera!" /tmp/test.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera

echo "--- create file then hardlink twice ---"
echo "data" > /mnt/tessera/orig
ln /mnt/tessera/orig /mnt/tessera/link1
ln /mnt/tessera/orig /mnt/tessera/link2
ls -la /mnt/tessera/orig /mnt/tessera/link1 /mnt/tessera/link2
echo "all 3 should show nlink=3"

echo "--- rm one link ---"
rm /mnt/tessera/link1
ls -la /mnt/tessera/orig /mnt/tessera/link2 2>&1
cat /mnt/tessera/orig
cat /mnt/tessera/link2
echo "remaining 2 should show nlink=2; data still readable"

echo "--- rm second link ---"
rm /mnt/tessera/link2
ls -la /mnt/tessera/orig
cat /mnt/tessera/orig
echo "orig should show nlink=1; data still readable"

echo "--- rm last link ---"
rm /mnt/tessera/orig
ls /mnt/tessera/

echo "--- remount and verify state ---"
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
ls /mnt/tessera/
cat /mnt/tessera/hello
echo

umount /mnt/tessera
mdconfig -d -u 0
kldunload tessera_fs
echo DONE
