#!/bin/sh
# Build the Atrium bundle ON the R9700 box's Ubuntu (16 threads + KVM: the
# kernel builds in minutes and the helper VM runs at native speed, and the
# bundle never has to cross the network). docs/spec/r9700-bare-metal.md.
#
#   sh scripts/r9700/build-on-box.sh deps     # apt packages + rustup (once)
#   sh scripts/r9700/build-on-box.sh build    # kernel, kmods, helper VM, bundle
#
# Expects the atrium checkout at ~/atrium and atrium-os/freebsd-src (branch
# atrium-os) at ~/freebsd-src. Everything else lives under ~ and /root/obj-amd64.
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
export SRC="${SRC:-$HOME/freebsd-src}" OBJ="${OBJ:-$HOME/obj-amd64}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
L=/usr/lib/llvm-19/bin
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }
say() { printf '\n=== %s\n' "$*"; }

case "${1:-}" in
deps)
    say "apt"
    apt-get update -q
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q \
        clang-19 lld-19 llvm-19 llvm-19-dev libclang-19-dev flex bison libarchive-dev time \
        qemu-system-x86 qemu-utils ovmf gdisk mtools dosfstools zstd xz-utils curl git \
        build-essential pkg-config openssh-client python3
    say "rust"
    command -v rustup >/dev/null || curl -fsS https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
    rustup toolchain install nightly --profile minimal -c rust-src
    rustup target add --toolchain nightly x86_64-unknown-freebsd
    rustup toolchain install nightly-2025-07-23 --profile minimal -c rust-src -c rustfmt
    command -v bindgen >/dev/null || cargo install --locked bindgen-cli
    ;;
build)
    . "$HOME/.cargo/env"
    [ -d "$SRC/sys" ] || die "no $SRC (clone atrium-os/freebsd-src, branch atrium-os)"
    mkdir -p "$OBJ"
    say "kernel $KERNCONF"
    t0=$(date +%s)
    env CC=/usr/bin/clang-19 CXX=/usr/bin/clang++-19 CPP=/usr/bin/clang-cpp-19 MAKEOBJDIRPREFIX="$OBJ" \
        "$SRC/tools/build/make.py" --cross-bindir=$L TARGET=amd64 TARGET_ARCH=amd64 -j"$(nproc)" \
        KERNCONF="$KERNCONF" kernel-toolchain buildkernel > "$HOME/kbuild.log" 2>&1 || die "kernel (see ~/kbuild.log)"
    echo "  $(( $(date +%s) - t0 )) s"
    say "gpu kmods"
    [ -d "$SRC/sys/rust" ] || sh "$ROOT/scripts/gpu-kmod-rust.sh" prepare > "$HOME/gpukmod.log" 2>&1
    sh "$ROOT/scripts/gpu-kmod-rust.sh" build >> "$HOME/gpukmod.log" 2>&1 || die "gpu kmods (see ~/gpukmod.log)"
    say "helper VM (KVM)"
    [ -f "$HOME/atrium-vm-amd64/vm.qcow2" ] || \
        ARCH=amd64 SETUP_VM_ONLY=1 sh "$ROOT/scripts/cloud-vm.sh" setup > "$HOME/vmsetup.log" 2>&1 || die "VM setup (see ~/vmsetup.log)"
    say "bundle"
    ARCH=amd64 sh "$ROOT/scripts/r9700/make-image.sh" 2>&1 | grep -v SCHILY | tee "$HOME/mkimg.log" | tail -4
    ;;
*) sed -n '2,10p' "$0"; exit 2 ;;
esac
