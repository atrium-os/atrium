#!/bin/sh
# Runs INSIDE the Solum P0 host as root. Streams every vmm.ko function entry
# and return to stdout (run it over ssh so the lines leave the guest before a
# hang), then creates a throwaway VM and touches vCPU 0, which hangs this
# host under Apple's nested EL2 (2026-11-27). The last lines received on the
# Mac name the function that never returned.
#
#   ssh ... 'sh vcpu-trace.sh' | tee trace.log
set -u
kldstat -q -m vmm || kldload vmm || { echo "FAILED: kldload vmm"; exit 1; }
kldstat -q -m dtraceall || kldload dtraceall || { echo "FAILED: kldload dtraceall"; exit 1; }
bhyvectl --vm=t --destroy 2>/dev/null
bhyvectl --vm=t --create || { echo "FAILED: create"; exit 1; }
# Fast switch rate so little is buffered when the host stops.
dtrace -q -x switchrate=1000hz -x bufsize=4m -n '
    fbt:vmm::entry  { printf("%d > %s\n", cpu, probefunc); }
    fbt:vmm::return { printf("%d < %s\n", cpu, probefunc); }' &
dpid=$!
sleep 5
echo "=== touching vCPU 0"
bhyvectl --vm=t --get-stats
echo "=== survived; rc $?"
sleep 2
kill "$dpid"
bhyvectl --vm=t --destroy
