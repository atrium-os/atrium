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
   │  fresco-server (host shim — thin)                      │
   │    • protocol I/O (sockets, framing)                   │
   │    • scene CAS state, delta-uploads to GPU buffer      │
   │    • text shaping (rustybuzz/swash), asset upload      │
   │    • input event handling                              │
   │    • registers loaded extensions; per frame dispatches │
   │      ONE compute pass + ONE render pass                │
   │    NO per-op host code in the per-frame critical path  │
   └────────────────────────────────────────────────────────┘
                              │   ── Per-frame host→GPU traffic:
                              │      ~tens of bytes (frame counter +
                              │      structural deltas). See §3.6.
                              ▼
   ┌────────────────────────────────────────────────────────┐
   │  Engine extensions (SPIR-V bundles, GPU-resident)      │
   │    ├─ atrium-core (built-in): rect, path, texture,     │
   │    │   glyph, transform                                │
   │    ├─ ueke      (loadable): UE op set                  │
   │    ├─ godot     (loadable): Godot op set               │
   │    └─ vendor    (loadable, signed): RT, neural, etc.   │
   │                                                        │
   │  Each extension ships:                                 │
   │    • compute-shader fragments (per-op processing       │
   │      during the per-frame traversal pass)              │
   │    • render pipelines (vertex+frag for the op's draws) │
   │    • manifest (op IDs, dependencies, GPU resources)    │
   │                                                        │
   │  The extensions ARE the GPU runtime; the host shim     │
   │  composes them into a per-frame compute kernel + a     │
   │  per-frame indirect-instanced render pass.             │
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

Two boundaries inside our own implementation:

- **Host↔GPU boundary**: ~tens of bytes per frame (frame counter +
  scene deltas). See §3.6 for the v1+v2 benchmarks that locked this
  shape.
- **Host shim ↔ extension bundle boundary**: SPIR-V + JSON
  manifest. No host-loadable code; no dlopen of arbitrary `.so`.
  See §3.1.

Everything between the two outward-facing contracts is
implementation. We can rewrite it freely without breaking either.

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

An engine extension is a **SPIR-V bundle** loaded into fresco-server
at startup. It is NOT a shared library. There is no host-loadable
code; there are no per-op host callbacks; nothing in the extension
runs on the CPU per frame.

A bundle is a directory (or a single archive) containing:

```
my-extension.spvbundle/
├── manifest.json            # op IDs, entrypoints, resources, deps
├── compute/                 # SPIR-V compute shaders (per-op processors)
│   ├── op_rectangle.spv
│   ├── op_path.spv
│   └── ...
└── pipelines/               # SPIR-V vert+frag pairs (per-op render pipelines)
    ├── pipe_rectangle.vert.spv
    ├── pipe_rectangle.frag.spv
    └── ...
```

The manifest declares what the extension provides:

```jsonc
{
  "name":    "atrium-core",
  "version": 1,
  "ops": [
    {
      "id":               "rect",
      "compute_entry":    "compute/op_rectangle.spv:main",
      "render_pipeline":  "pipelines/pipe_rectangle"
    },
    {
      "id":               "glyph",
      "compute_entry":    "compute/op_glyph.spv:main",
      "render_pipeline":  "pipelines/pipe_glyph",
      "host_pre":         "shape_text"   // optional; see §3.2
    }
  ],
  "depends_on":   [],
  "gpu_resources": { "max_textures": 1024 }
}
```

At server startup, fresco-server:

1. Reads each registered bundle's manifest.
2. Validates every `.spv` with `spirv-val` (Khronos's static
   validator). Any failure means the bundle is rejected; the
   server logs and continues without it.
3. Builds the dispatch table: per-op-ID → (compute entry, render
   pipeline). Emits a composed compute kernel that, per scene
   node, looks up the node's op-ID and dispatches the
   corresponding extension's compute fragment.
4. Ahead-of-time-compiles the render pipelines for the current
   Vulkan device (`vkCreateGraphicsPipelines`); caches the
   pipeline cache to disk for next start.

Per frame, fresco-server's only work is:

1. Apply incoming protocol commands to the GPU-resident scene
   buffer (delta upload).
2. Run any host-side preprocessors (text shaping etc. — see §3.2).
3. Dispatch the composed compute kernel ONCE.
4. Dispatch the indirect-instanced render pass ONCE.
5. Present.

**No per-op host callbacks. No dlopen. No host-side fanout.**

Bundles are written ONCE in SPIR-V (or in GLSL/HLSL/Slang compiled
to SPIR-V offline by their author). They run on every Vulkan driver
unchanged. **No per-vendor engine extensions.**

### 3.2 Where shaders live (and the rare host-preprocessor exception)

**Shaders ARE the extension.** Per §3.1, an extension is a SPIR-V
bundle containing compute fragments + render pipelines. There is
no other "extension code" — the extension's contribution to the
running system is exactly the shaders it ships and the manifest
declaring how to wire them in.

Total industry shader-writing burden goes from O(thousands of apps
each writing their own) to O(N engine extensions writing one canonical
implementation each). Order-of-magnitude reduction.

#### The host-preprocessor exception

A small number of ops cannot be expressed efficiently as GPU
compute and need to run on the host BEFORE the GPU sees them.
The canonical example is **text shaping** — branchy state
machines over variable-length input (rustybuzz/swash/HarfBuzz):
serial, allocator-heavy, Unicode-rule-laden. Compute shaders are
genuinely bad at this.

For such ops, the manifest declares an optional `host_pre` step.
fresco-server keeps a small registry of named host preprocessors
that it knows how to run:

| `host_pre` name | what it does |
|---|---|
| `shape_text` | rustybuzz/swash → glyph runs (positions + glyph IDs) |
| `decode_image` | PNG/JPEG → uploaded RGBA texture handle |
| `compile_path` | SVG path → tessellated triangle strip |
| ... | extension-author-curated additions, all built-in to fresco-server |

When a scenegraph command arrives for an op whose manifest
declares `host_pre`, the host shim runs the preprocessor, writes
the result into GPU-resident memory (a glyph atlas, a texture
slot, a tessellation buffer), and writes a *processed* node
record into the scene buffer. From the per-frame compute kernel's
point of view, the node is just data — same shape as any other op.

**Critical design rule: the host_pre registry is closed-set,
curated, and built into fresco-server.** Extensions cannot ship
arbitrary host code as a "preprocessor." The list above is the
list. New entries require a fresco-server change. This keeps the
"no dlopen of arbitrary code" invariant from §3.1.

In practice we expect ~5-10 preprocessors over the lifetime of
the project. Every one of them serves a structural CPU-friendly /
GPU-hostile workload (text shaping, Unicode normalization, image
decode) — not a creative outlet for extension authors.

### 3.3 Why this is not Mesa

The extension layer looks superficially like Mesa: there's source
(scenegraph ops), there's an intermediate form (the SPIR-V
extensions ship), there's Vulkan output. But:

- We do NOT publish an IR of our own. **SPIR-V is the IR**;
  Khronos owns it. We assemble pre-built SPIR-V modules into a
  per-frame composition; we do not transform them.
- We do NOT ask vendors to implement against our intermediate
  form. The vendor consumes plain SPIR-V via plain Vulkan;
  there's nothing Atrium-specific at the vendor boundary.
- We do NOT include a shader compiler. Extension authors compile
  their HLSL/GLSL/Slang to SPIR-V offline using existing tools
  (`glslang`, `dxc`, `slangc`); the bundle ships pre-compiled
  modules; Vulkan drivers translate SPIR-V→hardware.
- We do NOT load host code per frame. After the redesign in §3.1,
  there is no `.so` dlopen, no per-op host callback, no host-side
  fanout in the per-frame critical path. The per-frame work is
  one compute dispatch + one render pass, both built from
  extension-provided SPIR-V.

Compare Mesa, which DOES publish an IR (NIR), DOES expect vendor
backends to consume it, DOES include a full shader compiler stack
(GLSL/SPIR-V → NIR → vendor backend), AND DOES carry per-vendor
runtime code paths in userspace.

Atrium's stack is responsible for ONE of those four roles
(orchestration: assemble SPIR-V at startup, dispatch per frame).
The other three are entirely owned by Khronos + Mesa-and-friends +
vendor drivers. The redesign in §3.1 specifically eliminated the
last bit of host code we still had per frame; the layer is now
substantially thinner than what the previous version of this spec
described.

### 3.4 Extensibility model

