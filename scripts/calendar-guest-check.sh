#!/bin/sh
# Run in a fresh directory on a disposable guest. Stage the two arm64 binaries,
# planning and service test executables, and a headless-capable frescod here.
set -eu
D=$(pwd)
test ! -e "$D/store"
test ! -e "$D/registry"
test "$(uname -s)" = FreeBSD
test "$(uname -m)" = arm64
./planning
FASTI_TEST_BIN="$D/fastid" ./service
mkdir -m 700 store
mkdir -m 755 fasti
chown daemon store fasti
signer=abababababababababababababababababababababababababababababababab
printf '%s mina org.atrium.calendar %s persona=work\n' "$(id -u nobody)" "$signer" >registry
daemon -f -p "$D/fasti.pid" -o "$D/fasti.log" -u daemon env FASTI_STORE="$D/store" FASTI_SOCKET="$D/fasti/calendar.sock" FASTI_REGISTRY="$D/registry" FASTI_USER=mina FASTI_PERSONA=work FASTI_PUBLISHER="$signer" "$D/fastid"
daemon -f -p "$D/fresco.pid" -o "$D/fresco.log" env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG="$D/screen" FRESCOD_SOCK="$D/fresco.sock" FRESCOD_WM_SOCK="$D/wm.sock" FRESCOD_INPUT_SOCK="$D/inject.sock" FRESCOD_ACTIVATION_SOCK="$D/act.sock" FRESCOD_DEV_CAPS=all "$D/frescod"
for sock in fasti/calendar fresco inject; do
    for i in $(jot 30); do test -S "$sock.sock" && break; sleep 1; done
    test -S "$sock.sock"
done
chmod 666 fasti/calendar.sock fresco.sock
daemon -f -p "$D/app.pid" -o "$D/app.log" -u nobody env FASTI_SOCKET="$D/fasti/calendar.sock" FRESCO_SOCKET="$D/fresco.sock" "$D/atrium-calendar"
inj() { printf '%s\n' "$@" | nc -w 3 -N -U inject.sock; }
press() { inj "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'; sleep 1; }
for i in $(jot 30); do test -s screen.png && break; sleep 1; done
test -s screen.png
sleep 2
press 220 97
# HID usages for “budget”.
for key in 5 24 7 10 8 23; do inj "KEY $key 1 0" "KEY $key 0 0"; done
press 205 138
for i in $(jot 10); do test -f store/calendar.json && break; sleep 1; done
grep -q '"title":"budget"' store/calendar.json
cp screen.png saved.png
press 570 137
press 584 223
grep -q '"kind":"Hold"' store/calendar.json
grep -q '"start":570,"end":615' store/calendar.json
press 570 137
cp screen.png held.png
press 584 260
grep -q '"revision":3' store/calendar.json
if grep -q '"kind":"Hold"' store/calendar.json; then exit 1; fi
echo 'PASS: manual event, Find 45 min, explicit hold and release through the native window'
