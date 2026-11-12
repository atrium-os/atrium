#!/bin/sh
# Cross-build hospes_trap.ko, the system call trap for Hospes (docs/spec/hospes.md §13 R1,
# step K1): C glue (hospes_trap.c) and the Rust range table (hospes_trap.rs).
#
#   TARGET=arm64|amd64 sh scripts/hospes-kmod.sh   # hospes/kmod/out/<target>-<kernconf>/hospes_trap.ko
#
# Needs a kernel from atrium-os/freebsd-src with <sys/systrap.h> (the core hook), already
# built for $KERNCONF in $OBJ (RUNBOOK §13.2), and freebsd-kpi-rs prepared in $SRC/sys/rust
# (scripts/rust-kmod-spike.sh prepare). Like scripts/tessera-kmod-rust.sh this runs
# buildkernel -DKERNFAST with LOCAL_MODULES, which reuses that kernel and builds only this
# module. Environment: SRC, OBJ, TARGET, KERNCONF, TOOLCHAIN, LLVM.
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
OUT="$ROOT/hospes/kmod/out/$TARGET-$KERNCONF"
LLVMBIN="${LLVMBIN:-/usr/lib/llvm-$LLVM/bin}"
NM="$LLVMBIN/llvm-nm"

[ -f "$SRC/sys/sys/systrap.h" ] || { echo "$SRC has no sys/systrap.h: not an atrium-os kernel with the K1 hook" >&2; exit 1; }

mdir="$OBJ$SRC/$TARGET.$TARGET_ARCH/sys/$KERNCONF/modules$ROOT/hospes/kmod/hospes_trap"
[ -d "$mdir" ] && [ ! -L "$mdir" ] && rm -rf "$mdir"
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
    LOCAL_MODULES_DIR="$ROOT/hospes/kmod" LOCAL_MODULES=hospes_trap \
    MODULES_OVERRIDE= WITHOUT_CTF=yes
ko="$mdir/hospes_trap.ko"
[ -f "$ko" ] || { echo "no $ko" >&2; exit 1; }
# A parallel make has linked a .ko with objects missing and exited 0 before; gate on
# symbols from both halves (kmod linking localises them, hence t as well as T).
for s in hospes_trap_hook sys_hospes_trap hospes_trap_hit hospes_trap_add hospes_trap_lock hospes_trap_table_size; do
    "$NM" "$ko" | grep -q " [tT] $s\$" || { echo "hospes_trap.ko: $s missing" >&2; exit 1; }
done
mkdir -p "$OUT"
cp "$ko" "$OUT/hospes_trap.ko"
ls -l "$OUT/hospes_trap.ko"
