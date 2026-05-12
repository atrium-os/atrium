/*
 * atrium_gpu.h — Atrium GPU ABI: kernel/userspace boundary.
 *
 * Mirrors docs/spec/gpu-abi.md v0.1.0. This header is consumed by both
 * the kmod (atrium_virtio_gpu.c, future native drivers) and userspace
 * (fresco-server's gpu_backend, diagnostic tools).
 *
 * Two cdevs:
 *   /dev/atrium-gpu0      — buffer objects, command submission, fences
 *   /dev/atrium-display0  — modesetting, page flip, vblank, cursor
 *
 * Wire-protocol constants (FRESCO_*) live in libfresco; the kernel does
 * not see them. Constants here are platform-level (ATRIUM_*) plus a few
 * cross-cutting enums (engine ids, sync directions) that the spec
 * happens to prefix FRESCO_* — kept verbatim from the spec for now.
 */

#ifndef ATRIUM_GPU_H_
#define ATRIUM_GPU_H_

#include <sys/types.h>
#include <sys/ioccom.h>

#define ATRIUM_GPU_ABI_VERSION_MAJOR  0
#define ATRIUM_GPU_ABI_VERSION_MINOR  1

/* ---------- Buffer objects ---------- */

#define ATRIUM_GPU_BO_GPU_VISIBLE     0x01
#define ATRIUM_GPU_BO_CPU_VISIBLE     0x02
#define ATRIUM_GPU_BO_COHERENT        0x04
#define ATRIUM_GPU_BO_SCANOUT         0x08
#define ATRIUM_GPU_BO_COMPUTE_INPUT   0x10
#define ATRIUM_GPU_BO_COMPUTE_OUTPUT  0x20
#define ATRIUM_GPU_BO_RT_AS           0x40

struct atrium_gpu_alloc {
	uint64_t size;
	uint32_t flags;
	uint32_t alignment;
	uint32_t handle;
	uint32_t _pad0;
	uint64_t mmap_offset;
};

#define ATRIUM_GPU_IOC_ALLOC  _IOWR('G', 1, struct atrium_gpu_alloc)
#define ATRIUM_GPU_IOC_FREE   _IOW ('G', 2, uint32_t)

#define FRESCO_SYNC_TO_CPU  0x1
#define FRESCO_SYNC_TO_GPU  0x2
#define FRESCO_SYNC_BOTH    0x3

struct atrium_gpu_sync {
	uint32_t handle;
	uint32_t direction;
	uint64_t offset;
	uint64_t size;
};

#define ATRIUM_GPU_IOC_SYNC   _IOW ('G', 3, struct atrium_gpu_sync)

/* ---------- Command submission and fences ---------- */

#define FRESCO_ENGINE_GRAPHICS      0
#define FRESCO_ENGINE_COMPUTE       1
#define FRESCO_ENGINE_COPY          2
#define FRESCO_ENGINE_RT            3
#define FRESCO_ENGINE_VIDEO_DECODE  4
#define FRESCO_ENGINE_VIDEO_ENCODE  5
#define FRESCO_ENGINE_JPEG          6
#define FRESCO_ENGINE_DSC           7
#define FRESCO_ENGINE_VENDOR_BASE   256

#define FRESCO_SUBMIT_HIGH_PRIORITY 0x01

struct atrium_gpu_submit {
	uint32_t cmd_handle;
	uint64_t cmd_offset;
	uint64_t cmd_size;
	uint32_t bo_count;
	uint64_t bo_handles_ptr;
	uint32_t wait_fence_count;
	uint64_t wait_fences_ptr;
	uint64_t fence_out;
	uint32_t engine;
	uint32_t flags;
};

#define ATRIUM_GPU_IOC_SUBMIT _IOWR('G', 4, struct atrium_gpu_submit)

struct atrium_gpu_fence_wait {
	uint64_t fence;
	int64_t  timeout_ns;
};

#define ATRIUM_GPU_IOC_FENCE_WAIT _IOW ('G', 5, struct atrium_gpu_fence_wait)

struct atrium_gpu_fence_query {
	uint32_t engine;
	uint32_t _pad0;
	uint64_t latest_retired;
};

