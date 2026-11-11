#!/bin/sh
# Cross-build atrium-gpu-amd's three modules (base, gpu, display) against a
# kernel built from atrium-os/freebsd-src, with the gpu module's front-end in
# Rust (atrium-gpu-amd/rs/README.md).
#
#   sh scripts/gpu-kmod-rust.sh prepare   # freebsd-kpi-rs into $SRC/sys/rust (once)
#   sh scripts/gpu-kmod-rust.sh build     # atrium-gpu-amd/out/<target>-<kernconf>/*.ko
#
# The kernel for $KERNCONF must already be built in $OBJ (RUNBOOK §13.2): this
# runs buildkernel -DKERNFAST with LOCAL_MODULES, which reuses that kernel and
# builds only these modules. Same environment as scripts/rust-kmod-spike.sh
# (SRC, OBJ, TARGET, KERNCONF, TOOLCHAIN, LLVM); TARGET defaults to amd64
# here, the architecture gpusim runs under.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${SRC:-$HOME/freebsd-src}"
OBJ="${OBJ:-$HOME/obj-amd64}"
TARGET="${TARGET:-amd64}"
case "$TARGET" in
    arm64) TARGET_ARCH=aarch64; LLVM_DEF=18 ;;
    amd64) TARGET_ARCH=amd64;   LLVM_DEF=19 ;;
    *) echo "TARGET must be arm64 or amd64" >&2; exit 2 ;;
esac
LLVM="${LLVM:-$LLVM_DEF}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
TOOLCHAIN="${TOOLCHAIN:-nightly-2025-07-23}"
DRV="$ROOT/atrium-gpu-amd"
OUT="$DRV/out/$TARGET-$KERNCONF"
NM=/usr/lib/llvm-$LLVM/bin/llvm-nm
L=/usr/lib/llvm-$LLVM/bin

make_py() {
    env CC=/usr/bin/clang-$LLVM CXX=/usr/bin/clang++-$LLVM CPP=/usr/bin/clang-cpp-$LLVM \
        XAR=$L/llvm-ar XNM=$L/llvm-nm XOBJCOPY=$L/llvm-objcopy XSIZE=$L/llvm-size \
        XSTRINGS=$L/llvm-strings XSTRIPBIN=$L/llvm-strip \
        MAKEOBJDIRPREFIX="$OBJ" \
        RUSTC="$(rustup which --toolchain "$TOOLCHAIN" rustc)" \
        RUSTFMT="$(rustup which --toolchain "$TOOLCHAIN" rustfmt)" \
        BINDGEN="$(command -v bindgen)" \
        RUST_LIB_SRC="$(rustc +"$TOOLCHAIN" --print sysroot)/lib/rustlib/src/rust" \
        "$SRC/tools/build/make.py" --cross-bindir=$L \
        TARGET="$TARGET" TARGET_ARCH="$TARGET_ARCH" "$@"
}

build() {
    mdir="$OBJ$SRC/$TARGET.$TARGET_ARCH/sys/$KERNCONF/modules$DRV"
    # -DKERNFAST does not notice changed make variables; start the gpu
    # module's objdir afresh (the Rust objects are the slow part anyway).
    [ -d "$mdir/gpu" ] && rm -rf "$mdir/gpu"
    make_py -j"$(nproc)" buildkernel KERNCONF="$KERNCONF" -DKERNFAST -DWITHOUT_CLEAN \
        LOCAL_MODULES_DIR="$DRV" LOCAL_MODULES="base gpu display" \
        MODULES_OVERRIDE= WITHOUT_CTF=yes \
        ATRIUM_BUILD_ID="$(sh "$DRV/build-id.sh")"
    mkdir -p "$OUT"
    for m in base/atrium_gpu_amd_pci gpu/atrium_gpu_amd display/atrium_gpu_amd_display; do
        [ -f "$mdir/$m.ko" ] || { echo "no $mdir/$m.ko" >&2; exit 1; }
        cp "$mdir/$m.ko" "$OUT/"
    done
    # A parallel make has linked a .ko with objects missing and exited 0
    # before (bootstrap-atrium.sh): gate on symbols from both languages.
    for s in amd_rs_ioctl amd_syncobj_signal amd_rs_reap amd_rs_watchdog amd_hwq_reset_prepare amd_hwq_submit amd_vm_setup; do
        "$NM" "$OUT/atrium_gpu_amd.ko" | grep -q " [tT] $s\$" \
            || { echo "atrium_gpu_amd.ko: $s missing" >&2; exit 1; }
    done
    ls -l "$OUT"
}

case "${1:-}" in
    prepare) SRC="$SRC" OBJ="$OBJ" TARGET="$TARGET" sh "$ROOT/scripts/rust-kmod-spike.sh" prepare ;;
    build)   build ;;
    *)       sed -n '2,13p' "$0"; exit 2 ;;
esac
