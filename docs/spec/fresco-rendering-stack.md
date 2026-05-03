# Fresco rendering stack — architecture and rationale

The Atrium graphics stack makes one load-bearing architectural
decision: **the boundary apps see is at the *scene level*, not the
GPU-command level.** Everything else falls out of that decision.
This document records what the layering is, why each boundary is
where it is, and (importantly) what we deliberately are NOT.

## 0. The layered model

```
   ┌────────────────────────────────────────────────────────┐
   │  App                                                   │
   │    emits scenegraph commands over Unix socket          │
   │    (rectangles, paths, textures, glyphs, transforms,   │
   │     plus high-level ops: GLOBAL_ILLUMINATION, etc.)    │
   └────────────────────────────────────────────────────────┘
                              │   ── Top contract: Atrium scenegraph protocol
                              │      Stable, ours, small (~50-200 ops).
                              ▼
   ┌────────────────────────────────────────────────────────┐
   │  fresco-server                                         │
   │    ├─ scene state machine (CAS + SceneGraph + slots)   │
   │    └─ engine extension layer                           │
   │         ├─ atrium-native renderer                      │
   │         ├─ ueke (UE compat extension, hypothetical)    │
   │         ├─ godot extension (hypothetical)              │
   │         └─ vendor-supplied extensions (future)         │
   │       Each extension lowers high-level ops to Vulkan   │
   │       calls + SPIR-V shaders.                          │
   └────────────────────────────────────────────────────────┘
                              │   ── Bottom contract: Vulkan + SPIR-V
                              │      Stable, theirs (Khronos), large.
                              ▼
   ┌────────────────────────────────────────────────────────┐
   │  Vulkan driver                                         │
   │    Mesa for open-source vendors (Intel/AMD/Mali/...)   │
   │    Proprietary for NVIDIA/Apple Silicon                │
   │    atrium-virtio-gpu for our virtualized environment   │
   │    (Vulkan-over-virtio-gpu via virgl is one option;    │
   │     a native scenegraph rasterizer is another.)        │
   └────────────────────────────────────────────────────────┘
```

Two API contracts that face stakeholders:

- **Top contract (us → app developers): Atrium scenegraph protocol.**
  Small. Stable. Curated by us. Apps targeting Atrium write
  against this and only this.
- **Bottom contract (us → GPU vendors): Vulkan.**
  Large. Stable. Curated by Khronos. Vendors are already
  shipping Vulkan drivers; we don't ask them to do new work.

Everything between the two contracts (the middle layer of fresco-
server, including engine extensions) is implementation. We can
rewrite it freely without breaking either contract.

## 1. Why the top boundary is at the scene level

