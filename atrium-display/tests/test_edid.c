/*
 * Host-side tests for atrium-display's EDID parser and connector probe.
 * Build and run: make -C atrium-display/tests
 */
#include <assert.h>
#include <stdio.h>
#include <string.h>

#include "../display_hw.h"
#include "desktop_monitor_edid.h"

static void
expect(const struct atrium_display_timing *t, uint32_t w, uint32_t h,
    uint32_t mhz)
{
	if (t->hactive != w || t->vactive != h ||
	    atrium_timing_refresh_mhz(t) != mhz) {
		fprintf(stderr, "got %ux%u @ %u mHz, want %ux%u @ %u\n",
		    t->hactive, t->vactive, atrium_timing_refresh_mhz(t), w, h,
		    mhz);
		assert(0);
	}
}

/* A fake driver whose monitor is `edid`. */
struct fake {
	const uint8_t *edid;
	uint32_t len;
	int present;
	int reads;
};

static int
fake_sense(void *hw)
{
	return (((struct fake *)hw)->present);
}

static int
fake_read(void *hw, uint32_t n, uint8_t *out)
{
	struct fake *f = hw;
	uint32_t i;

	f->reads++;
	for (i = 0; i < ATRIUM_EDID_BLOCK; i++) {
		uint32_t o = n * ATRIUM_EDID_BLOCK + i;
		out[i] = o < f->len ? f->edid[o] : 0xff;
	}
	return (0);
}

static uint32_t
atrium_edid_sum(const uint8_t *b)
{
	uint32_t i, s = 0;

	for (i = 0; i < ATRIUM_EDID_BLOCK; i++)
		s += b[i];
	return (s);
}

static const struct atrium_display_hw_ops fake_ops = {
	.sense = fake_sense,
	.read_edid_block = fake_read,
};

