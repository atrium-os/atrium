/*
 * Atrium display ABI 3.1 — /dev/atrium-display<N>. Normative header.
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

#define ATRIUM_DISPLAY_ABI_MAJOR	3
#define ATRIUM_DISPLAY_ABI_MINOR	1

/* Pixel formats a plane can scan out. */
#define ATRIUM_DISPLAY_FORMAT_XRGB8888	0  /* 32 bpp, B,G,R,X in memory (Vulkan B8G8R8A8) */
#define ATRIUM_DISPLAY_FORMAT_XRGB2101010 1 /* (2.3) 32 bpp little-endian words: B 9:0,
					     * G 19:10, R 29:20, X 31:30 (Vulkan
					     * A2R10G10B10_UNORM_PACK32). For HDR the
					     * values are the EOTF's code values (PQ or
					     * HLG), as HDR sets out */

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
#define ATRIUM_DISPLAY_FAULT_BANDWIDTH		9  /* (3.0) the lit outputs together would
						    * need more pixel rate than the
						    * display engine has (OUTPUTS'
						    * out_bandwidth_khz) */
#define ATRIUM_DISPLAY_FAULT_NO_OUTPUT		10 /* (3.0) no such output, or named twice
						    * in one commit */
#define ATRIUM_DISPLAY_FAULT_SYNC_MISMATCH	11 /* (3.1) a sync group's outputs don't
						    * all run one timing, or VRR is on
						    * for some and off for others */

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
#define ATRIUM_DISPLAY_COMMIT_OFF	0x10u /* (3.0) COMMIT_MULTI only: turn the output
					       * off (no buffer: bo_fd -1). Its
					       * buffer and pending flip are released;
					       * the monitor stays attached */

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

/*
 * (3.0) Which outputs' vblanks a vblank registration counts: fflags bit
 * NOTE_OUTPUT(n) for output n. Without any, output 0 (the 2.x behaviour). A
 * delivery's fflags carries NOTE_OUTPUT(n) for each output that had a
 * vblank since the last one, and data counts them all. (A file has one
 * EVFILT_READ knote, so one registration covers every output.) With
 * NOTE_RESET or NOTE_HOTPLUG the registration hears only those and the
 * output bits must be 0. NOTE_HOTPLUG's data is the topology generation
 * (OUTPUTS' out_topology_generation), bumped by a hotplug on any output.
 * An output's generation is the topology generation of its last hotplug, so
 * with one monitor the data still equals CONNECTOR's out_generation.
 */
#define ATRIUM_DISPLAY_NOTE_OUTPUT(n)		(0x100u << (n))
#define ATRIUM_DISPLAY_NOTE_OUTPUTS		0xff00u

/*
 * (2.3) HDR: what the monitor can show, and which EOTF and colorimetry the
 * stream is signalled with (CTA-861.3 / CTA-861-G infoframes).
 *
 * The out_* fields always describe the monitor, from its EDID: out_eotfs has
 * bit n set for EOTF n (SDR is always there), out_colorimetry bit n for
 * colorimetry n (DEFAULT always), and the luminances are the ones it would
 * like content mastered for (0 = not given).
 *
 * Without SET, eotf .. max_fall return what is signalled now. With SET they
 * are what to signal, and take effect with the next COMMIT that succeeds
 * (MODESET or flip), so the switch reaches the monitor with the first frame
 * rendered for it; a SET that no commit follows changes nothing on screen,
 * and a later SET replaces it. EOTF_SDR with COLORIMETRY_DEFAULT stops the
 * infoframes: the monitor's defaults. The surface format is the commit's
 * business: HDR works in XRGB8888 too, with more banding.
 *
 * A MODESET re-checks against the monitor then attached: if it no longer
 * offers the signalled EOTF or colorimetry (a hotplug swapped it), the
 * stream falls back to SDR / default and a read says so.
 *
 * Errors: EINVAL for a bad struct, flag, eotf or colorimetry, or one the
 * monitor does not offer; ENODEV with SET while no monitor is attached.
 */
