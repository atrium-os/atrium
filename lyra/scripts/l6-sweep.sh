#!/bin/sh
# l6-sweep.sh — Lyra's L6 measurement story (runs IN the VM).
#
# The headline the scheduler doc promised: minimum reliable buffer / underrun
# count under load. The control is apples-to-apples — the SAME engine with the
# deadline lane ON vs OFF (no PulseAudio/PipeWire in the VM, and the lane is the
# variable under test). For each buffer depth (LYRA_NFRAGS fragments of 128
# frames = 2.667 ms each) under a fixed spinner load, report the device's own
# play_underruns. Each config runs a few times; the host takes the best (min)
# run — host-stall contamination under HVF only ever ADDS spurious underruns, so
# the minimum is the truest measure of the engine's capability.
#
# Robustness: every run is wall-clock-bounded (a starved no-lane feed thread
# stretches real time), and a trap SIGKILLs any stray lyrad on exit — the load
# spinners are forked copies of lyrad, so `pkill -9 lyrad` reaps them even if a
# client disconnects mid-sweep. Results are also teed to a host-shared file so a
# dropped connection never loses data.
#
# Emits/append `RESULT nfrags=<n> lane=<0|1> rep=<r> underruns=<u>` lines.
LYRAD=/mnt/host/lyra/target/aarch64-unknown-freebsd/release/lyrad
OUT=/mnt/host/lyra/l6-results.txt
SECS=${SECS:-2}
SPINNERS=${SPINNERS:-6}
REPS=${REPS:-2}
RUN_CAP=${RUN_CAP:-25}
NFRAGS_LIST=${NFRAGS_LIST:-"2 4 8 16 32 64"}

cleanup() { pkill -9 lyrad 2>/dev/null; }
trap cleanup EXIT HUP INT TERM

# setup (idempotent): the deadline lane is gated behind this RWTUN (default 0,
# reverts on reboot — put deadline_enable=1 in loader.conf to persist), and the
# HDA sound driver must be loaded for /dev/dsp. Without these the `lane` arm
# silently falls back to timeshare and there is no contrast to measure.
sysctl kern.sched.deadline_enable=1 >/dev/null 2>&1
kldload snd_hda 2>/dev/null || true

: > "$OUT"
hdr="L6 sweep: secs=$SECS spinners=$SPINNERS reps=$REPS cap=${RUN_CAP}s nfrags='$NFRAGS_LIST'"
echo "$hdr" | tee -a "$OUT"
for n in $NFRAGS_LIST; do
  for lane in 0 1; do
    arg=""; [ "$lane" = "1" ] && arg="lane"
    r=1
    while [ $r -le $REPS ]; do
      u=$(LYRA_NFRAGS=$n timeout -k 2 $RUN_CAP $LYRAD --feed $SECS $SPINNERS $arg 2>/dev/null \
            | sed -n 's/.*play_underruns=\([0-9]*\).*/\1/p')
      pkill -9 lyrad 2>/dev/null          # reap any orphaned spinner copies
      [ -z "$u" ] && u=timeout            # run exceeded the wall-clock cap
      echo "RESULT nfrags=$n lane=$lane rep=$r underruns=$u" | tee -a "$OUT"
      r=$((r+1))
    done
  done
done
echo "SWEEP_DONE" | tee -a "$OUT"
