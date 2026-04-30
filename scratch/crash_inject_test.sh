#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

# Clear dmesg so subsequent greps show only this run's messages.
dmesg -c >/dev/null

$BIN/mkfs-tessera --create -s 16 --seed-file hello --seed-inode 1000 \
    --seed-content "Hello, Tessera!" /tmp/test.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera

# Use single-commit mutations (chmod, touch) so we don't accidentally
# overwrite the simulated-crashed SB with a successor commit's SB write.

echo "--- baseline mode and gen ---"
ls -la /mnt/tessera/hello
chmod 0644 /mnt/tessera/hello
ls -la /mnt/tessera/hello
dmesg | grep -E "mounted gen=|GC" | tail -3

echo "--- arm crash-injection sysctl ---"
sysctl kern.tessera.skip_next_sb=1
echo "--- chmod 0600 — its commit_sb will skip the SB write ---"
chmod 0600 /mnt/tessera/hello

echo "--- in-memory mountpoint view (mode 0600) ---"
ls -la /mnt/tessera/hello

# Hard umount + mdconfig -d. mountpoint state is gone; on-disk SB
# still says mode=0644 but journal has the gen=N+1 record with the
# new inode_root pointing at the 0600 inode tree.
umount /mnt/tessera
mdconfig -d -u 0

echo "--- remount; replay should roll forward to mode 0600 ---"
MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera
ls -la /mnt/tessera/hello
echo "--- dmesg replay messages ---"
dmesg | grep -E "crash-injection|rolled forward|replay" | tail -5

umount /mnt/tessera
mdconfig -d -u 0
kldunload tessera_fs
echo DONE
