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
