#!/bin/sh
# Boot the dev VM ENTIRELY on the Tessera dev root (vm/tessera-devroot.img) —
# maximal FS dogfood. vm.qcow2 (ZFS) is left untouched; to fall back, just run
# scripts/run-vm.sh instead. Headless: serial on tcp:4444, QMP on /tmp/qmp.sock,
# ssh via hostfwd 2222 (vssh works unchanged). The devroot is vtbd0 so its
# loader.conf (vfs.root.mountfrom=tessera:/dev/vtbd0p2) and fstab resolve.
#
# tessera-root.img is deliberately NOT attached here: it carries its own ESP and
# would give EFI a competing \EFI\BOOT\BOOTAA64.EFI. crashtest + storage stay for
# continued FS stress; the host share (bsd_share) mounts at /mnt/host.
set -eu
BSD_DIR="$(cd "$(dirname "$0")/.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
EFI_VARS="$BSD_DIR/vm/edk2-vars-devroot.fd"
SMP="${SMP:-4}"; MEM="${MEM:-12288}"
SHARE_DIR="$BSD_DIR"

test -f "$BSD_DIR/vm/tessera-devroot.img" || { echo "no tessera-devroot.img — run build_devroot.sh first"; exit 1; }
# Blank EFI NVRAM every boot (device set differs from run-vm.sh → stale boot
# entries would drop to the UEFI shell).
: > "$EFI_VARS"; truncate -s 67108864 "$EFI_VARS"

exec "$QEMU" \
    -L "$QEMU_DIR/pc-bios" \
    -d guest_errors -D /tmp/qemu-guest-errors.log \
    -accel hvf -cpu host -machine virt,gic-version=3 \
    -smp "$SMP" -m "$MEM" \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$EFI_VARS" \
    -drive file="$BSD_DIR/vm/tessera-devroot.img",format=raw,cache=writeback,if=none,id=devrootdrv \
    -device virtio-blk-pci,drive=devrootdrv,serial=tessera-devroot,config-wce=on \
    -drive file="$BSD_DIR/vm/crash-test.img",format=raw,cache=directsync,if=none,id=crashdrv \
    -device virtio-blk-pci,drive=crashdrv,serial=tessera-crashtest,config-wce=on \
    -drive file="$BSD_DIR/vm/tessera-storage.img",format=raw,cache=writeback,if=none,id=storagedrv \
    -device virtio-blk-pci,drive=storagedrv,serial=atrium-storage,config-wce=on \
    -device virtio-net-pci,netdev=net0 \
    -netdev user,id=net0,hostfwd=tcp::2222-:22 \
    -fsdev local,id=share,path="$SHARE_DIR",security_model=none \
    -device virtio-9p-pci,fsdev=share,mount_tag=bsd_share \
    -display none \
    -chardev socket,id=ser0,host=127.0.0.1,port=4444,server=on,wait=off,logfile=/tmp/tessera-serial.log \
    -serial chardev:ser0 \
    -monitor unix:/tmp/qmp.sock,server=on,wait=off
