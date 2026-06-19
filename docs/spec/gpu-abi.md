# Atrium GPU ABI Specification

> **SUPERSEDED (2026-06-19).** This v0.1 surface is the bring-up GPU ABI; the
> canonical GPU ABI is now [`atrium-gpu-abi-v2.md`](atrium-gpu-abi-v2.md). See
> [`gpu-abi-reconciliation.md`](gpu-abi-reconciliation.md) for the decision, the
> supersession lineage, and the per-component convergence plan. Kept for history.

**Version:** 0.1.0 (draft)
**Status:** Pre-implementation. The first concrete driver targeting this ABI is `atrium-virtio-gpu` (D0); the first real-hardware driver is `atrium-gpu-amd` (D5.2). Per-vendor architecture is a vendor choice — see [`atrium-gpu-amd-design.md`](atrium-gpu-amd-design.md) for the binding spec for the AMD module and the articulated principles offered as a non-binding reference for any future vendor module.

> See [../NAMING.md](../NAMING.md) for component naming. `ATRIUM_GPU_*` / `ATRIUM_DISPLAY_*` are the kernel-side constants for this boundary; `FRESCO_*` constants belong to the wire protocol layer above (see [wire-format.md](wire-format.md)).

This document specifies the kernel ↔ userspace ABI between native FreeBSD GPU drivers and `fresco-server`. It is the boundary that **replaces FreeBSD's linuxkpi+drm-kmod retrofit** for the GPUs we natively support.

## 1. Scope

This spec covers:

- Two character devices (`/dev/atrium-gpu*` and `/dev/atrium-display0`) and their ioctl/mmap/kqueue surface.
- Buffer-object lifecycle (allocate, mmap, sync, free).
- Command-stream submission and fence/sync semantics.
- Display: connector enumeration, modesetting, page flip, vblank, hardware cursor.
- Capability query and version negotiation.
- Privilege model and jail integration.
- Per-driver implementation pattern.

This spec does **not** cover:

