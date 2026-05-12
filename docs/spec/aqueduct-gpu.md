# Aqueduct-GPU — Atrium's GPU protocol

> **Status.** Design. Replaces atrium-venus.md for the macOS-host bring-up
> path; atrium-gpu-abi-v2.md remains the kernel ABI for both this transport
> and the future native-HW drivers.
>
> **Companion docs.** Read `aqueduct.md` first (envelope transport, CAS),
> then `atrium-gpu-abi-v2.md` (kmod cdev/ioctl conventions). This doc
> defines the GPU-dispatch opcode class layered on top of aqueduct.
>
> **One-line summary.** Aqueduct-gpu is a single GPU mechanism for all of
> Atrium: a frame-batched protocol that frescod's renderer and Atrium's
> Vulkan ICD both speak. Every shader runs in a sandbox; every Vulkan
> feature must be expressible with sandbox-compatible semantics; on
> native hardware, the round-trip profile is peer to native Linux
> Vulkan drivers like radv. No virtio-gpu, no virglrenderer, no venus.

---

## 1. Why we're doing this — and why now

Venus was a bring-up convenience: get a Vulkan ICD running in the guest
VM with minimal code, validate the atrium-mesa fork chassis, get
hardware-accelerated rendering on the macOS host so we can iterate on
the compositor and toolkit without a real GPU driver. Six months later,
the rendering loop on the macOS-HVF + venus + virglrenderer + MoltenVK
chain has accumulated a long tail of per-frame issues:

- `BLOB_MEM_HOST3D` is the only working shmem type (`GUEST` fails at
  venus's worker boundary because macOS lacks udmabuf)
- The virtio-gpu 512 MB BAR aperture with overlapping subregions trips
  HVF stage-2 staleness on every resource map (worked around in
  `qemu-build/hw/display/virtio-gpu-virgl.c` with a touch+mlock pre-fault)
- Venus async fences need `VIRTIO_GPU_FLAG_INFO_RING_IDX` routing on
  NO_VIRGL hosts; without it the kmod's legacy global-fence path is
  broken on macOS
- Venus's ring shmem at offset 0x80 aliases libthr's `struct pthread`
  on the guest because of HVF stage-2 inconsistency (worked around in
  mesa `vn_ring.c` by isolating the ring mutex on its own page)
- After all those fixes, `vkResetCommandBuffer` then `vkEndCommandBuffer`
  serially hang on frame 2 because venus serializes every Vulkan
  command-buffer operation through MoltenVK's MTLCommandBuffer release
  pipeline, which lags the fence-signal callback by more than one frame
  on macOS host

Each fix is "one more workaround in someone else's code." The pattern is
clear: **venus is a generic Vulkan-shaped paravirt protocol, but we are
not a generic Vulkan-shaped client.** We render at frame granularity, our
clients (vestibulum, atrium-edit, eventually pergola apps) never see
Vulkan, our long-term target (native atrium-gpu kernel drivers in D5+)
won't have a paravirt layer at all, and even our Vulkan path can compress
hundreds of `vkCmd*` calls per frame into one wire envelope. We have been
paying a tax for flexibility we don't use.

Venus also has no long-term home in the stack. When D5+ lands native
GPU drivers, the entire venus + virglrenderer + virtio-gpu chain has to
be ripped out anyway. Investing now in a protocol we'll keep is better
than investing in a protocol we'll throw away.

Aqueduct-gpu does the minimum amount of work that gets atrium's
compositor and its games onto the host GPU during bring-up — and onto
native hardware in D5+ — layered on the transport we already have.

---

## 2. Architecture

```
  GUEST  (FreeBSD VM under macOS, or native FreeBSD)

    Atrium UI clients (vestibulum, atrium-edit, ...)
       │  fresco-protocol over aqueduct
       ▼
    frescod (compositor) ──────────────┐
       │  GPU dispatch via aqueduct-gpu│
       │                               │
    3D games (Vulkan apps)             │
       │  libvulkan → atrium-vk-icd    │
       │  GPU dispatch via aqueduct-gpu│ Same protocol, same wire,
       ▼                               │ same sandbox. The ICD is
    aqueduct-gpu (this spec)           │ just a Vulkan-shaped API
       │                               │ surface over the universal
       │                               │ mechanism.
       ▼  via the host endpoint        │
                                       │
                                       │
  HOST   (macOS dev — bring-up only)   │
                                       │
    aqueduct-gpu-host daemon  ◄────────┘
       │  forks per connection, dispatches commands
       ▼  to:
    MoltenVK (macOS)   |   Vulkan loader (Linux host VM dev)
       │
       ▼
    Real GPU
```

In D5+ on native HW, the picture collapses: the guest-side
aqueduct-gpu client talks **directly to the atrium-gpu kmod** instead
of going through a host endpoint. Same wire protocol, the kmod just
*is* the endpoint. Atrium UI apps and games don't change.

### 2.1. What we keep from Mesa

Aqueduct-gpu uses Mesa as a compiler library, not as a driver chassis:

- `src/compiler/nir/` — IR + optimisation passes (constant folding,
  dead code, control flow flattening, scheduling hints, register
  allocation prep). Decades of work that we are not rewriting.
- `src/compiler/spirv/` — SPIR-V → NIR translator (consumed by the
  ICD when applications pass SPIR-V shader modules).
- `src/vulkan/runtime/`, `src/vulkan/util/` — object lifetimes,
  format helpers, alignment / extent / view utilities.
- Backend NIR codegen for each supported GPU vendor: `radv`'s
  AMDGCN backend, `anv`'s Intel Gen backend, `nouveau`/NVK for
  NVIDIA — used at AOT compile time only, *not* on the runtime
  hot path.
- `src/util/` — hash tables, arenas, blob containers.

These are vendored at `external/mesa/` and built as part of
`atrium-mesa`. All other Mesa subdirectories — gallium3d,
`src/virtio/vulkan/` (venus), `src/microsoft/`, llvmpipe — are not
part of the Atrium build.

### 2.2. What disappears

When aqueduct-gpu lands, the following are removed from the Atrium tree:

- `external/mesa/src/virtio/vulkan/` (venus driver — entire directory)
- `external/virglrenderer/` (no longer a dependency)
- The `CONFIG_DARWIN` touch+mlock block in
  `external/qemu-build/hw/display/virtio-gpu-virgl.c` (no fixed BAR
  aperture to fault in)
- The `atrium_ring_mutex_init` calloc-cb defense in
  `external/mesa/src/virtio/vulkan/vn_ring.c` (no venus shmem aliasing)
- `docs/spec/atrium-venus.md` (superseded by this doc)

The `external/qemu-build` patches for HVF instruction emulation
(`target/arm/hvf/hvf.c` ISV=0 decode) and ivshmem MSI-X delivery
stay — those are independent of venus.

---

## 3. Design principles

**One mechanism, two policy axes.** Aqueduct-gpu is the single GPU
protocol for Atrium. Apps choose two orthogonal policies on top:

| Axis | Options |
|---|---|
| **API surface** | Direct (Rust aqueduct-gpu API) for Atrium-native apps, or Vulkan (via atrium-vk-icd) for portable apps |
| **Composition strategy** | Semantic (emit scene commands, frescod composes at scene-graph level) or Surface (render to an image, present as a textured rect) |

These choices are independent. A Vulkan game can use semantic
composition for its UI and surface composition for its 3D viewport,
in the same frame. The mechanism underneath does not care.

**Frame-batched.** Every commit to the host is at frame granularity.
The guest accumulates commands into a Frame envelope and submits
atomically. There is no per-`vkCmd*` round trip, no per-cb fence, no
per-cb pool serialization. On the wire, one envelope per frame,
regardless of how many draws it contains.

**All shaders AOT-compiled. No JIT on the hot path.**

For Atrium-native code (frescod bundles, Pergola toolkit, Atrium-native
Rust apps), shaders are pre-compiled to each supported backend at
*build time* and shipped alongside the application — MoltenVK
MTLLibrary blobs for macOS bring-up, AMDGCN/Intel-Gen/NVK ISA for
native HW, atrium-gpu ISA for D5+.

For third-party Vulkan applications that ship only SPIR-V (games,
mainly), atrium-pkg performs the AOT compile at *install time*, not
at runtime. The install pass detects the system's installed GPU
backends, walks the package's SPIR-V, compiles each shader against
each detected backend, and stores the resulting binaries in Tessera
CAS keyed by `(spirv_hash, backend_id, compiler_version)`. The game's
first launch sees a fully-warmed cache; `OP_GPU_SHADER_RESOLVE` hits
immediately, no compilation on the wire.

This is the Steam Deck model. Atrium's licensing policy excludes the
proprietary NVIDIA driver, so for every GPU backend we support (AMD
via radv, Intel via anv, NVIDIA via NVK, Apple via MoltenVK during
bring-up, atrium-gpu in D5+) we have an AOT-capable open-source
compiler.

`OP_GPU_SHADER_UPLOAD` is the cold-path fallback for dev iteration,
specialization-constant variants the install-time pass couldn't
enumerate, and the unusual case of programmatically-generated SPIR-V.

**Universal sandbox. No escape valve.** Every shader runs through
the same validation + isolation regime regardless of API surface or
composition choice. There is no "trust this code unconditionally"
path. Specifically:

- Static SPIR-V validation at upload time: bounded loops, no
  buffer-device-address, descriptor-bounded resource access only,
  no subgroup ops crossing workgroup boundaries.
- Per-bundle descriptor namespace: shaders see only their own
  resources; cannot reach the compositor's state or another app's
  textures.
- GPU hardware timeout per dispatch: TDR fires on shaders exceeding
  their declared time budget; affected frame fails, bundle is
  flagged.
- Resource quotas per connection: bounded memory, texture count,
  pipeline count.
- Bundle signing + manifest audit: third-party bundles signed by
  developer key (atrium-pkg's existing infrastructure); manifest's
  declared capabilities cross-checked against actual shader
  behavior at install time.

The sandbox primitives are doing double duty: safety (untrusted
shaders cannot compromise the system) *and* enabling fire-and-forget
wire design (since static validation has run, runtime trust can be
local-by-default).

**Memory is explicit and stable.** Every shared region has an id, a
fixed size, and an intended-use tag at creation time. No subregion
overlays, no dynamic aperture management. To "update" a region,
write into its mapped memory. To resize, destroy and recreate.

**One sync primitive: the fence.** Frame-granular. No semaphores,
no events, no timeline semaphores. Intra-frame ordering is expressed
by command ordering within a frame; inter-frame ordering is
expressed by fences.

**Hash-addressed for immutability, id-addressed for mutability.**
Shaders and pipeline-state-objects are content-addressed through
aqueduct's CAS. Memory regions, images, buffers, fences are
id-addressed with explicit lifetimes. IDs are assigned by the guest
deterministically within the connection's namespace; the host
validates as it processes, not on a synchronous response.

**Partitioned ID namespace.** Each resource ID (pipeline, image,
buffer, sampler, fence) is a u32 with its top 4 bits reserved as a
namespace tag:

| Tag (high 4 bits) | Allocator | Use |
|---|---|---|
| `0x0` | Atrium build process | Built-in pipelines/resources shipped in atrium-core / atrium-text bundles |
| `0x1`–`0xE` | Host endpoint at `OP_GPU_BUNDLE_LOAD` | Per-third-party-bundle namespace; up to 14 bundles loaded concurrently |
| `0xF` | ICD-runtime (monotonic) | App-created resources via Vulkan API or direct aqueduct-gpu |

Bundle-shipped resources receive deterministic IDs derived from the
manifest (`bundle_namespace × bundle_local_id`), assigned once at
bundle load and stable for the lifetime of the bundle on the host.
ICD-runtime resources are allocated monotonically per connection.
This means a frame command stream can mix references to all three
kinds of IDs without ambiguity, and bundle-shipped pipelines need
no per-app round-trip to discover their handles.

**Closed wire vocabulary.** The set of wire opcodes and frame ops
(§4.3, §5.1) is fixed by this spec. Third-party bundles do **not**
extend the wire format. All app expressiveness flows through (1)
shader bytecode (AOT-compiled, hash-addressed), (2) pipeline state
declarations in the bundle manifest, (3) descriptor binding choices
encoded in `FOP_BIND_DESCRIPTORS`, and (4) push-constant data in
`FOP_PUSH_CONSTANTS`. A bundle that wants to provide a "particle
dispatch" op contributes a compute shader + a pipeline declaration;
the bundle's app-side SDK turns `dispatch_particles(buffer, count)`
into `FOP_BIND_PIPELINE` + `FOP_BIND_DESCRIPTORS` + `FOP_DISPATCH`
on the wire. No new opcodes, no new payload schemas.

This is a deliberate constraint — a closed vocabulary is far easier
to validate than an open one. The validation surface for shaders
is well-defined (SPIR-V → NIR → static checks); a parallel
validation surface for arbitrary third-party wire ops would
explode the trust boundary. By restricting bundles to shaders +
manifests, the sandbox primitives (§11) cover everything that can
go wrong at the GPU layer.

---

## 3.5. Round-trip reduction

The single biggest runtime win of aqueduct-gpu vs venus is *not* wire
bandwidth — it is round-trip count. The protocol is designed
end-to-end to move decision-making to the right side of the wire.

**Frame batching.** All `vkCmd*` calls in the ICD record into a
guest-local FrameBuilder buffer; zero wire traffic per call. Only
`vkQueueSubmit` flushes the whole frame as one envelope.

**Locally-assigned resource IDs.** Each connection has its own id
namespace. The ICD allocates IDs monotonically without waiting for
host acknowledgement. Host validates as it processes; failures
propagate as async events into the next fence wait. Saves one round
trip per `vkCreate*` call during init — frequently hundreds of round
trips collapsed.

**Format-rules tables compute locally.** Queries like
`vkGetImageMemoryRequirements` are deterministic given (format,
usage, extent). The ICD carries the format-rules table per supported
backend (small, hundreds of entries) and answers locally. Zero wire
cost.

**Handshake-cached device properties.** Device properties, features,
format support — returned once in the `OP_GPU_HANDSHAKE` response and
cached. Apps that query device state during init (most do, sometimes
dozens of times) pay zero wire cost after handshake.

**Hash-cached pipeline resolution.** With atrium-pkg pre-warming,
the ICD's local table of (pipeline-state-hash → resolved id) lets
`vkCreateGraphicsPipelines` skip the wire on a cache hit. Most
shipped games hit this 100% of the time after first install.

Putting these together, a typical 60 fps game frame goes from:

| Phase | Venus today | Aqueduct-gpu |
|---|---|---|
| Init (load shaders, create resources) | ~hundreds of RTTs | 1–10 RTTs (with pre-warming) |
| Per-frame recording (~1000 draws) | ~1000+ RTTs | 0 wire traffic |
| Per-frame submit | 1 RTT | 1 RTT |
| Per-frame fence wait | 1 RTT | 1 RTT |
| **Per-frame total** | **~1000+ RTTs** | **2 RTTs** |

On macOS-HVF (10–60 ms RTT due to HVF tax) this is the difference
between 10s+/frame and 20–120 ms/frame.

---

## 3.6. Comparative latency: native Linux Vulkan as the real baseline

The venus comparison is a strawman — venus is known-slow. The
honest competitive comparison is to native Linux Vulkan drivers
like radv (AMD), anv (Intel), NVK (open NVIDIA).

A native userspace driver lives in the app's process. Per frame:
- `vkCmd*` calls: pure userspace function calls, recording into a
  command buffer the kernel will read. ~ns per call.
- `vkQueueSubmit`: one DRM ioctl. ~0.5–2 µs syscall overhead.
- `vkWaitForFences`: one DRM ioctl or syncobj futex wait.
  ~0.5–2 µs.

**~2 syscalls per frame, ~1–4 µs of round-trip overhead.** This is
the gold standard.

Aqueduct-gpu on native Atrium hardware (D5+) is structurally identical:

- `vkCmd*` via atrium-vk-icd: pure userspace, recording into a wire-
  format arena. ~ns per call.
- `vkQueueSubmit` → kmod ioctl with the frame envelope. ~1 µs.
- `vkWaitForFences` → kmod ioctl or kqueue wait. ~1 µs.

**Same shape, same syscall count, same ~1–4 µs per-frame floor.**

| Metric | Native Linux radv | Aqueduct-gpu D5+ | Aqueduct-gpu macOS-HVF |
|---|---|---|---|
| Per-frame syscall/RTT count | 2 | 2 | 2 |
| Submit overhead | ~1 µs | ~1 µs | ~10–60 ms (HVF tax) |
| Fence-wait overhead | ~1 µs | ~1 µs | ~10–60 ms (HVF tax) |
| Per-frame floor | ~2 µs | ~2 µs | ~20–120 ms |

The 10–60 ms HVF tax is a property of running under HVF, not of
aqueduct-gpu. Any paravirt stack pays it. Venus pays it × hundreds
of calls per frame; aqueduct-gpu pays it × 2.

### Where aqueduct-gpu can beat native Linux Vulkan

- **First-frame latency.** radv compiles pipelines on first use
  unless the app pre-warmed `VkPipelineCache`. Tessera-cached +
  atrium-pkg pre-warming means our first-frame is always warm.
  Steam Deck does this trick on top of radv specifically because
  radv-by-itself stutters on cold pipelines; aqueduct-gpu builds
  the equivalent into the platform.
- **Resource creation churn.** radv's `vkCreateImage` is mostly
  userspace but `vkAllocateMemory` is an ioctl per call. With
  locally-assigned IDs and async-create batched into the next
  frame, we fold those ioctls into the next submit. Saves N round
  trips during heavy init.
- **Cross-app pipeline dedup.** Tessera is content-addressed. Two
  games using the same engine share post-process / common shaders
  → compile work happens once across the whole system. radv's
  pipeline cache is per-app + per-driver-version; no cross-app
  dedup.
- **Frame-level whole-stream optimization.** The host endpoint
  sees the entire frame as one command stream and can do
  whole-frame analyses (redundant state elimination, dead barriers)
  before issuing to the backend. Native drivers optimize per call
  but lack the same global view.

### Where native Linux Vulkan is honestly ahead

- **Mature edge-case handling.** radv has years of dealing with
  weird sync corner cases, validation-layer compatibility, broken
  games that rely on specific driver quirks. Our ICD accumulates
  this knowledge over time but starts behind.
- **Vulkan extension count.** radv exposes ~80+ extensions. Our
  curated extension set will be smaller at launch. Each unsupported
  extension is an app that won't run.
- **Vendor-specific micro-optimizations.** radv tunes for AMD's
  instruction scheduling, descriptor layouts, hardware features.
  We inherit Mesa's NIR backends for generated-code quality (so
  the shader code itself is essentially the same), but driver-side
  state-change minimization and command-buffer layout tricks are
  vendor-specific work we'd replicate or inherit through the
  backend over time.
- **Battle-tested perf characteristics.** Years of profiling and
  fixing slow paths. Our perf profile is unknown until it ships.
- **Validation layers + tooling.** Khronos validation, RenderDoc,
  NSight Graphics, AMD's RGP all integrate cleanly with native
  ICDs. Our equivalents need to be written.

These gaps narrow with platform maturity. None are structural;
all are work.

---

## 4. Wire format

Aqueduct-gpu lives on top of aqueduct's envelope transport (see
`aqueduct.md` §3). One new opcode class, registered in
`aqueduct/src/classes.rs`:

