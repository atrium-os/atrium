#!/bin/sh
# Boot FreeBSD 16.0-CURRENT arm64 under modified qemu + HVF on macOS.
#
# Usage:
#   run-vm.sh                # boot without ivshmem (smoke test)
#   run-vm.sh --gpu          # boot with ivshmem-doorbell (requires gpu server running)
#   run-vm.sh --virtio-gpu       # add virtio-gpu-pci (D0 driver bring-up; no UI)
#   run-vm.sh --virtio-gpu --display  # virtio-gpu + a Cocoa window so scanout is visible
#   run-vm.sh --gpu --virtio-gpu  # both (transitional during D0)

set -eu

BSD_DIR="$(cd "$(dirname "$0")/.." && pwd)"
QEMU_DIR="/Users/girivs/src/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_SRC="$QEMU_DIR/build/qemu-bundle/opt/homebrew/share/qemu/edk2-aarch64-code.fd"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
EFI_VARS="$BSD_DIR/vm/edk2-arm-vars.fd"
DISK="$BSD_DIR/vm/vm.qcow2"
SHARE_DIR="$BSD_DIR"

# qemu wants exactly 64MiB for the EFI flash; pad if needed.
if [ ! -f "$EFI_PAD" ]; then
    cp "$EFI_SRC" "$EFI_PAD"
    # pad to 64 MiB
    truncate -s 67108864 "$EFI_PAD"
fi
if [ ! -f "$EFI_VARS" ]; then
    truncate -s 67108864 "$EFI_VARS"
fi

# Always blank EFI NVRAM. Adding/removing -device changes PCI addresses,
# which invalidates saved boot entries (EFI drops into UEFI shell). Forcing
# fresh ESP discovery every boot avoids that. Costs ~3s of boot time.
: > "$EFI_VARS"
truncate -s 67108864 "$EFI_VARS"

GPU_ARGS=""
VIRTIO_GPU_ARGS=""
DISPLAY_FRONTEND=""
NOGRAPHIC="-nographic"
for arg in "$@"; do
    case "$arg" in
        --gpu)
            SOCK="/tmp/fresco-shmem.sock"
            if [ ! -S "$SOCK" ]; then
                echo "error: $SOCK not found — start karythra-gpu-server first" >&2
                exit 1
            fi
            GPU_ARGS="-chardev socket,path=$SOCK,id=ivshmem \
                      -device ivshmem-doorbell,vectors=2,chardev=ivshmem"
            ;;
        --virtio-gpu)
            VIRTIO_GPU_ARGS="-device virtio-gpu-pci"
            ;;
        --display)
            # Cocoa window so virtio-gpu scanout is actually visible.
            # Drops -nographic; serial console moves to mon:stdio.
            # Also add a USB controller + keyboard + tablet so input
            # in the Cocoa window flows through to the guest. ukbd
            # claims usb-kbd → kbdmux0 → /dev/input/event0, which is
            # what atrium-compositor's input_reader thread reads.
            DISPLAY_FRONTEND="-display cocoa -serial mon:stdio \
                              -device qemu-xhci \
                              -device usb-kbd \
                              -device usb-tablet"
            NOGRAPHIC=""
            ;;
        *)
            echo "error: unknown arg $arg" >&2
            exit 1
            ;;
    esac
done

exec "$QEMU" \
    -accel hvf -cpu host -machine virt,gic-version=2 \
    -smp 4 -m 12288 \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$EFI_VARS" \
    -drive if=virtio,file="$DISK",format=qcow2,cache=writeback \
    -device virtio-net-pci,netdev=net0 \
    -netdev user,id=net0,hostfwd=tcp::2222-:22 \
    -fsdev local,id=share,path="$SHARE_DIR",security_model=none \
    -device virtio-9p-pci,fsdev=share,mount_tag=bsd_share \
    $NOGRAPHIC \
    $DISPLAY_FRONTEND \
    $GPU_ARGS \
    $VIRTIO_GPU_ARGS
