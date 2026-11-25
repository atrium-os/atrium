#!/bin/sh
# The Linux test access point for Aura's sim gate (docs/spec/wifi.md §6.2):
# an Ubuntu 24.04 arm64 VM running hostapd on a mac80211_hwsim radio, with
# aura/sim-ap/air-hwsim.py as the wireless medium. Frames cross to aura-air
# in the FreeBSD VM over UDP: guest -> 10.0.2.2:$AIR_PORT (the host) ->
# the FreeBSD VM's hostfwd (scripts/aura-sim-gate.sh starts that VM with it).
#
#   sh scripts/aura-ap-vm.sh up        download (once), boot, install, wait for ssh
#   sh scripts/aura-ap-vm.sh ap [CONF...] start hwsim + medium + hostapd + udhcpd
#                                       (CONF: files in aura/sim-ap, default
#                                       hostapd-wpa2.conf; one radio and BSS
#                                       each, all bridged on br0 = 10.77.0.1)
#                                       (HOSTAPD_ARGS=-dd: hostapd's debug log)
#   sh scripts/aura-ap-vm.sh signal FREQ DBM [FREQ DBM...]
#                                       what the FreeBSD side hears per channel
#   sh scripts/aura-ap-vm.sh blackout SECS    # every channel out of range for SECS s (timed in the VM)
#   sh scripts/aura-ap-vm.sh wired     a wired 802.1X switch port on the cable NIC (hostapd
#                                       driver=wired; DHCP 10.79.0.0/24 only while signed in)
#   sh scripts/aura-ap-vm.sh wired-log the wired port's hostapd, open/close and DHCP logs
#   sh scripts/aura-ap-vm.sh log       hostapd and medium logs
#   sh scripts/aura-ap-vm.sh ssh CMD   run a command in the VM as root
#   sh scripts/aura-ap-vm.sh down      power off
#
# No KVM in the cloud container: TCG, -cpu neoverse-n2 (stock QEMU's MOPS
# bug, RUNBOOK §13). First boot with package install takes ~5-8 min.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
D=${AURA_AP_DIR:-$HOME/aura-ap-vm}
KVER=6.8.0-147
SSH_PORT=${AURA_AP_SSH:-2223}
HTTP_PORT=${AURA_AP_HTTP:-8770}
AIR_PORT=${AIR_PORT:-7701}
KEY=$HOME/.ssh/fresco_bsd_ed25519
IMG_URL=https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img
PORTS=http://ports.ubuntu.com/ubuntu-ports/pool
DEBS="main/l/linux/linux-modules-extra-${KVER}-generic_${KVER}.${KVER#6.8.0-}_arm64.deb
universe/w/wpa/hostapd_2.10-21ubuntu0.4_arm64.deb
main/i/iw/iw_6.7-1build1_arm64.deb"

# The command goes over stdin, so it may hold any quotes.
gssh() {
    printf '%s\n' "$*" | ssh -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=60 -o ServerAliveInterval=30 -p "$SSH_PORT" ubuntu@localhost "sudo sh -s"
}

fetch() {
    mkdir -p "$D/www"
    [ -f "$D/base.img" ] || curl -fsSL -o "$D/base.img" "$IMG_URL"
    for p in $DEBS; do
        f="$D/www/$(basename "$p")"
        [ -f "$f" ] || curl -fsSL -o "$f" "$PORTS/$p"
    done
    # The cloud image must run the kernel the modules are for.
    curl -fsSL "${IMG_URL%.img}.manifest" | grep -q "^linux-image-${KVER}-generic" || {
        echo "aura-ap-vm: cloud image kernel is no longer ${KVER}; update KVER and DEBS" >&2; exit 1; }
}

