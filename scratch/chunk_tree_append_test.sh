#!/bin/sh
# v2 step-3c follow-up: append-into-CHUNK_TREE fast-path.
#
# Verifies:
#   1. A small append to a CHUNK_TREE file produces a new CHUNK_TREE
#      manifest (chunk_tree_publish bumps) without reverting to flat.
#   2. Read-back hash equals concatenation of pre-existing bytes +
#      appended bytes.
#   3. Append that overflows the current tail group spawns a new
#      group (chunk_tree_publish bumps; no slow-path fallback).
#   4. Multiple sequential small appends (log-style) all stay on the
#      fast-path.
#   5. Remount + read still correct.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/ctappend.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/ctappend.img)
mount -t tessera -o tessera.chunk_size=4096 /dev/$MD /mnt/tessera

echo "--- 1. seed file as CHUNK_TREE (2 MiB at cs=4 KiB) ---"
dd if=/dev/random of=/tmp/seed.bin bs=1m count=2 2>/dev/null
dd if=/tmp/seed.bin of=/mnt/tessera/log bs=2m count=1 conv=sync 2>/dev/null
sync
PUB1=$(sysctl -n kern.tessera.chunk_tree_publish)
echo "  chunk_tree_publish after seed: $PUB1"

echo "--- 2. append 16 bytes (in-tail-group, partial-chunk merge) ---"
PUB_BEFORE=$(sysctl -n kern.tessera.chunk_tree_publish)
FALLBACK_BEFORE=$(sysctl -n kern.tessera.append_fast_fallback)
printf 'log-line-1234567' >> /mnt/tessera/log
sync
PUB_AFTER=$(sysctl -n kern.tessera.chunk_tree_publish)
FALLBACK_AFTER=$(sysctl -n kern.tessera.append_fast_fallback)
DELTA=$((PUB_AFTER - PUB_BEFORE))
FB_DELTA=$((FALLBACK_AFTER - FALLBACK_BEFORE))
echo "  chunk_tree_publish delta: $DELTA (expected ≥ 1)"
echo "  append_fast_fallback delta: $FB_DELTA (expected 0)"
[ "$DELTA" -ge 1 ] || { echo "FAIL: tree-append did not promote"; exit 1; }
[ "$FB_DELTA" -eq 0 ] || { echo "FAIL: fell back to slow path"; exit 1; }

# Verify content.
cp /tmp/seed.bin /tmp/expected
printf 'log-line-1234567' >> /tmp/expected
EXP_HASH=$(sha256 -q /tmp/expected)
GOT_HASH=$(sha256 -q /mnt/tessera/log)
[ "$EXP_HASH" = "$GOT_HASH" ] || { echo "FAIL: content mismatch"; exit 1; }
echo "  content hash matches"

echo "--- 3. append crossing group boundary (~ 1 MiB to spawn new group) ---"
PUB_BEFORE=$(sysctl -n kern.tessera.chunk_tree_publish)
FALLBACK_BEFORE=$(sysctl -n kern.tessera.append_fast_fallback)
dd if=/dev/random of=/tmp/big_app.bin bs=1m count=1 2>/dev/null
cat /tmp/big_app.bin >> /mnt/tessera/log
sync
PUB_AFTER=$(sysctl -n kern.tessera.chunk_tree_publish)
FALLBACK_AFTER=$(sysctl -n kern.tessera.append_fast_fallback)
DELTA=$((PUB_AFTER - PUB_BEFORE))
FB_DELTA=$((FALLBACK_AFTER - FALLBACK_BEFORE))
echo "  chunk_tree_publish delta: $DELTA (expected ≥ 1)"
echo "  append_fast_fallback delta: $FB_DELTA (expected 0)"
[ "$DELTA" -ge 1 ] || { echo "FAIL"; exit 1; }
[ "$FB_DELTA" -eq 0 ] || { echo "FAIL: fell back"; exit 1; }

cat /tmp/big_app.bin >> /tmp/expected
EXP_HASH=$(sha256 -q /tmp/expected)
GOT_HASH=$(sha256 -q /mnt/tessera/log)
[ "$EXP_HASH" = "$GOT_HASH" ] || { echo "FAIL: content"; exit 1; }
echo "  content matches after spillover"

echo "--- 4. 5x sequential small appends — log-style workload ---"
FB_BEFORE=$(sysctl -n kern.tessera.append_fast_fallback)
for i in 1 2 3 4 5; do
    printf "entry $i\n" >> /mnt/tessera/log
    printf "entry $i\n" >> /tmp/expected
done
sync
FB_AFTER=$(sysctl -n kern.tessera.append_fast_fallback)
FB_DELTA=$((FB_AFTER - FB_BEFORE))
echo "  append_fast_fallback delta: $FB_DELTA (expected 0)"
[ "$FB_DELTA" -eq 0 ] || { echo "FAIL: fell back during log appends"; exit 1; }

EXP_HASH=$(sha256 -q /tmp/expected)
GOT_HASH=$(sha256 -q /mnt/tessera/log)
[ "$EXP_HASH" = "$GOT_HASH" ] || { echo "FAIL: log content"; exit 1; }
echo "  log content matches"

echo "--- 5. remount + verify ---"
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
GOT_HASH=$(sha256 -q /mnt/tessera/log)
[ "$EXP_HASH" = "$GOT_HASH" ] || { echo "FAIL: post-remount"; exit 1; }
echo "  remount hash matches"

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
