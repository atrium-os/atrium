#!/bin/sh
# pjdfstest — POSIX file-system test suite. Written by Pawel Jakub
# Dawidek, lives at /usr/tests/sys/pjdfstest. Each test is a small shell
# script that drives the pjdfstest binary through a deterministic
# sequence of syscalls and emits TAP-style "ok N" / "not ok N" output.
#
# Tessera deliberately doesn't support certain syscalls per spec §8 —
# mkfifo, mknod, advisory locks, chflags. Subtests that exercise those
# return EOPNOTSUPP and trigger downstream cascades; treat them as
# expected and don't count them against tessera. The remaining failures
# are real signal.
#
# Usage:
#   stress_pjdfstest.sh                 # all categories
#   stress_pjdfstest.sh chown           # one category
#   stress_pjdfstest.sh chmod chown     # multiple categories
#   STRESS_PJDFSTEST_TIMEOUT=30 stress_pjdfstest.sh   # bump per-test cap
#
# Outputs a top-N table of scripts ranked by not-ok count. Full TAP
# output is in /tmp/pjd.log (one section per script, prefixed with
# "==> category/NN").
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
PJD=/usr/tests/sys/pjdfstest/pjdfstest
PJD_DIR=/usr/tests/sys/pjdfstest/tests

[ -x "$PJD" ] || { echo "FAIL: pjdfstest missing at $PJD"; exit 1; }
[ -d "$PJD_DIR" ] || { echo "FAIL: $PJD_DIR missing"; exit 1; }

# Category list. If args supplied, restrict; otherwise run them all.
ALL_CATS="chmod chown link mkdir mkfifo open rename rmdir symlink truncate unlink"
if [ $# -gt 0 ]; then
    CATEGORIES="$*"
else
    CATEGORIES="$ALL_CATS"
fi

# Per-test timeout. Tests like truncate/12 ran for 19+ minutes pre-fix
# (1 PB truncate); even with the EFBIG cap, defensive timeouts let a
# new infinite-work test fail loud rather than wedge the suite.
TIMEOUT=${STRESS_PJDFSTEST_TIMEOUT:-15}

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 256 --seed-file h --seed-content x \
    /tmp/pjd.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/pjd.img)
mount -t tessera /dev/$MD /mnt/tessera
cd /mnt/tessera

LOG=/tmp/pjd.log
SUMMARY=/tmp/pjd-summary.log
: > $LOG
: > $SUMMARY

# Counters.
SCRIPTS_PASS=0     # scripts where every subtest was "ok"
SCRIPTS_FAIL=0     # scripts with at least one "not ok"
SCRIPTS_RUN=0
OK_TOTAL=0         # individual "ok" subtests
NOK_TOTAL=0        # individual "not ok" subtests
EOPNOTSUPP_TOTAL=0 # "got EOPNOTSUPP" — spec-correct, not a tessera bug

for cat in $CATEGORIES; do
    [ -d $PJD_DIR/$cat ] || { echo "  skip: $cat (no such dir)"; continue; }
    for t in $PJD_DIR/$cat/[0-9]*; do
        [ -f "$t" ] || continue
        SCRIPTS_RUN=$((SCRIPTS_RUN + 1))
        name="$cat/$(basename $t)"
        cd /mnt/tessera
        out=$(env PJDFSTEST=$PJD timeout $TIMEOUT sh "$t" 2>&1)
        ok=$(echo "$out" | grep -c "^ok " || true)
        nok=$(echo "$out" | grep -c "^not ok " || true)
        eopn=$(echo "$out" | grep -c "got EOPNOTSUPP" || true)
        OK_TOTAL=$((OK_TOTAL + ok))
        NOK_TOTAL=$((NOK_TOTAL + nok))
        EOPNOTSUPP_TOTAL=$((EOPNOTSUPP_TOTAL + eopn))
        echo "==> $name (ok=$ok nok=$nok eopn=$eopn)" >> $LOG
        echo "$out" >> $LOG
        if [ $nok -eq 0 ] && [ $ok -gt 0 ]; then
            SCRIPTS_PASS=$((SCRIPTS_PASS + 1))
        else
            SCRIPTS_FAIL=$((SCRIPTS_FAIL + 1))
            # Top-N summary line: "category/NN  not_ok=N  ok=N"
            printf "%-24s nok=%-4d ok=%-4d eopn=%d\n" \
                "$name" "$nok" "$ok" "$eopn" >> $SUMMARY
        fi
    done
done

echo
echo "=== summary (categories: $CATEGORIES) ==="
echo "  scripts:    pass=$SCRIPTS_PASS fail=$SCRIPTS_FAIL run=$SCRIPTS_RUN"
echo "  subtests:   ok=$OK_TOTAL not_ok=$NOK_TOTAL"
echo "  EOPNOTSUPP: $EOPNOTSUPP_TOTAL  (mkfifo/mknod cascades — spec-correct)"
GENUINE_NOK=$((NOK_TOTAL - EOPNOTSUPP_TOTAL))
echo "  genuine nok (excl. EOPNOTSUPP cascades): $GENUINE_NOK"
echo "  log: $LOG"
echo
echo "=== top 15 failing scripts (by not_ok count) ==="
sort -k2 -t= -nr -k4 $SUMMARY 2>/dev/null | head -15 | sed 's/^/  /'

cd /
umount /mnt/tessera
mdconfig -d -u 0
echo DONE
