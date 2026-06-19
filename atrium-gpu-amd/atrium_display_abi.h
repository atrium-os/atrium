/*
 * atrium-gpu-amd-display — public userspace ABI for /dev/atrium-display0.
 *
 * The display engine is a SEPARATE driver from the GPU (§4.1 three-module
 * split): a distinct IP block (DCN) on the same PCI device, with its own
 * register aperture and its own cdev. This ABI is deliberately decoupled from
 * the GPU ABI — a compositor talks to the display to discover connectors, set a
 * mode, and page-flip, independent of how (or whether) the GPU is rendering.
 *
 * Scanout-buffer handoff is dma-buf-style: the compositor allocates a VRAM BO
 * through /dev/atrium-gpu0, exports it (ATRIUM_GPU_IOC_BO_EXPORT_SCANOUT) to a
 * plain {vram_offset, size}, and hands THAT to SET_MODE/PAGE_FLIP here. The
 * display imports the VRAM range by offset and never touches the GPU's BO table
 * — the offset is absolute against the shared VRAM aperture, so no cross-module
 * bind is needed. This mirrors importing a dma-buf fd into a DRM KMS plane.
 *
 * Struct layout rule (same as the GPU ABI): 64-bit fields first, then 32-bit,
 * explicit pad to a multiple of 8 — identical for 32- and 64-bit userspace.
 */
#ifndef _ATRIUM_DISPLAY_ABI_H_
#define _ATRIUM_DISPLAY_ABI_H_

#include <sys/ioccom.h>
#include <sys/types.h>

#define ATRIUM_DISPLAY_EDID_LEN		128

/* §8 connector interface type codes (mirrors the model's connector type). */
#define ATRIUM_DISPLAY_CONN_HDMI14	1
#define ATRIUM_DISPLAY_CONN_HDMI21	2
#define ATRIUM_DISPLAY_CONN_DP14	3
#define ATRIUM_DISPLAY_CONN_USBC_DP	4
#define ATRIUM_DISPLAY_CONN_EDP		5

/*
 * ENUM_CONNECTOR: report HPD state, the §8 interface type, and the full EDID
 * base block read over the modeled DDC (I2C). A disconnected connector floats
 * the DDC high (0xff), so the EDID then fails its header/checksum — the
 * realistic "monitor unplugged" failure mode rather than a separate error.
 */
struct atrium_display_connector {
	uint32_t connected;	/* out: 1 = monitor attached (HPD) */
	uint32_t connector_type; /* out: ATRIUM_DISPLAY_CONN_* */
	uint32_t usbc_lanes;	/* out: USB-C alt-mode lane count (0 = not USB-C) */
	uint32_t edid_len;	/* out: EDID bytes returned (128) */
	uint8_t  edid[ATRIUM_DISPLAY_EDID_LEN]; /* out: EDID base block over DDC */
};

/* One display mode (a detailed timing decoded from the EDID, or a built-in). */
struct atrium_display_mode {
	uint32_t width;		/* out: active pixels per line */
	uint32_t height;	/* out: active lines */
	uint32_t refresh_mhz;	/* out: vertical refresh in milli-Hz (e.g. 60000) */
	uint32_t pad;
};

/*
 * MODES: enumerate the connector's modes. The driver reads the EDID over DDC
 * and decodes its Detailed Timing Descriptors into mode entries (the preferred
 * timing is first, mirroring EDID order). `count` is the number filled; entries
 * beyond ATRIUM_DISPLAY_MAX_MODES are not reported (count is still the total).
 */
#define ATRIUM_DISPLAY_MAX_MODES	8
struct atrium_display_modes {
	uint32_t count;		/* out: total modes decoded from the EDID */
	uint32_t pad;
	struct atrium_display_mode modes[ATRIUM_DISPLAY_MAX_MODES]; /* out */
};

/*
 * SET_MODE: program the connector's mode and install `vram_offset`/`size` as the
 * initial scanout framebuffer (the exported VRAM BO). `fault` returns the
 * model's DisplayFault code (0 = ok; e.g. FB-not-resident, FB-too-small,
 * mode-exceeds-link) so a failed modeset is observable without a separate errno.
 */
struct atrium_display_setmode {
	uint64_t vram_offset;	/* in:  scanout FB VRAM offset (from BO_EXPORT) */
	uint64_t size;		/* in:  FB size in bytes */
	uint32_t fault;		/* out: DisplayFault code (0 = ok) */
	uint32_t pad;
};

