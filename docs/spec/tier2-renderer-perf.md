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
  tile dispatch (only dirty tiles) from the start.
  - **P1a — prototype + measure (DONE).** `bench_tier2_tiled`:
    real triangles, band-binned, rayon-parallel, compiled FS per
    pixel, blending. **4K full-frame 7.24 ms (138 fps) — MEETS
    4K@120; damage frame (200×40) 0.097 ms (~75× cheaper).** No
    SIMD. Confirms the architecture + ceiling probe. (vs 152 ms
    per-primitive Tier-2; tiled tiny-skia reference 2.5 ms.)
  - **P1b — integrate into the daemon draw model**, keeping the
    78-rung smoke + `differential_compute` green. **Lower-risk
    approach:** *reuse `rasterize_stripe` unchanged* (it is already
    the per-(stripe,triangle) worker, so the exact pixel semantics
    the rungs depend on are preserved) and hoist the rayon dispatch
    from per-triangle to pass-level, looping triangles inside each
    stripe task. The measured killer was per-triangle rayon +
    per-triangle full-FB chunking — both vanish when stripes are
    built once and parallelised once.
    - **P1b.1 (DONE)** — split `fill_image_triangle` into
      `build_triangle_setups` (VS run + clip + setup, owned) and
      `rasterize_setups` (stripes split once, `par_iter` over
      stripes, each stripe loops its intersecting triangles calling
      `rasterize_stripe`).  `dispatch_draw`/`_indexed` build all of
      one draw's triangle setups and call `rasterize_setups` once
      (per-triangle → per-draw rayon).  Smoke + differential green.
    - **P1b.2 (DONE)** — lifted batching to per-PASS.  `execute_pass`
      owns a `(Vec<TriangleSetup>, Vec<OwnedDraw>)` accumulator;
      every triangle Draw/DrawIndexed appends its setups (stamped
      with a `draw_idx`) + a shared `OwnedDraw` snapshot of its
      fragment-side state (uniforms / blend / derivatives /
      sample_count + the texture-descriptor heaps kept alive for the
      pass).  `flush_triangle_batch` rasterizes the whole pass in ONE
      `rasterize_pass` dispatch at `EndRenderPass` (and before any
      non-triangle / compute op, to preserve submission order), each
      `TriangleSetup`'s `draw_idx` selecting its draw's `fs_main` +
      state.  Draw order preserved (setups looped in submission order
      within each stripe).  Mixed depth-enable across a batch is
      handled by baking `compare=Always, write=false` into
      depth-disabled draws' setups so a single pass-level depth
      buffer is shared safely.  Damage tile gating (scissor union) is
      automatic: `build_triangle_setups` already clamps each
      triangle's bbox to its `draw.scissor`, so the union
      `tile_min_y..tile_max_y` only spins up stripes overlapping the
      dirty rect.  Validated end-to-end by the loader smoke rungs
      **GGG** (loadOp=LOAD damage-preserve across two passes) and
      **HHH** (two different-shader draws in one pass routed by
      `draw_idx`); full smoke MM..HHH + 106 differential tests green.
    - **P1b.2 measurement** (`bench_tier2_passbatch`, 4K, 14 cores,
      full-screen-coverage quads, constant per-pixel work — the
      delta is pure dispatch overhead):

      | draws | per-PASS ms | per-DRAW ms | speedup |
      |------:|------------:|------------:|--------:|
      |    16 |        3.85 |        5.53 |    1.4× |
      |    64 |        3.56 |       13.02 |    3.7× |
      |   256 |        3.79 |       31.10 |    8.2× |
      |  1024 |        3.79 |       74.30 |   19.6× |
      |  4096 |        4.67 |      137.69 |   29.5× |

      per-PASS stays **flat ~3.6–4.7 ms (≈210–260 fps), under the
      8.33 ms 4K@120 budget at every draw count**; per-DRAW grows
      linearly and blows the budget by 64 draws.  A many-widget
      compositor frame is ~8–16 fps pre-P1b.2 but comfortably
      4K@120 with it.  **Target met on the integrated rasterization
      model for the compositor case, no SIMD yet** — P2/P3 are
      headroom to close the gap to the tiny-skia SIMD-blitter
      reference (~2.5 ms) for texture/blend-heavy frames.
- **P2 — Batched fragment execution.** Remove the per-pixel call
  (#2): span/quad FS ABI with SoA inputs + mask.
- **P3 — vectorization (#3), split two ways:**
  - **P3a — SIMD the rasterizer's fixed-function loops (Rust).**
    Coverage + blend + write + texture sample in hand-written Rust
    SIMD (`std::simd` / `wide`, NEON+SSE). **Backend-agnostic** —
    this is the dominant win for the *compositor* (2D shaders are
    trivial; the cost is fixed-function), independent of the
    bespoke/cranelift choice.
  - **P3b — SoA SIMD shader codegen (cranelift).** Lane-batched
    vector code for *per-app* heavy fragment shaders. Use
    **cranelift** (first-class vector types + ISel + vector
    regalloc, LLVM-free, JIT-class compile) — NOT bespoke:
    hand-rolling NEON/SSE for the full op set inverts bespoke's
    leanness into complexity. bespoke stays the lean scalar/compute
    AOT path; cranelift is the LLVM-free "middle" for vectorized
    shaders. (Compile latency is amortized: compositor shaders are
    fixed/compiled-once; app shaders are content-hashed in the
    shader cache.)
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

## Power / energy (measured — cpu-ms/frame = total core-time)

Same 4K UI frame, `getrusage` CPU-time (user+sys, all threads) as
a first-order energy proxy:

| full 4K repaint | wall ms | cpu-ms/frame (energy) |
|---|---|---|
| tiny-skia, 1 thread | 9.56 | 9.50 |
| tiny-skia, tiled (14c) | 2.19 | 24.51 |
| Tier-2 P1 tiled (14c) | 7.34 | 83.15 |
| Tier-2 P1, damage frame | 0.10 | 0.86 |

Takeaways:
- tiny-skia is **~8.8× more energy-efficient** than P1 Tier-2 for
  the same frame (9.5 vs 83 cpu-ms) — the gap is the per-pixel call
  + scalar shading, i.e. exactly what **P2 + P3 remove**. So
  **vectorization (P3) is Tier-2's energy lever, not just its
  throughput lever.**
- **Multicore has a power tax:** tiny-skia 1-thread→tiled is 9.5 →
  24.5 cpu-ms (2.6×) to buy the wall-time. ⇒ dispatch should be
  **core-count-aware**: use the fewest cores (ideally 1) that meet
  the frame deadline; never spread tiny/damage frames across all
  cores.
- **Damage dominates:** a damage frame is ~96× less energy than a
  full repaint. Biggest energy lever for either renderer.
- Power ≠ energy: a GPU draws more *watts* but far fewer *joules*
  per frame (fixed-function silicon vs brute-forcing on CPU cores)
  — so the native GPU path is the power-efficient target; SW is the
  no-GPU / bring-up fallback.

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
