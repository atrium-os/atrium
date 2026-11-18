#!/bin/sh
# atrium-net's N0 + N2 gate (docs/spec/atrium-net.md §10): the FreeBSD VM's
# second NIC (vtnet1) on a fake LAN (atrium-net/sim/lan.py: router, DHCP, DNS,
# IPv6 RAs, DHCPv6) that can turn hostile.
#
#   sh scripts/atrium-net-gate.sh vm     boot the VM with the LAN NIC (and the
#                                        Aura gates' port, so one VM serves both)
#   sh scripts/atrium-net-gate.sh n0     build, install, run the checks (IPv4, IPv6)
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
    "$V" 'pkill -x atrium-netconf; pkill -f "atrium-net -v"; sleep 1
        for a in $(ifconfig vtnet1 inet | awk "/inet /{print \$2}"); do ifconfig vtnet1 inet $a -alias; done
        # RAs off before deleting SLAAC addresses (kernel race, spec §5.3).
        ifconfig vtnet1 inet6 -accept_rtadv
        for a in $(ifconfig vtnet1 inet6 | awk "/inet6 fd77:/{print \$2}"); do ifconfig vtnet1 inet6 $a -alias; done
        ifconfig vtnet1 inet6 ifdisabled
        g=$(cat /tmp/atrium-net/default.saved 2>/dev/null); [ -n "$g" ] && { while route -q delete default 2>/dev/null; do :; done; route -q add default $g; }
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
    QEMU_EXTRA="$LANDEV" NETDEV_EXTRA="hostfwd=udp:127.0.0.1:7701-:7701" sh "$ROOT/scripts/cloud-vm.sh" up
    ;;
