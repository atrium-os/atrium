#!/bin/sh
# Prelum R3 in the FreeBSD VM (docs/spec/printing.md §7.1, §11): printers
# found on the LAN by mDNS, through atrium-net's sockets and a jailed
# prelum-browse, then confirmed over IPP by a jailed prelum-ipp.
#
#   sh scripts/atrium-net-gate.sh vm   # (once) the VM with its second NIC on
#                                      # atrium-net/sim/lan.py's fake LAN
#   sh scripts/prelum-r3-vm.sh
#
# The LAN (lan.py on this host) answers mDNS for "Gate Printer" at
# 10.0.2.2:8661: CUPS's ippeveprinter on this host (a test oracle, never
# shipped), which the guest reaches over vtnet0. atrium-net runs on vtnet1
# with --no-egress-group, so the ipp jails' NAT stays on vtnet0.
#
# Checks: atrium-net hands prelumd the two sockets (atrium-netconf's
# print-discovery); the browser runs jailed with no network and in
# capability mode; the printer is found, confirmed (its key, its own name),
# added from the list and printed to; a flood of hostile answers leaves the
# browser bounded (memory, list size) and the printer still listed; nobody
# looking for a minute stops the browser.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
VSSH="$ROOT/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP="scp -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
TARGET=aarch64-unknown-freebsd
work=$(mktemp -d)
FAILS=0
STAGE=startup
pids=""
port_up() { python3 -I -c "import socket,sys; socket.create_connection(('127.0.0.1', int(sys.argv[1])), 1)" "$1" 2>/dev/null; }
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED [%s]: %s\n' "$STAGE" "$*" >&2; exit 1; }
check() { # check <label> <ok 0/1> <detail>
    if [ "$2" = 0 ]; then printf 'PASS %-58s %s\n' "$1" "$3"; else printf 'FAIL %-58s %s\n' "$1" "$3"; FAILS=$((FAILS+1)); fi
}
g() { "$VSSH" "$@"; }
lanctl() {
    python3 -I - "$1" <<'PY'
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(5)
s.sendto(sys.argv[1].encode(), ("127.0.0.1", 7803)); print(s.recv(4096).decode())
PY
}
stat() { lanctl stats | python3 -I -c "import json,sys; print(json.load(sys.stdin).get('$1', 0))"; }
cleanup() {
    g 'pkill -f "prelumd --socket /tmp/prelum.sock"; pkill -x atrium-netconf; pkill -f "atrium-net -v"; sleep 1
        for a in $(ifconfig vtnet1 inet | awk "/inet /{print \$2}"); do ifconfig vtnet1 inet $a -alias; done
        g=$(cat /tmp/atrium-net/default.saved 2>/dev/null); [ -n "$g" ] && { while route -q delete default 2>/dev/null; do :; done; route -q add default $g; }
        true' >/dev/null 2>&1
    for p in $pids; do kill "$p" 2>/dev/null; done
    [ -n "${KEEP:-}" ] && echo "work dir: $work" || rm -rf "$work"
}
trap cleanup EXIT INT TERM

STAGE=build
say "$STAGE (cross, on this host)"
CROSS_RUSTFLAGS=$(cd "$ROOT" && sh -c '. ./scripts/cloud-vm.sh >/dev/null 2>&1; printf %s "$CROSS_RUSTFLAGS"' sh env)
[ -n "$CROSS_RUSTFLAGS" ] || die "cloud-vm.sh env"
export CC_aarch64_unknown_freebsd=clang AR_aarch64_unknown_freebsd=llvm-ar
export CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$VMDIR/sysroot"
(cd "$ROOT/prelum" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TARGET -p prelumd -p prelum-ipp -p prelum-render -p prelum-browse --bins) || die "prelum cross-build"
(cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TARGET -p jaild) || die "jaild cross-build"
(cd "$ROOT/atrium-net" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TARGET -p atrium-net -p atrium-lease -p atrium-netconf) || die "atrium-net cross-build"

STAGE=deploy
say "$STAGE"
g 'ifconfig vtnet1 >/dev/null 2>&1' || die "no vtnet1: boot the VM with sh scripts/atrium-net-gate.sh vm"
START=1 sh "$ROOT/scripts/deploy-portcullis.sh" >"$work/deploy.log" 2>&1 \
    || grep -q "atrium-jaild  *RUNNING" "$work/deploy.log" \
    || { tail -20 "$work/deploy.log"; die "deploy-portcullis"; }
