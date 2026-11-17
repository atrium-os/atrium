#!/bin/sh
# Prelum R2 (docs/spec/printing.md §11): prelumd, prelum-ipp and
# prelum-render end to end against CUPS's ippeveprinter, a test oracle on the
# host only (never shipped; Debian/Ubuntu: cups-ipp-utils, plus avahi-daemon,
# which ippeveprinter insists on). Workers run unjailed (`prelumd --direct`):
# the jails are checked in the FreeBSD VM (scripts/prelum-r2-vm.sh).
#
#   A  IPPS, PDF and raster, two-sided: add by URI (key and UUID learned),
#      print a PDF (sent as the rewritten PDF), refresh.
#   B  plain IPP, raster only: added by URI, marked unencrypted; the job
#      arrives as PWG Raster.
#   C  a slow printer: cancel while it prints; the printer hears Cancel-Job.
#   D  A behind a TCP relay: relay down = offline, the job waits and prints
#      when the relay comes back.
#   E  identity: A restarted with a new key is held, not printed to; "use
#      the new identity" pins it again and releases the job. Another printer
#      at B's address (a new UUID, no key) likewise.
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

(cd "$here/prelum" && cargo build -q -p prelumd -p prelum-ipp -p prelum-render --bins)
bin="$here/prelum/target/debug"
pdf="$here/prelum/prelum-ipp-codec/examples/min.pdf"
sock="$work/print.sock"
ctl() { "$bin/prelumctl" --socket "$sock" "$@"; }

fails=0
port_up() { python3 -I -c "import socket,sys; socket.create_connection(('127.0.0.1', int(sys.argv[1])), 1)" "$1" 2>/dev/null; }
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }

# printer NAME PORT DIR KEYS ARGS... : start one ippeveprinter, wait for it.
printer() {
  name=$1 port=$2 dir=$3 keys=$4; shift 4
  mkdir -p "$dir" "$keys"
  LD_PRELOAD=$preload ippeveprinter -p "$port" -n 127.0.0.1 -d "$dir" -K "$keys" -k "$@" "$name" >>"$dir.log" 2>&1 &
  last=$!
  pids="$pids $last"
  for i in $(seq 50); do
    port_up "$port" && return 0
    sleep 0.2
  done
  echo "printer $name did not start"; cat "$dir.log"; exit 1
}

# relay LISTEN TARGET: a TCP relay (the offline test pulls it).
relay() {
  python3 -I -c '
import socket, sys, threading
l = socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
l.bind(("127.0.0.1", int(sys.argv[1]))); l.listen(16)
def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d: break
            b.sendall(d)
    except OSError: pass
    for s in (a, b):
        try: s.shutdown(socket.SHUT_RDWR)
        except OSError: pass
while True:
    c, _ = l.accept()
    u = socket.create_connection(("127.0.0.1", int(sys.argv[2])))
    threading.Thread(target=pipe, args=(c, u), daemon=True).start()
    threading.Thread(target=pipe, args=(u, c), daemon=True).start()
' "$1" "$2" &
  relay_pid=$!
  pids="$pids $relay_pid"
  for i in $(seq 50); do port_up "$1" && return 0; sleep 0.1; done
}

# phase JOB: the job's phase now.
phase() { ctl jobs | awk -v j="$1" '$1 == j { print $4 }'; }
# wait_phase JOB PHASE SECONDS
wait_phase() {
  for i in $(seq $(($3 * 5))); do
    [ "$(phase "$1")" = "$2" ] && return 0
    sleep 0.2
  done
  echo "  job $1 is $(phase "$1"), not $2:"; ctl jobs | sed 's/^/  /'
  return 1
}
job_of() { sed -n 's/^job //p'; }

slow="$work/slow.sh"
printf '#!/bin/sh\nsleep 60\n' >"$slow"; chmod +x "$slow"

printer "Prelum R2 A" 8641 "$work/a" "$work/keys-a" -2 -f application/pdf,image/pwg-raster,image/urf -M Atrium -m "R2 A"
pid_a=$last
printer "Prelum R2 B" 8642 "$work/b" "$work/keys-b" -f image/pwg-raster,image/urf -M Atrium -m "R2 B"
pid_b=$last
printer "Prelum R2 C" 8643 "$work/c" "$work/keys-c" -f application/pdf -c "$slow" -M Atrium -m "R2 C"

"$bin/atrium-prelumd" --direct "$bin" --socket "$sock" --state "$work/state" --spool "$work/spool" >"$work/prelumd.log" 2>&1 &
pids="$pids $!"
for i in $(seq 50); do [ -S "$sock" ] && break; sleep 0.1; done

