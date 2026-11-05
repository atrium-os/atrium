/*
 * atrium-display: EDID and CTA-861 parsing shared by every display driver.
 *
 * A driver reads the raw EDID blocks over whatever its hardware uses (DDC/I2C
 * for HDMI and DVI, the AUX channel for DisplayPort) through its
 * atrium_display_hw_ops (display_hw.h). Turning those bytes into modes is the
 * same for every GPU and lives here, so an AMD, Intel or older-RDNA driver
 * reports the same mode list for the same monitor.
 *
 * Plain C with no allocation and no libc: it builds into a kernel module and
 * into host-side tests (tests/test_edid.c) alike. Byte layouts are from the
 * public VESA E-EDID 1.4 and CTA-861 standards.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifndef _ATRIUM_DISPLAY_EDID_H_
#define _ATRIUM_DISPLAY_EDID_H_

#ifdef _KERNEL
#include <sys/types.h>
#else
#include <stdint.h>
#endif

#define ATRIUM_EDID_BLOCK	128
/* Extension blocks followed after the base block (E-EDID allows 255). */
#define ATRIUM_EDID_MAX_EXT	3
#define ATRIUM_EDID_MAX_LEN	(ATRIUM_EDID_BLOCK * (1 + ATRIUM_EDID_MAX_EXT))

#define ATRIUM_TIMING_PREFERRED	0x1u	/* the monitor's preferred (native) timing */

/*
 * One video timing. The line starts at the first active pixel: active, then
 * front porch, sync, back porch. Same for lines in a frame.
 */
struct atrium_display_timing {
	uint32_t pixel_clock_khz;
	uint32_t hactive, hfront, hsync, hback;
	uint32_t vactive, vfront, vsync, vback;
	uint32_t flags;			/* ATRIUM_TIMING_* */
};

/* Vertical refresh of `t` in milli-Hz (59940 for 59.94 Hz). */
uint32_t atrium_timing_refresh_mhz(const struct atrium_display_timing *t);

/* Do two timings describe the same raster and clock (flags ignored)? */
int atrium_timing_equal(const struct atrium_display_timing *a,
    const struct atrium_display_timing *b);

/* Does `block` (128 bytes) sum to 0 mod 256? */
int atrium_edid_block_ok(const uint8_t *block);

/* Is `e` a valid base block: fixed header and checksum? */
int atrium_edid_base_ok(const uint8_t *e, uint32_t len);

/* Extension blocks the base block announces (byte 126), capped at
 * ATRIUM_EDID_MAX_EXT. 0 for an invalid base block. */
uint32_t atrium_edid_ext_count(const uint8_t *e, uint32_t len);

/*
 * Decode an 18-byte Detailed Timing Descriptor. Returns 0 for a display
 * descriptor (pixel clock 0), an interlaced or a malformed timing.
 */
int atrium_edid_decode_dtd(const uint8_t *d, struct atrium_display_timing *t);

/* The timing a CTA-861 VIC names, for the progressive VICs we know. */
int atrium_cta_vic_timing(uint8_t vic, struct atrium_display_timing *t);

/*
 * Every mode in an EDID of `len` bytes, in a fixed order: the base block's
 * detailed timings (the first is the preferred one and is flagged), then for
 * each CTA-861 extension its short video descriptors and its detailed
 * timings. Duplicates are dropped. An extension with a bad checksum or
 * another tag is skipped; an invalid base block gives no modes. Fills at most
 * `max` entries of `out`; returns how many it filled.
 */
uint32_t atrium_edid_parse(const uint8_t *e, uint32_t len,
    struct atrium_display_timing *out, uint32_t max);

/*
 * The vertical total (lines) that runs `t`'s raster at `hz`: the same
 * pixel clock and line length, a longer vertical blank. At least t's own
 * total; 0 for hz 0 or a timing without a clock.
 */
uint32_t atrium_timing_vtotal_at(const struct atrium_display_timing *t,
    uint32_t hz);

