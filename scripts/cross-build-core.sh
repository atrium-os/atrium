#!/bin/sh
# Cross-build libtessera_core.a for aarch64-freebsd ON THE HOST.
#
# The tree used to say this had to happen inside the VM. The reason given was
# that macOS `ar` mangles ELF objects — but that indicts the ARCHIVER, not the
# compiler, and we already cross-compile the kmod here with the buildenv's
# clang. Two things were actually missing:
#
#   1. an ELF-aware archiver — the cross toolchain ships one (llvm-ar), and
#   2. FreeBSD userland headers — the obj-tree sysroot only carries what the
#      KERNEL build needed (55 dirs, no stdlib.h). `make includes` fails on
#      macOS in the install step (_INCSINS error 64), so the headers are
#      staged once into .bootstrap-state/freebsd-sysroot by
#      scripts/sync-freebsd-sysroot.sh.
#
# With those, this is an ordinary cross build and the VM is not involved.
set -eu
BSD_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CORE="$BSD_DIR/atrium-tessera/core"
SYSROOT="${TESSERA_CROSS_SYSROOT:-$BSD_DIR/.bootstrap-state/freebsd-sysroot}"
CC_BIN="${CC_BIN:-/opt/homebrew/opt/llvm/bin/clang}"
AR_BIN="${AR_BIN:-/opt/homebrew/opt/llvm/bin/llvm-ar}"
TRIPLE="${TRIPLE:-aarch64-unknown-freebsd16.0}"
OUT="$CORE/libtessera_core.a"

[ -f "$SYSROOT/usr/include/stdlib.h" ] || {
    echo "no cross sysroot at $SYSROOT — run scripts/sync-freebsd-sysroot.sh first" >&2
    exit 2
}

SRCS="error.c hash.c crc.c codec.c cdc.c btree.c manifest.c pack.c journal.c \
      extent.c gc.c volume.c quota.c quota_store.c \
      b3_blake3.c b3_blake3_portable.c b3_shim.c tessera_reader.c"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "cross-building libtessera_core.a ($TRIPLE)"
for f in $SRCS; do
    "$CC_BIN" -target "$TRIPLE" --sysroot="$SYSROOT" \
        -O2 -fno-strict-aliasing -fPIC \
        -I"$CORE/include" -I"$CORE/src" \
        -c "$CORE/src/$f" -o "$TMP/${f%.c}.o"
done
rm -f "$OUT"
"$AR_BIN" rcs "$OUT" "$TMP"/*.o
echo "  $OUT  $(wc -c < "$OUT" | tr -d ' ') bytes, $(ls "$TMP"/*.o | wc -l | tr -d ' ') objects"
