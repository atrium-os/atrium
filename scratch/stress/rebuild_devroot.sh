#!/bin/sh
# Rebuild the Tessera dev root on vtbd3p2. Adapted from
# scratch/stress/build_devroot.sh with four deliberate differences:
#
#  1. p1 (the ESP) is NOT touched. It already holds a working BOOTAA64.EFI and
#     loader.env; repartitioning would need loader_lua.efi staged and buys
#     nothing. Only p2's filesystem is recreated.
#  2. Tools come from /root (mkfs-tessera, tessera-fsck) instead of the 9p
#     share, which is not mounted in this VM.
#  3. /usr/obj is EXCLUDED. It is 3.1G of rebuildable output, the dogfood
#     creates its own objdir under MAKEOBJDIRPREFIX, and leaving it out is
#     most of the headroom the old root ran out of (it died at 90% full).
#  4. Kernel is /boot/gate/kernel — GENERIC carrying the three #65 fixes
#     (e8e022164f46, 775c45e30cc3, 03c8617bd94d), verified 6/6 boots — and the
#     kmod is tessera_fs_102diag.ko, built from the same /usr/src.
#
# DESTRUCTIVE to vtbd3p2 by design.
set -e
exec > /root/rebuild_devroot.log 2>&1

D=vtbd3
# This module gets copied to $T/boot/kernel/tessera_fs.ko and is what MOUNTS
# ROOT when the guest boots this volume — keep it at the current HEAD build,
# not a stale diagnostic one.
KMOD=/root/tessera_fs_ship.ko
KERN=/boot/gate/kernel
T=/mnt/tpd

test -e "$KMOD"; test -e "$KERN"; test -x /root/mkfs-tessera
[ "$(diskinfo -v /dev/$D | awk '/ident/{print $1}')" = "tessera-devroot" ] || {
	echo "REFUSING: /dev/$D is not tessera-devroot"; exit 1; }
echo "target /dev/$D confirmed = tessera-devroot"

for f in $(kldstat | awk '/tessera_fs/ {print $NF}'); do kldunload "$f" 2>/dev/null; done
umount $T 2>/dev/null || true
kldload "$KMOD"
sysctl kern.tessera.pack_ring_audit=1 >/dev/null 2>&1

echo "=== mkfs p2 (leaving the ESP on p1 untouched) ==="
P2MIB=$(( $(diskinfo /dev/${D}p2 | awk '{print $3}') / 1048576 ))
echo "    p2 = ${P2MIB} MiB"
/root/mkfs-tessera --create -s "$P2MIB" /dev/${D}p2 >/dev/null 2>&1 \
	|| /root/mkfs-tessera -s "$P2MIB" /dev/${D}p2 >/dev/null 2>&1
mkdir -p $T; mount -t tessera /dev/${D}p2 $T
df -h $T | tail -1

cd /
echo "=== base bin/lib dirs ==="
for d in bin sbin lib libexec rescue; do
	echo "    $d  $(date +%H:%M:%S)"
	tar -cpf - $d | tar -xpf - -C $T
done

echo "=== /usr WITHOUT obj (excl debug, tests) — the big one ==="
date +%H:%M:%S
tar -cpf - --exclude usr/lib/debug --exclude usr/tests --exclude usr/obj usr \
	| tar -xpf - -C $T
date +%H:%M:%S
df -h $T | tail -1

echo "=== /etc, /root, runtime skeleton ==="
tar -cpf - etc | tar -xpf - -C $T
mkdir -p $T/root/.ssh; chmod 700 $T/root $T/root/.ssh
cp /root/.ssh/authorized_keys $T/root/.ssh/ 2>/dev/null || true

# Stage the CURRENT tessera userland + kmod so a VM booted on this root can
# fsck/mkfs/repack itself without reaching back to the ZFS root. Only the
# live tools — not the historical tessera_fs_*.ko / tessera-fsck* zoo, which
# stays on the ZFS root (still attached as vtbd4; zpool import to reach it).
for t in mkfs-tessera tessera-fsck tessera-fsck-leak tessera-reindex \
         tessera-repack tessera-stat tessera-debug; do
	[ -x /root/$t ] && cp /root/$t $T/root/ || true
done
cp "$KMOD" $T/root/tessera_fs_ship.ko
mkdir -p $T/dev $T/proc $T/tmp $T/mnt $T/media $T/var $T/mnt/host $T/usr/obj
chmod 1777 $T/tmp
mtree -deU -f $T/etc/mtree/BSD.var.dist -p $T/var >/dev/null 2>&1 || true

echo "=== kernel + kmod + loader bits ==="
mkdir -p $T/boot/kernel
# ★ Stage the WHOLE module set (~51 MiB, 729 .ko). Without it the dev root
# boots but is a degraded environment: no p9fs.ko means /mnt/host never
# mounts and the kmod build (which compiles from /mnt/host/atrium-tessera)
# cannot run; no nullfs.ko breaks jails; no zfs.ko makes the ZFS disk still
# attached at vtbd4 unreachable, i.e. no way back to the old working set
# without a host-side reboot. loader.conf asks for three of these by name.
cp /boot/kernel/*.ko $T/boot/kernel/ 2>/dev/null || true
# ...then overwrite with the two we deliberately choose.
cp "$KERN" $T/boot/kernel/kernel
cp "$KMOD" $T/boot/kernel/tessera_fs.ko
# Stale linker.hints would name modules by the SOURCE tree's layout.
kldxref $T/boot/kernel 2>/dev/null || rm -f $T/boot/kernel/linker.hints
cp -a /boot/lua $T/boot/lua
cp -a /boot/defaults $T/boot/defaults
cp -a /boot/device.hints $T/boot/device.hints 2>/dev/null || true

# ★ These MUST name the device this root actually appears as when the guest
# boots it. The devroot is the FOURTH virtio-blk device in run-vm.sh
# (crashtest, storage, tessera-root, devroot) = vtbd3 — NOT vtbd0. Writing
# vtbd0p2 here panics at mountroot. Derived from $D so the two can never
# drift apart again; adding a virtio-blk device ahead of it in run-vm.sh
# renumbers this and the rebuild must be re-run.
cat > $T/boot/loader.conf <<EOF
kernel="kernel"
module_path="/boot/kernel"
tessera_fs_load="YES"
p9fs_load="YES"
nullfs_load="YES"
vfs.root.mountfrom="tessera:/dev/${D}p2"
autoboot_delay="2"
EOF
cat > $T/etc/fstab <<EOF
/dev/${D}p2	/		tessera	rw		0	0
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
sed -i.bak -e 's#^root:[^:]*:#root::#' $T/etc/master.passwd && rm -f $T/etc/master.passwd.bak
pwd_mkdb -d $T/etc -p $T/etc/master.passwd

echo "=== final state ==="
df -h $T | tail -1
sync; cd /; umount $T && echo UMOUNT_OK
/root/tessera-fsck /dev/${D}p2 2>&1 | tail -6
echo REBUILD_DONE
