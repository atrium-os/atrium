#!/bin/sh
# Crypta V1 gate (docs/spec/vpn.md §11): the FreeBSD amd64 VM's tunnel to a
# Linux kernel WireGuard peer, and the leak checks.
#
#   ARCH=amd64 sh scripts/atrium-net-gate.sh vm     the FreeBSD VM, LAN NIC on vtnet1
#   sh scripts/crypta-wg-linux-vm.sh up              the Ubuntu VM (wg0, the peer)
#   sh scripts/crypta-v1-gate.sh run                 build, install, check
#
# The LAN is atrium-net/sim/lan.py. Its "WireGuard gateway" 198.51.100.51:51820
# relays to the Ubuntu VM's forwarded :51820, and it counts every packet the
# guest puts on the LAN (`seen`): the leak checks read that, not the guest.
#
# In the VM: atrium-net on vtnet1, a gate arcad (machine store, cryptad's
# key helper as uid 65047), cryptad, and the new jaild and pf base rules.
# On Ubuntu: wg0 10.99.0.1/24, HTTP on :80 (also on 203.0.113.80, a dummy
# address only reachable through the tunnel) and DNS on 10.99.0.1:53.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T=x86_64-unknown-freebsd
V="$ROOT/scripts/vssh"
export VSSH_CONNECT_TIMEOUT="${VSSH_CONNECT_TIMEOUT:-30}"
LAN_LOG="${LAN_LOG:-$HOME/atrium-vm-amd64/lan.log}"
G=/tmp/crypta-gate
KEY=$HOME/.ssh/fresco_bsd_ed25519
LSSH=${CRYPTA_LINUX_SSH:-2224}
CTL="env CRYPTA_SOCK=$G/crypta.sock /usr/local/bin/atrium-cryptad ctl"

pass=0
ok() { echo "ok   $1"; pass=$((pass + 1)); }
fail() {
    echo "FAIL $1"
    "$V" "tail -n 30 $G/net.log; echo ---; tail -n 30 $G/cryptad.log" || true
    tail -n 10 "$LAN_LOG" || true
    exit 1
}
gssh() {
    ssh -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=60 -p "$LSSH" ubuntu@localhost "sudo sh -s"
}
lanctl() {
    python3 - "$1" <<'PY'
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(5)
s.sendto(sys.argv[1].encode(), ("127.0.0.1", 7803)); print(s.recv(65536).decode())
PY
}
stat() { lanctl stats | python3 -c "import json,sys; print(json.load(sys.stdin).get('$1', 0))"; }
# What the guest sent besides the tunnel's own datagrams (and DHCP, ARP, ND).
leaks() { lanctl seen | python3 -c "import json,sys; d=json.load(sys.stdin); print(' '.join(f'{k}={v}' for k,v in sorted(d.items()) if k != 'udp 198.51.100.51:51820'))"; }
tunnel() { "$V" "$CTL list" | awk -v k="$1" '/^tunnel /{for(i=2;i<=NF;i++){split($i,a,"=");if(a[1]==k)print a[2]}}'; }
cleanup() {
    "$V" "pkill -f 'atrium-cryptad -v'; sleep 1; pkill -f 'atrium-net -v'; pkill -f 'atrium-arcad-gate'; sleep 1
        pfctl -a atrium/vpn -F all >/dev/null 2>&1
        for a in \$(ifconfig vtnet1 inet | awk '/inet /{print \$2}'); do ifconfig vtnet1 inet \$a -alias; done
        for t in \$(ifconfig -l); do case \$t in tun*) ifconfig \$t destroy ;; esac; done
        g=\$(cat $G/default.saved 2>/dev/null); [ -n \"\$g\" ] && { while route -q delete default 2>/dev/null; do :; done; route -q add default \$g; }
        ifconfig vtnet0 group atrium_egress; ifconfig vtnet1 -group atrium_egress 2>/dev/null
        [ -f $G/resolv.conf.saved ] && cp $G/resolv.conf.saved /etc/resolv.conf
        true" || true
}