#define ATRIUM_GPU_IOC_FENCE_QUERY _IOWR('G', 6, struct atrium_gpu_fence_query)

/* ---------- Capability query ---------- */

#define FRESCO_FEAT_COMPUTE       0x0001
#define FRESCO_FEAT_RAYTRACING    0x0002
#define FRESCO_FEAT_MESH_SHADERS  0x0004
#define FRESCO_FEAT_BINDLESS      0x0008
#define FRESCO_FEAT_HW_CURSOR     0x0010

struct atrium_gpu_caps {
	uint32_t version_major;
	uint32_t version_minor;
	uint32_t vendor_id;
	uint32_t device_id;
	char     family[64];

	uint64_t vram_total_bytes;
	uint64_t system_memory_visible_bytes;

	uint32_t max_texture_2d;
	uint32_t max_texture_3d;
	uint32_t max_buffer_size_log2;
	uint32_t engine_mask;

	uint32_t feature_flags;
	uint32_t _pad0;

	uint64_t reserved[8];
};

#define ATRIUM_GPU_IOC_CAPS _IOR ('G', 7, struct atrium_gpu_caps)

/* ---------- Venus / Vulkan paravirt (V3+) ---------------------------- */
/*
 * These ioctls let userspace drive virtio-gpu's context-init machinery,
 * which is what `atrium-mesa-venus` (and any future paravirt-Vulkan ICD)
 * uses to ship Vulkan command streams to a host renderer. See
 * `docs/spec/atrium-venus.md` §3 for the contract.
 *
 * Capsicum shape: every operation here is per-fd. The kernel-side
 * context state lives in the per-open private record (no global handle
 * table), and CTX_INIT is idempotent at the fd level — the context is
 * destroyed implicitly when the fd is closed. There is no escape hatch
 * by which one fd can name another fd's context. A sandboxed process
 * can be granted just CAP_IOCTL with this command list and operate
 * normally.
 */

/* Capset IDs follow virglrenderer's VIRTGPU_DRM_CAPSET_* enum. Kept
 * here verbatim so the kmod doesn't pull in virglrenderer's headers. */
#define ATRIUM_GPU_CAPSET_VIRGL        1
#define ATRIUM_GPU_CAPSET_VIRGL2       2
#define ATRIUM_GPU_CAPSET_GFXSTREAM    3
#define ATRIUM_GPU_CAPSET_VENUS        4
#define ATRIUM_GPU_CAPSET_CROSS_DOMAIN 5
#define ATRIUM_GPU_CAPSET_DRM          6

/* Practical cap on capset blob size returned by GET_CAPSET. The host
 * picks whatever the renderer reports; venus is ~256 bytes today. */
#define ATRIUM_GPU_CAPSET_DATA_MAX  4096

/*
 * ATRIUM_GPU_IOC_CAPSET_QUERY — does the host advertise this capset?
 *
 *   in:   capset_id      — see ATRIUM_GPU_CAPSET_*
 *         capset_version — requested version (0 = latest)
 *         data_ptr       — userspace buffer for the capset blob, or NULL
 *                          to query size only (data_size still returned)
 *   out:  actual_version — 0 if capset isn't advertised by host
 *         data_size      — bytes the host returned (or would have)
 *
 * Side-effect-free at the kmod/host level beyond a virtio round-trip;
 * a sandboxed process that only needs to probe for venus can hold
 * CAP_IOCTL on this command alone.
 */
struct atrium_gpu_capset_query {
	uint32_t capset_id;
	uint32_t capset_version;
	uint32_t actual_version;
	uint32_t data_size;
	uint64_t data_ptr;
	uint64_t _reserved[2];
};
#define ATRIUM_GPU_IOC_CAPSET_QUERY  _IOWR('G', 0x40, struct atrium_gpu_capset_query)

