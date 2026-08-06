#!/bin/sh
# Rebuild the Tessera dev root, addressed BY GPT LABEL (/dev/gpt/atrium-root).
# Nothing here may name a device number: which vtbdN a disk gets depends on how
# many virtio-blk devices run-vm.sh attached ahead of it, and a different host
# or a changed --all-disks flag renumbers it. A rebuilt root that hardcodes
# vtbd3p2 panics at mountroot the moment that happens. Adapted from
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
# DESTRUCTIVE to /dev/gpt/atrium-root by design. Override with LABEL=.
set -e
exec > /root/rebuild_devroot.log 2>&1

LABEL=${LABEL:-atrium-root}     # GPT label on the root partition
APPLABEL=${APPLABEL:-atrium-apps}
IDENT=tessera-devroot           # disk ident, the safety gate
DEV=/dev/gpt/$LABEL
# This module gets copied to $T/boot/kernel/tessera_fs.ko and is what MOUNTS
# ROOT when the guest boots this volume — keep it at the current HEAD build,
# not a stale diagnostic one.
KMOD=/root/tessera_fs_ship.ko
KERN=/boot/gate/kernel
T=/mnt/tpd

test -e "$KMOD"; test -e "$KERN"; test -x /root/mkfs-tessera

# Find the devroot by IDENT, not by number — that is what makes this portable.
disk=$(for d in $(sysctl -n kern.disks); do
	[ "$(diskinfo -v /dev/$d 2>/dev/null | awk '/ident/{print $1}')" = "$IDENT" ] \
		&& { echo "$d"; break; }
done)
[ -n "$disk" ] || { echo "REFUSING: no disk with ident=$IDENT"; exit 1; }

# Label p2 if it has never been labelled (a root built before labels, or a
# fresh gpart). The label lives in the GPT, so the mkfs below does not erase it.
[ -e "$DEV" ] || {
	echo "no /dev/gpt/$LABEL — labelling ${disk}p2"
	gpart modify -i 2 -l "$LABEL" "$disk"
	sleep 1
}
[ -e "$DEV" ] || { echo "REFUSING: $DEV still absent after labelling"; exit 1; }

# The label must resolve back to the disk we just vetted; otherwise some OTHER
# volume is carrying this label and we would wipe it.
prov=$(glabel status -s 2>/dev/null | awk -v l="gpt/$LABEL" '$1 == l {print $3}')
# Strip the partition/slice suffix: vtbd0p2 -> vtbd0, and also vtbd1s1 / s1a,
# because a label can sit on an MBR slice too (atrium-apps is gpt/... on s1).
[ "$(echo "$prov" | sed -E 's/([sp][0-9]+|[a-h])+$//')" = "$disk" ] || {
	echo "REFUSING: $DEV resolves to '$prov', not a partition of $disk"; exit 1; }

# Never rebuild the volume we are running from.
[ "$(mount | awk '$3 == "/" {print $1}')" = "$DEV" ] && {
	echo "REFUSING: $DEV is the live root"; exit 1; }

echo "target $DEV confirmed = $prov on $disk (ident=$IDENT)"

for f in $(kldstat | awk '/tessera_fs/ {print $NF}'); do kldunload "$f" 2>/dev/null; done
umount $T 2>/dev/null || true
kldload "$KMOD"
sysctl kern.tessera.pack_ring_audit=1 >/dev/null 2>&1

echo "=== mkfs p2 (leaving the ESP on p1 untouched) ==="
P2MIB=$(( $(diskinfo $DEV | awk '{print $3}') / 1048576 ))
echo "    $DEV = ${P2MIB} MiB"
/root/mkfs-tessera --create -s "$P2MIB" $DEV >/dev/null 2>&1 \
	|| /root/mkfs-tessera -s "$P2MIB" $DEV >/dev/null 2>&1
mkdir -p $T; mount -t tessera $DEV $T
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
# stays on the ZFS root (NOT attached by default any more — run-vm.sh boots
# the devroot alone; use --all-disks to get the ZFS disk back, then zpool import).
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
# (reachable only with run-vm.sh --all-disks) unreachable, i.e. no way back
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

# ★ BY LABEL, never by number. vtbdN depends on how many virtio-blk devices
# run-vm.sh attached ahead of this one, so a hardcoded vtbd3p2 panics at
# mountroot on any host or flag combination that renumbers the bus. The GPT
# label follows the partition wherever it lands.
# module_path carries /boot/modules too: the gpusim display modules live there,
# and a loader.conf listing only /boot/kernel makes the preload SILENTLY do
# nothing — the modules never load and no error is printed.
cat > $T/boot/loader.conf <<EOF
kernel="kernel"
module_path="/boot/kernel;/boot/modules"
tessera_fs_load="YES"
p9fs_load="YES"
nullfs_load="YES"
vfs.root.mountfrom="tessera:/dev/gpt/$LABEL"
autoboot_delay="2"
EOF
cat > $T/etc/fstab <<EOF
/dev/gpt/$LABEL	/		tessera	rw		0	0
bsd_share	/mnt/host	p9fs	trans=virtio,rw,late,failok	0	0
EOF
# The app volume, if this host has one. Only written when the label actually
# exists — an fstab line for an absent device drops the boot to single user.
if [ -e "/dev/gpt/$APPLABEL" ]; then
	echo "/dev/gpt/$APPLABEL	/var/lib/atrium	tessera	rw,late	0	0" >> $T/etc/fstab
	mkdir -p $T/var/lib/atrium
	echo "    app volume /dev/gpt/$APPLABEL -> /var/lib/atrium"
fi
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
/root/tessera-fsck $DEV 2>&1 | tail -6
echo REBUILD_DONE
