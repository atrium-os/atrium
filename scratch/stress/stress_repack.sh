#!/bin/sh
# Repack engine smoke test (B1: per-pack helper).
#
# Steps:
#   1. Mount fresh tessera, set force_multi_extent=1, write files, unmount.
#      Unmount-time drain publishes packs through the multi-extent
#      allocator path, flagging them MULTI_EXTENT.
#   2. Remount, clear force, trigger kern.tessera.repack_one=1.
#   3. B1 helper rewrites the first MULTI_EXTENT pack into a contiguous
#      replacement, atomic registry update, frees old extents.
#   4. Verify content unchanged across an additional remount.
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/repack.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/repack.img)

echo "=== mount #1 — force ON, write, unmount (drain creates MULTI_EXTENT) ==="
mount -t tessera /dev/$MD /mnt/tessera
sysctl kern.tessera.force_multi_extent=1
cd /mnt/tessera
for i in 1 2 3; do
    dd if=/dev/random of=file$i bs=4096 count=4 2>/dev/null
    sha256 -q file$i > /tmp/sha.$i
done
cd /
umount /mnt/tessera

echo "=== mount #2 — force OFF, then repack ==="
sysctl kern.tessera.force_multi_extent=0
mount -t tessera /dev/$MD /mnt/tessera

echo "--- pack-creation messages ---"
dmesg | grep -E "pack —|multi-alloc|pack_alloc_and_write entry" | tail -10

echo "--- trigger repack ---"
sysctl kern.tessera.repack_one=1
rc=$?
echo "rc=$rc"

echo "--- repack messages ---"
dmesg | grep -iE "repack|pack —" | tail -10

echo "=== verify content unchanged ==="
fail=0
cd /mnt/tessera
for i in 1 2 3; do
    orig=$(cat /tmp/sha.$i)
    new=$(sha256 -q file$i)
    if [ "$orig" != "$new" ]; then
        echo "FAIL: file$i $orig -> $new"
        fail=1
    else
        echo "ok: file$i"
    fi
done
cd /
umount /mnt/tessera

echo "=== mount #3 — verify on-disk ==="
mount -t tessera /dev/$MD /mnt/tessera
cd /mnt/tessera
for i in 1 2 3; do
    orig=$(cat /tmp/sha.$i)
    new=$(sha256 -q file$i)
    if [ "$orig" != "$new" ]; then
        echo "FAIL after remount: file$i"
        fail=1
    else
        echo "ok after remount: file$i"
    fi
done
cd /
umount /mnt/tessera
mdconfig -d -u 0
if [ $fail -eq 0 ]; then echo DONE; else echo FAILED; exit 1; fi
