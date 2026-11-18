#!/bin/sh
# Prelum R3 (docs/spec/printing.md §7.1, §11) on the host: prelumd finds
# printers by DNS-SD, through prelum-browse, and confirms each over IPP.
# CUPS's ippeveprinter is the printer and the host's Avahi the responder
# (test oracles, never shipped; Debian/Ubuntu: cups-ipp-utils,
# avahi-daemon). A container has no LAN multicast, so prelumd asks Avahi by
# legacy unicast (`--mdns-legacy 127.0.0.1:5353`); the LAN path, with
# atrium-net's sockets and the worker jailed, is the VM's
# (scripts/prelum-r3-vm.sh). The flood is prelum-browse's own test
# (prelum-browse/tests/lan.rs), run here too.
#
#   A  IPPS printer: found, confirmed (its key as openssl reads it), added
#      from the list, printed to, then shown as saved.
#   B  plain IPP printer: found and shown unencrypted. (A offers both; it
#      is listed once, by IPPS.)
#   B  then leaves: gone from the list once Avahi forgets it.
#
# Two printers, because Avahi fits a legacy-unicast answer into 512 bytes
# by leaving instances out (without setting TC): a third printer's PTR may
# never come back this way. Multicast answers on a LAN have no such limit.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
pids=""
cleanup() {
  for p in $pids; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  [ -n "${KEEP:-}" ] && echo "work dir: $work" || rm -rf "$work"
}
trap cleanup EXIT INT TERM

command -v ippeveprinter >/dev/null || { echo "SKIP: ippeveprinter not installed"; exit 0; }
if ! avahi-daemon -c 2>/dev/null; then
  mkdir -p /run/dbus
  if ! dbus-daemon --system --fork 2>/dev/null; then
    rm -f /run/dbus/pid /run/dbus/system_bus_socket; dbus-daemon --system --fork 2>/dev/null || true
  fi
  rm -f /var/run/avahi-daemon/pid
  avahi-daemon -D || { echo "SKIP: no avahi-daemon"; exit 0; }
  sleep 1
fi
preload=${PRELUM_ORACLE_PRELOAD:-}
if [ ! -e /proc/net/if_inet6 ] && [ -z "$preload" ]; then
  cc -shared -fPIC -o "$work/noipv6.so" "$here/scripts/prelum-oracle-noipv6.c" -ldl
  preload="$work/noipv6.so"
fi

(cd "$here/prelum" && cargo build -q -p prelumd -p prelum-ipp -p prelum-render -p prelum-browse --bins)
bin="$here/prelum/target/debug"
pdf="$here/prelum/prelum-ipp-codec/examples/min.pdf"
sock="$work/print.sock"
ctl() { "$bin/prelumctl" --socket "$sock" "$@"; }

fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }
port_up() { python3 -I -c "import socket,sys; socket.create_connection(('127.0.0.1', int(sys.argv[1])), 1)" "$1" 2>/dev/null; }

printer() {
  name=$1 port=$2 dir=$3 keys=$4; shift 4
  mkdir -p "$dir"
  # No key dir: plain IPP only (ippeveprinter then announces only _ipp).
  if [ -n "$keys" ]; then mkdir -p "$keys"; set -- -K "$keys" "$@"; fi
  LD_PRELOAD=$preload ippeveprinter -p "$port" -n 127.0.0.1 -d "$dir" -k "$@" "$name" >>"$dir.log" 2>&1 &
  last=$!
  pids="$pids $last"
  for i in $(seq 50); do port_up "$port" && return 0; sleep 0.2; done
  echo "printer $name did not start"; cat "$dir.log"; exit 1
}

echo "== prelum-browse under a flood (its own test)"
(cd "$here/prelum" && cargo test -q -p prelum-browse --test lan 2>&1 | grep -q "test result: ok. 2 passed") \
  && ok "the browser stays at 256 services and keeps answering through 3000 hostile datagrams" || bad "prelum-browse flood test"

printer "Prelum R3 A" 8651 "$work/a" "$work/keys-a" -2 -f application/pdf,image/pwg-raster,image/urf -M Atrium -m "R3 A" -l "Room 3"
printer "Prelum R3 B" 8652 "$work/b" "" -f image/pwg-raster,image/urf -M Atrium -m "R3 B"
pid_b=$last
# Avahi takes a moment to register each.
sleep 3

"$bin/atrium-prelumd" --direct "$bin" --socket "$sock" --state "$work/state" --spool "$work/spool" --mdns-legacy 127.0.0.1:5353 >"$work/prelumd.log" 2>&1 &
pids="$pids $!"
for i in $(seq 50); do [ -S "$sock" ] && break; sleep 0.1; done

echo "== nearby"
out=$(ctl nearby --wait 6)
echo "$out" | sed 's/^/  /'
uri_a=$(echo "$out" | awk '/^ipps:\/\/[^ ]*:8651\//{print $1; exit}')
[ -n "$uri_a" ] && ok "A found over mDNS: $uri_a" || bad "A not found"
blk() { echo "$out" | awk -v u="$1" 'index($0, u) == 1 {p=1; print; next} /^[^ ]/ {p=0} p'; }
want=$(openssl x509 -in "$work/keys-a/127.0.0.1.crt" -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64 | tr -d '=')
blk "$uri_a" | grep -q "key SHA256:$want" && ok "A confirmed over IPP: its key is the one openssl reads" || bad "A's key: $(blk "$uri_a" | grep key)"
blk "$uri_a" | grep -q "model Atrium R3 A" && ok "A described by itself (make and model from Get-Printer-Attributes)" || bad "A's model"
blk "$uri_a" | grep -q "not saved" && ok "A not yet saved" || bad "A shown saved"
echo "$out" | grep -q '^ipp://[^ ]*:8651/' && bad "A listed twice (IPP and IPPS)" || ok "A listed once, by IPPS, though it also offers IPP"
echo "$out" | grep -q '^ipp://[^ ]*:8652/ipp/print  \[NOT ENCRYPTED\]' && ok "B found and shown unencrypted" || bad "B"

echo "== add from the list and print"
ctl add "$uri_a" >/dev/null && ok "A added by the URI the list gave" || bad "add A"
j=$(ctl print "$pdf" --printer 1 --title "R3" | sed -n 's/^job //p')
for i in $(seq 150); do [ "$(ctl jobs | awk -v j="$j" '$1 == j {print $4}')" = Done ] && break; sleep 0.2; done
[ "$(ctl jobs | awk -v j="$j" '$1 == j {print $4}')" = Done ] && ok "job $j printed on A" || bad "job $j: $(ctl jobs)"
out=$(ctl nearby --wait 2)
blk "$uri_a" | grep -q "saved as 1" && ok "the list now shows A as saved printer 1" || bad "A not shown saved: $(blk "$uri_a")"

echo "== a printer leaves"
kill "$pid_b"; wait "$pid_b" 2>/dev/null || true
# Avahi withdraws it; legacy answers live 10 s; the browser asks every 5 s.
gone=""
for i in $(seq 8); do
  out=$(ctl nearby --wait 4)
  echo "$out" | grep -q ':8652/' || { gone=1; break; }
done
[ -n "$gone" ] && ok "B gone from the list after it stopped" || bad "B still listed"
echo "$out" | grep -q ':8651/' && ok "A still listed" || bad "A dropped"

echo
if [ "$fails" = 0 ]; then echo "PASS: Prelum R3 on the host"; else echo "FAILED: $fails"; echo "--- prelumd log"; tail -30 "$work/prelumd.log"; exit 1; fi
