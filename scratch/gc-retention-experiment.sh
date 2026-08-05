#!/bin/sh
# Is the "deleted data is not collectable while mounted" effect just the
# snapshot retention horizon? If so, forcing >retention commits should make it
# collectable ONLINE, with no unmount.
set -u
DEV=/dev/vtbd0; MNT=/mnt/ret
sysc() { sysctl -n "kern.tessera.$1" 2>/dev/null || echo 0; }
umount $MNT 2>/dev/null; mkdir -p $MNT
/root/mkfs-tessera $DEV >/dev/null
mount -t tessera $DEV $MNT
echo "  snapshot_retention = $(sysc snapshot_retention)"
mkdir -p $MNT/live $MNT/trash
i=1; while [ $i -le 12 ]; do dd if=/dev/random of=$MNT/live/f$i bs=1m count=1 2>/dev/null; i=$((i+1)); done
i=1; while [ $i -le 40 ]; do dd if=/dev/random of=$MNT/trash/g$i bs=1m count=1 2>/dev/null; i=$((i+1)); done
sync; sleep 1
rm -rf $MNT/trash; sync; sleep 2

r0=$(sysc gc_reclaimed); s0=$(sysc snapshots_retired)
daemon -f /root/tq gc $MNT; w=0
while [ $w -lt 120 ]; do ps -ax -o command | grep -q "[t]q gc" || break; sleep 5; w=$((w+5)); done
r1=$(sysc gc_reclaimed)
echo "  GC immediately after delete : reclaimed $((r1-r0)) pack(s)"

# Force commits past the retention horizon. Each sync drives a commit_sb.
i=1
while [ $i -le 24 ]; do
    echo x > $MNT/live/tick$i; sync; sleep 1
    i=$((i+1))
done
s1=$(sysc snapshots_retired)
echo "  snapshot records retired by the horizon: $((s1-s0))"

r2=$(sysc gc_reclaimed)
daemon -f /root/tq gc $MNT; w=0
while [ $w -lt 180 ]; do ps -ax -o command | grep -q "[t]q gc" || break; sleep 5; w=$((w+5)); done
r3=$(sysc gc_reclaimed)
echo "  GC after $((s1-s0)) snapshots aged out: reclaimed $((r3-r2)) pack(s)  <-- ONLINE"
umount $MNT
