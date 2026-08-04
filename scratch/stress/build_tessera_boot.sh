#!/bin/sh
# Build a self-contained Tessera boot+root partition on vtbd2p1:
#   /boot/laminar/kernel        (the kernel, loaded by the tessera loader)
#   /boot/modules/tessera_fs.ko (so the kernel can mount the tessera root)
#   /boot/loader.conf           (autoboot config)
#   /rescue + /bin/sh + /etc    (single-user root)
# Runs INSIDE the VM. The loader reads the kernel from here via tessera_fsops;
# the kernel roots here via vfs.root.mountfrom=tessera:/dev/vtbd2p1.
set -e
DEV=/dev/vtbd2p1
MNT=/mnt/tboot
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

kldstat -q -n tessera_fs || kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
umount $MNT 2>/dev/null || true

echo "=== mkfs tessera on $DEV ==="
$BIN/mkfs-tessera --create -s 2900 $DEV >/dev/null 2>&1
mkdir -p $MNT
mount -t tessera $DEV $MNT

echo "=== /boot (kernel + tessera_fs.ko + loader.conf) ==="
mkdir -p $MNT/boot/laminar $MNT/boot/modules
cp /boot/laminar/kernel $MNT/boot/laminar/kernel
cp /boot/modules/tessera_fs.ko $MNT/boot/modules/tessera_fs.ko
cat > $MNT/boot/loader.conf <<'EOF'
kernel="laminar"
tessera_fs_load="YES"
tessera_fs_name="/boot/modules/tessera_fs.ko"
vfs.root.mountfrom="tessera:/dev/vtbd2p1"
vfs.root.mountfrom.options="rw"
init_path="/rescue/init"
boot_single="YES"
autoboot_delay="3"
EOF

echo "=== root (/rescue + /bin/sh + /etc + dirs) ==="
cp -a /rescue $MNT/rescue
mkdir -p $MNT/bin $MNT/sbin $MNT/dev $MNT/etc $MNT/tmp $MNT/root
cp /rescue/sh $MNT/bin/sh
cp /rescue/init $MNT/sbin/init
printf "TESSERA-NATIVE-BOOT-OK\n" > $MNT/ZFSFREE_MARKER

echo "=== contents ==="
ls -la $MNT
ls -la $MNT/boot/laminar/kernel $MNT/boot/modules/tessera_fs.ko
echo "=== unmount + fsck ==="
umount $MNT
$BIN/tessera-fsck $DEV | tail -4
echo "=== DONE build_tessera_boot ==="