n0)
    CROSS_RUSTFLAGS=$(set -- env; . "$ROOT/scripts/cloud-vm.sh" >/dev/null; printf %s "$CROSS_RUSTFLAGS")
    (cd "$ROOT/atrium-net" && cargo test -q >&2 && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build --release --target "$T" -p atrium-net -p atrium-lease -p atrium-netconf -p atrium-mdns >&2)
    R="$ROOT/atrium-net/target/$T/release"
    { cp "$ROOT/atrium-net/etc/unbound-atrium-local.conf" "$ROOT/atrium-net/etc/unbound-atrium-tls.conf" "$R/"; tar czf - -C "$R" atrium-net atrium-lease atrium-netconf atrium-netctl atrium-mdns unbound-atrium-local.conf unbound-atrium-tls.conf; } | "$V" 'rm -rf /tmp/atrium-net && mkdir -p /tmp/atrium-net && tar xzf - -C /tmp/atrium-net'

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
        cp /tmp/atrium-net/unbound-atrium-local.conf /var/unbound/conf.d/atrium-local.conf
        id _atrium-mdns >/dev/null 2>&1 || pw useradd _atrium-mdns -u 65037 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net .local names'
        sysrc local_unbound_enable=YES >/dev/null
        service local_unbound restart >/dev/null 2>&1 || service local_unbound start >/dev/null
        cp /etc/resolv.conf /tmp/atrium-net/resolv.conf.saved
        printf 'nameserver 127.0.0.1\n' > /etc/resolv.conf
        route -n get default 2>/dev/null | awk '/gateway/{print \$2}' > /tmp/atrium-net/default.saved
        daemon -f -o /tmp/atrium-net/net.log /tmp/atrium-net/atrium-net -v --iface vtnet1 --no-probe --capsicum --lease /tmp/atrium-net/atrium-lease --mdns-bin /tmp/atrium-net/atrium-mdns --sock /tmp/atrium-net/ctl.sock
        sleep 1"

    wait_addr 10.88.0.50 120 && ok "DHCPv4 through atrium-lease: vtnet1 has 10.88.0.50" || fail "no address (have: $(addrs))"
    gw=$("$V" 'route -n get default | awk "/gateway/{print \$2}"')
    [ "$gw" = 10.88.0.1 ] && ok "default route via 10.88.0.1" || fail "default route: $gw"
    "$V" 'ping -c 2 -t 10 10.88.0.1' >/dev/null && ok "ping the LAN's router" || fail "ping"
    fw=$("$V" 'local-unbound-control list_forwards 2>&1 | grep "^\. " || true')
    echo "$fw" | grep -q 10.88.0.53 && ok "unbound forwards to the LAN's DNS ($fw)" || fail "unbound forwarders: $fw"
    a=$("$V" 'drill gate.atrium.test @127.0.0.1 | awk "/^gate.atrium.test/{print \$5}"')
    [ "$a" = 10.88.0.77 ] && ok "name resolved through unbound: gate.atrium.test = $a" || fail "resolve: $a"

    # IPv6 (N2): kernel SLAAC from the LAN's RA, asked for by a router
    # solicitation; DNS servers from the RA's RDNSS through atrium-lease.
    v6() { "$V" 'ifconfig vtnet1 inet6' | awk '/inet6 fd77:/{print $2, ($0 ~ /temporary/) ? "temporary" : "stable"}'; }
    i=0; while [ $i -lt 90 ] && ! v6 | grep -q temporary; do sleep 3; i=$((i + 3)); done
    a6=$(v6); echo "$a6" | sed 's/^/    /'
    [ "$(stat rs)" -ge 1 ] && ok "router solicitation sent ($(stat rs)); the LAN answered with an RA" || fail "no router solicitation"
    st=$(echo "$a6" | awk '$2=="stable"{print $1; exit}')
    [ -n "$st" ] && ok "SLAAC address on fd77:0:0:88::/64: $st" || fail "no stable IPv6 address"
    # RFC 7217: not EUI-64 (02:4e:00:00:00:01 would give ...:4e:ff:fe00:1).
    case "$st" in *ff:fe*) fail "EUI-64 interface ID: $st" ;; *) ok "interface ID is stable-private (RFC 7217), not the MAC" ;; esac
    echo "$a6" | grep -q temporary && ok "temporary address (RFC 8981): $(echo "$a6" | awk '$2=="temporary"{print $1; exit}')" || fail "no temporary address"
    # vtnet0 (QEMU's slirp) has its own RA router, which may win the default.
    "$V" 'ndp -rn' | grep -q '^fe80::1%vtnet1 ' && ok "fe80::1 on vtnet1 is a default router (from the RA)" || fail "default routers: $("$V" 'ndp -rn')"
    "$V" 'ping6 -c 2 -t 10 fd77:0:0:88::1' >/dev/null && ok "ping6 the LAN's router" || fail "ping6"
    src=$("$V" 'ping6 -c 1 -t 10 fd77:0:0:88::1 | head -1' | sed -n 's/^PING6*([^)]*) \([^ ]*\) --> .*/\1/p')
    echo "$a6" | grep -q "^$src temporary" && ok "outgoing traffic uses the temporary address" || echo "    note: source '$src'"
    i=0; while [ $i -lt 30 ]; do fw=$("$V" 'local-unbound-control list_forwards 2>&1 | grep "^\. " || true'); echo "$fw" | grep -q fd77:0:0:88::53 && break; sleep 3; i=$((i + 3)); done
    echo "$fw" | grep -q fd77:0:0:88::53 && ok "unbound forwards to the RA's DNS servers too ($fw)" || fail "IPv6 forwarders: $fw"
    echo "$fw" | grep -q 'fe80::1' && ok "link-local DNS server forwarded with its interface" || fail "link-local forwarder: $fw"
    a=$("$V" 'drill -6 gate.atrium.test AAAA @fd77:0:0:88::53 | awk "/^gate.atrium.test/{print \$5}"')
    [ "$a" = fd77:0:0:88::77 ] && ok "DNS over IPv6 to the RA's server: AAAA $a" || fail "DNS over IPv6: $a"
    a=$("$V" 'drill gate.atrium.test AAAA @127.0.0.1 | awk "/^gate.atrium.test/{print \$5}"')
    [ "$a" = fd77:0:0:88::77 ] && ok "AAAA through unbound: $a" || fail "AAAA through unbound: $a"
    # A rogue router's bad RAs: nothing bad reaches unbound, the good servers stay.
    lanctl hostile6 >/dev/null
    bad=""
    i=0
    while [ $i -lt 120 ] && [ "$(stat hostile6_sent)" -lt 24 ]; do
        fw=$("$V" 'local-unbound-control list_forwards 2>&1 | grep "^\. " || true')
        for b in ' ::1 ' ff02:: ::ffff 2002: fd66:; do case " $fw " in *"$b"*) bad="$bad $b" ;; esac; done
        sleep 3; i=$((i + 3))
    done
    lanctl good6 >/dev/null
    n=$(stat hostile6_sent)
    [ "$n" -ge 12 ] || fail "rogue router only sent $n RAs"
    [ -z "$bad" ] && ok "$n hostile RAs (12 kinds, cycled): no bad DNS server reached unbound" || fail "hostile RA applied:$bad"
    sv=$("$V" "printf 'status\n' | nc -U -w 2 /tmp/atrium-net/ctl.sock")
    echo "$sv" | grep -q 'dns6=fd77:0:0:88::53,fe80::1' && ok "the LAN's IPv6 DNS servers kept through the rogue RAs" || fail "dns6 after rogue RAs: $sv"
    "$V" 'pgrep -x atrium-lease >/dev/null && pgrep -x atrium-net >/dev/null' && ok "atrium-net and atrium-lease still running" || fail "a process died on hostile RAs"
    # A resolver restart falls back to forward.conf; atrium-net notices
    # within 10 s and sets the network's servers again.
    "$V" 'service local_unbound restart >/dev/null 2>&1'
    i=0; while [ $i -lt 30 ]; do fw=$("$V" 'local-unbound-control list_forwards 2>&1 | grep "^\. " || true'); echo "$fw" | grep -q 10.88.0.53 && echo "$fw" | grep -q fd77:0:0:88::53 && break; sleep 2; i=$((i + 2)); done
    echo "$fw" | grep -q 10.88.0.53 && echo "$fw" | grep -q fd77:0:0:88::53 && ok "local_unbound restarted: forwarders set again in ${i}s ($fw)" || fail "forwarders after unbound restart: $fw"

    # DHCPv6 (§5.3): the RA's O flag asks for DNS servers by
    # Information-Request, M for an address by Solicit/Request; atrium-lease
    # reads the replies, atrium-net checks and applies them.
    fwd() { "$V" 'local-unbound-control list_forwards 2>&1 | grep "^\. " || true'; }
    lanctl d6-info >/dev/null
    i=0; while [ $i -lt 30 ] && ! fwd | grep -q fd77:0:0:88::153; do sleep 2; i=$((i + 2)); done
    fw=$(fwd)
    [ "$(stat d6_info)" -ge 1 ] && echo "$fw" | grep -q fd77:0:0:88::153 && ok "O flag: Information-Request, DHCPv6 DNS server forwarded ($fw)" || fail "DHCPv6 DNS: info=$(stat d6_info) $fw"
    lanctl d6-addr >/dev/null
    has6() { "$V" 'ifconfig vtnet1 inet6' | grep -q "inet6 fd77::88:0:0:1:50 prefixlen 128"; }
    i=0; while [ $i -lt 30 ] && ! has6; do sleep 2; i=$((i + 2)); done
    has6 && ok "M flag: Solicit/Request ($(stat d6_solicit)/$(stat d6_request)), fd77:0:0:88::1:50/128 on vtnet1" || fail "no DHCPv6 address: $("$V" 'ifconfig vtnet1 inet6')"
    "$V" 'ping6 -c 2 -t 10 -S fd77:0:0:88::1:50 fd77:0:0:88::1' >/dev/null && ok "ping6 from the DHCPv6 address" || fail "ping6 from the DHCPv6 address"
    i=0; while [ $i -lt 90 ] && [ "$(stat d6_renew)" -lt 1 ]; do sleep 3; i=$((i + 3)); done
    [ "$(stat d6_renew)" -ge 1 ] && has6 && ok "renewed at T1 ($(stat d6_renew)), address kept" || fail "no DHCPv6 renewal"
    # A hostile server: answers and pushes carry the client's own transaction
    # id. A fresh atrium-net so the client is soliciting again.
    lanctl d6-hostile >/dev/null
    "$V" 'pkill -f "atrium-net -v"; sleep 1
        daemon -f -o /tmp/atrium-net/net.log /tmp/atrium-net/atrium-net -v --iface vtnet1 --no-probe --capsicum --lease /tmp/atrium-net/atrium-lease --mdns-bin /tmp/atrium-net/atrium-mdns --sock /tmp/atrium-net/ctl.sock'
    bad=""
    i=0
    while [ $i -lt 120 ] && [ "$(stat d6_hostile_sent)" -lt 60 ]; do
        a=$("$V" 'ifconfig vtnet1 inet6' | awk '/inet6 /{print $2}' | tr '\n' ' ')
        for b in fd66: fe80::bad ff02:: ::1 ::ffff 2002:; do case " $a " in *" $b"*) bad="$bad addr:$b" ;; esac; done
        fw=$(fwd)
        for b in ' ::1 ' ff02:: ::ffff 2002: fd66:; do case " $fw " in *"$b"*) bad="$bad dns:$b" ;; esac; done
        sleep 3; i=$((i + 3))
    done
    n=$(stat d6_hostile_sent)
    [ "$n" -ge 34 ] || fail "hostile DHCPv6 server only sent $n"
    [ -z "$bad" ] && ok "$n hostile DHCPv6 messages (17 kinds, cycled): no bad address or DNS server applied" || fail "hostile DHCPv6 applied:$bad"
    "$V" 'pgrep -x atrium-lease >/dev/null && pgrep -x atrium-net >/dev/null' && ok "atrium-net and atrium-lease still running" || fail "a process died on hostile DHCPv6"
    # (A "huge lifetimes" answer is a usable lease, capped to a week: start
    # clean so the good server has to give the address again.)
    lanctl d6-addr >/dev/null
    "$V" 'pkill -f "atrium-net -v"; sleep 1; ifconfig vtnet1 inet6 fd77:0:0:88::1:50 -alias 2>/dev/null
        daemon -f -o /tmp/atrium-net/net.log /tmp/atrium-net/atrium-net -v --iface vtnet1 --no-probe --capsicum --lease /tmp/atrium-net/atrium-lease --mdns-bin /tmp/atrium-net/atrium-mdns --sock /tmp/atrium-net/ctl.sock'
    i=0; while [ $i -lt 60 ] && ! has6; do sleep 3; i=$((i + 3)); done
    has6 && ok "good server again: DHCPv6 address back" || fail "no DHCPv6 recovery"
    lanctl d6-off >/dev/null

    # The parser's sandbox: its own uid, only its socket (fds 0-3).
    p=$("$V" 'pgrep -x atrium-lease')
    u=$("$V" "ps -o user= -p $p")
    [ "$u" = _atrium-lease ] && ok "atrium-lease runs as _atrium-lease" || fail "lease uid: $u"
    n=$("$V" "procstat -f $p | awk 'NR>1 && \$3 ~ /^[0-9]+\$/' | wc -l | tr -d ' '")
    [ "$n" -le 4 ] && ok "atrium-lease holds $n descriptors" || fail "lease fds: $n"
    capmode() { "$V" "procstat -s $1 | awk 'NR==2{print \$10}'" | grep -q C; }
    capmode "$p" && ok "atrium-lease is in capability mode" || fail "atrium-lease not in capability mode"
    capmode "$("$V" 'pgrep -x atrium-net')" && ok "atrium-net is in capability mode" || fail "atrium-net not in capability mode"

    # A lease parser that dies is started again (atrium-net's spawner;
    # capability mode forbids exec in atrium-net itself), the address kept.
    old=$("$V" 'pgrep -x atrium-lease')
    d0=$(stat discover); r0=$(stat request)
    "$V" "kill -9 $old"
    i=0; new=""
    while [ $i -lt 20 ]; do new=$("$V" 'pgrep -x atrium-lease' || true); [ -n "$new" ] && [ "$new" != "$old" ] && break; sleep 1; i=$((i + 1)); done
    sv=$("$V" "printf 'status\n' | nc -U -w 2 /tmp/atrium-net/ctl.sock")
    "$V" 'pgrep -f "atrium-net -v" >/dev/null' && [ -n "$new" ] && [ "$new" != "$old" ] && echo "$sv" | grep -q 'lease_restarts=1' && wait_addr 10.88.0.50 5 \
        && ok "atrium-lease killed: started again (pid $old -> $new), atrium-net running, address kept" || fail "lease restart: old=$old new=$new $sv"
    u=$("$V" "ps -o user= -p $new")
    capmode "$new" && [ "$u" = _atrium-lease ] && ok "the new atrium-lease is sandboxed too ($u, capability mode)" || fail "new lease: user $u"
    i=0; while [ $i -lt 30 ] && [ "$(stat request)" -le "$r0" ]; do sleep 1; i=$((i + 1)); done
    [ "$(stat discover)" -gt "$d0" ] && [ "$(stat request)" -gt "$r0" ] && wait_addr 10.88.0.50 5 && ok "the new parser leased 10.88.0.50 again (DISCOVER, REQUEST)" || fail "no lease after restart: discover $d0->$(stat discover) request $r0->$(stat request)"

    # Printer discovery (printing.md §7.1): atrium-netconf's root-only
    # print-discovery socket hands out the two mDNS sockets atrium-net's
    # spawner makes on the active interface.
    "$V" "set -e
        id _atrium-netconf >/dev/null 2>&1 || pw useradd _atrium-netconf -u 65036 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net settings front'
        pkill -x atrium-netconf 2>/dev/null || true; sleep 1; rm -rf /tmp/atrium-net/sockets
        daemon -f -o /tmp/atrium-net/netconf.log /tmp/atrium-net/atrium-netconf -v --ctl /tmp/atrium-net/ctl.sock --dir /tmp/atrium-net/sockets --uid 65036 --capsicum
        sleep 2"
    PD=/tmp/atrium-net/sockets/print-discovery
    m=$("$V" "stat -f '%Sp %Su' $PD $PD/print-discovery.sock | tr '\n' ' '")
    [ "$m" = "drwx------ root srw------- root " ] && ok "print-discovery: root-only directory and socket ($m)" || fail "print-discovery permissions: $m"
    NC="/tmp/atrium-net/atrium-netctl --dir /tmp/atrium-net/sockets --app print-discovery"
    out=$("$V" "su -m nobody -c '$NC printers 1'" 2>&1) && fail "another user got the sockets: $out"
    ok "another user is refused ($(echo "$out" | tail -n 1))"
    lanctl "mdns-printer 10.0.2.2 8661" >/dev/null
    out=$("$V" "$NC printers 4" 2>&1) || fail "printers: $out"
    echo "$out" | sed 's/^/    /'
    echo "$out" | grep -q '^sockets iface=vtnet1 recv=0.0.0.0:5353 send=10.88.0.50:5353 peer=224.0.0.251:5353' && ok "two sockets on vtnet1, both on port 5353, the send one connected to 224.0.0.251:5353" || fail "sockets: $out"
    echo "$out" | grep -q 'recv-socket-send=refused' && echo "$out" | grep -q 'send-socket-elsewhere=refused' && ok "the receive socket cannot send; the send socket cannot reach another address" || fail "socket limits: $out"
    echo "$out" | grep -q 'answer from=10.88.0.60:5353 .*ptr="Gate Printer._ipp._tcp.local".* srv=8661 .*a=10.0.2.2' && [ "$(stat mdns_query)" -ge 1 ] \
        && ok "QU query out of vtnet1, unicast answer on the receive socket: Gate Printer at 10.0.2.2:8661" || fail "no printer: $out $(lanctl stats)"
    lanctl mdns-flood >/dev/null
    out=$("$V" "$NC printers 8" 2>&1) || fail "printers (flood): $out"
    n=$(echo "$out" | sed -n 's/^datagrams=//p')
    "$V" 'pgrep -x atrium-netconf >/dev/null && pgrep -f "atrium-net -v" >/dev/null' && [ "${n:-0}" -ge 100 ] \
        && ok "a flood of hostile answers ($(stat mdns_answer) sent) reaches only the socket ($n datagrams); atrium-net and atrium-netconf untouched" || fail "flood: n=$n"
    lanctl mdns-off >/dev/null

    # .local names (§6.1): unbound's stub zone -> atrium-mdns -> one mDNS
    # question on vtnet1; answers checked in the sandbox.
    i=0; until "$V" "printf 'status\n' | nc -U -w 2 /tmp/atrium-net/ctl.sock" | grep -q 'name=vtnet1 .*mdns=1'; do i=$((i + 1)); [ $i -lt 20 ] || fail "atrium-mdns not running: $("$V" 'tail -n 20 /tmp/atrium-net/net.log')"; sleep 1; done
    mp=$("$V" 'pgrep -x atrium-mdns')
    u=$("$V" "ps -o user= -p $mp" | tr -d ' ')
    capmode "$mp" && [ "$u" = _atrium-mdns ] && ok "atrium-mdns on vtnet1, $u in capability mode" || fail "atrium-mdns: user $u"
    look() { "$V" "drill $1 A @127.0.0.1" 2>&1 || true; }
    a0=$(stat mdns_a)
    r=$(look gatebox.local)
    echo "$r" | grep -qE '^gatebox\.local\.[[:space:]]+[0-9]+[[:space:]]+IN[[:space:]]+A[[:space:]]+10\.88\.0\.77' && [ "$(stat mdns_a)" -gt "$a0" ] \
        && ok "gatebox.local = 10.88.0.77 through unbound and one mDNS question" || fail "gatebox.local: $r $("$V" 'tail -n 5 /tmp/atrium-net/net.log')"
    a1=$(stat mdns_a)
    r=$(look gatebox.local)
    echo "$r" | grep -q '10\.88\.0\.77' && [ "$(stat mdns_a)" = "$a1" ] && ok "asked again: from unbound's cache, nothing on the link" || fail "cache: $r"
    for h in evil loopy nothere; do
        r=$(look $h.local)
        echo "$r" | grep -q 'rcode: NXDOMAIN' && ! echo "$r" | grep -qE 'IN[[:space:]]+A[[:space:]]' || fail "$h.local: $r"
    done
    ok "evil.local (8.8.8.8, 127.0.0.1, broadcast), loopy.local (pointer loop), nothere.local: NXDOMAIN"
    # AAAA (§6.1 L3): asked over the same IPv4 socket; only addresses in
    # vtnet1's /64s count.
    "$V" 'grep "atrium-mdns: .local on vtnet1" /tmp/atrium-net/net.log | tail -n 1' | grep -q 'fd77:0:0:88::/64' \
        || fail "atrium-mdns without the IPv6 prefix: $("$V" 'grep atrium-mdns /tmp/atrium-net/net.log | tail -n 3')"
    a0=$(stat mdns_a)
    r=$("$V" 'drill gatebox.local AAAA @127.0.0.1' 2>&1 || true)
    echo "$r" | grep -qE '^gatebox\.local\.[[:space:]]+[0-9]+[[:space:]]+IN[[:space:]]+AAAA[[:space:]]+fd77:0:0:88::77' && [ "$(stat mdns_a)" -gt "$a0" ] \
        && ok "gatebox.local AAAA = fd77:0:0:88::77 (prefix from SLAAC)" || fail "gatebox.local AAAA: $r"
    r=$("$V" 'drill evil.local AAAA @127.0.0.1' 2>&1 || true)
    echo "$r" | grep -q 'rcode: NOERROR' && ! echo "$r" | grep -qE 'IN[[:space:]]+AAAA[[:space:]]' \
        && ok "evil.local AAAA (::1, 2001:db8::1, fe80::1, the prefix's anycast): no address" || fail "evil.local AAAA: $r"
    r=$("$V" 'getent hosts gatebox.local' 2>&1 || true)
    echo "$r" | grep -qE '^(10\.88\.0\.77|fd77:0:0:88::77)[[:space:]]' && ok "programs resolve it too: getent hosts gatebox.local = $r" || fail "getent: $r"
    "$V" "kill -9 $mp"
    i=0; until [ -n "$("$V" 'pgrep -x atrium-mdns' || true)" ] && "$V" 'local-unbound-control flush gatebox.local >/dev/null; local-unbound-control flush_bogus >/dev/null; local-unbound-control flush_negative >/dev/null' && look gatebox.local | grep -q 10.88.0.77; do i=$((i + 1)); [ $i -lt 30 ] || fail "atrium-mdns not back"; sleep 1; done
    ok "atrium-mdns killed: started again (${i} s), atrium-net running"
    u=$("$V" 'ps -o user= -p $(pgrep -x atrium-netconf)' | tr -d ' ')
    capmode "$("$V" 'pgrep -x atrium-netconf')" && [ "$u" = _atrium-netconf ] && ok "atrium-netconf still $u in capability mode" || fail "netconf: $u"

    # DNS over TLS (§6): a second unbound on 192.0.2.53:853 (lo1) with a
    # test CA plays the machine's own server. It alone knows
    # dot-only.atrium.test and says gate.atrium.test = 10.99.0.77 (the LAN
    # says 10.88.0.77), so an answer shows which server gave it.
    ctl() { "$V" "printf '%s\n' '$1' | nc -U -w 5 /tmp/atrium-net/ctl.sock"; }
    look4() { "$V" "drill $1 A @127.0.0.1" 2>&1 || true; }
    flush() { "$V" 'local-unbound-control flush_zone atrium.test >/dev/null; local-unbound-control flush_bogus >/dev/null; local-unbound-control flush_negative >/dev/null; local-unbound-control flush_requestlist >/dev/null'; }
    "$V" 'set -e; D=/tmp/atrium-net/dot; rm -rf $D; mkdir -p $D; cd $D
        openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 2 -subj /CN=atrium-test-ca >/dev/null 2>&1
        openssl req -newkey rsa:2048 -nodes -keyout srv.key -out srv.csr -subj /CN=dot.atrium.test >/dev/null 2>&1
        printf "subjectAltName=DNS:dot.atrium.test\n" > ext
        openssl x509 -req -in srv.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out srv.pem -days 2 -extfile ext >/dev/null 2>&1
        cat srv.pem ca.pem > chain.pem
        ifconfig lo1 >/dev/null 2>&1 || ifconfig lo1 create
        ifconfig lo1 inet 192.0.2.53/32 alias
        printf "server:\n username: \"\"\n chroot: \"\"\n directory: \"%s\"\n pidfile: \"%s/pid\"\n use-syslog: no\n logfile: \"%s/log\"\n interface: 192.0.2.53@853\n tls-port: 853\n tls-service-key: \"%s/srv.key\"\n tls-service-pem: \"%s/chain.pem\"\n access-control: 0.0.0.0/0 allow\n do-ip6: no\n module-config: \"iterator\"\n local-zone: \".\" static\n local-data: \"dot-only.atrium.test. 60 IN A 10.99.0.1\"\n local-data: \"gate.atrium.test. 60 IN A 10.99.0.77\"\nremote-control:\n control-enable: no\n" $D $D $D $D $D > unbound.conf
        rm -f /var/unbound/conf.d/atrium-tls.conf /var/unbound/atrium-ca.pem
        service local_unbound restart >/dev/null 2>&1
        /usr/sbin/local-unbound -c $D/unbound.conf'
    r=$(ctl 'dns servers=192.0.2.53 tls=dot.atrium.test')
    echo "$r" | grep -q 'error.*no-tls-bundle' && ok "DNS over TLS refused while unbound has no certificates to check with" || fail "no bundle: $r"
    "$V" 'cat /etc/ssl/cert.pem /tmp/atrium-net/dot/ca.pem > /var/unbound/atrium-ca.pem; cp /tmp/atrium-net/unbound-atrium-tls.conf /var/unbound/conf.d/atrium-tls.conf; service local_unbound restart >/dev/null 2>&1'
    i=0; until look4 gate.atrium.test | grep -q 10.88.0.77; do i=$((i + 1)); [ $i -lt 30 ] || fail "unbound not back"; sleep 1; done
    r=$(ctl 'dns servers=192.0.2.53 tls=dot.atrium.test')
    echo "$r" | grep -q '^ok' || fail "dns over TLS: $r"
    flush
    d0=$(stat dns)
    r=$(look4 dot-only.atrium.test)
    echo "$r" | grep -qE 'IN[[:space:]]+A[[:space:]]+10\.99\.0\.1' || fail "dot-only.atrium.test over TLS: $r $("$V" 'tail -n 3 /tmp/atrium-net/net.log; tail -n 5 /tmp/atrium-net/dot/log')"
    r=$(look4 gate.atrium.test)
    echo "$r" | grep -qE 'IN[[:space:]]+A[[:space:]]+10\.99\.0\.77' && [ "$(stat dns)" = "$d0" ] \
        && ok "own server over TLS (192.0.2.53@853, certificate for dot.atrium.test): its answers, nothing to the LAN's DNS" || fail "gate.atrium.test over TLS: $r (lan dns $d0 -> $(stat dns))"
    st=$(ctl status)
    echo "$st" | grep -q '^dns own=192.0.2.53 tls=dot.atrium.test' && "$V" 'grep -q "^dns servers=192.0.2.53 tls=dot.atrium.test" /var/db/atrium-net/settings' \
        && ok "shown on the status line and kept in the settings" || fail "status/settings: $(echo "$st" | grep '^dns')"
    # A certificate for another name: no answers, and never plain DNS instead.
    r=$(ctl 'dns servers=192.0.2.53 tls=wrong.atrium.test')
    flush
    r=$(look4 gate.atrium.test)
    echo "$r" | grep -q 'rcode: SERVFAIL' && [ "$(stat dns)" = "$d0" ] \
        && ok "a certificate for another name: SERVFAIL, nothing in the clear" || fail "wrong name: $r (lan dns $d0 -> $(stat dns))"
    # unbound restarted: list_forwards can't show TLS, so atrium-net sees the
    # restart by its uptime and sets the servers again.
    ctl 'dns servers=192.0.2.53 tls=dot.atrium.test' >/dev/null
    "$V" 'service local_unbound restart >/dev/null 2>&1'
    i=0; until look4 dot-only.atrium.test | grep -q 10.99.0.1; do i=$((i + 1)); [ $i -lt 40 ] || fail "TLS after unbound restart: $("$V" 'local-unbound-control list_forwards; tail -n 3 /tmp/atrium-net/net.log')"; sleep 1; done
    [ "$(stat dns)" = "$d0" ] && ok "local_unbound restarted: over TLS again in ${i}s" || fail "plain DNS after restart (lan dns $d0 -> $(stat dns))"
    r=$(ctl 'dns')
    flush
    i=0; until look4 gate.atrium.test | grep -q 10.88.0.77; do i=$((i + 1)); [ $i -lt 20 ] || fail "back to the LAN's DNS"; sleep 1; done
    ok "turned off: the network's DNS servers again"
    "$V" 'kill $(cat /tmp/atrium-net/dot/pid); ifconfig lo1 destroy'

    # Renewal at T1 (half the lease).
    i=0; while [ "$(stat renew)" -lt 1 ] && [ $i -lt $((LEASE_S)) ]; do sleep 5; i=$((i + 5)); done
    [ "$(stat renew)" -ge 1 ] && wait_addr 10.88.0.50 10 && ok "lease renewed at T1, address kept" || fail "no renewal"

    # Hostile server: restart atrium-net into a LAN that sends bad leases.
    lanctl hostile >/dev/null
    "$V" 'pkill -f "atrium-net -v"; sleep 1; for a in $(ifconfig vtnet1 inet | awk "/inet /{print \$2}"); do ifconfig vtnet1 inet $a -alias; done
        daemon -f -o /tmp/atrium-net/net.log /tmp/atrium-net/atrium-net -v --iface vtnet1 --no-probe --capsicum --lease /tmp/atrium-net/atrium-lease --mdns-bin /tmp/atrium-net/atrium-mdns --sock /tmp/atrium-net/ctl.sock'
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

    # RFC 5227: another host already has the offered address. The lease
    # parser's ARP probes find it; DHCPDECLINE, and the server's next
    # address is used instead. 10.88.0.50 must never be applied meanwhile.
    lanctl conflict >/dev/null
    "$V" 'pkill -f "atrium-net -v"; sleep 1; for a in $(ifconfig vtnet1 inet | awk "/inet /{print \$2}"); do ifconfig vtnet1 inet $a -alias; done
        arp -d 10.88.0.50 >/dev/null 2>&1
        daemon -f -o /tmp/atrium-net/net.log /tmp/atrium-net/atrium-net -v --iface vtnet1 --no-probe --capsicum --lease /tmp/atrium-net/atrium-lease --mdns-bin /tmp/atrium-net/atrium-mdns --sock /tmp/atrium-net/ctl.sock'
    bad=""; i=0
    while [ $i -lt 60 ]; do
        a=$(addrs)
        case " $a " in *" 10.88.0.50 "*) bad=1 ;; esac
        case " $a " in *" 10.88.0.51 "*) break ;; esac
        sleep 1; i=$((i + 1))
    done
    [ "$(stat arp_conflict)" -ge 1 ] && [ "$(stat decline)" -ge 1 ] && [ -z "$bad" ] && wait_addr 10.88.0.51 2 \
        && ok "address in use: ARP probe answered ($(stat arp_conflict)), DHCPDECLINE ($(stat decline)), bound to 10.88.0.51 instead" \
        || fail "conflict: arp=$(stat arp_conflict) decline=$(stat decline) bad=$bad addrs=$(addrs)"
    lanctl noconflict >/dev/null

    echo "atrium-net N0+N2 gate: $pass checks passed"
    ;;
*)
    echo "usage: $0 vm|n0" >&2
    exit 2
    ;;
esac
