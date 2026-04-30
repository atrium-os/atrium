#!/bin/sh
# Concurrent mutation stress. Spawn N parallel shell processes each
# doing random ops (create/write/append/rename/remove/mkdir/rmdir)
# in their own subdirectory. Goals:
#
#   - Detect lock contention / deadlocks (a stuck child shows up as
#     a process not exiting after the deadline).
#   - Detect race conditions in commit_sb's flush serialization.
#   - Detect cross-directory rename hazards (currently EOPNOTSUPP for
#     cross-dir; in-dir should be safe).
#   - Probe the dirty-inode + pending-manifest cache under load.
#
# Each child does up to STRESS_OPS ops (default 200). Total wallclock
# is capped via a TIMEOUT.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

NPROC=${STRESS_NPROC:-8}
STRESS_OPS=${STRESS_OPS:-200}
TIMEOUT=${STRESS_TIMEOUT:-90}

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 256 --seed-file h --seed-content x /tmp/conc.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/conc.img)
mount -t tessera /dev/$MD /mnt/tessera

# Each worker mutates inside its own /mnt/tessera/wN directory.
# Random ops use $RANDOM (seeded per child via $$); operations:
#   c) create file with random small content
#   w) overwrite an existing file with new content
#   a) append to an existing file
#   r) remove an existing file
#   m) mkdir
#   d) rmdir (empty)
#   n) rename in-dir
#   s) stat + cat (read-only)
worker() {
    n=$1
    base=/mnt/tessera/w$n
    mkdir -p $base
    cd $base
    i=0
    while [ $i -lt $STRESS_OPS ]; do
        op=$(( ($$ + i) % 8 ))
        case $op in
        0)  echo "data-$n-$i" > f$i ;;
        1)  if [ -f f$((i - 1)) ]; then
                echo "v2-$i" > f$((i - 1))
            else
                echo "init-$n-$i" > f$i
            fi ;;
        2)  if [ -f f$((i - 1)) ]; then
                echo "appended-$i" >> f$((i - 1))
            fi ;;
        3)  if [ -f f$((i - 2)) ]; then
                rm -f f$((i - 2))
            fi ;;
        4)  mkdir -p d$i 2>/dev/null || true ;;
        5)  rmdir d$((i - 3)) 2>/dev/null || true ;;
        6)  if [ -f f$((i - 1)) ]; then
                mv f$((i - 1)) g$((i - 1)) 2>/dev/null || true
            fi ;;
        7)  if [ -f f$((i - 1)) ]; then
                cat f$((i - 1)) >/dev/null 2>&1 || true
                stat f$((i - 1)) >/dev/null 2>&1 || true
            fi ;;
        esac
        i=$((i + 1))
    done
    echo "  worker $n done ($STRESS_OPS ops)"
}

echo "--- spawning $NPROC workers, $STRESS_OPS ops each ---"
START=$(date +%s)
for n in $(jot $NPROC); do
    worker $n &
done

# Wait with a deadline.
DEADLINE=$((START + TIMEOUT))
while :; do
    NOW=$(date +%s)
    if [ $NOW -ge $DEADLINE ]; then
        echo "  TIMEOUT after $TIMEOUT s — killing stragglers"
        jobs -p | xargs kill -9 2>/dev/null || true
        wait
        echo "FAIL: workers did not finish in time (likely deadlock)"
        cd /
        umount /mnt/tessera
        exit 1
    fi
    if ! jobs -p | grep -q .; then
        break
    fi
    sleep 1
done
wait
END=$(date +%s)
echo "  all workers done in $((END - START)) s"

# Sanity: remount + readdir each worker's dir.
echo "--- remount + verify directories accessible ---"
cd /
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
for n in $(jot $NPROC); do
    [ -d /mnt/tessera/w$n ] || { echo "FAIL: w$n missing"; exit 1; }
    cnt=$(ls /mnt/tessera/w$n | wc -l | awk '{print $1}')
    echo "  w$n: $cnt entries"
done

umount /mnt/tessera
mdconfig -d -u 0

echo "--- counter snapshot ---"
sysctl kern.tessera.sb_commits kern.tessera.mark_dirty \
    kern.tessera.fsync_group_wait kern.tessera.dirty_drained \
    kern.tessera.pending_drained 2>&1 | sed 's/^/  /'

echo DONE