ubuntu_peer() { # $1: our public key; prints the server's
    gssh <<EOF | tail -1
set -e
ip link del wg0 2>/dev/null || true
ip link del dummy0 2>/dev/null || true
pkill -f crypta-gate-dns 2>/dev/null || true
pkill -f 'http.server 80' 2>/dev/null || true
umask 077; wg genkey > /root/wg.key
ip link add wg0 type wireguard
wg set wg0 listen-port 51820 private-key /root/wg.key peer $1 allowed-ips 10.99.0.2/32
ip addr add 10.99.0.1/24 dev wg0; ip link set wg0 up
ip link add dummy0 type dummy; ip addr add 203.0.113.80/32 dev dummy0; ip link set dummy0 up
mkdir -p /root/www; echo crypta-v1-ok > /root/www/index.html
cat > /root/crypta-gate-dns.py <<'PY'
# A, one question: *.corp.test -> 10.99.0.1, anything else -> 203.0.113.80.
import socket, struct, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("10.99.0.1", 53))
log = open("/root/dns.log", "a")
while True:
    q, a = s.recvfrom(512)
    if len(q) < 17: continue
    i, labels = 12, []
    while i < len(q) and q[i]:
        labels.append(q[i + 1:i + 1 + q[i]].decode("ascii", "replace").lower()); i += 1 + q[i]
    name = ".".join(labels); qt = struct.unpack("!H", q[i + 1:i + 3])[0]
    print(name, qt, a[0], file=log, flush=True)
    ip = bytes([10, 99, 0, 1]) if name.endswith("corp.test") else bytes([203, 0, 113, 80])
    hdr = q[:2] + struct.pack("!HHHHH", 0x8180, 1, 1 if qt == 1 else 0, 0, 0)
    r = hdr + q[12:i + 5]
    if qt == 1: r += b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 60, 4) + ip
    s.sendto(r, a)
PY
: > /root/dns.log
nohup python3 /root/crypta-gate-dns.py crypta-gate-dns >/root/dns.out 2>&1 &
nohup python3 -m http.server 80 --bind 0.0.0.0 --directory /root/www >/root/http.out 2>&1 &
sleep 1
wg pubkey < /root/wg.key
EOF
}

