#!/bin/sh
#
# import-region-stress.sh — N-iter mint/import/free stress for
# IMPORT_REGION. Run inside the FreeBSD VM. Watches the kmod's
# sysctls (kern.atrium_gpu.{bos,tokens}_outstanding) so a refcount
# leak would surface as the counters not returning to zero.
#
# Usage:
#   scripts/import-region-stress.sh [N]
#
# Default N=1000. ~1 min for 1000 iters on the dev VM.
#
# Requires:
# - atrium_virtio_gpu.ko loaded
# - /mnt/host/atrium-gpu-rs/target/aarch64-unknown-freebsd/release/
#   examples/import_region present (cross-build on host first)

set -e
N=${1:-1000}
BIN=/mnt/host/atrium-gpu-rs/target/aarch64-unknown-freebsd/release/examples/import_region

if [ ! -x "$BIN" ]; then
  echo "error: $BIN not found — cross-build atrium-gpu-rs/examples/import_region first" >&2
  exit 1
fi

echo "pre-run:"
sysctl kern.atrium_gpu

FAILS=0
for i in $(seq 1 "$N"); do
  rm -f /tmp/import_stress.hex
  $BIN mint > /tmp/import_stress.hex 2>/dev/null &
  MPID=$!
  # Wait until the full token (64 hex + newline = 65 bytes) is written.
  # Avoids racing on a partial read while mint is mid-flush.
  while [ "$(wc -c < /tmp/import_stress.hex 2>/dev/null || echo 0)" -lt 65 ]; do : ; done
  TOKEN=$(cat /tmp/import_stress.hex | tr -d '\n')
  $BIN import "$TOKEN" > /dev/null 2>&1 || FAILS=$((FAILS + 1))
  kill "$MPID" 2>/dev/null || true
  wait "$MPID" 2>/dev/null || true
  if [ $((i % 100)) -eq 0 ]; then echo "  iter $i: $FAILS fails"; fi
done
rm -f /tmp/import_stress.hex

# Give the kmod a moment to drain final fd-close decrefs.
sleep 1
echo "post-run:"
sysctl kern.atrium_gpu
echo "final: $FAILS fails / $N iters"
exit $FAILS
