#!/bin/sh
# Build the Atrium bundle ON the R9700 box's Ubuntu (16 threads + KVM: the
# kernel builds in minutes and the helper VM runs at native speed, and the
# bundle never has to cross the network). docs/spec/r9700-bare-metal.md.
#
#   sh scripts/r9700/build-on-box.sh deps     # apt packages + rustup (once)
#   sh scripts/r9700/build-on-box.sh build    # kernel, kmods, helper VM, bundle
#   sh scripts/r9700/build-on-box.sh testboot # boot the bundle under KVM, check ssh
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
testboot)
    # Boot the newest bundle under KVM on a disk laid out like this box's
    # (ESP, a Linux partition, the atrium partition) and check ssh, the
    # NIC-by-MAC config and the fallback timer, before the real install.
    B=$(ls -t "$HOME"/r9700-out/atrium-r9700-*.tar | head -1)
    T="$HOME/r9700-testboot"; rm -rf "$T"; mkdir -p "$T/b"; tar -xf "$B" -C "$T/b"
    (cd "$T/b" && sha256sum -c SHA256 >/dev/null) || die "bundle checksum"
    truncate -s 12G "$T/disk.img"
    sgdisk -o -n 1:2048:+200M -t 1:ef00 -n 2:0:+1G -t 2:8300 -n 3:0:0 -t 3:a503 -c 3:atrium "$T/disk.img" >/dev/null
    truncate -s 200M "$T/esp.img"; mkfs.vfat "$T/esp.img" >/dev/null
    mmd -i "$T/esp.img" ::/EFI ::/EFI/BOOT ::/EFI/atrium
    mcopy -i "$T/esp.img" "$T/b/loader.efi" ::/EFI/BOOT/BOOTX64.EFI
    printf 'mac=52:54:00:12:34:56\ncidr=10.0.2.15/24\ngw=10.0.2.2\ndns="10.0.2.3"\n' > "$T/net.conf"
    cp "$HOME/.ssh/fresco_bsd_ed25519.pub" "$T/authorized_keys"
    mcopy -i "$T/esp.img" "$T/net.conf" "$T/authorized_keys" ::/EFI/atrium/
    dd if="$T/esp.img" of="$T/disk.img" bs=512 seek=2048 conv=notrunc status=none
    s3=$(sgdisk -i 3 "$T/disk.img" | awk '/First sector/ {print $3}')
    zstd -q -dc "$T/b/root.ufs.zst" | dd of="$T/disk.img" bs=512 seek="$s3" conv=notrunc,sparse status=none
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$T/vars.fd"
    qemu-system-x86_64 -enable-kvm -machine q35 -cpu host -smp 4 -m 4G -display none \
        -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd -drive if=pflash,format=raw,file="$T/vars.fd" \
        -drive file="$T/disk.img",format=raw,if=none,id=d0 -device nvme,drive=d0,serial=testboot \
        -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2224-:22 -device e1000,netdev=n0,mac=52:54:00:12:34:56 \
        -serial file:"$T/serial.log" -daemonize -pidfile "$T/qemu.pid"
    ok=0
    for i in $(seq 1 60); do
        if ssh -i "$HOME/.ssh/fresco_bsd_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
            -o LogLevel=ERROR -o ConnectTimeout=5 -p 2224 root@127.0.0.1 \
            'cat /etc/atrium-image; uname -v; df -h / | tail -1; ifconfig wan0 | grep inet; service atrium_fallback status 2>/dev/null; pgrep -lf "sleep 1200"; atrium-gpu-probe 2>&1 | head -1; ls /boot/atrium-gpu-amd; kldstat | grep -c atrium_gpu || true'; then
            ok=1; break
        fi
        sleep 5
    done
    kill "$(cat "$T/qemu.pid")" 2>/dev/null || true
    [ "$ok" = 1 ] || { tail -40 "$T/serial.log"; die "test boot: no ssh"; }
    echo "TESTBOOT OK ($B)"
    ;;
*) sed -n '2,10p' "$0"; exit 2 ;;
esac
