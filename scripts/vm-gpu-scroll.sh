#!/bin/sh
# Live scroll check for frescod's renderers, in the amd64 gpusim VM
# (cloud-vm.sh up with GPUSIM=1, kernel.atrium, GPU kmods loaded).
#
# Runs IN THE GUEST from a directory holding live-frescod (frescod) and
# atrium-scroll-demo:
#   sh vm-gpu-scroll.sh RENDERER STEP FRAMES TAG
# (over ssh, give it </dev/null >log: its daemons keep ssh's stdio, so the
# ssh call would not return while frescod runs).
# frescod runs unjailed with split mode off on /tmp/live.sock; the demo
# scrolls STEP px per frame for FRAMES frames at 2 fps, then holds. The host
# then screendumps gpusim0 (QEMU monitor: screendump FILE gpusim0 0) and
# compares renderers: a whole-pixel STEP (7) makes the gpu renderer shift,
# a fractional one (6.5) redraws the viewport. Both must equal the compute
# renderer's screen bit for bit (2026-11-11: they do, offsets -84 and -78).
#
# gpusim frames take seconds: raise the driver's hang watchdog first, or the
# first full compose is killed at 5 s and frescod exits with EIO:
#   sysctl dev.atrium_gpu_amd.0.isolation.hang_ms=600000
# The gpu renderer compiles its kernels at start (minutes under TCG) before
# it listens. GPUSIM_RDNA4_STATS=1 on the host logs each dispatch.
R=$1; STEP=$2; N=$3; TAG=$4
cd "$(dirname "$0")"
pkill -f live-frescod; pkill -f atrium-scroll-demo; sleep 1
rm -f /tmp/live.sock /tmp/live-wm.sock
env RUST_LOG=info FRESCOD_SPLIT=0 FRESCOD_RENDERER=$R FRESCOD_FLIP_TIMEOUT_MS=900000 FRESCOD_SOCK=/tmp/live.sock FRESCOD_WM_SOCK=/tmp/live-wm.sock FRESCOD_ACTIVATION_SOCK=/tmp/live-activation.sock \
  daemon -o /tmp/live-$TAG.log ./live-frescod
i=0; while [ ! -S /tmp/live.sock ] && [ $i -lt 1800 ]; do sleep 1; i=$((i+1)); done
[ -S /tmp/live.sock ] || { echo "frescod did not listen"; exit 1; }
daemon -o /tmp/scroll-$TAG.log ./atrium-scroll-demo /tmp/live.sock 100000 $STEP 2 $N
echo started
