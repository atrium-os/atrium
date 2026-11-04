#!/bin/sh
# Runs in the guest on the stock kernel: take the snapshot's own src.txz,
# add the swap-free notification and swap fast-path patches, build and
# install a GENERIC-NODEBUG kernel as /boot/kernel.zbench, and build atrium_zram.ko and the fill
# workload against it.
set -eu
. /root/zb/env
cd /root/zb
NCPU=$(sysctl -n hw.ncpu)
log() { echo "[guest-build $(date +%H:%M:%S)] $*"; }

if [ ! -f /usr/src/sys/vm/swap_pager.c ]; then
	log "extract src.txz (sent by the host)"
	[ "$(sha256 -q /root/src.txz)" = "$SRCSUM" ] || { echo "src.txz checksum mismatch"; exit 1; }
	tar xJf /root/src.txz -C /
	rm -f /root/src.txz
fi
got_rev=$(uname -v | grep -oE '[0-9a-f]{12}' | head -1 || true)
log "running kernel $(uname -r) ${got_rev:-?}, snapshot $REVISION"

cd /usr/src
if ! grep -q swp_freenotify sys/vm/swap_pager.c; then
	log "apply swap-free notification patch"
	patch -s -p1 < /root/zb/swapfree.patch
fi
if ! grep -q swap_fastpath_register sys/vm/swap_pager.c; then
	log "apply swap fast-path patch"
	patch -s -p1 < /root/zb/fastpath.patch
fi
# A rerun after a finished build recompiles only what changed (KERNFAST).
FAST=
[ -f /usr/obj/usr/src/amd64.amd64/sys/GENERIC-NODEBUG/kernel ] && FAST=-DKERNFAST
log "buildkernel GENERIC-NODEBUG -j$NCPU $FAST"
make -j"$NCPU" buildkernel KERNCONF=GENERIC-NODEBUG $FAST > /root/zb/buildkernel.log 2>&1 ||
	{ tail -40 /root/zb/buildkernel.log; exit 1; }
# Installed beside the image's own (pkgbase) kernel, never over it, and
# booted once with nextboot: a plain reboot returns to the stock kernel.
make installkernel KERNCONF=GENERIC-NODEBUG INSTKERNNAME=kernel.zbench \
	> /root/zb/installkernel.log 2>&1 || { tail -40 /root/zb/installkernel.log; exit 1; }

log "build atrium_zram.ko"
cd /root/zb/zram
make SYSDIR=/usr/src/sys MAKEOBJDIR=/root/zb/zram > /root/zb/kmod.log 2>&1 ||
	{ tail -40 /root/zb/kmod.log; exit 1; }
cc -O2 -o /root/zb/fill /root/zb/fill.c
log "done; kernel in /boot/kernel.zbench"