/*
 * The refresh range, in Hz, a monitor takes any rate within (variable
 * refresh): a valid base block with the E-EDID 1.4 continuous-frequency bit
 * (byte 24 bit 0) and a display range limits descriptor (0xFD) whose max
 * vertical rate exceeds its min by at least 10 Hz. Returns 0 (and leaves
 * the outputs alone) for a fixed-refresh monitor. HDMI's VRR data block and
 * DisplayID's adaptive-sync block are not read yet.
 */
int atrium_edid_vrr_range(const uint8_t *e, uint32_t len, uint32_t *min_hz,
    uint32_t *max_hz);

/*
 * HDR (CTA-861.3): what a monitor's HDR Static Metadata Data Block (CTA
 * extended tag 6) and Colorimetry Data Block (extended tag 5) declare.
 * eotfs has bit n set for EOTF code n (ATRIUM_EOTF_*). The luminances are
 * the block's coded values, 0 when not given; atrium_hdr_cv_* decode them.
 */
#define ATRIUM_EOTF_SDR		0	/* traditional gamma, SDR range */
#define ATRIUM_EOTF_TRAD_HDR	1	/* traditional gamma, HDR range */
#define ATRIUM_EOTF_PQ		2	/* SMPTE ST 2084 */
#define ATRIUM_EOTF_HLG		3	/* ARIB STD-B67 hybrid log-gamma */

struct atrium_hdr_caps {
	uint8_t eotfs;
	uint8_t max_cv;		/* desired content max luminance */
	uint8_t max_fall_cv;	/* desired content max frame-average */
	uint8_t min_cv;		/* desired content min luminance */
	uint8_t bt2020_rgb;	/* Colorimetry Data Block offers BT.2020 RGB */
	uint8_t _pad[3];
};

/*
 * The HDR capabilities of the first CTA-861 extension with an HDR Static
 * Metadata Data Block that offers static metadata type 1. Returns 0 (and
 * zeroes *out) when there is none: the monitor is SDR only.
 */
int atrium_edid_hdr_caps(const uint8_t *e, uint32_t len,
    struct atrium_hdr_caps *out);

/* CTA-861.3 luminance coding: max and max frame-average are 50 x 2^(cv/32)
 * cd/m^2 (0 for cv 0, "not given"); min is max x (cv/255)^2 / 100, here in
 * units of 0.0001 cd/m^2. Integer arithmetic, within 0.1%. */
uint32_t atrium_hdr_cv_nits(uint8_t cv);
uint32_t atrium_hdr_min_cv_ten_thousandths(uint8_t max_cv, uint8_t min_cv);

/*
 * Infoframes (CTA-861-G) a driver sends to put the monitor in an HDR mode,
 * built here so every driver sends the same bytes. Each fills 36 bytes in
 * the generic-packet layout DCN and most encoders take: HB0..HB2, a zero
 * byte, then PB0 (the checksum) .. PB31.
 */
#define ATRIUM_INFOFRAME_LEN	36

struct atrium_hdr_metadata {
	uint8_t  eotf;			/* ATRIUM_EOTF_* */
	uint8_t  _pad;
	uint16_t primaries_x[3];	/* R, G, B; units of 0.00002 */
	uint16_t primaries_y[3];
	uint16_t white_x, white_y;
	uint16_t max_mastering_nits;	/* cd/m^2 */
	uint16_t min_mastering;		/* 0.0001 cd/m^2 */
	uint16_t max_cll;		/* cd/m^2, 0 = unknown */
	uint16_t max_fall;
};

/* Dynamic Range and Mastering infoframe (type 0x87, version 1, length 26)
 * with static metadata type 1. */
void atrium_infoframe_drm(const struct atrium_hdr_metadata *m, uint8_t *out);

/* AVI infoframe (type 0x82, version 2, length 13): RGB, full range, the
 * picture's aspect, the timing's VIC (0 when it has none), and BT.2020
 * colorimetry (C = 3, EC = 6) when bt2020, else the default. */
void atrium_infoframe_avi(int bt2020, uint8_t vic, uint8_t *out);

#endif /* _ATRIUM_DISPLAY_EDID_H_ */
