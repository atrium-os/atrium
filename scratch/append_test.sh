#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/append.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/append.img)
mount -t tessera /dev/$MD /mnt/tessera

echo "--- seed a 1 MiB chunked file ---"
dd if=/dev/urandom of=/tmp/base bs=1024 count=1024 2>/dev/null
cp /tmp/base /mnt/tessera/log
df -k /mnt/tessera | tail -1
COMMITS_BEFORE=$(sysctl -n kern.tessera.sb_commits)
DIRTY_BEFORE=$(sysctl -n kern.tessera.mark_dirty)

echo "--- append 10 lines (1 KiB each) — fast path should engage ---"
USED_BEFORE=$(df -k /mnt/tessera | tail -1 | awk '{print $3}')
for i in $(jot 10); do
    dd if=/dev/urandom bs=1024 count=1 2>/dev/null >> /mnt/tessera/log
done
USED_AFTER=$(df -k /mnt/tessera | tail -1 | awk '{print $3}')
echo "delta KiB: $((USED_AFTER - USED_BEFORE)) (slow path would be ~1.6 MiB+; fast path expects <200 KiB)"
COMMITS_AFTER=$(sysctl -n kern.tessera.sb_commits)
DIRTY_AFTER=$(sysctl -n kern.tessera.mark_dirty)
echo "commits delta: $((COMMITS_AFTER - COMMITS_BEFORE))"
echo "mark_dirty delta: $((DIRTY_AFTER - DIRTY_BEFORE))"

echo "--- read-back size + tail check ---"
SIZE=$(stat -f %z /mnt/tessera/log)
echo "size: $SIZE bytes (expect 1048576 + 10*1024 = 1058816)"
[ "$SIZE" -eq 1058816 ] && echo "size OK"

echo "--- compare prefix to the original 1 MiB ---"
dd if=/mnt/tessera/log of=/tmp/check_prefix bs=1024 count=1024 2>/dev/null
cmp /tmp/base /tmp/check_prefix && echo "prefix unchanged"

echo "--- remount, verify ---"
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
SIZE=$(stat -f %z /mnt/tessera/log)
[ "$SIZE" -eq 1058816 ] && echo "remount size OK"
dd if=/mnt/tessera/log of=/tmp/check_prefix2 bs=1024 count=1024 2>/dev/null
cmp /tmp/base /tmp/check_prefix2 && echo "remount prefix OK"

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
