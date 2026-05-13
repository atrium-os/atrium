# Tier-2 Renderer — AOT software Vulkan for Atrium

> **Status.** Design. No code yet.
>
> **Companion docs.** Read `aqueduct-gpu.md` (the GPU dispatch
> protocol) and `atrium-pkg.md` (Atrium's bundle / install
> story) first. This spec defines the second renderer tier
> on the daemon side of aqueduct-gpu, and the install-time
> shader-compile pipeline that feeds it.
>
> **One-line summary.** Tier-2 is the software renderer for
> third-party Vulkan apps whose shaders aren't part of Atrium's
> built-in bundle catalog. Shaders are translated SPIR-V → Rust
> → native `.so` *outside* the renderer process, cached on disk
> by content hash, and dlopen'd by the daemon at runtime. The
> daemon never runs a compiler; the renderer's hot loop only
> ever calls native code it loaded from disk.

---

## 1. Why we're doing this

Tier-1 (`aqueduct-gpu-host/src/software/`, the tiny-skia backend)
handles Atrium-native bundle pipelines: rect, path,
textured-rect, glyph_run. That covers every drawable in
fresco-server's scene graph and every renderer the in-tree
Atrium apps need. When atrium-vk-icd routes a third-party
SPIR-V pipeline at it, tier-1 returns:

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

The constraint that shapes the entire design: **no JIT, no
interpreter in the renderer's hot path.** Every per-pixel
shader call must be a regular function call into native code
that was compiled by a normal Rust toolchain at some point
*before* the renderer launches. Interpretation is too slow
for any non-trivial fragment shader (rule of thumb: 50–200×
slower than native). JIT compiles at runtime, lives in
mutable executable pages, and turns the renderer into a
security headache (RWX memory, codegen bugs become RCE
surfaces, can't audit what code runs in the hot loop). Both
are out.

The non-obvious move is that "no JIT" *doesn't* mean "no
SPIR-V → native translation" — it just means the translation
must happen in a separate process at a defined point in time
and produce a `.so` on disk. Once that file exists, the
renderer's relationship to it is the same as its relationship
to any other linked library: dlopen, grab symbols, call.

---

## 2. The execution model — AOT-on-first-use

The pattern has a name, used by Android's `dex2oat`, Mono's
`--aot`, GCJ, and several research VMs:

```
                     ┌──────────────────────────────────────────┐
                     │  vkCreateShaderModule(device, spirv)     │
                     │  (daemon-side, in tier-2 session router) │
                     └──────────────────────────────────────────┘
                                       │
                                       ▼
                        hash = sha256(spirv_bytes)
                        path = /var/atrium/shaders/v{N}/{hash}.so
                                       │
                ┌──────────────────────┴─────────────────────┐
                │                                            │
        path.exists()?                              path doesn't exist?
                │                                            │
                ▼                                            ▼
        ┌───────────────┐                  ┌─────────────────────────────┐
        │ dlopen(path)  │                  │ spawn atrium-spv-compile    │
        │ grab vs_main, │                  │   pipe SPIR-V bytes to it,  │
        │   fs_main     │                  │   wait for child to write   │
        │ cache module  │                  │   {hash}.so + exit          │
        │ return        │                  │ (runs in its own Portcullis │
        └───────────────┘                  │  jail; no FS write except   │
                                           │  the cache dir.)            │
                                           └─────────────────────────────┘
                                                         │
                                                         ▼
                                                 (then dlopen path)
```

Three properties fall out of this design:

1. **Cold-launch latency on first install** — typically 1–5 s
   per shader for `rustc --opt-level 3`. Happens once, ever,
   per `(shader-bytes, ABI-version)` pair. After that the
   cache is warm forever.

2. **Renderer process never runs a compiler.** It only does
   dlopen + symbol lookup + native function calls. No JIT
   pages, no in-process codegen, no surprises in the hot
   loop.

3. **Compile failure is recoverable.** If `atrium-spv-compile`
   exits non-zero on a malformed SPIR-V, the daemon returns
   `VK_ERROR_INVALID_SHADER_NV` from `vkCreateShaderModule`
   and the app's normal error path takes over — same as a
   real driver rejecting a bogus shader.

### When does the compile happen?

Two trigger points, both supported:

**(a) Install-time, driven by `atrium-pkg`** — the
happy path. When an Atrium-shipped app installs, `atrium-pkg`
scans the bundle's shader manifest (a static list of SPIR-V
blobs the bundle declares it uses), runs `atrium-spv-compile`
on each one, and lands the resulting `.so` files in the
shader cache before the app ever runs. First launch is
instant.

