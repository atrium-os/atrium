#!/bin/sh
# #114 verification set. NO set -e — every probe must report.
V=/mnt/v; D=$V/dom; SZ=4
free_blk() { /root/tq statfs $1 | awk '/f_blocks/{for(i=1;i<=NF;i++) if($i ~ /^f_bfree=/){sub(/f_bfree=/,"",$i); print $i}}'; }
settle() { sync; sleep 3; }

echo "=== setup: fresh volume, quota domain, deferred policy ==="
umount $V 2>/dev/null; mkdir -p $V
/root/mkfs-tessera --create -s 2048 /dev/vtbd1 >/dev/null 2>&1
mount -t tessera /dev/vtbd1 $V || { echo "MOUNT FAILED"; exit 1; }
mkdir -p $D
/root/tq set $D 0                      # quota root, unlimited
sysctl kern.tessera.dedup_deferred_enable=1 >/dev/null
/root/tq policy $D 1 || { echo "ARMING FAILED"; exit 1; }

echo
echo "=== 1. ORACLE: does free space stop depending on content? ==="
dd if=/dev/random of=/tmp/secret.bin bs=1m count=$SZ status=none
cp /tmp/secret.bin $D/existing.bin; settle
printf '%-7s %-12s %-12s %s\n' round dup uniq verdict
r=1
while [ $r -le 3 ]; do
    b0=$(free_blk $V); cp /tmp/secret.bin $D/dup$r.bin; settle; b1=$(free_blk $V)
    dup=$((b0-b1))
    dd if=/dev/random of=/tmp/u$r.bin bs=1m count=$SZ status=none
    b2=$(free_blk $V); cp /tmp/u$r.bin $D/uniq$r.bin; settle; b3=$(free_blk $V)
    uq=$((b2-b3))
    if [ "$dup" -lt $((uq/2)) ]; then v="STILL OPEN"; else v="flat"; fi
    printf '%-7s %-12s %-12s %s\n' $r "$dup blk" "$uq blk" "$v"
    r=$((r+1))
done

echo
echo "=== 2. dead-extent log populated? ==="
sysctl -n kern.tessera.dead_extent_recorded kern.tessera.dead_extent_sectors

echo
echo "=== 3. GC drain: does the space come back? ==="
sysctl -n kern.tessera.dead_extent_drained
umount $V; mount -t tessera /dev/vtbd1 $V     # remount to trigger GC paths
settle
sysctl -n kern.tessera.dead_extent_drained kern.tessera.dead_extent_sectors

echo
echo "=== 4. read-back integrity (dedup must not have lost data) ==="
cmp /tmp/secret.bin $D/dup1.bin && echo "  dup1 == secret OK"
cmp /tmp/u1.bin $D/uniq1.bin && echo "  uniq1 == source OK"

echo
echo "=== 5. fsck on a volume holding dead extents ==="
umount $V
/root/tessera-fsck /dev/vtbd1 2>&1 | tail -15
echo "fsck rc=$?"
echo VERIFY114_DONE
