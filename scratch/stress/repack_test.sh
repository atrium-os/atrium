#!/bin/sh
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
kldstat -q -n tessera_fs || kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
umount /mnt/rp 2>/dev/null; mdconfig -d -u 9 2>/dev/null
rm -f /tmp/rp.img; $BIN/mkfs-tessera --create -s 128 /tmp/rp.img >/dev/null
mdconfig -a -t vnode -u 9 -f /tmp/rp.img >/dev/null; mkdir -p /mnt/rp
mount -t tessera /dev/md9 /mnt/rp
# seed verifiable content
mkdir -p /mnt/rp/data
i=0; while [ $i -lt 30 ]; do dd if=/dev/random of=/mnt/rp/data/keep$i bs=1024 count=$((i+1)) 2>/dev/null; i=$((i+1)); done
sync
# checksum the seeded files (to prove content survives the repack)
( cd /mnt/rp && find data -type f | sort | xargs md5 ) > /tmp/rp_before.md5 2>/dev/null || \
  ( cd /mnt/rp && find data -type f | sort | xargs md5sum ) > /tmp/rp_before.md5 2>/dev/null
# churn to build snapshots + advance the bump
i=0; while [ $i -lt 400 ]; do echo c$i > /mnt/rp/churn$((i%8)); rm -f /mnt/rp/churn$((i%8)); sync; i=$((i+1)); done
sync; umount /mnt/rp; mdconfig -d -u 9

echo "=== BEFORE: dry-run report ==="
$BIN/tessera-repack /tmp/rp.img

echo "=== fsck before repack ==="
$BIN/tessera-fsck /tmp/rp.img 2>&1 | grep -E "result"

echo "=== REPACK -y ==="
$BIN/tessera-repack -y /tmp/rp.img

echo "=== fsck after repack ==="
$BIN/tessera-fsck /tmp/rp.img 2>&1 | grep -E "generation|result"

echo "=== remount in kmod + verify files intact ==="
mdconfig -a -t vnode -u 9 -f /tmp/rp.img >/dev/null
mount -t tessera /dev/md9 /mnt/rp
( cd /mnt/rp && find data -type f | sort | xargs md5 ) > /tmp/rp_after.md5 2>/dev/null || \
  ( cd /mnt/rp && find data -type f | sort | xargs md5sum ) > /tmp/rp_after.md5 2>/dev/null
umount /mnt/rp; mdconfig -d -u 9
if diff -q /tmp/rp_before.md5 /tmp/rp_after.md5 >/dev/null; then echo "FILES: intact (checksums match, $(wc -l </tmp/rp_before.md5 | tr -d ' ') files)"; else echo "FILES: MISMATCH"; diff /tmp/rp_before.md5 /tmp/rp_after.md5 | head; fi
echo REPACK_TEST_DONE
