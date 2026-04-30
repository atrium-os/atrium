#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
umount /mnt/snap 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

mkdir -p /mnt/snap
$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/gen.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/gen.img)

echo "--- gen 2: write 'v1', umount ---"
mount -t tessera /dev/$MD /mnt/tessera
echo "v1 content" > /mnt/tessera/log
umount /mnt/tessera

echo "--- gen 3: overwrite to 'v2', umount ---"
mount -t tessera /dev/$MD /mnt/tessera
echo "v2 content (newer)" > /mnt/tessera/log
umount /mnt/tessera

echo "--- gen 4: overwrite to 'v3', umount ---"
mount -t tessera /dev/$MD /mnt/tessera
echo "v3 final" > /mnt/tessera/log
umount /mnt/tessera

echo "--- live mount: see v3 ---"
mount -t tessera /dev/$MD /mnt/tessera
cat /mnt/tessera/log
umount /mnt/tessera

echo "--- forensic mount of gen=2 (read-only) ---"
mount -t tessera -o tessera.gen=2 /dev/$MD /mnt/snap
cat /mnt/snap/log
mount | grep tessera
umount /mnt/snap

echo "--- forensic mount of gen=3 ---"
mount -t tessera -o tessera.gen=3 /dev/$MD /mnt/snap
cat /mnt/snap/log
umount /mnt/snap

echo "--- forensic mount of bogus gen=999 should fail ---"
mount -t tessera -o tessera.gen=999 /dev/$MD /mnt/snap 2>&1 || echo "(rejected, as expected)"

mdconfig -d -u 0
echo DONE
