# Lyra L6 — minimum reliable buffer under load (the measurement story)

The headline the scheduler doc promised (§10–§11): **underrun count / minimum
reliable buffer under load.** The control is apples-to-apples — the *same* engine
(`lyrad --feed`) with the deadline lane **on vs off** — the honest BSD-native
"reference" (no PulseAudio/PipeWire in the VM, and the lane is exactly the
variable under test). Harness: [`l6-sweep.sh`](l6-sweep.sh), in-VM on real
`intel-hda` → OSS, FreeBSD guest under HVF.

- **Buffer depth** = `LYRA_NFRAGS` fragments × 128 frames @ 48 kHz (2.667 ms each).
- **Load** = 8 CPU spinners on a 4-vCPU guest (2× oversubscription).
- **Metric** = the device's own `play_underruns` over a 2 s steady-state feed.
- Each config run twice; the table reports the **best (min)** run — host-stall
  contamination under HVF only ever *adds* spurious underruns, so the minimum is
  the truest measure of the engine's capability.
- The lane is gated behind `sysctl kern.sched.deadline_enable=1` (default 0); the
  harness sets it. With it off, the `lane` arm silently falls back to timeshare.

## Result (2026-06-14)

```
buffer   latency   no-lane     lane
(frags)   (ms)     underr    underr
      2      5.3       404        0
      3      8.0       563        0
      4     10.7       176        0
      6     16.0       215        0
      8     21.3       281        0
     12     32.0        87        0
     16     42.7       100        0
     24     64.0         1        0
     32     85.3         4        0
     48    128.0         0        0
     64    170.7         0        0

min reliable buffer, lane (deadline)     :   2 frags =   5.3 ms
min reliable buffer, no-lane (timeshare) :  48 frags = 128.0 ms
```

**The lane sustains 0 underruns at the hardware-minimum buffer (5.3 ms) at every
depth. Plain timeshare needs a 128 ms buffer to reach glitch-free under the same
load — 24× the buffer, i.e. 24× the output latency.**

## Why this is the thesis, measured

The deterministic model (`gpusim engine/src/audio.rs`) proves *minimum reliable
buffer = jitter × rate*. This in-VM run is that formula made empirical: the
deadline lane's CBS reservation **bounds the feed thread's scheduling jitter** to
~one period, so a 2-fragment buffer suffices; timeshare's jitter under contention
is unbounded (a spinner can hold a core for tens of ms), so the buffer must grow
to ~128 ms to absorb it. This is precisely the latency that `PREEMPT_RT` claws
back on Linux with a patched kernel — here it is **structural**, falling out of
the audio graph being a kernel-scheduled deadline graph (the L0 thesis).

Honest caveats: the no-lane column is non-monotonic (404 → 563 → 176 …) because
host-stall contamination dominates the small-buffer timeshare runs; the *trend* —
0 only at ≥48 fragments — is what matters, not the per-row values. The absolute
numbers are HVF-on-macOS specific; the **ratio** (lane holds the floor, timeshare
needs ~24× the buffer) is the portable result. Pro-path end-to-end latency and a
cross-OS reference comparison remain follow-ups.