**(b) First-`vkCreateShaderModule`, on demand** — the
fallback. Apps that synthesize shaders at runtime (shader-
permutation engines, RenderDoc-style capture replay tools,
debug builds with hot-reload) hit this path. The vkCreate
call blocks for the compile, then proceeds. Slow on first
encounter, instant thereafter.

The (a) path is preferred because it makes the install ↔
runtime contract explicit: the bundle declares its shader
dependencies, atrium-pkg satisfies them ahead of time, and
the runtime can detect a missing shader and report it
honestly (vs. a runtime app that synthesises a shader on
the fly). It also keeps untrusted compile work out of the
hot path. The (b) path is the safety valve for binaries
that don't go through Atrium's installer at all.

---

## 3. The four decisions, locked

> Where this spec deviates from "obvious" it's deliberate.
> These are the four decisions whose alternatives were
> explicitly considered.

### D1. AOT-on-first-use, not JIT

**Decision.** All SPIR-V → native code generation happens in
`atrium-spv-compile`, in its own process, before the daemon
ever calls the shader. Output is a `.so` on disk.

**Why not JIT (LLVM/Cranelift).** JIT is the standard answer
for SW Vulkan (llvmpipe). For Atrium it's wrong because:
runtime codegen in a privileged daemon is a security concern
(JIT'd pages are W+X by definition; bugs in the codegen
become RCE); the codegen latency is paid every launch (no
cross-run cache); LLVM is a heavyweight dependency that
prevents us from shipping the daemon as a single self-
contained Rust binary.

**Why not an interpreter.** Per-pixel fragment-shader work
done by interpreting SPIR-V opcodes is 50–200× slower than
native. The whole point of tier-2 is to be a credible
software renderer, not a fallback for "at least it draws
something". Interpretation is fine for compute-shader-light
debug paths but is the wrong primitive for the per-pixel
hot loop.

### D2. Output language is Rust

**Decision.** `atrium-spv-translate` emits Rust source.
`atrium-spv-compile` invokes `rustc --crate-type cdylib
--opt-level 3 --target $atrium_target` to produce the `.so`.

**Why Rust.** Matches the rest of Atrium's userspace
language policy (`docs/LANGUAGE-POLICY.md`). Lets the
translator emit straightforward Rust that LLVM-via-rustc
optimises hard. Gives shader code access to `std::simd` for
hand-vectorisable patterns. Calling convention from the
rasterizer (also Rust) is zero-cost; no FFI marshalling.

**Why not LLVM IR directly.** Would skip the `rustc`
frontend but would lock us into LLVM's exact IR — a moving
target — and lose the readability that lets us debug
generated shaders by reading the source. The `rustc`
dependency at install time is acceptable: Atrium dev
machines have it, and end-user machines can either depend
on `rust` package or ship a stripped `rustc`-as-shader-
compiler (decision deferred).

### D3. Compilation runs in a jailed sub-process

**Decision.** `atrium-spv-compile` is a standalone binary,
not a library linked into the daemon. It runs under
Portcullis with no capabilities except read on
`/var/atrium/shaders/incoming/<hash>.spv` (the SPIR-V the
daemon dropped for it) and write on
`/var/atrium/shaders/v{N}/<hash>.so` (the result).

**Why a sub-process.** Two reasons. First: rustc is itself
a large attack surface, and we don't want untrusted SPIR-V
processed in the same process tree as the renderer.
Second: it makes the daemon's `cargo` dependency tree
strictly smaller — no `rspirv`, no `naga`, no codegen
machinery; the daemon links only against the runtime
shader-cache table.

### D4. Cache versioning lives in the path

**Decision.** Cache layout is `/var/atrium/shaders/v{N}/
<sha256>.so` where `N` is the shader-ABI version. ABI
changes (new uniform-layout rule, varying-passing
convention change, calling-convention bump) bump `N`. Old
versions' directories become unreachable; first-launch
post-bump recompiles transparently.

**Why path-based versioning.** Same playbook Tessera uses
for its content-addressed pack format. Lets multiple ABI
versions coexist during a daemon upgrade transition. A
periodic `atrium-pkg gc shader-cache` sweeps unused
versions.

---

## 4. The shader ABI

