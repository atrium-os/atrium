# Tier-2 Renderer — AOT software Vulkan for Atrium

> **Status.** Design v2. No code yet.
>
> **Companion docs.** Read `aqueduct-gpu.md` (the GPU dispatch
> protocol) and `atrium-pkg.md` (Atrium's bundle / install
> story) first. This spec defines the second renderer tier
> on the daemon side of aqueduct-gpu, and the AOT shader-
> compile pipeline that feeds it.
>
> **One-line summary.** Tier-2 is the software renderer for
> third-party Vulkan apps whose shaders aren't part of
> Atrium's built-in bundle catalog. SPIR-V is translated to
> a small SSA IR (`atrium-spv-ir`) and then compiled to
> native code by *two* backends: a bespoke ARM64/x86_64
> backend (leveraging the encoder from the `pptk` project)
> for performance, plus a Cranelift backend as a graceful-
> degradation fallback. A third reference path — a SPIR-V
> interpreter — exists only in the test harness as the
> differential-test oracle. All compilation happens in a
> jailed sub-process *before* the daemon dlopens the
> resulting `.so`. The daemon process never runs a compiler
> in its hot loop.

---

## 1. Why we're doing this

Tier-1 (`aqueduct-gpu-host/src/software/`, the tiny-skia
backend) handles Atrium-native bundle pipelines: rect,
path, textured-rect, glyph_run. That covers every drawable
in fresco-server's scene graph and every renderer the
in-tree Atrium apps need. When atrium-vk-icd routes a
third-party SPIR-V pipeline at it, tier-1 returns:

```
WARN aqueduct_gpu_host::backend: SoftwareBackend::submit_frame:
  pass into id(icd-runtime, 0x3) failed:
  tier-1 software renderer cannot handle ICD-runtime pipelines
  (third-party SPIR-V) — tier-2 territory
```

That structured rejection is the correct response — tier-1's
job is *Atrium's* shaders, and widening it would compromise
its main-loop simplicity. Tier-2 exists to take the rejection
path and turn it into pixels.

Three constraints shape the design:

1. **No JIT or interpreter in the renderer's hot path.**
   Every per-pixel shader call must be a regular function
   call into native code that was compiled by a defined
   pipeline at some point *before* the renderer launches.
   Interpretation is too slow (50–200× native) for any
   non-trivial fragment shader. JIT-in-process commits the
   daemon to W+X memory pages, complicates auditing, and
   couples codegen security to the renderer.

2. **Target the steady-state perf of hand-written ARM64
   code, not "credible-fallback" perf.** Past experience
   on a sibling project (PPTK — see §13) measured general-
   purpose AOT translation at <30% of hand-written native.
   For a software renderer in the pixel hot loop, that's
   the difference between "Atrium app at 30 FPS" and
   "Atrium app at 8 FPS." We need ≥80% of hand-written
   native, which means a bespoke backend that exploits
   the structure of shader code rather than a general-
   purpose compiler tuned for arbitrary input.

3. **Ship working code on day 1, then improve perf
   incrementally.** The bespoke backend will take months
   to cover the full SPIR-V opcode surface for the GLSL
   4.50 core subset that Khronos samples use. We can't
   block tier-2's first end-to-end demo on full bespoke
   coverage. The architectural answer is a second
   backend (Cranelift) that handles the long tail at
   acceptable-but-degraded perf, with bespoke taking over
   shader-by-shader as its op coverage widens.

These three constraints land us at the three-tier model in
§2.

---

## 2. The three-tier execution model

```
                                  SPIR-V module
                                       │
                                       ▼
                  ┌───────────────────────────────────────────┐
                  │  atrium-spv-frontend                      │
                  │  (rspirv parse + SSA construction +       │
                  │   structured-CFG recovery from            │
                  │   OpSelectionMerge / OpLoopMerge)         │
                  └───────────────────────────────────────────┘
                                       │
                                       ▼
                            atrium-spv-ir module
                                       │
              ┌────────────────────────┼─────────────────────────┐
              ▼                        ▼                         ▼
     ┌────────────────┐      ┌─────────────────┐      ┌──────────────────┐
     │  bespoke       │      │  Cranelift      │      │  interpreter     │
     │  ARM64/x86_64  │      │  fallback       │      │  (test-only;     │
     │  backend       │      │  (atrium-spv-ir │      │   walks SPIR-V   │
     │  (linear-scan  │      │   → cranelift   │      │   directly, no   │
     │   RA + ISel +  │      │   IR adapter,   │      │   shared         │
     │   pptk asm.rs) │      │   then          │      │   frontend with  │
     │                │      │   cranelift-    │      │   the production │
     │                │      │   object)       │      │   backends)      │
     └────────┬───────┘      └────────┬────────┘      └─────────┬────────┘
              │                       │                         │
              ▼                       ▼                         ▼
        .o → ld → .so          .o → ld → .so              in-process
              │                       │                  shader execution
              │                       │                  (cargo test only)
              └───────┬───────────────┘
                      ▼
        /var/atrium/shaders/v{N}/{hash}.so
                      │
                      ▼
       daemon dlopens at runtime, normal path
```

### Backend selection at compile time

`atrium-spv-compile` tries bespoke first. The bespoke
backend exposes a `can_handle(&atrium_spv_ir::Function) -> bool`
predicate; if it returns false (e.g., the function uses a
SPIR-V op the bespoke backend hasn't grown support for
yet), `atrium-spv-compile` falls back to the Cranelift
backend, which has wider coverage from day 1 because
Cranelift's IR is more general-purpose.

The resulting `.so` is identical in shape regardless of
which backend produced it — same exported symbols, same
ABI, same metadata struct (§4). The daemon doesn't know or
care which backend was used. The cache directory tracks
which backend produced each entry (in metadata) so we can
measure coverage and prioritise bespoke work, but
behaviourally it's transparent.

### Cache + on-demand compile

Two trigger points for compilation, both supported:

**(a) Install-time, driven by `atrium-pkg`** — the
happy path. When an Atrium-shipped app installs, `atrium-pkg`
scans the bundle's shader manifest, runs `atrium-spv-compile`
on each blob, and lands the resulting `.so` files in
`/var/atrium/shaders/v{N}/` before the app ever runs.
First launch is instant.

**(b) First-`vkCreateShaderModule`, on demand** — the
fallback. Apps that synthesize shaders at runtime or
weren't installed through `atrium-pkg` hit this path. The
vkCreate call blocks for the compile (bespoke ~50–200 ms,
Cranelift ~50–200 ms; comparable), then proceeds.

### Test path

The interpreter never ships. In `cargo test`, every test
shader is compiled three ways:

1. Bespoke output (if bespoke can_handle returns true)
2. Cranelift output
3. Interpreter "output" (just a function we can call with
   inputs to get the expected outputs)

The test harness drives the same inputs through all three
and compares pixel-for-pixel. Disagreement is a bug; the
disagreement pattern (which two of three agree) tells us
where the bug is (frontend, bespoke backend, Cranelift
adapter).

---

## 3. The five locked decisions

> Where this spec deviates from "obvious" it's deliberate.
> These are the five decisions whose alternatives were
> explicitly considered and rejected.

### D1. AOT, not JIT, not interpreter (in production)

**Decision.** All SPIR-V → native code generation happens
in `atrium-spv-compile`, in its own process, before the
daemon ever calls the shader. Output is a `.so` on disk.

**Why not JIT (LLVM/Cranelift in-process).** JIT is the
standard answer for SW Vulkan (llvmpipe). For Atrium it's
wrong because: runtime codegen in a privileged daemon is
a security concern (JIT'd pages are W+X by definition;
bugs in the codegen become RCE); the codegen latency is
paid every launch (no cross-run cache); LLVM is a
heavyweight dependency that prevents us from shipping the
daemon as a single self-contained Rust binary.

**Why not an interpreter (in production).** Per-pixel
fragment-shader work done by interpreting SPIR-V opcodes
is 50–200× slower than native. The whole point of tier-2
is to be a credible software renderer.

(The interpreter exists in the test harness — see D5.)

### D2. Bespoke ARM64/x86_64 backend for the fast path, Cranelift for the fallback

**Decision.** Two production backends:

