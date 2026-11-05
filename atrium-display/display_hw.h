/*
 * atrium-display: what a display driver provides, and the connector logic
 * every driver shares.
 *
 * The display ABI userspace sees (include/atrium/display_abi_v2.h) is the
 * same for every GPU: raw EDID, a mode list, "commit at mode N", hotplug and
 * reset events. Each driver supplies only the physical side through
 * atrium_display_hw_ops: how it reads EDID blocks from the monitor (DDC/I2C,
 * DP AUX), how it senses a monitor (the HPD pin), and how it programs a
 * timing into its CRTC (DCN's OTG on AMD). Everything above that, reading
 * the extension blocks, building the mode list and choosing the mode for a
 * commit, is shared (sink.c, edid.c).
 *
 * A driver also calls atrium_display_* from its hotplug interrupt; see
 * atrium-gpu-amd/display/display_dev.c for the AMD (DCN 4) implementation.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifndef _ATRIUM_DISPLAY_HW_H_
#define _ATRIUM_DISPLAY_HW_H_

#include "edid.h"

#define ATRIUM_DISPLAY_SINK_MODES	16

struct atrium_display_hw_ops {
	/* Is a monitor present (hot plug sense)? */
	int	(*sense)(void *hw);
	/* Read EDID block `n` (128 bytes) from the monitor. 0 or an errno. */
	int	(*read_edid_block)(void *hw, uint32_t n, uint8_t *out);
	/* Program `t` into the CRTC; it takes effect at the next mode set. */
	void	(*program_timing)(void *hw, const struct atrium_display_timing *t);
	/* Re-arm hot plug detection for the next change (after one fired, and
	 * at attach). May run from the interrupt handler. */
	void	(*hpd_rearm)(void *hw);
	/* Variable refresh for the timing `t` just programmed: frames from
	 * t's own period up to `min_hz`'s (see atrium_timing_vtotal_at), a
	 * pending flip ending a stretched one. min_hz 0 = fixed refresh.
	 * Optional: NULL means the CRTC can't. */
	void	(*set_vrr)(void *hw, const struct atrium_display_timing *t,
		    uint32_t min_hz);
};

/* What a connector's monitor offers, as last probed. */
struct atrium_display_sink {
	uint32_t connected;
	uint32_t edid_len;			/* bytes in edid[] */
	uint8_t  edid[ATRIUM_EDID_MAX_LEN];
	uint32_t ntimings;
	struct atrium_display_timing timings[ATRIUM_DISPLAY_SINK_MODES];
	uint32_t vrr_min_hz;			/* variable refresh range; 0 = none */
	uint32_t vrr_max_hz;
};

/*
 * Probe the monitor through `ops`: sense it, read the base block and the
 * extension blocks it announces, and parse the modes. A monitor whose EDID
 * is unreadable or invalid is reported connected with no modes. Sleeps on
 * the driver's register reads: call it without spin locks held.
 */
int	atrium_display_probe_sink(const struct atrium_display_hw_ops *ops,
	    void *hw, struct atrium_display_sink *s);

#endif /* _ATRIUM_DISPLAY_HW_H_ */