This is the load-bearing contract between
`atrium-spv-translate`'s output and tier-2's rasterizer.
It is the surface that the cache-version (D4) protects.

### 4.1 Exported symbols

A compiled shader `.so` exports exactly the symbols that
its SPIR-V entry points declared, prefixed with `atrium_`:

```rust
// Vertex shader.
#[no_mangle]
pub unsafe extern "C" fn atrium_vs_main(
    in_attributes:    *const u8,  // packed per-vertex attribute bytes
    in_attr_strides:  *const u32, // stride per binding (for indexing)
    uniforms:         *const u8,  // descriptor-set-flattened uniform block
    push_constants:   *const u8,  // 128 bytes max per PhysicalDeviceLimits
    vertex_index:     u32,
    instance_index:   u32,
    out_position:     *mut [f32; 4],   // gl_Position
    out_varyings:     *mut u8,         // packed varying-out bytes
    out_clip_distance: *mut f32,       // optional; null if shader didn't write it
);

// Fragment shader.
#[no_mangle]
pub unsafe extern "C" fn atrium_fs_main(
    in_varyings:    *const u8,    // interpolated per-pixel varyings
    uniforms:       *const u8,
    push_constants: *const u8,
    frag_coord:     [f32; 4],     // gl_FragCoord
    samples_mask:   u32,          // gl_SampleMask in (currently always 0x1)
    out_color:      *mut [f32; 4],
    out_depth:      *mut f32,     // optional; null if shader didn't write gl_FragDepth
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

Each emitted shader also exports a metadata blob the
daemon reads at dlopen time to validate the ABI version
and discover attribute/varying/uniform layout:

```rust
#[no_mangle]
pub static ATRIUM_SHADER_METADATA: AtriumShaderMetadata = ...;

#[repr(C)]
pub struct AtriumShaderMetadata {
    pub abi_version:      u32,   // must == TIER2_SHADER_ABI_VERSION
    pub stage_kind:       u32,   // 0=vertex, 1=fragment, 2=compute, ...
    pub attr_count:       u32,
    pub attr_offsets:     [u32; 16],   // per-attribute byte offset
    pub attr_formats:     [u32; 16],   // VkFormat numeric
    pub varying_count:    u32,
    pub varying_offsets:  [u32; 16],
    pub varying_formats:  [u32; 16],
    pub uniform_size:     u32,         // bytes of the flattened uniform block
    pub local_size:       [u32; 3],    // compute only
}
```

### 4.2 Layout rules

- **Vertex attributes** are packed per-binding in the order
  declared by `VkPipelineVertexInputStateCreateInfo`. The
  rasterizer's vertex-fetch step gathers bytes per binding/
  stride into a contiguous buffer the shader sees as
  `in_attributes`. Padding follows std430 rules for
  alignment within the buffer.

- **Varyings** are packed in `location` order, std430-style.
  Perspective-correct interpolation coefficients are
  computed by the rasterizer and applied before the
  fragment shader sees `in_varyings`.

- **Uniforms** from all bound descriptor sets are flattened
  into one contiguous block at `vkCmdBindDescriptorSets`
  time. The translator emits accesses that point into the
  flat block via fixed offsets computed at translate time
  from the descriptor-set layout. No indirection at run
  time.

- **Push constants** are passed as a separate 128-byte block
  (capped by `maxPushConstantsSize` advertised in
  `PhysicalDeviceLimits`). The translator generates direct
  reads off this pointer.

- **Sampled images** are special: the translator emits a
  call into `atrium-vk-icd-rt::sample_image_2d` (or 1D/3D/
  cube variants) — a small runtime library tier-2 ships
  alongside the rasterizer. Sampler state (filter, wrap,
  border colour) is in the uniform block. This keeps the
  per-shader `.so` small and lets us improve sampling
  quality across all shaders by upgrading one library.

### 4.3 ABI versioning

`TIER2_SHADER_ABI_VERSION` is a build-time `u32` constant
in both the daemon and the translator. Bumped when any of:
- a symbol name changes
- a function signature changes
- the metadata struct layout changes
- the per-shader runtime library's API changes
- the uniform/varying layout rules change

The daemon checks the `.so`'s `abi_version` field on
dlopen and rejects mismatches (returns the loaded module
to the cold-compile path under the new `v{N}/` directory).

---

## 5. The compile pipeline

Two binaries + one library.

### 5.1 `atrium-spv-translate` (library + thin CLI)

Takes SPIR-V bytes, emits a `.rs` source string + a
`AtriumShaderMetadata` populated from the SPIR-V's
reflection.

```rust
pub fn translate(spirv: &[u8]) -> Result<TranslatedShader, TranslateError>;