- The GPU hardware command-stream format (per-vendor; the server encodes for the target chip).
- The Fresco wire protocol (separate spec — `wire-format.md`).
- Userspace API surface of `libfresco` / `fresco-rs` (unrelated; that's between apps and `fresco-server`).
- Multi-GPU coordination (deferred; spec accommodates `atrium-gpu0`, `atrium-gpu1`, ... but defines only a single device's behaviour).

## 2. Design principles

1. **Smallest possible surface.** `fresco-server` is the only userspace consumer; we don't need a Vulkan-equivalent ABI. Twelve ioctls cover memory + submit + fence + modeset + capabilities + bind.
2. **Opaque command streams.** The driver doesn't parse commands — it DMAs them to the engine. The server is the only thing that knows the hardware's instruction encoding. This keeps the kernel module small and per-vendor.
3. **kqueue-native.** All asynchronous events (fence retire, vblank, hot-plug, page-flip-completion) surface as kqueue events on the cdev — no Linux-style poll/select hack.
4. **No GEM, no dma-buf, no DRM modesetting framework.** Buffer objects are integers (u32 handles). Modesetting is direct ioctls on `/dev/atrium-display0`. Display sharing across processes is not a feature in v0.1; if needed later, expose via a purpose-specific capability-gated cdev.
5. **Per-vendor implementations, common ABI.** The ioctl numbers and structs are uniform across all GPU drivers. Each driver implements them for its hardware. `fresco-server` has a `GpuBackend` trait per vendor that emits the right command-stream encoding.
6. **Capsicum-clean.** Once `fresco-server` has bound up its cdevs, no further filesystem or syscall paths are required for GPU operations. The server can `cap_enter(2)` after init and continue operating indefinitely.
7. **No linuxkpi.** Pure newbus, kqueue, cdev, bus_dma. No emulation layer.

## 3. Character devices

Two cdevs per system. Future multi-GPU systems can have multiple `gpuN`.

### `/dev/atrium-gpu0` — GPU memory + command submission

Owned by `fresco-server`. Open exclusively by one process at a time (FreeBSD `D_PSEUDO` style). The Fresco-protocol cdev (`/dev/fresco0`, transport for app clients) is a separate device — not to be confused with this one.

### `/dev/atrium-display0` — Display engine: modesetting + scanout

Owned by `fresco-server`. Single open at a time. Drives the actual outputs (HDMI/DP/eDP/etc.). Receives vblank and hot-plug events on its kqueue.

These can be the same driver internally (single softc, two minor numbers) or separate drivers — implementation choice. The user-visible names are uniform.

## 4. Buffer objects

All GPU memory is allocated through buffer objects. A buffer object (BO) is identified by an opaque `u32` handle, valid only within the cdev's open file.

### 4.1 Allocation

```c
struct atrium_gpu_alloc {
    /* Inputs */
    uint64_t size;          /* in bytes; rounded up to page size */
    uint32_t flags;         /* ATRIUM_GPU_BO_* below */
    uint32_t alignment;     /* 0 = default (page) */

    /* Outputs */
    uint32_t handle;        /* opaque, valid until FREE */
    uint32_t _pad0;
    uint64_t mmap_offset;   /* pass to mmap(2) on the cdev to map this BO */
};

#define ATRIUM_GPU_IOC_ALLOC  _IOWR('G', 1, struct atrium_gpu_alloc)
```

Flags:

| Flag | Bit | Meaning |
|---|---|---|
| `ATRIUM_GPU_BO_GPU_VISIBLE` | 0x01 | GPU can read/write. Default for all BOs. |
| `ATRIUM_GPU_BO_CPU_VISIBLE` | 0x02 | CPU can mmap. May force a non-tiled / WC layout. |
| `ATRIUM_GPU_BO_COHERENT` | 0x04 | CPU and GPU caches coherent (snoop). Slower but no explicit sync. |
| `ATRIUM_GPU_BO_SCANOUT` | 0x08 | Will be used as a display scanout source. Driver may pick a tiled format compatible with its display engine. |
| `ATRIUM_GPU_BO_COMPUTE_INPUT` | 0x10 | Hint: input buffer for a kernel dispatch. May affect placement. |
| `ATRIUM_GPU_BO_COMPUTE_OUTPUT` | 0x20 | Hint: output buffer. Driver may need to handle GPU-write-then-CPU-read sync. |
| `ATRIUM_GPU_BO_RT_AS` | 0x40 | Acceleration structure for ray-tracing. Driver may use a special heap. |

### 4.2 Mapping

Standard `mmap(2)`:

```c
void *cpu_addr = mmap(fd, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, mmap_offset);
```

`mmap_offset` was returned from `IOC_ALLOC`. It's a synthetic offset the driver internally maps to the BO; not a physical address.

The CPU-visible mapping is uncached or write-combined depending on `ATRIUM_GPU_BO_COHERENT`. If non-coherent, an explicit sync is required (§4.4).

### 4.3 Free

```c
#define ATRIUM_GPU_IOC_FREE   _IOW('G', 2, uint32_t)  /* arg: handle */
```

Frees the BO. Outstanding fences referencing the BO must complete first (driver may return `EBUSY`).

### 4.4 CPU/GPU sync (non-coherent BOs)

For BOs without `ATRIUM_GPU_BO_COHERENT`:

```c
struct atrium_gpu_sync {
    uint32_t handle;
    uint32_t direction;     /* FRESCO_SYNC_TO_CPU | FRESCO_SYNC_TO_GPU | FRESCO_SYNC_BOTH */
    uint64_t offset;        /* range to sync; 0 = whole BO */
    uint64_t size;          /* 0 = to end */
};

#define ATRIUM_GPU_IOC_SYNC   _IOW('G', 3, struct atrium_gpu_sync)
```

Issued before CPU read of GPU output, or after CPU write that the GPU should see.

Coherent BOs don't need this; the driver uses snooped or coherent allocations transparently. Hardware that doesn't support coherent allocation simply rejects `ATRIUM_GPU_BO_COHERENT` at alloc time.

## 5. Command submission and fences

### 5.1 Submit

```c
struct atrium_gpu_submit {
    /* Command stream: bytes that the hardware engine consumes directly.
     * The server pre-encoded these for this specific chip. */
    uint32_t cmd_handle;       /* BO containing commands */
    uint64_t cmd_offset;       /* byte offset within BO */
    uint64_t cmd_size;

    /* BOs referenced by the command stream. The driver pins these
     * for the duration of execution. Optional: drivers with full
     * IOMMU may not need an explicit list. */
    uint32_t bo_count;
    uint64_t bo_handles_ptr;   /* u32[bo_count] */

    /* Wait dependencies: this submission won't start until each of
     * these fences has retired. Fences are opaque u64 — drivers
     * internally encode (engine, counter); clients never decode. */
    uint32_t wait_fence_count;
    uint64_t wait_fences_ptr;  /* u64[wait_fence_count] */

    /* Output: this submission's fence id. Opaque, monotonically
     * increasing within its engine. */
    uint64_t fence_out;

    /* Engine routing. */
    uint32_t engine;           /* FRESCO_ENGINE_* */
    uint32_t flags;            /* FRESCO_SUBMIT_* */
};

/* Standard engine assignments (0..255). */
#define FRESCO_ENGINE_GRAPHICS      0
#define FRESCO_ENGINE_COMPUTE       1
#define FRESCO_ENGINE_COPY          2  /* DMA / blit engine if separate */
#define FRESCO_ENGINE_RT            3  /* dedicated RT cores */
#define FRESCO_ENGINE_VIDEO_DECODE  4
#define FRESCO_ENGINE_VIDEO_ENCODE  5
#define FRESCO_ENGINE_JPEG          6
#define FRESCO_ENGINE_DSC           7  /* display-stream compression */
/* 8..255 reserved for future standard engines. */
#define FRESCO_ENGINE_VENDOR_BASE   256
/* 256+ vendor-specific. */

#define FRESCO_SUBMIT_HIGH_PRIORITY 0x01
/* 0x02..0x80 reserved for future QoS classes (background, video-decode-paced, etc.).
 * v0.1 only honors HIGH_PRIORITY; other bits MUST be zero. */

#define ATRIUM_GPU_IOC_SUBMIT _IOWR('G', 4, struct atrium_gpu_submit)
```

The driver appends the command stream to its hardware command queue, retires the fence when the engine finishes (via interrupt → kqueue notify), and returns the `fence_out` value immediately.

**Fences are opaque.** Internally the driver tracks per-engine monotonic counters; the u64 it returns may encode `(engine, counter)` however convenient. Clients pass fence values back through `IOC_FENCE_WAIT` and `IOC_FENCE_QUERY` without decoding. This lets drivers evolve their counter scheme without ABI change.

### 5.2 Fence wait

```c
struct atrium_gpu_fence_wait {
    uint64_t fence;
    int64_t  timeout_ns;       /* -1 = forever, 0 = poll */
};

#define ATRIUM_GPU_IOC_FENCE_WAIT _IOW('G', 5, struct atrium_gpu_fence_wait)
```

Returns 0 on retire, `ETIMEDOUT` on timeout, `EIO` if the fence is **lost** (driver detected GPU hang and reset; the dependent work never completed). For event-driven waits, prefer kqueue.

### 5.3 Fence query (poll without blocking)

```c
struct atrium_gpu_fence_query {
    uint32_t engine;
    uint32_t _pad0;
    uint64_t latest_retired;   /* fence with highest retired counter on this engine */
};

#define ATRIUM_GPU_IOC_FENCE_QUERY _IOWR('G', 6, struct atrium_gpu_fence_query)
```

Used for polling the latest retired fence per engine. Server-side `WaitFence` abstracts this behind kqueue: register `EVFILT_READ` on `/dev/atrium-gpu0`; on every wakeup, query each engine's `latest_retired` and check whether outstanding fences have passed.

### 5.4 GPU-hang recovery (reserved)

In v0.1 a hung GPU is detected by driver-level timeout (e.g. 2 s on no progress). The driver may reset the engine; in-flight fences become "lost". `IOC_FENCE_WAIT` on a lost fence returns `EIO`. Future v0.2 will:

- Specify a `FENCE_LOST` event type on the kqueue (delivered as a status payload alongside fence retirement).
- Define recovery semantics for `fresco-server` (re-submit, replay, or panic-and-restart).
- Make timeout configurable per `IOC_SUBMIT`.

For v0.1, `EIO` is enough.

## 6. Display

### 6.1 Bind GPU (capability authentication)

The display cdev is dormant until the controlling process binds a GPU cdev fd to it. This authenticates "the same context owns both" without implicit `td_proc` checks, and supports future fd-passing patterns under capsicum.

```c
struct atrium_display_bind_gpu {
    int gpu_fd;          /* an open atrium-gpu cdev fd */
    int _pad0;
};

#define ATRIUM_DISPLAY_IOC_BIND_GPU  _IOW('D', 0, struct atrium_display_bind_gpu)
```

After successful bind, the display cdev resolves BO handles in the same namespace as the bound GPU cdev. Until bound, only `IOC_ENUM_CONNECTORS`, `IOC_MODES`, `IOC_CAPS_DISPLAY` (read-only) work; any ioctl referencing a BO handle returns `EINVAL`.

`IOC_BIND_GPU` is **capsicum-safe**: it accepts an integer fd, uses `fdgetf()` (or equivalent) to look up the kernel `file *`, and binds. No path lookups, no `/dev` traversal. A `cap_enter()`-restricted process can call it on a fd it already holds.

### 6.2 Connector enumeration

```c
struct atrium_display_connector {
    uint32_t id;              /* stable connector id */
    uint16_t type;            /* FRESCO_CONNECTOR_* (HDMI/DP/eDP/DSI/...) */
    uint16_t flags;           /* CONNECTED, INTERNAL, ... */
    uint32_t edid_size;       /* set by driver: number of bytes available */
    uint32_t _pad0;
    uint64_t edid_ptr;        /* user buffer; driver writes EDID here. NULL = skip. */
};

struct atrium_display_enum {
    uint32_t count_in;        /* size of the user array */
    uint32_t count_out;       /* actual number of connectors */
    uint64_t connectors_ptr;  /* atrium_display_connector[count_in] */
};

#define ATRIUM_DISPLAY_IOC_ENUM_CONNECTORS _IOWR('D', 1, struct atrium_display_enum)
```

Two-call pattern (count_in=0 first to get size, then allocate, then call again).

**EDID is handed back as raw bytes.** The driver reads EDID over the connector's I²C/AUX channel, caches it, and copies it into the user buffer. **Userspace parses.** This is intentional: EDID has a long tail of extension blocks (CTA-861, DisplayID 2.0, HDR static metadata, FreeSync ranges) that evolve faster than kernel update cadence. Putting parsing in `fresco-server` lets a `pkg upgrade` ship new EDID handling without rebuilding the kernel module — the long-running pain point of Linux DRM.

### 6.3 Mode list

```c
struct atrium_display_mode {
    /* Resolution */
    uint32_t width;
    uint32_t height;

    /* Pixel clock (kHz) and refresh rate (×1000, e.g. 60000 = 60.000 Hz). */
    uint32_t pixel_clock_khz;
    uint32_t refresh_mhz;

    /* Horizontal timings (pixels). */
    uint16_t h_sync_start;
    uint16_t h_sync_end;
    uint16_t h_total;
    uint16_t h_skew;

    /* Vertical timings (lines). */
    uint16_t v_sync_start;
    uint16_t v_sync_end;
    uint16_t v_total;
    uint16_t v_scan;

    /* Mode flags. */
    uint16_t flags;
    uint16_t _pad0;

    /* Reserved for color depth, bpp, HDR metadata, etc. v0.1 = zero. */
    uint64_t _reserved[2];
};

#define FRESCO_MODE_FLAG_HSYNC_POS    0x0001
#define FRESCO_MODE_FLAG_HSYNC_NEG    0x0002
#define FRESCO_MODE_FLAG_VSYNC_POS    0x0004
#define FRESCO_MODE_FLAG_VSYNC_NEG    0x0008
#define FRESCO_MODE_FLAG_INTERLACED   0x0010
#define FRESCO_MODE_FLAG_DBL_SCAN     0x0020
#define FRESCO_MODE_FLAG_PREFERRED    0x0040  /* monitor's native */

struct atrium_display_modes_query {
    uint32_t connector_id;
    uint32_t count_in;
    uint32_t count_out;
    uint32_t _pad0;
    uint64_t modes_ptr;       /* atrium_display_mode[count_in] */
};

#define ATRIUM_DISPLAY_IOC_MODES _IOWR('D', 2, struct atrium_display_modes_query)
```

### 6.4 Set mode

```c
struct atrium_display_set_mode {
    uint32_t connector_id;
    uint32_t scanout_handle;  /* BO handle from the bound GPU cdev */
    struct atrium_display_mode mode;
};

#define ATRIUM_DISPLAY_IOC_SET_MODE _IOW('D', 3, struct atrium_display_set_mode)
```

`scanout_handle` is resolved against the BO namespace established by `IOC_BIND_GPU`.

### 6.5 Page flip

```c
struct atrium_display_page_flip {
    uint32_t connector_id;
    uint32_t scanout_handle;
    uint64_t wait_fence;       /* 0 = no wait; otherwise wait for this GPU fence before scanout */
    uint64_t flip_id;          /* user cookie; echoed in the FLIP_COMPLETE event */
    uint32_t flags;            /* FRESCO_PAGE_FLIP_* */
    uint32_t _pad0;
};

#define FRESCO_PAGE_FLIP_INCLUDE_CURSOR  0x02
/* Reserved for v0.2: when set, driver composites the hardware cursor into
 * the scanout BO before display. Useful for screen-capture / record paths
 * that need cursor in the captured frame. v0.1 implementations MUST
 * reject this flag with EINVAL. */

#define ATRIUM_DISPLAY_IOC_PAGE_FLIP _IOW('D', 4, struct atrium_display_page_flip)
```

The flip is queued for the next vblank. Returns immediately. When the flip executes, the driver KNOTEs the cdev's kqueue with a `FLIP_COMPLETE` event (§6.7).

If `wait_fence` is non-zero, the flip waits until that GPU fence retires before scanning out (avoids tearing when the rendering isn't done yet).

### 6.6 Hardware cursor

```c
struct atrium_display_cursor {
    uint32_t connector_id;
    uint32_t cursor_handle;   /* BO; 0 = hide */
    int32_t  x;
    int32_t  y;
    uint32_t hot_x;
    uint32_t hot_y;
};

#define ATRIUM_DISPLAY_IOC_CURSOR _IOW('D', 5, struct atrium_display_cursor)
```

Updates cursor position and (optionally) image atomically. Position updates run independently of the page-flip cadence — significant for cursor latency on slower frame rates.

### 6.7 Vblank and events

`EVFILT_READ` on `/dev/atrium-display0` fires for any of these events. Userspace `read(fd, &evt, sizeof(evt))` to dequeue:

```c
struct atrium_display_event {
    uint32_t kind;            /* FRESCO_DISP_EVT_* */
    uint32_t connector_id;
    uint64_t flip_id;         /* echoed from page_flip; 0 for non-flip events */
    uint64_t timestamp_ns;    /* CLOCK_MONOTONIC */
};

#define FRESCO_DISP_EVT_VBLANK         1   /* connector entered vblank */
#define FRESCO_DISP_EVT_FLIP_COMPLETE  2   /* a page flip presented */
#define FRESCO_DISP_EVT_HOTPLUG        3   /* connector status changed */
```

Multiple events may be available per kqueue wakeup; userspace drains via repeated read.

## 7. Capability query

```c
struct atrium_gpu_caps {
    uint32_t version_major;     /* this ABI version this driver implements */
    uint32_t version_minor;
    uint32_t vendor_id;         /* PCI vendor (or pseudo for non-PCI) */
    uint32_t device_id;
    char     family[64];        /* e.g. "amd-rdna2", "virtio-gpu", "mali-g720" */

    uint64_t vram_total_bytes;  /* dedicated; 0 if integrated */
    uint64_t system_memory_visible_bytes;

    uint32_t max_texture_2d;
    uint32_t max_texture_3d;
    uint32_t max_buffer_size_log2;
    uint32_t engine_mask;       /* bitmask of FRESCO_ENGINE_* supported (low 32 engines) */

    uint32_t feature_flags;     /* FRESCO_FEAT_* */
    uint32_t _pad0;

    /* reserved[0..1]: per-context resource accounting (driver writes 0 in
     * v0.1, fills in v0.2+).
     *   reserved[0] — bytes of GPU memory currently allocated by THIS
     *                 cdev's open file (per-context VRAM in use).
     *   reserved[1] — count of BOs currently held by THIS cdev's open
     *                 file.
     * Used by future per-jail rctl integration and diagnostic tooling.
     * reserved[2..7] reserved for future per-context counters. */
    uint64_t reserved[8];
};

#define FRESCO_FEAT_COMPUTE       0x0001
#define FRESCO_FEAT_RAYTRACING    0x0002
#define FRESCO_FEAT_MESH_SHADERS  0x0004
#define FRESCO_FEAT_BINDLESS      0x0008
#define FRESCO_FEAT_HW_CURSOR     0x0010

#define ATRIUM_GPU_IOC_CAPS _IOR('G', 7, struct atrium_gpu_caps)
```

Both `(vendor_id, device_id)` and `family[]` are populated. The numeric pair is for code-side switching (per-hardware encoding paths in `fresco-server`); the string is for logs, configuration files, and human-readable diagnostics.

## 8. ioctl number registry (v0.1)

| Number | Cdev | Name | Purpose |
|---|---|---|---|
| `_IOWR('G', 1, ...)` | gpu | `IOC_ALLOC` | Allocate BO |
| `_IOW('G', 2, ...)` | gpu | `IOC_FREE` | Free BO |
| `_IOW('G', 3, ...)` | gpu | `IOC_SYNC` | CPU/GPU coherency sync |
| `_IOWR('G', 4, ...)` | gpu | `IOC_SUBMIT` | Submit command stream → fence |
| `_IOW('G', 5, ...)` | gpu | `IOC_FENCE_WAIT` | Wait on fence (blocking) |
| `_IOWR('G', 6, ...)` | gpu | `IOC_FENCE_QUERY` | Query latest retired fence |
| `_IOR('G', 7, ...)` | gpu | `IOC_CAPS` | Capability query |
| `_IOR('G', 8, ...)` | gpu | reserved — `IOC_AUDIT` | v0.2 — runtime audit log query (returns `ENOSYS` in v0.1) |
| `_IOW('D', 0, ...)` | display | `IOC_BIND_GPU` | Bind a GPU cdev fd as the BO namespace |
| `_IOWR('D', 1, ...)` | display | `IOC_ENUM_CONNECTORS` | Enumerate displays |
| `_IOWR('D', 2, ...)` | display | `IOC_MODES` | Per-connector mode list |
| `_IOW('D', 3, ...)` | display | `IOC_SET_MODE` | Set mode + initial scanout |
| `_IOW('D', 4, ...)` | display | `IOC_PAGE_FLIP` | Queue page flip |
| `_IOW('D', 5, ...)` | display | `IOC_CURSOR` | Update HW cursor |

Reserved ranges:
- `'G'` 9–127: future GPU ops (compute-specific, RT-specific, perf counters, BO export, audit). Open for additive ioctls without ABI break.
- `'G'` 128–255: vendor-private.
- `'D'` 6–127: future display ops (color management, HDR, VRR, brightness, sub-region damage).
- `'D'` 128–255: vendor-private.

## 9. Driver-side implementation pattern

Each per-vendor driver follows this skeleton.

```c
/* fresco-virtio-gpu.c — example */

static struct cdevsw atrium_gpu_cdevsw = {
    .d_version = D_VERSION,
    .d_open    = atrium_gpu_open,
    .d_close   = atrium_gpu_close,
    .d_ioctl   = atrium_gpu_ioctl,
    .d_mmap    = atrium_gpu_mmap,
    .d_kqfilter= atrium_gpu_kqfilter,
    .d_name    = "atrium-gpu",
};

static struct cdevsw atrium_display_cdevsw = { /* analogous */ };

struct atrium_gpu_softc {
    device_t                dev;

    /* PCI / virtio resources. Pure newbus; no linuxkpi. */
    struct resource        *regs_res;
    struct resource        *irq_res;
    void                   *irq_cookie;
    bus_dma_tag_t           dma_tag;

    /* BO manager (per-cdev-open via cdevpriv). */
    struct mtx              bo_lock;
    struct fresco_bo_table  bos;
    uint32_t                next_bo_handle;

    /* Per-engine command rings + fences. */
    struct fresco_engine    engines[N];

    /* kqueue listener list for fence + display events. */
    struct selinfo          sel;
    struct knlist           knl;

    struct cdev            *gpu_cdev;
    struct cdev            *display_cdev;
};
```

Per-driver responsibilities:

- **PCIe attach** (`device_attach`): map BARs, allocate IRQ, set up bus_dma_tag.
- **BO allocation**: integrate with FreeBSD UVM (`vm_object_allocate` + `vm_phys_alloc_contig` for contiguous physical, or scatter-gather via `bus_dmamem_alloc`).
- **Command submission**: write the command stream to the hardware ring; advance ring tail; ring doorbell.
- **Interrupt handler**: identify which engine retired which fence; advance retired-fence high-water-mark; KNOTE listeners.
- **Modesetting**: program display engine registers (CRTC, encoder, PHY); on virtio-gpu, send `VIRTIO_GPU_CMD_SET_SCANOUT`.
- **Vblank**: enable vblank interrupt; on tick, KNOTE display kqueue listeners.
- **GPU-cdev / display-cdev binding**: store the bound `gpu_fd` in the display cdev's per-open state; resolve BO handles against it on every BO-using ioctl.

The `linuxkpi` substitutes don't appear: no `struct device`, no `dma_buf`, no `drm_*`, no `mutex_lock_irqsave`. Just `mtx`, `bus_dma`, `cdev`, `kqueue`.

## 10. Versioning and extensions

Major.minor.patch as in the wire-format spec.

Drivers report via `IOC_CAPS`. Server checks at startup; refuses to run if major doesn't match its expectation.

Vendor-private ioctls in the `'G'`/`'D'` 128–255 range. Not promised to work cross-implementation.

**Capsicum invariant.** Any future ABI extension MUST preserve the property that all GPU operations are ioctls / mmap / read / kqueue on already-open fds. No new path opens, no `/proc`-style traversal, no `sysctlbyname()` requirement. This keeps `fresco-server` deployable in `cap_enter()`-mode indefinitely.

## 11. Threading and concurrency

- Cdev open is exclusive per-cdev (one open at a time).
- All ioctls are reentrant (multiple threads in one process can issue concurrently).
- Driver internally serializes hardware access (mtx) but allows parallel CPU work (BO bookkeeping, fence query).
- Fences are monotonically increasing per engine; server can rely on `latest_retired_engine_X >= my_fence` ⇒ done.
- kqueue notifications are level-triggered (KNOTE while pending).

## 12. Privilege model and jail integration

The GPU cdevs are part of the Trusted Computing Base, **not exposed to jailed apps**:

- `/dev/atrium-gpu0` — owner `_atrium`, group `_atrium`, mode `0600`.
- `/dev/atrium-display0` — owner `_atrium`, group `_atrium`, mode `0600`.

`fresco-server` runs as user `_atrium`. No root needed for normal operation (root only during initial hardware bring-up, dropped after).

Jails hosting apps **DO NOT** include these cdevs in their `devfs.rules`. Apps reach the GPU only indirectly, through the Fresco wire protocol (`/dev/fresco0`, the transport cdev), which IS jail-exposed and which the kernel module per-slot-isolates.

This means: an app that escapes its sandboxed renderer cannot directly issue `IOC_SUBMIT`, allocate GPU memory, modeset the display, or read scanout buffers. All GPU operations are gated through `fresco-server`, which enforces protocol-level capability checks (window ownership, CAS hash validation, opcode allowlist). The GPU ABI itself never sees a jailed process.

For diagnostics, a separate read-only tool (`atrium-gpu-info`) may open `/dev/atrium-gpu0` for `IOC_CAPS` only — gated by group membership, not by jail visibility.

### 12.1 Capsicum compatibility

After bind-up (open the cdevs, `IOC_BIND_GPU`, `IOC_CAPS`, `IOC_ENUM_CONNECTORS`, `IOC_SET_MODE`), `fresco-server`'s interaction with the GPU subsystem is exclusively via ioctls and mmap on already-open file descriptors. No new path opens, no Linux-style sysfs traversal, no `/proc`.

This means `fresco-server` can call `cap_enter(2)` immediately after init and continue running indefinitely. All GPU operations remain valid; the kernel-side check is "do you hold this fd?" which capsicum preserves. Recommended hardening: drop into capsicum mode after init, load fonts/icons via `SCM_RIGHTS`-passed fds from a parent supervisor that holds those capabilities.

Future ABI extensions preserve this property (§10).

## 13. Future: capability-gated alternative consumers

The current ABI assumes `fresco-server` is the sole consumer. Future spec versions may add additional consumers under the platform's capability model:

- **`/dev/atrium-display-tap0`** — read-only display capture. For screen-record, screenshot, screensharing. Granted to apps holding the `capture` capability via `SCM_RIGHTS` fd-passing from a supervisor. Provides scanout-content reads + cursor position events; cannot modeset.
- **`/dev/atrium-gpu-compute0`** — per-jail compute access. For trusted apps needing direct GPU compute (CAD, scientific computing, ML inference). Granted via the `gpu-compute` capability. Provides a sandboxed BO namespace and the compute / copy engines; no display access, no scanout BO type.

These are explicitly **NOT in v0.1**. Reserved here so future implementations have a clear naming and authority pattern. The fundamental rule: any capability-gated GPU access uses a purpose-specific cdev with a restricted ioctl set, **never** a relaxation of `/dev/atrium-gpu0` or `/dev/atrium-display0`.

## 14. Multi-GPU (deferred)

When multiple GPUs are present:

- `/dev/atrium-gpu0`, `/dev/atrium-gpu1`, ... per device.
- A shared `/dev/atrium-display0` if the system has unified output (e.g. Optimus-style hybrid graphics).
- `fresco-server` opens all and picks one for primary based on `IOC_CAPS` heuristics + user config.

Cross-GPU BOs (peer-to-peer) are explicitly out of scope for v0.1. If needed, a copy through host memory.

## 15. Error returns

Standard `errno` values:

| `errno` | Meaning |
|---|---|
| `EINVAL` | Bad argument (missing field, invalid handle, unknown flag). |
| `ENOSPC` | Out of GPU memory. |
| `ETIMEDOUT` | Fence wait timed out. |
| `EBUSY` | Resource is in use (e.g. BO with outstanding fence on FREE). |
| `EOPNOTSUPP` | Driver doesn't support this op (e.g. compute on a display-only chip; reserved flag set). |
| `ENXIO` | Hardware error (driver detected an unrecoverable GPU fault). |
| `EIO` | Fence is **lost** — work was in flight when GPU was reset; results are invalid. |
| `EFAULT` | Bad userspace pointer. |
| `EPERM` | Capability denied (e.g. `IOC_BIND_GPU` on a non-GPU fd). |
| `ENOSYS` | Reserved-but-not-yet-implemented ioctl (e.g. `IOC_AUDIT` in v0.1). |

## 16. Worked example — virtio-gpu in this ABI

Concrete sketch of how `fresco-server` brings up a virtio-gpu device.

1. Open `/dev/atrium-gpu0`, `/dev/atrium-display0`.
2. `IOC_BIND_GPU` on the display fd, passing the gpu fd → display authenticates the BO namespace.
3. `IOC_CAPS` on the gpu fd → confirms virtio-gpu, learns VRAM size, engine support.
4. `IOC_ENUM_CONNECTORS` on the display fd → finds one virtual display.
5. `IOC_MODES` for that connector → list of supported modes.
6. `IOC_ALLOC` size = 4 MiB, flags = `BO_SCANOUT | BO_CPU_VISIBLE` → handle X.
7. `mmap` the BO; clear it to 0.
8. `IOC_SET_MODE` with that handle → display lights up.
9. `cap_enter()` — drop into capsicum mode.
10. Per frame:
    a. Render scene into per-window FBOs (virtio-gpu's 2D blit / 3D submit).
    b. Composite into the scanout BO (`IOC_SUBMIT` with cmd stream encoding the blits).
    c. `IOC_PAGE_FLIP` with the scanout handle and a wait fence on the composite submit.
11. On vblank event from kqueue → metrics, frame pacing.

This whole flow is `fresco-server` code, no linuxkpi anywhere, virtio-gpu driver is ~3-5k LoC, no DRM compatibility shims.

## 17. Open questions

Items deliberately not addressed in v0.1:

- **BO export across processes** (analog of dma-buf). Not needed for v0.1 — server is sole consumer. When we add screen capture or virtualization, a separate ioctl returning an exportable fd. Ioctl numbers `'G'` 9+ remain open for this.
- **VRR / FreeSync / variable refresh.** Not in v0.1. Add via display extension (new ioctl) when we have hardware to test against.
- **HDR / wide-gamut / color management.** Not in v0.1. Color management is its own space; reserved fields in `atrium_display_mode._reserved` and `atrium_gpu_caps.reserved[]` accommodate future addition.
- **Power-management hooks.** Suspend/resume across `acpi` notifications. Reserved.
- **PRIME-style hybrid graphics with offload to a discrete GPU.** Out of scope.
- **GPU recovery details.** §5.4 marks it as v0.1-best-effort; v0.2 will specify reset and replay semantics.

## 18. Cross-references

- Architecture overview: [../ARCHITECTURE.md](../ARCHITECTURE.md)
- Wire-format spec: [wire-format.md](wire-format.md) (separate; uses different opcodes)
- Graphics subsystem doc: [../subsystems/graphics.md](../subsystems/graphics.md) (philosophy; this spec is the concrete ABI)
- Sandbox / jails subsystem: [../subsystems/sandbox.md](../subsystems/sandbox.md)
- D0 in the roadmap: [../ROADMAP.md#d0--native-freebsd-kernel-gpu-abi--virtio-gpu-driver](../ROADMAP.md#d0--native-freebsd-kernel-gpu-abi--virtio-gpu-driver)

## 19. Changelog

- **0.1.0** (2026-04-28) — Initial draft. Surface designed for the D0 deliverable: bring up `fresco-virtio-gpu` against this ABI on bare-metal FreeBSD with no linuxkpi. Includes:
  - Twelve ioctls covering memory, submit, fence, modeset, capabilities, GPU-display binding.
  - Full VESA-style mode timings.
  - Opaque u64 fences (driver tracks engine internally).
  - Standard engine registry through engine 7, vendor space at 256+.
  - Privilege model and jail integration explicit (TCB-only cdevs).
  - Capsicum compatibility as a normative invariant.
  - Per-context resource accounting fields reserved (zero in v0.1).
  - Future capability-gated cdevs reserved by name (`atrium-display-tap0`, `atrium-gpu-compute0`).
  - `IOC_AUDIT` slot reserved (`'G'` 8) for v0.2 hardening pass.
