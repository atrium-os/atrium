# Atrium Venus — Paravirtualized Vulkan transport

> **Status.** Draft. Implementation tracking name: V-series milestones (V1 = this doc, V2-V6 = code). First atrium-mesa driver. Targets QEMU + virglrenderer with venus support enabled.
>
> **Companion docs.** Read `atrium-gpu-abi-v2.md` first for the broader cdev/ioctl conventions. This doc covers what venus needs *on top of* that base and where it deliberately diverges from native vendor drivers.
>
> **One-line summary.** Venus is the first Mesa Vulkan ICD ported into atrium-mesa. The kernel surface it requires is a small paravirtualization transport — the kmod ships opaque command streams to a host renderer rather than executing them. Useful as a chassis-validation step for atrium-mesa and as a way to get hardware-accelerated Vulkan in the FreeBSD VM. **Not a template for native vendor drivers** (radv, anv, nvk) — those need a fundamentally larger kernel surface that this driver does not exercise.

---

## 1. Why venus, why now, what it does and does not validate

The roadmap puts atrium-mesa in D5 alongside per-vendor native drivers. Venus is a deliberate down-payment on D5: ship the chassis (fork structure, libdrm-replacement, atrium-gpu cdev convention) before committing to the multi-month work of porting radv. Venus is the right vehicle because:

1. **Smallest possible Mesa driver.** Venus is a Vulkan front-end + a transport — no GPU compiler, no register definitions, no per-IP-block code. Roughly 10× smaller than radv. The exercise is "make Mesa build and link without libdrm" rather than "port a vendor backend."