echo "== A: IPPS, PDF"
out=$(ctl add ipps://127.0.0.1:8641/ipp/print)
echo "$out" | sed 's/^/  /'
fp_a=$(echo "$out" | sed -n 's/^ *key //p')
want=$(openssl x509 -in "$work/keys-a/127.0.0.1.crt" -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64 | tr -d '=')
[ "$fp_a" = "SHA256:$want" ] && ok "A's key pinned ($fp_a, as openssl reads its certificate)" || bad "A's key $fp_a, openssl says SHA256:$want"
echo "$out" | grep -q "encrypted" && echo "$out" | grep -q " new" && ok "A is encrypted and new" || bad "A not shown encrypted and new"
j=$(ctl print "$pdf" --printer 1 --title "R2 test" | job_of)
wait_phase "$j" Done 60 && ok "job $j printed on A" || bad "job $j on A"
f=$(ls "$work/a"/*.pdf 2>/dev/null | head -1)
if [ -n "$f" ] && head -c 5 "$f" | grep -q '%PDF-' && pdfinfo "$f" >/dev/null 2>&1; then
  grep -aq "/Subtype /Form" "$f" && ok "A received the rewritten PDF ($(wc -c <"$f") bytes, pdfinfo reads it)" || bad "A's PDF is not the rewrite"
else
  bad "A received no readable PDF"
fi
ctl printers | grep -q "Prelum R2 A  \[idle\]\$" && ok "A no longer new" || bad "A still new: $(ctl printers | head -1)"

echo "== B: plain IPP, raster"
out=$(ctl add ipp://127.0.0.1:8642/ipp/print "Plain B")
echo "$out" | grep -q "NOT ENCRYPTED" && ! echo "$out" | grep -q "key " && ok "B marked unencrypted, no key" || bad "B: $out"
j=$(ctl print "$pdf" --printer "Plain B" --gray --quality draft | job_of)
wait_phase "$j" Done 60 && ok "job $j printed on B" || bad "job $j on B"
f=$(ls "$work/b"/* 2>/dev/null | head -1)
[ -n "$f" ] && [ "$(head -c 4 "$f")" = "RaS2" ] && ok "B received PWG Raster ($(wc -c <"$f") bytes)" || bad "B received $(head -c 8 "$f" 2>/dev/null | od -c | head -1)"

echo "== C: cancel while printing"
ctl add ipps://127.0.0.1:8643/ipp/print "Slow C" >/dev/null
j=$(ctl print "$pdf" --printer "Slow C" | job_of)
wait_phase "$j" Printing 30 && ok "job $j is printing on C" || bad "job $j never printing"
ctl cancel "$j" >/dev/null
wait_phase "$j" Cancelled 30 && ok "job $j cancelled" || bad "job $j not cancelled"
sleep 1
grep -q "Cancel-Job successful-ok" "$work/c.log" && ok "C heard Cancel-Job" || bad "C never got Cancel-Job"

echo "== D: offline"
relay 8644 8641
ctl add ipps://127.0.0.1:8644/ipp/print "Relayed A" >/dev/null
kill "$relay_pid"; wait "$relay_pid" 2>/dev/null || true
j=$(ctl print "$pdf" --printer "Relayed A" | job_of)
sleep 3
[ "$(phase "$j")" = Pending ] && ctl jobs | grep "^ *$j " | grep -q offline && ok "job $j waits: printer offline" || bad "job $j: $(ctl jobs | grep "^ *$j ")"
ctl printers | grep -q "Relayed A  \[offline\]" && ok "Relayed A shown offline" || bad "Relayed A not offline"
relay 8644 8641
wait_phase "$j" Done 90 && ok "job $j printed once the printer came back" || bad "job $j after coming back"

echo "== E: identity changed"
kill "$pid_a"; wait "$pid_a" 2>/dev/null || true
rm -rf "$work/keys-a"
printer "Prelum R2 A" 8641 "$work/a" "$work/keys-a" -2 -f application/pdf,image/pwg-raster,image/urf -M Atrium -m "R2 A"
before=$(ls "$work/a" | wc -l)
ctl refresh 1 | grep -q "IDENTITY CHANGED" && ok "refresh: A's identity changed" || bad "refresh didn't see A's new key"
j=$(ctl print "$pdf" --printer 1 | job_of)
wait_phase "$j" Held 30 && ok "job $j held, not sent" || bad "job $j not held"
[ "$(ls "$work/a" | wc -l)" = "$before" ] && ok "A received nothing" || bad "A received a file while held"
out=$(ctl repin 1)
new_fp=$(echo "$out" | sed -n 's/^ *key //p')
[ -n "$new_fp" ] && [ "$new_fp" != "$fp_a" ] && ok "A pinned again ($new_fp)" || bad "repin: $out"
wait_phase "$j" Done 60 && ok "job $j printed after repin" || bad "job $j after repin"

# Another printer at B's address (B replaced): ippeveprinter derives its
# UUID from name, host and port.
kill "$pid_b"; wait "$pid_b" 2>/dev/null || true
printer "Prelum R2 B2" 8642 "$work/b" "$work/keys-b" -f image/pwg-raster,image/urf -M Atrium -m "R2 B"
j=$(ctl print "$pdf" --printer "Plain B" | job_of)
if wait_phase "$j" Held 30; then
  ok "B (plain IPP) with a new UUID: job $j held"
  ctl repin "Plain B" >/dev/null
  wait_phase "$j" Done 60 && ok "job $j printed after repin" || bad "job $j after repin"
else
  bad "B's new UUID not caught"
fi

echo
if [ "$fails" = 0 ]; then echo "PASS: Prelum R2 end to end"; else echo "FAILED: $fails"; echo "--- prelumd log"; tail -30 "$work/prelumd.log"; exit 1; fi
