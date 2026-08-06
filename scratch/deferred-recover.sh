#!/bin/sh
# Does `deferred` PRESERVE dedup, or merely delay the oracle at full disk cost?
#
# If the duplicate bytes are never reclaimed, deferred = no dedup, and a shared
# volume with deferred domains buys nothing over per-app volumes. The whole
# argument for keeping one shared volume rests on this recovering.
set -u
DEV=/dev/vtbd0; V=/mnt/qvol; SZ=4
sysc() { sysctl -n "kern.tessera.$1" 2>/dev/null || echo 0; }
freek() { df -k $V | tail -1 | awk '{print $4}'; }
umount $V 2>/dev/null; mkdir -p $V
/root/mkfs-tessera $DEV >/dev/null 2>&1
sysctl kern.tessera.dedup_deferred_enable=1 >/dev/null
mount -t tessera $DEV $V || exit 2
/root/tq set $V 200000000 >/dev/null 2>&1
/root/tq policy $V 1 >/dev/null 2>&1

dd if=/dev/random of=/tmp/secret.bin bs=1m count=$SZ status=none
cp /tmp/secret.bin $V/original.bin; sync; sleep 2
base=$(freek)
echo "  free after the original ${SZ} MiB:            ${base}K"

# 5 exact duplicates, written under the deferred policy
i=1; while [ $i -le 5 ]; do cp /tmp/secret.bin $V/dup$i.bin; i=$((i+1)); done
sync; sleep 3
after=$(freek)
echo "  free after 5 duplicates (deferred):          ${after}K   (consumed $((base-after))K)"
echo "  dead_extent_recorded: $(sysc dead_extent_recorded)"

# Age past the retention horizon so the deferred duplicates are collectable,
# then drain + GC.
R=$(sysc snapshot_retention)
i=1; while [ $i -le $((R + 6)) ]; do echo t > $V/.tick; sync; sleep 1; i=$((i+1)); done
rm -f $V/.tick; sync
daemon -f /root/tq gc $V
w=0; while [ $w -lt 180 ]; do ps -ax -o command | grep -q "[t]q gc" || break; sleep 5; w=$((w+5)); done
sync; sleep 2
recl=$(freek)
echo "  free after drain + GC:                       ${recl}K   (recovered $((recl-after))K)"
echo "  dead_extent_drained: $(sysc dead_extent_drained)  gc_reclaimed: $(sysc gc_reclaimed)"
ls $V/dup*.bin | wc -l | sed 's/^/  duplicate files still present: /'
cmp -s /tmp/secret.bin $V/dup3.bin && echo "  dup3.bin content intact" || echo "  ★ dup3.bin CORRUPT"
umount $V
