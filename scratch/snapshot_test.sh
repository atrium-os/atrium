#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/snap.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/snap.img)

echo "--- mount 1 (allocates snapshots tree) ---"
mount -t tessera /dev/$MD /mnt/tessera
echo "v1" > /mnt/tessera/file
echo "data" > /mnt/tessera/file2
umount /mnt/tessera   # forces commit gen=2

echo "--- mount 2: modify, umount (commit gen=3 with NEW inode_root) ---"
mount -t tessera /dev/$MD /mnt/tessera
echo "v2 contents updated" > /mnt/tessera/file
umount /mnt/tessera

echo "--- mount 3: verify content + snapshot-aware GC kicks in ---"
mount -t tessera /dev/$MD /mnt/tessera
cat /mnt/tessera/file
cat /mnt/tessera/file2

echo "--- dmesg trace (expect 'retained snapshots unionised') ---"
dmesg | grep -E "snapshots|gc pass1|GC reclaimed" | tail -10

df -k /mnt/tessera | tail -1
sysctl kern.tessera.sb_commits

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
