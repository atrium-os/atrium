#!/bin/sh
# The Sigillum S2 gate's VM with a REAL FIDO key (docs/spec/sigillum.md §16.2):
# the Mac's arm64 FreeBSD 16-CURRENT guest (~/atrium-vm, Homebrew qemu, HVF)
# with the key passed through by QEMU usb-host on an xHCI controller, in
# place of u2f-emulated. qemu needs root for usb-host on macOS (else macOS
# keeps the interfaces and the guest's SET_CONFIGURATION stalls), so:
#
#   sudo sh scripts/sigillum-fido-vm.sh          # starts qemu in the background
#   ARCH=aarch64 SIGILLUM_REAL_KEY=1 sh scripts/vm-sigillum-check.sh
#
# KEY=vid:pid picks the key (default 1050:0406, a YubiKey 5 FIDO+CCID).
# The VM's dir (VMDIR, default ~girivs/atrium-vm when run under sudo) gets
# qemu.pid, qemu-root.out and the serial/monitor sockets, made writable for
# the non-root gate. Stop it with: echo quit | nc -U $VMDIR/mon.sock
set -eu
U=${SUDO_USER:-$(id -un)}
H=$(eval echo "~$U")
VMDIR=${VMDIR:-$H/atrium-vm}
KEY=${KEY:-1050:0406}
VID=${KEY%%:*}; PID=${KEY##*:}
QEMU=${QEMU:-/opt/homebrew/bin/qemu-system-aarch64}
[ "$(id -u)" = 0 ] || { echo "run as root (sudo): usb-host needs it on macOS" >&2; exit 1; }
cd "$VMDIR"
if [ -f qemu.pid ] && kill -0 "$(cat qemu.pid)" 2>/dev/null; then echo "VM already running (pid $(cat qemu.pid))"; exit 0; fi
if pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null; then echo "another qemu holds $VMDIR/vm.qcow2; not starting" >&2; exit 1; fi
rm -f qemu.pid serial.sock mon.sock
nohup "$QEMU" -machine virt,gic-version=3 -accel hvf -cpu host -smp 4 -m 8G \
  -drive if=pflash,format=raw,unit=0,file="$VMDIR/code.fd",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VMDIR/vars.fd" \
  -drive file="$VMDIR/vm.qcow2",format=qcow2,cache=writeback,if=none,id=zfsd -device virtio-blk-pci,drive=zfsd,bootindex=0 \
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=net0 \
  -device qemu-xhci,id=xhci -device usb-host,bus=xhci.0,vendorid=0x$VID,productid=0x$PID,id=fidokey \
  -display none -serial unix:"$VMDIR/serial.sock",server,nowait -monitor unix:"$VMDIR/mon.sock",server,nowait \
  -pidfile "$VMDIR/qemu.pid" > "$VMDIR/qemu-root.out" 2>&1 < /dev/null &
sleep 3
chmod 666 serial.sock mon.sock 2>/dev/null || true
chown "$U" qemu-root.out qemu.pid 2>/dev/null || true
if [ -f qemu.pid ] && kill -0 "$(cat qemu.pid)" 2>/dev/null; then
  echo "started qemu pid $(cat qemu.pid) with usb-host $VID:$PID; log: $VMDIR/qemu-root.out"
else
  echo "qemu did not start:" >&2; cat qemu-root.out >&2; exit 1
fi
