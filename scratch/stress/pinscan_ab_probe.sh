#!/bin/sh
# Boot the multiuser Tessera root, reach a shell, and A/B the background
# pin-bitmap scan debounce: measure "pinscan done" rate over a fixed idle
# window with the debounce ON (default 1000ms) then OFF (legacy per-retire).
# Serial console is timestamped by the guest via `date +%s` markers so we
# can bound the idle windows precisely.
set -u
BSD_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
VARS="$BSD_DIR/vm/edk2-vars-mu.fd"
ROOT="$BSD_DIR/vm/tessera-root.img"
LOG="${1:-/tmp/mu-pinscan-ab.log}"
FIFO="/tmp/mu-ab.in"

cp "$BSD_DIR/vm/edk2-vars-blank.fd" "$VARS"
rm -f "$FIFO"; mkfifo "$FIFO"
: > "$LOG"

"$QEMU" \
    -L "$QEMU_DIR/pc-bios" \
    -accel hvf -cpu host -machine virt,gic-version=3 \
    -smp 4 -m 4096 \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$VARS" \
    -drive file="$ROOT",format=raw,cache=directsync,if=none,id=troot \
    -device virtio-blk-pci,drive=troot,serial=tessera-root,config-wce=on \
    -nographic -serial mon:stdio <"$FIFO" >"$LOG" 2>&1 &
QPID=$!
exec 3>"$FIFO"

ok=0
for i in $(seq 1 90); do
    grep -q "login:" "$LOG" && { ok=1; break; }
    grep -qiE "panic|mountroot>|Fatal|Enter full pathname" "$LOG" && break
    sleep 1
done
if [ "$ok" = "1" ]; then
    sleep 1
    printf 'root\n' >&3
    sleep 2
    # Confirm the knob exists and its default.
    printf 'echo KNOB=$(sysctl -n kern.tessera.pinscan_debounce_ms)\n' >&3
    sleep 1
    # ---- Window A: debounce ON (default 1000ms) ----
    printf 'echo AB_START_ON=$(date +%%s)\n' >&3
    sleep 30
    printf 'echo AB_END_ON=$(date +%%s)\n' >&3
    sleep 1
    # ---- Flip to legacy per-retire (0) ----
    printf 'sysctl kern.tessera.pinscan_debounce_ms=0\n' >&3
    sleep 1
    printf 'echo AB_START_OFF=$(date +%%s)\n' >&3
    sleep 30
    printf 'echo AB_END_OFF=$(date +%%s)\n' >&3
    sleep 1
    # Restore ON and dump a couple of meta stats.
    printf 'sysctl kern.tessera.pinscan_debounce_ms=1000\n' >&3
    printf 'echo DONE_AB_PROBE\n' >&3
    sleep 3
fi
sleep 1
kill "$QPID" 2>/dev/null || true
exec 3>&- ; rm -f "$FIFO"
