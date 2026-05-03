# Fresco runtime benchmark — CPU-driven vs GPU-driven scenegraph

## 0. The question we're answering

Does a **GPU-driven scenegraph runtime** beat the conventional
**CPU-driven engine + per-frame GPU dispatch** model on real
hardware, for workloads representative of an Atrium-style
desktop?

The answer determines a load-bearing architectural decision in
`docs/spec/fresco-rendering-stack.md`: where does the engine
extension layer live?

- If **GPU-driven wins** (or ties), the Fresco engine extensions
  should be designed as GPU-resident runtimes (persistent
  megakernels / indirect command buffers / work-graph-style
  dispatch). The host shim becomes thin; per-frame CPU↔GPU
  traffic collapses; the same engine code maps cleanly onto
  future scenegraph-native silicon.
- If **CPU-driven wins decisively**, we go with the conventional
  layered architecture (host-CPU engine extensions calling Vulkan/
  Metal per frame). The "engine on GPU" becomes a future
  optimization, not the core design.

Either result is useful. We don't know yet which way it goes.

## 1. Platform

**macOS host with Metal.** Reasons:

- Apple Silicon has all the relevant primitives first-class: indirect
  command buffers (ICBs), argument buffers, persistent compute via
  long-running command buffers, tile shading, GPU-driven indirect
  dispatch.
- Native Metal avoids the MoltenVK translation layer obscuring
  what we're measuring.
- Profiling is excellent (Xcode GPU Frame Capture, Instruments,
  `MTLCounterSet` for cycle/bandwidth counters).
- We're on this hardware right now; no VM round-trip needed for
  iteration.

**Language: Swift.** Cleanest Metal binding; fast iteration; no
need for FFI gymnastics. Don't use Rust+metal-rs for this — adds
friction without any benefit for a self-contained benchmark.

## 2. What we are testing — and what we are not

**Testing:** does moving scenegraph traversal off the CPU win?
That's the *core* hypothesis behind the GPU-runtime architecture.

**NOT testing:**

- Persistent compute kernels that never return. (Bigger
  research project. If this benchmark shows GPU-driven wins, a
  follow-on benchmark adds persistence.)
- Cross-vendor portability. (Metal-only here; Vulkan generalization
  is a separate concern.)
- Real shader complexity. (Solid-color rectangles are sufficient
  to exercise the dispatch path. Adding texture/glyph/path
  rendering is independent of where the runtime lives.)
- Production-quality code. (This is a measurement tool, not
  shipping software.)

## 3. The two implementations to compare

Both paths render the **same visual output** for the same scene
state. They differ only in *where the scene traversal happens.*

### Path A — CPU-driven (conventional)

Per frame, on the host CPU:

1. Walk the scenegraph (list of N apps × M rectangles each).
2. For each rectangle, compute its current screen-space transform.
3. Encode a `MTLRenderCommandEncoder` draw call per rectangle
   (or batched into a single instanced draw if the implementation
   supports it — both batched and unbatched should be measured).
4. Commit the command buffer; present.

This is the "naive Vulkan dispatch" model in Metal terms. It's
what every conventional engine does today — Wayland compositors,
games, desktop apps — translated to Metal.

### Path B — GPU-driven

Setup (one time):
- Allocate a Metal buffer holding the entire scene (N × M
  rectangle records). Host-visible, GPU-accessible.
- Allocate an indirect command buffer (ICB) sized for N × M draws.
- Compile a compute kernel `traverse_scene(scene, icb, frame_state)`.

Per frame:
1. Host writes deltas to the scene buffer (only the
   rectangles that changed). Submits ONE compute dispatch per
   frame (`traverse_scene`) which:
   - Walks the scene in parallel (one thread per rectangle, or
     per group)
   - Computes per-rect transforms
   - Encodes draw commands into the ICB
2. Host commits a render pass that executes the ICB.
3. Host presents the drawable.

The CPU does almost nothing per frame: write deltas, dispatch
compute, dispatch ICB, present. Scene traversal happens entirely
on the GPU.

### Optional Path C — fully persistent (stretch goal)

