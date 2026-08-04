#!/bin/sh
# Build a FULL DEV Tessera root on vtbd3 (tessera-devroot.img, ~25 GiB): the
# stripped multiuser root (build_multiuser_boot.sh) plus the dev toolchain
# (/usr/src, /usr/obj, /usr/include, /usr/local), network (DHCP), sshd with the
# vssh key, and a boot-time 9p mount of the host share at /mnt/host — so the dev
# VM can boot ENTIRELY on Tessera and do real work (kernel/kmod builds, git,
# edit-compile). ZFS vm.qcow2 stays untouched as the fallback. Runs INSIDE the
# ZFS dev VM.
set -e
D=vtbd3
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
KMOD=/mnt/host/atrium-tessera/kmod/tessera_fs.ko
LOADER=$(find /usr/obj -name loader_lua.efi | head -1)
test -n "$LOADER"; test -e "$KMOD"
kldstat -q -n tessera_fs || kldload "$KMOD"
umount /mnt/espd 2>/dev/null || true; umount /mnt/tpd 2>/dev/null || true
T=/mnt/tpd

echo "=== partition $D: EFI(64M) + tessera(rest) ==="
gpart destroy -F $D 2>/dev/null || true
gpart create -s gpt $D
gpart add -t efi -s 64M -a 1m $D
gpart add -t freebsd-ufs -a 1m $D

echo "=== ESP (p1) ==="
newfs_msdos -F 32 -c 1 /dev/${D}p1 >/dev/null 2>&1
mkdir -p /mnt/espd; mount -t msdosfs /dev/${D}p1 /mnt/espd
mkdir -p /mnt/espd/EFI/BOOT /mnt/espd/efi/freebsd
cp "$LOADER" /mnt/espd/EFI/BOOT/BOOTAA64.EFI
printf 'currdev="disk0p2:"\n' > /mnt/espd/efi/freebsd/loader.env
umount /mnt/espd

echo "=== tessera (p2): full dev userland ==="
P2MIB=$(( $(diskinfo /dev/${D}p2 | awk '{print $3}') / 1048576 ))
echo "    p2 = ${P2MIB} MiB"
$BIN/mkfs-tessera --create -s "$P2MIB" /dev/${D}p2 >/dev/null 2>&1
mkdir -p $T; mount -t tessera /dev/${D}p2 $T

cd /
echo "--- base bin/lib dirs ---"
for d in bin sbin lib libexec rescue; do
    echo "    $d"; tar -cpf - $d | tar -xpf - -C $T
done
echo "--- /usr FULL incl src/obj/include/local (minus debug/tests) — large ---"
tar -cpf - --exclude usr/lib/debug --exclude usr/tests usr | tar -xpf - -C $T
echo "--- /etc ---"
tar -cpf - etc | tar -xpf - -C $T
echo "--- fresh /root + vssh key ---"
mkdir -p $T/root/.ssh; chmod 700 $T/root; chmod 700 $T/root/.ssh
cp /root/.ssh/authorized_keys $T/root/.ssh/ 2>/dev/null || true
echo "--- runtime dir skeleton + /var + 9p mountpoint ---"
mkdir -p $T/dev $T/proc $T/tmp $T/mnt $T/media $T/var $T/mnt/host
chmod 1777 $T/tmp
mtree -deU -f $T/etc/mtree/BSD.var.dist -p $T/var >/dev/null 2>&1 || true

echo "=== kernel + tessera_fs.ko + lua loader ==="
mkdir -p $T/boot/kernel
cp /boot/laminar/kernel $T/boot/kernel/kernel
cp "$KMOD" $T/boot/kernel/tessera_fs.ko
cp -a /boot/lua $T/boot/lua
cp -a /boot/defaults $T/boot/defaults
cp -a /boot/device.hints $T/boot/device.hints 2>/dev/null || true

echo "=== loader.conf (tessera root, p9fs preloaded) ==="
cat > $T/boot/loader.conf <<'EOF'
kernel="kernel"
module_path="/boot/kernel"
tessera_fs_load="YES"
p9fs_load="YES"
nullfs_load="YES"
vfs.root.mountfrom="tessera:/dev/vtbd0p2"
autoboot_delay="2"
EOF

echo "=== /etc: fstab (tessera root + 9p host share), rc.conf (net+sshd) ==="
cat > $T/etc/fstab <<'EOF'
/dev/vtbd0p2	/		tessera	rw		0	0
bsd_share	/mnt/host	p9fs	trans=virtio,rw,late,failok	0	0
EOF
cat > $T/etc/rc.conf <<'EOF'
hostname="atrium-devroot"
ifconfig_vtnet0="DHCP"
sshd_enable="YES"
sendmail_enable="NONE"
sendmail_submit_enable="NO"
sendmail_outbound_enable="NO"
sendmail_msp_queue_enable="NO"
update_motd="NO"
EOF
rm -f $T/etc/rc.conf.d/* 2>/dev/null || true
# passwordless root for the console; key auth (authorized_keys) covers ssh
sed -i.bak -e 's#^root:[^:]*:#root::#' $T/etc/master.passwd && rm -f $T/etc/master.passwd.bak
pwd_mkdb -d $T/etc -p $T/etc/master.passwd

sync
umount $T
echo "=== fsck ==="
$BIN/tessera-fsck /dev/${D}p2 2>&1 | tail -4
echo "=== DONE build_devroot ==="