Apps emit scene structure ("a rectangle here, this text glyph there,
this transform"), not GPU commands ("bind this pipeline, set this
descriptor set, draw 6 vertices"). This is the original Fresco bet.

Direct consequences (mostly wins):

- **App developer surface is tiny.** Writing a Fresco app is closer
  to writing CSS than writing OpenGL. The whole atrium-edit-socket
  app is ~600 lines; the Vulkan equivalent would be 5-10k.
- **Composition is first-class.** The window manager doesn't stitch
  finished buffers from N processes; it operates on one
  scenegraph. Effects (drop shadow, blur, animation) are scenegraph
  ops, not "ask each app to render to a buffer then composite via
  Vulkan."
- **Network transparent by construction.** The protocol IS the
  API. Forwarding it over TCP gives you remote display with no
  X11-style "we kludged remoting in" architecture.
- **Security: apps never touch GPU memory.** No DMA-BUFs handed
  across process boundaries; no "compromised app reads another
  app's textures via dangling buffer handle." The compositor owns
  all GPU resources.
- **Verifiability.** Same scenegraph rendered offline in CI =
  pixel-identical to what the user sees. Golden-image tests work.
- **Backend-uniform.** Same client runs against tiny-skia (today's
  CPU rasterizer), against fresco-gpu-vulkan (planned), against a
  remote rasterizer. No app changes.

Direct consequences (real costs):

- **Apps cannot ship custom shaders.** They describe WHAT, not HOW.
- **No existing graphics apps run unchanged.** Firefox, Blender,
  Steam games — all assume Vulkan/GL/EGL. We've structurally
  precluded "drop-in replacement for Linux desktop." (This is fine
  if Atrium is a curated appliance OS; not fine if it's a
  general-purpose desktop replacement.)
- **The server's render loop is a contention point.** Fifty
  animating apps → server is the bottleneck. Mitigated by per-app
  GPU contexts inside fresco-server, parallel scenegraph traversal,
  etc., but that's our problem to solve.

## 2. Why the bottom boundary is Vulkan

The natural temptation, having decided "apps see scenegraph," is to
push the abstraction all the way down: design our own device-
independent IR, ask vendors to implement backends for it. Don't do
that. **At the bottom we use Vulkan + SPIR-V, not a competing IR
of our own.**

The argument is short:

| What new GPU vendor has to do | Path A (we ship our own IR) | Path B (we use Vulkan) |
|---|---|---|
| Existing investment | None — they don't know Atrium-IR | Already shipping Vulkan driver |
| New work to support Atrium | Implement full Atrium-IR backend | None |
| Incentive | Convince vendor to do unpaid work | Already done |

There's no path where vendors implement an Atrium-specific GPU IR
because we asked nicely. Vulkan is what they already ship; we
should consume it.

The bottom-of-the-stack equivalence is then:

- **Mesa** (open-source GPU drivers) becomes our Vulkan implementation
  for Intel, AMD, Mali, etc. We don't compete with Mesa; we sit
  above it.
- **Proprietary vendor drivers** (NVIDIA, Apple Metal-via-MoltenVK,
  closed Mali) are usable the same way.
- **atrium-virtio-gpu** (our existing kmod) continues to serve VM /
  embedded targets. Either as a Vulkan-over-virtio-gpu transport
  (virgl-style) or as a native scenegraph rasterizer for tightly
  controlled environments.

We are NEVER trying to BE Mesa. We are trying to *consume* Mesa
(plus other Vulkan drivers) from a higher abstraction layer.

## 3. The middle layer: engine extensions

Between the two stable contracts lives the engine extension layer.
This is where the "do it for me" part of the high-level scenegraph
gets fleshed out.

### 3.1 The shape of an extension

An engine extension is a shared library (`.so` / `.dylib`) loaded
into fresco-server at startup. It exposes:

```c
struct AtriumExtension {
    const char *name;            /* e.g. "atrium-core", "ueke", "godot" */
    uint32_t    version;
    AtriumOpHandler *ops;        /* op_id → handler fn */
    size_t       n_ops;
    AtriumCaps   caps;           /* what this extension can do */
};
```

Handlers receive a high-level op (e.g. `GLOBAL_ILLUMINATION` with
its parameter struct) and emit Vulkan calls + SPIR-V shaders into
the per-frame command buffer. fresco-server provides the Vulkan
context, the render targets, the synchronization.

The extension is written ONCE against Vulkan; it runs on every GPU
that has a Vulkan driver. **No per-vendor engine extensions.**

### 3.2 Where shaders live

Shaders live in engine extensions, written in GLSL or SPIR-V
directly. They are NOT exposed at the app boundary.

Total industry shader-writing burden goes from O(thousands of apps
each writing their own) to O(N engine extensions writing one canonical
implementation each). Order-of-magnitude reduction.

### 3.3 Why this is not Mesa

The extension layer looks superficially like Mesa: there's source
(scenegraph ops), there's an intermediate decomposition (extension's
internal lowering), there's Vulkan output. But:

- We do NOT publish the extension's intermediate form as a
  standardized IR. Each extension's lowering is its own internal
  detail.
- We do NOT ask vendors to implement against our intermediate form.
  The intermediate form is "whatever the extension uses to organize
  its Vulkan calls" — a private structure.
- We do NOT include a shader compiler. Extensions emit SPIR-V
  directly (or GLSL→SPIR-V via existing tools like glslang), and
  Vulkan drivers handle the rest.

Compare Mesa, which DOES publish an IR (NIR), DOES expect vendor
backends to consume it, DOES include a full shader compiler stack.
Mesa is the equivalent of "extension layer + IR standardization +
shader compiler + vendor backends" in one project. We are the
"extension layer" only — three of those four roles are filled by
existing software (Vulkan/SPIR-V/Mesa-and-friends/vendor drivers).

### 3.4 Extensibility model

New rendering capabilities arrive as new extensions OR as new ops
within an existing extension:

- **Atrium-core extension** ships canonical scene primitives:
  rectangles, paths, textures, glyphs, transforms, basic 3D mesh.
  This is the bulk of what apps actually use.
- **Engine compat extensions** (UE, Godot, others if/when they
  port) emit their engine's high-level ops and lower them to
  Vulkan. App code targeting those engines runs on Atrium without
  modification of the engine's app-facing API.
- **Vendor / research extensions** (hypothetical: NVIDIA-supplied
  RT, Apple-supplied Metal-FX equivalents, research extensions for
  new techniques) can ship if there's reason. Trust model below.

Capability discovery: apps query at startup which extensions are
loaded and which ops each provides. Apps can adapt (use the high-
quality op if available, fall back to a coarser one if not).

### 3.5 Extension trust model

A loaded extension lives in fresco-server's address space and has
full Vulkan access. A buggy or malicious extension can leak
textures across apps, crash the server, or corrupt GPU state.

Three tiers of trust:

1. **Built-in extensions** (atrium-core, the renderer we ship):
   compiled into / linked from fresco-server. Reviewed as part of
   the system. Same trust as the kernel.
2. **Signed extensions** (vendor-supplied or curated third-party):
   loaded from a curated directory; signature verified against an
   admin-managed key set. Same trust model as kernel modules.
3. **Untrusted / per-app extensions**: not supported in v1. If we
   ever need this, the implementation is "extension runs in a
   separate process, talks to fresco-server's main process via a
   restricted IPC, has its own sandboxed Vulkan context." Real
   work; defer until there's a concrete need.

For the foreseeable future Atrium ships only tier-1 extensions plus
maybe one tier-2 (e.g., a vendor-supplied performance extension on
hardware that benefits). We're not building a third-party extension
marketplace.

### 3.6 Where scenegraph traversal runs

Engine extensions (§3.1–§3.5) describe how *individual ops* lower to
Vulkan. But before any op handler runs, fresco-server has to walk the
incoming scenegraph each frame: compose parent×child transforms,
evaluate animations parameterised by the current frame, cull against
clip rects, and decide which ops to dispatch. That traversal is its
own architectural decision.

**Decision: traversal runs on the GPU**, in a compute pass that
reads a CAS-resident scene buffer and writes per-batch instance
buffers + counts consumed by the render pass via indirect-instanced
draws. The fresco-server core dispatches one compute kernel + one
render pass per frame; per-frame host→GPU traffic is bounded by the
frame counter and any structural deltas (~tens of bytes for the
common case of stable scene structure with animation parameters).
Engine extensions themselves keep their host-side shape — they emit
SPIR-V/Vulkan against composed parameters supplied by the compute
output.

This decision is settled by two benchmarks:

- **bench-fresco-runtime v1** (NO-GO): per-leaf
  `MTLIndirectCommandBuffer render_command` encoding from a compute
  kernel loses ~3.2× to instanced batched draws. Settled question:
  **the render pass uses indirect-instanced draws**, not per-leaf
  ICB encoding.
- **bench-fresco-runtime v2** (GO): with the v1 verdict baked in
  (both paths share an indirect-instanced render pass), GPU-side
  traversal ties host-side 4-thread traversal on frame time and
  wins decisively on host CPU (52× lower worst-case, constant
  vs O(N×A×D) on host) and per-frame host→GPU bytes (240,000× at
  N=100k, 55,532× average across 216 cells). On Apple M4 Max /
  Metal 4.

The host-CPU and bus-traffic axes are not optional in a desktop OS
context: the host CPU also runs the compositor, app processes, input
handling, and the engine extensions themselves; freeing 2 ms/frame
of pure traversal cost at scale is the difference between
interactive and laggy under load.

#### Cross-platform note

The v2 measurements are Apple Silicon; the architectural decision
must hold on Vulkan-targeted discrete-GPU hosts too (per §2 the
bottom contract is Vulkan). Reasoning, not measurement:

- **Bus-traffic axis strengthens off Apple.** UMA makes H's
  multi-MB/frame instance upload a free memcpy. PCIe makes it a
  real cross-bus transfer in the critical path of every frame —
  either with sync-stall risk or double-buffered staging eating
  bandwidth otherwise available for asset streaming. The 240,000×
  ratio that read as "interesting" on UMA reads as load-bearing
  on discrete.
- **Host-CPU axis is unchanged.** The host walks the scenegraph at
  the same speed regardless of GPU topology.
- **Frame-time axis depends on rasterizer-vs-compute throughput
  and driver overhead.** On a beefier discrete GPU, both paths
  shrink, but the host path now contains a real PCIe round-trip
  the GPU has to wait for. Likely tilts further toward the GPU
  path at large N, not less.
- **Driver-maturity caveat.** Vulkan's
  `VK_EXT_device_generated_commands` (the indirect-instanced
  equivalent) is best-supported on NVIDIA, present on AMD, weakest
  on Intel/Linux. Implementation risk is real but doesn't change
  the architectural direction.

We do not re-bench on Vulkan/discrete before locking the
architectural decision. We do plan to validate via a Vulkan port
of v2 (~2–3 days) ahead of any production Fresco runtime targeting
non-Apple hardware, so we have one real number for the most likely
external consumer (Linux + NVIDIA) before perf incidents make us
re-derive it under pressure.

#### Persistent megakernel (Path C) — explicitly deferred

A long-running compute kernel that polls for frame-ready signals
and never returns is the canonical "GPU runtime" shape. v2's per-
frame compute dispatch already wins the architectural axes; Path C
would shave at most tens of µs of dispatch overhead per frame, in
the regime where v2's per-frame G already runs sub-millisecond, and
zero help in the regime where both paths are rasterization-bound.

