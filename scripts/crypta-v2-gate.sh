#!/bin/sh
# Crypta V2 gate (docs/spec/vpn.md §11, app scope): a persona's tunnel.
#
#   ARCH=amd64 sh scripts/atrium-net-gate.sh vm     the FreeBSD VM, LAN NIC on vtnet1
#   sh scripts/crypta-wg-linux-vm.sh up              the Ubuntu VM (wg0, the peer)
#   sh scripts/crypta-v2-gate.sh run                 build, install, check
#
# The same stand as the V1 gate (it shares its setup): lan.py counts every
# packet the guest puts on the LAN, the Ubuntu VM is the WireGuard peer with
# HTTP on 10.99.0.1 and 203.0.113.80 (only reachable through the tunnel) and
# DNS on 10.99.0.1 (anything not under corp.test answers 203.0.113.80).
#
# Person `crgate` has a Work persona whose policy says `vpn = "work"`; the
# profile `work` is a full tunnel scoped to that persona. Two vnet jails stand
# in for app instances side by side, their networks made the way portcullisd
# makes them (atrium-portcullisd-jclient persona-net: the persona's route
# domain from the policy and atrium-net, then jaild's epair in that table):
# `crw` in Work, `crp` in Personal.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRYPTA_GATE_LIB=1 . "$ROOT/scripts/crypta-v1-gate.sh"
JC="env ATRIUM_NET_SOCK=$G/net.sock /usr/local/bin/atrium-portcullisd-jclient /var/run/atrium/jaild.sock"
PERSON=crgate
# Work's resolver as its resolv.conf names it (jaild DOMAIN_RESOLVER).
WDNS=100.64.0.1