If A vs B is conclusive, optionally measure Path C:

- A long-running compute kernel polls a "frame ready" semaphore
  in shared memory.
- Host writes scene deltas + sets the semaphore.
- The compute kernel runs traversal + ICB encoding + signals
  completion.
- Render pass executes the ICB.
- No per-frame compute dispatch from the host — the GPU is
  always running.

This is the "megakernel" model. Real production-grade
implementations use this. It's harder to write and debug, so we
defer unless A vs B already shows GPU-driven is the way.

## 4. Workload

A scene of **N apps × M rectangles per app** = N×M total
rectangles. Each rectangle has:

- 2D position (animated)
- size (static)
- color (static, randomized per rectangle at scene init)
- z-order / app index (for compositing)

Per frame, **K% of rectangles change position** (simple random
walk: each animated rect moves by ±1 pixel in x and y). The
remaining 100-K% are static.

This is representative of an Atrium-style desktop workload:
many windows (apps), each containing several visual elements
(buttons, panels, text-as-glyph-quads), most of which don't
change every frame.

### Test matrix

| dimension | sweep |
|---|---|
| N (apps) | 1, 10, 50, 100, 500 |
| M (rects per app) | 10, 100, 1000 |
| K (% animated) | 1, 10, 50, 100 |

That's 5×3×4 = 60 cells per implementation. Both Paths A and B
must be benchmarked across the full matrix. (Path A in both
batched and unbatched flavors → 120 measurements per matrix. If
that's too many, drop K=1 and K=50, keep 1 and 100.)

Render target: 1920×1080 offscreen MTLTexture (no display
present), to avoid vsync coupling. Present-to-display is a
separate measurement (one configuration with vsync on).

Frame budget: run each cell for **2000 frames** after a 500-frame
warmup. That's enough to stabilize caches and get statistically
useful percentiles.

## 5. Measurements

For each cell of the matrix, both Path A and Path B, capture:

- **Frame time** — wall-clock, p50 / p95 / p99 / max in
  microseconds. Measured via `CFAbsoluteTimeGetCurrent()` between
  presents. Steady state only (post-warmup).
- **Host CPU time per frame** — the actual CPU time spent in the
  rendering loop, not wall-clock. Use `mach_absolute_time()`
  bracketed around the per-frame CPU work. **This is the
  load-bearing metric** — the GPU-driven path's whole pitch is
  "host CPU does almost nothing."
- **GPU time per frame** — via `MTLCounterSampleBuffer`, sample at
  start/end of the per-frame GPU work.
- **GPU utilization %** — via Instruments / `MTLCounterSet`
  performance counters. (At least sample with Instruments
  separately if programmatic capture is too involved.)
- **Memory bandwidth** — read and written bytes per frame from
  GPU-side counters. Useful for understanding scaling behavior.
- **Energy if available** — `powermetrics` or Instruments Energy
  Log, sampled across 30s of steady-state running. (M-series GPUs
  expose package power; this is a real metric for the laptop
  battery argument.)

### Output format

One JSON file per benchmark run:

```json
{
  "machine": "MacBook Pro M2",
  "macos": "14.5",
  "metal_version": "3.1",
  "git_commit": "abcdef0",
  "started_at": "2026-05-04T12:34:56Z",
  "cells": [
    {
      "path": "A_unbatched",
      "n_apps": 50,
      "m_rects": 100,
      "k_pct_animated": 10,
      "warmup_frames": 500,
      "measured_frames": 2000,
      "frame_time_us": { "p50": 1234, "p95": 1500, "p99": 1800, "max": 5000 },
      "host_cpu_us":   { "p50": 800,  "p95": 950,  "p99": 1100, "max": 3000 },
      "gpu_time_us":   { "p50": 400,  "p95": 500,  "p99": 600,  "max": 1200 },
      "gpu_util_pct":  43.2,
      "mem_read_mb_per_frame":  12.5,
      "mem_write_mb_per_frame": 8.0,
      "package_power_w_steady": 5.4
    },
    ...
  ]
}
```

## 6. Evaluation criteria

After collecting the matrix, produce a short report (2-3 pages)
with these answers:

1. **At what point on the (N, M, K) axes does Path B beat Path
   A?** Likely at high N (many apps), high K (much animation).
   May lose at low N (CPU dispatch is fast for trivial scenes).
2. **What does the host CPU axis look like?** This is the
   strongest argument for GPU-driven — even if frame times are
   comparable, freeing the host CPU is a real win for a laptop
   OS where battery and thermals matter.
3. **Is GPU-driven actually faster, or just shifts work?** Look at
   total system time (host + GPU) — if GPU does the same work
   slower than CPU would have, the architectural argument
   weakens.
4. **What's the energy story?** If GPU-driven uses 30% more
   package power for the same frame rate, the laptop angle
   reverses.
5. **Cross-cutting recommendation:** GO / NO-GO / CONDITIONAL.
   - GO: GPU-driven wins or ties on most cells AND has CPU/energy
     advantage. Architect Fresco engine extensions as GPU
     runtimes.
   - NO-GO: GPU-driven loses materially. Use the conventional
     layered architecture from the rendering-stack spec; revisit
     when GPU APIs (work graphs, etc.) mature.
   - CONDITIONAL: wins for some workload classes only. Hybrid
     architecture: certain ops (high-fanout primitive draws) go
     GPU-driven; others stay CPU-driven.

## 7. Engineering tasks

In priority order. Each step should be its own commit.

1. **Skeleton + boilerplate.** Swift project that opens a Metal
   device, allocates a 1920×1080 offscreen texture, runs an empty
   loop. Just to validate the Metal setup.
2. **Workload generator.** Generate the N×M scene with reproducible
   randomness (seeded RNG). Implement the per-frame "advance
   animation" step in pure Swift (data structure work; no Metal yet).
3. **Path A (unbatched).** Per-frame CPU traversal + per-rect
   `drawPrimitives` calls. First implementation; the slowest
   reasonable baseline.
4. **Path A (batched/instanced).** Same scene, single instanced
   draw with per-instance buffer. The fast-CPU baseline.
5. **Path B (GPU-driven).** Compute kernel that walks the scene
   buffer + writes ICB + executes it. The hypothesis we're testing.
6. **Measurement harness.** Wrap all three with the metrics
   collector + JSON output. Make sure GPU timestamps are calibrated.
7. **Sweep runner.** Iterate the (N, M, K) matrix; write one
   JSON per cell + a combined results file.
8. **Analysis script.** Python or Swift that ingests the combined
   results and produces:
   - A heatmap of "Path B speedup over Path A batched" across the
     matrix (color-coded: green = B wins, red = A wins).
   - Line plots for N=50, M=100 sweeping K (the realistic-desktop
     midpoint).
   - The 5-question report from §6.
9. **(Optional) Path C — persistent megakernel.** Only if §6
   results are conclusively in favor of GPU-driven. This is the
   research stretch goal.

## 8. Out of scope

- Texture / glyph / path / 3D primitive rendering. Solid rectangles
  exercise the dispatch path; the result generalizes.
- Multi-GPU.
- Power/thermal recovery testing. Steady-state numbers only.
- Comparing against existing toolkits (SwiftUI, AppKit, Wayland).
  We're measuring our own architecture against itself.
- Production polish. This is a measurement tool. Treat the code
  as throwaway — clarity beats cleverness.

## 9. Where the code lives

A new directory `~/src/bench-fresco-runtime/` (eventually a repo
of its own under atrium-os). Self-contained; no dependency on the
atrium-os/atrium repo. Results JSON committed alongside the code
for reproducibility.

## 10. Deliverable

When complete:

1. Benchmark code that reproduces the full matrix in <30 minutes
   on the host machine.
2. A `results-<timestamp>.json` file with all measurements.
3. A `REPORT.md` with the analysis and the GO / NO-GO / CONDITIONAL
   recommendation.
4. The `analysis.py` (or equivalent) script for re-running the
   analysis on different result sets.

The recommendation in REPORT.md is what feeds back into
`docs/spec/fresco-rendering-stack.md` as the architectural
decision.
