#!/bin/sh
# Run after the desktop jobs in the same disposable guest.
set -eu
P=$1
marker=$2
cd "$P"
test "$(cat .isolated-gate)" = "$marker"
test "$(uname -s)" = FreeBSD

held="^ +12 +printer +1 +Held +0/0 +Changed printer UUID +.*alert IdentityChanged since "
previous_hold=

case "$3" in
  hold)
    "$P/prelumctl" --socket "$P/print.sock" print identity-proof.pdf \
        --printer 1 --title "Changed printer UUID"
    snapshot=identity-jobs.log
    expected="${held}[0-9]+$"
    ;;
  retry)
    previous_hold=$(sed -nE "s|${held}([0-9]+)$|\1|p" identity-jobs.log)
    test -n "$previous_hold"
    # Alert timestamps have one-second resolution.
    sleep 1
    "$P/prelumctl" --socket "$P/print.sock" release 12
    snapshot=identity-reheld-jobs.log
    expected="${held}[0-9]+$"
    ;;
  release)
    "$P/prelumctl" --socket "$P/print.sock" release 12
    snapshot=identity-released-jobs.log
    expected="^ +12 +printer +1 +Done +2/2 +Changed printer UUID *$"
    ;;
  *) echo "Expected hold, retry, or release" >&2; exit 2 ;;
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
