/*
 * Atrium display ABI v2.1 — /dev/atrium-display<N>. Normative header.
 *
 * Spec: docs/spec/atrium-display-architecture.md §7 and §7.1, and
 * docs/spec/atrium-gpu-abi-v2.md §7. This header follows the GPU header's
 * rules (gpu_abi_v2.h): every argument struct starts with uint32_t
 * struct_size, _pad and unknown flag bits must be zero (EINVAL), every fd
 * field is int32_t with -1 meaning none, no implicit padding.
 *
 * The display and the GPU share buffer fds and syncobj fds; that is the whole
 * interface between them. A scanout buffer is named by its bo_fd, never by an
 * address: the display module resolves the fd, checks the buffer against the
 * plane and keeps it pinned for as long as it is on screen.
 *
 * Ownership: the display node is exclusive. A second open() fails with EBUSY.
 * When the holder closes it, whatever is on screen stays on screen until the
 * next holder commits (hold-last-frame).
 *
 * Events: the display fd is EVFILT_READ-able. Each vblank adds 1 to kn_data
 * (EV_CLEAR is forced, so a kevent() returns the vblanks since the last one).
 * Flip completion is the commit's out-fence: a syncobj, itself kqueue-able.
 *
 * Connector and mode discovery is CONNECTOR (2.1): the monitor's raw EDID
 * and every mode it offers. The same for every GPU: the driver reads the
 * EDID over its own hardware (DDC, DP AUX) and the shared parser in
 * atrium-display/ builds the list. The interim ENUM / MODES ioctls and the
 * simulated-monitor test knobs in atrium-gpu-amd/atrium_display_abi.h remain
 * for the tests. The
 * interim SET_MODE / PAGE_FLIP ioctls, which took a raw VRAM offset, are
 * retired. v2 ioctls are numbered from 64 so the two headers can coexist.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifndef _ATRIUM_DISPLAY_ABI_V2_H_
#define _ATRIUM_DISPLAY_ABI_V2_H_

#ifdef _KERNEL
#include <sys/types.h>
#else
#include <stdint.h>
#endif
#ifdef __FreeBSD__
#include <sys/ioccom.h>
#else
#include <sys/ioctl.h>
#endif

#define ATRIUM_DISPLAY_ABI_MAJOR	2
#define ATRIUM_DISPLAY_ABI_MINOR	1

/* Pixel formats a plane can scan out. */
#define ATRIUM_DISPLAY_FORMAT_XRGB8888	0  /* 32 bpp, B,G,R,X in memory (Vulkan B8G8R8A8) */

/* Layout modifiers (GPU ABI §5.12). Only linear in 2.0. */
#define ATRIUM_DISPLAY_MODIFIER_LINEAR	0ull

/* Referee faults reported in out_fault (0 = none). */
#define ATRIUM_DISPLAY_FAULT_NONE		0
#define ATRIUM_DISPLAY_FAULT_NO_MODE		1  /* flip before any modeset */
#define ATRIUM_DISPLAY_FAULT_NO_CONNECTOR	2  /* modeset with no monitor / bad EDID */
#define ATRIUM_DISPLAY_FAULT_NOT_RESIDENT	3  /* buffer not in VRAM */
#define ATRIUM_DISPLAY_FAULT_TOO_SMALL		4  /* buffer smaller than pitch x height */
#define ATRIUM_DISPLAY_FAULT_EXCEEDS_LINK	5  /* mode needs more than the link carries */
#define ATRIUM_DISPLAY_FAULT_BAD_PITCH		6  /* pitch < a line, or not 256-byte aligned */
#define ATRIUM_DISPLAY_FAULT_BAD_FORMAT		7  /* format the plane can't scan out */
#define ATRIUM_DISPLAY_FAULT_MODE_UNSUPPORTED	8  /* (2.1) the monitor doesn't offer the
						    * timing that was programmed */

/* Commit flags. */
#define ATRIUM_DISPLAY_COMMIT_MODESET	0x1u  /* (re)program the CRTC at mode_index; the
					       * buffer is on screen from the first frame.
					       * Applies at once: its in-fence must
					       * already be signalled (EAGAIN if not) */
#define ATRIUM_DISPLAY_COMMIT_ASYNC	0x2u  /* flip now, not at vblank (may tear) */
#define ATRIUM_DISPLAY_COMMIT_TEST_ONLY	0x4u  /* validate the fds and the buffer, change nothing */
#define ATRIUM_DISPLAY_COMMIT_VRR	0x8u  /* (2.2) with MODESET: variable refresh. A frame
					       * lasts from the mode's own period up to the
					       * monitor's lowest refresh (CONNECTOR's
					       * out_vrr_min_hz), and a flip ends the frame
					       * it waits on as soon as the shortest frame
					       * allows. EINVAL without MODESET, for a
					       * monitor without VRR, or a mode whose
					       * refresh is outside the monitor's range.
					       * A MODESET without it turns VRR off */

