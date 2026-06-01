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
- **P2 — Batched fragment execution.** Remove the per-pixel FS
  call.  Today `rasterize_stripe` makes one indirect call to
  `fs_main` *per covered pixel* (8.3M calls/frame at 4K full-screen);
  each call re-marshals pointers and the runtime sets up
  gl_FragCoord / descriptors per pixel.  P2 introduces a **span FS
  ABI** that shades a run of pixels in one call, with SoA inputs +
  a coverage mask.  Design + sub-phases:

  - **Span ABI (`FsSpanMain`).** New entry alongside `fs_main`
    (never replaces it — the per-pixel path stays as the universal
    fallback):
    ```
    fn fs_span(
      in_varyings_soa: *const u8,  // lane i at +i*varying_stride
      varying_stride:  u32,
      uniforms:        *const u8,
      push_constants:  *const u8,
      frag_x: *const f32, frag_y: *const f32,   // [lanes]
      frag_z: *const f32, frag_w: *const f32,   // [lanes]
      coverage_mask:   u64,        // bit i = lane i shaded
      samples_mask:    u32,        // shared (per-lane MSAA deferred)
      out_color_soa:   *mut f32,   // lane i colour at +i*4 f32
      out_depth:       *mut f32,   // [lanes]
      front_facing:    u32,        // shared per triangle
      primitive_id:    u32,        // shared per triangle
      lane_count:      u32,
    );
    ```
    Span = a contiguous horizontal run of covered pixels within one
    tile row (natural fit for the existing per-row pixel walk; lane
    count tunable, e.g. 8/16).
  - **P2.1 — ABI + loader.** Add `FsSpanMain` to `atrium-spv-loader`
    and an optional `fs_span_main` to `ShaderEntryPoints` (symbol
    `atrium_fs_main_span`, resolved if present, else `None`).  No
    behaviour change; everything still runs the per-pixel path.
  - **P2.2 (DONE) — cranelift span codegen.** `atrium_fs_main_span`
    is emitted by `emit_fragment_span`: a `header(lane) → maskcheck →
    body_entry → latch → header` loop wrapping the existing body
    codegen (walked by the shared `emit_body_blocks`).  The body's
    fragment I/O is made lane-relative via `FsAnchors` (varyings =
    `varyings_soa + lane*stride`, colour = `out_color_soa + lane*16`,
    depth = `out_depth + lane*4`; uniforms / push / front_facing /
    primitive_id shared); `Op::Return` jumps to the latch instead of
    returning (`span_latch`).  The scalar path is byte-unchanged
    (`fs_anchors`/`span_latch` are `None`).  Emission is **gated to a
    supported subset** — non-MRT, non-image-sampling, non-derivative
    fragment shaders — because texture/derivative codegen still reads
    the descriptor/helper table from a hardcoded scalar param index
    (`params[1]`); those shaders simply don't get a span symbol and
    keep the per-pixel path.  **Additive + safe:** nothing calls the
    span entry yet (P2.3), so this only had to emit *valid IR* —
    validated by a cache-cleared full smoke (every FS recompiled, 0
    compile panics, all rungs MM..HHH correct) + 106 differential.
    Textured/MRT/derivative span = follow-up (anchor those param
    reads).  The lane-indexed entry infrastructure is also what
    **P3b** reuses (swap scalar lane ops for SIMD lanes).
  - **P2.2b (DONE) — bespoke span codegen (the win that actually
    lands).** Pieces 1–2 landed + validated: `.afblob` format v2
    (`fs_span` entry slot) + jitmap resolution (efb933b); bespoke
    ARM64 call-per-lane thunk (78399a0); correctness test + Apple-ABI
    stack-arg fix (c294ebd).  `tests/span_thunk.rs` mmaps the blob
    and proves a multi-lane masked span call is bit-identical to
    per-lane `fs_main` (masked-off lanes untouched).  The test caught
    Apple's ARM64 stack-arg packing (u32s in 4-byte slots vs AAPCS64
    8-byte slots) — the thunk now picks offsets by `target`.  Only
    P2.3 (rasterizer calling `fs_span`) remains to realize the win.
    Original design notes below.
  - **P2.2b-design — bespoke span codegen (the win that actually lands).**
    The `bench_fs_span` probe (4K, constant-colour FS) measured the
    per-pixel `fs_main` call at **~93% of trivial-FS render time**
    (2.30 ms vs a 0.15 ms no-FS-call floor) — so batching the call
    is the dominant win.  BUT simple/compositor shaders compile to
    **bespoke** (`.afblob`), not cranelift, so the P2.2 cranelift
    span never reaches them.  Decision (locked): emit the span entry
    in the **bespoke** backend.  Three coordinated pieces:
    1. **`.afblob` format + loader.** Add an `fs_span` entry offset
       alongside `entries.{vs,fs,cs}` (bump the blob/ABI version so
       stale cached blobs recompile).  `jitmap.rs` resolves
       `fs_span_main` from it (currently hardcoded `None`).
    2. **bespoke `emit_fragment_span` (finalized design).** Reuse the
       *existing* `atrium_fs_main` body via a hand-emitted ARM64
       **running-pointer** thunk, appended to the same combined
       fragment body so the `bl` needs **no relocation**: fs_main is
       at body offset 0, the thunk at `fs_main_len`, so the `bl`'s
       imm26 = `-((fs_main_len + bl_local)/4)` — computable at emit
       time (`patch` it).  `compile_blob` sets `entries.fs =
       off`, `entries.fs_span = off + fs_main_len`.
       - **Reg allocation (all confirmed present in
         `pptk_codegen_arm64::asm`):** running pointers live in
         callee-saved `x19=lane, x20=lane_count, x21=varyings,
         x22=out_color, x23=out_depth, x24..x27=fx/fy/fz/fw,
         x28=mask`.  Shared args (uniforms, push, stride,
         samples_mask, ff, pid) spilled to the frame at entry and
         reloaded per active lane.  Frame = 144 B
         (`stp_x_pre`/`ldp_x_post` x29/x30 + x19..x28 + 48 B spill);
         incoming AAPCS64 stack args (mask@0, smask@8, out_color@16,
         out_depth@24, ff@32, pid@40, lane_count@48) read from
         `[sp+144+k]`.
       - **Loop:** `cmp x19,x20; b.ge end`; mask bit via
         `movz x10,#1; and_x x9,x28,x10; cbz_x x9, advance`; active →
         `mov x0,x21`, `ldr x1=uniforms`, `ldr x2=push`,
         `ldr w9,[x24]; fmov_s_from_w s0,w9` (×4 frag coords),
         `ldr w3=smask`, `mov x4,x22`, `mov x5,x23`, `ldr w6=ff`,
         `ldr w7=pid`, `bl fs_main`; `advance:` `add x21,+stride`
         (`add_x`), `add_imm_x x22,#16 / x23,#4 / x24..x27,#4`,
         `lsr_imm_x x28,#1`, `add_imm_x x19,#1`, `b loop`; `end:`
         epilogue + `ret`.  Three branches (`b.ge`/`cbz`/`b`) patched
         after layout like `emit_function`'s branch sites.
       - **Gate** to the same subset as the cranelift span (non-MRT,
         non-image, non-derivative); else emit no span entry.
       - `call-per-lane` amortizes the 12-arg-per-pixel marshalling +
         FFI crossing across the span (the bulk of the 93%); if the
         per-lane `bl`+prologue still dominates, escalate to an
         inlined-body loop (re-emit the body with `ret`→branch-back).
       - **Validation (required at landing):** a unit test in the
         bespoke crate that `mmap`s the blob, resolves both
         `fs_main` + `fs_main_span`, and asserts a 1-lane span call ==
         a single `fs_main` call (bit-identical), plus a multi-lane
         masked call — because the thunk is emitted-but-unused until
         P2.3, "compiles + smoke-green" is NOT sufficient to prove
         the machine code is correct.
    3. **Additive + safe to land before P2.3:** nothing calls the
       span until P2.3, so the bar is "all bespoke shaders still
       load + smoke green (span emitted-but-unused)"; the thunk's
       machine-code correctness is validated when P2.3 first calls
       it (single-lane mask == `fs_main` bit-identical).
  - **P2.3 — rasterizer span path.** In `rasterize_stripe`, for the
    per-row pixel walk, accumulate a run of covered pixels: gather
    SoA varyings (perspective-correct interp per lane), frag coords,
    and the coverage mask; call `fs_span` once; then run the
    existing per-lane depth/stencil/blend/write scatter.  Gate on
    `fs_span.is_some()` AND the fast-path conditions (no per-pixel
    derivatives quad dependency, no implicit-LOD per-pixel descriptor
    rewrite); else fall back to the per-pixel call.  Must be
    byte-identical to the scalar path for every rung.
  - **P2.3 (DONE, gated off) + P2.4 measurement.** Rasterizer span
    path wired end-to-end (e0aebeb): `fs_span` threaded
    build→`OwnedDraw`→`rasterize_pass`→`rasterize_stripe`;
    `rasterize_stripe_span` gathers a tile-row's covered pixels into
    SoA, calls `fs_span` once, scatters.  Full smoke MM..HHH green
    BOTH per-pixel AND `ATRIUM_TIER2_SPAN=1` (byte-identical).
    **But `bench_fs_span` (4K, const FS) measured the call-per-lane
    bespoke span at 3.00 ms vs 2.58 ms per-pixel — a ~17%
    REGRESSION** (the per-lane `bl` + `fs_main` prologue + SoA
    gather/scatter exceed the amortized FFI-crossing saving).  So
    the span path is correct + wired but **OFF by default**
    (`ATRIUM_TIER2_SPAN=1` opts in).
    - **Key datum:** the FS *call* is ~94% of trivial-FS cost
      (per-pixel 2.58 ms vs a 0.15 ms no-call floor).  The win is
      real but needs the **inlined-body** span — FS prologue/body
      run once per span with the lane loop INSIDE — not
      call-per-lane.  Routes: (a) upgrade the bespoke thunk to an
      inlined-body loop (`ret`→branch-back, re-emit body with
      lane-relative regs); (b) route span-eligible simple FS to
      cranelift, whose `emit_fragment_span` IS already an
      inlined-body loop (reuses P2.2; revisits bespoke-first); (c)
      P3a SIMD, which attacks the gather/coverage/blend that ALSO
      cost.
    - **Inlined-body measured (cranelift, d7445d0):** with the
      cranelift span exposed in the `.afblob` + the Apple call-conv
      fix, `bench_fs_span` gives **2.24 ms vs 2.59 ms per-pixel —
      1.16× (+14% of the FS-call headroom)**, vs call-per-lane's
      −17%.  So inlined-body is the right shape, but the win is
      MODEST for trivial fills (not the hypothesized ~5–8×): the
      per-pixel coverage/interp **gather** dominates the remaining
      ~2 ms, and that's untouched by the span — it's what **P3a
      SIMD** attacks.  Heavier (textured) FS would amortize the call
      more, but those are gated out of the span today.  Net: routing
      span-eligible simple FS to cranelift buys ~14% on opaque
      fills at the cost of cranelift's ~24× slower compile
      (one-time, cached); P3a is the bigger lever for the dominant
      gather cost.
  - **Correctness invariant:** `fs_span` over a mask of one lane must
    produce bit-identical output to `fs_main` for the same inputs;
    the span path is purely a call-overhead optimization, not a
    semantic change.
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
