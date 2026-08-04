#!/bin/sh
# Build a minimal bootable FreeBSD root on the tessera-root disk (vtbd2).
# Runs INSIDE the VM (native FreeBSD). Populates a tessera volume with the
# statically-linked /rescue toolset + the dirs init needs, enough to boot
# single-user to a shell = proof the kernel mounted tessera as root.
#
# Usage (from host):  vssh 'sh /mnt/host/scratch/stress/build_tessera_root.sh'
set -e
DEV=/dev/vtbd2                 # serial=tessera-root (vm/tessera-root.img, 3 GiB)
MNT=/mnt/troot
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

echo "=== load kmod + mkfs tessera on $DEV ==="
kldstat -q -n tessera_fs || kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
umount $MNT 2>/dev/null || true
# 3 GiB disk = 3072 MiB; leave a little headroom.
$BIN/mkfs-tessera --create -s 3000 $DEV

echo "=== mount + populate ==="
mkdir -p $MNT
mount -t tessera $DEV $MNT

# statically-linked rescue toolset (init, sh, ls, mount, ...)
cp -a /rescue $MNT/rescue
# init's single-user default shell is /bin/sh — provide it so we don't
# have to answer the "Enter pathname of shell" prompt over serial.
mkdir -p $MNT/bin $MNT/sbin $MNT/dev $MNT/etc $MNT/tmp $MNT/root
cp /rescue/sh $MNT/bin/sh
# a real init at the conventional path too (static rescue init)
cp /rescue/init $MNT/sbin/init
# minimal marker so we can prove-by-reading which root we booted
echo "tessera-root-ok $(date 2>/dev/null || echo)" > $MNT/TESSERA_ROOT_MARKER
# a trivial rc so multi-user boot (later) has something; single-user ignores it
cat > $MNT/etc/rc <<'EOF'
#!/rescue/sh
echo "=== TESSERA ROOT: /etc/rc reached ==="
/rescue/sh
EOF
chmod 0755 $MNT/etc/rc

echo "=== contents ==="
ls -la $MNT
echo "--- marker ---"; cat $MNT/TESSERA_ROOT_MARKER
echo "=== unmount cleanly ==="
umount $MNT
echo "=== fsck the result ==="
$BIN/tessera-fsck $DEV
echo "=== DONE build_tessera_root ==="