/*
 * COMMIT: put a buffer on the primary plane.
 *
 * Without MODESET the flip latches at the next vblank (or at once with ASYNC).
 * With in_fence_fd set, the flip waits until that syncobj reaches
 * in_fence_value, and is programmed as soon as it does (an ASYNC flip then
 * lands at once, a vsync'd one at the following vblank). If the in-fence
 * fails instead (its render was lost to a hang, fault or reset), the commit
 * is dropped: the previous buffer stays up and the out-fence fails with the
 * same error. With out_fence_fd set, that syncobj is signalled to
 * out_fence_value once this buffer is on screen, which is also the moment the
 * previous buffer is released and may be rendered into again.
 *
 * One commit can be outstanding at a time. A commit issued while the previous
 * one is still waiting on its in-fence or its vblank fails with EBUSY: wait on
 * the out-fence (or the next vblank) and commit again. Nothing is dropped
 * silently.
 *
 * If the flip had to wait for its in-fence and the hardware then refuses the
 * buffer, the previous buffer stays on screen, the out-fence is still
 * signalled (so nothing waits forever) and STATUS reports the fault in
 * out_last_fault.
 *
 * A referee fault (the hardware refused the buffer or the mode) is not an
 * errno: COMMIT returns 0 with out_fault set and nothing changed on screen.
 * The kernel copies an ioctl's struct back only when it returns 0, so a fault
 * reported alongside an errno would never reach the caller.
 *
 * MODESET programs the timing of CONNECTOR's out_modes[mode_index]: any mode
 * the monitor offers, at any refresh rate it offers. The buffer must cover
 * that mode (EINVAL otherwise). Send the out_generation the index came from
 * in mode_generation so a monitor swapped in between is caught (ESTALE).
 *
 * Errors: EINVAL for a bad struct, flag, format, modifier, pitch alignment,
 * mode_index,
 * bo_offset or fd type; EBADF for a closed fd; EBUSY as above; EAGAIN for a
 * MODESET whose in-fence isn't signalled (its error if the in-fence failed);
 * ENXIO if the GPU module isn't
 * loaded (no BO can be resolved).
 */
struct atrium_display_commit {
	uint32_t struct_size;
	uint32_t flags;			/* ATRIUM_DISPLAY_COMMIT_* */
	uint32_t mode_index;		/* MODESET: index into CONNECTOR's out_modes */
	uint32_t format;		/* ATRIUM_DISPLAY_FORMAT_* */
	int32_t  bo_fd;			/* VRAM BO to scan out */
	uint32_t pitch;			/* bytes per line; 0 = packed (width * 4) */
	uint64_t bo_offset;		/* must be 0 in 2.0 */
	uint64_t modifier;		/* ATRIUM_DISPLAY_MODIFIER_* */
	int32_t  in_fence_fd;		/* syncobj to wait on, -1 = none */
	int32_t  out_fence_fd;		/* syncobj to signal when on screen, -1 = none */
	uint64_t in_fence_value;
	uint64_t out_fence_value;
	uint32_t out_fault;		/* ATRIUM_DISPLAY_FAULT_* */
	uint32_t mode_generation;	/* (2.1) MODESET: CONNECTOR's out_generation the
					 * mode_index was read from; ESTALE if a
					 * hotplug has changed it since. 0 = don't
					 * check (2.0 callers send 0 here) */
};

/* STATUS flags. */
#define ATRIUM_DISPLAY_STATUS_COMMIT_PENDING	0x1u  /* a commit hasn't reached the screen yet */
#define ATRIUM_DISPLAY_STATUS_MODE_SET		0x2u  /* the CRTC is lit */
#define ATRIUM_DISPLAY_STATUS_CONTENT_LOST	0x4u  /* a device reset (GPU ABI v2.1
						 * §10.3) may have wiped what is on
						 * screen; cleared when a commit made
						 * after the reset reaches the screen */

/*
 * kqueue fflags on EVFILT_READ (GPU ABI v2.1 §10.3). Register with
 * fflags = ATRIUM_DISPLAY_NOTE_RESET for a reset-only event: it fires once per
 * device reset, with data = the device's reset generation, and never for a
 * vblank. A plain vblank registration (fflags 0) also sees NOTE_RESET in the
 * returned fflags when a reset happened since its last event. Either way the
 * holder should re-render and commit: VRAM, the on-screen buffer included,
 * may not have survived.
 */
