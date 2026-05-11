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
`aqueduct.md` §3). One new opcode class:

```
CLASS_GPU = 0x0030
```

### 4.1. The two-phase shader path

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
| `OP_GPU_FENCE_SIGNALED`  | 0x0301 | S → C     | async event       |
| `OP_GPU_DEVICE_LOST`     | 0x0302 | S → C     | async event       |
| `OP_GPU_VALIDATION_ERR`  | 0x0303 | S → C     | async event       |

The handshake exchanges supported features, format-rules tables, and
backend identification. Most create ops return no response — the
client pre-assigns IDs within its namespace and the host validates as
it processes. Validation failures and resource-creation failures
propagate as async events into the next fence wait, where they surface
to the client as standard Vulkan error returns.

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
`atrium-core` / `atrium-text` shape) with custom scene ops and
AOT-compiled shaders. frescod loads the bundle, validates the shaders
through the universal sandbox primitives (§3), and dispatches custom
ops alongside built-ins.

This is the path for apps that want semantic composition's wire
benefits (dedup, delta updates) but need rendering beyond
rects+text+textures. Indie/2D games, engine-shipped Atrium backends
(Bevy, Godot, etc.), specialized visualization apps all fit here.
The mechanism is the same as how Atrium itself ships `atrium-core`
and `atrium-text`; user-shipped bundles get the same dispatch path
with full sandbox enforcement.

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

Deliverables:
- `aqueduct-gpu` crate: opcodes, payload schemas, encoders/decoders,
  guest-side client (used by frescod first, ICD later).
- `aqueduct-gpu-host` crate: macOS host endpoint daemon. Bound to one
  MoltenVK device. Per-connection resource tables. Frame dispatcher
  → MoltenVK.
- `atrium-virtio-gpu` kmod extensions: `IOC_GPU_IMPORT_REGION`,
  `IOC_GPU_LIST_BACKENDS`, region table, ivshmem command-ring
  servicing.
- Migrate frescod's `HeadlessRenderer` to aqueduct-gpu.
- Validation: vestibulum renders end-to-end on macOS-HVF without
  venus, virglrenderer, or virtio-gpu in the stack. **Multiple frames
  per second**, not one frame total.

Exit criterion: `vm/v?-aqueduct-gpu-vestibulum.png` shows the login
form (heading + fields + button rendered), captured 30 seconds after
launch with the frame counter still incrementing.

### Phase 2 — Vulkan ICD + install-time AOT (4–6 weeks)

Deliverables:
- `atrium-vk-icd` crate: Vulkan ICD speaking aqueduct-gpu.
- Sandbox primitives: SPIR-V validator (bounded loops, no
  buffer-device-address, descriptor-bounded access), per-bundle
  descriptor isolation in the host endpoint, GPU-timeout enforcement
  hooks in the kmod.
- atrium-pkg integration: install-time `shader-precompile` hook
  walking the package's SPIR-V, detecting installed GPU backends,
  cross-compiling via the appropriate Mesa subset, storing results
  in Tessera CAS keyed by (spirv_hash, backend_id, compiler_version),
  recording cache table in package metadata. Runs in a background
  Portcullis jail with progress UI.
- A small Vulkan 3D test (rotating cube, textured) running under
  Atrium and presenting to frescod via surface share.

Exit criteria:
- Vulkan triangle demo + textured cube demo run at ≥30 fps on
  macOS-HVF host, composited by frescod.
- `atrium-pkg install` of a third-party SPIR-V-shipping package
  populates Tessera; subsequent `atrium-pkg run` shows
  `OP_GPU_SHADER_RESOLVE` hits with no SHADER_UPLOAD traffic.
- `docs/spec/atrium-pkg.md` gains a "Shader precompile install hook"
  section.

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

## 12. Decisions and non-decisions

### Decided

- Frame-batched, not Vulkan-call-batched.
- One sync primitive: fence.
- All shaders AOT-compiled. Always. No JIT on the hot path.
  Atrium-native: build time. Third-party Vulkan: install time via
  atrium-pkg. Runtime is pure hash lookup.
- Universal sandbox; no unsandboxed GPU path.
- Locally-assigned resource IDs; async create + async error reporting.
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

## 13. Risks

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

## 14. Open questions

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

## 15. Companion code locations

When implementation lands:

```
crates/aqueduct-gpu/                  guest + host shared types
crates/aqueduct-gpu-client/           guest-side client (frescod, ICD)
crates/aqueduct-gpu-host/             macOS daemon
crates/atrium-vk-icd/                 Vulkan ICD
external/wgpu/                        wgpu fork with aqueduct-gpu backend (Phase 1.5?)
atrium-kmod/atrium_virtio_gpu.c       IOC_GPU_IMPORT_REGION, IOC_GPU_LIST_BACKENDS
docs/spec/aqueduct-gpu.md             this file
docs/spec/atrium-venus.md             marked superseded, archived
docs/spec/atrium-pkg.md               extended with shader-precompile hook
docs/spec/atrium-gpu-abi-v2.md        extended with new ioctls
```