#define ATRIUM_DISPLAY_HDR_SET			0x1u

#define ATRIUM_DISPLAY_EOTF_SDR			0  /* traditional gamma, SDR range */
#define ATRIUM_DISPLAY_EOTF_TRAD_HDR		1  /* traditional gamma, HDR range */
#define ATRIUM_DISPLAY_EOTF_PQ			2  /* SMPTE ST 2084 (HDR10) */
#define ATRIUM_DISPLAY_EOTF_HLG			3  /* ARIB STD-B67 hybrid log-gamma */

#define ATRIUM_DISPLAY_COLORIMETRY_DEFAULT	0  /* the format's default (sRGB / BT.709) */
#define ATRIUM_DISPLAY_COLORIMETRY_BT2020	1  /* BT.2020 RGB */

struct atrium_display_hdr {
	uint32_t struct_size;
	uint32_t flags;			/* ATRIUM_DISPLAY_HDR_* */
	uint32_t eotf;			/* ATRIUM_DISPLAY_EOTF_* */
	uint32_t colorimetry;		/* ATRIUM_DISPLAY_COLORIMETRY_* */
	/* Static metadata type 1: the mastering display and the content. */
	uint16_t primaries_x[3];	/* R, G, B chromaticity, units of 0.00002 */
	uint16_t primaries_y[3];
	uint16_t white_x;
	uint16_t white_y;
	uint16_t max_mastering_nits;	/* cd/m^2 */
	uint16_t min_mastering;		/* units of 0.0001 cd/m^2 */
	uint16_t max_cll;		/* brightest pixel, cd/m^2; 0 = unknown */
	uint16_t max_fall;		/* brightest frame average, cd/m^2; 0 = unknown */
	/* The monitor. */
	uint32_t out_eotfs;		/* bit n = ATRIUM_DISPLAY_EOTF n offered */
	uint32_t out_colorimetry;	/* bit n = ATRIUM_DISPLAY_COLORIMETRY n offered */
	uint32_t out_max_nits;		/* desired content max luminance, cd/m^2 */
	uint32_t out_max_fall_nits;	/* desired content max frame average */
	uint32_t out_min_nits_x10000;	/* desired content min, 0.0001 cd/m^2 */
	uint32_t output;		/* (3.0) the output; 0 for 2.x callers. A SET
					 * applies with the next commit that
					 * succeeds on that output */
};

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
#define ATRIUM_DISPLAY_CONNECTOR_HDR		0x8u  /* (2.3) the monitor declares HDR (see HDR) */
#define ATRIUM_DISPLAY_CONNECTOR_HDR_ACTIVE	0x10u /* (2.3) an HDR EOTF is being signalled */
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
#define ATRIUM_DISPLAY_IOC_HDR		_IOWR('D', 67, struct atrium_display_hdr)

/*
 * --- 3.0: several outputs on one node --------------------------------------
 *
 * A GPU drives up to ATRIUM_DISPLAY_MAX_OUTPUTS outputs, each a scanout pipe
 * with its own connector. The 2.x ioctls (COMMIT, STATUS_V2, CONNECTOR) act
 * on output 0, so a single-monitor caller works unchanged; HDR names its
 * output in `output`.
 *
 * Topology changes (lighting, re-moding or turning off outputs) go through
 * COMMIT_MULTI as one transaction: every output it names changes, or none
 * does. TEST_ONLY runs every check without touching the hardware and says
 * which output would fail and why, so a caller can pick another
 * configuration first instead of discovering a black screen.
 */
#define ATRIUM_DISPLAY_MAX_OUTPUTS		8

