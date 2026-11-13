#!/bin/sh
# Aura W0 gate in the FreeBSD VM (cloud-vm.sh; docs/spec/wifi.md §7):
# aura_wlan.ko + aura_sim.ko, driven by aura-sim-check through /dev/aura/*.
#
#   ARCH=aarch64 sh scripts/aura-vm-check.sh kernel   # fork kernel as kernel.atrium (nextboot)
#   ARCH=aarch64 sh scripts/aura-vm-check.sh test     # build modules + checker, load, run
#
# The modules are built against the fork kernel (scripts/aura-kmod.sh), so the
# guest must be running it: `kernel` installs it (scripts/hospes-k1-vm.sh
# kernel does the work) and is needed again after every VM restart (nextboot
# is one-shot).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-aarch64}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
case "$ARCH" in
    aarch64|arm64) TGT=arm64; T=aarch64-unknown-freebsd; S="$HOME/atrium-vm/sysroot" ;;
    *) TGT=amd64; T=x86_64-unknown-freebsd; S="$HOME/atrium-vm-amd64/sysroot" ;;
esac
V="$ROOT/scripts/vssh"

case "${1:-}" in
kernel)
    ARCH="$ARCH" KERNCONF="$KERNCONF" sh "$ROOT/scripts/hospes-k1-vm.sh" kernel ;;
test)
    TARGET=$TGT KERNCONF="$KERNCONF" sh "$ROOT/scripts/aura-kmod.sh" >&2
    X="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
    (cd "$ROOT/aura" && RUSTFLAGS="$X" cargo build --release -p aura-sim-check --target "$T" >&2)
    B=$(mktemp -d)/aura
    mkdir -p "$B"
    cp "$ROOT/aura/kmod/out/$TGT-$KERNCONF/"*.ko "$ROOT/aura/target/$T/release/aura-sim-check" "$B/"
    tar czf - -C "$(dirname "$B")" aura | "$V" 'rm -rf /tmp/aura && tar xzf - -C /tmp'
    rm -rf "$(dirname "$B")"
    "$V" 'set -e
        sysctl -n kern.bootfile | grep -q kernel.atrium || { echo "guest is not running the fork kernel: run the kernel step first"; exit 1; }
        cd /tmp/aura
        kldunload aura_sim 2>/dev/null || true
        kldunload aura_wlan 2>/dev/null || true
        kldload ./aura_wlan.ko
        kldload ./aura_sim.ko
        ls -l /dev/aura
        ifconfig wlan0 inet 10.77.0.2/24 up
        arp -s 10.77.0.1 02:bb:00:00:00:03
        ./aura-sim-check
        sysctl net.aura.wlan0
        kldunload aura_sim
        kldunload aura_wlan
        echo "modules unloaded cleanly"
        dmesg | tail -5' ;;
*)
    echo "usage: $0 kernel|test" >&2; exit 2 ;;
esac
