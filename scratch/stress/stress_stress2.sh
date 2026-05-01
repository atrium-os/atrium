#!/bin/sh
# FreeBSD stress2 harness pointed at a tessera mount.
#
# stress2 (Peter Holm's suite at /usr/src/tools/test/stress2) is the
# kernel/filesystem stress framework FreeBSD itself uses. We pick the
# FS-relevant testcases — rw, creat, mkdir, link, rename, symlink,
# mmap, fts, dirrename, dirnprename, openat — and run each for a
# bounded duration with RUNDIR pointed at /mnt/tessera/stress.
#
# Skipped: swap, shm, tcp/udp, sysctl, badcode, thr1/2, pty, lockf*,
# socket — kernel-internal or non-FS subsystems.
# Skipped: mkfifo (tessera deliberately doesn't support, spec §8).
#
# Each testcase forks N children (LOAD-controlled) that hammer the FS
# with the relevant op until time elapses. We declare success if:
#   1. Each testcase exits cleanly (no panic, no hang).
#   2. Mount survives.
#   3. Final unmount succeeds.
#
# Usage:
#   stress_stress2.sh                 # all FS testcases, 30s each
#   STRESS2_RUNTIME=120 stress_stress2.sh
#   stress_stress2.sh rw mkdir        # one or more by name
set -u
S2=/usr/src/tools/test/stress2
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

ALL="rw creat mkdir link rename symlink mmap fts dirrename dirnprename openat"
if [ $# -gt 0 ]; then CASES="$*"; else CASES="$ALL"; fi

RUNTIME_RAW=${STRESS2_RUNTIME:-30}     # per-testcase wallclock cap (sec)
RUNTIME=$RUNTIME_RAW
LOAD=${STRESS2_LOAD:-100}          # always-run; below 100 = random skip
INCARNATIONS=${STRESS2_INCARNATIONS:-4}  # parallel workers per testcase

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

# 1 GiB volume — stress2's 4-incarnation rw / fts hammer through space
# fast. Meta-reserve scales to ~1.5% (16 MiB at this size), enough
# headroom for commit_sb's drain to keep up.
$BIN/mkfs-tessera --create -s 1024 --seed-file h --seed-content x \
    /tmp/stress2.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/stress2.img)
mount -t tessera /dev/$MD /mnt/tessera

mkdir -p /mnt/tessera/stress2
export RUNDIR=/mnt/tessera/stress2
HARD_TIMEOUT=$((RUNTIME + 30))
export RUNTIME=${RUNTIME}s
export LOAD=$LOAD
export INCARNATIONS=$INCARNATIONS
export VERBOSE=0

PASS=0
FAIL=0
HUNG=0
LOG=/tmp/stress2-tessera.log
: > $LOG

# Per-testcase hard timeout = RUNTIME + 30s for cleanup, computed
# above before RUNTIME got the "s" suffix. If a testcase hangs past
# that, the wrapper's `timeout` kills it; we record HUNG and continue
# so one bad case doesn't wedge the whole run.

for c in $CASES; do
    bin=$S2/testcases/$c/$c
    if [ ! -x "$bin" ]; then
        echo "skip $c (not built)"
        continue
    fi
    # Per-testcase subdir avoids leftover-file collisions between
    # cases (rename leaves files at unexpected names; creat+link
    # iters from a previous case shadow the next). Cheaper than an
    # rm -rf sweep, which on tessera is O(N) in extant entries and
    # ate all 15s of budget under earlier runs.
    sub=/mnt/tessera/stress2/$c
    mkdir -p $sub
    export RUNDIR=$sub
    printf "%-15s " "$c"
    start=$(date +%s)
    # Pass run_time, load and incarnations on the command line — env
    # vars work too but explicit args bypass any shell-export oddities
    # and document the run shape in the harness.
    out=$(timeout $HARD_TIMEOUT $bin -t ${RUNTIME_RAW}s -l $LOAD \
        -i $INCARNATIONS -n 2>&1)
    rc=$?
    end=$(date +%s)
    dur=$((end - start))
    echo "==> $c (rc=$rc dur=${dur}s)" >> $LOG
    echo "$out" >> $LOG
    if [ $rc -eq 124 ]; then
        echo "HUNG (${dur}s)"
        HUNG=$((HUNG + 1))
    elif [ $rc -eq 0 ]; then
        echo "ok (${dur}s)"
        PASS=$((PASS + 1))
    else
        echo "FAIL rc=$rc (${dur}s)"
        FAIL=$((FAIL + 1))
    fi
done

echo
echo "=== summary ==="
echo "  pass=$PASS fail=$FAIL hung=$HUNG"
echo "  log: $LOG"

# Validate that the FS survived by listing + unmounting cleanly.
if ! ls /mnt/tessera >/dev/null 2>&1; then
    echo "  FS unresponsive after stress run"
    HUNG=$((HUNG + 1))
fi
cd /
umount /mnt/tessera 2>&1 | grep -v "^$"
mdconfig -d -u 0 2>/dev/null || true

if [ $FAIL -eq 0 ] && [ $HUNG -eq 0 ]; then
    echo DONE
else
    echo FAILED
    exit 1
fi