/* Per-output flags in OUTPUTS (the CONNECTOR_* flags, plus these). */
#define ATRIUM_DISPLAY_OUTPUT_LIT		0x100u /* scanning out a mode */
#define ATRIUM_DISPLAY_OUTPUT_SYNC_MASTER	0x200u /* (3.1) its timing drives its sync group */
#define ATRIUM_DISPLAY_OUTPUT_FLIP_LOCKED	0x400u /* (3.1) its flips land with its group's */

/*
 * A monitor's identity: a 64-bit hash of its EDID's manufacturer, product
 * and serial number (the serial-number descriptor too). A monitor that gives
 * no serial also hashes in the output it is on, so two of them still differ
 * (but that one changes identity when moved). 0 = no monitor. The same
 * monitor has the same identity on every boot, every port and every GPU
 * running this ABI; compositors key remembered layouts by the set of them.
 */
struct atrium_display_output_info {
	uint64_t identity;
	uint32_t flags;			/* ATRIUM_DISPLAY_CONNECTOR_* | OUTPUT_LIT */
	uint32_t connector_type;	/* ATRIUM_DISPLAY_CONN_* (interim header) */
	uint32_t generation;		/* this output's hotplug generation (CONNECTOR_AT's
					 * out_generation) */
	uint32_t width;			/* the lit mode; 0 when off */
	uint32_t height;
	uint32_t refresh_mhz;
	uint32_t pixel_clock_khz;	/* its share of out_bandwidth_khz */
	uint32_t sync_group;		/* (3.1) its COMMIT_SYNC_GROUP group, 0 = none */
};

/* OUTPUTS: every output at a glance, one consistent snapshot. */
struct atrium_display_outputs {
	uint32_t struct_size;
	uint32_t out_count;		/* outputs this GPU has */
	uint32_t out_topology_generation; /* bumped by a hotplug on any output */
	uint32_t out_bandwidth_khz;	/* pixel rate the lit outputs may share */
	uint32_t out_bandwidth_used_khz; /* what they draw now */
	uint32_t _pad;
	struct atrium_display_output_info out[ATRIUM_DISPLAY_MAX_OUTPUTS];
};

/* CONNECTOR_AT: CONNECTOR for any output (set conn.struct_size too). */
struct atrium_display_connector_at {
	uint32_t struct_size;
	uint32_t output;
	uint64_t out_identity;
	struct atrium_display_connector_v2 conn;
};

/* STATUS_AT: STATUS_V2 for any output (set status.struct_size too). */
struct atrium_display_status_at {
	uint32_t struct_size;
	uint32_t output;
	struct atrium_display_status_v2 status;
};

/*
 * One output's part of a COMMIT_MULTI: COMMIT's fields (TEST_ONLY is the
 * whole commit's flag, not an output's) plus the output. c.out_fault is set
 * for the output that failed.
 */
struct atrium_display_output_commit {
	uint32_t output;
	uint32_t _pad;
	struct atrium_display_commit c;
};

/*
 * COMMIT_MULTI: change several outputs at once.
 *
 * A topology commit has every element MODESET or OFF. It is atomic: the
 * whole new configuration is checked first (modes, buffers, the shared
 * bandwidth across every output that will be lit, in-fences signalled),
 * then applied; if the hardware still refuses one output, the outputs
 * already changed are put back as they were and nothing has changed. The
 * outputs it names go dark for the mode set and come back together; the
 * outputs it does not name keep running untouched.
 *
 * A flip commit has no MODESET or OFF element. Each output's flip waits on
 * its own in-fence and lands at its own vblank (monitors at different rates
 * never wait on each other), with its own out-fence. All outputs must be
 * free (EBUSY, nothing queued, if any still has a commit outstanding).
 *
 * Mixing the two kinds is EINVAL.
 *
 * Infeasibility is not an errno: the call returns 0 with out_failed = the
 * index (into the array) of the first output that cannot take its part and
 * out_reason = why (ATRIUM_DISPLAY_FAULT_*, also in that element's
 * c.out_fault); the array is copied back. With TEST_ONLY nothing changes
 * either way. For a flip commit a refusal stops at that output: the
 * outputs before it in the array stay queued.
 *
 * Errors: EINVAL for a bad struct, count (0 or over MAX_OUTPUTS), flag or
 * element (as COMMIT); EFAULT for a bad `commits` pointer; plus COMMIT's.
 */
