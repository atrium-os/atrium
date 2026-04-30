#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true

$BIN/mkfs-tessera --create -s 16 --seed-file hello --seed-inode 1000 \
    --seed-content "Hello, Tessera!" /tmp/test.img >/dev/null
echo "MKFS_OK"

MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera

# Burn through 5000 mutations. Without journal checkpoint we'd fail at
# ~80 commits; without metadata-reserve recycling we'd fail at ~1024.
i=0
while [ $i -lt 5000 ]; do
    touch /mnt/tessera/hello
    i=$((i + 1))
    [ $((i % 500)) -eq 0 ] && echo "  touched $i times"
done
echo "5000 touches OK"

# Mix in some other ops.
echo X > /mnt/tessera/foo
mkdir /mnt/tessera/sub
mv /mnt/tessera/foo /mnt/tessera/sub/foo
echo "Mixed ops OK"

# Remount + verify state survives.
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
ls /mnt/tessera/ /mnt/tessera/sub/
cat /mnt/tessera/sub/foo
echo "Remount OK"

umount /mnt/tessera
mdconfig -d -u 0
echo "DONE"