- **Bespoke**: SSA IR (`atrium-spv-ir`) → linear-scan
  register allocator → hand-written instruction selector
  that emits through the `pptk-codegen-arm64::asm.rs`
  encoder (reused; see §13). x86_64 backend parallel
  structure; deferrable to v1.5 if we ship FreeBSD-ARM64
  only first. Output: object file via PPTK's encoder
  patterns; linked with system `ld` to produce the `.so`.

- **Cranelift fallback**: atrium-spv-ir → cranelift IR
  via a thin adapter (~1500 LoC) → `cranelift-object` →
  object file → `ld` → `.so`. Used when the bespoke
  backend's `can_handle` returns false.

**Why bespoke.** PPTK measured general-purpose AOT
translation at <30% of hand-written native on a different
workload (binary translation), but for SPIR-V → ARM64 the
adversarial conditions don't apply (we have full semantic
information, no x86 flag/calling-convention emulation
required). The achievable perf for shader-shape code with
a bespoke backend is 80–95% of hand-tuned native. For a
SW renderer that's the difference between viable and
not-viable.

**Why also Cranelift, not "just ship bespoke."** Bespoke
will take months to cover the full SPIR-V op surface.
Cranelift handles the long tail from day 1 at acceptable
(if slower) perf. Trading a 30 MB binary-size cost for
"working tier-2 from the first commit, with quality
ramping in over time" is the right call.

**Why not LLVM-as-library** (the third option). LLVM
would give ~95% of hand-written perf with ~6 weeks of
work, vs bespoke's 90% with ~20 weeks. The blocker is
policy: committing tier-2 to LLVM means LLVM is a
permanent base dependency, vs Cranelift's pure-Rust
self-contained nature. The atrium-mesa precedent shows
LLVM as a *transitional* layer is acceptable, but tier-2
is the long-term Vulkan story and shouldn't inherit
LLVM's release cadence + security disclosures + C++
surface area permanently.

### D3. Compilation runs in a jailed sub-process

**Decision.** `atrium-spv-compile` is a standalone
binary. It runs under Portcullis with FS capabilities
only for the input SPIR-V and output `.so` paths.

**Why a sub-process.** Two reasons. First: untrusted
SPIR-V is the input, and bugs in the translator/codegen
shouldn't reach the privileged daemon. Second: it makes
the daemon's `cargo` dependency tree strictly smaller —
no rspirv, no Cranelift, no pptk codegen libraries in
the daemon's link line; the daemon links only against
the runtime shader-cache + Tier2Backend dispatch.

### D4. Cache versioning lives in the path

**Decision.** Cache layout is
`/var/atrium/shaders/v{N}/<sha256>.so` where `N` is the
shader-ABI version. ABI changes bump `N`. Old versions'
directories become unreachable; first launch post-bump
recompiles transparently. A periodic
`atrium-pkg gc shader-cache` sweeps unused versions.

**Why path-based versioning.** Same playbook Tessera
uses for its content-addressed pack format. Lets
multiple ABI versions coexist during a daemon upgrade
transition.

### D5. Differential testing via a third (test-only) SPIR-V interpreter

**Decision.** A SPIR-V interpreter lives in the test
harness (`atrium-spv-tests/src/interpreter.rs`),
walking SPIR-V bytes directly — *no shared frontend
with the production backends.* Every test shader is
compiled all three ways (bespoke, Cranelift,
interpreter); outputs are compared pixel-for-pixel.

**Why have an interpreter even when we have two
production backends.** If bespoke and Cranelift share
the SPIR-V → atrium-spv-ir frontend, a bug in the
frontend produces wrong output in *both* backends —
they agree with each other but disagree with reality,
and a "compare bespoke vs Cranelift" test won't catch
it. The interpreter sidesteps this by reading SPIR-V
directly with no shared code. It's the only path that
can catch frontend-translation bugs.

**Why it doesn't violate D1.** D1 is about the renderer
process at production runtime. The interpreter only runs
in `cargo test`, in a developer's shell. The shipped
daemon never sees it.

---

## 4. The shader ABI

This is the load-bearing contract between every backend's
output and tier-2's rasterizer. The cache-version (D4)
protects this surface.

### 4.1 Exported symbols

A compiled shader `.so` exports exactly the symbols its
SPIR-V entry points declared, prefixed with `atrium_`:

```rust
// Vertex shader.
#[no_mangle]
pub unsafe extern "C" fn atrium_vs_main(
    in_attributes:    *const u8,
    in_attr_strides:  *const u32,
    uniforms:         *const u8,
    push_constants:   *const u8,
    vertex_index:     u32,
    instance_index:   u32,
    out_position:     *mut [f32; 4],
    out_varyings:     *mut u8,
    out_clip_distance: *mut f32,
);

// Fragment shader.
#[no_mangle]
pub unsafe extern "C" fn atrium_fs_main(
    in_varyings:    *const u8,
    uniforms:       *const u8,
    push_constants: *const u8,
    frag_coord:     [f32; 4],
    samples_mask:   u32,
    out_color:      *mut [f32; 4],
    out_depth:      *mut f32,
);

// Compute shader.
#[no_mangle]
pub unsafe extern "C" fn atrium_cs_main(
    uniforms:        *const u8,
    push_constants:  *const u8,
    workgroup_id:    [u32; 3],
    local_id:        [u32; 3],
);
```

Plus a metadata blob for ABI validation and layout
discovery:

```rust
#[no_mangle]
pub static ATRIUM_SHADER_METADATA: AtriumShaderMetadata = ...;

#[repr(C)]
pub struct AtriumShaderMetadata {
    pub abi_version:      u32,   // must == TIER2_SHADER_ABI_VERSION
    pub stage_kind:       u32,
    pub backend_kind:     u32,   // 0=bespoke, 1=cranelift (informational)
    pub attr_count:       u32,
    pub attr_offsets:     [u32; 16],
    pub attr_formats:     [u32; 16],   // VkFormat numeric
    pub varying_count:    u32,
    pub varying_offsets:  [u32; 16],
    pub varying_formats:  [u32; 16],
    pub uniform_size:     u32,
    pub local_size:       [u32; 3],
}
```

`backend_kind` lets the daemon's metrics distinguish how
many shaders are still on the Cranelift path vs bespoke
— operational visibility, not behavioural.

### 4.2 Layout rules

- **Vertex attributes** packed per-binding in declaration
  order; std430-style padding for alignment.
- **Varyings** packed in `location` order, std430-style.
  Perspective-correct interpolation coefficients computed
  by the rasterizer before the fragment shader sees
  `in_varyings`.
- **Uniforms** from all bound descriptor sets flattened
  into one contiguous block at `vkCmdBindDescriptorSets`
  time. Translator emits direct offsets at translate time.
- **Push constants** as a separate 128-byte block.
- **Sampled images** are special: translator emits calls
  into `atrium-spv-runtime::sample_image_2d` (or 1D/3D/
  cube variants) — a small Rust library tier-2 ships
  alongside the rasterizer.

### 4.3 ABI versioning

`TIER2_SHADER_ABI_VERSION` is a build-time `u32` constant
in both the daemon and every backend. Bumped when any of:
- a symbol name or function signature changes
- the metadata struct layout changes
- the per-shader runtime library's API changes
- the uniform/varying layout rules change

Mismatch on dlopen → daemon rejects the cached `.so`
and falls through to the cold-compile path under the
new `v{N}/`.

---

## 5. atrium-spv-ir — the internal IR

A small SSA-form IR designed for shader workloads. Shared
by both production backends so they consume the same
frontend output.

### 5.1 Shape

- **SSA form.** Every value is defined exactly once;
  phi nodes at block merge points. Standard SPIR-V
  shape.
- **Structured control flow.** Blocks are organised as
  `Block { kind: BlockKind, instructions: Vec<Inst> }`
  where `BlockKind` is one of `Linear`, `If { then, else,
  merge }`, `Loop { header, body, continue, merge }`,
  or `Switch { cases, default, merge }`. The frontend
  recovers this from SPIR-V's `OpSelectionMerge` /
  `OpLoopMerge` markers and refuses to compile
  unstructured CFGs (these don't come out of glslc
  anyway).
- **First-class vector types.** `Type::Vec2(f32)`,
  `Vec3(f32)`, `Vec4(f32)` are not lowered to scalar
  until very late (instruction selection). This is
  critical for SIMD codegen — late lowering preserves
  vectorisation information for the backend.
- **Numeric type set.** `i32`, `u32`, `i64` (rare),
  `u64` (rare), `f32`, `f64` (rare; many tier-2 targets
  may not implement). Bools represented as `i32` with
  the standard 0/1 convention.
