# Fresco runtime benchmark v2 — host vs GPU scenegraph traversal

## 0. Why v2

The v1 benchmark (`fresco-runtime-benchmark.md`) returned NO-GO: a
GPU-driven path that encodes one ICB `render_command` per leaf rectangle
loses ~3.2× in aggregate frame cost to a CPU-driven instanced draw on
M4 Max.

That answer is correct *for the question v1 actually asked*. But v1 asked
the wrong question for Atrium. Both v1 paths were given a flat,
pre-resolved list of rectangles with absolute screen positions. There
was no scenegraph traversal in either path; we were only measuring
"once you have the answer, who redraws faster?" Of course an instanced
draw won — there was nothing left to compute.

The Atrium scenegraph is **hierarchical, content-addressed, and
declarative over time.** Per-frame work is dominated by:

- Walking the hierarchy and composing parent × child transforms.
- Evaluating animations parameterised by frame counter
  (`θ(F) = θ₀ + F·ω`) for thousands of nodes.
- Clip-rect culling at every level.

That work scales with node count and is embarrassingly parallel.
The architectural question is whether the host CPU should do it serially
(or with SIMD) or whether the GPU should do it in a compute pass. v1
did not test this; v2 does.

## 1. The question we are answering

**Does GPU-side scenegraph traversal + transform composition beat
host-side traversal, when both feed the same indirect-instanced-draw
render pass?**

Both paths share the same drawing primitive (the verdict from v1:
indirect-instanced draws are the right way to put pixels on screen).
What differs is *where the per-frame traversal of the CAS scenegraph
runs.*

- **GO** — GPU traversal wins on most cells AND has CPU/energy
  advantage. Architect Fresco engine extensions as GPU-resident
  scenegraph evaluators.
- **NO-GO** — Host traversal wins materially. CAS scenegraph stays
  host-side; GPU's role is purely the indirect-instanced raster pass.
- **CONDITIONAL** — Wins by hierarchy depth or animation density.
  Hybrid: lift a defined subset of traversal work to GPU.

## 2. What is in scope, what is not

**In scope:**

- Hierarchical scenegraph: parent → child transform composition.
- Declarative animation: each node's transform is a closed-form
  function of frame counter F (rotation, scroll, scale, ease).
- CAS-style mesh/material deduplication: the meshes themselves live
  in a GPU buffer once and never re-upload.
- Clip-rect culling at each hierarchy level.
- Indirect-instanced-draw render pass shared by both paths (verdict
  from v1).

**Not in scope:**

- ICB-per-leaf encoding (v1 already disqualified it).
- Persistent megakernels (orthogonal; revisit only if v2 is
  inconclusive).