2. **Validates the chassis without validating the native-driver ABI.** What it exercises:
   - atrium-mesa fork mechanics (build system, license audit, vendored SPIR-V / NIR / vk_common deps)
   - libdrm-removal pattern (replace `xf86drm.h` and `libdrm_virtgpu` calls with our cdev's ioctls)
   - Atrium GPU ABI cdev framing (open, ioctl families, mmap, kqueue notification)
   - BO allocator + fence delivery + kqueue integration on the kernel side
   - Mesa's `vk_common` abstraction layer running against a non-Linux kernel for the first time

   What it does **not** exercise:
   - Command-stream validation in the kernel (venus pushes opaque bytes to the host)
   - Per-engine ring buffer management
   - Heap-aware memory placement (VRAM vs GTT vs invisible)
   - GPU reset / TDR machinery
   - MMIO programming for power/clock gating
   - Hardware fence-to-syncobj mapping

3. **Unblocks downstream work in the meantime.** Today the FreeBSD VM renders through Mesa lavapipe — a CPU rasterizer. Frescod, Pergola development, Servo bring-up at D6 — every visual task is bottlenecked by software Vulkan. Venus puts a real GPU under the same Mesa surface, with the host (QEMU) doing the heavy lifting until our native drivers exist.

The honest framing for posterity: **venus is the first atrium-mesa driver, not the first step of D5's native-driver work.** When AMD lands, the chassis carries over but the kernel surface multiplies in size.

## 2. Architectural placement

```
                                                                 GUEST                 HOST
   ┌──────────────────┐                                                   ┃
   │ Vulkan app       │  vkCmdDraw / vkQueueSubmit / vkCreateImage ...   ┃
   │ (vkcube,         │                                                   ┃
   │  frescod)        │                                                   ┃
   └────────┬─────────┘                                                   ┃
            │ libvulkan.so dispatch                                       ┃
   ┌────────▼──────────────┐                                              ┃
   │ atrium-mesa-venus     │  serialize Vulkan call → vn_command stream  ┃
   │ (libvulkan_venus.so)  │                                              ┃
   └────────┬──────────────┘                                              ┃
            │ atrium-gpu cdev ioctls                                      ┃
   ┌────────▼──────────────┐                                              ┃
   │ atrium-virtio-gpu kmod│  package as VIRTIO_GPU_CMD_SUBMIT_3D        ┃
   │  (this spec extends   │                                              ┃
   │   it)                 │                                              ┃
   └────────┬──────────────┘                                              ┃
            │ virtio-gpu controlq                                         ┃
   ════════════════════════════════════════════════════════════════════════
            │                                                              ┃
   ┌────────▼──────────────┐                                              ┃
   │ QEMU virtio-gpu       │  dequeue, route by context, deliver to     ┃
   │ device                │  virglrenderer                                ┃
   └────────┬──────────────┘                                              ┃
            │                                                              ┃
   ┌────────▼──────────────┐                                              ┃
   │ virglrenderer +       │  parse vn_command stream, replay against    ┃
   │ venus backend         │  host's real Vulkan driver                  ┃
   └────────┬──────────────┘                                              ┃
            │                                                              ┃
   ┌────────▼──────────────┐                                              ┃
   │ host Vulkan ICD       │  AMD radv / Intel anv / Apple MoltenVK      ┃
   │ (whatever the host    │  whichever the host actually has            ┃
   │  has)                 │                                              ┃
   └───────────────────────┘                                              ┃
```

The contract this doc nails down is the boundary at the line. Above the line: how `atrium-mesa-venus` talks to the kmod via cdev. Below the line: what virtio-gpu commands the kmod emits to the host. The host side is upstream's responsibility — we just need a virglrenderer build with venus enabled.

## 3. Kernel ABI extensions

### 3.1 Capability negotiation

The kmod must advertise venus support to userspace so atrium-mesa-venus can bail cleanly on hosts that don't speak it.

New ioctl on `/dev/atrium-gpu0`:

```c
#define ATRIUM_GPU_IOC_CAPSET_QUERY  _IOWR('a', 0x40, struct atrium_capset_query)

struct atrium_capset_query {
    uint32_t capset_id;       /* in: 1=virgl, 2=venus, 3=cross_domain, ... */
    uint32_t capset_version;  /* in: requested version, 0 = latest */
    uint32_t actual_version;  /* out: 0 if capset not advertised by host */
    uint32_t data_size;       /* out: bytes of capset_data the host returned */
    uint64_t data_ptr;        /* in: userspace pointer for capset blob (may be NULL on size query) */
};
```

Implementation: kmod issues `VIRTIO_GPU_CMD_GET_CAPSET_INFO` and `VIRTIO_GPU_CMD_GET_CAPSET` against the host on first call per capset_id, caches the result. atrium-mesa-venus calls this once at driver init with `capset_id=2` (venus) — if `actual_version == 0`, the driver loader returns "no devices" and Mesa falls through to the next ICD (lavapipe).

### 3.2 Context lifecycle

A venus "context" is a one-to-one binding between an open file descriptor on `/dev/atrium-gpu0` and a host renderer context. One process gets one context per open. Multiple opens (e.g. one per Vulkan device) yield independent contexts.

```c
#define ATRIUM_GPU_IOC_CTX_INIT    _IOW('a', 0x41, struct atrium_ctx_init)

struct atrium_ctx_init {
    uint32_t capset_id;       /* in: which renderer context to create */
    uint32_t flags;           /* in: bit 0 = enable_polled_submit (no fence required per submit) */
    char     debug_name[32];  /* in: passed through to host for debugging */
};
```

The kmod issues `VIRTIO_GPU_CMD_CTX_CREATE` with the supplied capset and stores the context_id (chosen by kmod, monotonic per-driver-instance) in the file's per-fd state. Subsequent ioctls on this fd implicitly target this context.

`CTX_CREATE` is **idempotent at the fd level** — calling it twice on the same fd returns `EBUSY`. Close the fd to destroy the context (kmod issues `VIRTIO_GPU_CMD_CTX_DESTROY` on `fdrop`).

Rationale for fd-bound contexts: matches Capsicum's "everything is an fd" model and avoids a separate context-handle namespace that would need its own lifecycle management. One process wanting two contexts opens the cdev twice.

### 3.3 Resource (BO) creation, attached to context

The existing `ATRIUM_GPU_IOC_BO_ALLOC` allocates BOs visible to the kmod's own scanout code path. Venus needs BOs that are **also visible to the host renderer** (so the host can read vertex buffers, write framebuffers, etc.). New flag and ioctl:

```c
#define ATRIUM_GPU_BO_HOST_VISIBLE  (1u << 4)   /* extends existing flag set */

#define ATRIUM_GPU_IOC_RESOURCE_ATTACH  _IOW('a', 0x42, struct atrium_resource_attach)

struct atrium_resource_attach {
    uint32_t bo_handle;       /* in: BO handle from BO_ALLOC */
    uint32_t blob_mem;        /* in: VIRTIO_GPU_BLOB_MEM_HOST3D / GUEST */
    uint32_t blob_flags;      /* in: VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE / SHAREABLE */
    uint64_t blob_id;         /* in: opaque host-side identifier (venus assigns) */
    uint32_t resource_id;     /* out: kmod-allocated resource id */
};
```

The kmod issues `VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB` with the blob_mem/flags/blob_id and walks the BO's pages through `VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING`. Resource id is returned to userspace for inclusion in command streams.

### 3.4 Command submission

The core operation. atrium-mesa-venus serializes Vulkan calls into a `vn` command stream and submits:

```c
#define ATRIUM_GPU_IOC_SUBMIT_3D  _IOW('a', 0x43, struct atrium_submit_3d)

struct atrium_submit_3d {
    uint64_t cmd_ptr;         /* in: userspace pointer to command bytes */
    uint32_t cmd_size;        /* in: bytes */
    uint32_t flags;           /* in: bit 0 = signal_fence */

    uint32_t bo_handle_count; /* in: number of BO handles in bo_handles */
    uint64_t bo_handles_ptr;  /* in: userspace pointer to uint32_t array */
    /* bo_handles is the BO list this submit references (so kmod can
     * keep them resident and account refcounts). Kernel does not parse
     * the command stream to discover them. */

    uint64_t out_fence;       /* out: kernel-allocated fence handle if flags & SIGNAL_FENCE */
};
```

The kmod's responsibilities, in order:

1. Validate `bo_handle_count` against a per-context cap (default 4096; tunable).
2. Pin every referenced BO until the fence signals.
3. Copy `cmd_ptr/cmd_size` into a contiguous DMA-mapped buffer.
4. Allocate a per-context monotonic fence id if `SIGNAL_FENCE`.
5. Issue `VIRTIO_GPU_CMD_SUBMIT_3D` with the context_id and command bytes.
6. Return the fence id (or `0` if no fence requested).

**The kernel does not parse the command stream.** Bytes are opaque from the kmod's perspective. Validation, hang detection, and any vendor-specific protection are the host renderer's concern. This is the key shape difference from a native driver.

### 3.5 Fence wait

```c
#define ATRIUM_GPU_IOC_FENCE_WAIT  _IOW('a', 0x44, struct atrium_fence_wait)

struct atrium_fence_wait {
    uint64_t fence;           /* in: handle from SUBMIT_3D */
    uint64_t timeout_ns;      /* in: ~0 = block forever, 0 = poll */
    uint32_t status;          /* out: 0 = signalled, EBUSY = timed out */
};
```

Plus a non-blocking variant: kqueue integration via `EVFILT_USER` registered against the fence. atrium-mesa-venus uses the kqueue path; the blocking ioctl is a fallback for synchronous code paths.

When the host signals fence completion via `VIRTIO_GPU_RESP_OK_FENCE` on the responseq, the kmod walks the per-context fence map, marks the entry signalled, and triggers any registered kevents.

### 3.6 BO mmap

Existing `ATRIUM_GPU_IOC_BO_MAP` (returns an offset for `mmap(2)`) handles host-visible BOs once they've been attached as resources. No change required — the existing scanout path already has working mmap; we just allow venus-attached BOs to use it.

### 3.7 What we deliberately skip

Things native drivers need that venus does **not**:

- **CS validation hooks.** Native drivers must inspect command streams to enforce that BO references stay within the process's BO list, that ring writes don't escape the per-process ringbuffer, etc. Venus's host renderer does this on its side; our kmod is byte-pump only.
- **Heap-aware allocation.** Venus uses generic blob memory; the host decides VRAM vs GTT placement.
- **Per-engine ring management.** Venus uses one logical queue per context.
- **Hang recovery.** If the host renderer crashes, the kmod's only recourse is to fail outstanding fences with `ESTALE`. Native drivers need TDR machinery to reset specific engines without taking down the whole device.
- **DPM / power management.** Host owns the GPU; we don't touch power state.

These are real D5 work for AMD/Intel/NVIDIA — and they don't get any easier because we built venus first. Venus validates the chassis around them, not the things themselves.

## 4. Userspace: atrium-mesa-venus

### 4.1 Fork scope

Mesa is large. We do not fork all of it. The atrium-mesa repository starts as a sparse subtree of upstream Mesa containing only:

```
src/vulkan/runtime/        # vk_common — Mesa's Vulkan dispatch helpers
src/vulkan/util/           # generic helpers
src/virtio/vulkan/         # the venus driver itself
src/util/                  # NIR-adjacent utilities venus pulls in
src/compiler/spirv/        # SPIR-V parser (venus needs it for nothing today
                           # but vk_common pulls it in)
include/                   # public headers
meson.build + meson_options.txt
```

Everything else (GLX, EGL, GLU, mesa/main, every Gallium driver, every native Vulkan driver) is excluded at fork time. The repo is small (~200 KLoC instead of ~3 MLoC) and reviewable.

License audit at fork time: `LICENSES.md` enumerates every distinct license present. Per `LICENSING-POLICY.md`, only permissive licenses are allowed in the runtime; we drop any GPL/LGPL/CDDL fragment caught in the included subtree.

### 4.2 libdrm replacement

Vanilla venus calls into libdrm:

- `xf86drm.h` — generic DRM ioctl wrapper (`drmIoctl`, `drmCommandWrite`, etc.)
- `libdrm_virtgpu` — virtio-gpu specific (`drm_virtgpu_*` ioctls)

We replace these with a thin adapter at `src/atrium/atrium_gpu.c` that:

- Calls `open("/dev/atrium-gpu0", ...)` instead of `drmOpen`
- Translates each libdrm-virtgpu call to the corresponding ATRIUM_GPU_IOC_* ioctl
- Returns errors in the same shape libdrm did (so venus's own error handling is unchanged)

The shim is **not** a general-purpose libdrm; it covers exactly the surface venus uses. Nothing else (radv, anv) gets to call into it. When AMD ports later, it gets its own much larger atrium-gpu surface — possibly extending the cdev's ioctl set significantly — and a similarly direct adapter.

This is deliberately not "build a libdrm that runs on Atrium." That path leads to maintaining a Linux-DRM-shaped userspace forever.

### 4.3 Driver discovery

Mesa's Vulkan loader walks `VK_DRIVER_FILES` (or `/etc/vulkan/icd.d/*.json`) for ICD JSON manifests pointing at .so files. Atrium ships:

```
/usr/local/share/vulkan/icd.d/atrium_venus_icd.json:
{
    "ICD": {
        "library_path": "/usr/local/lib/atrium-mesa/libvulkan_venus.so",
        "api_version": "1.3.0"
    },
    "file_format_version": "1.0.0"
}
```

Plus the lavapipe ICD already present. Loader tries both at `vkCreateInstance`; venus's `vkEnumeratePhysicalDevices` fails fast (returns 0 devices) on systems without venus capset, letting lavapipe take over for headless / CI use.

frescod prefers venus when both are present (set `VK_ICD_FILENAMES=...atrium_venus_icd.json` or pick by physical-device extension list).

## 5. Implementation phases

V1 (this doc).
V2: host-side QEMU + virglrenderer build with venus enabled. Sanity-check on a Linux guest first to confirm the host path works. **No Atrium code yet.**
V3: kmod — capset query + context lifecycle (3.1, 3.2). Verifiable via a tiny test program that opens the cdev, calls CAPSET_QUERY for venus, calls CTX_INIT, closes.
V4: kmod — resource attach + SUBMIT_3D + fence wait (3.3, 3.4, 3.5). Verifiable by hand-crafting a minimal venus command stream (bytes captured from a known-good Linux run) and round-tripping through SUBMIT_3D + fence.
V5: atrium-mesa fork + venus driver retargeting (4). Verify with `vkcube` (the canonical Vulkan demo).
V6: frescod ICD selection. Confirm the QEMU window updates at host frame rate when frescod uses venus.

Each step is independently verifiable; failures localize.

## 6. Open questions, deferred decisions

- **Cross-domain support** (capset 3, used for things like importing Wayland buffers) — deferred. Venus alone is enough for the chassis-validation goal.
- **Implicit-sync compatibility shim** — venus uses explicit fences exclusively, matching the atrium-gpu-abi-v2 convention. No implicit-sync work.
- **Power management on the host side.** If the host's GPU power-states cause submission stalls, the kmod has no way to know. Venus is a "best-effort" performance layer; users wanting deterministic latency need real native drivers.
- **Multi-process resource sharing.** Venus contexts are per-fd; processes can't share BOs without going through the host. Acceptable for the bring-up; D5 native drivers will handle proper cross-process sharing via the cdev convention.
- **Bare-metal venus.** None. Venus is paravirt-only by definition. The whole point of D5 is to exit venus dependency for hardware Atrium installs.

## 7. Relationship to D5 native drivers

A future D5 spec (`atrium-radv.md` or similar) will land alongside this one. The relationship:

| Concern | Venus (this doc) | radv/anv/nvk (D5) |
|---|---|---|
| Kernel CS validation | None — opaque bytes to host | Required, vendor-specific |
| Memory placement | Generic blob, host decides | Per-heap, kernel + Mesa cooperate |
| Engine queues | One per context | Per-engine ring buffers |
| GPU reset | Stale-fence the context, log | TDR machinery, per-engine reset |
| MMIO programming | None | Init sequence, power, clock, display |
| Lines of kmod code | ~1500-2500 incremental | ~30,000-50,000 per vendor |
| Lines of Mesa code | ~5,000 (just venus) | ~50,000-200,000 per vendor |

Venus exists to validate the **shared 30%** — cdev framing, BO allocator, fence delivery, kqueue integration, atrium-mesa fork structure. The remaining 70% of native-driver work is unchanged by venus.

This separation is worth restating because it's tempting, after venus ships, to assume D5 is "just more of the same." It is not.
