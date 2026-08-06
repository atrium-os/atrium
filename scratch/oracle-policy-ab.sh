#!/bin/sh
# Does the `deferred` dedup policy close channel 1 while KEEPING dedup?
#
# This is the question that decides whether jails need their own volumes (which
# destroys cross-app dedup — every bundle's shared base stored N times) or can
# live on one shared volume with untrusted domains marked `deferred`.
#
# Two arms on the same fresh volume shape, one variable: the domain policy.
#   GLOBAL   -> expect dup << uniq  (oracle OPEN, the 208x result)
#   DEFERRED -> expect dup ~= uniq  (content-independent, oracle CLOSED)
#
# ★ GATE: the deferred arm must show dead_extent_recorded MOVING. If the policy
# never armed, every write dedups as usual and "dup ~= uniq" could only come
# out by accident — a closed-looking result for the wrong reason.
set -u
DEV=/dev/vtbd0; V=/mnt/qvol; SZ=4
sysc() { sysctl -n "kern.tessera.$1" 2>/dev/null || echo 0; }
free_blocks() { df -k $V | tail -1 | awk '{print $4}'; }
settle() { sync; sleep 3; }

arm() {
    pol=$1; name=$2
    umount $V 2>/dev/null; mkdir -p $V
    /root/mkfs-tessera $DEV >/dev/null 2>&1
    mount -t tessera $DEV $V || { echo "mount failed"; return 1; }
    /root/tq set $V 200000000 >/dev/null 2>&1
    /root/tq policy $V $pol >/dev/null 2>&1
    echo "== $name (policy=$pol) =="
    d0=$(sysc dead_extent_recorded)

    dd if=/dev/random of=/tmp/secret.bin bs=1m count=$SZ status=none
    cp /tmp/secret.bin $V/secret-existing.bin
    settle
    r=1
    while [ $r -le 3 ]; do
        b0=$(free_blocks); cp /tmp/secret.bin $V/probe-dup-$r.bin;  settle; b1=$(free_blocks)
        dd if=/dev/random of=/tmp/u$r.bin bs=1m count=$SZ status=none
        b2=$(free_blocks); cp /tmp/u$r.bin  $V/probe-uniq-$r.bin; settle; b3=$(free_blocks)
        dup=$((b0-b1)); uniq=$((b2-b3))
        if [ "$dup" -lt $((uniq / 2)) ]; then vd="ORACLE OPEN (dup cheaper)"; else vd="content-independent"; fi
        printf '  round %s: dup %-8s uniq %-8s %s\n' "$r" "${dup}K" "${uniq}K" "$vd"
        r=$((r+1))
    done
    d1=$(sysc dead_extent_recorded)
    echo "  dead_extent_recorded: $d0 -> $d1  (delta $((d1-d0)))"
    if [ "$pol" = "1" ] && [ "$((d1-d0))" -eq 0 ]; then
        echo "  ★ GATE FAILED: deferred never armed — this arm proves nothing"
    fi
    umount $V
}

sysctl kern.tessera.dedup_deferred_enable=1 >/dev/null
arm 0 "ARM A: GLOBAL (synchronous dedup)"
echo
arm 1 "ARM B: DEFERRED"
