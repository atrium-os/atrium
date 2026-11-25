#!/bin/sh
# The Atrium C SDK into a machine's base system (dev-tools.md §5.4,
# dev-tools-plan.md M4). Runs on the HOST against the cloud VM:
#   ARCH=amd64 sh scripts/install-c-sdk.sh
# Installs, in the base (so every workspace sees them, read-only):
#   /usr/lib/libfresco.so.0 (+ .so link, .a)   built natively in the VM
#   /usr/lib/libatrium.so.0 (+ .so link, .a)   cross-built here
#   /usr/include/atrium/{fresco.h,atrium.h,atrium_plugin.h}
#   /usr/libdata/pkgconfig/{fresco,atrium}.pc
#   /usr/share/mk/atrium.app.mk
# The image builders call the same steps for an image.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=${ARCH:-amd64}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
S=/var/tmp/c-sdk

echo "=== libatrium (cross)"
# A shared object: the cross flags without the executable's start file.
F=$(echo "$CROSS_RUSTFLAGS" | sed 's# -C link-arg=[^ ]*/Scrt1.o##')
(cd "$ROOT/libatrium" && RUSTFLAGS="$F -C link-arg=-soname=libatrium.so.0" \
    cargo build -q --release --target "$TARGET" --lib)
LA=$ROOT/libatrium/target/$TARGET/release

echo "=== copy"
V "rm -rf $S; mkdir -p $S/libfresco"
tar -C "$ROOT/libfresco" -cf - Makefile src include | V "tar -C $S/libfresco -xf -"
# Its kernel interface headers, at ../fresco-kmod as the tree has them.
V "mkdir -p $S/fresco-kmod"
(cd "$ROOT/fresco-kmod" && tar -cf - *.h) | V "tar -C $S/fresco-kmod -xf -"
for f in "$LA/libatrium.so" "$LA/libatrium.a" "$ROOT/libatrium/include/atrium.h" "$ROOT/libatrium/include/atrium_plugin.h" \
         "$ROOT/etc/pkgconfig/fresco.pc" "$ROOT/etc/pkgconfig/atrium.pc" "$ROOT/etc/mk/atrium.app.mk"; do
    scp -q $K -P 2222 "$f" root@localhost:$S/
done

echo "=== build libfresco and install"
V 'sh -s' <<EOS
set -e
cd $S/libfresco
make -s clean >/dev/null 2>&1 || true
make -s libfresco.a libfresco.so
install -m 0444 libfresco.so /usr/lib/libfresco.so.0
ln -sf libfresco.so.0 /usr/lib/libfresco.so
install -m 0444 libfresco.a /usr/lib/libfresco.a
install -m 0444 $S/libatrium.so /usr/lib/libatrium.so.0
ln -sf libatrium.so.0 /usr/lib/libatrium.so
install -m 0444 $S/libatrium.a /usr/lib/libatrium.a
mkdir -p /usr/include/atrium /usr/libdata/pkgconfig /usr/share/mk
install -m 0444 include/fresco.h $S/atrium.h $S/atrium_plugin.h /usr/include/atrium/
install -m 0444 $S/fresco.pc $S/atrium.pc /usr/libdata/pkgconfig/
install -m 0444 $S/atrium.app.mk /usr/share/mk/atrium.app.mk
EOS
V "ls -l /usr/lib/libfresco.so.0 /usr/lib/libatrium.so.0 /usr/include/atrium/fresco.h /usr/share/mk/atrium.app.mk" | awk '{print "  " $NF}'
echo "c-sdk: installed"
