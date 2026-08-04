#!/bin/sh
# Boot tessera-root.img and drive the single-user shell over the serial:
# send RETURN at the "Enter full pathname of shell" prompt, then run a few
# commands to prove the ZFS-free root is live. Dev VM must be down.
set -e
BSD_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
VARS="$BSD_DIR/vm/edk2-vars-tessint.fd"
ROOT="$BSD_DIR/vm/tessera-root.img"
LOG="${1:-/tmp/tessint.log}"
FIFO="/tmp/tessint.in"

cp "$BSD_DIR/vm/edk2-vars-blank.fd" "$VARS"
rm -f "$FIFO"; mkfifo "$FIFO"
: > "$LOG"

"$QEMU" \
    -L "$QEMU_DIR/pc-bios" \
    -accel hvf -cpu host -machine virt,gic-version=3 \
    -smp 4 -m 2048 \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$VARS" \
    -drive file="$ROOT",format=raw,cache=directsync,if=none,id=troot \
    -device virtio-blk-pci,drive=troot,serial=tessera-root,config-wce=on \
    -nographic -serial mon:stdio <"$FIFO" >"$LOG" 2>&1 &
QPID=$!
# hold the fifo open for writing
exec 3>"$FIFO"

# wait for the single-user shell prompt
for i in $(seq 1 40); do
    grep -q "Enter full pathname of shell" "$LOG" && break
    sleep 1
done
printf '\n' >&3                       # RETURN -> /rescue/sh
sleep 3
printf 'echo TESSERA_SHELL_LIVE=$?\n' >&3
printf 'uname -a\n' >&3
printf 'ls -la / \n' >&3
printf 'cat /ZFSFREE_MARKER\n' >&3
printf 'echo DONE_MARKER_9931\n' >&3
sleep 5
kill "$QPID" 2>/dev/null || true
exec 3>&-
rm -f "$FIFO"
