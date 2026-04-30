#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/snap /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

mkdir -p /mnt/snap /mnt/tessera
$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/im.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/im.img)

mount -t tessera /dev/$MD /mnt/tessera
echo "v1" > /mnt/tessera/log
umount /mnt/tessera

echo "--- gen=2 immediately, before ANY other mount ---"
mount -t tessera -o tessera.gen=2 /dev/$MD /mnt/snap
ls -la /mnt/snap
cat /mnt/snap/log 2>&1
umount /mnt/snap
mdconfig -d -u 0
echo DONE
