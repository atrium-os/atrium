#!/bin/sh
# Cross-build Aura's kernel modules (docs/spec/wifi.md §3.3): aura_wlan.ko
# (C glue + the Rust core: aura/aura-proto and aura/aura-wlan-core compiled in
# as crates) and aura_sim.ko (the test radio).
#
#   TARGET=arm64|amd64 sh scripts/aura-kmod.sh   # aura/kmod/out/<target>-<kernconf>/*.ko
#
# Needs the atrium-os kernel built for $KERNCONF in $OBJ (RUNBOOK §13.2) and
# freebsd-kpi-rs prepared in $SRC/sys/rust (scripts/rust-kmod-spike.sh
# prepare). Runs buildkernel -DKERNFAST with LOCAL_MODULES, which reuses that
# kernel and builds only these modules. Environment: SRC, OBJ, TARGET,
# KERNCONF, TOOLCHAIN, LLVM.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${SRC:-$HOME/freebsd-src}"
TARGET="${TARGET:-arm64}"
case "$TARGET" in
    # amd64 kmods need LLVM 19: clang 18 emits GOTPCRELX, which kldload rejects.
    arm64) TARGET_ARCH=aarch64; LLVM_DEF=18 ;;
    amd64) TARGET_ARCH=amd64;   LLVM_DEF=19 ;;
    *) echo "TARGET must be arm64 or amd64" >&2; exit 2 ;;
esac
OBJ="${OBJ:-$HOME/obj-$TARGET}"
LLVM="${LLVM:-$LLVM_DEF}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
TOOLCHAIN="${TOOLCHAIN:-nightly-2025-07-23}"
K="$ROOT/aura/kmod"
OUT="$K/out/$TARGET-$KERNCONF"
LLVMBIN="${LLVMBIN:-/usr/lib/llvm-$LLVM/bin}"
NM="$LLVMBIN/llvm-nm"

mdir="$OBJ$SRC/$TARGET.$TARGET_ARCH/sys/$KERNCONF/modules$K"
for m in wlan sim; do
    [ -d "$mdir/$m" ] && [ ! -L "$mdir/$m" ] && rm -rf "$mdir/$m"
done
env CC="$LLVMBIN/clang" CXX="$LLVMBIN/clang++" CPP="$LLVMBIN/clang-cpp" \
    MAKEOBJDIRPREFIX="$OBJ" \
    RUSTC="$(rustup which --toolchain "$TOOLCHAIN" rustc)" \
    RUSTFMT="$(rustup which --toolchain "$TOOLCHAIN" rustfmt 2>/dev/null || echo /bin/true)" \
    BINDGEN="$(command -v bindgen)" \
    RUST_LIB_SRC="$(rustc +"$TOOLCHAIN" --print sysroot)/lib/rustlib/src/rust" \
    XOBJCOPY="$LLVMBIN/llvm-objcopy" \
    "$SRC/tools/build/make.py" --cross-bindir="$LLVMBIN" \
    TARGET="$TARGET" TARGET_ARCH="$TARGET_ARCH" \
    -j"$(nproc)" buildkernel KERNCONF="$KERNCONF" -DKERNFAST \
    LOCAL_MODULES_DIR="$K" LOCAL_MODULES="wlan sim" \
    MODULES_OVERRIDE= WITHOUT_CTF=yes
mkdir -p "$OUT"
for ko in wlan/aura_wlan sim/aura_sim; do
    [ -f "$mdir/$ko.ko" ] || { echo "no $mdir/$ko.ko" >&2; exit 1; }
    cp "$mdir/$ko.ko" "$OUT/"
done
# A parallel make has linked a .ko with objects missing and exited 0 before;
# gate on symbols from both languages.
for s in aura_wlan_attach aura_c_request aura_rs_write aura_rs_rx_data aura_rs_init; do
    "$NM" "$OUT/aura_wlan.ko" | grep -qE " [TtDd] $s\$" || { echo "aura_wlan.ko lacks $s" >&2; exit 1; }
done
ls -l "$OUT"
