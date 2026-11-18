#!/bin/sh
# crypta-wg against Linux kernel WireGuard (docs/spec/vpn.md §11, V0): an
# Ubuntu 24.04 amd64 cloud VM (TCG, no KVM here) with wg0 at 10.99.0.1,
# and crypta-wg's udp_peer example on the host at 10.99.0.2 over a plain
# UDP socket (QEMU user networking: host 127.0.0.1:$HOST_PORT is the
# guest's 10.0.2.2, the guest's :51820 is forwarded to host :$WG_PORT).
#
#   sh scripts/crypta-wg-linux-vm.sh up      download (once), boot, wait for ssh
#   sh scripts/crypta-wg-linux-vm.sh check   both directions; ends "crypta-wg-linux: N failed"
#   sh scripts/crypta-wg-linux-vm.sh down
#
# wireguard-tools (GPL) is only a test tool in the VM, never shipped.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
D=${CRYPTA_LINUX_DIR:-$HOME/crypta-linux-vm}
SSH_PORT=${CRYPTA_LINUX_SSH:-2224}
HTTP_PORT=${CRYPTA_LINUX_HTTP:-8771}
WG_PORT=${CRYPTA_LINUX_WG:-51820}
HOST_PORT=${CRYPTA_LINUX_HOSTPORT:-41820}
KEY=$HOME/.ssh/fresco_bsd_ed25519
IMG_URL=https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img
DEB=http://archive.ubuntu.com/ubuntu/pool/main/w/wireguard/wireguard-tools_1.0.20210914-1ubuntu4_amd64.deb
QEMU=${QEMU_BIN:-qemu-system-x86_64}

gssh() {
    printf '%s\n' "$*" | ssh -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=60 -p "$SSH_PORT" ubuntu@localhost "sudo sh -s"
}

up() {
    mkdir -p "$D/www"
    [ -f "$D/base.img" ] || curl -fsSL -o "$D/base.img" "$IMG_URL"
    [ -f "$D/www/wg.deb" ] || curl -fsSL -o "$D/www/wg.deb" "$DEB"
    pub=$(cat "$KEY.pub")
    printf 'instance-id: crypta-linux-1\nlocal-hostname: crypta-linux\n' > "$D/www/meta-data"
    cat > "$D/www/user-data" <<EOF
#cloud-config
ssh_authorized_keys:
  - $pub
package_update: false
package_upgrade: false
runcmd:
  - systemctl disable --now snapd.service snapd.socket unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer ModemManager.service || true
  - curl -fsS -o /root/wg.deb http://10.0.2.2:$HTTP_PORT/wg.deb
  - dpkg -i --force-depends /root/wg.deb || true
  - modprobe wireguard
  - touch /var/lib/crypta-ready
EOF
    [ -f "$D/vm.qcow2" ] || qemu-img create -q -f qcow2 -F qcow2 -b "$D/base.img" "$D/vm.qcow2" 8G
    pkill -f "http.server $HTTP_PORT" 2>/dev/null || true
    (cd "$D/www" && nohup python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 >"$D/http.log" 2>&1 &)
    if ! [ -f "$D/qemu.pid" ] || ! kill -0 "$(cat "$D/qemu.pid")" 2>/dev/null; then
        $QEMU -machine q35 -accel tcg,thread=multi -cpu max -smp 2 -m 2048 \
            -drive if=virtio,format=qcow2,file="$D/vm.qcow2" \
            -netdev user,id=n0,hostfwd=tcp:127.0.0.1:$SSH_PORT-:22,hostfwd=udp:127.0.0.1:$WG_PORT-:51820 \
            -device virtio-net-pci,netdev=n0 \
            -smbios "type=1,serial=ds=nocloud;s=http://10.0.2.2:$HTTP_PORT/" \
            -display none -serial "file:$D/console.log" -daemonize -pidfile "$D/qemu.pid"
    fi
    i=0
    until gssh test -f /var/lib/crypta-ready 2>/dev/null; do
        i=$((i + 1)); [ $i -lt 120 ] || { echo "crypta-wg-linux: not ready after 20 min (see $D/console.log)" >&2; exit 1; }
        sleep 10
    done
    gssh "uname -r; wg --version"
}

check() {
    fails=0
    ok() { echo "  ok $*"; }
    bad() { echo "  FAIL $*"; fails=$((fails + 1)); }
    (cd "$ROOT/crypta/crypta-wg" && cargo build -q --release --example udp_peer)
    P="$ROOT/crypta/crypta-wg/target/release/examples/udp_peer"
    SEED=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
    OURS=$("$P" pubkey "$SEED")
    THEIRS=$(gssh "set -e
        ip link del wg0 2>/dev/null || true
        umask 077; wg genkey > /root/wg.key
        ip link add wg0 type wireguard
        wg set wg0 listen-port 51820 private-key /root/wg.key peer $OURS allowed-ips 10.99.0.2/32
        ip addr add 10.99.0.1/24 dev wg0; ip link set wg0 up
        wg pubkey < /root/wg.key" | tail -1)
    echo "== we initiate: 20 pings of growing size through Linux's wg0"
    if "$P" ping "$SEED" "$HOST_PORT" "127.0.0.1:$WG_PORT" "$THEIRS" 20 > "$D/ping.log" 2>&1; then
        ok "$(tail -1 "$D/ping.log")"
    else
        bad "$(tail -3 "$D/ping.log" | tr '\n' ' ')"
    fi
    grep -q 'HandshakeComplete { initiator: true }' "$D/ping.log" && ok "handshake as initiator" || bad "no handshake"
    gssh "wg show wg0" | tee "$D/wg-show-1.txt" | grep -q 'latest handshake' && ok "Linux saw the handshake" || bad "Linux shows no handshake"
    echo "== Linux initiates: it pings us"
    gssh "wg set wg0 peer $OURS remove; wg set wg0 peer $OURS allowed-ips 10.99.0.2/32 endpoint 10.0.2.2:$HOST_PORT"
    "$P" serve "$SEED" "$HOST_PORT" "$THEIRS" 40 > "$D/serve.log" 2>&1 &
    SP=$!
    sleep 1
    gssh "ping -c 5 -i 0.5 -W 5 10.99.0.2" > "$D/vm-ping.txt" 2>&1 || true
    wait $SP || true
    grep -q ' 0% packet loss' "$D/vm-ping.txt" && ok "Linux's 5 pings answered" || bad "Linux's ping: $(tail -2 "$D/vm-ping.txt" | tr '\n' ' ')"
    grep -q 'HandshakeComplete { initiator: false }' "$D/serve.log" && ok "handshake as responder" || bad "no responder handshake: $(tail -3 "$D/serve.log" | tr '\n' ' ')"
    echo "crypta-wg-linux: $fails failed"
    [ "$fails" = 0 ]
}

case "${1:-}" in
up) up ;;
check) check ;;
down) gssh poweroff || true; sleep 5; [ -f "$D/qemu.pid" ] && kill "$(cat "$D/qemu.pid")" 2>/dev/null || true ;;
*) echo "usage: $0 up|check|down" >&2; exit 2 ;;
esac
