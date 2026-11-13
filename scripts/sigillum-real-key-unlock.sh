#!/bin/sh
# Enrol a REAL FIDO key as a login key, then unlock with it (sigillum.md
# §16.6): run it as root IN THE GATE VM, at a terminal, after
# `ARCH=aarch64 SIGILLUM_REAL_KEY=1 sh scripts/vm-sigillum-check.sh` has
# commissioned sigillumd, ostiarius (with PAM) and the key's driver jail.
#
#   sh /root/s/sigillum-real-key-unlock.sh
#
# The login user is the gate's test account, veutest; its passcode is the
# gate's test password, typed by the UI from SPROBE_PASSCODES (it is no
# secret). The key's PIN is typed by a person at this terminal, echo off,
# once per ceremony; the UI stops at the first wrong PIN. Two ceremonies,
# each one PIN and one touch, asked as ostiarius (_ostiarius):
#   1. EnrolKey veutest: the passcode (PAM), then a UV registration for the
#      local RP ID atrium:sigillum/verify. Nothing resident on the key.
#   2. VERIFY_USER veutest (unlock, key-uv): an assertion over a fresh
#      challenge, checked by sigillumd against the enrolled public key.
# Results: /tmp/sprobe-unlock.log.
set -u
P=/usr/local/bin/sigillum-probe-test
KD=/var/db/sigillum/keys
PW=veu-Correct-2468
[ "$(id -u)" = 0 ] || { echo "run as root in the VM" >&2; exit 1; }
[ -x "$P" ] || { echo "$P missing: run vm-sigillum-check.sh first" >&2; exit 1; }
[ -t 0 ] || { echo "needs a terminal (the PIN is typed here)" >&2; exit 1; }
id veutest >/dev/null 2>&1 && id _ostiarius >/dev/null 2>&1 || { echo "no veutest/_ostiarius: run vm-sigillum-check.sh first" >&2; exit 1; }
[ -d $KD ] || { echo "$KD missing: sigillum-service.sh predates key unlock; rerun the gate" >&2; exit 1; }
pkill -f "sigillum-probe-test ui" 2>/dev/null; sleep 1
rm -f /tmp/sprobe-unlock.log $KD/veutest.json
(
  sleep 2   # the UI connects first
  echo "== enrol (passcode, PIN, touch 1)"
  if su -m _ostiarius -c "timeout 150 $P enrol veutest YubiKey"; then
    echo "enrol-rc 0"
    echo "stored $(stat -f '%Sp %Su' $KD/veutest.json) $(grep -o '"sign_count":[0-9]*' $KD/veutest.json)"
    echo "== unlock (PIN, touch 2)"
    su -m _ostiarius -c "timeout 150 $P verify veutest"; echo "verify-rc $?"
    echo "after $(grep -o '"sign_count":[0-9]*' $KD/veutest.json)"
  else
    echo "enrol-rc 1 (unlock skipped)"
    pkill -f "sigillum-probe-test ui-tty"
  fi
) > /tmp/sprobe-unlock.log 2>&1 < /dev/null &
CL=$!
su -m _sigillum-ui -c "env SPROBE_PASSCODES=$PW $P ui-tty 2"
wait $CL
echo "======== result"
cat /tmp/sprobe-unlock.log
if grep -q "^enrol: KeyEnrolled" /tmp/sprobe-unlock.log && grep -q "^verify: Verified .*SecurityKey" /tmp/sprobe-unlock.log && grep -q "^stored -rw------- _sigillum" /tmp/sprobe-unlock.log; then
  echo "REAL KEY: enrol + unlock PASSED"
else
  echo "REAL KEY: FAILED (sigillumd: tail /var/log/atrium/atrium-sigillumd.log; driver: tail /var/log/atrium/atrium-sigillum-fido-0.log)"
fi
