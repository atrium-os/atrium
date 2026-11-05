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

#endif /* _ATRIUM_DISPLAY_EDID_H_ */