/*
 * ATRIUM_GPU_IOC_CTX_INIT — bind a virtio-gpu context to this fd.
 *
 * Per-fd: each open(/dev/atrium-gpu0) gets at most one context. A
 * second CTX_INIT on the same fd returns EBUSY. Closing the fd implicitly
 * destroys the context (CTX_DESTROY issued from the file-priv destructor).
 *
 *   in:   capset_id   — usually ATRIUM_GPU_CAPSET_VENUS
 *         flags       — reserved, must be 0
 *         debug_name  — passed to host for debugging; not interpreted
 *   out:  ctx_id_out  — server-assigned id (informational; not a handle)
 */
struct atrium_gpu_ctx_init {
	uint32_t capset_id;
	uint32_t flags;
	char     debug_name[64];
	uint32_t ctx_id_out;
	uint32_t _reserved[3];
};
#define ATRIUM_GPU_IOC_CTX_INIT  _IOWR('G', 0x41, struct atrium_gpu_ctx_init)

/*
 * ATRIUM_GPU_IOC_RESOURCE_ATTACH — bind an existing BO as a venus
 * blob resource visible to the host renderer.
 *
 * Per virtio-gpu spec §5.7 (BLOB), `blob_mem` is one of:
 *   ATRIUM_GPU_BLOB_MEM_GUEST  (0x0001)  — guest-allocated pages,
 *                                          host reads via attached
 *                                          backing
 *   ATRIUM_GPU_BLOB_MEM_HOST3D (0x0002)  — host-allocated, identified
 *                                          by blob_id from venus
 * `blob_flags` is a bitmask of:
 *   ATRIUM_GPU_BLOB_USE_MAPPABLE (0x01)  — mappable into guest
 *   ATRIUM_GPU_BLOB_USE_SHAREABLE(0x02)  — exportable to other ctxs
 *   ATRIUM_GPU_BLOB_USE_CROSS_DEV(0x04)  — exportable to other devs
 *
 * Per-fd / per-context: the resource is owned by `f->ctx_id` (must be
 * non-zero — call CTX_INIT first). Resource id is server-assigned;
 * userspace embeds it in subsequent SUBMIT_3D payloads.
 */
#define ATRIUM_GPU_BLOB_MEM_GUEST    0x0001
#define ATRIUM_GPU_BLOB_MEM_HOST3D   0x0002

#define ATRIUM_GPU_BLOB_USE_MAPPABLE  0x01
#define ATRIUM_GPU_BLOB_USE_SHAREABLE 0x02
#define ATRIUM_GPU_BLOB_USE_CROSS_DEV 0x04

struct atrium_gpu_resource_attach {
	uint32_t bo_handle;       /* in: BO from IOC_ALLOC */
	uint32_t blob_mem;        /* in: ATRIUM_GPU_BLOB_MEM_* */
	uint32_t blob_flags;      /* in: ATRIUM_GPU_BLOB_USE_* bitmask */
	uint32_t _pad0;
	uint64_t blob_id;         /* in: opaque host-side id (venus assigns) */
	uint32_t resource_id_out; /* out: kmod-allocated, monotonic */
	uint32_t _reserved[3];
};
#define ATRIUM_GPU_IOC_RESOURCE_ATTACH \
	_IOWR('G', 0x42, struct atrium_gpu_resource_attach)

/*
 * ATRIUM_GPU_IOC_SUBMIT_3D — submit an opaque venus command stream
 * to this fd's context.
 *
 * The kernel does not parse `cmd_ptr`/`cmd_size` — bytes are forwarded
 * verbatim to virglrenderer's render-server, which dispatches them
 * against the per-context worker.
 *
 *   in:  cmd_ptr / cmd_size      — userspace command stream
 *        bo_handles_ptr / count  — BOs the submit references; kept
 *                                  pinned until the fence retires
 *        flags                   — bit 0 = SIGNAL_FENCE (default on)
 *   out: fence_out               — fence-id (0 if SIGNAL_FENCE clear)
 */
#define ATRIUM_GPU_SUBMIT_3D_SIGNAL_FENCE 0x01

