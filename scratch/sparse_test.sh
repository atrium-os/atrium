#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/sparse.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/sparse.img)
mount -t tessera /dev/$MD /mnt/tessera

echo "--- baseline df ---"
df -k /mnt/tessera | tail -1

echo "--- write a 4 MiB all-zero file via dd ---"
dd if=/dev/zero of=/mnt/tessera/zeros bs=1M count=4 2>/dev/null
ls -la /mnt/tessera/zeros
df -k /mnt/tessera | tail -1
echo "(should be ~baseline + few KiB for manifest, NOT +4 MiB)"

echo "--- read back, verify all zeros ---"
md5 /mnt/tessera/zeros
md5 /tmp/zero4mb 2>/dev/null || dd if=/dev/zero bs=1M count=4 2>/dev/null | md5

echo "--- mixed file: 1 MiB random + 1 MiB zero + 1 MiB random ---"
{ dd if=/dev/urandom bs=1M count=1 2>/dev/null
  dd if=/dev/zero bs=1M count=1 2>/dev/null
  dd if=/dev/urandom bs=1M count=1 2>/dev/null; } > /tmp/mixed
cp /tmp/mixed /mnt/tessera/mixed
df -k /mnt/tessera | tail -1
echo "(zero region should not have published chunks)"
cmp /tmp/mixed /mnt/tessera/mixed && echo "mixed round-trip OK"

echo "--- remount, verify ---"
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
cmp /tmp/mixed /mnt/tessera/mixed && echo "mixed remount OK"
md5 /mnt/tessera/zeros
df -k /mnt/tessera | tail -1

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