```
CLASS_GPU = 9
```

### 4.1. The two-phase shader path

**Source language posture.** Atrium-native bundles author shaders in
**Slang** (Khronos-stewarded, Apache-2.0, no GL/DX legacy, multi-
backend emit). slangc compiles to SPIR-V, which is what flows over
the aqueduct-gpu wire; atrium-pkg's install hook (§4.2) further
cross-compiles to backend-native bytecode for whatever GPUs the host
exposes. The Slang choice is locked in
[`docs/LANGUAGE-POLICY.md`](../LANGUAGE-POLICY.md#shader-source-language);
the validator's `Unroll` acceptance (§11.1) is calibrated against
slangc output.

Third-party Vulkan apps still ship SPIR-V regardless of source
language (dxc, glslang, naga, etc.); the wire shape is the same.
"Slang by default" is a posture for *new* Atrium-shipped code, not
a wire restriction.

Aqueduct-gpu distinguishes between **resolve** (cheap, by hash) and
**upload** (slow, transfers bytecode + triggers compile). The common
case is RESOLVE always hits.

```
Atrium-native app launch:
  open atrium-text bundle
  read precompiled-shader-blob hashes from manifest.json
  OP_GPU_SHADER_RESOLVE { hash, backend: detected_gpu }
    → host has the blob (Atrium build process pre-compiled all
       supported backends)
    → returns shader_id immediately
  OP_GPU_PIPELINE_CREATE { state, shader_ids }
    → ready in O(microseconds)

Third-party Vulkan game AFTER atrium-pkg install:
  vkCreateShaderModule(spirv) → atrium-vk-icd computes hash
  OP_GPU_SHADER_RESOLVE { hash, backend }
    → host has the blob (atrium-pkg pre-compiled at install)
    → returns shader_id immediately
  (zero compile work on the runtime hot path)

Third-party Vulkan game, RESOLVE miss (rare cold path):
  ICD follows up with OP_GPU_SHADER_UPLOAD { hash, kind: SpirV, bytes }
  host translates SPIR-V → NIR → backend-native, caches in Tessera
  (next launch hits RESOLVE warm)
```

### 4.2. The atrium-pkg install-time shader pass

When `atrium-pkg install <game>` runs:

1. Detect installed GPU backends via `IOC_GPU_LIST_BACKENDS`. Backends
   are identified by `(vendor_id, generation_id)` pairs corresponding
   to compiler targets (RDNA3, Gen12LP, Ampere-SM86, M-series-Apple7,
   atrium-gpu-v1, etc.).
2. Scan the unpacked game for SPIR-V bytecode (`*.spv` files or
   embedded in the binary).
3. For each shader × each detected backend:
   - Hash the SPIR-V.
   - If Tessera already has `(spirv_hash, backend_id,
     compiler_version)` → skip (dedup wins, big in practice across
     games using the same engine).
   - Otherwise: compile via the appropriate backend compiler (Mesa
     subset built into atrium-pkg's compile worker; `xcrun metal` on
     macOS-host bring-up dev workflow), store the result in Tessera
     CAS, record `(spirv_hash, backend_id, compiler_version) → cas_hash`
     in the game's package metadata.
4. The shader pass runs in a background Portcullis jail (no game logic,
   just Mesa's compiler + CAS writes — small attack surface). Progress
   surfaced via standard atrium-pkg install UI.
5. When the game runs, atrium-vk-icd reads the package's shader-cache
   table and uses recorded `cas_hash` values when constructing
   `OP_GPU_SHADER_RESOLVE` requests.

Cache invalidation by compiler version: a Mesa rebase or atrium-mesa
bump retires stale entries; affected shaders recompile lazily on next
launch.

### 4.3. Opcodes

| Op                       | Value  | Direction | Returns response? |
|--------------------------|--------|-----------|-------------------|
| `OP_GPU_HANDSHAKE`       | 0x0001 | C → S     | yes (caps + format-rules) |
| `OP_GPU_MEMORY_CREATE`   | 0x0100 | C → S     | yes (id + import token) |
| `OP_GPU_MEMORY_DESTROY`  | 0x0101 | C → S     | no                |
| `OP_GPU_IMAGE_CREATE`    | 0x0110 | C → S     | no (id pre-assigned) |
| `OP_GPU_IMAGE_DESTROY`   | 0x0111 | C → S     | no                |
| `OP_GPU_BUFFER_CREATE`   | 0x0120 | C → S     | no (id pre-assigned) |
| `OP_GPU_BUFFER_DESTROY`  | 0x0121 | C → S     | no                |
| `OP_GPU_SAMPLER_CREATE`  | 0x0130 | C → S     | no (id pre-assigned) |
| `OP_GPU_SAMPLER_DESTROY` | 0x0131 | C → S     | no                |
| `OP_GPU_SHADER_RESOLVE`  | 0x0140 | C → S     | yes (id; lazy upload if miss) |
| `OP_GPU_SHADER_UPLOAD`   | 0x0141 | C → S     | yes (cold path)   |
| `OP_GPU_PIPELINE_CREATE` | 0x0150 | C → S     | no (id pre-assigned) |
| `OP_GPU_PIPELINE_DESTROY`| 0x0151 | C → S     | no                |
| `OP_GPU_FENCE_CREATE`    | 0x0160 | C → S     | no (id pre-assigned) |
| `OP_GPU_FENCE_DESTROY`   | 0x0161 | C → S     | no                |
| `OP_GPU_SUBMIT_FRAME`    | 0x0200 | C → S     | no (async)        |
| `OP_GPU_WAIT_FENCE`      | 0x0201 | C → S     | yes (signalled?)  |
| `OP_GPU_SHARE_SURFACE`   | 0x0210 | C → S     | yes (share token) |
| `OP_GPU_BUNDLE_LOAD`     | 0x0220 | C → S     | yes (bundle_namespace_id) |
| `OP_GPU_BUNDLE_UNLOAD`   | 0x0221 | C → S     | no                |
| `OP_GPU_FENCE_SIGNALED`  | 0x0301 | S → C     | async event       |
| `OP_GPU_DEVICE_LOST`     | 0x0302 | S → C     | async event       |
| `OP_GPU_VALIDATION_ERR`  | 0x0303 | S → C     | async event       |
| `OP_GPU_BUNDLE_LOAD_ERR` | 0x0304 | S → C     | async per-resource validation failure during bundle load |

The handshake exchanges supported features, format-rules tables, and
backend identification. Most create ops return no response — the
client pre-assigns IDs within its namespace and the host validates as
it processes. Validation failures and resource-creation failures
propagate as async events into the next fence wait, where they surface
to the client as standard Vulkan error returns.

`OP_GPU_BUNDLE_LOAD` and `OP_GPU_BUNDLE_UNLOAD` are the lifecycle
hooks for third-party scene-graph bundles (§7.3). LOAD takes a CAS
hash of the manifest (which transitively references shader hashes,
all pre-warmed in Tessera by atrium-pkg's install pass); the host
materialises all declared pipelines + render passes + samplers,
assigns them IDs in a fresh bundle namespace (`0x1`–`0xE` tag),
returns the namespace ID. Per-resource validation failures are
surfaced as `OP_GPU_BUNDLE_LOAD_ERR` async events; the bundle's load
returns success only if all declared resources passed validation.

### 4.4. Memory transport

Three transports, picked by data class:

**Inline (payload ≤ 4 KiB).** In the envelope payload itself. Used for
push constants, small descriptor-set updates, draw indirect arguments.

**CAS upload (large + immutable).** Via aqueduct's existing
`upload_blob` path. Used for shader bytecode and immutable texture
content. The CAS hash IS the resource identity host-side; same shader
uploaded by two connections deduplicates.

**Named memory region (large + mutable).** Created via
`OP_GPU_MEMORY_CREATE`. The host endpoint allocates host memory
(macOS: `shm_open` + `ftruncate` + `mmap`; native HW: real GPU memory
through the kmod). Returns:

```
MemoryCreateResponse {
    id:               u32,
    size:             u64,
    host_va_hint:     u64,
    atrium_gpu_token: [u8; 32],
}
```

The guest passes `atrium_gpu_token` to the atrium-gpu kmod via
`IOC_GPU_IMPORT_REGION`. The kmod resolves the token to the underlying
page-set; userspace `mmap(2)` on the kmod cdev fd exposes the region.

No fixed BAR aperture. Each region is its own mmap. No subregion
overlay games. No HVF stage-2 thrashing.

### 4.5. macOS-HVF stage-2 contract

The painful lessons from venus apply here too, but bounded to one
call per memory region (not per resource map cycle):

1. Host endpoint allocates region via `shm_open` + `ftruncate`.
2. Host endpoint touches every page with a non-zero byte then zeros
   it back (forces unique-page allocation, defeats CoW-zero-page
   sharing).
3. Host endpoint `mlock`s the region.
4. Host endpoint registers the region with QEMU (via ivshmem or
   `atrium-gpu-shmem` virtio device — see §6).
5. QEMU's MemoryListener installs the stage-2 mapping. Pages are
   pre-faulted + pinned, so HVF captures real backing PAs.
6. Guest kmod maps the region into userspace VA.

This dance happens **once per region**, at creation time. Not per
frame, not per command. The macOS-HVF serialization issue does not
arise because there is no per-frame mapping churn.

---

## 5. Command stream format

`OP_GPU_SUBMIT_FRAME` carries a single contiguous byte buffer in its
payload — the **frame command stream**, with its own internal
opcode/length framing, packed for fast guest-side serialisation and
host-side dispatch.

```
SubmitFramePayload {
    fence_id:    u32,
    command_buf: Vec<u8>,   // packed command stream
    timeline:    u64,        // monotonic; client-assigned, for ordering
}

record_header {
    op:     u16,
    flags:  u8,
    _pad:   u8,
    length: u32,    // total bytes of this record including header
}
```

### 5.1. Frame ops

| FrameOp                | Value | Body                                                |
|------------------------|-------|-----------------------------------------------------|
| `FOP_BEGIN_RENDERPASS` | 0x10  | color_target_ids[], depth_target_id, clear_values, viewport |
| `FOP_END_RENDERPASS`   | 0x11  | (empty)                                             |
| `FOP_BIND_PIPELINE`    | 0x20  | pipeline_id                                         |
| `FOP_BIND_DESCRIPTORS` | 0x21  | set_index, buffer_ids[], image_ids[], sampler_ids[] |
| `FOP_BIND_VERTEX_BUF`  | 0x22  | buffer_id, binding_index, offset                    |
| `FOP_BIND_INDEX_BUF`   | 0x23  | buffer_id, index_type, offset                       |
| `FOP_SET_VIEWPORT`     | 0x30  | x, y, w, h, min_depth, max_depth                    |
| `FOP_SET_SCISSOR`      | 0x31  | x, y, w, h                                          |
| `FOP_PUSH_CONSTANTS`   | 0x32  | stage_mask, offset, inline_bytes (≤ 128)            |
| `FOP_DRAW`             | 0x40  | vertex_count, instance_count, first_vertex, first_instance |
| `FOP_DRAW_INDEXED`     | 0x41  | index_count, instance_count, first_index, vertex_offset, first_instance |
| `FOP_DRAW_INDIRECT`    | 0x42  | buffer_id, offset, draw_count, stride               |
| `FOP_DISPATCH`         | 0x50  | x, y, z                                             |
| `FOP_DISPATCH_INDIRECT`| 0x51  | buffer_id, offset                                   |
| `FOP_COPY_BUF_TO_IMG`  | 0x60  | buffer_id, image_id, regions[]                      |
| `FOP_COPY_IMG_TO_BUF`  | 0x61  | image_id, buffer_id, regions[]                      |
| `FOP_BLIT`             | 0x62  | src_image_id, dst_image_id, regions[], filter       |
| `FOP_PIPELINE_BARRIER` | 0x70  | src_stage_mask, dst_stage_mask, image_barriers[], buffer_barriers[] |

Features reserved but not in v1 (added when a concrete app needs them):
mesh/task shaders, ray tracing (build-AS, trace-rays), sparse residency
binding, conditional rendering. Each is a finite extension to this
table plus a validator-rule addition; none changes the architecture.

### 5.2. Host-side decode

The host endpoint walks the command stream in one pass and translates
to its backend:

- **MoltenVK (macOS bring-up).** Each frame's command stream becomes
  one `MTLCommandBuffer` with one `MTLRenderCommandEncoder` per
  renderpass. Submitting the frame commits the command buffer.
- **Native Vulkan (Linux host or dev workstation).** One
  `VkCommandBuffer` per frame, recorded inline as the stream walks.
- **Atrium-gpu HW (D5+).** Direct submission ring entry, one per
  frame, no host daemon hop.

Pipelines are materialised host-side at `OP_GPU_PIPELINE_CREATE` time
(or pre-loaded at handshake from package metadata). By the time a
frame is submitted, every referenced pipeline_id is already a native
pipeline.

---

## 6. Transport on macOS-HVF bring-up

This section is specific to the **bring-up environment** (FreeBSD VM
under macOS HVF, host endpoint running on macOS). Native HW (D5+)
skips all of this — the guest-side client talks directly to the kmod.

### 6.1. Wire choice: ivshmem

We already have an ivshmem channel between QEMU and the FreeBSD guest
for the doorbell + shared-page notification path. Aqueduct-gpu reuses
it. Decision rationale: keeps QEMU-side new code minimal; the hot
path is well-understood; doesn't depend on MSI-X (which we already
poll for via `FRESCO_POLL_HZ`).

```
ivshmem layout (bring-up):
  [0x00000 .. 0x01000]   handshake + capability table
  [0x01000 .. 0x10000]   command ring (guest → host)
  [0x10000 .. 0x20000]   reply ring (host → guest)
  [0x20000 .. 0x30000]   fence-signal ring (host → guest)
  [0x30000 .. end]       reserved for region-table updates
```

Memory regions themselves are NOT in ivshmem — they're separate SHM-fd
mappings registered with QEMU one-by-one, exposed to the guest through
the atrium-gpu kmod's existing cdev mmap path.

If we hit multi-consumer contention or need richer per-connection
isolation, migrate to a dedicated `atrium-gpu-shmem` virtio device.
That's a v2 question.

### 6.2. Host endpoint structure

```
aqueduct-gpu-host-daemon (macOS)
  ├─ accept() on /tmp/aqueduct-gpu.sock
  ├─ per-connection:
  │    ├─ aqueduct envelope decoder (reused from aqueduct crate)
  │    ├─ resource table (id → MoltenVK / native objects)
  │    ├─ shader cache (CAS hash → MTLLibrary / VkShaderModule)
  │    ├─ pipeline cache (state hash → MTLRenderPipelineState / VkPipeline)
  │    └─ frame dispatcher
  ├─ shmem region allocator (shm_open + touch + mlock)
  └─ MoltenVK / native-Vulkan backend
```

Per-connection MTLDevice or shared: shared. Resource isolation is
enforced protocol-side via per-connection id namespaces.

---

## 6.5. Backend tiers and power policy

Aqueduct-gpu's `Backend` trait (`aqueduct-gpu-host/src/backend.rs`)
allows multiple host-side implementations of the same wire protocol.
Three tiers exist, picked by the daemon's `--backend` flag (or by
the kmod in D5+ native HW). The wire protocol is identical across
all of them; clients (frescod's renderer, atrium-vk-icd) never know
or care which backend they're running on.

### 6.5.1. The three tiers

**Tier-1: SoftwareBackend (tiny-skia).** Pure-CPU rasterisation of
Atrium-native bundle operations (atrium-core: rect, path, texture;
atrium-text: glyph_run). Hand-implemented equivalents per bundle op
— **does not interpret SPIR-V/NIR**. Restricted to bundles whose
semantics we know at Atrium build time. Used for:

- Power-policy-driven compositor rendering on battery: the GPU
  stays asleep during static / low-activity UI, dropping system
  power draw by ~5–15W on a typical laptop.
- Devices without a usable GPU at all (basic VMs, embedded HMI,
  certain industrial boxes, headless servers running a graphics
  service for remote display).
- CI / regression test environments where shipping a GPU stack
  would be infrastructure overhead.
- Compositor bring-up on platforms not yet covered by an MoltenVK
  / native-Vulkan / atrium-gpu backend.

Implementation cost: ~2–3 weeks for full atrium-core + atrium-text
coverage. Reuses the tiny-skia integration Pergola already has
(`pergola/`). Pure Atrium code, permissively licensed, no Mesa
runtime dependency. Tracks tier-1 capability bits in the handshake
response so clients can detect what's supported (initially:
`CAPS_TIER1_RECT`, `CAPS_TIER1_TEXT`, `CAPS_TIER1_TEXTURE` — bits
in `HandshakeResponse::caps`).

**Tier-2: General SW Vulkan (deferred).** A SPIR-V-conformant CPU
Vulkan implementation — what llvmpipe / lavapipe provides today on
Linux. Required for:

- Third-party bundles with custom shaders that we don't have
  hand-coded equivalents for.
- Vulkan games on GPU-less systems (a vanishingly small audience).
- Full CTS-equivalent SW conformance testing.

Phase 1 **does not** ship Tier-2. The only realistic open-source
implementation is Mesa's gallium + llvmpipe stack; vendoring it at
runtime would break the "Mesa only at build time" rule we
committed to in §3. The cost-benefit is poor for the bring-up
phase: maintenance overhead of carrying llvmpipe + gallium vs the
small population of users who need pure-CPU general Vulkan on
Atrium. Revisit when concrete demand surfaces.

Until then, third-party bundles with custom shaders on a SW-only
system get `OP_GPU_VALIDATION_ERR` at bundle-load time; Vulkan
games get `VK_ERROR_FEATURE_NOT_PRESENT` at instance creation.
This is an explicit scope choice, documented in the engine
landscape (§10): "if you need Tier-2, run on a system with a GPU."

**Tier-3: Hardware-accelerated.** The expected default —
MoltenVkBackend on macOS bring-up, native Vulkan on Linux dev
hosts, AtriumGpuBackend on D5+ native HW. Full pipeline
materialisation, real GPU execution. Phase 1.3b lands MoltenVK;
Phase 3 lands the native HW path.

### 6.5.2. Power policy framework

Tier selection is policy-driven, not capability-driven. A system
with a discrete GPU still chooses tier-1 for static UI on battery
to conserve power. The flow:

```
Power policy daemon (atrium-power-policy, future)
   │  publishes current policy: {AC|Battery|LowPower|Performance}
   ▼
Backend selector (in aqueduct-gpu-host or atrium-gpu kmod)
   │  picks Backend based on (available HW, policy, workload hints)
   ▼
Active Backend serves frame submissions
   │  switches mid-session if policy or workload changes
```

Backend switching mid-session is the architecturally interesting
part: in-flight resources need to drain on the outgoing backend
before the incoming backend can pick up. The `Backend` trait will
grow a `quiesce()` method in a future Phase, and aqueduct-gpu will
surface a `BackendSwitched` event (analogous to `DeviceLost` but
recoverable) for clients to recreate resources on the new backend.

For **Phase 1 the policy is boot-time only**: the `--backend` CLI
flag (or kmod boot-time tunable in D5+) picks one backend per
daemon lifetime. Live switching deferred to Phase 2 when there's a
concrete app whose performance benefits from it.

### 6.5.3. Backend selection at daemon startup

```
aqueduct-gpu-host --backend stub | software | moltenvk
                  [--socket /tmp/aqueduct-gpu.sock]

stub      → StubBackend: protocol-correct, no GPU work; used for
            wire-path tests and dev iteration without the real GPU.
software  → SoftwareBackend: tiny-skia rasterisation for known
            Atrium-native bundles; refuses unknown shaders.
moltenvk  → MoltenVkBackend: ash::Entry::load → MoltenVK ICD →
            real Metal GPU work. The default on macOS bring-up.
```

On D5+, the daemon goes away entirely; backend selection moves
into the atrium-gpu kmod's boot-time configuration. The wire
protocol stays the same.

### 6.5.4. Demonstration angle

The tier-1 SW renderer is itself a demonstrable architectural
choice (per `docs/ARCHITECTURE.md` § "Atrium as technology
demonstrator"): *"Atrium's compositor renders at full visual
quality on a low-power CPU; the GPU stays asleep until you need
it."* On a battery-powered laptop, idle-desktop power draw with
tier-1 should be measurably lower than the equivalent on
Linux+Wayland-mutter (which currently has no analogous
GPU-bypass-for-static-UI mode). That delta is a reproducible
benchmark + a 30-second video — both are demo-driven phase-exit
deliverables.

### 6.5.5. Scanout buffering & vsync — current state and deferred work

**Current state (frescod-aqueduct, single-buffered, wall-clock paced):**

- One `atrium-gpu` scanout BO. Renders go directly into it; then
  `Display::page_flip` (which today does `virtio-gpu
  RESOURCE_FLUSH` on the BO's resource_id, signalling the host to
  redraw).
- Pacing is `thread::sleep` against the connector's reported
  `mode.refresh_mhz`, not phase-locked to actual panel refresh.
- Tear-prone in theory (QEMU's host display backend can poll the
  BO mid-update); visually clean in practice on virtio-gpu under
  HVF because the host backend is asynchronous and forgiving.

**Three improvements bundled together** — all gated on kmod work
(D0 step 3.5 / Phase 1.5b in the plan; the kmod's current
`atrium_display_page_flip` only flushes, doesn't rebind scanout
or signal vblanks):

#### 6.5.5.a. Multi-buffer scanout (double / triple)

Kmod `IOC_DISPLAY_PAGE_FLIP` must issue `SET_SCANOUT(scanout,
new_resource_id)` when the supplied BO differs from the one
currently bound to the connector, then `RESOURCE_FLUSH`. Without
this rebinding step, flipping to any BO other than the `set_mode`
BO results in the connector continuing to scan out the original
BO (visually: black, or "frozen on first-frame contents").
Implementation note: ~20 lines of C in
`atrium-kmod/atrium_virtio_gpu.c`'s `atrium_display_page_flip`.

Once it lands, frescod-aqueduct switches to a triple-buffer ring
(three BOs, round-robin advance on real flips, keepalive on the
current scanout BO). Eliminates render-into-live-BO tearing
structurally. Client-side cost: ~30 lines in
`frescod/src/bin/frescod_aqueduct.rs`.

#### 6.5.5.b. Vblank events for phase-locked rendering

Kmod gains an IRQ handler that posts vblank events to subscribers
(new ioctl `IOC_DISPLAY_SUBSCRIBE_VBLANK` returning an event fd).
`atrium-gpu-rs` wraps the fd as a `kqueue`-able source per the
project's kqueue posture (`feedback_kqueue_native.md`).

frescod-aqueduct's frame loop becomes event-driven: kqueue waits
on `(vblank_fd, socket_server_reader, input_reader)`. On vblank:
check per-window dirty, render dirty windows, page_flip. Real
vsync, no wall-clock drift, no wasted frames.

#### 6.5.5.c. Queued (vsync-aligned) page-flip semantics

Kmod `IOC_DISPLAY_PAGE_FLIP` gains a "queue for next vblank" flag
that complements current immediate-mode. With the triple-buffer
ring from 6.5.5.a and vblank events from 6.5.5.b, the slots
become proper front / queued / back: client renders into back,
queues it via page_flip, kmod swaps at next vblank, posts a
vblank event, client begins next render. On a VRR panel the
kmod's vblank cadence drops naturally to scene-change rate
(matches §6.5.2's power-policy goal).

#### Architectural staging today

The per-window `window_surfaces` ring + the `any_dirty` /
`layout_changed` outer-skip path are staged so the three changes
above land as localised swap-ins, not redesigns. Total expected
diff when the kmod work is in:

- `atrium-kmod/atrium_virtio_gpu.c`: ~80 lines (page_flip
  rebinding + vblank IRQ + ioctl)
- `atrium-gpu-rs`: ~50 lines (wrapper for VblankEvents fd, flag
  for queued page_flip)
- `frescod/src/bin/frescod_aqueduct.rs`: ~80 lines (BO ring,
  kqueue main loop, queued-flip semantics)

---

## 7. API surfaces and composition strategies

### 7.1. The Vulkan ICD path (atrium-vk-icd)

`atrium-vk-icd` is a Vulkan ICD implemented in Rust that translates
Vulkan API calls into aqueduct-gpu protocol calls. It is not a generic
Vulkan driver — it targets aqueduct-gpu specifically. There is no
Vulkan loader, no WSI plugin, no driver-discovery dance: the ICD is
what `libvulkan.so.1` resolves to on Atrium.

Key API translations:

```
vkCreateInstance
   → open aqueduct connection, send OP_GPU_HANDSHAKE,
     decode device caps + format-rules, return VkInstance handle

vkAllocateMemory(size, ...)
   → ICD assigns local memory_id, sends OP_GPU_MEMORY_CREATE
     fire-and-forget. atrium-gpu kmod IOC_GPU_IMPORT_REGION(token)
     then mmap(). VkDeviceMemory wraps the mmap pointer + region_id.

vkGetImageMemoryRequirements / vkGetBufferMemoryRequirements
   → computed locally from format-rules table. No wire traffic.

vkCreateBuffer / vkCreateImage / vkCreateSampler
   → ICD assigns local id, sends *_CREATE fire-and-forget.
     (Binding deferred to vkBindBufferMemory / vkBindImageMemory.)

vkCreateShaderModule(spirv)
   → ICD computes SPIR-V hash, consults the game's package metadata
     for a pre-compiled entry at (hash, detected_backend, version).
     OP_GPU_SHADER_RESOLVE { hash, backend }
       expected: immediate hit (atrium-pkg pre-warmed)
       fallback: OP_GPU_SHADER_UPLOAD { ... }  for sideloaded /
                 dev iteration / runtime-generated SPIR-V

vkCreateGraphicsPipelines
   → compute pipeline-state hash from state + shader_ids
     check ICD's local resolve cache first
     OP_GPU_PIPELINE_CREATE if not cached, fire-and-forget

vkBeginCommandBuffer / vkCmd* / vkEndCommandBuffer
   → record into guest-side FrameBuilder arena. NO wire traffic.

vkQueueSubmit(cmds, fence)
   → flatten cmds[] into one frame command stream
     OP_GPU_SUBMIT_FRAME { fence_id, command_buf, timeline }

vkWaitForFences
   → OP_GPU_WAIT_FENCE (returns when host signals)

vkQueuePresentKHR
   → OP_GPU_SHARE_SURFACE returns a share token; game hands the
     token to frescod via fresco-protocol's slot_set_texture
     mechanism. Image stays GPU-resident; only the handle traverses
     the wire.
```

What is *not* in the ICD:
- Multi-queue (single graphics+compute queue initially).
- Sparse resources / mesh shaders / ray tracing (reserved opcodes; add
  when a concrete app needs them).
- VK_KHR_swapchain. Compositor-mediated presentation only.
- `VK_KHR_buffer_device_address` (raw GPU pointers — incompatible with
  the universal sandbox; see §11).
- Validation layers as separate libraries — Mesa's vk_common
  validation builds into the ICD's own dispatch path.
- Khronos CTS conformance certificate. We aim for "popular Vulkan
  workloads run correctly," not the conformance badge.

The ICD reports `VK_API_VERSION_1_3` with a curated extension list.
Extensions are added as concrete apps need them, with a hard "no
extension we can't implement honestly under sandbox semantics" policy
(no `VK_KHR_synchronization2` stub that silently ignores barriers).

### 7.2. Composition: semantic vs surface

Apps emit work in one of two composition modes, independently of API
surface:

**Semantic.** App emits scene-graph commands (rects, paths, glyph runs,
custom bundle ops) through fresco-protocol over aqueduct. frescod's
compositor stores them in its scene graph; per frame it renders the
union of all clients' scenes through its own aqueduct-gpu connection.
Inter-frame deltas are tiny. This is how Atrium UI apps work today;
custom bundle ops extend this to richer per-app rendering.

**Surface.** App renders to its own image (any size, any format)
through aqueduct-gpu. App calls `OP_GPU_SHARE_SURFACE` to hand frescod
a reference to the image. frescod composes the image as a textured
rect in its desktop scene. Image stays GPU-resident; no pixels
traverse the wire.

A single app can use both modes in the same frame: surface composition
for a 3D viewport, semantic composition for UI overlay. The choice is
per-surface, not per-app.

### 7.3. Custom bundle ops (third-party scene-graph extensions)

Atrium-native apps can ship their own bundle (matching the
`atrium-core` / `atrium-text` shape) with custom rendering. frescod
loads the bundle, validates each declared resource through the
universal sandbox primitives (§3, §11), and dispatches the bundle's
work alongside built-ins.

This is the path for apps that want semantic composition's wire
benefits (dedup, delta updates) but need rendering beyond
rects+text+textures. Indie/2D games, engine-shipped Atrium backends
(Bevy, Godot, etc.), specialised visualisation apps all fit here.

**What a bundle ships:**

- AOT-compiled shaders for every supported backend (in
  Tessera, content-addressed by hash; see §4.2)
- A manifest declaring:
  - Pipelines (compute or graphics): shader hashes + state +
    descriptor-set layouts + push-constant ranges
  - Render passes: attachment formats, load/store ops, sample counts
  - Samplers: filter modes, address modes, anisotropy
  - Bundle-local IDs for each declared resource (within the
    bundle's namespace assigned at `OP_GPU_BUNDLE_LOAD`)
- An app-side SDK (Rust crate, ideally) that translates the
  bundle's higher-level operations into standard FOPs

**What a bundle does NOT ship:** new wire opcodes, new frame-ops,
new payload schemas. The wire vocabulary is closed (§3); the bundle
operates entirely through composition of standard FOPs referencing
bundle-shipped resources.

**Concrete example.** A particle-system bundle:

```
particles.bundle/
  manifest.json
  shaders/
    particles_update.spv          → AOT-compiled per backend in Tessera
    particles_render.spv          → AOT-compiled per backend in Tessera
  sdk/
    Cargo.toml                    → Rust crate the app links
    src/lib.rs                    → exposes dispatch_particles(),
                                    render_particles() in app-friendly API
```

Bundle manifest (excerpt):

```
{
  pipelines: [
    { local_id: 0x0001, name: "update", kind: "compute",
      shader: "particles_update.spv (sha256:abc...)",
      descriptors: { set0: [storage_buffer, uniform_buffer] },
      push_constants: { offset: 0, size: 32 } },
    { local_id: 0x0002, name: "render", kind: "graphics",
      vertex_shader:   "particles_render_vs.spv (sha256:def...)",
      fragment_shader: "particles_render_fs.spv (sha256:ghi...)",
      ... }
  ],
  passes: [ ... ],
  samplers: [ ... ]
}
```

Bundle's app-side SDK (excerpt):

```rust
impl ParticleSystem {
  pub fn dispatch_update(&mut self, dt: f32, count: u32) {
    let params = UpdateParams { dt, count };
    self.frame.fop(FopBindPipeline { pipeline_id: self.update_pipeline });
    self.frame.fop(FopBindDescriptors { set: 0,
        buffer_ids: [self.particle_buffer, self.uniforms_buffer] });
    self.frame.fop(FopPushConstants { offset: 0,
        inline_bytes: postcard::to_bytes(&params)? });
    self.frame.fop(FopDispatch { x: count.div_ceil(64), y: 1, z: 1 });
  }
}
```

`self.update_pipeline` is the bundle-namespaced pipeline_id returned
by `OP_GPU_BUNDLE_LOAD` (the high 4 bits are the bundle's namespace
tag, low 28 bits are `0x0000001`). At wire time, the host endpoint
sees a sequence of standard FOPs referencing a pipeline_id it
materialised at bundle load. No custom wire format, no schema
negotiation, no per-frame validation.

**This is exactly how atrium-core and atrium-text already work
today** — they're bundles with manifests + shaders + an app-side
SDK (frescod's rendering crate). The only thing that changes for
third-party bundles is the trust posture: validation runs at install
time (atrium-pkg) and at bundle-load time (frescod), and the
sandbox primitives in §11 apply to every bundle-shipped shader
regardless of source.

---

## 8. Frescod's renderer migration

`fresco-vulkan/src/headless.rs` currently uses Mesa-Vulkan via venus
to render frescod's scene. Migration path:

1. Replace `HeadlessRenderer` internals with an `AqueductGpuRenderer`
   that resolves bundle-op pipelines by content hash and submits one
   frame per render call.
2. Bundle format extension: alongside the NIR / SPIR-V shader sources,
   each bundle ships **pre-compiled per-backend blobs** for each
   supported host backend. `bundles/<bundle>/build.sh` cross-compiles
   to MoltenVK MTLLibrary for macOS-arm64, atrium-gpu ISA for D5+
   (when that backend exists). Manifest lists blob hashes;
   aqueduct-gpu-host pre-loads them at daemon startup.
3. Per-frame: build one frame command stream, submit, wait on fence,
   read back through a stable mapped memory region. No
   `vkResetCommandBuffer`, no `vkBeginCommandBuffer`, no `vkQueue*` —
   those leave frescod's code entirely. First-frame latency is
   bounded by atomic ops + memcpy.

The compositor protocol (`fresco-protocol`) is unchanged. Atrium UI
clients see no difference. Vestibulum and friends keep working
exactly as before, faster and without the venus failure modes between
frescod and the GPU.

---

## 9. Implementation phases

### Phase 1 — protocol + macOS host endpoint (4–6 weeks)

**Status: Phase 1.1–1.4 landed; 1.3b-cmdbuf, 1.4-frescod-swap, 1.5 pending.**

Deliverables:
- ✅ `aqueduct-gpu` crate: opcodes, payload schemas, encoders/decoders.
- ✅ `aqueduct-gpu-client` crate: guest-side client (`GpuClient`,
  `FrameBuilder`, async event demux).
- ✅ `aqueduct-gpu-host` crate: macOS host endpoint daemon. Per-
  connection [`Session`](aqueduct-gpu-host/src/session.rs) with
  resource tables; pluggable [`Backend`] trait.
  - ✅ `StubBackend` — protocol-correct, no GPU work.
  - ✅ `SoftwareBackend` — tier-1 tiny-skia rasterisation.
    Built-in pipelines: rect, path, textured-rect, glyph_run.
  - ⚠️ `MoltenVkBackend` — **skeleton only**. Loader, instance,
    device, queue selection done; `VkCommandBuffer` recording not
    yet (Phase 1.3b-cmdbuf).
- ✅ `fresco-aqueduct-bridge` crate: pure translator from
  `fresco_protocol::*Params` to FrameOp records. Per-node
  functions: `translate_rect`, `translate_path`,
  `translate_texture`, `translate_glyph_run`.
- ⚠️ Frescod render-loop swap onto bridge — **pending**
  (FreeBSD-only; can't host-test, deferred to VM session).
- ⚠️ `atrium-virtio-gpu` kmod extensions: `IOC_GPU_IMPORT_REGION`,
  `IOC_GPU_LIST_BACKENDS` — **pending** (Phase 1.5).

Validation done so far:
- Tier-1 SW backend produces real pixels through full wire stack
  (10 end-to-end socket tests, e.g. `software_backend_renders_*`,
  `multi_renderpass_frame`).
- `fresco-aqueduct-bridge` end-to-end tests render fresco-shaped
  scenes (rect, path, multi-node) through real Unix socket →
  Session → `SoftwareBackend` → tiny-skia → readback.
- All four Atrium crates (`aqueduct-gpu`, `aqueduct-gpu-client`,
  `fresco-aqueduct-bridge`, `aqueduct-gpu-host`) cross-compile to
  `aarch64-unknown-freebsd` from the macOS host.
- `aqueduct-shader-tool verify-bundle` runs natively inside the
  FreeBSD VM and gives identical verdicts to the macOS host on
  both `bundles/atrium-core` (9 ok / 0 rejected) and
  `bundles/atrium-text` (3 ok / 0 rejected).
- The `examples/demo` (fresco-protocol scene → bridge →
  GpuClient → SoftwareBackend → PNG) runs **inside the FreeBSD
  VM** producing a byte-identical PNG to the macOS-host run.
  This is the minimum-viable Phase 1 demonstration:
  fresco-protocol scene rendered by FreeBSD-native code without
  venus / virglrenderer / virtio-gpu in the chain.
- Bug found-and-fixed during glyph_run e2e: tiny-skia
  `Pattern.transform` is pattern→world, not the inverse. Affected
  both textured-rect and glyph_run; the textured-rect test passed
  initially only because that test used a uniform-colour atlas.
- Bug found-and-fixed during `inspect`-driven review: LoopControl
  bit values were off by one position in the validator + annotate
  (annotate was emitting `IterationMultiple` (0x40) instead of
  `MaxIterations` (0x20)). Tests passed because validator-strict
  accepted either, but driver semantics differed. Now correct.

Exit criterion (unchanged): `vm/v?-aqueduct-gpu-vestibulum.png`
shows the login form, captured 30 seconds after launch with the
frame counter still incrementing.

### Phase 2 — Vulkan ICD + install-time AOT (4–6 weeks)

**Status: validator (2.0–2.2) and shader cache (2.3) landed;
2.4 (spirv-tools), ICD, atrium-pkg integration pending.**

Deliverables:
- ⚠️ `atrium-vk-icd` crate: Vulkan ICD speaking aqueduct-gpu —
  **pending**.
- ✅ Sandbox primitives (`aqueduct_gpu_host::shader_validator`):
  - Phase 2.0: magic / version / size caps, forbidden capability
    list (PhysicalStorageBufferAddresses, RayTracing*, MeshShading*,
    CooperativeMatrix*), forbidden extension list
    (SPV_KHR_physical_storage_buffer, ray/mesh extensions),
    truncation / zero-word-count safety.
  - Phase 2.1: instruction-count / loop-count / function-count
    caps, storage-class denylist (defense-in-depth against BDA).
  - Phase 2.2: every `OpLoopMerge` must carry a bounded-iteration
    annotation (`MinIterations` / `MaxIterations` / etc.); literals
    capped at `MAX_LOOP_ITERATIONS`. Modules without producer-
    supplied iteration bounds are rejected with the diagnostic
    `"rebuild with bounded-loop emission enabled, e.g. glslang -Os"`.
  - 24 unit tests + 1000-byte-sequence fuzz; wired into
    `Session::handle_shader_upload` (SpirV kind only; NIR bypasses).
- ⚠️ Phase 2.4: link `spirv-tools-rs` for the long tail beyond
  Atrium policy — pending.
- ✅ Phase 2.3: warm-path shader cache
  (`aqueduct_gpu_host::shader_cache::ShaderCache`):
  - Disk store keyed by `(hash, backend_vendor, generation,
    compiler_version, kind)`. Atomic write-to-temp+rename.
  - In-memory LRU (bounded, default cap 64) above the disk store.
  - `Listener::with_shader_cache(Arc<ShaderCache>)` builder-style
    wiring. `Session::handle_shader_resolve` consults the cache and
    emits real `Hit { shader_id }` so subsequent draws can
    reference it; `handle_shader_upload` writes post-validation.
  - End-to-end test `shader_cache_warm_path_hit_after_upload`
    proves a fresh connection resolves what a prior connection
    uploaded.
  - **Eventual home: Tessera CAS.** API is shaped so the Tessera-
    backed implementation swaps in mechanically.
- ⚠️ atrium-pkg `shader-precompile` install-hook — **pending**.
- ⚠️ Vulkan triangle / textured-cube demo — **pending**.

Exit criteria (unchanged):
- Vulkan triangle demo + textured cube demo run at ≥30 fps on
  macOS-HVF host, composited by frescod.
- `atrium-pkg install` populates Tessera; `atrium-pkg run` shows
  `OP_GPU_SHADER_RESOLVE` hits with no SHADER_UPLOAD traffic.
- ✅ `docs/spec/atrium-pkg.md` §3.6.5 documents the
  shader-precompile install step.

### Phase 3 — native HW backend (D5+, deferred)

Deliverables:
- atrium-gpu kmod gains direct command-ring submission.
- aqueduct-gpu-host endpoint becomes optional — guest-side
  ICD/renderer talks directly to the kmod with no host daemon hop.
- macOS host endpoint stays as a dev/CI configuration.

This is long-tail D5+ work, out of scope for this doc's near-term plan.

### Phase 4 — Proton/DXVK reachability (deferred)

DXVK is open-source Vulkan code (D3D11/D3D12 → Vulkan translation),
plausibly sandbox-compatible. If a significant fraction of DXVK's
Vulkan output passes our validator, a wide swath of Windows games
becomes reachable on Atrium via Wine + DXVK + atrium-vk-icd. Phase 4
is the assessment + the gap-closing work to make it work.

Not a Phase 1 concern. Worth documenting as a real direction so the
sandbox decisions in Phase 2 are made with this end-state in mind.

---

## 10. Engine landscape

Atrium's addressable game market is *not* AAA games using bespoke
engines. Those engines never shipped on BSD and aren't going to. The
addressable market is **game engines that ship an Atrium-native
renderer** — and via them, every game built on the engine.

| Engine | Source posture | Effort to support Atrium |
|---|---|---|
| Bevy | Open (MIT), Rust, uses wgpu | Trivial. Add an aqueduct-gpu backend to wgpu; Bevy inherits it. Weekend project. |
| Godot | Open (MIT), C++, native Vulkan RHI | Small. Port the existing Vulkan RHI to aqueduct-gpu API. Weeks. Engaged upstream community. |
| O3DE | Open (Apache), C++, RHI abstraction | Small-medium. Write an O3DE RHI for aqueduct-gpu, modelled on its existing Linux RHI. |
| Unreal | Source-available, C++, multi-RHI | Medium downstream. Add an Atrium RHI; ship as Epic launcher patch or third-party fork. Upstream into Epic main is a separate conversation. |
| Unity | Closed, SRP for custom render pipelines | Needs contractual relationship with Unity Inc. or a long-running SRP. Realistic only via company-to-company engagement. |
| id Tech / Frostbite / RE Engine / Decima | Closed, bespoke, AAA-only | Off the table. These never ship on BSD anyway. |

The wgpu path deserves special note: Bevy uses it, but so do many
other Rust graphics apps and toolkits. An aqueduct-gpu backend for
wgpu makes Atrium the natural target for Rust game/graphics
development, with zero per-app porting work.

Proton/DXVK (Phase 4) is the path for Windows-game compatibility
without per-engine adoption. This is how Steam Deck reaches a wide
catalog despite running Linux underneath.

---

## 11. Universal sandbox: no escape valve

Atrium chooses safety over compatibility, the same way we chose
permissive licensing over GPL-encumbered ecosystem reach, native
FreeBSD primitives over linuxkpi compatibility shims, and a custom
kernel GPU ABI over drm-kmod inheritance. These are not accidents of
scope; they are the platform's identity.

There is no unsandboxed GPU path on Atrium. Every shader runs through
static validation + descriptor-bounded isolation + GPU timeout
enforcement, regardless of API surface (Vulkan or direct) or
composition strategy (semantic or surface).

**Two pre-execution gates, one set of primitives.** Validation runs at
two well-defined points, never at runtime on the hot path:

1. **Install-time** (atrium-pkg's shader-precompile hook, §4.2). For
   third-party Vulkan apps shipping SPIR-V: spirv-val, bounded-loop
   analysis, forbidden-feature check, descriptor-layout audit. Failures
   reject the install. For third-party bundles shipping manifests:
   shader interface vs declared descriptor layout / push-constant size,
   manifest schema validation. Same primitives, applied to the bundle's
   declarations.

2. **Bundle-load-time** (`OP_GPU_BUNDLE_LOAD`, §4.3). When a bundle is
   loaded into a running host endpoint, per-resource materialisation
   runs again — the host validates that the shader's reflected interface
   matches the manifest's claimed descriptor layout and that all
   referenced shader CAS hashes are present in Tessera. Mismatch =
   bundle load fails atomically (no partial state).

The runtime wire path (`OP_GPU_SUBMIT_FRAME`) does no shader validation
at all. Every shader on the wire has already passed both gates. Bundle
resources have already been materialised. The frame stream is a closed
vocabulary (§3) referencing pre-validated IDs.

This is what makes fire-and-forget IDs safe: by the time an ID appears
on the wire, the host knows it's well-formed.

### 11.1. Bounded-loop policy: strict literal bounds

Every `OpLoopMerge` in a SPIR-V module must declare a literal
iteration bound via one of the `LoopControl` bits that carries an
operand: `MinIterations`, `MaxIterations`, `IterationMultiple`,
`PeelCount`, `PartialCount`, `DependencyLength`. The validator
caps annotated values at `MAX_LOOP_ITERATIONS` (1 << 24).

What's rejected:
- `LoopControl = 0` (no annotation)
- `LoopControl = Unroll` alone (no literal)
- `LoopControl = DontUnroll` alone (no literal)

**Why strict.** A single, uniform rule — every loop has a literal
bound — is easier to reason about than a tiered "Unroll counts but
only if the backend refuses to fall back" policy. The earlier
permissive design shifted trust onto every backend's codegen path
to enforce the no-runtime-fallback rule; strict mode eliminates
that obligation.

The cost is that bare slangc / glslang / dxc output does NOT pass
the validator directly — producers don't reliably emit literal
bounds. Atrium handles this in the toolchain: see §11.2.

### 11.2. The `annotate` step: closing the producer gap

slangc 2026.8 silently drops `[MaxIters(N)]` source annotations
(verified upstream; tracked as a producer-side bug). glslang and
dxc behave similarly for many patterns. To bridge the gap,
Atrium's shader-tool ships an `annotate` subcommand that walks a
SPIR-V binary and injects `MaxIterations | <literal>` into every
`OpLoopMerge` whose `LoopControl` lacks a literal-bearing bit:

```sh
aqueduct-shader-tool annotate --max-iters 65536 input.spv
```

Properties:
- **Idempotent**: loops that already carry a literal-bearing bit
  (e.g. emitted by a future slangc release that fixes the bug,
  or by an upstream `spirv-opt` pass) are skipped.
- **Preserves `Unroll`**: the bit stays in the mask so drivers
  continue to unroll where appropriate. The literal is just
  additional evidence the validator demands.
- **In-tree library** (`aqueduct_gpu_host::shader_annotate`):
  unit-tested, called by `bundles/*/build.sh` after slangc and
  by atrium-pkg's install hook (§Phase 2.5) before validation.

Atrium pins a real `slangc 2026.8` output corpus
(`tests::SLANGC_UNROLL_LOOP_SPV`) with two CI-locked properties:
1. The raw corpus is rejected by the strict-mode validator.
2. After `annotate → validate`, the corpus passes.

CI catches regressions in either direction — accidental validator
relaxation, or annotate-step breakage.

### 11.3. Descriptor-binding coverage

Every `OpVariable` declared in a resource storage class must carry
**both** `DescriptorSet` and `Binding` decorations. The driver's
descriptor-table lookup is the boundary that contains a shader's
view of host memory; without explicit slot decorations the driver
picks slots non-deterministically, opening a descriptor-confusion
attack vector.

Resource storage classes the validator enforces this on (SPIR-V
§3.7 StorageClass):

| Code | Name | Used for |
|---|---|---|
| `0`  | `UniformConstant` | samplers, images, opaque types |
| `2`  | `Uniform`         | UBOs |
| `10` | `AtomicCounter`   | atomic counter buffers |
| `11` | `Image`           | legacy image load/store |
| `12` | `StorageBuffer`   | SSBOs (SPV 1.3+) |

Classes deliberately NOT enforced (they're not descriptor-backed):
`Function` (7), `PushConstant` (9), `Input` (1), `Output` (3),
`Workgroup` (4), `Private` (6).

The validator is order-tolerant: decorations may appear before or
after the OpVariable they target. The check runs as a post-pass
after the single forward walk has collected both sides.

slangc, glslang, and dxc all emit `DescriptorSet` and `Binding`
for properly-decorated source. Modules failing this check are
typically hand-crafted attack inputs or compilation bugs.

### 11.4. Entry-point / capability consistency

Every `OpEntryPoint` declares an `ExecutionModel` (Vertex, Fragment,
GLCompute, etc.). Each model requires a specific `Capability` to be
declared via `OpCapability` somewhere in the module. The validator
cross-checks these:

| Execution model | Required capability |
|---|---|
| `Vertex` (0)              | `Shader` (1) |
| `TessellationControl` (1) | `Tessellation` (3) |
| `TessellationEvaluation` (2) | `Tessellation` (3) |
| `Geometry` (3)            | `Geometry` (2) |
| `Fragment` (4)            | `Shader` (1) |
| `GLCompute` (5)           | `Shader` (1) |
| `Kernel` (6)              | **forbidden** (OpenCL-style; outside Atrium's Vulkan-shaped sandbox) |

A module with an entry point whose required capability isn't
declared has driver-undefined behaviour: the driver may silently
fall back to a model it does support, or it may dispatch the
intended model on hardware that lacks the feature. Both are
sandbox-escape vectors. The validator rejects upfront.

`Kernel` execution model is rejected outright, regardless of cap
declarations. Atrium's sandbox is Vulkan-shaped — compute work
flows through `GLCompute` entry points and descriptor-bounded
buffers, not the OpenCL kernel/address-space model.

Modules with no `OpEntryPoint` at all are accepted by the
validator (library/fragment shaders linked into a full module
before submission). The cross-check is per-entry-point; absence
of entry points means nothing to check.

### 11.5. Atrium-shipped bundles

`bundles/atrium-core/build.sh` and `bundles/atrium-text/build.sh`
both invoke `slangc -target spirv` then
`aqueduct-shader-tool annotate --max-iters N` (per-kernel `N`
chosen from a worst-case analysis comment in the build script).
The resulting SPIR-V passes the strict-mode validator directly
when the bundle is loaded.

**Vulkan features that the sandbox excludes outright:**

- `VK_KHR_buffer_device_address` — raw GPU pointers; the whole point
  is bypassing descriptor-based bounds checking.
- `VK_EXT_descriptor_buffer` — descriptors as raw memory; bypasses the
  descriptor-set boundary.
- External memory imports from foreign handles whose provenance is
  not on an allowlist (CUDA, V4L2-equivalent buffers from outside
  Atrium).

**Sandbox-friendly equivalents we provide:**

- **Bounded-handle indirection** for pointer-rich data structures.
  Shader thinks it's chasing pointers; each pointer is an opaque
  handle the GPU MMU translates to a pre-allocated bounded region.
  Functionally equivalent for the common use cases (linked lists,
  BVH traversal stacks) at modest cost.
- **Descriptor sets** for resource access. Already sufficient for
  the vast majority of Vulkan usage. The "descriptor buffer" extension
  is a perf optimization, not a capability — apps that need it can
  use descriptor sets at moderate overhead.
- **Atrium-internal interop** for cross-app memory sharing. External
  imports from outside Atrium's trust boundary require an allowlisted
  provider; this matches Portcullis's broader capability model.

**Apps the sandbox excludes:**

- Games using `buffer-device-address` for raw memory chasing (rare
  outside AAA bespoke engines we don't expect to run on Atrium
  anyway).
- ML frameworks that import GPU memory from a closed-source vendor
  blob (e.g., CUDA).
- Anything relying on closed-source GPU drivers that won't expose
  enough of its compilation pipeline for AOT sandboxing.

This is a deliberate scope choice. Atrium is not targeting "every
Vulkan app runs unmodified." It is targeting "every shader on Atrium
runs in a sandbox."

---

## 12. Trust boundaries and threat model

### 12.1. What aqueduct-gpu defends against, and what it does not

The aqueduct-gpu wire protocol enforces shader sandboxing (§11) and
relies on the kernel + Portcullis layer for everything else. This
section makes the trust-boundary decomposition explicit so the design
is not silently assuming protection at the wrong layer.

**Threats inside aqueduct-gpu's scope:**

- *Untrusted shader code*. Apps and bundles ship shaders that may be
  buggy or malicious. Defended by §11's universal sandbox: static
  SPIR-V validation at install/bundle-load, descriptor-bounded resource
  access (hardware MMU enforcement), GPU timeout, no
  `buffer-device-address`, no descriptor-buffer, no foreign-handle
  memory imports.
- *Cross-app resource leakage via the GPU*. Apps must not be able to
  read or write each other's textures, buffers, or descriptor sets via
  the GPU. Defended by per-connection ID namespaces (§3) and per-fd
  resource tables in the kmod (§12.3).
- *Wire-protocol confusion*. Apps must not be able to issue commands
  on another connection's behalf. Defended by aqueduct's per-connection
  envelope routing and Portcullis's peer-uid-to-manifest cross-check.

**Threats outside aqueduct-gpu's scope (deferred to other layers):**

- *Compromised kmod / host endpoint*. The kmod and (on bring-up) the
  `aqueduct-gpu-host` daemon are privileged mediators with full
  visibility into every app's GPU memory by construction. If either is
  compromised, the platform is gone. Mitigation: keep them small,
  audit them, hold to the same scrutiny as any kernel/root daemon.
- *GPU hardware side channels*. Cache contention, memory-bandwidth
  observation, performance counter readbacks across shader executions.
  These are properties of GPU hardware and not addressable at the
  protocol layer; same situation on every commodity GPU.
- *Wire snooping by an attacker who already has root or local kernel
  access*. Encrypting the wire would not defend against this — the
  attacker can read process memory directly.
- *Compromised Atrium image / supply chain*. Atrium's signing + Tessera
  CAS integrity properties live in atrium-pkg's threat model
  (see `docs/spec/atrium-pkg.md`), not here.

### 12.2. Wire encryption: no, with rationale

Aqueduct-gpu does **not** encrypt its wire. The transports it uses
during Phase 1 and the foreseeable arc are all local-machine:
ivshmem (VM ↔ host endpoint on bring-up), Unix domain sockets
(app ↔ aqueduct-gpu-host on the same machine), and direct kmod
ioctls (D5+ native HW). For local transports:

- Authentication is enforced by `SO_PEERCRED` (or kernel-level fd
  ownership for the cdev path) and by Portcullis's manifest gate.
  Encryption adds nothing here; the kernel already attests who is
  who.
- Snooping requires kernel/root access. An attacker with that access
  can read process memory directly; encryption keys live in process
  memory; therefore encryption defends against no threat that the
  kernel doesn't already defend against.

For remote aqueduct-gpu in a future deployment shape (thin-client
compositor displaying an Atrium app rendered on a server; cross-machine
collaboration; etc.), the recommendation matches aqueduct's broader
position (`docs/spec/aqueduct.md` §7): tunnel through an existing
encrypted transport (SSH, WireGuard, QUIC). Do not bake crypto into
aqueduct itself. Same rationale as why HTTP didn't include TLS — the
wrapping layer composes more cleanly than the embedded layer, and
keeps the protocol's hot path narrow.

This is consistent with how D-Bus, Wayland, X11, and every other
local IPC protocol on commodity OSes handle the question.

### 12.3. The aqueduct-gpu path through jails — concrete data flow

For an app inside a Portcullis jail wanting GPU access:

```
1. Jail config (atrium-jaild) — devfs ruleset gates whether the
   jail can see /dev/atrium-gpu0 at all. Jails without GPU access
   never have the cdev in their /dev namespace.

2. Portcullis manifest (atrium.toml) declares which GPU capability
   the app needs. Required keys (proposed, §12.4):
     [capabilities]
     gpu.access  = true   # open /dev/atrium-gpu0, do GPU work
     gpu.scanout = false  # default; only frescod sets true

3. portcullisd cap-mediator cross-checks the connecting peer-uid
   against the manifest before granting GPU access on the aqueduct
   connection. Apps without gpu.access in the manifest cannot
   establish an aqueduct-gpu connection at all.

4. Inside the jail, the app's atrium-vk-icd (or aqueduct-gpu-client)
   opens /dev/atrium-gpu0. The kmod's d_open callback creates a
   fresh per-fd struct atrium_gpu_file via devfs_set_cdevpriv.

5. All resources (BOs, memory regions, pipelines, fences, samplers,
   images, buffers) the app creates live in *that fd's* resource
   table. Resource IDs do not collide across fds; the kmod resolves
   handles via the calling fd's private state, never via a global
   table.

6. The app's mmap(/dev/atrium-gpu0, offset = region_id × PAGE_SIZE)
   maps only that fd's regions. App A's offsets do not address App
   B's memory because the kmod's d_mmap walks only the calling
   fd's BO list.

7. On macOS bring-up: the underlying SHM fd that backs each region
   lives in the aqueduct-gpu-host daemon's address space, not the
   app's. The app never holds a raw SHM fd. The host endpoint is
   the privileged dispatcher (analogous to portcullisd's role for
   capability mediation) and is audited to that standard.

8. On D5+ native HW: the kmod owns the GPU memory directly. The
   app's mmap goes through the kmod's d_mmap, same as bring-up,
   just without the host-endpoint hop.

9. On fd close (app exit or crash): the per-fd resource table is
   torn down by the kmod's d_close callback, releasing all the
   fd's BOs / regions / pipelines. No leakage across the
   close-reopen boundary.
```

Each layer is load-bearing. Removing any one of (1)–(6) weakens the
isolation. The kernel-enforced layers — (1), (3) and (5)–(6) — are
the structural primitives; the manifest layer (2) and the cap-mediator
layer (3) are the policy that runs on top of them.

### 12.4. `gpu.scanout` as a distinct capability

The kmod's `IOC_ALLOC` accepts an `ATRIUM_GPU_BO_SCANOUT` flag that
designates the BO as a scanout buffer (i.e., the buffer the display
hardware reads directly via the kmod's `page_flip` operation). In a
single-app bring-up world (frescod is the only thing using the GPU),
any fd can request SCANOUT. In a multi-app world this becomes a
privilege-escalation vector: a malicious app could allocate a scanout
BO and write into it, observing or interfering with what the user
sees on screen.

The fix is a distinct Portcullis capability `gpu.scanout`, gated at
the kmod's allocation path:

| Capability | Grants | Mechanism |
|---|---|---|
| `gpu.access` | Open `/dev/atrium-gpu0`, allocate ordinary BOs, submit work | devfs rule + portcullisd cap check at open time |
| `gpu.scanout` | Additionally allowed to set `ATRIUM_GPU_BO_SCANOUT` flag on allocations and call `page_flip` | kmod cross-checks the calling fd's Portcullis cap token against the SCANOUT flag at `IOC_ALLOC` and `IOC_PAGE_FLIP`; rejects with `EPERM` if not granted |

Only `frescod` (and, in D5+, designated display-server processes)
gets `gpu.scanout`. The vast majority of apps — vestibulum, Vulkan
games, third-party bundles, Pergola apps — get `gpu.access` only
and physically cannot allocate a scanout BO.

The capability token from portcullisd needs to make it to the kmod
in a kernel-trusted way. Options:

- *(a) Portcullisd-issued descriptor.* portcullisd opens
  `/dev/atrium-gpu0` itself with the appropriate cap flags set on
  the fd via a new `IOC_SET_CAPS` ioctl, then passes the fd to the
  app via SCM_RIGHTS. The app uses that fd; the kmod reads the
  fd's cap flags from `struct atrium_gpu_file`.
- *(b) Cred-tagged jail attribute.* portcullisd sets a jail-level
  attribute (`security.atrium.gpu_caps`) at jail-create time; the
  kmod reads it via `prison_get` when the app opens the cdev.
- *(c) Aqueduct handshake.* aqueduct-gpu-host validates the cap on
  connection establishment via portcullisd's existing peer-uid check
  and stores the cap in its per-connection state; the kmod is
  out-of-band and trusts the host endpoint's enforcement.

For Phase 2 (when sandbox primitives land), option (a) is the
preferred answer — kernel-enforced, doesn't require trusting the
host endpoint for the privileged-cap decision, and reuses the
existing portcullisd/SCM_RIGHTS infrastructure. Open implementation
question; both (b) and (c) are workable fallbacks.

The portcullis manifest spec gains the two capability declarations
above (companion edit to `docs/spec/portcullis.md` §3.2).

---

## 13. Decisions and non-decisions

### Decided

- Frame-batched, not Vulkan-call-batched.
- One sync primitive: fence.
- All shaders AOT-compiled. Always. No JIT on the hot path.
  Atrium-native: build time. Third-party Vulkan: install time via
  atrium-pkg. Runtime is pure hash lookup.
- Universal sandbox; no unsandboxed GPU path.
- Locally-assigned resource IDs; async create + async error reporting.
- Partitioned u32 ID namespace: tag `0x0` for built-ins, `0x1`–`0xE`
  for third-party bundles (assigned at `OP_GPU_BUNDLE_LOAD`), `0xF`
  for ICD-runtime app allocations.
- **Closed wire vocabulary.** Third-party bundles ship shaders +
  manifests, not new wire opcodes. All app expressiveness flows
  through standard FOPs referencing bundle-shipped pipelines.
- Format-rules tables cached in the ICD; many `vkGet*` queries become
  local.
- Memory regions are named, page-aligned, immutable in size.
- ivshmem (bring-up) → atrium-gpu cdev (native HW).
- Mesa kept for NIR/SPIR-V + backend codegen + Vulkan runtime helpers.
  Venus, virglrenderer, virtio-gpu protocol all dropped.
- One mechanism (aqueduct-gpu) with two policy axes: API surface
  (Direct vs Vulkan ICD) and composition strategy (semantic vs surface).
- Compositor presentation only — no Vulkan WSI swapchain.

### Deferred — added when a concrete app needs it

- Mesh/task shaders, ray tracing, sparse residency.
- Multi-queue + cross-queue sync.
- Multi-GPU on the host endpoint.
- Resource lifetime tracking via Vulkan timeline semaphores.
- Cross-connection surface sharing beyond compositor present.

### Not doing

- Vulkan WSI swapchain protocols.
- Window-system Vulkan extensions (X11/Wayland/Xcb surfaces).
- Vulkan loader/ICD-loader shim — libvulkan is a direct alias to
  atrium-vk-icd.
- Validation layers as separate plugins.
- OpenGL/GLES via aqueduct-gpu. Apps port to Vulkan or use a
  translation layer (DXVK, MoltenGL, etc.) that itself targets our
  Vulkan ICD.
- `VK_KHR_buffer_device_address` or any extension that fundamentally
  requires unsandboxed memory access.

---

## 14. Risks

1. **NIR API stability.** Mesa's NIR is internal and changes between
   versions. Atrium-mesa is a fork; we pin the NIR version at fork
   time and rebase deliberately.

2. **MoltenVK feature gaps.** Some Vulkan features aren't fully
   supported by MoltenVK. The aqueduct-gpu-host endpoint reports its
   actual capabilities through `OP_GPU_HANDSHAKE`; the ICD honestly
   reports only what the host can deliver.

3. **Driver-maturity gap vs native Linux Vulkan.** radv has years
   of edge-case handling and vendor-specific micro-optimization. Our
   ICD starts behind. Closes with time + profiling + bug reports.

4. **Vulkan extension coverage.** radv exposes ~80+ extensions; our
   initial set is much smaller. Apps that depend on extensions we
   don't yet support won't run until those extensions land. Curated
   roadmap based on actual app demand, not vendor-spec completeness.

5. **Cold-cache install latency for SPIR-V games.** atrium-pkg's
   shader pre-compile pass can take 5–30 minutes for an AAA game's
   shader set. Mitigation: background Portcullis jail with progress
   UI, Tessera dedup across games (common engine shaders amortize),
   user can launch the game while pre-compile is still running
   (cold-path SHADER_UPLOAD covers the not-yet-compiled stragglers).

6. **Validation tooling gap.** RenderDoc / NSight don't work
   out-of-box with our ICD. Atrium-native debugging would need
   equivalent tooling (probably built into the ICD with hooks into
   spirv-val and SPIRV-Tools). Real work, not yet scoped.

7. **Engine adoption pace.** The engine landscape (§10) assumes
   engines port to Atrium. If none do, we have a beautiful platform
   that runs vestibulum and not much else. Mitigation: start with
   the easiest engines (Bevy via wgpu), prove the pattern, use that
   to make the case to higher-effort engines.

---

## 15. Open questions

- **Command-stream encoding.** Postcard (matching fresco-protocol) vs
  a flat memcpy-friendly format. Postcard wins for consistency but
  costs ~5–15 ns per record. Flat wins for hot paths. Likely
  resolution: postcard for envelope, flat per-record for the
  command-stream buffer.

- **Per-connection MTLDevice vs shared.** Single shared MTLDevice in
  the host daemon; per-connection resource tables enforce isolation.

- **Atrium-gpu HW token format.** Opaque `[u8; 32]` for region
  imports. On macOS bring-up: "shmem fd + length + ivshmem offset."
  On native HW: kmod-issued reference. 32 bytes generous for both.

- **Render-pass dynamic-rendering vs classic.** Vulkan 1.3 supports
  dynamic rendering (no `VkRenderPass`); MoltenVK supports it.
  Likely: emit dynamic rendering on the wire.

- **wgpu backend.** Should aqueduct-gpu ship with an official wgpu
  backend in `external/wgpu/`, or leave that to the wgpu community?
  Probably ship official — it's the lever for the entire Rust
  graphics ecosystem.

---

## 16. Companion code locations

Repository layout as of Phase 2.3 (✅ = landed, ⚠️ = skeleton/pending):

```
aqueduct-gpu/                         ✅ guest + host shared types
                                         (opcodes, payloads, frame
                                          encoder/decoder, ids,
                                          BackendId, GpuVendor)
aqueduct-gpu-client/                  ✅ guest-side client
                                         (GpuClient, FrameBuilder,
                                          IdAllocator, event demux)
aqueduct-gpu-host/                    ✅ macOS daemon
   src/backend.rs                     ✅   Backend trait, StubBackend,
                                            SoftwareBackend
   src/moltenvk.rs                    ⚠️   MoltenVkBackend skeleton
                                            (loader/instance/device;
                                             cmdbuf recording pending)
   src/software/                      ✅   tiny-skia tier-1 rasteriser
       renderer.rs                          (rect/path/textured-rect/
                                             glyph_run)
   src/shader_validator.rs            ✅   SPIR-V validator (Phase 2.0–2.2:
                                            structural + policy + DoS
                                            caps + bounded-loop
                                            annotation enforcement)
   src/shader_cache.rs                ✅   warm-path cache (Phase 2.3:
                                            disk + in-mem LRU; future
                                            Tessera-backed)
   src/session.rs                     ✅   per-connection dispatch
   src/listener.rs                    ✅   accept loop
   src/resources.rs                   ✅   per-session resource table
   src/bin/main.rs                    ✅   daemon entry point
                                          (--backend stub|software|moltenvk)
fresco-aqueduct-bridge/               ✅ fresco-protocol → FrameOp
                                         translator
atrium-vk-icd/                        ⚠️ Vulkan ICD (Phase 2.5+)
external/wgpu/                        ⚠️ wgpu fork w/ aqueduct-gpu
                                         backend (Phase 1.5?)
atrium-kmod/atrium_virtio_gpu.c       ⚠️ IOC_GPU_IMPORT_REGION,
                                         IOC_GPU_LIST_BACKENDS,
                                         IOC_SET_CAPS (Phase 1.5 +
                                         Phase 2 for gpu.scanout
                                         gating)
docs/spec/aqueduct-gpu.md             ✅ this file
docs/spec/atrium-venus.md             ✅ marked superseded, archived
docs/spec/atrium-pkg.md               ✅ extended with shader-
                                         precompile hook
docs/spec/atrium-gpu-abi-v2.md        ⚠️ extended with new ioctls +
                                         cap-gating
docs/spec/portcullis.md               ✅ extended with gpu.access +
                                         gpu.scanout caps
```