pub struct TranslatedShader {
    pub rust_source: String,
    pub metadata: AtriumShaderMetadata,
}
```

Implementation: `rspirv` for the parse, a custom emitter
that walks the SPIR-V `Function` records and prints
matching Rust. The emitter is the bulk of the work — every
SPIR-V opcode gets a Rust mapping, with care taken for the
ones that don't have direct equivalents (texture sample
ops become library calls; control-flow with `OpPhi`
becomes labelled blocks). Roughly 3–6 weeks of careful
work for a useful first cut covering the GLSL 4.50 core
subset that the Khronos samples use.

### 5.2 `atrium-spv-compile` (binary)

Glue around translate + rustc:

```
Usage: atrium-spv-compile --input <spirv> --output <so>
       [--opt-level <0|1|2|3>] [--target <triple>]
```

Reads SPIR-V from `--input` (or stdin), calls
`atrium-spv-translate::translate`, writes the result to a
tempdir, invokes `rustc --crate-type cdylib
--edition 2024 -C opt-level=3 -C target-cpu=native
<tempdir>/shader.rs -o <output>`, exits 0 on success.

Designed to be jailed: no network, no FS access outside
the input path and output path and a private tempdir.

### 5.3 `atrium-vk-icd-rt` (the per-shader runtime library)

A small `staticlib` linked into every compiled shader
`.so`. Contains the texture-sampling kernels, the
fixed-function math helpers (perspective divide,
barycentric interpolation receivers — actually the
rasterizer side calls these so they live in the daemon,
not in the .so; this library is only stuff the *shader*
calls).

Surface area should stay tiny — a handful of functions —
to minimise churn against the ABI version.

---

## 6. The Tier2Backend interface

A new module `aqueduct-gpu-host/src/tier2/` parallel to
`software/`. The top-level type:

```rust
pub struct Tier2Backend {
    shader_cache: Mutex<HashMap<ShaderHash, LoadedShader>>,
    pipelines:    Mutex<HashMap<ResourceId, Tier2Pipeline>>,
    rasterizer:   RefCell<Rasterizer>,
    // metrics, etc.
}

struct LoadedShader {
    handle:        libloading::Library,
    vs:            Option<unsafe extern "C" fn(...)>,
    fs:            Option<unsafe extern "C" fn(...)>,
    cs:            Option<unsafe extern "C" fn(...)>,
    metadata:      AtriumShaderMetadata,
}

struct Tier2Pipeline {
    vs:                  unsafe extern "C" fn(...),
    fs:                  unsafe extern "C" fn(...),
    raster_state:        RasterState,    // cull, fill, blend, depth/stencil
    vertex_input:        VertexInputState,
    descriptor_layout:   DescriptorLayout,
}

