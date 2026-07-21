#!/bin/sh
# fsck --repair reserve-bump aliasing verification (DiskCtx &mut-across-FFI).
#
# Proves the committed meta_reserve_bump reflects the reserve nodes that
# --repair actually allocated (COW inode btree_put during nlink repair),
# NOT a stale pre-repair value. If the bump were stale (the aliasing-UB
# symptom), a subsequent rw mount would hand out sectors the repair had
# already written its COW nodes into, and the FINAL fsck would go dirty.
#
# nlink corruption is injected in place: the btree node-header CRC covers
# only the header prefix, not the leaf record payload, so patching a
# record's nlink needs no CRC fixup and hits the real repair path.
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
IMG=/tmp/fr.img
MNT=/mnt/fr
U=7

kldstat -q -n tessera_fs || kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
umount $MNT 2>/dev/null; mdconfig -d -u $U 2>/dev/null; rm -f $IMG

$BIN/mkfs-tessera --create -s 64 $IMG >/dev/null
rm -rf $MNT; mkdir -p $MNT
mdconfig -a -t vnode -u $U -f $IMG >/dev/null
mount -t tessera /dev/md$U $MNT

mkdir -p $MNT/dir
i=0; while [ $i -lt 8 ]; do echo "file-$i-content" > $MNT/dir/f$i; i=$((i+1)); done
# churn so the inode tree has real depth and the bump has advanced organically
i=0; while [ $i -lt 60 ]; do echo x > $MNT/c$((i%4)); rm -f $MNT/c$((i%4)); sync; i=$((i+1)); done
sync
for i in 0 1 2 3 4; do ls -i $MNT/dir/f$i | awk '{print $1}'; done > /tmp/fr_inos.txt
echo "target inodes: $(tr '\n' ' ' < /tmp/fr_inos.txt)"
umount $MNT; mdconfig -d -u $U

echo "=== baseline fsck ==="
$BIN/tessera-fsck $IMG | grep -E 'result|inodes'
echo "=== baseline debug ==="
$BIN/tessera-debug $IMG | grep -E 'generation|inode_root|bump pointer'

echo "=== inject nlink corruption (in place) ==="
python3 - "$IMG" /tmp/fr_inos.txt <<'PY'
import sys, struct
img = sys.argv[1]
inos = sorted({int(l.split()[0]) for l in open(sys.argv[2]) if l.strip()})
data = bytearray(open(img, 'rb').read())
REC = 144
patched = 0
for ino in inos:
    off = 0
    while off + REC <= len(data):
        if struct.unpack_from('<I', data, off)[0] == ino:
            mode  = struct.unpack_from('<I', data, off + 8)[0]
            nlink = struct.unpack_from('<I', data, off + 64)[0]
            if (mode & 0o170000) == 0o100000 and nlink == 1:
                struct.pack_into('<I', data, off + 64, 42)   # bogus nlink
                patched += 1
        off += 4
open(img, 'wb').write(data)
print(f"  patched {patched} live/stale record copy(ies) across {len(inos)} inode(s)")
PY

echo "=== fsck after corruption (expect nlink problems) ==="
$BIN/tessera-fsck $IMG | grep -E 'result|nlink' | head
PRE=$($BIN/tessera-debug $IMG | awk '/bump pointer/{print $4}')
echo "pre-repair committed bump sector: $PRE"

echo "=== REPAIR (-y) ==="
$BIN/tessera-fsck -y $IMG 2>&1 | grep -vE '^\s*$' | tail -25

POST=$($BIN/tessera-debug $IMG | awk '/bump pointer/{print $4}')
echo "=== post-repair debug ==="
$BIN/tessera-debug $IMG | grep -E 'generation|inode_root|bump pointer'
echo "bump: pre=$PRE  post=$POST"
if [ "$POST" -gt "$PRE" ]; then
  echo "OK: committed bump ADVANCED (repair's reserve allocations reflected)"
else
  echo "NOTE: bump unchanged (repair may have reused nodes) pre=$PRE post=$POST"
fi

echo "=== fsck after repair (expect CLEAN) ==="
$BIN/tessera-fsck $IMG | grep -E 'result'

echo "=== SAFETY PROOF: remount rw, write new data from committed bump, fsck ==="
mdconfig -a -t vnode -u $U -f $IMG >/dev/null
mount -t tessera /dev/md$U $MNT
i=0; while [ $i -lt 40 ]; do echo "post-repair-write-$i-payload-data" > $MNT/dir/new$i; i=$((i+1)); done
echo "  pre-existing f0=[$(cat $MNT/dir/f0)] f4=[$(cat $MNT/dir/f4)]"
sync; umount $MNT; mdconfig -d -u $U

echo "=== FINAL fsck (must be CLEAN: proves committed bump never clobbered repair nodes) ==="
$BIN/tessera-fsck $IMG | grep -E 'result|PROBLEM' | head
$BIN/tessera-debug $IMG | grep -E 'generation|bump pointer'
echo FSCK_REPAIR_ALIAS_TEST_DONE
