#!/bin/sh
# Repack engine slice 3 (C) — auto-trigger tests:
#   (a) background trigger: repack_threshold=2, write enough multi-extent
#       packs to exceed it, observe taskqueue self-drains them.
#   (b) mount-time safety net: write many multi-extent packs, unmount,
#       lower repack_severe_threshold so remount fires the mount-time
#       pass; observe synchronous "mount-time repack" message.
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/repack.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/repack.img)

echo "=== (a) background trigger ==="
dmesg -c >/dev/null
sysctl kern.tessera.repack_threshold=2 \
       kern.tessera.repack_severe_threshold=10000
mount -t tessera /dev/$MD /mnt/tessera
sysctl kern.tessera.force_multi_extent=1
cd /mnt/tessera
i=0
while [ $i -lt 8 ]; do
    dd if=/dev/random of=fa$i bs=4096 count=2 2>/dev/null
    i=$((i + 1))
done
cd /
# Keep force=1 through umount so the drain creates MULTI_EXTENT packs.
umount /mnt/tessera
sysctl kern.tessera.force_multi_extent=0
mount -t tessera /dev/$MD /mnt/tessera

# After remount: count walked, and any subsequent mark_dirty above the
# threshold should arm the background task. Force one mark_dirty by
# touching a file.
cd /mnt/tessera
touch trigger
cd /
sleep 2
echo "--- background pass stats ---"
sysctl kern.tessera.repack_total_packs kern.tessera.repack_last_packs \
    kern.tessera.repack_last_time_ms

echo "--- recent dmesg ---"
dmesg | grep -E "MULTI_EXTENT|repack|mount-time" | tail -20

umount /mnt/tessera

echo "=== (b) mount-time safety net ==="
# Force a build-up: lots of multi-extent packs, then mount with
# severe_threshold below the count.
sysctl kern.tessera.repack_threshold=10000   # disable background
sysctl kern.tessera.repack_severe_threshold=10000
mount -t tessera /dev/$MD /mnt/tessera
sysctl kern.tessera.force_multi_extent=1
cd /mnt/tessera
i=0
while [ $i -lt 15 ]; do
    dd if=/dev/random of=fb$i bs=4096 count=2 2>/dev/null
    i=$((i + 1))
done
cd /
umount /mnt/tessera
sysctl kern.tessera.force_multi_extent=0

# Now mount with a low severe threshold.
sysctl kern.tessera.repack_severe_threshold=3
dmesg -c >/dev/null
mount -t tessera /dev/$MD /mnt/tessera
echo "--- mount messages ---"
dmesg | grep -E "MULTI_EXTENT|mount-time repack|repack pass" | tail -10
umount /mnt/tessera
mdconfig -d -u 0
echo DONE
