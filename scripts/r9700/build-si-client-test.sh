#!/bin/sh
# Cross-build the stage 5c-2 userspace test (atrium-gpu-amd/tests/
# si_client_test.c) as a static FreeBSD amd64 binary for the R9700 box.
#
#   sh scripts/r9700/build-si-client-test.sh [out]   # default: atrium-gpu-amd/out/si-client-test
#
# SYSROOT is a FreeBSD amd64 world (default: the VM image's,
# /root/atrium-vm-amd64/sysroot); LLVM as in gpu-kmod-rust.sh. The IB comes
# from the oracle: regenerate tests/si_w42.h with
#   (cd atrium-rdna4-oracle && cargo run -q --example si_client_w42 > ../atrium-gpu-amd/tests/si_w42.h)
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SYSROOT="${SYSROOT:-/root/atrium-vm-amd64/sysroot}"
LLVM="${LLVM:-19}"
OUT="${1:-$ROOT/atrium-gpu-amd/out/si-client-test}"
mkdir -p "$(dirname "$OUT")"
/usr/bin/clang-$LLVM --target=x86_64-unknown-freebsd16.0 --sysroot="$SYSROOT" \
    -fuse-ld=lld -static -O2 -Wall -Wextra -Werror -Wno-unused-parameter \
    -I"$ROOT/atrium-gpu-amd" -o "$OUT" "$ROOT/atrium-gpu-amd/tests/si_client_test.c"
echo "$OUT: $(sha256sum "$OUT" | cut -d' ' -f1)"