struct atrium_gpu_submit_3d {
	uint64_t cmd_ptr;          /* in:  userspace pointer */
	uint32_t cmd_size;         /* in:  bytes */
	uint32_t flags;            /* in:  ATRIUM_GPU_SUBMIT_3D_* */
	uint32_t bo_count;         /* in:  number of BO handles referenced */
	uint32_t _pad0;
	uint64_t bo_handles_ptr;   /* in:  userspace pointer to uint32_t[] */
	uint64_t fence_out;        /* out: fence id (0 if no fence) */
	uint64_t _reserved[2];
};
#define ATRIUM_GPU_IOC_SUBMIT_3D \
	_IOWR('G', 0x43, struct atrium_gpu_submit_3d)

/*
 * ATRIUM_GPU_IOC_CTX_FENCE_WAIT — block until a venus fence retires.
 *
 * Distinct from the older `ATRIUM_GPU_IOC_FENCE_WAIT` (which targets
 * the engine-wide fence stream): this waits on a fence_id from a
 * SUBMIT_3D call on this fd's context.
 *
 *   in:   fence       — id from SUBMIT_3D fence_out
 *         timeout_ns  — ~0 = block forever, 0 = poll
 *   out:  status      — 0 signalled, EBUSY timed out
 */
struct atrium_gpu_ctx_fence_wait {
	uint64_t fence;
	uint64_t timeout_ns;
	uint32_t status;
	uint32_t _pad0;
};
#define ATRIUM_GPU_IOC_CTX_FENCE_WAIT \
	_IOWR('G', 0x44, struct atrium_gpu_ctx_fence_wait)

/*
 * ATRIUM_GPU_IOC_HOST_BLOB — V5h: allocate a HOST3D blob backed by the
 * virtio-gpu host_visible PCI BAR.
 *
 * Combines what would be ALLOC + RESOURCE_ATTACH + RESOURCE_MAP_BLOB on
 * the upstream Linux uAPI. The BO returned has NO guest pages: mmap()ing
 * it returns BAR pages (cacheable on the guest, allocated by virglrenderer
 * on the host). This is the path the venus frontend's shmem ring + every
 * VkDeviceMemory allocation needs — the proxy can export the host shmem
 * fd to the render-server worker, where guest sglists fail (V5g
 * fd_type=-1 problem).
 *
 *   in:   size        — requested bytes (page-rounded by kmod)
 *         blob_flags  — VIRTIO_GPU_BLOB_USE_MAPPABLE + USE_SHAREABLE etc
 *         blob_id     — venus mem_id when caller has one, 0 for shmem
 *   out:  bo_handle      — kmod-local BO handle (for IOC_FREE)
 *         resource_id    — virtio-gpu resource id (for SUBMIT_3D bo_handles)
 *         mmap_offset    — pass to mmap(/dev/atrium-gpu0, ...) at this off
 *         actual_size    — page-rounded size
 *
 * Requires CTX_INIT (fd has a venus context). Returns ENXIO if the host
 * didn't expose a host_visible region (no QEMU -hostmem); userspace
 * should then fall back to BLOB_MEM_GUEST + ATTACH (V5g path) and accept
 * that venus won't work past the first vn_call.
 */
struct atrium_gpu_host_blob {
	uint64_t size;
	uint32_t blob_flags;
	uint32_t _pad0;
	uint64_t blob_id;
	uint32_t bo_handle;
	uint32_t resource_id;
	uint64_t mmap_offset;
	uint64_t actual_size;
	uint64_t _reserved[2];
};
#define ATRIUM_GPU_IOC_HOST_BLOB \
	_IOWR('G', 0x45, struct atrium_gpu_host_blob)

/* ---------- Display ---------- */

struct atrium_display_bind_gpu {
	int gpu_fd;
	int _pad0;
};

#define ATRIUM_DISPLAY_IOC_BIND_GPU _IOW ('D', 0, struct atrium_display_bind_gpu)

#define FRESCO_CONNECTOR_UNKNOWN  0
#define FRESCO_CONNECTOR_HDMI     1
#define FRESCO_CONNECTOR_DP       2
#define FRESCO_CONNECTOR_EDP      3
#define FRESCO_CONNECTOR_DSI      4
#define FRESCO_CONNECTOR_VIRTUAL  5  /* virtio-gpu, vnc, etc. */

#define FRESCO_CONNECTOR_FLAG_CONNECTED 0x01
#define FRESCO_CONNECTOR_FLAG_INTERNAL  0x02

