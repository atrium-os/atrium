#!/bin/sh
# Microbench: per-op cost of touch / rename / unlink as the parent
# directory grows. Helps quantify how steep the O(parent-size) curve
# is for tessera's dirent_rewrite path.
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 1024 --seed-file h --seed-content x \
    /tmp/b.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/b.img)
mount -t tessera /dev/$MD /mnt/tessera
cd /mnt/tessera

for N in 100 500 1000; do
    echo "=== N=$N ==="
    rm -f f* g* 2>/dev/null
    sync
    t0=$(date +%s)
    i=0
    while [ $i -lt $N ]; do
        : > f$i
        i=$((i + 1))
    done
    t1=$(date +%s)
    echo "  touch x$N: $((t1 - t0))s"
    t0=$(date +%s)
    i=0
    while [ $i -lt $N ]; do
        mv f$i g$i
        i=$((i + 1))
    done
    t1=$(date +%s)
    echo "  mv    x$N: $((t1 - t0))s"
    t0=$(date +%s)
    i=0
    while [ $i -lt $N ]; do
        rm g$i
        i=$((i + 1))
    done
    t1=$(date +%s)
    echo "  rm    x$N: $((t1 - t0))s"
done

cd /
umount /mnt/tessera
mdconfig -d -u 0