- Texture / glyph / path rendering (still solid-coloured quads;
  the dispatch/traversal axis is what's under test).
- Cross-vendor portability (Metal-only).
- Production polish.

## 3. The two implementations to compare

Both paths render the same visual output for the same scene state and
share the **same render pass** — a small set of `drawPrimitives(...
indirect:)` calls reading instance buffers and instance counts written
by whichever path produced them. They differ only in where the
traversal runs.

Workload assumption: at startup we upload the static scene structure
(parent links, mesh refs, animation parameters) once into a GPU buffer.
Per frame, only the frame counter `F` (or equivalent time delta)
changes from the host's point of view.

### Path H — host-driven traversal

Per frame, on the host CPU:

1. Walk the scenegraph (depth-first, accumulating parent transform).
2. For each node: evaluate `transform(F)`, compose with parent,
   apply clip culling, append the resulting model matrix + colour to
   the per-batch instance buffer (`MTLBuffer`, `.storageModeShared`).
3. Write instance counts into the indirect-arg buffer.
4. Encode one render pass that issues `drawPrimitives(...indirect:)`
   per batch.

Host work scales O(nodes). Multi-threading via GCD is allowed but
must be measured (single-threaded and 4-thread variants). This is
the "conventional engine" path with the v1-verdict draw call.

### Path G — GPU-driven traversal

Per frame, on the host CPU:

1. Write `F` to a tiny shared buffer (16 bytes).
2. Dispatch one compute pass `traverse_scene(scene, F, instance_bufs,
   instance_counts)` — one thread per scenegraph node, hierarchy
   composed via either (a) pre-flattened parent-prefix arrays
   computed at upload time, or (b) iterative parent-pointer walk.
3. Dispatch the same indirect-instanced render pass as Path H.

Host per-frame work collapses to ~tens of bytes. All traversal,
transform composition, animation evaluation, and culling happens on
the GPU.

## 4. Workload

A scenegraph of:

- **D** — hierarchy depth (1 = flat, 4 = window→panel→widget→glyph,
  8 = pathological).
- **B** — branching factor per level.
- **N** — total leaf node count = roughly `B^D` (pick branching to hit
  the target N).
- **A** — fraction of nodes with non-trivial animation
  (rotation / scroll / scale parameterised by F).
- **C** — fraction of nodes culled by clip-rects per frame (interior
  test against parent's clip).

Each leaf is a solid-coloured quad with a 4×4 transform applied. Mesh
geometry (the unit quad) is the same CAS-resident buffer for every
leaf — meshes do not re-upload.

### Test matrix

| dimension | sweep |
|---|---|
| D (depth) | 1, 4, 8 |
| N (leaves) | 100, 1k, 10k, 100k |
| A (% animated) | 0, 10, 100 |
| C (% culled) | 0, 50 |

= 3 × 4 × 3 × 2 = 72 cells per implementation.

Path H is measured single-threaded and with 4-thread GCD =
2 H variants + 1 G variant = 216 measurements. Drop C=50 if budget
forces it (cull behaviour is informative but secondary).

Render target: 1920×1080 offscreen, no display present (vsync-decoupled).
500 warmup frames + 2000 measured per cell, same as v1.

## 5. Measurements

Per cell, capture the same metrics v1 captured (frame time / host CPU /
GPU time / memory bandwidth). Add:

- **Per-frame host bytes uploaded** — on Path G this should be O(16)
  bytes; on Path H it is O(N × instance_size). The compression ratio
  is itself a result.
- **Compute kernel time** (Path G only) — GPU-side traversal cost in
  isolation. Sample via counter sample buffer at compute pass
  boundaries.

Output JSON shape: same as v1's `cells` array, with a `path` value
of `"H_serial" | "H_gcd4" | "G"` and the new fields above.

## 6. Evaluation criteria

After collecting the matrix, the report must answer:

1. **At what (D, N, A) does G beat H_serial? H_gcd4?**
   Likely G wins as D grows (deeper hierarchy = more traversal per
   leaf) and as A grows (more matrices to derive). Likely loses at
   D=1 (no hierarchy = host trivially fast).
2. **What does the host CPU axis look like?** This is the strongest
   architectural argument. If G holds host CPU < 100 µs/frame across
   the matrix while H_gcd4 saturates 4 threads at large N, the laptop /
   battery / responsiveness story writes itself.
3. **Per-frame upload bytes — H vs G ratio.** A 1000× reduction in
   host→GPU traffic at large N is meaningful even if frame times
   tie, because it frees the memory bus for actual content.
4. **Energy.** Same caveat as v1; collect via `powermetrics` if
   feasible.
5. **Recommendation.** GO / NO-GO / CONDITIONAL with the cell
   regions identified.

## 7. Engineering tasks

In priority order. One commit each.

1. **CAS scene format.** Define the on-GPU scene buffer layout:
   parent index, local-transform parameters (animation kind +
   `θ₀, ω, ...`), mesh ref, material ref, clip-rect-id. Static after
   upload.
2. **Workload generator.** Build (D, N, A, C) scenegraphs with
   reproducible randomness. Two outputs: a host-side tree (for Path H)
   and the flattened GPU buffer (for Path G), guaranteed equivalent.
3. **Shared render pass.** The indirect-instanced raster pass that
   both paths feed. Bind one PSO; multiple draw-indirect calls per
   batch class. Verify it produces correct pixels from a manually
   filled instance buffer.
4. **Path H (serial).** Single-threaded recursive traversal +
   transform composition + cull + instance-buffer writes. Slow
   baseline.
5. **Path H (gcd4).** Same algorithm, parallelised across 4 worker
   threads via GCD. Per-thread instance-buffer regions then merged.
   The realistic CPU-driven baseline.
6. **Path G.** Compute kernel that traverses the scene buffer, derives
   transforms from F, applies clip-cull, and writes per-batch instance
   buffers + counts. Hierarchy: pre-flattened parent-prefix or
   iterative parent walk — pick whichever measures better.
7. **Harness + sweep.** Reuse v1 harness skeleton with the new
   metrics; emit a single combined results JSON.
8. **Analysis + REPORT.md.** Heatmap of "G speedup over H_gcd4"
   across (D, N, A); line plots at the realistic-desktop midpoint
   (D=4, N=10k); the 5-question report.

## 8. Where the code lives

A new directory `~/src/bench-fresco-runtime-v2/`. Self-contained.
Reuse only ideas from v1, not code — v1's renderer is structurally
wrong for v2 and copying it forward would just propagate confusion.

## 9. Deliverable

1. Benchmark code that reproduces the matrix in <60 minutes on the
   host. (Larger budget than v1 because the hierarchy traversal
   matrix is bigger.)
2. `results-<timestamp>.json` with all measurements.
3. `REPORT.md` with the GO / NO-GO / CONDITIONAL recommendation,
   feeding back into `docs/spec/fresco-rendering-stack.md`.

## 10. What v1 told us that we are keeping

- **Per-leaf ICB `render_command` encoding does not work.** Both v2
  paths skip it and use indirect-instanced draws.
- **UMA storage modes are correctly understood.** Scene + instance
  buffers `.storageModeShared`; render targets and ICBs (if any)
  `.storageModePrivate`.
- **Auto-skip protocol for over-budget cells** is reusable.
- **Runtime shader compilation via `device.makeLibrary(source:)`**
  is the right approach for a throwaway benchmark.

## 11. Open questions to resolve during build

- Hierarchy walk on GPU: pre-flattened parent-prefix arrays vs
  iterative parent-pointer walk vs Apple's experimental work-graph
  primitives — measure all three if implementation cost is low.
- Should clip-cull live in the compute pass or as a vertex-shader
  early-discard? Probably former (avoids wavefront waste), but
  worth a measurement.
- Is GCD overhead at 4 threads actually a fair host baseline for an
  Atrium compositor? An Atrium compositor likely has 1 render
  thread; H_gcd4 may flatter the host case. Report both H_serial
  and H_gcd4 and let the architectural decision pick.
