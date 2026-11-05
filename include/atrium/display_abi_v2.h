/*
 * Atrium display ABI v2.0 — /dev/atrium-display<N>. Normative header.
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
 * Connector and mode discovery (ATRIUM_DISPLAY_IOC_ENUM / _MODES) and the
 * simulated-monitor test knobs are still the interim ones in
 * atrium-gpu-amd/atrium_display_abi.h; they carry no fds or addresses. The
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
#define ATRIUM_DISPLAY_ABI_MINOR	0

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

/* Commit flags. */
#define ATRIUM_DISPLAY_COMMIT_MODESET	0x1u  /* (re)program the CRTC at mode_index; the
					       * buffer is on screen from the first frame.
					       * Applies at once: its in-fence must
					       * already be signalled (EAGAIN if not) */
#define ATRIUM_DISPLAY_COMMIT_ASYNC	0x2u  /* flip now, not at vblank (may tear) */
#define ATRIUM_DISPLAY_COMMIT_TEST_ONLY	0x4u  /* validate the fds and the buffer, change nothing */

/*
 * COMMIT: put a buffer on the primary plane.
 *
 * Without MODESET the flip latches at the next vblank (or at once with ASYNC).
 * With in_fence_fd set, the flip waits until that syncobj reaches
 * in_fence_value. With out_fence_fd set, that syncobj is signalled to
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
 * Errors: EINVAL for a bad struct, flag, format, modifier, pitch alignment,
 * bo_offset or fd type; EBADF for a closed fd; EBUSY as above; EAGAIN for a
 * MODESET whose in-fence isn't signalled; ENXIO if the GPU module isn't
 * loaded (no BO can be resolved).
 */
struct atrium_display_commit {
	uint32_t struct_size;
	uint32_t flags;			/* ATRIUM_DISPLAY_COMMIT_* */
	uint32_t mode_index;		/* MODESET: index into MODES; must be 0 in 2.0 */
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
	uint32_t _pad;
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

#define ATRIUM_DISPLAY_IOC_COMMIT	_IOWR('D', 64, struct atrium_display_commit)
#define ATRIUM_DISPLAY_IOC_STATUS_V2	_IOWR('D', 65, struct atrium_display_status_v2)

#endif /* _ATRIUM_DISPLAY_ABI_V2_H_ */