seed() {
    pub=$(cat "$KEY.pub")
    cat > "$D/www/meta-data" <<EOF
instance-id: aura-ap-1
local-hostname: aura-ap
EOF
    cat > "$D/www/user-data" <<EOF
#cloud-config
ssh_authorized_keys:
  - $pub
package_update: false
package_upgrade: false
runcmd:
  - systemctl disable --now snapd.service snapd.socket unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer ModemManager.service udisks2.service || true
  - mkdir -p /root/debs
  - for f in $(for p in $DEBS; do printf '%s ' "$(basename "$p")"; done); do curl -fsS -o /root/debs/\$f http://10.0.2.2:$HTTP_PORT/\$f; done
  - dpkg -i --force-depends /root/debs/*.deb || true
  - depmod -a
  - systemctl disable --now hostapd.service || true
  - touch /var/lib/aura-ap-ready
EOF
}

up() {
    fetch
    seed
    if ! [ -f "$D/vm.qcow2" ]; then
        qemu-img create -q -f qcow2 -F qcow2 -b "$D/base.img" "$D/vm.qcow2" 8G
        cp /usr/share/AAVMF/AAVMF_CODE.no-secboot.fd "$D/code.fd"
        cp /usr/share/AAVMF/AAVMF_VARS.fd "$D/vars.fd"
    fi
    pkill -f "http.server $HTTP_PORT" 2>/dev/null || true
    (cd "$D/www" && nohup python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 >"$D/http.log" 2>&1 &)
    if ! [ -f "$D/qemu.pid" ] || ! kill -0 "$(cat "$D/qemu.pid")" 2>/dev/null; then
        qemu-system-aarch64 -machine virt,gic-version=3 -accel tcg,thread=multi -cpu neoverse-n2 \
            -smp "${AURA_AP_SMP:-2}" -m 2048 \
            -drive if=pflash,format=raw,unit=0,file="$D/code.fd",readonly=on \
            -drive if=pflash,format=raw,unit=1,file="$D/vars.fd" \
            -drive if=virtio,format=qcow2,file="$D/vm.qcow2" \
            -netdev user,id=n0,hostfwd=tcp:127.0.0.1:$SSH_PORT-:22 -device virtio-net-pci,netdev=n0 \
            -netdev socket,id=cable,udp=127.0.0.1:7812,localaddr=127.0.0.1:7811 -device virtio-net-pci,netdev=cable,mac=02:4e:00:00:02:01 \
            -smbios "type=1,serial=ds=nocloud;s=http://10.0.2.2:$HTTP_PORT/" \
            -display none -serial "file:$D/console.log" -daemonize -pidfile "$D/qemu.pid"
    fi
    i=0
    until gssh test -f /var/lib/aura-ap-ready 2>/dev/null; do
        i=$((i + 1)); [ $i -lt 120 ] || { echo "aura-ap-vm: not ready after 20 min (see $D/console.log)" >&2; exit 1; }
        sleep 10
    done
    echo "aura-ap-vm: up ($(gssh uname -r))"
}

ap() {
    [ $# -gt 0 ] || set -- hostapd-wpa2.conf
    for f in air-hwsim.py udhcpd.conf inet.py ra6.py "$@"; do
        scp -q -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
            -P "$SSH_PORT" "$ROOT/aura/sim-ap/$f" ubuntu@localhost:/tmp/
    done
    # The radios stay loaded between runs (unloading hwsim takes minutes
    # under TCG): two radios, wlan0 and wlan1; a config only uses its own.
    gssh "systemctl disable --now ModemManager.service udisks2.service >/dev/null 2>&1; \
        pkill -x hostapd; pkill -f [a]ir-hwsim; pkill -f [u]dhcpd; sleep 1; rm -f /tmp/air-signal /run/udhcpd.leases; true"
    # One radio per config: config i runs on wlan<i>, every BSS on br0.
    confs="" i=0
    for f in "$@"; do
        gssh "sed -e 's/^interface=.*/interface=wlan$i/' /tmp/$f > /tmp/ap$i.conf && echo bridge=br0 >> /tmp/ap$i.conf"
        confs="$confs /tmp/ap$i.conf"
        i=$((i + 1))
    done
    [ "$i" -le 2 ] || { echo "aura-ap-vm: at most 2 configs" >&2; exit 2; }
    gssh "set -e; [ \$(ls /sys/class/ieee80211 2>/dev/null | wc -l) -ge 2 ] || { rmmod mac80211_hwsim 2>/dev/null || true; depmod -a; modprobe mac80211_hwsim radios=2; sleep 2; }; \
        ip link show br0 >/dev/null 2>&1 || { ip link add br0 type bridge; ip link set br0 up; ip addr add 10.77.0.1/24 dev br0; }; \
        (nohup python3 /tmp/air-hwsim.py 10.0.2.2 $AIR_PORT >/tmp/air.log 2>&1 &) && sleep 1 && \
        (nohup hostapd ${HOSTAPD_ARGS:-}$confs >/tmp/hostapd.log 2>&1 &) && sleep 4 && \
        (nohup busybox udhcpd -f /tmp/udhcpd.conf >/tmp/udhcpd.log 2>&1 &) && true"
    t=0
    until [ "$(gssh "grep -c AP-ENABLED /tmp/hostapd.log" || echo 0)" -ge "$i" ]; do
        t=$((t + 1)); [ $t -lt 30 ] || { gssh "cat /tmp/hostapd.log /tmp/air.log"; exit 1; }
        sleep 3
    done
    echo "aura-ap-vm: AP up ($*)"
}

