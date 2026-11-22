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
  pin-no-send-document) job=17; title="PIN without Send-Document"; prefix=pin-no-send-document ;;
  pin-no-job-password) job=18; title="PIN without job-password support"; prefix=pin-no-job-password ;;
  pin-no-create-job) job=19; title="PIN without Create-Job"; prefix=pin-no-create-job ;;
  pin-fresh) job=20; title="Fresh PIN after replay refusal"; prefix=pin-fresh ;;
  pin-create-replay) job=$5; title="PIN response $6 without-id none"; prefix="pin-replay-$6" ;;
  pin-replay) job=$5; title="PIN response ignored with-id $6"; prefix="pin-replay-$6" ;;
  pin-send-failure) job=$5; title="PIN upload failure $6"; prefix="pin-send-$6" ;;
  pin-recovery) job=$5; title="PIN recovery after $6"; prefix="pin-recovery-$6" ;;
  pin-rejection) job=$5; title="PIN response $6 $7 $8"; prefix="pin-$6-$7-$8" ;;
  *) echo "Unknown print fixture: $kind" >&2; exit 2 ;;
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
    case "$kind" in
      pin-uuid) job=16; prefix=pin-replay ;;
      pin-replay|pin-create-replay) ;;
      pin-send-failure) prefix=pin-replay-send-$6 ;;
      *) exit 2 ;;
    esac
    # Replay the retained handle, then leave no handle file for the next job.
    exec 3<pin-handle
    rm -f pin-handle
    "$P/r7-pin-fixture" submit "$P/print.sock" "$P/identity-proof.pdf" "$title" <&3
    exec 3<&-
    snapshot=$prefix-jobs.log
    expected="^ +$job +printer +1 +Failed +0/2 +$title +Arca did not supply a usable PIN for this job *$"
    ;;
  missing-capability)
    case "$kind" in
      pin-no-send-document|pin-no-job-password|pin-no-create-job) ;;
      *) exit 2 ;;
    esac
    # Keep the root-only handle for the positive control after capability restoration.
    "$P/r7-pin-fixture" submit "$P/print.sock" "$P/identity-proof.pdf" "$title" <pin-handle
    snapshot=$prefix-jobs.log
    expected="^ +$job +printer +1 +Failed +0/2 +$title +this printer cannot accept a PIN before the document is sent *$"
    ;;
  fresh|retain)
    if [ "$phase" = retain ]; then
      case "$kind" in
        pin-rejection|pin-send-failure) ;;
        *) exit 2 ;;
      esac
    fi
    case "$kind" in
      pin-fresh|pin-recovery) expected="^ +$job +printer +1 +Done +2/2 +$title *$" ;;
      pin-send-failure)
        case "$6" in
          ipp-refused) reason="the printer refused [(]0x040b[)]:.*" ;;
          ipp-busy) reason="the printer refused [(]0x0507[)]:.*" ;;
          http-refused) reason="the printer answered HTTP 403" ;;
          dropped|truncated) reason=".+" ;;
          invalid-ipp) reason="the printer's answer is not valid IPP: .+" ;;
          *) exit 2 ;;
        esac
        expected="^ +$job +printer +1 +Failed +0/2 +$title +$reason *$"
        ;;
      pin-rejection)
        case "$6" in
          success) reason="the printer gave no job id" ;;
          create-http-refused) reason="the printer answered HTTP 403" ;;
          create-invalid-ipp) reason="the printer's answer is not valid IPP: .+" ;;
          create-dropped|create-truncated) reason=".+" ;;
          *) reason="the printer did not accept the PIN job" ;;
        esac
        expected="^ +$job +printer +1 +Failed +0/2 +$title +$reason *$"
        ;;
      *) exit 2 ;;
    esac
    exec 3<pin-handle
    # Keep the exact root-only handle until the cancellation failure is replayed.
    if [ "$phase" = fresh ]; then rm -f pin-handle; fi
    "$P/r7-pin-fixture" submit "$P/print.sock" "$P/identity-proof.pdf" "$title" <&3
    exec 3<&-
    snapshot=$prefix-jobs.log
    ;;
  *) echo "Expected hold, retry, release, replay, missing-capability, fresh, or retain" >&2; exit 2 ;;
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
