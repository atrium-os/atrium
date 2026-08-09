#!/bin/sh
# Boot the ZFS dev image as root, with the Tessera devroot attached as a PLAIN
# DATA DISK, so the devroot can be fsck'd/repaired while UNMOUNTED.
#
# Why this exists: tessera-fsck must never run on a mounted volume, and the
# devroot is normally the live root — there is no way to repair it from inside
# itself. vm.qcow2 is an independent bootable FreeBSD (the pre-Tessera ZFS dev
# image), so booting it leaves the devroot untouched and inert.
#
# The ZFS drive is given bootindex=0 here and the devroot none, which is the
# reverse of run-vm.sh (where the devroot boots so ongoing work dogfoods the
# FS). Devices carrying a bootindex sort ahead of those without.
#
# A raw SWAP disk is attached (recover-swap.img, sparse) and the guest swaps
# onto it. tessera-fsck holds a large transient heap and was OOM-killed on the
# 25 GB devroot in a 4 GiB guest ("failed to reclaim memory"). The answer is
# NOT a bigger guest — on a 36 GiB host that just moves the pressure onto
# macOS, which is why run-vm.sh went from 12 GiB back to 4. Swap is disk, so
# guest RAM stays 4 GiB. Raw device, never a vnode-md on ZFS: swapping through
# a filesystem that itself needs memory to write is a deadlock.
#
# usage: sh scripts/recover-devroot.sh          (then ssh -p 2222 root@localhost)
set -eu
BSD_DIR="$(cd "$(dirname "$0")/.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
EFI_VARS="$BSD_DIR/vm/edk2-arm-vars-recover.fd"
# Which image to attach as the DATA disk to be repaired/inspected.
RECOVER_IMG="${RECOVER_IMG:-tessera-devroot.img}"
SMP="${SMP:-4}"
MEM="${MEM:-4096}"

# Private EFI vars: never disturb the normal VM's persisted boot order.
[ -f "$EFI_VARS" ] || cp "$BSD_DIR/vm/edk2-arm-vars.fd" "$EFI_VARS"

exec "$QEMU" \
    -L "$QEMU_DIR/pc-bios" \
    -accel hvf -cpu host -machine virt,gic-version=3 \
    -smp "$SMP" -m "$MEM" \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$EFI_VARS" \
    -drive file="$BSD_DIR/vm/vm.qcow2",format=qcow2,cache=writeback,if=none,id=zfsdrv \
    -device virtio-blk-pci,drive=zfsdrv,bootindex=0 \
    -drive file="$BSD_DIR/vm/$RECOVER_IMG",format=raw,cache=writeback,if=none,id=devrootdrv \
    -device virtio-blk-pci,drive=devrootdrv,serial=tessera-devroot,config-wce=on \
    -drive file="$BSD_DIR/vm/recover-swap.img",format=raw,cache=none,if=none,id=swapdrv \
    -device virtio-blk-pci,drive=swapdrv,serial=recover-swap,config-wce=on \
    -device virtio-net-pci,netdev=net0 \
    -netdev user,id=net0,hostfwd=tcp::2222-:22 \
    -fsdev local,id=share,path="$BSD_DIR",security_model=none \
    -device virtio-9p-pci,fsdev=share,mount_tag=bsd_share \
    -display none -serial tcp:127.0.0.1:4444,server=on,wait=off \
    -monitor unix:/tmp/qmp-recover.sock,server=on,wait=off
