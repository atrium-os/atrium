#!/bin/sh
# Run after the desktop jobs in the same disposable guest.
set -eu
P=$1
marker=$2
phase=$3
kind=$4
cd "$P"
test "$(cat .isolated-gate)" = "$marker"
test "$(uname -s)" = FreeBSD

case "$kind" in
  uuid) job=12; title="Changed printer UUID"; prefix=identity ;;
  tls) job=13; title="Changed printer TLS key"; prefix=tls-identity ;;
  pin) job=14; title="PIN after TLS key restoration"; prefix=pin-identity ;;
  pin-uuid) job=15; title="PIN after UUID restoration"; prefix=pin-uuid-identity ;;
  *) echo "Expected uuid, tls, pin, or pin-uuid" >&2; exit 2 ;;
esac
held="^ +$job +printer +1 +Held +0/0 +$title +.*alert IdentityChanged since "
previous_hold=

case "$phase" in
  hold)
    if [ "$kind" = pin ] || [ "$kind" = pin-uuid ]; then
      "$P/r7-pin-fixture" submit "$P/print.sock" "$P/identity-proof.pdf" "$title" <pin-handle
      # Keep the UUID fixture's root-only handle for the final replay check.
      if [ "$kind" = pin ]; then rm -f pin-handle; fi
    else
      "$P/prelumctl" --socket "$P/print.sock" print identity-proof.pdf \
          --printer 1 --title "$title"
    fi
    snapshot=$prefix-jobs.log
    expected="${held}[0-9]+$"
    ;;
  retry)
    previous_hold=$(sed -nE "s|${held}([0-9]+)$|\1|p" "$prefix-jobs.log")
    test -n "$previous_hold"
    # Alert timestamps have one-second resolution.
    sleep 1
    "$P/prelumctl" --socket "$P/print.sock" release "$job"
    snapshot=$prefix-reheld-jobs.log
    expected="${held}[0-9]+$"
    ;;
  release)
    "$P/prelumctl" --socket "$P/print.sock" release "$job"
    snapshot=$prefix-released-jobs.log
    expected="^ +$job +printer +1 +Done +2/2 +$title *$"
    ;;
  replay)
    test "$kind" = pin-uuid
    # Open the exact handle staged for job 15, then unlink it before submitting.
    exec 3<pin-handle
    rm -f pin-handle
    "$P/r7-pin-fixture" submit "$P/print.sock" "$P/identity-proof.pdf" "$title" <&3
    exec 3<&-
    snapshot=pin-replay-jobs.log
    expected="^ +16 +printer +1 +Failed +[0-9]+/[0-9]+ +$title +Arca did not supply a usable PIN for this job *$"
    ;;
  *) echo "Expected hold, retry, release, or replay" >&2; exit 2 ;;
esac
matches_snapshot() {
  if [ -n "$previous_hold" ]; then
    current_hold=$(sed -nE "s|${held}([0-9]+)$|\1|p" "$snapshot")
    [ -n "$current_hold" ] && [ "$current_hold" -gt "$previous_hold" ]
  else
    grep -Eq "$expected" "$snapshot"
  fi
}
for attempt in $(seq 35); do
  "$P/prelumctl" --socket "$P/print.sock" jobs >"$snapshot"
  if matches_snapshot; then break; fi
  sleep 1
done
matches_snapshot || { cat "$snapshot" >&2; exit 1; }
grep -E "$expected" "$snapshot"

# Include dying VNET jails. The daemon janitor also removes the worker roots
# after outstanding sockets release their references.
for attempt in $(seq 90); do
  jls -d -n name >jails-after-print.log
  remaining=$(find /var/lib/atrium/jails -mindepth 1 -maxdepth 1 -name 'system-prelum-*' -print)
  printf '%s\n' "$remaining" >worker-roots-after-print.log
  if ! grep -q '^name=system-prelum-' jails-after-print.log && [ -z "$remaining" ]; then
    echo "Prelum: all worker jails and roots removed after printing"
    exit 0
  fi
  sleep 1
done
echo "Prelum worker jails or roots remain after the teardown deadline" >&2
cat jails-after-print.log worker-roots-after-print.log >&2
exit 1
