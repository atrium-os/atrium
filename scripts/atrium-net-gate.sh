#!/bin/sh
# atrium-net's N0 gate (docs/spec/atrium-net.md §10): the FreeBSD VM's second
# NIC (vtnet1) on a fake LAN (atrium-net/sim/lan.py: router, DHCP, DNS) that
# can turn hostile.
#
#   sh scripts/atrium-net-gate.sh vm     boot the VM with the LAN NIC
#   sh scripts/atrium-net-gate.sh n0     build, install, run the checks
#
# vtnet0 (slirp, ssh) is left alone except for its default route, which
# atrium-net takes over during the run and which is put back at the end.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T=aarch64-unknown-freebsd
V="$ROOT/scripts/vssh"
export VSSH_CONNECT_TIMEOUT="${VSSH_CONNECT_TIMEOUT:-30}"
LAN_LOG="${LAN_LOG:-$HOME/atrium-vm/lan.log}"
LEASE_S="${LEASE_S:-120}"
LANDEV="-netdev socket,id=lan,udp=127.0.0.1:7801,localaddr=127.0.0.1:7802 -device virtio-net-pci,netdev=lan,mac=02:4e:00:00:00:01"

pass=0
ok() { echo "ok   $1"; pass=$((pass + 1)); }
cleanup() {
    "$V" 'pkill -f "atrium-net -v"; sleep 1
        for a in $(ifconfig vtnet1 inet | awk "/inet /{print \$2}"); do ifconfig vtnet1 inet $a -alias; done
        g=$(cat /tmp/atrium-net/default.saved 2>/dev/null); [ -n "$g" ] && { route -q change default $g || route -q add default $g; }
        [ -f /tmp/atrium-net/resolv.conf.saved ] && cp /tmp/atrium-net/resolv.conf.saved /etc/resolv.conf
        true' || true
}
fail() {
    echo "FAIL $1"
    "$V" 'tail -n 40 /tmp/atrium-net/net.log' || true
    tail -n 20 "$LAN_LOG" || true
    exit 1
}
lanctl() {
    python3 - "$1" <<'PY'
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(5)
s.sendto(sys.argv[1].encode(), ("127.0.0.1", 7803)); print(s.recv(4096).decode())
PY
}
stat() { lanctl stats | python3 -c "import json,sys; print(json.load(sys.stdin).get('$1', 0))"; }
addrs() { "$V" 'ifconfig vtnet1 inet | awk "/inet /{print \$2}" | tr "\n" " "'; }
wait_addr() {
    i=0
    while [ $i -lt "$2" ]; do
        a=$(addrs)
        case " $a " in *" $1 "*) return 0 ;; esac
        sleep 2; i=$((i + 2))
    done
    return 1
}

case "${1:-}" in
vm)
    QEMU_EXTRA="$LANDEV" sh "$ROOT/scripts/cloud-vm.sh" up
    ;;
