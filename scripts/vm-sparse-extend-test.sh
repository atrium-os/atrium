#!/bin/sh
# Regression test: truncate-EXTEND past the 512 MiB materialise cap.
#
# THE LIMIT THIS PINS DOWN
#
#   vop_setattr extended a file by building the WHOLE new file — old content
#   plus zero padding — in one contiguous M_WAITOK buffer, so it refused any
#   new size past TESSERA_WRITE_MATERIALIZE_MAX (512 MiB) with EFBIG. The cap
#   is on the operation, not on file size, and an extension is zeros, which
#   the append paths already store as ZERO_HOLE records with no blob at all.
#
#   What it broke: a per-app overlay VOLUME is an image created by
#   mkfs-tessera's ftruncate, so no overlay could exceed 512 MiB and the
#   usable quota topped out at 384 MiB (portcullis.md §4.1).
#
#   tessera_fs_extend_sparse now appends hole windows instead. This test checks
#   that the result is BYTE-EXACT, costs no data space, survives unmount +
#   fsck + remount, keeps prefix content intact across every starting layout
#   (empty, INLINE, chunked, partial tail chunk), and is still bounded.
#
# Destructive to the SCRATCH disk only, gated on its GEOM ident.
#
#   sh scripts/vm-sparse-extend-test.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$BSD/scripts/vssh"

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
[ -f "$KO" ] || { echo "no $KO — build it first"; exit 1; }
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
. "$BSD/scripts/lib/guest-ident.sh"   # GUEST_KMOD_HASH: the LOADED module, only on Laminar/RLC/tessera root
k=$($VSSH "$GUEST_KMOD_HASH" 2>/dev/null | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != tree [$KMOD]"; exit 1; }

echo "=== SPARSE-EXTEND TEST $(date) kmod=$KMOD ==="
OUT=$($VSSH '
M=/mnt/scratch; DEV=/dev/vtbd2
diskinfo -s $DEV | grep -q "^atrium-scratch$" || { echo REFUSING_ident; exit 2; }
mount | grep -q " $M " && umount $M
mkdir -p $M
mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV $M || { echo MKFS_FAIL; exit 3; }
S(){ sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
X0=$(S extend_sparse); W0=$(S extend_sparse_windows); F0=$(S extend_sparse_fallback)
MiB=1048576
fail=0
bad(){ echo "BAD: $*"; fail=$((fail+1)); }

# expected hash of <prefix file> followed by zeros up to <size>
expect(){ p=$1; n=$2; ps=0; [ -n "$p" ] && ps=$(stat -f %z "$p")
  { [ -n "$p" ] && cat "$p"; head -c $((n-ps)) /dev/zero; } | sha256; }

used0=$(df -k $M | tail -1 | awk "{print \$3}")

# A: empty -> 1 GiB (no content to retain: head window rebuilt as a tree)
: > $M/a; truncate -s $((1024*MiB)) $M/a || bad "A truncate failed"
[ "$(stat -f %z $M/a)" = $((1024*MiB)) ] || bad "A size"
echo "A $(expect "" $((1024*MiB)))" > /root/se.expect

# B: 1 KiB INLINE -> 700 MiB
head -c 1024 /dev/random > /root/se.b; cp /root/se.b $M/b
truncate -s $((700*MiB)) $M/b || bad "B truncate failed"
echo "B $(expect /root/se.b $((700*MiB)))" >> /root/se.expect

# C: 100 MiB of content -> 1500 MiB (chunked; appends at its own chunk size)
dd if=/dev/random of=/root/se.c bs=1m count=100 2>/dev/null; cp /root/se.c $M/c; sync
t0=$(date +%s)
truncate -s $((1500*MiB)) $M/c || bad "C truncate failed"
echo "C_seconds=$(( $(date +%s) - t0 ))"
echo "C $(expect /root/se.c $((1500*MiB)))" >> /root/se.expect

# D: partial last chunk (70 MiB + 12345) -> 900 MiB
head -c $((70*MiB+12345)) /dev/random > /root/se.d; cp /root/se.d $M/d; sync
truncate -s $((900*MiB)) $M/d || bad "D truncate failed"
echo "D $(expect /root/se.d $((900*MiB)))" >> /root/se.expect

# E: just over the sparse threshold, small growth (65 -> 80 MiB)
dd if=/dev/random of=/root/se.e bs=1m count=65 2>/dev/null; cp /root/se.e $M/e; sync
truncate -s $((80*MiB)) $M/e || bad "E truncate failed"
echo "E $(expect /root/se.e $((80*MiB)))" >> /root/se.expect

sync
used1=$(df -k $M | tail -1 | awk "{print \$3}")
X1=$(S extend_sparse); W1=$(S extend_sparse_windows); F1=$(S extend_sparse_fallback)
echo "sparse_extends=$((X1-X0)) windows=$((W1-W0)) fallbacks=$((F1-F0))"
# Content written by the test is 1K + 100M + 70M + 65M = ~235 MiB; the ~4.2 GiB
# of holes on top must add essentially nothing.
echo "used_kib_delta=$((used1-used0))"

# F: in-place write into the middle of the hole region of A
dd if=/dev/random of=/root/se.f bs=1m count=8 2>/dev/null
dd if=/root/se.f of=$M/a bs=1m seek=500 conv=notrunc 2>/dev/null; sync
[ "$(stat -f %z $M/a)" = $((1024*MiB)) ] || bad "F changed the size"
{ head -c $((500*MiB)) /dev/zero; cat /root/se.f; head -c $((516*MiB)) /dev/zero; } | sha256 > /root/se.fx
sed -i "" "s/^A .*/A $(cat /root/se.fx)/" /root/se.expect

# G: bounded — an extension past the volume capacity is refused, size unchanged
if truncate -s $((8192*MiB)) $M/e 2>/dev/null; then bad "G 8 GiB extend on a 4 GiB volume succeeded"; fi
[ "$(stat -f %z $M/e)" = $((80*MiB)) ] || bad "G left the size changed"

verify(){ tag=$1
  while read f want; do got=$(sha256 -q $M/$(echo $f | tr A-Z a-z))
    [ "$got" = "$want" ] || bad "$tag: $f content differs"; done < /root/se.expect; }
verify live

# H: persistence — unmount, fsck the device, remount, re-verify from disk
sync; umount $M || { echo UMOUNT_FAIL; exit 4; }
tessera-fsck $DEV > /root/se.fsck 2>&1
echo "fsck_problems=$(grep -ciE "dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem" /root/se.fsck)"
mount -t tessera $DEV $M || { echo REMOUNT_FAIL; exit 5; }
verify remount
umount $M
echo "bad=$fail"
' 2>&1 | tr -d '\r')

echo "$OUT"
case "$OUT" in *REFUSING*|*MKFS_FAIL*|*UMOUNT_FAIL*|*REMOUNT_FAIL*) echo "FAIL — harness could not run"; exit 1;; esac
BAD=$(echo "$OUT" | sed -n 's/^bad=\([0-9]*\).*/\1/p')
FS=$(echo "$OUT" | sed -n 's/.*fsck_problems=\([0-9]*\).*/\1/p')
XS=$(echo "$OUT" | sed -n 's/.*sparse_extends=\([0-9]*\).*/\1/p')
FB=$(echo "$OUT" | sed -n 's/.*fallbacks=\([0-9]*\).*/\1/p')
UD=$(echo "$OUT" | sed -n 's/.*used_kib_delta=\([0-9-]*\).*/\1/p')
rc=0
[ "${BAD:-1}" = 0 ] || { echo "FAIL — $BAD check(s) failed"; rc=1; }
[ "${FS:-1}" = 0 ]  || { echo "FAIL — fsck found $FS problems"; rc=1; }
# ★ DEAD-ARM GUARD. Five extends above cross the sparse threshold. If the
# counter did not move five times, the materialising path answered instead and
# every size-under-512 MiB case would pass without testing anything new.
[ "${XS:-0}" = 5 ] || { echo "FAIL — extend_sparse moved ${XS:-0} times, expected 5: the sparse path did not run"; rc=1; }
[ "${FB:-1}" = 0 ] || { echo "FAIL — $FB extend(s) fell back to materialising"; rc=1; }
# Holes must not cost data space: allow the ~235 MiB of real content plus
# metadata slack, far below the ~4.2 GiB of logical growth.
[ "${UD:-999999999}" -lt $((300*1024)) ] 2>/dev/null || { echo "FAIL — used grew by ${UD} KiB: holes are being stored as data"; rc=1; }
[ $rc = 0 ] && echo "PASS — sparse truncate-extend is byte-exact, hole-backed, bounded, and survives fsck + remount"
exit $rc
