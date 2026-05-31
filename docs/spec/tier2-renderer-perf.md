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

`bench_tier2_ceiling` (pre-investment go/no-go — runs the actual
cranelift-compiled FS per pixel + FB write over a 4K, 2× overdraw
workload):

| | ms/frame | Gpix/s |
|---|---|---|
| 1-core scalar FS | ~36 | 0.46 |
| 14-core scalar FS (**P1-only prediction**) | **~5.0** | 3.3 |

⇒ **P1 alone is predicted to clear 4K@120 (1.7× under budget)** for
solid-fill UI, *without* P2/P3. P2/P3 are headroom for texture/
blend-heavy frames and for arbitrary per-app shaders. (Solid FS is
the optimistic case; textured/glyph shading is ~2–3× heavier.)

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
- **P1 — Tile-binned, per-tile-parallel, DAMAGE-AWARE rasterization.**
  No codegen change. Biggest single win (kills #1) + damage-aware
  tile dispatch (only dirty tiles) from the start. Prototype +
  measure in the bench *before* integrating into the daemon's draw
  model.
- **P2 — Batched fragment execution.** Remove the per-pixel call
  (#2): span/quad FS ABI with SoA inputs + mask.
- **P3 — SoA SIMD shader codegen.** The real vectorization (#3):
  cranelift vector ISel over a lane batch + SIMD helper variants.
- **P4 — Compositor fast paths + integration.** Opaque/occlusion/
  damage; feature parity for `HeadlessRenderer` (indirect draw +
  whatever compute it issues); point `fresco-vulkan`'s ICD at
  `atrium-vk-icd` so the compositor runs on Tier-2. Apps are already
  bridged.
- **PT — Partial-update transport (in-app sub-rect damage).** Add
  `slot_update_region` / CAS patch + present damage rect to the
  Fresco protocol + bridge + compositor, so a small in-app dirty
  rect doesn't re-upload/recomposite the whole window surface.
  Independent of the rasterizer rework; lands alongside P4.

## Damage / dirty-rect rendering (the dominant real-world lever)

Fresco is scene-graph based with delta nodes + dirty rects, so the
renderer should "draw as little as possible."  This is bigger than
SIMD: with damage tracking, most frames touch a few hundred–few
thousand pixels (caret, hover, a typed glyph), so the 4K full-screen
repaint is the *rare* case (resize, wallpaper).  Tiling + damage
compose perfectly — work scales with **damage area, not screen
area** (only dispatch tiles intersecting a dirty rect).

Tier-2 already has the primitives:
- **scissor** (`DrawTriangle.scissor`) — clip rasterization to a rect.
- **preserve / no-clear** — `BeginRenderPass`'s `BEGIN_RP_FLAG_NO_
  CLEAR` keeps the prior framebuffer (= `VK_ATTACHMENT_LOAD_OP_LOAD`).
- **persistent target** — images live across frames in the daemon.

Two layers, handled differently:

1. **Per-window / per-app (compositor):** the compositor recomposites
   only the screen regions of changed/moved windows (LOAD-preserve +
   scissor + damage-aware tile dispatch).  End-to-end today; the
   compositor already produces the deltas.  → folded into **P1/P4**.
2. **In-app sub-rect (within a window's surface):** the app can
   LOAD-preserve its surface + scissor + redraw a sub-rect (Tier-2
   supports it) — but the *transport* is currently whole-surface:
   `atrium-tier2-fresco-bridge` does `upload_blob(whole)` +
   `slot_set_texture(whole)` + `window_present(window_id)` with no
   damage region.  So in-app damage saves render but not upload/
   recomposite (bites for tiny damage in a large window).  Closing
   it needs a **partial texture update** (`slot_update_region(slot,
   sub_rect, bytes)` / CAS patch) + a **present damage rect**
   (`window_present(window_id, damage)`).  This is a protocol +
   bridge + compositor item, independent of the rasterizer rework.
   → tracked as **PT (partial-update transport)**.

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