case "${1:-}" in
run)
    CROSS_RUSTFLAGS=$(set -- env; ARCH=amd64 . "$ROOT/scripts/cloud-vm.sh" >/dev/null; printf %s "$CROSS_RUSTFLAGS")
    export RUSTFLAGS="$CROSS_RUSTFLAGS"
    (cd "$ROOT/atrium-net" && cargo build -q --release --target $T -p atrium-net -p atrium-lease -p atrium-mdns)
    (cd "$ROOT/crypta" && cargo build -q --release --target $T -p cryptad -p crypta-engine)
    (cd "$ROOT/arca/arcad" && cargo build -q --release --target $T --bin arcad)
    (cd "$ROOT/portcullis" && cargo build -q --release --target $T -p jaild)
    unset RUSTFLAGS
    S=$(mktemp -d)
    cp "$ROOT/atrium-net/target/$T/release/atrium-net" "$ROOT/atrium-net/target/$T/release/atrium-lease" \
        "$ROOT/atrium-net/target/$T/release/atrium-mdns" "$ROOT/atrium-net/etc/unbound-atrium-local.conf" \
        "$ROOT/crypta/target/$T/release/atrium-cryptad" "$ROOT/crypta/target/$T/release/atrium-crypta-wg" \
        "$ROOT/arca/arcad/target/$T/release/arcad" "$ROOT/portcullis/target/$T/release/atrium-jaild" \
        "$ROOT/etc/jaild.policy.toml" "$ROOT/etc/atrium.pf" "$S/"
    tar czf - -C "$S" . | "$V" "rm -rf $G && mkdir -p $G && tar xzf - -C $G"
    rm -rf "$S"

    pkill -f atrium-net/sim/lan.py 2>/dev/null || true
    python3 "$ROOT/atrium-net/sim/lan.py" 60 > "$LAN_LOG" 2>&1 &
    LANPID=$!
    trap 'cleanup; kill $LANPID 2>/dev/null || true' EXIT
    sleep 1

    echo "== install: jaild (crypta engines), pf base rules (atrium/vpn), the binaries"
    "$V" "set -e
        cd $G
        service atrium-jaild stop >/dev/null 2>&1 || true
        install -m 755 atrium-jaild /usr/local/bin/atrium-jaild
        install -m 644 jaild.policy.toml /etc/atrium/jaild.policy.toml
        service atrium-jaild start >/dev/null
        install -m 644 atrium.pf /usr/local/etc/atrium/pf.conf
        pfctl -f /usr/local/etc/atrium/pf.conf
        install -m 755 atrium-cryptad /usr/local/bin/atrium-cryptad
        install -m 755 atrium-crypta-wg /usr/local/bin/atrium-crypta-wg
        install -m 755 arcad /usr/local/bin/atrium-arcad-gate
        id _atrium-lease >/dev/null 2>&1 || pw useradd _atrium-lease -u 65034 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net lease parser'
        id _atrium-mdns >/dev/null 2>&1 || pw useradd _atrium-mdns -u 65037 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net .local names'
        id _crypta-key >/dev/null 2>&1 || pw useradd _crypta-key -u 65047 -d /nonexistent -s /usr/sbin/nologin -c 'cryptad key helper'
        mkdir -p /var/empty /var/run/atrium
        if [ ! -f /var/unbound/unbound.conf ]; then local-unbound-setup >/dev/null || true; fi
        mkdir -p /var/unbound/conf.d
        printf 'server:\n    local-zone: \"test.\" nodefault\n    domain-insecure: \"test.\"\n' > /var/unbound/conf.d/atrium-test.conf
        cp unbound-atrium-local.conf /var/unbound/conf.d/atrium-local.conf
        sysrc local_unbound_enable=YES >/dev/null
        service local_unbound restart >/dev/null 2>&1 || service local_unbound start >/dev/null
        cp /etc/resolv.conf $G/resolv.conf.saved
        printf 'nameserver 127.0.0.1\n' > /etc/resolv.conf
        route -n get default 2>/dev/null | awk '/gateway/{print \$2}' > $G/default.saved
        pkill -f 'atrium-net -v' 2>/dev/null || true
        for a in \$(ifconfig vtnet1 inet | awk '/inet /{print \$2}'); do ifconfig vtnet1 inet \$a -alias; done
        daemon -f -o $G/net.log $G/atrium-net -v --iface vtnet1 --no-probe --capsicum --lease $G/atrium-lease --mdns-bin $G/atrium-mdns --sock $G/net.sock
        mkdir -p -m 700 $G/arca; chmod 755 $G
        : > $G/apps.registry
        env ARCA_DIR=$G/arca ARCA_SOCKET=$G/arcad.sock ARCA_APP_REGISTRY=$G/apps.registry ARCA_SERVICES=cryptad=65047 ARCA_SIGILLUM_SOCKET=none \
            daemon -f -o $G/arcad.log /usr/local/bin/atrium-arcad-gate
        sleep 1
        daemon -f -o $G/cryptad.log /usr/local/bin/atrium-cryptad -v --sock $G/crypta.sock --state $G/state --net-sock $G/net.sock --arca $G/arcad.sock
        sleep 1"
    i=0; until "$V" 'ifconfig vtnet1 inet' | grep -q 10.88.0.50; do i=$((i + 1)); [ $i -lt 60 ] || fail "no LAN address"; sleep 2; done
    "$V" 'pfctl -sn' | grep -q 'nat-anchor "atrium/vpn"' && ok "pf base rules carry nat-anchor atrium/vpn" || fail "no nat-anchor"
    "$V" 'pgrep -f "atrium-cryptad -v" >/dev/null' && ok "cryptad up" || fail "cryptad not running"
    kh=$("$V" 'procstat -s $(pgrep -f -- "cryptad --arca-key") | tail -1' | awk '{print $3, $4, $6, $NF}')
    [ "$kh" = "65047 65047 65047 65047" ] && ok "key helper runs as _crypta-key with no other groups" || fail "key helper: $kh"

    echo "== import a .conf: split tunnel to 10.99.0.0/24, DNS for corp.test"
    IMP=$(mktemp)
    PRIV=$(python3 -c 'import os,base64; print(base64.b64encode(os.urandom(32)).decode())')
    conf() { # $1 AllowedIPs, $2 DNS line
        printf '[Interface]\nPrivateKey = %s\nAddress = 10.99.0.2/24\nDNS = %s\nPostUp = touch /tmp/pwned\n\n[Peer]\nPublicKey = %s\nEndpoint = 198.51.100.51:51820\nAllowedIPs = %s\nPersistentKeepalive = 25\n' \
            "$PRIV" "$2" "$SRV" "$1"
    }
    # The server's key first needs ours: a throwaway import, then the real ones.
    SRV=$(python3 -c 'import os,base64; print(base64.b64encode(os.urandom(32)).decode())')
    conf 10.99.0.0/24 "10.99.0.1, corp.test" | "$V" "$CTL import name=probe" > "$IMP" || fail "import: $(cat "$IMP")"
    PUB=$(sed -n 's/^imported .*public=\([^ ]*\).*/\1/p' "$IMP")
    [ -n "$PUB" ] && ok "imported; public key $PUB (the private key now lives only in Arca)" || fail "no public key: $(cat "$IMP")"
    "$V" "$CTL delete name=probe" >/dev/null
    "$V" "grep -rq '$PRIV' $G/state" && fail "the private key is on disk in cryptad's state" || ok "the private key is not in cryptad's state"
    "$V" "test -e /tmp/pwned" && fail "PostUp ran" || ok "PostUp ignored"
    SRV=$(ubuntu_peer "$PUB")
    [ -n "$SRV" ] && ok "Ubuntu wg0 up, peer = our key; server key $SRV" || fail "Ubuntu peer"
    conf 10.99.0.0/24 "10.99.0.1, corp.test" | "$V" "$CTL import name=split" >/dev/null || fail "import split"
    conf 0.0.0.0/0 "10.99.0.1" | "$V" "$CTL import name=full block=1" >/dev/null || fail "import full"

    echo "== split tunnel"
    "$V" "$CTL up name=split" >/dev/null || fail "up split"
    i=0; until [ "$(tunnel handshake | head -1)" = 1 ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "no handshake: $("$V" "$CTL list")"; sleep 1; done
    ok "handshake with Linux's wg0 ($(tunnel iface | head -1))"
    r=$("$V" 'fetch -T 10 -qo - http://10.99.0.1/' 2>&1) && [ "$r" = crypta-v1-ok ] && ok "HTTP to 10.99.0.1 through the tunnel" || fail "HTTP 10.99.0.1: $r"
    a=$("$V" 'drill web.corp.test @127.0.0.1 | awk "/^web.corp.test/{print \$5}"')
    [ "$a" = 10.99.0.1 ] && ok "web.corp.test resolved by the tunnel's DNS: $a" || fail "corp.test: $a ($("$V" 'local-unbound-control list_forwards'))"
    a=$("$V" 'drill gate.atrium.test @127.0.0.1 | awk "/^gate.atrium.test/{print \$5}"')
    [ "$a" = 10.88.0.77 ] && ok "other names still go to the LAN's DNS: $a" || fail "gate.atrium.test: $a"
    gw=$("$V" 'route -n get default | awk "/gateway/{print \$2}"')
    [ "$gw" = 10.88.0.1 ] && ok "split: the default route stays on the LAN" || fail "default route $gw"
    lanctl seen-reset >/dev/null
    "$V" 'ping -c 1 -t 3 10.88.0.1' >/dev/null 2>&1 || true
    case " $(leaks) " in *" icmp 10.88.0.1="*) ok "the leak counter sees ordinary LAN traffic (control)" ;; *) fail "leak counter blind: $(leaks)" ;; esac
    "$V" "$CTL down name=split" >/dev/null || fail "down split"
    "$V" 'local-unbound-control list_forwards' | grep -q corp.test && fail "corp.test forward left behind" || ok "down: corp.test forward removed"

    echo "== full tunnel, block when not connected"
    "$V" "$CTL up name=full" >/dev/null || fail "up full"
    i=0; until [ "$(tunnel handshake | sed -n 1p)" = 1 ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "no handshake: $("$V" "$CTL list")"; sleep 1; done
    ok "full tunnel up"
    # Counted from here: what was in flight as the tunnel came up was sent
    # before it was up.
    lanctl seen-reset >/dev/null
    r=$("$V" 'fetch -T 10 -qo - http://203.0.113.80/' 2>&1) && [ "$r" = crypta-v1-ok ] && ok "HTTP to 203.0.113.80 (only reachable through the tunnel)" || fail "HTTP 203.0.113.80: $r"
    a=$("$V" 'drill anything.example.test @127.0.0.1 | awk "/^anything.example.test/{print \$5}"')
    [ "$a" = 203.0.113.80 ] && ok "all DNS through the tunnel: $a" || fail "full-tunnel DNS: $a"
    "$V" 'fetch -T 5 -qo /dev/null http://198.51.100.80/' >/dev/null 2>&1 || true
    "$V" 'ping -c 1 -t 3 10.88.0.53' >/dev/null 2>&1 || true
    l=$(leaks)
    [ -z "$l" ] && ok "LAN saw only the tunnel's datagrams ($(stat wg_out) out, $(stat wg_in) in)" || fail "leaked: $l"

    echo "== TunnelVision: the LAN's DHCP pushes 128/1, 1/8 and 203.0.113.0/24"
    lanctl tunnelvision >/dev/null
    r0=$(stat request); r1=$(stat renew)
    i=0; while [ $i -lt 90 ] && [ "$(stat renew)" = "$r1" ] && [ "$(stat request)" = "$r0" ]; do sleep 5; i=$((i + 5)); done
    [ "$(stat renew)" != "$r1" ] && ok "a lease renewed while blocked (DHCP is not pf's)" || fail "no renewal"
    "$V" "grep -q 'lease dropped\|send refused' $G/net.log" && fail "the lease lapsed: $("$V" "grep 'lease dropped\|send refused' $G/net.log")"
    sleep 3
    rt=$("$V" 'netstat -rn -f inet' | awk '$1 ~ /^(128.0.0.0\/1|1.0.0.0\/8|1\/8|203.0.113)/ && $2 == "10.88.0.1"')
    [ -z "$rt" ] && ok "no lease route under the tunnel's" || fail "lease routes applied: $rt"
    lanctl seen-reset >/dev/null
    r=$("$V" 'fetch -T 10 -qo - http://203.0.113.80/' 2>&1) && [ "$r" = crypta-v1-ok ] && ok "203.0.113.80 still through the tunnel" || fail "after TunnelVision: $r"
    l=$(leaks); [ -z "$l" ] && ok "nothing left by the LAN's routes" || fail "leaked: $l"
    lanctl good >/dev/null

    echo "== kill the engine"
    starts=$(tunnel starts | sed -n 1p)
    lanctl seen-reset >/dev/null
    "$V" 'pkill -9 -f /usr/local/bin/atrium-crypta-wg'
    for n in 1 2 3; do
        "$V" 'fetch -T 2 -qo /dev/null http://203.0.113.80/; fetch -T 2 -qo /dev/null http://198.51.100.80/; drill x.example.test @127.0.0.1; ping -c 1 -t 1 10.88.0.1' >/dev/null 2>&1 || true
    done
    l=$(leaks); [ -z "$l" ] && ok "engine dead: nothing left the LAN interface but the tunnel's" || fail "leaked with the engine dead: $l"
    i=0; until [ "$(tunnel starts | sed -n 1p)" -gt "$starts" ] && [ "$(tunnel handshake | sed -n 1p)" = 1 ]; do
        i=$((i + 1)); [ $i -lt 60 ] || fail "engine not back: $("$V" "$CTL list")"; sleep 1; done
    ok "engine restarted and shook hands again (${i}s)"
    r=$("$V" 'fetch -T 10 -qo - http://203.0.113.80/' 2>&1) && [ "$r" = crypta-v1-ok ] && ok "traffic flows again" || fail "after restart: $r"
    l=$(leaks); [ -z "$l" ] && ok "no leak across the restart" || fail "leaked: $l"

    echo "== down"
    "$V" "$CTL down name=full" >/dev/null || fail "down full"
    "$V" 'pfctl -a atrium/vpn -sr' | grep -q . && fail "atrium/vpn not emptied" || ok "pf anchor emptied"
    gw=$("$V" 'route -n get default | awk "/gateway/{print \$2}"')
    [ "$gw" = 10.88.0.1 ] && ok "default route back on the LAN" || fail "default route $gw"
    "$V" 'ping -c 1 -t 5 10.88.0.1' >/dev/null && ok "LAN reachable again" || fail "LAN after down"
    echo "== full tunnel without the block: IPv6 still kept from going around it"
    "$V" "$CTL set name=full block=0" >/dev/null || fail "set block=0"
    "$V" "$CTL up name=full" >/dev/null || fail "up full (no block)"
    i=0; until [ "$(tunnel handshake | sed -n 1p)" = 1 ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "no handshake"; sleep 1; done
    lanctl seen-reset >/dev/null
    "$V" 'ping6 -c 2 -t 3 fd77:0:0:88::1; ping6 -c 1 -t 3 fd77:0:0:88::53' >/dev/null 2>&1 || true
    l=$(leaks)
    case " $l " in *" ip6/"*) fail "IPv6 around the full tunnel: $l" ;; *) ok "no IPv6 out the LAN with a v4-only full tunnel ($l)" ;; esac
    "$V" "$CTL down name=full" >/dev/null || fail "down full (no block)"
    lanctl seen-reset >/dev/null
    "$V" 'ping6 -c 1 -t 3 fd77:0:0:88::1' >/dev/null 2>&1 || true
    case " $(leaks) " in *" ip6/58 fd77:0:0:88::1="*) ok "IPv6 flows again once it is down (control)" ;; *) fail "v6 control: $(leaks)" ;; esac

    # Control for the TunnelVision check: with no tunnel, the same lease's
    # routes are taken.
    lanctl tunnelvision >/dev/null
    r1=$(stat renew)
    i=0; while [ $i -lt 90 ] && [ "$(stat renew)" = "$r1" ]; do sleep 5; i=$((i + 5)); done
    sleep 3
    rt=$("$V" 'netstat -rn -f inet' | awk '$1 ~ /^203.0.113/ && $2 == "10.88.0.1"')
    [ -n "$rt" ] && ok "without a tunnel the lease's 203.0.113.0/24 route is applied (control)" || fail "control: lease routes not applied: $("$V" 'netstat -rn -f inet')"
    lanctl good >/dev/null
    echo "crypta V1 gate: $pass checks passed"
    ;;
*)
    echo "usage: $0 run" >&2
    exit 2
    ;;
esac
