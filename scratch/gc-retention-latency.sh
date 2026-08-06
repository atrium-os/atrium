#!/bin/sh
# How much does lowering kern.tessera.snapshot_retention shorten the time from
# "files deleted" to "space actually reclaimed"?
#
# The unit is COMMITS, not seconds: eligibility is gated by snapshot records
# ageing out of the retention horizon, and one commit retires at most one
# record. Wall-clock latency is (commits x commit interval), and the commit
# interval here is my sync loop, not a real workload — so seconds measured in
# this harness would say more about the harness than about the filesystem.
#
# Same workload, same device, fresh mkfs per arm, one variable changed.
set -u
DEV=/dev/vtbd0; MNT=/mnt/rl
sysc() { sysctl -n "kern.tessera.$1" 2>/dev/null || echo 0; }
MAXC=26

printf '%-6s %-9s %-9s %-10s %s\n' RETENT COMMITS RECLAIMED SNAPRETIRED ELAPSED_S
for R in 16 8 4 2; do
    umount $MNT 2>/dev/null; mkdir -p $MNT
    /root/mkfs-tessera $DEV >/dev/null 2>&1
    sysctl kern.tessera.snapshot_retention=$R >/dev/null
    mount -t tessera $DEV $MNT

    mkdir -p $MNT/live $MNT/trash
    i=1; while [ $i -le 6 ];  do dd if=/dev/random of=$MNT/live/f$i  bs=1m count=1 2>/dev/null; i=$((i+1)); done
    i=1; while [ $i -le 40 ]; do dd if=/dev/random of=$MNT/trash/g$i bs=1m count=1 2>/dev/null; i=$((i+1)); done
    sync; sleep 1
    rm -rf $MNT/trash; sync; sleep 2

    r0=$(sysc gc_reclaimed); s0=$(sysc snapshots_retired)
    t0=$(date +%s)
    c=0; got=0
    while [ $c -lt $MAXC ]; do
        # one commit
        echo tick > $MNT/live/.t; sync; sleep 1
        c=$((c+1))
        # try to collect
        rb=$(sysc gc_reclaimed)
        daemon -f /root/tq gc $MNT
        w=0
        while [ $w -lt 120 ]; do
            ps -ax -o command | grep -q "[t]q gc" || break
            sleep 2; w=$((w+2))
        done
        ra=$(sysc gc_reclaimed)
        if [ $((ra - rb)) -ge 20 ]; then got=1; break; fi
    done
    t1=$(date +%s)
    rf=$(sysc gc_reclaimed); sf=$(sysc snapshots_retired)
    [ $got -eq 1 ] || c="none<=$MAXC"
    printf '%-6s %-9s %-9s %-10s %s\n' "$R" "$c" "$((rf-r0))" "$((sf-s0))" "$((t1-t0))"
    rm -f $MNT/live/.t
    umount $MNT
done
sysctl kern.tessera.snapshot_retention=16 >/dev/null
echo "(restored snapshot_retention=16)"
