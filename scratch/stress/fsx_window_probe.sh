#!/bin/sh
# Reproduce the windowing fsx mismatch deterministically and A/B it
# against windowing-off (kern.tessera.append_window_bytes=0) with the SAME
# seed and a volume big enough that ENOSPC churn doesn't confound.
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
FSX=/usr/local/bin/fsx
SEED=${SEED:-1}
OPS=${OPS:-4000}
FSIZE=${FSIZE:-8388608}
BSIZE=${BSIZE:-262144}
WIN=${WIN:-16777216}        # 0 = windowing off

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
sysctl kern.tessera.append_window_bytes=$WIN >/dev/null

rm -f /tmp/fsx.img
$BIN/mkfs-tessera --create -s 1024 --seed-file h --seed-content x /tmp/fsx.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/fsx.img)
mount -t tessera /dev/$MD /mnt/tessera

echo "--- fsx seed=$SEED ops=$OPS fsize=$FSIZE window_bytes=$WIN ---"
$FSX -S $SEED -W -R -U -N $OPS -l $FSIZE -r $BSIZE -t $BSIZE -w $BSIZE \
    /mnt/tessera/fsx_target 2>&1 | tail -12
echo "fsx_exit=$?"

umount /mnt/tessera 2>/dev/null
mdconfig -d -u 0 2>/dev/null
rm -f /tmp/fsx.img
echo PROBE_DONE
