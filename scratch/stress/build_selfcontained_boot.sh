#!/bin/sh
# Build a SELF-CONTAINED Tessera boot disk on vtbd2 (tessera-root.img):
#   p1 = EFI System Partition (FAT): the tessera-enabled loader + loader.env
#        (currdev=disk0p2:) so the loader reads its config from the tessera
#        partition at startup.
#   p2 = Tessera: /boot (kernel + tessera_fs.ko + loader.conf) + /rescue root.
# Booted in a minimal QEMU with ONLY this disk (no ZFS anywhere), the loader
# reads the kernel from tessera and the kernel roots on tessera — no ZFS/UFS
# in the boot path. Runs INSIDE the dev VM.
set -e
D=vtbd2
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
LOADER=$(find /usr/obj -name loader_lua.efi | head -1)
test -n "$LOADER"

kldstat -q -n tessera_fs || kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
umount /mnt/esp2 2>/dev/null || true; umount /mnt/tp2 2>/dev/null || true

echo "=== partition $D: EFI(64M) + tessera(rest) ==="
gpart destroy -F $D 2>/dev/null || true
gpart create -s gpt $D
gpart add -t efi -s 64M -a 1m $D          # ${D}p1
gpart add -t freebsd-ufs -a 1m $D         # ${D}p2 (holds tessera)
gpart show $D

echo "=== ESP (p1): FAT + loader + loader.env ==="
newfs_msdos -F 32 -c 1 /dev/${D}p1 >/dev/null 2>&1
mkdir -p /mnt/esp2; mount -t msdosfs /dev/${D}p1 /mnt/esp2
mkdir -p /mnt/esp2/EFI/BOOT /mnt/esp2/efi/freebsd
cp "$LOADER" /mnt/esp2/EFI/BOOT/BOOTAA64.EFI
# currdev override read early (no ZFS present in the minimal VM to override it)
printf 'currdev="disk0p2:"\n' > /mnt/esp2/efi/freebsd/loader.env
ls -la /mnt/esp2/EFI/BOOT /mnt/esp2/efi/freebsd
umount /mnt/esp2

echo "=== tessera (p2): /boot + root ==="
$BIN/mkfs-tessera --create -s 2800 /dev/${D}p2 >/dev/null 2>&1
mkdir -p /mnt/tp2; mount -t tessera /dev/${D}p2 /mnt/tp2
# STANDARD layout: kernel at /boot/kernel/kernel so the loader's bootable-
# partition probe (which looks for /boot/kernel/kernel) picks disk0p2.
mkdir -p /mnt/tp2/boot/kernel
cp /boot/laminar/kernel /mnt/tp2/boot/kernel/kernel
cp /boot/modules/tessera_fs.ko /mnt/tp2/boot/kernel/tessera_fs.ko
cat > /mnt/tp2/boot/loader.conf <<'EOF'
kernel="kernel"
module_path="/boot/kernel"
tessera_fs_load="YES"
vfs.root.mountfrom="tessera:/dev/vtbd0p2"
vfs.root.mountfrom.options="rw"
init_path="/rescue/init"
boot_single="YES"
autoboot_delay="2"
EOF
# the lua loader needs its scripts + base config on the boot partition
cp -a /boot/lua /mnt/tp2/boot/lua
cp -a /boot/defaults /mnt/tp2/boot/defaults
cp -a /rescue /mnt/tp2/rescue
mkdir -p /mnt/tp2/bin /mnt/tp2/sbin /mnt/tp2/dev /mnt/tp2/etc /mnt/tp2/tmp /mnt/tp2/root
cp /rescue/sh /mnt/tp2/bin/sh
cp /rescue/init /mnt/tp2/sbin/init
printf "ZFS-FREE-TESSERA-BOOT-OK\n" > /mnt/tp2/ZFSFREE_MARKER
ls -la /mnt/tp2 /mnt/tp2/boot
umount /mnt/tp2
$BIN/tessera-fsck /dev/${D}p2 | tail -3
echo "=== DONE build_selfcontained_boot ==="
