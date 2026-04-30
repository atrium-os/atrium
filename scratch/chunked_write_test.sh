#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/chunked.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/chunked.img)
mount -t tessera /dev/$MD /mnt/tessera

echo "--- write a 1 MiB file (> 256 KiB threshold → CHUNK_LIST) ---"
dd if=/dev/urandom of=/tmp/big bs=1024 count=1024 2>/dev/null
cp /tmp/big /mnt/tessera/big
ls -la /mnt/tessera/big
sysctl kern.tessera.sb_commits

echo "--- read-back and compare ---"
cmp /tmp/big /mnt/tessera/big && echo "round-trip OK ($(wc -c < /mnt/tessera/big) bytes)"

echo "--- remount ---"
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
cmp /tmp/big /mnt/tessera/big && echo "remount round-trip OK"

echo "--- write a small file (<= 256 KiB → INLINE) ---"
dd if=/dev/urandom of=/tmp/small bs=1024 count=200 2>/dev/null
cp /tmp/small /mnt/tessera/small
cmp /tmp/small /mnt/tessera/small && echo "small INLINE round-trip OK"

echo "--- write same big content to 2 files (chunk-level dedup expected) ---"
df -k /mnt/tessera | tail -1
cp /tmp/big /mnt/tessera/big2
df -k /mnt/tessera | tail -1
echo "(used should grow only by manifest, not by 1 MiB of chunks)"

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
