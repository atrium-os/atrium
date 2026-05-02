#!/bin/sh
# Throughput benchmark — Tessera vs ZFS on the same md-vnode-backed
# image, across the workloads that exercise the dirty-content
# coalescing path, the chunked path, and the CAS read cache.
#
# Reports BOTH cold (iter 1) and steady-state (iters 2-5 mean).
# Cold-start is naturally worse for tessera (~5x cold/warm) because
# more first-time-init structures get touched on iter 1; ZFS shows
# similar but smaller cold/warm gap (~2x).
#
# Steady-state numbers are the ones to focus on for real-world
# expectation. As of 2026-05-03 (commit 03b499e) tessera matches or
# beats ZFS on multi-write fsync workloads (4KB×256, 4KB×1024,
# 1M×N) and trails on lone-fsync small-data cases (256K×1, 1M×1).
set -u

BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

umount /mnt/tessera 2>/dev/null || true
umount /mnt/zfs 2>/dev/null || true
zpool destroy ztest 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
mdconfig -d -u 2 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko 2>/dev/null

# ZFS pool on a fresh 256 MiB image, compression off so dd-of-random
# isn't artificially deflated by zero-block detection.
rm -f /tmp/bench_zfs.img
truncate -s 256M /tmp/bench_zfs.img
MDZ=$(mdconfig -a -t vnode -f /tmp/bench_zfs.img)
zpool create -f -O compression=off -m /mnt/zfs ztest /dev/$MDZ

# Tessera mount on 64 MiB image
$BIN/mkfs-tessera --create -s 64 /tmp/bench_t.img >/dev/null
MDT=$(mdconfig -a -t vnode -f /tmp/bench_t.img)
mount -t tessera /dev/$MDT /mnt/tessera

# 8 MiB random buffer (worst case input — defeats compression /
# zero-detection on either FS).
dd if=/dev/random of=/tmp/bench_rand.bin bs=1M count=8 2>/dev/null

# Run dd → return MB/s as a number.
mbps() {
    rm -f $1/f
    dd if=/tmp/bench_rand.bin of=$1/f bs=$2 count=$3 $4 2>&1 \
        | tail -1 \
        | awk -F'[(]' '{print $2}' \
        | awk '{printf "%.1f", $1/1048576}'
}

# Run a workload N times against both FSes; report iter1 and steady
# (mean of remaining iters).
sweep() {
    label=$1; bs=$2; count=$3; flags=$4
    z1="" t1=""
    z_steady="0" t_steady="0"
    n_steady=0
    for i in 1 2 3 4 5; do
        z=$(mbps /mnt/zfs $bs $count "$flags")
        t=$(mbps /mnt/tessera $bs $count "$flags")
        if [ $i -eq 1 ]; then
            z1=$z
            t1=$t
        else
            z_steady=$(awk "BEGIN{printf \"%.1f\", $z_steady + $z}")
            t_steady=$(awk "BEGIN{printf \"%.1f\", $t_steady + $t}")
            n_steady=$((n_steady + 1))
        fi
    done
    z_avg=$(awk "BEGIN{printf \"%.1f\", $z_steady / $n_steady}")
    t_avg=$(awk "BEGIN{printf \"%.1f\", $t_steady / $n_steady}")
    ratio_cold=$(awk "BEGIN{printf \"%.2f\", $t1 / $z1}")
    ratio_warm=$(awk "BEGIN{printf \"%.2f\", $t_avg / $z_avg}")
    printf "  %-22s  cold:  ZFS=%6s  T=%6s  T/Z=%4s    steady:  ZFS=%6s  T=%6s  T/Z=%4s\n" \
        "$label" "$z1" "$t1" "$ratio_cold" "$z_avg" "$t_avg" "$ratio_warm"
}

echo "=== Tessera vs ZFS throughput sweep (random data, dd conv=fsync) ==="
echo "    All numbers MB/s."
echo
sweep "4KB × 64"            4k    64   conv=fsync
sweep "4KB × 256 (P1)"      4k    256  conv=fsync
sweep "4KB × 1024 (4 MiB)"  4k    1024 conv=fsync
sweep "256K × 1"            256k  1    conv=fsync
sweep "1M × 1"              1M    1    conv=fsync
sweep "1M × 4 (4 MiB)"      1M    4    conv=fsync
sweep "1M × 8 (8 MiB)"      1M    8    conv=fsync

echo
echo "=== Read-heavy: 100 small files × 5 rounds ==="
# Create files
for i in $(seq 1 100); do
    echo "content of file $i with some bytes" > /mnt/tessera/r$i
    echo "content of file $i with some bytes" > /mnt/zfs/r$i
done
sync

for fs_name in ZFS Tessera; do
    case $fs_name in
        ZFS)     mount=/mnt/zfs ;;
        Tessera) mount=/mnt/tessera ;;
    esac
    times=""
    for round in 1 2 3 4 5; do
        t0=$(date +%s%N)
        for i in $(seq 1 100); do cat $mount/r$i > /dev/null; done
        t1=$(date +%s%N)
        times="$times $(( (t1 - t0) / 1000000 ))"
    done
    printf "  %-10s  per-round ms: %s\n" "$fs_name" "$times"
done

echo
echo "=== Tessera CAS-cache + dirty-content stats ==="
sysctl kern.tessera.cas_loc_hits kern.tessera.cas_loc_misses \
       kern.tessera.cas_byte_hits kern.tessera.cas_byte_misses \
       kern.tessera.dirty_content_hits kern.tessera.dirty_content_creates \
       kern.tessera.dirty_content_flushes 2>/dev/null

zpool destroy ztest
mdconfig -d -u "$MDZ"
umount /mnt/tessera
mdconfig -d -u "$MDT"
echo
echo "DONE"