- **Memory model.** Pointers represent typed slots into
  the uniform / push-constant blocks. No general-purpose
  heap; no aliasing concerns; all addresses are known at
  translate time from descriptor-set layout.
- **No exceptions, no GC, no dynamic dispatch.** The
  only "external" calls are texture-sample helpers
  (resolved at link time to `atrium-spv-runtime`) and
  the trio of derivative ops (dFdx / dFdy / fwidth),
  which the rasterizer provides via numerical estimates
  computed across pixel quads.

### 5.2 Why this shape

- **Bespoke backend stays simple.** Pattern-matching
  vector ops to NEON / AVX2 instructions is much easier
  when the IR carries first-class vec types. If we
  lowered to scalar early, we'd need an autovectoriser,
  and that's a deep rabbit hole.
- **Cranelift adapter stays simple.** Cranelift IR has
  vector types too; the adapter is a near-1:1 walk.
- **Interpreter stays simple.** Scalar walk over typed
  values; vector ops dispatch to per-lane scalar ops.

### 5.3 What it doesn't have

- **No general CFG.** Unstructured control flow gets
  rejected at the frontend, returning `VK_ERROR_INVALID_SHADER_NV`
  from `vkCreateShaderModule`. This is fine for ~99% of
  glslc output; the few corner cases (computed gotos,
  irreducible loops) aren't shapes shader compilers
  produce.
- **No SSA-deconstruction pass in the frontend.** Both
  backends are SSA-aware and handle phi nodes directly.
  Cranelift IR is SSA; bespoke's regalloc resolves phis
  during linear-scan as part of liveness analysis.

---

## 6. The compile pipeline

```
                        ┌──────────────────────────────────────┐
                        │  atrium-spv-compile (jailed binary)  │
                        │  ────────────────────────────────    │
                        │  1. Read SPIR-V from --input         │
                        │  2. atrium-spv-frontend: parse +     │
                        │     SSA + structured CFG → IR        │
                        │  3. Try bespoke backend:             │
                        │     bespoke::can_handle(&ir)?        │
                        │       yes: bespoke::compile(&ir)     │
                        │       no:  cranelift::compile(&ir)   │
                        │  4. Emit object file (.o)            │
                        │  5. spawn `ld` to link → .so         │
                        │  6. Generate PC-map sidecar (.pcmap) │
                        │  7. write both to --output dir       │
                        │  8. exit                             │
                        └──────────────────────────────────────┘
```

### 6.1 Crates

- **`atrium-spv-ir`** — the IR types (Function, Block,
  Inst, Type, Value, etc.). No backends, no parsing.
  Used by everything downstream.

- **`atrium-spv-frontend`** — SPIR-V → atrium-spv-ir.
  rspirv for the byte-level parse; our own SSA builder
  + structured-CFG recoverer on top.

- **`atrium-spv-backend-bespoke`** — atrium-spv-ir → object
  bytes. Contains the linear-scan regalloc, the instruction
  selector, and the ARM64 + x86_64 emitters. Depends on
  `pptk-codegen-arm64` for the ARM64 instruction-encoding
  primitives (the `asm.rs` layer; see §13).

- **`atrium-spv-backend-cranelift`** — atrium-spv-ir →
  cranelift IR → object bytes. Thin adapter layer; the
  heavy lifting is Cranelift's.

- **`atrium-spv-runtime`** — small `staticlib` linked
  into every compiled shader `.so`. Texture-sample
  kernels, derivative helpers, miscellaneous math.
  Surface area kept tiny to minimise ABI-version churn.

- **`atrium-spv-compile`** — the binary, glues the above.

- **`atrium-spv-tests`** — the test harness, including
  the SPIR-V interpreter and the differential-test
  framework.

### 6.2 Linker

Output is an object file (Mach-O on macOS dev hosts,
ELF on FreeBSD targets). `atrium-spv-compile` shells
out to the system `ld` (`/usr/bin/ld`) for the final
link. ~50 ms of `ld` startup is included in the
per-shader compile latency budget.

We do not bundle our own linker. System `ld` is
universally available, well-tested, and the only
external program tier-2 invokes.

---

## 7. The Tier2Backend interface

A new module `aqueduct-gpu-host/src/tier2/` parallel to
`software/`. Sketch:

```rust
pub struct Tier2Backend {
    shader_cache: Mutex<HashMap<ShaderHash, LoadedShader>>,
    pipelines:    Mutex<HashMap<ResourceId, Tier2Pipeline>>,
    rasterizer:   RefCell<Rasterizer>,
    metrics:      Tier2Metrics,
}

struct LoadedShader {
    handle:        libloading::Library,
    vs:            Option<unsafe extern "C" fn(...)>,
    fs:            Option<unsafe extern "C" fn(...)>,
    cs:            Option<unsafe extern "C" fn(...)>,
    metadata:      AtriumShaderMetadata,
    pc_map:        Option<PcMap>,    // for crash triage
}

struct Tier2Pipeline {
    vs:                  unsafe extern "C" fn(...),
    fs:                  unsafe extern "C" fn(...),
    raster_state:        RasterState,
    vertex_input:        VertexInputState,
    descriptor_layout:   DescriptorLayout,
}

impl Backend for Tier2Backend {
    fn identity(&self) -> BackendId { BackendId::new(GpuVendor::Software, 1) }
    // ... usual Backend trait methods.
    fn submit_frame(&self, ...) -> Result<(), String> {
        // Walks FrameOp stream like SoftwareBackend does,
        // but the BindPipeline / Draw* dispatch goes
        // through self.rasterizer.draw(...) which calls
        // the pipeline's vs/fs function pointers per
        // vertex/pixel.
    }
}
```

### Backend trait change

Tier-2 needs per-draw state tier-1 doesn't care about:
vertex buffer pointers + strides, full viewport, render-
target metadata. The existing `Backend` trait flattens
many of those. We'll bump the trait to pass the full
state, with tier-1 ignoring fields it doesn't care
about. One source of truth, no duplicated dispatch.

---

## 8. The rasterizer

The other half of the renderer. Self-contained, no SPIR-V
or shader concerns; takes a Tier2Pipeline + draw call +
bound buffers and writes pixels.

### 8.1 Pipeline stages

```
  Index/draw walker  →  Vertex shading  →  Primitive assembly
        →  Clipping  →  Perspective divide + viewport
        →  Triangle setup (edge funcs, bbox, perspective-
                            correct interpolation coefs)
        →  Tiled rasteriser (8×8 or 16×16 tiles):
              interpolate varyings →
              depth test →
              fragment shader call →
              blend →
              colour / depth / stencil write
```

### 8.2 Performance budget

Aspirational, not blocking on first iteration:

- 1280×800 fullscreen quad with a 50-instruction fragment
  shader: target ~16 ms / frame on an M4 Max single core.
- Multi-thread the tile raster across N cores in iteration
  3; expect ~linear scaling up to memory bandwidth.
