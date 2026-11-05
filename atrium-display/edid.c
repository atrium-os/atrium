/*
 * atrium-display: EDID and CTA-861 parsing (see edid.h).
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifdef _KERNEL
#include <sys/param.h>
#include <sys/systm.h>
#else
#include <string.h>
#endif
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

int
atrium_edid_hdr_caps(const uint8_t *e, uint32_t len, struct atrium_hdr_caps *out)
{
	const uint8_t *x, *b;
	uint32_t k, nx, d, i, l;
	int found;
	uint8_t bt2020;

	memset(out, 0, sizeof(*out));
	if (!atrium_edid_base_ok(e, len))
		return (0);
	nx = atrium_edid_ext_count(e, len);
	for (k = 1; k <= nx; k++) {
		if ((k + 1) * ATRIUM_EDID_BLOCK > len)
			break;
		x = &e[k * ATRIUM_EDID_BLOCK];
		if (x[0] != CTA_TAG || !atrium_edid_block_ok(x))
			continue;
		d = x[2] < ATRIUM_EDID_BLOCK - 1 ? x[2] : ATRIUM_EDID_BLOCK - 1;
		found = 0;
		bt2020 = 0;
		for (i = 4; i < d; i += 1 + l) {
			l = x[i] & 0x1f;
			b = &x[i + 1];
			/* Tag 7: extended; byte 1 is the extended tag. */
			if ((x[i] >> 5) != 7 || l < 2 || i + 1 + l > d)
				continue;
			if (b[0] == 0x05)
				bt2020 = (b[1] & 0x80) != 0;
			else if (b[0] == 0x06 && l >= 3 && (b[2] & 0x01)) {
				out->eotfs = b[1];
				out->max_cv = l >= 4 ? b[3] : 0;
				out->max_fall_cv = l >= 5 ? b[4] : 0;
				out->min_cv = l >= 6 ? b[5] : 0;
				found = 1;
			}
		}
		if (found) {
			out->bt2020_rgb = bt2020;
			return (1);
		}
	}
	return (0);
}

/* 2^(k/32) for k = 0..31, in 1/65536ths. */
static const uint32_t pow2_32nds[32] = {
	65536, 66971, 68438, 69936, 71468, 73032, 74632, 76266,
	77936, 79642, 81386, 83169, 84990, 86851, 88752, 90696,
	92682, 94711, 96785, 98905, 101070, 103283, 105545, 107856,
	110218, 112631, 115098, 117618, 120194, 122825, 125515, 128263,
};

uint32_t
atrium_hdr_cv_nits(uint8_t cv)
{
	uint64_t v;

	if (cv == 0)
		return (0);
	v = (uint64_t)50 * pow2_32nds[cv % 32] << (cv / 32);
	return ((uint32_t)((v + 32768) >> 16));
}

uint32_t
atrium_hdr_min_cv_ten_thousandths(uint8_t max_cv, uint8_t min_cv)
{
	uint64_t max = atrium_hdr_cv_nits(max_cv);

	/* max x (cv/255)^2 / 100 cd/m^2, x 10000 */
	return ((uint32_t)((max * min_cv * min_cv * 100 + 65025 / 2) / 65025));
}

static void
infoframe_finish(uint8_t *out, uint8_t type, uint8_t version, uint8_t len)
{
	uint32_t i;
	uint8_t sum;

	out[0] = type;
	out[1] = version;
	out[2] = len;
	out[3] = 0;
	sum = type + version + len;
	for (i = 0; i < len; i++)
		sum += out[5 + i];
	out[4] = (uint8_t)(0x100 - sum);
}

static void
put16(uint8_t *p, uint16_t v)
{
	p[0] = v & 0xff;
	p[1] = v >> 8;
}

void
atrium_infoframe_drm(const struct atrium_hdr_metadata *m, uint8_t *out)
{
	uint8_t *pb = out + 4;	/* pb[0] is the checksum */
	uint32_t i;

	memset(out, 0, ATRIUM_INFOFRAME_LEN);
	pb[1] = m->eotf & 0x7;
	pb[2] = 0;		/* static metadata type 1 */
	for (i = 0; i < 3; i++) {
		put16(&pb[3 + 4 * i], m->primaries_x[i]);
		put16(&pb[5 + 4 * i], m->primaries_y[i]);
	}
	put16(&pb[15], m->white_x);
	put16(&pb[17], m->white_y);
	put16(&pb[19], m->max_mastering_nits);
	put16(&pb[21], m->min_mastering);
	put16(&pb[23], m->max_cll);
	put16(&pb[25], m->max_fall);
	infoframe_finish(out, 0x87, 0x01, 26);
}

void
atrium_infoframe_avi(int bt2020, uint8_t vic, uint8_t *out)
{
	uint8_t *pb = out + 4;

	memset(out, 0, ATRIUM_INFOFRAME_LEN);
	pb[1] = 0x00;		/* Y = RGB, no active format, no bars */
	/* C (7:6): 3 = extended, else 0 (no data); R (3:0) 8 = as picture. */
	pb[2] = (bt2020 ? 0xc0 : 0x00) | 0x08;
	/* EC (6:4) 6 = BT.2020 RGB / YCC; Q (3:2) 2 = full range RGB. */
	pb[3] = (bt2020 ? 0x60 : 0x00) | 0x08;
	pb[4] = vic & 0x7f;
	infoframe_finish(out, 0x82, 0x02, 13);
}

/* --- identity (display ABI 3.0) ------------------------------------------ */

static uint64_t
fnv1a(uint64_t h, const uint8_t *p, uint32_t n)
{
	uint32_t i;

	for (i = 0; i < n; i++) {
		h ^= p[i];
		h *= 0x100000001b3ull;
	}
	return (h);
}

uint64_t
atrium_edid_identity(const uint8_t *e, uint32_t len, uint32_t path)
{
	uint64_t h = 0xcbf29ce484222325ull;
	const uint8_t *d;
	uint8_t pb[4];
	int serial = 0;
	uint32_t k;

	if (!atrium_edid_base_ok(e, len))
		return (0);
	h = fnv1a(h, &e[8], 8);		/* maker, product, serial number */
	if (e[12] | e[13] | e[14] | e[15])
		serial = 1;
	for (k = 0; k < 4; k++) {
		d = &e[54 + 18 * k];
		if (d[0] == 0 && d[1] == 0 && d[3] == 0xff) {
			h = fnv1a(h, &d[5], 13);
			serial = 1;
		}
	}
	if (!serial) {
		pb[0] = (uint8_t)path;
		pb[1] = (uint8_t)(path >> 8);
		pb[2] = (uint8_t)(path >> 16);
		pb[3] = (uint8_t)(path >> 24);
		h = fnv1a(h, pb, 4);
	}
	return (h != 0 ? h : 1);
}