impl Backend for Tier2Backend {
    fn identity(&self) -> BackendId { BackendId::new(GpuVendor::Software, 1) }
    // ... usual Backend trait methods.
    fn submit_frame(&self, ...) -> Result<(), String> {
        // Walks FrameOp stream like SoftwareBackend does,
        // but the BindPipeline / Draw* dispatch goes through
        // self.rasterizer.draw(...) which calls the
        // pipeline's vs/fs function pointers per vertex/pixel.
    }
}
```

### Trait change worth scoping

Tier-2 needs per-draw state that tier-1 doesn't care
about: vertex buffer pointers + strides, full viewport,
render-target metadata. The existing `Backend` trait
flattens many of those because tier-1 doesn't need them.
Two options:

**(a)** Bump the `Backend` trait to pass the full state.
Tier-1 ignores fields it doesn't care about.
**(b)** Add a `tier_caps()` accessor and split the
`submit_frame` path: tier-1 takes the existing simplified
flattened state; tier-2 takes a richer struct.

Recommendation: (a). One source of truth, no
duplicated dispatch, minor cost to tier-1's
trait-method size. Decided when the
`Tier2Backend` skeleton lands.

---

## 7. The rasterizer

The other half of the renderer. Self-contained, no SPIR-V
or shader concerns; takes a Tier2Pipeline + draw call +
bound buffers and writes pixels.

### 7.1 Pipeline stages

```
        ┌────────────┐
        │ Index/draw │  vkCmdDraw / DrawIndexed / DrawIndirect
        │  walker    │
        └─────┬──────┘
              ▼
        ┌────────────┐
        │ Vertex     │  → calls pipeline.vs per vertex
        │ assembly + │  → outputs gl_Position + varyings
        │ shading    │
        └─────┬──────┘
              ▼
        ┌────────────┐
        │ Primitive  │  → 3 vertices per triangle (handles topology,
        │ assembly   │     index buffer, instancing)
        └─────┬──────┘
              ▼
        ┌────────────┐
        │ Clipping   │  → against view frustum + per-vertex
        │            │     gl_ClipDistance
        └─────┬──────┘
              ▼
        ┌────────────┐
        │ Perspective│  → clip-space → NDC
        │ divide     │
        │ Viewport   │  → NDC → screen-space
        │ transform  │
        └─────┬──────┘
              ▼
        ┌────────────┐
        │ Triangle   │  → edge functions, bounding rect,
        │ setup      │     perspective-correct interpolation coefs
        └─────┬──────┘
              ▼
        ┌────────────┐
        │ Tiled      │  → 8×8 or 16×16 tiles
        │ rasteriser │  → per-pixel inner loop:
        │            │       interpolate varyings
        │            │       depth test
        │            │       fragment shader call
        │            │       blend
        │            │       depth/stencil/colour write
        └────────────┘
