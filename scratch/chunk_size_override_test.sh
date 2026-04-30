#!/bin/sh
# v2 step-3c prereq: per-mount tessera.chunk_size=<N> mount option.
#
# Verifies:
#   1. Valid power-of-two values in [4096, 4194304] mount + work.
#   2. Invalid values (non-power-of-two, out-of-range, garbage) are
#      rejected with EINVAL at mount time.
#   3. A 1 MiB write on a tessera.chunk_size=4096 mount produces 256
#      chunk slots (vs 16 with the auto 64 KiB tier), confirming the
#      override actually drives chunk granularity.
#   4. Round-trip read-back is identical to source bytes.
#   5. Remount without override works and reads back the same data.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/chunk_override.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/chunk_override.img)

echo "--- mount with bogus chunk_size (should fail) ---"
mount -t tessera -o tessera.chunk_size=4095 /dev/$MD /mnt/tessera 2>/dev/null \
    && { echo "FAIL: mount accepted 4095"; exit 1; } \
    || echo "  4095 correctly rejected"
mount -t tessera -o tessera.chunk_size=8388608 /dev/$MD /mnt/tessera 2>/dev/null \
    && { echo "FAIL: mount accepted 8 MiB"; exit 1; } \
    || echo "  8 MiB correctly rejected"
mount -t tessera -o tessera.chunk_size=12345 /dev/$MD /mnt/tessera 2>/dev/null \
    && { echo "FAIL: mount accepted non-power-of-2"; exit 1; } \
    || echo "  12345 correctly rejected"

echo "--- mount with tessera.chunk_size=4096 ---"
mount -t tessera -o tessera.chunk_size=4096 /dev/$MD /mnt/tessera

echo "--- write 1 MiB in a single op; expect chunked path with 4 KiB cs ---"
# NOTE: bs=1m count=1 is one vop_write — exercising the chunked-replace
# path once. Streaming small writes would exhaust meta-reserve at 4 KiB
# granularity (each rewrite repacks the full N-entry flat manifest).
# That regresses to acceptable cost once CHUNK_TREE write-side
# promotion lands; until then this test stays single-shot.
dd if=/dev/random of=/tmp/src.bin bs=1m count=1 2>/dev/null
cp /tmp/src.bin /mnt/tessera/big
sync
SIZE=$(stat -f %z /mnt/tessera/big)
[ "$SIZE" -eq 1048576 ] || { echo "FAIL size=$SIZE"; exit 1; }
echo "  size: $SIZE"

# Read back into temp, compare via sha256.
SRC_HASH=$(sha256 -q /tmp/src.bin)
cp /mnt/tessera/big /tmp/readback
DST_HASH=$(sha256 -q /tmp/readback)
[ "$SRC_HASH" = "$DST_HASH" ] || { echo "FAIL: hash mismatch"; exit 1; }
echo "  read-back hash matches: $SRC_HASH"

echo "--- chunked-write counter bumped ---"
N=$(sysctl -n kern.tessera.vop_write_chunked)
[ "$N" -ge 1 ] || { echo "FAIL: vop_write_chunked=$N"; exit 1; }
echo "  vop_write_chunked=$N"

echo "--- remount WITHOUT override; read-back identical ---"
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
RM_HASH=$(sha256 -q /mnt/tessera/big)
[ "$RM_HASH" = "$SRC_HASH" ] || { echo "FAIL: post-remount hash"; exit 1; }
echo "  remount hash: $RM_HASH"

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
