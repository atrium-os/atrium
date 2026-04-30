#!/bin/sh
# pjdfstest — POSIX file-system test suite. Written by Pawel Jakub
# Dawidek, lives at /usr/tests/sys/pjdfstest. ~8000 individual tests
# spanning chmod, chown, link, symlink, mkdir, rmdir, rename, open,
# truncate, etc. Each test is a deterministic sequence of syscalls
# with expected error codes / fs state.
#
# Tessera supports a subset (no chflags, no advisory locks yet, etc.)
# so SOME tests are expected to fail. This script runs the suite,
# captures fail counts, and lets the operator inspect /tmp/pjd.log.
#
# Usage:
#   stress_pjdfstest.sh [TESTS_GLOB]
# Default: run everything. Pass e.g. "chmod" to only run chmod tests.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
PJD=/usr/tests/sys/pjdfstest/pjdfstest
PJD_DIR=/usr/tests/sys/pjdfstest/tests

[ -x "$PJD" ] || { echo "FAIL: pjdfstest missing"; exit 1; }
[ -d "$PJD_DIR" ] || { echo "FAIL: $PJD_DIR missing"; exit 1; }

GLOB=${1:-}

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 256 --seed-file h --seed-content x /tmp/pjd.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/pjd.img)
mount -t tessera /dev/$MD /mnt/tessera

cd /mnt/tessera

# pjdfstest's tests/ dir contains shell scripts that call the
# binary with specific syscall sequences. Each test exits 0 on pass
# and emits "ok N" / "not ok N" TAP-style output.
PASS=0
FAIL=0
TOTAL=0
LOGFILE=/tmp/pjd.log
: > $LOGFILE

if [ -n "$GLOB" ]; then
    TESTS=$(find $PJD_DIR/$GLOB -name '*.t' 2>/dev/null)
else
    TESTS=$(find $PJD_DIR -name '*.t' 2>/dev/null)
fi
if [ -z "$TESTS" ]; then
    echo "FAIL: no tests found"
    cd /
    umount /mnt/tessera
    exit 1
fi

echo "--- running $(echo "$TESTS" | wc -l | awk '{print $1}') pjdfstest scripts ---"
for t in $TESTS; do
    TOTAL=$((TOTAL + 1))
    name=$(basename $(dirname $t))/$(basename $t)
    if env PJDFSTEST=$PJD sh "$t" >> $LOGFILE 2>&1; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
        echo "  FAIL: $name" >> $LOGFILE
    fi
done

echo "  total=$TOTAL pass=$PASS fail=$FAIL"
echo "  log: $LOGFILE"
echo "  failed-test summary:"
grep "FAIL:" $LOGFILE | head -20 | sed 's/^/    /'

cd /
umount /mnt/tessera
mdconfig -d -u 0

# Exit 0 even on failures — pjdfstest is informational; some failures
# are expected (chflags etc.). Operator inspects log to triage.
echo DONE
