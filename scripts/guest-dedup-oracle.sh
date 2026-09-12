#!/bin/sh
# Validate tessera-fs.md §20.2 `deferred` dedup: does it actually close the
# §20.1 channel-1 existence oracle (free-space observable via statfs)?
#
# THE TEST. Write a NOVEL 4 MiB file and a DUPLICATE 4 MiB file (identical
# bytes to one already stored), fsync each, and measure the free-space delta.
#   GLOBAL   : duplicate costs ~nothing, novel costs ~4 MiB  -> ORACLE OPEN
#   DEFERRED : both cost ~4 MiB, content-independent          -> ORACLE CLOSED
# Spec's own 2026-08-06 numbers: 20K vs 4156K under GLOBAL (208x).
#
# ★ Then the part that matters for shipping it: DEFERRED only preserves dedup
#   if the duplicate extents are actually RECLAIMED later. The kernel's own
#   refusal message says "the dead-extent log is not wired yet", so measure
#   whether the space comes back after drain + GC rather than assuming.
set -u
M=/mnt/scratch; DEV=/dev/vtbd2
diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
free_k() { df -k "$M" | tail -1 | awk '{print $4}'; }

mount | grep -q " $M " && umount $M; mkdir -p $M
mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV $M || { echo MKFS_FAIL; exit 3; }

# One 4 MiB pattern, written once so a later identical write is a duplicate.
mkdir -p $M/seed
dd if=/dev/random of=/root/pattern.bin bs=1m count=4 2>/dev/null
cp /root/pattern.bin $M/seed/original; sync; sleep 1

arm() {   # $1 = policy name, $2 = policy number
  rm -rf $M/dom; mkdir -p $M/dom
  /root/tquota set $M/dom 0 >/dev/null 2>&1 || { echo "  tquota set FAILED"; return 1; }
  /root/tdedup $M/dom "$2" 2>&1 | sed 's/^/  /'
}

measure() {  # $1 = label
  sync; sleep 1; b=$(free_k)
  cp /root/pattern.bin $M/dom/dup-$1 ; sync; sleep 1
  d=$(free_k); DUP=$(( b - d ))
  b=$(free_k)
  dd if=/dev/random of=$M/dom/novel-$1 bs=1m count=4 2>/dev/null; sync; sleep 1
  d=$(free_k); NOV=$(( b - d ))
  R=$( [ "$DUP" -gt 0 ] && echo $(( NOV / DUP )) || echo "inf" )
  printf "  duplicate cost=%-8sK   novel cost=%-8sK   ratio=%sx\n" "$DUP" "$NOV" "$R"
  printf "  verdict: %s\n" "$( [ "$DUP" -gt 0 ] && [ "$R" != inf ] && [ "$R" -le 3 ] 2>/dev/null && echo 'content-INDEPENDENT (oracle closed)' || echo 'content-DEPENDENT (ORACLE OPEN)')"
}

echo "=== ARM 0: GLOBAL (default) — expect the oracle OPEN ==="
sysctl kern.tessera.dedup_deferred_enable=0 >/dev/null 2>&1
arm global 0 && measure global

echo
echo "=== ARM 1: DEFERRED — expect content-INDEPENDENT ==="
sysctl kern.tessera.dedup_deferred_enable=1 >/dev/null 2>&1
echo "  dedup_deferred_enable=$(sysctl -n kern.tessera.dedup_deferred_enable)"
if arm deferred 1; then
  measure deferred
  echo
  echo "=== does the duplicate space come BACK? (drain + GC) ==="
  before=$(free_k)
  sync; /root/tq $M >/dev/null 2>&1; /root/tq $M >/dev/null 2>&1; sync; sleep 2
  after=$(free_k)
  echo "  free before GC=${before}K  after=${after}K  recovered=$(( after - before ))K"
  echo "  files intact: $(ls $M/dom | wc -l | tr -d ' ') in dom, seed=$(ls $M/seed | wc -l | tr -d ' ')"
  cmp -s /root/pattern.bin $M/dom/dup-deferred && echo "  duplicate content VERIFIED identical" || echo "  ★ DUPLICATE CONTENT CORRUPTED"
fi

echo
sync; umount $M 2>/dev/null
tessera-fsck $DEV > /root/dd.fsck 2>&1
echo "fsck_problems=$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/dd.fsck)"
