#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true

$BIN/mkfs-tessera --create -s 16 --seed-file hello --seed-inode 1000 \
    --seed-content "Hello, Tessera!" /tmp/test.img >/dev/null
echo "MKFS_OK"

# Session A: do 800 touches (well under the 1024-sector reserve so the
# bump pointer goes deep but doesn't exhaust). Recycler is in-memory so
# each commit reuses sectors freed by earlier commits.
MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera
i=0
while [ $i -lt 5000 ]; do
    touch /mnt/tessera/hello
    i=$((i + 1))
    [ $((i % 1000)) -eq 0 ] && echo "  A: $i"
done
echo "Session A: 5000 touches OK"

# Snapshot the on-disk meta_reserve_bump after umount.
umount /mnt/tessera
mdconfig -d -u 0
# meta_reserve_bump is at SB offset (in struct, see format.h). For a
# rough heuristic, dump the SB and look at non-zero bytes after the
# magic. Easier: read the SB length from sector 0 (the kmod will print
# the reclaimed count on remount).

# Session B: mount again. The on-disk bump pointer is high (since we
# didn't reset it across mounts). Without persistent recycling, we'd
# run out of bump space well before 800 more touches. With it, the
# walk-on-mount reconstructs the free list and we can keep going.
MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera
i=0
while [ $i -lt 5000 ]; do
    touch /mnt/tessera/hello
    i=$((i + 1))
    [ $((i % 1000)) -eq 0 ] && echo "  B: $i"
done
echo "Session B: 5000 more touches OK"

cat /mnt/tessera/hello
umount /mnt/tessera
mdconfig -d -u 0

echo "--- dmesg meta-reserve messages:"
dmesg | grep -E "meta-reserve|meta_reserve|reclaimed" | tail -5
echo "DONE"
