#!/bin/sh
# Verify /.tessera/snapshots/<gen>/ is hidden from jailed processes.
# Without the gate, a jail with `/` access could read other jails'
# (or the host's) historical state via the magic dir.
#
# Approach: build a jail rootfs by nullfs-mounting the host's / into
# a scratch dir, then nullfs-mounting tessera as a subdirectory. The
# jail's chroot is the scratch dir; from its POV, tessera lives at
# /tess. Then jexec stat /tess/.tessera from inside.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
JROOT=/tmp/jail_iso
JNAME=tess_iso_test

cleanup() {
    jail -r $JNAME 2>/dev/null || true
    umount $JROOT/tess 2>/dev/null || true
    umount $JROOT/usr/bin 2>/dev/null || true
    umount $JROOT/bin 2>/dev/null || true
    umount $JROOT/lib 2>/dev/null || true
    umount $JROOT/libexec 2>/dev/null || true
    rm -rf $JROOT
    umount /mnt/tessera 2>/dev/null || true
    mdconfig -d -u 0 2>/dev/null || true
    kldunload tessera_fs 2>/dev/null || true
}
trap cleanup EXIT

cleanup
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 32 --seed-file h --seed-content x \
    /tmp/jail.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/jail.img)
mount -t tessera /dev/$MD /mnt/tessera

echo "secret-from-host" > /mnt/tessera/host_data
fsync /mnt/tessera/host_data

# Build a minimal jail rootfs using nullfs mounts of host binaries.
mkdir -p $JROOT/bin $JROOT/usr/bin $JROOT/lib $JROOT/libexec $JROOT/tess
mount -t nullfs /bin     $JROOT/bin
mount -t nullfs /usr/bin $JROOT/usr/bin
mount -t nullfs /lib     $JROOT/lib
mount -t nullfs /libexec $JROOT/libexec
mount -t nullfs /mnt/tessera $JROOT/tess

echo "--- HOST view: /mnt/tessera/.tessera reachable ---"
ls /mnt/tessera/.tessera/snapshots/ >/dev/null 2>&1 \
    && echo "  host can see /.tessera/snapshots/ — OK" \
    || { echo "FAIL: host can't see magic dir"; exit 1; }

echo "--- JAIL view: /tess/.tessera should be ENOENT ---"
jail -c persist=true name=$JNAME path=$JROOT host.hostname=jail0 \
    allow.mount=false ip4=disable ip6=disable >/dev/null
JAIL_OUT=$(jexec $JNAME /bin/ls /tess/.tessera 2>&1 || true)
echo "  output: $JAIL_OUT"
echo "$JAIL_OUT" | grep -qE "No such file|Operation not permitted|ENOENT" \
    || { echo "FAIL: jail saw /tess/.tessera"; exit 1; }

# Also verify the jail CAN see the regular file (proves the mount works,
# the gate is selective).
JAIL_FILE=$(jexec $JNAME /bin/cat /tess/host_data 2>&1 || true)
[ "$JAIL_FILE" = "secret-from-host" ] \
    || { echo "FAIL: jail can't read normal file: $JAIL_FILE"; exit 1; }
echo "  jail sees host_data normally — selective gate confirmed"

echo "  jailed magic-dir lookup correctly returns ENOENT — OK"
echo DONE
