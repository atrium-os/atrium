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
#
# The emulated security key (-device u2f-emulated, for the Sigillum gate,
# scripts/vm-sigillum-check.sh) needs libu2f-emu at configure time; build it
# into a prefix and point pkg-config there:
#   PKG_CONFIG_PATH=$HOME/u2f-prefix/lib/pkgconfig \
#   LDFLAGS="-Wl,-rpath,$HOME/u2f-prefix/lib" TARGETS=x86_64-softmmu sh scripts/build-qemu.sh
# (libu2f-emu: cmake with -DCMAKE_C_FLAGS=-Wno-error=deprecated-declarations
# for OpenSSL 3.)
#
# On macOS it builds into external/qemu-build/build of this checkout, where
# run-vm.sh and sigillum-fido-vm.sh look, with HVF, Cocoa and libusb (for
# usb-host) and without apple-gfx (PVG), whose source does not build with the
# macOS 27 SDK. Needs Homebrew's glib, pixman, libslirp, libusb, ninja, meson.
set -eu
if [ "$(uname -s)" = Darwin ]; then
    ROOT=$(cd "$(dirname "$0")/.." && pwd)
    SRC="${QEMU_SRC:-$ROOT/external/qemu-build}"
    [ -d "$SRC/.git" ] || git clone --depth 1 https://github.com/atrium-os/qemu "$SRC"
    mkdir -p "$SRC/build"
    cd "$SRC/build"
    # --prefix=/opt/homebrew: the EFI firmware lands where run-vm.sh's EFI_SRC expects
    # (build/qemu-bundle/opt/homebrew/share/qemu/edk2-aarch64-code.fd).
    ../configure --prefix=/opt/homebrew --target-list=aarch64-softmmu --enable-hvf --enable-slirp \
        --enable-cocoa --enable-libusb --disable-pvg --disable-docs --disable-werror --disable-gtk \
        --disable-sdl --disable-spice --disable-tools --disable-guest-agent >/dev/null
    ninja -j"${JOBS:-$(sysctl -n hw.ncpu)}" qemu-system-aarch64
    # HVF needs the hypervisor entitlement on the binary.
    ent=$(mktemp)
    printf '<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>\n' > "$ent"
    codesign -s - --entitlements "$ent" --force qemu-system-aarch64
    rm -f "$ent"
    ./qemu-system-aarch64 --version | head -1
    exit 0
fi
SRC="${QEMU_SRC:-$HOME/qemu-atrium}"
TARGETS="${TARGETS:-aarch64-softmmu}"
JOBS="${JOBS:-$(nproc)}"

pkgs=""
for p in libslirp-dev libglib2.0-dev libpixman-1-dev ninja-build; do
    dpkg -s "$p" >/dev/null 2>&1 || pkgs="$pkgs $p"
done
[ -z "$pkgs" ] || apt-get install -y $pkgs

[ -d "$SRC/.git" ] || git clone --depth 1 https://github.com/atrium-os/qemu "$SRC"
# Fixes carried here until they are in the fork (scripts/qemu-patches/):
# each is applied once; one already in the tree is skipped.
for p in "$(cd "$(dirname "$0")" && pwd)"/qemu-patches/*.patch; do
    [ -e "$p" ] || continue
    if git -C "$SRC" apply --check "$p" 2>/dev/null; then
        git -C "$SRC" apply "$p" && echo "applied $(basename "$p")"
    fi
done
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
