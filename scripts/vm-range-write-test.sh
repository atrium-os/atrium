#!/bin/sh
# Regression test: truncate-shrink and size-changing writes WITHOUT
# materialising the whole file.
#
# THE BUG THIS PINS DOWN
#
#   Two Tessera paths built the whole file in RAM on every call: a truncate
#   read the entire old file and allocated the entire new one, and a write that
#   ended past EOF (unless it was a pure append the fast path accepted) did the
#   same. fsx on a 300 MiB file in the 4 GiB dev VM wired ~900 MiB, pinned free
#   memory at 30-200 MiB and made the guest unresponsive within minutes. A
#   dtrace profile named malloc_large from tessera_vop_write, followed by
#   kmem back/unback + pagezero churn.
#
#   Shrinks are now a manifest prefix (tessera_fs_truncate_prefix) and growing
#   writes decompose into overwrite + holes + append
#   (tessera_fs_write_grow_range). This test checks both are BYTE-EXACT against
#   an oracle — the same operations on a tmpfs file — across the layouts and
#   boundaries where a prefix rebuild can go wrong, then again after unmount,
#   fsck and remount.
#
# Destructive to the SCRATCH disk only, gated on its GEOM ident.
#
#   sh scripts/vm-range-write-test.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$BSD/scripts/vssh"

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
[ -f "$KO" ] || { echo "no $KO — build it first"; exit 1; }
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
k=$($VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != tree [$KMOD]"; exit 1; }

echo "=== RANGE-WRITE TEST $(date) kmod=$KMOD ==="
OUT=$($VSSH '
M=/mnt/scratch; DEV=/dev/vtbd2; O=/mnt/rwo      # O = tmpfs oracle
diskinfo -s $DEV | grep -q "^atrium-scratch$" || { echo REFUSING_ident; exit 2; }
mount | grep -q " $M " && umount $M
# ★ The oracle gets its OWN sized tmpfs. The guest /tmp is a 20 MiB tmpfs, and
# an oracle write that fails with ENOSPC looks exactly like a Tessera
# content mismatch — the first run of this test reported 17 "failures" that
# were all the oracle running out of space.
mkdir -p $M $O; mount | grep -q " $O " || mount -t tmpfs -o size=700m tmpfs $O || { echo ORACLE_FAIL; exit 6; }
rm -f $O/*
mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV $M || { echo MKFS_FAIL; exit 3; }
S(){ sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
TP0=$(S truncate_prefix); TF0=$(S truncate_prefix_fallback)
WG0=$(S write_range_grow); WF0=$(S write_range_grow_fallback)
WW0=$(S write_wholefile); SW0=$(S setattr_wholefile)
MiB=1048576; fail=0; checks=0
bad(){ echo "BAD: $*"; fail=$((fail+1)); }
# same op on both files
both(){ "$@" $M/$F; "$@" $O/$F; }
tr_(){ truncate -s $1 $M/$F; truncate -s $1 $O/$F; }
wr(){ dd if=$3 of=$M/$F bs=1m seek=$1 count=$2 conv=notrunc 2>/dev/null; dd if=$3 of=$O/$F bs=1m seek=$1 count=$2 conv=notrunc 2>/dev/null; }
# ONE write syscall of 8 MiB at 8-MiB block $1. `wr` issues 1 MiB writes, so a
# "straddling" wr splits into in-place writes below EOF and exact appends at
# it — the first version of this test never straddled EOF at all, which the
# range_grow dead-arm guard caught.
wr8(){ dd if=$2 of=$M/$F bs=8m seek=$1 count=1 conv=notrunc 2>/dev/null; dd if=$2 of=$O/$F bs=8m seek=$1 count=1 conv=notrunc 2>/dev/null; }
cmpf(){ checks=$((checks+1)); a=$(sha256 -q $M/$F); b=$(sha256 -q $O/$F)
  [ "$a" = "$b" ] || bad "$1: content differs"
  [ "$(stat -f %z $M/$F)" = "$(stat -f %z $O/$F)" ] || bad "$1: size differs"; }
dd if=/dev/random of=$O/rand8 bs=1m count=8 2>/dev/null

# ── CHUNK_TREE file: 200 MiB written sequentially (64 KiB chunks, 16 MiB groups)
F=tree; dd if=/dev/random of=$O/$F bs=1m count=200 2>/dev/null; cp $O/$F $M/$F; sync
cmpf "tree base"
tr_ $((150*MiB+12345));        cmpf "shrink mid-chunk mid-group"
tr_ $((128*MiB));              cmpf "shrink exact group boundary"
tr_ $((100*MiB+3*65536));      cmpf "shrink exact chunk boundary"
tr_ $((300*1024));             cmpf "shrink to 300 KiB (just over INLINE)"
tr_ $((100*1024));             cmpf "shrink to 100 KiB (INLINE result)"
tr_ 0;                         cmpf "shrink to 0"

# ── growing writes on a big file
F=grow; dd if=/dev/random of=$O/$F bs=1m count=100 2>/dev/null; cp $O/$F $M/$F; sync
wr8 12 $O/rand8;               cmpf "one write straddling EOF (96..104 MiB over 100)"
wr 180 1 $O/rand8;             cmpf "write past EOF with a 76 MiB gap"
wr 181 4 $O/rand8;             cmpf "write at exact EOF"
tr_ $((170*MiB+777));          cmpf "shrink back into the hole region"
wr 60 8 $O/rand8;              cmpf "in-place overwrite after all that"

# ── CHUNK_LIST file (<256 chunks) and a still-buffered one
F=list; dd if=/dev/random of=$O/$F bs=1m count=10 2>/dev/null; cp $O/$F $M/$F; sync
tr_ $((5*MiB+7));              cmpf "list shrink mid-chunk"
F=buffered; dd if=/dev/random of=$O/$F bs=1m count=2 2>/dev/null
cp $O/$F $M/$F                 # deliberately no sync: content may be in RAM
tr_ $((1*MiB+4095));           cmpf "shrink of un-drained buffered content"

sync
echo "counters truncate_prefix=$(( $(S truncate_prefix)-TP0 )) prefix_fallback=$(( $(S truncate_prefix_fallback)-TF0 )) range_grow=$(( $(S write_range_grow)-WG0 )) grow_fallback=$(( $(S write_range_grow_fallback)-WF0 )) write_wholefile=$(( $(S write_wholefile)-WW0 )) setattr_wholefile=$(( $(S setattr_wholefile)-SW0 ))"

# ── persistence: unmount, fsck, remount, re-verify every file from disk
sync; umount $M || { echo UMOUNT_FAIL; exit 4; }
tessera-fsck $DEV > $O/rw.fsck 2>&1
echo "fsck_problems=$(grep -ciE "dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem" $O/rw.fsck)"
grep -E "result:|^    - " $O/rw.fsck | head -5
mount -t tessera $DEV $M || { echo REMOUNT_FAIL; exit 5; }
for F in tree grow list buffered; do cmpf "remount $F"; done
umount $M; umount $O
echo "checks=$checks bad=$fail"
' 2>&1 | tr -d '\r')

echo "$OUT"
case "$OUT" in *REFUSING*|*MKFS_FAIL*|*ORACLE_FAIL*|*"No space left"*|*UMOUNT_FAIL*|*REMOUNT_FAIL*) echo "FAIL — harness could not run"; exit 1;; esac
BAD=$(echo "$OUT" | sed -n 's/.*bad=\([0-9]*\).*/\1/p')
CK=$(echo "$OUT" | sed -n 's/^checks=\([0-9]*\).*/\1/p')
FS=$(echo "$OUT" | sed -n 's/.*fsck_problems=\([0-9]*\).*/\1/p')
TP=$(echo "$OUT" | sed -n 's/.*truncate_prefix=\([0-9]*\).*/\1/p')
WG=$(echo "$OUT" | sed -n 's/.*range_grow=\([0-9]*\).*/\1/p')
WW=$(echo "$OUT" | sed -n 's/.*write_wholefile=\([0-9]*\).*/\1/p')
SW=$(echo "$OUT" | sed -n 's/.*setattr_wholefile=\([0-9]*\).*/\1/p')
rc=0
[ "${BAD:-1}" = 0 ] || { echo "FAIL — $BAD check(s) failed"; rc=1; }
[ "${CK:-0}" -ge 18 ] 2>/dev/null || { echo "FAIL — only ${CK:-0} comparisons ran"; rc=1; }
[ "${FS:-1}" = 0 ] || { echo "FAIL — fsck found $FS problems"; rc=1; }
# ★ DEAD-ARM GUARDS. The shrinks above must have taken the prefix path and the
# straddling/gap writes the range-grow path; if the whole-file counters moved
# instead, the new code did not run and the byte checks prove nothing new.
[ "${TP:-0}" -ge 7 ] 2>/dev/null || { echo "FAIL — truncate_prefix moved ${TP:-0} times, expected >= 7"; rc=1; }
[ "${WG:-0}" -ge 2 ] 2>/dev/null || { echo "FAIL — write_range_grow moved ${WG:-0} times, expected >= 2"; rc=1; }
[ "${SW:-1}" = 0 ] || { echo "FAIL — $SW truncate(s) still materialised the whole file"; rc=1; }
[ "${WW:-1}" = 0 ] || echo "NOTE — $WW write(s) took the whole-file path"
[ $rc = 0 ] && echo "PASS — prefix shrinks and range-scoped growing writes are byte-exact and survive fsck + remount"
exit $rc
