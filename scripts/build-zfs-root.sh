#!/bin/sh
# Build a fresh ZFS root on /dev/vtbd1 from base.txz + kernel.txz.
# Run from inside the recovered UFS dev VM, with vm-zfs.qcow2 attached
# as the second virtio-blk-pci disk.
#
# Outputs a self-contained, bootable ZFS root with:
#   - 200M EFI boot partition (FAT32, FreeBSD loader.efi)
#   - zpool zroot/{ROOT/default, var, usr/{home,obj,ports,src}, tmp}
#   - lz4 compression, atime=off, vfs.zfs.arc_max=2G
#   - atrium-virtio-gpu kmod preloaded (so it wins probe at boot)
#   - 9p share auto-mount via /etc/fstab
#   - SSH host keys generated, authorized_keys for the dev key
#   - sshd enabled, networking via dhclient on vtnet0
#
# After this script exits successfully, shut down the UFS VM, swap
# vm-zfs.qcow2 → vm.qcow2, and boot.

set -eu

# ── Config ────────────────────────────────────────────────────────────
DEV=vtbd0                 # blank target disk (UFS root is vtbd1)
POOL=zroot
ALTROOT=/mnt/zroot
EFI_LABEL=efi-zroot
ZFS_LABEL=zfs-zroot

# Public key copied from the dev host:
DEV_AUTH_KEY="ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICblY/3ELIm5QR3D4XMx9vjdrWqK698J0oqjEeSmGpV5 karythra-bsd-vm"

# Tarballs (on 9p share):
TARBALLS=/mnt/host/tarballs
ATRIUM_KMOD=/mnt/host/atrium-kmod/atrium_virtio_gpu.ko

# ── Sanity ────────────────────────────────────────────────────────────
[ -e /dev/$DEV ]      || { echo "no /dev/$DEV (attach vm-zfs.qcow2 as 2nd disk)"; exit 1; }
[ -f $TARBALLS/base.txz ]   || { echo "no $TARBALLS/base.txz (mount 9p first)"; exit 1; }
[ -f $TARBALLS/kernel.txz ] || { echo "no $TARBALLS/kernel.txz"; exit 1; }
[ -f $ATRIUM_KMOD ]   || { echo "no $ATRIUM_KMOD (build atrium-kmod first)"; exit 1; }

# ── 1. Partition target ───────────────────────────────────────────────
echo "=== 1. Partitioning /dev/$DEV ==="
gpart destroy -F /dev/$DEV 2>/dev/null || true
gpart create -s gpt /dev/$DEV
gpart add -a 4k -s 200M -t efi          -l $EFI_LABEL /dev/$DEV
gpart add -a 4k        -t freebsd-zfs   -l $ZFS_LABEL /dev/$DEV
gpart show /dev/$DEV

# ── 2. EFI partition + FreeBSD loader ─────────────────────────────────
echo "=== 2. Setting up EFI boot partition ==="
newfs_msdos -F 32 -c 1 -L EFI /dev/${DEV}p1
EFI_TMP=$(mktemp -d)
mount -t msdosfs /dev/${DEV}p1 $EFI_TMP
mkdir -p $EFI_TMP/EFI/BOOT
cp /boot/loader.efi $EFI_TMP/EFI/BOOT/BOOTAA64.EFI
sync
umount $EFI_TMP
rmdir $EFI_TMP

# ── 3. Create zpool + datasets ────────────────────────────────────────
echo "=== 3. Creating zpool $POOL ==="
kldload zfs 2>/dev/null || true
# Destroy stale pool if a previous run left one behind
zpool import -f $POOL 2>/dev/null && zpool destroy -f $POOL 2>/dev/null || true

zpool create -f \
    -o altroot=$ALTROOT \
    -o cachefile=/tmp/zpool.cache \
    -O compress=lz4 \
    -O atime=off \
    -O canmount=off \
    -O mountpoint=none \
    $POOL /dev/${DEV}p2

zfs create -o canmount=off -o mountpoint=none      $POOL/ROOT
zfs create -o mountpoint=/                          $POOL/ROOT/default
zfs create -o mountpoint=/usr  -o canmount=off      $POOL/usr
zfs create                                          $POOL/usr/home
zfs create -o setuid=off                            $POOL/usr/ports
zfs create                                          $POOL/usr/src
zfs create                                          $POOL/usr/obj
zfs create -o mountpoint=/var  -o canmount=off      $POOL/var
zfs create -o exec=off  -o setuid=off               $POOL/var/audit
zfs create -o exec=off  -o setuid=off               $POOL/var/crash
zfs create -o exec=off  -o setuid=off               $POOL/var/log
zfs create                                          $POOL/var/mail
zfs create -o setuid=off                            $POOL/var/tmp
zfs create -o mountpoint=/tmp -o setuid=off         $POOL/tmp

