#!/bin/sh
# Build the EFI loader with Atrium Tessera CAS-FS read-only support.
# Runs INSIDE the VM. The tessera stand integration now lives in the
# freebsd-src tree (stand/libsa/Makefile + stand/efi/loader/{Makefile,conf.c},
# gated on LOADER_TESSERA_SUPPORT=yes). This script syncs those three files
# from the 9p freebsd-src checkout into the VM's build tree (/usr/src, a
# separate ZFS copy) and builds with the knob enabled.
set -e
SRC=/mnt/host/freebsd-src/usr/src/stand
DST=/usr/src/stand
CORE=/mnt/host/atrium-tessera/core

test -e $CORE/src/tessera_reader.c || { echo "9p not mounted?"; exit 1; }

# Sync the tessera-aware stand files from the tracked freebsd-src tree.
cp $SRC/libsa/Makefile         $DST/libsa/Makefile
cp $SRC/efi/loader/Makefile    $DST/efi/loader/Makefile
cp $SRC/efi/loader/conf.c      $DST/efi/loader/conf.c

echo "=== building EFI loader (LOADER_TESSERA_SUPPORT=yes) ==="
cd $DST
make -j4 LOADER_TESSERA_SUPPORT=yes TESSERA_SRCTOP=/mnt/host/atrium-tessera \
    2>&1 | tail -40
echo "=== find the built loader ==="
find /usr/obj -name 'loader*.efi' -newer $DST/libsa/Makefile 2>/dev/null | head