```

### 7.2 Performance budget

Aspirational, not blocking on first iteration:

- 1280×800 fullscreen quad with a 50-instruction fragment
  shader: target ~16 ms / frame on an M4 Max single core.
- Multi-thread the tile raster across N cores in iteration
  3; expect ~linear scaling up to memory bandwidth.
- SIMD-vectorise the per-pixel inner loop with `std::simd`
  in iteration 4 (4-wide or 8-wide pixel quads).

First iteration is scalar single-thread; the architecture
is laid out to make those upgrades drop-in.

---

## 8. Implementation phases

**Phase 1 — translator MVP.** `atrium-spv-translate`
handles the no-vertex-shader pass-through case: a built-in
fullscreen-triangle vertex shader, an arbitrary SPIR-V
fragment shader using only constant colour + uniform
reads. Output: `.rs` source emitter for ~40 SPIR-V
opcodes. Deliverable: standalone CLI that compiles a
trivial GLSL fragment shader to a working `.so`. (~3
weeks)

**Phase 2 — rasterizer skeleton.** `Tier2Backend` with
scalar single-thread rasterizer, no MSAA, single colour
attachment, no depth, no blend modes beyond opaque. Drives
a triangle to pixels by hand-injecting a `Tier2Pipeline`.
Deliverable: a unit test that handed three vertices +
trivial shaders produces the expected RGBA8 buffer.
(~2 weeks)

**Phase 3 — wire end-to-end.** Tier-2 picked by the
session router for `id(icd-runtime, *)` pipelines.
Daemon-side cache + on-demand compile via
`atrium-spv-compile`. End-to-end VM test: drive
atrium-vk-icd's headless_triangle example against a daemon
running with tier-2 enabled, observe a real rendered
triangle. (~2 weeks)

**Phase 4 — atrium-pkg install hook.** Bundle manifest
scanner; pre-compile bundle shaders at install time;
ensure runtime is always cache-hit for properly installed
apps. (~1 week)

**Phase 5 — broaden the translator.** Vertex shaders with
attribute fetch, varyings, more SPIR-V opcodes, texture
sampling. Iterative; one test app at a time. (Open-ended)

**Phase 6 — depth/stencil + blend + MSAA + multi-attachment.**
Rasterizer features. Iterative. (Open-ended)

**Phase 7 — multi-thread + SIMD.** Performance work.
(Open-ended)

Phases 1–4 are the "first credible third-party Vulkan app
runs through atrium-vk-icd via tier-2" milestone. Estimate
~8 weeks of focused work.

---

## 9. Open questions

- **rustc as a runtime dependency on user systems.** Fine
  on dev machines. On end-user atrium systems we either
  add `rust` to the base or ship a stripped-down `rustc`-
  as-shader-compiler bundle. Decision deferred until
  Phase 4 ↔ atrium-pkg.

- **Shader IL choice for the cache key.** Today: sha256 of
  the SPIR-V bytes. Two apps that compile from the same
  GLSL source through different SPIR-V optimisation
  passes get different cache entries. Tolerable for v1.
  A future "canonicalise SPIR-V before hashing" pass
  could de-duplicate, but adds translator-side complexity
  for modest cache-size savings. Defer.

- **Per-shader runtime library distribution.** If
  `atrium-vk-icd-rt` is statically linked into every
  shader `.so`, each `.so` carries its copy of the
  texture-sample kernels (~100 KB after opt). On a system
  with hundreds of installed Atrium apps this is a few
  tens of MB of dup. Alternative: ship the runtime as a
  separate `.so` and have shader `.so`s reference it via
  dynamic symbol resolution. Decision deferred to Phase
  3 once we measure typical shader-`.so` size.

- **Compute-shader workgroup execution model.** Tier-2's
  rasterizer naturally parallelises across tiles for
  graphics. Compute shaders need their own dispatcher:
  per-workgroup × per-invocation iteration. First-cut:
  scalar single-thread iteration matching the SPIR-V
  invocation count. Multi-thread is Phase 7.

- **vkCmdDispatchIndirect** with a count buffer the
  daemon hasn't seen the contents of. Tier-2's compute
  dispatcher needs to read the count buffer at draw time.
  The aqueduct-gpu protocol already plumbs buffer
  contents (CopyBufToImg path); a similar
  `READ_BUFFER_FOR_DISPATCH` op or equivalent might
  be needed. Defer until Phase 5 / 6.

- **Validation-layer hostility.** Apps with
  `VK_LAYER_KHRONOS_validation` loaded will probe a lot
  of state our tier-2 doesn't honour. Most are already
  fine because our PhysicalDeviceLimits answer is real
  (commit `7092903`). Worth running the layer in CI
  against Phase-3 builds to flush out any remaining
  surprises.

---

## 10. Relationship to other Atrium components

- **atrium-vk-icd.** Already routes third-party shaders as
  `id(icd-runtime, *)`. No code changes required to start
  hitting tier-2; the dispatch decision is daemon-side.

- **aqueduct-gpu protocol.** No new opcodes for tier-2's
  steady state. Phase 5+ may add `READ_BUFFER_FOR_DISPATCH`.

- **atrium-pkg.** Owns the install-time shader-precompile
  hook (Phase 4). The bundle manifest format gets a
  `shaders: [...]` array listing the SPIR-V blobs the
  bundle declares.

- **Portcullis / jails.** `atrium-spv-compile` runs in a
  jail with FS caps for the input/output paths only.

- **Tessera.** The `/var/atrium/shaders/v{N}/` cache lives
  on a Tessera-backed volume; cache eviction integrates
  with the standard Tessera GC policy (LRU on
  last-dlopen time).

- **Future tier-3 (real GPU).** When native atrium-gpu
  drivers land at D5+, they'll have their own
  shader-compile pipeline (probably HW vendor-specific).
  Tier-2 stays as the universal-compatibility software
  fallback, the way Mesa's llvmpipe stays alongside
  vendor drivers.

---

## 11. What we are NOT building

- A JIT. Period.
- A SPIR-V interpreter. Even for debug builds; the AOT
  pipeline is fast enough on cold compile that an
  interpreter wouldn't be meaningfully faster to first
  pixel.
- A general OpenGL ES path. Tier-2 is Vulkan-shaped only;
  apps that ship GLES go through a separate translator
  (out of scope here).
- A GPU-shaped tile binner with vertex pre-pass. Tier-2
  is a forward immediate-mode rasterizer. Performance
  ceiling is "credible for an Atrium curated app, not for
  AAA games" — that's the right ceiling.
- Vulkan extensions beyond core 1.3 plus what
  atrium-vk-icd already advertises. Ray tracing, mesh
  shaders, video — all out of scope.

---

## 12. Status / next steps

This is the design doc. No code yet.

If approved, the next deliverable is a phase-1
`atrium-spv-translate` skeleton — `rspirv` parse + a Rust
emitter walking the SPIR-V module tree, emitting matching
Rust for the GLSL 4.50 fragment-shader core subset. That's
the riskiest piece of the design; building it first
de-risks everything downstream.

After phase 1: phase 2 (rasterizer skeleton) and phase 3
(end-to-end wiring) can land roughly in parallel. Phase 4
(atrium-pkg integration) is gated on phase 3 being
demonstrably working in the VM.
