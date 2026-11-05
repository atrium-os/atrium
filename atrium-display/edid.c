/*
 * atrium-display: EDID and CTA-861 parsing (see edid.h).
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#include "edid.h"

static const uint8_t edid_header[8] = {
	0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00
};

#define CTA_TAG		0x02
#define CTA_VIDEO_BLOCK	2

uint32_t
atrium_timing_refresh_mhz(const struct atrium_display_timing *t)
{
	uint64_t px = (uint64_t)(t->hactive + t->hfront + t->hsync + t->hback) *
	    (t->vactive + t->vfront + t->vsync + t->vback);

	if (px == 0)
		return (0);
	return ((uint32_t)((uint64_t)t->pixel_clock_khz * 1000000u / px));
}

int
atrium_timing_equal(const struct atrium_display_timing *a,
    const struct atrium_display_timing *b)
{
	return (a->pixel_clock_khz == b->pixel_clock_khz &&
	    a->hactive == b->hactive && a->hfront == b->hfront &&
	    a->hsync == b->hsync && a->hback == b->hback &&
	    a->vactive == b->vactive && a->vfront == b->vfront &&
	    a->vsync == b->vsync && a->vback == b->vback);
}

int
atrium_edid_block_ok(const uint8_t *block)
{
	uint32_t i, sum = 0;

	for (i = 0; i < ATRIUM_EDID_BLOCK; i++)
		sum += block[i];
	return ((sum & 0xff) == 0);
}

int
atrium_edid_base_ok(const uint8_t *e, uint32_t len)
{
	uint32_t i;

	if (len < ATRIUM_EDID_BLOCK)
		return (0);
	for (i = 0; i < 8; i++)
		if (e[i] != edid_header[i])
			return (0);
	return (atrium_edid_block_ok(e));
}

uint32_t
atrium_edid_ext_count(const uint8_t *e, uint32_t len)
{
	if (!atrium_edid_base_ok(e, len))
		return (0);
	return (e[126] < ATRIUM_EDID_MAX_EXT ? e[126] : ATRIUM_EDID_MAX_EXT);
}

int
atrium_edid_decode_dtd(const uint8_t *d, struct atrium_display_timing *t)
{
	uint32_t pc10k, hblank, vblank;

	pc10k = (uint32_t)d[0] | ((uint32_t)d[1] << 8);
	if (pc10k == 0)
		return (0);		/* a display descriptor, not a timing */
	if (d[17] & 0x80)
		return (0);		/* interlaced: not scanned out */
	t->pixel_clock_khz = pc10k * 10;
	t->hactive = (uint32_t)d[2] | ((uint32_t)(d[4] >> 4) << 8);
	hblank = (uint32_t)d[3] | ((uint32_t)(d[4] & 0x0f) << 8);
	t->vactive = (uint32_t)d[5] | ((uint32_t)(d[7] >> 4) << 8);
	vblank = (uint32_t)d[6] | ((uint32_t)(d[7] & 0x0f) << 8);
	t->hfront = (uint32_t)d[8] | ((uint32_t)((d[11] >> 6) & 3) << 8);
	t->hsync = (uint32_t)d[9] | ((uint32_t)((d[11] >> 4) & 3) << 8);
	t->vfront = (uint32_t)(d[10] >> 4) | ((uint32_t)((d[11] >> 2) & 3) << 4);
	t->vsync = (uint32_t)(d[10] & 0x0f) | ((uint32_t)(d[11] & 3) << 4);
	/* Blanking that can't hold its own porch and sync is corrupt. */
	if (t->hactive == 0 || t->vactive == 0 || t->hsync == 0 ||
	    t->vsync == 0 || hblank < t->hfront + t->hsync ||
	    vblank < t->vfront + t->vsync)
		return (0);
	t->hback = hblank - t->hfront - t->hsync;
	t->vback = vblank - t->vfront - t->vsync;
	t->flags = 0;
	return (1);
}

/* CTA-861 progressive formats: VIC, clock kHz, h active/front/sync/back,
 * v active/front/sync/back. */
static const struct {
	uint8_t vic;
	uint32_t khz;
	uint16_t h[4], v[4];
} cta_vics[] = {
	{  1,  25175, {  640,   16,  96,  48 }, {  480, 10,  2, 33 } },
	{  2,  27000, {  720,   16,  62,  60 }, {  480,  9,  6, 30 } },
	{  3,  27000, {  720,   16,  62,  60 }, {  480,  9,  6, 30 } },
	{  4,  74250, { 1280,  110,  40, 220 }, {  720,  5,  5, 20 } },
	{ 16, 148500, { 1920,   88,  44, 148 }, { 1080,  4,  5, 36 } },
	{ 17,  27000, {  720,   12,  64,  68 }, {  576,  5,  5, 39 } },
	{ 18,  27000, {  720,   12,  64,  68 }, {  576,  5,  5, 39 } },
	{ 19,  74250, { 1280,  440,  40, 220 }, {  720,  5,  5, 20 } },
	{ 31, 148500, { 1920,  528,  44, 148 }, { 1080,  4,  5, 36 } },
	{ 32,  74250, { 1920,  638,  44, 148 }, { 1080,  4,  5, 36 } },
	{ 33,  74250, { 1920,  528,  44, 148 }, { 1080,  4,  5, 36 } },
	{ 34,  74250, { 1920,   88,  44, 148 }, { 1080,  4,  5, 36 } },
	{ 63, 297000, { 1920,   88,  44, 148 }, { 1080,  4,  5, 36 } },
	{ 64, 297000, { 1920,  528,  44, 148 }, { 1080,  4,  5, 36 } },
	{ 93, 297000, { 3840, 1276,  88, 296 }, { 2160,  8, 10, 72 } },
	{ 94, 297000, { 3840, 1056,  88, 296 }, { 2160,  8, 10, 72 } },
	{ 95, 297000, { 3840,  176,  88, 296 }, { 2160,  8, 10, 72 } },
	{ 96, 594000, { 3840, 1056,  88, 296 }, { 2160,  8, 10, 72 } },
	{ 97, 594000, { 3840,  176,  88, 296 }, { 2160,  8, 10, 72 } },
};

