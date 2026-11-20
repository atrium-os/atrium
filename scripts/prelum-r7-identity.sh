#!/bin/sh
# Run after the desktop jobs in the same disposable guest.
set -eu
P=$1
marker=$2
cd "$P"
test "$(cat .isolated-gate)" = "$marker"
test "$(uname -s)" = FreeBSD

"$P/prelumctl" --socket "$P/print.sock" print identity-proof.pdf \
    --printer 1 --title "Changed printer UUID"
for attempt in $(seq 35); do
  "$P/prelumctl" --socket "$P/print.sock" jobs >identity-jobs.log
  if grep -Eq "^ +12 +printer +1 +Held +0/0 +Changed printer UUID +.*alert IdentityChanged" identity-jobs.log; then
    break
  fi
  sleep 1
done
grep -E "^ +12 +printer +1 +Held +0/0 +Changed printer UUID +.*alert IdentityChanged" identity-jobs.log

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
