---
marp: true
theme: default
paginate: true
html: true
header: "Fresco Rendering Stack"
footer: "docs/spec/fresco-rendering-stack.md"
style: |
  section { font-size: 24px; }
  h1 { color: #0a5d6c; }
  h2 { color: #0a5d6c; border-bottom: 2px solid #0a5d6c; padding-bottom: 4px; }
  code { background: #f0f0f0; padding: 2px 6px; border-radius: 3px; }
  pre { background: #f6f8fa; border: 1px solid #d0d7de; border-radius: 4px; }
  pre code { background: transparent; color: #24292f; padding: 0; }
  .small { font-size: 18px; }
  .big   { font-size: 36px; }
  table  { font-size: 20px; }
  blockquote { border-left: 4px solid #0a5d6c; color: #555; }
  .cols-2 {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 28px;
    font-size: 22px;
  }
  .cols-2 ul { margin: 4px 0 10px 0; padding-left: 20px; }
  .cols-2 li { margin: 3px 0; line-height: 1.35; }
  .cols-2 strong { color: #0a5d6c; }
  .cols-2 p { margin: 8px 0 4px 0; font-weight: bold; color: #0a5d6c; }
  .acronym-grid {
    column-count: 2;
    column-gap: 28px;
    font-size: 21px;
    line-height: 1.4;
  }
  .acronym-grid ul { margin: 0; padding-left: 20px; }
  .acronym-grid li { margin: 3px 0; break-inside: avoid; }
  .acronym-grid strong { color: #0a5d6c; }
---

# Fresco Rendering Stack

**A retained-mode scene-graph rendering stack for Atrium**

Wire format → SPIR-V bundles → GPU compute scene processing → Vulkan

<br>

<span class="small">
Spec: `docs/spec/fresco-rendering-stack.md` <br>
POC:  `~/src/fresco-poc` (verified on Apple M4 Max via MoltenVK)
</span>

---

## Legend (1/3) — projects & comparison

<div class="cols-2">

<div>

**Atrium project**
- **Atrium** — FreeBSD desktop platform
- **Fresco** — the rendering stack (this deck)
- **fresco-server** — host scene-server binary (POC name)
- **atrium-rpc** — Atrium IPC substrate
- **atrium-core** — default Fresco bundle
- **Tessera** — Atrium CAS filesystem
- **Portcullis** — jail launcher / capability runtime
- **D0 … D7** — Atrium roadmap phases

</div>

<div>

**Other stacks (for comparison)**
- **Wayland / X11** — Linux display protocols
- **Quartz / Core Animation (CA)** — Apple compositor
- **DWM** — Windows Desktop Window Manager
- **WebRender** — Firefox's GPU compositor
- **Skia** — 2D rasterization library (Chrome, Flutter)
- **MoltenVK** — Vulkan-over-Metal translation (macOS)

</div>

</div>

---

## Legend (2/3) — acronyms

<div class="acronym-grid">

- **ABI** — application binary interface
- **AOT** — ahead-of-time compilation
- **API** — application programming interface
- **CAS** — content-addressed storage
- **CPU / GPU** — central / graphics processing unit
- **GLSL** — OpenGL shading language
- **HLSL** — high-level shading language
- **IPC** — inter-process communication
- **KiB / MiB** — kibibyte / mebibyte (1024-based)
- **KHR** — Khronos (Vulkan extension prefix)
- **OS** — operating system
- **POC** — proof of concept
- **POSIX** — portable OS interface
- **RPC** — remote procedure call
- **SHA-256** — secure hash algorithm, 256-bit
- **SPIR-V** — Std. Portable Intermediate Representation, Vulkan
- **SSBO / UBO** — shader storage / uniform buffer object
- **UDS** — Unix domain socket
- **vk-** — Vulkan namespace prefix (`vkImage`, `vkBuffer`, …)

</div>

---

## Legend (3/3) — Vulkan, wire format, tools

<div class="cols-2">

<div>

**Vulkan concepts**
- **vkImage / vkBuffer** — GPU memory resources
- **swapchain** — display image queue
- **render pass** — framebuffer-attachment scope
- **descriptor set** — shader binding-slot → resource table
- **compute kernel** — non-pixel GPU program (`cmd_dispatch`)
- **indirect draw** — `cmd_draw*` whose count lives in a GPU buffer
- **pipeline barrier** — GPU memory/execution sync between stages

**Tools**
- **glslangValidator** — GLSL → SPIR-V
- **spirv-val** — SPIR-V validator
- **postcard** — Rust serialization

</div>

<div>

**Wire-format terms**
- **slot / slot_id** — 4-byte CAS-hash → GPU-resource handle
- **op-id** — 16-bit scene-primitive code (`rect=0x1000`)
- **scene op** vs **control op** — bundle-dispatched vs host-handled

**atrium-rpc message classes**
- **CLASS_CORE** (0) — built-in CAS upload / fetch
- **CLASS_DISPLAY** (1) — Fresco scene + slot ops

**Protocol opcodes**
- **SLOT_SET / SLOT_CLEAR**
- **SCENE_FRAME_BEGIN / END**
- **SCENE_NODE_SET / CLEAR**
- **UPLOAD_BEGIN / DATA / FINISH / ACK**

</div>

</div>

---

## What problem does this solve?

A modern OS needs to put pixels on screen for **N applications at once**, with:

- **Bandwidth efficiency** — apps shouldn't re-send 4 MB textures every frame
- **GPU efficiency** — the compositor shouldn't be the CPU bottleneck
- **Extensibility** — third parties (game engines, browsers) need to add their own scene primitives without recompiling the compositor
- **Portability** — same wire protocol on macOS / Linux / FreeBSD; same backend across Vulkan vendors

Existing compositors (Wayland, Quartz, Windows DWM) solve some of these. Fresco's bet: solve all four with a **single coherent design**.

---

## The naive approach (and why it fails)

```
client                                  compositor
──────                                  ──────────
[texture bytes inline]                 ┌─────────────┐
[per-rect command stream]              │ host CPU    │
[redraw whole scene per frame]   ───►  │ traverses,  │ ──► GPU draws
[full state in every message]          │ rasterizes  │
                                       └─────────────┘
```

Three failure modes:

1. **Wire bandwidth** — 100 instances of one 4 MiB texture = 400 MiB/frame
2. **CPU-bound per-node work** — host both walks the tree *and* computes per-node draws; doesn't scale
3. **Closed extension model** — only ops the compositor knows about can be drawn

---

## The two-axis insight

Most existing stacks pick *one* of these axes; Fresco optimizes *both*:

| Axis | Conventional choice | Fresco choice |
|------|---------------------|---------------|
| **Where does per-node scene work run?** | Host CPU | **GPU compute** (host still walks tree) |
| **How are new primitives added?** | Compositor source-tree changes | **Drop-in SPIR-V bundles** |

Picking the right side of *both* axes is what lets the stack stay small (the scene server is a coordinator, not a renderer) while staying open (anyone with a `glslangValidator` can ship a new op).

---

## Architectural alternatives considered

| Approach | Verdict | Why not |
|----------|---------|---------|
| Quartz/CA-style server-managed scene tree | Too closed; new primitives need OS work |
| Wayland-style "client renders, server composes" | Loses CAS dedup story; clients duplicate raster work |
| WebRender (browser-internal) | Right architecture, wrong layering — too tied to one app |
| Skia + IPC bitmaps | Pixels on the wire; GPU under-utilized |
| **Fresco: scenegraph protocol + GPU bundles** | ✓ | All four goals |

The benchmark probe (`docs/spec/fresco-runtime-benchmark*.md`) measured GPU compute per-node scaling on Metal as the *go/no-go* gate before committing to this design. (See later slide for what "GPU traversal" really means.)

---

## The four layers

```
┌────────────────────────────────────────────────────────────┐
│ 1. Scenegraph protocol  ── what apps send                  │
│    (atrium-rpc CLASS_DISPLAY: SLOT_SET, SCENE_NODE_SET, …) │
├────────────────────────────────────────────────────────────┤
│ 2. SPIR-V bundles       ── what extensions ship            │
│    (manifest + compute kernels + render pipelines)         │
├────────────────────────────────────────────────────────────┤
│ 3. Scene server         ── fresco-server (POC binary)      │
│    (wire dispatch, CAS, GPU resource table, frame driver)  │
├────────────────────────────────────────────────────────────┤
│ 4. Vulkan + vendor      ── what runs on the GPU            │
│    (MoltenVK on macOS; vendor Vulkan on Linux/FreeBSD)     │
└────────────────────────────────────────────────────────────┘
```

The contract between each layer is **explicit and small**. Most of the complexity lives at the boundaries, not inside the layers.

---

## Stack comparison — current FreeBSD vs. Fresco

<div style="font-size: 12px; line-height: 1.05;">

```
   Current FreeBSD desktop          Fresco on FreeBSD
   ───────────────────────          ──────────────────────────
   ┌────────────────────┐           ┌────────────────────┐
   │    Application     │           │    Application     │
   └─────────┬──────────┘           └─────────┬──────────┘
             │                                │
   ┌─────────▼──────────┐           ┌─────────▼──────────┐
   │ Toolkit + renderer │           │ Toolkit (any) —    │
   │ Qt/GTK/SDL +       │           │ emits scene-graph  │
   │ Skia/Cairo or      │           │ messages; does NOT │
   │ app-side GL / Vk   │           │ render pixels      │
   └─────────┬──────────┘           └─────────┬──────────┘
             │ pixel buffers                  │ atrium-rpc msgs
   ┌─────────▼──────────┐           ┌─────────▼──────────┐
   │ libwayland (UDS)   │           │ atrium-rpc (UDS)   │
   └─────────┬──────────┘           └─────────┬──────────┘
             │                                │
   ┌─────────▼──────────┐           ┌─────────▼──────────┐
   │ Wayland compositor │           │   fresco-server    │
   │ sway / KWin /      │           │ WM + per-app tree  │
   │ Mutter — composes  │           │ flatten +          │
   │ N textures via GL  │           │ ░░atrium-core░░    │
   │                    │           │ ░SPIR-V bundle░    │
   │                    │           │ (GPU code, loaded) │
   └─────────┬──────────┘           └─────────┬──────────┘
             │                                │
   ┌─────────▼──────────┐           ┌─────────▼──────────┐
   │ Mesa / vendor      │           │ Vulkan API         │
   │ Vulkan + libdrm    │           │                    │
   └─────────┬──────────┘           └─────────┬──────────┘
             │                                │
   ┌─────────▼──────────┐           ┌─────────▼──────────┐
   │ drm-kmod via       │           │ Atrium GPU ABI +   │
   │ linuxkpi (Linux    │           │ native FreeBSD     │
   │ DRM port)          │           │ kernel driver      │
   └─────────┬──────────┘           └─────────┬──────────┘
             │                                │
   ┌─────────▼──────────┐           ┌─────────▼──────────┐
   │   GPU hardware     │           │   GPU hardware     │
   └────────────────────┘           └────────────────────┘
```

</div>

---

## What the stack diagram shows

Three structural changes vs. the current FreeBSD desktop:

1. **The "rendering" responsibility splits three ways** (next slide unpacks this). What was monolithic in each app — describe + coordinate the GPU + draw pixels — becomes three separate concerns living in three different homes: the app, fresco-server, and the SPIR-V bundle on the GPU.

2. **A new SPIR-V bundle layer appears inside the compositor box.** Per-node GPU work runs as bundle code, not host code. Extensions ship as SPIR-V, not `.so`. There's no equivalent in any existing stack.

3. **The kernel-side stack becomes native.** Today FreeBSD desktops run Linux's DRM driver through the `linuxkpi` shim (drm-kmod). The Atrium phase-D5 work replaces this with a native FreeBSD GPU driver behind the **Atrium GPU ABI** — Fresco still talks Vulkan; the layer underneath becomes a first-class FreeBSD citizen rather than a Linux compatibility surface.

Boxes that didn't change: the application itself, the IPC mechanism (UDS in both cases), Vulkan as the GPU API, and the hardware. Fresco isn't reinventing those; it's recomposing the layers between them.

---

## Three responsibilities, three layers

The "rendering" job in a Wayland-style app is actually three concerns wearing one hat. Fresco splits them:

<style scoped>
table { font-size: 18px; }
th, td { padding: 4px 8px; vertical-align: top; }
</style>

| Responsibility | Wayland-style app does it all | **Fresco splits it** |
|---|---|---|
| **WHAT** — describe the scene as data | App (in its own head) | <span style="color:#0a5d6c">**App** — wire-format scene tree</span> |
| **HOW/WHEN** — GPU buffers, command buffers, dispatch, submit, present | App's own GL/Vulkan context | <span style="color:#0a5d6c">**Scene server** — orchestrator only, pushes zero pixels</span> |
| **PIXELS** — vertex transforms, fragment shading, sampling | App's GL/Vulkan + app-supplied shaders | <span style="color:#0a5d6c">**SPIR-V bundle on the GPU**</span> |

The scene server is **not** "the app's renderer relocated." It's a *coordinator role that didn't exist before*: pixel work moved out of each app's GPU context into a shared SPIR-V bundle, and the scene server is the new layer between scene descriptions and that bundle.

**Each layer does exactly one thing** — where a Wayland-style app did all three.

---

## Layer 1 — The scenegraph protocol

Apps don't draw. They **describe scenes**, then commit frames.

```
SLOT_SET slot=1 hash=c2a3193a… kind=Texture(1024,1024,Rgba8Srgb)
SCENE_FRAME_BEGIN
  SCENE_NODE_SET node=0  op=texture  params=(x=4, y=4, w=184, h=100, slot_id=1)
  SCENE_NODE_SET node=1  op=texture  params=(x=196,…)
  …
  SCENE_NODE_SET node=99 op=texture  params=(…)
SCENE_FRAME_END
```

- Retained-mode: nodes persist across frames; only deltas travel
- `slot_id` (4 bytes) replaces `texture_bytes` (4 MiB) in scene messages
- Compositor renders at its own cadence; client commits when it has a new frame

---

## Wire format — atrium-rpc envelope

Borrowed wholesale from the Atrium IPC substrate. **Don't reinvent IPC.**

```
┌──────────────────── 10-byte envelope ────────────────────┐
│ ver(1) │ class(1) │ op(2) │ flags(2) │ payload_len(4)    │
└──────────────────────────────────────────────────────────┘
                          │
                          └─► payload (postcard-encoded)
```

- `class = CLASS_DISPLAY (1)` for Fresco; `CLASS_CORE (0)` for built-in CAS
- Two op categories:
  - **Control ops** (host-handled): `SLOT_SET`, `SCENE_FRAME_BEGIN/END`, `SCENE_NODE_SET/CLEAR`
  - **Scene ops** (bundle-dispatched): inner payload's `op_id` selects the GPU pipeline

---

## CAS dedup — the killer feature

Built-in to atrium-rpc's `CLASS_CORE`:

```
client                                       compositor
──────                                       ──────────
upload_blob(4 MB texture)
  │  UPLOAD_BEGIN  (10 + 4 KiB inline)  ──►  store(hash → bytes)
  │  UPLOAD_DATA × 64 (64 KiB each)     ──►  append
  │  UPLOAD_FINISH                      ──►  ACK(hash)
  ▼
hash = c2a3193acffee3fc…

SLOT_SET slot=1 hash=c2a3193a… kind=Texture(…)  ──►  vkImage upload
SCENE_NODE_SET node=N op=texture slot_id=1      ──►  reference (4B)
                       × 100
```

The blob travels the wire **once**. Scene nodes reference it by 4-byte `slot_id`.

---

## CAS dedup — measured

POC's Scene B (1× 4 MiB texture, 100 instances):

```text
scene-b wire bytes: 4 200 830 (4.01 MiB)
naive  (texture inline × 100): 419 430 400 (400 MiB)
ratio: 99.8×
```

<span class="big">**99.8×**</span> bandwidth reduction.

The framing overhead (~6 KiB out of 4 MiB) is dominated by the 4 MiB blob itself; the 100 references are ~3 KiB total. For long-lived scenes (UI redraws, animations), the blob travels **zero** times after the first frame — the CAS hit on the compositor side is free.

Same hash space as Tessera (the FreeBSD CAS-FS), so persistent assets dedup across the whole machine when the OS port lands.

---

## Layer 2 — SPIR-V bundles

An "extension" to Fresco isn't a `.so` file. It's a **directory of SPIR-V**.

```text
bundles/atrium-core/
├── manifest.json
├── compute/
│   ├── op_rectangle.comp.spv
│   └── op_texture.comp.spv
└── pipelines/
    ├── pipe_rectangle.vert.spv
    ├── pipe_rectangle.frag.spv
    ├── pipe_texture.vert.spv
    └── pipe_texture.frag.spv
```

- No host code. No dlopen. No syscalls. Pure GPU code.
- Validated by `spirv-val` at load time
- AOT-compiled into Vulkan pipelines at compositor startup
- **Descriptor layouts derived from SPIR-V reflection** — the shader is the source of truth

---

## Bundle manifest

```json
{
  "name":    "atrium-core",
  "version": 1,
  "ops": [
    { "id": 4096, "name": "rect",
      "compute_entry":   "compute/op_rectangle.comp.spv:main",
      "render_pipeline": "pipelines/pipe_rectangle" },
    { "id": 4097, "name": "texture",
      "compute_entry":   "compute/op_texture.comp.spv:main",
      "render_pipeline": "pipelines/pipe_texture" }
  ],
  "depends_on":   [],
  "gpu_resources": { "max_instances": 65536 }
}
```

Manifest is *just* registration. No host-side ABI to keep stable. Adding a new op is: drop a `.comp` + `.vert` + `.frag`, add an entry, restart compositor.

---

## The closed op-ID registry (§3.4)

Op IDs are **not** vendor-allocated at runtime. They live in a closed registry:

| Range          | Owner                | Examples                               |
|----------------|----------------------|----------------------------------------|
| `0x0000–0x0FFF` | Reserved             | (future control ops)                   |
| `0x1000–0x1FFF` | **atrium-core**      | `rect=0x1000`, `texture=0x1001`, …     |
| `0x2000–0x7FFF` | Atrium-blessed bundles | atrium-text, atrium-vector, …        |
| `0x8000–0xFFFE` | Engine compat layers | unreal, unity, godot, …                |
| `0xFFFF`        | `vendor.*` namespace | private experiments, signed bundles    |

Why closed: an open registry leads to op-ID squatting and protocol forking. Each bundle has a stable, conflict-free home.

---

## Why SPIR-V (not host-loadable extensions)

The deeper question: *what's the bottom contract?*

|                | Host `.so` plugin             | SPIR-V bundle                         |
|----------------|-------------------------------|---------------------------------------|
| Sandboxing     | Trust, or full process isolation | **Implicit** — GPU can't escape    |
| ABI stability  | C ABI of compositor internals | Vulkan + SPIR-V (industry-standard)   |
| Compile target | Per-OS, per-arch              | **Single artifact, all platforms**    |
| Tooling        | Each language's toolchain     | `glslangValidator` (or HLSL, Slang)   |
| Failure mode   | Plugin crash = compositor crash | **Pipeline rejection at load time** |

The architectural cost: extensions can't do host-side work (file I/O, networking, audio). For a *rendering* extension that's the right constraint.

---

## What's a "scene server"?

`fresco-server` is the POC binary; the role it plays is the **scene server**. Why a new term?

- **Not a "compositor"** — that word (Wayland/X11/DWM lineage) means *composes pre-rendered surfaces into a final image*. The scene server never sees pixels.
- **Not just a "server"** — too generic; "server" tells you nothing about what it owns.
- **"Scene server"** captures both halves: it owns the multi-app **scene** state, and it serves clients over IPC.

**Closest shape in existing systems: iOS Render Server.** It also has a server-side canonical scene-graph mirror that can advance animations while the app is suspended — the *server-side scene-graph* idea isn't novel. Where Fresco differs is the **SPIR-V bundle layer**: primitives are an open extension point, not baked into the server. (macOS WindowServer is *not* analogous — it composites pre-rendered IOSurfaces, Wayland-shaped.)

---

## Scene server vs. iOS Render Server

<style scoped>
table { font-size: 17px; }
th, td { padding: 4px 8px; vertical-align: top; }
</style>

| Property | iOS Render Server | Fresco scene server |
|---|---|---|
| Server-side scene-graph mirror | ✓ | ✓ |
| Delta-only updates | ✓ (CA property diffs) | ✓ (`SCENE_NODE_SET`/`CLEAR`) |
| Server-side animation (app suspended) | ✓ (`CAAnimation`) | ✗ — re-sends per frame *(gap)* |
| Resource sharing | IOSurface — zero-copy, intra-machine, per-app | CAS — content-hash dedup across apps / machines / time (shared with Tessera CAS-FS) |
| Wire-format scene graph (open, debuggable) | ✗ — Apple-internal IPC | ✓ — `atrium-rpc` postcard schemas |
| Extensible primitives | ✗ — baked into CA | ✓ — SPIR-V bundles |

**iOS wins on**: server-side animations, intra-machine zero-copy.
**Fresco wins on**: extensibility, openness, cross-app/machine/time CAS dedup.

---

## Scene server — seven roles in one process

| Role | What it does |
|---|---|
| **IPC server** | Listens on UDS, parses atrium-rpc envelopes |
| **Scene-state authority** | Owns canonical per-app scene trees + slot tables |
| **Scene-graph processor** | Walks trees, applies WM transforms, flattens per-frame |
| **Window manager** | Placement, z-order, focus, input routing |
| **Bundle host** | Loads SPIR-V bundles, AOT-compiles pipelines |
| **GPU resource broker** | CAS hash → vkImage, slot → descriptor set |
| **GPU command driver** | Records command buffers, submits, presents |

These are not seven separate components — they're one process whose responsibilities span the full path from "client opened a connection" to "swapchain image presented." The next slide shows what one frame of this looks like.

---

## Layer 3 — The scene server (`fresco-server`)

Per-frame work, nothing else:

```text
on RedrawRequested:
   1. drain pending CAS uploads from dispatcher
        for each: vkImage + transfer barrier + cmd_copy
   2. snapshot SceneState under lock; decode params per op
   3. write per-op scene buffers (host-mapped, persistent)
   4. begin command buffer
        for each plan:
           cmd_fill_buffer(counter = 0)
           cmd_dispatch(compute kernel)            ← traversal
           barrier compute → vertex + host
        begin render pass
        for each plan:
           cmd_draw(4 verts, n instances)         ← rasterization
        end render pass
   5. submit + present
```

**~1300 lines of Rust** for the entire scene server (POC). The scene server is a *coordinator*, not a renderer.

---

## Layer 4 — GPU-driven scene processing

The architectural bet: **per-node scene work runs on the GPU**, not the CPU.

```glsl
// op_rectangle.comp — one thread per scene node, in parallel
layout(set=0, binding=0) readonly buffer SceneBuf  { uint node_count; SceneNode nodes[]; } scene;
layout(set=0, binding=1) writeonly buffer InstanceBuf { InstanceRecord instances[]; } instance_buf;
layout(set=0, binding=2) buffer CounterBuf { uint count; } counter;

void main() {
    uint id = gl_GlobalInvocationID.x;
    if (id >= scene.node_count) return;
    uint slot = atomicAdd(counter.count, 1);   // <-- GPU emits draw
    instance_buf.instances[slot].model = vec4(/* …from node… */);
}
```

CPU dispatches `ceil(N/64)` work-groups; GPU does per-node SIMD work and atomic-appends. ONE indirect draw paints all of them.

---

## What "GPU traversal" really means (and doesn't)

**GPUs are bad at** classical tree walking:
- Pointer-chasing diverges across SIMD lanes
- Variable-depth recursion stalls warps on slow lanes
- Random memory access kills cache locality

**So Fresco never asks the GPU to walk a tree.** What v2 GO measured (and what the POC validated) is the *parallel-for* shape:

```
flat array of N nodes  ──►  one thread per node  ──►  atomicAdd / write
       (input)               (independent work)       (output)
```

…exactly what GPUs are *great* at — same shape as particle systems, GPU sorting, image filters.

The spec's **§3.6 "GPU-driven traversal"** is shorthand for *per-node work runs on the GPU after the host flattens the scene*. Not a claim that the GPU walks parent pointers.

---

## How the host handles hierarchy

The host (`fresco-server`) keeps the **scene tree** — the canonical retained-mode structure with parent/child links, transforms, clip rects. Each frame the host does the irregular work:

```
SceneState (tree)         Per-op flat buffers
─────────────────         ───────────────────
  root                       SceneNode[0]   ← rect at (10, 10)
    rect (10, 10)            SceneNode[1]   ← rect at (110, 50)   (parent xform applied)
    group (translate 100,50) SceneNode[2]   ← texture at (320, 70)
      rect (10, 0)
      texture (220, 20, slot=1)
```

- **Host** walks the tree (cheap — only when the scene mutates), bakes parent transforms into per-node screen-space coords, sorts/groups by op-id
- **GPU** runs one parallel-for per op-id over the resulting flat array

This split is the same one game engines have used for 20+ years: irregular work on CPU, regular work on GPU. Trees stay where trees belong.

---

## How the host merges multiple apps

Each app has its **own** scene tree on the host (per-connection state — apps can't see or mutate each other's scenes). The merge happens at the *flatten* step, not at the tree level:

```
   conn 1 ──► SceneState_app1 ─┐
   conn 2 ──► SceneState_app2 ─┼─► WM transform per app ─► per-op flat buffer
   conn 3 ──► SceneState_app3 ─┘                            (shared, fed to GPU)
```

**What's shared, what isn't:**
- **Per-app** (privacy/security): scene tree, slot table, CAS cache
- **Shared**: GPU pipelines, descriptor pools, per-op flat buffers, the GPU
- **Window manager** (z-order, focus, app placement) sits between per-app scenes and the flatten step — that's where compositing policy lives

From the GPU's view there's no notion of "app" — just N rect-instances, M texture-instances, etc.

<span class="small">*POC currently uses one global `SceneState`; per-connection split is designed-but-unimplemented.*</span>

---

## Why the host's per-frame job is small

For a typical UI (~10⁴ nodes total across apps): tree walk + transforms + memcpy → **tens to hundreds of µs** (≈ 5 % of a 16.6 ms frame).

<style scoped>
table { font-size: 17px; }
th, td { padding: 4px 8px; vertical-align: top; }
</style>

| Approach | CPU work per frame | Cost |
|---|---|---|
| Software-rasterizing apps | Each app runs Skia/Cairo on CPU | **ms/app × N apps** |
| GPU-rendering apps (Wayland, DWM, Quartz) | Each app records its own GL/Vulkan, then compositor composes | **~µs/app × N + N GPU contexts** |
| **Fresco** | One host walks all apps' trees; GPU does per-node work | **~µs total + 1 GPU context** |

Fresco's host-side cost is *less than what a single existing app* spends rendering itself: no per-pixel CPU work, **one** GPU context (not N), one shared font/glyph/texture cache. The systemic win is **dedup of the rendering pipeline across apps** — existing stacks pay the per-app GPU-context tax N times; Fresco pays it once.

<span class="small">*Numbers reasoned from first principles + Quartz/CA design docs; v1/v2 benchmarks measured the GPU side, not host tree-walking.*</span>

---

## When the tree IS deep — the multi-level pattern

For workloads where the tree is too deep to flatten on the host every frame (large UI hierarchies, animated rigs), the planned **§3.6 multi-level scene buffer** uses parallel rounds:

```
Round 0 (level K=0): GPU dispatch over root          → 1 thread
Round 1 (level K=1): GPU dispatch over K=0's outputs → N₁ threads
Round 2 (level K=2): GPU dispatch over K=1's outputs → N₂ threads
…
Round D (leaves):    GPU dispatch over level D-1     → N_leaf threads
```

Each round is a flat parallel-for; the host orchestrates `D` dispatches with barriers between. Depth `D` becomes the host-side cost (cheap — typically < 10 for UI scenes), and width at each level is GPU-parallel.

**This is not implemented in the POC** — the POC's scenes are one level deep. It's the architectural escape hatch for when host-side flattening becomes the bottleneck.

---

## The v1 NO-GO → v2 GO benchmark

The earlier work in `docs/spec/fresco-runtime-benchmark{,-v2}.md`:

- **v1**: probed CPU-side per-node work at scale (host loop, one rect at a time). Result: **NO-GO** — host can't keep up past a few × 10⁵ nodes/frame.
- **v2**: probed the GPU compute pattern — flat node array, one thread per node, atomic counter + indirect draw — on Metal as a stand-in for Vulkan. Result: **GO** — GPU saturates well past 10⁷ nodes/frame.

Both benchmarks measured the *parallel-for* shape. Neither measured tree-walking on the GPU; that was deliberately out of scope (and would have failed). The POC validates that the v2 pattern survives the move from a synthetic Metal probe to real Vulkan + descriptors + barriers + swapchain. **Same shape, same outcome.**

---

## CPU/GPU work split

```text
                CPU                              GPU
                ───                              ───
Frame N:        Decode wire deltas
                Update SceneState                  ─
                                                   ─
Frame N+1:      Snapshot scene                     ─
                Memcpy nodes → mapped buffer
                Record cb (≈ 30 vk calls)        Compute traversal
                Submit                           Indirect draw
                                                 Present

```

CPU per frame: O(deltas), not O(scene). The compositor's CPU cost is constant in scene size.

---

## What the POC built

12 commits in `~/src/fresco-poc`:

| Phase | What |
|-------|------|
| 1–2   | winit + Vulkan instance + swapchain + clear |
| 3–4   | `atrium-core` bundle (GLSL → SPIR-V → AOT pipelines) |
| 5a    | `atrium-rpc-display` payload schemas (postcard) |
| 5b    | UDS server, `Connection::recv_message` dispatch |
| 6     | `SLOT_SET` → `vkImage` upload + per-slot resource table |
| 7     | Compute traversal (atomic counter + readback verification) |
| 8     | `cmd_draw(4, n, 0, 0)` — rect rendering visually verified |
| 9     | Texture op + per-slot descriptor sets + sampler |
| 10–11 | scene-a (1000 rects), scene-b (100 textures), bandwidth assertion |
| +     | SPIR-V reflection (replaces hardcoded layouts) |

---

## What the POC validated

- ✓ **§3 SPIR-V bundle architecture** — manifest + reflection + AOT, two ops in one bundle
- ✓ **§3.6 GPU per-node compute** — atomic-counter readback proves kernel ran on every node (parallel-for shape; no tree walking on GPU, by design)
- ✓ **§3.7 CAS dedup wire format** — 99.8× measured
- ✓ **§3.4 op-ID registry mechanism** — runtime dispatch by op-id
- ✓ **Cross-platform Vulkan** — works on MoltenVK; same code paths for FreeBSD/Linux

Two screenshots:

- **scene-a**: 1000 randomly-placed coloured rects at 60 fps
- **scene-b**: 10×10 grid of textured quads, all sampling one uploaded image

---

## What the POC does *not* validate

Limitations:

- **Throughput at v2's scale** — POC ran 1000 rects + 100 textures, not the millions the benchmark hit. Architecture *shape* validated; scaling re-measure is downstream work.
- **Multi-bundle composition** — only `atrium-core` loaded. Bundle dependency resolution and op-ID conflict handling exist in code but are exercised by exactly one bundle.
- **Hierarchical scene traversal** — POC scene is flat. Real apps want parent transforms, clip rects, etc.; the host-flatten + multi-level GPU pattern is designed but not implemented.
- **POSIX signals / app crashes** — what happens when an app dies mid-frame, or holds a slot forever, isn't tested.

These are *future work*, not *broken assumptions*. The thesis claims hold.

---

## What the closed registry buys us

A concrete example of why the registry shape matters:

> Suppose Unity ships a bundle that uses op-id `0x4242` for "lit mesh".
> Suppose Unreal *also* ships a bundle for "lit mesh" — and picks `0x4242`.

Without a registry, the second bundle silently overrides the first (or fails to load).
With the registry, both engines get their own range (`0x8000–0x9FFF` for unreal, `0xA000–0xBFFF` for unity) at design time. **No coordination at runtime.**

The Atrium project owns `0x1000–0x7FFF` — and is the only authority that can reissue.

---

## Where this leads

The POC unblocks several Atrium initiatives:

- **D2 (scene server on FreeBSD)** — port `fresco-server` (likely renamed `fresco-scene-server` / `frescod`) to vendor Vulkan + native event loop. The wire protocol is stable; the porting cost is the platform shim.
- **D3 (atrium-text bundle)** — ship a glyph-rendering bundle (`0x2000` range). Validates the multi-bundle path.
- **D5 (Atrium GPU ABI)** — replace MoltenVK / linuxkpi with the native FreeBSD GPU driver. Fresco still talks Vulkan; the driver layer changes underneath.
- **Tessera integration** — same SHA-256 hash space, so once Atrium ships, persistent assets dedup across CAS-FS automatically.

The scene server isn't a research vehicle. It's the **default Atrium graphics stack** going forward.

---

## Tradeoffs we deliberately accepted

| Got                                | Gave up                                           |
|------------------------------------|---------------------------------------------------|
| GPU-driven traversal               | Extensions can't do host-side work                |
| Wire-format dedup                  | Apps must be CAS-aware (use `upload_blob`)        |
| AOT pipeline compilation           | First-frame latency on bundle load                |
| Closed op-ID registry              | Atrium is a coordination point                    |
| Vulkan as the bottom contract      | macOS needs MoltenVK shim; iOS/console-locked GPUs need different bottom contract |
| Single coherent design across 4 layers | Can't slot Fresco into an existing Wayland/X11 stack — it IS the stack |

These tradeoffs are *features*, not bugs. They're what makes the four goals achievable simultaneously.

---

## Summary

Fresco's contribution isn't any single technique — GPU compute traversal, CAS dedup, SPIR-V bundles, and op-ID registries all have prior art. The contribution is **picking all four at the same time** and arranging them so they reinforce each other:

- The wire format is small *because* CAS deduplicates
- The scene server is small *because* extensions are SPIR-V
- The scene server scales *because* per-node work runs on the GPU
- The protocol stays clean *because* op IDs are closed

POC measured this works. Spec captures the design. Next step is shipping it as the FreeBSD desktop default.

---

# Questions

<br>

**Spec**: `docs/spec/fresco-rendering-stack.md`
**Benchmarks**: `docs/spec/fresco-runtime-benchmark.md`, `…-v2.md`
**POC**: `~/src/fresco-poc` (commits `b782ce1..9c3033f`)
**This deck**: `docs/spec/fresco-rendering-stack.slides.md`

<br>

<span class="small">
Render with Marp:
`marp fresco-rendering-stack.slides.md --pdf` (or `--html`, `--pptx`)
</span>
