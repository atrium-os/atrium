#!/bin/sh
# The Mac's arm64 FreeBSD 16-CURRENT guest (~/atrium-vm, our qemu fork, HVF)
# with one or more REAL USB devices passed through by QEMU usb-host on an
# EHCI (or xHCI) controller: USB DACs for Lyra, or any other device a gate needs.
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
# controller's bus; the guest sees it attach as if plugged in at boot. While the VM
# runs, macOS cannot use the device (a DAC vanishes from Sound settings).
#
# !! WARNING (2026-11-13): with two full-speed DACs (DragonFly Red 21b4:0082
# and a PCM2704C 08bb:27c4) on the UHCI companions, playing through them
# PANICKED THE MAC: a kernel data abort in IOUSBHostFamily, panicked task
# qemu-system-aarch64. A macOS kernel bug, reached from qemu's usb-host
# (libusb) isochronous path as root. The high-speed Sparrow on EHCI played
# cleanly. Do not pass full-speed isochronous devices through again without
# the person's go-ahead.
#
# HCD picks the guest's host controller: ehci (default) or xhci. qemu's
# xHCI model mangles isochronous streams from usb-host: a UAC2 DAC (EarMen
# Sparrow) played a buzz where lyrad sent a clean 440 Hz tone, with no
# underrun anywhere in the guest. EHCI's iTD path is the one qemu's USB
# audio passthrough is known to work on; high-speed devices run at full
# speed on it, and full-speed ones go to its UHCI companions. ISOBUFS/ISOBSIZE (default 8/32) set usb-host's iso buffering.
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
HCD=${HCD:-ehci}
ISOBUFS=${ISOBUFS:-8}; ISOBSIZE=${ISOBSIZE:-32}
case $HCD in
    # ICH9's EHCI with its three UHCI companions (qemu's plain usb-ehci takes
    # no companions): full-speed devices (most UAC1 DACs, e.g. an AudioQuest
    # DragonFly at 12 Mb/s) are handed to a UHCI on the same ports.
    ehci) ctl="-device ich9-usb-ehci1,id=usbhc"
          for c in 1:0 2:2 3:4; do
              ctl="$ctl -device ich9-usb-uhci${c%:*},masterbus=usbhc.0,firstport=${c#*:}"
          done ;;
    xhci) ctl="-device qemu-xhci,id=usbhc" ;;
    *) echo "HCD must be ehci or xhci" >&2; exit 1 ;;
esac
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
    devs="$devs -device usb-host,bus=usbhc.0,vendorid=0x${d%%:*},productid=0x${d##*:},isobufs=$ISOBUFS,isobsize=$ISOBSIZE,id=usbdev$n"
done

rm -f qemu.pid serial.sock mon.sock
# shellcheck disable=SC2086
nohup "$QEMU" -machine virt,gic-version=3 -accel hvf -cpu host -smp 4 -m 8G \
  -drive if=pflash,format=raw,unit=0,file="$VMDIR/code.fd",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VMDIR/vars.fd" \
  -drive file="$VMDIR/vm.qcow2",format=qcow2,cache=writeback,if=none,id=zfsd -device virtio-blk-pci,drive=zfsd,bootindex=0 \
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=net0 \
  $ctl $devs \
  -display none -serial unix:"$VMDIR/serial.sock",server,nowait -monitor unix:"$VMDIR/mon.sock",server,nowait \
  -pidfile "$VMDIR/qemu.pid" > "$VMDIR/qemu-root.out" 2>&1 < /dev/null &
sleep 3
chmod 666 serial.sock mon.sock 2>/dev/null || true
chown "$U" qemu-root.out qemu.pid 2>/dev/null || true
if [ -f qemu.pid ] && kill -0 "$(cat qemu.pid)" 2>/dev/null; then
  echo "started qemu pid $(cat qemu.pid) with usb-host $USB on $HCD; log: $VMDIR/qemu-root.out"
else
  echo "qemu did not start:" >&2; cat qemu-root.out >&2; exit 1
fi
