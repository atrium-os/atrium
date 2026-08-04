#!/bin/sh
# Boot the tessera-root.img and capture RAW serial (no line-based filter) so a
# trailing prompt with no newline (shell "# ", init "Enter full pathname…")
# is visible. Dev VM must be down. Runs on the macOS host.
set -e
BSD_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
VARS="$BSD_DIR/vm/edk2-vars-tessraw.fd"
ROOT="$BSD_DIR/vm/tessera-root.img"
LOG="${1:-/tmp/tessraw.log}"

cp "$BSD_DIR/vm/edk2-vars-blank.fd" "$VARS"
: > "$LOG"

"$QEMU" \
    -L "$QEMU_DIR/pc-bios" \
    -accel hvf -cpu host -machine virt,gic-version=3 \
    -smp 4 -m 2048 \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$VARS" \
    -drive file="$ROOT",format=raw,cache=directsync,if=none,id=troot \
    -device virtio-blk-pci,drive=troot,serial=tessera-root,config-wce=on \
    -nographic -serial mon:stdio 2>&1 | tee "$LOG"
