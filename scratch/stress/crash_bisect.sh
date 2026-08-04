#!/bin/sh
# Op-bisect for the crash-recovery bug. OPS env selects which cycle slots do
# what; everything else is a benign inline write. Fresh img, stop+report first fail.
# OPS e.g. "1append 3reflink" => op-1 appends big, op-3 reflinks, others write.
set -u
CYCLES=${CYCLES:-200}
OPS=${OPS:-"3reflink"}
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
IMG=/tmp/cbi.img
umount /mnt/tessera 2>/dev/null||true; mdconfig -d -u 0 2>/dev/null||true
kldunload tessera_fs 2>/dev/null||true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
$BIN/mkfs-tessera --create -s 64 $IMG >/dev/null
MD=$(mdconfig -a -t vnode -f $IMG); mount -t tessera /dev/$MD /mnt/tessera
echo seed > /mnt/tessera/seed
dd if=/dev/random of=/mnt/tessera/big bs=4096 count=20 2>/dev/null
mkdir /mnt/tessera/d
umount /mnt/tessera; mdconfig -d -u $MD
i=1
while [ $i -le $CYCLES ]; do
  MD=$(mdconfig -a -t vnode -f $IMG); mount -t tessera /dev/$MD /mnt/tessera
  sysctl kern.tessera.skip_next_sb=1 >/dev/null
  slot=$((i % 5)); OP="write"; act=""
  for tok in $OPS; do
    case "$tok" in
      ${slot}append)  act="append";  echo "m$i" >> /mnt/tessera/big ;;
      ${slot}mkdir)   act="mkdir";   mkdir -p /mnt/tessera/d/s$i ;;
      ${slot}reflink) act="reflink"; cp /mnt/tessera/big /mnt/tessera/rl$i ;;
      ${slot}rm)      act="rm";      [ -f /mnt/tessera/log$((i-5)) ] && rm /mnt/tessera/log$((i-5)) ;;
    esac
  done
  [ -z "$act" ] && { OP="write"; echo "iter $i" > /mnt/tessera/log$i; }
  umount /mnt/tessera; mdconfig -d -u $MD
  MD=$(mdconfig -a -t vnode -f $IMG); mount -t tessera /dev/$MD /mnt/tessera
  umount /mnt/tessera; mdconfig -d -u $MD
  if ! $BIN/tessera-fsck $IMG >/tmp/cbi_fsck.out 2>&1; then
    echo "FIRST-FAIL cycle=$i op=${act:-write} [OPS=$OPS]"
    grep -E "result|dangling|orphan|leaked" /tmp/cbi_fsck.out | head -3
    exit 0
  fi
  i=$((i+1))
done
echo "ALL $CYCLES CLEAN [OPS=$OPS]"