# Wired 802.1X (wifi.md §7.5): the NIC on the cable to the FreeBSD VM
# (QEMU socket netdev, MAC 02:4e:00:00:02:01) becomes a switch port that
# asks for a sign-in: hostapd with driver=wired, and DHCP only while
# authorized (wired-port.sh). Run after `ap`, which stops every hostapd.
wired() {
    for f in hostapd-wired.conf udhcpd-wired.conf wired-port.sh; do
        scp -q -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
            -P "$SSH_PORT" "$ROOT/aura/sim-ap/$f" ubuntu@localhost:/tmp/
    done
    gssh "set -e; n=\$(grep -l 02:4e:00:00:02:01 /sys/class/net/*/address | cut -d/ -f5); [ -n \"\$n\" ] || { echo 'no cable NIC: aura-ap-vm.sh up boots with it'; exit 1; }
        pkill -f '[h]ostapd /tmp/hostapd-wired' || true; pkill -f '[h]ostapd_cli -i' || true; pkill -f '[u]dhcpd -f /tmp/udhcpd-wired.conf' || true; sleep 1
        ip addr flush dev \$n; ip addr add 10.79.0.1/24 dev \$n; ip link set \$n up
        sed -i \"s/cable0/\$n/\" /tmp/hostapd-wired.conf /tmp/udhcpd-wired.conf; chmod 755 /tmp/wired-port.sh
        rm -f /run/udhcpd-wired.leases /tmp/wired-port.log /tmp/udhcpd-wired.log; touch /run/udhcpd-wired.leases
        (nohup hostapd ${HOSTAPD_ARGS:-} /tmp/hostapd-wired.conf >/tmp/hostapd-wired.log 2>&1 &)
        i=0; until [ -S /run/hostapd-wired/\$n ]; do i=\$((i+1)); [ \$i -lt 30 ] || { cat /tmp/hostapd-wired.log; exit 1; }; sleep 1; done
        hostapd_cli -B -p /run/hostapd-wired -i \$n -a /tmp/wired-port.sh
        echo \"aura-ap-vm: wired 802.1X port on \$n\""
}

signal() {
    l=""
    while [ $# -ge 2 ]; do l="$l$1 $2\n"; shift 2; done
    gssh "printf '$l' > /tmp/air-signal.new && mv /tmp/air-signal.new /tmp/air-signal"
}

# Every channel out of range for SECS seconds, timed inside the AP VM (an ssh
# round trip under TCG can take most of a minute), then the old signals back.
blackout() {
    l=""
    for f in 2412 2417 2422 2427 2432 2437 2442 2447 2452 2457 2462 2467 2472 2484; do l="$l$f -100\n"; done
    gssh "cp /tmp/air-signal /tmp/air-signal.keep 2>/dev/null || : > /tmp/air-signal.keep
        printf '$l' > /tmp/air-signal.new && mv /tmp/air-signal.new /tmp/air-signal
        nohup sh -c 'sleep $1; cp /tmp/air-signal.keep /tmp/air-signal.new && mv /tmp/air-signal.new /tmp/air-signal' </dev/null >/dev/null 2>&1 &"
}

case "${1:-}" in
up) up ;;
blackout) blackout "$2" ;;
ap) shift; ap "$@" ;;
signal) shift; signal "$@" ;;
wired) wired ;;
wired-log) gssh "tail -n 80 /tmp/hostapd-wired.log; echo ---; cat /tmp/wired-port.log; echo ---; tail -n 20 /tmp/udhcpd-wired.log" ;;
log) gssh "tail -n 60 /tmp/hostapd.log; echo ---; tail -n 20 /tmp/air.log; echo ---; tail -n 20 /tmp/udhcpd.log" ;;
ssh) shift; gssh "$*" ;;
down) gssh "poweroff" || true; sleep 5; [ -f "$D/qemu.pid" ] && kill "$(cat "$D/qemu.pid")" 2>/dev/null || true ;;
*) sed -n 2,20p "$0"; exit 2 ;;
esac
