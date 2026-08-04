#!/bin/sh
# Dedup existence oracle, channel 1 (tessera-fs.md §20.1).
#
# The attack: write a candidate, fsync, re-read free space. If free space is
# UNCHANGED the bytes already existed somewhere on the volume -> existence leak.
#
# Controls that matter here:
#   - duplicate and unique payloads are the SAME SIZE and written identically;
#     only the CONTENT differs, which is the whole point.
#   - free space is read as statfs blocks, never `df -h` (which rounds away the
#     signal).
#   - sync + settle between steps so a buffered write is not miscounted.
#   - 3 rounds: one pair is not a measurement.
# NO set -e.
V=/mnt/qvol
SZ=4      # MiB — well above the inline threshold so content is really chunked

free_blocks() { /root/tquota statfs $V | awk '/f_blocks/ {for(i=1;i<=NF;i++) if($i ~ /^f_bfree=/) {sub(/f_bfree=/,"",$i); print $i}}'; }
settle() { sync; sleep 3; }

echo "seed: one 'secret' file that ALREADY EXISTS on the volume"
dd if=/dev/random of=/tmp/secret.bin bs=1m count=$SZ status=none
cp /tmp/secret.bin $V/secret-existing.bin
settle

echo
printf '%-7s %-14s %-14s %s\n' round dup_delta uniq_delta verdict
r=1
while [ $r -le 3 ]; do
    # --- arm DUP: write content identical to the existing secret ---
    b0=$(free_blocks)
    cp /tmp/secret.bin $V/probe-dup-$r.bin
    settle
    b1=$(free_blocks)
    dup=$((b0 - b1))

    # --- arm UNIQ: same size, never-before-seen content ---
    dd if=/dev/random of=/tmp/uniq-$r.bin bs=1m count=$SZ status=none
    b2=$(free_blocks)
    cp /tmp/uniq-$r.bin $V/probe-uniq-$r.bin
    settle
    b3=$(free_blocks)
    uniq=$((b2 - b3))

    if [ "$dup" -lt $((uniq / 2)) ]; then v="ORACLE OPEN (dup cheaper)"; else v="content-independent"; fi
    printf '%-7s %-14s %-14s %s\n' "$r" "$dup blk" "$uniq blk" "$v"
    r=$((r + 1))
done

echo
echo "expected if dedup is SYNCHRONOUS: dup~=0, uniq~=$((SZ * 256)) blocks (${SZ} MiB / 4096)"
echo "expected if DEFERRED works:       dup ~= uniq"
echo "dedup counters:"
sysctl -n kern.tessera.publish_dedup_manifest kern.tessera.publish_dedup_chunked 2>/dev/null
echo ORACLE_DONE
