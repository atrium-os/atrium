#!/bin/sh
# Tessera stress-test orchestrator. Runs all stress scenarios in
# series, captures pass/fail per script, and emits a summary.
#
# Each individual script:
#   - Loads the kmod fresh.
#   - Mkfs's its own image.
#   - Runs its workload.
#   - Cleans up (umount + mdconfig -d).
#
# This wrapper:
#   - Builds + installs fsx if needed.
#   - Sets default tunables (smaller for CI; override via env).
#   - Captures each test's output to /tmp/stress.<name>.log.
#   - Reports total wallclock + which tests failed.
#
# Tunables:
#   STRESS_LEVEL = quick (default) | normal | thorough
#     quick:    ~5 min total (development sanity).
#     normal:   ~15 min (pre-merge gate).
#     thorough: ~60+ min (overnight soak).
set -eu

LEVEL=${STRESS_LEVEL:-quick}
case $LEVEL in
quick)
    export STRESS_FSX_OPS=2000
    export STRESS_OPS=100
    export STRESS_NPROC=4
    export STRESS_CRASH_CYCLES=20
    export STRESS_SOAK_CYCLES=30
    export STRESS_TIMEOUT=60
    ;;
normal)
    export STRESS_FSX_OPS=10000
    export STRESS_OPS=300
    export STRESS_NPROC=8
    export STRESS_CRASH_CYCLES=50
    export STRESS_SOAK_CYCLES=100
    export STRESS_TIMEOUT=120
    ;;
thorough)
    export STRESS_FSX_OPS=100000
    export STRESS_OPS=1000
    export STRESS_NPROC=16
    export STRESS_CRASH_CYCLES=200
    export STRESS_SOAK_CYCLES=500
    export STRESS_TIMEOUT=300
    ;;
*)
    echo "STRESS_LEVEL=$LEVEL not recognized (use quick/normal/thorough)"
    exit 2
    ;;
esac

echo "================================================================"
echo " Tessera stress harness — level=$LEVEL"
echo "================================================================"

# Ensure fsx is built + installed.
if [ ! -x /usr/local/bin/fsx ]; then
    echo "--- building fsx ---"
    cd /usr/src/tools/regression/fsx
    bmake 2>/dev/null || make 2>/dev/null
    FSX_BIN=$(find /usr/obj -name 'fsx' -type f 2>/dev/null | head -1)
    [ -n "$FSX_BIN" ] || { echo "FAIL: fsx build failed"; exit 1; }
    cp $FSX_BIN /usr/local/bin/fsx
    cd -
fi

DIR=$(dirname "$0")
TESTS="
stress_fsx.sh
stress_concurrent.sh
stress_crash_torture.sh
stress_exhaustion.sh
stress_soak.sh
stress_pjdfstest.sh
"

PASS=0
FAIL=0
START=$(date +%s)
FAILED_TESTS=""

for t in $TESTS; do
    name=${t%.sh}
    name=${name#stress_}
    echo
    echo "================================================================"
    echo " stress_$name"
    echo "================================================================"
    LOG=/tmp/stress.$name.log
    if sh "$DIR/$t" > $LOG 2>&1; then
        echo "  PASS — log $LOG ($(wc -l < $LOG | awk '{print $1}') lines)"
        PASS=$((PASS + 1))
    else
        rc=$?
        echo "  FAIL rc=$rc — log $LOG"
        tail -10 $LOG | sed 's/^/    /'
        FAIL=$((FAIL + 1))
        FAILED_TESTS="$FAILED_TESTS $name"
    fi
done

END=$(date +%s)
echo
echo "================================================================"
echo " Summary"
echo "================================================================"
echo "  level:     $LEVEL"
echo "  duration:  $((END - START)) s"
echo "  pass:      $PASS"
echo "  fail:      $FAIL"
if [ -n "$FAILED_TESTS" ]; then
    echo "  failed:   $FAILED_TESTS"
    exit 1
fi
echo "  ALL GREEN"
