/*
 * atrium-display: connector probing shared by every driver (display_hw.h).
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifdef _KERNEL
#include <sys/param.h>
#include <sys/systm.h>
#else
#include <string.h>
#endif
#include "display_hw.h"

int
atrium_display_probe_sink(const struct atrium_display_hw_ops *ops, void *hw,
    struct atrium_display_sink *s)
{
	uint32_t k, nx;
	int err;

	memset(s, 0, sizeof(*s));
	if (!ops->sense(hw))
		return (0);
	s->connected = 1;
	if ((err = ops->read_edid_block(hw, 0, s->edid)) != 0)
		return (err);
	s->edid_len = ATRIUM_EDID_BLOCK;
	nx = atrium_edid_ext_count(s->edid, s->edid_len);
	for (k = 1; k <= nx; k++) {
		if ((err = ops->read_edid_block(hw, k,
		    &s->edid[k * ATRIUM_EDID_BLOCK])) != 0)
			break;		/* keep the blocks we have */
		s->edid_len += ATRIUM_EDID_BLOCK;
	}
	s->ntimings = atrium_edid_parse(s->edid, s->edid_len, s->timings,
	    ATRIUM_DISPLAY_SINK_MODES);
	if (!atrium_edid_vrr_range(s->edid, s->edid_len, &s->vrr_min_hz,
	    &s->vrr_max_hz))
		s->vrr_min_hz = s->vrr_max_hz = 0;
	s->hdr = atrium_edid_hdr_caps(s->edid, s->edid_len, &s->hdr_caps);
	return (0);
}
