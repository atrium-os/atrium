#!/bin/sh
# Build a FULL MULTIUSER ZFS-free Tessera boot disk on vtbd2 (tessera-root.img).
# No buildworld available (only the kernel is built in /usr/obj), so lay down a
# base by selectively copying the dev VM's own working userland (excluding the
# Atrium jail/local/src/obj cruft) plus a clean /etc overlay. rc(8) then runs
# organically: rc.d/root does `mount -uw /` (exercising our ro->rw upgrade),
# base services start, and boot reaches a multiuser login on the serial console.
# Runs INSIDE the dev VM.
set -e
D=vtbd2
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
KMOD=/mnt/host/atrium-tessera/kmod/tessera_fs.ko
LOADER=$(find /usr/obj -name loader_lua.efi | head -1)
test -n "$LOADER"; test -e "$KMOD"
kldstat -q -n tessera_fs || kldload "$KMOD"
umount /mnt/esp2 2>/dev/null || true; umount /mnt/tp2 2>/dev/null || true
T=/mnt/tp2

echo "=== partition $D: EFI(64M) + tessera(rest) ==="
gpart destroy -F $D 2>/dev/null || true
gpart create -s gpt $D
gpart add -t efi -s 64M -a 1m $D
gpart add -t freebsd-ufs -a 1m $D

echo "=== ESP (p1) ==="
newfs_msdos -F 32 -c 1 /dev/${D}p1 >/dev/null 2>&1
mkdir -p /mnt/esp2; mount -t msdosfs /dev/${D}p1 /mnt/esp2
mkdir -p /mnt/esp2/EFI/BOOT /mnt/esp2/efi/freebsd
cp "$LOADER" /mnt/esp2/EFI/BOOT/BOOTAA64.EFI
printf 'currdev="disk0p2:"\n' > /mnt/esp2/efi/freebsd/loader.env
umount /mnt/esp2

echo "=== tessera (p2): base userland (selective copy) ==="
$BIN/mkfs-tessera --create -s 2800 /dev/${D}p2 >/dev/null 2>&1
mkdir -p $T; mount -t tessera /dev/${D}p2 $T

cd /
echo "--- base bin/lib dirs ---"
for d in bin sbin lib libexec rescue; do
    echo "    $d"; tar -cpf - $d | tar -xpf - -C $T
done
# /root on this dev box holds huge corpses (nlink-corpse.img etc.) — don't
# copy it; a fresh empty root home is all the boot needs.
mkdir -p $T/root; chmod 700 $T/root
echo "--- /etc (base config; rc.conf/fstab overwritten below) ---"
tar -cpf - etc | tar -xpf - -C $T
echo "--- /usr (minus local/src/obj/tests/debug/include) ---"
tar -cpf - \
    --exclude usr/obj --exclude usr/src --exclude usr/local \
    --exclude usr/tests --exclude 'usr/lib/debug' --exclude usr/include \
    usr | tar -xpf - -C $T
echo "--- runtime dir skeleton + /var ---"
mkdir -p $T/dev $T/proc $T/tmp $T/mnt $T/media $T/var
chmod 1777 $T/tmp
mtree -deU -f $T/etc/mtree/BSD.var.dist -p $T/var >/dev/null 2>&1 || true

echo "=== kernel + tessera_fs.ko + lua loader ==="
mkdir -p $T/boot/kernel
cp /boot/laminar/kernel $T/boot/kernel/kernel
cp "$KMOD" $T/boot/kernel/tessera_fs.ko
cp -a /boot/lua $T/boot/lua
cp -a /boot/defaults $T/boot/defaults
cp -a /boot/device.hints $T/boot/device.hints 2>/dev/null || true

echo "=== loader.conf (multiuser: NO boot_single) ==="
cat > $T/boot/loader.conf <<'EOF'
kernel="kernel"
module_path="/boot/kernel"
tessera_fs_load="YES"
vfs.root.mountfrom="tessera:/dev/vtbd0p2"
autoboot_delay="2"
EOF

echo "=== /etc: clean rc.conf + tessera fstab + passwordless root ==="
cat > $T/etc/fstab <<'EOF'
/dev/vtbd0p2    /   tessera rw  0   0
EOF
cat > $T/etc/rc.conf <<'EOF'
hostname="atrium-tessera"
sendmail_enable="NONE"
sendmail_submit_enable="NO"
sendmail_outbound_enable="NO"
sendmail_msp_queue_enable="NO"
update_motd="NO"
EOF
# neutralize any Atrium service overrides copied from the dev box
rm -f $T/etc/rc.conf.d/* 2>/dev/null || true
# passwordless root for the console smoke test
sed -i.bak -e 's#^root:[^:]*:#root::#' $T/etc/master.passwd && rm -f $T/etc/master.passwd.bak
pwd_mkdb -d $T/etc -p $T/etc/master.passwd

sync
umount $T
$BIN/tessera-fsck /dev/${D}p2 2>&1 | tail -3
echo "=== DONE build_multiuser_boot ==="
