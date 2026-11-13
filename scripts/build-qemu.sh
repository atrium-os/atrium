#!/bin/sh
# Build the atrium-os/qemu fork (QEMU 10.2 + hw/misc/gpusim.c) in a cloud
# container, for scripts/cloud-vm.sh.
#
# Why: Ubuntu 24.04's QEMU 8.2.2 lacks the SETM fix (QEMU 8.2.8, upstream
# bab209af35). FreeBSD's arm64 kernel uses FEAT_MOPS for memset and page
# zeroing, so an arm64 guest on -cpu max hangs or corrupts memory now and
# then. cloud-vm.sh picks this build up automatically from
# $QEMU_SRC/build (default ~/qemu-atrium).
#
#   sh scripts/build-qemu.sh                      # aarch64 only (~10 min, ~600 MB)
#   TARGETS=aarch64-softmmu,x86_64-softmmu sh scripts/build-qemu.sh
set -eu
SRC="${QEMU_SRC:-$HOME/qemu-atrium}"
TARGETS="${TARGETS:-aarch64-softmmu}"
JOBS="${JOBS:-$(nproc)}"

pkgs=""
for p in libslirp-dev libglib2.0-dev libpixman-1-dev ninja-build; do
    dpkg -s "$p" >/dev/null 2>&1 || pkgs="$pkgs $p"
done
[ -z "$pkgs" ] || apt-get install -y $pkgs

[ -d "$SRC/.git" ] || git clone --depth 1 https://github.com/atrium-os/qemu "$SRC"
mkdir -p "$SRC/build"
cd "$SRC/build"
# user networking (slirp) is required: cloud-vm.sh uses -netdev user.
../configure --target-list="$TARGETS" --enable-slirp \
    --disable-docs --disable-werror --disable-gtk --disable-sdl \
    --disable-opengl --disable-spice --disable-vnc --disable-tools \
    --disable-guest-agent >/dev/null
for t in $(echo "$TARGETS" | tr , ' '); do
    ninja -j"$JOBS" "qemu-system-${t%-softmmu}"
done
ls -l "$SRC"/build/qemu-system-*