int
main(void)
{
	struct atrium_display_timing t[16];
	struct atrium_display_sink s;
	struct fake f = { desktop_monitor_edid, 256, 1, 0 };
	uint8_t bad[256];
	uint32_t n;

	/* Every mode, preferred first and flagged; VICs then the CTA DTD. */
	n = atrium_edid_parse(desktop_monitor_edid, 256, t, 16);
	assert(n == 5);
	expect(&t[0], 1920, 1080, 60000);
	assert(t[0].flags & ATRIUM_TIMING_PREFERRED);
	expect(&t[1], 1920, 1080, 120000);
	expect(&t[2], 1280, 720, 60000);
	expect(&t[3], 640, 480, 59940);
	expect(&t[4], 2560, 1440, 59950);
	assert(!(t[1].flags & ATRIUM_TIMING_PREFERRED));
	assert(t[1].pixel_clock_khz == 297000 && t[4].pixel_clock_khz == 241500);
	assert(t[0].hfront == 88 && t[0].hsync == 44 && t[0].hback == 148);
	assert(t[0].vfront == 4 && t[0].vsync == 5 && t[0].vback == 36);

	/* A short output array is filled, not overrun. */
	assert(atrium_edid_parse(desktop_monitor_edid, 256, t, 2) == 2);

	/* A corrupt extension is skipped; a missing one too. */
	memcpy(bad, desktop_monitor_edid, 256);
	bad[200] ^= 0x55;
	assert(atrium_edid_parse(bad, 256, t, 16) == 1);
	assert(atrium_edid_parse(desktop_monitor_edid, 128, t, 16) == 1);
	/* A corrupt base block yields nothing. */
	bad[0] = 0x55;
	assert(atrium_edid_parse(bad, 256, t, 16) == 0);

	/* The probe reads exactly the blocks the base block announces. */
	assert(atrium_display_probe_sink(&fake_ops, &f, &s) == 0);
	assert(s.connected && s.edid_len == 256 && s.ntimings == 5);
	assert(f.reads == 2);
	assert(memcmp(s.edid, desktop_monitor_edid, 256) == 0);

	/* No monitor: nothing read. */
	f.present = 0;
	f.reads = 0;
	assert(atrium_display_probe_sink(&fake_ops, &f, &s) == 0);
	assert(!s.connected && s.ntimings == 0 && f.reads == 0);

	/* A floating bus (all 0xff): connected, no modes. */
	f.present = 1;
	f.len = 0;
	assert(atrium_display_probe_sink(&fake_ops, &f, &s) == 0);
	assert(s.connected && s.ntimings == 0 && s.edid_len == 128);

	/* Variable refresh: the desktop monitor takes 48..120 Hz. */
	{
		uint32_t lo = 0, hi = 0;
		uint8_t fixed[256];

		assert(atrium_edid_vrr_range(desktop_monitor_edid, 256, &lo, &hi));
		assert(lo == 48 && hi == 120);
		/* Without continuous frequency the range is only limits. */
		memcpy(fixed, desktop_monitor_edid, 256);
		fixed[24] &= ~0x01;
		fixed[127] += 0x01;	/* keep the checksum */
		assert(atrium_edid_block_ok(fixed));
		assert(!atrium_edid_vrr_range(fixed, 256, &lo, &hi));
		f.present = 1;
		f.len = 256;
		assert(atrium_display_probe_sink(&fake_ops, &f, &s) == 0);
		assert(s.vrr_min_hz == 48 && s.vrr_max_hz == 120);
		/* 1080p120 stretched to 48 Hz: 297 MHz / (2200 * 48). */
		assert(atrium_timing_vtotal_at(&s.timings[1], 48) == 2812);
		assert(atrium_timing_vtotal_at(&s.timings[1], 240) == 1125);
		assert(atrium_timing_vtotal_at(&s.timings[1], 0) == 0);
	}

	/* HDR: the desktop monitor offers SDR, PQ and HLG in BT.2020. */
	{
		struct atrium_hdr_caps h;
		struct atrium_hdr_metadata m = {
			.eotf = ATRIUM_EOTF_PQ,
			.primaries_x = { 35400, 8500, 6550 },
			.primaries_y = { 14600, 39850, 2300 },
			.white_x = 15635, .white_y = 16450,
			.max_mastering_nits = 1000, .min_mastering = 50,
			.max_cll = 800, .max_fall = 300,
		};
		uint8_t fr[ATRIUM_INFOFRAME_LEN], sum;
		uint32_t i;

		assert(atrium_edid_hdr_caps(desktop_monitor_edid, 256, &h));
		assert(h.eotfs == ((1 << ATRIUM_EOTF_SDR) | (1 << ATRIUM_EOTF_PQ) |
		    (1 << ATRIUM_EOTF_HLG)));
		assert(h.max_cv == 115 && h.max_fall_cv == 96 && h.min_cv == 23);
		assert(h.bt2020_rgb);
		assert(atrium_hdr_cv_nits(115) == 604);
		assert(atrium_hdr_cv_nits(96) == 400);
		assert(atrium_hdr_cv_nits(0) == 0);
		assert(atrium_hdr_cv_nits(255) == 12526);	/* 50 x 2^(255/32) */
		i = atrium_hdr_min_cv_ten_thousandths(115, 23);
		assert(i >= 489 && i <= 493);			/* ~0.049 cd/m^2 */
		assert(s.hdr && s.hdr_caps.max_cv == 115);
		/* An EDID without the block: SDR only. */
		assert(!atrium_edid_hdr_caps(desktop_monitor_edid, 128, &h));
		assert(h.eotfs == 0);

		atrium_infoframe_drm(&m, fr);
		assert(fr[0] == 0x87 && fr[1] == 1 && fr[2] == 26 && fr[3] == 0);
		for (sum = 0, i = 0; i < 3; i++)
			sum += fr[i];
		for (i = 0; i <= 26; i++)
			sum += fr[4 + i];
		assert(sum == 0);
		assert(fr[5] == ATRIUM_EOTF_PQ && fr[6] == 0);
		assert(fr[7] == (35400 & 0xff) && fr[8] == (35400 >> 8));
		assert(fr[4 + 23] == (800 & 0xff) && fr[4 + 24] == (800 >> 8));
		assert(fr[4 + 25] == (300 & 0xff) && fr[4 + 26] == (300 >> 8));
		atrium_infoframe_avi(1, 16, fr);
		assert(fr[0] == 0x82 && fr[1] == 2 && fr[2] == 13);
		assert(fr[6] >> 6 == 3 && ((fr[7] >> 4) & 7) == 6 && fr[8] == 16);
		for (sum = 0, i = 0; i < 3; i++)
			sum += fr[i];
		for (i = 0; i <= 13; i++)
			sum += fr[4 + i];
		assert(sum == 0);
	}

	/* Identity: by maker, product and serial; the connector only for a
	 * monitor without a serial. */
	{
		uint8_t a[256], b2[256];
		uint64_t ia, ib;

		memcpy(a, desktop_monitor_edid, 256);
		a[12] = a[13] = a[14] = a[15] = 0;
		a[127] = 0;
		a[127] = (uint8_t)(0x100 - (atrium_edid_sum(a) & 0xff));
		assert(atrium_edid_block_ok(a));
		ia = atrium_edid_identity(a, 256, 0);
		assert(ia != 0 && ia != atrium_edid_identity(a, 256, 1));
		memcpy(b2, a, 256);
		b2[12] = 7;
		b2[127] = 0;
		b2[127] = (uint8_t)(0x100 - (atrium_edid_sum(b2) & 0xff));
		ib = atrium_edid_identity(b2, 256, 0);
		assert(ib != ia && ib == atrium_edid_identity(b2, 256, 3));
		b2[10] ^= 1;	/* another product: another identity */
		b2[127] = 0;
		b2[127] = (uint8_t)(0x100 - (atrium_edid_sum(b2) & 0xff));
		assert(atrium_edid_identity(b2, 256, 0) != ib);
		memset(b2, 0xff, sizeof(b2));
		assert(atrium_edid_identity(b2, 256, 0) == 0);
	}

	/* Known VICs decode; unknown ones don't. */
	assert(atrium_cta_vic_timing(97, &t[0]));
	expect(&t[0], 3840, 2160, 60000);
	assert(!atrium_cta_vic_timing(5, &t[0]));	/* 1080i: interlaced */

	printf("test_edid: all passed\n");
	return (0);
}
