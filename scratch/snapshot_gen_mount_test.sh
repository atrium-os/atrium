#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
umount /mnt/snap 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

mkdir -p /mnt/snap
$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x \
    /tmp/gen.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/gen.img)

echo "--- gen 2: write 'v1', umount ---"
mount -t tessera /dev/$MD /mnt/tessera
echo "v1 content" > /mnt/tessera/log
umount /mnt/tessera

echo "--- gen 3: overwrite to 'v2', umount ---"
mount -t tessera /dev/$MD /mnt/tessera
echo "v2 content (newer)" > /mnt/tessera/log
umount /mnt/tessera

echo "--- gen 4: overwrite to 'v3', umount ---"
mount -t tessera /dev/$MD /mnt/tessera
echo "v3 final" > /mnt/tessera/log
umount /mnt/tessera

echo "--- live mount: see v3 ---"
mount -t tessera /dev/$MD /mnt/tessera
cat /mnt/tessera/log
umount /mnt/tessera

echo "--- forensic mount of gen=2 (read-only) ---"
mount -t tessera -o tessera.gen=2 /dev/$MD /mnt/snap
GEN2_LOG=$(cat /mnt/snap/log)
mount | grep tessera
umount /mnt/snap
# Slice-4 fix: gen=2 forensic must return v1's content. Pre-slice-4
# this returned ENOENT (the original v2 bug) because mid-session COWs
# in later sessions recycled gen=2's btree node sectors.
[ "$GEN2_LOG" = "v1 content" ] || \
    { echo "FAIL: gen=2 forensic returned: $GEN2_LOG (want 'v1 content')"; exit 1; }
echo "  gen=2 → '$GEN2_LOG' OK"

echo "--- forensic mount of gen=3 ---"
mount -t tessera -o tessera.gen=3 /dev/$MD /mnt/snap
GEN3_LOG=$(cat /mnt/snap/log)
umount /mnt/snap
# gen=3's content is whatever the auto-snapshot at gen=3's commit_sb
# captured. With mount-time GC committing before user writes, gen=3
# may capture either v1 or v2 depending on commit ordering — verify
# only that the read succeeds (no EIO/ENOENT, the original bug).
echo "  gen=3 → '$GEN3_LOG' OK (content is commit-ordering dependent)"

echo "--- forensic mount of bogus gen=999 should fail (no-such-snapshot) ---"
# Pre-slice-4 + before the sb_a/sb_b double-free fix, this hung the VM.
# Now: btree_get returns ENOTFOUND → fail_close → ENOENT → mount fails
# cleanly without corrupting the malloc allocator.
mount -t tessera -o tessera.gen=999 /dev/$MD /mnt/snap 2>&1 \
    && { echo "FAIL: gen=999 should not have mounted"; exit 1; } \
    || echo "  gen=999 → ENOENT (rejected, as expected)"

mdconfig -d -u 0
echo DONE