New rendering capabilities arrive as new SPIR-V bundles OR as new
ops within an existing bundle. The three categories below are
distinguished by **who writes them** — this is the central
question for what the Atrium project itself is on the hook to ship
versus what we depend on others for.

- **atrium-core** (built into fresco-server's binary distribution).
  **Atrium project writes this.** The canonical primitives every
  app needs: rect, path, texture, glyph, transform, basic 3D mesh
  draw. ~5-15 ops. Well-understood graphics, manageable scope,
  exhaustively tested. This is the bulk of what most apps actually
  use, and is what every Atrium installation has available
  unconditionally.

- **Engine compat bundles** (ueke, godot, others if/when they
  port). **Engine vendors write these.** Epic's UE, the Godot
  Foundation, etc. own their rendering algorithms (Lumen GI,
  Nanite, Godot's GI, etc.) — those are decades of investment we
  cannot replicate. They port their existing HLSL/GLSL/MSL to
  SPIR-V via tools they already use (`dxc`, `glslang`, `slangc`),
  package with our manifest format, distribute. App code targeting
  those engines runs on Atrium without modification of the engine's
  app-facing API; the engine's RENDERER ports once, the engine's
  app-facing surface unchanged.

- **Vendor / research bundles**. **GPU vendors or research groups
  write these.** Hardware-optimized variants (NVIDIA-supplied RTX
  path tracing, Apple Metal-FX equivalents, vendor neural
  denoisers) or novel techniques from research labs. Same bundle
  shape — SPIR-V + manifest. Trust model in §3.5.

The pattern is the standard host-platform shape:

| Atrium                | analogous to |
|---|---|
| atrium-core           | a browser's built-in CSS layout engine |
| engine compat bundles | JavaScript libraries (D3, Three.js) |
| vendor bundles        | DAW plugins (VST reverb / EQ from third parties) |

We are the host platform for graphics. Sophisticated rendering
algorithms live with the people who already build them.

#### What this means for early Atrium

With only atrium-core loaded (today, and for the near future), apps
have access to: rectangles, paths, textures, glyphs, transforms,
basic 3D mesh. Sufficient for desktops, GUIs, productivity apps,
terminals, editors, the entire Atrium-on-FreeBSD development
target. **Insufficient** for AAA games, sophisticated 3D scenes
with realistic lighting, GPU-accelerated visualization — until
someone writes/ports a bundle providing the relevant ops.

This is not a bug in the architecture; it's the correct division of
labor. We don't ship Lumen because we don't write Lumen. Epic does.
If/when Epic ports, those apps can use it. Until then, apps get
atrium-core or fall back gracefully via capability discovery
(below).

#### Op-ID vocabulary policy: closed registry

The scenegraph protocol's op-ID space is **a closed registry**, not
an open vocabulary. An op-ID can be used by apps only if **at least
one shipping bundle implements it** AND **the op-ID + parameter
schema have been added to the standardized registry** (owned by
the Atrium project, with engine-vendor input).

Two consequences:

1. Apps writing against the protocol have a clear answer to "what
   ops can I use?" — the registry. No ambiguity, no "this works on
   my system but breaks on yours because the bundle isn't there."
   (Capability discovery still applies for ops outside atrium-core
   — the registry tells you the op exists; capability discovery
   tells you whether THIS installation has a bundle for it.)
2. New ops require coordination: a bundle author can't just invent
   `vendorx.cool_new_thing` and expect apps to use it. The op gets
   reserved in the registry first (Atrium project + engine-vendor
   discussion); then bundles ship implementing it; then apps
   target it.

This is the standards-body work flagged earlier as a real cost —
it's the price for "apps know what they can rely on." Vendor-
specific extensions can use a `vendor.` prefix namespace
(`nvidia.path_traced_gi`) for opt-in use without going through
the standardized core.

#### How a bundle is composed into the per-frame pass

#### How a bundle is composed into the per-frame pass

At startup, fresco-server walks the manifest of every loaded
bundle and builds a single dispatch table:

```
op-id → (compute entrypoint, render pipeline) ─→ which bundle owns it
```

Op-ID collisions across bundles are resolved by load order with a
warning logged (last bundle wins). Two extensions claiming the
same op-ID is almost always a packaging mistake and should fail
loud.

The per-frame compute kernel reads each scene node's op-ID, looks
up the dispatch table, and invokes the corresponding extension's
SPIR-V compute fragment for that node. All extensions execute
within the same compute pass; SPIR-V's linkage allows one composed
kernel to invoke many entry points. (For the small N of bundles
expected in practice — single digits — the dispatch is a switch
on op-ID, not a full dynamic dispatch system.)

#### Capability discovery from the app side

Apps query at startup which bundles are loaded and which ops each
provides:

```
Request::ListExtensions
    → Response::Extensions {
          bundles: [ { name, version, ops: [...] }, ... ]
      }
```

Apps adapt: use the high-quality op if available, fall back to a
coarser one if not. (For example, if `ueke.global_illumination` is
loaded, use it; otherwise emit `atrium-core.solid_color` for
unshaded fallback.)

#### Distribution

Bundles are simple directory/archive trees. They can be:
- Built into a server image (atrium-core)
- Installed system-wide (`/usr/local/share/fresco/extensions/`)
- Per-user-installed (`~/.fresco/extensions/`) — only with the
  trust escalation in §3.5

We are explicitly NOT building a third-party extension marketplace.
Bundles ship with the engines that emit them, with vendor packages,
or as part of curated distributions.

### 3.5 Extension trust model

The redesign in §3.1 (extensions are SPIR-V bundles, not host
`.so` files) collapses what was previously a delicate trust
problem into a much smaller one. The risk surfaces are now:

| can a malicious bundle... | answer | why |
|---|---|---|
| ...execute arbitrary host code? | **No.** | No dlopen path. Bundles ship SPIR-V, not native binaries. |
| ...make syscalls? | **No.** | SPIR-V has no syscall vocabulary — no file I/O, no network, no fork/exec, no signal handling. The instruction set just doesn't contain those concepts. |
| ...corrupt fresco-server's heap or stack? | **No.** | SPIR-V runs in the GPU's memory space, not the host's. Out-of-bounds in compute touches GPU memory only. |
| ...corrupt other apps' GPU memory? | Bounded by Vulkan's protections. | Each compute dispatch operates on the buffers the host shim binds to it. The host shim only binds the current frame's scene buffer + the extension's declared resources. Cross-app texture leaks would require fresco-server to bind the wrong textures — a fresco-server bug, not an extension capability. |
| ...cause GPU hangs (TDR / driver reset)? | **Yes** (infinite loops in compute). | Bounded by spirv-val's structural checks (terminating CFG required) plus the OS's GPU watchdog. Worst case: a buggy bundle triggers a driver reset; the system recovers; the bundle is logged and marked unloadable. |
| ...produce wrong output? | **Yes.** | A bundle with bad SPIR-V renders garbage. That's a quality issue, not a security issue. |

This is dramatically less dangerous than the previous host-`.so`
model, where a malicious extension could `system()`, mmap host
memory, fork another process, or read `/etc/shadow`.

#### What bundles do still need

1. **Validation at load time.** `spirv-val` runs on every `.spv`
   in the bundle before it's accepted. Bundles that fail
   validation are rejected loudly, not silently degraded.
2. **Resource accounting.** The manifest declares what GPU
   resources (texture slots, buffer counts) the extension needs;
   fresco-server reserves them at load time. A bundle that
   exceeds its declared budget at runtime is denied and the
   compute dispatch returns an error frame.
3. **Provenance trail.** Bundles record where they came from
   (built-in / signed-vendor / user-installed). On a render
   incident, fresco-server logs which bundles were active so
   blame is attributable.

#### Tiers (simplified)

Two tiers, not three. The previous tier-3 was deferred because
extension code in process was too dangerous to allow per-app
loading; with SPIR-V bundles, that constraint mostly evaporates.

1. **Built-in / signed.** atrium-core ships in the binary; vendor
   bundles ship signed against an admin-managed key set.
   `spirv-val` still runs at load. These are the bundles in
   serious deployments.
2. **User-installed (`~/.fresco/extensions/`).** Allowed by
   default. Risk is bounded to "renders garbage / triggers TDR
   for that user's session." spirv-val still runs. Suitable for
   experimenting with research extensions, custom artistic
   effects, etc. Can be globally disabled by a system policy
   (per the Portcullis-style policy file model) for tightly
   curated deployments.

The "extension marketplace" question is still out of scope — we
just don't preclude it the way the old trust model did.

### 3.6 Where scenegraph traversal runs

Engine extensions (§3.1–§3.5) describe how *individual ops* lower to
Vulkan. But before any op handler runs, fresco-server has to walk the
incoming scenegraph each frame: compose parent×child transforms,
evaluate animations parameterised by the current frame, cull against
clip rects, and decide which ops to dispatch. That traversal is its
own architectural decision.

**Decision: traversal runs on the GPU via per-frame compute
dispatch**, in a compute pass that reads a CAS-resident scene buffer
and writes per-batch instance buffers + counts consumed by the render
pass via indirect-instanced draws. The fresco-server core dispatches
one compute kernel + one render pass per frame; per-frame host→GPU
traffic is bounded by the frame counter and any structural deltas
(~tens of bytes for the common case of stable scene structure with
animation parameters). Engine extensions themselves keep their
host-side shape — they emit SPIR-V/Vulkan against composed parameters
supplied by the compute output. (Persistent megakernels — see below —
are explicitly deferred; "GPU traversal" here means per-frame compute
dispatch, not a long-running kernel.)

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
  ratio that read as "interesting" on UMA becomes
  **latency- and energy-load-bearing** on discrete: each frame's
  upload adds µs to the host→GPU critical path before any compute
  or render work can start, and PCIe transfers cost real package
  power at scale. (Bandwidth itself is not saturated — 4.8 MB/frame
  at 60 fps is ~0.9% of PCIe 4.0 x16 — the cost is per-transfer
  fixed overhead and energy, not throughput.)
- **Host-CPU axis is unchanged.** The host walks the scenegraph at
  the same speed regardless of GPU topology.
- **Frame-time axis depends on rasterizer-vs-compute throughput
  and driver overhead.** On a beefier discrete GPU, both paths
  shrink, but the host path now contains a real PCIe round-trip
  the GPU has to wait for. Likely tilts further toward the GPU
  path at large N, not less.
- **No exotic-extension dependency.** v2's pattern is "compute
  kernel writes instance buffer + count → render pass consumes via
  `vkCmdDrawIndexedIndirect` (or `vkCmdDrawIndexedIndirectCount`
  for the count-buffer variant)." Both are core Vulkan 1.0 / 1.2
  respectively, available on every conformant Vulkan
  implementation. We deliberately do NOT depend on the more
  experimental `VK_EXT_device_generated_commands` (GPU-encoded
  command buffers) — v1's NO-GO already established that
  per-leaf GPU command encoding is the wrong pattern. Driver-
  maturity risk for our pattern is therefore low; the compute +
  indirect-draw path is among the most exercised in any Vulkan
  driver.

We do not re-bench on Vulkan/discrete before locking the
architectural decision. We do plan to validate via a Vulkan port
of v2 ahead of any production Fresco runtime targeting non-Apple
hardware, so we have one real number for the most likely external
consumer (Linux + NVIDIA) before perf incidents make us re-derive
it under pressure. Realistic budget: **~1 week with an existing
Vulkan template (`vulkano` / `vulkan-rs` / a cribbed
`vulkan-tutorial` setup), 1–2 weeks from scratch** — Vulkan's
boilerplate (instance + device + swapchain + descriptor sets +
pipeline layout + MSL→SPIR-V cross-compile) is real work even
for a port of an already-designed benchmark.

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

### 3.7 Wire format: aqueduct + the display dictionary

The scenegraph protocol does NOT define its own wire envelope.
It rides on **aqueduct** (see [`aqueduct.md`](aqueduct.md)),
the unified IPC substrate every Atrium service uses (clipboard,
notify, broker, audio control, ...). One envelope, one client
library, one debugger view, one CAS namespace across the OS.

Fresco's display protocol is the dictionary at `opcode_class = 1`
(`CLASS_DISPLAY` in `aqueduct/src/classes.rs`), implemented as a
companion crate `fresco-protocol`. The migration of Fresco from
its legacy 128-byte fixed-frame format to this envelope is the
deferred D1.7+ work that
[`aqueduct.md` §9.1](aqueduct.md#91-fresco-migration-deferred-d17-or-later)
specs out. Building fresco-protocol alongside the new
fresco-server (per §5 Phase 2) IS that migration.

#### What we get for free

- **CAS upload + dedup.** `aqueduct` (`opcode_class = 0`)
  defines `UPLOAD_BEGIN/DATA/FINISH/ACK` and `FETCH_REQUEST/BEGIN`.
  Every aqueduct speaker implements them. Hashes are SHA-256,
  shared with Tessera. We do NOT redefine these in the display
  dictionary.
- **fd-passing for shm.** aqueduct handles `SCM_RIGHTS`
  fd-passing for big payloads (decoded video frames, GPU
  textures). Capability is the fd. Already available; no
  display-specific work.
- **Capability boundary.** Portcullis nullfs-mounts
  `/atrium/sockets/fresco.sock` into jails that declare the
  `graphics = "fresco"` capability. Standard pattern; nothing
  Fresco-specific.
- **Tessera integration.** Hashes shared across CAS namespace
  means a texture rendered by Fresco AND copied to clipboard
  AND in a notification is one allocation.

#### What fresco-protocol defines

The display dictionary has two op categories — **control ops**
and **scene ops** — both addressed by the envelope's `op` field
(u16) within `opcode_class = 1`.

**Control ops** are handled by fresco-server's host shim directly.
They mutate host-side state (CAS table, slot table, scene buffer);
no SPIR-V dispatch involved. Examples (numbering mirrors the
legacy wire-format.md vocabulary, adapted to the envelope):

| op | name | purpose |
|---|---|---|
| `0x0020` | `OP_SLOT_SET`         | bind a CAS hash to a slot ID (per-client slot table) |
| `0x0021` | `OP_SLOT_CLEAR`       | release a slot |
| `0x0030` | `OP_SCENE_FRAME_BEGIN`| start composing a frame |
| `0x0031` | `OP_SCENE_FRAME_END`  | commit + present |
| `0x0040` | `OP_SCENE_NODE_SET`   | install/update a scene node (carries scene op-ID + params) |
| `0x0041` | `OP_SCENE_NODE_CLEAR` | remove a scene node |
| `0x0500` | `OP_WINDOW_CREATE`    | create a top-level window |
| `0x0504` | `OP_WINDOW_PRESENT`   | per-window frame end |

(Full set comes from translating `wire-format.md` §6.2 into
envelope payloads. Bytes per command shrink because the envelope
is variable-length; the legacy format was 128-byte fixed.)

**Scene ops** are the §3.4 closed registry — what bundles
implement. Scene op-IDs travel inside `OP_SCENE_NODE_SET`
payloads, not as envelope opcodes. The host shim's per-frame
compute kernel reads each scene node's op-ID and dispatches the
appropriate bundle's SPIR-V compute fragment.

```rust
// fresco-protocol/src/lib.rs (sketch)
pub mod control {
    pub const OP_SLOT_SET:          u16 = 0x0020;
    pub const OP_SCENE_FRAME_BEGIN: u16 = 0x0030;
    pub const OP_SCENE_NODE_SET:    u16 = 0x0040;
    // ...
}

pub mod scene_ops {
    /// op-IDs from the §3.4 closed registry. Carried as a u32
    /// inside SceneNodeSetPayload, NOT as envelope ops.
    pub const ATRIUM_CORE_RECT:    u32 = 0x1000;
    pub const ATRIUM_CORE_TEXTURE: u32 = 0x1001;
    pub const ATRIUM_CORE_PATH:    u32 = 0x1002;
    pub const ATRIUM_CORE_GLYPH:   u32 = 0x1003;
    // ... future engine bundles get their own op-ID ranges
    // pub const UEKE_LUMEN_GI:       u32 = 0x2000;
}

pub struct SceneNodeSetPayload {
    pub node_id: u32,
    pub op_id:   u32,        // from scene_ops above
    pub params:  Vec<u8>,    // op-specific schema (per-op modules)
}
```

#### Why this two-category split matters

- **Control ops never reach bundles.** They mutate fresco-server
  state (CAS table, slot table, scene buffer). A bundle CANNOT
  define new control ops — the privileged host-side state is not
  extensible from extension code. This is the same invariant that
  §3.1 enforces by making bundles SPIR-V-only.
- **Scene ops are bundle-defined.** New scene ops require an
  op-ID reservation in the §3.4 registry plus a bundle that
  implements them. The wire format itself doesn't change.
- **Atrium-RPC is unaware of the distinction.** From the
  envelope's view, both are just opcodes within `CLASS_DISPLAY`.
  The host shim's dispatcher routes control ops to its own
  handlers and scene ops to the bundle compute pass.

#### What about the legacy fresco-socket-rs and 128-byte format?

Both stay valid until their consumers migrate. The current
fresco-server, fresco-socket-rs, atrium-edit-socket, etc. continue
to use the 128-byte format documented in
[`wire-format.md`](wire-format.md). They are unaffected by
fresco-server-poc adopting the new envelope-based path. When the
new path supersedes the old one (post-POC, after a migration
window), `wire-format.md` gets a deprecation note. For now: two
specs coexist, each governing its own implementations.

### 3.8 Planned op families: window management, animation, accessibility

The wire protocol grows three additional op families beyond the scene-
graph primitives in §3.7. Each is **planned-but-deferred** — captured
here so the design space is consistent across phases, but not yet
implemented in the POC. All live in CLASS_DISPLAY (no new classes).

#### 3.8.1 Window management

The scene server already plays a window-manager role (placement, focus,
z-order, input routing). Apps express window lifecycle through the
toolkit (Pergola — see [`pergola.md`](pergola.md)), which emits these
ops via fresco-socket-rs underneath. App code never calls these
directly.

```
Control (client → server):
  WINDOW_CREATE         id, hints (size, decorations, modal, parent)
  WINDOW_DESTROY        id
  WINDOW_SET_TITLE      id, string
  WINDOW_SET_HINTS      id, hint-flags
  WINDOW_REQUEST_CLOSE  id

Events (server → client):
  WINDOW_RESIZED         id, width, height
  WINDOW_FOCUS_CHANGED   id, gained|lost
  WINDOW_CLOSE_REQUESTED id   (user clicked X)
  WINDOW_DPI_CHANGED     id, scale
```

This is what xdg-shell solves for Wayland; we get to solve it once
cleanly because we own the protocol. POC has stubs (`atrium-window-demo`);
finalize the op set in `fresco-protocol` during D2.

#### 3.8.2 Animation (declarative, server-driven)

Phase A (D2/D3): client-driven animation only — toolkit emits per-frame
state via `SCENE_NODE_SET`. App must be running 60+ Hz during the
animation; battery cost is real; smoothness suffers during app GC
pauses or other host-side work.

Phase B (D4+): declarative `ANIMATION_*` ops let fresco-server run an
interpolator on its own tick. App can be suspended; animations still
play to completion. Closes the iOS Render Server gap.

```
Control (client → server):
  ANIMATION_START   handle, target_node, property,
                    from, to, duration, curve, on_complete
  ANIMATION_CANCEL  handle

Events (server → client):
  ANIMATION_FINISHED handle
```

Pergola's animation API surface should be the same in both phases —
the app code never changes when we flip the implementation.

#### 3.8.3 Accessibility (CLASS_AX)

Architecturally first-class, not retrofitted. The scene server already
mirrors per-app retained-mode trees; the AX tree is the same shape with
different node payloads. Pergola owns both trees (scene + AX) and
mirrors them in lockstep.

Sibling dictionary to CLASS_DISPLAY:

```
AX_NODE_SET          node_id, role, label, value, state, parent, rect
AX_NODE_CLEAR        node_id
AX_TREE_FOCUS_CHANGE node_id
```

**Single semantic tree, three consumers:**

1. Screen readers / voice control / switch control (assistive tech)
2. UI testing / automation tools (the AX tree IS the test surface)
3. Scripting (drive the UI from outside the app)

Land in D5 alongside `pergola-ax`. Real differentiator vs. desktop
Linux's fragmented AT-SPI.

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

### Phase 2 (next): fresco-gpu-vulkan + extension composition machinery

Replace tiny-skia with the Vulkan-based runtime architecture
described in §3 + §3.6. This is the proof point that BOTH the
bottom contract (Vulkan) AND the redesigned middle layer
(SPIR-V bundles composed at startup, GPU traversal per frame)
close the loop. The composition machinery is part of Phase 2,
NOT deferred to Phase 3 — atrium-core is itself a SPIR-V bundle
under the new model, so we have to ship the loader to ship a
working renderer at all.

Scope:

1. **Vulkan core:** instance + device + queue setup (handle
   multiple GPUs reasonably), descriptor pool + pipeline cache,
   swapchain.
2. **Extension bundle loader (§3.1, §3.4):** read manifest.json,
   run `spirv-val` on each `.spv`, resolve op-ID → (compute
   entrypoint, render pipeline) dispatch table, AOT-compile
   pipelines, persist pipeline cache to disk.
3. **atrium-core bundle:** ship the canonical scene primitives
   (rect, path, texture, glyph, transform) as a SPIR-V bundle —
   this is the dogfood that validates the bundle format. Built
   into fresco-server's distribution; loaded the same way every
   other bundle is loaded.
4. **Per-frame composition (§3.6):** one compute dispatch reading
   the CAS-resident scene buffer, dispatching the appropriate
   bundle's compute fragment per node, writing per-bucket
   instance buffers + counts; one indirect-instanced render pass
   consuming via `vkCmdDrawIndexedIndirectCount`.
5. **Host preprocessors (§3.2):** start with `shape_text`
   (rustybuzz/swash) and `decode_image`. These are the ones
   atrium-core depends on. Add others only with concrete bundle
   demand.
6. **CPU traversal fallback** (selectable via
   `FRESCO_TRAVERSAL=cpu`): always compiled in. Used for runtime
   fallback when GPU compute regresses, for golden-image tests,
   for headless / no-GPU contexts (CI, remote rendering,
   bring-up on hardware without working Vulkan). Reuses Phase 1's
   tiny-skia path rather than introducing a third code path.
7. **Output to scanout:** initially via virtio-gpu in the VM;
   later via KMS-equivalent on real hardware.

Estimated effort:

- Vulkan core + bundle loader + atrium-core bundle + per-frame
  composition + scanout: **~5-6 weeks** for a working but
  unoptimized version on the FreeBSD VM. (Up from the prior
  3-4 week estimate because the bundle loader + composition
  machinery is real new work; previously this was implicitly
  deferred to "host code calls Vulkan per op," which the v2
  finding obsoleted.)
- Vulkan port of bench-fresco-runtime v2 to validate the v2
  verdict on Linux+NVIDIA before production traffic hits
  non-Apple hardware: **~1 week** with a template, 1-2 weeks
  from scratch (per §3.6 cross-platform note).

### Phase 3 (later): second non-core bundle (proof of composition)

Ship one bundle distinct from atrium-core — probably a 3D mesh-
rendering bundle with sanctioned PBR shaders — loaded at runtime
through the same machinery. The point of Phase 3 is **prove that
two bundles compose** (no op-ID collisions, dispatch table
correctly resolves, both bundles' compute fragments execute in
the same per-frame pass).

If Phase 2 already loads atrium-core through the bundle path,
Phase 3 is small — manifest + ~one weekend of shader work + a
few integration tests.

### Phase 4 (much later): real-hardware Vulkan + native GPU backends

Use vendor Vulkan drivers on real laptops/desktops. Native per-GPU
scenegraph rasterizers (bypassing Vulkan) become a performance
optimization for specific platforms, not a portability
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
- **Not a games platform that runs Steam titles unmodified.**
  Engines (UE, Godot, Unity if/when) need to port their renderer
  backend to ship a SPIR-V bundle implementing their ops in our
  manifest format (per §3.4). The port is bounded — one bundle,
  no per-vendor work, no per-app work, the engine's app-facing
  API is unchanged — but it is not zero. Apps already targeting a
  ported engine run on Atrium without modification. Apps using an
  unported engine don't run until someone ports it; this is the
  standard cost of any new platform's first decade.
- **Not a host-extension architecture.** Per the §3.1 redesign,
  there is no dlopen path for arbitrary `.so` files into
  fresco-server. Extensions are SPIR-V bundles. The host shim
  carries a small closed set of CPU-bound preprocessors
  (`shape_text`, `decode_image`, etc.) that extension authors
  cannot extend. This is a deliberate constraint.
- **Not a third-party-extension marketplace.** We don't preclude
  it (the §3.5 trust model now permits user-installed bundles
  safely), but we're not building one. Bundles ship with engines,
  vendor packages, or curated distributions.

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
