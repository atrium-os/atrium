#!/bin/sh
# Cross-build the sysfil ATF tests and benchmark on the Linux host for the
# FreeBSD guest (docs/spec/jail-sysfil.md §7). Nothing is compiled in the VM.
#
#   FBSD_SRC=~/freebsd-src SYSROOT=~/atrium-vm-amd64/sysroot \
#       sh scripts/sysfil/cross-build.sh [amd64|aarch64] <outdir>
#
# libatf-c is built from the fork's contrib/atf, because the VM snapshot
# image ships kyua but not the ATF libraries.
set -eu
ARCH="${1:-amd64}"; OUT="${2:?outdir}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FBSD_SRC="${FBSD_SRC:-$HOME/freebsd-src}"
case "$ARCH" in
    amd64)   T=x86_64-unknown-freebsd16.0;  SYSROOT="${SYSROOT:-$HOME/atrium-vm-amd64/sysroot}" ;;
    aarch64) T=aarch64-unknown-freebsd16.0; SYSROOT="${SYSROOT:-$HOME/atrium-vm/sysroot}" ;;
    *) echo "ARCH: amd64 or aarch64" >&2; exit 2 ;;
esac
CC="${CC:-/usr/lib/llvm-19/bin/clang}"
CFLAGS="--target=$T --sysroot=$SYSROOT -fuse-ld=lld -O2 -Wall -Werror"
mkdir -p "$OUT/atf"
A="$FBSD_SRC/contrib/atf"
for f in atf-c/build.c atf-c/check.c atf-c/error.c atf-c/tc.c atf-c/tp.c atf-c/utils.c \
         atf-c/detail/dynstr.c atf-c/detail/env.c atf-c/detail/fs.c atf-c/detail/list.c \
         atf-c/detail/map.c atf-c/detail/process.c atf-c/detail/sanity.c atf-c/detail/text.c \
         atf-c/detail/tp_main.c atf-c/detail/user.c; do
    $CC --target=$T --sysroot=$SYSROOT -O1 -w -I"$A" -DHAVE_CONFIG_H \
        '-DATF_BUILD_CC="cc"' '-DATF_BUILD_CFLAGS="-Wall"' '-DATF_BUILD_CPP="cpp"' \
        '-DATF_BUILD_CPPFLAGS=""' '-DATF_BUILD_CXX="c++"' '-DATF_BUILD_CXXFLAGS="-Wall"' \
        "-DATF_ARCH=\"$ARCH\"" "-DATF_MACHINE=\"$ARCH\"" '-DATF_CONFDIR="/etc/atf"' \
        '-DATF_INCLUDEDIR="/usr/include"' '-DATF_LIBEXECDIR="/usr/libexec"' \
        '-DATF_PKGDATADIR="/usr/share/atf"' '-DATF_SHELL="/bin/sh"' '-DATF_WORKDIR="/tmp"' \
        -c "$A/$f" -o "$OUT/atf/$(basename "$f" .c).o"
done
/usr/lib/llvm-19/bin/llvm-ar rcs "$OUT/libatf-c.a" "$OUT"/atf/*.o
# The test uses the fork's <sys/jail.h> only through libjail strings, so the
# guest's own headers are enough; -I"$A" supplies atf-c.h.
$CC $CFLAGS -I"$A" -o "$OUT/jail_sysfil" "$FBSD_SRC/tests/sys/kern/jail_sysfil.c" \
    -L"$OUT" -latf-c -ljail
$CC $CFLAGS -o "$OUT/sysfil-bench" "$ROOT/scripts/sysfil/sysfil-bench.c"
echo "built $OUT/jail_sysfil $OUT/sysfil-bench"
