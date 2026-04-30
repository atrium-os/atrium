#!/bin/sh
# Long-running soak. Many cycles of mount/work/umount on the same
# image. Exposes slow leaks: malloc tags growing, vnode pressure,
# meta-reserve exhaustion that takes 100+ commits to surface.
#
# Each cycle does a mix of: small writes, a chunked write, some
# removes. The total ops per cycle stay small so individual cycles
# are quick; the goal is the count.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

CYCLES=${STRESS_SOAK_CYCLES:-100}

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 256 --seed-file h --seed-content x /tmp/soak.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/soak.img)

# Pre-create a 2 MiB random file we can re-write each cycle without
# allocating fresh data each time.
dd if=/dev/random of=/tmp/soak.payload bs=1m count=2 2>/dev/null

echo "--- $CYCLES soak cycles ---"
START=$(date +%s)
i=0
while [ $i -lt $CYCLES ]; do
    mount -t tessera /dev/$MD /mnt/tessera
    # Small file ops.
    echo "v$i" > /mnt/tessera/log
    mkdir -p /mnt/tessera/d$((i % 8))
    # Chunked write (forces CHUNK_LIST/CHUNK_TREE depending on size).
    cp /tmp/soak.payload /mnt/tessera/payload
    # Read-back to exercise the read path.
    cat /mnt/tessera/log >/dev/null
    sha256 -q /mnt/tessera/payload >/dev/null
    # Sometimes remove things.
    if [ $((i % 5)) -eq 0 ]; then
        rm -f /mnt/tessera/payload
    fi
    umount /mnt/tessera
    i=$((i + 1))
    if [ $((i % 10)) -eq 0 ]; then
        echo "  cycle $i / $CYCLES"
    fi
done
END=$(date +%s)

echo "  $CYCLES cycles in $((END - START)) s"

# Final integrity check — mount, list, ensure no panic.
mount -t tessera /dev/$MD /mnt/tessera
ls /mnt/tessera >/dev/null
umount /mnt/tessera
mdconfig -d -u 0

echo "--- counters ---"
sysctl kern.tessera.sb_commits kern.tessera.mark_dirty \
    kern.tessera.dirty_drained kern.tessera.pending_drained \
    kern.tessera.snapshots_retired \
    2>&1 | sed 's/^/  /'

echo DONE
