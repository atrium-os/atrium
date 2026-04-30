#!/bin/sh
# v2 step-3c: CHUNK_TREE write-side promotion.
#
# With tessera.chunk_size=4096 and a 2 MiB file (= 512 chunks > 256
# fanout), the write path must:
#   1. Build a CHUNK_TREE outer manifest (verified via
#      kern.tessera.chunk_tree_publish counter).
#   2. Round-trip read identical bytes via the existing
#      read_into_uio recursion.
#   3. Survive an unmount/remount cycle.
#
# Sub-fanout case (1 MiB / 4 KiB = 256 chunks, ≤ fanout) stays on the
# flat CHUNK_LIST path — guard it with the existing
# vop_write_chunked counter.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/ctree.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/ctree.img)
mount -t tessera -o tessera.chunk_size=4096 /dev/$MD /mnt/tessera

PUB_BEFORE=$(sysctl -n kern.tessera.chunk_tree_publish)

echo "--- 1. write 2 MiB at 4 KiB cs (= 512 chunks → CHUNK_TREE) ---"
# Use dd directly (single write_resid = 2 MiB) so we exercise the
# replace_content_chunked path. cp does staged writes which take the
# append fast-path; that path now correctly bails to slow when chunk
# count would exceed fanout, but a single-shot write keeps the test
# tighter on the promotion logic itself.
dd if=/dev/random of=/tmp/big.bin bs=1m count=2 2>/dev/null
SRC_HASH=$(sha256 -q /tmp/big.bin)
dd if=/tmp/big.bin of=/mnt/tessera/big bs=2m count=1 conv=sync 2>/dev/null
sync

PUB_AFTER=$(sysctl -n kern.tessera.chunk_tree_publish)
DELTA=$((PUB_AFTER - PUB_BEFORE))
echo "  chunk_tree_publish delta: $DELTA (expected ≥ 1)"
[ "$DELTA" -ge 1 ] || { echo "FAIL: CHUNK_TREE not triggered"; exit 1; }

echo "--- 2. read-back hash matches source ---"
DST_HASH=$(sha256 -q /mnt/tessera/big)
[ "$SRC_HASH" = "$DST_HASH" ] || { echo "FAIL: hash mismatch"; exit 1; }
echo "  hash: $SRC_HASH"

echo "--- 3. partial-range read (mid-file, crossing group boundary) ---"
# Group boundary at chunk 256 = byte 1048576. Read 8 KiB straddling it
# (chunks 255 + 256 — last of group 0, first of group 1).
# Use bs=4096 iseek=255 (block-aligned) — bs=1 with 8192-iter loops
# masks the actual VFS read behaviour with single-byte syscall noise.
dd if=/mnt/tessera/big of=/tmp/mid.bin bs=4096 iseek=255 count=2 2>/dev/null
dd if=/tmp/big.bin    of=/tmp/mid.ref bs=4096 iseek=255 count=2 2>/dev/null
cmp /tmp/mid.bin /tmp/mid.ref || { echo "FAIL: mid-file mismatch"; exit 1; }
echo "  mid-file 8 KiB straddling group boundary OK"

echo "--- 4. remount, verify CHUNK_TREE read still works ---"
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
RM_HASH=$(sha256 -q /mnt/tessera/big)
[ "$RM_HASH" = "$SRC_HASH" ] || { echo "FAIL: remount hash"; exit 1; }
echo "  remount hash: $RM_HASH"

echo "--- 5. small file stays flat CHUNK_LIST (sanity) ---"
umount /mnt/tessera
mount -t tessera -o tessera.chunk_size=4096 /dev/$MD /mnt/tessera
PUB_BEFORE=$(sysctl -n kern.tessera.chunk_tree_publish)
# 1 MiB / 4 KiB = 256 chunks ≤ fanout → flat.
dd if=/dev/random of=/tmp/small.bin bs=1m count=1 2>/dev/null
cp /tmp/small.bin /mnt/tessera/small
sync
PUB_AFTER=$(sysctl -n kern.tessera.chunk_tree_publish)
DELTA=$((PUB_AFTER - PUB_BEFORE))
echo "  chunk_tree_publish delta: $DELTA (expected 0)"
[ "$DELTA" -eq 0 ] || { echo "FAIL: 256-chunk file promoted spuriously"; exit 1; }

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
