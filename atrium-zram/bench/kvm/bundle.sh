#!/bin/sh
# Build the one-file, paste-able zram-kvm-bench.sh: host.sh with the guest
# scripts, the zram kmod sources, the fill workload, the swap-free patch
# (atrium-os/freebsd-src da0b30649, sys/vm only) and the swap fast-path patch,
# appended as a base64 tar.xz.
#   sh atrium-zram/bench/kvm/bundle.sh [OUT]
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
Z="$HERE/../.."
OUT="${1:-$HERE/zram-kvm-bench.sh}"
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/zram"
cp "$Z/atrium_zram.c" "$Z/zram_pool.c" "$Z/zram_pool.h" "$Z/zram_lz4.c" "$Z/zram_lz4.h" \
    "$Z/Makefile" "$T/zram/"
cp "$Z/test/fill.c" "$HERE/classify.awk" "$HERE/guest-build.sh" "$HERE/guest-run.sh" \
    "$HERE/swapfree.patch" "$HERE/fastpath.patch" "$T/"
{ cat "$HERE/host.sh"; tar cf - -C "$T" . | xz -9 | base64 -w 76; } > "$OUT"
chmod +x "$OUT"
echo "$OUT: $(wc -c < "$OUT") bytes"
