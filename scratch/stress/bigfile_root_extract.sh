#!/bin/sh
# Verify tessera supports >64 MiB files: extract the FULL FreeBSD base.txz
# (which ships libprivatellvm.so.19 ~87 MB and libprivateclang.so.19 ~70 MB)
# onto a fresh tessera volume on /dev/vtbd2, read the big libs back and
# compare hashes against the originals from the tarball, then fsck.
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
DEV=/dev/vtbd2
MNT=/mnt/tessera
TXZ=/root/base.txz

BIG1=./usr/lib/libprivatellvm.so.19
BIG2=./usr/lib/libprivateclang.so.19

umount $MNT 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko || exit 1

echo "=== mkfs on $DEV (3072 MiB) ==="
$BIN/mkfs-tessera --create -s 3072 $DEV || exit 1

mkdir -p $MNT
mount -t tessera $DEV $MNT || exit 1
echo "=== mounted; extracting full base.txz (no excludes) ==="
if tar -xpf $TXZ -C $MNT; then
    echo "TAR_EXTRACT: OK"
else
    rc=$?
    echo "TAR_EXTRACT: FAILED rc=$rc"
fi

echo "=== big files on tessera ==="
ls -l $MNT/$BIG1 $MNT/$BIG2 2>&1

echo "=== reference hashes from the tarball ==="
ref1=$(tar -xOf $TXZ $BIG1 2>/dev/null | sha256 -q)
ref2=$(tar -xOf $TXZ $BIG2 2>/dev/null | sha256 -q)
echo "ref llvm  = $ref1"
echo "ref clang = $ref2"

echo "=== read-back hashes from tessera ==="
got1=$(sha256 -q $MNT/$BIG1)
got2=$(sha256 -q $MNT/$BIG2)
echo "got llvm  = $got1"
echo "got clang = $got2"

ok=1
[ "$ref1" = "$got1" ] && [ -n "$ref1" ] && echo "llvm  MATCH" || { echo "llvm  MISMATCH"; ok=0; }
[ "$ref2" = "$got2" ] && [ -n "$ref2" ] && echo "clang MATCH" || { echo "clang MISMATCH"; ok=0; }

echo "=== df ==="
df -h $MNT

echo "=== umount + fsck ==="
umount $MNT || { echo "UMOUNT FAILED"; exit 1; }
$BIN/tessera-fsck $DEV
fsckrc=$?
echo "fsck rc=$fsckrc"

[ $ok -eq 1 ] && [ $fsckrc -eq 0 ] && echo "=== OVERALL: PASS ===" || echo "=== OVERALL: FAIL ==="
