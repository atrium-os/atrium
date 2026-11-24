#!/bin/sh
# Host self-test of scripts/lib/gate.sh: each way a gate can go wrong must
# end non-zero, and a clean run zero. Ends "gate-selftest: N failed".
set -u
LIB=$(cd "$(dirname "$0")" && pwd)/gate.sh
fails=0
run() { # name, expected status, script body
    sh -c ". '$LIB'; $3" >/dev/null 2>&1
    got=$?
    if [ "$got" = "$2" ]; then echo "  ok $1"; else echo "  FAIL $1: status $got, want $2"; fails=$((fails + 1)); fi
}
run "all checks pass"            0 'gate_begin t 2; check a 1 1; ok b; gate_end'
run "one check fails"            1 'gate_begin t 2; check a 1 2; ok b; gate_end'
run "fewer checks than declared" 1 'gate_begin t 3; ok a; ok b; gate_end'
run "no checks at all"           1 'gate_begin t; gate_end'
run "abort stops at once"        1 'gate_begin t; ok a; gate_abort "no tool"; ok never'
run "abort skips the rest"       1 'gate_begin t; gate_abort x; exit 0'
echo "gate-selftest: $fails failed"
[ "$fails" -eq 0 ]