Path C is therefore **not on the critical path for the architectural
decision**. It is filed as:

1. A future optimization to revisit if a measured perf incident
   traces to dispatch overhead.
2. A future programming-model question to revisit if/when the
   engine wants to consume async scenegraph deltas without frame
   coupling — at which point persistence is the natural runtime
   shape, independent of frame-time numbers.

Until one of those signals arrives, the production fresco-server
core uses per-frame compute dispatch.

## 4. The "raw GPU access" escape hatch — and why we mostly don't need it

Some workloads cannot fit any extensible scene model: graphics
research, custom-shader-heavy demos, things that invent new
techniques.

Our position: **for the appliance-OS target Atrium serves, this is
acceptable to leave unsupported.** The categories are narrow
(probably <1% of intended apps), and the architectural cost of
sanctioning raw GPU access for arbitrary apps is high (every
security property in §1 weakens).

If/when we need to support such a case (e.g. for system services
like Fresco's own diagnostics), we'd add a sanctioned, signed-only
"system extension" path — same tier-1 trust as the built-ins. We
do NOT plan to expose raw Vulkan to general apps.

## 5. Implementation plan

### Phase 1 (existing): tiny-skia CPU rasterizer
Already shipped. Gives us correctness and the protocol shape.
Useful indefinitely as the "no GPU available" fallback and for CI.

### Phase 2 (next): fresco-gpu-vulkan backend
Replace tiny-skia with a Vulkan-based renderer in fresco-server.
This is the proof point that the bottom contract (Vulkan) closes
the loop.

Scope:
- Vulkan instance + device + queue setup (handle multiple GPUs
  reasonably)
- Per-app render contexts (memory budget separation)
- Atrium-core extension implemented natively over Vulkan: rect /
  path / texture / glyph / transform ops
- **GPU-side scenegraph traversal** per §3.6: CAS-resident scene
  buffer, per-frame compute kernel composing transforms / animation
  / culling, indirect-instanced render pass consuming compute output.
  Path H_gcd4-equivalent (CPU-side traversal) kept available behind
  a compile-time flag as the correctness reference and as the
  fallback for debugging compute-shader regressions.
- Output to scanout (initially via virtio-gpu in the VM; later via
  KMS-equivalent on real hardware)
- Software fallback (tiny-skia) when no Vulkan device is available

Estimated effort: ~3-4 weeks for a working but unoptimized version
on the FreeBSD VM with virtio-gpu Vulkan. Add ~3 days for a Vulkan
port of bench-fresco-runtime v2 to validate the v2 verdict on
Linux+NVIDIA before production traffic hits non-Apple hardware (per
§3.6 cross-platform note).

### Phase 3 (later): extension ABI + first non-core extension
Define the C ABI for engine extensions, the manifest format, the
loader in fresco-server, the capability discovery in the wire
protocol. Ship one reference extension distinct from atrium-core
(probably a 3D mesh-rendering extension with sanctioned shaders) to
prove the model works end-to-end.

### Phase 4 (much later): real-hardware Vulkan + native GPU backends
Use vendor Vulkan drivers on real laptops/desktops. Native
per-GPU scenegraph rasterizers (bypassing Vulkan) become a
performance optimization for specific platforms, not a portability
requirement.

## 6. What this stack is deliberately NOT

A useful list to keep us honest:

- **Not a Vulkan replacement.** We sit ABOVE Vulkan; we don't
  compete with it.
- **Not a Mesa replacement.** We CONSUME Mesa (and proprietary
  Vulkan drivers); we don't reimplement the GPU-driver layer.
- **Not a NIR / SPIR-V competitor.** We don't standardize an IR;
  we use SPIR-V via Vulkan.
- **Not a Wayland replacement that just renames things.** Wayland
  apps still allocate their own GPU buffers and drive their own
  GPU pipelines; the compositor stitches finished buffers. Atrium
  apps emit scene structure; the compositor IS the renderer. Two
  fundamentally different layering decisions.
- **Not a games platform that runs Steam titles.** Engines (UE,
  Godot, Unity if/when) would need to port their backend to emit
  Atrium scenegraph ops. We can make that port small (one engine
  extension, no per-vendor work), but we cannot make it zero.
- **Not a third-party-extension marketplace.** Tier-3 trust model
  is deliberately unsupported in v1.

## 7. Strategic verdict

This stack is the right answer **if Atrium's target is a curated
appliance OS with native or engine-ported apps**, which is what
the Karythra-OS direction and Atrium-on-FreeBSD development
imply.

It is the wrong answer if Atrium is meant as a desktop Linux
replacement that runs Firefox unchanged. We've structurally
precluded that path.

The bet: small-app-surface + Vulkan-as-bottom-contract + curated
engine extensions covers the design space we actually care about,
without requiring us to write or maintain a Mesa-equivalent.
