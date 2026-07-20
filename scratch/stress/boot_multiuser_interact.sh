#!/bin/sh
# Boot the full-userland Tessera root (tessera-root.img) to MULTIUSER and drive
# the login: prompt over serial — proves rc(8) ran (incl. rc.d/root's
# `mount -uw /` = the ro->rw upgrade) and reached multiuser. Dev VM must be down.
set -u
BSD_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
VARS="$BSD_DIR/vm/edk2-vars-mu.fd"
ROOT="$BSD_DIR/vm/tessera-root.img"
LOG="${1:-/tmp/mu-boot.log}"
FIFO="/tmp/mu.in"

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

# wait for the multiuser login prompt (rc finished)
ok=0
for i in $(seq 1 90); do
    grep -q "login:" "$LOG" && { ok=1; break; }
    grep -qiE "panic|mountroot>|Fatal|Enter full pathname" "$LOG" && break
    sleep 1
done
if [ "$ok" = "1" ]; then
    sleep 1
    printf 'root\n' >&3            # passwordless root
    sleep 2
    printf 'echo MU_LOGIN_OK=$?\n' >&3
    printf 'mount\n' >&3
    printf 'sysctl -n kern.disks\n' >&3
    printf 'uname -a\n' >&3
    printf 'service -e | head\n' >&3
    printf 'echo DONE_MU_7727\n' >&3
    sleep 4
fi
sleep 1
kill "$QPID" 2>/dev/null || true
exec 3>&- ; rm -f "$FIFO"
