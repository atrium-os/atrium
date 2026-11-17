#!/usr/bin/env bash
# Prelum R0 cross-check (docs/spec/printing.md §11): our IPP codec and raster
# writers against CUPS's ippeveprinter, a test oracle on the host only (never
# shipped). Needs ippeveprinter and ippeveps (Debian/Ubuntu: cups-ipp-utils)
# and a running avahi-daemon (ippeveprinter refuses to start without DNS-SD).
#
# Two printers: one whose print command (ippeveps) decodes PWG Raster and URF
# with CUPS's own raster reader into PostScript, whose pixels
# prelum-r0-pscheck.py compares with the pages we drew (an independent decoder
# reading our streams); one that takes PDF.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'kill $p1 $p2 2>/dev/null || true; rm -rf "$work"' EXIT
command -v ippeveprinter >/dev/null || { echo "SKIP: ippeveprinter not installed"; exit 0; }
# ippeveprinter needs DNS-SD: start Avahi (and the system bus) if it isn't up.
if ! avahi-daemon -c 2>/dev/null; then
  mkdir -p /run/dbus; dbus-daemon --system --fork 2>/dev/null || true
  avahi-daemon -D || { echo "SKIP: no avahi-daemon"; exit 0; }
  sleep 1
fi
# Cloud containers may have no IPv6 at all; ippeveprinter insists on an IPv6
# listener, so give it an IPv4 stand-in (test-only shim).
if [ ! -e /proc/net/if_inet6 ] && [ -z "${PRELUM_ORACLE_PRELOAD:-}" ]; then
  cc -shared -fPIC -o "$work/noipv6.so" "$here/scripts/prelum-oracle-noipv6.c" -ldl
  PRELUM_ORACLE_PRELOAD="$work/noipv6.so"
fi
conv=$(command -v ippeveps || echo /usr/sbin/ippeveps)

(cd "$here/prelum" && cargo build -q --example oracle)
oracle="$here/prelum/target/debug/examples/oracle"

mkdir -p "$work/r" "$work/p"
LD_PRELOAD=${PRELUM_ORACLE_PRELOAD:-} ippeveprinter -p 8631 -n 127.0.0.1 -d "$work/r" -k -c "$conv" \
  -f image/pwg-raster,image/urf -F application/postscript -M Atrium -m "Raster Oracle" \
  "Prelum Raster Oracle" >"$work/r.log" 2>&1 &
p1=$!
LD_PRELOAD=${PRELUM_ORACLE_PRELOAD:-} ippeveprinter -p 8632 -n 127.0.0.1 -d "$work/p" -k -2 \
  -f application/pdf,image/pwg-raster,image/urf -M Atrium -m "PDF Oracle" \
  "Prelum PDF Oracle" >"$work/p.log" 2>&1 &
p2=$!
for i in $(seq 50); do
  (exec 3<>/dev/tcp/127.0.0.1/8631 && exec 3<>/dev/tcp/127.0.0.1/8632) 2>/dev/null && break
  sleep 0.2
done

rc=0
echo "== raster printer"; "$oracle" 127.0.0.1:8631 raster || rc=1
echo "== PDF printer";    "$oracle" 127.0.0.1:8632 pdf || rc=1
# By now both printers have long finished registering with Avahi.
(cd "$here/prelum" && cargo build -q --example probe)
echo "== mDNS: our query and parser against Avahi (legacy unicast)"
ok=0
for i in 1 2 3; do "$here/prelum/target/debug/examples/probe" 127.0.0.1:5353 && { ok=1; break; }; sleep 1; done
[ $ok -eq 1 ] || rc=1
if [ $rc -ne 0 ]; then
  echo "-- raster printer log"; tail -40 "$work/r.log"
  echo "-- PDF printer log"; tail -20 "$work/p.log"
fi
[ -n "${PRELUM_ORACLE_KEEP:-}" ] && cp -r "$work" "$PRELUM_ORACLE_KEEP"
if grep -q "ERROR" "$work/r.log"; then echo "FAIL: the oracle's converter reported errors:"; grep ERROR "$work/r.log"; rc=1; fi
# The oracle draws US Letter at the printer's first PWG resolution (300 dpi
# here), sRGB, 2 pages; and at the URF resolution (600 dpi), sGray, 1 page.
pwg_ps=$(ls "$work"/r/1-*.ps "$work"/r/1-*.prn 2>/dev/null | head -1)
urf_ps=$(ls "$work"/r/2-*.ps "$work"/r/2-*.prn 2>/dev/null | head -1)
python3 "$here/scripts/prelum-r0-pscheck.py" "$pwg_ps" 2550 3300 3 2 || rc=1
python3 "$here/scripts/prelum-r0-pscheck.py" "$urf_ps" 5100 6600 1 1 || rc=1
[ $rc -eq 0 ] && echo "PASS: all oracle checks"
exit $rc
