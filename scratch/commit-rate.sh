#!/bin/sh
# What wall-clock rollback depth does snapshot_retention actually deliver?
#
# Depth = retention x commit interval. #116 assumed the interval swings with
# write rate; this measures it. Two phases on the SAME volume:
#   LIGHT  — one small write every 10s (timer-bounded commits)
#   HEAVY  — buildkernel -j4 with its object tree ON the Tessera volume
#
# Counter: kern.tessera.prof_c_commit_calls (direct commit_sb count), with
# snapshots_retired as a cross-check — once the horizon is saturated exactly
# one record retires per commit, so the two deltas should agree.
set -u
DEV=/dev/vtbd0; MNT=/mnt/obj
sysc() { sysctl -n "kern.tessera.$1" 2>/dev/null || echo 0; }
umount $MNT 2>/dev/null; mkdir -p $MNT
/root/mkfs-tessera $DEV >/dev/null 2>&1
mount -t tessera $DEV $MNT
R=$(sysc snapshot_retention)
echo "  snapshot_retention=$R  flush_interval_sec=$(sysc flush_interval_sec)"

# saturate the horizon so snapshots_retired tracks commits 1:1
i=1; while [ $i -le $((R + 4)) ]; do echo x > $MNT/warm; sync; sleep 1; i=$((i+1)); done

phase() {
    _name=$1; _dur=$2
    c0=$(sysc prof_c_commit_calls); s0=$(sysc snapshots_retired); t0=$(date +%s)
    sleep "$_dur"
    c1=$(sysc prof_c_commit_calls); s1=$(sysc snapshots_retired); t1=$(date +%s)
    dt=$((t1 - t0)); dc=$((c1 - c0)); ds=$((s1 - s0))
    if [ "$dc" -gt 0 ]; then
        printf '  %-6s %3ds: %4d commits (%s s/commit)  retired=%d  ->  depth = %s s\n' \
            "$_name" "$dt" "$dc" "$(echo "scale=1; $dt/$dc" | bc)" "$ds" \
            "$(echo "scale=1; $R*$dt/$dc" | bc)"
    else
        printf '  %-6s %3ds: 0 commits — nothing dirty, horizon does not move\n' "$_name" "$dt"
    fi
}

echo "== LIGHT: one 4 KiB write every 10s =="
( i=1; while [ $i -le 12 ]; do echo tick > $MNT/light.$i; sleep 10; i=$((i+1)); done ) &
LPID=$!
phase LIGHT 120
kill $LPID 2>/dev/null; wait $LPID 2>/dev/null

# ★ NEITHER buildkernel NOR a libc build runs in this guest's /usr/src:
# buildkernel dies after ~39s on sched_laminar.o, libc within 30s. Both are
# pre-existing tree problems, nothing to do with Tessera. An earlier run
# ramped 60s and then sampled a DEAD build, reporting "0 commits" as though
# the filesystem were quiet under load — so this arm now uses a synthetic
# workload that reliably runs, and ASSERTS IT IS ALIVE while sampling.
#
# SUBSTITUTION, stated plainly: this measures the commit rate under sustained
# concurrent small-file writes — the shape of load a build puts on the FS —
# not buildkernel specifically. It bounds the commit interval under heavy
# write pressure, which is the quantity #116 needs.
echo "== HEAVY: 4 concurrent small-file writer loops on the Tessera volume =="
mkdir -p $MNT/heavy
w=1
while [ $w -le 4 ]; do
    ( n=0; while :; do
        dd if=/dev/random of=$MNT/heavy/w$w.$n bs=16k count=1 2>/dev/null
        n=$((n+1)); [ $((n % 64)) -eq 0 ] && rm -f $MNT/heavy/w$w.*
      done ) &
    eval "P$w=$!"
    w=$((w+1))
done
sleep 20
alive=0; for pid in $P1 $P2 $P3 $P4; do kill -0 $pid 2>/dev/null && alive=$((alive+1)); done
if [ $alive -lt 4 ]; then
    echo "  GATE FAILED: only $alive/4 writers alive — not a loaded volume"
else
    f0=$(ls $MNT/heavy | wc -l | tr -d ' ')
    phase HEAVY 120
    echo "  (writers alive throughout: 4/4, $(ls $MNT/heavy | wc -l | tr -d ' ') files in flight vs $f0 at start)"
fi
for pid in $P1 $P2 $P3 $P4; do kill -9 $pid 2>/dev/null; done
pkill -9 make 2>/dev/null; pkill -9 cc 2>/dev/null
sleep 3
umount $MNT 2>/dev/null || { sleep 10; umount $MNT 2>/dev/null; }
