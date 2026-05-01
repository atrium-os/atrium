#!/bin/sh
# Quick microbench specifically for the v2.5 BTREE fast paths.
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 1024 --seed-file h --seed-content x \
    /tmp/btree.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/btree.img)
mount -t tessera /dev/$MD /mnt/tessera
cd /mnt/tessera

for N in 100 500 1000; do
    echo "=== N=$N ==="
    rm -f f* g* 2>/dev/null
    sync
    t0=$(date +%s%N)
    i=0
    while [ $i -lt $N ]; do
        : > f$i
        i=$((i + 1))
    done
    t1=$(date +%s%N)
    ms=$(( (t1 - t0) / 1000000 ))
    avg=$(( ms / N ))
    echo "  touch: ${ms}ms total, avg ${avg}ms/op"
done

cd /
umount /mnt/tessera
mdconfig -d -u 0
