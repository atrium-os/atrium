#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/gc.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/gc.img)
mount -t tessera /dev/$MD /mnt/tessera

# Create files for each worker
for i in $(jot 8); do
    echo "x" > /mnt/tessera/f$i
done

WAIT_BEFORE=$(sysctl -n kern.tessera.fsync_group_wait)
COMMITS_BEFORE=$(sysctl -n kern.tessera.sb_commits)

echo "--- 8 concurrent writers + fsyncs ---"
for i in $(jot 8); do
    (
        echo "writer-$i payload" > /mnt/tessera/f$i
        fsync /mnt/tessera/f$i
    ) &
done
wait

WAIT_AFTER=$(sysctl -n kern.tessera.fsync_group_wait)
COMMITS_AFTER=$(sysctl -n kern.tessera.sb_commits)
echo "fsync_group_wait delta: $((WAIT_AFTER - WAIT_BEFORE))"
echo "sb_commits delta:       $((COMMITS_AFTER - COMMITS_BEFORE))"
echo "(8 fsyncs; without group-commit each would commit once = 8;"
echo " with group-commit one commits, the rest wait → wait>=1 expected)"

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
