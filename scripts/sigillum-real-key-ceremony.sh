#!/bin/sh
# Register then assert with a REAL FIDO key, the PIN typed by a person: run
# it as root IN THE GATE VM, at a terminal, after
# `ARCH=aarch64 SIGILLUM_REAL_KEY=1 sh scripts/vm-sigillum-check.sh` has
# commissioned sigillumd and the key's driver jail (docs/spec/sigillum.md §16.2).
#
#   sh /root/s/sigillum-real-key-ceremony.sh
#
# The trusted UI (`sigillum-probe ui-tty`, as _sigillum-ui) runs in the
# foreground on this terminal: it asks for the key's PIN with echo off, once
# per ceremony, and stops at the first wrong PIN (or when 3 or fewer tries
# are left). The PIN goes from the terminal to sigillumd over the UI socket
# only: never a file, argument, environment variable or log. The clients
# (as _sprobe, Navigator's broker uid) run in the background: a non-resident
# registration for example.com (nothing is stored on the key), then a
# sign-in with it, the signature checked against the registered public key.
# Each needs one touch. Results: /tmp/sprobe-real.log.
set -u
P=/usr/local/bin/sigillum-probe-test
[ "$(id -u)" = 0 ] || { echo "run as root in the VM" >&2; exit 1; }
[ -x "$P" ] || { echo "$P missing: run vm-sigillum-check.sh first" >&2; exit 1; }
[ -t 0 ] || { echo "needs a terminal (the PIN is typed here)" >&2; exit 1; }
id _sprobe >/dev/null 2>&1 || { echo "no _sprobe user: run vm-sigillum-check.sh first" >&2; exit 1; }
# One trusted UI at a time: the gate's scripted one goes.
pkill -f "sigillum-probe-test ui" 2>/dev/null; sleep 1
rm -f /tmp/sprobe-real.log /tmp/sprobe-real.cred
touch /tmp/sprobe-real.cred; chown _sprobe /tmp/sprobe-real.cred
(
  sleep 2   # the UI connects first
  echo "== register (touch 1)"
  if su -m _sprobe -c "timeout 150 $P register example.com /tmp/sprobe-real.cred"; then
    echo "register-rc 0"
    echo "== assert (touch 2)"
    su -m _sprobe -c "timeout 150 $P assert example.com /tmp/sprobe-real.cred"; echo "assert-rc $?"
  else
    echo "register-rc 1 (assert skipped)"
    pkill -f "sigillum-probe-test ui-tty"
  fi
) > /tmp/sprobe-real.log 2>&1 < /dev/null &
CL=$!
su -m _sigillum-ui -c "$P ui-tty 2"
wait $CL
echo "======== result"
cat /tmp/sprobe-real.log
if grep -q "^register: ok .*rp_hash_ok=true" /tmp/sprobe-real.log && grep -q "^assert: cred_match=true signature_ok=true" /tmp/sprobe-real.log; then
  echo "REAL KEY: register + assert PASSED"
else
  echo "REAL KEY: FAILED (sigillumd: tail /var/log/atrium/atrium-sigillumd.log; driver: tail /var/log/atrium/atrium-sigillum-fido-0.log)"
fi
