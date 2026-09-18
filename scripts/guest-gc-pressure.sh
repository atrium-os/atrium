#!/bin/sh
# Does the background data-zone GC reclaim dead packs on its own once the zone
# is under pressure — or does space only come back via a manual gc_now?
#
# GC is gated on free space (kern.tessera.gc_pressure_pct, 12%), so dead packs
# accumulate by design until the zone is nearly full; an unproductive pass then
# backs off exponentially. This fills a volume, deletes half of it, watches
# whether space returns on its own, and finally forces a pass for comparison.
# It is also the regression test for b1c95154: GC frees a pack only when the
# registry no longer names it, and an over-broad guard there stopped reclaim
# dead on a healthy volume (67490 skipped frees, 0 reclaimed, root at 99%).
# Scratch disk only.
set -u
DISK=vtbd2; DEV=/dev/vtbd2; PART=/dev/vtbd2p1; M=/mnt/gcp
PART_MB=${PART_MB:-2048}; WATCH=${WATCH:-300}
S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
row() { printf '%-6s free=%-6s used%%=%-4s armed=%-5s prod=%-4s unprod=%-4s last_recl=%-7s backoff_ms=%-7s last_ms=%s\n' \
    "$1" "$(df -m $M | awk 'NR==2{print $4}')" "$(df -m $M | awk 'NR==2{print $5}')" \
    "$(S gc_armed)" "$(S gc_productive_passes)" "$(S gc_unproductive_passes)" \
    "$(S gc_last_reclaimed)" "$(S gc_backoff_ms)" "$(S gc_last_ms)"; }

diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
mount | grep -q " $M " && umount -f $M
gpart destroy -F $DISK >/dev/null 2>&1
gpart create -s gpt $DISK >/dev/null && gpart add -t freebsd-ufs -s ${PART_MB}M -i 1 $DISK >/dev/null || { echo PART_FAIL; exit 3; }
mkdir -p $M; mkfs-tessera $PART >/dev/null 2>&1 && mount -t tessera $PART $M || { echo MKFS_FAIL; exit 3; }

# Fill to ~92% with 256 KiB files.
i=0
while [ $i -lt 20000 ]; do
    dd if=/dev/random of=$M/f$i bs=262144 count=1 2>/dev/null || break
    i=$((i+1))
    [ $((i % 200)) = 0 ] && { u=$(df -m $M | awk 'NR==2{print $5}' | tr -d '%'); [ "$u" -ge 92 ] && break; }
done
sync; sleep 5
echo "filled: $i files, $(df -h $M | awk 'NR==2{print $3" used, "$4" free, "$5}')"
row before

# Delete half — every dead pack is now reclaimable.
j=0; while [ $j -lt $i ]; do rm -f $M/f$j; j=$((j+2)); done
sync; sleep 5
echo "deleted $(( i / 2 )) files"
row postdel

t=0
while [ $t -lt $WATCH ]; do sleep 30; t=$((t+30)); row "t=$t"; done

echo "--- forcing gc_now"
t0=$(date +%s); sysctl kern.tessera.gc_now=1 >/dev/null 2>&1; sync; sleep 5
echo "gc_now took $(( $(date +%s) - t0 ))s"; row after
cd /; umount $M; gpart destroy -F $DISK >/dev/null 2>&1; echo done