int
atrium_cta_vic_timing(uint8_t vic, struct atrium_display_timing *t)
{
	uint32_t i;

	for (i = 0; i < sizeof(cta_vics) / sizeof(cta_vics[0]); i++) {
		if (cta_vics[i].vic != vic)
			continue;
		t->pixel_clock_khz = cta_vics[i].khz;
		t->hactive = cta_vics[i].h[0];
		t->hfront = cta_vics[i].h[1];
		t->hsync = cta_vics[i].h[2];
		t->hback = cta_vics[i].h[3];
		t->vactive = cta_vics[i].v[0];
		t->vfront = cta_vics[i].v[1];
		t->vsync = cta_vics[i].v[2];
		t->vback = cta_vics[i].v[3];
		t->flags = 0;
		return (1);
	}
	return (0);
}

/* Append `t` unless it is already listed. */
static void
add(struct atrium_display_timing *out, uint32_t max, uint32_t *n,
    const struct atrium_display_timing *t)
{
	uint32_t i;

	for (i = 0; i < *n; i++)
		if (atrium_timing_equal(&out[i], t))
			return;
	if (*n < max)
		out[(*n)++] = *t;
}

uint32_t
atrium_edid_parse(const uint8_t *e, uint32_t len,
    struct atrium_display_timing *out, uint32_t max)
{
	struct atrium_display_timing t;
	const uint8_t *x;
	uint32_t n = 0, nx, k, i, d, l, o;
	uint8_t vic;

	if (!atrium_edid_base_ok(e, len))
		return (0);
	for (o = 54; o + 18 <= 126; o += 18) {
		if (!atrium_edid_decode_dtd(&e[o], &t))
			continue;
		if (n == 0 && o == 54)
			t.flags |= ATRIUM_TIMING_PREFERRED;
		add(out, max, &n, &t);
	}
	nx = atrium_edid_ext_count(e, len);
	for (k = 1; k <= nx; k++) {
		if ((k + 1) * ATRIUM_EDID_BLOCK > len)
			break;
		x = &e[k * ATRIUM_EDID_BLOCK];
		if (x[0] != CTA_TAG || !atrium_edid_block_ok(x))
			continue;
		d = x[2] < ATRIUM_EDID_BLOCK - 1 ? x[2] : ATRIUM_EDID_BLOCK - 1;
		/* Data block collection: bytes 4 .. d-1. */
		for (i = 4; i < d; i += 1 + l) {
			l = x[i] & 0x1f;
			if ((x[i] >> 5) != CTA_VIDEO_BLOCK)
				continue;
			for (o = i + 1; o < i + 1 + l && o < d; o++) {
				vic = x[o];
				if (vic >= 129 && vic <= 192)
					vic &= 0x7f;	/* bit 7: native */
				if (atrium_cta_vic_timing(vic, &t))
					add(out, max, &n, &t);
			}
		}
		/* Detailed timings from d until padding (pixel clock 0). */
		if (d < 4)
			continue;
		for (o = d; o + 18 <= ATRIUM_EDID_BLOCK - 1; o += 18) {
			if (x[o] == 0 && x[o + 1] == 0)
				break;
			if (atrium_edid_decode_dtd(&x[o], &t))
				add(out, max, &n, &t);
		}
	}
	return (n);
}

int
atrium_edid_vrr_range(const uint8_t *e, uint32_t len, uint32_t *min_hz,
    uint32_t *max_hz)
{
	static const uint32_t slots[] = { 54, 72, 90, 108 };
	const uint8_t *d;
	uint32_t i, lo, hi;

	if (!atrium_edid_base_ok(e, len) || (e[24] & 0x01) == 0)
		return (0);
	for (i = 0; i < sizeof(slots) / sizeof(slots[0]); i++) {
		d = e + slots[i];
		if (d[0] != 0 || d[1] != 0 || d[3] != 0xfd)
			continue;
		/* Byte 4 bits 1:0 add 255 to the max (bit 1) and min (bit 0)
		 * vertical rates, for monitors past 255 Hz. */
		lo = d[5] + ((d[4] & 0x01) ? 255 : 0);
		hi = d[6] + ((d[4] & 0x02) ? 255 : 0);
		if (lo == 0 || hi < lo + 10)
			return (0);
		*min_hz = lo;
		*max_hz = hi;
		return (1);
	}
	return (0);
}

uint32_t
atrium_timing_vtotal_at(const struct atrium_display_timing *t, uint32_t hz)
{
	uint64_t htotal = t->hactive + t->hfront + t->hsync + t->hback;
	uint32_t vtotal = t->vactive + t->vfront + t->vsync + t->vback;
	uint64_t lines;

	if (hz == 0 || htotal == 0 || t->pixel_clock_khz == 0)
		return (0);
	lines = (uint64_t)t->pixel_clock_khz * 1000 / (htotal * hz);
	return (lines < vtotal ? vtotal : (uint32_t)lines);
}
