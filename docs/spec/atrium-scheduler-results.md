# Laminar on real hardware — the measurement story

Companion to the cost-composition whitepaper (`scratch/sched-sim/WHITEPAPER.md`,
the simulated timeshare mechanism) and the federation design
(`atrium-scheduler-federation.md`). That paper is scoped to the within-member
timeshare picker, validated in simulation, and explicitly defers the declared
**deadline lane**, real-hardware results, and any cross-scheduler comparison.
This document is exactly those: the **P7 measurement story** — the two headline
metrics the federation thesis stands or falls on, measured in-VM on real silicon,
plus the honest comparison.

The thesis in one line: **we changed what the scheduler is allowed to *know*.**
ULE and EEVDF are the best you can do when the kernel *cannot* know which threads
need latency — they guess (ULE's interactivity score, EEVDF's synthetic
deadlines). Atrium owns the compositor (Fresco), the audio engine (Lyra), the
manifests, and the jails, so the input→render→flip and source→mix→DAC chains
carry **real** deadlines anchored to hardware facts (a vblank timestamp, an audio
buffer period), declared by trusted brokers, never inferred. The measurements
below are what that buys.

## 1. The mechanism under test

The deadline lane (phase I, `sched_laminar.c`): a CBS-lite-over-EDF reservation
class sitting **above** timeshare and **below** POSIX RT, on the same per-CPU
runqueue machinery as the timeshare picker. A thread (or a broker on its behalf)
declares `(Q, T)`; admission caps per-CPU lane utilization at 75%
(`deadline_util_max`), leaving a 25% WFQ floor so the lane can never starve
timeshare; budget is charged at the switch boundary and a per-entity replenish
callout resets it each period, so an overrunning entity is throttled to timeshare
(overrun isolation — a liar cannot eat its neighbours' deadlines). `/dev/laminar`
is `root:wheel 0600`, so sponsorship is privileged; brokers reach it via a
Portcullis `deadline_broker` capability. Enabled by default
(`kern.sched.deadline_enable=1`).

The control in every measurement below is **apples-to-apples**: the *same* engine,
the *same* workload, the *same* load — lane on vs lane off. No second OS, no
different code path. The lane is the only variable.

## 2. Headline 1 — audio: minimum reliable buffer under load

`lyra/scripts/L6-RESULTS.md`. `lyrad --feed` plays a tone to real `intel-hda` →
OSS under N CPU spinners on a 4-vCPU guest; the device's own `play_underruns`
counter (kernel/hardware-counted, robust to host jitter) is read after a
steady-state run. Sweep the buffer depth (fragments × 128 frames @ 48 kHz) to
find the smallest buffer that sustains 0 underruns.

```
buffer    latency    no-lane    lane
   2       5.3 ms      404        0
   3       8.0 ms      563        0
  24      64.0 ms        1        0
  48     128.0 ms        0        0

min reliable buffer:  lane = 5.3 ms (hardware floor)   timeshare = 128 ms
```

**The lane holds 0 underruns at the 5.3 ms hardware-minimum buffer at every depth;
plain timeshare needs a 128 ms buffer to reach glitch-free under the same load —
24× the buffer, i.e. 24× the output latency.** This makes the model's
`min_reliable_buffer = jitter × rate` empirical: the CBS reservation bounds the
feed thread's scheduling jitter to ~one period; timeshare jitter under contention
is unbounded (a spinner can hold a core for tens of ms). The latency Linux needs
`PREEMPT_RT` plus privilege to claw back is **structural** here, and unprivileged.

## 3. Headline 2 — display: dropped frames under load

`tools/test/sched-laminar/bench/PACE-RESULTS.md` (`metronome` + `pace-sweep.sh`).
Same periodic workload at 60 Hz (a "frame" = `work_us` of real thread CPU time,
`CLOCK_THREAD_CPUTIME_ID` — calibration-free, so starvation deterministically
stretches the wall-clock and a frame misses its vblank). Lane misses are
kernel-counted (`LAMIOC_STATS`); the timeshare arm counts dropped vblanks in
userspace and resyncs to the grid. Sweep frame cost (the display analog of the
audio buffer-depth sweep — a 16.2 ms period has slack, so timeshare copes with
*light* frames and only janks once a frame's CPU cost approaches its fair share).

```
              8 spinners (2x)     16 spinners (4x)
frame cost    plain    lane       plain    lane
 2.0 ms        4%       3%         12%       1%
 4.0 ms        2%       2%         42%       0%
 6.0 ms       54%       0%         72%       0%
 8.0 ms       83%       0%         66%       0%
10.0 ms       83%       0%         88%       0%       (median dropped-frame %, of ~150)
```

**The lane drops 0% of frames at every depth and both loads; timeshare copes with
light frames then collapses — 42–88% dropped once a frame costs ≳ 4 ms under
load.** The lane *refuses admission* past the 75% duty cap rather than degrade
silently — it won't promise a frame it can't keep. This is a guarantee, not a
better average: a reserved frame thread is replenished every vblank regardless of
load; timeshare gives a sleeping render thread only its fair share, which drops
below a heavy frame's need exactly when the machine is busy.

## 4. Where the legs actually sit — the frame-pacing budget, measured

A frame meets its vblank only if **CPU compositor + GPU render** both fit in
16.7 ms. We measured each leg on real hardware to locate the bottleneck by
content type:

| Leg | Desktop content | Heavy 3D content |
|---|---|---|
| CPU compositor (Tier-2 SW) | 0.3 ms damage → 2–4 ms interactive → 25–187 core-ms full repaint | (app-side) |
| GPU render (M4 Max, MoltenVK) | **33–90 µs** | **~11 ms @720p** (Orbis, real) |
| Display | 16.7 ms vblank grid (the deadline) | 16.7 ms |

(CPU: `bench_tier2_tiled` / `bench_textured_glyph`. GPU: `bench_gpu_frametime`,
Vulkan timestamp queries — desktop render is *tens of microseconds*, the 0.4–0.6 ms
wall-clock is CPU submit/copy overhead. Heavy anchor: Orbis 3D, ~11 ms @720p.)

**The bottleneck flips by content:**
- **Desktop**: GPU is near-instant; the CPU compositor is the lever → the deadline
  lane (for the common 2–4 ms interactive frame) plus damage tracking (to keep
  most frames light). This is the regime the §3 measurement lives in.
- **Heavy 3D** (games, Orbis): GPU render at ~11 ms against a 16.7 ms budget has
  thin headroom → GPU render-timing is the lever. `gpusim framepace.rs` models
  this, now **anchored on the measured costs**: desktop never slips, Orbis @720p
  holds 60 fps, and only a ~2× slower target GPU slips — where a **render-timing-
  aware** compositor (`pace_frames_resync`) keeps the dropped-frame count *bounded*
  instead of cascading like a naive queue-ahead pacer (the GPU analog of §3's
  resync-vs-cascade). The model + anchors are ready; the real-silicon GPU-queue
  priority is gated on the native GPU driver (the host MoltenVK/Metal path does
  not expose it), not a design gap — the GPU is a first-class federation member
  (GPU `modulate-L`, the watt budget) by construction.

So three complementary legs — damage tracking, the deadline lane, GPU
render-timing — and the lane is necessary for the interactive middle regime (the
common case), not sufficient for the whole pipeline.

## 5. The honest comparison

**vs ULE / EEVDF (general-purpose timeshare).** Laminar is roughly ULE-class at
the job they are built for — and younger. On undeclared latency tails ULE wins
p99 (~25 µs vs a Laminar quantum plateau of 2.8–15 ms when there is no forced
wakeup-preempt); Laminar's symmetric bounded-lag clamp wins the *extreme* tail
~5×. "Competitive on undeclared tails" holds at p99.9/max, not p99 — and the
federation doc is explicit that this axis is *validation, not the selling point*.
The categorical difference is the lane: real hardware-anchored deadlines for the
media pipeline, unprivileged and admission-controlled, which neither ULE nor
EEVDF has natively (Linux's `SCHED_DEADLINE` is the nearest analog but has no
compositor/audio brokers and no cross-IPC deadline inheritance).

**vs Android.** The sharpest comparison, because Android also owns its stack and
has fought exactly these battles. Its answers map onto Laminar's: EAS ≈
energy-aware placement; uclamp ≈ DVFS boost; ADPF hint-sessions ≈ declared frame
deadlines (but *advisory to the governor*, not admission to an EDF class); the
`SCHED_FIFO` fast-mixer ≈ the audio lane (but *privileged*, no admission control,
capped only by the blunt global RT throttle); binder priority inheritance ≈ K-b
adoption (but inherits *priority*, where K-b inherits the *deadline* and charges
the server's CPU back to the client's budget). Android validates the bet — owning
the stack beats guessing — but accreted heuristics and advisory hints on top of a
fair scheduler. Atrium makes the real-time path a **structured first-class
construct**: an admission-controlled deadline lane, gated by capabilities, with
deadline+budget inheritance across IPC, under one energy budget. Android is the
benchmark to beat and the proof at scale Atrium does not yet have; Atrium's claim
is principle, not maturity.

## 6. Limitations

- **Absolute numbers are HVF-on-macOS / 4-vCPU specific.** The *ratios* (24×
  buffer; 0% vs 42–88% frames; the cliff shape) are the portable claims, not the
  exact figures. Every headline metric is kernel/device-counted, which the
  episodic host-stall window can only *inflate* — so min/median over reps recovers
  the true floor (the methodology that unblocked P7 without a perfectly clean
  timing rig).
- **The lane sponsors a thread.** It maps cleanly onto single-threaded paths (the
  audio feed thread; the SW text compositor, which is ~single-threaded). A rayon-
  parallel compositor would need the lane applied to its worker pool or critical
  path — unaddressed here.
- **Owed:** a cross-OS reference comparison (vs a real Linux/PREEMPT_RT VM) and
  pro-path end-to-end latency tails. Both are the validation axis, deferred by the
  federation doc's own pitch order (Atrium-native axis first).

## 7. References

- `scratch/sched-sim/WHITEPAPER.md` — the cost-composition timeshare mechanism (sim).
- `atrium-scheduler-federation.md` — the within/across-member design + the thesis.
- `atrium-scheduler-implementation-plan.md` — phases P0–P7, status.
- `lyra/scripts/L6-RESULTS.md` — audio underrun / min reliable buffer.
- `tools/test/sched-laminar/bench/PACE-RESULTS.md` — display frame pacing.
- `atrium-lyra-architecture.md` §7.1, §11 — the seat, and L6 in the audio phasing.
