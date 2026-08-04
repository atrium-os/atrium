#!/bin/sh
# Boot ONLY the self-contained tessera-root.img in a minimal QEMU (no ZFS,
# no other disks), capturing serial with host timestamps so the loader's
# kernel-load phase can be timed. Runs on the macOS host.
set -e
BSD_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
VARS_SRC="$BSD_DIR/vm/edk2-vars-blank.fd"   # erased flash: default boot policy
VARS="$BSD_DIR/vm/edk2-vars-tessboot.fd"
ROOT="$BSD_DIR/vm/tessera-root.img"     # boot the real image (dev VM must be down)
LOG="${1:-/tmp/tessboot-serial.log}"

cp "$VARS_SRC" "$VARS"          # fresh vars: no stale dev-VM boot order
: > "$LOG"

# timestamp each serial line (ms since epoch) via awk
"$QEMU" \
    -L "$QEMU_DIR/pc-bios" \
    -accel hvf -cpu host -machine virt,gic-version=3 \
    -smp 4 -m 2048 \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$VARS" \
    -drive file="$ROOT",format=raw,cache=directsync,if=none,id=troot \
    -device virtio-blk-pci,drive=troot,serial=tessera-root,config-wce=on \
    -nographic -serial mon:stdio 2>&1 \
  | awk '{ cmd="date +%s.%N"; cmd | getline t; close(cmd); printf "%s %s\n", t, $0; fflush() }' \
  | tee "$LOG"
