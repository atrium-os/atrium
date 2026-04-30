#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true

$BIN/mkfs-tessera --create -s 16 --seed-file hello --seed-inode 1000 \
    --seed-content "Hello, Tessera!" /tmp/test.img >/dev/null
echo "MKFS_OK"

# Backup pristine (gen=1) SBs.
dd if=/tmp/test.img of=/tmp/sb_backup.bin bs=4096 count=2 2>/dev/null
echo "PRISTINE SB byte 8 (gen LSB):"
dd if=/tmp/sb_backup.bin bs=4096 count=1 skip=0 2>/dev/null | hexdump -C | head -1

MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera
echo "Pre-mutation gen=1; running mutations..."
touch /mnt/tessera/hello
echo X > /mnt/tessera/foo
mkdir /mnt/tessera/sub
mv /mnt/tessera/foo /mnt/tessera/sub/foo
umount /mnt/tessera
mdconfig -d -u 0

echo "POST-MUTATION SB byte 8:"
dd if=/tmp/test.img bs=4096 count=1 skip=0 2>/dev/null | hexdump -C | head -1

# Restore the gen=1 SBs. Journal records still have the post-mutation gen.
echo "Restoring gen=1 SB to simulate crash before SB write..."
dd if=/tmp/sb_backup.bin of=/tmp/test.img bs=4096 count=2 conv=notrunc 2>/dev/null

echo "POST-RESTORE SB byte 8:"
dd if=/tmp/test.img bs=4096 count=1 skip=0 2>/dev/null | hexdump -C | head -1

# Mount with stale SB but live journal - replay should roll forward.
MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera
echo "MOUNT_OK"
echo "--- ls / cat (should show post-mutation state):"
ls /mnt/tessera/ /mnt/tessera/sub/
cat /mnt/tessera/sub/foo
echo "--- dmesg replay messages:"
dmesg | grep -E "tessera_fs:" | tail -8
umount /mnt/tessera
mdconfig -d -u 0
echo "DONE"
