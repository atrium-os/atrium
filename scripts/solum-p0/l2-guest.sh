#!/bin/sh
# Runs INSIDE the Solum P0 host (the FreeBSD arm64 guest with EL2) as root.
# Boots a nested bhyve guest ("L2") from /root/l2/disk.raw and waits for its
# login prompt on the emulated UART. Driven by scripts/solum-p0-mac.sh l2.
#
#   sh l2-guest.sh setup            # u-boot for bhyve arm64, vmm loaded
#   sh l2-guest.sh start [KERNEL]   # boot L2 (optionally a kernel dir to stage first)
#   sh l2-guest.sh wait             # until "login:" appears (bounded)
#   sh l2-guest.sh stop
set -u
D=/root/l2
VM=l2
UBOOT=/usr/local/share/u-boot/u-boot-bhyve-arm64/u-boot.bin
die() { echo "FAILED: $*" >&2; exit 1; }

case "${1:-}" in
setup)
    kldstat -q -m vmm || kldload vmm || die "kldload vmm"
    [ -f "$UBOOT" ] || ASSUME_ALWAYS_YES=yes pkg install -y u-boot-bhyve-arm64 || die "pkg u-boot"
    [ -f "$UBOOT" ] || die "no $UBOOT after install"
    echo "ok: setup"
    ;;
start)
    [ -f "$D/disk.raw" ] || die "no $D/disk.raw"
    if [ -f "$D/bhyve.pid" ] && kill -0 "$(cat "$D/bhyve.pid")" 2>/dev/null; then die "L2 already running"; fi
    bhyvectl --vm="$VM" --destroy 2>/dev/null
    : > "$D/console.log"
    daemon -o "$D/console.log" -p "$D/bhyve.pid" \
        bhyve -c 2 -m 2G -o console=stdio -o bootrom="$UBOOT" \
        -s 0,hostbridge -s 1,virtio-blk,"$D/disk.raw" -s 2,virtio-rnd "$VM" \
        || die "bhyve"
    sleep 2
    kill -0 "$(cat "$D/bhyve.pid")" 2>/dev/null || { tail -40 "$D/console.log"; die "bhyve exited"; }
    echo "ok: L2 started (pid $(cat "$D/bhyve.pid"))"
    ;;
wait)
    i=0
    while [ $i -lt "${WAIT_S:-1800}" ]; do
        grep -q 'login:' "$D/console.log" 2>/dev/null && { echo "ok: L2 reached login after ~${i}s"; exit 0; }
        kill -0 "$(cat "$D/bhyve.pid" 2>/dev/null)" 2>/dev/null || { tail -60 "$D/console.log"; die "bhyve exited"; }
        i=$((i + 10)); sleep 10
    done
    tail -60 "$D/console.log"; die "no login prompt after ${WAIT_S:-1800}s"
    ;;
stop)
    bhyvectl --vm="$VM" --force-poweroff 2>/dev/null
    sleep 2
    bhyvectl --vm="$VM" --destroy 2>/dev/null
    rm -f "$D/bhyve.pid"; echo "ok: L2 stopped"
    ;;
*) sed -n '2,10p' "$0"; exit 2 ;;
esac