struct atrium_display_connector {
	uint32_t id;
	uint16_t type;
	uint16_t flags;
	uint32_t edid_size;
	uint32_t _pad0;
	uint64_t edid_ptr;
};

struct atrium_display_enum {
	uint32_t count_in;
	uint32_t count_out;
	uint64_t connectors_ptr;
};

#define ATRIUM_DISPLAY_IOC_ENUM_CONNECTORS _IOWR('D', 1, struct atrium_display_enum)

#define FRESCO_MODE_FLAG_HSYNC_POS    0x0001
#define FRESCO_MODE_FLAG_HSYNC_NEG    0x0002
#define FRESCO_MODE_FLAG_VSYNC_POS    0x0004
#define FRESCO_MODE_FLAG_VSYNC_NEG    0x0008
#define FRESCO_MODE_FLAG_INTERLACED   0x0010
#define FRESCO_MODE_FLAG_DBL_SCAN     0x0020
#define FRESCO_MODE_FLAG_PREFERRED    0x0040

struct atrium_display_mode {
	uint32_t width;
	uint32_t height;
	uint32_t pixel_clock_khz;
	uint32_t refresh_mhz;
	uint16_t h_sync_start;
	uint16_t h_sync_end;
	uint16_t h_total;
	uint16_t h_skew;
	uint16_t v_sync_start;
	uint16_t v_sync_end;
	uint16_t v_total;
	uint16_t v_scan;
	uint16_t flags;
	uint16_t _pad0;
	uint64_t _reserved[2];
};

struct atrium_display_modes_query {
	uint32_t connector_id;
	uint32_t count_in;
	uint32_t count_out;
	uint32_t _pad0;
	uint64_t modes_ptr;
};

#define ATRIUM_DISPLAY_IOC_MODES _IOWR('D', 2, struct atrium_display_modes_query)

struct atrium_display_set_mode {
	uint32_t connector_id;
	uint32_t scanout_handle;
	struct atrium_display_mode mode;
};

#define ATRIUM_DISPLAY_IOC_SET_MODE _IOW ('D', 3, struct atrium_display_set_mode)

#define FRESCO_PAGE_FLIP_INCLUDE_CURSOR 0x02

struct atrium_display_page_flip {
	uint32_t connector_id;
	uint32_t scanout_handle;
	uint64_t wait_fence;
	uint64_t flip_id;
	uint32_t flags;
	uint32_t _pad0;
};

#define ATRIUM_DISPLAY_IOC_PAGE_FLIP _IOW ('D', 4, struct atrium_display_page_flip)

/* IOC_WAIT_VBLANK — block until next vblank tick on `connector_id`.
 * Returns post-wait sequence counter in `seq`. Callers can detect
 * missed vblanks: `seq[N] - seq[N-1] > 1` ⇒ frames were skipped.
 *
 * On virtio-gpu (no native vblank IRQ) the kmod emulates ticks via
 * a `callout(9)` firing at the connector's mode refresh interval.
 * On D5+ native HW the callout is replaced by a real GPU IRQ; the
 * ABI is unchanged.
 */
struct atrium_display_wait_vblank {
	uint32_t connector_id;
	uint32_t _pad0;
	uint64_t seq;
};

#define ATRIUM_DISPLAY_IOC_WAIT_VBLANK _IOWR('D', 5, struct atrium_display_wait_vblank)

struct atrium_display_cursor {
	uint32_t connector_id;
	uint32_t cursor_handle;
	int32_t  x;
	int32_t  y;
	uint32_t hot_x;
	uint32_t hot_y;
};

#define ATRIUM_DISPLAY_IOC_CURSOR _IOW ('D', 5, struct atrium_display_cursor)

#define FRESCO_DISP_EVT_VBLANK         1
#define FRESCO_DISP_EVT_FLIP_COMPLETE  2
#define FRESCO_DISP_EVT_HOTPLUG        3

struct atrium_display_event {
	uint32_t kind;
	uint32_t connector_id;
	uint64_t flip_id;
	uint64_t timestamp_ns;
};

#endif /* ATRIUM_GPU_H_ */