- SIMD-vectorise the per-pixel inner loop with `std::simd`
  in iteration 4 (4-wide or 8-wide pixel quads, calling
  the bespoke-backend's SIMD fs entry point).

First iteration is scalar single-thread; the architecture
makes those upgrades drop-in.

---

## 9. ARM64 codegen constraints

Lifted from PPTK's `crates/pptk-codegen-arm64/CONSTRAINTS.md`
(see §13) and adapted for our SSA IR → ARM64 path. These
are the gotchas the PPTK author hit in 100+ debugging
sessions; encoded here so we don't re-hit them.

### 9.1 PSTATE / NZCV semantics

- **Flag-setting forms.** ARM64 has separate ADD / ADDS
  variants; only ADDS updates NZCV. If subsequent code
  reads flags (e.g. via B.cond), the prior arithmetic
  *must* be the `-s` form. The instruction selector
  must track which IR values feed into branches and
  emit flag-setting forms for those producers.
- **Carry-flag polarity.** ARM64 SUBS sets C *inverted*
  from x86. For shader code (no x86 emulation involved)
  this doesn't matter directly, but it bites if we ever
  hand-write a sequence assuming x86 semantics. PPTK had
  to emit `CFINV` after ADDS to compensate; we don't,
  because shader code never has "expected x86 carry."
- **Narrow-register flag updates.** `AND w17, w0, #1`
  doesn't update flags. `ANDS w17, w0, #1` does. The
  selector must use the `-s` form when feeding flags.

### 9.2 Immediate encoding

- **Logical immediates** (used by AND/ORR/EOR) must be
  one of 5334 representable "bit-mask" patterns. Random
  constants don't fit; emit `MOV` to a scratch register
  first.
- **Arithmetic immediates** (used by ADD/SUB/CMP) are
  12 bits unsigned, optionally shifted by 12. Larger
  constants need `MOV` + register form.
- **Load/store offsets.** Unsigned 12-bit scaled by
  element size. Larger offsets need address materialisation.
- The encoder (`pptk asm.rs`) asserts on invalid
  immediates rather than silently truncating. Selector
  must materialise constants before calling encoder.

### 9.3 Addressing modes

- ARM64 has rich addressing modes
  (`[Xn, Xm, lsl #imm]`, `[Xn, #imm]!`, `[Xn], #imm`)
  but the encoder only supports the subset PPTK needed.
  Our selector should prefer the base+offset form for
  simplicity; pre/post-index variants are perf wins for
  loop-carried address increments and can come later.

### 9.4 Branch reach

- B and BL have ±128 MB range (26-bit signed PC-relative).
- B.cond has ±1 MB range (19-bit signed).
- Per-shader `.so` sizes are tiny (<1 MB typically), so
  both fit comfortably. Cross-function calls within the
  same `.so` are direct; calls into `atrium-spv-runtime`
  go through the GOT and have unbounded range.

### 9.5 SIMD lowering

- NEON 128-bit registers `v0`-`v31`. `vec4<f32>` maps to
  one register; `vec3<f32>` uses a 4-lane register with
  the high lane ignored (waste but simple).
- `vec2<f32>` uses the low 64 bits (instructions like
  `FADD v0.2s, v1.2s, v2.2s`).
- Per-lane element selectors (e.g. `OpVectorExtractDynamic`)
  with a non-constant index require `DUP` + lane-extract
  patterns. Constant-index extracts use `MOV.S w_dst, vN.s[i]`.

### 9.6 Encoder gaps to fill

PPTK's `asm.rs` covers the x86 subset 7-Zip needs. For
shaders we additionally need:

- **NEON shuffle/permute** (`TBL`, `TBX`, `ZIP1/ZIP2`,
  `UZP1/UZP2`, `EXT`) — for swizzles like `.xyzw → .wxyz`.
- **NEON dup-from-scalar** (`DUP V, W`) — for splatting a
  uniform value across a vector.
- **Floating-point conversions** (`SCVTF`, `FCVTZS`) —
  int↔float casts in shaders.
- **Reciprocal / reciprocal-sqrt approximations** (`FRECPE`,
  `FRSQRTE` + Newton-Raphson refinement) — for `1.0/x`
  and `inversesqrt`.
- **Min/max/abs** (`FMIN`, `FMAX`, `FABS`) — every shader
  uses these.

Estimated ~1–2 weeks of additive encoder work, layered on
top of PPTK's solid foundation. Not a rewrite.

---

## 10. PC-map sidecar + differential test harness

Two pieces of infrastructure that PPTK's runbook
identifies as their biggest hindsight regret — "should
have had this from day 1."

### 10.1 PC-map sidecar

Every compiled shader `.so` ships with a `.pcmap` sidecar:

```rust
#[repr(C)]
pub struct PcMap {
    pub entries: Vec<PcMapEntry>,
}

#[repr(C)]
pub struct PcMapEntry {
    pub spirv_offset:    u32,   // byte offset into source SPIR-V
    pub host_offset:     u32,   // byte offset into the .so's __text
}
```

Generated by the bespoke backend during instruction
emission (we already know which IR instruction we're
emitting; the IR carries provenance to the SPIR-V offset
that produced it). Cranelift backend generates a less-
precise map (per-function granularity).

When a shader crashes (SIGSEGV, SIGILL) or produces
unexpected pixels, the daemon's crash handler reads the
faulting PC, subtracts the `.so` base, looks up the
`.pcmap`, and reports "shader %s, SPIR-V offset 0x%x"
in the log instead of just a raw PC. PPTK explicitly
wishes they had this from day 1.

### 10.2 Differential test harness

In `atrium-spv-tests`:

```rust
/// Compile a SPIR-V shader through all three backends,
/// run with given inputs, and assert pixel-for-pixel
/// agreement.
fn assert_shader_agrees(
    spirv: &[u8],
    inputs: ShaderInputs,
    tolerance: ColorTolerance,
);
```

Internally:
1. Spawn `atrium-spv-compile` to produce bespoke `.so`
   (if `can_handle` allows).
2. Spawn `atrium-spv-compile` with `--backend cranelift`
   to produce Cranelift `.so`.
3. Walk SPIR-V via the in-process interpreter.
4. Compare outputs.

Disagreement pattern → bug location:
- Bespoke ≠ Cranelift, both = interpreter: **bespoke
  backend bug.**
- Bespoke = Cranelift, both ≠ interpreter: **frontend
  bug** (both production paths inherit the bug from
  shared frontend).
- Bespoke ≠ Cranelift ≠ interpreter: **probably
  multiple bugs** (rare; investigate carefully).
- All three agree: shader works ✓.

This harness IS the project's debugging infrastructure.
Every bug report should be reproducible as a failing
`assert_shader_agrees` test.

---

## 11. Implementation phases

The infrastructure-first ordering reflects PPTK's
hindsight regret. The dependencies are pretty linear
through phase 6; phase 7 (rasterizer) can land in
parallel with phases 3–5.

| Phase | Work | Est. |
|---|---|---|
| **0** | **Infrastructure.** SPIR-V interpreter (~1500 LoC, test-only). PC-map sidecar format + reader. Differential-test harness. Lift PPTK's CONSTRAINTS.md into `docs/spec/tier2-shader-codegen-constraints.md`. Stand up `atrium-spv-ir` crate with the basic types. | 3 wk |
| **1** | `atrium-spv-frontend`: rspirv parse → atrium-spv-ir SSA construction → structured CFG recovery. Tests use the interpreter to verify roundtrip semantics. | 3 wk |
| **2** | `atrium-spv-backend-cranelift`: atrium-spv-ir → cranelift IR → object → ld → .so. Establishes the working-shader-end-to-end demo and the production fallback. End-to-end VM render of a constant-colour fullscreen quad. | 3 wk |
| **3** | `atrium-spv-backend-bespoke` skeleton: linear-scan regalloc + instruction selector + ARM64 emitter (via pptk-codegen-arm64's asm.rs). Initially handles only a narrow op subset (`+`, `*`, constants). Differential tests pass for the supported ops. | 4 wk |
| **4** | x86_64 backend (selector + encoder). Deferrable if FreeBSD-ARM64-only for v1. | 3 wk |
| **5** | ELF object writer (parallel to pptk-macho). Reuse pptk-codegen-common's Reloc enum. | 1 wk |
| **6** | `Tier2Backend` skeleton + `atrium-spv-compile` binary + Portcullis jail setup + cache + dlopen plumbing. | 2 wk |
| **7** | Rasterizer (scalar single-thread, single colour attachment, no depth). End-to-end VM render with a textured triangle. | 3 wk |
| **8** | NEON encoder extensions (§9.6) + SIMD lowering in bespoke selector. 4-wide pixel quads. | 2 wk |
| **9** | Texture sampling (atrium-spv-runtime kernels), derivatives, broaden SPIR-V coverage. Iterative; one Khronos sample at a time. | open |
| **10** | atrium-pkg install-time shader-precompile hook. | 1 wk |

**Phases 0–8: ~24 weeks of focused work** to first end-
to-end render with SIMD. Working tier-2 (Cranelift only,
scalar rasterizer) demo possible at the ~11-week mark
after phases 0+1+2+6+7. Bespoke perf ramps in shader-by-
shader through phases 3, 8, 9.

Without phase 4 (x86_64), ~21 weeks. Worth considering
as the v1 scope, with x86_64 added later when we want
tier-2 on macOS dev hosts outside the VM.

---

## 12. Open questions

- **Bundle x86_64 in v1, or ship FreeBSD-ARM64-only?**
  Saves 3 weeks if deferred. Cost: can't dev-iterate on
  macOS host outside the VM. Probably worth deferring;
  the VM is the production target anyway.

- **Cranelift cache poisoning.** If a Cranelift backend
  bug produces wrong output, the cache holds the wrong
  `.so` until the ABI bumps. Mitigation: tests run
  every CI pass; bugs surface fast. Worst case: bump
  the ABI version even for a Cranelift-only fix to
  invalidate stale caches.

- **PPTK encoder version pinning.** We depend on pptk-
  codegen-arm64 as a path dep (since both are in the
  Atrium tree). When PPTK lands a new release, do we
  pull it in automatically or pin to a known-tested
  rev? Recommend: pin to a sha in our Cargo.toml,
  refresh deliberately.

- **Compute-shader workgroup execution model.** Tier-2's
  rasterizer parallelises across tiles for graphics.
  Compute shaders need their own dispatcher; first cut
  is scalar per-invocation iteration.

- **What does cargo's link line look like?** rustc
  built-in linker, or shell out to system ld? PPTK uses
  the system `ld`. We probably do too — keep the
  toolchain dependency minimal.

- **Validation-layer hostility.** Apps with
  `VK_LAYER_KHRONOS_validation` loaded will probe a lot
  of state. Tier-2 should pass the layer's tests as
  much as possible; worth running CI against it from
  phase 6.

---

## 13. Relationships to other components

- **atrium-vk-icd.** Already routes third-party
  shaders as `id(icd-runtime, *)`. No code changes
  required to start hitting tier-2; the dispatch
  decision is daemon-side.

- **aqueduct-gpu protocol.** No new opcodes for tier-2's
  steady state. Phase 9 may add `READ_BUFFER_FOR_DISPATCH`
  for vkCmdDispatchIndirect.

- **atrium-pkg.** Owns the install-time shader-precompile
  hook (phase 10). Bundle manifest gets a
  `shaders: [...]` array.

- **Portcullis / jails.** `atrium-spv-compile` runs in a
  jail with FS caps for input/output paths only.

- **Tessera.** `/var/atrium/shaders/v{N}/` lives on a
  Tessera-backed volume; cache eviction integrates with
  the standard Tessera GC policy.

- **PPTK.** The pptk project (under `/Users/girivs/src/pptk`)
  provides:
  - `pptk-codegen-arm64::asm.rs` — the ARM64 instruction
    encoder we link as a path dep.
  - `pptk-codegen-common::Reloc` — relocation enum we reuse.
  - `pptk-macho` — Mach-O object emitter; useful for
    macOS dev iteration. ELF emitter (phase 5) is our
    own work but follows the same architectural
    pattern.
  - `CONSTRAINTS.md` — the gotchas catalogue we adapt
    in §9 and `docs/spec/tier2-shader-codegen-constraints.md`.
  - The runbook (`debug/NEXT_SESSION.md`) — read by the
    spec author at design time; supplied the
    infrastructure-first phase 0 ordering, the dual-
    backend oracle insight, and many specific gotcha
    rules. *Not* a runtime dependency.

  We do NOT reuse: the PE loader, the x86→ARM64
  lowering tables, the pinned-register convention, the
  PE-mirror / IAT-shim / segment-handling code, or the
  pptk-lift pipeline. Tier-2's architecture differs in
  having an actual IR and an actual register allocator;
  PPTK's pipeline shape doesn't transfer.

- **Future tier-3 (real GPU).** When native atrium-gpu
  drivers land at D5+, they'll have their own
  shader-compile pipeline (probably HW vendor-specific).
  Tier-2 stays as the universal-compatibility software
  fallback, the way Mesa's llvmpipe stays alongside
  vendor drivers.

---

## 14. What we are NOT building

- A JIT in the renderer process. Period.
- A SPIR-V interpreter in production.
- A general OpenGL ES path.
- A GPU-shaped tile binner with vertex pre-pass. Tier-2
  is a forward immediate-mode rasterizer.
- Vulkan extensions beyond core 1.3 plus what
  atrium-vk-icd already advertises. Ray tracing, mesh
  shaders, video — all out of scope.
- A custom linker. We shell out to system `ld`.
- Sparse memory. Already stubbed at the ICD layer.
- Tier-2 on macOS host for dev iteration in v1 (deferred
  to v1.5 if we ship FreeBSD-ARM64-only).

---

## 15. Status / next steps

Phases 0–7 landed; the stack runs end-to-end through
both backends.  As of 2026-05-21 the compute path
specifically supports:

**Frontend (atrium-spv-frontend):**

- Full SPIR-V interface discovery (entry points, uniforms,
  push-constants, vertex inputs, varyings, descriptor
  bindings).
- BuiltIn dispatch for WorkgroupId, LocalInvocationId,
  GlobalInvocationId, LocalInvocationIndex, WorkgroupSize,
  VertexIndex, InstanceIndex.
- AccessChain with constant-index struct + array walks
  and a single trailing dynamic index into
  RuntimeArray/Array (Op::PtrOffsetDynamic).
- Subgroup ops at `subgroupSize=1`: each workgroup runs
  serially on one CPU thread, so every workgroup contains
  exactly one subgroup of size 1.  All `OpGroupNonUniform*`
  ops lower to trivial expressions at frontend time:
  `Elect`/`AllEqual` → ConstantTrue; `All`/`Any`/`Broadcast`
  /`BroadcastFirst`/`Shuffle*` → source value;
  `Ballot(p)` → uvec4(p?1:0, 0, 0, 0); arithmetic/bitwise/
  logical reductions with `Reduce` or `InclusiveScan` →
  source value; with `ExclusiveScan` → the operation's
  identity element (0 for Add/Or/Xor, 1 for Mul, ~0 for
  And, INT_MAX/MIN for SMin/SMax, +∞/-∞ for FMin/FMax, etc).
  `ClusteredReduce` is rejected.  Real parallelism for these
  ops is a separate later arc (would require dispatching
  multiple invocations per "subgroup" with cross-invocation
  buffering).
- Specialization constants: `OpSpecConstant{,True,False,
  Composite}` are translated to regular constants.  The
  frontend exposes both `translate(spv)` (uses declared
  defaults) and `translate_with_spec_overrides(spv,
  &SpecOverrides)` where `SpecOverrides` is a
  `HashMap<u32 /* SpecId */, u32 /* 32-bit bit pattern */>`.
  Overrides are applied by rewriting the matching
  `OpSpecConstant*` instruction in place and re-tagging it
  as a plain `OpConstant*` before the constants pass.
  `atrium-spv-compile` exposes the mechanism via
  `--spec-const SPECID=VALUE` (repeatable; accepts decimal,
  hex `0x...`, signed `-N`, or float `f:N.N` literals) and
  mixes the overrides into the cache hash when present so
  specialised builds don't collide with the default-value
  build in the cache.  `atrium-spv-loader::ShaderCache` and
  `Tier2Registry` both expose `*_with_spec_overrides`
  variants that route the overrides through to
  `atrium-spv-compile --spec-const` and key the in-process
  registry by the spec-aware hash, so two pipelines
  specialising the same SPIR-V land on distinct
  `Tier2ShaderId`s backed by distinct compiled artifacts.
  Verified end-to-end by a test that runs the same fragment
  shader at two override settings and observes the runtime
  colour change.  `atrium-vk-icd` now parses the `VkSpecializationInfo`
  attached to `VkPipelineShaderStageCreateInfo` and forwards
  every `(constantID, value)` entry on the pipeline-create
  envelope (Tier2ComputeStateBlob's `spec_overrides` field).
  The daemon's session handler retains the original SPIR-V
  per `Tier2ShaderId` so it can call
  `Tier2Registry::register_with_spec_overrides` at pipeline-
  create time without the ICD re-uploading the module.  End-
  to-end verified by a test that dispatches a shader writing
  a `OpSpecConstant uint` to an SSBO under
  `VkSpecializationInfo {0 -> 0xCAFEBABE}` and observes the
  override value land in `ssbo[0]` (vs the SPIR-V default).
  `OpSpecConstantOp` is folded at frontend constant-context
  build time: the sub-opcode + operand-id list is evaluated
  in Rust against the already-resolved constants, and the
  result enters the constants map as a regular constant.
  Supported sub-opcodes cover the set glslang emits for
  arithmetic / bitwise / shift / compare / Select on integer
  spec constants (IAdd, ISub, IMul, S/UDiv, S/UMod, SNegate,
  Bitwise{And,Or,Xor}, Shift{Left,Right{Logical,Arithmetic}},
  IEqual, INotEqual, S/ULessThan{,Equal}, S/UGreaterThan{,Equal},
  Logical{And,Or,Equal,NotEqual,Not}, Select).
- Atomic ops: AtomicIAdd, ISub, IIncrement, IDecrement,
  And, Or, Xor, Exchange, Load, Store,
  CompareExchange, SMin/SMax/UMin/UMax — all lowered to
  ARMv8.1 LSE instructions (LDADDAL, LDSETAL, LDCLRAL +
  MVN-for-AND, LDEORAL, SWPAL, LDSMAXAL/LDSMINAL/
  LDUMAXAL/LDUMINAL, CASAL) so they are race-safe under
  workgroup-parallel dispatch.
- Core bitwise ops: BitwiseAnd/Or/Xor, Not,
  ShiftLeft/RightLogical/RightArithmetic, BitReverse,
  BitCount (SWAR popcount synthesised at IR level).
- Float classification: IsNan (FUnordNe(x, x)) and IsInf
  (FOrdEq(|x|, +∞)) — both synthesised onto the existing
  compare ops.
- Workgroup-shared memory: `StorageClass::Workgroup`
  OpVariables are packed into a per-workgroup scratch
  buffer; the frontend records each var's byte offset
  (`Function::workgroup_var_offset`) and the total size
  (`Function::workgroup_size`).  `aggregate_type_size`
  sizes scalars, vectors, arrays, matrices and structs
  recursively (array lengths resolved from their OpConstant).
  The dispatcher allocates one buffer per worker thread,
  zeroes it per workgroup, and passes its base as the 10th
  cs_main argument (`workgroup_buf`, AAPCS64 stack slot
  SP+8).  Array indexing rides the existing AccessChain +
  Op::PtrOffsetDynamic path.
- Barriers (ControlBarrier, MemoryBarrier) — no-ops: the
  dispatcher runs a workgroup's invocations serially on one
  thread, so the causal order is already total within a
  workgroup and a barrier needs no codegen.
- Storage images: `OpImageRead` / `OpImageWrite` on 2D
  `image2D` bindings.  Both lower to a v1-ABI call into the
  runtime (`atrium_img_read_2d` / `atrium_img_write_2d`)
  through a compute image descriptor table — a SEPARATE
  table from the fragment uniforms table, passed in the X0
  (`uniforms`) cs_main slot: 16-byte helper header +
  8-byte `ImageDesc*` slots.  The bespoke backend stashes
  the table base in callee-saved X19 (helper calls clobber
  caller-saved regs) and spills live V-regs + caller-saved
  int regs across the call; Cranelift's call_indirect
  handles spilling itself.  The dispatcher builds the
  table from images bound either via the direct
  `bind_compute_storage_image` API or via the real Vulkan
  descriptor path (`vkCreateImage` + `vkBindImageMemory` +
  `vkCreateImageView` + `vkUpdateDescriptorSets` +
  `vkCmdBindDescriptorSets` → the `BindDescriptors` FrameOp,
  parsed in `execute_compute_ops` for STORAGE_IMAGE writes).
  Multi-binding SSBO + storage-image co-use works: the
  SSBO base registers (X12..X17) are caller-saved and the
  image-helper `blr` clobbers them, so the lowering
  re-loads each from the descriptor table (X2, itself
  saved/restored) after every image call.
- Atomic storage-image ops: `OpImageTexelPointer` forms a
  raw byte pointer to one texel
  (`data + z*slice_bytes + y*stride_bytes + x*4`, computed
  inline off the descriptor table — no helper call); a
  following `OpAtomic*` then read-modify-writes it.
  `imageAtomicAdd` / `imageAtomicCompareSwap` and friends on
  R32 storage images work this way.  Cross-backend: bespoke
  uses ARMv8.1 LSE for race safety under workgroup-parallel
  dispatch, Cranelift's path lowers to a non-atomic
  load+op+store on the same address (same as its SSBO
  atomics — single-threaded under the Cranelift dispatcher).
- 3D storage images: `image3D` `OpImageRead` /
  `OpImageWrite` route to dedicated 3D helpers
  (`atrium_img_read_3d` / `atrium_img_write_3d`), selected
  by coord-lane count (2 → image2D, 3 → image3D).  The
  image-table helper header doubles to 32 B
  (`read_2d @ #0`, `write_2d @ #8`, `read_3d @ #16`,
  `write_3d @ #24`) and the descriptor base shifts to #32.
  The 3D helper signature passes `z` in W3 and the rgba
  scratch slot in X4 (vs X3 for 2D).  `OpImageTexelPointer`
  with a 3-lane coord folds in `z*slice_bytes` for the
  inline texel-address path.  `ImageDesc` carries `depth` +
  `slice_bytes` (appended after the v1 2D fields).
  `OpImageQuerySize` reads `(width, height [, depth])`
  directly off the `ImageDesc` (fields @ #8 / #12 / #24) —
  no helper call; returns uvec2 for image2D, uvec3 for
  image3D.  Both backends.
- `image2DArray` storage images share the image3D code
  path: a 3-lane coord routes to the 3D helper / texel-
  pointer arithmetic, with the `ImageDesc.depth` field
  carrying the layer count and `slice_bytes` the per-
  layer byte stride.  No additional IR or backend work
  was required — verified end-to-end by a differential
  test that writes (x, y, layer, 1.0) per invocation on
  a 2×2×3 array and reads back identical bytes from both
  backends.
  Mip-level *sampling* (the sampler-side counterpart of
  the storage-image work below) is also wired: `TexDesc`
  carries the same `mip_count` + `mip_descs` shape;
  `atrium_tex_sample_2d_lod(tex, samp, u, v, lod, out)` is
  the new helper (uniforms-table slot #16); the
  `atrium_tex_fetch_2d` helper now also honours its `lod`
  parameter.  `UNIFORMS_DESC_BASE` grew 16 → 24.  Both
  backends route `OpImageSampleExplicitLod` through the
  new helper with the LOD passed in V2 (bespoke) or as
  an extra `f32` call arg (Cranelift).
  `sampler2DArray` (Arc 30) sampling: 3-lane coord routes
  to `atrium_tex_sample_2d_array` (helper @ #24) with the
  layer `f32` in V2 / extra-arg position.  `TexDesc` gains
  `depth` + `slice_bytes` for per-layer addressing.
  `samplerCube` (Arc 31) sampling: 3-lane direction routes
  to `atrium_tex_sample_cube` (helper @ #32); the helper
  does the standard major-axis face selection (+X/-X/+Y/
  -Y/+Z/-Z) and (sc/ma, tc/ma) remap.  Cube/array dispatch
  is by `sampled_image.ty`'s `ImageDimensionality::Cube`
  flag rather than coord-lane count.
  `textureGather(sampler2D, uv, component)` (Arc 32):
  `OpImageGather` lowers to a new `Op::ImageGather`
  emitted by the frontend; both backends route through
  `atrium_tex_gather_2d` (helper @ #40) which fetches the
  four texels around the bilinear footprint and packs the
  chosen channel into a vec4 in GLSL order `{(0,1),(1,1),
  (1,0),(0,0)}`.  Wrap modes honoured; component clamped
  `0..3`.  `UNIFORMS_DESC_BASE` grew 24 → 32 → 40 → 48.
  Array/Cube + ExplicitLod combos (Arc 35): two new helpers
  `atrium_tex_sample_2d_array_lod` (#48) and
  `atrium_tex_sample_cube_lod` (#56) combine the Arc 29
  `pick_tex_mip()` mip-indirection with the Arc 30 / Arc 31
  layer-or-face selection.  Helper-table grew 48 → 64 B;
  `UNIFORMS_DESC_BASE` grew 48 → 64.  Bespoke routes the
  call with a four-source parallel copy (V0=u, V1=v,
  V2=third, V3=lod) staged through scratch V4..V7;
  Cranelift extends the existing `call_indirect` signature
  with two `f32` arg slots.  `texture(sampler2DArray,
  vec3(u, v, layer), lod)` and `texture(samplerCube,
  vec3(dir), lod)` now compile through both backends.
  `Image-Operands::Bias` (Arc 36): GLSL `texture(sampler2D,
  uv, bias)` lowers to `OpImageSampleImplicitLod` with the
  `Bias` image-operand.  Tier-2's implicit-LOD collapses to
  mip 0 (no 2×2 quad), so the bias *is* the effective LOD.
  Frontend translates Bias to an `Op::ImageSampleExplicitLod`
  carrying the bias as the lod operand; the existing
  `sample_2d_lod` (#16) helper handles the rest.  No new
  helpers; no backend changes.
  Projective texturing (Arc 37): `OpImageSampleProjImplicitLod`
  / `OpImageSampleProjExplicitLod` lower at the frontend with
  `Op::VectorExtract` (per-lane peel) + `Op::FDiv` (per-lane
  divide by the last lane `q`) + `Op::ConstVec` (rebuild the
  smaller coord), then dispatch as a normal sample.  Interpreter
  mirrors the divide.  No new helpers, no backend changes.
  Currently 2D-Proj only (vec3→vec2 coord); cube/3D-Proj
  paths are gated by the unsupported divided-lane-count error.
  `textureQueryLod` / `textureSamples` (Arc 38): both lower
  at the frontend to constants — `OpImageQueryLod` →
  `Op::ConstVec([0.0, 0.0])` (no derivatives, so lod = 0 and
  clamped-lod = 0); `OpImageQuerySamples` → `Op::ConstInt(1)`
  (no MSAA).  Interpreter adds matching `Op::ImageQueryLod`
  + `Op::ImageQuerySamples` short-circuits so all three
  runners agree.  No backend changes.
  Shadow samplers (Arc 40): `OpImageSampleDref{Implicit,Explicit}Lod`
  lower entirely at the frontend with no new runtime helpers
  or backend changes.  The op decomposes into:
    `r       = ImageSample{Implicit,Explicit}Lod(coord)`
    `r0      = VectorExtract(r, 0)`
    `cond    = FOrdLe(r0, dref)`
    `result  = Select(cond, 1.0, 0.0)`
  GLSL's `texture(sampler2DShadow, vec3(s, t, dref))` returns
  scalar f32; vec4 results would require a splat and are
  gated Unsupported for now.  Compare op is LESS-OR-EQUAL
  (the canonical shadow case); other compare modes need a
  SamplerDesc field — wire-format work, deferred.
  ConstOffset / Offset on `OpImageFetch` (Arc 41): pure
  frontend lowering.  `texelFetch(tex, coord, lod, offset)`
  → decompose coord and offset into scalar lanes with
  `Op::VectorExtract`, add lane-wise via `Op::IAdd`, rebuild
  the integer coord with `Op::ConstVec`, then dispatch the
  regular `Op::ImageFetch`.  Backends only know scalar IAdd
  so the lane decomposition is mandatory.  Interpreter
  mirrors the offset application.  Grad image-operand is
  rejected (only for ImageSample anyway).  Sample (MSAA) +
  Offset image operand for `OpImageSample*` are deferred.
  Projective shadow samplers (Arc 42): `OpImageSampleProjDref{Implicit,
  Explicit}Lod` compose Arc 37 (proj divide) + Arc 40 (dref
  compare) at the frontend.  Coord lanes ÷ last lane, sample,
  extract R, FOrdLe vs dref, Select 1.0/0.0.  Interpreter
  mirrors the divide + compare in one arm.  Bespoke also
  passes after the Arc 44 regalloc fix below.
  ConstOffset / Offset on `OpImageRead` / `OpImageWrite`
  (Arc 43): same lane-decomposition pattern as Arc 41's
  `OpImageFetch` path.  Factored into a shared
  `lane_add_int_vec()` helper that all three storage-image
  ops (Fetch / Read / Write) now share.  Bespoke + Cranelift
  both honor the offset; new
  `differential_image_write_const_offset` test verifies a
  `gid + ivec2(1, 0)` write produces a column-shifted output.
  Bespoke dead-vector cleanup fix (Arc 44): the cleanup that
  reclaims lane scalars when a `vectors` entry dies only
  checked whether *another currently-live `vectors` entry*
  referenced the lane.id; a downstream future ConstVec
  consumer hadn't yet emitted, so its reference was missed and
  the lane scalar was freed prematurely.  Added a
  `last_use[lane.id] >= i` guard so any lane with a future
  use stays alive.  Surfaced by the ProjDref pattern where
  `c_one` is shared between a constant_composite `uvq` and a
  later vec4 `pixel = vec4(compare, 0, 0, c_one)`: when uvq
  died (post-extract), pixel hadn't been emitted yet, and
  c_one got freed.
  Logical / Any / All (Arc 46): five missing scalar logical
  opcodes and the two vec-bool reductions now compile via
  pure frontend lowering.
    `OpLogicalAnd(a, b)`     → `INe(BitAnd(a, b), 0)`
    `OpLogicalOr(a, b)`      → `INe(BitOr(a, b), 0)`
    `OpLogicalEqual(a, b)`   → `IEq(a, b)`
    `OpLogicalNotEqual(a, b)`→ `INe(a, b)`
    `OpLogicalNot(b)`        → `IEq(b, 0)`
    `OpAny(v)`               → `INe(fold BitOr  across lanes, 0)`
    `OpAll(v)`               → `INe(fold BitAnd across lanes, 0)`
  The final `INe/IEq` step lifts the i32-backed boolean into
  the bespoke backend's `bools` map so a downstream `OpSelect`
  or branch finds it.  `bvec<N>` types are now accepted in
  `OpTypeVector` by aliasing element kind `Bool` to `U32` —
  same bit layout, no separate bool-vec lane class needed.
  Bit-field ops (Arc 47): `OpBitFieldUExtract`,
  `OpBitFieldSExtract`, `OpBitFieldInsert` compile via pure
  frontend lowering using the existing `LShr` / `Shl` / `AShr`
  / `BitAnd` / `BitOr` / `BitNot` / `ISub` ops.  UExtract uses
  `(base >> offset) & ((1 << count) - 1)`; SExtract uses
  `(base << (32 - offset - count)) >> (32 - count)` with the
  arithmetic right shift sign-extending; Insert uses the
  standard mask-clear / mask-place / OR sequence.  Only 32-bit
  operands in v1.
  Floating-point remainder (Arc 48): `OpFRem` (truncated,
  same sign as `x`) and `OpFMod` (floored, same sign as `y`)
  compile via pure frontend lowering.  Both expand to
  `x - y * round(x / y)` where the rounding op is `FTrunc`
  for FRem and `FFloor` for FMod.  The backends never see a
  native `Op::FRem` from this path; everything goes through
  `FDiv` + `FTrunc/FFloor` + `FMul` + `FSub`.
  Signed integer remainder (Arc 49): `OpSRem` (truncated;
  same sign as dividend) compiles via pure frontend lowering
  as `x - y * (x sdiv y)`, using the existing `Op::SDiv` /
  `Op::IMul` / `Op::ISub`.  `OpSMod` (floored; same sign as
  divisor) is also now lowered at the frontend (Arc 50)
  rather than emitting `Op::SMod` and relying on the
  bespoke's incomplete native arm.  The lowering applies
  the standard sign-adjust on top of SRem, using bit-trick
  computations (`>> 31` for sign bits, `r | -r` for nonzero
  detection) so the cond stays in the int/bool pipelines
  the bespoke already handles.
  Runtime-indexed vector access (Arc 51):
  `OpVectorExtractDynamic` and `OpVectorInsertDynamic`
  compile via pure frontend lowering as chains of `Op::Select`
  on statically-extracted lanes.
    Extract(v, idx) → right-fold (idx == k) ? v[k] : acc.
    Insert(v, val, idx) → per lane `new[k] = (idx == k) ? val : v[k]`,
       rebuilt via `ConstVec`.
  Only F32 / I32 / U32 lane types in v1.
  Static composite insert + alias ops (Arc 52):
    `OpCompositeInsert(value, composite, index)` -- pure
       frontend lowering: per-lane `VectorExtract` of the
       source, replace lane `index` with `value`, rebuild via
       `ConstVec`.  Single-level vector inserts only.
    `OpCopyObject(src)` -- aliases the SPIR-V Result Id to
       `src` directly via `id_map`.  No new IR Value, no new
       instruction.
    `OpUndef` -- materializes as `ConstFloat 0.0` / `ConstInt 0`
       per the result type.  Bool maps to int 0.
  `textureSize(sampler2D, lod)` (Arc 34): a new IR op
  `Op::SampledImageQuerySizeLod { image, lod }` is emitted
  by the frontend for `OpImageQuerySizeLod`.  Both backends
  read TexDesc.width @ #8 / height @ #12 directly off the
  X1-anchored uniforms table -- no helper call.  The LOD
  operand is captured for liveness but ignored at codegen
  (single-mip TexDesc); real multi-mip would indirect
  through `mip_descs[lod]`.
  Pixel-quad derivatives (Arc 33): `OpDPdx` / `OpDPdy` /
  `OpFwidth` (+ Fine / Coarse variants) lower at the frontend
  to a zero of the result type (no 2×2 quad in the
  dispatcher).  Shaders that defensively use derivatives
  compile; sampler implicit-LOD continues to collapse to
  mip 0.  Real quad dispatch is a dispatcher refactor and
  is deferred.
  MVP scope: Rgba8Unorm / R32Float / Rgba32Float.  Mip-
  level storage images are supported via `OpImageRead` /
  `OpImageWrite` with `Image-Operands::Lod`: the runtime
  carries `mip_count` + `mip_descs` (pointer to a per-mip
  `ImageDesc` array) on the base descriptor; four new
  helpers (`atrium_img_read_2d_lod` / `..._write_2d_lod` /
  `..._read_3d_lod` / `..._write_3d_lod`) indirect through
  `mip_descs[lod]` when `lod < mip_count`.  The image-table
  helper header grew 32 → 64 B; both backends emit the
  shifted helper-offsets + extra Lod register
  (W3 / W4 / X4 / X5 depending on dim × lod).
- GLSL.std.450 ExtInst dispatch for: FAbs, SAbs, Floor,
  Ceil, Trunc, Fract, FSign, FMod, Sqrt, InverseSqrt,
  FMin, FMax, FClamp, FMix, Step, SmoothStep, Length,
  Distance, Normalize, Reflect, Cross, Sin, Cos, Tan,
  Exp, Exp2, Log, Log2, Pow, Atan, Asin, Acos, Atan2,
  Sinh, Cosh, Tanh, Asinh, Acosh, Atanh,
  SMin, UMin, SMax, UMax, SClamp, UClamp,
  FindILsb, FindSMsb, FindUMsb,
  NMin, NMax, NClamp, PackHalf2x16, UnpackHalf2x16.
  PackHalf2x16/UnpackHalf2x16 lower to the ARM FCVT
  half-precision instructions on the bespoke backend
  (f16 is internal to the op — never an IR type); they
  are bespoke-only, since Cranelift's aarch64 backend does
  not ISLE-lower f16 conversion.
  Sin/Cos/Tan use Horner-form Taylor on a range-reduced
  argument (x → x_red ∈ [-π/2, π/2] mod π, with (-1)^k
  parity sign), so the full real line is accepted at ~6
  ULPs near the reduced domain.  Exp/Exp2 use 5-term
  Horner-form Taylor on a fractional residual (r ∈
  [-0.5, 0.5]) combined with IEEE-754 exponent
  reconstruction (Op::Bitcast f32↔i32 + Op::Shl).
  Log/Log2 use Mineiro-style mantissa-split + 1 FDiv
  rational approximation (~4e-4 relative error). Pow
  is just Exp2(y * Log2(x)).  Atan uses a 6-coefficient
  Horner minimax on [-1, 1] with reciprocal range
  reduction (sign(x)*π/2 - atan(1/x) when |x|>1), so
  the full real line is accepted at ~5e-7.  Asin =
  Atan(x / sqrt(1-x²)); Acos = π/2 - Asin.
  Atan2(y, x) = Atan(y/x) + quadrant bias (±π or 0
  selected on sign(x) × sign(y) via Op::Select); the
  x=0 case rides through Atan's reciprocal branch.
  Sinh/Cosh/Tanh decompose into Exp(x) ± Exp(-x);
  Asinh/Acosh/Atanh into Log(x + sqrt(x²±1)) or
  0.5·Log((1+x)/(1-x)).  SMin/UMin/SMax/UMax/SClamp/UClamp
  lower to Op::Select on the corresponding signed or
  unsigned ordered compare (SLt/ULt/SGt/UGt).
  FindILsb/FindSMsb/FindUMsb lower to new IR Op::Clz +
  Op::Rbit (ARM64 CLZ/RBIT, Cranelift clz/bitrev) with
  Select guards for the x=0 and x<0 corner cases.
  NMin/NMax/NClamp alias to FMin/FMax/FClamp; the IEEE
  754-2008 NaN-suppression semantic (NMin(NaN, x) = x)
  is deferred -- workloads that need it can lower to
  FMINNM/FMAXNM in a follow-up.

**Bespoke backend (atrium-spv-backend-bespoke):**

- Single-pass ISel + linear-scan regalloc, scalar f32 +
  i32 + NEON-packed vec4f.
- Compute calling convention with 1–6 SSBO bindings via a
  descriptor-table prologue (X16, X17, X13, X14, X15, X12
  pre-loaded from X2).
- Op::PtrOffsetDynamic via lsl + add.
- All listed atomics via ARMv8.1 LSE instructions
  (LDADDAL / LDSETAL / LDCLRAL+MVN / LDEORAL / SWPAL /
  LDSMINAL / LDSMAXAL / LDUMINAL / LDUMAXAL / CASAL) so
  they remain correct under the workgroup-parallel
  dispatcher.
- GLSL.std.450 math listed above; vec4 NEON path for
  FAbs/FSqrt/FMin/FMax/FFloor/FCeil/FTrunc, scalar f32 for
  the rest (the synthesised ones ride through the
  underlying primitive ops' existing vec dispatch).
- Architectural perf wins: unused-prologue-NOP truncation
  (–36 bytes per function header in the common case),
  identity-Phi-move peephole, classifier extensions for
  the unary math ops.

**Cranelift backend (atrium-spv-backend-cranelift):**

- Mirror of the bespoke feature set for the fallback
  path; per-lane vec4 via the existing emit_float_unop /
  emit_float_binop helpers.
- Cross-backend differential test framework
  (atrium-spv-backend-bespoke/tests/differential_compute.rs)
  exercises 41 distinct shader shapes for byte-identical
  output, including a real-world histogram and a
  reflect/normalize lighting kernel.

**Host (aqueduct-gpu-host):**

- Tier2Backend workgroup-parallel dispatcher with per-
  binding descriptor-table assembly, per-binding readback,
  and pre-fill API for SSBO inputs.  Workgroups are
  partitioned across `std::thread::available_parallelism()`
  worker threads via `std::thread::scope`; within each
  workgroup the local-invocation loop stays serial so the
  eventual shared-memory + ControlBarrier semantics can be
  layered in cleanly per workgroup.
- Tier2ComputeStateBlob carries `ssbo_binding_count`
  alongside LocalSize.

**ICD (atrium-vk-icd):**

- vkCreateComputePipelines threads LocalSize + SSBO
  binding count from a SPIR-V scan onto the compute
  state blob.
- `ATRIUM_SPV_FORCE_BACKEND` env var pins a specific
  backend for testing + bisecting cross-backend drift.

**Verified workloads end-to-end:**

- Empty + constant-store compute (the floor case).
- Per-pixel parallel writes via gid_x indexing.
- Histogram (atomicAdd into a dynamically-indexed bin).
- Lighting math (length, normalize, reflect, smoothstep).
- Multi-binding SSBO routing across 1–6 bindings.

**Known gaps:**

- (closed 2026-05-21) V-pool exhaustion on chains of 5+
  vec4 ops that share Load-synth lanes.  Fixed via the
  copy-on-extract approach: OpVectorExtract now emits one
  `mov v.16b` (f32) or `mov w` (i32) to copy into a fresh
  register owned by the extract result, removing the
  V-reg aliasing that previously prevented the source
  vec's lanes from expiring.  The copy uses an ORR-form
  move that target cores rename-eliminate (zero-latency),
  so the runtime cost is approximately zero.  The
  vec-lane synth-liveness expire pass then reclaims lane
  V-regs when their TOP Value's last_use has passed.
- The bespoke bool W-pool is 3 slots (W10..W12) with
  recycle-on-ConvertUToF; arbitrary-hole free-list lands
  if real workloads exceed the eager-free pattern.
- Multi-binding cap is 6 (driven by the spare-X-reg
  count); shaders needing 7+ bindings fall back to
  Cranelift.

Phases 8–9 (peephole + perf polish) are iterative; the
next concrete arcs are listed in the Known gaps + section
9.6 (encoder gaps).