g 'grep -q "^browse_exec" /etc/atrium/jaild.policy.toml' || die "jaild's policy has no browse_exec"
B="$ROOT/prelum/target/$TARGET/release"
N="$ROOT/atrium-net/target/$TARGET/release"
g 'mkdir -p /root/prelum /tmp/atrium-net'
put() { # SRC DEST
    $SCP "$1" "root@localhost:$2" >/dev/null || die "scp $1"
    [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$(g "sha256 -q $2")" ] || die "$1 arrived damaged"
}
for b in atrium-prelumd prelumctl atrium-prelum-ipp atrium-prelum-render atrium-prelum-browse; do put "$B/$b" "/root/prelum/$b"; done
for b in atrium-net atrium-lease atrium-netconf atrium-netctl; do put "$N/$b" "/tmp/atrium-net/$b"; done
g 'pkill -f "prelumd --socket /tmp/prelum.sock"; sleep 1; for b in atrium-prelumd prelumctl atrium-prelum-ipp atrium-prelum-render atrium-prelum-browse; do install -m 0555 /root/prelum/$b /usr/local/bin/$b; done; chmod 0755 /tmp/atrium-net/*' || die "install"
put "$ROOT/prelum/prelum-ipp-codec/examples/min.pdf" /root/prelum/min.pdf

STAGE=lan
say "$STAGE (lan.py, atrium-net on vtnet1)"
pkill -f atrium-net/sim/lan.py 2>/dev/null
python3 "$ROOT/atrium-net/sim/lan.py" 600 >"$work/lan.log" 2>&1 &
pids="$pids $!"
sleep 1
g 'set -e
    pkill -x atrium-netconf 2>/dev/null || true; pkill -f "atrium-net -v" 2>/dev/null || true
    for a in $(ifconfig vtnet1 inet | awk "/inet /{print \$2}"); do ifconfig vtnet1 inet $a -alias; done
    id _atrium-lease >/dev/null 2>&1 || pw useradd _atrium-lease -u 65034 -d /nonexistent -s /usr/sbin/nologin
    id _atrium-netconf >/dev/null 2>&1 || pw useradd _atrium-netconf -u 65036 -d /nonexistent -s /usr/sbin/nologin
    mkdir -p /var/empty
    route -n get default 2>/dev/null | awk "/gateway/{print \$2}" > /tmp/atrium-net/default.saved
    daemon -f -o /tmp/atrium-net/net.log /tmp/atrium-net/atrium-net -v --iface vtnet1 --no-probe --no-unbound --no-egress-group --capsicum --lease /tmp/atrium-net/atrium-lease --sock /tmp/atrium-net/ctl.sock
    for i in $(seq 120); do ifconfig vtnet1 inet | grep -q "inet 10.88.0.50 " && break; sleep 1; done
    ifconfig vtnet1 inet | grep -q "inet 10.88.0.50 "
    rm -rf /tmp/atrium-net/sockets
    daemon -f -o /tmp/atrium-net/netconf.log /tmp/atrium-net/atrium-netconf --ctl /tmp/atrium-net/ctl.sock --dir /tmp/atrium-net/sockets --uid 65036 --capsicum
    for i in $(seq 30); do [ -S /tmp/atrium-net/sockets/print-discovery/print-discovery.sock ] && exit 0; sleep 1; done; exit 1' \
    || die "atrium-net/netconf: $(g 'tail -20 /tmp/atrium-net/net.log /tmp/atrium-net/netconf.log')"
check "atrium-net on vtnet1 (10.88.0.50), print-discovery socket up" 0 ""

STAGE=printer
say "$STAGE (ippeveprinter on this host)"
command -v ippeveprinter >/dev/null || die "ippeveprinter not installed (cups-ipp-utils)"
if ! avahi-daemon -c 2>/dev/null; then
    mkdir -p /run/dbus
    dbus-daemon --system --fork 2>/dev/null || { rm -f /run/dbus/pid /run/dbus/system_bus_socket; dbus-daemon --system --fork; }
    rm -f /var/run/avahi-daemon/pid; avahi-daemon -D; sleep 1
fi
preload=""
if [ ! -e /proc/net/if_inet6 ]; then
    cc -shared -fPIC -o "$work/noipv6.so" "$ROOT/scripts/prelum-oracle-noipv6.c" -ldl
    preload="$work/noipv6.so"
fi
pkill -f "ippeveprinter -p 8661" 2>/dev/null
mkdir -p "$work/a" "$work/keys-a"
LD_PRELOAD=$preload ippeveprinter -p 8661 -n 127.0.0.1 -d "$work/a" -K "$work/keys-a" -k -f application/pdf,image/pwg-raster -M Atrium -m "VM R3" "Prelum VM R3" >"$work/a.log" 2>&1 &
pids="$pids $!"
for i in $(seq 50); do port_up 8661 && break; sleep 0.2; done
port_up 8661 || die "printer did not start"
lanctl "mdns-printer 10.0.2.2 8661" >/dev/null

STAGE=prelumd
say "$STAGE (jailed workers, sockets from atrium-net)"
g 'rm -rf /tmp/prelum-state /tmp/prelum-spool; daemon -f -o /tmp/prelumd.log atrium-prelumd --socket /tmp/prelum.sock --state /tmp/prelum-state --spool /tmp/prelum-spool --netconf /tmp/atrium-net/sockets </dev/null >/dev/null 2>&1; for i in $(seq 120); do [ -S /tmp/prelum.sock ] && exit 0; sleep 0.5; done; exit 1' || die "prelumd did not start: $(g 'cat /tmp/prelumd.log')"
ctl() { g "prelumctl --socket /tmp/prelum.sock $*"; }
# The browse jail: the one running atrium-prelum-browse.
bjail() { g 'for j in $(jls name | grep "^system-prelum-"); do pgrep -j $j -f atrium-prelum-browse >/dev/null 2>&1 && echo $j; done; true' | head -1; }

STAGE=nearby
say "$STAGE"
out=$(ctl nearby --wait 10)
echo "$out" | sed 's/^/  /'
uri=$(echo "$out" | awk '/^ipps:\/\/10\.0\.2\.2:8661\//{print $1; exit}')
check "Gate Printer found on vtnet1 and confirmed over IPPS" "$([ -n "$uri" ]; echo $?)" "$uri"
want="SHA256:$(openssl x509 -in "$work/keys-a/127.0.0.1.crt" -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64 | tr -d '=')"
echo "$out" | grep -q "key $want" ; check "its key, as openssl reads its certificate" $? "$want"
echo "$out" | grep -q 'name Prelum VM R3  (announced as "Gate Printer")'; check "listed by its own name, not the announced one" $? ""
echo "$out" | grep -q '^ipp://10.0.2.2:8661/'; check "listed once (IPPS, not also IPP)" "$([ $? != 0 ]; echo $?)" ""
[ "$(stat mdns_query)" -ge 1 ]; check "QU queries reached the LAN" $? "$(stat mdns_query) queries"
j=$(bjail)
info=$(g "jls -j $j -n vnet ip4 2>/dev/null")
echo "$info" | grep -q "ip4=disable" && ! echo "$info" | grep -q "vnet=new"; check "prelum-browse jailed with no network" $? "$j: $info"
p=$(g "pgrep -j $j -f atrium-prelum-browse | head -1")
g "procstat -s $p | awk 'NR==2{print \$10}'" | grep -q C; check "prelum-browse in capability mode" $? "pid $p"
g "sockstat -4 -p $p 2>/dev/null; sockstat -4 | grep -w $p" >"$work/sockstat.txt" 2>&1
grep -q "5353" "$work/sockstat.txt"; check "its only inet sockets are atrium-net's two on 5353" $? "$(grep -c 5353 "$work/sockstat.txt") lines"

STAGE=add
say "$STAGE"
ctl add "$uri" >/dev/null; check "added from the list" $? ""
j=$(ctl print /root/prelum/min.pdf --printer 1 --title R3 | sed -n 's/^job //p')
t0=$(date +%s); ph=""
while [ $(( $(date +%s) - t0 )) -lt 300 ]; do ph=$(ctl jobs | awk -v j="$j" '$1 == j {print $4}'); [ "$ph" = Done ] && break; sleep 3; done
check "job $j printed" "$([ "$ph" = Done ]; echo $?)" "$ph"
ctl nearby --wait 3 | grep -q "saved as 1"; check "the list shows it saved" $? ""

STAGE=flood
say "$STAGE"
lanctl mdns-flood >/dev/null
for i in 1 2 3; do ctl nearby --wait 8 >"$work/flood.txt" 2>&1 || true; done
sent=$(stat mdns_answer)
j=$(bjail); p=$(g "pgrep -j $j -f atrium-prelum-browse | head -1")
rss=$(g "ps -o rss= -p $p" | tr -d ' ')
check "browser alive after the flood" "$([ -n "$p" ]; echo $?)" "$sent hostile answers sent"
check "its memory stays small" "$([ "${rss:-999999}" -lt 32768 ]; echo $?)" "${rss} KB resident"
grep -q "^ipps://10.0.2.2:8661/" "$work/flood.txt"; check "the printer still listed" $? ""
n=$(grep -c "://" "$work/flood.txt")
check "no junk listed (none answered over IPP)" "$([ "$n" = 1 ]; echo $?)" "$n listed"
lanctl mdns-off >/dev/null

STAGE=idle
say "$STAGE"
t0=$(date +%s)
while [ $(( $(date +%s) - t0 )) -lt 120 ]; do [ -z "$(bjail)" ] && break; sleep 5; done
check "browser stopped once nobody looked for a minute" "$([ -z "$(bjail)" ]; echo $?)" "after $(( $(date +%s) - t0 )) s"
sleep 10
left=$(g 'jls name | grep -c "^system-prelum-"')
check "no worker jail left" "$([ "$left" = 0 ]; echo $?)" "$left left"

echo
if [ "$FAILS" = 0 ]; then echo "PASS: Prelum R3 in the VM"; else echo "FAILED: $FAILS"; g 'tail -20 /tmp/prelumd.log'; exit 1; fi
