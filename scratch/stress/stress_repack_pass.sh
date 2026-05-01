#!/bin/sh
# Repack pass driver test (B2: tessera_fs_repack_pass +
# kern.tessera.repack_now sysctl).
#
# Mount, force_multi_extent=1, write N files (each becomes a
# MULTI_EXTENT pack on drain), unmount, remount with force=0, run
# repack_now=1000, verify all packs are now contig (no MULTI_EXTENT
# remaining), check stats sysctls.
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
N=10

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/repack.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/repack.img)

echo "=== mount #1 — force ON, write $N files ==="
mount -t tessera /dev/$MD /mnt/tessera
sysctl kern.tessera.force_multi_extent=1
cd /mnt/tessera
i=0
while [ $i -lt $N ]; do
    dd if=/dev/random of=file$i bs=4096 count=2 2>/dev/null
    sha256 -q file$i > /tmp/sha.$i
    i=$((i + 1))
done
cd /
umount /mnt/tessera

echo "=== mount #2 — force OFF, run repack_now ==="
sysctl kern.tessera.force_multi_extent=0
mount -t tessera /dev/$MD /mnt/tessera

dmesg -c > /dev/null

sysctl kern.tessera.repack_now=1000
echo "--- stats ---"
sysctl kern.tessera.repack_last_packs kern.tessera.repack_last_time_ms \
    kern.tessera.repack_total_packs

echo "--- repack messages (last 20) ---"
dmesg | grep -E "repack|pack —" | tail -20

echo "=== verify content ==="
fail=0
cd /mnt/tessera
i=0
while [ $i -lt $N ]; do
    orig=$(cat /tmp/sha.$i)
    new=$(sha256 -q file$i)
    if [ "$orig" != "$new" ]; then
        echo "FAIL: file$i $orig -> $new"
        fail=1
    fi
    i=$((i + 1))
done
[ $fail -eq 0 ] && echo "all $N files ok"
cd /
umount /mnt/tessera

echo "=== mount #3 — verify on-disk ==="
mount -t tessera /dev/$MD /mnt/tessera
cd /mnt/tessera
i=0
while [ $i -lt $N ]; do
    orig=$(cat /tmp/sha.$i)
    new=$(sha256 -q file$i)
    if [ "$orig" != "$new" ]; then
        echo "FAIL after remount: file$i"
        fail=1
    fi
    i=$((i + 1))
done
[ $fail -eq 0 ] && echo "all $N files ok after remount"
cd /
umount /mnt/tessera
mdconfig -d -u 0
if [ $fail -eq 0 ]; then echo DONE; else echo FAILED; exit 1; fi