n0)
    CROSS_RUSTFLAGS=$(set -- env; . "$ROOT/scripts/cloud-vm.sh" >/dev/null; printf %s "$CROSS_RUSTFLAGS")
    (cd "$ROOT/atrium-net" && cargo test -q >&2 && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build --release --target "$T" -p atrium-net -p atrium-lease >&2)
    R="$ROOT/atrium-net/target/$T/release"
    tar czf - -C "$R" atrium-net atrium-lease | "$V" 'rm -rf /tmp/atrium-net && mkdir -p /tmp/atrium-net && tar xzf - -C /tmp/atrium-net'

    pkill -f atrium-net/sim/lan.py 2>/dev/null || true
    python3 "$ROOT/atrium-net/sim/lan.py" "$LEASE_S" > "$LAN_LOG" 2>&1 &
    LANPID=$!
    # Clean up on any exit, while the LAN still answers: with resolv.conf on
    # unbound and unbound forwarding to the LAN, sshd's reverse lookups stall
    # the moment the LAN goes away.
    trap 'cleanup; kill $LANPID 2>/dev/null || true' EXIT
    sleep 1

    "$V" "set -e
        ifconfig vtnet1 >/dev/null 2>&1 || { echo 'no vtnet1: boot with: sh scripts/atrium-net-gate.sh vm'; exit 1; }
        pkill -f 'atrium-net -v' 2>/dev/null || true
        pkill -f 'dhclient.*vtnet1' 2>/dev/null || true
        for a in \$(ifconfig vtnet1 inet | awk '/inet /{print \$2}'); do ifconfig vtnet1 inet \$a -alias; done
        id _atrium-lease >/dev/null 2>&1 || pw useradd _atrium-lease -u 65034 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net lease parser'
        mkdir -p /var/empty
        # local-unbound: the system resolver; atrium.test is a private zone
        # (and test. is one of unbound's built-in empty zones, RFC 6761).
        if [ ! -f /var/unbound/unbound.conf ]; then local-unbound-setup >/dev/null; fi
        mkdir -p /var/unbound/conf.d
        printf 'server:\n    local-zone: \"test.\" nodefault\n    domain-insecure: \"atrium.test\"\n' > /var/unbound/conf.d/atrium-test.conf
        sysrc local_unbound_enable=YES >/dev/null
        service local_unbound restart >/dev/null 2>&1 || service local_unbound start >/dev/null
        cp /etc/resolv.conf /tmp/atrium-net/resolv.conf.saved
        printf 'nameserver 127.0.0.1\n' > /etc/resolv.conf
        route -n get default 2>/dev/null | awk '/gateway/{print \$2}' > /tmp/atrium-net/default.saved
        daemon -f -o /tmp/atrium-net/net.log /tmp/atrium-net/atrium-net -v --iface vtnet1 --capsicum --lease /tmp/atrium-net/atrium-lease --sock /tmp/atrium-net/ctl.sock
        sleep 1"

    wait_addr 10.88.0.50 120 && ok "DHCPv4 through atrium-lease: vtnet1 has 10.88.0.50" || fail "no address (have: $(addrs))"
    gw=$("$V" 'route -n get default | awk "/gateway/{print \$2}"')
    [ "$gw" = 10.88.0.1 ] && ok "default route via 10.88.0.1" || fail "default route: $gw"
    "$V" 'ping -c 2 -t 10 10.88.0.1' >/dev/null && ok "ping the LAN's router" || fail "ping"
    fw=$("$V" 'local-unbound-control list_forwards 2>&1 | grep "^\. " || true')
    echo "$fw" | grep -q 10.88.0.53 && ok "unbound forwards to the LAN's DNS ($fw)" || fail "unbound forwarders: $fw"
    a=$("$V" 'drill gate.atrium.test @127.0.0.1 | awk "/^gate.atrium.test/{print \$5}"')
    [ "$a" = 10.88.0.77 ] && ok "name resolved through unbound: gate.atrium.test = $a" || fail "resolve: $a"

    # The parser's sandbox: its own uid, only its socket (fds 0-3).
    p=$("$V" 'pgrep -x atrium-lease')
    u=$("$V" "ps -o user= -p $p")
    [ "$u" = _atrium-lease ] && ok "atrium-lease runs as _atrium-lease" || fail "lease uid: $u"
    n=$("$V" "procstat -f $p | awk 'NR>1 && \$3 ~ /^[0-9]+\$/' | wc -l | tr -d ' '")
    [ "$n" -le 4 ] && ok "atrium-lease holds $n descriptors" || fail "lease fds: $n"
    capmode() { "$V" "procstat -s $1 | awk 'NR==2{print \$10}'" | grep -q C; }
    capmode "$p" && ok "atrium-lease is in capability mode" || fail "atrium-lease not in capability mode"
    capmode "$("$V" 'pgrep -x atrium-net')" && ok "atrium-net is in capability mode" || fail "atrium-net not in capability mode"

    # Renewal at T1 (half the lease).
    i=0; while [ "$(stat renew)" -lt 1 ] && [ $i -lt $((LEASE_S)) ]; do sleep 5; i=$((i + 5)); done
    [ "$(stat renew)" -ge 1 ] && wait_addr 10.88.0.50 10 && ok "lease renewed at T1, address kept" || fail "no renewal"

    # Hostile server: restart atrium-net into a LAN that sends bad leases.
    lanctl hostile >/dev/null
    "$V" 'pkill -f "atrium-net -v"; sleep 1; for a in $(ifconfig vtnet1 inet | awk "/inet /{print \$2}"); do ifconfig vtnet1 inet $a -alias; done
        daemon -f -o /tmp/atrium-net/net.log /tmp/atrium-net/atrium-net -v --iface vtnet1 --capsicum --lease /tmp/atrium-net/atrium-lease --sock /tmp/atrium-net/ctl.sock'
    bad=""
    i=0
    while [ $i -lt 300 ] && [ "$(stat hostile_sent)" -lt 34 ]; do
        # Some hostile kinds still carry a usable lease (four DNS servers are
        # cut to three; a mutation may hit a harmless byte): 10.88.0.50 via
        # 10.88.0.1 is fine. Anything else on the interface is not.
        for a in $(addrs); do [ "$a" = 10.88.0.50 ] || bad="$bad $a"; done
        g=$("$V" 'route -n get default 2>/dev/null | awk "/gateway/{print \$2}"')
        case "$g" in ""|10.88.0.1|10.0.2.2) ;; *) bad="$bad default:$g" ;; esac
        m=$("$V" 'ifconfig vtnet1 | sed -n "s/.*mtu \([0-9]*\).*/\1/p"')
        [ "$m" = 1500 ] || bad="$bad mtu:$m"
        r=$("$V" 'netstat -rn4 | awk "NR>4{print \$1}" | grep -E "^(100\.6|127\.[1-9])" || true')
        [ -n "$r" ] && bad="$bad route:$r"
        sleep 3; i=$((i + 3))
    done
    n=$(stat hostile_sent)
    [ "$n" -ge 17 ] || fail "hostile server only got $n chances"
    [ -z "$bad" ] && ok "$n hostile DHCP replies (17 kinds, cycled): no bad address, route, gateway or MTU applied" || fail "hostile lease applied:$bad"
    "$V" 'pgrep -f "atrium-net -v" >/dev/null' && ok "atrium-net still running" || fail "atrium-net died"
    refused=$("$V" "printf 'status\n' | nc -U -w 2 /tmp/atrium-net/ctl.sock | sed -n 's/.* refused=\([0-9]*\).*/\1/p'")
    [ "${refused:-0}" -ge 1 ] && ok "atrium-net refused $refused leases (§5.2 checks)" || fail "no refusals counted"

    lanctl good >/dev/null
    wait_addr 10.88.0.50 120 && ok "back on a good server: bound again" || fail "no recovery"

    echo "atrium-net N0 gate: $pass checks passed"
    ;;
*)
    echo "usage: $0 vm|n0" >&2
    exit 2
    ;;
esac
