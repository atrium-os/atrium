#!/bin/sh
# Cross-build tessera_fs.ko with its on-disk decoders in Rust (D3,
# atrium-tessera/docs/rust-decoders.md), and the all-C module beside it for
# comparison runs.
#
#   sh scripts/tessera-kmod-rust.sh prepare   # freebsd-kpi-rs into $SRC/sys/rust (once)
#   sh scripts/tessera-kmod-rust.sh build     # out/<target>-<kernconf>/tessera_fs{,-c}.ko
#
# The kernel for $KERNCONF must already be built in $OBJ (RUNBOOK §13.2):
# like the Rust kmod spike, this runs buildkernel -DKERNFAST with
# LOCAL_MODULES, which reuses that kernel and builds only this module, with
# the KPI's kmod.rust.mk compiling kmod/tessera_decode.rs. Same environment
# as scripts/rust-kmod-spike.sh (SRC, OBJ, TARGET, KERNCONF, TOOLCHAIN, LLVM).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${SRC:-$HOME/freebsd-src}"
OBJ="${OBJ:-$HOME/obj}"
TARGET="${TARGET:-arm64}"
case "$TARGET" in
    arm64) TARGET_ARCH=aarch64; LLVM_DEF=18 ;;
    amd64) TARGET_ARCH=amd64;   LLVM_DEF=19 ;;
    *) echo "TARGET must be arm64 or amd64" >&2; exit 2 ;;
esac
LLVM="${LLVM:-$LLVM_DEF}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
TOOLCHAIN="${TOOLCHAIN:-nightly-2025-07-23}"
OUT="$ROOT/atrium-tessera/kmod/out/$TARGET-$KERNCONF"
NM=/usr/lib/llvm-$LLVM/bin/llvm-nm

make_py() {
    env CC=/usr/bin/clang-$LLVM CXX=/usr/bin/clang++-$LLVM CPP=/usr/bin/clang-cpp-$LLVM \
        MAKEOBJDIRPREFIX="$OBJ" \
        RUSTC="$(rustup which --toolchain "$TOOLCHAIN" rustc)" \
        RUSTFMT="$(rustup which --toolchain "$TOOLCHAIN" rustfmt)" \
        BINDGEN="$(command -v bindgen)" \
        RUST_LIB_SRC="$(rustc +"$TOOLCHAIN" --print sysroot)/lib/rustlib/src/rust" \
        XOBJCOPY=/usr/lib/llvm-$LLVM/bin/llvm-objcopy \
        "$SRC/tools/build/make.py" --cross-bindir=/usr/lib/llvm-$LLVM/bin \
        TARGET="$TARGET" TARGET_ARCH="$TARGET_ARCH" "$@"
}

# build_one <out-name> [make vars...]
build_one() {
    name="$1"; shift
    # -DKERNFAST does not notice a changed make variable, so start the
    # module's objdir afresh each time; otherwise the second build relinks
    # the first one's objects.
    mdir="$OBJ$SRC/$TARGET.$TARGET_ARCH/sys/$KERNCONF/modules$ROOT/atrium-tessera/kmod"
    [ -d "$mdir" ] && [ ! -L "$mdir" ] && rm -rf "$mdir"
    # LOCAL_MODULES names directories under LOCAL_MODULES_DIR: ours is "kmod".
    make_py -j"$(nproc)" buildkernel KERNCONF="$KERNCONF" -DKERNFAST \
        LOCAL_MODULES_DIR="$ROOT/atrium-tessera" LOCAL_MODULES=kmod \
        MODULES_OVERRIDE= WITHOUT_CTF=yes "$@"
    ko="$mdir/tessera_fs.ko"
    [ -f "$ko" ] || { echo "no $ko" >&2; exit 1; }
    mkdir -p "$OUT"
    cp "$ko" "$OUT/$name"
    # A parallel make has linked a .ko with objects missing and exited 0
    # before (bootstrap-atrium.sh); gate on symbols from both halves.
    "$NM" "$OUT/$name" | grep -q ' [tT] tessera_vop_mknod$' \
        || { echo "$name: tessera_vop_mknod missing" >&2; exit 1; }
}

build() {
    build_one tessera_fs.ko
    n=$("$NM" "$OUT/tessera_fs.ko" | grep -c ' [tT] tessera_rs_')
    [ "$n" -gt 30 ] || { echo "tessera_fs.ko has only $n tessera_rs_ exports" >&2; exit 1; }
    # (kmod linking localises symbols, hence t as well as T.)
    echo "tessera_fs.ko: $n Rust decoder entry points"

    build_one tessera_fs-c.ko TESSERA_C_DECODE=1
    if "$NM" "$OUT/tessera_fs-c.ko" | grep -q tessera_rs_; then
        echo "tessera_fs-c.ko still has Rust symbols" >&2; exit 1
    fi
    ls -l "$OUT"
}

case "${1:-}" in
    prepare) SRC="$SRC" OBJ="$OBJ" TARGET="$TARGET" sh "$ROOT/scripts/rust-kmod-spike.sh" prepare ;;
    build)   build ;;
    *)       sed -n '2,13p' "$0"; exit 2 ;;
esac
