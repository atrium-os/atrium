#!/bin/sh
# Rust kernel-module spike (review 2026-11-07 step 3): build, deploy, test.
#
#   sh scripts/rust-kmod-spike.sh prepare   # freebsd-kpi-rs into $SRC/sys/rust + our patches
#   sh scripts/rust-kmod-spike.sh build     # rspike.ko + cspike.ko for $KERNCONF (kernel must be built)
#   sh scripts/rust-kmod-spike.sh deploy    # copy modules + spike-bench to the guest, kldload
#   sh scripts/rust-kmod-spike.sh test      # differential test, Rust vs C (on the guest)
#   sh scripts/rust-kmod-spike.sh bench     # latency, Rust vs C (on the guest)
#
# Environment:
#   SRC        atrium-os/freebsd-src checkout (default ~/freebsd-src)
#   OBJ        MAKEOBJDIRPREFIX of the kernel build (default ~/obj)
#   TARGET     arm64 (default) or amd64
#   KERNCONF   ATRIUM-NODEBUG (default; the one to measure), ATRIUM-KASAN, ...
#   TOOLCHAIN  rustup toolchain (default nightly-2025-07-23, the KPI's CI pin)
#   VSSH       ssh wrapper for the guest (default scripts/vssh)
#
# Cross-builds from Linux use tools/build/make.py with clang-18 for arm64 and
# clang-19 for amd64 (LLVM=N overrides; RUNBOOK §13).  OBJ for amd64 is
# usually a separate objdir (e.g. ~/obj-amd64).
# The KPI is ayrtonm/freebsd-kpi-rs (BSD-2-Clause sources) at KPI_REV, with
# kmod-rust-spike/kpi-patches applied; see kmod-rust-spike/README.md.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SPIKE="$ROOT/kmod-rust-spike"
SRC="${SRC:-$HOME/freebsd-src}"
OBJ="${OBJ:-$HOME/obj}"
TARGET="${TARGET:-arm64}"
case "$TARGET" in
    arm64) TARGET_ARCH=aarch64; CTRIPLE=aarch64-unknown-freebsd16; LLVM_DEF=18 ;;
    # amd64 kmods need LLVM 19: clang 18 emits GOTPCRELX, which kldload rejects.
    amd64) TARGET_ARCH=amd64;   CTRIPLE=x86_64-unknown-freebsd16;  LLVM_DEF=19 ;;
    *) echo "TARGET must be arm64 or amd64" >&2; exit 2 ;;
esac
LLVM="${LLVM:-$LLVM_DEF}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
TOOLCHAIN="${TOOLCHAIN:-nightly-2025-07-23}"
KPI_URL="https://github.com/ayrtonm/freebsd-kpi-rs"
KPI_REV="4434e5398e7520c4fc70d7f45cda105bd81b3ea6"
VSSH="${VSSH:-$ROOT/scripts/vssh}"
SYSROOT="${SYSROOT:-$HOME/atrium-vm/sysroot}"
[ "$TARGET" = amd64 ] && SYSROOT="${SYSROOT_AMD64:-$HOME/atrium-vm-amd64/sysroot}"

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

prepare() {
    if [ ! -d "$SRC/sys/rust/.git" ]; then
        git clone -q "$KPI_URL" "$SRC/sys/rust"
    fi
    git -C "$SRC/sys/rust" checkout -q -B atrium-spike "$KPI_REV"
    git -C "$SRC/sys/rust" -c user.name=atrium -c user.email=atrium@localhost \
        am -q "$SPIKE"/kpi-patches/*.patch
    grep -q 'sys/rust/' "$SRC/.git/info/exclude" 2>/dev/null || echo 'sys/rust/' >> "$SRC/.git/info/exclude"
    rustup component add --toolchain "$TOOLCHAIN" rust-src rustfmt >/dev/null
    rustup target add --toolchain "$TOOLCHAIN" aarch64-unknown-none-softfloat x86_64-unknown-none >/dev/null
    command -v bindgen >/dev/null || cargo install -q bindgen-cli
    echo "prepared $SRC/sys/rust at $(git -C "$SRC/sys/rust" rev-parse --short HEAD)"
}

build() {
    # buildkernel -DKERNFAST with LOCAL_MODULES: reuses the built kernel and
    # builds only our two modules against it (and its KERN_OPTS: KASAN etc.).
    make_py -j"$(nproc)" buildkernel KERNCONF="$KERNCONF" -DKERNFAST \
        LOCAL_MODULES_DIR="$SPIKE" LOCAL_MODULES="cspike rspike" MODULES_OVERRIDE= \
        WITHOUT_CTF=yes
    K="$OBJ$SRC/$TARGET.$TARGET_ARCH/sys/$KERNCONF"
    mkdir -p "$SPIKE/out/$TARGET-$KERNCONF"
    for m in cspike rspike; do
        cp "$K/modules$SPIKE/$m/$m.ko" "$SPIKE/out/$TARGET-$KERNCONF/"
    done
    clang-$LLVM --target="$CTRIPLE" --sysroot="$SYSROOT" -fuse-ld=lld -O2 -Wall \
        -o "$SPIKE/out/$TARGET-$KERNCONF/spike-bench" "$SPIKE/bench/spike-bench.c"
    ls -l "$SPIKE/out/$TARGET-$KERNCONF"
}

deploy() {
    D="$SPIKE/out/$TARGET-$KERNCONF"
    tar cf - -C "$D" . | "$VSSH" 'mkdir -p /root/spike && tar xf - -C /root/spike && cd /root/spike && sha256 -q *.ko spike-bench'
    sha256sum "$D"/*.ko "$D/spike-bench" | cut -d' ' -f1
    "$VSSH" 'kldunload rspike 2>/dev/null; kldunload cspike 2>/dev/null; kldload /root/spike/cspike.ko && kldload /root/spike/rspike.ko && kldstat | grep spike && ls -l /dev/rspike /dev/cspike'
}

case "${1:-}" in
    prepare) prepare ;;
    build)   build ;;
    deploy)  deploy ;;
    test)    "$VSSH" "/root/spike/spike-bench test ${ITERS:-20000}" ;;
    bench)   "$VSSH" "/root/spike/spike-bench bench ${OPS:-200000} ${RUNS:-11}" ;;
    *)       sed -n '2,20p' "$0"; exit 2 ;;
esac
