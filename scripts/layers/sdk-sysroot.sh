#!/bin/sh
# The SDK sysroot archive for cross builds from Linux and macOS
# (dev-tools.md §5.4, dev-tools-plan.md M12): lib, usr/lib, usr/include,
# usr/libdata/pkgconfig, the CRT objects and ld-elf.so.1 of FreeBSD 16 for
# one arch, from pkgbase via freebsd-sysroot.sh.
#
#   sh scripts/layers/sdk-sysroot.sh amd64|aarch64 [OUT]     env SDK_VERSION (0.1), SYSROOT
#
# Writes OUT/atrium-sysroot-<sdk version>-<arch>.tar.xz and .sha256. Unpacked
# at $ATRIUM_SYSROOT/<arch> (or ~/.atrium/sysroot/<arch>) it is what
# `cargo atrium cross <arch>` uses.
set -eu
ARCH=${1:?amd64 or aarch64}
OUT=${2:-$PWD}
SDK_VERSION=${SDK_VERSION:-0.1}
HERE=$(cd "$(dirname "$0")" && pwd)
case "$ARCH" in amd64|aarch64) ;; *) echo "arch: amd64 or aarch64" >&2; exit 2 ;; esac
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
FULL=${SYSROOT:-}
if [ -z "$FULL" ]; then
    FULL=$(mktemp -d)/full
    sh "$HERE/freebsd-sysroot.sh" "$ARCH" "$FULL"
fi
NAME=atrium-sysroot-$SDK_VERSION-$ARCH
STAGE=$(mktemp -d)
S=$STAGE/$ARCH
mkdir -p "$S/libexec" "$S/usr/libdata"
(cd "$FULL" && tar -cf - lib usr/lib usr/include usr/libdata/pkgconfig libexec/ld-elf.so.1 COPYRIGHT SOURCES) | (cd "$S" && tar -xf -)
# Profiling and PIC archives aren't linked by anything an app builds.
find "$S/usr/lib" -name '*_p.a' -delete
find "$S/usr/lib" -name '*_pic.a' -delete
# A link that points at an absolute path would resolve on the build host.
if [ -n "$(find "$S" -type l -lname '/*' | head -1)" ]; then
    echo "sdk-sysroot: absolute symlinks in the sysroot:" >&2
    find "$S" -type l -lname '/*' >&2
    exit 1
fi
cat > "$S/README" <<T
Atrium SDK $SDK_VERSION sysroot, FreeBSD 16 $ARCH (from pkg.freebsd.org
base_latest, each package checked against the signed catalogue; SOURCES
lists them). BSD-licensed base libraries; see COPYRIGHT.

Unpack so this folder is \$ATRIUM_SYSROOT/$ARCH (or ~/.atrium/sysroot/$ARCH),
then: cargo atrium cross $ARCH
T
(cd "$STAGE" && tar -cf - "$ARCH" | xz -T0 -6 > "$OUT/$NAME.tar.xz")
(cd "$OUT" && sha256sum "$NAME.tar.xz" > "$NAME.tar.xz.sha256")
rm -rf "$STAGE"
echo "sdk-sysroot: $OUT/$NAME.tar.xz ($(du -h "$OUT/$NAME.tar.xz" | cut -f1))"
