#!/bin/sh
# Hospes K1 in a FreeBSD VM (cloud-vm.sh): the system call trap.
#
#   ARCH=aarch64|amd64 sh scripts/hospes-k1-vm.sh kernel   # install the fork kernel as
#                                                          # kernel.atrium, nextboot, reboot
#   ARCH=aarch64|amd64 sh scripts/hospes-k1-vm.sh test     # module + fixtures + H3 CDM
#
# kernel: the fork (atrium-os/freebsd-src with <sys/systrap.h>) must be built for KERNCONF
# in $HOME/obj-<target> (RUNBOOK §13.2), for amd64 with LLVM 19: kernel.atrium carries its
# own modules, and clang 18's amd64 modules do not load (xdr.ko, then zfs.ko and the root
# file system, fail). nextboot is one-shot: rerun `kernel` after the VM
# restarts. test: builds hospes_trap.ko (scripts/hospes-kmod.sh), the static fixture runner,
# hospes-cdm and hospes-cdm-check, copies them with the fixtures and the H3 test CDM to the
# guest and runs hospes/kmod/vm/run.sh there.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-aarch64}"
SRC="${SRC:-$HOME/freebsd-src}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
case "$ARCH" in
    aarch64|arm64) TGT=arm64; TA=aarch64; LLVM=18; T=aarch64-unknown-freebsd; A=aarch64; S="$HOME/atrium-vm/sysroot" ;;
    *) TGT=amd64; TA=amd64; LLVM=19; T=x86_64-unknown-freebsd; A=x86_64; S="$HOME/atrium-vm-amd64/sysroot" ;;
esac
OBJ="${OBJ:-$HOME/obj-$TGT}"
V="$ROOT/scripts/vssh"

kernel() {
    stage="$HOME/stage-$TGT"
    rm -rf "$stage"
    env CC=/usr/bin/clang-$LLVM CXX=/usr/bin/clang++-$LLVM CPP=/usr/bin/clang-cpp-$LLVM MAKEOBJDIRPREFIX="$OBJ" \
        "$SRC/tools/build/make.py" --cross-bindir=/usr/lib/llvm-$LLVM/bin TARGET=$TGT TARGET_ARCH=$TA \
        installkernel KERNCONF="$KERNCONF" DESTDIR="$stage" -DNO_ROOT KODIR=/boot/kernel.atrium >&2
    sum=$(tar czf - -C "$stage" boot/kernel.atrium | tee "$stage.tgz" | sha256sum | cut -d' ' -f1)
    "$V" 'rm -rf /boot/kernel.atrium /tmp/k.tgz; cat > /tmp/k.tgz' < "$stage.tgz"
    got=$("$V" 'sha256 -q /tmp/k.tgz')
    [ "$got" = "$sum" ] || { echo "kernel copy corrupt ($got != $sum)" >&2; exit 1; }
    "$V" 'tar xzf /tmp/k.tgz -C / && rm /tmp/k.tgz && nextboot -k kernel.atrium && (shutdown -r now >/dev/null 2>&1 &)'
    rm -rf "$stage" "$stage.tgz"
    sleep 20
    i=0
    until VSSH_CONNECT_TIMEOUT=5 "$V" true 2>/dev/null; do
        i=$((i + 1)); [ $i -lt 240 ] || { echo "guest did not come back" >&2; exit 1; }; sleep 5
    done
    "$V" 'sysctl -n kern.bootfile; uname -v'
}

test_k1() {
    TARGET=$TGT OBJ="$OBJ" SRC="$SRC" KERNCONF="$KERNCONF" sh "$ROOT/scripts/hospes-kmod.sh" >&2
    runner=$(sh "$ROOT/scripts/hospes-build.sh" runner "$T")
    cdm=$(sh "$ROOT/scripts/hospes-build.sh" cdm "$T")
    X="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
    (cd "$ROOT/hospes" && RUSTFLAGS="$X" cargo build --release -p hospes-cdm-check --target "$T" >&2)
    B=$(mktemp -d)/k1vm
    mkdir -p "$B/bin" "$B/fixtures" "$B/vectors"
    cp "$runner" "$cdm" "$ROOT/hospes/target/$T/release/hospes-cdm-check" "$B/bin/"
    cp "$ROOT"/hospes/fixtures/bin/$A/*.so "$B/fixtures/"
    cp "$ROOT/hospes/test-cdm/bin/$A-clearkey.so" "$B/"
    cp "$ROOT"/hospes/test-cdm/vectors/* "$B/vectors/"
    cp "$ROOT/hospes/kmod/out/$TGT-$KERNCONF/hospes_trap.ko" "$ROOT/hospes/kmod/vm/run.sh" "$ROOT/scripts/hospes-build.sh" "$B/"
    tar czf - -C "$(dirname "$B")" k1vm | "$V" 'rm -rf /tmp/k1vm && tar xzf - -C /tmp'
    rm -rf "$(dirname "$B")"
    "$V" "cd /tmp/k1vm && ARCH=$A sh run.sh"
}

case "${1:-}" in
    kernel) kernel ;;
    test) test_k1 ;;
    *) sed -n '2,7p' "$0"; exit 2 ;;
esac