zpool set bootfs=$POOL/ROOT/default $POOL
zfs list -o name,mountpoint

# ── 4. Extract base + kernel onto the pool ────────────────────────────
echo "=== 4. Extracting base.txz + kernel.txz ==="
cd $ALTROOT
tar -xf $TARBALLS/base.txz
echo "  base.txz extracted"
tar -xf $TARBALLS/kernel.txz
echo "  kernel.txz extracted"

# ── 5. Drop in the cached zpool.cache so loader can find the pool ─────
echo "=== 5. Caching zpool metadata into /boot ==="
mkdir -p $ALTROOT/boot/zfs
cp /tmp/zpool.cache $ALTROOT/boot/zfs/zpool.cache

# ── 6. /etc configuration ────────────────────────────────────────────
echo "=== 6. Writing /etc config ==="

cat > $ALTROOT/etc/rc.conf <<EOF
hostname="atrium-dev"
ifconfig_vtnet0="DHCP"
sshd_enable="YES"
zfs_enable="YES"
EOF

cat > $ALTROOT/etc/fstab <<EOF
# device         mountpoint     fstype   options                   dump pass
proc             /proc          procfs   rw                        0    0
bsd_share        /mnt/host      p9fs     rw,trans=virtio,noauto    0    0
EOF

cat > $ALTROOT/etc/sysctl.conf <<EOF
# ZFS ARC cap (dev VM has 12 GiB RAM; default 7/8 ARC is wasteful)
vfs.zfs.arc_max=2147483648  # 2 GiB

# Allow break-into-debugger via serial console (RUNBOOK §ddb)
debug.kdb.alt_break_to_debugger=1
EOF

cat > $ALTROOT/etc/hosts <<EOF
::1     localhost atrium-dev
127.0.0.1 localhost atrium-dev
EOF

cat > $ALTROOT/etc/resolv.conf <<EOF
nameserver 10.0.2.3
EOF

# ── 7. /boot/loader.conf — ZFS root + atrium kmod preload ─────────────
echo "=== 7. Writing /boot/loader.conf ==="
cat > $ALTROOT/boot/loader.conf <<EOF
# ZFS root
zfs_load="YES"
vfs.root.mountfrom="zfs:$POOL/ROOT/default"

# 9p host share
p9fs_load="YES"
virtio_p9fs_load="YES"

# Atrium GPU kmod — preload so its probe wins against stock vtgpu at
# boot (BUS_PROBE_VENDOR > BUS_PROBE_DEFAULT). Runtime kldload after
# vtgpu has already attached doesn't displace it; preloading is the
# clean fix.
atrium_virtio_gpu_load="YES"
EOF

mkdir -p $ALTROOT/boot/modules
cp $ATRIUM_KMOD $ALTROOT/boot/modules/

# ── 8. SSH setup ─────────────────────────────────────────────────────
echo "=== 8. SSH setup ==="
mkdir -p $ALTROOT/root/.ssh
chmod 700 $ALTROOT/root/.ssh
echo "$DEV_AUTH_KEY" > $ALTROOT/root/.ssh/authorized_keys
chmod 600 $ALTROOT/root/.ssh/authorized_keys

mkdir -p $ALTROOT/etc/ssh
for t in rsa ecdsa ed25519; do
    ssh-keygen -q -N "" -t $t -f $ALTROOT/etc/ssh/ssh_host_${t}_key
done

# PermitRootLogin without-password (key-auth only)
if grep -q "^#*PermitRootLogin" $ALTROOT/etc/ssh/sshd_config; then
    sed -i "" 's|^#*PermitRootLogin .*|PermitRootLogin without-password|' $ALTROOT/etc/ssh/sshd_config
else
    echo "PermitRootLogin without-password" >> $ALTROOT/etc/ssh/sshd_config
fi

# ── 9. Misc ──────────────────────────────────────────────────────────
echo "=== 9. Misc setup ==="
chroot $ALTROOT pwd_mkdb -p /etc/master.passwd 2>/dev/null || true
mkdir -p $ALTROOT/mnt/host
mkdir -p $ALTROOT/proc

# ── 10. Sync + export ─────────────────────────────────────────────────
echo "=== 10. Sync + export pool ==="
sync
zpool export $POOL

echo ""
echo "==============================================================="
echo "  ZFS root build complete."
echo "  Shut down this VM, swap vm-zfs.qcow2 → vm.qcow2, boot."
echo "==============================================================="
