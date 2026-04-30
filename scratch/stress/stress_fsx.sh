#!/bin/sh
# fsx — Apple's File System eXerciser. The canonical "torture a single
# file with random read/write/truncate/mmap ops + verify against a
# shadow buffer" tool. Catches:
#   - Read returning stale/wrong bytes (manifest pointer bugs)
#   - Truncate not actually truncating
#   - Write/read interleaving inconsistencies
#   - Off-by-one at chunk/manifest boundaries
#
# fsx is in /usr/src/tools/regression/fsx — built once + installed to
# /usr/local/bin/fsx by stress_runall.sh. If missing, this script
# will refuse to run.
#
# Defaults to 10000 ops on a 256 KiB max-size file. Tune via env:
#   STRESS_FSX_OPS    (default 10000)
#   STRESS_FSX_FSIZE  (default 262144)
#   STRESS_FSX_BSIZE  (default 4096; max op size)
#
# fsx exits non-zero on any mismatch. Per its convention, the failing
# operation is logged + the file is left in place for inspection.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
FSX=/usr/local/bin/fsx
[ -x "$FSX" ] || { echo "FAIL: $FSX missing — see stress_runall.sh"; exit 1; }

OPS=${STRESS_FSX_OPS:-10000}
FSIZE=${STRESS_FSX_FSIZE:-262144}
BSIZE=${STRESS_FSX_BSIZE:-4096}

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 --seed-file h --seed-content x /tmp/fsx.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/fsx.img)
mount -t tessera /dev/$MD /mnt/tessera

echo "--- fsx: $OPS ops, max file=$FSIZE B, max op=$BSIZE B ---"
# -W: disable mmap writes; -R: disable mmap reads. Tessera doesn't
# implement vop_getpages/vop_putpages yet (mmap-via-VFS is a deferred
# v2 piece), so fsx's mmap ops fail with EINVAL. Plain read/write
# still gives us strong torture coverage of the manifest + chunk
# tree path.
$FSX -W -R -U -N $OPS -l $FSIZE -p $((OPS / 100 + 1)) -r $BSIZE -t $BSIZE -w $BSIZE \
    /mnt/tessera/fsx_target

# Sanity remount + read-back.
umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
[ -f /mnt/tessera/fsx_target ] || { echo "FAIL: target file lost across remount"; exit 1; }
SIZE=$(stat -f %z /mnt/tessera/fsx_target)
echo "  post-remount file size: $SIZE bytes"

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
