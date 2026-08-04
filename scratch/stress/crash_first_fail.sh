#!/bin/sh
# Focused crash-recovery repro: fresh image, log each cycle's op, STOP at the
# first fsck-dirty cycle and preserve a corpse. Isolates the trigger op.
set -u
CYCLES=${CYCLES:-300}
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
IMG=/tmp/cff.img
umount /mnt/tessera 2>/dev/null || true; mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
$BIN/mkfs-tessera --create -s 64 /tmp/cff.img >/dev/null
MD=$(mdconfig -a -t vnode -f $IMG)
mount -t tessera /dev/$MD /mnt/tessera
echo seed > /mnt/tessera/seed
dd if=/dev/random of=/mnt/tessera/big bs=4096 count=20 2>/dev/null
mkdir /mnt/tessera/d
umount /mnt/tessera; mdconfig -d -u $MD
i=1
while [ $i -le $CYCLES ]; do
  MD=$(mdconfig -a -t vnode -f $IMG)
  mount -t tessera /dev/$MD /mnt/tessera
  sysctl kern.tessera.skip_next_sb=1 >/dev/null
  case $((i % 5)) in
  0) OP="write";  echo "iter $i" > /mnt/tessera/log$i ;;
  1) OP="append"; echo "more-$i" >> /mnt/tessera/big ;;
  2) OP="mkdir";  mkdir -p /mnt/tessera/d/sub$i ;;
  3) OP="reflink"; cp /mnt/tessera/big /mnt/tessera/rl$i ;;
  4) OP="rm";     [ -f /mnt/tessera/log$((i-5)) ] && rm /mnt/tessera/log$((i-5)) ;;
  esac
  umount /mnt/tessera; mdconfig -d -u $MD          # crash: SB skipped
  MD=$(mdconfig -a -t vnode -f $IMG)               # remount: replay
  mount -t tessera /dev/$MD /mnt/tessera
  umount /mnt/tessera; mdconfig -d -u $MD
  if ! $BIN/tessera-fsck $IMG >/tmp/cff_fsck.out 2>&1; then
    echo "FIRST-FAIL cycle=$i op=$OP"
    cp $IMG /tmp/cff_corpse.img
    grep -E "PROBLEM|result|dangling|orphan|leaked|overlap|nlink|neither" /tmp/cff_fsck.out | head -8
    exit 0
  fi
  [ $((i % 25)) -eq 0 ] && echo "  ...$i clean"
  i=$((i+1))
done
echo "ALL $CYCLES CLEAN"
