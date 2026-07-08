#!/bin/sh
# Task #18 validation: repack-vs-read race stress + regression suite.
set -u
for p in atrium-frescod atrium-memfed atrium-memoryd; do pkill -STOP "$p" 2>/dev/null; done
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

echo "=== build ==="
umount -f /mnt/tessera 2>/dev/null; mdconfig -d -u 0 2>/dev/null; kldunload tessera_fs 2>/dev/null
cd /mnt/host/atrium-tessera/kmod
make >/tmp/build.out 2>&1 || { echo BUILD-FAILED; tail -25 /tmp/build.out; exit 1; }
echo "build ok: $(ls -l tessera_fs.ko | awk '{print $5}') bytes"
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

fresh() {
  umount -f /mnt/tessera 2>/dev/null; mdconfig -d -u 0 2>/dev/null
  rm -f /tmp/fsx.img
  $BIN/mkfs-tessera --create -s $1 --seed-file h --seed-content x /tmp/fsx.img >/dev/null 2>&1
  MD=$(mdconfig -a -t vnode -f /tmp/fsx.img); mount -t tessera /dev/$MD /mnt/tessera
}

echo "=== S0: big-file window path (100MB urandom dd + readback) ==="
umount -f /mnt/troot 2>/dev/null
$BIN/mkfs-tessera -j 65536 --create -s 3072 /dev/vtbd2 >/dev/null 2>&1
mount -t tessera /dev/vtbd2 /mnt/troot || { echo S0-MOUNT-FAIL; exit 1; }
dd if=/dev/urandom of=/tmp/s0.bin bs=1m count=100 2>/dev/null
W=$(sha256 -q /tmp/s0.bin)
T0=$(date +%s); dd if=/tmp/s0.bin of=/mnt/troot/big bs=64k 2>/dev/null; sync; T1=$(date +%s)
R=$(sha256 -q /mnt/troot/big)
echo "S0: $((T1-T0))s $( [ "$W" = "$R" ] && echo BYTES-OK || echo BYTES-MISMATCH )"
umount /mnt/troot
$BIN/tessera-fsck /dev/vtbd2 2>&1 | tail -1
rm -f /tmp/s0.bin

echo "=== S1: repack-vs-read stress (seed multi packs, then converge under read load) ==="
fresh 256
dmesg -c >/dev/null 2>&1
# Seed reader files as MULTI_EXTENT packs (knob on during creation only)
sysctl kern.tessera.force_multi_extent=1 >/dev/null
mkdir -p /mnt/tessera/rd
i=1; while [ $i -le 6 ]; do
  dd if=/dev/urandom of=/mnt/tessera/rd/r$i bs=1m count=2 2>/dev/null
  i=$((i+1))
done
sync; sleep 1; sync
sysctl kern.tessera.force_multi_extent=0 >/dev/null
i=1; while [ $i -le 6 ]; do sha256 -q /mnt/tessera/rd/r$i > /tmp/want.$i; i=$((i+1)); done
echo "multi packs seeded: $(sysctl -n kern.tessera.repack_total_packs) repacked so far"
# threshold=1 arms background repack on next mark_dirty; contig now available
sysctl kern.tessera.repack_threshold=1 >/dev/null
# Background readers hammering the very files being relocated
MISMATCH=/tmp/mismatch18; rm -f $MISMATCH
i=1; while [ $i -le 6 ]; do
  ( n=0; while [ $n -lt 300 ] && [ ! -f $MISMATCH ]; do
      h=$(sha256 -q /mnt/tessera/rd/r$i 2>/dev/null)
      [ "$h" != "$(cat /tmp/want.$i)" ] && { echo "reader $i MISMATCH iter $n: $h" >> $MISMATCH; }
      n=$((n+1)); done ) &
  i=$((i+1))
done
# Writer churn (each dirty op re-arms repack while multi packs remain)
/tmp/fsxbig -S 7 -R -W -N 6000 -l 786432 -o 4096 /mnt/tessera/ft > /tmp/s1fsx.out 2>&1
FSXRC=$?
wait
echo "fsx rc=$FSXRC $(grep -cE 'RRRR|WWWW' /tmp/s1fsx.out) badops"
if [ -f $MISMATCH ]; then echo "READ-MISMATCH:"; cat $MISMATCH; else echo "readers: all iterations byte-correct"; fi
echo "repacks total: $(sysctl -n kern.tessera.repack_total_packs)"
sysctl kern.tessera.repack_threshold=50 >/dev/null
umount /mnt/tessera
$BIN/tessera-fsck /dev/md0 2>&1 | tail -2

echo "=== S2: regression F1 64MB ENOSPC ==="
fresh 64
/tmp/replay /tmp/oplog.txt /mnt/tessera/rt; echo "replay rc=$? (4 expected)"
umount /mnt/tessera 2>/dev/null || umount -f /mnt/tessera
$BIN/tessera-fsck /dev/md0 2>&1 | tail -2

echo "=== S3: regression F2 256MB full replay ==="
fresh 256
/tmp/replay /tmp/oplog.txt /mnt/tessera/rt; echo "replay rc=$?"
umount /mnt/tessera; $BIN/tessera-fsck /dev/md0 2>&1 | tail -2

echo "=== S4: regression F3 fsx seed-1 8000 ==="
fresh 256
/tmp/fsxbig -S 1 -R -W -N 8000 -l 786432 -o 4096 /mnt/tessera/ft > /tmp/f3.out 2>&1
echo "fsx rc=$? $(grep -cE 'RRRR|WWWW' /tmp/f3.out) badops"
umount /mnt/tessera; $BIN/tessera-fsck /dev/md0 2>&1 | tail -2
mdconfig -d -u 0 2>/dev/null
for p in atrium-frescod atrium-memfed atrium-memoryd; do pkill -CONT "$p" 2>/dev/null; done
echo STRESS18_DONE