/*
 * PAGE_FLIP: scan out a new framebuffer. vsync = 1 latches it at the next vblank
 * (no tear); vsync = 0 takes effect immediately (tears at the current beam
 * line). The depth-1 flip queue drops-and-counts a flip issued while one is
 * already pending (see STATUS.dropped_flips).
 */
struct atrium_display_flip {
	uint64_t vram_offset;	/* in:  new scanout FB VRAM offset */
	uint64_t size;		/* in:  FB size in bytes */
	uint32_t vsync;		/* in:  1 = latch at vblank (no tear) */
	uint32_t fault;		/* out: DisplayFault code (0 = ok) */
};

/* STATUS: refresh + flip telemetry the compositor uses for frame pacing. */
struct atrium_display_status {
	uint64_t vblank_count;	/* out: vblanks elapsed */
	uint32_t dropped_flips;	/* out: flips dropped by the depth-1 queue */
	uint32_t tear_line;	/* out: last tear scanline (0xffffffff = none) */
};

/*
 * Reconfigure the simulated monitor (bring-up / test): re-cable the connector to
 * a different §8 interface type and/or re-plug it advertising a different
 * built-in mode — drives the link-bandwidth referee (e.g. a 4K monitor on an
 * HDMI 1.4 cable -> ModeExceedsLink).
 */
struct atrium_display_config {
	uint32_t connector_type; /* in: ATRIUM_DISPLAY_CONN_* */
	uint32_t plug_mode;	/* in: re-plug advertising mode (0=VGA 1=1080p 2=4K) */
};

/*
 * USB-C DisplayPort Alt Mode (§8 cross-subsystem): the connector is virtual
 * until PD negotiates alt-mode. lanes = 0 puts the port in USB mode (no
 * display); 2 or 4 enters DP Alt Mode with that lane count (4 = full bandwidth,
 * 2 = half — USB takes the other pair).
 */
struct atrium_display_usbc {
	uint32_t lanes;
};

/*
 * DisplayPort MST (§8): one link fans out to a dynamic set of sinks sharing its
 * bandwidth. op 0 = enable/reset the hub; op 1 = add a sink advertising mode
 * `arg` (0=VGA 1=1080p 2=4K); op 2 = query sink `arg`. count/starved are out.
 */
struct atrium_display_mst {
	uint32_t op;
	uint32_t arg;
	uint32_t count;		/* out: number of sinks */
	uint32_t starved;	/* out: (op 2) selected sink bandwidth-starved? */
};

/*
 * DisplayPort link training (§8): negotiate against a physical cable. Usable
 * bandwidth is an outcome — a marginal cable falls back to a lower rate.
 */
struct atrium_display_dptrain {
	uint32_t cable_rate;	/* in: cable's max rate (0=RBR 1=HBR 2=HBR2 3=HBR3) */
	uint32_t cable_lanes;	/* in: cable's wired lane count */
	uint32_t bw_mbps;	/* out: trained bandwidth, MB/s */
	uint32_t trained;	/* out: 1 = a link trained (0 = dead cable) */
};

/* 'D' = the display device; distinct from the GPU's 'A' namespace. */
#define ATRIUM_DISPLAY_IOC_ENUM		_IOR('D', 1, struct atrium_display_connector)
#define ATRIUM_DISPLAY_IOC_MODES	_IOR('D', 2, struct atrium_display_modes)
#define ATRIUM_DISPLAY_IOC_SET_MODE	_IOWR('D', 3, struct atrium_display_setmode)
#define ATRIUM_DISPLAY_IOC_PAGE_FLIP	_IOWR('D', 4, struct atrium_display_flip)
#define ATRIUM_DISPLAY_IOC_STATUS	_IOR('D', 5, struct atrium_display_status)
#define ATRIUM_DISPLAY_IOC_CONFIG	_IOW('D', 6, struct atrium_display_config)
#define ATRIUM_DISPLAY_IOC_USBC		_IOW('D', 7, struct atrium_display_usbc)
#define ATRIUM_DISPLAY_IOC_MST		_IOWR('D', 8, struct atrium_display_mst)
#define ATRIUM_DISPLAY_IOC_DPTRAIN	_IOWR('D', 9, struct atrium_display_dptrain)

#endif /* _ATRIUM_DISPLAY_ABI_H_ */
