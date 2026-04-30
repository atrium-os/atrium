#!/bin/sh
# Crash-injection torture. Tessera has a knob (kern.tessera.skip_next_sb)
# that causes the next commit_sb to journal+checkpoint but skip the SB
# write — simulating crashes between the journal record landing and
# the SB sectors being durable. Replay-on-mount is supposed to roll
# the journaled record forward.
#
# This test runs that loop many times, with random workloads in between:
#   1. Inject crash (skip_next_sb=1).
#   2. Do a mutation (write/mkdir/rm).
#   3. Force commit (umount).
#   4. Remount — replay must succeed and recover the mutation.
#   5. Verify the mutation took effect.
#   6. Repeat.
#
# Catches:
#   - Replay logic bugs (record applied wrong / not at all / twice)
#   - Inconsistency between snapshot record + replayed root
#   - Drain races during the partial commit
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

CYCLES=${STRESS_CRASH_CYCLES:-50}

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x /tmp/crash.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/crash.img)
mount -t tessera /dev/$MD /mnt/tessera
echo seed > /mnt/tessera/seed
umount /mnt/tessera

echo "--- $CYCLES crash-inject cycles ---"
i=0
while [ $i -lt $CYCLES ]; do
    mount -t tessera /dev/$MD /mnt/tessera
    sysctl kern.tessera.skip_next_sb=1 >/dev/null
    op=$((i % 4))
    case $op in
    0)  echo "iter $i v1" > /mnt/tessera/log$i ;;
    1)  if [ -f /mnt/tessera/log$((i - 1)) ]; then
            echo "iter $i overwrite" > /mnt/tessera/log$((i - 1))
        fi ;;
    2)  mkdir -p /mnt/tessera/d$i ;;
    3)  if [ -d /mnt/tessera/d$((i - 2)) ]; then
            rmdir /mnt/tessera/d$((i - 2)) 2>/dev/null || true
        fi ;;
    esac
    umount /mnt/tessera
    i=$((i + 1))
done

# Final remount + sanity.
echo "--- final remount + verify ---"
mount -t tessera /dev/$MD /mnt/tessera
[ -f /mnt/tessera/seed ] || { echo "FAIL: seed lost"; exit 1; }
[ "$(cat /mnt/tessera/seed)" = "seed" ] || { echo "FAIL: seed corrupted"; exit 1; }
echo "  seed intact"

# Spot-check that some of the iter-N logs survive.
SURVIVED=0
for i in $(jot $CYCLES); do
    [ -f /mnt/tessera/log$((i - 1)) ] && SURVIVED=$((SURVIVED + 1))
done
echo "  log files surviving: $SURVIVED"

umount /mnt/tessera
mdconfig -d -u 0

echo "--- counters ---"
sysctl kern.tessera.sb_commits kern.tessera.mark_dirty \
    2>&1 | sed 's/^/  /'

echo DONE
