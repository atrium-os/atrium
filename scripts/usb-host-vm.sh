#!/bin/sh
# The Mac's arm64 FreeBSD 16-CURRENT guest (~/atrium-vm, our qemu fork, HVF)
# with one or more REAL USB devices passed through by QEMU usb-host on an
# xHCI controller: USB DACs for Lyra, or any other device a gate needs.
# sigillum-fido-vm.sh is the one-device special case of this.
#
# qemu needs root for usb-host on macOS (else macOS keeps the interfaces,
# e.g. AppleUSBAudio holds a DAC, and the guest's SET_CONFIGURATION stalls):
#
#   sudo USB="08bb:27c4" sh scripts/usb-host-vm.sh     # starts qemu in the background
#   sh scripts/usb-dac-check.sh                         # then, as yourself
#
# USB is a space-separated list of vid:pid (find them with
# `system_profiler SPUSBDataType`). Each device gets its own usb-host on the
# xHCI bus; the guest sees it attach as if plugged in at boot. While the VM
# runs, macOS cannot use the device (a DAC vanishes from Sound settings).
#
# The VM's dir (VMDIR, default ~girivs/atrium-vm when run under sudo) gets
# qemu.pid, qemu-root.out and the serial/monitor sockets, made writable for
# the non-root user. Stop it with: echo system_powerdown | nc -U $VMDIR/mon.sock
# (a clean guest shutdown; `quit` is the hard stop).
set -eu
U=${SUDO_USER:-$(id -un)}
H=$(eval echo "~$U")
VMDIR=${VMDIR:-$H/atrium-vm}
USB=${USB:?set USB to a list of vid:pid, e.g. USB=08bb:27c4}
FORK=${ATRIUM_SRC:-$H/src/bsd}/external/qemu-build/build/qemu-system-aarch64
if [ -x "$FORK" ]; then QEMU=${QEMU:-$FORK}; else QEMU=${QEMU:-/opt/homebrew/bin/qemu-system-aarch64}; fi
[ "$(id -u)" = 0 ] || { echo "run as root (sudo): usb-host needs it on macOS" >&2; exit 1; }
cd "$VMDIR"
if [ -f qemu.pid ] && kill -0 "$(cat qemu.pid)" 2>/dev/null; then echo "VM already running (pid $(cat qemu.pid)); stop it first" >&2; exit 1; fi
if pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null; then echo "another qemu holds $VMDIR/vm.qcow2; not starting" >&2; exit 1; fi

devs=""; n=0
for d in $USB; do
    case $d in *:*) ;; *) echo "bad USB entry '$d' (want vid:pid)" >&2; exit 1 ;; esac
    n=$((n + 1))
    devs="$devs -device usb-host,bus=xhci.0,vendorid=0x${d%%:*},productid=0x${d##*:},id=usbdev$n"
done

rm -f qemu.pid serial.sock mon.sock
# shellcheck disable=SC2086
nohup "$QEMU" -machine virt,gic-version=3 -accel hvf -cpu host -smp 4 -m 8G \
  -drive if=pflash,format=raw,unit=0,file="$VMDIR/code.fd",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VMDIR/vars.fd" \
  -drive file="$VMDIR/vm.qcow2",format=qcow2,cache=writeback,if=none,id=zfsd -device virtio-blk-pci,drive=zfsd,bootindex=0 \
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=net0 \
  -device qemu-xhci,id=xhci $devs \
  -display none -serial unix:"$VMDIR/serial.sock",server,nowait -monitor unix:"$VMDIR/mon.sock",server,nowait \
  -pidfile "$VMDIR/qemu.pid" > "$VMDIR/qemu-root.out" 2>&1 < /dev/null &
sleep 3
chmod 666 serial.sock mon.sock 2>/dev/null || true
chown "$U" qemu-root.out qemu.pid 2>/dev/null || true
if [ -f qemu.pid ] && kill -0 "$(cat qemu.pid)" 2>/dev/null; then
  echo "started qemu pid $(cat qemu.pid) with usb-host $USB; log: $VMDIR/qemu-root.out"
else
  echo "qemu did not start:" >&2; cat qemu-root.out >&2; exit 1
fi
