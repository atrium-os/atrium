#!/bin/sh
# Cross-build the fork's userland (buildworld) on a Linux host and stage it as
# a tarball for a FreeBSD guest. Lite world: no toolchain, tests, lib32 or debug
# files (etc/src.conf.world-lite); the guest keeps its own cc. About 62 min on 4 cores.
#
#   FBSD_SRC=~/atrium-os/freebsd-src TARGET=amd64 sh scripts/cross-build-world.sh
#   then:  cat $OUT | scripts/vssh 'cat > /var/tmp/world.tgz'   and in the guest:
#   chflags -R noschg /bin /sbin /lib /libexec /usr/bin /usr/sbin /usr/lib /usr/libexec
#   cd / && tar -xpUf /var/tmp/world.tgz --exclude ./etc --exclude ./var --exclude ./root \
#       --exclude ./dev --exclude ./tmp --exclude ./METALOG && shutdown -r now
#
# Needs: clang-19/lld-19 (clang 18 breaks kmods; use one compiler for everything),
# flex, bison, time, libarchive-tools (bsdtar reads the NO_ROOT METALOG).
# Stop the VM first if the host is short on RAM: the build plus an 8 GiB TCG guest
# got the guest OOM-killed on a 15 GiB host.
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
FBSD_SRC=${FBSD_SRC:-$HOME/atrium-os/freebsd-src}
TARGET=${TARGET:-amd64}
case $TARGET in amd64) TARGET_ARCH=amd64 ;; arm64) TARGET_ARCH=aarch64 ;; *) echo "TARGET?"; exit 1 ;; esac
OBJ=${MAKEOBJDIRPREFIX:-/home/user/fbsd-obj}
STAGE=${STAGE:-/home/user/stage-world-$TARGET}
OUT=${OUT:-/home/user/world-$TARGET.tgz}
MK="tools/build/make.py --cross-bindir=/usr/lib/llvm-19/bin TARGET=$TARGET TARGET_ARCH=$TARGET_ARCH -j$(nproc) SRCCONF=$ROOT/etc/src.conf.world-lite"
export CC=/usr/bin/clang-19 CXX=/usr/bin/clang++-19 CPP=/usr/bin/clang-cpp-19 MAKEOBJDIRPREFIX=$OBJ
cd "$FBSD_SRC"
$MK buildworld
rm -rf "$STAGE"; mkdir -p "$STAGE"
$MK installworld DESTDIR="$STAGE" -DNO_ROOT
(cd "$STAGE" && bsdtar -czf "$OUT" @METALOG)
ls -lh "$OUT"
