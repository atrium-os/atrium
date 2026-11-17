#!/bin/sh
# A FreeBSD 16 sysroot for cross builds, from pkgbase (no VM, no ISO):
# libraries, headers, CRT objects and libc++ for one arch.
#
#   sh scripts/layers/freebsd-sysroot.sh amd64|aarch64 OUT
#
# Packages come from pkg.freebsd.org/FreeBSD:16:<arch>/base_latest, checked
# against the signed catalogue (pkgfetch.py). OUT/SOURCES lists what went in.
# Used by build-rust-layer.sh and the SDK sysroot archives (dev-tools-plan.md
# M3, M12).
set -eu
ARCH=${1:?amd64 or aarch64}
OUT=${2:?out dir}
HERE=$(cd "$(dirname "$0")" && pwd)
case "$ARCH" in amd64|aarch64) ;; *) echo "arch: amd64 or aarch64" >&2; exit 2 ;; esac
PKGS=${PKGS_DIR:-$OUT.pkgs}
mkdir -p "$OUT" "$PKGS"
# Everything a library or header lives in; no debug info, 32-bit, kernels,
# sources, tests or the base toolchain itself.
python3 -I "$HERE/pkgfetch.py" --abi "FreeBSD:16:$ARCH" --repo base_latest --out "$PKGS" \
    --match '^FreeBSD-' \
    --exclude='-(dbg|lib32|man)(-|$)' --exclude='kernel|^FreeBSD-src|^FreeBSD-tests|^FreeBSD-set-' \
    --exclude='firmware|clang|lldb|^FreeBSD-lld$|^FreeBSD-toolchain$|googletest|bootloader|rescue|locales|zoneinfo|games|examples|syscons|vt-data' \
    >/dev/null
for p in "$PKGS"/*.pkg; do
    bsdtar -xf "$p" -C "$OUT" --exclude '+*' 2>/dev/null || bsdtar -xf "$p" -C "$OUT" --exclude '+*'
done
cp "$PKGS/SOURCES" "$OUT/SOURCES"
for f in usr/lib/Scrt1.o usr/lib/crti.o usr/lib/crtbeginS.o usr/lib/libc.so libexec/ld-elf.so.1 usr/include/stdio.h usr/include/c++/v1/vector; do
    [ -e "$OUT/$f" ] || { echo "freebsd-sysroot: $OUT has no $f" >&2; exit 1; }
done
echo "freebsd-sysroot: $ARCH in $OUT ($(du -sh "$OUT" | cut -f1), $(grep -vc '^#' "$OUT/SOURCES") packages)"
