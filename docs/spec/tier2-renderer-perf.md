# Tier-2 SW renderer — performance roadmap (native, vectorized, 4K@120)

## Goal

Evolve the Tier-2 software rasterizer from a correctness-first
scalar renderer into a **native, vectorized, multi-core SW Vulkan
renderer** good enough to be:

1. the **per-app** SW rendering path (apps via `atrium-vk-icd` →
   daemon → Tier-2, bridged into the compositor as textures by
   `atrium-tier2-fresco-bridge`), and
2. the **scene-graph compositor's** native SW renderer — replacing
   **lavapipe** (Mesa/LLVM) under `fresco-vulkan`'s
   `HeadlessRenderer`, honouring the BSD-native / no-LLVM charter.

Target: **4K (3840×2160) @ 120 fps = 8.33 ms/frame**, with typical
UI overdraw (2–4×) ⇒ ~2–4 Gpix/s of blended fill on CPU. This is
the regime lavapipe / llvmpipe / SwiftShader already operate in;
the plan adopts their proven architecture.

## Baseline (measured — `aqueduct-gpu-host/examples/`)

`bench_tinyskia_vs_tier2` (720p UI frame: bg + 24 panels + 3200
alpha glyph quads, 14 cores):

| Renderer | ms/frame |
|---|---|
| tiny-skia (SIMD blitter, 1 thread) | ~2.0 |
| Tier-2 (default, rayon) | ~152 (**74× slower**) |
| Tier-2 (1 thread) | ~56 (**28× slower**) |

`bench_4k_tiled` (reference: tiled tiny-skia hits 2.5 ms @ 4K with
spatial binning + rayon — i.e. the *architecture* below works; the
remaining task is to give Tier-2 that architecture + SIMD shading).

## Measured bottlenecks (in priority order)

1. **Per-primitive dispatch.** `fill_image_triangle` splits the
   *whole framebuffer* into stripes and spawns a rayon par-iter
   **per triangle**. Across thousands of small primitives this is
   pure overhead — and makes multicore *net-negative* (the 74→56 ms
   gain from disabling rayon proves it). This is the dominant cost.
2. **Per-pixel dlopened FS call.** A C-ABI `fs_main(...)` call per
   pixel (≈ 5× per pixel for derivative shaders), no inlining across
   the rasterizer boundary.
3. **Scalar shading.** cranelift compiles SPIR-V per-invocation
   scalar; no cross-pixel SIMD. (The deliberate "keep per-lane
   scalar" choice in the backend.)

> Vectorization (SIMD) alone — bottleneck #3 — cannot reach the
> target. #1 and #2 must be fixed first or SIMD shades into a
> dispatch-bound wall.

## Architecture (target = the llvmpipe/lavapipe model)

- **Tile-binned rasterization.** Defer a render pass's primitives
  into a list; bin them into fixed screen tiles (32×32 or 64×64).
- **Per-tile parallelism.** rayon over **tiles**, not primitives —
  one dispatch per frame. Each tile rasterizes its binned primitives
  into **tile-local** colour/depth buffers (fit L1/L2), then writes
  back once. Fixes #1; makes parallelism net-positive + cache-hot.
- **Batched fragment execution.** Shade a run/quad of pixels per FS
  entry (SoA inputs + coverage mask), not one call per pixel. Fixes
  #2.
- **SoA SIMD shader codegen.** Backend emits lane-batched vector code
  (f32x4 / f32x8 / NEON) so each invocation computes 4–8 pixels in
  vector registers; SIMD variants of the runtime helpers (texture
  sample, blend). Fixes #3. This is the "vectorize Tier-2" the
  charter wants — layered on top of tiling + batching.
- **Compositor fast paths.** Opaque overwrite (skip blend), solid
  memset fills, **damage tiles** (only re-rasterize dirty tiles),
  front-to-back opaque occlusion skip.

## Phases (each gated on a re-run of the 4K bench)

- **P0 — Baseline + evidence.** ✅ Benchmarks landed; bottlenecks
  quantified (this doc).
- **P1 — Tile-binned, per-tile-parallel rasterization.** No codegen
  change. Biggest single win (kills #1). Prototype + measure in the
  bench *before* integrating into the daemon's draw model.
- **P2 — Batched fragment execution.** Remove the per-pixel call
  (#2): span/quad FS ABI with SoA inputs + mask.
- **P3 — SoA SIMD shader codegen.** The real vectorization (#3):
  cranelift vector ISel over a lane batch + SIMD helper variants.
- **P4 — Compositor fast paths + integration.** Opaque/occlusion/
  damage; feature parity for `HeadlessRenderer` (indirect draw +
  whatever compute it issues); point `fresco-vulkan`'s ICD at
  `atrium-vk-icd` so the compositor runs on Tier-2. Apps are already
  bridged.

## Integration notes

- Apps: already wired — `atrium-vk-icd` → daemon → Tier-2 →
  `vkQueuePresentKHR` → `atrium-tier2-fresco-bridge` uploads the
  frame as a Fresco texture slot.
- Compositor: `fresco-vulkan` is `ash`-based and loads whatever ICD
  the loader resolves. Making the compositor run on Tier-2 = (a)
  perf (this roadmap) + (b) feature parity with what
  `HeadlessRenderer` issues, then point its `VK_DRIVER_FILES` /
  loader at `atrium-vk-icd` instead of lavapipe.

## Non-goals / guardrails

- Don't regress the 78-rung correctness suite or `differential_
  compute` while reworking the hot path — each phase re-runs them.
- Keep the BSD-native charter: pure-Rust, no LLVM/Mesa dependency in
  the Tier-2 path.