#define ATRIUM_DISPLAY_MULTI_NONE		0xffffffffu

struct atrium_display_commit_multi {
	uint32_t struct_size;
	uint32_t flags;			/* ATRIUM_DISPLAY_COMMIT_TEST_ONLY */
	uint32_t count;			/* elements in commits */
	uint32_t out_failed;		/* index of the first refused element, or
					 * ATRIUM_DISPLAY_MULTI_NONE */
	uint32_t out_reason;		/* ATRIUM_DISPLAY_FAULT_* for it */
	uint32_t _pad;
	uint64_t commits;		/* user pointer: struct atrium_display_output_commit[count] */
};

#define ATRIUM_DISPLAY_IOC_OUTPUTS	_IOWR('D', 68, struct atrium_display_outputs)
#define ATRIUM_DISPLAY_IOC_CONNECTOR_AT	_IOWR('D', 69, struct atrium_display_connector_at)
#define ATRIUM_DISPLAY_IOC_STATUS_AT	_IOWR('D', 70, struct atrium_display_status_at)
#define ATRIUM_DISPLAY_IOC_COMMIT_MULTI	_IOWR('D', 71, struct atrium_display_commit_multi)

/*
 * COMMIT_SYNC_GROUP (3.1): lock outputs together, for a fullscreen window
 * spanning several monitors (docs/spec/fresco-span.md §5).
 *
 * A sync group's members run one timing generator clock: their vblanks
 * line up (on DCN, OTG timing synchronisation). The members in flip_lock
 * also flip together: a flip on one of them is held until every one of
 * them has a flip queued, and then all land in the same vblank. A flip
 * commit that names a flip-locked output must name every flip-locked output
 * of its group (EINVAL otherwise), so a group's flip is one COMMIT_MULTI.
 *
 * The call sets group `group`'s whole membership: outputs named join (moved
 * out of another group if they were in one), members not named leave, and
 * members = 0 dissolves the group. Every member must be lit
 * (FAULT_NO_MODE), all at one timing, with variable refresh on all of them
 * or none (FAULT_SYNC_MISMATCH). With VRR the group has one frame period:
 * every member runs the range all of them can take (the highest floor),
 * and goes back to its own when it leaves.
 * A member that is gone for a while (§6: its monitor dropped out) stays a
 * member, keeping the clock, with its flip lock off: the others flip
 * without it. A topology commit (MODESET or OFF) on a member takes it out
 * of its group; set the group again after re-moding it.
 *
 * Infeasibility is not an errno, as in COMMIT_MULTI: out_failed = the first
 * output refused and out_reason why, and nothing changes. EINVAL for a bad
 * struct, group, flag or mask (flip_lock not within members, or naming an
 * output that doesn't exist).
 */
#define ATRIUM_DISPLAY_SYNC_GROUPS		4	/* groups 1..4 */

struct atrium_display_sync_group {
	uint32_t struct_size;
	uint32_t flags;			/* none yet: 0 */
	uint32_t group;			/* 1..ATRIUM_DISPLAY_SYNC_GROUPS */
	uint32_t members;		/* bit n = output n */
	uint32_t flip_lock;		/* the members whose flips land together */
	uint32_t out_failed;		/* output refused, or ATRIUM_DISPLAY_MULTI_NONE */
	uint32_t out_reason;		/* ATRIUM_DISPLAY_FAULT_* for it */
	uint32_t _pad;
};

#define ATRIUM_DISPLAY_IOC_COMMIT_SYNC_GROUP _IOWR('D', 72, struct atrium_display_sync_group)

#endif /* _ATRIUM_DISPLAY_ABI_V2_H_ */