# What leaks() counts that Work could have sent: its jail has IPv4 only, and
# the machine itself (not blocked by a scoped tunnel) may multicast meanwhile.
wleaks() { lanctl seen | python3 -c "
import json, sys
d = json.load(sys.stdin)
def work(k):
    proto, dst = k.split(' ', 1)
    # The machine's own resolver asks the LAN's server (10.88.0.53) whenever
    # it likes; Work's direct lookups are tested against 10.88.0.54 instead.
    return k != 'udp 198.51.100.51:51820' and not proto.startswith('ip6/') and not 224 <= int(dst.split('.')[0]) <= 239 \\
        and not dst.startswith('10.88.0.53:')
print(' '.join(f'{k}={v}' for k, v in sorted(d.items()) if work(k)))"; }
in_w() { "$V" "jexec crw sh -c '$1'"; }
in_p() { "$V" "jexec crp sh -c '$1'"; }
# The Ubuntu side's HTTP log: one line per request it served.
served() { gssh <<'EOF'
grep -c 'GET /' /root/http.out || true
EOF
}
jails_down() {
    "$V" "for j in crw crp; do jail -r \$j 2>/dev/null; $JC release-net \$j; done; true" >/dev/null 2>&1 || true
}
# A jail of `persona` named `name`, its network from persona-net. Prints the fib.
mkjail() { # $1 name, $2 persona, $3 mac
    "$V" "set -e
        jail -r $1 2>/dev/null || true; $JC release-net $1
        n=\$($JC persona-net $1 $3 $PERSON $2)
        set -- \$n
        [ \$# = 4 ] || exit 1
        fib=\${1#fib=}; b=\$2; app=\$3; host=\$4
        jail -c name=$1 path=/ vnet persist host.hostname=$1 >/dev/null
        ifconfig \$b vnet $1
        ifconfig -j $1 \$b ether $3
        ifconfig -j $1 \$b inet \$app/30 up
        ifconfig -j $1 lo0 inet 127.0.0.1/8 up
        route -q -j $1 add default \$host
        echo \$fib"
}

case "${1:-}" in
run)
    CROSS_RUSTFLAGS=$(set -- env; ARCH=amd64 . "$ROOT/scripts/cloud-vm.sh" >/dev/null; printf %s "$CROSS_RUSTFLAGS")
    (cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T -p portcullisd --bin atrium-portcullisd-jclient)
    EXTRA="$ROOT/portcullis/target/$T/release/atrium-portcullisd-jclient"
    setup
    trap 'jails_down; cleanup; kill $LANPID 2>/dev/null || true' EXIT
    # KEEP=1: leave the stand up on a failure, to look at it.
    [ -z "${KEEP:-}" ] || trap - EXIT
    "$V" "install -m 755 $G/atrium-portcullisd-jclient /usr/local/bin/atrium-portcullisd-jclient
        mkdir -p /var/db/atrium/$PERSON
        printf '[personas.work]\ncolour = \"orange\"\nvpn = \"work\"\n' > /var/db/atrium/$PERSON/policy.toml"
    "$V" 'pfctl -sn' | grep -q 'rdr-anchor "atrium/vpn"' && ok "pf base rules carry rdr-anchor atrium/vpn" || fail "no rdr-anchor"

    echo "== a Work profile: full tunnel, scoped to the persona"
    PRIV=$(python3 -c 'import os,base64; print(base64.b64encode(os.urandom(32)).decode())')
    SRV=$(python3 -c 'import os,base64; print(base64.b64encode(os.urandom(32)).decode())')
    wconf() { printf '[Interface]\nPrivateKey = %s\nAddress = 10.99.0.2/24\nDNS = 10.99.0.1\n\n[Peer]\nPublicKey = %s\nEndpoint = 198.51.100.51:51820\nAllowedIPs = 0.0.0.0/0\nPersistentKeepalive = 25\n' "$PRIV" "$SRV"; }
    IMP=$(mktemp)
    wconf | "$V" "$CTL import name=probe" > "$IMP" || fail "import: $(cat "$IMP")"
    PUB=$(sed -n 's/^imported .*public=\([^ ]*\).*/\1/p' "$IMP")
    "$V" "$CTL delete name=probe" >/dev/null
    SRV=$(ubuntu_peer "$PUB")
    [ -n "$SRV" ] && ok "Ubuntu wg0 up; server key $SRV" || fail "Ubuntu peer"
    wconf | "$V" "$CTL import name=work" >/dev/null || fail "import work"
    "$V" "$CTL set name=work scoped=1" >/dev/null || fail "set scoped"
    "$V" "$CTL list" | grep -q 'scoped=1' && ok "profile work is persona-scoped" || fail "scoped not saved: $("$V" "$CTL list")"

    echo "== Work and Personal side by side, the tunnel down"
    WF=$(mkjail crw work 02:a7:00:00:00:01) || fail "Work jail"
    PF=$(mkjail crp personal 02:a7:00:00:00:02) || fail "Personal jail"
    [ "$WF" -ge 1 ] && ok "Work's network is in route domain work (fib $WF)" || fail "Work fib $WF"
    [ "$PF" = 0 ] && ok "Personal's network is in the host's table" || fail "Personal fib $PF"
    "$V" "pfctl -t atrium_fib_$WF -T show" | grep -q 100.64. && ok "Work's address is in pf table atrium_fib_$WF" || fail "not in the domain table"
    lanctl seen-reset >/dev/null
    in_w "fetch -T 5 -qo /dev/null http://203.0.113.80/; fetch -T 5 -qo /dev/null http://10.88.0.1/; ping -c 1 -t 2 10.88.0.1; drill -Q x.example.test @$WDNS; drill -Q x.example.test @10.88.0.54" >/dev/null 2>&1 &&
        fail "Work reached something with its tunnel down" || true
    l=$(wleaks); [ -z "$l" ] && ok "tunnel down: Work reaches nothing, nothing of it on the LAN" || fail "Work leaked with the tunnel down: $l"
    in_p 'ping -c 1 -t 3 10.88.0.1' >/dev/null && ok "Personal reaches the LAN directly" || fail "Personal: no LAN"
    case " $(leaks) " in *" icmp 10.88.0.1="*) ok "the leak counter sees Personal's packets (control)" ;; *) fail "leak counter blind: $(leaks)" ;; esac

    echo "== the tunnel up"
    "$V" "$CTL up name=work" >/dev/null || fail "up work"
    i=0; until [ "$(tunnel handshake | head -1)" = 1 ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "no handshake: $("$V" "$CTL list")"; sleep 1; done
    ok "handshake with Linux's wg0"
    gw=$("$V" 'route -n get default | awk "/gateway/{print \$2}"')
    [ "$gw" = 10.88.0.1 ] && ok "the machine's default route stays on the LAN" || fail "default route $gw"
    "$V" "pfctl -a atrium/vpn -sr" | grep -q 'block out quick on vtnet1 all' && fail "a scoped tunnel blocks the machine" || ok "no machine-wide block for a scoped tunnel"
    lanctl seen-reset >/dev/null
    n0=$(served)
    r=$(in_w 'fetch -T 10 -qo - http://203.0.113.80/' 2>&1) && [ "$r" = crypta-v1-ok ] && ok "Work: HTTP to 203.0.113.80 through the tunnel" || fail "Work HTTP: $r"
    r=$(in_w 'fetch -T 10 -qo - http://10.99.0.1/' 2>&1) && [ "$r" = crypta-v1-ok ] && ok "Work: HTTP to 10.99.0.1" || fail "Work HTTP 10.99.0.1: $r"
    a=$(in_w "drill anything.example.test @$WDNS" | awk '/^anything.example.test/{print $5}')
    [ "$a" = 203.0.113.80 ] && ok "Work's resolver answers from the tunnel's DNS: $a" || fail "Work DNS: $a"
    a=$(in_w "drill web.corp.test @10.88.0.53" | awk '/^web.corp.test/{print $5}')
    [ "$a" = 10.99.0.1 ] && ok "Work's lookups to the LAN's server go to the tunnel's instead: $a" || fail "Work DNS to the LAN server: $a"
    l=$(wleaks); [ -z "$l" ] && ok "LAN saw nothing of Work but the tunnel's datagrams ($(stat wg_out) out)" || fail "Work leaked: $l"
    n1=$(served)
    [ "$n1" -ge $((n0 + 2)) ] && ok "the peer served Work's requests ($((n1 - n0)))" || fail "peer served $n0 -> $n1"

    lanctl seen-reset >/dev/null
    in_p 'fetch -T 5 -qo /dev/null http://203.0.113.80/; fetch -T 5 -qo /dev/null http://10.99.0.1/' >/dev/null 2>&1 &&
        fail "Personal reached the tunnel's side" || ok "Personal does not reach the tunnel's networks"
    "$V" 'fetch -T 5 -qo /dev/null http://10.99.0.1/' >/dev/null 2>&1 && fail "the machine reached 10.99.0.1" || ok "nor does the machine itself"
    n2=$(served)
    [ "$n2" = "$n1" ] && ok "the peer saw nothing of Personal's or the machine's" || fail "peer served $n1 -> $n2"
    case " $(leaks) " in *" tcp 203.0.113.80:80="*) ok "Personal's 203.0.113.80 went out the LAN, as direct traffic does" ;; *) fail "Personal's traffic: $(leaks)" ;; esac
    a=$(in_p "drill gate.atrium.test @10.88.0.53" | awk '/^gate.atrium.test/{print $5}')
    [ "$a" = 10.88.0.77 ] && ok "Personal's lookups go to the LAN's DNS: $a" || fail "Personal DNS: $a"
    a=$("$V" 'drill gate.atrium.test @127.0.0.1 | awk "/^gate.atrium.test/{print \$5}"')
    [ "$a" = 10.88.0.77 ] && ok "the machine's resolver is untouched: $a" || fail "machine DNS: $a"

    echo "== kill the engine"
    starts=$(tunnel starts | sed -n 1p)
    lanctl seen-reset >/dev/null
    "$V" 'pkill -9 -f /usr/local/bin/atrium-crypta-wg'
    for n in 1 2 3; do
        in_w "fetch -T 2 -qo /dev/null http://203.0.113.80/; drill -Q x.example.test @$WDNS; drill -Q x.example.test @10.88.0.54; ping -c 1 -t 1 10.88.0.1" >/dev/null 2>&1 || true
    done
    l=$(wleaks); [ -z "$l" ] && ok "engine dead: nothing of Work on the LAN" || fail "Work leaked with the engine dead: $l"
    i=0; until [ "$(tunnel starts | sed -n 1p)" -gt "$starts" ] && [ "$(tunnel handshake | sed -n 1p)" = 1 ]; do
        i=$((i + 1)); [ $i -lt 60 ] || fail "engine not back: $("$V" "$CTL list")"; sleep 1; done
    r=$(in_w 'fetch -T 10 -qo - http://203.0.113.80/' 2>&1) && [ "$r" = crypta-v1-ok ] && ok "engine back (${i}s), Work flows again" || fail "after restart: $r"

    echo "== the tunnel down again"
    "$V" "$CTL down name=work" >/dev/null || fail "down work"
    lanctl seen-reset >/dev/null
    in_w "fetch -T 5 -qo /dev/null http://203.0.113.80/; drill -Q x.example.test @$WDNS; drill -Q x.example.test @10.88.0.54; ping -c 1 -t 2 10.88.0.1" >/dev/null 2>&1 &&
        fail "Work reached something after down" || true
    l=$(wleaks); [ -z "$l" ] && ok "down: Work reaches nothing again (the kill switch)" || fail "Work leaked after down: $l"
    rt=$("$V" "netstat -rn -F $WF -f inet" | awk '$1 ~ /^(0|default|10\.99|203\.)/')
    [ -z "$rt" ] && ok "route domain $WF holds no tunnel routes" || fail "left in fib $WF: $rt"
    "$V" "pfctl -a atrium/vpn -sn" | grep -q rdr && fail "DNS redirect left behind" || ok "DNS redirect removed"
    in_p 'ping -c 1 -t 3 10.88.0.1' >/dev/null && ok "Personal unaffected" || fail "Personal after down"
    jails_down
    "$V" "pfctl -t atrium_fib_$WF -T show 2>/dev/null" | grep -q 100.64. && fail "Work's address left in atrium_fib_$WF" || ok "released: Work's address out of the domain table"
    echo "crypta V2 gate: $pass checks passed"
    ;;
*)
    echo "usage: $0 run" >&2
    exit 2
    ;;
esac