#define ATRIUM_DISPLAY_NOTE_RESET		0x1u

/*
 * (2.1) Hotplug: a monitor was connected, disconnected or swapped. Register
 * with fflags = ATRIUM_DISPLAY_NOTE_HOTPLUG for a hotplug-only event with
 * data = the connector generation (CONNECTOR's out_generation); a vblank
 * registration also sees it in the returned fflags. NOTE_RESET and
 * NOTE_HOTPLUG may be registered together; data is then the generation of
 * whichever fired last, and fflags says which. On hotplug the holder reads
 * CONNECTOR again and, if the monitor is there, commits a MODESET at one of
 * its modes. What was on screen stays programmed while no monitor is
 * attached.
 */
#define ATRIUM_DISPLAY_NOTE_HOTPLUG		0x2u

/* STATUS: refresh and flip telemetry for frame pacing and tests. */
struct atrium_display_status_v2 {
	uint32_t struct_size;
	uint32_t out_flags;		/* ATRIUM_DISPLAY_STATUS_* */
	uint64_t out_vblank_count;	/* vblanks since the mode was set */
	uint64_t out_flips_done;	/* flips that reached the screen */
	uint32_t out_dropped_flips;	/* flips the hardware queue dropped (always 0
					 * through COMMIT, which returns EBUSY instead) */
	uint32_t out_tear_line;		/* first tear scanline this frame, 0xffffffff = none */
	uint32_t out_refresh_mhz;	/* current mode's refresh in milli-Hz, 0 = no mode */
	uint32_t out_last_fault;	/* fault of the last flip that ran after its
					 * commit returned (ATRIUM_DISPLAY_FAULT_*) */
};

/* (2.1) CONNECTOR flags and per-mode flags. */
#define ATRIUM_DISPLAY_CONNECTOR_CONNECTED	0x1u
#define ATRIUM_DISPLAY_CONNECTOR_VRR		0x2u  /* (2.2) the monitor offers variable refresh */
#define ATRIUM_DISPLAY_CONNECTOR_VRR_ACTIVE	0x4u  /* (2.2) the lit mode runs with it */
#define ATRIUM_DISPLAY_MODE_PREFERRED		0x1u  /* the monitor's native mode */

#define ATRIUM_DISPLAY_EDID_MAX		512	/* base block + 3 extensions */
#define ATRIUM_DISPLAY_CONNECTOR_MODES	16

struct atrium_display_mode_v2 {
	uint32_t width;			/* active pixels per line */
	uint32_t height;		/* active lines */
	uint32_t refresh_mhz;		/* vertical refresh, milli-Hz (59940 = 59.94 Hz) */
	uint32_t pixel_clock_khz;
	uint32_t flags;			/* ATRIUM_DISPLAY_MODE_* */
	uint32_t _pad;
};

/*
 * CONNECTOR (2.1): the monitor and its modes. out_modes lists every mode the
 * EDID offers that the parser knows, preferred first (flagged), then in EDID
 * order without duplicates; at most ATRIUM_DISPLAY_CONNECTOR_MODES.
 * out_generation changes on every hotplug; COMMIT's mode_generation checks
 * it. Without a monitor, out_flags lacks CONNECTED and there are no modes.
 */
struct atrium_display_connector_v2 {
	uint32_t struct_size;
	uint32_t out_flags;		/* ATRIUM_DISPLAY_CONNECTOR_* */
	uint32_t out_connector_type;	/* ATRIUM_DISPLAY_CONN_* (interim header) */
	uint32_t out_generation;	/* hotplug generation, never 0 */
	uint32_t out_edid_len;		/* bytes of out_edid filled */
	uint32_t out_mode_count;
	uint32_t out_current_mode;	/* index of the lit mode, 0xffffffff = none
					 * or not in this list */
	uint16_t out_vrr_min_hz;	/* (2.2) variable refresh range the monitor */
	uint16_t out_vrr_max_hz;	/* takes any rate in; 0 = fixed refresh */
	uint8_t  out_edid[ATRIUM_DISPLAY_EDID_MAX];
	struct atrium_display_mode_v2 out_modes[ATRIUM_DISPLAY_CONNECTOR_MODES];
};

#define ATRIUM_DISPLAY_IOC_COMMIT	_IOWR('D', 64, struct atrium_display_commit)
#define ATRIUM_DISPLAY_IOC_STATUS_V2	_IOWR('D', 65, struct atrium_display_status_v2)
#define ATRIUM_DISPLAY_IOC_CONNECTOR	_IOWR('D', 66, struct atrium_display_connector_v2)

#endif /* _ATRIUM_DISPLAY_ABI_V2_H_ */
