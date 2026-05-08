#!/bin/bash
# Run the venus smoke harness with full cross-stack tracing enabled,
# drain the kmod ring, and merge all fragments into a single Chrome Trace
# JSON for Perfetto viewing.
#
# Prerequisites:
#   - VM running with --venus (run-vm.sh --venus --display)
#   - Patched virglrenderer + MoltenVK already built on host
#   - atrium-virtio-gpu kmod rebuilt + loaded inside the VM
#   - frescod-vulkan-smoke + atrium-test-client cross-built
#   - vssh script working (~/src/bsd/scripts/vssh)
#
# Output:
#   /tmp/atrium-trace-merged.<timestamp>.json
#
# Usage: run_venus_trace.sh [frame_count]
set -euo pipefail

FRAMES=${1:-100}
TS=$(date +%s)
OUT_DIR="/tmp/atrium-trace-${TS}"
HOST_TRACE_BASE="${OUT_DIR}/host.trace"
GUEST_TRACE_BASE="${OUT_DIR}/guest.trace"
KMOD_DUMP="${OUT_DIR}/kmod.dump"
MERGED="${OUT_DIR}/merged.json"

mkdir -p "$OUT_DIR"

VSSH=~/src/bsd/scripts/vssh

echo "[1/7] Reset kmod trace ring"
$VSSH "sysctl kern.atrium_trace.reset=1 >/dev/null && sysctl kern.atrium_trace.enable=1 >/dev/null"

echo "[2/7] Restart frescod-vulkan-smoke server with tracing on"
$VSSH "pkill -9 frescod-vulkan-smoke 2>/dev/null; \
       rm -f /tmp/frescod-smoke.sock /tmp/atrium-trace-guest.json* /tmp/frescod-smoke-frame-*.png; \
       FRESCOD_BUNDLES_ROOT=/mnt/host/bundles \
       ATRIUM_TRACE_FILE=/tmp/atrium-trace-guest.json \
       FRESCOD_SMOKE_NO_PNG=1 \
       nohup /mnt/host/frescod/target/aarch64-unknown-freebsd/release/frescod-vulkan-smoke \
         > /tmp/smoke.log 2>&1 &"
sleep 1

echo "[3/7] Drive ${FRAMES} frames through atrium-test-client"
# atrium-test-client drives one frame and holds the socket. To get N frames,
# we need a different driver — fall back to the bouncer for now (ignores frame
# count but renders ~30 fps until killed; we let it run for a fixed wall time).
WALL_SEC=$(( (FRAMES + 14) / 15 ))   # ~15 fps assumed, generous
$VSSH "timeout ${WALL_SEC} /mnt/host/atrium-test-client/target/aarch64-unknown-freebsd/release/atrium-rect-bouncer /tmp/frescod-smoke.sock >/dev/null 2>&1 || true"
sleep 1

echo "[4/7] Stop smoke server (flushes guest-side trace file)"
$VSSH "pkill -TERM frescod-vulkan-smoke 2>/dev/null; sleep 0.5; pkill -KILL frescod-vulkan-smoke 2>/dev/null || true"
sleep 1

echo "[5/7] Drain kmod trace ring + disable"
$VSSH "sysctl -n kern.atrium_trace.dump > /tmp/atrium-trace-kmod.dump; \
       sysctl kern.atrium_trace.enable=0 >/dev/null"
$VSSH "cp /tmp/atrium-trace-kmod.dump /mnt/host/$(basename $KMOD_DUMP).vm; \
       cp /tmp/atrium-trace-guest.json* /mnt/host/$OUT_DIR/ 2>/dev/null || true"
mv "/Users/girivs/src/bsd/$(basename $KMOD_DUMP).vm" "$KMOD_DUMP"

echo "[6/7] Collect host-side traces from this run"
# Host trace files are written by virgl_render_server (forked from QEMU)
# and named by ATRIUM_TRACE_FILE.<pid>. The user must have set
# ATRIUM_TRACE_FILE=/tmp/atrium-trace-host.json *before* launching QEMU
# with --venus; this script can't set it retroactively.
HOST_TRACES=( /tmp/atrium-trace-host.json.* )
if [[ ! -e "${HOST_TRACES[0]}" ]]; then
    echo "  WARNING: no host trace fragments found at /tmp/atrium-trace-host.json.*"
    echo "  Did you launch QEMU with ATRIUM_TRACE_FILE in env?"
    HOST_TRACES=()
else
    cp /tmp/atrium-trace-host.json.* "$OUT_DIR/"
fi

echo "[7/7] Merge fragments into Perfetto-viewable JSON"
GUEST_FRAGS=( "$OUT_DIR"/atrium-trace-guest.json.* )
ARGS=( "$MERGED" )
ARGS+=( --kmod-dump "$KMOD_DUMP" )
for f in "${GUEST_FRAGS[@]}" "${HOST_TRACES[@]}"; do
    [[ -e "$f" ]] && ARGS+=( "$f" )
done

python3 ~/src/bsd/atrium-trace/scripts/merge_traces.py "${ARGS[@]}"

echo
echo "merged trace: $MERGED"
echo
echo "Open in Perfetto UI:"
echo "  open https://ui.perfetto.dev"
echo "  drag $MERGED into the browser"
echo
echo "Or chrome://tracing → Load → $MERGED"
